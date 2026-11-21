//! Per-task git worktrees. In a folder shared with `--worktrees`, each task that may
//! edit files runs in its own worktree on a new branch (`p2p/<peer>/<task>-<time>`)
//! from the folder's current commit. Its work is committed there and returned as
//! a patch, so concurrent tasks never collide, the owner reviews and merges
//! branches, and another peer can apply the patch to check the work independently.
//! Git hooks never run (an empty hooks folder is used).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::evidence::{GitCommit, sha256_hex};
use crate::llm::policy::Workspace;
use crate::profile::INHERITED_CREDENTIAL_VARS;

/// Largest patch sent back with a result (its hash is always signed).
pub const MAX_PATCH_BYTES: usize = 1 << 20;

/// The git notes ref (`refs/notes/proofkin`) where an agent keeps, on each commit its
/// tasks made, the evidence it signed for it, so a pull request from the branch can
/// carry it (see [`crate::github::branch_proof`]).
pub const NOTES_REF: &str = "proofkin";

/// `git worktree add/remove` on one repository must not run concurrently.
static GIT_LOCK: Mutex<()> = Mutex::const_new(());

pub struct Worktree {
    repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    hooks: PathBuf,
}

async fn git(dir: &Path, hooks: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .arg("-c")
        .arg(format!("core.hooksPath={}", hooks.display()))
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for var in INHERITED_CREDENTIAL_VARS {
        cmd.env_remove(var);
    }
    let out = cmd
        .output()
        .await
        .context("running git (is it installed and on PATH?)")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A branch-safe form of `name`.
pub fn branch_part(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The paths in `git apply --numstat -z` output (both sides of a rename).
fn numstat_paths(listing: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut tokens = listing.split('\0');
    while let Some(token) = tokens.next() {
        let mut fields = token.splitn(3, '\t');
        let (Some(_), Some(_), Some(path)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if path.is_empty() {
            // A rename: the old and new paths follow.
            paths.extend(tokens.next().map(str::to_owned));
            paths.extend(tokens.next().map(str::to_owned));
        } else {
            paths.push(path.to_owned());
        }
    }
    paths
}

impl Worktree {
    /// Adds a worktree of `repo`'s current commit on new branch `branch`, under `root`.
    pub async fn create(repo: &Path, root: &Path, branch: &str) -> Result<Self> {
        let _lock = GIT_LOCK.lock().await;
        let hooks = root.join(".no-hooks");
        std::fs::create_dir_all(&hooks).with_context(|| format!("creating {}", hooks.display()))?;
        let base = git(repo, &hooks, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_owned();
        let path = root.join(branch.replace('/', "-"));
        let path_arg = path.to_string_lossy().into_owned();
        git(
            repo,
            &hooks,
            &["worktree", "add", "-b", branch, &path_arg, &base],
        )
        .await?;
        Ok(Self {
            repo: repo.to_owned(),
            path,
            branch: branch.to_owned(),
            base,
            hooks,
        })
    }

    /// Applies `patch` to the worktree's files. Refused if it would create a symlink
    /// or write any file a task's model may not write (credentials, git hooks and
    /// config, editor tasks, agent settings), so a peer can't plant them by patch.
    pub async fn apply(&self, patch: &str) -> Result<()> {
        if patch.lines().any(|l| {
            let l = l.trim_end();
            (l.starts_with("new file mode ") || l.starts_with("new mode ")) && l.ends_with("120000")
        }) {
            bail!("the patch creates a symbolic link, which tasks may not do");
        }
        let file = self.path.with_extension("incoming.patch");
        std::fs::write(&file, patch)?;
        let file_arg = file.to_string_lossy().into_owned();
        let result = async {
            let listing = git(
                &self.path,
                &self.hooks,
                &["apply", "--numstat", "-z", &file_arg],
            )
            .await
            .context("reading the patch from the earlier step")?;
            let workspace = Workspace::new(&self.path)?;
            for path in numstat_paths(&listing) {
                if let Some(reason) = workspace.check_write(&path) {
                    bail!("the patch may not change {path}: {reason}");
                }
            }
            git(
                &self.path,
                &self.hooks,
                &["apply", "--whitespace=nowarn", &file_arg],
            )
            .await
            .context("applying the patch from the earlier step")
        }
        .await;
        let _ = std::fs::remove_file(&file);
        result.map(drop)
    }

    /// Commits everything the task changed; returns the commit record and the patch
    /// (None if the task changed nothing or the patch is too large to send).
    pub async fn commit(&self, message: &str) -> Result<(GitCommit, Option<String>)> {
        let mut record = GitCommit {
            branch: self.branch.clone(),
            base: self.base.clone(),
            commit: None,
            files: Vec::new(),
            patch_sha256: None,
        };
        git(&self.path, &self.hooks, &["add", "-A"]).await?;
        if git(&self.path, &self.hooks, &["status", "--porcelain"])
            .await?
            .trim()
            .is_empty()
        {
            return Ok((record, None));
        }
        git(
            &self.path,
            &self.hooks,
            &[
                "-c",
                "user.name=proofkin",
                "-c",
                "user.email=proofkin@localhost",
                "commit",
                "--no-verify",
                "-q",
                "-m",
                message,
            ],
        )
        .await?;
        let commit = git(&self.path, &self.hooks, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_owned();
        let range = format!("{}..{commit}", self.base);
        record.files = git(&self.path, &self.hooks, &["diff", "--name-only", &range])
            .await?
            .lines()
            .map(str::to_owned)
            .collect();
        let patch = git(&self.path, &self.hooks, &["diff", "--binary", &range]).await?;
        record.patch_sha256 = Some(sha256_hex(patch.as_bytes()));
        record.commit = Some(commit);
        Ok((record, (patch.len() <= MAX_PATCH_BYTES).then_some(patch)))
    }

    /// Attaches `text` to `commit` as a note under [`NOTES_REF`].
    pub async fn note(&self, commit: &str, text: &str) -> Result<()> {
        let _lock = GIT_LOCK.lock().await;
        let file = self.path.with_extension("note");
        std::fs::write(&file, text)?;
        let file_arg = file.to_string_lossy().into_owned();
        let notes_ref = format!("--ref={NOTES_REF}");
        let result = git(
            &self.path,
            &self.hooks,
            &[
                "-c",
                "user.name=proofkin",
                "-c",
                "user.email=proofkin@localhost",
                "notes",
                &notes_ref,
                "add",
                "-f",
                "-F",
                &file_arg,
                commit,
            ],
        )
        .await;
        let _ = std::fs::remove_file(&file);
        result.map(drop)
    }
}

/// The note under [`NOTES_REF`] on commit `rev` of `repo`, if it has one.
pub async fn read_note(repo: &Path, rev: &str) -> Result<Option<String>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args(["notes", &format!("--ref={NOTES_REF}"), "show", rev])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for var in INHERITED_CREDENTIAL_VARS {
        cmd.env_remove(var);
    }
    let out = cmd.output().await.context("running git")?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned()))
}

impl Drop for Worktree {
    /// Removes the worktree folder; the branch (and its commit) stays in the repository.
    fn drop(&mut self) {
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        std::fs::create_dir(&hooks).unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &hooks, &["init", "-q"]).await.unwrap();
        git(dir.path(), &hooks, &["config", "core.autocrlf", "false"])
            .await
            .unwrap();
        git(dir.path(), &hooks, &["add", "-A"]).await.unwrap();
        git(
            dir.path(),
            &hooks,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        )
        .await
        .unwrap();
        dir
    }

    #[tokio::test]
    async fn tasks_commit_on_their_own_branch_and_patches_apply_elsewhere() {
        let repo = repo().await;
        let root = tempfile::tempdir().unwrap();
        let wt = Worktree::create(repo.path(), root.path(), "p2p/boss/1-1")
            .await
            .unwrap();
        assert!(wt.path.join("a.txt").is_file());
        std::fs::write(wt.path.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(wt.path.join("b.txt"), "new\n").unwrap();
        let (record, patch) = wt.commit("task 1").await.unwrap();
        let patch = patch.unwrap();
        assert_eq!(record.files, vec!["a.txt", "b.txt"]);
        assert_eq!(
            record.patch_sha256.as_deref(),
            Some(sha256_hex(patch.as_bytes()).as_str())
        );
        let path = wt.path.clone();
        drop(wt);
        assert!(!path.exists(), "the worktree folder is removed");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
            "one\n",
            "the shared folder itself is untouched"
        );
        let hooks = root.path().join(".no-hooks");
        let branches = git(repo.path(), &hooks, &["branch", "--list", "p2p/*"])
            .await
            .unwrap();
        assert!(
            branches.contains("p2p/boss/1-1"),
            "the branch stays: {branches}"
        );

        // Another task starts from the same base and applies the patch.
        let other = Worktree::create(repo.path(), root.path(), "p2p/rev/2-1")
            .await
            .unwrap();
        other.apply(&patch).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(other.path.join("b.txt")).unwrap(),
            "new\n"
        );
        let (unchanged, _) = Worktree::create(repo.path(), root.path(), "p2p/x/3-1")
            .await
            .unwrap()
            .commit("nothing")
            .await
            .unwrap();
        assert_eq!(unchanged.commit, None);
        assert!(other.apply("not a patch").await.is_err());

        // A patch can't plant what the model itself may not write.
        let planted = "diff --git a/.vscode/tasks.json b/.vscode/tasks.json\n\
                       new file mode 100644\n--- /dev/null\n+++ b/.vscode/tasks.json\n\
                       @@ -0,0 +1 @@\n+{}\n";
        let err = other.apply(planted).await.unwrap_err();
        assert!(format!("{err:#}").contains(".vscode/tasks.json"), "{err:#}");
        assert!(!other.path.join(".vscode").exists());
        let link = "diff --git a/link b/link\nnew file mode 120000\n--- /dev/null\n+++ b/link\n\
                    @@ -0,0 +1 @@\n+/etc/passwd\n\\ No newline at end of file\n";
        let err = other.apply(link).await.unwrap_err();
        assert!(format!("{err:#}").contains("symbolic link"), "{err:#}");
        assert_eq!(numstat_paths("1\t0\ta.txt\0"), ["a.txt"]);
        assert_eq!(
            numstat_paths("0\t0\t\0old.rs\0new.rs\0"),
            ["old.rs", "new.rs"]
        );
    }

    #[tokio::test]
    async fn a_commits_signed_record_travels_with_it_into_its_pull_request() {
        use crate::evidence::{CheckResult, Evidence, EvidenceKind};
        let repo = repo().await;
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let profile = crate::Profile::open_in(home.path(), "alice").unwrap();
        let key = iroh::SecretKey::generate();
        let wt = Worktree::create(repo.path(), root.path(), "p2p/boss/7-1")
            .await
            .unwrap();
        std::fs::write(wt.path.join("a.txt"), "one\nfixed\n").unwrap();
        let (record, _) = wt.commit("task 7").await.unwrap();
        let commit = record.commit.clone().unwrap();
        let mut e = Evidence::new(EvidenceKind::Task, "worker", key.public(), key.public(), 7);
        e.attempts = 1;
        e.checks.push(CheckResult {
            name: "test".into(),
            command: "cargo test --secret-flag".into(),
            exit_code: Some(0),
            passed: true,
            millis: 2100,
            output: "ok".into(),
        });
        e.git = Some(record);
        let e = e.sign("fixed", &key);
        wt.note(&commit, &serde_json::to_string(&e).unwrap())
            .await
            .unwrap();
        drop(wt);

        let proof = crate::github::branch_proof(repo.path(), "p2p/boss/7-1", &profile, true)
            .await
            .unwrap()
            .expect("the head commit has its record");
        assert!(
            proof.summary.starts_with(&format!(
                "Signed by worker for {}: ✓ test",
                &key.public().to_string()[..10]
            )),
            "{}",
            proof.summary
        );
        let md = &proof.markdown;
        assert!(
            md.contains(&format!("Commit `{}` (1 file)", &commit[..12])),
            "{md}"
        );
        assert!(md.contains(&key.public().to_string()), "{md}");
        assert!(md.contains("- ✅ `test`: exit 0 in 2.1s"), "{md}");
        assert!(
            !md.contains("cargo test"),
            "check commands stay private: {md}"
        );
        // The requester's evidence run with the record adds what it proves.
        let task = crate::llm::LlmTask::new(crate::llm::Provider::Copilot, "Fix it")
            .with_checks(vec!["test".into()], None);
        let delivered = crate::agent::Delivered {
            ok: true,
            output: "fixed".into(),
            evidence: Some(e.clone()),
            patch: None,
            verified: true,
            refused: false,
        };
        let mut run = crate::runs::RunRecord::for_task(
            "boss",
            &key.public().to_string(),
            "worker",
            &task,
            delivered,
            0,
        );
        run.peer_ids
            .insert("worker".into(), key.public().to_string());
        run.save(&profile.runs_dir()).unwrap();
        let proof = crate::github::branch_proof(repo.path(), "p2p/boss/7-1", &profile, true)
            .await
            .unwrap()
            .unwrap();
        assert!(
            proof.summary.ends_with(&format!(
                "; evidence run {}: PASS: 1/1 claims proven, 1/1 steps done",
                run.id
            )),
            "{}",
            proof.summary
        );
        assert!(
            proof
                .markdown
                .contains("| T-1 | test passes after: Fix it | ✅ proven |"),
            "{}",
            proof.markdown
        );

        // A commit without a record carries no proof.
        let wt = Worktree::create(repo.path(), root.path(), "p2p/boss/8-1")
            .await
            .unwrap();
        std::fs::write(wt.path.join("c.txt"), "unsigned\n").unwrap();
        let (other, _) = wt.commit("task 8").await.unwrap();
        drop(wt);
        let proof = |branch: &'static str| {
            let (repo, profile) = (repo.path().to_owned(), profile.clone());
            async move { crate::github::branch_proof(&repo, branch, &profile, true).await }
        };
        assert_eq!(proof("p2p/boss/8-1").await.unwrap(), None);
        // A record copied to another commit doesn't cover it.
        let hooks = root.path().join(".no-hooks");
        let head = other.commit.unwrap();
        let notes = |args: Vec<String>| {
            let (repo, hooks) = (repo.path().to_owned(), hooks.clone());
            async move {
                let mut all = vec![
                    "-c".to_owned(),
                    "user.name=t".into(),
                    "-c".into(),
                    "user.email=t@t".into(),
                    "notes".into(),
                    "--ref=proofkin".into(),
                ];
                all.extend(args);
                let all: Vec<&str> = all.iter().map(String::as_str).collect();
                git(&repo, &hooks, &all).await.unwrap();
            }
        };
        notes(vec!["copy".into(), commit.clone(), head.clone()]).await;
        let err = proof("p2p/boss/8-1").await.unwrap_err();
        assert!(format!("{err:#}").contains("another commit"), "{err:#}");
        // A record edited to name that commit fails its signature check.
        let mut forged = e.clone();
        if let Some(g) = forged.git.as_mut() {
            g.commit = Some(head.clone());
        }
        let forged = serde_json::to_string(&forged).unwrap();
        notes(vec!["add".into(), "-f".into(), "-m".into(), forged, head]).await;
        let err = proof("p2p/boss/8-1").await.unwrap_err();
        assert!(format!("{err:#}").contains("doesn't verify"), "{err:#}");
        assert!(proof("main").await.is_err(), "only task branches");
    }
}
