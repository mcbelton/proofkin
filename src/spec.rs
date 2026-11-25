//! Spec-driven development: built-in, read-only checks of a feature's spec package,
//! so a spec, its REST API contract and its UX prototype are evidence before any
//! code is written. A package lives in a folder as:
//!
//! - `spec/SPEC.md`: problem, users, user stories (`US-1: As a ...`), testable
//!   acceptance criteria (`AC-1 (US-1): Given ..., when ..., then ...`),
//!   non-functional requirements (security and privacy included), out of scope;
//! - `spec/openapi.json`: the REST API contract (OpenAPI 3); every operation names
//!   the criteria it serves (`x-acceptance`), and its examples match its schemas;
//! - `prototype/`: a self-contained clickable UX prototype (`index.html`, no
//!   external resources, example data inline); screens mark the stories and
//!   criteria they show (`data-story="US-1"`, `data-ac="AC-1"`);
//! - `evidence/claims.json`: one claim per acceptance criterion (`"id": "AC-1"`),
//!   which development then proves.
//!
//! Plan steps run them as checks named `spec:spec`, `spec:api`, `spec:ux` and
//! `spec:trace`; the design package and the review pack have their own (see
//! [`crate::design`]). They only read files, so peers may ask for them at any access
//! level.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

/// The spec package's checks.
pub const SPEC_CHECKS: &[&str] = &["spec:spec", "spec:api", "spec:ux", "spec:trace"];

/// Every built-in check, by name: the spec package's, the design package's, the review
/// pack's and an agent's kept as code (see [`crate::factory::code`]).
pub const BUILT_IN_CHECKS: &[&str] = &[
    "spec:spec",
    "spec:api",
    "spec:ux",
    "spec:trace",
    "design:arch",
    "design:data",
    "design:threats",
    "design:tests",
    "design:ops",
    "review:decks",
    "review:rehearsal",
    "agent:blueprint",
    "agent:evals",
    "agent:report",
];

pub fn is_built_in(check: &str) -> bool {
    BUILT_IN_CHECKS.contains(&check)
}

/// The built-in checks of group `name` (`spec` -> `spec:spec`, `spec:api`, ...), none
/// if it isn't a group.
pub fn group(name: &str) -> Vec<&'static str> {
    BUILT_IN_CHECKS
        .iter()
        .copied()
        .filter(|c| c.split_once(':').is_some_and(|(g, _)| g == name))
        .collect()
}

/// A hint for checks named like a group of built-in ones (`verify=spec`), which no
/// folder has: what the built-in checks are called.
pub fn group_hint(missing: &[&str]) -> String {
    missing
        .iter()
        .filter_map(|m| {
            let checks = group(m);
            (!checks.is_empty()).then(|| {
                format!(
                    "; the built-in {m} checks are {} (verify={})",
                    checks.join(", "),
                    checks.join(",")
                )
            })
        })
        .collect()
}

/// What built-in check `check` proves, in a line (for the plan builder).
pub fn about(check: &str) -> &'static str {
    match check {
        "spec:spec" => "spec/SPEC.md has user stories with testable acceptance criteria",
        "spec:api" => {
            "spec/openapi.json is a REST API contract serving the acceptance criteria, with examples"
        }
        "spec:ux" => "prototype/index.html is a clickable prototype of the stories",
        "spec:trace" => "evidence/claims.json has a claim for every acceptance criterion",
        "design:arch" => "design/ has the architecture, workflows and ADRs, traced to the spec",
        "design:data" => "design/ has the data dictionary and data flows with their protection",
        "design:threats" => "design/threat-model.md has STRIDE threats with mitigations",
        "design:tests" => {
            "design/test-plan.md tests every acceptance criterion and high-risk threat"
        }
        "design:ops" => "design/operations.md has rollout, rollback, SLOs, alerts and runbooks",
        "review:decks" => "review/ has a document for each review audience in its own form",
        "review:rehearsal" => "review/rehearsal.md has each audience's likely verdict and answers",
        "agent:blueprint" => "agent/blueprint.json is a valid agent blueprint, its skills there",
        "agent:evals" => {
            "agent/evals.json proves every requirement deterministically, with its fixtures"
        }
        "agent:report" => {
            "agent/reports/ has a passed, verified evaluation of the folder's agent as built"
        }
        _ => "",
    }
}

/// The outcome of a check: what is wrong (nothing if it passes) and what was seen.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub problems: Vec<String>,
    pub notes: Vec<String>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.problems.is_empty()
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        for n in &self.notes {
            out += &format!("ok: {n}\n");
        }
        for p in &self.problems {
            out += &format!("PROBLEM: {p}\n");
        }
        out += if self.passed() {
            "passed\n"
        } else {
            "failed\n"
        };
        out
    }
}

/// Runs built-in check `name` (one of [`BUILT_IN_CHECKS`]) on folder `dir`.
pub fn run(name: &str, dir: &Path) -> Report {
    match name {
        "spec:spec" => check_spec(dir),
        "spec:api" => check_api(dir),
        "spec:ux" => check_ux(dir),
        "spec:trace" => check_trace(dir),
        _ if name.starts_with("design:") || name.starts_with("review:") => {
            crate::design::run(name, dir)
        }
        _ if name.starts_with("agent:") => crate::factory::code::run(name, dir),
        _ => Report {
            problems: vec![format!("unknown built-in check {name}")],
            notes: vec![],
        },
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Spec {
    pub sections: Vec<String>,
    pub stories: BTreeMap<String, String>,
    /// id -> (stories it refines, text)
    pub criteria: BTreeMap<String, (Vec<String>, String)>,
    pub nonfunctional: String,
    pub duplicates: Vec<String>,
}

pub(crate) fn id_with(prefix: &str, text: &str) -> Option<String> {
    let rest = text.strip_prefix(prefix)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (!digits.is_empty()).then(|| format!("{prefix}{digits}"))
}

/// Ids like `US-3` mentioned anywhere in `text`.
pub(crate) fn mentions(prefix: &str, text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(prefix) {
        if let Some(id) = id_with(prefix, &rest[i..]) {
            ids.push(id);
        }
        rest = &rest[i + prefix.len()..];
    }
    ids
}

pub fn parse_spec(text: &str) -> Spec {
    let mut spec = Spec::default();
    let mut section = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(h) = trimmed.strip_prefix("## ") {
            section = h.trim().to_lowercase();
            spec.sections.push(section.clone());
            continue;
        }
        let item = trimmed
            .trim_start_matches(['-', '*', ' '])
            .trim_start_matches("**");
        if section.contains("non-functional") {
            spec.nonfunctional.push_str(trimmed);
            spec.nonfunctional.push('\n');
        }
        if section.contains("user stories") {
            if let Some(id) = id_with("US-", item) {
                let text = item[id.len()..]
                    .trim_start_matches(['*', ':', ' '])
                    .to_owned();
                if spec.stories.insert(id.clone(), text).is_some() {
                    spec.duplicates.push(id);
                }
            }
        } else if section.contains("acceptance criteria")
            && let Some(id) = id_with("AC-", item)
        {
            let rest = &item[id.len()..];
            let (refs, text) = match rest.trim_start().strip_prefix('(') {
                Some(r) => match r.split_once(')') {
                    Some((inside, after)) => (mentions("US-", inside), after),
                    None => (Vec::new(), rest),
                },
                None => (Vec::new(), rest),
            };
            let text = text.trim_start_matches(['*', ':', ' ']).to_owned();
            if spec.criteria.insert(id.clone(), (refs, text)).is_some() {
                spec.duplicates.push(id);
            }
        }
    }
    spec
}

/// Largest file the checks read.
pub(crate) const MAX_FILE: u64 = 2 * 1024 * 1024;

/// Reads `rel` inside `dir` without following symbolic links (built-in checks run in
/// this process, outside any sandbox, so a link must not lead them out of the folder).
pub(crate) fn read_inside(dir: &Path, rel: &str) -> Result<String, String> {
    read_inside_up_to(dir, rel, MAX_FILE)
}

/// [`read_inside`], for files up to `max` bytes.
pub(crate) fn read_inside_up_to(dir: &Path, rel: &str, max: u64) -> Result<String, String> {
    let mut path = dir.to_path_buf();
    for part in Path::new(rel).components() {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!(
                    "{rel} is a symbolic link, which built-in checks don't follow"
                ));
            }
            Ok(_) => {}
            Err(_) => return Err(format!("{rel} is missing")),
        }
    }
    match std::fs::metadata(&path) {
        Ok(m) if m.len() > max => Err(format!("{rel} is larger than {max} bytes")),
        _ => std::fs::read_to_string(&path).map_err(|e| format!("{rel} can't be read: {e}")),
    }
}

pub(crate) fn read(dir: &Path, rel: &str, report: &mut Report) -> Option<String> {
    match read_inside(dir, rel) {
        Ok(text) => Some(text),
        Err(problem) => {
            report.problems.push(problem);
            None
        }
    }
}

pub(crate) fn load_spec(dir: &Path, report: &mut Report) -> Option<Spec> {
    read(dir, "spec/SPEC.md", report).map(|t| parse_spec(&t))
}

pub fn check_spec(dir: &Path) -> Report {
    let mut r = Report::default();
    let Some(spec) = load_spec(dir, &mut r) else {
        return r;
    };
    for needed in [
        "problem",
        "users",
        "user stories",
        "acceptance criteria",
        "non-functional",
        "out of scope",
    ] {
        if !spec.sections.iter().any(|s| s.contains(needed)) {
            r.problems
                .push(format!("SPEC.md has no '## {needed}' section"));
        }
    }
    if spec.stories.is_empty() {
        r.problems.push(
            "no user stories (lines like `- US-1: As a ..., I want ..., so that ...`)".into(),
        );
    }
    if spec.criteria.is_empty() {
        r.problems.push(
            "no acceptance criteria (lines like `- AC-1 (US-1): Given ..., when ..., then ...`)"
                .into(),
        );
    }
    for id in &spec.duplicates {
        r.problems.push(format!("{id} is defined twice"));
    }
    for (id, (refs, text)) in &spec.criteria {
        if refs.is_empty() {
            r.problems.push(format!(
                "{id} names no user story: write `{id} (US-n): ...`"
            ));
        }
        for story in refs.iter().filter(|s| !spec.stories.contains_key(*s)) {
            r.problems
                .push(format!("{id} refers to unknown story {story}"));
        }
        let lower = text.to_lowercase();
        if !(lower.contains("given") && lower.contains("then")) && !lower.contains("must") {
            r.problems.push(format!(
                "{id} isn't testable: write it as Given/When/Then or with 'must'"
            ));
        }
    }
    for story in spec.stories.keys() {
        if !spec.criteria.values().any(|(refs, _)| refs.contains(story)) {
            r.problems
                .push(format!("{story} has no acceptance criterion"));
        }
    }
    let nf = spec.nonfunctional.to_lowercase();
    for topic in ["security", "privacy"] {
        if !nf.contains(topic) {
            r.problems.push(format!(
                "the non-functional requirements don't cover {topic}"
            ));
        }
    }
    r.notes.push(format!(
        "{} user stories, {} acceptance criteria",
        spec.stories.len(),
        spec.criteria.len()
    ));
    r
}

const METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];

/// Follows local `$ref`s (`#/components/...`) to what they point to.
fn resolve<'a>(schema: &'a Value, root: &'a Value) -> &'a Value {
    let mut s = schema;
    for _ in 0..16 {
        match s
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix('#'))
            .and_then(|pointer| root.pointer(pointer))
        {
            Some(target) => s = target,
            None => return s,
        }
    }
    s
}

/// Where `value` doesn't fit the JSON schema `schema` (types, required properties,
/// enums, local `$ref`s), a line each; none when it fits.
pub(crate) fn schema_mismatches(value: &Value, schema: &Value) -> Vec<String> {
    let mut out = Vec::new();
    mismatches(value, schema, schema, "$", 0, &mut out);
    out
}

/// [`schema_mismatches`] for a schema whose `$ref`s point into the OpenAPI document `root`:
/// an API's answer against its contract (contract tests).
pub fn contract_mismatches(value: &Value, schema: &Value, root: &Value) -> Vec<String> {
    let mut out = Vec::new();
    mismatches(value, schema, root, "$", 0, &mut out);
    out
}

/// Where `value` doesn't match `schema` (types, required properties, enums).
fn mismatches(
    value: &Value,
    schema: &Value,
    root: &Value,
    at: &str,
    depth: usize,
    out: &mut Vec<String>,
) {
    if depth > 12 {
        return;
    }
    let schema = resolve(schema, root);
    if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
        out.push(format!("{at}: unresolved $ref {r}"));
        return;
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        out.push(format!("{at}: {value} isn't one of the allowed values"));
    }
    let ok = match schema.get("type").and_then(Value::as_str) {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("number") => value.is_number(),
        Some("boolean") => value.is_boolean(),
        _ => true,
    };
    if !ok && !(value.is_null() && schema.get("nullable") == Some(&Value::Bool(true))) {
        out.push(format!(
            "{at}: expected {}, the example has {value}",
            schema["type"].as_str().unwrap_or("?")
        ));
        return;
    }
    if let (Some(obj), Some(props)) = (
        value.as_object(),
        schema.get("properties").and_then(Value::as_object),
    ) {
        for req in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(name) = req.as_str()
                && !obj.contains_key(name)
            {
                out.push(format!("{at}: required property '{name}' is missing"));
            }
        }
        for (name, v) in obj {
            if let Some(p) = props.get(name) {
                mismatches(v, p, root, &format!("{at}.{name}"), depth + 1, out);
            }
        }
    }
    if let (Some(items), Some(item_schema)) = (value.as_array(), schema.get("items")) {
        for (i, v) in items.iter().take(5).enumerate() {
            mismatches(v, item_schema, root, &format!("{at}[{i}]"), depth + 1, out);
        }
    }
}

/// Examples of a JSON media type: `example`, or every `examples.*.value`.
fn examples(media: &Value) -> Vec<&Value> {
    if let Some(e) = media.get("example") {
        return vec![e];
    }
    media
        .get("examples")
        .and_then(Value::as_object)
        .map(|m| m.values().filter_map(|e| e.get("value")).collect())
        .unwrap_or_default()
}

fn check_media(media: &Value, root: &Value, at: &str, r: &mut Report) {
    let Some(schema) = media.get("schema") else {
        r.problems.push(format!("{at} has no schema"));
        return;
    };
    let found = examples(media);
    if found.is_empty() {
        r.problems.push(format!("{at} has no example"));
    }
    for (i, e) in found.into_iter().enumerate() {
        let mut out = Vec::new();
        mismatches(
            e,
            schema,
            root,
            &format!("{at} example {}", i + 1),
            0,
            &mut out,
        );
        r.problems.extend(out);
    }
}

/// The API contract's operations: (method path, operation).
pub(crate) fn operations(api: &Value) -> Vec<(String, &Value)> {
    let mut ops = Vec::new();
    for (path, item) in api
        .get("paths")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        for m in METHODS {
            if let Some(op) = item.get(*m) {
                ops.push((format!("{} {path}", m.to_uppercase()), op));
            }
        }
    }
    ops
}

pub(crate) fn load_api(dir: &Path, r: &mut Report) -> Option<Value> {
    let text = read(dir, "spec/openapi.json", r)?;
    match serde_json::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            r.problems
                .push(format!("spec/openapi.json isn't valid JSON: {e}"));
            None
        }
    }
}

pub fn check_api(dir: &Path) -> Report {
    let mut r = Report::default();
    let Some(api) = load_api(dir, &mut r) else {
        return r;
    };
    if !api
        .get("openapi")
        .and_then(Value::as_str)
        .is_some_and(|v| v.starts_with("3."))
    {
        r.problems.push("'openapi' must be a 3.x version".into());
    }
    for field in ["title", "version"] {
        if api
            .pointer(&format!("/info/{field}"))
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            r.problems.push(format!("info.{field} is missing"));
        }
    }
    let ops = operations(&api);
    if ops.is_empty() {
        r.problems
            .push("the contract has no operations under 'paths'".into());
    }
    let mut ids = BTreeSet::new();
    let mut secured = api.get("security").is_some();
    for (name, op) in &ops {
        match op.get("operationId").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => {
                if !ids.insert(id.to_owned()) {
                    r.problems
                        .push(format!("{name}: operationId {id} is used twice"));
                }
            }
            _ => r.problems.push(format!("{name}: no operationId")),
        }
        if op
            .get("summary")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            r.problems.push(format!("{name}: no summary"));
        }
        if op
            .get("x-acceptance")
            .and_then(Value::as_array)
            .is_none_or(|a| a.iter().all(|v| v.as_str().is_none_or(str::is_empty)))
        {
            r.problems.push(format!(
                "{name}: no x-acceptance (the acceptance criteria it serves, e.g. [\"AC-1\"])"
            ));
        }
        secured |= op.get("security").is_some();
        let responses = op.get("responses").and_then(Value::as_object);
        let codes: Vec<&String> = responses.map(|m| m.keys().collect()).unwrap_or_default();
        if !codes.iter().any(|c| c.starts_with('2')) {
            r.problems
                .push(format!("{name}: no success (2xx) response"));
        }
        if !codes.iter().any(|c| c.starts_with('4')) {
            r.problems
                .push(format!("{name}: no client error (4xx) response"));
        }
        for (code, resp) in responses.into_iter().flatten() {
            let resp = resolve(resp, &api);
            if let Some(dangling) = resp.get("$ref").and_then(Value::as_str) {
                r.problems.push(format!(
                    "{name} response {code}: unresolved $ref {dangling}"
                ));
            }
            if let Some(media) = resp.pointer("/content/application~1json") {
                check_media(media, &api, &format!("{name} response {code}"), &mut r);
            }
        }
        if name.starts_with("POST") || name.starts_with("PUT") || name.starts_with("PATCH") {
            let body = op.get("requestBody").map(|b| resolve(b, &api));
            match body.and_then(|b| b.pointer("/content/application~1json")) {
                Some(media) => check_media(media, &api, &format!("{name} request"), &mut r),
                None => r.problems.push(format!("{name}: no JSON request body")),
            }
        }
    }
    if secured
        && api
            .pointer("/components/securitySchemes")
            .and_then(Value::as_object)
            .is_none_or(|m| m.is_empty())
    {
        r.problems
            .push("operations use 'security' but components.securitySchemes is empty".into());
    }
    r.notes.push(format!("{} operations", ops.len()));
    r
}

/// The prototype's HTML files (path relative to `prototype/`, text).
fn prototype_pages(dir: &Path) -> Vec<(String, String)> {
    let root = dir.join("prototype");
    if read_inside(dir, "prototype").is_err_and(|e| e.contains("symbolic link")) {
        return Vec::new();
    }
    let mut pages = Vec::new();
    let mut stack = vec![root.clone()];
    let mut seen = 0;
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > 2_000 {
                break;
            }
            let path = entry.path();
            // file_type() doesn't follow links: links are skipped.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file()
                && path.extension().is_some_and(|e| e == "html" || e == "htm")
                && entry.metadata().is_ok_and(|m| m.len() <= MAX_FILE)
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                pages.push((rel, text));
            }
        }
    }
    pages.sort();
    pages
}

/// Ids in `data-story="..."` / `data-ac="..."` attributes.
fn marked(attr: &str, prefix: &str, html: &str) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for quote in ['"', '\''] {
        let needle = format!("{attr}={quote}");
        let mut rest = html;
        while let Some(i) = rest.find(&needle) {
            let after = &rest[i + needle.len()..];
            let value = after.split(quote).next().unwrap_or_default();
            ids.extend(mentions(prefix, value));
            rest = after;
        }
    }
    ids
}

pub fn check_ux(dir: &Path) -> Report {
    let mut r = Report::default();
    let pages = prototype_pages(dir);
    if !pages.iter().any(|(p, _)| p == "index.html") {
        r.problems.push("prototype/index.html is missing".into());
    }
    let mut stories = BTreeSet::new();
    for (path, html) in &pages {
        let lower = html.to_lowercase();
        for bad in [
            "src=\"http",
            "src='http",
            "url(http",
            "url('http",
            "url(\"http",
            "src=\"//",
            "@import",
        ] {
            if lower.contains(bad) {
                r.problems.push(format!(
                    "prototype/{path} loads an external resource ({bad}...): keep it self-contained"
                ));
            }
        }
        for line in lower
            .lines()
            .filter(|l| l.contains("<link") && l.contains("http"))
        {
            r.problems.push(format!(
                "prototype/{path} links an external resource: {}",
                line.trim()
            ));
        }
        if lower.contains("fetch(") || lower.contains("xmlhttprequest") {
            r.problems.push(format!(
                "prototype/{path} calls a server: use inline example data instead"
            ));
        }
        stories.extend(marked("data-story", "US-", html));
    }
    let mut ignored = Report::default();
    if let Some(spec) = load_spec(dir, &mut ignored) {
        for story in spec.stories.keys().filter(|s| !stories.contains(*s)) {
            r.problems.push(format!(
                "no prototype screen shows {story} (mark it with data-story=\"{story}\")"
            ));
        }
    } else {
        r.problems
            .push("spec/SPEC.md is missing, so the stories to show are unknown".into());
    }
    r.notes.push(format!(
        "{} prototype pages, {} stories shown",
        pages.len(),
        stories.len()
    ));
    r
}

pub fn check_trace(dir: &Path) -> Report {
    let mut r = Report::default();
    let Some(spec) = load_spec(dir, &mut r) else {
        return r;
    };
    let mut covered: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut quiet = Report::default();
    if let Some(api) = load_api(dir, &mut quiet) {
        for (name, op) in operations(&api) {
            for id in op
                .get("x-acceptance")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(id) = id.as_str() {
                    covered.entry(id.to_owned()).or_default().push(name.clone());
                }
            }
        }
    }
    let pages = prototype_pages(dir);
    for (path, html) in &pages {
        for id in marked("data-ac", "AC-", html) {
            covered
                .entry(id)
                .or_default()
                .push(format!("prototype/{path}"));
        }
        for story in marked("data-story", "US-", html) {
            if !spec.stories.contains_key(&story) {
                r.problems.push(format!(
                    "prototype/{path} shows {story}, which the spec doesn't define"
                ));
            }
        }
    }
    for (id, places) in &covered {
        if !spec.criteria.contains_key(id) {
            r.problems.push(format!(
                "{} refer to {id}, which the spec doesn't define",
                places.join(", ")
            ));
        }
    }
    for id in spec.criteria.keys() {
        if !covered.contains_key(id) {
            r.problems.push(format!(
                "{id} is covered by neither the API (x-acceptance) nor the prototype (data-ac)"
            ));
        }
    }
    let claims: BTreeSet<String> = read(dir, "evidence/claims.json", &mut r)
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("claims").cloned())
        .and_then(|c| c.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    for id in spec.criteria.keys().filter(|id| !claims.contains(*id)) {
        r.problems
            .push(format!("evidence/claims.json has no claim {id}"));
    }
    r.notes.push(format!(
        "{} of {} acceptance criteria covered and claimed",
        spec.criteria
            .keys()
            .filter(|id| covered.contains_key(*id) && claims.contains(*id))
            .count(),
        spec.criteria.len()
    ));
    r
}

const STARTER_SPEC: &str = include_str!("../spec-starter/SPEC.md");
const STARTER_API: &str = include_str!("../spec-starter/openapi.json");
const STARTER_PROTOTYPE: &str = include_str!("../spec-starter/index.html");

pub(crate) fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// A starter spec package titled `title` that passes every check: (path, content).
pub fn starter(title: &str) -> Vec<(&'static str, String)> {
    let title = title.trim();
    let quoted = serde_json::to_string(title).unwrap_or_else(|_| "\"\"".into());
    let json_title = &quoted[1..quoted.len() - 1];
    vec![
        ("spec/SPEC.md", STARTER_SPEC.replace("{{title}}", title)),
        (
            "spec/openapi.json",
            STARTER_API.replace("{{title}}", json_title),
        ),
        (
            "prototype/index.html",
            STARTER_PROTOTYPE.replace("{{title}}", &escape_html(title)),
        ),
    ]
}

/// Writes the starter package into `dir`, and a claim for each of its acceptance
/// criteria into `evidence/claims.json` (added to any claims already there). Refuses
/// to overwrite files unless `force`. Returns what it wrote.
pub fn init(dir: &Path, title: &str, force: bool) -> anyhow::Result<Vec<String>> {
    use anyhow::{Context, bail};
    if title.trim().is_empty() || title.contains(['\n', '\r']) {
        bail!("the title must be a single, non-empty line");
    }
    let files = starter(title);
    if !force && let Some((path, _)) = files.iter().find(|(p, _)| dir.join(p).exists()) {
        bail!("{path} exists already (use --force to replace the spec package)");
    }
    let mut written = Vec::new();
    for (path, text) in &files {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().expect("has a parent"))?;
        std::fs::write(&full, text).with_context(|| format!("writing {}", full.display()))?;
        written.push((*path).to_owned());
    }
    let claims_path = dir.join("evidence/claims.json");
    let mut doc: Value = match std::fs::read_to_string(&claims_path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} isn't valid JSON", claims_path.display()))?,
        Err(_) => serde_json::json!({ "claims": [] }),
    };
    let claims = doc
        .get_mut("claims")
        .and_then(Value::as_array_mut)
        .context("evidence/claims.json has no \"claims\" list")?;
    let spec = parse_spec(&files[0].1);
    let mut added = 0;
    for (id, (_, text)) in &spec.criteria {
        if !claims
            .iter()
            .any(|c| c.get("id").and_then(Value::as_str) == Some(id))
        {
            claims.push(serde_json::json!({ "id": id, "claim": text, "tests": [] }));
            added += 1;
        }
    }
    if added > 0 {
        std::fs::create_dir_all(claims_path.parent().expect("has a parent"))?;
        std::fs::write(&claims_path, serde_json::to_string_pretty(&doc)? + "\n")?;
        written.push(format!("evidence/claims.json ({added} claims added)"));
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = "# Shop: wishlist\n\n## Problem\nUsers lose items.\n\n## Users\nShoppers.\n\n\
        ## User stories\n- US-1: As a shopper, I want to save an item, so that I can buy it later.\n\
        - US-2: As a shopper, I want to see my wishlist.\n\n## Acceptance criteria\n\
        - AC-1 (US-1): Given a product page, when I press Save, then the item is in my wishlist.\n\
        - AC-2 (US-2): The wishlist must list saved items newest first.\n\n\
        ## Non-functional requirements\n- Security: only the owner sees a wishlist.\n- Privacy: no tracking of saved items.\n\n\
        ## Out of scope\n- Sharing.\n";

    fn api() -> Value {
        serde_json::json!({
            "openapi": "3.0.3",
            "info": { "title": "Wishlist", "version": "1.0.0" },
            "components": {
                "schemas": { "Item": { "type": "object", "required": ["id", "name"],
                    "properties": { "id": { "type": "integer" }, "name": { "type": "string" } } } },
                "securitySchemes": { "bearer": { "type": "http", "scheme": "bearer" } }
            },
            "security": [{ "bearer": [] }],
            "paths": { "/wishlist": {
                "get": { "operationId": "listWishlist", "summary": "List saved items", "x-acceptance": ["AC-2"],
                    "responses": {
                        "200": { "description": "ok", "content": { "application/json": {
                            "schema": { "type": "array", "items": { "$ref": "#/components/schemas/Item" } },
                            "example": [{ "id": 1, "name": "Lamp" }] } } },
                        "401": { "description": "not signed in" } } },
                "post": { "operationId": "saveItem", "summary": "Save an item", "x-acceptance": ["AC-1"],
                    "requestBody": { "content": { "application/json": {
                        "schema": { "$ref": "#/components/schemas/Item" }, "example": { "id": 2, "name": "Rug" } } } },
                    "responses": { "201": { "description": "saved" }, "400": { "description": "bad item" } } }
            } }
        })
    }

    const PROTOTYPE: &str = "<!doctype html><html><body>\
        <section data-story=\"US-1\" data-ac=\"AC-1\"><button>Save</button></section>\
        <section data-story=\"US-2\"><ul><li>Lamp</li></ul></section></body></html>";

    fn package() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["spec", "prototype", "evidence"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        std::fs::write(dir.path().join("spec/SPEC.md"), SPEC).unwrap();
        std::fs::write(dir.path().join("spec/openapi.json"), api().to_string()).unwrap();
        std::fs::write(dir.path().join("prototype/index.html"), PROTOTYPE).unwrap();
        std::fs::write(
            dir.path().join("evidence/claims.json"),
            r#"{"claims":[{"id":"AC-1","claim":"save","tests":[]},{"id":"AC-2","claim":"list","tests":[]}]}"#,
        )
        .unwrap();
        dir
    }

    #[test]
    fn a_complete_spec_package_passes_every_check() {
        let dir = package();
        for name in SPEC_CHECKS {
            let report = run(name, dir.path());
            assert!(report.passed(), "{name}: {}", report.text());
        }
        assert!(
            check_trace(dir.path())
                .text()
                .contains("2 of 2 acceptance criteria covered and claimed")
        );
    }

    #[test]
    fn spec_checks_find_untestable_or_untraced_requirements() {
        let dir = package();
        let p = dir.path();
        let has = |r: Report, text: &str| {
            assert!(
                r.problems.iter().any(|x| x.contains(text)),
                "{text:?} not in {:?}",
                r.problems
            )
        };

        std::fs::write(
            p.join("spec/SPEC.md"),
            SPEC.replace(
                "- AC-2 (US-2): The wishlist must list saved items newest first.\n",
                "- AC-2: Nice wishlist.\n",
            )
            .replace("- Privacy: no tracking of saved items.\n", ""),
        )
        .unwrap();
        let r = check_spec(p);
        has(r.clone(), "AC-2 names no user story");
        has(r.clone(), "AC-2 isn't testable");
        has(r.clone(), "US-2 has no acceptance criterion");
        has(r, "don't cover privacy");
        std::fs::write(p.join("spec/SPEC.md"), SPEC).unwrap();

        let mut bad = api();
        bad["paths"]["/wishlist"]["get"]["responses"]["200"]["content"]["application/json"]["example"] =
            serde_json::json!([{ "id": "one" }]);
        for method in ["get", "post"] {
            bad["paths"]["/wishlist"][method]
                .as_object_mut()
                .unwrap()
                .remove("x-acceptance");
        }
        bad["paths"]["/wishlist"]["post"]["responses"]
            .as_object_mut()
            .unwrap()
            .remove("400");
        std::fs::write(p.join("spec/openapi.json"), bad.to_string()).unwrap();
        let r = check_api(p);
        has(r.clone(), "example 1[0].id: expected integer");
        has(r.clone(), "required property 'name' is missing");
        has(r.clone(), "POST /wishlist: no x-acceptance");
        has(r, "POST /wishlist: no client error (4xx) response");
        let t = check_trace(p);
        has(t.clone(), "AC-2 is covered by neither the API");
        assert!(
            !t.problems.iter().any(|x| x.starts_with("AC-1 ")),
            "the prototype covers AC-1"
        );

        std::fs::write(p.join("spec/openapi.json"), api().to_string()).unwrap();
        std::fs::write(p.join("prototype/index.html"), "<html><script src=\"https://cdn.example/x.js\"></script><section data-story=\"US-1\" data-ac=\"AC-9\"></section><script>fetch('/api')</script></html>").unwrap();
        let r = check_ux(p);
        has(r.clone(), "loads an external resource");
        has(r.clone(), "calls a server");
        has(r, "no prototype screen shows US-2");
        has(check_trace(p), "AC-9, which the spec doesn't define");

        std::fs::write(p.join("prototype/index.html"), PROTOTYPE).unwrap();
        std::fs::write(
            p.join("evidence/claims.json"),
            r#"{"claims":[{"id":"AC-1"}]}"#,
        )
        .unwrap();
        has(check_trace(p), "evidence/claims.json has no claim AC-2");
        std::fs::remove_file(p.join("spec/SPEC.md")).unwrap();
        has(check_spec(p), "spec/SPEC.md is missing");
        assert!(!run("spec:nope", p).passed());
    }

    #[test]
    fn the_starter_package_passes_every_check_and_keeps_existing_claims() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir(p.join("evidence")).unwrap();
        std::fs::write(
            p.join("evidence/claims.json"),
            r#"{"claims":[{"id":"BUILD-1","claim":"it builds","tests":["lib::x"]}]}"#,
        )
        .unwrap();
        let written = init(p, "Team <Tasks> \"v1\"", false).unwrap();
        assert!(
            written
                .iter()
                .any(|w| w == "evidence/claims.json (4 claims added)"),
            "{written:?}"
        );
        for name in SPEC_CHECKS {
            let report = run(name, p);
            assert!(report.passed(), "{name}: {}", report.text());
        }
        let claims: Value =
            serde_json::from_str(&std::fs::read_to_string(p.join("evidence/claims.json")).unwrap())
                .unwrap();
        assert_eq!(claims["claims"][0]["id"], "BUILD-1");
        assert_eq!(claims["claims"].as_array().unwrap().len(), 5);
        let html = std::fs::read_to_string(p.join("prototype/index.html")).unwrap();
        assert!(html.contains("<h1>Team &lt;Tasks&gt; &quot;v1&quot;</h1>"));
        assert!(
            init(p, "Again", false)
                .unwrap_err()
                .to_string()
                .contains("exists already")
        );
        assert!(init(p, "Again", true).is_ok());
    }

    #[test]
    fn built_in_checks_do_not_follow_symbolic_links() {
        let dir = package();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.md");
        std::fs::write(&secret, SPEC).unwrap();
        let link = dir.path().join("spec/SPEC.md");
        std::fs::remove_file(&link).unwrap();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&secret, &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&secret, &link).is_ok();
        if !made {
            eprintln!("skipped: this account can't create symbolic links");
            return;
        }
        let r = check_spec(dir.path());
        assert!(
            r.problems.iter().any(|p| p.contains("symbolic link")),
            "{:?}",
            r.problems
        );
    }
}
