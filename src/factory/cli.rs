//! `proofkin factory`: the agent factory from the command line. It lives here rather
//! than in `main.rs` so the factory's commands grow next to its code.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use serde_json::Value;

use crate::factory::blueprint::{
    Blueprint, Catalog, Problem, check_value, from_value, has_errors, name_problem, summary,
};
use crate::factory::evals::check_fixtures;
use crate::factory::store::{Draft, FactoryStore, Provenance, now};
use crate::factory::{capabilities, draft, routing, templates};
use crate::llm::{ExecutorConfig, Provider, SdkExecutor};
use crate::profile::Profile;

#[derive(Args, Debug)]
pub struct ProfileOpt {
    /// The profile whose factory it is.
    #[arg(long = "profile", id = "profile")]
    pub name: String,
}

#[derive(Subcommand, Debug)]
pub enum FactoryCommand {
    /// List the built-in templates: what each is for, the most access it asks for and
    /// how many eval cases it starts with.
    Templates,
    /// Start an agent: a draft from a template, or one the profile's model writes
    /// from your words (read-only, in an empty folder; it grants nothing).
    #[command(after_help = "\
examples:
  proofkin factory new reviewer --template code-reviewer --profile alice
  proofkin factory new fin-analyst --describe \"Analyze quarterly reports, extract key metrics, flag risks, write an executive summary\" --profile alice
  proofkin factory new fin-analyst --template financial-analyst --describe \"...for our board, in euros\" --using claude --profile alice")]
    New {
        /// The agent's name.
        name: String,
        /// Start from this template (proofkin factory templates lists them).
        #[arg(long)]
        template: Option<String>,
        /// What the agent should do, in your words: the profile's model writes the draft.
        #[arg(long)]
        describe: Option<String>,
        /// The provider whose model writes the draft (default: the first one the
        /// profile is signed in to).
        #[arg(long, value_enum)]
        using: Option<Provider>,
        #[command(flatten)]
        profile: ProfileOpt,
        /// Node bridge script for the Claude and Codex SDKs (default: the one `setup` installed).
        #[arg(long, env = "PROOFKIN_BRIDGE")]
        bridge: Option<PathBuf>,
    },
    /// List the factory's agents and drafts, with their problems.
    List {
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Show an agent's draft: what it asks for, its problems, how it was written and
    /// its history; --json prints its blueprint.
    Show {
        name: String,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Edit an agent's draft in your editor (VISUAL or EDITOR), or replace it from a
    /// file (- for stdin). It is checked when saved and kept with its problems.
    Edit {
        name: String,
        /// Take the blueprint from this file instead of an editor.
        #[arg(long)]
        from: Option<PathBuf>,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Check a draft, or a blueprint file, against the profile: each problem with its
    /// fix, and what it would get in each folder it is deployed to. Fails if any problem
    /// is an error.
    #[command(after_help = "\
examples:
  proofkin factory check fin-analyst --profile alice
  proofkin factory check fin-analyst --folders reports,ledger --profile alice
  proofkin factory check --file fin-analyst.json --profile alice
What it asks for is resolved against what each folder offers: what isn't offered is an
error naming the command that offers it. Checking never shares, offers or grants anything.")]
    Check {
        /// The draft to check.
        name: Option<String>,
        /// Check this blueprint file instead (its fixtures are in fixtures/ next to it).
        #[arg(long, conflicts_with = "name")]
        file: Option<PathBuf>,
        /// The folders to resolve its capabilities in (comma-separated; default: its
        /// deploy folders).
        #[arg(long, value_delimiter = ',')]
        folders: Option<Vec<String>>,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Build a draft: the files the agent is made of (its agent file, blueprint, evals,
    /// skills and team plan), the same bytes on every platform, kept by their hash. A
    /// draft with an error isn't built.
    Build {
        name: String,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Delete an agent's draft and records (not an agent with releases: their records
    /// are kept).
    Delete {
        name: String,
        #[command(flatten)]
        profile: ProfileOpt,
    },
}

pub async fn run(command: FactoryCommand) -> Result<()> {
    match command {
        FactoryCommand::Templates => {
            println!(
                "  {:<20} {:<16} {:>5}  for",
                "template", "most access", "cases"
            );
            for t in templates::list() {
                println!(
                    "  {:<20} {:<16} {:>5}  {}",
                    t.name, t.access, t.eval_cases, t.purpose
                );
            }
            println!(
                "Start an agent from one: proofkin factory new NAME --template TEMPLATE --profile P"
            );
        }
        FactoryCommand::New {
            name,
            template,
            describe,
            using,
            profile,
            bridge,
        } => new(&name, template, describe, using, &profile.name, bridge).await?,
        FactoryCommand::List { profile } => list(&Profile::open(&profile.name)?)?,
        FactoryCommand::Show {
            name,
            json,
            profile,
        } => show(&Profile::open(&profile.name)?, &name, json)?,
        FactoryCommand::Edit {
            name,
            from,
            profile,
        } => edit(&Profile::open(&profile.name)?, &name, from.as_deref())?,
        FactoryCommand::Check {
            name,
            file,
            folders,
            profile,
        } => check(
            &Profile::open(&profile.name)?,
            name.as_deref(),
            file.as_deref(),
            folders,
        )?,
        FactoryCommand::Build { name, profile } => build(&Profile::open(&profile.name)?, &name)?,
        FactoryCommand::Delete { name, profile } => {
            let profile = Profile::open(&profile.name)?;
            if !FactoryStore::of(&profile).delete(&name)? {
                bail!(
                    "the factory has no agent '{name}' (proofkin factory list --profile {})",
                    profile.name
                );
            }
            println!("profile '{}': deleted {name}", profile.name);
        }
    }
    Ok(())
}

async fn new(
    name: &str,
    template: Option<String>,
    describe: Option<String>,
    using: Option<Provider>,
    profile: &str,
    bridge: Option<PathBuf>,
) -> Result<()> {
    let profile = Profile::open(profile)?;
    let catalog = Catalog::of(&profile)?;
    let (draft, problems, what) = match (describe, template) {
        (Some(words), template) => {
            let provider = match using {
                Some(provider) => provider,
                None => *catalog.providers.first().ok_or_else(|| {
                    anyhow!(
                        "profile '{}' isn't signed in to a provider whose model could write the draft: proofkin auth login copilot --profile {0}",
                        profile.name
                    )
                })?,
            };
            if !catalog.providers.contains(&provider) {
                bail!(
                    "profile '{}' isn't signed in to {provider}: {}",
                    profile.name,
                    profile.login_hint(provider)
                );
            }
            if let Some(problem) = name_problem(name, &catalog) {
                bail!("{problem}");
            }
            let bridge = crate::setup::resolve_bridge(
                bridge.or_else(|| crate::profile::env_setting_os("BRIDGE").map(PathBuf::from)),
            );
            let executor = SdkExecutor::start(ExecutorConfig {
                providers: vec![provider],
                bridge_script: bridge,
                profile: profile.clone(),
            })
            .await?;
            let request = draft::Request {
                name,
                description: &words,
                template: template.as_deref(),
                provider,
            };
            let written = draft::write(executor.as_ref(), &request, &catalog, &|line: String| {
                println!("  {line}")
            })
            .await;
            executor.stop().await;
            let written = written?;
            let model = written
                .draft
                .provenance
                .model
                .clone()
                .unwrap_or_else(|| "its default".into());
            let what = format!(
                "drafted by {provider}'s model ({model}) from a description{}",
                template
                    .map(|t| format!(" and template {t}"))
                    .unwrap_or_default()
            );
            (written.draft, written.problems, what)
        }
        (None, Some(template)) => {
            if let Some(problem) = name_problem(name, &catalog) {
                bail!("{problem}");
            }
            let blueprint = templates::instantiate(&template, name, &catalog)?;
            let problems = check_value(&blueprint, &catalog);
            let draft = Draft {
                blueprint,
                provenance: Provenance {
                    source: "template".into(),
                    template: Some(template.clone()),
                    at: now(),
                    ..Provenance::default()
                },
                tries: 0,
            };
            (draft, problems, format!("drafted from template {template}"))
        }
        (None, None) => bail!(
            "give --template NAME (proofkin factory templates lists them) or --describe \"what the agent should do\""
        ),
    };
    FactoryStore::of(&profile).create_draft(name, &draft, &profile.name, &what)?;
    println!(
        "profile '{}': created draft {name} ({what}): {}",
        profile.name,
        summary(&problems)
    );
    print_problems(&problems);
    println!(
        "next: proofkin factory show {name} --profile {0}   (what it asks for and how it is tested)\n      proofkin factory edit {name} --profile {0}   (change it: it is checked when you save)",
        profile.name
    );
    Ok(())
}

/// The problems of agent `name`'s draft: the blueprint's, and its fixtures'.
fn problems_of(
    store: &FactoryStore,
    name: &str,
    blueprint: &Value,
    catalog: &Catalog,
) -> Vec<Problem> {
    let mut problems = check_value(blueprint, catalog);
    if let Ok(parsed) = from_value(blueprint) {
        problems.extend(check_fixtures(&parsed.evals, &store.fixtures_dir(name)));
        problems.sort_by_key(|p| !p.error);
    }
    problems
}

fn list(profile: &Profile) -> Result<()> {
    let store = FactoryStore::of(profile);
    let listed = store.list()?;
    if listed.is_empty() {
        println!(
            "no agents yet: proofkin factory new NAME --template TEMPLATE --profile {} (proofkin factory templates lists them)",
            profile.name
        );
        return Ok(());
    }
    let catalog = Catalog::of(profile)?;
    println!("  {:<24} {:<10} {:<40} problems", "agent", "state", "title");
    for agent in listed {
        let problems = agent.draft.as_ref().map(|d| {
            summary(&problems_of(
                &store,
                &agent.name,
                &d.blueprint,
                &catalog.clone().without(&agent.name),
            ))
        });
        println!(
            "  {:<24} {:<10} {:<40} {}",
            agent.name,
            agent.state,
            agent.title.unwrap_or_default(),
            problems.unwrap_or_default()
        );
    }
    Ok(())
}

fn draft_of(store: &FactoryStore, profile: &Profile, name: &str) -> Result<Draft> {
    store.draft(name)?.ok_or_else(|| {
        anyhow!(
            "the factory has no draft '{name}' (proofkin factory list --profile {})",
            profile.name
        )
    })
}

fn show(profile: &Profile, name: &str, json: bool) -> Result<()> {
    let store = FactoryStore::of(profile);
    let draft = draft_of(&store, profile, name)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&draft.blueprint)?);
        return Ok(());
    }
    let state = store.state(name)?;
    let lifecycle = state
        .as_ref()
        .map(|s| s.state.to_string())
        .unwrap_or_else(|| "draft".into());
    let catalog = Catalog::of(profile)?.without(name);
    match from_value(&draft.blueprint) {
        Ok(blueprint) => {
            describe(&blueprint, &lifecycle);
            // Copilot's offers are known once it runs: its evaluation checks multipliers.
            let routes = routing::candidates(&blueprint, &catalog, None, true);
            let listed: Vec<String> = routes.candidates.iter().map(|r| r.to_string()).collect();
            println!(
                "  evaluated on: {}",
                if listed.is_empty() {
                    "no model (see below)".to_owned()
                } else {
                    listed.join("; ")
                }
            );
            for out in &routes.left_out {
                println!("    left out: {}: {}", out.route, out.why);
            }
            let memory = &blueprint.memory;
            if !memory.is_empty() {
                let mut text = if memory.context.is_empty() {
                    "no knowledge in front of its tasks".to_owned()
                } else {
                    format!("reads {}", memory.context.join(", "))
                };
                if let Some(topic) = &memory.lessons {
                    text += &format!(
                        "; lessons go to '{topic}', kept {} days",
                        memory
                            .retention_days
                            .unwrap_or(crate::factory::memory::DEFAULT_RETENTION_DAYS)
                    );
                }
                println!("  memory: {text}");
            }
        }
        Err(_) => println!("{name} ({lifecycle})"),
    }
    println!("  written: {}", provenance_text(&draft));
    let builds = store.builds(name)?;
    if !builds.is_empty() {
        let short: Vec<&str> = builds.iter().map(|h| &h[..12]).collect();
        println!("  builds: {}", short.join(", "));
    }
    if let Some(state) = &state {
        println!("  history:");
        for change in state.history.iter().rev().take(10) {
            println!("    {}  {}  {}", change.at, change.by, change.what);
        }
    }
    let problems = problems_of(&store, name, &draft.blueprint, &catalog);
    println!("problems: {}", summary(&problems));
    print_problems(&problems);
    Ok(())
}

fn describe(blueprint: &Blueprint, lifecycle: &str) {
    println!("{} ({lifecycle}): {}", blueprint.name, blueprint.title);
    println!("  {}", blueprint.purpose);
    println!("  requirements:");
    for r in &blueprint.requirements {
        println!("    {} {}", r.id, r.text);
    }
    let caps = &blueprint.capabilities;
    let joined = |list: &[String]| {
        if list.is_empty() {
            "none".to_owned()
        } else {
            list.join(", ")
        }
    };
    let mcp: Vec<String> = caps
        .mcp
        .iter()
        .map(|m| format!("{} ({})", m.server, m.tools.join(", ")))
        .collect();
    println!(
        "  asks for: {} access; skills {}; MCP servers {}; checks {}; Microsoft 365 {}; delegation {}",
        caps.access,
        joined(&blueprint.skills),
        joined(&mcp),
        joined(&caps.checks),
        joined(&caps.workiq),
        if caps.delegate { "yes" } else { "no" }
    );
    let providers: Vec<String> = blueprint
        .model
        .providers
        .iter()
        .map(|p| p.to_string())
        .collect();
    println!("  runs on: {}", joined(&providers));
    let smoke = blueprint.evals.cases.iter().filter(|c| c.smoke).count();
    println!(
        "  evals: {} cases ({smoke} smoke), {} runs each",
        blueprint.evals.cases.len(),
        blueprint.evals.repetitions()
    );
    if !blueprint.approvers.is_empty() {
        println!("  approvers: {}", blueprint.approvers.join(", "));
    }
}

fn provenance_text(draft: &Draft) -> String {
    let p = &draft.provenance;
    let mut text = match p.source.as_str() {
        "model" => format!(
            "by {}'s model{}{} from a description",
            p.provider
                .map(|x| x.to_string())
                .unwrap_or_else(|| "a".into()),
            p.model
                .as_deref()
                .map(|m| format!(" ({m}"))
                .unwrap_or_default(),
            match (&p.model, &p.effort) {
                (Some(_), Some(e)) => format!(", {e})"),
                (Some(_), None) => ")".into(),
                _ => String::new(),
            }
        ),
        "template" => "from a template".into(),
        other => format!("from {other}"),
    };
    if let Some(template) = &p.template {
        text += &format!(" (template {template})");
    }
    if draft.tries > 1 {
        text += &format!(", in {} tries", draft.tries);
    }
    format!("{text}, {}", p.at)
}

fn edit(profile: &Profile, name: &str, from: Option<&Path>) -> Result<()> {
    let store = FactoryStore::of(profile);
    let draft = draft_of(&store, profile, name)?;
    let (text, edited) = match from {
        Some(path) => (read_input(path)?, None),
        None => {
            let (text, path) = in_editor(name, &draft.blueprint)?;
            (text, Some(path))
        }
    };
    let kept = || {
        edited
            .as_ref()
            .map(|p| format!(" (your edit is in {})", p.display()))
            .unwrap_or_default()
    };
    let blueprint: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| anyhow!("not saved: this isn't JSON: {e}{}", kept()))?;
    if blueprint.get("name").and_then(Value::as_str) != Some(name) {
        bail!(
            "not saved: the blueprint's name must stay \"{name}\"; for another name, start a new draft{}",
            kept()
        );
    }
    store.save_draft(
        name,
        &Draft {
            blueprint: blueprint.clone(),
            ..draft
        },
        &profile.name,
        "edited",
    )?;
    if let Some(path) = edited {
        let _ = std::fs::remove_file(path);
    }
    let catalog = Catalog::of(profile)?.without(name);
    let problems = problems_of(&store, name, &blueprint, &catalog);
    println!(
        "profile '{}': saved draft {name}: {}",
        profile.name,
        summary(&problems)
    );
    print_problems(&problems);
    Ok(())
}

/// Opens `blueprint` in the user's editor; the text they saved, and its file.
fn in_editor(name: &str, blueprint: &Value) -> Result<(String, PathBuf)> {
    let path = std::env::temp_dir().join(format!(
        "proofkin-{name}-{}.blueprint.json",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&path, serde_json::to_string_pretty(blueprint)? + "\n")
        .with_context(|| format!("writing {}", path.display()))?;
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".into()
            } else {
                "vi".into()
            }
        });
    let parts = crate::mcp::split_command(&editor)?;
    let (program, args) = parts
        .split_first()
        .context("VISUAL or EDITOR names no program")?;
    let status = std::process::Command::new(program)
        .args(args)
        .arg(&path)
        .status()
        .with_context(|| format!("starting the editor {editor:?} (set VISUAL or EDITOR)"))?;
    if !status.success() {
        bail!(
            "the editor ended with {status}: the draft wasn't changed (your edit is in {})",
            path.display()
        );
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok((text, path))
}

fn check(
    profile: &Profile,
    name: Option<&str>,
    file: Option<&Path>,
    folders: Option<Vec<String>>,
) -> Result<()> {
    let store = FactoryStore::of(profile);
    let (label, parsed, mut problems) = match (name, file) {
        (Some(name), None) => {
            let draft = draft_of(&store, profile, name)?;
            let catalog = Catalog::of(profile)?.without(name);
            let problems = problems_of(&store, name, &draft.blueprint, &catalog);
            let parsed = from_value(&draft.blueprint).ok().map(|b| (b, catalog));
            (name.to_owned(), parsed, problems)
        }
        (None, Some(path)) => {
            let label = path.display().to_string();
            let text = read_input(path)?;
            match crate::factory::blueprint::parse(&text) {
                Err(problem) => (label, None, vec![problem]),
                Ok(blueprint) => {
                    let catalog = Catalog::of(profile)?.without(&blueprint.name);
                    let mut problems = crate::factory::blueprint::validate(&blueprint, &catalog);
                    let fixtures = path.parent().unwrap_or(Path::new(".")).join("fixtures");
                    problems.extend(check_fixtures(&blueprint.evals, &fixtures));
                    (label, Some((blueprint, catalog)), problems)
                }
            }
        }
        _ => bail!("give a draft's name, or --file with a blueprint"),
    };
    // What it asks for, in the folders it would get it in.
    let resolved = parsed.as_ref().map(|(blueprint, catalog)| {
        let folders = folders
            .clone()
            .unwrap_or_else(|| blueprint.deploy.folders.clone());
        let resolution = capabilities::resolve(blueprint, &folders, catalog);
        (folders, resolution)
    });
    if let Some((_, resolution)) = &resolved {
        for problem in &resolution.problems {
            if !problems.contains(problem) {
                problems.push(problem.clone());
            }
        }
    }
    problems.sort_by_key(|p| !p.error);
    println!("{label}: {}", summary(&problems));
    print_problems(&problems);
    match &resolved {
        Some((folders, _)) if folders.is_empty() => println!(
            "no folders to resolve its capabilities in: it names no deploy folders (--folders A,B resolves them in some)"
        ),
        Some((_, resolution)) => print_resolution(resolution),
        None => {}
    }
    if has_errors(&problems) {
        let errors = problems.iter().filter(|p| p.error).count();
        bail!(
            "{label} has {errors} error{}",
            if errors == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/// What an agent's tasks would get, folder by folder.
fn print_resolution(resolution: &capabilities::Resolution) {
    println!("its tasks would get:");
    for grant in &resolution.grants {
        let mcp: Vec<String> = grant
            .mcp
            .iter()
            .map(|(server, tools)| {
                if tools.is_empty() {
                    format!("{server} (every tool)")
                } else {
                    format!("{server} ({})", tools.join(", "))
                }
            })
            .collect();
        let or_none = |list: &[String]| {
            if list.is_empty() {
                "none".to_owned()
            } else {
                list.join(", ")
            }
        };
        println!(
            "  in {}: {} access; MCP servers {}; checks {}",
            grant.folder,
            grant.access,
            or_none(&mcp),
            or_none(&grant.checks)
        );
    }
    if !resolution.workiq.is_empty() {
        println!(
            "  Microsoft 365 context: {} (your Work IQ account)",
            resolution.workiq.join(", ")
        );
    }
    if resolution.delegate {
        println!("  delegation: to the peers you allow on its worker's allowlist");
    }
}

/// Builds draft `name` and keeps the build by its hash.
fn build(profile: &Profile, name: &str) -> Result<()> {
    let store = FactoryStore::of(profile);
    let draft = draft_of(&store, profile, name)?;
    let catalog = Catalog::of(profile)?.without(name);
    let problems = problems_of(&store, name, &draft.blueprint, &catalog);
    if has_errors(&problems) {
        println!("{name}: {}", summary(&problems));
        print_problems(&problems);
        bail!(
            "{name} isn't built: its draft has errors (proofkin factory check {name} --profile {})",
            profile.name
        );
    }
    let blueprint = from_value(&draft.blueprint).map_err(|p| anyhow!("{p}"))?;
    let built = crate::factory::build::build(&blueprint, &profile.skills())?;
    let dir = store.save_build(&built)?;
    println!(
        "profile '{}': built {name}: {}",
        profile.name,
        built.short_hash()
    );
    for (path, text) in &built.files {
        println!(
            "  {}  {path} ({} bytes)",
            &crate::evidence::sha256_hex(text.as_bytes())[..12],
            text.len()
        );
    }
    println!("hash: {}", built.hash);
    println!("in: {}", dir.display());
    Ok(())
}

fn print_problems(problems: &[Problem]) {
    for p in problems {
        let at = p
            .path
            .as_deref()
            .map(|x| format!("{x}  "))
            .unwrap_or_default();
        println!(
            "  {:<8} {at}{}",
            if p.error { "error" } else { "warning" },
            p.message
        );
        if let Some(fix) = &p.fix {
            println!("  {:<8} fix: {fix}", "");
        }
    }
}

/// A file's text, or stdin's for `-`.
fn read_input(path: &Path) -> Result<String> {
    if path.as_os_str() == "-" {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        Ok(text)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
    }
}
