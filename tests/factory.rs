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

    /// `proofkin` in this home.
    fn base(&self) -> Command {
        let mut command = Command::new(EXE);
        command
            .env("PROOFKIN_HOME", self.path())
            .env("PROOFKIN_SECRET_STORE", "file")
            .env_remove("VISUAL")
            .env_remove("EDITOR")
            .stdin(Stdio::null());
        command
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = self.base();
        command.arg("factory").args(args);
        command
    }

    /// Another proofkin command (not `factory`), and what it did.
    fn proofkin(&self, args: &[&str]) -> Ran {
        ran(self.base().args(args).output().unwrap())
    }

    fn proofkin_ok(&self, args: &[&str]) -> String {
        let ran = self.proofkin(args);
        assert!(ran.ok, "proofkin {} failed:\n{}", args.join(" "), ran.out);
        ran.out
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
#[test]
fn a_draft_is_built_into_the_same_files_each_time_and_kept_by_their_hash() {
    let home = Home::new();
    home.ok(&[
        "new",
        "rev",
        "--template",
        "code-reviewer",
        "--profile",
        "alice",
    ]);
    let out = home.ok(&["build", "rev", "--profile", "alice"]);
    for file in [
        "agent.md",
        "blueprint.json",
        "evals.json",
        "skills/rev.code-review.md",
        "skills/rev.security-review.md",
    ] {
        assert!(
            out.contains(&format!("  {file} (")),
            "{file} isn't in:\n{out}"
        );
    }
    let field = |out: &str, name: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(name))
            .unwrap_or_else(|| panic!("no {name:?} in:\n{out}"))
            .to_owned()
    };
    let hash = field(&out, "hash: ");
    assert!(hash.len() == 64 && out.contains(&format!("built rev: {}", &hash[..12])));
    let dir = std::path::PathBuf::from(field(&out, "in: "));
    let agent = std::fs::read_to_string(dir.join("agent.md")).unwrap();
    let front = agent.split("---").nth(1).unwrap();
    assert!(
        front.contains("name: rev")
            && front.contains("skills: [rev.code-review, rev.security-review]"),
        "{agent}"
    );
    assert!(
        !front.contains("tools:") && !front.contains("model:"),
        "{agent}"
    );
    assert!(agent.contains("OUT_OF_SCOPE"), "{agent}");

    // The same draft builds the same bytes again, kept once.
    let again = home.ok(&["build", "rev", "--profile", "alice"]);
    assert_eq!(field(&again, "hash: "), hash);
    let shown = home.ok(&["show", "rev", "--profile", "alice"]);
    assert!(
        shown.contains(&format!("builds: {}", &hash[..12])),
        "{shown}"
    );
    // It is evaluated on what the profile runs: alice's Codex, at its default model.
    assert!(
        shown.contains("evaluated on: codex (its default model)"),
        "{shown}"
    );

    // A build changed after it was made is refused, not used.
    std::fs::write(
        dir.join("agent.md"),
        agent.replace("OUT_OF_SCOPE", "anything goes"),
    )
    .unwrap();
    let out = home.fails(&["build", "rev", "--profile", "alice"]);
    assert!(out.contains("doesn't match its hash"), "{out}");

    // A draft with an error isn't built.
    let mut blueprint = home.blueprint("rev");
    blueprint["skills"] = json!(["no-such-skill"]);
    let edit = ["edit", "rev", "--from", "-", "--profile", "alice"];
    assert!(home.run_with_stdin(&edit, &blueprint.to_string()).ok);
    let out = home.fails(&["build", "rev", "--profile", "alice"]);
    assert!(
        out.contains("rev isn't built: its draft has errors"),
        "{out}"
    );
}

#[test]
fn check_resolves_what_an_agent_asks_for_in_each_folder_and_grants_nothing() {
    let home = Home::new();
    // Signed in to Claude: its tasks may use MCP servers (Codex's can't).
    home.profile("alice", r#"{"claude": {"source": "login"}}"#);
    let app = home.path().join("app");
    std::fs::create_dir_all(&app).unwrap();
    let app = app.to_str().unwrap();
    let share = |access: &str, checks: &[&str]| {
        let mut args = vec!["share", "add", "app", app, "--access", access];
        for check in checks {
            args.extend(["--check", check]);
        }
        args.extend(["--profile", "alice"]);
        home.proofkin_ok(&args);
    };
    let offer_pdf = |tools: &[&str]| {
        let mut args = vec!["mcp", "add", "app", "pdf", "--access", "read-only"];
        for tool in tools {
            args.extend(["--tool", tool]);
        }
        args.extend(["--profile", "alice", "--", "pdf-server", "--stdio"]);
        home.proofkin_ok(&args);
    };
    share("workspace-write", &[]);
    offer_pdf(&["read_pdf"]);
    // A peer on alice's allowlist, so there is one to leave as it is.
    let out = home.proofkin_ok(&["peers", "id", "--profile", "bob"]);
    let bob = out
        .split("endpoint id: ")
        .nth(1)
        .and_then(|rest| rest.get(..64))
        .expect(&out)
        .to_owned();
    home.proofkin_ok(&[
        "peers",
        "allow",
        &bob,
        "--name",
        "bob",
        "--profile",
        "alice",
    ]);
    home.ok(&[
        "new",
        "impl",
        "--template",
        "implementer",
        "--profile",
        "alice",
    ]);
    let mut blueprint = home.blueprint("impl");
    blueprint["capabilities"]["mcp"] = json!([{
        "server": "pdf",
        "tools": ["read_pdf", "search_pdf"],
        "why": "It reads the design documents it implements."
    }]);
    blueprint["model"]["needs_mcp"] = json!(true);
    let edit = ["edit", "impl", "--from", "-", "--profile", "alice"];
    assert!(home.run_with_stdin(&edit, &blueprint.to_string()).ok);
    let alice = home.path().join("profiles").join("alice");
    let stores = || ["shares.json", "peers.json"].map(|f| std::fs::read(alice.join(f)).unwrap());
    let before = stores();

    let out = home.ok(&["check", "impl", "--profile", "alice"]);
    assert!(
        out.contains("impl: no problems") && out.contains("no folders to resolve"),
        "{out}"
    );
    let out = home.fails(&[
        "check",
        "impl",
        "--folders",
        "app,data,ledger",
        "--profile",
        "alice",
    ]);
    for part in [
        "'app' gives tasks at most workspace-write access, and it asks for full",
        "--access full --profile alice",
        "'pdf' in 'app' doesn't offer search_pdf",
        "fix: proofkin mcp add app pdf --access read-only --tool read_pdf --tool search_pdf --profile alice",
        "'app' has no check 'test'",
        "--check test=<command>",
        "every deployed agent has its own data folder",
        "you share no folder 'ledger'",
        "fix: proofkin share add ledger <path> --profile alice",
        "in app: workspace-write access; MCP servers none; checks none",
    ] {
        assert!(out.contains(part), "{part:?} isn't in:\n{out}");
    }
    assert_eq!(
        stores(),
        before,
        "checking offers, shares and grants nothing"
    );

    // Once the folder offers what it asks for, as the fixes say, it resolves.
    share("full", &["test=cargo test"]);
    offer_pdf(&["read_pdf", "search_pdf"]);
    let out = home.ok(&["check", "impl", "--folders", "app", "--profile", "alice"]);
    assert!(
        out.contains("impl: no problems")
            && out.contains(
                "in app: full access; MCP servers pdf (read_pdf, search_pdf); checks test"
            ),
        "{out}"
    );
}

#[test]
fn a_workers_memory_copies_its_topics_with_their_marks_from_its_owner() {
    let home = Home::new();
    let knowledge = home.path().join("profiles").join("alice").join("knowledge");
    std::fs::create_dir_all(&knowledge).unwrap();
    std::fs::write(
        knowledge.join("glossary.md"),
        "# Glossary\n\nEBITDA: earnings before interest, taxes, depreciation and amortization.\n",
    )
    .unwrap();
    std::fs::write(
        knowledge.join("q3-board.md"),
        "---\norigin: workiq\nlabels: Confidential\n---\n# Q3 board\n\nRevenue was 12.\n",
    )
    .unwrap();
    home.proofkin_ok(&["worker", "create", "analyst", "--owner", "alice"]);
    let memory = |args: &[&str]| {
        let mut all = vec!["worker", "memory", "analyst"];
        all.extend(args);
        home.proofkin(&all)
    };

    let out = memory(&[]).out;
    assert!(
        out.contains("in front of every task: no knowledge") && out.contains("lessons: none"),
        "{out}"
    );
    let out = memory(&[
        "--context",
        "glossary,q3-board",
        "--lessons",
        "analyst-lessons",
        "--retention-days",
        "30",
    ]);
    assert!(out.ok, "{}", out.out);
    for part in [
        "copied from 'alice''s knowledge, with their marks: glossary, q3-board",
        "in front of every task: glossary, q3-board",
        "lessons: topic 'analyst-lessons', kept 30 days",
        "it takes effect when 'analyst' next starts",
    ] {
        assert!(out.out.contains(part), "{part:?} isn't in:\n{}", out.out);
    }
    let worker = home.path().join("profiles").join("analyst");
    let copied = std::fs::read_to_string(worker.join("knowledge").join("q3-board.md")).unwrap();
    assert!(
        copied.starts_with("---\norigin: workiq\nlabels: Confidential\n---\n")
            && copied.contains("copied from alice's knowledge")
            && copied.contains("Revenue was 12."),
        "{copied}"
    );
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(worker.join("profile.json")).unwrap())
            .unwrap();
    assert_eq!(
        config["memory"],
        json!({"context": ["glossary", "q3-board"], "lessons": "analyst-lessons", "retention_days": 30})
    );

    // Only its owner's topics, named one by one, and only on a digital worker.
    let out = memory(&["--context", "nope"]);
    assert!(
        !out.ok && out.out.contains("no topic 'nope'"),
        "{}",
        out.out
    );
    let out = memory(&["--context", "all"]);
    assert!(
        !out.ok && out.out.contains("name the topics it reads"),
        "{}",
        out.out
    );
    let out = home.proofkin(&["worker", "memory", "alice", "--lessons", "x"]);
    assert!(
        !out.ok && out.out.contains("'alice' isn't a digital worker"),
        "{}",
        out.out
    );

    let out = memory(&["--off"]).out;
    assert!(
        out.contains("in front of every task: no knowledge") && out.contains("lessons: none"),
        "{out}"
    );
}

/// AC-8: evaluating says first what it would run (its cases with the built-in attacks, the
/// runs on each candidate) and runs nothing until it is confirmed with --yes.
#[test]
fn an_evaluation_shows_its_estimate_and_runs_nothing_until_it_is_confirmed() {
    let home = Home::new();
    home.ok(&[
        "new",
        "rev",
        "--template",
        "code-reviewer",
        "--profile",
        "alice",
    ]);
    let cases = home.blueprint("rev")["evals"]["cases"]
        .as_array()
        .unwrap()
        .len();
    let out = home.ok(&["evaluate", "rev", "--profile", "alice"]);
    let runs = (cases + 7) * 3;
    for part in [
        "candidates: codex (its default model)".to_owned(),
        format!(
            "estimate: {} cases (7 built-in adversarial) x 3 runs x 1 candidate = {runs} runs",
            cases + 7
        ),
        "nothing ran: run it with --yes (its tasks go to your evaluation worker alice-eval)"
            .to_owned(),
    ] {
        assert!(out.contains(&part), "{part:?} isn't in:\n{out}");
    }
    assert!(out.contains("rev: build "), "it is of a build: {out}");
    // Nothing was sent: there is no evaluation worker and no evaluation.
    let profiles = home.path().join("profiles");
    assert!(!profiles.join("alice-eval").exists());
    let agent = profiles
        .join("alice")
        .join("factory")
        .join("agents")
        .join("rev");
    assert!(!agent.join("evaluations").exists());
    assert!(
        !home
            .ok(&["show", "rev", "--profile", "alice"])
            .contains("evaluations")
    );

    // On other models, fewer times.
    let out = home.ok(&[
        "evaluate",
        "rev",
        "--candidate",
        "codex:gpt-5:high",
        "--candidate",
        "codex:gpt-5-mini",
        "--repetitions",
        "1",
        "--profile",
        "alice",
    ]);
    assert!(
        out.contains("candidates: codex gpt-5 (high reasoning); codex gpt-5-mini"),
        "{out}"
    );
    assert!(out.contains("x 1 run x 2 candidates"), "{out}");
    for (args, why) in [
        (vec!["--repetitions", "9"], "repetitions is 1 to 5"),
        (
            vec!["--candidate", "copilot:gpt-5"],
            "it can't be evaluated on copilot gpt-5: copilot isn't one of its providers",
        ),
        (vec!["--candidate", "codex"], "names no model"),
        (vec!["--candidate", "nobody:gpt-5"], "isn't a candidate"),
    ] {
        let mut all = vec!["evaluate", "rev", "--profile", "alice"];
        all.extend(args);
        let out = home.fails(&all);
        assert!(out.contains(why), "{why:?} isn't in:\n{out}");
    }
    // A draft with an error isn't evaluated.
    let original = home.blueprint("rev");
    let mut blueprint = original.clone();
    blueprint["skills"] = json!(["no-such-skill"]);
    let edit = ["edit", "rev", "--from", "-", "--profile", "alice"];
    assert!(home.run_with_stdin(&edit, &blueprint.to_string()).ok);
    let out = home.fails(&["evaluate", "rev", "--profile", "alice"]);
    assert!(
        out.contains("the draft has 1 error: fix it, then evaluate"),
        "{out}"
    );
    // A suite that doesn't prove a requirement is what's wrong with the request.
    let mut blueprint = original;
    blueprint["evals"]["cases"]
        .as_array_mut()
        .unwrap()
        .retain(|c| c["requirement"] != "R2");
    assert!(home.run_with_stdin(&edit, &blueprint.to_string()).ok);
    let out = home.fails(&["evaluate", "rev", "--profile", "alice"]);
    assert!(
        out.contains(
            "its eval suite isn't valid: no case proves R2 with a deterministic expectation"
        ),
        "{out}"
    );
    let out = home.fails(&["report", "ev-20261124-091500-rev", "--profile", "alice"]);
    assert!(
        out.contains("there is no evaluation ev-20261124-091500-rev"),
        "{out}"
    );
}

/// Agents as code: `factory init` writes a folder whose agent:blueprint and agent:evals checks
/// pass (agent:report waits for a report), and a draft starts from it, with its fixtures.
#[test]
fn an_agent_kept_as_code_is_checked_in_its_folder_and_drafted_from_it() {
    let home = Home::new();
    let repo = home.path().join("repo");
    let dir = repo.to_str().unwrap();
    let out = home.ok(&["init", dir, "--name", "ask", "--profile", "alice"]);
    assert!(
        out.contains("wrote agent ask (template researcher)"),
        "{out}"
    );
    let blueprint: Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join("agent").join("blueprint.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(blueprint["name"], "ask");
    assert_eq!(
        blueprint["model"]["providers"],
        json!(["codex"]),
        "fit to alice"
    );
    assert!(
        blueprint.get("evals").is_none(),
        "its suite is in evals.json"
    );
    let out = home.fails(&["init", dir, "--name", "ask"]);
    assert!(out.contains("exists already"), "{out}");

    let checked = home.proofkin(&["spec", "check", "agent", "--dir", dir]);
    assert!(!checked.ok, "no report yet:\n{}", checked.out);
    for part in [
        "agent:blueprint: passed",
        "agent:evals: passed",
        "agent:report: FAILED",
        "agent/reports/ has no passed, verified evaluation of build",
    ] {
        assert!(
            checked.out.contains(part),
            "{part:?} isn't in:\n{}",
            checked.out
        );
    }
    let fixtures = repo.join("agent").join("fixtures");
    std::fs::write(fixtures.join("notes.md"), "# Notes\n").unwrap();
    let out = home.ok(&["check", "--file", dir, "--profile", "alice"]);
    assert!(out.contains(&format!("{dir}: no problems")), "{out}");
    let out = home.ok(&["new", "ask", "--from", dir, "--profile", "alice"]);
    assert!(out.contains("created draft ask (drafted from"), "{out}");
    assert!(out.contains("1 fixture): no problems"), "{out}");
    let copied = home
        .path()
        .join("profiles/alice/factory/agents/ask/fixtures/notes.md");
    assert!(copied.is_file());
    let out = home.fails(&["new", "other", "--from", dir, "--profile", "alice"]);
    assert!(out.contains("isn't named \"other\""), "{out}");
}
