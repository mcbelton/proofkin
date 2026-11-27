use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use proofkin::audit::{AuditLog, Outcome};
use proofkin::auth::{LoginMethod, ProviderTools};
use proofkin::plan::{Plan, SquadStore, StepState, run_plan_with};
use proofkin::profile::{DEFAULT_GITHUB_HOST, WorkIqChanges};
use proofkin::trust::DEFAULT_TASKS_PER_HOUR;
use proofkin::{
    Access, Agent, AgentConfig, Event, Executor, ExecutorConfig, LlmTask, Network, Profile,
    Provider, SdkExecutor, TaskContext, Ticket, TrustedPeer,
};
use tokio::io::{AsyncBufReadExt, BufReader};

const COMMANDS: &str = "\
commands:
  <text>                                broadcast a chat message
  /ask <provider> [--model <name>] [--effort <level>] <prompt>  run a task locally (provider: copilot|claude|codex; level: minimal|low|medium|high|xhigh|max)
  /delegate <provider>[@peer[:folder]] [--model <name>] [--effort <level>] <prompt>  run a task on a peer, in a folder it shares with you (default: data), on a model it allows (default: its default); @#squad: the squad's least busy member that can run it
  /tasks                                list your running tasks
  /cancel <n>                           cancel your running task n (the peer stops it too)
  /plan <file>                          run a plan: steps on peers (@name) or squads (#squad), in dependency order
  /runs                                 list saved evidence runs (proofkin runs show <id> for a report)
  /approvals                            list approval requests waiting for you
  /approve <n> [note], /reject <n> <why>  answer approval request n (signed with your key)
  /changes <n> <what>                   ask for changes instead: the work is done again, then you are asked again
  /squads                               list your squads and which members are online
  /peers                                list connected peers and the providers they serve
  /ticket                               print this agent's ticket (give it to peers)
  /help                                 show this help
  /quit                                 exit

Each agent has its own identity (key) and Copilot/Claude/Codex accounts from its profile
(--profile, default: --name). Peers must allowlist each other's keys:  proofkin peers --help
Sign a profile in to providers:  proofkin auth --help";

/// Proofkin: peer-to-peer agents that prove their work. Runs an agent that runs and
/// delegates LLM coding tasks (with no command), or manages its profile.
#[derive(Parser)]
#[command(version, after_help = COMMANDS, args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Serves a task's checks to its model as an MCP tool, over stdio. A worker starts it
    /// for a task that asks for its folder's checks; it isn't meant to be run by hand.
    #[command(hide = true)]
    TaskChecks {
        /// The task's workspace, where the checks run.
        #[arg(long)]
        dir: PathBuf,
        /// A check, as NAME=COMMAND.
        #[arg(long = "check")]
        checks: Vec<String>,
    },
    /// Sign agent profiles in/out of Copilot, Claude and Codex (each profile has its own accounts).
    #[command(subcommand)]
    Auth(AuthCommand),
    /// Manage which peers (public keys) a profile's agents talk to and accept tasks from.
    #[command(subcommand)]
    Peers(PeersCommand),
    /// Manage the folders a profile shares with peers for their tasks.
    #[command(subcommand)]
    Share(ShareCommand),
    /// MCP servers (e.g. a browser, a docs or ticket system) a shared folder offers
    /// to peers' tasks: you define them, peers only learn their names.
    #[command(subcommand)]
    Mcp(McpCommand),
    /// Plugins: skills and MCP servers packaged together (GitHub Copilot CLI, Claude
    /// Code or Agent Plugins layouts, or one picked from a marketplace). Importing
    /// runs nothing: you offer a plugin's MCP servers in a folder when you choose.
    #[command(subcommand)]
    Plugin(PluginCommand),
    /// Create digital workers: agents with their own peer identity that run on
    /// their owner's accounts.
    #[command(subcommand)]
    Worker(WorkerCommand),
    /// Work IQ (Microsoft 365 work context): its call usage and budgets.
    #[command(subcommand)]
    Workiq(WorkiqCommand),
    /// Group peers into squads that plans can assign steps to. into squads that plans can assign steps to.
    #[command(subcommand)]
    Squad(SquadCommand),
    /// Evidence runs: saved plan runs with a verdict per claim and signed evidence.
    #[command(subcommand)]
    Runs(RunsCommand),
    /// The knowledge base: what agents learned about your systems (topics, TSGs).
    #[command(subcommand)]
    Kb(KbCommand),
    /// Ready-made plans: bootstrap, discover, sdlc, learn, spec, design, implement-spec, rehearse.
    #[command(subcommand)]
    Playbook(PlaybookCommand),
    /// Skills: instructions agents follow in plan steps (skill=name). Built-in ones
    /// ship with proofkin; a profile's own replace or add to them.
    #[command(subcommand)]
    Skill(SkillCommand),
    /// Agents: roles plan steps take (agent=name), with their instructions, the
    /// skills they follow and the MCP servers they use; a profile's own, or from plugins.
    #[command(subcommand)]
    Agent(AgentCommand),
    /// The agent factory: agents described as blueprints, checked against the profile,
    /// started from a template or drafted by its model from your words.
    #[command(subcommand)]
    Factory(proofkin::factory::cli::FactoryCommand),
    /// Spec-driven development: check a feature's spec, REST API contract and UX
    /// prototype, its design and its review pack (the checks plan steps run as
    /// verify=spec:api,design:threats,...), or import a Spec Kit or OpenSpec spec as a plan.
    #[command(subcommand)]
    Spec(SpecCommand),
    /// Plans: have the profile's model draft one from a goal or an issue, for you to
    /// review and run.
    #[command(subcommand)]
    Plan(PlanCommand),
    /// The design package (architecture, ADRs, diagrams, data dictionary, data flows,
    /// threat model, test and operations plans) and the review pack (decks per
    /// audience, customer feedback document).
    #[command(subcommand)]
    Design(DesignCommand),
    /// Sign a profile in to GitHub (an account of the GitHub CLI) for repositories.
    #[command(subcommand)]
    Github(GithubCommand),
    /// Models: each provider's default model (tasks that name none run on it) and the
    /// models tasks may ask for. Peers learn them when they connect.
    #[command(subcommand)]
    Models(ModelsCommand),
    /// Link GitHub repositories: clone, share with peers, sync, open pull requests.
    #[command(subcommand)]
    Repo(RepoCommand),
    /// Track plan runs on GitHub: an issue per run and a sub-issue per step, on a
    /// GitHub Project's board whose status follows the run.
    #[command(subcommand)]
    Tracker(TrackerCommand),
    /// Monitors: a folder's check run on a schedule; failures open incidents that
    /// TSGs or the debugging agent try to fix.
    #[command(subcommand)]
    Monitor(MonitorCommand),
    /// Scheduled plans: plans or playbooks the agent runs on its own at set times
    /// (each run is an evidence run).
    #[command(subcommand)]
    Schedule(ScheduleCommand),
    /// Operations: health report and incidents.
    #[command(subcommand)]
    Ops(OpsCommand),
    /// Let other agents (VS Code, Claude Code, any MCP or A2A client on this computer)
    /// hand work to this profile's peers: turns it on and shows the token and how to
    /// set up clients. They run tasks and plans on peers, never on this computer.
    #[command(after_help = "\
The running agent serves them with its web UI (--ui): MCP at /mcp (Streamable HTTP;
the 2026-07-28 revision and the initialize-based ones before it), A2A 1.0 at /a2a
with its agent card at /.well-known/agent-card.json. Start the agent with --ui-port
so their address stays the same. The token works only there, never for the web UI.
examples:
  proofkin connect --profile boss
  proofkin connect --profile boss --new      (a new token; the old one stops working)
  proofkin connect --profile boss --off")]
    Connect {
        #[command(flatten)]
        profile: ProfileArg,
        /// Make a new token (the old one stops working).
        #[arg(long)]
        new: bool,
        /// Turn it off: the token stops working.
        #[arg(long, conflicts_with = "new")]
        off: bool,
    },
    /// Manage profiles.
    #[command(subcommand)]
    Profile(ProfileCommand),
    /// Install the Claude/Codex bridge (Node packages) into ~/.proofkin/bridge.
    /// Not needed for Copilot-only agents.
    Setup {
        /// npm registry/mirror to install from (default: your npm config, usually registry.npmjs.org).
        #[arg(long)]
        registry: Option<String>,
    },
    /// Check this machine for everything agents need (CLIs, bridge, internet relay, profiles).
    Doctor {
        #[command(flatten)]
        bridge: BridgeArg,
    },
    /// Show the tasks peers ran on a profile's agents (who, provider, access, outcome).
    /// Prompts are not stored, only their SHA-256 and length.
    Audit {
        #[command(flatten)]
        profile: ProfileArg,
        /// How many of the most recent entries to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Print the raw JSON lines.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ProfileCommand {
    /// Delete a profile: its identity key, stored credentials, allowlist and provider sign-ins.
    Delete {
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum WorkiqCommand {
    /// Print today's Work IQ calls against the budgets (from the profile's audit file).
    Usage {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// List the declarative agents Work IQ offers this profile's account: their names and the
    /// ids `agent=` takes on a `@workiq` step in a plan.
    Agents {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Check the Work IQ connection step by step (discovery, sign-in, token, tools, one small
    /// retrieve) and stop at the first failure, saying what fixes it and who can do that.
    /// The last step is one real call, counted in the audit and against the budget.
    Doctor {
        /// Print each check's name, result and fix as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Change how many Work IQ calls this profile may make (a limit you leave out is kept).
    #[command(after_help = "\
examples:
  proofkin workiq budget --per-day 500 --profile alice
  proofkin workiq budget --per-run 40 --profile alice
Without a budget, 200 calls a day and 25 calls per run are allowed. A call over a budget
is not sent; it fails and is recorded as refused in workiq-audit.jsonl.")]
    Budget {
        /// Most calls a day (the local date).
        #[arg(long)]
        per_day: Option<u32>,
        /// Most calls in one run.
        #[arg(long)]
        per_run: Option<u32>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Allow Microsoft 365 changes, or stop them. On, a plan's `@workiq apply=STEP` step makes
    /// the changes step STEP proposed, once you signed them off (`approve @me`), and a digital
    /// worker's task for you may make changes in a folder whose `workiq` server was added with
    /// `--changes`, each one once you sign it off. Off (the default), neither makes any.
    #[command(after_help = "\
examples:
  proofkin workiq changes on --profile alice
  proofkin workiq changes on --wait 10m --profile alice
  proofkin workiq changes off --profile alice
A plan proposes changes in one step, you sign them off, and a later step makes exactly
those, once and in order:
  draft @worker:app mcp=workiq Draft the replies to this week's open escalations
  ok after draft approve @me Send these replies?
  send after ok @workiq apply=draft
In a digital worker's task for you (chat, Run task, the REPL, MCP or A2A), with the folder's
server added with `proofkin mcp add app workiq --workiq --changes`, each change its model
asks for waits for your sign-off of exactly that change, under Waiting for your approval,
for up to --wait; without your approval nothing is changed. Plan steps' tasks and peers'
tasks never get the change tools.
Your tenant's policy still applies on top: it may deny a change.")]
    Changes {
        #[arg(value_enum)]
        state: Switch,
        /// How long a change in a digital worker's task for you waits for your sign-off, in
        /// awake time: 1 to 10 minutes, as 5m or 5 (default: 5m, or the wait set before).
        #[arg(long, value_name = "MINUTES", value_parser = WorkIqChanges::parse_wait)]
        wait: Option<u32>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Also tell you in Microsoft 365 when a sign-off waits for you, or stop that: by mail to
    /// your Work IQ account's own address, or in a Teams chat you are a member of. A notice
    /// says which sign-off waits and how to answer it (in the web UI, or with /approve in the
    /// agent's terminal), never what you are asked to sign off. Off by default.
    #[command(after_help = "\
examples:
  proofkin workiq notices mail --profile alice
  proofkin workiq notices teams --list --profile alice
  proofkin workiq notices teams --chat 19:abc123@thread.v2 --profile alice
  proofkin workiq notices off --profile alice
A notice is sent for a plan's `approve @me` step (one that shows Microsoft 365 changes too)
and for each change a digital worker's task for you holds for your sign-off. It holds the
sign-off's title, the run and step (or the task's number), the agent's name and how to
answer, never the change's content, recipients or body, nor any step's result. It is sent
once, in the background, with your Work IQ account, through the audit (purpose notice,
counted against the budgets); one that fails is told in the activity and isn't sent again,
and the sign-off still waits. You still answer it in Proofkin.
Mail goes to the signed-in account's own address and isn't kept in Sent Items. Before
keeping a Teams chat, Proofkin checks you are a member of it. Teams' chat with yourself
(48:notes) isn't in the Microsoft Graph reference, so it isn't offered.
A digital worker that uses your Work IQ uses your setting: its notices come to you.")]
    Notices {
        /// Where notices go: off (the default), mail, or teams (with --chat).
        #[arg(value_enum)]
        to: NoticeTarget,
        /// The Teams chat notices go to, by its id (as --list shows it).
        #[arg(long, value_name = "ID")]
        chat: Option<String>,
        /// List your recent Teams chats to choose from, and change nothing.
        #[arg(long)]
        list: bool,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

/// On or off.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Switch {
    On,
    Off,
}

/// Where sign-off notices go.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum NoticeTarget {
    Off,
    Mail,
    Teams,
}

#[derive(Subcommand)]
enum WorkerCommand {
    /// Create a digital worker owned by --owner.
    #[command(after_help = "\
examples:
  proofkin worker create tester --owner alice --role \"test engineer for the parser\" \\
      --providers copilot --folder app --start-with-owner
  proofkin --name tester --providers copilot --serve        (or run it yourself)
The worker gets its own key, allowlist, shared folders and audit log, but no accounts:
it always uses its owner's. Its owner is allowlisted on it (may give it tasks in its data
folder and the --folder ones, which it shares as its owner does) and it is allowlisted
on its owner (chat only). With --start-with-owner, its owner's agent starts it (with
--providers) whenever it starts, and stops it when it stops; the owner's web UI starts,
stops and deletes it too.")]
    Create {
        /// Name of the new worker (also its profile name).
        name: String,
        /// Profile that owns the worker and whose accounts it uses.
        #[arg(long)]
        owner: String,
        /// What the worker is for; shown to peers and given to the model with every task.
        #[arg(long)]
        role: Option<String>,
        /// Providers it runs when its owner's agent starts it (comma-separated); the
        /// owner must be signed in to each.
        #[arg(long, value_enum, value_delimiter = ',')]
        providers: Vec<Provider>,
        /// A folder its owner shares that the worker works in too (repeatable).
        #[arg(long = "folder", value_delimiter = ',')]
        folders: Vec<String>,
        /// Its owner's agent starts it whenever it starts.
        #[arg(long)]
        start_with_owner: bool,
        /// It uses its owner's Work IQ account (Microsoft 365 work context). Without this
        /// flag it can't, even though it runs on its owner's other sign-ins.
        #[arg(long)]
        workiq: bool,
    },
    /// List digital workers (all, or those of --owner).
    List {
        #[arg(long)]
        owner: Option<String>,
    },
    /// Delete a digital worker: its key, allowlist, folder settings and audit log. It
    /// leaves its owner's allowlist and squads; its data folder stays.
    Remove {
        /// Name of the worker.
        name: String,
    },
    /// Show or set a digital worker's memory: the knowledge put in front of every task it
    /// serves, and where what its verified work teaches it goes.
    #[command(after_help = "\
examples:
  proofkin worker memory analyst                    (show it)
  proofkin worker memory analyst --context glossary,q3-results --lessons analyst-lessons
  proofkin worker memory analyst --retention-days 30
  proofkin worker memory analyst --off
The --context topics are copied from its owner's knowledge into the worker's, with their
Microsoft 365 marks. A marked topic goes only into the tasks of requesters who may have its
labels (its owner, or a peer whose grant names them), and the task's signed evidence then
carries the labels, so its result is marked with them. A task whose checks passed adds a
lesson to the --lessons topic; lessons older than --retention-days are removed. Changes
take effect when the worker next starts.")]
    Memory {
        /// Name of the worker.
        name: String,
        /// Its owner's knowledge topics put in front of every task it serves
        /// (comma-separated; replaces the list; \"\" for none).
        #[arg(long, value_delimiter = ',')]
        context: Option<Vec<String>>,
        /// The topic its lessons go to (\"\" for none).
        #[arg(long)]
        lessons: Option<String>,
        /// How many days lessons are kept (default 90).
        #[arg(long)]
        retention_days: Option<u32>,
        /// It remembers nothing: no topics in front of its tasks, no lessons (its
        /// knowledge stays).
        #[arg(long, conflicts_with_all = ["context", "lessons", "retention_days"])]
        off: bool,
    },
}

#[derive(Subcommand)]
enum SquadCommand {
    /// Create or replace a squad of allowlisted peers.
    #[command(after_help = "\
examples:
  proofkin squad create qa --member alice,tester --profile boss
In a plan, `#qa` runs a step on the least busy online member of squad qa.")]
    Create {
        name: String,
        /// Allowlist names of the members (comma-separated).
        #[arg(long, value_delimiter = ',', required = true)]
        member: Vec<String>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// List squads.
    List {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Remove a squad.
    Remove {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum KbCommand {
    /// List topics (troubleshooting guides are marked).
    List {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Print a topic.
    Show {
        topic: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Add or replace a topic from a Markdown file (`-` for stdin).
    Add {
        topic: String,
        file: PathBuf,
        #[command(flatten)]
        profile: ProfileArg,
    },
    Remove {
        topic: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Topics containing all the words.
    Search {
        #[arg(required = true)]
        words: Vec<String>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Learn from how things ran: write the ops report (health, incidents, tasks,
    /// runs) into the topic `ops-summary`, for the learn playbook and context=all.
    Learn {
        #[arg(long, default_value_t = 168)]
        hours: u64,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Keep content from elsewhere as a topic.
    #[command(subcommand)]
    Import(KbImportCommand),
}

#[derive(Subcommand)]
enum KbImportCommand {
    /// Search Microsoft 365 through Work IQ and keep what it cites as a topic, marked with
    /// the results' sensitivity labels, for a step's context=. Nothing is written when
    /// every result was withheld.
    Workiq {
        topic: String,
        /// What to look for.
        #[arg(long)]
        query: String,
        /// Where to look, as a step's workiq= takes it: all (default), or a list of email,
        /// meetings, files, teams, people, connectors.
        #[arg(long)]
        from: Option<String>,
        /// Overwrite the topic if it exists.
        #[arg(long)]
        replace: bool,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum GithubCommand {
    /// Use a GitHub CLI account (see `gh auth status`) for this profile's repositories,
    /// or sign in another one in the browser with --new.
    Login {
        #[arg(long, required_unless_present = "new", conflicts_with = "new")]
        gh_user: Option<String>,
        /// Sign in another account with the GitHub CLI's device flow and use it; gh's
        /// active account is switched back afterwards.
        #[arg(long)]
        new: bool,
        /// With --new: scopes for the new account's token besides gh's own.
        #[arg(
            long = "scope",
            value_delimiter = ',',
            default_value = "project,workflow"
        )]
        scopes: Vec<String>,
        #[arg(long, default_value = DEFAULT_GITHUB_HOST)]
        host: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Give the profile's GitHub account more scopes, e.g. `project` for the tracker's
    /// board: gh is switched to it for the refresh and back afterwards.
    Grant {
        #[arg(value_delimiter = ',', required = true)]
        scopes: Vec<String>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Show the profile's GitHub account and the GitHub CLI accounts on this machine.
    Status {
        #[command(flatten)]
        profile: ProfileArg,
    },
    Logout {
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum ModelsCommand {
    /// The models each provider the profile is signed in to offers it (Copilot lists
    /// them), with the default and the allowed ones marked.
    List {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Set the model tasks that name none run on (`none`: the provider's own default).
    #[command(after_help = "\
examples:
  proofkin models default copilot claude-opus-5.5 --effort max --profile worker
  proofkin models default claude none --profile worker")]
    Default {
        #[arg(value_enum)]
        provider: Provider,
        /// A model's name (see `models list`), or `none`.
        model: String,
        /// Also set the reasoning effort tasks work at (see `models effort`).
        #[arg(long)]
        effort: Option<String>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Set the reasoning effort tasks that ask for none work at: minimal, low, medium,
    /// high, xhigh or max (`none`: the model's own). A model that supports fewer takes
    /// the nearest it supports below it.
    #[command(after_help = "\
examples:
  proofkin models effort copilot max --profile worker
  proofkin models effort codex none --profile worker")]
    Effort {
        #[arg(value_enum)]
        provider: Provider,
        /// A reasoning effort, or `none`.
        effort: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Set the models tasks may ask for (a comma-separated list, or `any`), and with
    /// `--efforts` the reasoning efforts they may ask for. A task asking for another is
    /// refused. With a list, tasks that name none run on the default, else the first in
    /// the list; with efforts, at the default effort, else the least in the list.
    #[command(after_help = "\
examples:
  proofkin models allow copilot claude-sonnet-5.5,gpt-5.5 --profile worker
  proofkin models allow copilot any --profile worker
  proofkin models allow copilot --efforts low,medium,high --profile worker
  proofkin models allow copilot any --efforts any --profile worker")]
    Allow {
        #[arg(value_enum)]
        provider: Provider,
        /// Model names, comma-separated, or `any` (left out with --efforts: unchanged).
        #[arg(value_delimiter = ',', required_unless_present = "efforts")]
        models: Vec<String>,
        /// The reasoning efforts tasks may ask for, comma-separated (minimal, low,
        /// medium, high, xhigh, max), or `any`.
        #[arg(long, value_delimiter = ',')]
        efforts: Option<Vec<String>>,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum RepoCommand {
    /// Clone a repository with the profile's GitHub account and share it with peers
    /// (a git worktree per task).
    #[command(after_help = "\
examples:
  proofkin github login --gh-user alice --profile alice
  proofkin repo link acme/shop --access full --check test=\"npm test\" --check lint=\"npm run lint\" --profile alice
  proofkin peers allow <boss-ticket> --profile alice --delegate --access full --share data,shop
Agents' tasks commit on p2p/... branches; review them with repo branches and open a
pull request with repo pr.")]
    Link {
        /// owner/repo, host/owner/repo or an https URL.
        repo: String,
        /// Folder name peers use (default: the repository name).
        #[arg(long = "as")]
        name: Option<String>,
        #[arg(long, value_enum, default_value_t = Access::WorkspaceWrite)]
        access: Access,
        #[arg(long = "check", value_name = "NAME=COMMAND")]
        checks: Vec<String>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    List {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Fetch and fast-forward the clone.
    Sync {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Branches agents' tasks committed to, newest first.
    Branches {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Push a task branch and open a pull request into the repository's branch. Its
    /// description closes the issues the branch's commit is for (its `Refs:` lines)
    /// and ends with the proof the commit carries: the record its agent signed (who
    /// made it, the checks that ran on it) and the claims of your evidence run that
    /// has it.
    Pr {
        name: String,
        branch: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, default_value = "")]
        body: String,
        /// Leave the proof out of the description.
        #[arg(long)]
        no_proof: bool,
        /// Don't close the issues the commit is for.
        #[arg(long)]
        no_close: bool,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Stop sharing a repository and forget it (--delete also removes the clone).
    Unlink {
        name: String,
        #[arg(long)]
        delete: bool,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum TrackerCommand {
    /// Track the profile's plan runs as issues of a repository and, with --project, on
    /// a GitHub Project's board (with the profile's GitHub account).
    #[command(after_help = "\
examples:
  proofkin github login --gh-user alice --profile boss
  proofkin tracker link acme/shop --project https://github.com/users/alice/projects/4 --profile boss
Then every plan run gets an issue and each step a sub-issue; in plan text, track: #12
works on issue 12, track: off doesn't track the run and issue=#15 makes a step work
on issue 15. The board needs the account's project scope:
  gh auth refresh --hostname github.com --scopes project")]
    Link {
        /// owner/repo (or its URL) the issues go to.
        repo: String,
        /// The GitHub Project's URL (or owner/number).
        #[arg(long)]
        project: Option<String>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Show the tracker and whether its board works.
    Status {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// The board's items (without a board, the repository's open issues).
    Items {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Put the repository's issues and pull requests on the board, with a status from
    /// their state (open issues to do, open pull requests in review, closed and merged
    /// ones done); statuses already set stay.
    #[command(after_help = "\
examples:
  proofkin tracker add --open --closed --profile boss   every issue and pull request
  proofkin tracker add 12 '#15' --profile boss          these ones")]
    Add {
        /// Issue or pull request numbers (12 or #12).
        numbers: Vec<String>,
        /// Every open issue and pull request.
        #[arg(long)]
        open: bool,
        /// Every closed issue and closed or merged pull request.
        #[arg(long)]
        closed: bool,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Close the run issues whose sub-issues are all closed (their pull requests merged).
    Sync {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Stop tracking plan runs (their issues stay on GitHub).
    Unlink {
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum SkillCommand {
    /// List skills (built-in and the profile's own).
    List {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Print a skill.
    Show {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Add a skill, or replace a built-in one, from a Markdown file (`-` for stdin).
    #[command(after_help = "\
A skill file starts with front matter:
  ---
  name: security-review
  description: Our security review checklist
  ---
  # Security review
  ...
Use it in a plan step with skill=security-review.")]
    Add {
        name: String,
        file: PathBuf,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Remove a profile skill (a built-in one of the same name applies again).
    Remove {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Import a skill: a SKILL.md, a folder with one (e.g. from a skills repository)
    /// or a Markdown file. Its name comes from its front matter, else the file or folder.
    Import {
        path: PathBuf,
        /// Save it under another name.
        #[arg(long = "as", value_name = "NAME")]
        rename: Option<String>,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum AgentCommand {
    /// List agents: the profile's own and those of enabled plugins.
    List {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Print an agent: what it follows and uses, and its instructions.
    Show {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Add an agent, or replace one, from a Markdown file (`-` for stdin). Its name
    /// comes from its front matter, else the file's name.
    #[command(after_help = "\
An agent file starts with front matter, as GitHub Copilot and Claude Code agents do:
  ---
  name: security-reviewer
  description: Reviews a change for security problems
  skills: [security-review, code-review]
  mcp-servers: [semgrep]
  ---
  You review changes the way a security engineer would...
Use it in a plan step with agent=security-reviewer: its instructions come first, its
skills with them, and the step's folder must offer its MCP servers.")]
    Add {
        file: PathBuf,
        /// Save it under another name.
        #[arg(long = "as", value_name = "NAME")]
        rename: Option<String>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Remove a profile agent (a plugin's go with their plugin).
    Remove {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum SpecCommand {
    /// Run built-in checks on a folder: a check (api, threats, review:decks, ...), a
    /// group (spec, design, review, agent) or all.
    #[command(after_help = "\
checks:
  spec:spec spec:api spec:ux spec:trace              the spec package (spec/, prototype/, evidence/claims.json)
  design:arch design:data design:threats             the design package (design/)
  design:tests design:ops
  review:decks review:rehearsal                      the review pack (review/) and its rehearsal
  agent:blueprint agent:evals agent:report           an agent kept as code (agent/, proofkin factory init)
'all' (the default) runs the spec checks, and the design, review and agent checks when
design/, review/ and agent/ exist; name a check or a group (spec, design, review, agent)
to run it anyway. Short names work too: api, threats, decks.
A spec package is:
  spec/SPEC.md          user stories (US-1: ...) and acceptance criteria (AC-1 (US-1): Given/When/Then)
  spec/openapi.json     the REST API contract; operations list their criteria in x-acceptance
  prototype/index.html  a self-contained clickable prototype; screens marked data-story/data-ac
  evidence/claims.json  a claim per acceptance criterion, proven later by tests
examples:
  proofkin spec check
  proofkin spec check api --dir D:\\src\\shop
  proofkin spec check design")]
    Check {
        /// A check, a group (spec, design, review, agent) or all.
        #[arg(default_value = "all")]
        part: String,
        /// The folder holding the package.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Create a starter spec package (an example that passes every check) to edit.
    Init {
        /// The feature's title.
        title: String,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Replace an existing spec package.
        #[arg(long)]
        force: bool,
    },
    /// Turn a Spec Kit feature, an OpenSpec change or a spec document on SharePoint or
    /// OneDrive into a plan: a claim per user story, requirement and success criterion,
    /// proven by the folder's tests or a sign-off.
    #[command(after_help = "\
What it reads:
  Spec Kit   specs/NNN-name/ (spec.md, plan.md, tasks.md; .specify/memory/constitution.md)
             claims US-1.. (user stories), FR-001.. (requirements), SC-001.. (success criteria)
  OpenSpec   openspec/changes/<id>/ (proposal.md, tasks.md, specs/<capability>/spec.md)
             claims <capability>-1.. (added and modified requirements), ACCEPT
  --from     a Word (.docx), Markdown (.md) or text (.txt) document on SharePoint or OneDrive,
             read through Work IQ with the profile's sign-in, as a Spec Kit spec.md: claims
             US-1.., FR-001.., SC-001.., or DOC-1 (a sign-off) if it has none of them
Give a feature's folder, or a project folder with one feature. The spec is saved as
knowledge topic spec-<feature> (openspec-<change>, spec-<document>) in the profile; the
plan's steps are given it, so peers need no copy. A document's topic keeps its sensitivity
label: peers need a Microsoft 365 grant that includes it. Its plan says track: off, since
its claims and steps quote it and a tracked run would put them on GitHub. The plan goes to
stdout (or --out): run it with /plan <file>, or paste it in the web UI's Plan mode (or pick
the implement-speckit playbook).
examples:
  proofkin spec import D:\\src\\shop\\specs\\001-photo-albums --profile boss --set peer=alice > albums.plan
  proofkin spec import ~/src/shop/openspec/changes/add-dark-mode --profile boss --set peer=alice --set reviewers=qa --out dark.plan
  proofkin spec import --from https://contoso.sharepoint.com/sites/Shop/Shared%20Documents/Albums.docx --profile boss --set peer=alice --out albums.plan")]
    Import {
        /// The feature's folder (or a project folder with one feature).
        path: Option<PathBuf>,
        /// The https:// link of a Word, Markdown or text document on SharePoint or OneDrive
        /// to read through Work IQ, instead of a folder.
        #[arg(long, value_name = "URL")]
        from: Option<String>,
        /// Who and where: peer=NAME (required), folder=app, reviewers=qa (a squad),
        /// test=test (the folder's check), approver=me.
        #[arg(long = "set", value_name = "NAME=VALUE")]
        set: Vec<String>,
        /// Write the plan to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum DesignCommand {
    /// Create a starter design package and review pack (an example that, with the
    /// spec starter, passes every check) to edit.
    #[command(after_help = "\
It writes:
  design/architecture.md      context, components, domain model, deployment, quality attributes, decisions
  design/workflows.md         UML sequence and state diagrams, a workflow per user story
  design/adr/ADR-0001-*.md    architecture decision records
  design/data-dictionary.md   every field with type, classification, purpose and retention
  design/dataflow.md          the data flow diagram with trust boundaries, and the flows (DF-1)
  design/threat-model.md      STRIDE threats (TM-1) with mitigations, risk and status
  design/test-plan.md         a test for every acceptance criterion and high-risk threat
  design/operations.md        rollout, rollback, SLOs, metrics, dashboards, alerts, runbooks
  review/technical.md         technical review deck (Marp)
  review/product.md           product review deck (Marp)
  review/leadership.md        leadership review deck (Marp)
  review/customer-feedback.md customer feedback document
Check them with: proofkin spec check design (and review)")]
    Init {
        /// The feature's title.
        title: String,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Replace an existing design package.
        #[arg(long)]
        force: bool,
    },
    /// Render the review pack and design documents as self-contained HTML: decks as
    /// slide shows (speaker notes, print to PDF), documents as pages.
    #[command(after_help = "\
Open <out>/index.html in a browser. For PowerPoint, the decks are Marp Markdown:
  npx @marp-team/marp-cli review/leadership.md --pptx")]
    Render {
        /// The folder holding design/, review/ and prototype/.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Where to write the HTML (default: <dir>/review/html).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum PlanCommand {
    /// Have the profile's own model draft a plan: from a goal in your words, an issue of
    /// the tracker's repository, or a plan and a note that says what should change.
    #[command(after_help = "\
The model gets how plans are written and what this agent may use, as the plan check sees
it: the peers connected within --wait (their roles, providers, folders, access, checks and
MCP servers, by name), squads, skills, agents, knowledge topic names, whether Work IQ is
signed in and the tracker's repository; with --issue, the issue's text, as external
content. Never file contents, knowledge text, tokens or Microsoft 365 content: it works
read-only in an empty folder with no tools, and secrets in the goal are redacted. The plan
is checked as /plan and Run plan check it; while it has errors the model is told them and
writes it again, at most 3 tries in all. Drafting runs nothing: the plan goes to stdout
(or --out), then its problems (to stderr); it exits 1 when errors remain. Review it, then
run it with /plan <file>, or paste it in the web UI's Plan mode. Each draft is recorded in
the audit (proofkin audit), with the goal's SHA-256, not its text.
examples:
  proofkin plan draft \"Add CSV export to the reports page, with tests\" --profile boss > csv.plan
  proofkin plan draft --issue 42 --using claude --profile boss --out issue-42.plan
  proofkin plan draft --from csv.plan --note \"add a review by #qa before the sign-off\" --profile boss --out csv.plan")]
    Draft {
        /// What the plan should do, in your words (at most 4000 characters).
        goal: Option<String>,
        /// The issue of the tracker's repository the plan is for (N of #N).
        #[arg(long, value_name = "N")]
        issue: Option<u64>,
        /// The provider whose model writes it (default: the first the profile is signed
        /// in to).
        #[arg(long, value_enum)]
        using: Option<Provider>,
        /// A plan file to change, as --note says.
        #[arg(long, value_name = "FILE", requires = "note")]
        from: Option<PathBuf>,
        /// What should change in the plan --from gives.
        #[arg(long, requires = "from")]
        note: Option<String>,
        /// Write the plan to this file instead of stdout.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// Seconds to wait for the profile's peers to connect.
        #[arg(long, default_value_t = 10)]
        wait: u64,
        /// How to reach peers: over the internet or direct IPs only.
        #[arg(long, value_enum, default_value_t = Network::Internet)]
        network: Network,
        #[command(flatten)]
        profile: ProfileArg,
        #[command(flatten)]
        bridge: BridgeArg,
    },
}

#[derive(Subcommand)]
enum PlaybookCommand {
    /// List the playbooks and what they need.
    List,
    /// Explain what a playbook does, step by step, and what you need to run it.
    Explain { name: String },
    /// Print a playbook as a plan, filled in with --set values.
    #[command(after_help = "\
examples:
  proofkin playbook show discover --set peer=alice --set folder=app --set project=Shop > discover.plan
  proofkin playbook show sdlc --set peer=alice --set change=\"Add CSV export\" --set change_id=csv
Then run it in an agent with /plan <file>, or pick the playbook in the web UI's Plan mode.")]
    Show {
        name: String,
        /// A value for a placeholder: NAME=VALUE (repeatable).
        #[arg(long = "set", value_name = "NAME=VALUE")]
        set: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ScheduleCommand {
    /// Run a plan file or a playbook at set times (replaces a schedule of that name).
    #[command(after_help = "\
when (local time): 'every 30m', 'every 2h', 'daily 09:00', 'weekdays 08:30', 'mon,fri 17:00'
examples:
  proofkin schedule add nightly-learn --when \"daily 02:00\" --playbook learn --set peer=alice --set folder=app --profile boss
  proofkin schedule add smoke --when \"every 1h\" --plan smoke.plan --profile boss
Schedules run while the profile's agent runs (with or without --ui), never overlap
themselves, and each run is saved as an evidence run. A run missed while the agent
was off runs once when it starts again.")]
    Add {
        name: String,
        /// When to run, e.g. "weekdays 09:00" or "every 2h".
        #[arg(long)]
        when: String,
        /// A plan file to run.
        #[arg(long, conflicts_with = "playbook")]
        plan: Option<PathBuf>,
        /// A playbook to run, filled in with --set values.
        #[arg(long)]
        playbook: Option<String>,
        /// A value for a playbook placeholder: NAME=VALUE (repeatable).
        #[arg(long = "set", value_name = "NAME=VALUE", requires = "playbook")]
        set: Vec<String>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// List a profile's schedules, their next run and last verdict.
    List {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Stop running a schedule until resumed.
    Pause {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Run a paused schedule again (its next run counts from now).
    Resume {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Delete a schedule.
    Remove {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum MonitorCommand {
    /// Add or replace a monitor (runs in this profile's agent while it is running).
    #[command(after_help = "\
example:
  proofkin share add app D:\\src\\app --access full --check health=\"curl -fsS http://localhost:8080/health\" --check restart=\"pwsh -File restart.ps1\" --profile alice
  proofkin monitor add api --share app --check health --every 60 --slo 99.5 --profile alice")]
    Add {
        name: String,
        /// Shared folder whose check to run (fixes run there too).
        #[arg(long)]
        share: String,
        #[arg(long)]
        check: String,
        /// Seconds between runs (at least 10).
        #[arg(long, default_value_t = 60)]
        every: u64,
        /// Failures in a row that open an incident.
        #[arg(long, default_value_t = 3)]
        fail_after: u32,
        /// Availability objective, percent.
        #[arg(long, default_value_t = 99.0)]
        slo: f64,
        #[command(flatten)]
        profile: ProfileArg,
    },
    List {
        #[command(flatten)]
        profile: ProfileArg,
    },
    Remove {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum OpsCommand {
    /// Health against SLOs, incidents, peer tasks and evidence runs (Markdown).
    Report {
        #[arg(long, default_value_t = 24)]
        hours: u64,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Incident events, oldest first.
    Incidents {
        #[arg(long, default_value_t = 24)]
        hours: u64,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum RunsCommand {
    /// List saved runs, newest first.
    List {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Print a run's evidence report (Markdown).
    Show {
        id: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Re-check every signature in a run, from its run.json alone; exits 1 if any fails.
    #[command(after_help = "\
Works on a run from someone else too: a run folder they copied
  proofkin runs verify --dir <folder>
or a run downloaded from the web UI (its patches aren't checked then)
  proofkin runs verify --file <run-id>.json")]
    Verify {
        /// Run id (in the profile's runs folder).
        #[arg(required_unless_present_any = ["dir", "file"])]
        id: Option<String>,
        /// A run folder (with run.json) from anywhere.
        #[arg(long, conflicts_with_all = ["id", "file"])]
        dir: Option<PathBuf>,
        /// A run's run.json file from anywhere, under any name.
        #[arg(long, conflicts_with = "id")]
        file: Option<PathBuf>,
        /// Profile whose runs folder holds the run (with a run id).
        #[arg(long = "profile", required_unless_present_any = ["dir", "file"])]
        profile: Option<String>,
    },
}

#[derive(Subcommand)]
enum ShareCommand {
    /// Share a folder with peers (grant it per peer with `peers allow --share`).
    #[command(after_help = "\
examples:
  proofkin share add myrepo D:\\src\\myrepo --access workspace-write --profile alice
  proofkin share add app D:\\src\\app --access full --worktrees --check test=\"cargo test\" --check lint=\"cargo clippy -- -D warnings\"
  proofkin peers allow <bob-ticket> --profile alice --delegate --access workspace-write --share data,myrepo
Every profile also shares its own data folder (.proofkin-data) as 'data' (read-only); peers'
tasks run there unless they name another folder shared with them.
Checks are commands you define; a peer's task can ask your agent to run them after the
model (verify=test in a plan) as signed evidence. They run your folder's code with your
rights, so only peers with full access may request them.")]
    Add {
        /// Name peers use for the folder (e.g. `/delegate copilot@alice:myrepo ...`).
        name: String,
        /// Existing folder to share.
        path: PathBuf,
        /// Most any peer's task may do in this folder.
        #[arg(long, value_enum, default_value_t = Access::ReadOnly)]
        access: Access,
        /// A check peers' tasks may ask for: NAME=COMMAND (repeatable).
        #[arg(long = "check", value_name = "NAME=COMMAND")]
        checks: Vec<String>,
        /// Run each editing task in its own git worktree and branch, and commit its work
        /// (the folder must be a git repository).
        #[arg(long)]
        worktrees: bool,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// List the folders a profile shares.
    List {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Set (or clear) a folder's team plan: what this agent's team does with team steps
    /// peers hand the folder (`name team @you:folder ...` in their plans), or have this
    /// agent's model write the team plan for each one.
    #[command(after_help = "\
A team plan is plan text where {task} is the task handed down, e.g. team.plan:
  claim: T-1 {task} works
  dev #devs:app verify=test attempts=3 claims=T-1 {task}
  review after dev #qa:app apply=dev verify=test claims=T-1 Review this change: {task}
It runs on this agent's own peers, digital workers and squads, under its allowlist;
each of its steps is reported to whoever handed the work down, and the result carries
every step's signed evidence. At most 20 steps.
With --model, this agent's model writes the team plan for each team step instead, from
the task and the peers, squads and skills it may use, reading the folder but changing
nothing; it runs only if it checks out. With --approve too, you approve each plan before
it runs (web UI: Approvals).
examples:
  proofkin share team app team.plan --profile alice
  proofkin share team app --model --approve --profile alice")]
    Team {
        /// The shared folder.
        name: String,
        /// The team plan file (plan text with {task}).
        file: Option<PathBuf>,
        /// This agent's model writes the team plan for each team step.
        #[arg(long, conflicts_with_all = ["file", "clear"])]
        model: bool,
        /// You approve each team plan its model writes before it runs.
        #[arg(long, requires = "model")]
        approve: bool,
        /// Remove the folder's team plan, or stop its model writing them.
        #[arg(long, conflicts_with = "file")]
        clear: bool,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Let peers with edit access end their plans with a pull request step in a linked
    /// repository's folder (`ship after work pr @you:folder Title`), or stop letting them.
    #[command(after_help = "\
Your agent then pushes the branch their earlier steps committed to and opens the pull
request with your GitHub account, closing the issues the work is for and carrying its
signed proof, and follows it until it is merged or closed. It opens only work it did for
the peer that asks, as it was done. Branch protection still decides what merging needs.
With --approve, it opens each one only once you approve it (web UI: Approvals).
examples:
  proofkin share pull-requests app --profile alice
  proofkin share pull-requests app --approve --profile alice")]
    PullRequests {
        /// The shared folder (a repository linked with `repo link`).
        name: String,
        /// Stop taking pull request steps.
        #[arg(long)]
        off: bool,
        /// You approve each pull request before it is opened.
        #[arg(long, conflicts_with = "off")]
        approve: bool,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Stop sharing a folder.
    Remove {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    /// Offer an MCP server to peers' tasks in a shared folder (they ask for it with
    /// mcp=NAME in a plan step).
    #[command(after_help = "\
examples:
  proofkin mcp add app browser --access read-only --tool browser_navigate --tool browser_snapshot -- npx @playwright/mcp@latest --headless
  proofkin mcp add app docs --url https://docs.example.com/mcp --header \"Authorization=Bearer <token>\"
  proofkin mcp add app tickets --plugin team-tools --set TICKETS_TOKEN=<token> --description \"Our ticket system\"
  proofkin mcp add app workiq --workiq --profile my-worker
  proofkin mcp add app workiq --workiq --changes --profile my-worker
  plan step:  check @alice:app mcp=browser Open http://localhost:3000 and report what the home page shows
The server runs on this machine with your rights; commands, variables, headers and
URLs stay here (peers learn only the names they may use, and descriptions you give).
Tasks call only the tools you list (default: all), and tool results reach the model
marked as untrusted content. A plugin server's values not given with --set are
taken from environment variables of the same name. A `workiq` server (--workiq) is
Work IQ through this agent, for its owner's tasks and peers granted --m365-tools: each
task gets an address of its own on this machine, never the account's token, and only
tools that read. With --changes, and `proofkin workiq changes on` for the profile whose
account it uses, a digital worker's task for its owner also gets the change tools: each
change waits for the owner's sign-off of exactly that change, and is made once. Plan
steps' tasks and peers' tasks still get only tools that read.")]
    Add {
        /// The shared folder whose tasks may use the server.
        share: String,
        /// Name tasks use for the server.
        name: String,
        /// A remote server's URL (https://, or http:// on this machine).
        #[arg(long)]
        url: Option<String>,
        /// A header for the remote server: NAME=VALUE (repeatable).
        #[arg(long = "header", value_name = "NAME=VALUE", requires = "url")]
        headers: Vec<String>,
        /// The remote server uses Server-Sent Events.
        #[arg(long, requires = "url")]
        sse: bool,
        /// An environment variable for a local server: NAME=VALUE (repeatable).
        #[arg(long = "env", value_name = "NAME=VALUE")]
        env: Vec<String>,
        /// Make the server from an imported plugin (see `plugin show`).
        #[arg(long, conflicts_with_all = ["url", "command", "env"])]
        plugin: Option<String>,
        /// The plugin's server (default: the one called NAME, or its only one).
        #[arg(long, requires = "plugin")]
        server: Option<String>,
        /// A value the plugin's server needs: NAME=VALUE (repeatable).
        #[arg(long = "set", value_name = "NAME=VALUE", requires = "plugin")]
        set: Vec<String>,
        /// Work IQ through this agent's gateway (the server must be called `workiq`).
        #[arg(long, conflicts_with_all = ["url", "command", "env", "plugin"])]
        workiq: bool,
        /// With --workiq: a digital worker's tasks for its owner may also change Microsoft 365
        /// (create_entity, update_entity, delete_entity, do_action), each change waiting for
        /// the owner's sign-off. Needs `proofkin workiq changes on` too.
        #[arg(long, requires = "workiq")]
        changes: bool,
        /// A tool tasks may call (repeatable; default: all of the server's tools; with
        /// --changes it may name the change tools).
        #[arg(long = "tool", value_name = "TOOL")]
        tools: Vec<String>,
        /// The least access a peer's task needs to use the server (default: full, or
        /// read-only for --workiq, which runs nothing here).
        #[arg(long, value_enum)]
        access: Option<Access>,
        /// What the server is for, in a line (peers see it next to its name).
        #[arg(long)]
        description: Option<String>,
        #[command(flatten)]
        profile: ProfileArg,
        /// A local server: its command and arguments, after `--`.
        #[arg(last = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Offer the servers of an MCP configuration file (mcp.json, .mcp.json,
    /// .vscode/mcp.json) in a shared folder.
    #[command(after_help = "\
examples:
  proofkin mcp import .vscode/mcp.json --share app --server playwright --access read-only
  proofkin mcp import mcp.json --share app --set API_KEY=<key>
${workspaceFolder} becomes the shared folder. Values the servers need (${input:x},
${env:X}) come from --set, else environment variables of the same name. Disabled
servers and WebSocket servers are left out.")]
    Import {
        file: PathBuf,
        /// The shared folder whose tasks may use the servers.
        #[arg(long)]
        share: String,
        /// Only this server (repeatable; default: all of them).
        #[arg(long = "server", value_name = "NAME")]
        servers: Vec<String>,
        /// A value the servers need: NAME=VALUE (repeatable).
        #[arg(long = "set", value_name = "NAME=VALUE")]
        set: Vec<String>,
        /// The least access a peer's task needs to use them.
        #[arg(long, value_enum, default_value_t = Access::Full)]
        access: Access,
        /// Replace servers the folder offers under the same names.
        #[arg(long)]
        replace: bool,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// List the MCP servers a profile's shared folders offer (secrets hidden).
    List {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Stop offering an MCP server for now; it keeps its settings.
    Pause {
        share: String,
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Offer a paused MCP server again.
    Resume {
        share: String,
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Stop offering an MCP server.
    Remove {
        share: String,
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum PluginCommand {
    /// Import a plugin from a folder, a GitHub repository or a git URL. Nothing in it
    /// runs: its skills become usable in plan steps, its MCP servers become templates
    /// you can offer in a folder (mcp add --plugin).
    #[command(after_help = "\
examples:
  proofkin plugin add ./team-tools
  proofkin plugin add octo-org/agent-plugins:plugins/reviewer
  proofkin plugin add octo-org/marketplace --plugin reviewer
  proofkin plugin add https://git.example.com/tools.git#plugins/qa --as qa-tools
Private GitHub repositories use the profile's GitHub sign-in (proofkin github login).")]
    Add {
        /// A folder, owner/repo[:path], host/owner/repo[:path] or a git URL (#path for a folder in it).
        source: String,
        /// The plugin to import, when the source is a marketplace.
        #[arg(long)]
        plugin: Option<String>,
        /// Import it under another name.
        #[arg(long = "as", value_name = "NAME")]
        rename: Option<String>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// List imported plugins.
    List {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Show what a plugin contains and where its MCP servers are offered.
    Show {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Fetch a plugin again from where it came from. MCP servers that now start
    /// differently are paused until you review and resume them.
    Update {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Turn a plugin on: its skills and offered MCP servers apply again.
    Enable {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Turn a plugin off: its skills and MCP servers stop applying (settings kept).
    Disable {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Remove a plugin, its files and the MCP servers it offered.
    Remove {
        name: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum PeersCommand {
    /// Print this profile's endpoint id and a ticket to give to peers.
    Id {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Allowlist a peer by its ticket or endpoint id.
    #[command(after_help = "\
examples:
  proofkin peers allow p2pa1eyJuYW1lIjoi... --profile alice --delegate
  proofkin peers allow p2pa1eyJuYW1lIjoi... --profile alice --delegate --access workspace-write
  proofkin peers allow 5f2c...e9 --profile alice --name bob --tasks-per-hour 5
Both sides must allowlist each other. Only peers allowed with --delegate can make this
agent run LLM tasks (on this profile's accounts), with at most their --access and the
agent's own --access.")]
    Allow {
        /// Peer's ticket (`p2pa1...`, from `peers id` or `/ticket`) or endpoint id.
        target: String,
        #[command(flatten)]
        profile: ProfileArg,
        /// Local name for the peer (default: the name in the ticket).
        #[arg(long)]
        name: Option<String>,
        /// Allow this peer to delegate LLM tasks to this profile's agents.
        #[arg(long)]
        delegate: bool,
        /// Maximum tasks this peer may delegate per hour.
        #[arg(long, default_value_t = DEFAULT_TASKS_PER_HOUR)]
        tasks_per_hour: u32,
        /// What this peer's tasks may do: read-only, workspace-write (edit files) or
        /// full (also shell commands). Capped by each shared folder's maximum.
        #[arg(long, value_enum, default_value_t = Access::ReadOnly, requires = "delegate")]
        access: Access,
        /// Shared folders this peer's tasks may use (see `proofkin share`).
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "data",
            requires = "delegate"
        )]
        share: Vec<String>,
        /// Microsoft 365 content this peer may receive: sensitivity label names, e.g.
        /// General,Public,unlabeled (`unlabeled` allows content without a label), or `off`
        /// to remove the grant. Without it, a grant already set is kept.
        #[arg(long)]
        m365: Option<String>,
        /// Also let this peer's tasks use this agent's Work IQ (read only).
        #[arg(long, requires = "m365")]
        m365_tools: bool,
        /// The peer's GitHub account (its login): pull request steps ask it to review
        /// when it signed off on the work, or is named in `reviewers=`. Without it, an
        /// account already set is kept.
        #[arg(long, value_name = "LOGIN", conflicts_with = "no_github")]
        github: Option<String>,
        /// Forget the peer's GitHub account.
        #[arg(long)]
        no_github: bool,
        /// Factory agents this peer may see in its catalog and import, as you publish them
        /// (comma-separated; `proofkin factory publish NAME --to PEER` grants them too).
        /// Without it, grants already set are kept.
        #[arg(
            long,
            value_delimiter = ',',
            value_name = "AGENTS",
            conflicts_with = "no_agents"
        )]
        agents: Option<Vec<String>>,
        /// Take back every factory agent granted to the peer.
        #[arg(long)]
        no_agents: bool,
    },
    /// List allowlisted peers.
    List {
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Set (or forget, with --off) an allowlisted peer's GitHub account: pull request
    /// steps ask it to review when it signed off on the work, or is named in reviewers=.
    #[command(after_help = "\
examples:
  proofkin peers github carol octocat --profile boss
  proofkin peers github carol --off --profile boss")]
    Github {
        /// Peer name or endpoint id.
        peer: String,
        /// The peer's GitHub login.
        #[arg(required_unless_present = "off", conflicts_with = "off")]
        login: Option<String>,
        /// Forget the peer's GitHub account.
        #[arg(long)]
        off: bool,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Remove a peer from the allowlist.
    Remove {
        /// Peer name or endpoint id.
        peer: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Sign a profile in to a provider.
    #[command(after_help = "\
examples:
  proofkin auth login copilot --profile worker1 --gh-user alice
  proofkin auth login copilot --profile worker2 --gh-user bob --host mycorp.ghe.com
  proofkin auth login copilot --profile worker3 --token        (prompts for a GitHub token)
  proofkin auth login claude  --profile worker1                 (browser sign-in)
  proofkin auth login claude  --profile worker2 --api-key
  proofkin auth login codex   --profile worker1                 (browser sign-in)
  proofkin auth login codex   --profile worker2 --api-key
  proofkin auth login workiq  --profile worker1 --tenant contoso.com --client-id <app-id> [--device-code]
  proofkin auth login workiq  --profile worker1 --cli [--account <email>] [--cli-path <path>]   (Microsoft's Work IQ CLI)
Secrets are read from a hidden prompt, or from stdin when piped.")]
    Login {
        #[arg(value_enum)]
        target: AuthTarget,
        #[command(flatten)]
        profile: ProfileArg,
        /// Work IQ: the Entra tenant (id or domain).
        #[arg(long)]
        tenant: Option<String>,
        /// Work IQ: the application (client) id registered for Proofkin.
        #[arg(long)]
        client_id: Option<String>,
        /// Work IQ: sign in with a device code instead of the browser.
        #[arg(long)]
        device_code: bool,
        /// Work IQ: use Microsoft's Work IQ CLI, which signs in by itself (and picks the
        /// tenant from its account).
        #[arg(long, conflicts_with_all = ["client_id", "device_code", "tenant"])]
        cli: bool,
        /// Work IQ CLI: the account it signs in with (the email of an account the CLI has
        /// signed in; default: the CLI's own default account).
        #[arg(long, requires = "cli")]
        account: Option<String>,
        /// Work IQ CLI: where it is, when it isn't on the PATH or in npm's global folder.
        #[arg(long, requires = "cli")]
        cli_path: Option<PathBuf>,
        /// Copilot: use this GitHub CLI account's token (add accounts with `gh auth login`).
        #[arg(long, conflicts_with_all = ["token", "api_key"])]
        gh_user: Option<String>,
        /// Copilot: GitHub host of the account.
        #[arg(long, default_value = DEFAULT_GITHUB_HOST)]
        host: String,
        /// Copilot: a GitHub token. Claude: an OAuth token from `claude setup-token`.
        #[arg(long, conflicts_with = "api_key")]
        token: bool,
        /// Claude: Anthropic API key. Codex: OpenAI API key.
        #[arg(long)]
        api_key: bool,
        #[command(flatten)]
        bridge: BridgeArg,
    },
    /// Show which account each provider uses, for one profile or all profiles.
    Status {
        /// Profile to show (default: all profiles).
        #[arg(long)]
        profile: Option<String>,
        #[command(flatten)]
        bridge: BridgeArg,
    },
    /// Sign a profile out of a provider (or Work IQ) and delete its stored credentials.
    Logout {
        #[arg(value_enum)]
        target: AuthTarget,
        #[command(flatten)]
        profile: ProfileArg,
        #[command(flatten)]
        bridge: BridgeArg,
    },
}

/// What `proofkin auth logout` signs out of: a provider, or the Work IQ account (which is
/// not an LLM provider).
#[derive(Clone, Copy, ValueEnum)]
enum AuthTarget {
    Copilot,
    Claude,
    Codex,
    Workiq,
}

impl AuthTarget {
    fn provider(self) -> Option<Provider> {
        match self {
            AuthTarget::Copilot => Some(Provider::Copilot),
            AuthTarget::Claude => Some(Provider::Claude),
            AuthTarget::Codex => Some(Provider::Codex),
            AuthTarget::Workiq => None,
        }
    }
}

#[derive(ClapArgs)]
struct ProfileArg {
    /// Agent profile (identity) to sign in.
    #[arg(long = "profile", id = "profile")]
    name: String,
}

#[derive(ClapArgs)]
struct BridgeArg {
    /// Node bridge script for the Claude and Codex SDKs (default: the one `setup` installed).
    #[arg(long, env = "PROOFKIN_BRIDGE")]
    bridge: Option<PathBuf>,
}

impl BridgeArg {
    fn path(&self) -> PathBuf {
        proofkin::setup::resolve_bridge(
            self.bridge
                .clone()
                .or_else(|| proofkin::profile::env_setting_os("BRIDGE").map(PathBuf::from)),
        )
    }
}

#[derive(ClapArgs)]
struct RunArgs {
    /// Agent name shown to peers (required to run an agent).
    #[arg(long)]
    name: Option<String>,
    /// Profile whose provider instances and accounts this agent uses (default: --name).
    #[arg(long)]
    profile: Option<String>,
    /// How to reach peers: over the internet (relays, NAT traversal) or direct IPs only.
    #[arg(long, value_enum, default_value_t = Network::Internet)]
    network: Network,
    /// UDP address to bind (default: any port). Required with --network local.
    #[arg(long)]
    listen: Option<SocketAddr>,
    /// Allowlisted peer to connect to on startup, as a ticket or endpoint id (repeatable).
    /// All allowlisted peers are also dialed automatically.
    #[arg(long = "peer")]
    peers: Vec<String>,
    /// LLM providers to enable on this agent (comma-separated).
    #[arg(long, value_enum, value_delimiter = ',')]
    providers: Vec<Provider>,
    /// Run tasks delegated by peers with the enabled providers.
    #[arg(long)]
    serve: bool,
    /// Folder your own tasks (/ask, "This agent" in the web UI) work in. Peers never
    /// get it: their tasks run only in folders you share (see `proofkin share`).
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Most your own tasks may do in --workspace: read-only, workspace-write (edit
    /// files) or full (also shell commands). Peers' access is set per peer and folder.
    #[arg(long, value_enum, default_value_t = Access::ReadOnly)]
    access: Access,
    /// Open the web UI (served on 127.0.0.1 only) instead of the terminal prompt.
    #[arg(long)]
    ui: bool,
    /// Port for the web UI (default: any free port).
    #[arg(long, default_value_t = 0, requires = "ui")]
    ui_port: u16,
    /// Don't open the web UI in the browser automatically; just print its link.
    #[arg(long, requires = "ui")]
    no_open: bool,
    #[command(flatten)]
    bridge: BridgeArg,
}

/// Opens `url` in the default browser.
fn open_browser(url: &str) {
    let result = if cfg!(windows) {
        // `start` treats the first quoted argument as a window title.
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if result.is_err() {
        eprintln!("could not open a browser; open the link above manually");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Auth(command)) => run_auth(command).await,
        Some(Command::Peers(command)) => run_peers(command),
        Some(Command::Share(command)) => run_share(command),
        Some(Command::Mcp(command)) => run_mcp(command),
        Some(Command::Plugin(command)) => run_plugin(command).await,
        Some(Command::Worker(command)) => run_worker(command),
        Some(Command::Workiq(command)) => run_workiq(command).await,
        Some(Command::Squad(command)) => run_squad(command),
        Some(Command::Runs(command)) => run_runs(command),
        Some(Command::Kb(command)) => run_kb(command).await,
        Some(Command::Playbook(command)) => run_playbook(command),
        Some(Command::Skill(command)) => run_skill(command),
        Some(Command::Agent(command)) => run_agent_command(command),
        Some(Command::Factory(command)) => proofkin::factory::cli::run(command).await,
        Some(Command::Spec(command)) => run_spec(command).await,
        Some(Command::Plan(command)) => run_plan_command(command).await,
        Some(Command::Design(command)) => run_design(command),
        Some(Command::Github(command)) => run_github(command).await,
        Some(Command::Models(command)) => run_models(command).await,
        Some(Command::Repo(command)) => run_repo(command).await,
        Some(Command::Tracker(command)) => run_tracker(command).await,
        Some(Command::Monitor(command)) => run_monitor(command),
        Some(Command::Schedule(command)) => run_schedule(command),
        Some(Command::Ops(command)) => run_ops(command),
        Some(Command::Connect { profile, new, off }) => run_connect(&profile.name, new, off),
        Some(Command::Profile(ProfileCommand::Delete { profile })) => {
            let profile = Profile::open(&profile.name)?;
            let name = profile.name.clone();
            if profile.is_running() {
                bail!("profile '{name}' is running; stop its agent first");
            }
            if profile.config.owner.is_some() {
                return remove_worker(&name);
            }
            profile.delete()?;
            println!("profile '{name}' deleted");
            Ok(())
        }
        Some(Command::Setup { registry }) => {
            let script = proofkin::setup::install_bridge(registry.as_deref())?;
            println!("bridge installed: {}", script.display());
            println!(
                "claude and codex providers are ready; sign a profile in with: proofkin auth login claude --profile <name>"
            );
            Ok(())
        }
        Some(Command::Doctor { bridge }) => {
            if !proofkin::setup::doctor(&bridge.path()).await? {
                std::process::exit(1);
            }
            Ok(())
        }
        Some(Command::Audit {
            profile,
            limit,
            json,
        }) => run_audit(&profile.name, limit, json),
        Some(Command::TaskChecks { dir, checks }) => {
            proofkin::checks_mcp::serve(dir, proofkin::shares::parse_checks(&checks)?).await
        }
        None => run_agent(cli.run).await,
    }
}

fn run_audit(profile: &str, limit: usize, json: bool) -> Result<()> {
    let profile = Profile::open(profile)?;
    let entries = AuditLog::read(&profile.audit_path(), limit)?;
    if json {
        for entry in &entries {
            println!("{}", serde_json::to_string(entry)?);
        }
        return Ok(());
    }
    if entries.is_empty() {
        println!("profile '{}': no peer tasks recorded yet", profile.name);
        return Ok(());
    }
    println!(
        "{:<20} {:<14} {:>5} {:<8} {:<16} {:<8} {:>8}  prompt",
        "time (UTC)", "peer", "task", "provider", "access", "outcome", "seconds"
    );
    for e in &entries {
        let secs = e.ts_ms / 1000;
        let (days, rem) = (secs / 86_400, secs % 86_400);
        let access = e.access.map_or("-".to_owned(), |a| a.to_string());
        let outcome = serde_json::to_value(e.outcome)?;
        println!(
            "{:<20} {:<14} {:>5} {:<8} {:<16} {:<8} {:>8.1}  {} chars, sha256 {}{}{}{}",
            format!(
                "{} {:02}:{:02}:{:02}",
                civil_date(days),
                rem / 3600,
                rem % 3600 / 60,
                rem % 60
            ),
            e.peer,
            e.task_id,
            if e.team {
                "team".to_owned()
            } else {
                e.provider.to_string()
            },
            access,
            outcome.as_str().unwrap_or("?"),
            e.duration_ms as f64 / 1000.0,
            e.prompt_chars,
            &e.prompt_sha256[..12],
            e.origin
                .as_deref()
                .map(|of| format!("\n{:>22}{of}", "part of: "))
                .unwrap_or_default(),
            e.error
                .as_deref()
                .map(|err| format!("\n{:>22}{err}", "error: "))
                .unwrap_or_default(),
            e.usage
                .as_ref()
                .map(|u| format!("\n{:>22}{}", "used: ", u.describe()))
                .unwrap_or_default(),
        );
    }
    let totals = proofkin::audit::usage_totals(&entries);
    if !totals.is_empty() {
        println!("usage by peer and provider, as providers reported it:");
        for t in &totals {
            println!(
                "  {:<14} {:<8} {:>5} task{}  {}",
                t.peer,
                t.provider,
                t.tasks,
                if t.tasks == 1 { " " } else { "s" },
                t.usage.describe()
            );
        }
    }
    println!("log: {}", profile.audit_path().display());
    Ok(())
}

/// `YYYY-MM-DD` for days since 1970-01-01 (proleptic Gregorian calendar).
fn civil_date(days: u64) -> String {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

async fn run_workiq(command: WorkiqCommand) -> Result<()> {
    use proofkin::workiq::audit::{Audit, set_budget};
    match command {
        WorkiqCommand::Usage { profile } => {
            let profile = Profile::open(&profile.name)?;
            let report = Audit::for_profile(&profile).report()?;
            print!("{}", report.describe(&profile.name));
        }
        WorkiqCommand::Agents { profile } => {
            let profile = Profile::open(&profile.name)?;
            let client = proofkin::workiq::context::client_for_profile(&profile)?;
            print!(
                "{}",
                proofkin::workiq::context::agents_listing(&client).await?
            );
        }
        WorkiqCommand::Doctor { json, profile } => {
            let profile = Profile::open(&profile.name)?;
            let report = proofkin::workiq::doctor::Doctor::new(&profile).run().await;
            if json {
                println!("{}", serde_json::to_string_pretty(&report.to_json())?);
            } else {
                print!("{}", report.describe());
            }
            if !report.passed() {
                std::process::exit(1);
            }
        }
        WorkiqCommand::Budget {
            per_day,
            per_run,
            profile,
        } => {
            let mut profile = Profile::open(&profile.name)?;
            let budget = set_budget(&mut profile, per_day, per_run)?;
            println!(
                "profile '{}': Work IQ budget is {} calls a day and {} calls per run",
                profile.name,
                budget.per_day(),
                budget.per_run()
            );
        }
        WorkiqCommand::Changes {
            state,
            wait,
            profile,
        } => {
            let mut profile = Profile::open(&profile.name)?;
            let on = state == Switch::On;
            if !on && wait.is_some() {
                bail!("--wait is for `proofkin workiq changes on`");
            }
            // Turned on again without --wait, it keeps the wait set before.
            let kept = profile
                .config
                .workiq_changes
                .as_ref()
                .and_then(|c| c.wait_minutes);
            profile.config.workiq_changes = on.then(|| WorkIqChanges {
                wait_minutes: wait.or(kept),
            });
            profile.save()?;
            if let Some(changes) = &profile.config.workiq_changes {
                println!(
                    "profile '{}': Microsoft 365 changes are on: a plan's @workiq apply=STEP step makes the changes step STEP proposed, once you sign them off, and a digital worker's task for you, in a folder whose workiq server has --changes, makes each change it asks for once you sign it off, waiting up to {} minute{} for you",
                    profile.name,
                    changes.wait_minutes(),
                    if changes.wait_minutes() == 1 { "" } else { "s" }
                );
            } else {
                println!(
                    "profile '{}': Microsoft 365 changes are off: plans and digital workers' tasks make none",
                    profile.name
                );
            }
        }
        WorkiqCommand::Notices {
            to,
            chat,
            list,
            profile,
        } => {
            use proofkin::profile::WorkIqNotices;
            use proofkin::workiq::notice;
            let mut profile = Profile::open(&profile.name)?;
            let holder = profile.workiq_profile()?;
            if holder.name != profile.name {
                bail!(
                    "'{}' uses '{}''s Work IQ account, so its sign-off notices go where '{}''s go; set them with: proofkin workiq notices off|mail|teams --profile {}",
                    profile.name,
                    holder.name,
                    holder.name,
                    holder.name
                );
            }
            if to != NoticeTarget::Teams && (chat.is_some() || list) {
                bail!("--chat and --list are for `proofkin workiq notices teams`");
            }
            let setting = match (to, chat) {
                (NoticeTarget::Off, _) => None,
                (NoticeTarget::Mail, _) => {
                    let (_, account) = profile.workiq_account()?;
                    if !account.upn.contains('@') {
                        bail!(
                            "notices by mail go to the Work IQ account's own address, and Proofkin doesn't know it for this sign-in ({}); sign in with your account (for Microsoft's CLI: proofkin auth login workiq --cli --account you@example.com --profile {}), or send notices to Teams: proofkin workiq notices teams --chat <id> --profile {}",
                            account.upn,
                            profile.name,
                            profile.name
                        );
                    }
                    Some(WorkIqNotices::Mail)
                }
                (NoticeTarget::Teams, Some(_)) if list => {
                    bail!("give --chat <id> or --list, not both");
                }
                (NoticeTarget::Teams, None) if list => {
                    let client = proofkin::workiq::context::client_for_profile(&profile)?;
                    let chats = notice::list_chats(&client).await?;
                    if chats.is_empty() {
                        println!(
                            "Microsoft Graph lists no Teams chats of yours that notices can go to"
                        );
                    } else {
                        println!(
                            "your recent Teams chats (id, type, topic); choose one with: proofkin workiq notices teams --chat <id> --profile {}",
                            profile.name
                        );
                        for chat in chats {
                            println!("  {}", chat.describe());
                        }
                    }
                    return Ok(());
                }
                (NoticeTarget::Teams, None) => {
                    bail!(
                        "give the Teams chat notices go to with --chat <id>; list yours with: proofkin workiq notices teams --list --profile {}",
                        profile.name
                    );
                }
                (NoticeTarget::Teams, Some(id)) => {
                    notice::check_chat_id(&id).map_err(|e| anyhow!(e))?;
                    let client = proofkin::workiq::context::client_for_profile(&profile)?;
                    notice::check_chat(&client, &id).await?;
                    Some(WorkIqNotices::Teams { chat: id })
                }
            };
            profile.config.workiq_notices = setting;
            profile.save()?;
            match &profile.config.workiq_notices {
                Some(setting) => {
                    let (_, account) = profile.workiq_account()?;
                    println!(
                        "profile '{}': sign-off notices go {}: each sign-off that waits for you is also told there, once (which one and how to answer it, never what you sign off); you still answer it in Proofkin",
                        profile.name,
                        setting.describe(&account.upn)
                    );
                }
                None => println!(
                    "profile '{}': sign-off notices are off: a sign-off that waits for you shows only in Proofkin",
                    profile.name
                ),
            }
        }
    }
    Ok(())
}

fn run_worker(command: WorkerCommand) -> Result<()> {
    use proofkin::workers;
    match command {
        WorkerCommand::Create {
            name,
            owner,
            role,
            providers,
            folders,
            start_with_owner,
            workiq,
        } => {
            let owner = Profile::open(&owner)?;
            let addr = proofkin::EndpointAddr::new(owner.identity()?.public());
            let mut trust = owner.trust()?;
            let spec = workers::WorkerSpec {
                name,
                role,
                providers,
                folders,
                start_with_owner,
                workiq,
            };
            let created = workers::create(&owner, addr, &trust, &spec)?;
            trust.allow(created.entry);
            trust.save(&owner.trust_path())?;
            let worker = &created.worker;
            println!(
                "digital worker '{}' created for '{}' (endpoint id {}); it runs on '{}''s accounts",
                worker.name, owner.name, created.id, owner.name
            );
            let mut works_in = vec!["its data folder".to_owned()];
            works_in.extend(workers::folders(worker));
            println!("works in: {}", works_in.join(", "));
            if worker.config.workiq_from_owner {
                println!("it uses '{}''s Work IQ account", owner.name);
            }
            let providers: Vec<String> = worker
                .config
                .providers
                .iter()
                .map(Provider::to_string)
                .collect();
            if worker.config.start_with_owner {
                println!(
                    "'{}''s agent starts it (running {}) whenever it starts",
                    owner.name,
                    providers.join(", ")
                );
            } else {
                println!(
                    "start it:  proofkin --name {} --providers {} --serve",
                    worker.name,
                    if providers.is_empty() {
                        "copilot".to_owned()
                    } else {
                        providers.join(",")
                    }
                );
            }
            println!(
                "share its ticket with peers:  proofkin peers id --profile {}",
                worker.name
            );
            if owner.is_running() {
                println!(
                    "note: '{}' is running and lets the worker in once restarted; its web UI creates workers without a restart",
                    owner.name
                );
            }
        }
        WorkerCommand::List { owner } => {
            for name in Profile::list()? {
                let profile = Profile::open(&name)?;
                let Some(of) = &profile.config.owner else {
                    continue;
                };
                if owner.as_ref().is_some_and(|o| o != of) {
                    continue;
                }
                println!(
                    "  {name:<16} owner {of:<12} {}{}",
                    profile.config.role.as_deref().unwrap_or(""),
                    if profile.config.start_with_owner {
                        "  [starts with its owner]"
                    } else {
                        ""
                    }
                );
            }
        }
        WorkerCommand::Remove { name } => remove_worker(&name)?,
        WorkerCommand::Memory {
            name,
            context,
            lessons,
            retention_days,
            off,
        } => {
            use proofkin::factory::memory;
            let mut worker = Profile::open(&name)?;
            let mut settings = worker.config.memory.clone();
            let changed = off || context.is_some() || lessons.is_some() || retention_days.is_some();
            if off {
                settings = memory::Memory::default();
            }
            if let Some(context) = context {
                settings.context = context
                    .into_iter()
                    .map(|t| t.trim().to_owned())
                    .filter(|t| !t.is_empty())
                    .collect();
            }
            if let Some(lessons) = lessons {
                settings.lessons = Some(lessons.trim().to_owned()).filter(|t| !t.is_empty());
            }
            if retention_days.is_some() {
                settings.retention_days = retention_days;
            }
            if changed {
                let copied = memory::configure(&mut worker, settings)?;
                if !copied.is_empty() {
                    println!(
                        "copied from '{}''s knowledge, with their marks: {}",
                        worker.config.owner.as_deref().unwrap_or_default(),
                        copied.join(", ")
                    );
                }
            }
            let settings = &worker.config.memory;
            println!("memory of '{name}':");
            println!(
                "  in front of every task: {}",
                if settings.context.is_empty() {
                    "no knowledge".to_owned()
                } else {
                    settings.context.join(", ")
                }
            );
            match &settings.lessons {
                Some(topic) => println!(
                    "  lessons: topic '{topic}', kept {} days",
                    settings
                        .retention_days
                        .unwrap_or(memory::DEFAULT_RETENTION_DAYS)
                ),
                None => println!("  lessons: none"),
            }
            if changed {
                println!("it takes effect when '{name}' next starts");
            }
        }
    }
    Ok(())
}

/// Deletes digital worker `name` (ending a factory agent's deployment on it) and takes it
/// off its owner's allowlist and squads.
fn remove_worker(name: &str) -> Result<()> {
    use proofkin::workers;
    let worker = Profile::open(name)?;
    let Some(owner) = worker.config.owner.clone() else {
        bail!(
            "'{name}' isn't a digital worker; delete a profile with: proofkin profile delete --profile {name}"
        );
    };
    let owner = Profile::open(&owner)?;
    let ended = workers::delete(&owner, name)?;
    let squads = workers::forget(&owner, name)?;
    println!(
        "digital worker '{name}' deleted; it left '{}''s allowlist{}",
        owner.name,
        if squads.is_empty() {
            String::new()
        } else {
            format!(" and squads {}", squads.join(", "))
        }
    );
    if !ended.is_empty() {
        println!("its deployment of factory agent {} ended", ended.join(", "));
    }
    println!("its data folder stays: {}", worker.data_folder().display());
    if owner.is_running() {
        println!(
            "note: '{}' is running; its web UI deletes workers without a restart, otherwise restart it",
            owner.name
        );
    }
    Ok(())
}

/// `; checks: test, lint; worktrees` for a share listing.
fn share_extras(share: &proofkin::shares::Share) -> String {
    let mut extras = String::new();
    if !share.checks.is_empty() {
        extras += &format!(
            "; checks: {}",
            share
                .checks
                .iter()
                .map(|(name, cmd)| format!("{name} ({cmd})"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if share.worktrees {
        extras += "; a git worktree per editing task";
    }
    if share.pull_requests {
        extras += if share.pull_request_approval {
            "; takes pull request steps, each once you approve it"
        } else {
            "; takes pull request steps"
        };
    }
    if !share.mcp.is_empty() {
        extras += &format!(
            "; MCP servers: {}",
            share.mcp.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    if let Some(plan) = &share.team {
        let steps = proofkin::plan::Plan::parse(plan).map_or(0, |p| p.steps.len());
        extras += &format!(
            "; team plan ({steps} step{}) for team steps",
            if steps == 1 { "" } else { "s" }
        );
    } else if share.team_model {
        extras += if share.team_approval {
            "; team plans written by its model for team steps (you approve each)"
        } else {
            "; team plans written by its model for team steps"
        };
    }
    extras
}

/// Runs the profile's scheduled plans in the background while the agent runs (the
/// web UI runs its own, so their steps show up there).
fn start_schedules(profile: &Profile, agent: &Agent) -> Result<()> {
    use proofkin::schedule::{Launch, Outcome, Schedule, ScheduleStore, Scheduler};
    let store = ScheduleStore::load(&profile.schedules_path())?;
    if !store.schedules.is_empty() {
        println!(
            "schedules: {}",
            store
                .schedules
                .iter()
                .map(|s| format!(
                    "{} ({}{})",
                    s.name,
                    s.when,
                    if s.enabled { "" } else { ", paused" }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let (agent, owner) = (agent.clone(), profile.clone());
    let launch: Launch = Arc::new(move |s: Schedule| {
        let ready = proofkin::plan::start(&agent, &owner, &s.plan)?;
        let agent = agent.clone();
        Ok(Box::pin(async move {
            println!("=== scheduled plan '{}' started ({}) ===", s.name, s.when);
            let run = ready.run(&agent, print_step).await;
            println!(
                "=== scheduled plan '{}' finished: {} (runs show {}) ===",
                s.name,
                run.verdict_line(),
                run.id
            );
            Outcome {
                verdict: run.verdict_line(),
                run_id: Some(run.id),
            }
        }))
    });
    let tick = proofkin::profile::env_setting("SCHEDULE_TICK_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    Scheduler::new(profile.schedules_path(), launch).spawn(Duration::from_secs(tick));
    Ok(())
}

/// Runs the profile's monitors in the background. Incidents are printed and sent to
/// connected peers as chat, so the team sees alerts. Monitors added, removed or
/// changed while the agent runs (in the web UI or with `monitor add`) are followed.
fn start_monitors(
    profile: &Profile,
    agent: &Agent,
    executor: Option<Arc<dyn Executor>>,
) -> Result<()> {
    use proofkin::ops::{IncidentEvent, IncidentKind, MonitorList, MonitorStore, Ops};
    let monitors = MonitorStore::load(&profile.monitors_path())?.monitors;
    if !monitors.is_empty() {
        println!(
            "monitors: {} (incidents try troubleshooting guides, then {})",
            monitors
                .iter()
                .map(|m| format!("{} every {}s", m.name, m.every_secs))
                .collect::<Vec<_>>()
                .join(", "),
            if executor.is_some() {
                "the debugging agent"
            } else {
                "a person (no --providers for a debugging agent)"
            }
        );
    }
    let alerts = agent.clone();
    let name = agent.name().to_owned();
    let notify = move |e: &IncidentEvent| {
        let line = e.text.lines().next().unwrap_or_default();
        println!("! [{}] {:?}: {line}", e.monitor, e.kind);
        if matches!(
            e.kind,
            IncidentKind::Opened
                | IncidentKind::Escalated
                | IncidentKind::Resolved
                | IncidentKind::Learned
        ) {
            // Peers on the allowlist that aren't connected get it when they connect.
            alerts.alert(format!(
                "[{name} alert] {} {}: {line}",
                e.monitor,
                format!("{:?}", e.kind).to_lowercase()
            ));
        }
    };
    let current = agent.clone();
    // The file as it is now; while it can't be read (being written), the last list.
    let path = profile.monitors_path();
    let last = std::sync::Mutex::new(monitors);
    let monitors: MonitorList = Arc::new(move || match MonitorStore::load(&path) {
        Ok(store) => {
            *last.lock().unwrap() = store.monitors.clone();
            store.monitors
        }
        Err(_) => last.lock().unwrap().clone(),
    });
    let ops = Arc::new(Ops {
        agent_name: agent.name().to_owned(),
        monitors,
        reload: Duration::from_secs(5),
        // As they are now: shares changed while the agent runs apply to its monitors.
        shares: Arc::new(move || current.shares()),
        store: profile.ops_store(),
        kb: profile.knowledge(),
        skills: profile.skills(),
        executor,
        notify: Arc::new(notify),
    });
    tokio::spawn(ops.run());
    // Keep learning: refresh the ops summary in the knowledge base every hour (while
    // there are monitors).
    let profile = profile.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            let watched = MonitorStore::load(&profile.monitors_path())
                .is_ok_and(|store| !store.monitors.is_empty());
            if watched && let Ok(report) = ops_report(&profile, 168) {
                let _ = profile.knowledge().write(
                    "ops-summary",
                    &report,
                    "ops report of the last 168 hours (refreshed hourly)",
                );
            }
        }
    });
    Ok(())
}

/// The profile's ops report for the last `hours`.
fn ops_report(profile: &Profile, hours: u64) -> Result<String> {
    use proofkin::ops::{MonitorStore, report};
    let monitors = MonitorStore::load(&profile.monitors_path())?.monitors;
    let since_ms = proofkin::runs::unix_now().saturating_sub(hours * 3600) * 1000;
    let audit: Vec<_> = AuditLog::read(&profile.audit_path(), usize::MAX)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.ts_ms >= since_ms)
        .collect();
    let runs = proofkin::runs::RunRecord::list(&profile.runs_dir()).unwrap_or_default();
    let mut md = report(
        &profile.name,
        hours,
        &monitors,
        &profile.shares()?,
        &profile.ops_store(),
        &audit,
        &runs,
    );
    let since = chrono::Utc::now() - chrono::Duration::hours(hours as i64);
    if let Some(agents) = proofkin::factory::metrics::ops_section(profile, since)? {
        md += &agents;
    }
    Ok(md)
}

async fn run_kb(command: KbCommand) -> Result<()> {
    match command {
        KbCommand::Import(KbImportCommand::Workiq {
            topic,
            query,
            from,
            replace,
            profile,
        }) => {
            let profile = Profile::open(&profile.name)?;
            let work = proofkin::workiq::context::ClientWork::for_profile(&profile)?;
            let done = proofkin::workiq::import::import_knowledge(
                &work,
                &profile.knowledge(),
                &topic,
                &query,
                from.as_deref(),
                replace,
            )
            .await?;
            println!(
                "profile '{}': saved topic {} from Work IQ: {} result(s) kept ({}), {} withheld",
                profile.name,
                done.topic,
                done.kept,
                done.labels.join(", "),
                done.withheld
            );
        }
        KbCommand::List { profile } => {
            let profile = Profile::open(&profile.name)?;
            let topics = profile.knowledge().list();
            if topics.is_empty() {
                println!(
                    "profile '{}' has no knowledge yet: run the discover playbook, or kb add",
                    profile.name
                );
            }
            for t in topics {
                let kind = match &t.tsg {
                    Some(g) => format!(
                        "TSG ({}, fix: {}{})",
                        g.status,
                        g.fix,
                        g.monitor
                            .as_ref()
                            .map(|m| format!(", monitor {m}"))
                            .unwrap_or_default()
                    ),
                    None => format!("{} chars", t.chars),
                };
                println!("  {:<28} {kind}", t.name);
            }
        }
        KbCommand::Show { topic, profile } => {
            print!(
                "{}",
                Profile::open(&profile.name)?.knowledge().read(&topic)?
            )
        }
        KbCommand::Add {
            topic,
            file,
            profile,
        } => {
            let text = if file.as_os_str() == "-" {
                let mut text = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
                text
            } else {
                std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?
            };
            let profile = Profile::open(&profile.name)?;
            profile.knowledge().write(&topic, &text, "added by hand")?;
            println!("profile '{}': saved topic {topic}", profile.name);
        }
        KbCommand::Remove { topic, profile } => {
            if !Profile::open(&profile.name)?.knowledge().remove(&topic)? {
                bail!("there is no topic '{topic}'");
            }
        }
        KbCommand::Search { words, profile } => {
            for topic in Profile::open(&profile.name)?
                .knowledge()
                .search(&words.join(" "))
            {
                println!("  {topic}");
            }
        }
        KbCommand::Learn { hours, profile } => {
            let profile = Profile::open(&profile.name)?;
            let report = ops_report(&profile, hours)?;
            profile.knowledge().write(
                "ops-summary",
                &report,
                &format!("ops report of the last {hours} hours"),
            )?;
            println!(
                "profile '{}': wrote topic ops-summary ({} chars); analyze it with the learn playbook",
                profile.name,
                report.chars().count()
            );
        }
    }
    Ok(())
}

/// Signs `profile` in to GitHub with GitHub CLI account `user@host`; returns the
/// verified login.
fn github_login(profile: &mut Profile, user: &str, host: &str) -> Result<String> {
    profile.github_login(user, host)
}

/// Runs a GitHub CLI device flow in the terminal: shows the code, waits for the user to
/// enter it on GitHub (Ctrl-C cancels), and returns how it ended.
async fn device_flow(
    kind: proofkin::gh_auth::Kind,
    host: &str,
    what: &str,
) -> Result<proofkin::gh_auth::Flow> {
    use proofkin::gh_auth::{Flows, GhCli};
    let flows = Flows::new(std::sync::Arc::new(GhCli::new()));
    let flow = flows.start(kind, host, |_| {}).await?;
    println!("Enter the code {} at {} to {what}.", flow.code, flow.url);
    if let (Some(before), Some(login)) = (&flow.active_before, &flow.login)
        && !before.eq_ignore_ascii_case(login)
    {
        println!(
            "The GitHub CLI's active account is {login} until you finish, then {before} again."
        );
    }
    println!("Waiting (Ctrl-C cancels)...");
    let ended = tokio::select! {
        ended = flows.ended() => ended,
        _ = tokio::signal::ctrl_c() => {
            flows.cancel();
            flows.ended().await
        }
    };
    let ended = ended.context("the sign-in ended without a result")?;
    if let Some(note) = &ended.note {
        eprintln!("{note}");
    }
    Ok(ended)
}

/// `none` (any case) as no value.
fn unless_none(value: &str) -> Option<&str> {
    Some(value.trim()).filter(|v| !v.eq_ignore_ascii_case("none"))
}

async fn run_models(command: ModelsCommand) -> Result<()> {
    use proofkin::models::{
        choice, describe as describe_models, set_allowed, set_default, set_effort, set_efforts,
    };
    let apply_note = |profile: &Profile| {
        if profile.is_running() {
            println!(
                "(its agent is running: it applies them within seconds, and tells connected peers)"
            );
        }
    };
    match command {
        ModelsCommand::List { profile } => {
            let profile = Profile::open(&profile.name)?;
            let accounts = profile.account_profile()?;
            let models = profile.models();
            let inherited = profile.inherited_models();
            let mut any = false;
            for provider in [Provider::Copilot, Provider::Claude, Provider::Codex] {
                if !accounts.is_configured(provider) {
                    continue;
                }
                any = true;
                let set = choice(&models, provider);
                println!(
                    "{provider}: {}{}",
                    describe_models(&set),
                    if inherited.contains(&provider) {
                        format!(" (its owner {}'s settings)", accounts.name)
                    } else {
                        String::new()
                    }
                );
                if provider != Provider::Copilot {
                    println!(
                        "  (its SDK takes any model name it knows, and the efforts {}; there is no list to show)",
                        if provider == Provider::Claude {
                            "low to max (minimal is low)"
                        } else {
                            "minimal to max"
                        }
                    );
                    continue;
                }
                let offered =
                    proofkin::llm::CopilotBackend::list_models_with(profile.copilot_credentials()?)
                        .await?;
                let width = offered.iter().map(|m| m.id.len()).max().unwrap_or(0);
                for m in &offered {
                    let mut marks = Vec::new();
                    if set
                        .default
                        .as_deref()
                        .is_some_and(|d| d.eq_ignore_ascii_case(&m.id))
                    {
                        marks.push("default");
                    }
                    if !set.allowed.is_empty() {
                        marks.push(if set.allows(&m.id) {
                            "allowed"
                        } else {
                            "not allowed"
                        });
                    }
                    let mut about = vec![m.name.clone()];
                    about.extend(m.vendor.clone());
                    about.extend(m.multiplier.as_ref().map(|x| format!("{x}x")));
                    about.extend(m.policy.as_ref().map(|p| format!("policy: {p}")));
                    if !m.efforts.is_empty() {
                        about.push(format!(
                            "reasoning {}{}",
                            m.efforts.join("/"),
                            m.default_effort
                                .as_ref()
                                .map(|e| format!(" (its own: {e})"))
                                .unwrap_or_default()
                        ));
                    }
                    println!(
                        "  {:width$}  {}{}",
                        m.id,
                        about.join(" · "),
                        if marks.is_empty() {
                            String::new()
                        } else {
                            format!("  [{}]", marks.join(", "))
                        }
                    );
                }
                for missing in set
                    .allowed
                    .iter()
                    .chain(&set.default)
                    .filter(|a| !offered.iter().any(|m| m.id.eq_ignore_ascii_case(a)))
                {
                    println!("  {missing}: set here, but not offered to this Copilot account");
                }
            }
            if !any {
                println!(
                    "profile '{}' isn't signed in to a provider: proofkin auth login <provider> --profile {}",
                    profile.name, profile.name
                );
            }
        }
        ModelsCommand::Default {
            provider,
            model,
            effort,
            profile,
        } => {
            let mut profile = Profile::open(&profile.name)?;
            set_default(&mut profile.config.models, provider, unless_none(&model))?;
            if let Some(effort) = &effort {
                set_effort(&mut profile.config.models, provider, unless_none(effort))?;
            }
            profile.save()?;
            println!(
                "profile '{}': {provider}: {}",
                profile.name,
                describe_models(&choice(&profile.config.models, provider))
            );
            apply_note(&profile);
        }
        ModelsCommand::Effort {
            provider,
            effort,
            profile,
        } => {
            let mut profile = Profile::open(&profile.name)?;
            set_effort(&mut profile.config.models, provider, unless_none(&effort))?;
            profile.save()?;
            println!(
                "profile '{}': {provider}: {}",
                profile.name,
                describe_models(&choice(&profile.config.models, provider))
            );
            apply_note(&profile);
        }
        ModelsCommand::Allow {
            provider,
            models,
            efforts,
            profile,
        } => {
            let mut profile = Profile::open(&profile.name)?;
            let any =
                |list: &[String]| list.len() == 1 && list[0].trim().eq_ignore_ascii_case("any");
            if !models.is_empty() {
                let list = if any(&models) { Vec::new() } else { models };
                set_allowed(&mut profile.config.models, provider, &list)?;
            }
            if let Some(efforts) = efforts {
                let list = if any(&efforts) { Vec::new() } else { efforts };
                set_efforts(&mut profile.config.models, provider, &list)?;
            }
            profile.save()?;
            println!(
                "profile '{}': {provider}: {}",
                profile.name,
                describe_models(&choice(&profile.config.models, provider))
            );
            apply_note(&profile);
        }
    }
    Ok(())
}

async fn run_github(command: GithubCommand) -> Result<()> {
    use proofkin::gh_auth::{Kind, State, check_scopes};
    match command {
        GithubCommand::Login {
            gh_user: Some(gh_user),
            host,
            profile,
            ..
        } => {
            let mut profile = Profile::open(&profile.name)?;
            let login = github_login(&mut profile, &gh_user, &host)?;
            println!(
                "profile '{}': GitHub repositories as {login}@{host}",
                profile.name
            );
        }
        GithubCommand::Login {
            scopes,
            host,
            profile,
            ..
        } => {
            let mut profile = Profile::open(&profile.name)?;
            if profile.config.owner.is_some() {
                bail!("digital workers use their owner's GitHub account");
            }
            // `--scope ""`: only the GitHub CLI's own scopes.
            let scopes: Vec<String> = scopes.into_iter().filter(|s| !s.is_empty()).collect();
            check_scopes(&scopes)?;
            let ended = device_flow(Kind::SignIn { scopes }, &host, "sign in to GitHub").await?;
            match ended.state {
                State::Done { login } => {
                    let login = github_login(&mut profile, &login, &host)?;
                    println!(
                        "profile '{}': GitHub repositories as {login}@{host}",
                        profile.name
                    );
                }
                State::Failed { reason } => bail!("the sign-in failed: {reason}"),
                State::Cancelled | State::Waiting => bail!("the sign-in was cancelled"),
            }
        }
        GithubCommand::Grant { scopes, profile } => {
            let profile = Profile::open(&profile.name)?;
            if profile.config.owner.is_some() {
                bail!(
                    "a digital worker uses its owner's GitHub account: grant the scope to the owner's profile"
                );
            }
            let Some(proofkin::profile::GithubAuth::Gh { user, host }) =
                profile.config.github.clone()
            else {
                bail!(
                    "profile '{}' isn't signed in to GitHub; run: proofkin github login --gh-user <login> --profile {}",
                    profile.name,
                    profile.name
                );
            };
            check_scopes(&scopes)?;
            let what = format!("approve the {} scope for {user}", scopes.join(", "));
            let ended = device_flow(
                Kind::Grant {
                    login: user,
                    scopes: scopes.clone(),
                },
                &host,
                &what,
            )
            .await?;
            match ended.state {
                State::Done { login } => {
                    println!("{login}'s token now has the {} scope", scopes.join(", "))
                }
                State::Failed { reason } => bail!("the grant failed: {reason}"),
                State::Cancelled | State::Waiting => bail!("the grant was cancelled"),
            }
        }
        GithubCommand::Status { profile } => {
            let profile = Profile::open(&profile.name)?;
            match profile.github() {
                Ok(c) => println!(
                    "profile '{}': GitHub as {}@{}",
                    profile.name, c.user, c.host
                ),
                Err(e) => println!("{e:#}"),
            }
            println!("GitHub CLI accounts on this machine:");
            for a in proofkin::github::gh_accounts() {
                println!(
                    "  {}@{}{} (scopes: {})",
                    a.login,
                    a.host,
                    if a.active { ", active" } else { "" },
                    a.scopes.join(", ")
                );
            }
        }
        GithubCommand::Logout { profile } => {
            let mut profile = Profile::open(&profile.name)?;
            profile.config.github = None;
            profile.save()?;
        }
    }
    Ok(())
}

async fn run_repo(command: RepoCommand) -> Result<()> {
    use proofkin::github::{RepoStore, link, task_branches, unlink};
    match command {
        RepoCommand::Link {
            repo,
            name,
            access,
            checks,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            let checks = proofkin::shares::parse_checks(&checks)?;
            let (repo, _) = link(&profile, &repo, name.as_deref(), access, checks).await?;
            println!(
                "profile '{}': linked {} ({}) as folder '{}' at {} (at most {access}, a worktree per task)",
                profile.name,
                repo.slug,
                repo.branch,
                repo.name,
                repo.path.display()
            );
            println!(
                "grant it to a peer: proofkin peers allow <peer> --profile {} --delegate --access {access} --share data,{}",
                profile.name, repo.name
            );
        }
        RepoCommand::List { profile } => {
            for r in RepoStore::load(&Profile::open(&profile.name)?.repos_path())?.repos {
                println!(
                    "  {:<14} {}/{} ({})  {}",
                    r.name,
                    r.host,
                    r.slug,
                    r.branch,
                    r.path.display()
                );
            }
        }
        RepoCommand::Sync { name, profile } => {
            let profile = Profile::open(&profile.name)?;
            let repo = RepoStore::load(&profile.repos_path())?.get(&name)?.clone();
            let head = profile.github()?.sync(&repo).await?;
            println!("{name}: {} at {head}", repo.branch);
        }
        RepoCommand::Branches { name, profile } => {
            let profile = Profile::open(&profile.name)?;
            let repo = RepoStore::load(&profile.repos_path())?.get(&name)?.clone();
            for b in task_branches(&repo).await? {
                println!(
                    "  {:<36} {}  {}",
                    b.name,
                    proofkin::runs::utc(b.unix),
                    b.subject
                );
            }
        }
        RepoCommand::Pr {
            name,
            branch,
            title,
            mut body,
            no_proof,
            no_close,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            let repo = RepoStore::load(&profile.repos_path())?.get(&name)?.clone();
            let creds = profile.github()?;
            let found = task_branches(&repo)
                .await?
                .into_iter()
                .find(|b| b.name == branch);
            let title = match title {
                Some(t) => t,
                None => found.as_ref().map(|b| b.subject.clone()).with_context(|| {
                    format!("{name} has no branch {branch} (see repo branches)")
                })?,
            };
            if !no_close && let Some(b) = &found {
                let closes = proofkin::tracker::closing_lines(&b.issues, &repo.slug);
                if !closes.is_empty() {
                    println!("closes: {}", b.issues.join(", "));
                    body = format!("{closes}\n{body}");
                }
            }
            creds.push(&repo, &branch).await?;
            if !no_proof {
                match creds.pr_proof(&repo, &branch, &profile).await {
                    Ok(Some(proof)) => {
                        println!("with its proof: {}", proof.summary);
                        body += &proof.markdown;
                    }
                    Ok(None) => println!("without proof: its commit has no signed record"),
                    Err(e) => println!("without proof: {e:#}"),
                }
            }
            println!("{}", creds.create_pr(&repo, &branch, &title, &body).await?);
        }
        RepoCommand::Unlink {
            name,
            delete,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            unlink(&profile, &name, delete)?;
            println!("profile '{}': unlinked {name}", profile.name);
        }
    }
    Ok(())
}

fn print_tracker(info: &proofkin::tracker::TrackerInfo, waiting: &[proofkin::tracker::WaitingRun]) {
    println!(
        "issues: {} ({}{}), as {}",
        info.repo,
        info.repo_url,
        match info.visibility.as_deref() {
            Some("public") => ", public: anyone can read them",
            Some("internal") => ", internal: everyone in its enterprise can read them",
            _ => "",
        },
        info.login
    );
    match (&info.board, &info.board_problem) {
        (Some(b), _) => println!(
            "board: {} ({}), status {}{}",
            b.title,
            b.url,
            if b.statuses.is_empty() {
                "field missing".to_owned()
            } else {
                b.statuses.join(" / ")
            },
            if b.agent_field {
                ", Agent field"
            } else {
                ", no Agent field"
            }
        ),
        (None, Some(problem)) => println!("board: not updated: {problem}"),
        (None, None) => println!("board: none (add one with --project)"),
    }
    print_waiting(waiting);
}

/// The run issues left open and what each waits for.
fn print_waiting(waiting: &[proofkin::tracker::WaitingRun]) {
    if waiting.is_empty() {
        return;
    }
    println!("run issues left open until their sub-issues close:");
    for run in waiting {
        let closed = run.sub_issues.iter().filter(|s| !s.open).count();
        println!(
            "  #{} {}: {closed} of {} sub-issues closed",
            run.number,
            run.title,
            run.sub_issues.len()
        );
        for sub in run.sub_issues.iter().filter(|s| s.open) {
            let prs: Vec<String> = sub.pull_requests.iter().map(|n| format!("#{n}")).collect();
            println!(
                "    #{} {}: {}",
                sub.number,
                sub.title,
                if prs.is_empty() {
                    "no pull request yet".to_owned()
                } else {
                    format!("pull request {}", prs.join(", "))
                }
            );
        }
    }
}

async fn run_tracker(command: TrackerCommand) -> Result<()> {
    use proofkin::tracker::TrackerConfig;
    match command {
        TrackerCommand::Link {
            repo,
            project,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            let tracker = proofkin::tracker::link(&profile, &repo, project.as_deref()).await?;
            println!("profile '{}' tracks its plan runs on GitHub", profile.name);
            let waiting = tracker.waiting().await?;
            print_tracker(&tracker.info(), &waiting);
        }
        TrackerCommand::Status { profile } => {
            let profile = Profile::open(&profile.name)?;
            match proofkin::tracker::open(&profile).await? {
                Some(tracker) => {
                    let waiting = tracker.waiting().await?;
                    print_tracker(&tracker.info(), &waiting);
                }
                None => println!(
                    "profile '{}' has no tracker; link one with: proofkin tracker link <owner/repo> --project <url> --profile {}",
                    profile.name, profile.name
                ),
            }
        }
        TrackerCommand::Items { profile } => {
            let profile = Profile::open(&profile.name)?;
            let tracker = proofkin::tracker::open(&profile)
                .await?
                .with_context(|| format!("profile '{}' has no tracker", profile.name))?;
            if let Some(problem) = &tracker.board_problem {
                println!("(the repository's open issues: the board isn't used: {problem})");
            }
            for item in tracker.items().await? {
                let number = item
                    .number
                    .map(|n| format!("{}#{n}", item.repo.as_deref().unwrap_or_default()))
                    .unwrap_or_else(|| "draft".into());
                let mut extra = Vec::new();
                if let Some((done, all)) = item.sub_issues {
                    extra.push(format!("{done}/{all} sub-issues"));
                }
                if let Some(parent) = item.parent {
                    extra.push(format!("part of #{parent}"));
                }
                if let Some(agent) = &item.agent {
                    extra.push(format!("agent {agent}"));
                }
                if !item.assignees.is_empty() {
                    extra.push(format!("assigned to {}", item.assignees.join(", ")));
                }
                if !item.pull_requests.is_empty() {
                    let prs: Vec<String> =
                        item.pull_requests.iter().map(|n| format!("#{n}")).collect();
                    extra.push(format!("closed by {}", prs.join(", ")));
                }
                if !item.open {
                    extra.push("closed".into());
                }
                println!(
                    "  {:<8} {:<14} {}{}",
                    number,
                    item.status.as_deref().unwrap_or("-"),
                    item.title,
                    if extra.is_empty() {
                        String::new()
                    } else {
                        format!("  ({})", extra.join("; "))
                    }
                );
            }
        }
        TrackerCommand::Add {
            numbers,
            open,
            closed,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            let numbers = numbers
                .iter()
                .map(|n| {
                    proofkin::plan::issue_number(n)
                        .with_context(|| format!("{n:?} is not an issue or pull request number"))
                })
                .collect::<Result<Vec<u64>>>()?;
            if numbers.is_empty() && !open && !closed {
                bail!("say which to put on the board: numbers, --open or --closed");
            }
            let tracker = proofkin::tracker::open(&profile)
                .await?
                .with_context(|| format!("profile '{}' has no tracker", profile.name))?;
            let result = tracker
                .add_to_board(&proofkin::tracker::AddItems {
                    numbers,
                    open,
                    closed,
                })
                .await?;
            for a in &result.added {
                println!(
                    "  #{:<6} {:<13} {:<14} {}{}",
                    a.number,
                    a.kind.replace('_', " "),
                    a.status.as_deref().unwrap_or("-"),
                    a.title,
                    if a.already {
                        "  (on the board already)"
                    } else {
                        ""
                    }
                );
            }
            let new = result.added.iter().filter(|a| !a.already).count();
            println!(
                "{new} put on the board, {} on it already",
                result.added.len() - new
            );
            for problem in &result.problems {
                println!("problem: {problem}");
            }
        }
        TrackerCommand::Sync { profile } => {
            let profile = Profile::open(&profile.name)?;
            let synced = proofkin::tracker::sync(&profile).await?;
            if synced.closed.is_empty() {
                println!("no run issue to close");
            }
            for n in synced.closed {
                println!("closed run issue #{n}: all its sub-issues are closed");
            }
            print_waiting(&synced.waiting);
        }
        TrackerCommand::Unlink { profile } => {
            let profile = Profile::open(&profile.name)?;
            let path = profile.tracker_path();
            if TrackerConfig::load(&path)?.is_none() {
                println!("profile '{}' has no tracker", profile.name);
            } else {
                std::fs::remove_file(&path)?;
                println!(
                    "profile '{}' no longer tracks its plan runs (their issues stay on GitHub)",
                    profile.name
                );
            }
        }
    }
    Ok(())
}

fn read_input(file: &std::path::Path) -> Result<String> {
    if file.as_os_str() == "-" {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        Ok(text)
    } else {
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))
    }
}

/// The built-in checks `part` names: a check (full or short name), a group or all.
fn spec_checks(part: &str, dir: &std::path::Path) -> Result<Vec<&'static str>> {
    use proofkin::spec::BUILT_IN_CHECKS;
    let group = |prefix: &str| -> Vec<&'static str> {
        BUILT_IN_CHECKS
            .iter()
            .copied()
            .filter(|n| n.starts_with(prefix))
            .collect()
    };
    Ok(match part {
        "all" => {
            let mut names = group("spec:");
            for (folder, prefix) in [("design", "design:"), ("review", "review:")] {
                if dir.join(folder).is_dir() {
                    names.extend(group(prefix));
                } else {
                    println!("{folder}/: not started (proofkin design init writes a starter)");
                }
            }
            // A rehearsal is optional: the rehearse playbook writes it.
            if !dir.join("review").join("rehearsal.md").exists() {
                names.retain(|n| *n != "review:rehearsal");
            }
            // An agent kept as code, with its report once it was evaluated.
            if dir.join("agent").is_dir() {
                names.extend(group("agent:"));
                let reported = std::fs::read_dir(dir.join("agent").join("reports"))
                    .map(|mut e| {
                        e.any(|f| {
                            f.is_ok_and(|f| f.file_name().to_string_lossy().ends_with(".json"))
                        })
                    })
                    .unwrap_or(false);
                if !reported {
                    names.retain(|n| *n != "agent:report");
                }
            }
            names
        }
        "design" | "review" | "agent" => group(&format!("{part}:")),
        name if BUILT_IN_CHECKS.contains(&name) => {
            vec![
                *BUILT_IN_CHECKS
                    .iter()
                    .find(|n| **n == name)
                    .expect("listed"),
            ]
        }
        short => {
            let found: Vec<&'static str> = BUILT_IN_CHECKS
                .iter()
                .copied()
                .filter(|n| n.split_once(':').is_some_and(|(_, s)| s == short))
                .collect();
            if found.len() != 1 {
                bail!(
                    "there is no check '{short}' (checks: {}; groups: spec, design, review, agent, all)",
                    BUILT_IN_CHECKS.join(", ")
                );
            }
            found
        }
    })
}

/// Turns connecting other agents on (or off), and says how to set up clients.
fn run_connect(profile: &str, new: bool, off: bool) -> Result<()> {
    let profile = Profile::open(profile)?;
    if off {
        profile.delete_integration_token()?;
        println!(
            "profile '{}': other agents can no longer connect (their token stopped working).",
            profile.name
        );
        return Ok(());
    }
    let token = match profile.integration_token()? {
        Some(token) if !new => token,
        _ => profile.new_integration_token()?,
    };
    // The running agent's web UI port, if it has one.
    let port = profile
        .is_running()
        .then(|| std::fs::read_to_string(profile.ui_port_path()).ok())
        .flatten()
        .and_then(|p| p.trim().parse::<u16>().ok());
    let base = match port {
        Some(port) => format!("http://127.0.0.1:{port}"),
        None => "http://127.0.0.1:<port>".to_owned(),
    };
    let mcp = format!("{base}/mcp");
    let vscode = serde_json::json!({
        "inputs": [{ "type": "promptString", "id": "proofkin-token", "description": "Proofkin integration token", "password": true }],
        "servers": { "proofkin": { "type": "http", "url": mcp, "headers": { "Authorization": "Bearer ${input:proofkin-token}" } } },
    });
    println!(
        "profile '{}': other agents can connect (MCP and A2A) with this token:\n  {token}\n\
         MCP server:  {mcp}\n\
         A2A agent:   {base}/.well-known/agent-card.json\n\
         Set up a client:\n  \
         VS Code    add to .vscode/mcp.json (VS Code asks for the token):\n             {vscode}\n  \
         Claude     claude mcp add --transport http proofkin {mcp} --header \"Authorization: Bearer {token}\"\n  \
         A2A        the agent card above, with headers Authorization: Bearer <token> and A2A-Version: 1.0\n\
         They run tasks and plans on this profile's peers, never on this computer.",
        profile.name
    );
    if port.is_none() {
        println!(
            "The agent serves them with its web UI: start it with --ui --ui-port 7420 (any fixed port), then use that port above."
        );
    }
    println!(
        "Turn it off: proofkin connect --off --profile {}",
        profile.name
    );
    Ok(())
}

async fn run_plan_command(command: PlanCommand) -> Result<()> {
    match command {
        PlanCommand::Draft {
            goal,
            issue,
            using,
            from,
            note,
            out,
            wait,
            network,
            profile,
            bridge,
        } => {
            let profile = Profile::open(&profile.name)?;
            let plan = match &from {
                Some(file) => Some(
                    std::fs::read_to_string(file)
                        .with_context(|| format!("reading {}", file.display()))?,
                ),
                None => None,
            };
            let ask = proofkin::plan_draft::Ask {
                goal,
                issue,
                provider: using,
                plan,
                note,
            };
            let drafted = proofkin::plan_draft::draft_offline(
                &profile,
                &ask,
                Duration::from_secs(wait),
                network,
                bridge.path(),
                &|line: String| eprintln!("  {line}"),
            )
            .await?;
            match &out {
                Some(file) => {
                    std::fs::write(file, &drafted.plan)
                        .with_context(|| format!("writing {}", file.display()))?;
                    eprintln!("Wrote the plan to {}.", file.display());
                }
                None => print!("{}", drafted.plan),
            }
            for problem in &drafted.problems {
                let kind = if problem.error { "error" } else { "warning" };
                match &problem.step {
                    Some(step) => eprintln!("{kind}: step {step}: {}", problem.message),
                    None => eprintln!("{kind}: {}", problem.message),
                }
            }
            let errors = drafted.errors();
            if errors > 0 {
                eprintln!(
                    "{errors} error{} left: fix the plan before you run it",
                    if errors == 1 { "" } else { "s" }
                );
                std::process::exit(1);
            }
            eprintln!(
                "Review it, then run it with /plan <file> in the agent of profile '{}', or paste it in the web UI's Plan mode.",
                profile.name
            );
        }
    }
    Ok(())
}

async fn run_spec(command: SpecCommand) -> Result<()> {
    use proofkin::spec;
    match command {
        SpecCommand::Check { part, dir } => {
            let names = spec_checks(part.trim(), &dir)?;
            let mut failed = Vec::new();
            for name in names {
                let report = spec::run(name, &dir);
                println!(
                    "{name}: {}",
                    if report.passed() { "passed" } else { "FAILED" }
                );
                for note in &report.notes {
                    println!("  ok: {note}");
                }
                for problem in &report.problems {
                    println!("  - {problem}");
                }
                if !report.passed() {
                    failed.push(name);
                }
            }
            if !failed.is_empty() {
                bail!("{} failed", failed.join(", "));
            }
        }
        SpecCommand::Init { title, dir, force } => {
            for path in spec::init(&dir, &title, force)? {
                println!("created {path}");
            }
            println!(
                "Edit them, then: proofkin spec check --dir {}\n\
                 Or let peers write them: pick the 'spec' playbook in Plan mode.",
                dir.display()
            );
        }
        SpecCommand::Import {
            path,
            from,
            set,
            out,
            profile,
        } => {
            let mut vars = set_vars(&set)?;
            let link = match (path, from) {
                (Some(_), Some(_)) => bail!(
                    "give a folder or --from <link>, not both: --from reads a spec document on \
                     SharePoint or OneDrive instead of a folder"
                ),
                (None, None) => bail!(
                    "give the spec to import: a feature's folder, or --from <link> for a \
                     document on SharePoint or OneDrive"
                ),
                (Some(path), None) => {
                    vars.insert("spec".into(), path.display().to_string());
                    false
                }
                (None, Some(link)) => {
                    // A link that isn't one is refused before the sign-in is looked at.
                    proofkin::workiq::document::share_path(&link)?;
                    vars.insert("spec".into(), link);
                    true
                }
            };
            let profile = Profile::open(&profile.name)?;
            let rendered = if link {
                let work = proofkin::workiq::context::ClientWork::for_profile(&profile)?;
                proofkin::playbooks::render_document(&vars, &work).await?
            } else {
                proofkin::playbooks::render_full(proofkin::playbooks::SPEC_IMPORT, &vars)?
            };
            rendered.save_knowledge(&profile.knowledge())?;
            for note in &rendered.notes {
                eprintln!("{note}");
            }
            match out {
                Some(file) => {
                    std::fs::write(&file, &rendered.text)
                        .with_context(|| format!("writing {}", file.display()))?;
                    eprintln!(
                        "Wrote the plan to {0}. Run it with /plan {0} in the agent of profile '{1}', or paste it in the web UI's Plan mode.",
                        file.display(),
                        profile.name
                    );
                }
                None => print!("{}", rendered.text),
            }
        }
    }
    Ok(())
}

fn run_design(command: DesignCommand) -> Result<()> {
    use proofkin::{design, preview};
    match command {
        DesignCommand::Init { title, dir, force } => {
            for path in design::init(&dir, &title, force)? {
                println!("created {path}");
            }
            println!(
                "Edit them, then: proofkin spec check design --dir {0} (and review)\n\
                 See the review pack: proofkin design render --dir {0}\n\
                 Or let peers write them: pick the 'design' playbook in Plan mode.",
                dir.display()
            );
        }
        DesignCommand::Render { dir, out } => {
            let out = out.unwrap_or_else(|| dir.join("review").join("html"));
            let sources = preview::sources_from_dir(&dir);
            if preview::label(&sources) != Some("review pack") {
                bail!(
                    "{} has no review/ decks or design/ documents to render (proofkin design init writes a starter)",
                    dir.display()
                );
            }
            let files = preview::site(&sources);
            for (path, content) in &files {
                let target = out.join(path);
                std::fs::create_dir_all(target.parent().expect("has a parent"))?;
                std::fs::write(&target, content)
                    .with_context(|| format!("writing {}", target.display()))?;
            }
            println!(
                "wrote {} files; open {}",
                files.len(),
                out.join("index.html").display()
            );
        }
    }
    Ok(())
}

fn run_skill(command: SkillCommand) -> Result<()> {
    match command {
        SkillCommand::List { profile } => {
            for s in Profile::open(&profile.name)?.skills().list() {
                println!("  {:<22} {:<9} {}", s.name, s.source, s.description);
            }
        }
        SkillCommand::Show { name, profile } => {
            print!("{}", Profile::open(&profile.name)?.skills().text(&name)?)
        }
        SkillCommand::Add {
            name,
            file,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            profile.skills().add(&name, &read_input(&file)?)?;
            println!("profile '{}': saved skill {name}", profile.name);
        }
        SkillCommand::Remove { name, profile } => {
            if !Profile::open(&profile.name)?.skills().remove(&name)? {
                bail!("profile has no skill '{name}' of its own");
            }
        }
        SkillCommand::Import {
            path,
            rename,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            let (found, text) = proofkin::skills::read_skill_file(&path)?;
            let name = rename.unwrap_or(found);
            let skills = profile.skills();
            let replaces = skills.list().into_iter().find(|s| s.name == name);
            skills.add(&name, &text)?;
            println!(
                "profile '{}': imported skill {name}{}. Use it with skill={name} in a plan step.",
                profile.name,
                match replaces.map(|s| s.source) {
                    Some("built-in") => " (replaces the built-in one)",
                    Some("profile") => " (replaces the profile's own)",
                    Some(_) => " (used instead of the plugin's)",
                    None => "",
                }
            );
        }
    }
    Ok(())
}

fn run_agent_command(command: AgentCommand) -> Result<()> {
    let uses = |a: &proofkin::skills::AgentDef| {
        [("skills", &a.skills), ("MCP servers", &a.mcp)]
            .into_iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(what, v)| format!("{what}: {}", v.join(", ")))
            .collect::<Vec<_>>()
            .join("; ")
    };
    match command {
        AgentCommand::List { profile } => {
            let agents = Profile::open(&profile.name)?.skills().agents();
            if agents.is_empty() {
                println!(
                    "no agents yet: add one with agent add FILE, or import a plugin that has some"
                );
            }
            let from = |a: &proofkin::skills::AgentDef| {
                a.plugin
                    .as_deref()
                    .map(|p| format!("plugin {p}"))
                    .unwrap_or_else(|| "yours".into())
            };
            let width = agents.iter().map(|a| from(a).len()).max().unwrap_or(0);
            for a in &agents {
                println!("  {:<22} {:<width$} {}", a.name, from(a), a.description);
                let uses = uses(a);
                if !uses.is_empty() {
                    println!("  {:<22} {:<width$} {uses}", "", "");
                }
            }
        }
        AgentCommand::Show { name, profile } => {
            let a = Profile::open(&profile.name)?.skills().agent(&name)?;
            println!(
                "agent {}{}",
                a.name,
                a.plugin
                    .as_deref()
                    .map(|p| format!(" (plugin {p})"))
                    .unwrap_or_default()
            );
            if !a.description.is_empty() {
                println!("  {}", a.description);
            }
            let uses = uses(&a);
            if !uses.is_empty() {
                println!("  {uses}");
            }
            for note in &a.notes {
                println!("  note: {note}");
            }
            println!("\n{}", a.instructions);
        }
        AgentCommand::Add {
            file,
            rename,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            let text = read_input(&file)?;
            let fallback = file
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("agent")
                .trim_end_matches(".md")
                .trim_end_matches(".agent")
                .to_owned();
            let name = rename
                .or_else(|| proofkin::skills::front_matter_name(&text))
                .unwrap_or(fallback);
            let a = profile.skills().add_agent(&name, &text)?;
            println!(
                "profile '{}': saved agent {}. Use it with agent={} in a plan step.",
                profile.name, a.name, a.name
            );
            for note in &a.notes {
                println!("  note: {note}");
            }
        }
        AgentCommand::Remove { name, profile } => {
            if !Profile::open(&profile.name)?.skills().remove_agent(&name)? {
                bail!("profile has no agent '{name}' of its own");
            }
        }
    }
    Ok(())
}

fn run_playbook(command: PlaybookCommand) -> Result<()> {
    use proofkin::playbooks;
    match command {
        PlaybookCommand::List => {
            for (name, description) in playbooks::list() {
                let vars: Vec<String> = playbooks::variables(name)?
                    .into_iter()
                    .map(|(v, d)| match d {
                        Some(d) => format!("{v}={d}"),
                        None => format!("{v} (required)"),
                    })
                    .collect();
                println!(
                    "  {name:<15} {description}\n                  --set {}",
                    vars.join(", ")
                );
            }
        }
        PlaybookCommand::Explain { name } => {
            let description = playbooks::list()
                .into_iter()
                .find(|(n, _)| *n == name)
                .map(|(_, d)| d)
                .unwrap_or_default();
            let o = playbooks::outline(&name)?;
            println!("{name}: {description}\n\nWhat it does, step by step:");
            for (i, step) in o.steps.iter().enumerate() {
                println!("  {}. {}", i + 1, step.replace("**", ""));
            }
            println!("\nWhat it proves (only with passing checks or a sign-off):");
            for (id, claim) in &o.claims {
                println!("  - {id}: {claim}");
            }
            println!("\nWhat you need:");
            for need in &o.needs {
                println!("  - {need}");
            }
            println!(
                "\nFill it in: proofkin playbook show {name} --set peer=<peer> ... > {name}.plan, then /plan {name}.plan"
            );
        }
        PlaybookCommand::Show { name, set } => {
            print!("{}", playbooks::render(&name, &set_vars(&set)?)?);
        }
    }
    Ok(())
}

/// `--set NAME=VALUE` playbook values.
fn set_vars(set: &[String]) -> Result<std::collections::BTreeMap<String, String>> {
    let mut vars = std::collections::BTreeMap::new();
    for pair in set {
        let (k, v) = pair
            .split_once('=')
            .with_context(|| format!("--set {pair:?} must look like NAME=VALUE"))?;
        vars.insert(k.trim().to_owned(), v.to_owned());
    }
    Ok(vars)
}

fn run_schedule(command: ScheduleCommand) -> Result<()> {
    use proofkin::schedule::{Schedule, ScheduleStore};
    let when = |s: &Schedule| {
        s.next_run()
            .map(|t| t.format("%a %Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "paused".into())
    };
    match command {
        ScheduleCommand::Add {
            name,
            when: at,
            plan,
            playbook,
            set,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            let (text, source) = match (plan, playbook) {
                (Some(file), None) => (
                    std::fs::read_to_string(&file)
                        .with_context(|| format!("reading plan file {}", file.display()))?,
                    file.display().to_string(),
                ),
                (None, Some(book)) => (
                    proofkin::playbooks::render(&book, &set_vars(&set)?)?,
                    format!("playbook {book}"),
                ),
                _ => bail!("give the plan to run: --plan FILE or --playbook NAME (with --set)"),
            };
            let schedule = Schedule::new(&name, &at, &text, &source)?;
            let next = when(&schedule);
            ScheduleStore::update(&profile.schedules_path(), |store| {
                store.add(schedule);
                Ok(())
            })?;
            println!(
                "profile '{}': scheduled '{name}' ({at}; next run {next}). It runs while this profile's agent runs; each run is an evidence run.",
                profile.name
            );
        }
        ScheduleCommand::List { profile } => {
            let profile = Profile::open(&profile.name)?;
            for s in ScheduleStore::load(&profile.schedules_path())?.schedules {
                println!(
                    "  {:<14} {:<20} next: {:<22} {}{}",
                    s.name,
                    s.when,
                    when(&s),
                    s.source,
                    s.last_result
                        .as_deref()
                        .map(|r| format!("\n  {:<14} last: {r}", ""))
                        .unwrap_or_default()
                );
            }
        }
        ScheduleCommand::Remove { name, profile } => {
            let profile = Profile::open(&profile.name)?;
            ScheduleStore::update(&profile.schedules_path(), |store| {
                if !store.remove(&name) {
                    bail!("profile '{}' has no schedule named '{name}'", profile.name);
                }
                Ok(())
            })?;
            println!("profile '{}': removed schedule '{name}'", profile.name);
        }
        ScheduleCommand::Pause { name, profile } => set_enabled(&name, &profile.name, false)?,
        ScheduleCommand::Resume { name, profile } => set_enabled(&name, &profile.name, true)?,
    }
    Ok(())
}

fn set_enabled(name: &str, profile: &str, enabled: bool) -> Result<()> {
    use proofkin::schedule::ScheduleStore;
    let profile = Profile::open(profile)?;
    ScheduleStore::update(&profile.schedules_path(), |store| {
        store
            .get_mut(name)
            .with_context(|| format!("profile '{}' has no schedule named '{name}'", profile.name))?
            .set_enabled(enabled);
        Ok(())
    })?;
    println!(
        "profile '{}': schedule '{name}' {}",
        profile.name,
        if enabled {
            "resumed (its time counts from now)"
        } else {
            "paused"
        }
    );
    Ok(())
}

fn run_monitor(command: MonitorCommand) -> Result<()> {
    use proofkin::ops::{Monitor, MonitorStore};
    match command {
        MonitorCommand::Add {
            name,
            share,
            check,
            every,
            fail_after,
            slo,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            let shares = profile.shares()?;
            MonitorStore::update(&profile.monitors_path(), |store| {
                store.set(
                    Monitor {
                        name: name.clone(),
                        share,
                        check,
                        every_secs: every,
                        fail_after,
                        slo,
                    },
                    &shares,
                )
            })?;
            println!(
                "profile '{}': monitor {name} every {every}s (SLO {slo}%); it runs while the profile's agent runs (a running agent starts it within seconds)",
                profile.name
            );
        }
        MonitorCommand::List { profile } => {
            for m in MonitorStore::load(&Profile::open(&profile.name)?.monitors_path())?.monitors {
                println!(
                    "  {:<12} {}:{} every {}s, incident after {} failures, SLO {}%",
                    m.name, m.share, m.check, m.every_secs, m.fail_after, m.slo
                );
            }
        }
        MonitorCommand::Remove { name, profile } => {
            let profile = Profile::open(&profile.name)?;
            if !proofkin::ops::remove_monitor(
                &profile.monitors_path(),
                &profile.ops_store(),
                &name,
            )? {
                bail!("there is no monitor '{name}'");
            }
        }
    }
    Ok(())
}

fn run_ops(command: OpsCommand) -> Result<()> {
    match command {
        OpsCommand::Report { hours, profile } => {
            print!("{}", ops_report(&Profile::open(&profile.name)?, hours)?)
        }
        OpsCommand::Incidents { hours, profile } => {
            let since = proofkin::runs::unix_now().saturating_sub(hours * 3600);
            for e in Profile::open(&profile.name)?.ops_store().incidents(since) {
                println!(
                    "{}  {:<10} {:<10} {}",
                    proofkin::runs::utc(e.ts),
                    e.monitor,
                    format!("{:?}", e.kind).to_lowercase(),
                    e.text.lines().next().unwrap_or_default()
                );
            }
        }
    }
    Ok(())
}

fn run_runs(command: RunsCommand) -> Result<()> {
    use proofkin::runs::{RunRecord, utc};
    match command {
        RunsCommand::List { profile } => {
            let profile = Profile::open(&profile.name)?;
            let runs = RunRecord::list(&profile.runs_dir())?;
            if runs.is_empty() {
                println!(
                    "profile '{}' has no evidence runs yet (run a plan)",
                    profile.name
                );
            }
            for run in runs {
                println!(
                    "  {}  {} UTC  {}  {}/{} claims proven, {}/{} steps done",
                    run.id,
                    utc(run.started_unix),
                    if run.passed { "PASS" } else { "FAIL" },
                    run.proven,
                    run.claims,
                    run.steps_done,
                    run.steps
                );
            }
        }
        RunsCommand::Show { id, profile } => {
            let profile = Profile::open(&profile.name)?;
            print!(
                "{}",
                RunRecord::load(&profile.runs_dir(), &id)?.render_markdown()
            );
        }
        RunsCommand::Verify {
            id,
            dir,
            file,
            profile,
        } => {
            // A single file has no patches next to it: only signatures and outputs count.
            let (path, dir) = match (file, dir, id, profile) {
                (Some(file), _, _, _) => (file, None),
                (None, Some(dir), _, _) => (dir.join("run.json"), Some(dir)),
                (None, None, Some(id), Some(profile)) => {
                    let dir = Profile::open(&profile)?.runs_dir().join(id);
                    (dir.join("run.json"), Some(dir))
                }
                _ => unreachable!("clap requires a run id with --profile, --dir or --file"),
            };
            let run: RunRecord = serde_json::from_str(
                &std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?,
            )
            .with_context(|| format!("parsing {}", path.display()))?;
            let mut failed = false;
            let (run, checks) = run.reverified(dir.as_deref());
            for (step, result) in &checks {
                match result {
                    Ok(text) => println!("  ok      {step}: {text}"),
                    Err(e) => {
                        failed = true;
                        println!("  FAILED  {step}: {e:#}");
                    }
                }
            }
            println!("run {}: {}", run.id, run.verdict_line());
            if failed {
                bail!("some evidence doesn't verify");
            }
            println!("all {} signed records verify", checks.len());
            println!(
                "requested by {} ({}); signed by (check these keys are the people you expect):",
                run.boss, run.boss_id
            );
            let signers = run.signers();
            if signers.is_empty() {
                println!("  nobody: no step has signed evidence");
            }
            for (name, id) in signers {
                println!("  {name:<12} {id}");
            }
        }
    }
    Ok(())
}

fn run_squad(command: SquadCommand) -> Result<()> {
    match command {
        SquadCommand::Create {
            name,
            member,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            let trust = profile.trust()?;
            if let Some(unknown) = member.iter().find(|m| trust.get_by_name(m).is_none()) {
                bail!(
                    "'{unknown}' is not on profile '{}''s allowlist; add it with: proofkin peers allow <ticket> --profile {}",
                    profile.name,
                    profile.name
                );
            }
            let mut squads = SquadStore::load(&profile.squads_path())?;
            squads.set(&name, member)?;
            squads.save(&profile.squads_path())?;
            let squad = squads.get(&name).expect("just set");
            println!(
                "profile '{}': squad {name} = {}",
                profile.name,
                squad.members.join(", ")
            );
        }
        SquadCommand::List { profile } => {
            let profile = Profile::open(&profile.name)?;
            for squad in SquadStore::load(&profile.squads_path())?.squads {
                println!("  {:<12} {}", squad.name, squad.members.join(", "));
            }
        }
        SquadCommand::Remove { name, profile } => {
            let profile = Profile::open(&profile.name)?;
            let mut squads = SquadStore::load(&profile.squads_path())?;
            if !squads.remove(&name) {
                bail!("profile '{}' has no squad {name}", profile.name);
            }
            squads.save(&profile.squads_path())?;
            println!("profile '{}': removed squad {name}", profile.name);
        }
    }
    Ok(())
}

fn run_share(command: ShareCommand) -> Result<()> {
    match command {
        ShareCommand::Add {
            name,
            path,
            access,
            checks,
            worktrees,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            let mut shares = profile.shares()?;
            let checks = proofkin::shares::parse_checks(&checks)?;
            let share = shares
                .add_with(&name, &path, access, checks, worktrees)?
                .clone();
            shares.save(&profile.shares_path())?;
            println!(
                "profile '{}': sharing {} as '{name}' (at most {access}){}. Grant it to a peer with: proofkin peers allow <peer> --profile {} --delegate --share data,{name}",
                profile.name,
                share.path.display(),
                share_extras(&share),
                profile.name
            );
        }
        ShareCommand::List { profile } => {
            let profile = Profile::open(&profile.name)?;
            for share in profile.shares()?.shares {
                println!(
                    "  {:<12} at most {:<16} {}{}",
                    share.name,
                    share.max_access.to_string(),
                    share.path.display(),
                    share_extras(&share)
                );
            }
        }
        ShareCommand::Team {
            name,
            file,
            model,
            approve,
            clear,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            let data = profile.data_folder();
            if model {
                proofkin::shares::ShareStore::update(&profile.shares_path(), data, |shares| {
                    shares.set_team_model(&name, approve)
                })?;
                println!(
                    "profile '{}': '{name}' takes team steps; its model writes the team plan for each one{}; peers hand them over with: name team @{}:{name} what to do",
                    profile.name,
                    if approve {
                        ", and you approve each plan before it runs"
                    } else {
                        ""
                    },
                    profile.name
                );
                return Ok(());
            }
            let plan = match (file, clear) {
                (Some(file), _) => Some(
                    std::fs::read_to_string(&file)
                        .with_context(|| format!("reading {}", file.display()))?,
                ),
                (None, true) => None,
                (None, false) => bail!(
                    "give the team plan file, --model to have its model write each one, or --clear to remove it"
                ),
            };
            let steps = plan
                .as_deref()
                .map(proofkin::team::validate_template)
                .transpose()?
                .map(|p| p.steps.len());
            proofkin::shares::ShareStore::update(&profile.shares_path(), data, |shares| {
                shares.set_team(&name, plan)
            })?;
            match steps {
                Some(n) => println!(
                    "profile '{}': '{name}' takes team steps with a team plan of {n} step{}; peers hand them over with: name team @{}:{name} what to do",
                    profile.name,
                    if n == 1 { "" } else { "s" },
                    profile.name
                ),
                None => println!(
                    "profile '{}': '{name}' has no team plan any more",
                    profile.name
                ),
            }
        }
        ShareCommand::PullRequests {
            name,
            off,
            approve,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            if !off {
                proofkin::github::RepoStore::load(&profile.repos_path())?
                    .get(&name)
                    .with_context(|| {
                        format!(
                            "pull requests need a GitHub repository linked as '{name}' (repo link)"
                        )
                    })?;
            }
            let data = profile.data_folder();
            proofkin::shares::ShareStore::update(&profile.shares_path(), data, |shares| {
                shares.set_pull_requests(&name, !off, approve)
            })?;
            if off {
                println!(
                    "profile '{}': '{name}' no longer takes pull request steps",
                    profile.name
                );
            } else {
                println!(
                    "profile '{}': '{name}' takes pull request steps from peers with edit access{}; they end a plan with: ship after <steps> pr @{}:{name} <title>",
                    profile.name,
                    if approve {
                        ", each opened once you approve it (web UI: Approvals)"
                    } else {
                        ""
                    },
                    profile.name
                );
            }
        }
        ShareCommand::Remove { name, profile } => {
            let profile = Profile::open(&profile.name)?;
            let mut shares = profile.shares()?;
            if !shares.remove(&name)? {
                bail!("profile '{}' shares no folder named '{name}'", profile.name);
            }
            shares.save(&profile.shares_path())?;
            println!("profile '{}': stopped sharing '{name}'", profile.name);
        }
    }
    Ok(())
}

fn run_mcp(command: McpCommand) -> Result<()> {
    use proofkin::mcp::{Fill, McpServer, McpTemplate, parse_config, parse_pairs, server_name};
    use proofkin::shares::McpOffer;
    let tools_text = |tools: &[String]| {
        if tools.is_empty() {
            "all".to_owned()
        } else {
            tools.join(", ")
        }
    };
    match command {
        McpCommand::Add {
            share,
            name,
            url,
            headers,
            sse,
            env,
            plugin,
            server,
            set,
            workiq,
            changes,
            tools,
            access,
            description,
            profile,
            command,
        } => {
            let profile = Profile::open(&profile.name)?;
            let access = access.unwrap_or(if workiq {
                Access::ReadOnly
            } else {
                Access::Full
            });
            let offer = McpOffer {
                share: share.clone(),
                name: name.clone(),
                access,
                tools,
                description: description.unwrap_or_default(),
                replace: true,
            };
            let (server, _) = if workiq {
                profile.update_shares(|shares| {
                    shares.offer_mcp(&offer, |_| {
                        let mut server = McpServer::workiq(Vec::new());
                        server.changes = changes;
                        Ok(server)
                    })
                })?
            } else if let Some(plugin) = plugin {
                let store = profile.plugins();
                let p = store.get(&plugin)?;
                let template = p.pick_server(server.as_deref(), &name)?;
                let mut values = parse_pairs(&set, "value")?;
                values_from_env(&p.mcp[&template], &mut values);
                profile.update_shares(|shares| {
                    shares.offer_mcp(&offer, |folder| {
                        store.instantiate(&plugin, &template, folder, &values)
                    })
                })?
            } else {
                let mut server = match (url, command.split_first()) {
                    (Some(url), None) => McpServer::remote(url, sse),
                    (None, Some((cmd, args))) => McpServer::local(cmd.clone(), args.to_vec()),
                    (Some(_), Some(_)) => {
                        bail!("give either --url or a command after --, not both")
                    }
                    (None, None) => bail!(
                        "give the server's command after -- (local), its --url (remote) or --plugin"
                    ),
                };
                server.env = parse_pairs(&env, "variable")?;
                server.headers = parse_pairs(&headers, "header")?;
                profile.update_shares(|shares| shares.offer_mcp(&offer, |_| Ok(server)))?
            };
            println!(
                "profile '{}': tasks in '{share}' with at least {access} access may use MCP server '{name}' ({}); tools: {}. {}",
                profile.name,
                server.describe(),
                tools_text(&server.tools),
                if server.workiq {
                    format!(
                        "Its owner's tasks ask for it with mcp={name} in a plan step, and peers' tasks once you grant them --m365-tools; it needs this profile's Work IQ sign-in (or its owner's, for a digital worker)."
                    )
                } else {
                    format!("Peers ask for it with mcp={name} in a plan step.")
                }
            );
        }
        McpCommand::Import {
            file,
            share,
            servers,
            set,
            access,
            replace,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            let text = read_input(&file)?;
            let config: serde_json::Value =
                serde_json::from_str(text.trim_start_matches('\u{feff}'))
                    .with_context(|| format!("{} isn't valid JSON", file.display()))?;
            let (mut found, notes) = parse_config(&config, &Default::default())?;
            for note in &notes {
                println!("  note: {note}");
            }
            let chosen: Vec<(String, McpTemplate)> = if servers.is_empty() {
                found.into_iter().collect()
            } else {
                let all = found.keys().cloned().collect::<Vec<_>>().join(", ");
                servers
                    .iter()
                    .map(|s| {
                        found.remove(s).map(|t| (s.clone(), t)).with_context(|| {
                            format!("{} has no server '{s}' (it has: {all})", file.display())
                        })
                    })
                    .collect::<Result<_>>()?
            };
            if chosen.is_empty() {
                bail!("{} has no servers to offer", file.display());
            }
            let mut values = parse_pairs(&set, "value")?;
            for (_, template) in &chosen {
                values_from_env(template, &mut values);
            }
            let (offered, _) = profile.update_shares(|shares| {
                let mut offered = Vec::new();
                for (original, template) in &chosen {
                    let offer = McpOffer {
                        share: share.clone(),
                        name: server_name(original),
                        access,
                        tools: Vec::new(),
                        description: String::new(),
                        replace,
                    };
                    let server = shares
                        .offer_mcp(&offer, |folder| {
                            template.instantiate(&Fill {
                                plugin_root: None,
                                data_dir: None,
                                folder,
                                values: &values,
                            })
                        })
                        .with_context(|| format!("server '{original}'"))?;
                    offered.push((offer.name, server));
                }
                Ok(offered)
            })?;
            for (name, server) in offered {
                println!(
                    "profile '{}': '{share}' offers MCP server '{name}' ({}) to tasks with at least {access} access; tools: {}",
                    profile.name,
                    server.describe(),
                    tools_text(&server.tools)
                );
            }
        }
        McpCommand::List { profile } => {
            let profile = Profile::open(&profile.name)?;
            for share in profile.shares()?.shares {
                for (name, server) in &share.mcp {
                    let state = match (server.paused, server.plugin_off) {
                        (_, true) => "  [plugin off]",
                        (true, _) => "  [paused]",
                        _ => "",
                    };
                    println!(
                        "  {:<20} {}+ access, tools: {}  {}{}{state}",
                        format!("{}/{name}", share.name),
                        server.access,
                        tools_text(&server.tools).replace(", ", ","),
                        server.describe(),
                        server
                            .from
                            .as_ref()
                            .map(|f| format!("  (plugin {}: {})", f.plugin, f.server))
                            .unwrap_or_default(),
                    );
                    if !server.description.is_empty() {
                        println!("  {:<20} {}", "", server.description);
                    }
                }
            }
        }
        McpCommand::Pause {
            share,
            name,
            profile,
        } => set_mcp_paused(&share, &name, &profile.name, true)?,
        McpCommand::Resume {
            share,
            name,
            profile,
        } => set_mcp_paused(&share, &name, &profile.name, false)?,
        McpCommand::Remove {
            share,
            name,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            profile.update_shares(|shares| {
                if !shares.remove_mcp(&share, &name)? {
                    bail!("folder '{share}' offers no MCP server named '{name}'");
                }
                Ok(())
            })?;
            println!(
                "profile '{}': '{share}' no longer offers MCP server '{name}' (a running agent stops offering it within seconds)",
                profile.name
            );
        }
    }
    Ok(())
}

/// Fills values a server template needs, not given with --set, from environment
/// variables of the same name.
fn values_from_env(
    template: &proofkin::mcp::McpTemplate,
    values: &mut std::collections::BTreeMap<String, String>,
) {
    for input in &template.inputs {
        if !values.contains_key(&input.name)
            && let Ok(value) = std::env::var(&input.name)
            && !value.is_empty()
        {
            println!("  using {} from the environment", input.name);
            values.insert(input.name.clone(), value);
        }
    }
}

fn set_mcp_paused(share: &str, name: &str, profile: &str, paused: bool) -> Result<()> {
    let profile = Profile::open(profile)?;
    profile.update_shares(|shares| {
        shares.edit_mcp(share, name, |server| {
            server.paused = paused;
            Ok(())
        })
    })?;
    println!(
        "profile '{}': MCP server '{name}' in '{share}' is {}",
        profile.name,
        if paused {
            "paused (offered to no task)"
        } else {
            "offered again"
        }
    );
    Ok(())
}

async fn run_plugin(command: PluginCommand) -> Result<()> {
    use proofkin::plugins::{Inspection, Source, fetch, inspect};
    let print_changes = |lines: &[String]| {
        for line in lines {
            println!("  {line}");
        }
    };
    match command {
        PluginCommand::Add {
            source,
            plugin,
            rename,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            let store = profile.plugins();
            let creds = profile.github().ok();
            let fetched = fetch(&Source::parse(&source)?, &store.staging(), creds.as_ref()).await?;
            let entry = match inspect(&fetched.root)? {
                Inspection::Plugin(_) if plugin.is_some() => {
                    bail!("{source} is a plugin, not a marketplace: leave out --plugin")
                }
                Inspection::Plugin(_) => None,
                Inspection::Marketplace { name, plugins } => {
                    let Some(pick) = plugin else {
                        println!("{source} is the marketplace '{name}'. Its plugins:");
                        for e in &plugins {
                            println!("  {:<24} {}", e.name, e.description);
                        }
                        bail!("pick one: proofkin plugin add {source} --plugin NAME");
                    };
                    let entry = plugins
                        .iter()
                        .find(|e| e.name == pick)
                        .with_context(|| format!("marketplace '{name}' has no plugin '{pick}'"))?;
                    Some(
                        fetched
                            .entry(entry, &store.staging(), creds.as_ref())
                            .await?,
                    )
                }
            };
            let installed = store.install(entry.as_ref().unwrap_or(&fetched), rename.as_deref())?;
            print_plugin(&installed, &profile)?;
            println!(
                "\nprofile '{}': imported plugin '{}'. Nothing in it ran.",
                profile.name, installed.name
            );
            if let Some((server, _)) = installed.mcp.iter().next() {
                println!(
                    "Offer its MCP servers in a shared folder: proofkin mcp add <folder> {server} --plugin {} [--set NAME=VALUE]",
                    installed.name
                );
            }
        }
        PluginCommand::List { profile } => {
            let profile = Profile::open(&profile.name)?;
            let plugins = profile.plugins().list()?;
            if plugins.is_empty() {
                println!(
                    "profile '{}' has no plugins: proofkin plugin add <source>",
                    profile.name
                );
            }
            for p in plugins {
                println!(
                    "  {:<20} {:<10} {:<4} {} skills, {} MCP servers  {}",
                    p.name,
                    if p.version.is_empty() {
                        "-"
                    } else {
                        &p.version
                    },
                    if p.enabled { "on" } else { "off" },
                    p.skills.len(),
                    p.mcp.len(),
                    p.source
                );
            }
        }
        PluginCommand::Show { name, profile } => {
            let profile = Profile::open(&profile.name)?;
            print_plugin(&profile.plugins().get(&name)?, &profile)?;
        }
        PluginCommand::Update { name, profile } => {
            let profile = Profile::open(&profile.name)?;
            let store = profile.plugins();
            let old = store.get(&name)?;
            let creds = profile.github().ok();
            let fetched = fetch(
                &Source::parse(&old.source)?,
                &store.staging(),
                creds.as_ref(),
            )
            .await?;
            let ((plugin, changes), _) =
                profile.update_shares(|shares| store.update(&name, &fetched, shares))?;
            println!(
                "profile '{}': updated plugin '{}':",
                profile.name, plugin.name
            );
            print_changes(&changes.lines);
        }
        PluginCommand::Enable { name, profile } => set_plugin_enabled(&name, &profile.name, true)?,
        PluginCommand::Disable { name, profile } => {
            set_plugin_enabled(&name, &profile.name, false)?
        }
        PluginCommand::Remove { name, profile } => {
            let profile = Profile::open(&profile.name)?;
            let store = profile.plugins();
            let (changes, _) = profile.update_shares(|shares| store.remove(&name, shares))?;
            println!("profile '{}': removed plugin '{name}'", profile.name);
            print_changes(&changes.lines);
        }
    }
    Ok(())
}

fn set_plugin_enabled(name: &str, profile: &str, enabled: bool) -> Result<()> {
    let profile = Profile::open(profile)?;
    let store = profile.plugins();
    let (changes, _) = profile.update_shares(|shares| store.set_enabled(name, enabled, shares))?;
    println!(
        "profile '{}': plugin '{name}' is {}",
        profile.name,
        if enabled {
            "on"
        } else {
            "off: its skills and MCP servers no longer apply"
        }
    );
    for line in changes.lines {
        println!("  {line}");
    }
    Ok(())
}

/// Prints what plugin `p` contains and where its MCP servers are offered.
fn print_plugin(p: &proofkin::plugins::Plugin, profile: &Profile) -> Result<()> {
    println!(
        "{} {}({}, {})",
        p.name,
        if p.version.is_empty() {
            String::new()
        } else {
            format!("{} ", p.version)
        },
        p.format,
        if p.enabled { "on" } else { "off" }
    );
    if !p.description.is_empty() {
        println!("  {}", p.description);
    }
    println!(
        "  from {}{}",
        p.source,
        p.commit
            .as_deref()
            .map(|c| format!(" at {}", &c[..c.len().min(12)]))
            .unwrap_or_default()
    );
    if !p.skills.is_empty() {
        println!("  skills (use with skill=NAME in a plan step):");
        for s in &p.skills {
            println!("    {:<24} {}", s.name, s.description);
        }
    }
    if !p.agents.is_empty() {
        println!("  agents (use with agent=NAME in a plan step):");
        for a in &p.agents {
            println!("    {:<24} {}", a.name, a.description);
            for (what, names) in [("skills", &a.skills), ("MCP servers", &a.mcp)] {
                if !names.is_empty() {
                    println!("    {:<24} {what}: {}", "", names.join(", "));
                }
            }
        }
    }
    if !p.mcp.is_empty() {
        let shares = profile.shares()?;
        println!(
            "  MCP servers (offer one with: mcp add <folder> <name> --plugin {}):",
            p.name
        );
        for (name, t) in &p.mcp {
            println!("    {:<24} {}", name, t.describe());
            if !t.inputs.is_empty() {
                let needs: Vec<String> = t
                    .inputs
                    .iter()
                    .map(|i| {
                        format!(
                            "{}{}{}",
                            i.name,
                            if i.secret { " (secret)" } else { "" },
                            i.default
                                .as_deref()
                                .map(|d| format!(" [default {d}]"))
                                .unwrap_or_default()
                        )
                    })
                    .collect();
                println!("    {:<24} needs: {}", "", needs.join(", "));
            }
            for share in &shares.shares {
                for (offered, def) in &share.mcp {
                    if def
                        .from
                        .as_ref()
                        .is_some_and(|f| f.plugin == p.name && &f.server == name)
                    {
                        println!(
                            "    {:<24} offered in '{}' as {offered}{}",
                            "",
                            share.name,
                            if def.paused { " (paused)" } else { "" }
                        );
                    }
                }
            }
        }
    }
    for note in &p.notes {
        println!("  note: {note}");
    }
    Ok(())
}

fn run_peers(command: PeersCommand) -> Result<()> {
    match command {
        PeersCommand::Id { profile } => {
            let profile = Profile::open(&profile.name)?;
            let new = !profile.exists()?;
            let key = profile.identity()?;
            if new {
                eprintln!(
                    "note: '{0}' is a new profile; it got its identity just now (a typo? remove it with: proofkin profile delete {0})",
                    profile.name
                );
            }
            let ticket = Ticket {
                name: profile.name.clone(),
                addr: key.public().into(),
            };
            println!(
                "profile '{}'\n  endpoint id: {}",
                profile.name,
                key.public()
            );
            println!("  ticket:      {}", ticket.encode());
            println!(
                "Give the ticket to peers (they run: proofkin peers allow <ticket>). A running"
            );
            println!("agent prints a fuller ticket with its current addresses (/ticket).");
        }
        PeersCommand::Allow {
            target,
            profile,
            name,
            delegate,
            tasks_per_hour,
            access,
            share,
            m365,
            m365_tools,
            github,
            no_github,
            agents,
            no_agents,
        } => {
            let profile = Profile::open(&profile.name)?;
            if let Some(bad) = agents
                .iter()
                .flatten()
                .find(|a| proofkin::profile::validate_name(a).is_err())
            {
                bail!("{bad:?} isn't a factory agent's name");
            }
            let grant = m365
                .as_deref()
                .map(|spec| proofkin::M365Grant::parse(spec, m365_tools))
                .transpose()?;
            let github = github
                .map(|login| {
                    let login = login.trim().trim_start_matches('@').to_owned();
                    if proofkin::github::login_ok(&login) {
                        Ok(login)
                    } else {
                        Err(anyhow::anyhow!("{login:?} isn't a GitHub login"))
                    }
                })
                .transpose()?;
            let ticket = Ticket::parse(&target)?;
            let name = name
                .or_else(|| Some(ticket.name.clone()).filter(|n| !n.is_empty()))
                .context("a bare endpoint id has no name; pass --name")?;
            proofkin::profile::validate_name(&name).context("invalid peer name")?;
            if ticket.addr.id == profile.identity()?.public() {
                bail!("that is this profile's own id");
            }
            let shared = profile.shares()?;
            if let Some(missing) = share.iter().find(|s| shared.get(s).is_none()) {
                bail!(
                    "profile '{}' shares no folder named '{missing}'; add it first: proofkin share add {missing} <folder> --profile {}",
                    profile.name,
                    profile.name
                );
            }
            let mut trust = profile.trust()?;
            let m365 = match grant {
                Some(grant) => grant,
                None => trust.m365_of(&ticket.addr.id, &name),
            };
            let m365_note = m365
                .as_ref()
                .map(|g| format!("; Microsoft 365 {}", g.describe()))
                .unwrap_or_default();
            let github = match github {
                Some(login) => Some(login),
                None if no_github => None,
                None => trust.github_of(&ticket.addr.id, &name),
            };
            let github_note = github
                .as_deref()
                .map(|g| format!("; GitHub account {g}"))
                .unwrap_or_default();
            let agents = match agents {
                Some(mut list) => {
                    list.sort();
                    list.dedup();
                    list
                }
                None if no_agents => Vec::new(),
                None => trust.agents_of(&ticket.addr.id, &name),
            };
            let agents_note = if agents.is_empty() {
                String::new()
            } else {
                format!("; factory agents {}", agents.join(","))
            };
            trust.allow(TrustedPeer {
                name: name.clone(),
                id: ticket.addr.id,
                addrs: ticket.addr.addrs,
                can_delegate: delegate,
                tasks_per_hour,
                access,
                shares: share.clone(),
                m365,
                github,
                agents,
            });
            trust.save(&profile.trust_path())?;
            let rights = if delegate {
                format!(
                    "may delegate up to {tasks_per_hour} tasks/hour with {access} access in: {}",
                    share.join(", ")
                )
            } else {
                "chat only, may not delegate".into()
            };
            println!(
                "profile '{}': allowed {name} ({}), {rights}{m365_note}{github_note}{agents_note}",
                profile.name,
                ticket.addr.id.fmt_short()
            );
        }
        PeersCommand::List { profile } => {
            let profile = Profile::open(&profile.name)?;
            let trust = profile.trust()?;
            if trust.peers.is_empty() {
                println!("profile '{}' has no allowlisted peers", profile.name);
            }
            for p in trust.peers {
                let rights = if p.can_delegate {
                    format!(
                        "delegate <= {}/h, {}, folders: {}",
                        p.tasks_per_hour,
                        p.access,
                        p.shares.join(",")
                    )
                } else {
                    "no delegation".into()
                };
                let m365 = match &p.m365 {
                    Some(g) => format!("; Microsoft 365 {}", g.describe()),
                    None => String::new(),
                };
                let github = match &p.github {
                    Some(g) => format!("; GitHub {g}"),
                    None => String::new(),
                };
                let agents = if p.agents.is_empty() {
                    String::new()
                } else {
                    format!("; factory agents {}", p.agents.join(","))
                };
                println!("  {:<16} {}  {rights}{m365}{github}{agents}", p.name, p.id);
            }
        }
        PeersCommand::Github {
            peer,
            login,
            off: _,
            profile,
        } => {
            let profile = Profile::open(&profile.name)?;
            let account = login
                .map(|login| {
                    let login = login.trim().trim_start_matches('@').to_owned();
                    if proofkin::github::login_ok(&login) {
                        Ok(login)
                    } else {
                        Err(anyhow::anyhow!("{login:?} isn't a GitHub login"))
                    }
                })
                .transpose()?;
            let mut trust = profile.trust()?;
            let Some(entry) = trust
                .peers
                .iter_mut()
                .find(|p| p.name == peer || p.id.to_string() == peer)
            else {
                bail!("no allowlisted peer {peer} in profile '{}'", profile.name);
            };
            entry.github = account.clone();
            let name = entry.name.clone();
            trust.save(&profile.trust_path())?;
            match account {
                Some(login) => println!(
                    "profile '{}': {name}'s GitHub account is {login}",
                    profile.name
                ),
                None => println!("profile '{}': forgot {name}'s GitHub account", profile.name),
            }
            println!(
                "A running agent uses it once it restarts (the web UI's Change… sets it at once)."
            );
        }
        PeersCommand::Remove { peer, profile } => {
            let profile = Profile::open(&profile.name)?;
            let mut trust = profile.trust()?;
            if !trust.remove(&peer) {
                bail!("no allowlisted peer {peer} in profile '{}'", profile.name);
            }
            trust.save(&profile.trust_path())?;
            println!("profile '{}': removed {peer}", profile.name);
        }
    }
    Ok(())
}

async fn run_auth(command: AuthCommand) -> Result<()> {
    match command {
        AuthCommand::Login {
            target,
            tenant,
            client_id,
            device_code,
            cli,
            account,
            cli_path,
            profile,
            gh_user,
            host,
            token,
            api_key,
            bridge,
        } => {
            let Some(provider) = target.provider() else {
                if cli {
                    let mut profile = Profile::open(&profile.name)?;
                    profile.check_can_sign_in()?;
                    let mut login = proofkin::workiq::login::CliLogin::new(account.as_deref());
                    if let Some(path) = cli_path {
                        login = login.with_cli_path(path);
                    }
                    let who = login.run(&mut profile).await?;
                    let at = match profile.config.workiq.as_ref().map(|a| &a.connection) {
                        Some(proofkin::profile::WorkIqConnection::Cli {
                            program: Some(program),
                            ..
                        }) => format!(" ({})", program.display()),
                        _ => String::new(),
                    };
                    println!(
                        "profile '{}': Work IQ signed in through the Work IQ CLI{at} as {who}",
                        profile.name
                    );
                    return Ok(());
                }
                let (Some(tenant), Some(client_id)) = (tenant, client_id) else {
                    bail!(
                        "Work IQ sign-in needs --tenant <id-or-domain> and --client-id <app-id>, \
                         or --cli"
                    );
                };
                let mut profile = Profile::open(&profile.name)?;
                profile.check_can_sign_in()?;
                let flow = if device_code {
                    proofkin::workiq::login::Flow::DeviceCode
                } else {
                    proofkin::workiq::login::Flow::Browser
                };
                let endpoints = proofkin::workiq::Endpoints::configured()?;
                let who = proofkin::workiq::login::Login::new(&tenant, &client_id)
                    .with_authority(&endpoints.authority)
                    .with_mcp_url(&endpoints.mcp_url)
                    .run(&mut profile, flow)
                    .await?;
                println!("profile '{}': Work IQ signed in as {who}", profile.name);
                return Ok(());
            };
            let method = match (gh_user, token, api_key) {
                (Some(user), _, _) => LoginMethod::GhUser { user, host },
                (None, true, _) => LoginMethod::Token { host },
                (None, false, true) => LoginMethod::ApiKey,
                (None, false, false) => LoginMethod::Browser,
            };
            let mut profile = Profile::open(&profile.name)?;
            profile.check_can_sign_in()?;
            let account = ProviderTools::new(&bridge.path())
                .login(&mut profile, provider, method)
                .await?;
            println!(
                "profile '{}': {provider} signed in as {account}",
                profile.name
            );
        }
        AuthCommand::Logout {
            target,
            profile,
            bridge,
        } => {
            let mut profile = Profile::open(&profile.name)?;
            profile.check_can_sign_in()?;
            match target.provider() {
                Some(provider) => {
                    ProviderTools::new(&bridge.path()).logout(&mut profile, provider)?;
                    println!("profile '{}': {provider} signed out", profile.name);
                }
                None => {
                    proofkin::workiq::account::logout(&mut profile)?;
                    println!("profile '{}': Work IQ signed out", profile.name);
                }
            }
        }
        AuthCommand::Status { profile, bridge } => {
            let names = match profile {
                Some(name) => vec![name],
                None => Profile::list()?,
            };
            if names.is_empty() {
                println!(
                    "no profiles yet; create one with: proofkin auth login copilot --profile <name> --gh-user <login>"
                );
            }
            let tools = ProviderTools::new(&bridge.path());
            for name in names {
                let profile = Profile::open(&name)?;
                println!("profile '{name}' ({})", profile.dir.display());
                if let Some(owner) = &profile.config.owner {
                    println!(
                        "  digital worker of '{owner}': runs on its accounts (see profile '{owner}'){}",
                        profile
                            .config
                            .role
                            .as_deref()
                            .map(|r| format!("; role: {r}"))
                            .unwrap_or_default()
                    );
                    println!(
                        "  workiq   {}",
                        proofkin::workiq::account::describe(&profile)
                    );
                    continue;
                }
                for provider in [Provider::Copilot, Provider::Claude, Provider::Codex] {
                    let state = if profile.is_configured(provider) {
                        match tools.check(&profile, provider).await {
                            Ok(account) => format!("signed in: {account}"),
                            Err(e) => format!("ERROR: {e:#}"),
                        }
                    } else {
                        format!("not signed in  ({})", profile.login_hint(provider))
                    };
                    println!("  {provider:<8} {state}");
                }
                println!(
                    "  workiq   {}",
                    proofkin::workiq::account::describe(&profile)
                );
            }
        }
    }
    Ok(())
}

async fn run_agent(args: RunArgs) -> Result<()> {
    let Some(name) = args.name.clone() else {
        use clap::CommandFactory;
        Cli::command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "--name <NAME> is required to run an agent",
            )
            .exit();
    };
    let profile = Profile::open(args.profile.as_deref().unwrap_or(&name))?;
    // One agent per profile: two would share one identity.
    let _instance = profile.lock_instance()?;
    let executor: Option<Arc<SdkExecutor>> = if args.providers.is_empty() {
        None
    } else {
        Some(
            SdkExecutor::start(ExecutorConfig {
                providers: args.providers.clone(),
                bridge_script: args.bridge.path(),
                profile: profile.clone(),
            })
            .await?,
        )
    };
    let local = TaskContext::new(args.access, std::path::absolute(&args.workspace)?);
    let shares = profile.shares()?;

    let (agent, mut events) = {
        if args.serve && executor.is_none() {
            bail!("--serve requires --providers");
        }
        if args.network == Network::Local && args.listen.is_none() {
            bail!("--network local needs --listen <IP:PORT> (a concrete IP peers can reach)");
        }
        let mut config = AgentConfig::new(name, profile.identity()?);
        config.network = args.network;
        config.bind = args.listen;
        config.trust = profile.trust()?;
        config.owner = profile.config.owner.clone();
        // A digital worker knows its owner by key: names on its allowlist can change.
        if profile.config.owner.is_some() {
            config.owner_id = Some(profile.account_profile()?.identity()?.public());
        }
        config.role = profile.config.role.clone();
        config.models = profile.models();
        config.worktrees_dir = profile.worktrees_dir();
        // Task ids go on after a restart (a plan run resumed waits for its old ones).
        config.task_ids = Some(profile.task_ids_path());
        // An owner's agent publishes what their factory shares with peers (a worker owns none).
        if profile.config.owner.is_none() {
            config.publisher = Some(proofkin::factory::catalog::ProfilePublisher::new(
                profile.clone(),
            ));
        }
        if let Some(secs) =
            proofkin::profile::env_setting("TASK_GRACE_SECS").and_then(|v| v.parse::<u64>().ok())
        {
            config.task_grace = Duration::from_secs(secs.max(3));
        }
        // Known for granting folders to peers; they're offered only while serving.
        config.shares = shares.clone();
        if args.serve {
            let data = profile.data_folder();
            std::fs::create_dir_all(&data)
                .with_context(|| format!("creating the shared data folder {}", data.display()))?;
            config.executor = executor.clone().map(|e| e as Arc<dyn Executor>);
            config.audit = Some(Arc::new(AuditLog::new(profile.audit_path())));
            config.team = Some(Arc::new(proofkin::team::TeamRunner::new(profile.clone())));
            config.memory = proofkin::factory::memory::WorkerMemory::of(&profile);
            // A deployed factory agent runs its pinned release, checked as it loads.
            config.deployed = proofkin::factory::deploy::Deployed::of(&profile, config.owner_id);
            config.pulls = Some(Arc::new(proofkin::pulls::ProfilePulls::new(
                profile.clone(),
            )));
            // This program serves a task's checks to its model (proofkin task-checks).
            config.task_checks = std::env::current_exe().ok();
            // And Work IQ to its owner's tasks that ask for a folder's `workiq` server.
            config.workiq_gateway = Some(proofkin::workiq::gateway::Gateway::new(profile.clone()));
        }
        Agent::bind(config).await?
    };
    agent.watch_shares(
        profile.shares_path(),
        profile.data_folder(),
        Duration::from_secs(2),
    );
    // What the factory publishes can change (the web UI, the command line): peers hear of it.
    agent.watch_catalog(Duration::from_secs(3));
    // Model settings changed while it runs (the CLI, the owner's) apply right away.
    agent.watch_models(
        profile.clone(),
        executor.clone().map(|e| e as Arc<dyn Executor>),
        Duration::from_secs(2),
    );
    // Peers pick from the models its accounts offer: listed once it runs.
    if let Some(executor) = executor.clone() {
        let agent = agent.clone();
        tokio::spawn(async move {
            for (provider, error) in agent.learn_offers(&*executor).await {
                eprintln!(
                    "could not list the models {provider} offers (peers can still name one): {error}"
                );
            }
        });
    }
    println!(
        "{} [profile '{}'] endpoint id {}",
        agent.name(),
        profile.name,
        agent.id()
    );
    if let Some(owner) = &profile.config.owner {
        println!(
            "digital worker of '{owner}' (runs on its accounts){}",
            profile
                .config
                .role
                .as_deref()
                .map(|r| format!("; role: {r}"))
                .unwrap_or_default()
        );
    }
    if let Some(deployed) = agent.deployed() {
        match deployed.current() {
            Ok(pinned) => println!(
                "deployed factory agent {} {} ({})",
                pinned.agent, pinned.version, pinned.state
            ),
            Err(why) => eprintln!("its release can't run, so it refuses every task: {why}"),
        }
        // Its owner pins another release (a rollback, a canary's verdict, budgets): taken
        // within seconds.
        deployed.watch(proofkin::factory::deploy::WATCH_EVERY);
    }
    if args.network == Network::Internet && !agent.online(Duration::from_secs(15)).await {
        eprintln!("warning: no relay reachable yet; peers behind NAT may not reach this agent");
    }
    let ticket = Ticket {
        name: profile.name.clone(),
        addr: agent.addr(),
    };
    println!("ticket: {}", ticket.encode());
    let allowed = profile.trust()?.peers.len();
    if allowed == 0 {
        println!(
            "no allowlisted peers yet: proofkin peers allow <their-ticket> --profile {}",
            profile.name
        );
    } else {
        println!("{allowed} allowlisted peer(s); connecting automatically");
    }
    if let Some(executor) = &executor {
        println!(
            "providers: {:?} (your tasks: {} in {})",
            executor.providers(),
            args.access,
            local.workspace.display()
        );
        for (provider, account) in executor.accounts() {
            println!("  {provider}: {account} [profile '{}']", profile.name);
        }
        if args.serve {
            println!("folders shared with peers (each peer sees only those you granted it):");
            for share in &shares.shares {
                println!(
                    "  {:<12} at most {:<16} {}{}",
                    share.name,
                    share.max_access.to_string(),
                    share.path.display(),
                    share_extras(share)
                );
            }
        }
    }

    start_monitors(
        &profile,
        &agent,
        executor.clone().map(|e| e as Arc<dyn Executor>),
    )?;

    for peer in &args.peers {
        let result = match Ticket::parse(peer) {
            Ok(ticket) => agent.connect(ticket.addr).await,
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            eprintln!("{e:#}");
        }
    }
    agent.keep_connected(Duration::from_secs(30));

    // Digital workers this profile owns: started by this agent, which stops them
    // when it stops (a digital worker can't own workers).
    let workers = match (profile.config.owner.is_none(), std::env::current_exe()) {
        (true, Ok(exe)) => Some(proofkin::workers::Supervisor::new(
            profile.clone(),
            agent.clone(),
            proofkin::workers::Launch {
                exe,
                network: args.network,
                ip: args.listen.map(|a| a.ip()),
                bridge: Some(args.bridge.path()),
            },
        )),
        _ => None,
    };
    if let Some(workers) = &workers {
        for (name, started) in workers.start_all() {
            match started {
                Ok(()) => println!("started digital worker {name}"),
                Err(e) => eprintln!("could not start digital worker {name}: {e:#}"),
            }
        }
    }

    if args.ui {
        let info = proofkin::ui::UiInfo {
            profile: profile.clone(),
            network: args.network,
            serving: args.serve,
            access: args.access,
            workspace: local.workspace.clone(),
            providers: executor.as_ref().map(|e| e.providers()).unwrap_or_default(),
            accounts: executor
                .as_ref()
                .map(|e| e.accounts().to_vec())
                .unwrap_or_default(),
            workers: workers.clone(),
        };
        let local = executor.clone().map(|e| e as Arc<dyn Executor>);
        let bind = SocketAddr::from(([127, 0, 0, 1], args.ui_port));
        let server = proofkin::ui::start(agent.clone(), events, info, local, bind).await?;
        println!("web UI: {}", server.url);
        println!("(the link includes a private access token; press Ctrl+C to stop the agent)");
        if !args.no_open {
            open_browser(&server.url);
        }
        tokio::signal::ctrl_c().await?;
        if let Some(workers) = &workers {
            workers.stop_all().await;
        }
        agent.shutdown().await;
        return Ok(());
    }
    start_schedules(&profile, &agent)?;
    // The agent factory's releases waiting for sign-offs, and deployments for their canary.
    if profile.config.owner.is_none() {
        let changed: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(|name: &str| {
            println!("agent factory: {name} changed (proofkin factory show {name})")
        });
        proofkin::factory::signoff::spawn(
            proofkin::factory::FactoryStore::of(&profile),
            agent.clone(),
            changed.clone(),
        );
        if let Some(workers) = &workers {
            let host = Arc::new(proofkin::factory::deploy::Host {
                owner: profile.clone(),
                agent: agent.clone(),
                key: profile.identity()?,
                supervisor: workers.clone(),
                ready_wait: proofkin::factory::runner::READY_WAIT,
            });
            tokio::spawn(proofkin::factory::manage::drive_canaries(
                host.clone(),
                changed.clone(),
            ));
            // Drift incidents reach the owner as monitors' do: here, and peers by chat.
            let (alerts, me) = (agent.clone(), agent.name().to_owned());
            let notify: proofkin::factory::drift::Notify =
                Arc::new(move |e: &proofkin::ops::IncidentEvent| {
                    let line = e.text.lines().next().unwrap_or_default();
                    println!("! [{}] {:?}: {line}", e.monitor, e.kind);
                    alerts.alert(format!(
                        "[{me} alert] {} {}: {line}",
                        e.monitor,
                        format!("{:?}", e.kind).to_lowercase()
                    ));
                });
            tokio::spawn(proofkin::factory::drift::drive(host, changed, notify));
        }
    }

    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                Event::PeerConnected { name, id } => println!("+ {name} ({})", id.fmt_short()),
                Event::PeerDisconnected { name, id } => println!("- {name} ({})", id.fmt_short()),
                Event::PeerRejected { id } => {
                    println!("! refused connection from {id} (not on the allowlist)")
                }
                Event::Chat { from, text } => println!("<{from}> {text}"),
                Event::PeerBusy { .. } | Event::PeerModels { .. } | Event::ModelsChanged => {}
                Event::Catalog { name, entries, .. } => {
                    if entries > 0 {
                        println!(
                            "catalog: {name} publishes {entries} agent release(s) to you (proofkin factory catalog)"
                        );
                    } else {
                        println!("catalog: {name} publishes nothing to you any more");
                    }
                }
                Event::TaskReceived {
                    from,
                    task_id,
                    provider,
                    origin,
                    team,
                } => {
                    let what = if team {
                        "team step".to_owned()
                    } else {
                        provider.to_string()
                    };
                    match origin
                        .as_ref()
                        .and_then(proofkin::protocol::Origin::describe)
                    {
                        Some(of) => {
                            println!("> task #{task_id} from {from} ({what}, its {of}) started")
                        }
                        None => println!("> task #{task_id} from {from} ({what}) started"),
                    }
                }
                Event::TeamRun {
                    from,
                    task_id,
                    run,
                    steps,
                } => println!(
                    "> task #{task_id} from {from} is a team step: team run {run} ({} step{})",
                    steps.len(),
                    if steps.len() == 1 { "" } else { "s" }
                ),
                Event::TeamStep {
                    task_id,
                    step,
                    state,
                    ..
                } => print_step(&format!("{step} (team run of task #{task_id})"), &state),
                Event::TaskCompleted {
                    from,
                    task_id,
                    outcome,
                    stop,
                } => {
                    let outcome = match outcome {
                        Outcome::Done => "done",
                        Outcome::Cancelled => "cancelled",
                        _ => "failed",
                    };
                    let outcome = stop.as_deref().unwrap_or(outcome);
                    println!("> task #{task_id} from {from} {outcome}")
                }
                Event::TaskRefused {
                    from,
                    task_id,
                    reason,
                } => println!("> task #{task_id} from {from} refused: {reason}"),
                Event::ApprovalRequested { id, from, title } => println!(
                    "? {from} asks you to approve: {title}\n  see it with /approvals, then /approve {id} [note] or /reject {id} <why> (or /changes {id} <what>, where /approvals offers it)"
                ),
                Event::ApprovalClosed { .. } => {}
                Event::NoticeFailed { title, error } => println!(
                    "! the Microsoft 365 notice of the sign-off \"{title}\" wasn't sent (it isn't sent again; the sign-off still waits): {error}"
                ),
            }
        }
    });

    let tasks = RunningTasks::default();
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = stdin.next_line().await? {
        let line = line.trim();
        let (command, rest) = line.split_once(' ').unwrap_or((line, ""));
        match command {
            "" => {}
            "/quit" => break,
            "/help" => println!("{COMMANDS}"),
            "/tasks" => {
                let running = tasks.list();
                if running.is_empty() {
                    println!("no running tasks");
                }
                for (n, label) in running {
                    println!("  {n}: {label}");
                }
            }
            "/cancel" => match rest.trim().parse::<u64>() {
                Ok(n) => match tasks.cancel(n) {
                    Some(label) => println!("=== {label} cancelled (task {n})"),
                    None => eprintln!("no running task {n}; see /tasks"),
                },
                Err(_) => eprintln!("usage: /cancel <n> (see /tasks)"),
            },
            "/ticket" => {
                println!(
                    "{}",
                    Ticket {
                        name: profile.name.clone(),
                        addr: agent.addr()
                    }
                    .encode()
                )
            }
            "/peers" => {
                for peer in agent.peers() {
                    let shares: Vec<String> = peer
                        .shares
                        .iter()
                        .map(|s| {
                            format!(
                                "{} ({}{})",
                                s.name,
                                s.access,
                                if s.mcp.is_empty() {
                                    String::new()
                                } else {
                                    format!("; MCP: {}", s.mcp.join(", "))
                                }
                            )
                        })
                        .collect();
                    println!(
                        "  {} ({}){} serves {:?}{}",
                        peer.name,
                        peer.id.fmt_short(),
                        peer_kind(&peer),
                        peer.capabilities,
                        if shares.is_empty() {
                            String::new()
                        } else {
                            format!(", folders shared with you: {}", shares.join(", "))
                        }
                    );
                }
            }
            "/squads" => {
                let squads = SquadStore::load(&profile.squads_path()).unwrap_or_default();
                if squads.squads.is_empty() {
                    println!(
                        "no squads; create one with: proofkin squad create <name> --member a,b --profile {}",
                        profile.name
                    );
                }
                let online: Vec<String> = agent.peers().into_iter().map(|p| p.name).collect();
                for squad in squads.squads {
                    let members: Vec<String> = squad
                        .members
                        .iter()
                        .map(|m| {
                            let state = if online.contains(m) {
                                "online"
                            } else {
                                "offline"
                            };
                            format!("{m} ({state})")
                        })
                        .collect();
                    println!("  #{}: {}", squad.name, members.join(", "));
                }
            }
            "/plan" => {
                // As the profile is now: a sign-in made while the agent runs (Work IQ,
                // GitHub) counts.
                let profile = profile.reload().unwrap_or_else(|_| profile.clone());
                let file = rest.trim();
                let text = std::fs::read_to_string(file)
                    .with_context(|| format!("reading plan file {file:?}"));
                let kb = profile.knowledge();
                let skills = profile.skills();
                let squads_now = SquadStore::load(&profile.squads_path()).unwrap_or_default();
                let recipients = proofkin::plan::Recipients::of(&agent, &profile);
                // A peer reconnecting (e.g. both agents dialed at once) is waited for.
                agent.settle_peers().await;
                let parsed = text.and_then(|text| {
                    let plan = Plan::parse(&text)?;
                    let problems = proofkin::plan::preflight(
                        &plan,
                        &agent.peers(),
                        &squads_now,
                        &kb,
                        &skills,
                        &recipients,
                    );
                    for p in &problems {
                        println!(
                            "  {} {}{}",
                            if p.error { "error:" } else { "warning:" },
                            p.step
                                .as_ref()
                                .map(|s| format!("step {s}: "))
                                .unwrap_or_default(),
                            p.message
                        );
                    }
                    if problems.iter().any(|p| p.error) {
                        bail!("the plan was not started: fix the errors above");
                    }
                    let ready = plan.prepare(&kb, &skills)?;
                    let work = proofkin::plan::work_context(&profile, &plan)?;
                    Ok((text, plan, ready, work))
                });
                match parsed {
                    Err(e) => eprintln!("{e:#}"),
                    Ok((text, plan, ready, work)) => {
                        let squads = SquadStore::load(&profile.squads_path()).unwrap_or_default();
                        let agent = agent.clone();
                        let runs_dir = profile.runs_dir();
                        // Known from the start: each step's peer, and a sign-off's notice,
                        // are told it.
                        let run_id = proofkin::runs::new_run_id();
                        let ctx = proofkin::plan::RunContext {
                            run_id: Some(run_id.clone()),
                            recipients,
                            work_context: work,
                            notices: proofkin::plan::notices(&profile, &plan),
                            ..Default::default()
                        };
                        let n = tasks.spawn(
                            format!("plan {file} ({} steps)", plan.steps.len()),
                            async move {
                                let started = proofkin::runs::unix_now();
                                let states =
                                    run_plan_with(&agent, &squads, &ready, &ctx, print_step).await;
                                let mut run = proofkin::runs::RunRecord::new(
                                    agent.name(),
                                    &agent.id().to_string(),
                                    &text,
                                    &plan,
                                    &states,
                                    started,
                                );
                                run.id = run_id;
                                run.peer_ids = proofkin::runs::peer_ids(&agent);
                                run.workiq = ctx.work_log.records();
                                println!("=== plan finished: {} ===", run.verdict_line());
                                for claim in run.claim_results() {
                                    println!(
                                        "  {} {}: {}",
                                        claim.verdict.label(),
                                        claim.claim.id,
                                        claim.claim.text
                                    );
                                }
                                match run.save(&runs_dir) {
                                    Ok(report) => println!("  report: {}", report.display()),
                                    Err(e) => eprintln!("could not save the run: {e:#}"),
                                }
                                let saved = proofkin::plan::save_knowledge(
                                    &kb,
                                    &proofkin::plan::with_actual_labels(&plan, &ctx.work_log),
                                    &states,
                                    &run.id,
                                );
                                if !saved.is_empty() {
                                    println!("  knowledge saved: {}", saved.join(", "));
                                }
                            },
                        );
                        println!("running plan {file} as task {n} (/cancel {n} to stop it)");
                    }
                }
            }
            "/runs" => match proofkin::runs::RunRecord::list(&profile.runs_dir()) {
                Ok(runs) if runs.is_empty() => println!("no evidence runs yet"),
                Ok(runs) => {
                    for run in runs.iter().take(10) {
                        println!(
                            "  {}  {}  {}/{} claims proven",
                            run.id,
                            if run.passed { "PASS" } else { "FAIL" },
                            run.proven,
                            run.claims
                        );
                    }
                }
                Err(e) => eprintln!("{e:#}"),
            },
            "/approvals" => {
                let open = agent.approvals();
                if open.is_empty() {
                    println!("no approval requests");
                }
                for a in open {
                    println!(
                        "=== #{} from {}: {} ===\n{}\n{}=== /approve {} [note]{} or /reject {} <why> ===",
                        a.id,
                        a.from,
                        a.title,
                        a.text,
                        match a.preview {
                            Some("prototype") => {
                                "(it comes with a UX prototype: open it from this request in the web UI, --ui)\n"
                            }
                            Some(_) => {
                                "(it comes with a review pack, decks and design documents: open it from this request in the web UI, --ui)\n"
                            }
                            None => "",
                        },
                        a.id,
                        if a.changes {
                            format!(", /changes {} <what to change>", a.id)
                        } else {
                            String::new()
                        },
                        a.id
                    );
                }
            }
            "/approve" | "/reject" | "/changes" => {
                use proofkin::agent::Answer;
                let (n, note) = rest.trim().split_once(' ').unwrap_or((rest.trim(), ""));
                let answer = match command {
                    "/approve" => Answer::Approve,
                    "/changes" => Answer::Changes,
                    _ => Answer::Reject,
                };
                match n.parse::<u64>() {
                    Err(_) => eprintln!("usage: {command} <n> [note] (see /approvals)"),
                    Ok(_) if answer == Answer::Reject && note.trim().is_empty() => {
                        eprintln!("say why: /reject {n} <why>")
                    }
                    Ok(_) if answer == Answer::Changes && note.trim().is_empty() => {
                        eprintln!("say what to change: /changes {n} <what>")
                    }
                    Ok(id) => match agent.answer(id, answer, note).await {
                        Ok(()) => match answer {
                            Answer::Approve => println!("approved request {id}"),
                            Answer::Reject => println!("rejected request {id}"),
                            Answer::Changes => println!(
                                "asked for changes on request {id}: the work is done again, then you are asked again"
                            ),
                        },
                        Err(e) => eprintln!("{e:#}"),
                    },
                }
            }
            "/ask" => match (parse_task(rest), &executor) {
                (Err(e), _) => eprintln!("{e:#}"),
                (Ok(_), None) => eprintln!("no local providers; start with --providers"),
                (Ok((task, _)), Some(executor)) => {
                    let executor = executor.clone();
                    let label = format!("{} (local)", task.provider);
                    let ctx = local.clone();
                    tasks.spawn(label.clone(), async move {
                        print_result(&label, executor.execute(&task, &ctx).await);
                    });
                }
            },
            "/delegate" => {
                let squads = SquadStore::load(&profile.squads_path()).unwrap_or_default();
                match parse_task(rest)
                    .and_then(|(task, peer)| pick_peer(&agent, &squads, &task, peer))
                {
                    Err(e) => eprintln!("{e:#}"),
                    Ok((task, peer, squad)) => {
                        let agent = agent.clone();
                        let to = squad
                            .as_ref()
                            .map_or(peer.name.clone(), |s| format!("#{s}"));
                        let label = format!("{} on {to}", task.provider);
                        let note = if squad.is_some() {
                            format!(", the least busy member of {to}")
                        } else {
                            String::new()
                        };
                        let (provider, name, short) =
                            (task.provider, peer.name.clone(), peer.id.fmt_short());
                        let n = tasks.spawn(label.clone(), async move {
                            let (ran_on, result) =
                                delegate_cli(&agent, &squads, squad.as_deref(), peer, task).await;
                            let label = match ran_on {
                                Some(member) => format!("{provider} on {member} ({to})"),
                                None => label,
                            };
                            print_result(&label, result);
                        });
                        println!(
                            "delegating to {name} ({short}){note} as task {n} (/cancel {n} to stop it)"
                        );
                    }
                }
            }
            _ if command.starts_with('/') => eprintln!("unknown command {command}; try /help"),
            _ => {
                agent.broadcast(line);
            }
        }
    }
    if let Some(workers) = &workers {
        workers.stop_all().await;
    }
    agent.shutdown().await;
    Ok(())
}

/// Parses `<provider>[@peer[:folder]] [--model <name>] [--effort <level>] <prompt>`.
fn parse_task(rest: &str) -> Result<(LlmTask, Option<String>)> {
    const USAGE: &str =
        "usage: <provider>[@peer[:folder]] [--model <name>] [--effort <level>] <prompt>";
    let (target, mut prompt) = rest.trim().split_once(' ').context(USAGE)?;
    let (provider, peer) = match target.split_once('@') {
        Some((provider, peer)) => (provider, Some(peer)),
        None => (target, None),
    };
    let (peer, share) = match peer.map(|p| p.split_once(':').unwrap_or((p, ""))) {
        Some((peer, share)) => (Some(peer.to_owned()), Some(share).filter(|s| !s.is_empty())),
        None => (None, None),
    };
    let provider = Provider::from_str(provider, true).map_err(|e| anyhow!("{e}"))?;
    // `--model <name>` and `--effort <level>` right after the target pick them.
    let (mut model, mut effort) = (None, None);
    loop {
        let start = prompt.trim_start();
        let (flag, after) = match (
            start.strip_prefix("--model"),
            start.strip_prefix("--effort"),
        ) {
            (Some(after), _) => ("--model", after),
            (_, Some(after)) => ("--effort", after),
            _ => break,
        };
        if !after.starts_with(char::is_whitespace) {
            bail!("{USAGE}");
        }
        let (value, rest) = after.trim_start().split_once(' ').context(USAGE)?;
        if flag == "--model" {
            if model.is_some() {
                bail!("--model is given twice");
            }
            proofkin::models::check_name(value)?;
            model = Some(value.to_owned());
        } else {
            if effort.is_some() {
                bail!("--effort is given twice");
            }
            proofkin::models::check_effort(value)?;
            effort = Some(value.to_ascii_lowercase());
        }
        prompt = rest;
    }
    if prompt.trim().is_empty() {
        bail!("{USAGE}");
    }
    let mut task = LlmTask::new(provider, prompt.trim());
    task.model = model;
    task.effort = effort;
    task.title = Some(proofkin::llm::title_of(prompt.trim()));
    if let Some(share) = share {
        task = task.in_share(share);
    }
    Ok((task, peer))
}

/// The peer `/delegate` gives `task` to: the named one, the least busy member that can
/// run it of a squad named `#squad` (and the squad's name), or the first that serves
/// its provider.
fn pick_peer(
    agent: &Agent,
    squads: &SquadStore,
    task: &LlmTask,
    name: Option<String>,
) -> Result<(LlmTask, proofkin::PeerInfo, Option<String>)> {
    if let Some(squad) = name.as_deref().and_then(|n| n.strip_prefix('#')) {
        let peer = proofkin::plan::squad_member(agent, squads, squad, task, &Default::default())?;
        return Ok((task.clone(), peer, Some(squad.to_owned())));
    }
    let peer = match name {
        Some(name) => agent
            .peers()
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| anyhow!("no peer named {name}"))?,
        None => agent
            .find_peer(task.provider)
            .ok_or_else(|| anyhow!("no peer serves {}", task.provider))?,
    };
    Ok((task.clone(), peer, None))
}

/// Delegates `task` to `peer`. A squad's task moves on to another member while one
/// turns it down or goes away before running it, as a plan step on the squad does;
/// then it says which member ran it.
async fn delegate_cli(
    agent: &Agent,
    squads: &SquadStore,
    squad: Option<&str>,
    mut peer: proofkin::PeerInfo,
    task: LlmTask,
) -> (Option<String>, Result<proofkin::Ran>) {
    let mut tried = std::collections::HashSet::new();
    loop {
        let delivered = agent.delegate_verified(peer.id, task.clone()).await;
        if let Some(squad) = squad
            && let Some(why) = proofkin::plan::movable(&delivered)
        {
            tried.insert(peer.name.clone());
            if let Ok(next) = proofkin::plan::squad_member(agent, squads, squad, &task, &tried) {
                println!("task moves from {} to {}: {why}", peer.name, next.name);
                peer = next;
                continue;
            }
        }
        let result = delivered.and_then(|d| {
            if d.ok {
                Ok(proofkin::Ran::new(d.output, d.model))
            } else {
                Err(anyhow!("{}", d.output))
            }
        });
        return (squad.map(|_| peer.name), result);
    }
}

/// Last task number, and running tasks by number with their label and abort handle.
type TaskTable = (
    u64,
    std::collections::BTreeMap<u64, (String, tokio::task::AbortHandle)>,
);

/// Tasks started from the prompt, numbered so `/cancel <n>` can stop them.
#[derive(Clone, Default)]
struct RunningTasks {
    inner: Arc<std::sync::Mutex<TaskTable>>,
}

impl RunningTasks {
    fn spawn(
        &self,
        label: String,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> u64 {
        let mut guard = self.inner.lock().unwrap();
        guard.0 += 1;
        let n = guard.0;
        let this = self.clone();
        let handle = tokio::spawn(async move {
            task.await;
            this.inner.lock().unwrap().1.remove(&n);
        });
        guard.1.insert(n, (label, handle.abort_handle()));
        n
    }

    fn list(&self) -> Vec<(u64, String)> {
        let guard = self.inner.lock().unwrap();
        guard
            .1
            .iter()
            .map(|(n, (label, _))| (*n, label.clone()))
            .collect()
    }

    /// Aborts task `n`; dropping a delegation tells the peer to stop it.
    fn cancel(&self, n: u64) -> Option<String> {
        let (label, handle) = self.inner.lock().unwrap().1.remove(&n)?;
        handle.abort();
        Some(label)
    }
}

/// ` [digital worker of alice: role]`-style suffix for a peer.
fn peer_kind(peer: &proofkin::PeerInfo) -> String {
    match (&peer.owner, &peer.role) {
        (Some(owner), Some(role)) => format!(" [digital worker of {owner}: {role}]"),
        (Some(owner), None) => format!(" [digital worker of {owner}]"),
        (None, Some(role)) => format!(" [{role}]"),
        (None, None) => String::new(),
    }
}

fn print_step(id: &str, state: &StepState) {
    match state {
        StepState::Waiting => println!("  step {id}: waiting"),
        StepState::Running {
            peer,
            progress: Some(progress),
        } => println!("  step {id}: {peer}: {}", progress.text),
        StepState::Running { peer, .. } => println!("  step {id}: running on {peer}"),
        StepState::Done {
            peer,
            output,
            evidence,
            verified,
            model,
            ..
        } => {
            println!(
                "=== step {id} on {peer}{} ===\n{output}\n=== end ===",
                model
                    .as_deref()
                    .map(|m| format!(" · {m}"))
                    .unwrap_or_default()
            );
            print_evidence(evidence.as_ref(), *verified);
        }
        StepState::Failed {
            peer,
            error,
            evidence,
            verified,
            model,
        } => {
            println!(
                "=== step {id}{}{} failed: {error}",
                peer.as_deref()
                    .map(|p| format!(" on {p}"))
                    .unwrap_or_default(),
                model
                    .as_deref()
                    .map(|m| format!(" · {m}"))
                    .unwrap_or_default()
            );
            print_evidence(evidence.as_ref(), *verified);
        }
        StepState::Skipped { reason } => println!("  step {id}: skipped ({reason})"),
    }
}

fn print_evidence(evidence: Option<&proofkin::evidence::Evidence>, verified: bool) {
    if let Some(e) = evidence {
        println!(
            "  evidence: signed by {} ({}) {}",
            e.signer,
            e.signer_id.chars().take(10).collect::<String>(),
            if verified {
                "✓ verified"
            } else {
                "✗ NOT valid"
            }
        );
    }
}

fn print_result(label: &str, result: Result<proofkin::Ran>) {
    match result {
        Ok(ran) => println!(
            "=== {label}{} ===\n{}\n=== end ===",
            ran.shown_model()
                .map(|m| format!(" · {m}"))
                .unwrap_or_default(),
            ran.output
        ),
        Err(e) => eprintln!("=== {label} failed: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn delegated_tasks_may_name_their_model_right_after_the_target() {
        let (task, peer) = parse_task("copilot@worker:repo --model gpt-5.5 Fix the build").unwrap();
        assert_eq!(
            (
                task.model.as_deref(),
                peer.as_deref(),
                task.share.as_deref()
            ),
            (Some("gpt-5.5"), Some("worker"), Some("repo"))
        );
        assert_eq!(task.prompt, "Fix the build");
        let (plain, _) = parse_task("claude Explain --model in the README").unwrap();
        assert_eq!(
            (plain.model, plain.prompt.as_str()),
            (None, "Explain --model in the README")
        );
        for bad in [
            "copilot --model",
            "copilot --model gpt-5.5",
            "copilot --model `x` Do it",
            "copilot --modelx Do it",
            "copilot --effort turbo Do it",
            "copilot --effort max --effort low Do it",
            "copilot --effort max",
        ] {
            assert!(parse_task(bad).is_err(), "{bad}");
        }
        // --effort goes with --model, in either order.
        let (task, _) = parse_task("copilot@worker --effort MAX --model gpt-5.5 Fix it").unwrap();
        assert_eq!(
            (
                task.model.as_deref(),
                task.effort.as_deref(),
                task.prompt.as_str()
            ),
            (Some("gpt-5.5"), Some("max"), "Fix it")
        );
        let (task, _) = parse_task("codex --effort low Fix it").unwrap();
        assert_eq!((task.model, task.effort.as_deref()), (None, Some("low")));
    }

    #[test]
    fn runs_verify_takes_a_folder_alone_or_an_id_with_a_profile() {
        let parse = |args: &[&str]| Cli::try_parse_from([&["proofkin"], args].concat()).is_ok();
        assert!(parse(&["runs", "verify", "--dir", "some/run"]));
        assert!(parse(&[
            "runs",
            "verify",
            "20261115-1",
            "--profile",
            "boss"
        ]));
        assert!(!parse(&["runs", "verify", "20261115-1"]));
        assert!(!parse(&["runs", "verify"]));
    }

    #[test]
    fn all_checks_include_the_rehearsal_only_once_there_is_one() {
        let dir = tempfile::tempdir().unwrap();
        for folder in ["design", "review"] {
            std::fs::create_dir(dir.path().join(folder)).unwrap();
        }
        let all = spec_checks("all", dir.path()).unwrap();
        assert!(all.contains(&"review:decks") && !all.contains(&"review:rehearsal"));
        std::fs::write(dir.path().join("review/rehearsal.md"), "# Rehearsal\n").unwrap();
        assert!(
            spec_checks("all", dir.path())
                .unwrap()
                .contains(&"review:rehearsal")
        );
        assert_eq!(
            spec_checks("rehearsal", dir.path()).unwrap(),
            ["review:rehearsal"]
        );
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(11_016), "2000-02-29");
        assert_eq!(civil_date(20_772), "2026-11-15");
    }

    #[test]
    fn models_allow_takes_the_models_the_efforts_or_both() {
        let allow = |args: &[&str]| {
            let args = [
                &["proofkin", "models", "allow", "copilot"],
                args,
                &["--profile", "w"],
            ]
            .concat();
            match Cli::try_parse_from(args) {
                Ok(Cli {
                    command:
                        Some(Command::Models(ModelsCommand::Allow {
                            models, efforts, ..
                        })),
                    ..
                }) => Some((models, efforts)),
                _ => None,
            }
        };
        let list = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            allow(&["gpt-5.5,claude-opus-5.5"]),
            Some((list(&["gpt-5.5", "claude-opus-5.5"]), None))
        );
        assert_eq!(
            allow(&["--efforts", "low,medium"]),
            Some((Vec::new(), Some(list(&["low", "medium"]))))
        );
        assert_eq!(
            allow(&["any", "--efforts", "any"]),
            Some((list(&["any"]), Some(list(&["any"]))))
        );
        assert_eq!(allow(&[]), None, "models, efforts or both");
    }
}
