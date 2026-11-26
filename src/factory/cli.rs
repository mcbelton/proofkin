//! `proofkin factory`: the agent factory from the command line. It lives here rather
//! than in `main.rs` so the factory's commands grow next to its code.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use serde_json::Value;

use crate::agent::{Agent, AgentConfig, MAX_CONCURRENT_TASKS, Network};
use crate::factory::blueprint::{
    Blueprint, Catalog, Problem, from_value, has_errors, name_problem, summary,
};
use crate::factory::evals::check_fixtures;
use crate::factory::report::{EvalState, Evaluation, RunState};
use crate::factory::runner::{self, Judges, Planned, Request, Runner};
use crate::factory::store::{Draft, FactoryStore, Provenance, now};
use crate::factory::{capabilities, code, draft, improve, manage, routing, templates};
use crate::llm::{ExecutorConfig, Provider, SdkExecutor};
use crate::profile::Profile;
use crate::workers::{Launch, Supervisor};

#[derive(Args, Debug)]
pub struct ProfileOpt {
    /// The profile whose factory it is.
    #[arg(long = "profile", id = "profile")]
    pub name: String,
}

#[derive(Subcommand, Debug)]
pub enum FactoryCommand {
    /// List the built-in templates: what each is for, the most access it asks for, how
    /// many eval cases it starts with and what its evaluation needs, if anything.
    Templates,
    /// Start an agent: a draft from a template, or one the profile's model writes
    /// from your words (read-only, in an empty folder; it grants nothing), or from a
    /// folder made by `factory init`.
    #[command(after_help = "\
examples:
  proofkin factory new reviewer --template code-reviewer --profile alice
  proofkin factory new fin-analyst --describe \"Analyze quarterly reports, extract key metrics, flag risks, write an executive summary\" --profile alice
  proofkin factory new fin-analyst --template financial-analyst --describe \"...for our board, in euros\" --using claude --profile alice
  proofkin factory new ask --from ./my-repo --profile alice")]
    New {
        /// The agent's name.
        name: String,
        /// Start from this template (proofkin factory templates lists them); the files its
        /// cases work on go in the agent's fixtures folder, keeping any there already.
        #[arg(long)]
        template: Option<String>,
        /// What the agent should do, in your words: the profile's model writes the draft.
        #[arg(long)]
        describe: Option<String>,
        /// Start from the agent kept as code in this folder (agent/blueprint.json and
        /// agent/evals.json; its fixtures are copied), or a blueprint file.
        #[arg(long, conflicts_with_all = ["template", "describe"])]
        from: Option<PathBuf>,
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
    /// file (- for stdin) or a folder made by `factory init` (its fixtures are copied too).
    /// It is checked when saved and kept with its problems.
    Edit {
        name: String,
        /// Take the blueprint from this file, or this folder's agent/, instead of an editor.
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
        /// Check this blueprint file instead (its fixtures are in fixtures/ next to it), or
        /// the agent kept as code in this folder (agent/).
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
    /// Evaluate a draft: its build's eval cases and the built-in adversarial ones, on each
    /// candidate model, a few times each, as real tasks of your evaluation worker
    /// (<profile>-eval, made the first time) at the access the agent asks for. Without
    /// --yes it only says what it would run.
    #[command(after_help = "\
examples:
  proofkin factory evaluate fin-analyst --profile alice
  proofkin factory evaluate fin-analyst --candidate copilot:gpt-5:high --candidate copilot:claude-sonnet-4.5 --repetitions 3 --yes --profile alice
Each run is graded by its deterministic expectations; a judge of another model family
scores rubrics and never decides. The release gate needs 90% of the agent's own runs, 2 of
3 of each requirement's and every adversarial run to pass. Run it again to go on with an
evaluation of the same build that didn't end; Ctrl+C cancels it.")]
    Evaluate {
        name: String,
        /// A model to evaluate it on: provider:model[:effort] (repeat it; default: those its
        /// blueprint lists, else its providers' defaults).
        #[arg(long = "candidate", value_name = "PROVIDER:MODEL[:EFFORT]")]
        candidates: Vec<String>,
        /// Runs per case and candidate, 1 to 5 (default: its suite's, else 3).
        #[arg(long)]
        repetitions: Option<u32>,
        /// Run it: give the tasks the estimate counts to your evaluation worker.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        profile: ProfileOpt,
        /// Node bridge script for the Claude and Codex SDKs (default: the one `setup` installed).
        #[arg(long, env = "PROOFKIN_BRIDGE")]
        bridge: Option<PathBuf>,
    },
    /// An evaluation's report: what ran, the release gate, each candidate's results and
    /// the failing runs with their outputs; --json prints the signed record.
    Report {
        /// The evaluation's id (ev-...), as `factory show` lists them.
        id: String,
        #[arg(long)]
        json: bool,
        /// Write it to this file instead.
        #[arg(long)]
        out: Option<PathBuf>,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Keep an agent as code: write its files into a folder under git, for plan steps to
    /// prove with the built-in checks agent:blueprint, agent:evals and agent:report.
    #[command(after_help = "\
writes:
  agent/blueprint.json   its blueprint, without its eval suite
  agent/evals.json       its eval suite
  agent/fixtures/        files its cases work on (the template's, if it has any)
  agent/skills/          skills of its own (<name>.md), next to the built-in ones
  agent/reports/         its evaluations' signed reports
examples:
  proofkin factory init . --name ask --template researcher
  proofkin spec check agent --dir .
  proofkin factory new ask --from . --profile alice
  proofkin factory evaluate ask --yes --profile alice
  proofkin factory report ev-20261124-091500-ask --json --out agent/reports/ev-20261124-091500-ask.json --profile alice")]
    Init {
        /// The folder (usually a repository).
        dir: PathBuf,
        /// The agent's name.
        #[arg(long)]
        name: String,
        /// The template it starts from (proofkin factory templates lists them).
        #[arg(long, default_value = "researcher")]
        template: String,
        /// Fit it to this profile: the providers it is signed in to.
        #[arg(long = "profile")]
        profile: Option<String>,
    },
    /// Delete an agent's draft and records (not an agent with releases: their records
    /// are kept).
    Delete {
        name: String,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Release an evaluated build: a semantic version and a manifest of its files' hashes,
    /// signed with your identity key. It waits for the approvers its blueprint names
    /// (and, when it asks for more than the active release, for you, last).
    #[command(after_help = "\
examples:
  proofkin factory release fin-analyst --evaluation ev-20261124-091500-fin-analyst --note \"Cites pages\" --profile alice
  proofkin factory release fin-analyst --from D:\\src\\fin-analyst --profile alice   (an agent kept as code, at its commit)
The version is the next patch by default; new requirements or capabilities make it minor,
a changed or removed requirement major. You may choose a higher one, never a lower one.")]
    Release {
        name: String,
        /// The evaluation that passed the gate for exactly the build to release.
        #[arg(long, required_unless_present = "from")]
        evaluation: Option<String>,
        /// The version to give it (MAJOR.MINOR.PATCH; default: what its change needs).
        #[arg(long)]
        version: Option<String>,
        /// What changed, for its approvers (at most 1,000 characters).
        #[arg(long)]
        note: Option<String>,
        /// Release the agent kept as code in this folder, at its commit, with the passed
        /// report in its agent/reports.
        #[arg(long, conflicts_with = "evaluation")]
        from: Option<PathBuf>,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Sign off (or reject) a release that waits for you; approvers on your allowlist are
    /// asked by your agent while it runs.
    Approve {
        name: String,
        /// The release (default: the one waiting).
        #[arg(long)]
        version: Option<String>,
        /// Reject it: the active release stays.
        #[arg(long)]
        reject: bool,
        #[arg(long, default_value = "")]
        note: String,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Check a release offline (NAME@VERSION), or every release of an agent: its
    /// signature, every file's hash, its report and its approvals.
    Verify {
        /// NAME or NAME@VERSION.
        target: String,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// What changed between two releases: requirements, instructions, skills,
    /// capabilities, model, budgets and files.
    Diff {
        name: String,
        from: String,
        to: String,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Deploy a release as a digital worker named after the agent, pinned to it: every
    /// task it serves gets the release's instructions and skills first. Its canary runs
    /// the release's smoke cases on it before it takes work.
    #[command(after_help = "\
examples:
  proofkin factory deploy fin-analyst --start --profile alice              (its active release, in its deploy folders; canary first)
  proofkin factory deploy fin-analyst --version 1.1.0 --folders reports --start --profile alice
Peers delegate to the worker as to any worker, within their grants, the folder's access
and the agent's budgets. If its canary fails it goes back to the release it ran before,
or stops when there was none.")]
    Deploy {
        name: String,
        /// The release (default: the active one).
        #[arg(long)]
        version: Option<String>,
        /// The shared folders it works in (comma-separated; default: its deploy folders).
        #[arg(long, value_delimiter = ',')]
        folders: Vec<String>,
        /// Take work at once, without running its smoke cases first.
        #[arg(long)]
        no_canary: bool,
        /// Start its worker now (and with your agent from now on), and run its canary.
        #[arg(long)]
        start: bool,
        #[command(flatten)]
        profile: ProfileOpt,
        /// Node bridge script for the Claude and Codex SDKs (default: the one `setup` installed).
        #[arg(long, env = "PROOFKIN_BRIDGE")]
        bridge: Option<PathBuf>,
    },
    /// Make an earlier release active on every deployment (within seconds), audited.
    Rollback {
        name: String,
        /// The release to go back to (default: the one before the active one).
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        reason: String,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Deprecate an agent: plan checks warn about it and name its replacement.
    Deprecate {
        name: String,
        #[arg(long)]
        replacement: String,
        #[arg(long)]
        reason: Option<String>,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Make a deprecated agent active again.
    Reactivate {
        name: String,
        #[arg(long)]
        reason: Option<String>,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Retire an agent: plans can't use it, its deployments stop, and its releases and
    /// reports are kept for audit (--purge: deleted 90 days from now).
    Retire {
        name: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        purge: bool,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// An agent's metrics per version, from its deployments' audits: tasks, done,
    /// failed, refused, p50 and p95 duration, attempts, check pass rate, models and usage.
    Metrics {
        name: String,
        /// Since when: 7d, 24h, a date or a time.
        #[arg(long, default_value = "7d")]
        since: String,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Change a deployed agent's budgets: its worker refuses a task that would go over
    /// them before any model call, and stops one that runs too long.
    Budget {
        name: String,
        #[arg(long)]
        tasks_per_day: Option<u32>,
        /// Model attempts a task may ask for, 1 to 5.
        #[arg(long)]
        attempts: Option<u32>,
        /// The longest a task runs (e.g. 20m, 1h).
        #[arg(long)]
        timeout: Option<String>,
        /// Back to its release's budgets.
        #[arg(long)]
        clear: bool,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Watch an agent for drift: its active release evaluated on its deployment on a
    /// schedule (every case once, the built-in attacks too), judged by its release's gate and
    /// within its budgets. fail-after failed runs in a row open an incident and alert you
    /// with the failing cases. Without options it shows the watch.
    #[command(after_help = "\
examples:
  proofkin factory watch fin-analyst --when \"daily 03:00\" --fail-after 2 --profile alice
  proofkin factory watch fin-analyst --now --profile alice    (one run now, here: your agent stopped)
  proofkin factory watch fin-analyst --off --profile alice    (paused; its open incident closes)
Your agent runs the watch while it runs. Each run's pass rate is an ops sample of monitor
factory:fin-analyst, and its incidents are ops incidents (proofkin ops report).")]
    Watch {
        name: String,
        /// When it runs: daily 03:00, every 6h, weekdays 08:30, mon,fri 17:00.
        #[arg(long)]
        when: Option<String>,
        /// Failed runs in a row that open an incident, 1 to 10 (default 2).
        #[arg(long)]
        fail_after: Option<u32>,
        /// Pause it: it runs nothing, and its open incident closes.
        #[arg(long, conflicts_with_all = ["when", "fail_after", "now"])]
        off: bool,
        /// Run it once now, here (your agent must be stopped): its deployment's worker is
        /// started for it, and stopped after.
        #[arg(long)]
        now: bool,
        #[command(flatten)]
        profile: ProfileOpt,
        /// Node bridge script for the Claude and Codex SDKs (default: the one `setup` installed).
        #[arg(long, env = "PROOFKIN_BRIDGE")]
        bridge: Option<PathBuf>,
    },
    /// Propose a better version of an agent from what failed: your model gets its blueprint,
    /// its failing cases with their outputs (secrets redacted), its deployments' failed tasks
    /// (outcomes only, never their text) and your note. The proposal is a new draft version
    /// with regression cases, shown as a diff next to the active release; it asks for nothing
    /// more, and only a release changes what runs.
    #[command(after_help = "\
examples:
  proofkin factory improve fin-analyst --profile alice                  (every case that failed)
  proofkin factory improve fin-analyst --cases E2,E5 --note \"Quote the sentence, don't paraphrase it\" --profile alice
  proofkin factory improve fin-analyst --evaluate --profile alice       (then evaluate it, your agent stopped)
Its draft replaces the agent's draft (the proposal keeps the one it replaced). Release it
once its evaluation passes, or edit it, or leave it: the active release runs meanwhile.
An imported agent is its publisher's: they improve it, and you import their next version.")]
    Improve {
        name: String,
        /// The cases to improve (comma-separated; default: every case that failed in the last
        /// evaluation of what runs).
        #[arg(long, value_delimiter = ',')]
        cases: Vec<String>,
        /// What to improve, in your words (at most 2,000 characters).
        #[arg(long)]
        note: Option<String>,
        /// The provider whose model writes it (default: the first one the profile is signed
        /// in to).
        #[arg(long, value_enum)]
        using: Option<Provider>,
        /// Then evaluate it on the model its active release runs on (your agent must be
        /// stopped).
        #[arg(long)]
        evaluate: bool,
        #[command(flatten)]
        profile: ProfileOpt,
        /// Node bridge script for the Claude and Codex SDKs (default: the one `setup` installed).
        #[arg(long, env = "PROOFKIN_BRIDGE")]
        bridge: Option<PathBuf>,
    },
    /// Publish released versions of an agent to peers on your allowlist: they see them in
    /// their catalog (name, version, description, most access and an eval summary; never
    /// knowledge, outputs or paths), and may import them. --to says who, in place of whom
    /// it was published to; --off unpublishes it.
    #[command(after_help = "\
examples:
  proofkin factory publish fin-analyst --to bob,dana --profile alice      (its active release)
  proofkin factory publish fin-analyst --to bob --versions 1.2.0,1.1.0 --profile alice
  proofkin factory publish fin-analyst --off --profile alice
The grants live in your allowlist (proofkin peers allow ... --agents fin-analyst grants too).
Your agent sends granted peers their catalog in its hello and whenever it changes; while it
runs, publish in its web UI (Agent factory). Retiring an agent unpublishes it.")]
    Publish {
        name: String,
        /// The peers to publish it to, by their names on your allowlist (comma-separated;
        /// default: those it is published to).
        #[arg(long, value_delimiter = ',')]
        to: Vec<String>,
        /// The released versions they see (comma-separated; default: its active release).
        #[arg(long, value_delimiter = ',')]
        versions: Vec<String>,
        /// Unpublish it: no peer sees it any more.
        #[arg(long, conflicts_with_all = ["to", "versions"])]
        off: bool,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// List the agent releases peers publish to you: each one's publisher (its name on your
    /// allowlist and its key), what it is for, the most access it asks for and how its
    /// evaluation went. This connects to your peers for a moment (your agent must be
    /// stopped; while it runs, its web UI lists them under Agent factory › Catalog).
    Catalog {
        /// How long to wait for your peers to connect, in seconds.
        #[arg(long, default_value_t = 15)]
        wait: u64,
        /// How to reach peers: over the internet (relays, NAT traversal) or direct IPs only.
        #[arg(long, value_enum, default_value_t = Network::Internet)]
        network: Network,
        #[command(flatten)]
        profile: ProfileOpt,
    },
    /// Import an agent release a peer publishes to you: its bundle is asked for over iroh,
    /// verified (the manifest signed by the publisher's key, every file's hash) and kept;
    /// nothing runs. It becomes active once it passes an evaluation on your accounts
    /// (factory evaluate NAME --yes), or at once with --accept-publisher-report.
    #[command(after_help = "\
examples:
  proofkin factory import alice/fin-analyst@1.2.0 --profile bob
  proofkin factory evaluate fin-analyst --yes --profile bob                 (on your accounts: then it is active)
  proofkin factory import alice/fin-analyst@1.2.0 --accept-publisher-report --profile bob
Your agent must be stopped: this connects to the publisher for a moment (while your agent
runs, import from its web UI: Agent factory › Catalog).")]
    Import {
        /// PUBLISHER/NAME@VERSION, the publisher by its name on your allowlist.
        target: String,
        /// Make it active from the publisher's report, without an evaluation here (recorded
        /// in its history).
        #[arg(long)]
        accept_publisher_report: bool,
        /// How long to wait for the publisher to connect, in seconds.
        #[arg(long, default_value_t = 30)]
        wait: u64,
        /// How to reach peers: over the internet (relays, NAT traversal) or direct IPs only.
        #[arg(long, value_enum, default_value_t = Network::Internet)]
        network: Network,
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
                let needs = t.needs.map(|n| format!("; {n}")).unwrap_or_default();
                println!(
                    "  {:<20} {:<16} {:>5}  {}{needs}",
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
            from,
            using,
            profile,
            bridge,
        } => {
            new(
                &name,
                template,
                describe,
                from.as_deref(),
                using,
                &profile.name,
                bridge,
            )
            .await?
        }
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
        FactoryCommand::Evaluate {
            name,
            candidates,
            repetitions,
            yes,
            profile,
            bridge,
        } => {
            evaluate(
                &Profile::open(&profile.name)?,
                &name,
                &candidates,
                repetitions,
                yes,
                bridge,
            )
            .await?
        }
        FactoryCommand::Report {
            id,
            json,
            out,
            profile,
        } => report(&Profile::open(&profile.name)?, &id, json, out.as_deref())?,
        FactoryCommand::Init {
            dir,
            name,
            template,
            profile,
        } => {
            let catalog = match &profile {
                Some(profile) => Some(Catalog::of(&Profile::open(profile)?)?),
                None => None,
            };
            let written = code::init(&dir, &name, &template, catalog.as_ref())?;
            println!(
                "wrote agent {name} (template {template}) in {}: {}",
                dir.display(),
                written.join(", ")
            );
            println!(
                "next: proofkin spec check agent --dir {0}   (agent:blueprint, agent:evals)\n      proofkin factory new {name} --from {0} --profile PROFILE   (a draft to evaluate)",
                dir.display()
            );
        }
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
        FactoryCommand::Release {
            name,
            evaluation,
            version,
            note,
            from,
            profile,
        } => manage::release(
            &Profile::open(&profile.name)?,
            &name,
            evaluation,
            version,
            note,
            from.as_deref(),
        )?,
        FactoryCommand::Approve {
            name,
            version,
            reject,
            note,
            profile,
        } => manage::approve(
            &Profile::open(&profile.name)?,
            &name,
            version.as_deref(),
            reject,
            &note,
        )?,
        FactoryCommand::Verify { target, profile } => {
            manage::verify(&Profile::open(&profile.name)?, &target)?
        }
        FactoryCommand::Diff {
            name,
            from,
            to,
            profile,
        } => manage::diff(&Profile::open(&profile.name)?, &name, &from, &to)?,
        FactoryCommand::Deploy {
            name,
            version,
            folders,
            no_canary,
            start,
            profile,
            bridge,
        } => {
            manage::deploy(
                &Profile::open(&profile.name)?,
                &name,
                version,
                (!folders.is_empty()).then_some(folders),
                no_canary,
                start,
                bridge,
            )
            .await?
        }
        FactoryCommand::Rollback {
            name,
            to,
            reason,
            profile,
        } => manage::rollback(
            &Profile::open(&profile.name)?,
            &name,
            to.as_deref(),
            &reason,
        )?,
        FactoryCommand::Deprecate {
            name,
            replacement,
            reason,
            profile,
        } => manage::deprecate(
            &Profile::open(&profile.name)?,
            &name,
            &replacement,
            reason.as_deref(),
        )?,
        FactoryCommand::Reactivate {
            name,
            reason,
            profile,
        } => manage::reactivate(&Profile::open(&profile.name)?, &name, reason.as_deref())?,
        FactoryCommand::Retire {
            name,
            reason,
            purge,
            profile,
        } => manage::retire(&Profile::open(&profile.name)?, &name, &reason, purge)?,
        FactoryCommand::Metrics {
            name,
            since,
            json,
            profile,
        } => manage::print_metrics(&Profile::open(&profile.name)?, &name, &since, json)?,
        FactoryCommand::Budget {
            name,
            tasks_per_day,
            attempts,
            timeout,
            clear,
            profile,
        } => manage::budget(
            &Profile::open(&profile.name)?,
            &name,
            tasks_per_day,
            attempts,
            timeout.as_deref(),
            clear,
        )?,
        FactoryCommand::Watch {
            name,
            when,
            fail_after,
            off,
            now,
            profile,
            bridge,
        } => {
            manage::watch(
                &Profile::open(&profile.name)?,
                &name,
                manage::WatchChange {
                    when,
                    fail_after,
                    off,
                    now,
                },
                bridge,
            )
            .await?
        }
        FactoryCommand::Improve {
            name,
            cases,
            note,
            using,
            evaluate,
            profile,
            bridge,
        } => {
            improve(
                &Profile::open(&profile.name)?,
                &name,
                &cases,
                note.as_deref(),
                using,
                evaluate,
                bridge,
            )
            .await?
        }
        FactoryCommand::Publish {
            name,
            to,
            versions,
            off,
            profile,
        } => manage::publish(&Profile::open(&profile.name)?, &name, to, versions, off)?,
        FactoryCommand::Catalog {
            wait,
            network,
            profile,
        } => manage::catalog(&Profile::open(&profile.name)?, wait, network).await?,
        FactoryCommand::Import {
            target,
            accept_publisher_report,
            wait,
            network,
            profile,
        } => {
            manage::import(
                &Profile::open(&profile.name)?,
                &target,
                accept_publisher_report,
                wait,
                network,
            )
            .await?
        }
    }
    Ok(())
}

/// Has the profile's model propose an improvement of agent `name` (see
/// [`crate::factory::improve`]), keeps it as its draft and prints its diff; with
/// `evaluate_it`, evaluates it on the model its active release runs on.
async fn improve(
    profile: &Profile,
    name: &str,
    cases: &[String],
    note: Option<&str>,
    using: Option<Provider>,
    evaluate_it: bool,
    bridge: Option<PathBuf>,
) -> Result<()> {
    let store = FactoryStore::of(profile);
    let material = improve::material(profile, name, cases, note)?;
    let catalog = Catalog::of(profile)?.without(name);
    let provider = match using {
        Some(provider) if catalog.providers.contains(&provider) => provider,
        Some(provider) => bail!(
            "profile '{}' isn't signed in to {provider}: {}",
            profile.name,
            profile.login_hint(provider)
        ),
        None => *catalog.providers.first().ok_or_else(|| {
            anyhow!(
                "profile '{}' isn't signed in to a provider whose model could write the proposal: proofkin auth login copilot --profile {0}",
                profile.name
            )
        })?,
    };
    let mut from = Vec::new();
    if !material.failures.is_empty() {
        from.push(plural(
            material.failures.len(),
            "failing case",
            "failing cases",
        ));
    }
    if !material.tasks.is_empty() {
        from.push(plural(material.tasks.len(), "failed task", "failed tasks"));
    }
    if material.note.is_some() {
        from.push("your note".to_owned());
    }
    let from = match from.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => "nothing".to_owned(),
    };
    println!("{name}: improving {} from {from}", material.basis_of);
    let bridge = crate::setup::resolve_bridge(
        bridge.or_else(|| crate::profile::env_setting_os("BRIDGE").map(PathBuf::from)),
    );
    let executor = SdkExecutor::start(ExecutorConfig {
        providers: vec![provider],
        bridge_script: bridge.clone(),
        profile: profile.clone(),
    })
    .await?;
    let proposed = improve::propose(
        executor.as_ref(),
        &store,
        &material,
        &catalog,
        provider,
        &|line: String| println!("  {line}"),
    )
    .await;
    executor.stop().await;
    let proposed = proposed?;
    let proposal = improve::keep(
        &store,
        &material,
        &proposed,
        &profile.name,
        &profile.skills(),
    )?;
    println!(
        "{name}: improvement {}: a new draft version of {} (what runs doesn't change until you release it)",
        proposal.id, material.basis_of
    );
    if proposal.diff.is_empty() {
        println!("  it changes nothing but its wording");
    }
    for line in &proposal.diff {
        println!("  {line}");
    }
    if proposal.instructions.iter().any(|l| !l.starts_with("  ")) {
        println!("  instructions:");
        for line in &proposal.instructions {
            println!("    {line}");
        }
    }
    if !proposal.regression.is_empty() {
        println!("  regression cases: {}", proposal.regression.join(", "));
    }
    print_problems(&proposed.problems);
    let candidate = improve::candidate_of(&store, name);
    if !evaluate_it {
        println!(
            "next: proofkin factory evaluate {name}{} --yes --profile {}",
            candidate
                .as_deref()
                .map(|c| format!(" --candidate {c}"))
                .unwrap_or_default(),
            profile.name
        );
        return Ok(());
    }
    let candidates: Vec<String> = candidate.into_iter().collect();
    evaluate(profile, name, &candidates, None, true, Some(bridge)).await
}

async fn new(
    name: &str,
    template: Option<String>,
    describe: Option<String>,
    from: Option<&Path>,
    using: Option<Provider>,
    profile: &str,
    bridge: Option<PathBuf>,
) -> Result<()> {
    let profile = Profile::open(profile)?;
    let catalog = Catalog::of(&profile)?;
    if let Some(from) = from {
        return new_from(&profile, &catalog, name, from);
    }
    let (draft, what) = match (describe, template) {
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
            (written.draft, what)
        }
        (None, Some(template)) => {
            if let Some(problem) = name_problem(name, &catalog) {
                bail!("{problem}");
            }
            let blueprint = templates::instantiate(&template, name, &catalog)?;
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
            (draft, format!("drafted from template {template}"))
        }
        (None, None) => bail!(
            "give --template NAME (proofkin factory templates lists them), --describe \"what the agent should do\" or --from FOLDER"
        ),
    };
    let store = FactoryStore::of(&profile);
    store.create_draft(name, &draft, &profile.name, &what)?;
    // The files the template's cases work on.
    let fixtures = match &draft.provenance.template {
        Some(template) => templates::write_fixtures(template, &store.fixtures_dir(name))?,
        None => Vec::new(),
    };
    let with = match fixtures.len() {
        0 => String::new(),
        n => format!(", {}", plural(n, "fixture", "fixtures")),
    };
    let problems = problems_of(&store, name, &draft.blueprint, &catalog.without(name));
    println!(
        "profile '{}': created draft {name} ({what}{with}): {}",
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

/// The blueprint at `path` (a file, - for stdin, or a folder whose agent is kept as code),
/// as text, and the folder its fixtures are in.
fn blueprint_at(path: &Path) -> Result<(String, PathBuf)> {
    if path.is_dir() {
        let value = code::blueprint_value(path)?;
        return Ok((
            serde_json::to_string_pretty(&value)?,
            path.join(code::FIXTURES),
        ));
    }
    let fixtures = path.parent().unwrap_or(Path::new(".")).join("fixtures");
    Ok((read_input(path)?, fixtures))
}

/// Starts draft `name` from the agent kept as code in folder `from` (or a blueprint file),
/// with its fixtures.
fn new_from(profile: &Profile, catalog: &Catalog, name: &str, from: &Path) -> Result<()> {
    if let Some(problem) = name_problem(name, catalog) {
        bail!("{problem}");
    }
    let (text, fixtures) = blueprint_at(from)?;
    let blueprint: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| anyhow!("{} isn't JSON: {e}", from.display()))?;
    if blueprint.get("name").and_then(Value::as_str) != Some(name) {
        bail!(
            "the agent in {} isn't named \"{name}\": give its name",
            from.display()
        );
    }
    let store = FactoryStore::of(profile);
    let draft = Draft {
        blueprint,
        provenance: Provenance {
            source: "file".into(),
            at: now(),
            ..Provenance::default()
        },
        tries: 0,
    };
    let what = format!("drafted from {}", from.display());
    store.create_draft(name, &draft, &profile.name, &what)?;
    let copied = code::copy_fixtures_from(&fixtures, &store.fixtures_dir(name))?;
    let problems = problems_of(
        &store,
        name,
        &draft.blueprint,
        &catalog.clone().without(name),
    );
    println!(
        "profile '{}': created draft {name} ({what}, {}): {}",
        profile.name,
        plural(copied, "fixture", "fixtures"),
        summary(&problems)
    );
    print_problems(&problems);
    Ok(())
}

/// The problems of agent `name`'s draft: the blueprint's, and its fixtures'.
fn problems_of(
    store: &FactoryStore,
    name: &str,
    blueprint: &Value,
    catalog: &Catalog,
) -> Vec<Problem> {
    crate::factory::runner::draft_problems(store, name, blueprint, catalog)
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
    let state = store.state(name)?;
    // An import has no draft: it is what its release (or the bundle it waits with) says.
    let draft = match (store.draft(name)?, &state) {
        (Some(draft), _) => Some(draft),
        (None, Some(_)) => None,
        (None, None) => Some(draft_of(&store, profile, name)?),
    };
    let blueprint = match &draft {
        Some(d) => d.blueprint.clone(),
        None => serde_json::to_value(undrafted_blueprint(&store, name, state.as_ref())?)?,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&blueprint)?);
        return Ok(());
    }
    let lifecycle = state
        .as_ref()
        .map(|s| s.state.to_string())
        .unwrap_or_else(|| "draft".into());
    let catalog = Catalog::of(profile)?.without(name);
    match from_value(&blueprint) {
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
    let written = match (&draft, state.as_ref().and_then(|s| s.imported.as_ref())) {
        (Some(d), _) => provenance_text(d),
        (None, Some(o)) => format!(
            "by its publisher {} (imported; key {})",
            o.publisher,
            &o.publisher_id[..10.min(o.publisher_id.len())]
        ),
        (None, None) => "elsewhere: it has no draft here".into(),
    };
    println!("  written: {written}");
    let builds = store.builds(name)?;
    if !builds.is_empty() {
        let short: Vec<&str> = builds.iter().map(|h| &h[..12]).collect();
        println!("  builds: {}", short.join(", "));
    }
    let evaluations = store.evaluations(name)?;
    if !evaluations.is_empty() {
        let running = store.evaluation_running();
        println!(
            "  evaluations (proofkin factory report ID --profile {}):",
            profile.name
        );
        for e in evaluations.iter().take(5) {
            println!("    {}  {}", e.id, evaluation_line(e, running));
        }
    }
    manage::print_releases(profile, name)?;
    if let Some(state) = &state {
        println!("  history:");
        for change in state.history.iter().rev().take(10) {
            println!("    {}  {}  {}", change.at, change.by, change.what);
        }
    }
    let Some(draft) = &draft else {
        return Ok(());
    };
    let problems = problems_of(&store, name, &draft.blueprint, &catalog);
    println!("problems: {}", summary(&problems));
    print_problems(&problems);
    Ok(())
}

/// The blueprint of agent `name`, which has no draft here: its active release's, else the
/// imported version's it waits with.
fn undrafted_blueprint(
    store: &FactoryStore,
    name: &str,
    state: Option<&crate::factory::store::AgentState>,
) -> Result<Blueprint> {
    if let Some(version) = state.and_then(|s| s.active_version.as_deref()) {
        return crate::factory::release::blueprint_of(store, name, version);
    }
    match crate::factory::catalog::pending(store, name)? {
        Some((_, build)) => runner::blueprint_of(&build),
        None => bail!("{name} has neither a draft nor a release here"),
    }
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
            "by {}'s model{}{} from your words",
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
    let (text, edited, fixtures) = match from {
        Some(path) => {
            let (text, fixtures) = blueprint_at(path)?;
            (text, None, path.is_dir().then_some(fixtures))
        }
        None => {
            let (text, path) = in_editor(name, &draft.blueprint)?;
            (text, Some(path), None)
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
    // A folder's agent comes with its fixtures.
    if let Some(fixtures) = fixtures {
        code::copy_fixtures_from(&fixtures, &store.fixtures_dir(name))?;
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
            let (text, fixtures) = blueprint_at(path)?;
            match crate::factory::blueprint::parse(&text) {
                Err(problem) => (label, None, vec![problem]),
                Ok(blueprint) => {
                    let catalog = Catalog::of(profile)?.without(&blueprint.name);
                    let mut problems = crate::factory::blueprint::validate(&blueprint, &catalog);
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

/// An evaluation in a line: its build and verdict, or how far it got. `running`: an
/// evaluation runs now (only one does at a time).
fn evaluation_line(e: &Evaluation, running: bool) -> String {
    let build = &e.build[..12.min(e.build.len())];
    match e.state {
        EvalState::Running if running => {
            format!(
                "build {build}: running, {} of {} runs done",
                e.done(),
                e.runs.len()
            )
        }
        EvalState::Running => format!(
            "build {build}: stopped with {} of {} runs done (evaluate it again to go on)",
            e.done(),
            e.runs.len()
        ),
        EvalState::Passed => format!(
            "build {build}: passed{}",
            e.recommended
                .as_ref()
                .map(|r| format!(", recommended {r}"))
                .unwrap_or_default()
        ),
        EvalState::Failed => format!(
            "build {build}: failed ({})",
            e.gate.reasons.first().map(String::as_str).unwrap_or("")
        ),
        EvalState::Cancelled => format!(
            "build {build}: cancelled{}",
            e.error
                .as_deref()
                .map(|why| format!(" ({why})"))
                .unwrap_or_default()
        ),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Evaluates draft `name`: says what it would run, and runs it with `yes`.
async fn evaluate(
    profile: &Profile,
    name: &str,
    candidates: &[String],
    repetitions: Option<u32>,
    yes: bool,
    bridge: Option<PathBuf>,
) -> Result<()> {
    let asked = candidates
        .iter()
        .map(|c| runner::parse_candidate(c))
        .collect::<Result<Vec<_>>>()?;
    let planned = runner::plan(profile, name, &asked, repetitions, None).map_err(|refused| {
        print_problems(&refused.problems);
        anyhow!("{}", refused.message)
    })?;
    let store = FactoryStore::of(profile);
    let resumable = runner::unfinished(&store, name, &planned.build.hash)?;
    let estimate = runner::estimate(
        &planned.blueprint,
        planned.candidates.len(),
        planned.repetitions,
        MAX_CONCURRENT_TASKS,
    );
    println!("{name}: build {}", planned.build.short_hash());
    let listed: Vec<String> = planned.candidates.iter().map(|c| c.to_string()).collect();
    println!("  candidates: {}", listed.join("; "));
    for out in &planned.left_out {
        println!("    left out: {}: {}", out.route, out.why);
    }
    println!(
        "  estimate: {} ({} built-in adversarial) x {} x {} = {}{}: about {} on {}",
        plural(estimate.cases, "case", "cases"),
        estimate.adversarial,
        plural(estimate.repetitions as usize, "run", "runs"),
        plural(estimate.candidates, "candidate", "candidates"),
        plural(estimate.runs, "run", "runs"),
        if estimate.judge_calls > 0 {
            format!(
                ", and {} by judges",
                plural(estimate.judge_calls, "score", "scores")
            )
        } else {
            String::new()
        },
        plural(estimate.minutes as usize, "minute", "minutes"),
        plural(estimate.slots, "task slot", "task slots"),
    );
    if let Some(e) = &resumable {
        println!(
            "  {} of this build didn't end ({} of {} runs done): it goes on from there",
            e.id,
            e.done(),
            e.runs.len()
        );
    }
    let worker = runner::worker_name(&profile.name);
    if !yes {
        println!(
            "nothing ran: run it with --yes (its tasks go to your evaluation worker {worker})"
        );
        return Ok(());
    }
    let _instance = profile.lock_instance().map_err(|_| {
        anyhow!(
            "profile '{}' runs in a proofkin agent: evaluate {name} in its web UI (Agent factory), or stop that agent first",
            profile.name
        )
    })?;
    let _running = store.lock_evaluation()?;
    let key = profile.identity()?;
    // The owner's agent, on this machine only: it gives the runs to its evaluation worker.
    let mut config = AgentConfig::new(profile.name.clone(), key.clone());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse()?);
    config.trust = profile.trust()?;
    config.task_ids = Some(profile.task_ids_path());
    config.worktrees_dir = profile.worktrees_dir();
    let (agent, _events) = Agent::bind(config).await?;
    let bridge = crate::setup::resolve_bridge(
        bridge.or_else(|| crate::profile::env_setting_os("BRIDGE").map(PathBuf::from)),
    );
    let result = run_evaluation(profile, &agent, &key, planned, resumable, bridge).await;
    agent.shutdown().await;
    result
}

/// Runs `planned` (or goes on with `resumable`) with the owner's `agent` on its evaluation
/// worker, which it starts and stops; prints each run as it ends, then the report's summary.
async fn run_evaluation(
    profile: &Profile,
    agent: &Agent,
    key: &iroh::SecretKey,
    planned: Planned,
    resumable: Option<Evaluation>,
    bridge: PathBuf,
) -> Result<()> {
    let store = FactoryStore::of(profile);
    let catalog = Catalog::of(profile)?;
    let mut providers: Vec<Provider> = planned.candidates.iter().map(|c| c.provider).collect();
    providers.sort();
    providers.dedup();
    let worker = runner::eval_worker(profile, agent.addr(), &agent.trusted(), &providers)?;
    if let Some(entry) = &worker.entry {
        agent.allow(entry.clone());
        let mut trust = profile.trust()?;
        trust.allow(entry.clone());
        trust.save(&profile.trust_path())?;
    }
    let folders = profile.shares()?;
    let setup = match resumable {
        Some(e) => runner::resume(&store, e, &planned.blueprint, &folders)?,
        None => runner::prepare(
            &store,
            Request {
                blueprint: &planned.blueprint,
                build: &planned.build,
                candidates: planned.candidates.clone(),
                repetitions: planned.repetitions,
                by: profile.name.clone(),
                worker: worker.profile.name.clone(),
                worker_id: worker.id,
                folders: &folders,
            },
        )?,
    };
    runner::install(&worker.profile, &setup.share)?;
    let supervisor = Supervisor::new(
        profile.clone(),
        agent.clone(),
        Launch {
            exe: std::env::current_exe()?,
            network: Network::Local,
            ip: None,
            bridge: Some(bridge.clone()),
        },
    );
    println!("starting your evaluation worker {}", worker.profile.name);
    runner::start_worker(&supervisor, agent, &worker).await?;
    // Judges score rubrics: the profile's models of another family than the candidate's.
    let rubrics = setup
        .evaluation
        .cases
        .iter()
        .any(|c| c.expect.iter().any(|e| e.get("judge").is_some()));
    let executor = if rubrics {
        Some(
            SdkExecutor::start(ExecutorConfig {
                providers: catalog.providers.iter().copied().collect(),
                bridge_script: bridge,
                profile: profile.clone(),
            })
            .await?,
        )
    } else {
        None
    };
    let judges = match &executor {
        Some(e) => Judges::of(e.clone(), &catalog, None),
        None => Judges::default(),
    };
    let runner = Runner {
        agent: agent.clone(),
        key: key.clone(),
        signer: profile.name.clone(),
        worker: worker.id,
        store: store.clone(),
        judges,
        ready_wait: runner::READY_WAIT,
    };
    let id = setup.evaluation.id.clone();
    let name = setup.evaluation.agent.clone();
    println!(
        "evaluating {name} ({id}): {} (Ctrl+C cancels it)",
        plural(setup.evaluation.runs.len(), "run", "runs")
    );
    let printed = std::sync::Mutex::new(std::collections::HashSet::new());
    let on_change = |e: &Evaluation| {
        let mut printed = printed.lock().unwrap();
        for (i, run) in e.runs.iter().enumerate() {
            if run.state != RunState::Done || !printed.insert(i) {
                continue;
            }
            let why = run.error.clone().or_else(|| {
                run.grades
                    .iter()
                    .find(|g| g.passed == Some(false))
                    .map(|g| g.note.clone())
            });
            println!(
                "  [{}/{}] {} on {}, run {}: {} ({:.0}s){}",
                printed.len(),
                e.runs.len(),
                run.case,
                e.candidates[run.candidate],
                run.repetition,
                if run.passed { "passed" } else { "FAILED" },
                run.secs,
                why.map(|w| format!(": {w}")).unwrap_or_default()
            );
        }
    };
    let outcome = tokio::select! {
        result = runner.run(setup.evaluation, &setup.share, on_change) => result,
        _ = tokio::signal::ctrl_c() => runner::cancel(
            &store,
            &id,
            &format!("{} cancelled it (Ctrl+C)", profile.name),
            &profile.name,
            key,
        ),
    };
    supervisor.stop(&worker.profile.name).await;
    if let Some(executor) = executor {
        executor.stop().await;
    }
    let evaluation = outcome?;
    runner::clean(&store, Some(&worker.profile))?;
    println!("{name}: {} ({id})", evaluation_line(&evaluation, false));
    // An import that passed here becomes active (once you sign off what it asks for).
    if let Some(r) =
        crate::factory::catalog::after_evaluation(&store, &profile.name, key, &evaluation)?
    {
        println!(
            "{name} {}: imported, evaluated here and released here ({}){}",
            r.manifest.version,
            r.state,
            if r.state == crate::factory::release::ReleaseState::Released {
                String::new()
            } else {
                format!(
                    "; sign off what it asks for: proofkin factory approve {name} --profile {}",
                    profile.name
                )
            }
        );
    }
    for r in &evaluation.results {
        let requirements: Vec<String> = r
            .requirements
            .iter()
            .map(|(k, v)| format!("{k} {:.0}%", v * 100.0))
            .collect();
        println!(
            "  {}: {}; {:.0}% of its runs passed ({}); adversarial {}/{}; p95 {:.0}s; {:.1} attempts; cost {}; judge {}",
            r.candidate,
            if r.gate.passed { "passed" } else { "failed" },
            r.pass_rate * 100.0,
            requirements.join(", "),
            r.adversarial.passed,
            r.adversarial.total,
            r.p95_secs,
            r.attempts,
            r.multiplier.as_deref().unwrap_or("unknown"),
            r.judge_score
                .map(|s| format!("{s:.1}"))
                .unwrap_or_else(|| "none".into())
        );
        for why in &r.gate.reasons {
            println!("    {why}");
        }
    }
    println!(
        "report: proofkin factory report {id} --profile {}",
        profile.name
    );
    if !evaluation.gate.passed {
        bail!("{name} didn't pass its evaluation");
    }
    Ok(())
}

/// Prints (or writes to `out`) evaluation `id`'s report, in Markdown or as its signed JSON.
fn report(profile: &Profile, id: &str, json: bool, out: Option<&Path>) -> Result<()> {
    let store = FactoryStore::of(profile);
    let evaluation = store.evaluation(id)?.ok_or_else(|| {
        anyhow!(
            "there is no evaluation {id} (proofkin factory show NAME --profile {} lists an agent's)",
            profile.name
        )
    })?;
    let text = if json {
        serde_json::to_string_pretty(&evaluation)? + "\n"
    } else {
        evaluation.markdown()
    };
    match out {
        Some(path) => {
            std::fs::write(path, &text).with_context(|| format!("writing {}", path.display()))?;
            println!("wrote {id}'s report to {}", path.display());
        }
        None => print!("{text}"),
    }
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
