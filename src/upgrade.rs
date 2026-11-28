//! The command-line updater. Only release builds call it, with their build's repository.

use std::fmt::{self, Display, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::{Client, Response, StatusCode, Url};
use serde::Deserialize;

use crate::Profile;
use crate::evidence::sha256_hex;
use crate::workiq::client::http_client_builder;

const MAX_RELEASE: usize = 1024 * 1024;
const MAX_ARCHIVE: usize = 256 * 1024 * 1024;
const MAX_SUMS: usize = 64 * 1024;
const MAX_BINARY: u64 = 512 * 1024 * 1024;
const SUMS: &str = "SHA256SUMS";
const BINARY: &str = if cfg!(windows) {
    "proofkin.exe"
} else {
    "proofkin"
};

/// A release version, with or without a leading `v` on input.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version([u64; 3]);

impl FromStr for Version {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let invalid = || anyhow!("invalid release version {text:?}: expected X.Y.Z or vX.Y.Z");
        let mut parts = text.strip_prefix('v').unwrap_or(text).split('.');
        let mut numbers = [0; 3];
        for number in &mut numbers {
            *number = parts
                .next()
                .filter(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
                .and_then(|part| part.parse().ok())
                .ok_or_else(invalid)?;
        }
        if parts.next().is_some() {
            return Err(invalid());
        }
        Ok(Self(numbers))
    }
}

impl Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch] = self.0;
        write!(formatter, "{major}.{minor}.{patch}")
    }
}

pub struct Upgrade<'a> {
    pub repo: &'a str,
    pub target: &'a str,
    pub current_version: &'a str,
    pub exe: &'a Path,
    pub api_base: &'a Url,
    pub profiles_root: Option<&'a Path>,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

impl Upgrade<'_> {
    /// Returns the result to print, after reporting download progress as it happens.
    pub async fn run(
        self,
        requested: Option<Version>,
        try_new: impl AsyncFnOnce(&Path) -> Result<String>,
    ) -> Result<String> {
        let current: Version = self.current_version.parse()?;
        let exe = self
            .exe
            .canonicalize()
            .map_err(|error| replace_error(self.exe, error))?;
        #[cfg(windows)]
        clean_old(&exe);

        let http = http_client_builder(Duration::from_secs(300))?
            .user_agent(format!("proofkin/{}", self.current_version))
            .https_only(self.api_base.scheme() == "https")
            .build()
            .context("could not build the HTTP client; nothing was changed")?;
        let Release {
            tag_name: tag,
            assets,
        } = self.find_release(&http, requested).await?;
        let version: Version = tag.parse()?;
        if let Some(wanted) = requested {
            ensure!(
                wanted == version,
                "requested v{wanted}, but found {tag}; nothing was changed"
            );
            if version == current {
                return Ok(format!("proofkin {current} is already installed"));
            }
        } else if version == current {
            return Ok(format!("proofkin {current} is the latest release"));
        } else if version < current {
            return Ok(format!(
                "this proofkin ({current}) is newer than the latest release ({version}); nothing changed"
            ));
        }

        let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
        let name = format!("proofkin-{}.{extension}", self.target);
        let find = |name| assets.iter().find(|asset| asset.name == name);
        let archive = find(name.as_str())
            .with_context(|| format!("{tag} has no build for {}", self.target))?;
        let sums = find(SUMS).with_context(|| {
            format!("{tag} has no SHA256SUMS, so its download can't be checked: install it with the installer")
        })?;
        let archive = download(&http, archive, MAX_ARCHIVE).await?;
        let sums = download(&http, sums, MAX_SUMS).await?;
        ensure!(
            checksum_matches(&sums, &name, &archive),
            "the download doesn't match {tag}'s SHA256SUMS; nothing was changed"
        );
        println!("{name} matches {tag}'s SHA256SUMS.");

        let staged = Staged::unpack(&exe, &archive)?;
        drop(archive);
        let expected = format!("proofkin {version}");
        let result = try_new(&staged.path).await.and_then(|shown| {
            ensure!(shown == expected, "expected {expected:?}, got {shown:?}");
            Ok(())
        });
        if let Err(why) = result {
            bail!("the new binary doesn't run here ({why:#}); nothing was changed");
        }
        staged.install(&exe)?;

        let action = if version < current {
            "Downgraded"
        } else {
            "Upgraded"
        };
        let mut result = format!(
            "{action} proofkin {current} -> {version} ({}).",
            exe.display()
        );
        if let Some(root) = self.profiles_root {
            let running: Vec<_> = Profile::list_in(root)
                .into_iter()
                .filter(|name| {
                    Profile::open_in(root, name).is_ok_and(|profile| profile.is_running())
                })
                .collect();
            if !running.is_empty() {
                writeln!(result)?;
                let names = running.join(", ");
                write!(
                    result,
                    "Agents running now keep {current} until you restart them (Ctrl+C or /quit, then start them as before): {names}."
                )?;
            }
        }
        Ok(result)
    }

    async fn find_release(&self, http: &Client, requested: Option<Version>) -> Result<Release> {
        let tag = requested.map_or_else(|| "latest".to_owned(), |version| format!("v{version}"));
        let lookup = if requested.is_some() {
            format!("tags/{tag}")
        } else {
            tag.clone()
        };
        let repo = self.repo;
        let url = self
            .api_base
            .join(&format!("repos/{repo}/releases/{lookup}"))?;
        let response =
            http.get(url).send().await.with_context(|| {
                format!("could not look up {tag} in {repo}; nothing was changed")
            })?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            bail!("no release {tag} in {repo}");
        }
        ensure!(
            status.is_success(),
            "could not look up {tag} in {repo}: HTTP {status}; nothing was changed"
        );
        let invalid =
            || format!("could not read {tag} in {repo} (HTTP {status}); nothing was changed");
        let body = read_bounded(response, MAX_RELEASE)
            .await
            .with_context(invalid)?;
        serde_json::from_slice(&body).with_context(invalid)
    }
}

async fn download(http: &Client, asset: &Asset, limit: usize) -> Result<Vec<u8>> {
    println!("Downloading {}...", asset.name);
    async {
        let response = http.get(&asset.browser_download_url).send().await?;
        let status = response.status();
        ensure!(status.is_success(), "HTTP {status}");
        read_bounded(response, limit).await
    }
    .await
    .with_context(|| format!("could not download {}; nothing was changed", asset.name))
}

async fn read_bounded(mut response: Response, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= limit as u64),
        "download is larger than {limit} bytes"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= limit - bytes.len(),
            "download is larger than {limit} bytes"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn checksum_matches(sums: &[u8], name: &str, archive: &[u8]) -> bool {
    let Ok(sums) = std::str::from_utf8(sums) else {
        return false;
    };
    let mut matches = sums
        .lines()
        .filter_map(|line| line.split_once("  "))
        .filter(|(_, file)| *file == name);
    let actual = sha256_hex(archive);
    matches
        .next()
        .is_some_and(|(hash, _)| hash.eq_ignore_ascii_case(&actual))
        && matches.next().is_none()
}

/// Runs the candidate and reads the single version line it prints.
pub async fn try_binary(path: &Path) -> Result<String> {
    let output = tokio::process::Command::new(path)
        .arg("--version")
        .kill_on_drop(true)
        .output()
        .await
        .context("could not run --version")?;
    ensure!(
        output.status.success(),
        "--version gave {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = std::str::from_utf8(&output.stdout)?;
    let mut lines = text.lines();
    let version = lines.next().context("--version printed nothing")?;
    ensure!(
        lines.next().is_none(),
        "--version printed more than one line"
    );
    Ok(version.to_owned())
}

struct Staged {
    path: PathBuf,
    installed: bool,
}

impl Staged {
    fn unpack(exe: &Path, archive: &[u8]) -> Result<Self> {
        let path = with_suffix(exe, ".new");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|error| replace_error(exe, error))?;
        let staged = Self {
            path,
            installed: false,
        };
        // Close the file before the staging guard tries to remove it on an error.
        let mut file = file;
        unpack_binary(archive, &mut file)
            .context("could not unpack the release; nothing was changed")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o755))
                .map_err(|error| replace_error(exe, error))?;
        }
        Ok(staged)
    }

    fn install(mut self, exe: &Path) -> Result<()> {
        swap(exe, &self.path).map_err(|error| replace_error(exe, error))?;
        self.installed = true;
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.installed
            && let Err(error) = fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("could not remove {}: {error}", self.path.display());
        }
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}

fn replace_error(exe: &Path, error: io::Error) -> anyhow::Error {
    anyhow!(
        "can't replace {}: {error}. Run it with rights to write there, or install with the installer into a folder you own",
        exe.display()
    )
}

#[cfg(windows)]
fn unpack_binary(archive: &[u8], file: &mut File) -> Result<()> {
    let mut zip = zip::ZipArchive::new(io::Cursor::new(archive))?;
    let entry = zip
        .by_name(BINARY)
        .with_context(|| format!("the archive has no {BINARY} at its root"))?;
    ensure!(
        entry.is_file()
            && entry
                .unix_mode()
                .is_none_or(|mode| matches!(mode & 0o170000, 0 | 0o100000)),
        "the archive has no regular {BINARY} at its root"
    );
    let size = entry.size();
    copy_binary(entry, size, file)
}

#[cfg(unix)]
fn unpack_binary(archive: &[u8], file: &mut File) -> Result<()> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries()? {
        let entry = entry?;
        if entry.header().entry_type().is_file()
            && matches!(entry.path_bytes().as_ref(), b"proofkin" | b"./proofkin")
        {
            let size = entry.size();
            return copy_binary(entry, size, file);
        }
    }
    bail!("the archive has no regular {BINARY} at its root");
}

fn copy_binary(mut entry: impl Read, size: u64, file: &mut File) -> Result<()> {
    ensure!(size <= MAX_BINARY, "{BINARY} is larger than 512 MiB");
    let written = io::copy(&mut entry.by_ref().take(MAX_BINARY), file)?;
    if written == MAX_BINARY {
        ensure!(
            entry.read(&mut [0])? == 0,
            "{BINARY} is larger than 512 MiB"
        );
    }
    ensure!(
        written == size,
        "the archive's {BINARY} size does not match its header"
    );
    Ok(())
}

#[cfg(unix)]
fn swap(exe: &Path, staged: &Path) -> io::Result<()> {
    fs::rename(staged, exe)
}

#[cfg(windows)]
fn swap(exe: &Path, staged: &Path) -> io::Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let old = loop {
        let old = with_suffix(exe, &format!(".old-{seconds}"));
        if !old.try_exists()? {
            break old;
        }
        // Several swaps in one second can leave more than one binary in use.
        seconds += 1;
    };
    fs::rename(exe, &old)?;
    if let Err(error) = fs::rename(staged, exe) {
        if let Err(rollback) = fs::rename(&old, exe) {
            return Err(io::Error::other(format!(
                "{error}; could not restore {} ({rollback}); the previous binary is at {}",
                exe.display(),
                old.display()
            )));
        }
        return Err(error);
    }
    remove_old(&old);
    Ok(())
}

#[cfg(windows)]
fn clean_old(exe: &Path) {
    let mut prefix = exe.file_name().unwrap_or_default().to_os_string();
    prefix.push(".old-");
    let Some(dir) = exe.parent() else { return };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(prefix.as_encoded_bytes())
            && entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            remove_old(&entry.path());
        }
    }
}

#[cfg(windows)]
fn remove_old(path: &Path) {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_DELETE: u32 = 4;
    // Leave the old name in place while a reader still holds it, even with delete sharing.
    if let Ok(_unused) = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_DELETE)
        .open(path)
    {
        let _ = fs::remove_file(path);
    }
}
