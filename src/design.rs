//! The design package and the review pack: built-in, read-only checks that a feature's
//! design is complete (the SDLC artifacts reviewers need) and traced to its spec,
//! and that each review audience gets a document in its own form.
//!
//! The design package, in `design/` next to the spec package (see [`crate::spec`]):
//!
//! | File | Check |
//! | --- | --- |
//! | `architecture.md`: context, components (with diagrams), domain model, deployment, quality attributes, decisions | `design:arch` |
//! | `workflows.md`: a UML sequence diagram per workflow, state or activity diagrams; every user story covered | `design:arch` |
//! | `adr/ADR-0001-title.md`: architecture decision records (status, context, decision, alternatives, consequences) | `design:arch` |
//! | `data-dictionary.md`: every field (the API's included) with type, classification, purpose and retention | `design:data` |
//! | `dataflow.md`: the data flow diagram with trust boundaries, and a table of flows (`DF-1`) and their protection | `design:data` |
//! | `threat-model.md`: assets, trust boundaries, STRIDE threats (`TM-1`) with mitigations, risk and status | `design:threats` |
//! | `test-plan.md`: a test for every acceptance criterion and high-risk threat | `design:tests` |
//! | `operations.md`: rollout, rollback, SLOs, metrics, dashboards, alerts and runbooks | `design:ops` |
//!
//! Diagrams are Mermaid (they render on GitHub and in VS Code). The review pack, in
//! `review/`: Marp decks for the technical, product and leadership reviews, and a
//! customer feedback document (`review:decks`); and the rehearsal of the reviews,
//! `rehearsal.md`, with each audience's likely verdict, the actions their concerns
//! lead to and prepared answers (`review:rehearsal`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use crate::render::Slide;
use crate::spec::{Report, id_with, load_api, load_spec, mentions, operations, read};

/// Runs built-in check `name` (`design:*` or `review:*`) on folder `dir`.
pub fn run(name: &str, dir: &Path) -> Report {
    match name {
        "design:arch" => check_arch(dir),
        "design:data" => check_data(dir),
        "design:threats" => check_threats(dir),
        "design:tests" => check_tests(dir),
        "design:ops" => check_ops(dir),
        "review:decks" => check_review(dir),
        "review:rehearsal" => check_rehearsal(dir),
        _ => Report {
            problems: vec![format!("unknown built-in check {name}")],
            notes: vec![],
        },
    }
}

// ---------- Markdown structure ----------

#[derive(Debug, Default)]
struct Doc {
    /// (level, lowercased title, body text outside tables and code).
    sections: Vec<(usize, String, String)>,
    tables: Vec<Table>,
    diagrams: Vec<Diagram>,
}

#[derive(Debug, Default, Clone)]
struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone)]
struct Diagram {
    kind: String,
    /// The heading it is under (lowercased).
    section: String,
    lines: Vec<String>,
}

fn cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_owned()).collect()
}

fn is_separator(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

fn normalize(text: &str) -> String {
    text.to_lowercase().replace(['’', '‘'], "'")
}

const DIAGRAM_KINDS: &[&str] = &[
    "flowchart",
    "sequenceDiagram",
    "classDiagram",
    "stateDiagram",
    "erDiagram",
    "c4",
    "journey",
    "gantt",
    "pie",
    "mindmap",
    "timeline",
    "requirementDiagram",
    "quadrantChart",
    "gitGraph",
    "sankey-beta",
    "xychart-beta",
    "block-beta",
    "packet-beta",
    "architecture-beta",
];

fn diagram_kind(body: &[String]) -> String {
    let mut lines = body
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with("%%"))
        .peekable();
    if lines.peek() == Some(&"---") {
        lines.next();
        for line in lines.by_ref() {
            if line == "---" {
                break;
            }
        }
    }
    let word = lines
        .next()
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or("");
    match word {
        "graph" | "flowchart" => "flowchart",
        "stateDiagram" | "stateDiagram-v2" => "stateDiagram",
        w if w.starts_with("C4") => "c4",
        other => other,
    }
    .to_owned()
}

fn parse_doc(text: &str) -> Doc {
    let mut doc = Doc::default();
    doc.sections.push((0, String::new(), String::new()));
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim_start();
        let section = doc.sections.last().map(|s| s.1.clone()).unwrap_or_default();
        if t.starts_with("```") || t.starts_with("~~~") {
            let (fence, lang) = (&t[..3], t[3..].trim().to_lowercase());
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with(fence) {
                body.push(lines[i].to_owned());
                i += 1;
            }
            i += 1;
            if lang.starts_with("mermaid") {
                doc.diagrams.push(Diagram {
                    kind: diagram_kind(&body),
                    section,
                    lines: body,
                });
            }
            continue;
        }
        if t.starts_with('#') {
            let level = t.chars().take_while(|c| *c == '#').count();
            doc.sections.push((
                level,
                normalize(t.trim_start_matches('#').trim()),
                String::new(),
            ));
        } else if t.starts_with('|') && lines.get(i + 1).is_some_and(|n| is_separator(n)) {
            let headers = cells(t)
                .into_iter()
                .map(|h| normalize(&h.replace(['*', '`'], "")))
                .collect();
            let mut rows = Vec::new();
            i += 2;
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                rows.push(cells(lines[i]));
                i += 1;
            }
            doc.tables.push(Table { headers, rows });
            continue;
        } else if let Some(last) = doc.sections.last_mut() {
            last.2.push_str(lines[i]);
            last.2.push('\n');
        }
        i += 1;
    }
    doc
}

impl Doc {
    /// Has a section (heading level 2 or deeper) whose title contains one of `words`.
    fn has_section(&self, words: &[&str]) -> bool {
        self.sections
            .iter()
            .any(|(level, title, _)| *level >= 2 && words.iter().any(|w| title.contains(w)))
    }

    /// The body of the first section whose title contains `word`.
    fn section(&self, word: &str) -> Option<&str> {
        self.sections
            .iter()
            .find(|(level, title, _)| *level >= 1 && title.contains(word))
            .map(|s| s.2.as_str())
    }

    fn require_sections(&self, file: &str, needed: &[(&str, &[&str])], r: &mut Report) {
        for (name, words) in needed {
            if !self.has_section(words) {
                r.problems
                    .push(format!("{file} has no '## {name}' section"));
            }
        }
    }

    /// The table with the most of `columns` ((name, header words)), with the index of
    /// each column; or the names of the columns missing from the best one.
    fn table(&self, columns: &[(&str, &[&str])]) -> Result<(&Table, Vec<usize>), Vec<String>> {
        let matches = |t: &Table| -> Vec<Option<usize>> {
            columns
                .iter()
                .map(|(_, words)| t.headers.iter().position(|h| header_is(h, words)))
                .collect()
        };
        let best = self
            .tables
            .iter()
            .max_by_key(|t| matches(t).iter().flatten().count());
        let Some(best) = best else {
            return Err(columns.iter().map(|(n, _)| (*n).to_owned()).collect());
        };
        let found = matches(best);
        if found.iter().all(Option::is_some) {
            Ok((best, found.into_iter().flatten().collect()))
        } else {
            Err(columns
                .iter()
                .zip(found)
                .filter(|(_, f)| f.is_none())
                .map(|((n, _), _)| (*n).to_owned())
                .collect())
        }
    }
}

/// A header names a column if it is one of `words` or has one of them as a word.
fn header_is(header: &str, words: &[&str]) -> bool {
    words.iter().any(|w| {
        header == *w
            || header
                .split(|c: char| !c.is_alphanumeric())
                .any(|part| part == *w)
    })
}

fn cell(row: &[String], col: usize) -> &str {
    row.get(col).map(|c| c.trim()).unwrap_or("")
}

fn blank(value: &str) -> bool {
    let v = normalize(value.trim().trim_matches('*').trim());
    matches!(
        v.as_str(),
        "" | "-" | "—" | "–" | "n/a" | "na" | "none" | "tbd" | "?" | "todo"
    )
}

fn first_word(value: &str) -> String {
    normalize(value)
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .find(|w| !w.is_empty())
        .unwrap_or("")
        .to_owned()
}

fn check_diagrams(doc: &Doc, file: &str, r: &mut Report) {
    for d in &doc.diagrams {
        if !DIAGRAM_KINDS.contains(&d.kind.as_str()) {
            r.problems.push(format!(
                "{file}: a mermaid block starts with '{}', which isn't a Mermaid diagram type",
                d.kind
            ));
        } else if d
            .lines
            .iter()
            .filter(|l| {
                let l = l.trim();
                !l.is_empty() && !l.starts_with("%%")
            })
            .count()
            < 2
        {
            r.problems
                .push(format!("{file}: a {} diagram is empty", d.kind));
        }
    }
}

fn load_doc(dir: &Path, rel: &str, r: &mut Report) -> Option<Doc> {
    let doc = parse_doc(&read(dir, rel, r)?);
    check_diagrams(&doc, rel, r);
    Some(doc)
}

/// Ids like `TM-3` in `text`.
fn ids(prefix: &str, text: &str) -> BTreeSet<String> {
    mentions(prefix, text).into_iter().collect()
}

// ---------- design:arch ----------

const ADR_STATUSES: &[&str] = &[
    "proposed",
    "accepted",
    "rejected",
    "deprecated",
    "superseded",
];

/// The ADRs in `design/adr/` (id -> status); problems for malformed ones.
fn load_adrs(dir: &Path, r: &mut Report) -> BTreeMap<String, String> {
    let mut adrs = BTreeMap::new();
    let folder = dir.join("design").join("adr");
    match std::fs::symlink_metadata(&folder) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            r.problems
                .push("design/adr is not a folder (symbolic links aren't followed)".into());
            return adrs;
        }
        Err(_) => {
            r.problems.push(
                "design/adr/ is missing: record each architecture decision there (ADR-0001-short-title.md)"
                    .into(),
            );
            return adrs;
        }
    }
    let mut names: Vec<String> = std::fs::read_dir(&folder)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .filter_map(|e| e.file_name().to_str().map(str::to_owned))
                .filter(|n| n.ends_with(".md"))
                .take(200)
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    if names.is_empty() {
        r.problems.push(
            "design/adr/ has no architecture decision record (ADR-0001-short-title.md)".into(),
        );
    }
    for name in names {
        let rel = format!("design/adr/{name}");
        let Some(id) = id_with("ADR-", &name) else {
            r.problems.push(format!(
                "{rel}: name ADR files like ADR-0001-short-title.md"
            ));
            continue;
        };
        let Some(doc) = load_doc(dir, &rel, r) else {
            continue;
        };
        let title_ok = doc
            .sections
            .iter()
            .find(|s| s.0 >= 1)
            .is_some_and(|s| s.1.starts_with(&id.to_lowercase()));
        if !title_ok {
            r.problems
                .push(format!("{rel}: its title must start with '# {id}: '"));
        }
        doc.require_sections(
            &rel,
            &[
                ("Status", &["status"]),
                ("Context", &["context"]),
                ("Decision", &["decision"]),
                ("Alternatives considered", &["alternative", "option"]),
                ("Consequences", &["consequence"]),
            ],
            r,
        );
        let status = doc.section("status").map(first_word).unwrap_or_default();
        if doc.has_section(&["status"]) && !ADR_STATUSES.contains(&status.as_str()) {
            r.problems.push(format!(
                "{rel}: Status must be Proposed, Accepted, Rejected, Deprecated or Superseded"
            ));
        }
        if adrs.insert(id.clone(), status).is_some() {
            r.problems.push(format!("{id} is used by two files"));
        }
    }
    adrs
}

pub fn check_arch(dir: &Path) -> Report {
    let mut r = Report::default();
    let adrs = load_adrs(dir, &mut r);
    let mut domain_model = false;
    let (mut diagrams, mut workflows) = (0, 0);
    if let Some(text) = read(dir, "design/architecture.md", &mut r) {
        let doc = parse_doc(&text);
        check_diagrams(&doc, "design/architecture.md", &mut r);
        doc.require_sections(
            "design/architecture.md",
            &[
                ("Context", &["context"]),
                ("Components", &["component"]),
                ("Deployment", &["deployment"]),
                ("Quality attributes", &["quality", "non-functional"]),
                ("Decisions", &["decision"]),
            ],
            &mut r,
        );
        let drawn = |sections: &[&str], kinds: &[&str]| {
            doc.diagrams.iter().any(|d| {
                sections.iter().any(|s| d.section.contains(s)) && kinds.contains(&d.kind.as_str())
            })
        };
        if !drawn(&["context"], &["flowchart", "c4", "architecture-beta"]) {
            r.problems.push(
                "design/architecture.md has no context diagram (a Mermaid flowchart or C4 diagram under 'Context')"
                    .into(),
            );
        }
        if !drawn(
            &["component"],
            &[
                "flowchart",
                "c4",
                "classDiagram",
                "block-beta",
                "architecture-beta",
            ],
        ) {
            r.problems.push(
                "design/architecture.md has no component diagram (a Mermaid flowchart or C4 diagram under 'Components')"
                    .into(),
            );
        }
        diagrams += doc.diagrams.len();
        domain_model |= doc
            .diagrams
            .iter()
            .any(|d| matches!(d.kind.as_str(), "classDiagram" | "erDiagram"));
        let cited = ids("ADR-", &text);
        for id in adrs.keys().filter(|id| !cited.contains(*id)) {
            r.problems.push(format!(
                "design/architecture.md doesn't list {id} under Decisions"
            ));
        }
        for id in cited.iter().filter(|id| !adrs.contains_key(*id)) {
            r.problems.push(format!(
                "design/architecture.md refers to {id}, which has no file in design/adr/"
            ));
        }
    }
    if let Some(text) = read(dir, "design/workflows.md", &mut r) {
        let doc = parse_doc(&text);
        check_diagrams(&doc, "design/workflows.md", &mut r);
        workflows = doc
            .diagrams
            .iter()
            .filter(|d| d.kind == "sequenceDiagram")
            .count();
        if workflows == 0 {
            r.problems.push(
                "design/workflows.md has no UML sequence diagram (sequenceDiagram) of a workflow"
                    .into(),
            );
        }
        if !doc
            .diagrams
            .iter()
            .any(|d| matches!(d.kind.as_str(), "stateDiagram" | "flowchart"))
        {
            r.problems.push(
                "design/workflows.md has no state or activity diagram (stateDiagram or flowchart)"
                    .into(),
            );
        }
        diagrams += doc.diagrams.len();
        domain_model |= doc
            .diagrams
            .iter()
            .any(|d| matches!(d.kind.as_str(), "classDiagram" | "erDiagram"));
        let covered = ids("US-", &text);
        let mut quiet = Report::default();
        match load_spec(dir, &mut quiet) {
            Some(spec) => {
                for story in spec.stories.keys().filter(|s| !covered.contains(*s)) {
                    r.problems.push(format!(
                        "no workflow covers {story}: name it in design/workflows.md"
                    ));
                }
            }
            None => r.problems.push(
                "spec/SPEC.md is missing, so the stories the workflows must cover are unknown"
                    .into(),
            ),
        }
    }
    if !domain_model && let Ok(text) = crate::spec::read_inside(dir, "design/data-dictionary.md") {
        domain_model = parse_doc(&text)
            .diagrams
            .iter()
            .any(|d| matches!(d.kind.as_str(), "classDiagram" | "erDiagram"));
    }
    if !domain_model {
        r.problems.push(
            "the design has no UML class or ER diagram of the domain model (classDiagram or erDiagram)"
                .into(),
        );
    }
    r.notes.push(format!(
        "{diagrams} diagrams ({workflows} sequence), {} ADRs",
        adrs.len()
    ));
    r
}

// ---------- design:data ----------

const CLASSIFICATIONS: &str = "Public, Internal, Confidential, Personal or Sensitive";

/// public, internal, confidential, personal or sensitive.
fn classification(value: &str) -> Option<&'static str> {
    Some(match first_word(value).as_str() {
        "public" => "public",
        "internal" => "internal",
        "confidential" => "confidential",
        "personal" | "pii" => "personal",
        "sensitive" => "sensitive",
        _ => return None,
    })
}

#[derive(Debug, Clone)]
struct Flow {
    id: String,
    data: String,
    boundary: bool,
    protection: String,
}

const FLOW_COLUMNS: &[(&str, &[&str])] = &[
    ("ID", &["id"]),
    ("From", &["from", "source"]),
    ("To", &["to", "destination"]),
    ("Data", &["data"]),
    ("Boundary", &["boundary"]),
    ("Protection", &["protection", "controls"]),
];

fn flows(doc: &Doc) -> Result<Vec<Flow>, Vec<String>> {
    let (table, c) = doc.table(FLOW_COLUMNS)?;
    Ok(table
        .rows
        .iter()
        .map(|row| Flow {
            id: id_with("DF-", cell(row, c[0])).unwrap_or_else(|| cell(row, c[0]).to_owned()),
            data: cell(row, c[3]).to_owned(),
            boundary: matches!(
                first_word(cell(row, c[4])).as_str(),
                "yes" | "y" | "true" | "crosses"
            ) || cell(row, c[4]).starts_with('✓')
                || cell(row, c[4]).starts_with('✔'),
            protection: cell(row, c[5]).to_owned(),
        })
        .collect())
}

pub fn check_data(dir: &Path) -> Report {
    let mut r = Report::default();
    let mut personal_entities = BTreeSet::new();
    let (mut fields, mut personal, mut flow_count, mut crossing) = (0, 0, 0, 0);
    if let Some(doc) = load_doc(dir, "design/data-dictionary.md", &mut r) {
        let columns: &[(&str, &[&str])] = &[
            ("Entity", &["entity", "table", "resource"]),
            ("Field", &["field", "attribute", "property", "column"]),
            ("Type", &["type"]),
            ("Classification", &["classification"]),
            ("Purpose", &["purpose"]),
            ("Retention", &["retention"]),
            ("Description", &["description", "meaning"]),
        ];
        match doc.table(columns) {
            Err(missing) => r.problems.push(format!(
                "design/data-dictionary.md needs a table with the columns Entity, Field, Type, Classification, Purpose, Retention, Description (missing: {})",
                missing.join(", ")
            )),
            Ok((table, c)) => {
                let mut seen = BTreeSet::new();
                for row in &table.rows {
                    let (entity, field) = (cell(row, c[0]), cell(row, c[1]));
                    if blank(entity) || blank(field) {
                        r.problems
                            .push("design/data-dictionary.md: a row has no entity or field".into());
                        continue;
                    }
                    fields += 1;
                    let name = format!("{entity}.{field}");
                    if !seen.insert((normalize(entity), normalize(field))) {
                        r.problems.push(format!("{name} is listed twice"));
                    }
                    if blank(cell(row, c[2])) {
                        r.problems.push(format!("{name} has no type"));
                    }
                    if blank(cell(row, c[6])) {
                        r.problems.push(format!("{name} has no description"));
                    }
                    match classification(cell(row, c[3])) {
                        None => r.problems.push(format!(
                            "{name}: classification must be {CLASSIFICATIONS}"
                        )),
                        Some("personal" | "sensitive") => {
                            personal += 1;
                            personal_entities.insert(entity.to_owned());
                            if blank(cell(row, c[4])) {
                                r.problems.push(format!(
                                    "{name} is personal data: say why it is needed (Purpose)"
                                ));
                            }
                            if blank(cell(row, c[5])) {
                                r.problems.push(format!(
                                    "{name} is personal data: say how long it is kept (Retention)"
                                ));
                            }
                        }
                        Some(_) => {}
                    }
                }
                if let Some(api) = load_api(dir, &mut r) {
                    let schemas = api
                        .pointer("/components/schemas")
                        .and_then(Value::as_object);
                    for (schema, def) in schemas.into_iter().flatten() {
                        let props = def.get("properties").and_then(Value::as_object);
                        for prop in props.into_iter().flatten().map(|(k, _)| k) {
                            if !seen.contains(&(normalize(schema), normalize(prop))) {
                                r.problems.push(format!(
                                    "the data dictionary has no entry for {schema}.{prop} (in spec/openapi.json)"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(doc) = load_doc(dir, "design/dataflow.md", &mut r) {
        let charts: Vec<&Diagram> = doc
            .diagrams
            .iter()
            .filter(|d| d.kind == "flowchart")
            .collect();
        if charts.is_empty() {
            r.problems
                .push("design/dataflow.md has no data flow diagram (a Mermaid flowchart)".into());
        } else if !charts.iter().any(|d| {
            d.lines
                .iter()
                .any(|l| l.trim_start().starts_with("subgraph"))
        }) {
            r.problems.push(
                "the data flow diagram shows no trust boundary (draw each boundary as a subgraph)"
                    .into(),
            );
        }
        match flows(&doc) {
            Err(missing) => r.problems.push(format!(
                "design/dataflow.md needs a table of flows with the columns ID, From, To, Data, Boundary, Protection (missing: {})",
                missing.join(", ")
            )),
            Ok(list) => {
                let mut seen = BTreeSet::new();
                for f in &list {
                    if !f.id.starts_with("DF-") {
                        r.problems
                            .push(format!("flow '{}': flow ids look like DF-1", f.id));
                    } else if !seen.insert(f.id.clone()) {
                        r.problems.push(format!("{} is used twice", f.id));
                    }
                    if f.boundary && blank(&f.protection) {
                        r.problems.push(format!(
                            "{} crosses a trust boundary: say how the data is protected (Protection)",
                            f.id
                        ));
                    }
                }
                for entity in &personal_entities {
                    let e = normalize(entity);
                    if !list.iter().any(|f| normalize(&f.data).contains(&e)) {
                        r.problems.push(format!(
                            "personal data in {entity} isn't in any data flow (design/dataflow.md)"
                        ));
                    }
                }
                flow_count = list.len();
                crossing = list.iter().filter(|f| f.boundary).count();
            }
        }
    }
    r.notes.push(format!(
        "{fields} fields ({personal} personal or sensitive), {flow_count} flows ({crossing} across a trust boundary)"
    ));
    r
}

// ---------- design:threats ----------

const STRIDE: &[(char, &str)] = &[
    ('S', "Spoofing"),
    ('T', "Tampering"),
    ('R', "Repudiation"),
    ('I', "Information disclosure"),
    ('D', "Denial of service"),
    ('E', "Elevation of privilege"),
];

fn stride_of(value: &str) -> BTreeSet<char> {
    let lower = normalize(value);
    let mut found = BTreeSet::new();
    for (word, letter) in [
        ("spoof", 'S'),
        ("tamper", 'T'),
        ("repudiat", 'R'),
        ("disclosure", 'I'),
        ("information", 'I'),
        ("denial", 'D'),
        ("elevation", 'E'),
        ("privilege", 'E'),
    ] {
        if lower.contains(word) {
            found.insert(letter);
        }
    }
    for token in value.split(|c: char| !c.is_alphabetic()) {
        if let [c] = token.chars().collect::<Vec<_>>()[..]
            && let Some((letter, _)) = STRIDE.iter().find(|(l, _)| *l == c.to_ascii_uppercase())
        {
            found.insert(*letter);
        }
    }
    found
}

#[derive(Debug, Clone)]
struct Threat {
    id: String,
    element: String,
    stride: String,
    threat: String,
    mitigation: String,
    risk: String,
    status: String,
}

fn threats(doc: &Doc) -> Result<Vec<Threat>, Vec<String>> {
    let columns: &[(&str, &[&str])] = &[
        ("ID", &["id"]),
        ("Element", &["element", "component", "flow", "target"]),
        ("STRIDE", &["stride", "category"]),
        ("Threat", &["threat", "description"]),
        ("Mitigation", &["mitigation", "mitigations", "control"]),
        ("Risk", &["risk", "severity"]),
        ("Status", &["status"]),
    ];
    let (table, c) = doc.table(columns)?;
    Ok(table
        .rows
        .iter()
        .map(|row| Threat {
            id: id_with("TM-", cell(row, c[0])).unwrap_or_else(|| cell(row, c[0]).to_owned()),
            element: cell(row, c[1]).to_owned(),
            stride: cell(row, c[2]).to_owned(),
            threat: cell(row, c[3]).to_owned(),
            mitigation: cell(row, c[4]).to_owned(),
            risk: first_word(cell(row, c[5])),
            status: first_word(cell(row, c[6])),
        })
        .collect())
}

fn risk_level(word: &str) -> Option<&'static str> {
    Some(match word {
        "critical" | "high" => "high",
        "medium" | "moderate" => "medium",
        "low" => "low",
        _ => return None,
    })
}

pub fn check_threats(dir: &Path) -> Report {
    let mut r = Report::default();
    let Some(doc) = load_doc(dir, "design/threat-model.md", &mut r) else {
        return r;
    };
    doc.require_sections(
        "design/threat-model.md",
        &[
            ("Assets", &["asset"]),
            ("Trust boundaries", &["boundar"]),
            ("Threats", &["threat"]),
        ],
        &mut r,
    );
    let list = match threats(&doc) {
        Ok(list) => list,
        Err(missing) => {
            r.problems.push(format!(
                "design/threat-model.md needs a table with the columns ID, Element, STRIDE, Threat, Mitigation, Risk, Status (missing: {})",
                missing.join(", ")
            ));
            return r;
        }
    };
    let (mut seen, mut covered, mut high) = (BTreeSet::new(), BTreeSet::new(), 0);
    for t in &list {
        if !t.id.starts_with("TM-") {
            r.problems
                .push(format!("threat '{}': threat ids look like TM-1", t.id));
        } else if !seen.insert(t.id.clone()) {
            r.problems.push(format!("{} is used twice", t.id));
        }
        let categories = stride_of(&t.stride);
        if categories.is_empty() {
            r.problems.push(format!(
                "{}: STRIDE must be Spoofing, Tampering, Repudiation, Information disclosure, Denial of service or Elevation of privilege (or S, T, R, I, D, E)",
                t.id
            ));
        }
        covered.extend(categories);
        if blank(&t.threat) {
            r.problems.push(format!("{} describes no threat", t.id));
        }
        if blank(&t.mitigation) {
            r.problems.push(format!("{} has no mitigation", t.id));
        }
        match risk_level(&t.risk) {
            None => r
                .problems
                .push(format!("{}: Risk must be High, Medium or Low", t.id)),
            Some("high") => {
                high += 1;
                if t.status == "open" {
                    r.problems.push(format!(
                        "{} is a high risk without a planned mitigation (status Open)",
                        t.id
                    ));
                }
            }
            Some(_) => {}
        }
        if !matches!(
            t.status.as_str(),
            "open" | "planned" | "mitigated" | "accepted"
        ) {
            r.problems.push(format!(
                "{}: Status must be Open, Planned, Mitigated or Accepted",
                t.id
            ));
        }
    }
    for (letter, name) in STRIDE.iter().filter(|(l, _)| !covered.contains(l)) {
        r.problems.push(format!(
            "no threat considers {name} ({letter}): add one, or one saying why it doesn't apply"
        ));
    }
    let mut quiet = Report::default();
    match load_doc(dir, "design/dataflow.md", &mut quiet).map(|d| flows(&d)) {
        Some(Ok(flow_list)) => {
            for f in flow_list.iter().filter(|f| f.boundary) {
                if !list.iter().any(|t| ids("DF-", &t.element).contains(&f.id)) {
                    r.problems.push(format!(
                        "{} crosses a trust boundary but no threat covers it (name it under Element)",
                        f.id
                    ));
                }
            }
        }
        _ => r.problems.push(
            "design/dataflow.md (with its table of flows) is missing, so the flows to cover are unknown"
                .into(),
        ),
    }
    r.notes.push(format!(
        "{} threats ({high} high risk), STRIDE categories covered: {}",
        list.len(),
        STRIDE
            .iter()
            .map(|(l, _)| *l)
            .filter(|l| covered.contains(l))
            .collect::<String>()
    ));
    r
}

// ---------- design:tests ----------

const TEST_LEVELS: &[&str] = &[
    "unit",
    "integration",
    "contract",
    "e2e",
    "end-to-end",
    "ui",
    "manual",
    "performance",
    "load",
    "security",
    "accessibility",
    "exploratory",
];

pub fn check_tests(dir: &Path) -> Report {
    let mut r = Report::default();
    let Some(doc) = load_doc(dir, "design/test-plan.md", &mut r) else {
        return r;
    };
    let columns: &[(&str, &[&str])] = &[
        (
            "Covers",
            &["covers", "criterion", "criteria", "requirement", "ac"],
        ),
        ("Level", &["level", "kind"]),
        ("Test", &["test", "scenario", "case"]),
    ];
    let (table, c) = match doc.table(columns) {
        Ok(found) => found,
        Err(missing) => {
            r.problems.push(format!(
                "design/test-plan.md needs a table with the columns Covers, Level, Test (missing: {})",
                missing.join(", ")
            ));
            return r;
        }
    };
    let mut levels: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in &table.rows {
        let (covers, test) = (cell(row, c[0]), cell(row, c[2]));
        let level = first_word(cell(row, c[1]));
        let named: Vec<String> = mentions("AC-", covers)
            .into_iter()
            .chain(mentions("TM-", covers))
            .collect();
        if named.is_empty() {
            r.problems.push(format!(
                "test '{test}' covers no acceptance criterion (AC-1) or threat (TM-1)"
            ));
        }
        if !TEST_LEVELS.contains(&level.as_str()) {
            r.problems.push(format!(
                "test '{test}': Level must be one of {}",
                TEST_LEVELS.join(", ")
            ));
        }
        if blank(test) {
            r.problems
                .push(format!("a test for {} isn't described", named.join(", ")));
        }
        for id in named {
            levels.entry(id).or_default().insert(level.clone());
        }
    }
    let mut quiet = Report::default();
    match load_spec(dir, &mut quiet) {
        Some(spec) => {
            for ac in spec.criteria.keys().filter(|ac| !levels.contains_key(*ac)) {
                r.problems
                    .push(format!("{ac} has no test in design/test-plan.md"));
            }
            for id in levels
                .keys()
                .filter(|id| id.starts_with("AC-") && !spec.criteria.contains_key(*id))
            {
                r.problems.push(format!(
                    "design/test-plan.md tests {id}, which the spec doesn't define"
                ));
            }
        }
        None => r
            .problems
            .push("spec/SPEC.md is missing, so the criteria to test are unknown".into()),
    }
    if let Some(api) = load_api(dir, &mut quiet) {
        for (name, op) in operations(&api) {
            let served = op.get("x-acceptance").and_then(Value::as_array);
            for ac in served.into_iter().flatten().filter_map(Value::as_str) {
                let api_level = levels.get(ac).is_some_and(|l| {
                    l.iter().any(|l| {
                        matches!(
                            l.as_str(),
                            "contract" | "integration" | "e2e" | "end-to-end"
                        )
                    })
                });
                if levels.contains_key(ac) && !api_level {
                    r.problems.push(format!(
                        "{ac} is served by {name}, so it needs a contract, integration or e2e test"
                    ));
                }
            }
        }
    }
    if let Some(threat_doc) = load_doc(dir, "design/threat-model.md", &mut quiet)
        && let Ok(list) = threats(&threat_doc)
    {
        let known: BTreeSet<&String> = list.iter().map(|t| &t.id).collect();
        for t in list
            .iter()
            .filter(|t| risk_level(&t.risk) == Some("high") && !levels.contains_key(&t.id))
        {
            r.problems.push(format!(
                "high-risk threat {} has no test proving its mitigation",
                t.id
            ));
        }
        for id in levels
            .keys()
            .filter(|id| id.starts_with("TM-") && !known.contains(id))
        {
            r.problems.push(format!(
                "design/test-plan.md tests {id}, which the threat model doesn't define"
            ));
        }
    }
    let text = doc
        .sections
        .iter()
        .map(|s| format!("{} {}", s.1, s.2))
        .collect::<String>()
        + &table.rows.concat().join(" ");
    let text = normalize(&text);
    for topic in ["performance", "security"] {
        if !text.contains(topic) {
            r.problems.push(format!(
                "design/test-plan.md doesn't say how {topic} is tested"
            ));
        }
    }
    r.notes.push(format!(
        "{} tests covering {} criteria and threats",
        table.rows.len(),
        levels.len()
    ));
    r
}

// ---------- design:ops ----------

fn has_percentage(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes
        .iter()
        .enumerate()
        .any(|(i, b)| *b == b'%' && i > 0 && bytes[i - 1].is_ascii_digit())
}

pub fn check_ops(dir: &Path) -> Report {
    let mut r = Report::default();
    let Some(text) = read(dir, "design/operations.md", &mut r) else {
        return r;
    };
    let doc = parse_doc(&text);
    check_diagrams(&doc, "design/operations.md", &mut r);
    doc.require_sections(
        "design/operations.md",
        &[
            ("Rollout", &["rollout", "deployment", "release"]),
            ("Rollback", &["rollback", "roll back"]),
            ("Monitoring", &["monitor", "observab"]),
            ("Alerts", &["alert"]),
            ("Runbook", &["runbook", "troubleshoot", "tsg"]),
        ],
        &mut r,
    );
    let lower = normalize(&text);
    let slo = text.lines().any(|line| {
        normalize(line)
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| w == "slo" || w == "slos")
            && has_percentage(line)
    });
    if !slo {
        r.problems.push(
            "design/operations.md has no SLO with a target (e.g. 99.9% of requests succeed)".into(),
        );
    }
    if !lower.contains("metric") {
        r.problems
            .push("design/operations.md names no metrics to collect".into());
    }
    if !lower.contains("dashboard") {
        r.problems
            .push("design/operations.md names no dashboard (what the team watches)".into());
    }
    let mut alerts = 0;
    match doc.table(&[
        ("Alert", &["alert", "name"]),
        ("Condition", &["condition", "when", "threshold", "trigger"]),
        ("Action", &["action", "runbook", "response", "tsg"]),
    ]) {
        Err(missing) => r.problems.push(format!(
            "design/operations.md needs an alert table with the columns Alert, Condition, Action (missing: {})",
            missing.join(", ")
        )),
        Ok((table, c)) => {
            alerts = table.rows.len();
            if alerts == 0 {
                r.problems.push("the alert table is empty".into());
            }
            for row in &table.rows {
                let alert = cell(row, c[0]);
                if blank(cell(row, c[1])) || blank(cell(row, c[2])) {
                    r.problems.push(format!(
                        "alert '{alert}' needs a condition and an action (what the on-call does)"
                    ));
                }
            }
        }
    }
    r.notes.push(format!("{alerts} alerts"));
    r
}

// ---------- review:decks ----------

struct Audience {
    file: &'static str,
    name: &'static str,
    max_slides: usize,
    slides: &'static [(&'static str, &'static [&'static str])],
}

const TECHNICAL: Audience = Audience {
    file: "review/technical.md",
    name: "technical",
    max_slides: 30,
    slides: &[
        ("Architecture", &["architecture"]),
        ("Decisions (ADRs)", &["decision", "adr", "adrs"]),
        ("API", &["api"]),
        ("Data", &["data"]),
        ("Threat model", &["threat", "security"]),
        ("Tests", &["test"]),
        ("Operations", &["operation", "rollout", "observability"]),
        ("Open questions", &["question", "ask", "decisions needed"]),
    ],
};

const PRODUCT: Audience = Audience {
    file: "review/product.md",
    name: "product",
    max_slides: 20,
    slides: &[
        ("Problem", &["problem", "why"]),
        ("Users and stories", &["user", "stor"]),
        (
            "Prototype",
            &["prototype", "experience", "demo", "walkthrough"],
        ),
        ("Scope", &["scope"]),
        ("Success metrics", &["success", "metric", "measure"]),
        (
            "Release plan",
            &["release", "timeline", "plan", "milestone"],
        ),
        ("Decisions needed", &["decision", "ask", "question"]),
    ],
};

const LEADERSHIP: Audience = Audience {
    file: "review/leadership.md",
    name: "leadership",
    max_slides: 12,
    slides: &[
        ("Summary", &["summary", "bluf", "bottom line"]),
        ("Impact", &["impact", "why", "value", "outcome"]),
        ("Timeline", &["timeline", "plan", "milestone", "roadmap"]),
        ("Risks", &["risk"]),
        ("Ask", &["ask", "decision"]),
    ],
};

/// Leadership slides stay short; detail goes in the speaker notes.
const LEADERSHIP_WORDS: usize = 90;
/// Internal ids that mean nothing to leadership or customers.
const INTERNAL_IDS: &[&str] = &["US-", "AC-", "ADR-", "TM-", "DF-"];

/// A slide title names a topic if one of its words starts with a topic word.
fn title_is(title: &str, words: &[&str]) -> bool {
    let title = normalize(title);
    words.iter().any(|w| {
        if w.contains(' ') {
            title.contains(w)
        } else {
            title
                .split(|c: char| !c.is_alphanumeric())
                .any(|part| part.starts_with(w))
        }
    })
}

fn internal_ids(text: &str) -> Vec<String> {
    let mut found: Vec<String> = INTERNAL_IDS
        .iter()
        .flat_map(|p| mentions(p, text))
        .collect();
    found.sort();
    found.dedup();
    found
}

fn check_deck(dir: &Path, a: &Audience, r: &mut Report) -> Option<(Vec<Slide>, String)> {
    let text = read(dir, a.file, r)?;
    let Some(slides) = crate::render::deck(&text) else {
        r.problems.push(format!(
            "{} isn't a Marp deck: start it with front matter '---' / 'marp: true' / '---' and separate slides with '---'",
            a.file
        ));
        return None;
    };
    if slides.len() > a.max_slides {
        r.problems.push(format!(
            "{} has {} slides: keep the {} review under {}",
            a.file,
            slides.len(),
            a.name,
            a.max_slides + 1
        ));
    }
    let titles: Vec<String> = slides
        .iter()
        .map(|s| s.title().unwrap_or_default())
        .collect();
    for (i, title) in titles.iter().enumerate() {
        if title.is_empty() {
            r.problems
                .push(format!("{} slide {} has no title", a.file, i + 1));
        }
    }
    for (topic, words) in a.slides {
        if !titles.iter().any(|t| title_is(t, words)) {
            r.problems.push(format!(
                "{} has no '{topic}' slide (a slide titled with it)",
                a.file
            ));
        }
    }
    Some((slides, text))
}

pub fn check_review(dir: &Path) -> Report {
    let mut r = Report::default();
    let mut summary = Vec::new();
    let mut quiet = Report::default();

    if let Some((slides, text)) = check_deck(dir, &TECHNICAL, &mut r) {
        summary.push(format!("technical {} slides", slides.len()));
        let adrs = load_adrs(dir, &mut quiet);
        let threat_ids: BTreeSet<String> = load_doc(dir, "design/threat-model.md", &mut quiet)
            .and_then(|d| threats(&d).ok())
            .map(|l| l.into_iter().map(|t| t.id).collect())
            .unwrap_or_default();
        let (cited_adrs, cited_threats) = (ids("ADR-", &text), ids("TM-", &text));
        if !adrs.is_empty() && cited_adrs.is_empty() {
            r.problems.push(
                "review/technical.md cites no ADR: show the decisions with their ids (ADR-0001)"
                    .into(),
            );
        }
        if !threat_ids.is_empty() && cited_threats.is_empty() {
            r.problems.push(
                "review/technical.md cites no threat: show the high risks with their ids (TM-1)"
                    .into(),
            );
        }
        for id in cited_adrs.iter().filter(|id| !adrs.contains_key(*id)) {
            r.problems.push(format!(
                "review/technical.md cites {id}, which isn't in design/adr/"
            ));
        }
        for id in cited_threats.iter().filter(|id| !threat_ids.contains(*id)) {
            r.problems.push(format!(
                "review/technical.md cites {id}, which the threat model doesn't define"
            ));
        }
    }

    if let Some((slides, text)) = check_deck(dir, &PRODUCT, &mut r) {
        summary.push(format!("product {} slides", slides.len()));
        if let Some(spec) = load_spec(dir, &mut quiet) {
            let shown = ids("US-", &text);
            for story in spec.stories.keys().filter(|s| !shown.contains(*s)) {
                r.problems
                    .push(format!("review/product.md doesn't show {story}"));
            }
        }
    }

    if let Some((slides, _)) = check_deck(dir, &LEADERSHIP, &mut r) {
        summary.push(format!("leadership {} slides", slides.len()));
        if slides.len() < 2
            || !title_is(
                &slides[1].title().unwrap_or_default(),
                LEADERSHIP.slides[0].1,
            )
        {
            r.problems.push(
                "review/leadership.md: the slide after the title must be the summary (bottom line up front)"
                    .into(),
            );
        }
        for (i, slide) in slides.iter().enumerate() {
            let words = slide.words();
            if words > LEADERSHIP_WORDS {
                r.problems.push(format!(
                    "review/leadership.md slide {} has {words} words: keep leadership slides under {} (put detail in speaker notes)",
                    i + 1,
                    LEADERSHIP_WORDS + 1
                ));
            }
            if slide.code_blocks() > 0 {
                r.problems.push(format!(
                    "review/leadership.md slide {} has code or diagram source: say it in plain words",
                    i + 1
                ));
            }
            let found = internal_ids(&slide.markdown);
            if !found.is_empty() {
                r.problems.push(format!(
                    "review/leadership.md slide {} uses internal ids ({}): say what they mean instead",
                    i + 1,
                    found.join(", ")
                ));
            }
        }
    }

    if let Some(text) = read(dir, "review/customer-feedback.md", &mut r) {
        let doc = parse_doc(&crate::render::split_notes(&text).markdown);
        doc.require_sections(
            "review/customer-feedback.md",
            &[
                (
                    "What we heard",
                    &["what we heard", "we heard", "your needs"],
                ),
                (
                    "What we're building",
                    &["building", "what's coming", "coming", "what we plan"],
                ),
                ("How it helps you", &["helps", "benefit"]),
                (
                    "What's not included",
                    &["not included", "out of scope", "not in this", "later"],
                ),
                ("Questions for you", &["question", "feedback"]),
                ("Next steps", &["next step"]),
            ],
            &mut r,
        );
        let questions = doc
            .sections
            .iter()
            .filter(|s| s.0 >= 2 && (s.1.contains("question") || s.1.contains("feedback")))
            .flat_map(|s| s.2.lines())
            .filter(|l| l.trim_end().ends_with('?'))
            .count();
        if questions < 3 {
            r.problems.push(format!(
                "review/customer-feedback.md asks {questions} questions: ask the customer at least 3"
            ));
        }
        let found = internal_ids(&text);
        if !found.is_empty() {
            r.problems.push(format!(
                "review/customer-feedback.md uses internal ids ({}): customers see plain words only",
                found.join(", ")
            ));
        }
        let lower = normalize(&text);
        for word in [
            "threat model",
            "vulnerab",
            "exploit",
            "internal only",
            "internal-only",
            "confidential",
        ] {
            if lower.contains(word) {
                r.problems.push(format!(
                    "review/customer-feedback.md mentions '{word}': keep internal and security details out of customer documents"
                ));
            }
        }
        if !doc.diagrams.is_empty() || text.contains("```") {
            r.problems.push(
                "review/customer-feedback.md has code or diagram source: customers get plain words"
                    .into(),
            );
        }
        if text.split_whitespace().any(|w| {
            let w = w.trim_matches(|c: char| !c.is_alphanumeric());
            w.split_once('@')
                .is_some_and(|(user, host)| !user.is_empty() && host.contains('.'))
        }) {
            r.problems.push(
                "review/customer-feedback.md contains an email address: leave personal data out"
                    .into(),
            );
        }
        summary.push(format!(
            "customer feedback document ({questions} questions)"
        ));
    }
    r.notes.push(summary.join(", "));
    r
}

// ---------- review:rehearsal ----------

/// The audiences a review rehearsal must cover: (name, title words).
const REHEARSAL_AUDIENCES: &[(&str, &[&str])] = &[
    ("Technical", &["technical", "tech", "engineering"]),
    ("Security", &["security"]),
    ("Privacy", &["privacy"]),
    ("Product", &["product"]),
    ("Leadership", &["leadership", "leaders", "executive"]),
    ("Customer", &["customer", "customers"]),
];
const SEVERITIES: &[&str] = &["blocker", "major", "minor"];

/// `review/rehearsal.md`: what simulated reviewers would say before the real reviews.
pub fn check_rehearsal(dir: &Path) -> Report {
    const FILE: &str = "review/rehearsal.md";
    let mut r = Report::default();
    let Some(text) = read(dir, FILE, &mut r) else {
        return r;
    };
    let doc = parse_doc(&text);
    doc.require_sections(
        FILE,
        &[
            ("Readiness", &["readiness", "ready"]),
            ("By audience", &["audience"]),
            ("Actions", &["action"]),
            ("Prepared answers", &["prepared answers", "answers", "faq"]),
        ],
        &mut r,
    );
    let readiness = doc.section("readiness").map(normalize).unwrap_or_default();
    if doc.has_section(&["readiness"]) && !readiness.contains("ready") {
        r.problems.push(format!(
            "{FILE}: say under Readiness whether the feature is ready, ready with changes or not ready"
        ));
    }
    let mut verdicts = Vec::new();
    for (audience, words) in REHEARSAL_AUDIENCES {
        let section = doc
            .sections
            .iter()
            .find(|(level, title, _)| *level >= 3 && title_is(title, words));
        match section {
            None => r.problems.push(format!(
                "{FILE} has no '### {audience}' section under By audience"
            )),
            Some((_, _, body)) => {
                let body = normalize(body);
                if body.contains("reject") {
                    verdicts.push(format!("{audience}: reject"));
                } else if body.contains("approve") {
                    verdicts.push(format!(
                        "{audience}: approve{}",
                        if body.contains("with changes") || body.contains("with conditions") {
                            " with changes"
                        } else {
                            ""
                        }
                    ));
                } else {
                    r.problems.push(format!(
                        "{FILE}: the {audience} section has no likely verdict (approve, approve with changes or reject)"
                    ));
                }
            }
        }
    }
    let columns: &[(&str, &[&str])] = &[
        ("Concern", &["concern", "issue", "question"]),
        ("Audience", &["audience", "reviewer", "who"]),
        ("Severity", &["severity"]),
        ("Action", &["action", "fix", "response"]),
    ];
    let mut actions = 0;
    match doc.table(columns) {
        Err(missing) => r.problems.push(format!(
            "{FILE}: the Actions table needs the columns {}",
            missing.join(", ")
        )),
        Ok((table, cols)) => {
            for (i, row) in table.rows.iter().enumerate() {
                let n = i + 1;
                if blank(cell(row, cols[0])) {
                    continue;
                }
                actions += 1;
                let severity = first_word(cell(row, cols[2]));
                if !SEVERITIES.contains(&severity.as_str()) {
                    r.problems.push(format!(
                        "{FILE}: action {n} has severity '{}': use blocker, major or minor",
                        cell(row, cols[2])
                    ));
                }
                if blank(cell(row, cols[3])) {
                    r.problems.push(format!(
                        "{FILE}: action {n} ({}) has no action: say what will change or how it will be answered",
                        cell(row, cols[0])
                    ));
                }
            }
            if actions == 0 {
                r.problems
                    .push(format!("{FILE}: the Actions table is empty"));
            }
        }
    }
    let answers = doc
        .sections
        .iter()
        .filter(|s| s.0 >= 2 && (s.1.contains("answer") || s.1.contains("faq")))
        .flat_map(|s| s.2.lines())
        .filter(|l| l.trim().trim_end_matches(['*', '_']).ends_with('?'))
        .count();
    if answers < 3 {
        r.problems.push(format!(
            "{FILE} prepares {answers} answers: prepare at least 3 for the hardest questions"
        ));
    }
    r.notes.push(format!(
        "{} audiences ({}), {actions} actions, {answers} prepared answers",
        verdicts.len(),
        verdicts.join(", ")
    ));
    r
}

// ---------- starter ----------

const STARTER: &[(&str, &str)] = &[
    (
        "design/architecture.md",
        include_str!("../design-starter/design/architecture.md"),
    ),
    (
        "design/workflows.md",
        include_str!("../design-starter/design/workflows.md"),
    ),
    (
        "design/adr/ADR-0001-cursor-paging.md",
        include_str!("../design-starter/design/adr/ADR-0001-cursor-paging.md"),
    ),
    (
        "design/data-dictionary.md",
        include_str!("../design-starter/design/data-dictionary.md"),
    ),
    (
        "design/dataflow.md",
        include_str!("../design-starter/design/dataflow.md"),
    ),
    (
        "design/threat-model.md",
        include_str!("../design-starter/design/threat-model.md"),
    ),
    (
        "design/test-plan.md",
        include_str!("../design-starter/design/test-plan.md"),
    ),
    (
        "design/operations.md",
        include_str!("../design-starter/design/operations.md"),
    ),
    (
        "review/technical.md",
        include_str!("../design-starter/review/technical.md"),
    ),
    (
        "review/product.md",
        include_str!("../design-starter/review/product.md"),
    ),
    (
        "review/leadership.md",
        include_str!("../design-starter/review/leadership.md"),
    ),
    (
        "review/customer-feedback.md",
        include_str!("../design-starter/review/customer-feedback.md"),
    ),
    (
        "review/rehearsal.md",
        include_str!("../design-starter/review/rehearsal.md"),
    ),
];

/// Writes a starter design package and review pack titled `title` into `dir` (an
/// example that, with the spec starter, passes every check). Refuses to overwrite
/// files unless `force`. Returns what it wrote.
pub fn init(dir: &Path, title: &str, force: bool) -> anyhow::Result<Vec<String>> {
    use anyhow::{Context, bail};
    let title = title.trim();
    if title.is_empty() || title.contains(['\n', '\r']) {
        bail!("the title must be a single, non-empty line");
    }
    if !force && let Some((path, _)) = STARTER.iter().find(|(p, _)| dir.join(p).exists()) {
        bail!("{path} exists already (use --force to replace the design package)");
    }
    let mut written = Vec::new();
    for (path, text) in STARTER {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().expect("has a parent"))?;
        std::fs::write(
            &full,
            text.replace("\r\n", "\n").replace("{{title}}", title),
        )
        .with_context(|| format!("writing {}", full.display()))?;
        written.push((*path).to_owned());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec starter plus the design starter: passes every built-in check.
    fn package() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        crate::spec::init(dir.path(), "Wishlist", false).unwrap();
        init(dir.path(), "Wishlist", false).unwrap();
        dir
    }

    fn edit(dir: &Path, rel: &str, from: &str, to: &str) {
        let path = dir.join(rel);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(from), "{rel} has no {from:?}");
        std::fs::write(path, text.replacen(from, to, 1)).unwrap();
    }

    fn has(r: &Report, text: &str) {
        assert!(
            r.problems.iter().any(|p| p.contains(text)),
            "{text:?} not in {:#?}",
            r.problems
        );
    }

    #[test]
    fn a_complete_design_package_passes_every_check() {
        let dir = package();
        // An agent kept as code has checks of its own (crate::factory::code).
        for name in crate::spec::BUILT_IN_CHECKS
            .iter()
            .filter(|n| !n.starts_with("agent:"))
        {
            let report = crate::spec::run(name, dir.path());
            assert!(report.passed(), "{name}: {}", report.text());
        }
        assert!(
            init(dir.path(), "Again", false)
                .unwrap_err()
                .to_string()
                .contains("exists already")
        );
        let arch = check_arch(dir.path()).text();
        assert!(arch.contains("1 ADRs"), "{arch}");
    }

    #[test]
    fn architecture_checks_find_missing_diagrams_decisions_and_workflows() {
        let dir = package();
        let p = dir.path();
        edit(
            p,
            "design/architecture.md",
            "```mermaid\nflowchart TB",
            "```text\nflowchart TB",
        );
        edit(p, "design/architecture.md", "- ADR-0001", "- ADR-0002");
        edit(p, "design/workflows.md", "US-2", "the list");
        edit(
            p,
            "design/adr/ADR-0001-cursor-paging.md",
            "Accepted",
            "Agreed",
        );
        std::fs::write(p.join("design/adr/notes.md"), "# Notes\n").unwrap();
        let r = check_arch(p);
        has(&r, "has no component diagram");
        has(&r, "doesn't list ADR-0001 under Decisions");
        has(&r, "refers to ADR-0002, which has no file");
        has(&r, "no workflow covers US-2");
        has(&r, "Status must be Proposed, Accepted");
        has(&r, "name ADR files like ADR-0001");
        edit(
            p,
            "design/workflows.md",
            "sequenceDiagram",
            "sequenceDiagrams",
        );
        has(&check_arch(p), "isn't a Mermaid diagram type");
    }

    #[test]
    fn data_and_threat_checks_find_privacy_and_security_gaps() {
        let dir = package();
        let p = dir.path();
        edit(
            p,
            "design/data-dictionary.md",
            "| Item | name |",
            "| Thing | name |",
        );
        edit(
            p,
            "design/data-dictionary.md",
            "| Show the member their item | Until the member deletes it |",
            "| - | - |",
        );
        let r = check_data(p);
        has(&r, "no entry for Item.name (in spec/openapi.json)");
        has(&r, "NewItem.name is personal data: say why it is needed");
        has(&r, "say how long it is kept");
        has(&r, "personal data in Thing isn't in any data flow");

        let flows_path = p.join("design/dataflow.md");
        let text = std::fs::read_to_string(&flows_path).unwrap();
        std::fs::write(&flows_path, text.replace("subgraph", "%% group")).unwrap();
        edit(
            p,
            "design/dataflow.md",
            "| Yes | HTTPS (TLS 1.2+), token checked on every call |",
            "| Yes | - |",
        );
        let r = check_data(p);
        has(&r, "shows no trust boundary");
        has(
            &r,
            "DF-1 crosses a trust boundary: say how the data is protected",
        );

        edit(
            p,
            "design/threat-model.md",
            "| Repudiation |",
            "| Nonsense |",
        );
        edit(
            p,
            "design/threat-model.md",
            "| High | Planned |",
            "| High | Open |",
        );
        edit(
            p,
            "design/threat-model.md",
            "| DF-4, Items API |",
            "| Items API |",
        );
        let r = check_threats(p);
        has(&r, "no threat considers Repudiation (R)");
        has(&r, "STRIDE must be Spoofing");
        has(&r, "TM-1 is a high risk without a planned mitigation");
        has(&r, "DF-4 crosses a trust boundary but no threat covers it");
    }

    #[test]
    fn test_and_ops_checks_find_unproven_criteria_and_missing_operations() {
        let dir = package();
        let p = dir.path();
        edit(
            p,
            "design/test-plan.md",
            "| AC-3 | Integration |",
            "| AC-3 | Unit |",
        );
        edit(p, "design/test-plan.md", "| AC-4 |", "| AC-9 |");
        edit(p, "design/test-plan.md", "| TM-2 |", "| TM-7 |");
        let r = check_tests(p);
        has(
            &r,
            "AC-3 is served by GET /items, so it needs a contract, integration or e2e test",
        );
        has(&r, "AC-4 has no test");
        has(&r, "tests AC-9, which the spec doesn't define");
        has(&r, "high-risk threat TM-2 has no test");

        edit(p, "design/operations.md", "## Rollback", "## Undo");
        edit(p, "design/operations.md", "99.9%", "three nines");
        edit(
            p,
            "design/operations.md",
            "| Latency | p95 > 300 ms for 15 minutes | Sev 3 |",
            "| Latency | - | Sev 3 |",
        );
        let r = check_ops(p);
        has(&r, "has no '## Rollback' section");
        has(&r, "no SLO with a target");
        has(&r, "alert 'Latency' needs a condition and an action");
    }

    #[test]
    fn review_check_enforces_each_audiences_form() {
        let dir = package();
        let p = dir.path();
        edit(p, "review/leadership.md", "## Risks", "## Risks TM-1");
        let long = "word ".repeat(120);
        edit(
            p,
            "review/leadership.md",
            "## Timeline",
            &format!("## Timeline\n{long}"),
        );
        edit(p, "review/product.md", "US-2", "the list");
        edit(p, "review/technical.md", "ADR-0001", "ADR-0009");
        edit(
            p,
            "review/customer-feedback.md",
            "## Next steps",
            "Write to ana@example.com about the threat model.\n\n## Next steps",
        );
        let r = check_review(p);
        has(&r, "uses internal ids (TM-1)");
        has(&r, "words: keep leadership slides under 91");
        has(&r, "review/product.md doesn't show US-2");
        has(&r, "cites ADR-0009, which isn't in design/adr/");
        has(&r, "mentions 'threat model'");
        has(&r, "contains an email address");

        std::fs::write(p.join("review/leadership.md"), "# Plan\n\nJust text.\n").unwrap();
        std::fs::remove_file(p.join("review/technical.md")).unwrap();
        let r = check_review(p);
        has(&r, "review/leadership.md isn't a Marp deck");
        has(&r, "review/technical.md is missing");
    }

    #[test]
    fn rehearsal_check_needs_every_audience_an_action_per_concern_and_answers() {
        let dir = package();
        let p = dir.path();
        let file = "review/rehearsal.md";
        let ok = check_rehearsal(p);
        assert!(ok.passed(), "{}", ok.text());
        assert!(ok.text().contains("6 audiences"), "{}", ok.text());
        edit(p, file, "### Privacy", "### Legal");
        edit(p, file, "Likely verdict: approve. Customers", "Customers");
        edit(p, file, "| Security | Major |", "| Security | High |");
        edit(
            p,
            file,
            "| Name the metric and its dashboard in the product deck |",
            "| TBD |",
        );
        edit(
            p,
            file,
            "Can we switch it off quickly?",
            "Switching it off.",
        );
        edit(p, file, "Ready with changes:", "Mostly fine:");
        let r = check_rehearsal(p);
        has(&r, "has no '### Privacy' section");
        has(&r, "the Customer section has no likely verdict");
        has(&r, "action 1 has severity 'High'");
        has(
            &r,
            "action 3 (How the 60% first-week goal is measured) has no action",
        );
        has(&r, "prepares 2 answers");
        has(&r, "say under Readiness");
        std::fs::remove_file(p.join(file)).unwrap();
        has(&check_rehearsal(p), "review/rehearsal.md is missing");
    }
}
