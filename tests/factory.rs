//! `proofkin factory` end to end: the binary, against a profiles home of its own whose
//! profile alice is signed in to Codex only. Nothing here calls a model: the draft
//! module's tests cover drafting with one.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

const EXE: &str = env!("CARGO_BIN_EXE_proofkin");

/// A test editor: `fail` exits non-zero, `garble` saves text that isn't JSON, `fix`
/// drops the skills and retitles the blueprint.
const EDITOR: &str = r#"import json, sys
mode, path = sys.argv[1], sys.argv[2]
if mode == "fail":
    sys.exit(3)
if mode == "garble":
    with open(path, "w", encoding="utf-8") as f:
        f.write("not json")
    sys.exit(0)
with open(path, encoding="utf-8") as f:
    blueprint = json.load(f)
blueprint.pop("skills", None)
blueprint["title"] = "Edited in an editor"
with open(path, "w", encoding="utf-8") as f:
    json.dump(blueprint, f)
"#;

struct Home(tempfile::TempDir);

/// What a command did: whether it succeeded, and what it printed (stdout, then stderr).
struct Ran {
    ok: bool,
    out: String,
}

impl Home {
    /// A profiles home where alice is signed in to Codex.
    fn new() -> Self {
        let home = Self(tempfile::tempdir().unwrap());
        home.profile("alice", r#"{"codex": {"source": "login"}}"#);
        home
    }

    fn profile(&self, name: &str, config: &str) {
        let dir = self.path().join("profiles").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("profile.json"), config).unwrap();
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(EXE);
        command
            .arg("factory")
            .args(args)
            .env("PROOFKIN_HOME", self.path())
            .env("PROOFKIN_SECRET_STORE", "file")
            .env_remove("VISUAL")
            .env_remove("EDITOR")
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Ran {
        ran(self.command(args).output().unwrap())
    }

    fn run_with_stdin(&self, args: &[&str], input: &str) -> Ran {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        ran(child.wait_with_output().unwrap())
    }

    fn ok(&self, args: &[&str]) -> String {
        let ran = self.run(args);
        assert!(ran.ok, "factory {} failed:\n{}", args.join(" "), ran.out);
        ran.out
    }

    fn fails(&self, args: &[&str]) -> String {
        let ran = self.run(args);
        assert!(
            !ran.ok,
            "factory {} should fail:\n{}",
            args.join(" "),
            ran.out
        );
        ran.out
    }

    /// Draft `name`'s blueprint, as `show --json` prints it.
    fn blueprint(&self, name: &str) -> Value {
        serde_json::from_str(&self.ok(&["show", name, "--json", "--profile", "alice"])).unwrap()
    }
}

fn ran(output: Output) -> Ran {
    Ran {
        ok: output.status.success(),
        out: format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

/// The file a refused edit was kept in, as the message names it; removed once read.
fn kept_edit(out: &str) -> String {
    let (_, rest) = out.split_once("your edit is in ").expect(out);
    let path = rest.split(')').next().unwrap();
    let text = std::fs::read_to_string(path).expect(path);
    std::fs::remove_file(path).unwrap();
    text
}

#[test]
fn a_template_starts_a_draft_that_is_listed_shown_checked_and_deleted() {
    let home = Home::new();
    let templates = home.ok(&["templates"]);
    for name in ["code-reviewer", "implementer", "financial-analyst"] {
        assert!(templates.contains(name), "{templates}");
    }
    assert!(
        home.ok(&["list", "--profile", "alice"])
            .contains("no agents yet")
    );

    let out = home.fails(&["new", "rev", "--profile", "alice"]);
    assert!(out.contains("give --template NAME"), "{out}");
    let out = home.fails(&["new", "rev", "--template", "nope", "--profile", "alice"]);
    assert!(out.contains("there is no template 'nope'"), "{out}");

    let out = home.ok(&[
        "new",
        "rev",
        "--template",
        "code-reviewer",
        "--profile",
        "alice",
    ]);
    assert!(
        out.contains("created draft rev (drafted from template code-reviewer): no problems"),
        "{out}"
    );
    let blueprint = home.blueprint("rev");
    assert_eq!(blueprint["name"], "rev");
    assert_eq!(
        blueprint["model"]["providers"],
        json!(["codex"]),
        "only what alice is signed in to"
    );

    let shown = home.ok(&["show", "rev", "--profile", "alice"]);
    for part in [
        "rev (draft): Code reviewer",
        "asks for: read-only access; skills code-review, security-review",
        "runs on: codex",
        "written: from a template (template code-reviewer)",
        "alice  drafted from template code-reviewer",
        "problems: no problems",
    ] {
        assert!(shown.contains(part), "{part:?} isn't in:\n{shown}");
    }
    let listed = home.ok(&["list", "--profile", "alice"]);
    assert!(
        listed.contains("rev") && listed.contains("Code reviewer"),
        "{listed}"
    );
    assert!(
        home.ok(&["check", "rev", "--profile", "alice"])
            .contains("rev: no problems")
    );

    let out = home.fails(&[
        "new",
        "rev",
        "--template",
        "researcher",
        "--profile",
        "alice",
    ]);
    assert!(
        out.contains("'rev' is already another agent or draft"),
        "{out}"
    );
    let out = home.fails(&[
        "new",
        "me",
        "--template",
        "researcher",
        "--profile",
        "alice",
    ]);
    assert!(out.contains("'me' is already a reserved name"), "{out}");

    assert!(
        home.ok(&["delete", "rev", "--profile", "alice"])
            .contains("deleted rev")
    );
    assert!(
        home.ok(&["list", "--profile", "alice"])
            .contains("no agents yet")
    );
    let out = home.fails(&["delete", "rev", "--profile", "alice"]);
    assert!(out.contains("the factory has no agent 'rev'"), "{out}");
    let out = home.fails(&["show", "rev", "--profile", "alice"]);
    assert!(out.contains("the factory has no draft 'rev'"), "{out}");
}

#[test]
fn edits_are_checked_when_saved_and_kept_with_their_problems() {
    let home = Home::new();
    home.ok(&[
        "new",
        "scout",
        "--template",
        "researcher",
        "--profile",
        "alice",
    ]);

    let mut blueprint = home.blueprint("scout");
    blueprint["skills"] = json!(["no-such-skill"]);
    let file = home.path().join("scout.json");
    std::fs::write(&file, blueprint.to_string()).unwrap();
    let file = file.to_str().unwrap();
    let out = home.ok(&["edit", "scout", "--from", file, "--profile", "alice"]);
    assert!(out.contains("saved draft scout: 1 error"), "{out}");
    assert!(
        out.contains("no-such-skill") && out.contains("fix:"),
        "{out}"
    );
    let out = home.fails(&["check", "scout", "--profile", "alice"]);
    assert!(out.contains("scout has 1 error"), "{out}");
    assert!(home.ok(&["list", "--profile", "alice"]).contains("1 error"));

    // What isn't this agent's blueprint isn't saved.
    let edit = ["edit", "scout", "--from", "-", "--profile", "alice"];
    let out = home.run_with_stdin(&edit, "{ not json");
    assert!(
        !out.ok && out.out.contains("not saved: this isn't JSON"),
        "{}",
        out.out
    );
    let mut renamed = blueprint.clone();
    renamed["name"] = json!("other");
    let out = home.run_with_stdin(&edit, &renamed.to_string());
    assert!(
        !out.ok && out.out.contains("the blueprint's name must stay \"scout\""),
        "{}",
        out.out
    );
    assert_eq!(home.blueprint("scout"), blueprint);

    // In an editor: one that fails changes nothing, text that isn't JSON is kept for
    // the owner, and what it saves is checked.
    let script = home.path().join("editor.py");
    std::fs::write(&script, EDITOR).unwrap();
    let in_editor = |mode: &str| {
        let mut command = home.command(&["edit", "scout", "--profile", "alice"]);
        command.env(
            "VISUAL",
            format!("{} \"{}\" {mode}", python(), script.display()),
        );
        ran(command.output().unwrap())
    };
    let out = in_editor("fail");
    assert!(
        !out.ok && out.out.contains("the draft wasn't changed"),
        "{}",
        out.out
    );
    assert!(kept_edit(&out.out).contains("no-such-skill"));
    let out = in_editor("garble");
    assert!(!out.ok && out.out.contains("not saved"), "{}", out.out);
    assert_eq!(kept_edit(&out.out), "not json");
    assert_eq!(home.blueprint("scout"), blueprint);

    let out = in_editor("fix");
    assert!(
        out.ok && out.out.contains("saved draft scout: no problems"),
        "{}",
        out.out
    );
    let fixed = home.blueprint("scout");
    assert_eq!(fixed["title"], "Edited in an editor");
    assert!(fixed.get("skills").is_none(), "{fixed}");
    home.ok(&["check", "scout", "--profile", "alice"]);
    let shown = home.ok(&["show", "scout", "--profile", "alice"]);
    assert_eq!(shown.matches("alice  edited").count(), 2, "{shown}");
}

#[test]
fn a_blueprint_file_is_checked_with_the_fixtures_next_to_it() {
    let home = Home::new();
    let dir = home.path().join("analyst");
    std::fs::create_dir_all(&dir).unwrap();
    let mut blueprint = proofkin::factory::templates::get("data-analyst").unwrap();
    blueprint["name"] = json!("analyst");
    blueprint["model"]["providers"] = json!(["codex"]);
    blueprint["evals"]["cases"][0]["fixtures"] = json!(["sales.csv"]);
    let path = dir.join("blueprint.json");
    std::fs::write(&path, blueprint.to_string()).unwrap();
    let file = path.to_str().unwrap();
    let check = ["check", "--file", file, "--profile", "alice"];

    let out = home.fails(&check);
    assert!(
        out.contains("fixture sales.csv isn't in the fixtures folder"),
        "{out}"
    );
    std::fs::create_dir_all(dir.join("fixtures")).unwrap();
    std::fs::write(
        dir.join("fixtures").join("sales.csv"),
        "quarter,revenue\nQ1,10\n",
    )
    .unwrap();
    assert!(home.ok(&check).contains("no problems"));

    blueprint["model"]["providers"] = json!(["codex", "claude"]);
    std::fs::write(&path, blueprint.to_string()).unwrap();
    let out = home.fails(&check);
    assert!(
        out.contains("the profile isn't signed in to claude")
            && out.contains("proofkin auth login claude --profile alice"),
        "{out}"
    );

    std::fs::write(&path, r#"{"schema": "proofkin.agent-blueprint/v9"}"#).unwrap();
    let out = home.fails(&check);
    assert!(out.contains("has 1 error"), "{out}");
}

#[test]
fn drafting_needs_a_provider_the_profile_is_signed_in_to() {
    let home = Home::new();
    home.profile("bob", "{}");

    let describe = "Summarize the quarterly reports";
    let out = home.fails(&["new", "x", "--describe", describe, "--profile", "bob"]);
    assert!(
        out.contains("isn't signed in to a provider")
            && out.contains("proofkin auth login copilot --profile bob"),
        "{out}"
    );
    let out = home.fails(&["new", "x", "--template", "researcher", "--profile", "bob"]);
    assert!(out.contains("proofkin auth login"), "{out}");
    let out = home.fails(&[
        "new",
        "x",
        "--describe",
        describe,
        "--using",
        "claude",
        "--profile",
        "alice",
    ]);
    assert!(
        out.contains("isn't signed in to claude: proofkin auth login claude --profile alice"),
        "{out}"
    );
    let out = home.fails(&["new", "alice", "--describe", describe, "--profile", "alice"]);
    assert!(out.contains("'alice' is already a profile"), "{out}");
    assert!(
        home.ok(&["list", "--profile", "alice"])
            .contains("no agents yet")
    );
    assert!(
        home.ok(&["list", "--profile", "bob"])
            .contains("no agents yet")
    );
}
