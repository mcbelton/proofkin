//! Linked repositories: clone with the profile's GitHub account (token only in the
//! environment), share with a worktree per task, sync, push task branches and open
//! pull requests; and the tracker's GraphQL requests. A local bare repository stands
//! in for GitHub, and a fake `gh` records how the pull request and the requests were
//! made.

use std::path::Path;
use std::process::Command;

use proofkin::audit::{AuditEntry, AuditLog, Outcome};
use proofkin::evidence::{Evidence, EvidenceKind};
use proofkin::github::{
    CloneUpdate, ClosingPr, FETCH_AGE, FileChange, GithubCredentials, RepoStore, branch_changes,
    branch_states, link_url, task_branches, unlink,
};
use proofkin::worktree::Worktree;
use proofkin::{Access, Profile, Provider, TrustStore, TrustedPeer};

/// GitHub's GraphQL API answering `answer` to anything, keeping the last query.
struct AnswerApi {
    answer: serde_json::Value,
    asked: std::sync::Mutex<String>,
}

#[async_trait::async_trait]
impl proofkin::tracker::Api for AnswerApi {
    async fn graphql(
        &self,
        query: &str,
        _variables: serde_json::Value,
    ) -> anyhow::Result<proofkin::tracker::Reply> {
        *self.asked.lock().unwrap() = query.to_owned();
        Ok(proofkin::tracker::Reply::from_value(self.answer.clone()))
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn commit(dir: &Path, file: &str, text: &str, message: &str) {
    std::fs::write(dir.join(file), text).unwrap();
    git(dir, &["add", "-A"]);
    git(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

#[tokio::test]
async fn linked_repositories_clone_share_sync_push_and_open_pull_requests() {
    let tmp = tempfile::tempdir().unwrap();
    // "GitHub": a bare repository with one commit on main.
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "--initial-branch=main"]);
    let seed = tmp.path().join("seed");
    Command::new("git")
        .args(["clone", "-q"])
        .arg(&origin)
        .arg(&seed)
        .status()
        .unwrap();
    git(&seed, &["config", "core.autocrlf", "false"]);
    git(&seed, &["checkout", "-q", "-b", "main"]);
    commit(&seed, "README.md", "app\n", "init");
    git(&seed, &["push", "-q", "origin", "main"]);

    let profile = Profile::open_in(&tmp.path().join("home/profiles"), "alice").unwrap();
    let creds = GithubCredentials {
        user: "alice".into(),
        host: "github.com".into(),
        token: "t0k3n-never-stored".into(),
    };
    let url = origin.to_string_lossy().into_owned();
    let checks = [("test".to_owned(), "exit 0".to_owned())].into();
    let (repo, shares) = link_url(
        &profile,
        &creds,
        &url,
        "github.com",
        "acme/app",
        None,
        Access::Full,
        checks,
    )
    .await
    .unwrap();
    assert_eq!((repo.name.as_str(), repo.branch.as_str()), ("app", "main"));
    assert!(
        repo.path.starts_with(profile.repos_dir()),
        "cloned outside the profiles folder"
    );
    let share = shares.get("app").unwrap();
    assert!(
        share.worktrees && share.checks.contains_key("test") && share.max_access == Access::Full
    );
    assert_eq!(
        profile.shares().unwrap().get("app").unwrap().path,
        repo.path,
        "saved"
    );
    assert_eq!(
        RepoStore::load(&profile.repos_path())
            .unwrap()
            .get("app")
            .unwrap(),
        &repo
    );
    let config = std::fs::read_to_string(repo.path.join(".git/config")).unwrap();
    assert!(
        !config.contains("t0k3n"),
        "the token is never stored in the clone"
    );
    let again = link_url(
        &profile,
        &creds,
        &url,
        "github.com",
        "acme/app",
        None,
        Access::Full,
        Default::default(),
    )
    .await;
    assert!(again.unwrap_err().to_string().contains("already linked"));

    // An agent's task commits on its own branch (as in a --worktrees folder).
    let root = tmp.path().join("worktrees");
    let started = std::time::SystemTime::now();
    let wt = Worktree::create(&repo.path, &root, "p2p/boss/1-1")
        .await
        .unwrap();
    std::fs::write(wt.path.join("feature.txt"), "new\n").unwrap();
    let (made, _) = wt
        .commit("proofkin: task 1 from boss\n\nAdd the feature")
        .await
        .unwrap();
    // The agent signed a record for it and kept it as a note; its audit log has the task.
    let key = proofkin::SecretKey::generate();
    let mut record = Evidence::new(EvidenceKind::Task, "worker", key.public(), key.public(), 1);
    record.git = Some(made.clone());
    let record = record.sign("done", &key);
    wt.note(
        made.commit.as_deref().unwrap(),
        &serde_json::to_string(&record).unwrap(),
    )
    .await
    .unwrap();
    drop(wt);
    TrustStore {
        peers: vec![TrustedPeer {
            name: "boss".into(),
            id: key.public(),
            addrs: Default::default(),
            can_delegate: true,
            tasks_per_hour: 20,
            access: Access::Full,
            shares: vec![],
            m365: None,
            github: None,
        }],
    }
    .save(&profile.trust_path())
    .unwrap();
    let ms = |t: std::time::SystemTime| {
        t.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
    };
    AuditLog::new(profile.audit_path()).record(&AuditEntry {
        ts_ms: ms(started),
        peer: "boss".into(),
        peer_id: key.public().to_string(),
        task_id: 1,
        provider: Provider::Copilot,
        share: "app".into(),
        access: Some(Access::Full),
        outcome: Outcome::Done,
        duration_ms: 5,
        prompt_sha256: String::new(),
        prompt_chars: 0,
        error: None,
        origin: Some("step ui of run 20261121-183724-726e7".into()),
        team: false,
        model: None,
        usage: None,
    });
    let branches = task_branches(&repo).await.unwrap();
    assert_eq!(branches[0].name, "p2p/boss/1-1");
    assert!(
        branches[0]
            .subject
            .starts_with("proofkin: task 1 from boss")
    );

    creds.push(&repo, "p2p/boss/1-1").await.unwrap();
    assert!(git(&seed, &["ls-remote", "origin", "refs/heads/p2p/*"]).contains("p2p/boss/1-1"));
    assert!(
        creds.push(&repo, "main").await.is_err(),
        "only task branches are pushed"
    );

    // Opening a pull request pushes the notes ref with the record, and the proof says
    // who asked, what it was part of and where the record is.
    assert_eq!(git(&seed, &["ls-remote", "origin", "refs/notes/*"]), "");
    // Another task commit in the clone, whose branch gets no pull request, has a record too.
    let other = Worktree::create(&repo.path, &root, "p2p/boss/2-1")
        .await
        .unwrap();
    std::fs::write(other.path.join("other.txt"), "other\n").unwrap();
    let (other_made, _) = other
        .commit("proofkin: task 2 from boss\n\nAnother change\n\nRefs: acme/app#5")
        .await
        .unwrap();
    other
        .note(other_made.commit.as_deref().unwrap(), "{\"other\":true}")
        .await
        .unwrap();
    drop(other);

    // Where each task branch's work is, in one GraphQL query: 1-1 has a merged pull
    // request (its newest), 2-1's issue is closed, and 3-1's commit is on main already.
    git(&repo.path, &["branch", "p2p/boss/3-1", "main"]);
    let one_head = git(&repo.path, &["rev-parse", "p2p/boss/1-1"]);
    let fake = AnswerApi {
        answer: serde_json::json!({ "data": { "repository": {
            "pullRequests": { "nodes": [
                { "number": 7, "state": "MERGED", "url": "https://github.com/acme/app/pull/7", "headRefName": "p2p/boss/1-1", "headRefOid": one_head },
                { "number": 3, "state": "CLOSED", "url": "https://github.com/acme/app/pull/3", "headRefName": "p2p/boss/1-1" },
            ] },
            "i5": { "state": "CLOSED", "timelineItems": { "nodes": [{ "closer": {
                "number": 11, "url": "https://github.com/acme/app/pull/11", "headRefName": "p2p/boss/9-1" } }] } },
        } } }),
        asked: Default::default(),
    };
    let branches = task_branches(&repo).await.unwrap();
    let states = branch_states(&fake, &repo, &branches).await.unwrap();
    let merged = states["p2p/boss/1-1"].pull_request.as_ref().unwrap();
    assert_eq!((merged.number, merged.state.as_str()), (7, "MERGED"));
    assert!(!states["p2p/boss/1-1"].in_base && !states["p2p/boss/1-1"].issues_closed);
    assert!(states["p2p/boss/2-1"].issues_closed && states["p2p/boss/2-1"].pull_request.is_none());
    assert!(states["p2p/boss/3-1"].in_base);
    // Each says what deleting it would lose: 1-1's pull request merged, so nothing; 2-1's
    // issue is closed, but its change isn't on main; 3-1 is on main.
    let one = &states["p2p/boss/1-1"];
    assert_eq!(
        (
            one.ahead,
            one.files,
            one.added,
            one.removed,
            one.same_as_base
        ),
        (1, 1, 1, 0, false)
    );
    assert!(one.safe_to_delete, "its pull request merged it as it is");
    assert!(one.pull_request.as_ref().unwrap().current);
    let two = &states["p2p/boss/2-1"];
    assert_eq!((two.ahead, two.files, two.same_as_base), (1, 1, false));
    assert!(
        !two.safe_to_delete,
        "its issue is closed, but its change isn't on main"
    );
    // What closed its issue: a pull request from another branch, which may have its work.
    assert_eq!(
        two.closed_by,
        [ClosingPr {
            number: 11,
            url: "https://github.com/acme/app/pull/11".into(),
            branch: "p2p/boss/9-1".into(),
        }]
    );
    let three = &states["p2p/boss/3-1"];
    assert!(three.ahead == 0 && three.same_as_base && three.safe_to_delete);
    let shown = serde_json::to_value(one).unwrap();
    assert_eq!(shown["safe_to_delete"], true);
    assert!(shown.get("head").is_none() && shown["pull_request"].get("head").is_none());
    // The changes a pull request from 2-1 would carry, to review first.
    let changes = branch_changes(&repo, "p2p/boss/2-1").await.unwrap();
    assert_eq!(changes.commits.len(), 1);
    assert!(
        changes.commits[0]
            .subject
            .starts_with("proofkin: task 2 from boss")
    );
    assert_eq!(
        changes.files,
        [FileChange {
            path: "other.txt".into(),
            added: Some(1),
            removed: Some(0),
        }]
    );
    assert!(
        changes.diff.contains("+++ b/other.txt\n") && changes.diff.contains("\n+other\n"),
        "{}",
        changes.diff
    );
    assert_eq!((changes.base.as_str(), changes.truncated), ("main", false));
    assert!(
        branch_changes(&repo, "main").await.is_err(),
        "task branches only"
    );
    assert!(branch_changes(&repo, "p2p/boss/nope").await.is_err());
    // A diff too big to show is cut at a line's end.
    let big = Worktree::create(&repo.path, &root, "p2p/boss/5-1")
        .await
        .unwrap();
    let text: String = (0..40_000).map(|n| format!("line {n}\n")).collect();
    std::fs::write(big.path.join("big.txt"), text).unwrap();
    big.commit("proofkin: task 5 from boss\n\nA big file")
        .await
        .unwrap();
    drop(big);
    let changes = branch_changes(&repo, "p2p/boss/5-1").await.unwrap();
    assert!(changes.truncated && changes.diff.len() <= proofkin::github::MAX_DIFF_BYTES);
    assert!(changes.diff.ends_with('\n'));
    assert_eq!(changes.files[0].added, Some(40_000));
    // One whose change reached main another way (the same content, in another commit)
    // loses nothing either. The states are checked against main as the repository has it
    // now: the clone fetches it when its last fetch is a few minutes old.
    let same = Worktree::create(&repo.path, &root, "p2p/boss/4-1")
        .await
        .unwrap();
    std::fs::write(same.path.join("same.txt"), "same\n").unwrap();
    same.commit("proofkin: task 4 from boss\n\nThe same change")
        .await
        .unwrap();
    drop(same);
    commit(&seed, "same.txt", "same\n", "the same change, made on main");
    git(&seed, &["push", "-q", "origin", "main"]);
    let fresh = std::time::Duration::from_secs(300);
    assert!(
        creds.refresh_base(&repo, fresh).await.unwrap(),
        "never fetched since it was cloned"
    );
    assert!(
        !creds.refresh_base(&repo, fresh).await.unwrap(),
        "fetched just now"
    );
    let branches = task_branches(&repo).await.unwrap();
    let states = branch_states(&fake, &repo, &branches).await.unwrap();
    let four = &states["p2p/boss/4-1"];
    assert!(
        !four.in_base && four.ahead == 1 && four.same_as_base && four.safe_to_delete,
        "{four:?}"
    );
    // A merged pull request loses nothing only if it had the branch as it is: 5-1's had
    // it (and a commit pushed after it from elsewhere), but 2-1's merged without its commit.
    let after = git(
        &repo.path,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit-tree",
            "p2p/boss/5-1^{tree}",
            "-p",
            "p2p/boss/5-1",
            "-m",
            "pushed from elsewhere",
        ],
    );
    let main_head = git(&repo.path, &["rev-parse", "origin/main"]);
    let later = AnswerApi {
        answer: serde_json::json!({ "data": { "repository": {
            "pullRequests": { "nodes": [
                { "number": 9, "state": "MERGED", "url": "https://github.com/acme/app/pull/9", "headRefName": "p2p/boss/5-1", "headRefOid": after },
                { "number": 11, "state": "MERGED", "url": "https://github.com/acme/app/pull/11", "headRefName": "p2p/boss/2-1", "headRefOid": main_head },
            ] },
            "i5": { "state": "CLOSED", "timelineItems": { "nodes": [{ "closer": {
                "number": 11, "url": "https://github.com/acme/app/pull/11", "headRefName": "p2p/boss/2-1" } }] } },
        } } }),
        asked: Default::default(),
    };
    let states = branch_states(&later, &repo, &branches).await.unwrap();
    let five = &states["p2p/boss/5-1"];
    assert!(five.pull_request.as_ref().unwrap().current && five.safe_to_delete);
    assert!(!five.same_as_base, "only the pull request has its work");
    let two = &states["p2p/boss/2-1"];
    assert!(
        !two.pull_request.as_ref().unwrap().current && !two.safe_to_delete,
        "{two:?}"
    );
    assert!(
        two.closed_by.is_empty(),
        "its own pull request closed its issue"
    );
    let asked = fake.asked.lock().unwrap().clone();
    assert!(
        asked.contains("i5: issueOrPullRequest(number: 5)") && asked.contains("CLOSED_EVENT"),
        "{asked}"
    );
    // A finished one is deleted in the clone and on GitHub; only task branches are.
    creds.push(&repo, "p2p/boss/3-1").await.unwrap();
    creds.delete_branch(&repo, "p2p/boss/3-1").await.unwrap();
    assert!(!git(&seed, &["ls-remote", "origin", "refs/heads/p2p/*"]).contains("p2p/boss/3-1"));
    assert!(
        !task_branches(&repo)
            .await
            .unwrap()
            .iter()
            .any(|b| b.name == "p2p/boss/3-1")
    );
    assert!(creds.delete_branch(&repo, "main").await.is_err());
    let proof = creds
        .pr_proof(&repo, "p2p/boss/1-1", &profile)
        .await
        .unwrap()
        .expect("the commit has its record");
    assert!(
        git(&seed, &["ls-remote", "origin", "refs/notes/proofkin"]).contains("refs/notes/proofkin")
    );
    let md = &proof.markdown;
    assert!(md.contains("for **boss** ("), "{md}");
    assert!(
        md.contains("as step ui of boss's evidence run 20261121-183724-726e7"),
        "{md}"
    );
    assert!(
        md.contains("git fetch origin refs/notes/proofkin:refs/notes/proofkin"),
        "{md}"
    );
    assert!(
        proof.summary.contains("Signed by worker for boss ("),
        "{}",
        proof.summary
    );
    assert!(!proof.summary.contains("stays on this agent's machine"));
    // Someone fetching the notes gets a record that verifies.
    let fetched = tmp.path().join("fetched");
    Command::new("git")
        .args(["clone", "-q"])
        .arg(&origin)
        .arg(&fetched)
        .status()
        .unwrap();
    git(
        &fetched,
        &[
            "fetch",
            "-q",
            "origin",
            "refs/notes/proofkin:refs/notes/proofkin",
        ],
    );
    let note = git(
        &fetched,
        &[
            "notes",
            "--ref=proofkin",
            "show",
            made.commit.as_deref().unwrap(),
        ],
    );
    let note: Evidence = serde_json::from_str(&note).unwrap();
    note.verify_record().unwrap();
    assert_eq!(
        git(&fetched, &["notes", "--ref=proofkin", "list"])
            .lines()
            .count(),
        1,
        "only the pull request's record was published"
    );
    // If the notes can't be pushed, the proof still comes and says the record stays here.
    git(
        &repo.path,
        &["remote", "set-url", "origin", "/no/such/repository.git"],
    );
    assert!(creds.push_notes(&repo, "p2p/boss/1-1").await.is_err());
    let proof = creds
        .pr_proof(&repo, "p2p/boss/1-1", &profile)
        .await
        .unwrap()
        .expect("the proof doesn't need the push");
    assert!(
        proof.markdown.contains("stays on this agent's machine")
            && !proof.markdown.contains("git fetch"),
        "{}",
        proof.markdown
    );
    assert!(
        proof
            .summary
            .contains("the record stays on this agent's machine")
    );
    git(&repo.path, &["remote", "set-url", "origin", &url]);

    // Someone else pushes to main; sync fast-forwards the clone.
    git(&seed, &["pull", "-q", "origin", "main"]);
    commit(&seed, "CHANGELOG.md", "v2\n", "release v2");
    git(&seed, &["push", "-q", "origin", "main"]);
    let head = creds.sync(&repo).await.unwrap();
    assert_eq!(head, git(&seed, &["rev-parse", "--short", "HEAD"]));
    assert!(repo.path.join("CHANGELOG.md").is_file());

    // Before a task starts in it, the clone gets the repository's latest commit, fetching
    // it unless it fetched a moment ago (so tasks starting together fetch once)...
    let now = std::time::Duration::ZERO;
    let short = |dir: &Path| git(dir, &["rev-parse", "--short", "HEAD"]);
    assert_eq!(
        creds.update_clone(&repo, now).await.unwrap(),
        CloneUpdate::Current {
            head: short(&repo.path)
        }
    );
    commit(&seed, "a.txt", "a\n", "a");
    commit(&seed, "b.txt", "b\n", "b");
    git(&seed, &["push", "-q", "origin", "main"]);
    assert!(matches!(
        creds.update_clone(&repo, FETCH_AGE).await.unwrap(),
        CloneUpdate::Current { .. }
    ));
    assert_eq!(
        creds.update_clone(&repo, now).await.unwrap(),
        CloneUpdate::Moved {
            head: short(&repo.path),
            commits: 2
        }
    );
    assert_eq!(
        git(&repo.path, &["rev-parse", "HEAD"]),
        git(&seed, &["rev-parse", "HEAD"])
    );
    // ...unless that would lose something, or it can't: then it is left as it is.
    let kept = |why: &str| CloneUpdate::Kept {
        head: short(&repo.path),
        why: why.into(),
    };
    commit(&repo.path, "mine.txt", "mine\n", "my own commit");
    assert_eq!(
        creds.update_clone(&repo, now).await.unwrap(),
        kept("its main has 1 commit the repository's hasn't")
    );
    git(&repo.path, &["reset", "-q", "--hard", "HEAD~1"]);
    commit(&seed, "README.md", "app v3\n", "v3");
    git(&seed, &["push", "-q", "origin", "main"]);
    std::fs::write(repo.path.join("README.md"), "my edit\n").unwrap();
    assert_eq!(
        creds.update_clone(&repo, now).await.unwrap(),
        kept("the update would overwrite its local changes")
    );
    git(&repo.path, &["checkout", "-q", "--", "README.md"]);
    git(&repo.path, &["checkout", "-q", "-b", "wip"]);
    assert_eq!(
        creds.update_clone(&repo, now).await.unwrap(),
        kept("it has branch wip checked out, not main")
    );
    git(&repo.path, &["checkout", "-q", "main"]);
    git(
        &repo.path,
        &["remote", "set-url", "origin", "/no/such/repository.git"],
    );
    let CloneUpdate::Kept { why, .. } = creds.update_clone(&repo, now).await.unwrap() else {
        panic!("GitHub can't be reached: the clone stays as it is")
    };
    assert!(
        why.starts_with("couldn't fetch the repository's main (git fetch failed: "),
        "{why}"
    );
    git(&repo.path, &["remote", "set-url", "origin", &url]);
    assert_eq!(
        creds.update_clone(&repo, now).await.unwrap(),
        CloneUpdate::Moved {
            head: short(&repo.path),
            commits: 1
        }
    );

    // The pull request goes through gh with the profile's token in its environment.
    let fake = tmp.path().join(if cfg!(windows) { "gh.cmd" } else { "gh" });
    let record = tmp.path().join("gh-call.txt");
    if cfg!(windows) {
        std::fs::write(
            &fake,
            format!("@echo off\r\necho %* > \"{0}\"\r\necho token=%GH_TOKEN% host=%GH_HOST%>> \"{0}\"\r\necho https://github.com/acme/app/pull/7\r\n", record.display()),
        )
        .unwrap();
    } else {
        std::fs::write(
            &fake,
            format!("#!/bin/sh\necho \"$@\" > '{0}'\necho \"token=$GH_TOKEN host=$GH_HOST\" >> '{0}'\necho https://github.com/acme/app/pull/7\n", record.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    // SAFETY: this test binary has a single test, so nothing reads the environment concurrently.
    unsafe { std::env::set_var("PROOFKIN_GH", &fake) };
    let pr = creds
        .create_pr(&repo, "p2p/boss/1-1", "Add the feature", "Made by agents.")
        .await
        .unwrap();
    assert_eq!(pr, "https://github.com/acme/app/pull/7");
    let call = std::fs::read_to_string(&record).unwrap();
    for part in [
        "pr create",
        "--repo github.com/acme/app",
        "--base main",
        "--head p2p/boss/1-1",
        "token=t0k3n-never-stored",
        "host=github.com",
    ] {
        assert!(call.contains(part), "{part:?} in {call}");
    }

    // The tracker's GraphQL goes through gh too: the request on stdin, the token in
    // its environment (never on the command line).
    let calls = tmp.path().join("gh-graphql.txt");
    let answer = r#"{"data":{"viewer":{"id":"U1","login":"alice"},"repository":{"id":"R1","url":"https://github.com/acme/app"},"repositoryOwner":{"projectV2":{"id":"P1","title":"Tracker","url":"https://github.com/users/alice/projects/4","fields":{"nodes":[{"id":"F1","name":"Status","dataType":"SINGLE_SELECT","options":[{"id":"o1","name":"Todo"},{"id":"o2","name":"Done"}]},{"id":"F2","name":"Agent","dataType":"TEXT"}]}}}}}"#;
    if cfg!(windows) {
        std::fs::write(
            &fake,
            format!("@echo off\r\necho %*>> \"{0}\"\r\necho token=%GH_TOKEN% host=%GH_HOST%>> \"{0}\"\r\nfindstr \"^\" >> \"{0}\"\r\necho.>> \"{0}\"\r\necho {answer}\r\n", calls.display()),
        )
        .unwrap();
    } else {
        std::fs::write(
            &fake,
            format!("#!/bin/sh\necho \"$@\" >> '{0}'\necho \"token=$GH_TOKEN host=$GH_HOST\" >> '{0}'\ncat >> '{0}'\necho >> '{0}'\necho '{answer}'\n", calls.display()),
        )
        .unwrap();
    }
    let config = proofkin::tracker::TrackerConfig {
        repo: "acme/app".into(),
        host: "github.com".into(),
        project: Some(
            proofkin::tracker::parse_project(
                "https://github.com/users/alice/projects/4",
                "github.com",
            )
            .unwrap(),
        ),
        open_runs: Vec::new(),
    };
    let tracker = proofkin::tracker::Tracker::connect(
        std::sync::Arc::new(proofkin::tracker::GhApi::new(creds.clone())),
        config,
    )
    .await
    .unwrap();
    let info = tracker.info();
    assert_eq!(info.login, "alice");
    let board = info.board.expect("the board");
    assert_eq!(board.title, "Tracker");
    assert_eq!(board.statuses, ["Todo", "Done"]);
    assert!(board.agent_field && info.board_problem.is_none());
    let sent = std::fs::read_to_string(&calls).unwrap();
    for part in [
        "api graphql --hostname github.com --input -",
        "token=t0k3n-never-stored host=github.com",
        "query Repo(",
        "query Board(",
        r#""number":4"#,
    ] {
        assert!(sent.contains(part), "{part:?} in {sent}");
    }
    assert_eq!(
        sent.matches("t0k3n-never-stored").count(),
        2,
        "the token is only in gh's environment: {sent}"
    );

    let shares = unlink(&profile, "app", true).unwrap();
    assert!(shares.get("app").is_none() && !repo.path.exists());
    assert!(
        RepoStore::load(&profile.repos_path())
            .unwrap()
            .repos
            .is_empty()
    );
}
