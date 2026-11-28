//! `proofkin plan draft` end to end: the binary, against a profiles home of its own whose
//! profile is signed in to Claude, with a stand-in bridge (tests/fixtures/plan_bridge.mjs)
//! as its model.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const EXE: &str = env!("CARGO_BIN_EXE_proofkin");

struct Drafted {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// `proofkin plan draft <args>` for profile planner in `home`, on Claude.
fn draft(home: &Path, args: &[&str]) -> Drafted {
    let bridge = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plan_bridge.mjs");
    let out = Command::new(EXE)
        .env("PROOFKIN_HOME", home)
        .env("PROOFKIN_SECRET_STORE", "file")
        .env("PROOFKIN_BRIDGE", bridge)
        .stdin(Stdio::null())
        .args(["plan", "draft"])
        .args(args)
        .args(["--using", "claude", "--wait", "0", "--profile", "planner"])
        .output()
        .unwrap();
    Drafted {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn planner_profile(home: &Path) -> PathBuf {
    let dir = home.join("profiles").join("planner");
    std::fs::create_dir_all(dir.join("secrets")).unwrap();
    std::fs::write(
        dir.join("profile.json"),
        r#"{
            "claude": {"source": "api-key"},
            "models": {"claude": {
                "allowed": ["fake-planner", "claude-sonnet-5.5"], "efforts": ["low", "medium"]
            }}
        }"#,
    )
    .unwrap();
    std::fs::write(dir.join("secrets").join("claude"), "sk-ant-test-key").unwrap();
    dir
}

/// The profile's model writes the plan: `plan draft` prints it (the prose around its fenced
/// block dropped), then its problems, and exits 1 while errors remain; `--out` writes it to
/// a file, and `--from` with `--note` has it changed. Each draft is in the audit, and none
/// starts a task or a run.
#[test]
fn plan_draft_prints_the_plan_then_its_problems_and_fails_while_errors_remain() {
    let home = tempfile::tempdir().unwrap();
    let dir = planner_profile(home.path());
    let ok = draft(home.path(), &["Ship the release notes"]);
    assert_eq!(ok.code, Some(0), "{}{}", ok.stdout, ok.stderr);
    assert_eq!(
        ok.stdout,
        "// Drafted by claude (fake-planner): Ship the release notes\nok approve @me Ship it?\n"
    );
    assert!(ok.stderr.contains("is writing the plan"), "{}", ok.stderr);
    assert!(!ok.stderr.contains("error:"), "{}", ok.stderr);
    assert!(
        !ok.stderr.contains("The draft doesn't track"),
        "{}",
        ok.stderr
    );

    let bad = draft(home.path(), &["Fix the bug on ghost's app"]);
    assert_eq!(bad.code, Some(1), "{}{}", bad.stdout, bad.stderr);
    assert_eq!(
        bad.stdout,
        "// Drafted by claude (fake-planner): Fix the bug on ghost's app\nfix @ghost:app Fix the bug\n"
    );
    assert!(bad.stderr.contains("try 3 of 3: fixing"), "{}", bad.stderr);
    assert!(
        bad.stderr
            .contains("error: step fix: ghost is not connected"),
        "{}",
        bad.stderr
    );

    let file = home.path().join("notes.plan");
    let file_arg = file.to_str().unwrap();
    let out = draft(home.path(), &["Ship the release notes", "--out", file_arg]);
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, "");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), ok.stdout);
    let changed = draft(
        home.path(),
        &[
            "--from",
            file_arg,
            "--note",
            "add a second sign-off after it",
            "--out",
            file_arg,
        ],
    );
    assert_eq!(changed.code, Some(0), "{}", changed.stderr);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "// Drafted by claude (fake-planner): Ship the release notes\nok approve @me Ship it?\n\
         again after ok approve @me Ship it, really?\n"
    );

    let none = draft(home.path(), &[]);
    assert_ne!(none.code, Some(0));
    assert!(
        none.stderr.contains("say what the plan should do"),
        "{}",
        none.stderr
    );

    let audit = std::fs::read_to_string(dir.join("audit.jsonl")).unwrap();
    let drafts: Vec<serde_json::Value> = audit
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|e: &serde_json::Value| e["origin"] == "a plan draft")
        .collect();
    assert_eq!(drafts.len(), 4, "{audit}");
    assert!(!audit.contains("Ship the release notes"), "{audit}");
    assert!(!dir.join("tasks.json").exists(), "drafting starts no task");
    assert!(
        !dir.join("runs").exists()
            || std::fs::read_dir(dir.join("runs"))
                .unwrap()
                .next()
                .is_none(),
        "drafting starts no run"
    );
}

#[test]
fn plan_draft_prints_the_untracked_notice_without_changing_the_plan() {
    let home = tempfile::tempdir().unwrap();
    planner_profile(home.path());
    let out = draft(home.path(), &["Keep this run off GitHub"]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert_eq!(
        out.stdout,
        "// Drafted by claude (fake-planner): Keep this run off GitHub\n\
         // Not tracked: the owner asked.\ntrack: off\nok approve @me Ship it?\n"
    );
    let notice = "The draft doesn't track the run on GitHub (track: off): no issues, and no \
                  results there. To track it, pick New issue under Track on GitHub.";
    assert!(
        out.stderr.lines().any(|line| line == notice),
        "{}",
        out.stderr
    );
}

#[test]
fn plan_draft_passes_the_model_and_effort_flags_to_the_bridge_and_refuses_bad_values() {
    let home = tempfile::tempdir().unwrap();
    let dir = planner_profile(home.path());
    let goal = "Ship the release notes";
    let ok = draft(
        home.path(),
        &[goal, "--model", "claude-sonnet-5.5", "--effort", "medium"],
    );
    assert_eq!(ok.code, Some(0), "{}{}", ok.stdout, ok.stderr);
    assert_eq!(
        ok.stdout.lines().next(),
        Some("// Drafted by claude (claude-sonnet-5.5): Ship the release notes")
    );
    let audit = proofkin::audit::AuditLog::read(&dir.join("audit.jsonl"), 10).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(
        audit[0].model.as_deref(),
        Some("claude-sonnet-5.5 (medium reasoning)")
    );

    for (flag, value, message) in [
        (
            "--model",
            "not-allowed",
            "not-allowed isn't a model allowed here: fake-planner, claude-sonnet-5.5",
        ),
        ("--effort", "turbo", "isn't a reasoning effort: use one of"),
    ] {
        // Even when an issue is named, the choice is refused before looking it up.
        let refused = draft(home.path(), &["--issue", "42", flag, value]);
        assert_ne!(refused.code, Some(0));
        assert!(refused.stderr.contains(message), "{}", refused.stderr);
    }
}
