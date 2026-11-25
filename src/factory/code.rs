//! Agents as code (docs/agent-factory/design/workflows.md, "Agents as code"): an agent kept
//! as files in a folder under git, next to the work it is for, proven by plan steps like
//! any other work. [`init`] writes them:
//!
//! ```text
//! agent/blueprint.json   its blueprint, without its eval suite
//! agent/evals.json       its eval suite
//! agent/fixtures/        files its cases work on
//! agent/skills/          skills of its own (<name>.md), next to the built-in ones
//! agent/reports/         its evaluations' signed reports (proofkin factory report ID --json --out agent/reports/ID.json)
//! ```
//!
//! The built-in checks `agent:blueprint`, `agent:evals` and `agent:report` ([`run`]) prove
//! the folder: read-only, following no links, like the other built-in checks. What depends
//! on where the agent runs (its folders, topics, peers and providers) is checked with its
//! owner's profile (`proofkin factory check`); here, the blueprint itself, its suite and its
//! fixtures, and that an evaluation of exactly this build passed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::factory::blueprint::{Blueprint, Catalog, Folder, Offer, Problem, from_value};
use crate::factory::report::{EvalState, Evaluation};
use crate::llm::Access;
use crate::spec::Report;

pub const BLUEPRINT: &str = "agent/blueprint.json";
pub const EVALS: &str = "agent/evals.json";
pub const FIXTURES: &str = "agent/fixtures";
pub const SKILLS: &str = "agent/skills";
pub const REPORTS: &str = "agent/reports";
/// The checks, as plan steps name them.
pub const CHECKS: &[&str] = &["agent:blueprint", "agent:evals", "agent:report"];
/// The largest report the checks read: reports keep their runs' outputs.
const MAX_REPORT: u64 = 32 * 1024 * 1024;

/// Writes agent `name`, from `template`, into `dir` as files (see the module); `catalog`, when
/// given, fits it to a profile (the providers it is signed in to). Returns the files written.
/// Fails if the folder has an agent already.
pub fn init(
    dir: &Path,
    name: &str,
    template: &str,
    catalog: Option<&Catalog>,
) -> Result<Vec<String>> {
    if !crate::profile::name_ok(name) {
        bail!("{name:?} can't name an agent: use letters, digits, '-', '_' and '.' (at most 64)");
    }
    for file in [BLUEPRINT, EVALS] {
        if dir.join(file).exists() {
            bail!("{} exists already: the folder has an agent", file);
        }
    }
    let mut blueprint = match catalog {
        Some(catalog) => crate::factory::templates::instantiate(template, name, catalog)?,
        None => {
            let mut value = crate::factory::templates::get(template)?;
            value["name"] = Value::from(name);
            value
        }
    };
    let evals = blueprint
        .as_object_mut()
        .and_then(|o| o.remove("evals"))
        .context("the template has no eval suite")?;
    let mut written = Vec::new();
    for (file, value) in [(BLUEPRINT, &blueprint), (EVALS, &evals)] {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().expect("in agent/"))?;
        std::fs::write(&path, serde_json::to_string_pretty(value)? + "\n")
            .with_context(|| format!("writing {}", path.display()))?;
        written.push(file.to_owned());
    }
    // Git keeps no empty folders.
    for folder in [FIXTURES, SKILLS, REPORTS] {
        let keep = dir.join(folder).join(".gitkeep");
        std::fs::create_dir_all(dir.join(folder))?;
        std::fs::write(&keep, "")?;
        written.push(format!("{folder}/"));
    }
    Ok(written)
}

/// The agent in folder `dir`: its blueprint with its eval suite, as JSON. A file that can't
/// be read, or isn't JSON, is the problem. Without `agent/evals.json`, `suite` says what to
/// do: fail, or go on with an empty suite.
fn value_of(dir: &Path, suite: bool) -> Result<Value, String> {
    let read = |file: &str| -> Result<Value, String> {
        let text = crate::spec::read_inside(dir, file)?;
        serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|e| format!("{file} isn't JSON: {e}"))
    };
    let mut value = read(BLUEPRINT)?;
    if !value.is_object() {
        return Err(format!("{BLUEPRINT} isn't a blueprint (a JSON object)"));
    }
    if value.get("evals").is_some() {
        return Err(format!(
            "{BLUEPRINT} has an eval suite: it goes in {EVALS} (fix: move its \"evals\" there)"
        ));
    }
    value["evals"] = match read(EVALS) {
        Ok(evals) => evals,
        Err(_) if !suite => serde_json::json!({ "cases": [] }),
        Err(problem) => return Err(problem),
    };
    Ok(value)
}

/// The agent in folder `dir` as a draft's blueprint (JSON), for `proofkin factory new` and
/// `edit --from`: its blueprint with its eval suite.
pub fn blueprint_value(dir: &Path) -> Result<Value> {
    value_of(dir, true).map_err(|problem| anyhow!("{problem}"))
}

/// Copies the fixtures in folder `from` (an agent's `agent/fixtures`, or the `fixtures/`
/// next to a blueprint file) to `to` (an agent's fixtures folder in the factory), replacing
/// what is there; symbolic links are left out. Returns how many files it copied.
pub fn copy_fixtures_from(from: &Path, to: &Path) -> Result<usize> {
    fn copy(from: &Path, to: &Path) -> Result<usize> {
        let mut copied = 0;
        let Ok(entries) = std::fs::read_dir(from) else {
            return Ok(0);
        };
        for entry in entries.flatten() {
            let kind = entry.file_type()?;
            let target = to.join(entry.file_name());
            if kind.is_dir() {
                copied += copy(&entry.path(), &target)?;
            } else if kind.is_file() && entry.file_name() != ".gitkeep" {
                std::fs::create_dir_all(to)?;
                std::fs::copy(entry.path(), &target)
                    .with_context(|| format!("copying {}", entry.path().display()))?;
                copied += 1;
            }
        }
        Ok(copied)
    }
    if to.exists() {
        std::fs::remove_dir_all(to).with_context(|| format!("replacing {}", to.display()))?;
    }
    copy(from, to)
}

/// The skills the agent in folder `dir` may follow: the built-in ones, and its own
/// (`agent/skills/<name>.md`, files, not links).
fn skills(dir: &Path) -> BTreeMap<String, String> {
    let mut all: BTreeMap<String, String> = crate::skills::Skills::new(dir.join(".no-skills"))
        .list()
        .into_iter()
        .map(|s| (s.name, s.description))
        .collect();
    if let Ok(entries) = std::fs::read_dir(dir.join(SKILLS)) {
        for entry in entries.flatten() {
            let path = entry.path();
            let own = entry.file_type().is_ok_and(|t| t.is_file())
                && path.extension().is_some_and(|e| e == "md");
            if let Some(name) = path.file_stem().and_then(|s| s.to_str())
                && own
                && crate::profile::name_ok(name)
            {
                all.insert(name.to_owned(), "the folder's own skill".into());
            }
        }
    }
    all
}

/// What `bp`, in folder `dir`, is checked against: its skills (built-in and the folder's
/// own), and whatever else it names, as offered: where the agent runs decides those, and its
/// owner's `proofkin factory check` checks them there.
pub fn catalog(bp: &Blueprint, dir: &Path) -> Catalog {
    let caps = &bp.capabilities;
    let offers: BTreeMap<String, Offer> = caps
        .mcp
        .iter()
        .map(|m| {
            let offer = Offer {
                access: Access::ReadOnly,
                tools: Vec::new(),
                paused: false,
                plugin_off: None,
            };
            (m.server.clone(), offer)
        })
        .collect();
    let folder = Folder {
        path: dir.to_path_buf(),
        access: Access::Full,
        checks: caps.checks.iter().cloned().collect(),
        mcp: offers,
        worktrees: true,
    };
    let mut topics: BTreeSet<String> = bp.memory.context.iter().cloned().collect();
    topics.extend(bp.memory.lessons.clone());
    Catalog {
        profile: "PROFILE".into(),
        skills: skills(dir),
        topics,
        marked: BTreeMap::new(),
        folders: bp
            .deploy
            .folders
            .iter()
            .map(|f| (f.clone(), folder.clone()))
            .collect(),
        providers: bp.model.providers.iter().copied().collect(),
        models: Default::default(),
        workiq: true,
        peers: bp
            .approvers
            .iter()
            .filter(|a| *a != "me")
            .cloned()
            .collect(),
        agents: BTreeSet::new(),
        profiles: BTreeSet::new(),
        factory: BTreeSet::new(),
    }
}

/// What a check says of a problem: where, what, and its fix.
fn line(p: &Problem) -> String {
    let mut text = match p.path.as_deref().filter(|x| !x.is_empty()) {
        Some(at) => format!("{at}: {}", p.message),
        None => p.message.clone(),
    };
    if let Some(fix) = &p.fix {
        text += &format!(" (fix: {fix})");
    }
    text
}

fn add(report: &mut Report, problems: impl IntoIterator<Item = Problem>) {
    for p in problems {
        if p.error {
            report.problems.push(line(&p));
        } else {
            report.notes.push(format!("warning: {}", line(&p)));
        }
    }
}

/// Runs built-in check `name` (one of [`CHECKS`]) on folder `dir`.
pub fn run(name: &str, dir: &Path) -> Report {
    let mut report = Report::default();
    match name {
        "agent:blueprint" => check_blueprint(dir, &mut report),
        "agent:evals" => check_evals(dir, &mut report),
        "agent:report" => check_report(dir, &mut report),
        _ => report
            .problems
            .push(format!("unknown built-in check {name}")),
    }
    report
}

/// The agent in `dir`, or the problem that keeps it from being one; `suite` as for
/// [`value_of`].
fn agent(dir: &Path, suite: bool, report: &mut Report) -> Option<Blueprint> {
    let value = match value_of(dir, suite) {
        Ok(value) => value,
        Err(problem) => {
            report.problems.push(problem);
            return None;
        }
    };
    match from_value(&value) {
        Ok(bp) => Some(bp),
        Err(p) => {
            report.problems.push(line(&p));
            None
        }
    }
}

/// The blueprint: everything [`crate::factory::blueprint::validate`] finds but its suite's
/// problems (`agent:evals`'), its skills being the built-in ones and the folder's own.
fn check_blueprint(dir: &Path, report: &mut Report) {
    let Some(bp) = agent(dir, false, report) else {
        return;
    };
    let suite = crate::factory::evals::check(&bp);
    let problems = crate::factory::blueprint::validate(&bp, &catalog(&bp, dir));
    add(report, problems.into_iter().filter(|p| !suite.contains(p)));
    report.notes.push(format!(
        "{BLUEPRINT}: agent {} ({}), {} requirements, {} access",
        bp.name,
        bp.title,
        bp.requirements.len(),
        bp.capabilities.access
    ));
}

/// The suite: a case for every requirement with a deterministic expectation, and fixtures
/// that are files in `agent/fixtures/` (see [`crate::factory::evals`]).
fn check_evals(dir: &Path, report: &mut Report) {
    let Some(bp) = agent(dir, true, report) else {
        return;
    };
    add(report, crate::factory::evals::check(&bp));
    add(
        report,
        crate::factory::evals::check_fixtures(&bp.evals, &dir.join(FIXTURES)),
    );
    let requirements: BTreeSet<&str> = bp
        .evals
        .cases
        .iter()
        .map(|c| c.requirement.as_str())
        .collect();
    report.notes.push(format!(
        "{EVALS}: {} cases, {} runs each, proving {}",
        bp.evals.cases.len(),
        bp.evals.repetitions(),
        requirements.into_iter().collect::<Vec<_>>().join(", ")
    ));
}

/// A report in `agent/reports/` of an evaluation of exactly the folder's build that passed
/// and verifies: its signature by its signer, each run's evidence and kept output.
fn check_report(dir: &Path, report: &mut Report) {
    let Some(bp) = agent(dir, true, report) else {
        return;
    };
    let skills = crate::skills::Skills::new(dir.join(SKILLS));
    let build = match crate::factory::build::build(&bp, &skills) {
        Ok(build) => build,
        Err(e) => {
            report
                .problems
                .push(format!("the folder's agent doesn't build: {e:#}"));
            return;
        }
    };
    let mut seen = Vec::new();
    let mut files: Vec<String> = std::fs::read_dir(dir.join(REPORTS))
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".json"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    for file in files {
        let rel = format!("{REPORTS}/{file}");
        let parsed = crate::spec::read_inside_up_to(dir, &rel, MAX_REPORT).and_then(|text| {
            serde_json::from_str::<Evaluation>(&text)
                .map_err(|e| format!("{rel} isn't an evaluation report: {e}"))
        });
        let evaluation = match parsed {
            Ok(evaluation) => evaluation,
            Err(problem) => {
                seen.push(problem);
                continue;
            }
        };
        if evaluation.agent != bp.name || evaluation.build != build.hash {
            seen.push(format!(
                "{rel} is of {} build {}, not this one",
                evaluation.agent,
                &evaluation.build[..12.min(evaluation.build.len())]
            ));
            continue;
        }
        if evaluation.state != EvalState::Passed {
            seen.push(format!(
                "{rel}: evaluation {} {}",
                evaluation.id, evaluation.state
            ));
            continue;
        }
        if let Err(e) = evaluation.verify() {
            seen.push(format!("{rel} doesn't verify: {e:#}"));
            continue;
        }
        report.notes.push(format!(
            "{rel}: evaluation {} of build {} passed, signed by {} ({}){}",
            evaluation.id,
            build.short_hash(),
            evaluation.signer,
            &evaluation.signer_id[..10.min(evaluation.signer_id.len())],
            evaluation
                .recommended
                .as_ref()
                .map(|r| format!(", recommending {r}"))
                .unwrap_or_default()
        ));
        return;
    }
    report.problems.push(format!(
        "{REPORTS}/ has no passed, verified evaluation of build {} (fix: proofkin factory evaluate {} --yes --profile PROFILE, then proofkin factory report ID --json --out {REPORTS}/ID.json --profile PROFILE)",
        build.short_hash(),
        bp.name
    ));
    report.problems.extend(seen);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::report::tests::evaluation;

    fn folder() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path(), "ask", "researcher", None).unwrap();
        dir
    }

    fn edit(dir: &Path, file: &str, change: impl FnOnce(&mut Value)) {
        let path = dir.join(file);
        let mut value: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        change(&mut value);
        std::fs::write(&path, value.to_string()).unwrap();
    }

    fn has(report: &Report, text: &str) {
        assert!(
            report.problems.iter().any(|p| p.contains(text)),
            "{text:?} isn't in {:#?}",
            report.problems
        );
    }

    #[test]
    fn init_writes_a_folder_whose_blueprint_and_suite_pass() {
        let dir = folder();
        for file in [
            BLUEPRINT,
            EVALS,
            "agent/fixtures/.gitkeep",
            "agent/reports/.gitkeep",
        ] {
            assert!(dir.path().join(file).is_file(), "{file}");
        }
        let blueprint = blueprint_value(dir.path()).unwrap();
        assert_eq!(blueprint["name"], "ask");
        assert_eq!(blueprint["evals"]["cases"].as_array().unwrap().len(), 3);
        for name in ["agent:blueprint", "agent:evals"] {
            let report = run(name, dir.path());
            assert!(report.passed(), "{name}: {}", report.text());
        }
        assert!(
            run("agent:evals", dir.path())
                .text()
                .contains("3 cases, 3 runs each, proving R1, R2, R3")
        );
        let again = init(dir.path(), "ask", "researcher", None).unwrap_err();
        assert!(format!("{again}").contains("exists already"), "{again}");
    }

    #[test]
    fn problems_are_named_with_their_fix_by_the_check_they_belong_to() {
        let dir = folder();
        // A skill nobody has: the blueprint's problem.
        edit(dir.path(), BLUEPRINT, |v| {
            v["skills"] = serde_json::json!(["no-such-skill"])
        });
        let report = run("agent:blueprint", dir.path());
        has(&report, "/skills/0: ");
        has(&report, "no-such-skill");
        has(&report, "(fix: ");
        assert!(run("agent:evals", dir.path()).passed(), "not the suite's");
        // The folder's own skill is one.
        let skills = dir.path().join(SKILLS);
        std::fs::write(
            skills.join("no-such-skill.md"),
            "---\nname: no-such-skill\ndescription: x\n---\nDo it well.\n",
        )
        .unwrap();
        assert!(run("agent:blueprint", dir.path()).passed());

        // R2's only deterministic case gone: the suite's problem.
        edit(dir.path(), EVALS, |v| {
            v["cases"]
                .as_array_mut()
                .unwrap()
                .retain(|c| c["requirement"] != "R2")
        });
        let report = run("agent:evals", dir.path());
        has(
            &report,
            "no case proves R2 with a deterministic expectation",
        );
        has(&report, "(fix: add a case for R2");
        assert!(
            run("agent:blueprint", dir.path()).passed(),
            "not the blueprint's"
        );
        // A fixture that isn't there.
        edit(dir.path(), EVALS, |v| {
            v["cases"][0]["fixtures"] = serde_json::json!(["notes.md"])
        });
        has(
            &run("agent:evals", dir.path()),
            "fixture notes.md isn't in the fixtures folder",
        );

        std::fs::write(dir.path().join(EVALS), "{").unwrap();
        has(
            &run("agent:evals", dir.path()),
            "agent/evals.json isn't JSON",
        );
        std::fs::remove_file(dir.path().join(BLUEPRINT)).unwrap();
        has(
            &run("agent:blueprint", dir.path()),
            "agent/blueprint.json is missing",
        );
    }

    #[test]
    fn a_passed_verified_report_of_the_folders_build_proves_it() {
        let dir = folder();
        let report = run("agent:report", dir.path());
        has(
            &report,
            "agent/reports/ has no passed, verified evaluation of build ",
        );
        has(&report, "proofkin factory evaluate ask --yes");

        let bp = from_value(&blueprint_value(dir.path()).unwrap()).unwrap();
        let build =
            crate::factory::build::build(&bp, &crate::skills::Skills::new(dir.path().join(SKILLS)))
                .unwrap();
        let key = iroh::SecretKey::generate();
        let mut e = evaluation();
        e.id = "ev-20261124-091500-ask".into();
        e.agent = "ask".into();
        e.build = build.hash.clone();
        e.runs.clear();
        e.finish(&[], None);
        e.state = EvalState::Passed;
        e.sign("alice", &key);
        let file = dir.path().join(REPORTS).join("ev-1.json");
        std::fs::write(&file, serde_json::to_string(&e).unwrap()).unwrap();
        let passed = run("agent:report", dir.path());
        assert!(passed.passed(), "{}", passed.text());
        assert!(
            passed.text().contains("passed, signed by alice"),
            "{}",
            passed.text()
        );

        // Changed after it was signed, or of another build: not proof.
        let mut forged = e.clone();
        forged.gate.reasons.push("x".into());
        std::fs::write(&file, serde_json::to_string(&forged).unwrap()).unwrap();
        has(&run("agent:report", dir.path()), "ev-1.json doesn't verify");
        edit(dir.path(), BLUEPRINT, |v| {
            v["title"] = "Another title".into()
        });
        std::fs::write(&file, serde_json::to_string(&e).unwrap()).unwrap();
        has(&run("agent:report", dir.path()), "is of ask build");
    }

    #[cfg(unix)]
    #[test]
    fn links_are_not_followed() {
        let dir = folder();
        let outside = tempfile::tempdir().unwrap();
        let real = outside.path().join("evals.json");
        std::fs::rename(dir.path().join(EVALS), &real).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join(EVALS)).unwrap();
        has(&run("agent:evals", dir.path()), "symbolic link");
    }
}
