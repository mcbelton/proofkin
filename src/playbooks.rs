//! Playbooks: ready-made plans for the whole lifecycle, filled in with your peers,
//! folders and checks. Each is a normal plan (claims, steps, checks, approvals):
//!
//! - `bootstrap`: a fresh project, with all prerequisites for development (toolchain,
//!   README, tests, CI, claims), proven by the owner's checks;
//! - `discover`: an existing project, reverse-engineered into the knowledge base
//!   (architecture, build and run, risks, glossary), confirmed by a person;
//! - `sdlc`: a change through design, implementation, independent verification,
//!   security, privacy and compliance gates and release approval;
//! - `learn`: insights and troubleshooting guide drafts from how the system runs;
//! - `spec`: spec-driven development: a feature's spec, REST API contract and UX
//!   prototype, proven by built-in checks and accepted before any code;
//! - `design`: the feature's SDLC design artifacts and a review pack (a deck per
//!   audience and a customer feedback document), reviewed by each audience;
//! - `implement-spec`: the feature built from its accepted spec, a test per criterion;
//! - `rehearse`: the feature's reviews rehearsed before they happen: agents play each
//!   stakeholder in parallel, then a synthesis of actions and prepared answers;
//! - `agent-factory`: an agent kept as code: its blueprint and evals proven by the
//!   built-in `agent:*` checks, its evaluation's signed report, a red-team squad's review
//!   and sign-offs, then a release from the folder's commit.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

const PLAYBOOKS: &[(&str, &str, &str)] = &[
    (
        "bootstrap",
        "Fresh project: set up everything needed for development, with evidence",
        include_str!("../playbooks/bootstrap.plan"),
    ),
    (
        "discover",
        "Existing project: reverse-engineer it into the knowledge base",
        include_str!("../playbooks/discover.plan"),
    ),
    (
        "sdlc",
        "A change through design, verification, security, privacy, compliance and release gates",
        include_str!("../playbooks/sdlc.plan"),
    ),
    (
        "learn",
        "Insights and troubleshooting guides from how the system runs",
        include_str!("../playbooks/learn.plan"),
    ),
    (
        "spec",
        "Spec-driven: a feature's spec, REST API contract and UX prototype, checked and accepted before code",
        include_str!("../playbooks/spec.plan"),
    ),
    (
        "design",
        "A feature's SDLC design (architecture, ADRs, diagrams, data, threats, tests, operations) and review decks per audience",
        include_str!("../playbooks/design.plan"),
    ),
    (
        "implement-spec",
        "Implement a feature from its accepted spec package, with a test proving each acceptance criterion",
        include_str!("../playbooks/implement-spec.plan"),
    ),
    (
        SPEC_IMPORT,
        "Implement a Spec Kit feature, an OpenSpec change or a spec document on SharePoint or OneDrive: a claim per user story, requirement and success criterion, each proven by a test or a sign-off",
        include_str!("../playbooks/implement-speckit.plan"),
    ),
    (
        "rehearse",
        "Rehearse a feature's reviews: agents play each stakeholder in parallel, then actions and prepared answers",
        include_str!("../playbooks/rehearse.plan"),
    ),
    (
        "agent-factory",
        "An agent kept as code: blueprint and evals proven by agent:* checks, its evaluation's report, a red-team squad, sign-offs, then a release from the folder's commit",
        include_str!("../playbooks/agent-factory.plan"),
    ),
];

/// Names and descriptions of the playbooks.
pub fn list() -> Vec<(&'static str, &'static str)> {
    PLAYBOOKS.iter().map(|(n, d, _)| (*n, *d)).collect()
}

fn template(name: &str) -> Result<&'static str> {
    PLAYBOOKS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, _, t)| *t)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "there is no playbook '{name}' (there are: {})",
                PLAYBOOKS.iter().map(|p| p.0).collect::<Vec<_>>().join(", ")
            )
        })
}

fn default(playbook: &str, var: &str) -> Option<&'static str> {
    Some(match var {
        "folder" if playbook == "learn" => "data",
        "folder" => "app",
        "project" => "the project",
        "approver" | "privacy_approver" | "product_owner" | "tech_lead" | "security_champion"
        | "leader" => "me",
        "reviewers" => "qa",
        "change_id" if playbook == "agent-factory" => return None,
        "change_id" => "change",
        "build" => "build",
        "test" => "test",
        "lint" => "lint",
        "secscan" => "secscan",
        "licenses" => "licenses",
        "secrets" => "secrets",
        _ => return None,
    })
}

/// A plain-language outline of a playbook: what each step does, what it proves and
/// what you need to run it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Outline {
    pub claims: Vec<(String, String)>,
    pub steps: Vec<String>,
    pub needs: Vec<String>,
}

const PICKED_PEER: &str = "PICKED-PEER";
const PICKED_FOLDER: &str = "PICKED-FOLDER";
const PICKED_SPEC: &str = "PICKED-SPEC";
/// The playbook that imports a Spec Kit or OpenSpec spec, or a spec document on SharePoint
/// or OneDrive (see [`crate::spec_import`]).
pub const SPEC_IMPORT: &str = "implement-speckit";

/// Describes playbook `name` step by step, before any value is filled in.
pub fn outline(name: &str) -> Result<Outline> {
    use crate::plan::{Plan, Target};
    let mut vars = BTreeMap::new();
    for (var, _) in variables(name)? {
        let value = match var.as_str() {
            "peer" => PICKED_PEER.to_owned(),
            "folder" => PICKED_FOLDER.to_owned(),
            "spec" => PICKED_SPEC.to_owned(),
            "change" => "the change you describe".to_owned(),
            "change_id" if name == "agent-factory" => "AGENT".to_owned(),
            "project" => "your project".to_owned(),
            _ => continue,
        };
        vars.insert(var, value);
    }
    let plan = Plan::parse(&render(name, &vars)?)?;
    let who = |target: &Target| match target {
        Target::Peer(p) if p == PICKED_PEER => "the peer you pick".to_owned(),
        Target::Peer(p) if p == "me" => "you".to_owned(),
        Target::Peer(p) => format!("@{p}"),
        Target::Squad(s) => format!("a member of squad #{s}"),
        Target::Workiq => "Microsoft 365 (Work IQ)".to_owned(),
    };
    let first_sentence = |text: &str| {
        let end = text.find(". ").map(|i| i + 1).unwrap_or(text.len());
        text[..end].trim().to_owned()
    };
    let mut steps = Vec::new();
    let (mut checks, mut squads, mut topics) = (Vec::new(), Vec::new(), Vec::new());
    let mut approvals = false;
    let mut builds_on = false;
    for step in &plan.steps {
        let mut line = format!("**{}**", step.id);
        if !step.after.is_empty() {
            line += &format!(" (after {})", step.after.join(", "));
        }
        if step.approve {
            approvals = true;
            let who = who(&step.target);
            let verb = if who == "you" {
                "sign off"
            } else {
                "signs off"
            };
            line += &format!(": {who} {verb}: {}", first_sentence(&step.prompt));
        } else {
            let folder = match step.share.as_deref() {
                Some(PICKED_FOLDER) => " in the folder you pick".to_owned(),
                Some(f) => format!(" in its '{f}' folder"),
                None => String::new(),
            };
            line += &format!(": {}{folder}", who(&step.target));
            if !step.skills.is_empty() {
                line += &format!(", following the {} skill", step.skills.join(" and "));
            }
            line += &format!(": {}", first_sentence(&step.prompt));
            if let Some(apply) = &step.apply {
                line +=
                    &format!(" It first applies the commit of step {apply} in its own worktree.");
            }
            if !step.checks.is_empty() {
                let (built_in, own): (Vec<&String>, Vec<&String>) = step
                    .checks
                    .iter()
                    .partition(|c| crate::spec::is_built_in(c));
                let names = |list: &[&String]| {
                    list.iter()
                        .map(|c| format!("`{c}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let mut what = Vec::new();
                if !own.is_empty() {
                    what.push(format!(
                        "the folder's {} check{}",
                        names(&own),
                        if own.len() == 1 { "" } else { "s" }
                    ));
                }
                if !built_in.is_empty() {
                    what.push(format!(
                        "the built-in {} check{}",
                        names(&built_in),
                        if built_in.len() == 1 { "" } else { "s" }
                    ));
                }
                line += &format!(
                    " Then {} must pass{}.",
                    what.join(" and "),
                    step.attempts
                        .map(|n| format!(" (up to {n} attempts, each told what failed)"))
                        .unwrap_or_default()
                );
                checks.extend(own.into_iter().cloned());
            }
            if step.apply.is_some() {
                builds_on = true;
            }
            if let Some(topic) = &step.save {
                line += &format!(" The result is saved as knowledge '{topic}'.");
            }
            if !step.context.is_empty() {
                if step.context.iter().any(|t| t == "all") {
                    line += " It is given the whole knowledge base.";
                } else {
                    line += &format!(" It is given the knowledge {}.", step.context.join(", "));
                }
                topics.extend(step.context.iter().filter(|t| *t != "all").cloned());
            }
            if let Target::Squad(s) = &step.target {
                squads.push(s.clone());
            }
        }
        if !step.claims.is_empty() {
            line += &format!(" Proves {}.", step.claims.join(", "));
        }
        steps.push(line);
    }
    checks.sort();
    checks.dedup();
    squads.sort();
    squads.dedup();
    topics.sort();
    topics.dedup();
    let mut needs = vec![
        "A connected peer that runs tasks (Copilot, Claude or Codex) and shares a folder with you"
            .to_owned(),
    ];
    if !checks.is_empty() {
        needs.push(format!(
            "Full access to that folder, and these checks defined on it by its owner (share add --check): {}",
            checks.join(", ")
        ));
    }
    if builds_on {
        needs.push(
            "The folder shared with --worktrees: steps apply the commit of an earlier step"
                .to_owned(),
        );
    }
    if !squads.is_empty() {
        needs.push(format!(
            "A squad of reviewers: {}",
            squads
                .iter()
                .map(|s| format!("#{s}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !topics.is_empty() {
        needs.push(format!(
            "Knowledge topics {} (the discover playbook saves them)",
            topics.join(", ")
        ));
    }
    if approvals {
        needs.push("Someone to sign off: you, or a connected teammate".to_owned());
    }
    Ok(Outline {
        claims: plan
            .claims
            .iter()
            .map(|c| (c.id.clone(), c.text.clone()))
            .collect(),
        steps,
        needs,
    })
}

/// The `{{name}}` placeholders of a playbook, with their defaults (None: required).
pub fn variables(name: &str) -> Result<Vec<(String, Option<&'static str>)>> {
    let text = template(name)?;
    let mut vars: Vec<(String, Option<&'static str>)> = Vec::new();
    for part in text.split("{{").skip(1) {
        if let Some((var, _)) = part.split_once("}}")
            && !vars.iter().any(|(v, _)| v == var)
        {
            vars.push((var.to_owned(), default(name, var)));
        }
    }
    Ok(vars)
}

/// A filled-in playbook: the plan, and the knowledge to save in the profile before it
/// runs (the implement-speckit playbook saves the spec it imports).
#[derive(Debug, Clone)]
pub struct Rendered {
    pub text: String,
    /// (topic, text, where it came from).
    pub knowledge: Vec<(String, String, String)>,
    /// For the person who filled it in: what was imported, and warnings.
    pub notes: Vec<String>,
}

impl Rendered {
    /// Saves the knowledge in `kb`; returns the topics.
    pub fn save_knowledge(&self, kb: &crate::knowledge::KnowledgeBase) -> Result<Vec<String>> {
        for (topic, text, source) in &self.knowledge {
            kb.write(topic, text, source)?;
        }
        Ok(self.knowledge.iter().map(|k| k.0.clone()).collect())
    }
}

/// `vars` checked, with defaults for those not given. Values may not contain line
/// breaks; ids (peers, folders, checks, topics) must be single words.
fn values(name: &str, vars: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    let mut missing = Vec::new();
    for (var, default) in variables(name)? {
        let value = match (vars.get(&var), default) {
            (Some(v), _) => v.trim().to_owned(),
            (None, Some(d)) => d.to_owned(),
            (None, None) => {
                missing.push(var);
                continue;
            }
        };
        if value.is_empty() || value.contains(['\n', '\r']) {
            bail!("{var} must be a single, non-empty line");
        }
        if !matches!(var.as_str(), "change" | "project" | "spec")
            && value.contains(char::is_whitespace)
        {
            bail!("{var} must be a single word, got {value:?}");
        }
        values.insert(var, value);
    }
    if !missing.is_empty() {
        bail!(
            "playbook '{name}' needs: {} (e.g. --set {}=...)",
            missing.join(", "),
            missing[0]
        );
    }
    Ok(values)
}

/// The playbook as plan text with `vars` (and defaults) filled in, and the knowledge
/// it needs. For [`SPEC_IMPORT`], `spec` is read and becomes the claims; a spec that is a
/// document's link is read through Work IQ, by [`render_document`] only.
pub fn render_full(name: &str, vars: &BTreeMap<String, String>) -> Result<Rendered> {
    let values = values(name, vars)?;
    if name == SPEC_IMPORT
        && let Some(spec) = values.get("spec").filter(|s| *s != PICKED_SPEC)
    {
        if crate::workiq::document::is_link(spec) {
            crate::workiq::document::share_path(spec)?;
            bail!(
                "the {name} playbook reads a spec on SharePoint or OneDrive through Work IQ, \
                 with a profile's sign-in: run proofkin spec import --from <link> --profile NAME \
                 --set peer=..., or pick it in the web UI's Plan mode"
            );
        }
        let import = crate::spec_import::read(std::path::Path::new(spec))?;
        let source = format!("{} {}", import.kind.label(), import.dir.display());
        return rendered(&import, &values, source);
    }
    let mut text = template(name)?.to_owned();
    for (var, value) in &values {
        text = text.replace(&format!("{{{{{var}}}}}"), value);
    }
    crate::plan::Plan::parse(&text)?;
    Ok(Rendered {
        text,
        knowledge: Vec::new(),
        notes: Vec::new(),
    })
}

/// Whether filling in playbook `name` with `vars` reads a document through Work IQ
/// ([`render_document`]): the [`SPEC_IMPORT`] playbook with a link as its spec.
pub fn reads_document(name: &str, vars: &BTreeMap<String, String>) -> bool {
    name == SPEC_IMPORT
        && vars
            .get("spec")
            .is_some_and(|spec| crate::workiq::document::is_link(spec))
}

/// The [`SPEC_IMPORT`] playbook filled in with `vars`, whose spec is the link of a Word,
/// Markdown or text document on SharePoint or OneDrive. The document is read through `work`
/// (audited, held to the profile's Work IQ budgets); a link that isn't one is refused before
/// any call. The knowledge's source names the document by its file name, not its link.
pub async fn render_document(
    vars: &BTreeMap<String, String>,
    work: &dyn crate::workiq::context::WorkContext,
) -> Result<Rendered> {
    let values = values(SPEC_IMPORT, vars)?;
    let url = values.get("spec").map(String::as_str).unwrap_or_default();
    let doc = crate::workiq::document::read(work, url).await?;
    let import = crate::spec_import::from_document(&doc)?;
    let source = format!("{} {} (Work IQ)", import.kind.label(), import.rel);
    rendered(&import, &values, source)
}

/// The plan and knowledge of a spec `import`, with the playbook's `values` as its roles.
fn rendered(
    import: &crate::spec_import::Import,
    values: &BTreeMap<String, String>,
    source: String,
) -> Result<Rendered> {
    let roles = crate::spec_import::Roles {
        peer: &values["peer"],
        folder: &values["folder"],
        reviewers: &values["reviewers"],
        test: &values["test"],
        approver: &values["approver"],
    };
    let text = crate::spec_import::plan_text(import, &roles)?;
    let mut notes = vec![crate::spec_import::summary(import)];
    notes.extend(import.warnings.iter().map(|w| format!("Warning: {w}")));
    Ok(Rendered {
        text,
        knowledge: vec![(import.topic.clone(), import.knowledge.clone(), source)],
        notes,
    })
}

/// The playbook as plan text with `vars` (and defaults) filled in, for playbooks that
/// need no knowledge saved first.
pub fn render(name: &str, vars: &BTreeMap<String, String>) -> Result<String> {
    let rendered = render_full(name, vars)?;
    if !rendered.knowledge.is_empty() {
        bail!(
            "the {name} playbook saves the spec it imports as knowledge in a profile: \
             run proofkin spec import <folder> --profile NAME (--set peer=...), or pick it in the web UI's Plan mode"
        );
    }
    Ok(rendered.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Plan;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn every_playbook_renders_to_a_valid_plan_with_claims() {
        let all = vars(&[
            ("peer", "alice"),
            ("change", "Add CSV export"),
            ("change_id", "csv"),
            ("spec", PICKED_SPEC),
        ]);
        for (name, _) in list() {
            let text = render(name, &all).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert!(!text.contains("{{"), "{name}: {text}");
            let plan = Plan::parse(&text).unwrap();
            assert!(!plan.claims.is_empty(), "{name} proves claims");
            for claim in &plan.claims {
                assert!(
                    plan.steps.iter().any(|s| s.claims.contains(&claim.id)),
                    "{name}: no step proves {}",
                    claim.id
                );
            }
            // Every model step follows a skill that exists.
            let skills = crate::skills::Skills::new(std::env::temp_dir().join("p2p-no-skills"));
            for step in plan.steps.iter().filter(|s| !s.approve) {
                assert!(
                    !step.skills.is_empty(),
                    "{name}: step {} has no skill",
                    step.id
                );
                skills.instructions(&step.skills).unwrap();
            }
            // Every claim is backed by runtime checks or a sign-off, never a model alone.
            for step in plan.steps.iter().filter(|s| !s.claims.is_empty()) {
                assert!(
                    step.approve || !step.checks.is_empty(),
                    "{name}: step {}",
                    step.id
                );
            }
        }
        let sdlc = Plan::parse(&render("sdlc", &all).unwrap()).unwrap();
        let verify = sdlc.steps.iter().find(|s| s.id == "verify").unwrap();
        assert_eq!(verify.apply.as_deref(), Some("impl"));
        assert_eq!(verify.target, crate::plan::Target::Squad("qa".into()));
        let discover = Plan::parse(&render("discover", &all).unwrap()).unwrap();
        assert!(
            discover
                .steps
                .iter()
                .any(|s| s.save.as_deref() == Some("architecture"))
        );
    }

    #[test]
    fn outlines_say_what_a_playbook_does_before_it_is_filled_in() {
        let discover = outline("discover").unwrap();
        assert_eq!(discover.claims.len(), 4);
        let arch = &discover.steps[0];
        assert!(
            arch.starts_with("**architecture**: the peer you pick in the folder you pick, following the reverse-engineer skill: Reverse-engineer this codebase."),
            "{arch}"
        );
        assert!(arch.contains("saved as knowledge 'architecture'"), "{arch}");
        let build = discover
            .steps
            .iter()
            .find(|s| s.starts_with("**build**"))
            .unwrap();
        assert!(
            build.contains("the folder's `test` check must pass") && build.contains("Proves KB-2"),
            "{build}"
        );
        assert!(discover.steps.last().unwrap().contains("you sign off"));
        assert!(
            discover
                .needs
                .iter()
                .any(|n| n.contains("checks defined on it by its owner") && n.contains("test"))
        );

        let sdlc = outline("sdlc").unwrap();
        let verify = sdlc
            .steps
            .iter()
            .find(|s| s.starts_with("**verify**"))
            .unwrap();
        assert!(
            verify.contains("a member of squad #qa")
                && verify.contains("applies the commit of step impl"),
            "{verify}"
        );
        assert!(sdlc.needs.iter().any(|n| n.contains("#qa")));
        let spec = outline("spec").unwrap();
        let api = spec
            .steps
            .iter()
            .find(|s| s.starts_with("**api**"))
            .unwrap();
        assert!(
            api.contains("following the api-contract skill")
                && api.contains("the built-in `spec:spec`, `spec:api` checks must pass")
                && api.contains("applies the commit of step spec"),
            "{api}"
        );
        assert!(
            !spec.needs.iter().any(|n| n.contains("Full access")),
            "built-in checks need no full access: {:?}",
            spec.needs
        );
        assert!(spec.needs.iter().any(|n| n.contains("--worktrees")));
        let implement = outline("implement-spec").unwrap();
        let impl_ = &implement.steps[0];
        assert!(
            impl_.contains("the folder's `test`, `lint` checks and the built-in `spec:api`, `spec:trace` checks must pass"),
            "{impl_}"
        );
        let design = outline("design").unwrap();
        assert_eq!(design.claims.len(), 9);
        let pack = design
            .steps
            .iter()
            .find(|s| s.starts_with("**pack** (after plans)"))
            .unwrap();
        assert!(
            pack.contains("following the review-technical and review-product and review-leadership and review-customer skill")
                && pack.contains("`review:decks`"),
            "{pack}"
        );
        let lt = design.steps.last().unwrap();
        assert!(
            lt.starts_with("**lt-review** (after tech-review, security-review, privacy-review, product-review): you sign off"),
            "{lt}"
        );
        let rehearse = outline("rehearse").unwrap();
        let parallel = rehearse
            .steps
            .iter()
            .filter(|s| {
                s.contains("a member of squad #qa") && s.contains("stakeholder-simulation skill")
            })
            .count();
        assert_eq!(parallel, 6, "one stakeholder per step, all at once");
        let synthesis = rehearse
            .steps
            .iter()
            .find(|s| s.starts_with("**synthesis**"))
            .unwrap();
        assert!(
            synthesis.contains("the built-in `review:rehearsal` check must pass"),
            "{synthesis}"
        );
        assert!(!design.needs.iter().any(|n| n.contains("Full access")));
        assert!(
            sdlc.needs
                .iter()
                .any(|n| n.contains("Knowledge topics architecture, risks"))
        );
        for (name, _) in list() {
            let o = outline(name).unwrap();
            assert!(
                !o.steps.is_empty() && !o.claims.is_empty() && !o.needs.is_empty(),
                "{name}"
            );
            assert!(
                o.steps.iter().all(|s| !s.contains("PICKED")),
                "{name}: {:?}",
                o.steps
            );
        }
    }

    #[test]
    fn the_spec_kit_playbook_imports_the_spec_as_claims_and_knowledge() {
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let feature = fixtures.join("speckit/specs/001-photo-albums");
        let given = vars(&[
            ("spec", feature.to_str().unwrap()),
            ("peer", "alice"),
            ("reviewers", "review"),
        ]);
        let rendered = render_full(SPEC_IMPORT, &given).unwrap();
        let plan = crate::plan::Plan::parse(&rendered.text).unwrap();
        assert_eq!(plan.claims.len(), 7);
        assert_eq!(
            plan.steps[1].target,
            crate::plan::Target::Squad("review".into())
        );
        assert_eq!(rendered.knowledge[0].0, "spec-001-photo-albums");
        assert!(
            rendered.notes[0].starts_with("Spec Kit feature specs/001-photo-albums (Photo Albums): 5 claims proven by tests, 2 by sign-off; 1 of 5 tasks done"),
            "{:?}",
            rendered.notes
        );
        assert!(rendered.notes[1].starts_with("Warning: spec.md has 1 open question"));

        let dir = tempfile::tempdir().unwrap();
        let kb = crate::knowledge::KnowledgeBase::new(dir.path().join("knowledge"));
        assert_eq!(
            rendered.save_knowledge(&kb).unwrap(),
            ["spec-001-photo-albums"]
        );
        assert!(
            kb.read("spec-001-photo-albums")
                .unwrap()
                .contains("Create an album")
        );
        crate::knowledge::check_topics(&kb, &plan.steps[0].context).unwrap();

        // Without a profile to save the spec in, it can't be rendered.
        let err = render(SPEC_IMPORT, &given).unwrap_err().to_string();
        assert!(err.contains("proofkin spec import"), "{err}");
        // A folder with several specs says which.
        let several = vars(&[("spec", fixtures.to_str().unwrap()), ("peer", "alice")]);
        let err = render_full(SPEC_IMPORT, &several).unwrap_err();
        assert_eq!(
            err.downcast_ref::<crate::spec_import::SeveralSpecs>()
                .unwrap()
                .0
                .len(),
            2
        );
    }

    #[test]
    fn rendering_needs_required_values_and_rejects_bad_ones() {
        let err = render("sdlc", &vars(&[("peer", "alice")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs: change"), "{err}");
        let err = render("discover", &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs: peer"), "{err}");
        assert!(render("discover", &vars(&[("peer", "a b")])).is_err());
        assert!(render("sdlc", &vars(&[("peer", "a"), ("change", "x\ny")])).is_err());
        assert!(render("nope", &BTreeMap::new()).is_err());
        let text = render("discover", &vars(&[("peer", "bob"), ("folder", "repo")])).unwrap();
        assert!(text.contains("@bob:repo") && text.contains("approve @me"));
        let vars = variables("sdlc").unwrap();
        assert!(vars.contains(&("change".into(), None)));
        assert!(vars.contains(&("test".into(), Some("test"))));
        let err = render(
            "agent-factory",
            &super::tests::vars(&[("peer", "alice"), ("change", "Answers from the notes")]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("needs: change_id"), "the agent's name: {err}");
    }
}
