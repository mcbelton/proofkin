use std::convert::Infallible;
use std::fs;
use std::io::Write as _;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Json, Router};
use proofkin::Profile;
use proofkin::upgrade::{Upgrade, try_binary};
use reqwest::Url;
use serde_json::json;
use sha2::{Digest, Sha256};

const EXE: &str = env!("CARGO_BIN_EXE_proofkin");
const TARGET: &str = env!("PROOFKIN_TARGET");
const BINARY: &str = if cfg!(windows) {
    "proofkin.exe"
} else {
    "proofkin"
};
const REPO: &str = "test/proofkin";
const LATEST: &str = "/repos/test/proofkin/releases/latest";
const NAMED: &str = "/repos/test/proofkin/releases/tags/v0.0.2";
const OLD: &[u8] = b"the previously installed binary";
const NEW: &[u8] = b"proofkin 0.0.2";

fn archive_name() -> String {
    let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
    format!("proofkin-{TARGET}.{extension}")
}

#[cfg(windows)]
fn pack(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

#[cfg(unix)]
fn pack(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gzip);
    for (name, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(data.len() as u64);
        tar.append_data(&mut header, name, *data).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}

struct Release {
    status: StatusCode,
    tag: &'static str,
    archive: Option<Vec<u8>>,
    sums: Option<Vec<u8>>,
}

impl Release {
    fn new(binary_path: &str, binary: &[u8]) -> Self {
        let archive = pack(&[
            ("README.md", b"readme"),
            ("LICENSE-MIT", b"MIT"),
            ("LICENSE-APACHE", b"Apache"),
            (binary_path, binary),
        ]);
        let mut sums = Vec::new();
        writeln!(sums, "{}  install.sh", "ab".repeat(32)).unwrap();
        let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(&archive));
        writeln!(sums, "{hash}  {}", archive_name()).unwrap();
        Self {
            status: StatusCode::OK,
            tag: "v0.0.2",
            archive: Some(archive),
            sums: Some(sums),
        }
    }
}

impl Default for Release {
    fn default() -> Self {
        let path = if cfg!(windows) { BINARY } else { "./proofkin" };
        Self::new(path, NEW)
    }
}

struct GitHub {
    api: Url,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    task: tokio::task::JoinHandle<()>,
}

struct Server {
    api: Url,
    release: Release,
    requests: Arc<Mutex<Vec<(String, String)>>>,
}

impl GitHub {
    async fn start(release: Release) -> Self {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let api = Url::parse(&format!("http:{}", listener.local_addr().unwrap())).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let router = Router::new().fallback(answer).with_state(Arc::new(Server {
            api: api.clone(),
            release,
            requests: Arc::clone(&requests),
        }));
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            api,
            requests,
            task,
        }
    }

    fn upgrade<'a>(&'a self, exe: &'a Path, current_version: &'a str) -> Upgrade<'a> {
        Upgrade {
            repo: REPO,
            target: TARGET,
            current_version,
            exe,
            api_base: &self.api,
            profiles_root: None,
        }
    }

    fn paths(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _)| path.clone())
            .collect()
    }
}

impl Drop for GitHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn answer(State(server): State<Arc<Server>>, uri: Uri, headers: HeaderMap) -> Response {
    assert!(!headers.contains_key(header::AUTHORIZATION));
    let agent = headers[header::USER_AGENT].to_str().unwrap();
    server
        .requests
        .lock()
        .unwrap()
        .push((uri.path().to_owned(), agent.to_owned()));
    match uri.path() {
        LATEST | NAMED => {
            if !server.release.status.is_success() {
                return server.release.status.into_response();
            }
            let mut assets = Vec::new();
            let mut asset = |name: &str, path| {
                assets.push(json!({
                    "name": name,
                    "browser_download_url": server.api.join(path).unwrap().as_str(),
                }));
            };
            if server.release.archive.is_some() {
                asset(&archive_name(), "archive");
            }
            if server.release.sums.is_some() {
                asset("SHA256SUMS", "SHA256SUMS");
            }
            Json(json!({"tag_name": server.release.tag, "assets": assets})).into_response()
        }
        "/archive" => Redirect::temporary("/download").into_response(),
        "/download" => server.release.archive.clone().unwrap().into_response(),
        "/SHA256SUMS" => {
            // A transfer without a Content-Length must still respect the body limit.
            let chunk = Ok::<_, Infallible>(server.release.sums.clone().unwrap());
            Body::from_stream(futures_util::stream::iter([chunk])).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn installed() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join(BINARY);
    fs::write(&exe, OLD).unwrap();
    (dir, exe)
}

fn assert_only_binary(exe: &Path, contents: &[u8]) {
    assert_eq!(fs::read(exe).unwrap(), contents);
    let files: Vec<_> = fs::read_dir(exe.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(files, [BINARY]);
}

async fn read_fake_version(path: &Path) -> Result<String> {
    assert_eq!(path.file_name().unwrap(), format!("{BINARY}.new").as_str());
    Ok(fs::read_to_string(path)?)
}

async fn unexpected_try(_: &Path) -> Result<String> {
    panic!("the candidate should not be run")
}

#[tokio::test]
async fn latest_replaces_only_the_binary_after_verification() {
    let (_dir, exe) = installed();
    let github = GitHub::start(Release::default()).await;
    let result = github
        .upgrade(&exe, "0.0.1")
        .run(None, read_fake_version)
        .await
        .unwrap();
    // The path as people write it: on Windows, without the `\\?\` of `canonicalize`.
    let canonical = exe.canonicalize().unwrap().display().to_string();
    let shown = canonical.strip_prefix(r"\\?\").unwrap_or(&canonical);
    assert_eq!(
        result,
        format!("Upgraded proofkin 0.0.1 -> 0.0.2 ({shown}).")
    );
    assert_only_binary(&exe, NEW);
    assert_eq!(
        github.paths(),
        [LATEST, "/archive", "/download", "/SHA256SUMS"]
    );
    assert!(
        github
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(_, agent)| agent == "proofkin/0.0.1")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(exe.metadata().unwrap().permissions().mode() & 0o777, 0o755);
    }
}

#[tokio::test]
async fn latest_never_downloads_if_installed_or_older() {
    let (_dir, exe) = installed();
    let github = GitHub::start(Release::default()).await;
    for (current, message) in [
        ("0.0.2", "proofkin 0.0.2 is the latest release"),
        (
            "0.0.10",
            "this proofkin (0.0.10) is newer than the latest release (0.0.2); nothing changed",
        ),
    ] {
        let result = github
            .upgrade(&exe, current)
            .run(None, unexpected_try)
            .await
            .unwrap();
        assert_eq!(result, message);
        assert_only_binary(&exe, OLD);
    }
    assert_eq!(github.paths(), [LATEST, LATEST]);
}

#[tokio::test]
async fn named_versions_lookup_the_same_tag_and_allow_a_downgrade() {
    for (requested, current, action) in [
        ("v0.0.2", "0.0.1", "Upgraded"),
        ("0.0.2", "0.0.10", "Downgraded"),
    ] {
        let (_dir, exe) = installed();
        let github = GitHub::start(Release::default()).await;
        let result = github
            .upgrade(&exe, current)
            .run(Some(requested.parse().unwrap()), read_fake_version)
            .await
            .unwrap();
        assert!(result.starts_with(&format!("{action} proofkin {current} -> 0.0.2 (")));
        assert_eq!(github.paths()[0], NAMED);
        assert_only_binary(&exe, NEW);
    }
    let (_dir, exe) = installed();
    let github = GitHub::start(Release::default()).await;
    let result = github
        .upgrade(&exe, "0.0.2")
        .run(Some("0.0.2".parse().unwrap()), unexpected_try)
        .await
        .unwrap();
    assert_eq!(result, "proofkin 0.0.2 is already installed");
    assert_eq!(github.paths(), [NAMED]);
    assert_only_binary(&exe, OLD);
}

#[tokio::test]
async fn checksum_mismatch_leaves_the_binary_untouched() {
    let (_dir, exe) = installed();
    let github = GitHub::start(Release {
        sums: Some(format!("{}  {}", "00".repeat(32), archive_name()).into_bytes()),
        ..Release::default()
    })
    .await;
    let error = github
        .upgrade(&exe, "0.0.1")
        .run(None, unexpected_try)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "the download doesn't match v0.0.2's SHA256SUMS; nothing was changed"
    );
    assert_only_binary(&exe, OLD);
}

#[tokio::test]
async fn missing_assets_are_reported_before_any_download() {
    for (release, expected) in [
        (Release { archive: None, ..Release::default() }, format!("v0.0.2 has no build for {TARGET}")),
        (Release { sums: None, ..Release::default() }, "v0.0.2 has no SHA256SUMS, so its download can't be checked: install it with the installer".to_owned()),
    ] {
        let (_dir, exe) = installed();
        let github = GitHub::start(release).await;
        let error = github.upgrade(&exe, "0.0.1").run(None, unexpected_try).await.unwrap_err();
        assert_eq!(error.to_string(), expected);
        assert_eq!(github.paths(), [LATEST]);
        assert_only_binary(&exe, OLD);
    }
}

#[tokio::test]
async fn bad_release_statuses_and_tags_do_not_start_downloads() {
    for (status, tag, expected) in [
        (
            StatusCode::NOT_FOUND,
            "v0.0.2",
            "no release v0.0.2 in test/proofkin",
        ),
        (
            StatusCode::TOO_MANY_REQUESTS,
            "v0.0.2",
            "HTTP 429 Too Many Requests; nothing was changed",
        ),
        (StatusCode::OK, "v0.0.2-preview", "invalid release version"),
    ] {
        let (_dir, exe) = installed();
        let github = GitHub::start(Release {
            status,
            tag,
            ..Release::default()
        })
        .await;
        let error = github
            .upgrade(&exe, "0.0.1")
            .run(Some("v0.0.2".parse().unwrap()), unexpected_try)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error:#}");
        assert_eq!(github.paths(), [NAMED]);
        assert_only_binary(&exe, OLD);
    }
}

#[tokio::test]
async fn checksum_download_stops_at_64_kib_even_without_a_length_header() {
    let (_dir, exe) = installed();
    let github = GitHub::start(Release {
        sums: Some(vec![b'a'; 64 * 1024 + 1]),
        ..Release::default()
    })
    .await;
    let error = github
        .upgrade(&exe, "0.0.1")
        .run(None, unexpected_try)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("download is larger than 65536 bytes"));
    assert_only_binary(&exe, OLD);
}

#[tokio::test]
async fn an_archive_with_only_a_nested_binary_is_refused() {
    let (_dir, exe) = installed();
    let github = GitHub::start(Release::new(&format!("nested/{BINARY}"), NEW)).await;
    let error = github
        .upgrade(&exe, "0.0.1")
        .run(None, unexpected_try)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("at its root"));
    assert_only_binary(&exe, OLD);
}

#[tokio::test]
async fn a_failed_try_or_wrong_version_removes_the_candidate() {
    let (_dir, exe) = installed();
    let github = GitHub::start(Release::default()).await;
    let error = github
        .upgrade(&exe, "0.0.1")
        .run(None, async |path| {
            assert_eq!(fs::read(path)?, NEW);
            bail!("cannot run on this machine")
        })
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "the new binary doesn't run here (cannot run on this machine); nothing was changed"
    );
    assert_only_binary(&exe, OLD);

    let github = GitHub::start(Release::new(BINARY, b"proofkin 0.0.3")).await;
    let error = github
        .upgrade(&exe, "0.0.1")
        .run(None, read_fake_version)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("expected"));
    assert_only_binary(&exe, OLD);
}

#[tokio::test]
async fn a_failed_swap_keeps_the_old_executable_at_its_path() {
    let (_dir, exe) = installed();
    let github = GitHub::start(Release::default()).await;
    let error = github
        .upgrade(&exe, "0.0.1")
        .run(None, async |path| {
            let version = read_fake_version(path).await?;
            fs::remove_file(path)?;
            Ok(version)
        })
        .await
        .unwrap_err();
    assert!(error.to_string().starts_with("can't replace "));
    assert!(
        error
            .to_string()
            .contains("or install with the installer into a folder you own")
    );
    assert_only_binary(&exe, OLD);
}

#[tokio::test]
async fn a_staged_binary_left_by_an_interrupted_upgrade_is_replaced() {
    let (_dir, exe) = installed();
    fs::write(
        exe.with_file_name(format!("{BINARY}.new")),
        b"left by an interrupted upgrade",
    )
    .unwrap();
    let github = GitHub::start(Release::default()).await;
    github
        .upgrade(&exe, "0.0.1")
        .run(None, read_fake_version)
        .await
        .unwrap();
    assert_only_binary(&exe, NEW);
}

#[cfg(windows)]
#[tokio::test]
async fn a_windows_binary_in_use_is_replaced_and_cleaned_up_next_time() {
    use std::os::windows::fs::OpenOptionsExt;

    let (dir, exe) = installed();
    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(1 | 4)
        .open(&exe)
        .unwrap();
    let github = GitHub::start(Release::default()).await;
    github
        .upgrade(&exe, "0.0.1")
        .run(None, read_fake_version)
        .await
        .unwrap();
    assert_eq!(fs::read(&exe).unwrap(), NEW);
    let prefix = format!("{BINARY}.old-");
    let entries: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap())
        .collect();
    assert_eq!(entries.len(), 2);
    let backup = entries
        .into_iter()
        .find(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .unwrap();
    assert_eq!(fs::read(backup.path()).unwrap(), OLD);
    drop(held);
    github
        .upgrade(&exe, "0.0.2")
        .run(None, unexpected_try)
        .await
        .unwrap();
    assert_only_binary(&exe, NEW);
}

#[cfg(unix)]
#[tokio::test]
async fn upgrading_through_a_symlink_replaces_its_target_not_the_link() {
    let (_dir, exe) = installed();
    let links = tempfile::tempdir().unwrap();
    let link = links.path().join("linked-proofkin");
    std::os::unix::fs::symlink(&exe, &link).unwrap();
    let github = GitHub::start(Release::default()).await;
    github
        .upgrade(&link, "0.0.1")
        .run(None, read_fake_version)
        .await
        .unwrap();
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_only_binary(&exe, NEW);
}

#[tokio::test]
async fn running_profiles_are_named_in_the_restart_notice() {
    let (_dir, exe) = installed();
    let profiles = tempfile::tempdir().unwrap();
    let active = Profile::open_in(profiles.path(), "belton-devbox").unwrap();
    active.save().unwrap();
    let _lock = active.lock_instance().unwrap();
    Profile::open_in(profiles.path(), "stopped")
        .unwrap()
        .save()
        .unwrap();

    let github = GitHub::start(Release::default()).await;
    let upgrade = Upgrade {
        profiles_root: Some(profiles.path()),
        ..github.upgrade(&exe, "0.0.1")
    };
    let result = upgrade.run(None, read_fake_version).await.unwrap();
    assert_eq!(
        result.lines().nth(1).unwrap(),
        "Agents running now keep 0.0.1 until you restart them (Ctrl+C or /quit, then start them as before): belton-devbox."
    );
}

#[tokio::test]
async fn default_try_reads_the_real_version_and_rejects_non_programs() {
    let exe = Path::new(EXE);
    let expected = concat!("proofkin ", env!("CARGO_PKG_VERSION"));
    assert_eq!(try_binary(exe).await.unwrap(), expected);
    #[cfg(windows)]
    {
        // Check the staging extension without making a copy of the large debug executable.
        let dir = tempfile::tempdir_in(exe.parent().unwrap()).unwrap();
        let staged = dir.path().join("proofkin.exe.new");
        fs::hard_link(exe, &staged).unwrap();
        assert_eq!(
            try_binary(&staged.canonicalize().unwrap()).await.unwrap(),
            expected
        );
    }
    let (_dir, not_a_program) = installed();
    assert!(try_binary(&not_a_program).await.is_err());
}

#[test]
fn cli_source_build_refuses_upgrade_and_alias_even_with_release_env_at_runtime() {
    if option_env!("PROOFKIN_RELEASE_REPO").is_some() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for flag in ["--upgrade", "--update"] {
        let mut command = std::process::Command::new(EXE);
        command.arg(flag);
        command.env("PROOFKIN_HOME", dir.path());
        command.envs([
            ("PROOFKIN_BRIDGE", "unused.js"),
            ("PROOFKIN_RELEASE_REPO", "test/proofkin"),
        ]);
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("this proofkin was built from source, so --upgrade doesn't replace it")
        );
    }
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}
