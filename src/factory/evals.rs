//! Eval suites (docs/agent-factory.md, section 7): the cases that prove a blueprint's
//! requirements. A case names the requirement it proves, the task the agent gets (its
//! input, with fixture files), and what its answer must show. Deterministic
//! expectations decide whether a run passes; a judge's rubric is scored by another
//! model and never decides (ADR-0003). So every requirement needs a case with a
//! deterministic expectation.
//!
//! | Expectation | Passes when |
//! | --- | --- |
//! | `{"contains": "text"}`, `{"not-contains": "text"}` | the answer has (hasn't) the text, in any case |
//! | `{"regex": "pattern"}` | the answer matches |
//! | `{"json-schema": {...}}` | the answer, or its first fenced block, is JSON that fits the schema |
//! | `{"max-words": 200}` | the answer is at most that many words |
//! | `{"files-changed": ["src/**"]}` | the run changed files, all matching one of the globs |
//! | `{"check": "test"}` | the folder's check passes in the run's folder |
//! | `{"judge": "rubric", "min": 4}` | a judge model scores it; never a gate |

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::factory::blueprint::{Blueprint, Problem};
use crate::llm::Access;
use crate::profile::name_ok;

/// Runs per case and candidate when the suite says none.
pub const DEFAULT_REPETITIONS: u32 = 3;
/// The most runs per case and candidate.
pub const MAX_REPETITIONS: u32 = 5;
/// The share of runs that must pass, when the suite says none.
pub const DEFAULT_PASS_RATE: f64 = 0.9;
/// The share of each requirement's runs that must pass, when the suite says none.
pub const DEFAULT_REQUIREMENT_PASS_RATE: f64 = 2.0 / 3.0;
/// The most cases a suite has.
pub const MAX_CASES: usize = 200;
/// The longest input, in bytes.
pub const MAX_INPUT: usize = 16 * 1024;
/// The most fixtures a case has.
pub const MAX_FIXTURES: usize = 20;
/// The longest case id, in characters.
pub const MAX_CASE_ID: usize = 32;
/// What a compiled regex may take, so a pattern can't eat the agent's memory.
const REGEX_SIZE: usize = 1 << 20;
/// The kinds of expectation, as cases write them.
pub const KINDS: &[&str] = &[
    "contains",
    "not-contains",
    "regex",
    "json-schema",
    "max-words",
    "files-changed",
    "check",
    "judge",
];

/// A blueprint's eval suite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    /// Runs per case and candidate ([`DEFAULT_REPETITIONS`] when none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repetitions: Option<u32>,
    #[serde(default, skip_serializing_if = "Thresholds::is_empty")]
    pub thresholds: Thresholds,
    pub cases: Vec<Case>,
}

impl Suite {
    pub fn repetitions(&self) -> u32 {
        self.repetitions.unwrap_or(DEFAULT_REPETITIONS)
    }
}

/// What a release needs from an evaluation; the defaults when unset.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Thresholds {
    /// The share of all runs that must pass ([`DEFAULT_PASS_RATE`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_rate: Option<f64>,
    /// The share of each requirement's runs that must pass
    /// ([`DEFAULT_REQUIREMENT_PASS_RATE`]: 2 of 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirement_pass_rate: Option<f64>,
    /// The longest a run may take at the 95th percentile, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_secs: Option<u64>,
}

impl Thresholds {
    pub fn is_empty(&self) -> bool {
        self.pass_rate.is_none() && self.requirement_pass_rate.is_none() && self.p95_secs.is_none()
    }
}

/// One case: the task the agent gets, and what its answer must show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    /// The requirement it proves (`R1`...).
    pub requirement: String,
    /// The task, as the agent gets it.
    pub input: String,
    /// Files from the agent's fixtures folder the task works on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixtures: Vec<String>,
    /// Expectations, each an object with one kind (see [`Expect::parse`]).
    pub expect: Vec<Value>,
    /// A quick case a deployment's canary runs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub smoke: bool,
    /// How long a run may take: `30s`, `10m`, `1h`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

impl Case {
    /// Its expectations, or the first that isn't one: its index and why.
    pub fn expectations(&self) -> Result<Vec<Expect>, (usize, String)> {
        self.expect
            .iter()
            .enumerate()
            .map(|(i, e)| Expect::parse(e).map_err(|why| (i, why)))
            .collect()
    }
}

/// What a run's answer must show.
#[derive(Debug, Clone, PartialEq)]
pub enum Expect {
    Contains(String),
    NotContains(String),
    Regex(String),
    JsonSchema(Value),
    MaxWords(usize),
    FilesChanged(Vec<String>),
    Check(String),
    Judge { rubric: String, min: u8 },
}

impl Expect {
    /// An expectation as a case writes it: an object with one of [`KINDS`] (a judge
    /// may add `min`, the lowest passing score from 1 to 5, 4 by default).
    pub fn parse(value: &Value) -> Result<Self, String> {
        let Some(object) = value.as_object() else {
            return Err("an expectation is an object like {\"contains\": \"text\"}".into());
        };
        let kinds: Vec<&str> = object
            .keys()
            .map(String::as_str)
            .filter(|k| KINDS.contains(k))
            .collect();
        let kind = match kinds[..] {
            [kind] => kind,
            [] => {
                return Err(format!("it has none of the kinds {}", KINDS.join(", ")));
            }
            _ => return Err(format!("it has more than one kind: {}", kinds.join(", "))),
        };
        if let Some(other) = object
            .keys()
            .find(|k| *k != kind && !(kind == "judge" && *k == "min"))
        {
            return Err(format!("{other:?} isn't part of a {kind} expectation"));
        }
        let value = &object[kind];
        let text = || {
            value
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("{kind} takes text"))
        };
        Ok(match kind {
            "contains" => Expect::Contains(text()?),
            "not-contains" => Expect::NotContains(text()?),
            "regex" => {
                let pattern = text()?;
                compile(&pattern).map_err(|e| format!("the regex doesn't compile: {e}"))?;
                Expect::Regex(pattern)
            }
            "json-schema" => {
                if !value.is_object() {
                    return Err("json-schema takes a JSON schema (an object)".into());
                }
                Expect::JsonSchema(value.clone())
            }
            "max-words" => Expect::MaxWords(
                value
                    .as_u64()
                    .filter(|n| (1..=100_000).contains(n))
                    .ok_or("max-words takes a whole number from 1 to 100000")?
                    as usize,
            ),
            "files-changed" => {
                let globs: Vec<String> = value
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let all_text = value.as_array().is_some_and(|a| a.len() == globs.len());
                if globs.is_empty() || !all_text || !globs.iter().all(|g| relative(g)) {
                    return Err(
                        "files-changed takes globs of paths in the folder, like [\"src/**\"]"
                            .into(),
                    );
                }
                Expect::FilesChanged(globs)
            }
            "check" => {
                let name = text()?;
                if !name_ok(&name) {
                    return Err(format!("{name:?} can't name a check"));
                }
                Expect::Check(name)
            }
            "judge" => {
                let rubric = text()?;
                let min = match object.get("min") {
                    None => 4,
                    Some(m) => m
                        .as_u64()
                        .filter(|m| (1..=5).contains(m))
                        .ok_or("min is a score from 1 to 5")? as u8,
                };
                Expect::Judge { rubric, min }
            }
            _ => unreachable!("one of KINDS"),
        })
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Expect::Contains(_) => "contains",
            Expect::NotContains(_) => "not-contains",
            Expect::Regex(_) => "regex",
            Expect::JsonSchema(_) => "json-schema",
            Expect::MaxWords(_) => "max-words",
            Expect::FilesChanged(_) => "files-changed",
            Expect::Check(_) => "check",
            Expect::Judge { .. } => "judge",
        }
    }

    /// Whether it decides by itself, without a model: everything but a judge's rubric.
    pub fn is_deterministic(&self) -> bool {
        !matches!(self, Expect::Judge { .. })
    }
}

fn compile(pattern: &str) -> Result<Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE)
        .build()
}

/// A path or glob inside a folder: relative, with '/' between its parts, none of
/// them empty, `.` or `..`.
fn relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// What one run of a case gave, to grade.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// The agent's answer.
    pub output: String,
    /// The files the run changed, relative to its folder, with '/' between their parts.
    pub files_changed: Vec<String>,
    /// The folder it worked in, where `check` expectations run.
    pub dir: Option<PathBuf>,
}

/// An expectation's verdict on a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Graded {
    pub kind: &'static str,
    /// Whether it passed; none for a judge's rubric, which a judge model scores.
    pub passed: Option<bool>,
    /// Why, in a line.
    pub note: String,
}

impl Graded {
    fn of(kind: &'static str, passed: bool, note: impl Into<String>) -> Self {
        Self {
            kind,
            passed: Some(passed),
            note: note.into(),
        }
    }
}

/// Grades `outcome` against `expect`. A `check` runs the folder's check from `checks`
/// (name to command) in the run's folder, with the checks' usual timeout.
pub async fn grade(
    expect: &Expect,
    outcome: &Outcome,
    checks: &BTreeMap<String, String>,
) -> Graded {
    let Expect::Check(name) = expect else {
        return grade_now(expect, outcome);
    };
    let Some(command) = checks.get(name) else {
        return Graded::of("check", false, format!("the folder has no check '{name}'"));
    };
    let Some(dir) = &outcome.dir else {
        return Graded::of(
            "check",
            false,
            format!("the run left no folder to run '{name}' in"),
        );
    };
    let result =
        crate::evidence::run_check(name, command, dir, crate::evidence::CHECK_TIMEOUT).await;
    let exit = result
        .exit_code
        .map(|c| format!("exit code {c}"))
        .unwrap_or_else(|| "no exit code".into());
    Graded::of(
        "check",
        result.passed,
        if result.passed {
            format!("check {name} passed")
        } else {
            format!("check {name} failed ({exit})")
        },
    )
}

/// Grades `outcome` against `expect` without running anything: every kind but a
/// `check` (which [`grade`] runs).
pub fn grade_now(expect: &Expect, outcome: &Outcome) -> Graded {
    let output = &outcome.output;
    let kind = expect.kind();
    match expect {
        Expect::Contains(text) => {
            let found = output.to_lowercase().contains(&text.to_lowercase());
            Graded::of(
                kind,
                found,
                if found {
                    format!("the answer has {text:?}")
                } else {
                    format!("the answer doesn't have {text:?}")
                },
            )
        }
        Expect::NotContains(text) => {
            let found = output.to_lowercase().contains(&text.to_lowercase());
            Graded::of(
                kind,
                !found,
                if found {
                    // The expectation, so a failing run doesn't read as the model's mistake.
                    format!("expected no {text:?} (not-contains), but the answer has it")
                } else {
                    format!("the answer doesn't have {text:?}")
                },
            )
        }
        Expect::Regex(pattern) => match compile(pattern) {
            Ok(re) => {
                let found = re.is_match(output);
                Graded::of(
                    kind,
                    found,
                    if found {
                        format!("the answer matches /{pattern}/")
                    } else {
                        format!("nothing in the answer matches /{pattern}/")
                    },
                )
            }
            Err(e) => Graded::of(kind, false, format!("the regex doesn't compile: {e}")),
        },
        Expect::JsonSchema(schema) => {
            let parsed = serde_json::from_str::<Value>(output.trim())
                .or_else(|_| serde_json::from_str::<Value>(&crate::team::written_plan(output)));
            match parsed {
                Err(_) => Graded::of(kind, false, "the answer has no JSON"),
                Ok(value) => {
                    let wrong = crate::spec::schema_mismatches(&value, schema);
                    if wrong.is_empty() {
                        Graded::of(kind, true, "the answer's JSON fits the schema")
                    } else {
                        let shown: Vec<&str> = wrong.iter().take(3).map(String::as_str).collect();
                        Graded::of(
                            kind,
                            false,
                            format!(
                                "the answer's JSON doesn't fit the schema: {}",
                                shown.join("; ")
                            ),
                        )
                    }
                }
            }
        }
        Expect::MaxWords(most) => {
            let words = output.split_whitespace().count();
            Graded::of(
                kind,
                words <= *most,
                format!("the answer has {words} words (at most {most})"),
            )
        }
        Expect::FilesChanged(globs) => {
            if outcome.files_changed.is_empty() {
                return Graded::of(kind, false, "the run changed no file");
            }
            let outside: Vec<&str> = outcome
                .files_changed
                .iter()
                .filter(|f| !globs.iter().any(|g| glob_matches(g, f)))
                .map(String::as_str)
                .collect();
            if outside.is_empty() {
                Graded::of(
                    kind,
                    true,
                    format!(
                        "it changed {} file(s), all in {}",
                        outcome.files_changed.len(),
                        globs.join(", ")
                    ),
                )
            } else {
                Graded::of(
                    kind,
                    false,
                    format!(
                        "it changed files outside {}: {}",
                        globs.join(", "),
                        outside.join(", ")
                    ),
                )
            }
        }
        Expect::Check(name) => Graded::of(
            kind,
            false,
            format!("check {name} runs in the run's folder: grade it with grade()"),
        ),
        Expect::Judge { min, .. } => Graded {
            kind,
            passed: None,
            note: format!("a judge model scores it (at least {min} of 5); never a gate"),
        },
    }
}

/// Whether `path` (with '/' between its parts) matches `glob`: `*` matches any run of
/// characters but '/', `**` any run at all (`**/` also none), `?` one character but '/'.
pub fn glob_matches(glob: &str, path: &str) -> bool {
    let chars: Vec<char> = glob.chars().collect();
    let mut pattern = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                if chars.get(i + 2) == Some(&'/') {
                    pattern.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    pattern.push_str(".*");
                    i += 2;
                }
                continue;
            }
            '*' => pattern.push_str("[^/]*"),
            '?' => pattern.push_str("[^/]"),
            c => pattern.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    pattern.push('$');
    compile(&pattern).is_ok_and(|re| re.is_match(path))
}

static CASE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9_-]*$").expect("valid regex"));

/// Everything wrong with `blueprint`'s suite: each case must name a requirement,
/// give a task and expect something; each requirement needs a case with a
/// deterministic expectation.
pub fn check(blueprint: &Blueprint) -> Vec<Problem> {
    let mut found = Vec::new();
    let suite = &blueprint.evals;
    let caps = &blueprint.capabilities;
    if suite.cases.is_empty() {
        found.push(
            Problem::error("no_cases", "/evals/cases", "the suite has no case")
                .fix("add a case for each requirement: its input and what the answer must show"),
        );
    } else if suite.cases.len() > MAX_CASES {
        found.push(Problem::error(
            "too_many",
            "/evals/cases",
            format!("the suite has over {MAX_CASES} cases"),
        ));
    }
    if let Some(n) = suite.repetitions
        && !(1..=MAX_REPETITIONS).contains(&n)
    {
        found.push(Problem::error(
            "out_of_range",
            "/evals/repetitions",
            format!("repetitions is 1 to {MAX_REPETITIONS}"),
        ));
    }
    for (field, rate) in [
        ("pass_rate", suite.thresholds.pass_rate),
        (
            "requirement_pass_rate",
            suite.thresholds.requirement_pass_rate,
        ),
    ] {
        if let Some(rate) = rate
            && !(0.0..=1.0).contains(&rate)
        {
            found.push(Problem::error(
                "out_of_range",
                &format!("/evals/thresholds/{field}"),
                format!("{field} is a share from 0 to 1, like 0.9"),
            ));
        }
    }
    if suite.thresholds.p95_secs == Some(0) {
        found.push(Problem::error(
            "out_of_range",
            "/evals/thresholds/p95_secs",
            "p95_secs is a number of seconds above 0",
        ));
    }
    let requirements: BTreeSet<&str> = blueprint
        .requirements
        .iter()
        .map(|r| r.id.as_str())
        .collect();
    let (mut ids, mut proven, mut smoke) = (BTreeSet::new(), BTreeSet::new(), false);
    for (i, case) in suite.cases.iter().enumerate() {
        let at = format!("/evals/cases/{i}");
        if !CASE_ID.is_match(&case.id) || case.id.len() > MAX_CASE_ID {
            found.push(
                Problem::error(
                    "invalid_case_id",
                    &format!("{at}/id"),
                    format!("{:?} isn't a case id", case.id),
                )
                .fix("use letters, digits, '-' and '_', starting with a letter, like E1"),
            );
        } else if !ids.insert(case.id.as_str()) {
            found.push(Problem::error(
                "duplicate_case",
                &format!("{at}/id"),
                format!("there are two cases {}", case.id),
            ));
        }
        if !requirements.contains(case.requirement.as_str()) {
            found.push(
                Problem::error(
                    "unknown_requirement",
                    &format!("{at}/requirement"),
                    format!(
                        "case {} proves {:?}, which the blueprint doesn't state",
                        case.id, case.requirement
                    ),
                )
                .fix("name one of the requirements: R1, R2..."),
            );
        }
        if case.input.trim().is_empty() {
            found.push(Problem::error(
                "empty",
                &format!("{at}/input"),
                format!("case {} gives the agent no task", case.id),
            ));
        } else if case.input.len() > MAX_INPUT {
            found.push(Problem::error(
                "too_long",
                &format!("{at}/input"),
                format!(
                    "case {}'s input is over 16 KB: put long material in fixtures",
                    case.id
                ),
            ));
        }
        if case.fixtures.len() > MAX_FIXTURES {
            found.push(Problem::error(
                "too_many",
                &format!("{at}/fixtures"),
                format!("case {} has over {MAX_FIXTURES} fixtures", case.id),
            ));
        }
        for (j, fixture) in case.fixtures.iter().enumerate() {
            if !relative(fixture) {
                found.push(
                    Problem::error(
                        "invalid_fixture",
                        &format!("{at}/fixtures/{j}"),
                        format!("{fixture:?} isn't a path in the agent's fixtures folder"),
                    )
                    .fix("use a relative path with '/' and no '..', like reports/q3.pdf"),
                );
            }
        }
        if let Some(timeout) = &case.timeout {
            match crate::plan::parse_timeout(timeout) {
                Err(e) => found.push(Problem::error(
                    "invalid_timeout",
                    &format!("{at}/timeout"),
                    format!("{e:#}"),
                )),
                Ok(secs) => {
                    if let Some(most) = blueprint.budgets.timeout_secs
                        && secs > most
                    {
                        found.push(
                            Problem::error(
                                "timeout_over_budget",
                                &format!("{at}/timeout"),
                                format!(
                                    "case {} may take {timeout}, longer than the agent's budget of {}",
                                    case.id,
                                    crate::plan::timeout_text(most)
                                ),
                            )
                            .fix("shorten the case's timeout, or raise budgets.timeout_secs"),
                        );
                    }
                }
            }
        }
        if case.expect.is_empty() {
            found.push(
                Problem::error(
                    "no_expectations",
                    &format!("{at}/expect"),
                    format!("case {} expects nothing", case.id),
                )
                .fix("add what the answer must show, e.g. {\"contains\": \"...\"}"),
            );
        }
        let mut deterministic = false;
        for (j, value) in case.expect.iter().enumerate() {
            let path = format!("{at}/expect/{j}");
            match Expect::parse(value) {
                Err(why) => found.push(
                    Problem::error(
                        "invalid_expectation",
                        &path,
                        format!("case {}'s expectation {} isn't one: {why}", case.id, j + 1),
                    )
                    .fix(format!("use one of {}", KINDS.join(", "))),
                ),
                Ok(expect) => {
                    deterministic |= expect.is_deterministic();
                    match &expect {
                        Expect::Check(name) if !caps.checks.contains(name) => found.push(
                            Problem::error(
                                "unknown_check",
                                &path,
                                format!(
                                    "case {} runs check '{name}', which the agent doesn't ask for",
                                    case.id
                                ),
                            )
                            .fix(format!("add \"{name}\" to capabilities.checks")),
                        ),
                        Expect::FilesChanged(_) if caps.access == Access::ReadOnly => found.push(
                            Problem::error(
                                "files_changed_needs_edit",
                                &path,
                                format!(
                                    "case {} expects changed files from a read-only agent",
                                    case.id
                                ),
                            )
                            .fix("give it workspace-write access, or expect something else"),
                        ),
                        _ => {}
                    }
                }
            }
        }
        if deterministic {
            proven.insert(case.requirement.as_str());
        }
        smoke |= case.smoke;
    }
    for (i, requirement) in blueprint.requirements.iter().enumerate() {
        if !proven.contains(requirement.id.as_str()) {
            found.push(
                Problem::error(
                    "uncovered_requirement",
                    &format!("/requirements/{i}"),
                    format!("no case proves {} with a deterministic expectation", requirement.id),
                )
                .fix(format!(
                    "add a case for {} that expects contains, not-contains, regex, json-schema, max-words, files-changed or check: judges score, they never prove",
                    requirement.id
                )),
            );
        }
    }
    if !suite.cases.is_empty() && !smoke {
        found.push(
            Problem::warning("no_smoke_case", "/evals/cases", "no case is marked smoke").fix(
                "mark a quick case \"smoke\": true: a deployment's canary runs the smoke cases",
            ),
        );
    }
    found
}

/// Problems with `suite`'s fixtures in `dir`, the agent's fixtures folder: each must
/// be a file there, reached without symbolic links.
pub fn check_fixtures(suite: &Suite, dir: &Path) -> Vec<Problem> {
    let mut found = Vec::new();
    for (i, case) in suite.cases.iter().enumerate() {
        for (j, fixture) in case.fixtures.iter().enumerate() {
            if !relative(fixture) {
                continue;
            }
            let at = format!("/evals/cases/{i}/fixtures/{j}");
            let mut path = dir.to_path_buf();
            let mut problem = None;
            for part in fixture.split('/') {
                path.push(part);
                match std::fs::symlink_metadata(&path) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        problem = Some(format!(
                            "{fixture} is reached through a symbolic link, which isn't followed"
                        ));
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        problem = Some(format!("fixture {fixture} isn't in the fixtures folder"));
                        break;
                    }
                }
            }
            if problem.is_none() && !path.is_file() {
                problem = Some(format!("fixture {fixture} isn't a file"));
            }
            if let Some(message) = problem {
                found.push(
                    Problem::error("missing_fixture", &at, message)
                        .fix(format!("put the file in {}", dir.display())),
                );
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::blueprint::tests::{catalog, sample};
    use crate::factory::blueprint::{check_value, from_value};
    use serde_json::json;

    fn outcome(output: &str, files: &[&str]) -> Outcome {
        Outcome {
            output: output.into(),
            files_changed: files.iter().map(|f| (*f).to_owned()).collect(),
            dir: None,
        }
    }

    fn passed(expect: Value, outcome: &Outcome) -> Option<bool> {
        grade_now(&Expect::parse(&expect).unwrap(), outcome).passed
    }

    #[test]
    fn every_expectation_kind_parses_and_bad_ones_say_why() {
        for good in [
            json!({ "contains": "margin" }),
            json!({ "not-contains": "OUT_OF_SCOPE" }),
            json!({ "regex": "(?i)risk" }),
            json!({ "json-schema": { "type": "object" } }),
            json!({ "max-words": 200 }),
            json!({ "files-changed": ["src/**", "tests/*.rs"] }),
            json!({ "check": "test" }),
            json!({ "judge": "Each risk quotes its sentence", "min": 3 }),
        ] {
            assert!(Expect::parse(&good).is_ok(), "{good}");
        }
        assert_eq!(
            Expect::parse(&json!({ "judge": "Clear" })).unwrap(),
            Expect::Judge {
                rubric: "Clear".into(),
                min: 4
            }
        );
        for (bad, why) in [
            (json!("contains margin"), "an object"),
            (json!({ "smells-like": "x" }), "none of the kinds"),
            (
                json!({ "contains": "a", "regex": "b" }),
                "more than one kind",
            ),
            (
                json!({ "contains": "a", "min": 2 }),
                "isn't part of a contains",
            ),
            (json!({ "contains": "" }), "takes text"),
            (json!({ "regex": "(unclosed" }), "doesn't compile"),
            (json!({ "json-schema": "object" }), "JSON schema"),
            (json!({ "max-words": 0 }), "whole number"),
            (
                json!({ "files-changed": ["../secrets/*"] }),
                "globs of paths",
            ),
            (json!({ "files-changed": [] }), "globs of paths"),
            (json!({ "check": "rm -rf" }), "can't name a check"),
            (json!({ "judge": "Clear", "min": 9 }), "score from 1 to 5"),
        ] {
            let err = Expect::parse(&bad).unwrap_err();
            assert!(err.contains(why), "{bad}: {err}");
        }
    }

    #[test]
    fn deterministic_graders_pass_and_fail_as_specified() {
        let answer = outcome("Operating margin: 18% (p. 4)\nRisk: supply costs.", &[]);
        assert_eq!(
            passed(json!({ "contains": "operating MARGIN" }), &answer),
            Some(true)
        );
        assert_eq!(
            passed(json!({ "contains": "free cash flow" }), &answer),
            Some(false)
        );
        assert_eq!(
            passed(json!({ "not-contains": "OUT_OF_SCOPE" }), &answer),
            Some(true)
        );
        assert_eq!(
            passed(json!({ "not-contains": "risk" }), &answer),
            Some(false)
        );
        // A failing run's note names the expectation, not only what the answer has.
        let risky = grade_now(
            &Expect::parse(&json!({ "not-contains": "risk" })).unwrap(),
            &answer,
        );
        assert_eq!(
            risky.note,
            "expected no \"risk\" (not-contains), but the answer has it"
        );
        assert_eq!(
            passed(json!({ "regex": "p\\. ?[0-9]+" }), &answer),
            Some(true)
        );
        assert_eq!(passed(json!({ "regex": "^Revenue" }), &answer), Some(false));
        assert_eq!(passed(json!({ "max-words": 8 }), &answer), Some(true));
        assert_eq!(passed(json!({ "max-words": 3 }), &answer), Some(false));

        let schema = json!({ "json-schema": {
            "type": "object", "required": ["margin"],
            "properties": { "margin": { "type": "number" } } } });
        assert_eq!(
            passed(schema.clone(), &outcome("{\"margin\": 0.18}", &[])),
            Some(true)
        );
        let fenced = outcome("Here it is:\n```json\n{\"margin\": 0.18}\n```\nDone.", &[]);
        assert_eq!(passed(schema.clone(), &fenced), Some(true));
        let wrong = grade_now(
            &Expect::parse(&schema).unwrap(),
            &outcome("{\"margin\": \"high\"}", &[]),
        );
        assert_eq!(wrong.passed, Some(false));
        assert!(wrong.note.contains("margin"), "{}", wrong.note);
        let none = grade_now(
            &Expect::parse(&schema).unwrap(),
            &outcome("No JSON here", &[]),
        );
        assert_eq!(
            (none.passed, none.note.as_str()),
            (Some(false), "the answer has no JSON")
        );

        let globs = json!({ "files-changed": ["src/**", "README.md"] });
        assert_eq!(passed(globs.clone(), &outcome("", &[])), Some(false));
        assert_eq!(
            passed(
                globs.clone(),
                &outcome("", &["src/a.rs", "src/deep/b.rs", "README.md"])
            ),
            Some(true)
        );
        let outside = grade_now(
            &Expect::parse(&globs).unwrap(),
            &outcome("", &["src/a.rs", ".github/workflows/ci.yml"]),
        );
        assert_eq!(outside.passed, Some(false));
        assert!(
            outside.note.contains(".github/workflows/ci.yml"),
            "{}",
            outside.note
        );
    }

    #[test]
    fn a_judge_rubric_is_scored_by_a_model_never_graded() {
        let judged = grade_now(
            &Expect::parse(&json!({ "judge": "Clear and concise" })).unwrap(),
            &outcome("anything", &[]),
        );
        assert_eq!(judged.passed, None);
        assert!(judged.note.contains("never a gate"));
        assert!(
            !Expect::parse(&json!({ "judge": "x" }))
                .unwrap()
                .is_deterministic()
        );
    }

    #[tokio::test]
    async fn a_check_expectation_runs_the_folders_check_in_the_runs_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "ok").unwrap();
        let checks: BTreeMap<String, String> = [
            (
                "found".to_owned(),
                if cfg!(windows) {
                    "if exist marker.txt (exit 0) else (exit 1)"
                } else {
                    "test -f marker.txt"
                }
                .to_owned(),
            ),
            (
                "missing".to_owned(),
                if cfg!(windows) {
                    "if exist nothing.txt (exit 0) else (exit 1)"
                } else {
                    "test -f nothing.txt"
                }
                .to_owned(),
            ),
        ]
        .into();
        let run = Outcome {
            dir: Some(dir.path().to_path_buf()),
            ..Outcome::default()
        };
        let check = |name: &str| Expect::Check(name.into());
        assert_eq!(
            grade(&check("found"), &run, &checks).await.passed,
            Some(true)
        );
        let failed = grade(&check("missing"), &run, &checks).await;
        assert_eq!(failed.passed, Some(false));
        assert!(failed.note.contains("exit code 1"), "{}", failed.note);
        let unknown = grade(&check("lint"), &run, &checks).await;
        assert_eq!(unknown.passed, Some(false));
        assert!(unknown.note.contains("no check 'lint'"));
        let nowhere = grade(&check("found"), &Outcome::default(), &checks).await;
        assert_eq!(nowhere.passed, Some(false));
    }

    #[test]
    fn glob_patterns_match_paths_as_documented() {
        for (glob, path, matches) in [
            ("src/*.rs", "src/main.rs", true),
            ("src/*.rs", "src/deep/main.rs", false),
            ("src/**", "src/deep/main.rs", true),
            ("src/**/*.rs", "src/main.rs", true),
            ("src/**/*.rs", "src/a/b/c.rs", true),
            ("**/*.md", "README.md", true),
            ("docs/?.md", "docs/a.md", true),
            ("docs/?.md", "docs/ab.md", false),
            ("a+b.txt", "a+b.txt", true),
            ("a+b.txt", "aab.txt", false),
        ] {
            assert_eq!(glob_matches(glob, path), matches, "{glob} vs {path}");
        }
    }

    #[test]
    fn every_requirement_needs_a_case_with_a_deterministic_expectation() {
        let mut value = sample();
        value["evals"]["cases"][1]["expect"] =
            json!([{ "judge": "Each risk quotes its sentence" }]);
        let found = check_value(&value, &catalog());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].code, "uncovered_requirement");
        assert_eq!(found[0].path.as_deref(), Some("/requirements/1"));
        assert!(found[0].message.contains("R2"));
        assert!(
            found[0]
                .fix
                .as_deref()
                .unwrap()
                .contains("judges score, they never prove")
        );
    }

    #[test]
    fn cases_name_a_requirement_give_a_task_and_expect_what_the_agent_can_do() {
        let mut value = sample();
        value["evals"]["repetitions"] = json!(9);
        value["evals"]["thresholds"] = json!({ "pass_rate": 1.5 });
        value["evals"]["cases"] = json!([
            { "id": "E1", "requirement": "R1", "input": "Analyze q3.pdf", "fixtures": ["../q3.pdf"],
              "expect": [{ "contains": "margin" }, { "check": "lint" }, { "files-changed": ["out/**"] }, { "kind": "x" }],
              "timeout": "2h", "smoke": true },
            { "id": "E1", "requirement": "R9", "input": " ", "expect": [] },
            { "id": "2nd", "requirement": "R2", "input": "List the risks", "expect": [{ "regex": "risk" }], "timeout": "soon" }
        ]);
        let found = check_value(&value, &catalog());
        let codes: Vec<(&str, &str)> = found
            .iter()
            .map(|p| (p.code, p.path.as_deref().unwrap_or("")))
            .collect();
        for expected in [
            ("out_of_range", "/evals/repetitions"),
            ("out_of_range", "/evals/thresholds/pass_rate"),
            ("invalid_fixture", "/evals/cases/0/fixtures/0"),
            ("timeout_over_budget", "/evals/cases/0/timeout"),
            ("unknown_check", "/evals/cases/0/expect/1"),
            ("files_changed_needs_edit", "/evals/cases/0/expect/2"),
            ("invalid_expectation", "/evals/cases/0/expect/3"),
            ("duplicate_case", "/evals/cases/1/id"),
            ("unknown_requirement", "/evals/cases/1/requirement"),
            ("empty", "/evals/cases/1/input"),
            ("no_expectations", "/evals/cases/1/expect"),
            ("invalid_case_id", "/evals/cases/2/id"),
            ("invalid_timeout", "/evals/cases/2/timeout"),
        ] {
            assert!(codes.contains(&expected), "{expected:?} in {codes:?}");
        }

        let mut quiet = sample();
        for case in quiet["evals"]["cases"].as_array_mut().unwrap() {
            case.as_object_mut().unwrap().remove("smoke");
        }
        let found = check_value(&quiet, &catalog());
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].code, found[0].error), ("no_smoke_case", false));
    }

    #[test]
    fn fixtures_must_be_files_in_the_agents_fixtures_folder() {
        let blueprint = from_value(&sample()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let missing = check_fixtures(&blueprint.evals, dir.path());
        assert_eq!(missing.len(), 2, "both cases use q3.pdf: {missing:?}");
        assert_eq!(missing[0].code, "missing_fixture");
        assert_eq!(
            missing[0].path.as_deref(),
            Some("/evals/cases/0/fixtures/0")
        );
        std::fs::write(dir.path().join("q3.pdf"), b"%PDF").unwrap();
        assert!(check_fixtures(&blueprint.evals, dir.path()).is_empty());
        std::fs::remove_file(dir.path().join("q3.pdf")).unwrap();
        std::fs::create_dir(dir.path().join("q3.pdf")).unwrap();
        let folder = check_fixtures(&blueprint.evals, dir.path());
        assert!(folder[0].message.contains("isn't a file"), "{folder:?}");
    }

    #[cfg(unix)]
    #[test]
    fn fixtures_reached_through_symbolic_links_are_refused() {
        let blueprint = from_value(&sample()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("q3.pdf"), b"%PDF").unwrap();
        std::os::unix::fs::symlink(outside.path().join("q3.pdf"), dir.path().join("q3.pdf"))
            .unwrap();
        let linked = check_fixtures(&blueprint.evals, dir.path());
        assert!(linked[0].message.contains("symbolic link"), "{linked:?}");
    }
}
