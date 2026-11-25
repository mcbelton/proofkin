# Proofkin

**Peer-to-peer agents that prove their work.** (Code name: *Murmuration*.)

Peer-to-peer agents in Rust that run and delegate LLM coding tasks **over the internet**.
Agents connect with [iroh](https://iroh.computer) (encrypted QUIC, NAT hole punching, relay
fallback), are addressed by public key instead of IP, only talk to peers on their
allowlist, discover each other by gossip, and advertise which LLM providers they serve.
Any agent can run a task locally or delegate it to a peer:

| Provider | SDK | How it is used |
| --- | --- | --- |
| `copilot` | [GitHub Copilot SDK for Rust](https://github.com/github/copilot-sdk/tree/main/rust) (`github-copilot-sdk` crate, official) | Native Rust, drives the Copilot CLI over JSON-RPC |
| `claude` | [Claude Agent SDK](https://github.com/anthropics/claude-agent-sdk-typescript) (`@anthropic-ai/claude-agent-sdk`, official) | Via the Node bridge in [bridge/](bridge/) |
| `codex` | [OpenAI Codex SDK](https://github.com/openai/codex/tree/main/sdk/typescript) (`@openai/codex-sdk`, official) | Via the Node bridge in [bridge/](bridge/) |

Claude and Codex publish official SDKs only for TypeScript and Python, so they are reached
through a small Node process ([bridge/index.mjs](bridge/index.mjs)) instead of unofficial
Rust ports.

**Every agent has its own provider instances and accounts.** An agent runs under a
*profile* (default: its `--name`) with private Copilot, Claude and Codex homes and its own
credentials, so different agents can use different GitHub, Anthropic and OpenAI accounts.
An agent never uses the machine's global Copilot/Claude/Codex sign-in. See
[Accounts and profiles](#accounts-and-profiles) and [Internet P2P](#internet-p2p).

**Microsoft 365 work context (preview).** With a [Work IQ](docs/work-iq.md) sign-in
(`proofkin auth login workiq`), a plan step can start from cited results from your mail,
meetings, files and Teams (`workiq=`). A step can also ask Microsoft 365 Copilot or one of
your organization's agents (`@workiq`), and `proofkin kb import workiq` keeps what it finds
as knowledge. Work data stays with you: a peer gets it only with your grant and within the
sensitivity labels you choose, and every call is audited. See
[Microsoft 365 work context (Work IQ)](#microsoft-365-work-context-work-iq) for setup, your
admin's steps and a first plan.

## Install (no source code or Rust needed)

Prebuilt binaries for Windows (x64, ARM64), Linux (x64, ARM64) and macOS (Apple Silicon) are published as
[GitHub releases](https://github.com/mcbelton/proofkin/releases). Everything an agent
needs at runtime is in the one `proofkin` executable, except the provider CLIs below.

**1. Install Proofkin**

```powershell
# Windows (PowerShell)
irm https://github.com/mcbelton/proofkin/releases/latest/download/install.ps1 | iex
```

```sh
# Linux / macOS
curl -fsSL https://github.com/mcbelton/proofkin/releases/latest/download/install.sh | sh
```

The installer puts `proofkin` in `%LOCALAPPDATA%\proofkin\bin` (Windows) or
`~/.local/bin`, and adds it to `PATH`; open a new terminal afterwards. You can also
download the archive for your platform from the
[releases page](https://github.com/mcbelton/proofkin/releases) and put `proofkin`
anywhere on `PATH`. To install a specific version on Windows, download `install.ps1` and
run `.\install.ps1 -Version v0.1.2` (Linux/macOS: `PROOFKIN_VERSION=v0.1.2 sh install.sh`).

> **Forks or private copies:** for a private repo, the installers download through the
> GitHub CLI. If `gh` has several accounts, pick one with access:
> `.\install.ps1 -Repo <owner>/<repo> -GhUser <login>` (Linux/macOS:
> `PROOFKIN_REPO=<owner>/<repo> PROOFKIN_GH_USER=<login> sh install.sh`). GitHub shows
> private repos as "not found" to accounts without access.

**2. Install what your agent uses**

| You want to | Install |
| --- | --- |
| Run Copilot tasks | Copilot CLI: `winget install GitHub.Copilot` (or `npm install -g @github/copilot`) and a GitHub account with Copilot |
| Sign agents in to GitHub accounts easily | GitHub CLI: `winget install GitHub.cli`, then `gh auth login` per account |
| Run Claude or Codex tasks | Node.js 18+, then `proofkin setup` (installs the Claude Agent SDK and Codex SDK into `~/.proofkin/bridge`; add `--registry <mirror>` if public npm is blocked) |
| Only delegate tasks to others | Nothing else |

**3. Check, sign in, connect**

```powershell
proofkin doctor                                         # checks CLIs (Copilot's protocol too), bridge, internet relay, profiles
proofkin auth login copilot --profile me --gh-user <github-login>
proofkin peers id --profile me                          # send your ticket to your peers
proofkin peers allow <their-ticket> --profile me [--delegate]
proofkin --name me --providers copilot --serve
proofkin --name me --providers copilot --serve --ui     # same, in a browser UI (light/dark)
```

See [Internet P2P](#internet-p2p) for how peers find each other, [Web UI](#web-ui) for the browser app, and
[Accounts and profiles](#accounts-and-profiles) for sign-in options. Building from source
is described in the rest of this README.

> **Renamed from p2pagents.** The program is now `proofkin`, and the repository moved from
> `mcbelton/p2pagents` to `mcbelton/proofkin` (old links, clones and installers are
> redirected). Installs from before keep working as they are: while there is no
> `~/.proofkin`, profiles stay in `~/.p2pagents`, the data, repository and worktree
> folders keep their old names (`.p2pdata`, ...), every `P2PAGENTS_*` setting still
> applies when its `PROOFKIN_*` name is unset, and secrets stored under `p2pagents` in the
> OS credential store are read and copied forward, so identities (endpoint ids),
> allowlists and sign-ins stay the same. The wire protocol is unchanged too (tickets
> still start with `p2pa1`), so peers on either version work together. Remove the old
> `p2pagents` executable when you like (`%LOCALAPPDATA%\p2pagents\bin` on Windows).

## Quick start (Windows, after setup)

Get the source once, then run these from its folder in any PowerShell window. The script
reloads `PATH` itself, so it also works in terminals opened before Rust was installed:

```powershell
git clone https://github.com/mcbelton/proofkin   # once
cd proofkin
.\scripts\p2p.ps1 doctor      # checks every prerequisite, prints the fix for anything missing
.\scripts\p2p.ps1 test        # automated tests (no LLM sign-in needed)
.\scripts\p2p.ps1 demo        # worker + boss windows, allowlisted to each other, connected over the internet
.\scripts\p2p.ps1 live-test   # 4 agents over the internet, 2 accounts, allowlist + real Copilot delegation (needs Python)
.\scripts\p2p.ps1 evidence    # every claim in evidence/claims.json checked against its tests (add -Live for the internet test)
```

In the **boss** window wait for `+ worker`, then type `/peers` and
`/delegate copilot@worker:repo Summarize README.md in 3 bullets` (`repo` is this folder,
which the demo shares read-only with boss; see [Shared folders](#shared-folders)).
The answer is printed in the boss window, and the worker window shows the task starting and
finishing. Type `/quit` in each window to exit.

`demo` signs the `worker` profile in to Copilot with your first GitHub CLI account and
allowlists `worker` and `boss` to each other. To pick the account yourself:
`.\scripts\p2p.ps1 agent auth login copilot --profile worker --gh-user <login>`.
To run any Proofkin command through the script: `.\scripts\p2p.ps1 agent <args>`, e.g.
`.\scripts\p2p.ps1 agent auth status`.
If PowerShell refuses to run the script, use
`powershell -ExecutionPolicy Bypass -File .\scripts\p2p.ps1 doctor`.

## Requirements

| Tool | Version | Needed for |
| --- | --- | --- |
| Rust toolchain (`rustc`, `cargo`) via rustup | **1.94+** (Copilot SDK minimum); tested on 1.100 | Build, run, test |
| `clippy`, `rustfmt` rustup components | matches toolchain | Lint and format |
| Linker toolchain: MinGW-w64 **or** MSVC Build Tools (Windows only) | any recent | Linking; the GNU target also needs `dlltool` |
| Node.js + npm | 18+; tested on 24 | `claude` and `codex` providers (bridge), bridge tests |
| GitHub Copilot CLI + a Copilot plan | recent; tested on 1.0.102 | `copilot` provider (each agent runs its own instance) |
| GitHub CLI (`gh`) | 2.40+ | Recommended: easiest way to give agents different GitHub accounts |
| `curl` | any (built into Windows 10+) | Shows which GitHub login a Copilot token belongs to |
| A Claude account or Anthropic API key per profile | | `claude` provider |
| A ChatGPT account or OpenAI API key per profile | | `codex` provider |
| Git | any | Source control |
| Python 3 | any | Only for `.\scripts\p2p.ps1 live-test` |
| Internet access: outbound UDP, or at least HTTPS (443) | | Internet mode: direct QUIC when UDP works, otherwise via the iroh relays over HTTPS |

You only need the tools and credentials for the providers you enable. Rust crates
(`tokio`, `serde`, `clap`, `github-copilot-sdk`, ...) are downloaded by Cargo, and the
Claude and Codex CLIs are bundled inside their npm packages.

## Setup on Windows

### 1. Rust toolchain

Pick **one** option. Option A is lighter (no Visual Studio) and is what this project was
developed with.

#### Option A: GNU toolchain + MinGW-w64 (recommended, no Visual Studio)

1. Install Rust with the GNU host as default (PowerShell):

   ```powershell
   Invoke-WebRequest https://win.rustup.rs/x86_64 -OutFile "$env:TEMP\rustup-init.exe"
   & "$env:TEMP\rustup-init.exe" -y --default-host x86_64-pc-windows-gnu --default-toolchain stable --profile minimal
   Remove-Item "$env:TEMP\rustup-init.exe"
   ```

   This installs to `%USERPROFILE%\.cargo\bin` and adds it to your user `PATH`.

2. Add lint/format components:

   ```powershell
   rustup component add clippy rustfmt
   ```

3. Install MinGW-w64 (provides `dlltool`, `gcc`, `ld`):

   ```powershell
   winget install --id BrechtSanders.WinLibs.POSIX.UCRT -e
   ```

   winget adds `...\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_*\mingw64\bin` to your
   user `PATH`.

#### Option B: MSVC toolchain

1. Install [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
   with the **Desktop development with C++** workload (MSVC + Windows SDK):

   ```powershell
   winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
   ```

2. Install Rust with the default (MSVC) host, then add components:

   ```powershell
   winget install --id Rustlang.Rustup -e
   rustup default stable-x86_64-pc-windows-msvc
   rustup component add clippy rustfmt
   ```

### 2. Node.js (for `claude` / `codex`)

```powershell
winget install --id OpenJS.NodeJS.LTS -e
```

### 3. GitHub Copilot CLI and GitHub CLI (for `copilot`)

```powershell
winget install --id GitHub.Copilot -e
winget install --id GitHub.cli -e
gh auth login                       # once per GitHub account you want agents to use
gh auth login --hostname mycorp.ghe.com   # e.g. a GitHub Enterprise Cloud account
```

You don't need to sign in to the `copilot` CLI itself; each agent signs in its own instance
(see [Accounts and profiles](#accounts-and-profiles)). Proofkin finds `copilot` on `PATH`;
set `COPILOT_CLI_PATH` to use a different binary.

### 4. Open a new terminal

Open a new terminal (or restart VS Code) so the updated `PATH` is picked up.

## Setup on Linux / macOS

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup component add clippy rustfmt
```

You also need a C linker (`build-essential` on Debian/Ubuntu, `xcode-select --install` on
macOS), Node.js 18+, and the Copilot CLI (`npm install -g @github/copilot` or your package
manager). On Linux the Copilot SDK also needs OpenSSL 3 development files (`libssl-dev`).

## Install the bridge (Claude and Codex SDKs)

```powershell
cd bridge
npm install
```

(From a source checkout the bridge is used in place. Installed binaries use
`proofkin setup` instead, which writes the same bridge to `~/.proofkin/bridge`.)

## Releasing

Push a version tag; GitHub Actions ([.github/workflows/release.yml](.github/workflows/release.yml))
builds Windows, Linux and macOS binaries and publishes them with the install scripts:

```powershell
# bump version in Cargo.toml first
git tag v0.1.1; git push origin v0.1.1
```

[.github/workflows/ci.yml](.github/workflows/ci.yml) runs fmt, clippy and the tests on every push.

## Accounts and profiles

Each agent runs under a **profile**: `--profile <name>`, default the agent's `--name`. A
profile is a private identity:

```text
%USERPROFILE%\.proofkin\profiles\<profile>\     (or $PROOFKIN_HOME/profiles/<profile>)
  profile.json   how each provider is signed in (no secrets)
  copilot\       COPILOT_HOME for this agent's own Copilot CLI instance
  claude\        CLAUDE_CONFIG_DIR for this agent's own Claude instance (+ its login)
  codex\         CODEX_HOME for this agent's own Codex instance (+ auth.json)
  tasks.json     the web UI's latest 200 tasks, kept across restarts
  runs-in-progress\  the web UI's plan runs in progress, so they go on after a restart
  next-task-id   the next id for the tasks this agent gives peers (ids go on after a restart)
```

Tokens and API keys are stored in the OS credential store (Windows Credential Manager,
macOS Keychain, Linux Secret Service) under `proofkin` / `<profile>/<provider>`, next to
the profile's identity key (`<profile>/identity`). Where no OS credential store is
available (headless Linux, WSL, containers), or with `PROOFKIN_SECRET_STORE=file`, they
are kept in owner-only files (`0600`) in `<profile>\secrets\` instead; `proofkin doctor`
shows which store is used. The allowlist is `<profile>\peers.json`.

**Isolation guarantees**

- Each agent starts its **own** Copilot CLI process and its own Node bridge (which runs its
  own Claude and Codex processes), each pointed at the profile's private home.
- Copilot is started with the profile's token and `--no-auto-login`, so it can't fall back to
  the machine's signed-in Copilot/`gh` user or to `GH_TOKEN`/`GITHUB_TOKEN`.
- Inherited `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `CODEX_API_KEY`, `GH_TOKEN`,
  `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `COPILOT_HOME` and similar variables are removed from
  every provider process.
- An agent whose profile isn't signed in to an enabled provider **refuses to start** and
  prints the `auth login` command to run.
- At startup each agent prints the account each provider uses, e.g.
  `copilot: alice@github.com [profile 'worker1']`.
- Peers that delegate tasks never see any credentials; the agent doing the work uses its own.

### Sign in

| Provider | Command | What it does |
| --- | --- | --- |
| `copilot` | `proofkin auth login copilot --profile P --gh-user <login> [--host <ghe-host>]` | Uses that GitHub CLI account's token (fetched from `gh` each time the agent starts) |
| `copilot` | `proofkin auth login copilot --profile P --token [--host <ghe-host>]` | Prompts for a GitHub token (fine-grained PAT with *Copilot Requests*, or a `gho_` OAuth token) and stores it in the OS credential store |
| `claude` | `proofkin auth login claude --profile P` | Browser sign-in (`claude auth login`) into the profile's own Claude config dir |
| `claude` | `proofkin auth login claude --profile P --api-key` | Prompts for an Anthropic API key |
| `claude` | `proofkin auth login claude --profile P --token` | Prompts for a long-lived OAuth token from `claude setup-token` |
| `codex` | `proofkin auth login codex --profile P` | Browser sign-in (`codex login`) into the profile's own `CODEX_HOME` |
| `codex` | `proofkin auth login codex --profile P --api-key` | Prompts for an OpenAI API key (saved by `codex login --with-api-key`) |
| `workiq` | `proofkin auth login workiq --profile P --tenant <id-or-domain> --client-id <app-id>` | Microsoft 365 work context: signs in to Entra ID in your browser (authorization code with PKCE, redirected to a loopback port on localhost; the address is printed too) and keeps the refresh token in the OS credential store |
| `workiq` | `proofkin auth login workiq --profile P --tenant <id-or-domain> --client-id <app-id> --device-code` | Same, for machines without a browser: prints a web address and a code to enter on another device |
| `workiq` | `proofkin auth login workiq --profile P --cli [--account <email>] [--cli-path <path>]` | Same Work IQ, through Microsoft's Work IQ CLI instead (install it with `npm install -g @microsoft/workiq`). The sign-in finds the CLI: `--cli-path`, else `workiq` on the PATH, else in npm's global folder (`npm prefix -g`; a Node version manager such as nvm keeps that folder off the PATH), and saves where it is, so agents start the same one; only when there is none does Proofkin use `npx -y @microsoft/workiq`. It runs `workiq mcp` (with `--account <email>` when you give one; else the CLI's default account) and talks to it over stdio. The CLI signs in by itself with its own app, which your tenant's admin must have approved, and picks the tenant from the account, so Proofkin keeps no token. `auth status` names the account: the one you gave, else the CLI's default account (`workiq config show`). The CLI doesn't get the agent's provider credentials (`GH_TOKEN`, `ANTHROPIC_API_KEY` and so on). If the CLI says its EULA isn't accepted, run `workiq accept-eula` yourself; Proofkin never accepts it for you. The web UI's **Work IQ** card signs in through the Work IQ CLI too, with the same account and CLI path (see [Web UI](#web-ui)) |

Secrets are read from a hidden prompt, or from stdin when piped. Every login is verified
right away; if verification fails, that provider is signed out again.

Work IQ needs your tenant's admin to set it up first: the client app (a public client with
the delegated `WorkIQAgent.Ask` permission and admin consent, and public client flows on for
`--device-code`), billing and the Work IQ policy. Without that, sign-in fails and says what
to ask the admin for; see [docs/work-iq.md](docs/work-iq.md). The Work IQ sign-in is
checked right away against Work IQ (`tools/list`) and signed out again if that fails.

```powershell
proofkin auth status                    # every profile: which account each provider uses
proofkin auth status --profile worker1
proofkin auth logout codex --profile worker1
proofkin auth logout workiq --profile me   # removes the Work IQ account and its token
```

`auth status` also shows each profile's Work IQ account: user, tenant and connection.

### Models

Unless a task names a model, each provider runs on its own default (Copilot's is whatever
the CLI picks for the account). A profile's owner chooses, per provider, the **default
model**, **which models tasks may ask for** (one or more, or any the account offers), the
**reasoning effort** tasks work at and **which efforts they may ask for**. Peers then pick
only from those. In the web UI, **Models…** on the This agent card does it: the models the
account offers, by vendor, with their cost multiplier and the reasoning levels each
supports, to tick (Claude and Codex don't list theirs: name them), and the levels to tick.
Each digital worker has its own **Models…** in the Digital workers list. Or from the CLI:

```powershell
proofkin models list --profile worker1                     # Copilot lists the models the account may use, with their efforts
proofkin models allow copilot claude-sonnet-5.5,gpt-6-sol --profile worker1
proofkin models allow copilot --efforts low,medium,high --profile worker1   # only these reasoning levels
proofkin models default copilot claude-sonnet-5.5 --profile worker1
proofkin models default copilot claude-opus-5.5 --effort max --profile worker1
proofkin models effort copilot high --profile worker1      # minimal, low, medium, high, xhigh or max
proofkin models default copilot none --profile worker1     # back to the provider's own
proofkin models effort copilot none --profile worker1      # back to the model's own effort
proofkin models allow copilot any --efforts any --profile worker1   # any model and level again
```

A default must be one of the allowed models, and the default effort one of the allowed
efforts. With a list and no default, tasks that name none run on the first model in the
list (a provider's own default may not be allowed) and at the least effort in the list. A
task asking for another model or effort is refused, naming the ones allowed, so a squad
step can go to another member (squad steps, and *Any peer* tasks, that name a model or an
effort go first to members that allow them); Copilot sessions are also limited to the list.

Peers learn the settings, and the models the agent's account offers that they may ask
for (with the reasoning levels each supports), from its hello, and again whenever they
change: the settings are kept in `profile.json`, and a running agent applies a change
(from the web UI or the CLI) within seconds and tells its connected peers. A
[digital worker](#digital-workers) runs on its owner's settings for each provider it sets
nothing for, as it runs on the owner's accounts, and follows them when they change.

**Reasoning effort** is how hard the model thinks: `minimal`, `low`, `medium`, `high`,
`xhigh` or `max`. Copilot gets it per session, as each model supports it (`models list`
shows which): a model that supports fewer levels takes the nearest it supports below the
one asked for, and one that supports none runs at its own. Claude takes `low` to `max`
(`minimal` is `low`); Codex takes them all.

A task picks its model and effort with `model=` and `effort=` in a plan step (or its agent's
`model:`), `/ask` and `/delegate --model <name> --effort <level>`, the **Model** and
**Reasoning** fields of the web UI's Run task form and plan builder, or the `model` and
`effort` arguments of the MCP and A2A `delegate`. The web UI lists exactly what the chosen
peer lets you pick: the models it allows, or all its account offers when it allows any (by
vendor, with their cost multiplier; a name typed by hand only for a peer that lists none,
such as Claude, Codex or an older version), and the levels it allows that the chosen model
supports, its defaults selected (the default level as the model takes it). For *Any peer*
or a squad it lists what their peers allow together. The result names the model that did
the work and its effort, e.g. `claude-opus-5.5 (max reasoning)`, and so does the signed
output.

### Example: two workers on two accounts

```powershell
proofkin auth login copilot --profile worker1 --gh-user alice
proofkin auth login copilot --profile worker2 --gh-user alice-corp --host mycorp.ghe.com
proofkin auth login claude  --profile worker2 --api-key
# allowlist them to each other (see Internet P2P), then:
proofkin --name worker1 --providers copilot --serve
proofkin --name worker2 --providers copilot,claude --serve
```

Several agents can share one identity by passing the same `--profile`. Each still runs its
own provider processes.

`proofkin profile delete --profile P` removes a profile: its identity key, stored
credentials, allowlist and provider sign-ins (not while an agent runs with it; a profile
runs in one agent at a time).

### Digital workers

A **digital worker** is an autonomous agent with no identity of its own: it works on the
accounts of the person (profile) who created it, its **owner**.

```powershell
proofkin worker create tester --owner alice --role "test engineer for the parser" `
    --providers copilot --folder app --start-with-owner   # alice's agent runs it
proofkin --name tester --providers copilot --serve     # or run it yourself, on alice's Copilot sign-in
proofkin worker list --owner alice
proofkin worker remove tester                           # delete it
```

In alice's [web UI](#web-ui), **Digital workers** does the same: **New worker** names it,
says what it's for, picks what it runs on alice's sign-ins (and signs alice in to Copilot
with a GitHub CLI account if she isn't yet) and the folders of hers it works in. Each
worker shows whether it is online, starting, stopped or stopped on its own (with the end
of its log), with **Start**/**Stop**, **Starts with this agent** and **Delete**.

- The worker is its own peer: its own key, allowlist, [shared folders](#shared-folders),
  hourly limits and [audit log](#audit-log). Peers allowlist it like any agent, and alice
  decides who may delegate to it and which folders it shares.
- It has **no accounts**: Copilot, Claude and Codex run with the owner's sign-in, in the
  worker's own private provider homes. `auth login`/`logout` are refused for workers, and
  `auth status` shows whose accounts they use. A worker can't own another worker.
- Peers see `[digital worker of alice: test engineer for the parser]` next to it, and the
  role is given to the model with every task the worker runs.
- `worker create` allowlists the owner on the worker (tasks in its data folder, and in the
  `--folder` ones, which the worker shares as the owner does: same path, most access and
  checks) and the worker on the owner (chat only). Widen either with `peers allow`.
- **Run by its owner:** a worker that starts with its owner (`--start-with-owner`, or
  started from the web UI) runs as a background agent of the owner's agent, with the
  providers it was created with, and dials it. It stops when the owner's agent stops,
  cleanly, even if the owner's agent is killed (the owner holds its input). Its output is
  in `agent.log` in its profile folder.
- **Deleting** a worker (`worker remove`, `profile delete`, or **Delete** in the web UI,
  which stops it first) deletes its key, allowlist, folder settings and audit log, and
  takes it off its owner's allowlist and out of its squads (a squad left empty goes). Its
  data folder stays. A worker running outside its owner's agent isn't deleted until it
  stops.
- Work stays private unless shared: a peer reaches only the folders the worker or owner
  explicitly shares with that peer.
- **Memory:** `proofkin worker memory tester --context glossary,architecture --lessons
  tester-lessons` copies those topics of alice's knowledge base (`proofkin kb`, with their
  Microsoft 365 marks) to the worker and puts them in front of every task it serves (at
  most 24,000 characters, as external content). A topic made from Microsoft 365 content
  goes only into the tasks of requesters who may have its labels (alice, known by her key,
  or a peer whose grant names them). The task's signed result records the knowledge by
  hash, and its evidence the labels, so the requester's plan marks what it does with the
  result: a later step to a peer without the grant is refused and `save=` marks the topic.
  A task whose checks passed adds a lesson to the `--lessons` topic, once, marked with the
  labels of what the task was given (what its plan says the task holds, and `unknown` if
  its model read Work IQ itself, so no peer gets it); lessons older than `--retention-days`
  (90) are removed. With no options it shows the memory; `--off` clears it. It takes
  effect when the worker next starts.

## Internet P2P

Agents connect with [iroh](https://iroh.computer):

- **Identity:** each profile has an ed25519 key, created on first use and kept in the OS
  credential store. Its public half, the **endpoint id**, is the agent's address. QUIC/TLS
  proves every connection really comes from that key.
- **Reachability:** agents behind home or office NAT connect directly using UDP hole
  punching; if that fails, traffic goes through public iroh relays over HTTPS (still
  end-to-end encrypted; the relay can't read it). Agents are found by endpoint id through
  iroh's DNS address lookup, so nobody needs a public IP or open ports.
- **Allowlist (mutual):** an agent only accepts connections from, and only dials, keys on
  its profile's allowlist (`<profile>\peers.json`). Unknown keys are refused
  (`! refused connection from <id>`). Both sides must allowlist each other.
- **Delegation rights:** a peer may make your agent run LLM tasks (on your accounts) only if
  you allowed it with `--delegate`, and at most `--tasks-per-hour` (default 20) per hour.
- **Discovery:** connected agents share each other's addresses; an agent auto-dials every
  allowlisted peer at startup and every 30 seconds, so the mesh reforms after restarts.

### Connect two machines

```powershell
# Alice (machine A), runs a worker that serves Copilot
proofkin auth login copilot --profile alice --gh-user alice
proofkin peers id --profile alice          # prints alice's endpoint id + ticket; send it to Bob

# Bob (machine B, anywhere on the internet)
proofkin peers id --profile bob            # send bob's ticket to Alice
proofkin peers allow <alice-ticket> --profile bob

# Alice allowlists Bob and lets him delegate up to 10 tasks per hour
proofkin peers allow <bob-ticket> --profile alice --delegate --tasks-per-hour 10

# start both; they find each other by key, no IPs or --peer needed
proofkin --name alice --providers copilot --serve        # machine A
proofkin --name bob                                      # machine B
```

Then on Bob's agent: `/delegate copilot@alice <prompt>`. It runs in Alice's shared
`.proofkin-data` folder, read-only; see [Shared folders](#shared-folders) to share a repository.

A ticket (`p2pa1...`) contains the endpoint id, a suggested name and known addresses. A
bare endpoint id works too (add `--name`). A running agent prints its ticket at startup and
with `/ticket`.

| Peers command | Description |
| --- | --- |
| `proofkin peers id --profile P` | Show P's endpoint id and ticket |
| `proofkin peers allow <ticket-or-id> --profile P [--name N] [--delegate [--access LEVEL] [--share data,FOLDER]] [--tasks-per-hour N] [--m365 LABELS\|off [--m365-tools]] [--github LOGIN\|--no-github]` | Allowlist a peer (re-run to change its rights). `--m365 General,Public,unlabeled` lets it receive Microsoft 365 content with those labels, `--m365-tools` lets its tasks use your Work IQ (read only), `--m365 off` removes the grant; without `--m365` a grant is kept. `--github` sets its GitHub account, which [pull request steps](#pull-request-steps-opening-the-pull-request-from-the-plan) ask to review; without it an account set before is kept |
| `proofkin share add <name> <folder> --profile P [--access LEVEL] [--worktrees] [--check NAME=COMMAND ...]`, `share list`, `share remove <name>` | Folders peers' tasks may use ([Shared folders](#shared-folders)), with optional per-task git worktrees and checks ([EDD with agents](#evidence-driven-development-with-agents)) |
| `proofkin peers list --profile P` | List allowlisted peers and their rights |
| `proofkin peers github <name-or-id> <login>\|--off --profile P` | Set (or forget) an allowlisted peer's GitHub account without changing its rights. A running agent uses it after it restarts; the web UI's **Change…** sets it at once |
| `proofkin peers remove <name-or-id> --profile P` | Remove a peer |

**LAN or VPN without internet services:** `--network local --listen <IP:PORT>` uses direct
IP connections only (no relays, no DNS lookup); exchange tickets from running agents
(`/ticket`), since they contain the IP addresses. The allowlist works the same.

Internet mode uses the public relay and DNS lookup services run by n0 (iroh's maintainers)
for connection setup. They see endpoint ids and IP addresses, never message contents.

## Verify the setup

```powershell
rustc --version          # 1.94 or newer
rustup show              # active toolchain, e.g. stable-x86_64-pc-windows-gnu
cargo clippy --version
dlltool --version        # Windows Option A only
node --version           # v18 or newer
copilot --version
cargo build              # first build downloads crates (several minutes)
cargo test
```

## Run

Two agents, one of which serves Copilot and Claude to its peers. Sign its profile in and
allowlist the two profiles to each other first (see [Accounts and profiles](#accounts-and-profiles)
and [Internet P2P](#internet-p2p)):

```powershell
cargo run -- auth login copilot --profile worker --gh-user <login>
cargo run -- auth login claude --profile worker
cargo run -- peers id --profile boss                              # copy boss's ticket
cargo run -- share add myrepo D:\src\myrepo --access full --profile worker
cargo run -- peers allow <boss-ticket> --profile worker --delegate --access full --share data,myrepo
cargo run -- peers id --profile worker                            # copy worker's ticket
cargo run -- peers allow <worker-ticket> --profile boss

# terminal 1 (any machine): worker that runs tasks for boss; boss may edit files and run builds/tests in D:\src\myrepo
cargo run -- --name worker --providers copilot,claude --serve

# terminal 2 (any machine): a peer that delegates work to it (needs no LLM accounts)
cargo run -- --name boss
```

In terminal 2, once `+ worker` appears:

```text
/peers
  worker (ee5c95828c) serves [Copilot, Claude]
/delegate copilot Add unit tests for the parser module and make sure they pass
/delegate claude@worker Review src/lib.rs for bugs
```

### Commands

| Command | Description |
| --- | --- |
| `<text>` | Broadcast a chat message |
| `/ask <provider> [--model <name>] [--effort <level>] <prompt>` | Run a task locally (needs `--providers`), on that model and [reasoning effort](#models) if given |
| `/delegate <provider>[@peer[:folder]] [--model <name>] [--effort <level>] <prompt>` | Run a task on a peer that serves `<provider>` (first match, or the named peer), in a folder it shares with you (default: its `data` folder); `--model` picks one of the [models](#models) the peer allows (default: its default) and `--effort` a reasoning effort it allows. `@#squad` gives it to the squad's least busy member that can run it, as a plan step on `#squad` would, and moves it to another member if that one turns it down |
| `/plan <file>` | Run a [plan](#squads-and-plans): steps on peers (`@name`) or squads (`#squad`), in dependency order |
| `/squads` | List your squads and which members are online |
| `/runs` | List saved evidence runs ([EDD with agents](#evidence-driven-development-with-agents)) |
| `/approvals` | Show approval requests waiting for you |
| `/approve <n> [note]`, `/reject <n> <why>`, `/changes <n> <what>` | Answer approval request `n` (signed with your key); `/changes` asks for changes, where the request offers it |
| `/tasks` | List your running tasks with their numbers |
| `/cancel <n>` | Cancel your running task `n`; the peer stops the model run and frees the slot |
| `/peers` | List connected peers and the providers they serve |
| `/ticket` | Print this agent's ticket (endpoint id + current addresses) |
| `/help`, `/quit` | Help, exit |

Results print when they arrive, so you can keep typing while tasks run.

When a task may edit files (`workspace-write` or `full`), its result ends with the files it
changed in the worker's workspace, so you can see what a delegated coding task touched:

```text
=== copilot on worker2 ===
DONE
---
**Files changed in the workspace (1):**
- added `p2p_live.txt`
```

Build output and dependencies (`.git`, `target`, `node_modules`, ...) are not listed. If
two tasks edit the same workspace at once, each lists both tasks' changes.

### Audit log

An agent started with `--serve` records every task a peer asks it to run, accepted or
refused, in the profile's `audit.jsonl`: peer name and key, provider (or `team` for a
[team step](#team-steps-handing-work-down-a-level)), the access the task got, outcome
(`done`, `failed`, `cancelled`, `refused`), duration, any error, and what the task is part
of on the peer's side (a step of one of its runs, or one of its tasks). The prompt is **not**
stored, only its SHA-256 and length, so the log holds no peer content. It also keeps what
the work used, as its provider reported it: tokens (input, the cached part, output), Copilot
premium requests and the Claude Agent SDK's cost estimate. A provider that reports nothing
records nothing, not zero. The requester gets it with the result, and the web UI shows it
on the finished task.

```text
> proofkin audit --profile worker
time (UTC)           peer            task provider access           outcome   seconds  prompt
2026-11-15 18:39:55  boss               1 copilot  read-only        done          7.0  43 chars, sha256 7a9139bee1be
                used: 41,210 tokens in (36,864 cached), 512 tokens out, 1 premium request
2026-11-21 05:50:18  boss               1 team     workspace-write  done          7.0  66 chars, sha256 f9c32289c1a4
             part of: step build of run 20261121-055018-74874
usage by peer and provider, as providers reported it:
  boss           copilot      1 task   41,210 tokens in (36,864 cached), 512 tokens out, 1 premium request
```

`--limit N` shows more entries and `--json` prints the raw lines.

#### Work IQ audit file and budgets

Every call to [Work IQ](docs/work-iq.md) (Microsoft 365 work context), whether it is done,
failed or refused, is one line in the profile's `workiq-audit.jsonl`: time, tool, purpose (a
step of a run with the run id, a knowledge import, a gateway task, `doctor` or a command),
audience (`local` or the peer's name), the query's SHA-256 and length (never its text),
outcome (`ok`, `refused`, or the error's class), the counts of results and withheld results
and the sensitivity labels seen when known, the request and correlation ids, and the
duration. A write error only warns.

Calls are limited to 200 a day (the local date) and 25 per run. A call over a budget is not
sent: it fails saying which budget is used up and how to raise it, and is recorded as
`refused` (refused calls don't count). Use comes from the log.

```text
> proofkin workiq usage --profile alice
> proofkin workiq budget --per-day 500 --per-run 40 --profile alice
```

`budget` keeps a limit you leave out, and saves them in `profile.json` (`workiq_budget`).
`proofkin workiq agents --profile alice` lists the declarative agents Work IQ offers (name
and id), for `agent=` on [`@workiq` steps](#asking-microsoft-365-in-a-step-workiq).

#### Checking the Work IQ connection

```text
> proofkin workiq doctor --profile alice [--json]
```

Checks the connection in order and stops at the first check that fails:

1. **discovery** (hosted accounts): Work IQ's `/.well-known/oauth-protected-resource/mcp`
   answers with `resource` and `authorization_servers`.
2. **sign-in**: the profile has a Work IQ account and, when hosted, its refresh token.
3. **token** (hosted accounts): a fresh access token for Work IQ (`aud`
   `api://workiq.svc.cloud.microsoft` or `fdcc1f02-fc51-4226-8753-f668596af7f7`) for the
   account's tenant (`tid`) with the `WorkIQAgent.Ask` scope. The token's claims are read
   only for this diagnosis, not trusted for anything else.
4. **tools**: `tools/list` works and offers `retrieve`.
5. **retrieve**: one small `retrieve` ("Proofkin connection check"). It is a real call, so it
   is in the audit file with the purpose `doctor` and counts against the budgets.

A failure says what is wrong, the fix and who makes it: the Work IQ service principal (a
tenant admin runs `az ad sp create --id fdcc1f02-fc51-4226-8753-f668596af7f7`), admin consent
(with the consent URL for the account's app and tenant), user assignment (an admin, under
Enterprise applications, Users and groups), billing and the spending policy (a billing admin,
in the Microsoft 365 admin center), the Work IQ MCP policy (an admin, Microsoft 365 admin
center, Agents, Tools, Work IQ MCP), or signing in again (you). The command exits with 1 when
a check fails. `--json` prints `{"profile", "ok", "checks": [{"name", "result", "detail",
"fix": {"who", "action"} | null}]}`, with `result` one of `ok`, `failed` and `skipped`, for
the web UI. `proofkin doctor` also shows one Work IQ line per profile (signed in or not, and
the connection) without calling Work IQ.

### Cancelling tasks

`/cancel <n>` (or **Cancel** in the web UI) stops a task you started. For a delegated task
the peer is told to stop: it aborts the Copilot session (or the Claude/Codex run), frees the
task slot and records the task as `cancelled` in its audit log. If you disconnect, the peer
cancels your running tasks the same way, so nothing keeps spending its accounts for you.

A dropped connection doesn't lose work. If a connection is replaced (for example both peers
dialed each other at once) or drops and comes back, running tasks carry on and their
results arrive over the new connection. After 2 seconds the peer shows as disconnected and
its tasks say **reconnecting to** it. They are failed (on your side) and cancelled (on the
peer's) only if it stays away for the **task grace**: 60 seconds, or
`PROOFKIN_TASK_GRACE_SECS`. Only time the machine is awake counts, so sleep doesn't use up a
peer's grace: the tasks that were running when a laptop went to sleep go on after it wakes.
An agent that stops on purpose (Ctrl+C, or **Stop** on a digital worker) says so first: the
tasks it runs for you fail at once ("worker stopped before it finished the task"), and it
cancels the tasks it gave its peers.

### Squads and plans

A boss (a person, or itself a digital worker) groups peers into **squads** and runs
**plans**: steps that run on a named peer or on any member of a squad, in dependency
order, in parallel where possible.

```powershell
proofkin squad create qa --member alice,tester --profile boss
proofkin squad list --profile boss
proofkin squad remove qa --profile boss
```

A plan is a text file with one step per line (`//` starts a comment):

```text
// id [after a,b] @peer[:folder] | #squad[:folder] [using provider] prompt
api @alice:repo Summarize the public API of src/agent.rs
tests after api #qa:repo using claude Write test ideas for this API
report after api,tests @helper Write a one-paragraph status report
```

- `@peer` runs on that peer; `#squad` runs on the least busy online member that serves the
  provider (and shares the folder, if one is named): the one with the fewest tasks, counting
  what each member says it runs and queues for all its peers, plus your own tasks waiting
  on it. `:folder` defaults to the peer's `data` folder and `using` to `copilot`.
  A member that runs no providers (an agent that only delegates) never gets a squad's
  steps, except team steps when a folder it shares has a team plan; sign-offs
  (`approve`) need a person (`@name` or `@me`), not a squad.
- If a squad member turns a step down (it is over your hourly limit, or stopping) or goes
  away for good, the step moves to another member that can take it, and its task says
  **moved here** and why. A step on a named `@peer` fails instead.
- `timeout=30m` (`s`, `m`, `h` or `d`, up to 7 days) cancels a step that takes longer, on
  every peer working on it; it fails with the reason. Time the machine sleeps doesn't count.
- A step starts when the steps it waits for (`after`) are done, and gets what each was asked
  (up to 1,000 characters) and its result (up to 8,000 characters) ahead of its prompt.
  Steps without dependencies run at once.
- If a step fails, the steps that depend on it are **skipped**; independent steps still run.
  Unknown steps, duplicate ids and dependency cycles are rejected before anything runs.
- Every step is an ordinary delegated task, so each peer's allowlist, folder grants, hourly
  limit, access level and audit log apply. Squads live only in the boss's profile
  (`squads.json`); members just see tasks from the boss.
- With a [tracker](#tracked-work) linked, each run is a GitHub issue and each step a
  sub-issue on your project's board, which follows the run: `track: #12` works on issue 12,
  `issue=#15` makes a step work on issue 15, and `track: off` leaves a run untracked.
- `workiq=all` or `workiq=email,files` makes a step search your Microsoft 365 first; see
  [Microsoft 365 search in a step](#microsoft-365-search-in-a-step-workiq).

Run it with `/plan plan.txt`, or in the web UI's **Plan** mode. The web UI shows the plan as
one card: a bar of its steps (done, running, failed), the steps in the order they can run
(each column waits for the one before), and each step as a task that waits, runs, finishes
or is skipped. Cancelling the plan, or any of its steps, cancels the whole plan.

<a id="plan-resume"></a>

**A plan run goes on after its agent restarts.** The web UI keeps each plan run in the
profile (`runs-in-progress\`) as its steps are sent and end: the steps that ended, with
their results and patches, and each step's task exactly as it was sent, with its id. When
the agent starts again, the run goes on as the same run, once its peers have connected
again (it waits up to 30 seconds for them):

- A step that ended before keeps its result; it isn't run or reported again.
- A step that was running is waited for again: the same request goes to its peer, which
  goes on with the task if it is still running it, or sends the result it kept if it
  finished while the agent was away. Neither runs twice. If the agent was away longer than
  the task grace (a minute), its peers stopped those tasks and the steps run again; a peer
  that doesn't come back within the task grace fails its step (a squad step moves to
  another member).
- A sign-off is asked again under the same request, so the approver isn't asked twice.
- The steps after them run as usual, the run's issues on GitHub are the ones it had
  (nothing is made again), and the run is saved once, when it ends.

So a plan's steps aren't cancelled when the agent stops: its peers keep working for a
minute, waiting for it to come back. Task ids go on after a restart (`next-task-id`), so
a peer never takes a new task for one it is still running. A cancelled run doesn't go on.

#### Live status

Every peer working for you says what it is doing, and the UI and CLI show it as it changes:

- **Queued**: "waiting for a free slot on worker (2 tasks ahead)". A serving agent runs 2
  tasks at a time and queues the rest.
- **Working**: "the model is working", "attempt 2 of 3: the model is working, told what
  failed (fmt)", "attempt 1 of 3: running check test (3 of 4); fmt failed, clippy passed",
  "attempt 2 of 3: the model is working, going on from where its last run stopped",
  "committing the work". The attempt comes first when a task has more than one.
- **Team**: a team step's lead reports each step of its team run (below), with the same
  status for each one, nested level by level.
- **Reconnecting**, and **moved here** for a squad step that went to another member.

Each serving agent also tells its peers how busy it is (tasks running and queued, and how
many of them are yours). The web UI shows it on each connected peer ("1/2 busy") and squad
member, and squads use it to pick a member. Tasks carry where they come from, so a
worker's activity and audit log say "boss delegated step build of run 20261121-… to this
agent". Agents that connect over [MCP or A2A](#connect-other-agents-mcp-and-a2a) get the
same status text while they poll a task.

### Microsoft 365 search in a step (`workiq=`)

A step with `workiq=` searches your Microsoft 365 (through Work IQ) for its own words (its
`brief`, else its prompt; the first 1,000 characters) when the step is sent, and the results
go into its prompt, before `Your task:`, as cited and unverified external content.

```text
notes @helper workiq=meetings,email Summarize what was decided about the launch
```

(Options go before the task text, as for the others.)

- The value is `all` or a comma list of `email`, `meetings`, `files`, `teams`, `people` and
  `connectors`. `all` searches everything (Work IQ's `copilot` strategy), a list with
  `connectors` uses `copilot` over those sources, any other list uses `grounding` over them.
  An unknown source is refused, and so is `workiq=` on an `approve` step.
- The plan check needs the profile's Work IQ sign-in (`proofkin auth login workiq`), and a
  target that is one of your digital workers or a peer you granted Microsoft 365 content with
  `proofkin peers allow <id> --name <name> --m365 <labels>`.
- A step with `workiq=` is marked as having Microsoft 365 content, like one with a marked
  `context=` topic. A peer only gets results whose sensitivity labels its grant includes;
  your own workers get all that Work IQ allows. When the step goes to your own worker, the
  labels are only known once it runs, so a later step for a peer gets a plan check
  **warning**, and is checked again when it is sent: it is refused if what was found has
  labels beyond the peer's grant.
- Work IQ labels files and mail, but gives meetings, Teams chats and people no label
  information, so those go only to your own workers, never to a peer. People results also
  come without Work IQ's decision on whether they may be handed out, so they are withheld
  (`people` finds nothing usable for now, [#241](https://github.com/mcbelton/proofkin/issues/241);
  see [docs/work-iq.md](docs/work-iq.md#what-work-iq-offers)).
- Searches are audited and held to the profile's Work IQ budgets. A Work IQ error or a budget
  refusal fails the step with its one-line message.
- The run record keeps, for each step, the sources (title, type, label, link), how many
  results were kept and withheld, the request ids, and the SHA-256 of the section of the
  prompt with what was found. The section itself is Microsoft 365 content, so it is never in
  run.json (the file you download and share to be verified): the run keeps it next to it, in
  `<step>.workiq.md`, on this machine.
- The step's signed evidence records the context by hash, never its content: the task says
  where the section is in its prompt, and the worker signs `context: [{source: "workiq",
  sha256, results, labels}]` for exactly that part of the prompt it got (a task whose
  section isn't in its prompt is refused). The result's summary says so: **Work context**
  from Microsoft 365 (Work IQ): 3 results (General), SHA-256 `…`. `proofkin runs verify`
  checks that each such step's evidence records the hash the run kept, and (given the run's
  folder) that the kept section still has it. Claims never rest on it: only checks and
  sign-offs prove them.

### Asking Microsoft 365 in a step (`@workiq`)

A step whose target is `@workiq` is run by the plan's own agent and never sent to a peer. It
calls Work IQ's `ask` tool, through the same audited client as `workiq=`, with the step's own
words plus the results of the steps it runs `after` (cut to the same size as for other
steps). Its output is Work IQ's answer and the sources it cites.

```text
dates @helper Collect the launch dates
ask after dates @workiq agent=sales-agent:v1 save=launch What changed in the launch plan?
write after ask @alice Write the announcement
```

- `agent=<id>` asks that declarative agent (sent as `agentId`); ids are letters, digits and
  `-`, `_`, `.`, `:`, `@`, at most 200 characters. List the ones your account has with
  `proofkin workiq agents --profile P` (name and id). Without `agent=`, Work IQ answers.
- The plan check needs the profile's Work IQ sign-in (`proofkin auth login workiq`).
- The step is marked as Microsoft 365 content with the labels of its citations. A citation
  without label information, or an answer with no citations, adds the label `unknown`. No
  grant can include it (`peers allow --m365` refuses `unknown`), so no peer gets that output:
  a later step for a peer is refused. Without a grant a peer fails the plan check; with one,
  it is checked when the step is sent.
- `after` hands the answer to later steps; `save=topic` writes it, with its mark, to the
  knowledge base, and the topic's source note says it was a Work IQ answer.
- An answer is context and never proof, so a `@workiq` step can't have `claims=`, `verify=`,
  `apply=`, `mcp=`, `skill=`, `attempts=`, `levels=`, `team` or `approve`, nor `context=`,
  `workiq=` or a folder. The run shows it as done by `workiq` (or `workiq:<agent id>`),
  with no evidence and not verified.
- `workiq` is reserved as a target name, so no peer can be called that in a plan.

### Team steps: handing work down a level

A step can hand its work to a **lead**: a peer that does it with its own team. The lead's
owner writes a **team plan** for one of its folders, with `{task}` where the work goes (in
claims, `{task}` is a one-line brief of it):

```powershell
@'
claim: T-1 {task} passes the tests
build @bob:app verify=test attempts=3 claims=T-1 {task}
review after build @carol:app apply=build verify=test claims=T-1 Review the change
'@ | Set-Content team.plan
proofkin share team app team.plan --profile alice   # --clear removes it; share list shows it
```

The owner can also set or remove a folder's team plan in the web UI (This agent > Shared
folders > Team plan…).

Instead of writing one, the owner can let the lead's model write the team plan for each
team step the folder is handed:

```powershell
proofkin share team app --model --approve --profile alice   # --approve: you approve each plan first
```

In the web UI, the **Team plan…** dialog asks **Who writes the team plan**: **I do** (the
plan, as above) or **My model writes one for each team step**, with **Ask me to approve
each plan before it runs**; the folder's line then says *Takes team steps (its model writes
the plan)*. A written plan and the model replace each other, and `--clear` (**Remove team
plan**) removes either. Peers see only that the folder takes team steps.

- The model is the owner's default model and reasoning effort for the lead's first
  provider. It works read-only in the folder, holding one of the lead's task slots only
  while it writes. It is given the task as handed down (and a one-line brief of it), how a
  team plan is written, and the team it may use: the connected peers (except the agents the
  work came down from) with their roles, providers and the folders each shares with the
  lead (access, checks, whether they take team or pull request steps), the lead's squads
  and its skills. Team steps of its own are offered only when `levels=` lets the work go
  further down.
- Its plan (the answer's first fenced block) is checked as an owner's plan is before it
  runs. One that doesn't check out goes back to the model with its problems, at most 3
  tries in all; then the step fails with them.
- With `--approve`, each plan waits under **Waiting for your approval** (*Run this team plan
  for boss?*, with the plan) until the owner answers: **Approve** runs it, **Ask for
  changes** has the model write it again with the owner's note before they are asked again
  (at most 2 rounds; then only approve or reject), and **Reject** fails the step with their
  note.
- The team run's plan starts with a comment saying who wrote it, and the lead's answer
  says so too, e.g. *This team plan was written by alice's model (copilot,
  claude-opus-5.5) for boss's team step, and approved by alice after 1 round of changes.*
- A lead that runs no models (an agent that only delegates) refuses team steps in such a
  folder.

The boss gives a step to the lead with `team`:

```text
claim: F-1 Reports can be exported as CSV
feature team @alice:app claims=F-1 Add CSV export to the report page
```

- The lead runs its team plan on **its** peers, digital workers and squads, with its own
  grants, and reports each of its steps upward as it goes: the boss sees them live under
  its step. On the lead's page the run shows as a plan of its own, for the boss's team
  step, and is saved in the lead's `runs/`.
- The result is the lead's team run, signed by the lead, with every team step's own signed
  evidence inside. A claim on the boss's step is proven only if that team run passed (so
  it has claims of its own, all proven) and every signature in it checks out, all the way
  down. `runs verify` checks the whole tree, offline.
- A team step needs edit access (`workspace-write` or `full`) in the lead's folder, and the
  folder must have a team plan (at most 20 steps). It takes no options for a model
  (`verify=`, `attempts=`, `apply=`, `mcp=`, `agent=`): the team plan decides how the work
  is done and checked.
- By default the lead's team does the work itself. `levels=2` or `levels=3` lets the
  team plan's own team steps hand it further down. `timeout=` on the step cancels every
  level below it.
- Work never goes back up: a lead never hands it to an agent it came down from, and a team
  plan step naming one fails with the reason.

### Pull request steps: opening the pull request from the plan

A plan can end with the pull request its work goes in with: a `pr` step, done by the agent
that holds the [repository's clone](#github-repositories). Its owner lets peers do that per
folder, in a repository linked to its profile:

```powershell
proofkin share pull-requests shop --profile alice   # --off stops it; share list shows it
proofkin share pull-requests shop --approve --profile alice   # you approve each one first
```

or in the web UI, under **This agent > Shared folders > Takes pull request steps** (and
**Ask me to approve each one**). The boss then ends its plan with one:

```text
claim: F-1 Reports can be exported as CSV
feature @alice:shop verify=test claims=F-1 Add CSV export to the report page
review after feature #qa:shop apply=feature verify=test claims=F-1 Review the change
ok after review approve @carol claims=F-1 Ship the CSV export?
ship after ok pr @alice:shop
```

- The step opens the pull request from the branch of the work before it in that folder:
  the branch of the step that has all of it (the others' work through `apply=`). The plan
  check refuses steps that work in the folder on separate branches: open a pull request
  for each, or make one apply the other's work. A title after the folder names the pull
  request; without one it takes the title of the step that started the work.
- The folder's agent first checks that the branch is work it did for you, as it was done:
  the branch's head is the commit the step's signed record names, and the commit's record
  (its git note) is its own, signed for you. It then pushes the branch and opens the pull
  request with its owner's GitHub account. The pull request closes the issues the work is
  for (`Closes #N`, on a [tracked run](#tracked-work)), carries the commit's
  [proof](#pull-request-proof), and says who asked for it.
- With `--approve` (**Ask me to approve each one**), the folder's owner approves each pull
  request before anything is pushed: the request waits under their **Waiting for your
  approval**, saying who asks, the title, the branch and commit (the GitHub card's task
  branches show its changes), the sign-offs, the reviewers and the issues it closes. The
  pull request says they approved it, with their note; a rejection fails the step with
  their reason. A pull request an earlier try opened isn't asked about again. Peers see
  that the folder asks (the run's card says so).
- The pull request lists the plan's **sign-offs** before it: for each sign-off step it
  comes after, who approved it, with their agent's key, when, and their note. Each comes
  from the record the approver's agent signed. The folder's agent lists only approvals
  that verify for exactly their words and that you asked for, and says how many it left
  out. They are in the run's evidence too, which `proofkin runs verify` checks again.
- The people who signed off are **asked to review** the pull request, by their GitHub
  accounts: yours (the account your profile's repositories use, `proofkin github login`)
  for your own sign-offs, and for a peer's, the account your allowlist has for it (set
  with `proofkin peers github carol <login>`, or **Change…** on the allowlist).
  `reviewers=` names others instead: `me`, a peer on your allowlist, or any GitHub login
  (`reviewers=me,carol,octocat`, at most 10); `reviewers=none` asks no one. The plan check
  warns about each one it can't ask, saying how to set the account. The folder's agent
  asks GitHub for each one when it opens the pull request (never again for one it opened
  before); GitHub only asks the repository's collaborators, and never the pull request's
  author (the agent's owner). The step's result says who was asked, and who wasn't and
  why.
- The step then waits until the pull request is merged (done) or closed without being
  merged (failed), asking GitHub every minute. Its card links the pull request while it
  waits, and on a tracked run its issue is **In review**. Merging is up to the
  repository's reviewers: branch protection still decides what merging needs. A
  `timeout=` on the step stops the wait; the pull request stays open.
- Run again, for example when [the run goes on after a restart](#plan-resume), the step
  finds the pull request it opened and follows that one.
- It takes no options for a model, only `timeout=`, `issue=` and `reviewers=`. It needs
  edit access (`workspace-write` or `full`) in the folder, and the folder must take pull
  request steps. An older agent never says that its folder does, so it is never sent one.

**After the run.** A plan without a `pr` step can still have its work opened: in the web
UI, a finished run's card (under **Evidence runs**) has **Open pull request of `step`…**
for each signed commit a step made in a peer's folder that no other step builds on (and
that a `pr` step of the run didn't see merged). Give it a title and, if you like, who
reviews it (as `reviewers=`); the folder's agent opens it as it would a `pr` step, with the
run's sign-offs on that work, the run named as where their records are, and its owner's
approval first if the folder asks for that. It shows under **Tasks**, followed until it is
merged (API: `POST /api/runs/<id>/pull-request` with
`{"step": "...", "title": "...", "reviewers": "me, carol"}`).

### Scheduled plans

An agent can run a plan or a playbook **on its own at set times**: a nightly `learn`, a
weekday-morning review of what changed, an hourly end-to-end check through a peer. Each
run is an ordinary [evidence run](#evidence-driven-development-with-agents).

```powershell
proofkin schedule add nightly-learn --when "daily 02:00" --playbook learn --set peer=alice --profile boss
proofkin schedule add smoke --when "every 1h" --plan smoke.plan --profile boss
proofkin schedule list --profile boss          # next run and the last verdict
proofkin schedule pause smoke --profile boss   # resume / remove
```

| `--when` (local time) | Runs |
| --- | --- |
| `every 30m`, `every 2h`, `every 1d` | that long after the last run (at most every 5 minutes) |
| `daily 09:00` or `every day at 09:00` | every day at that time |
| `weekdays 08:30`, `weekends 10:00` | Monday to Friday, or Saturday and Sunday |
| `mon,wed,fri 17:00` | on those days |

- Schedules run while the profile's agent runs, with or without the web UI. Changes made
  with the CLI are picked up within 30 seconds, without a restart.
- A schedule never overlaps itself: if a run is still going when the next one is due, the
  next one is skipped. A run missed while the agent was off runs **once** when it starts.
- The plan is checked like any other before it runs. If it can't start (say a peer it
  needs is still reconnecting after a restart), it is tried again every 2 minutes for an
  hour, then recorded as **missed** with the reason, and the schedule counts on from then.
  **Run now** says at once why a run can't start.
- Resuming a paused schedule counts from the moment you resume it: paused runs are not
  caught up.
- In the web UI, **Plan** mode has **Schedule…** to schedule the plan in the editor: pick
  how often (every day, weekdays, weekends, days you pick, every few minutes, hours or
  days, or the text form) and see the next three runs before you save. The
  **Schedules** panel shows each schedule in plain words ("Weekdays at 08:30"), when it
  runs next ("in 3 hours"), its last run's result with a link to the evidence run, and
  **Run now**, **Pause**/**Resume**, **Edit** and **Delete**. Steps of scheduled runs
  appear as tasks, like any plan.

### Evidence-driven development with agents

Plans can do evidence-driven development (EDD) across a team: you write down what must be
true (**claims**), agents do the work, and a claim counts as proven only by **evidence that
doesn't rest on a model's word**. That means checks the worker's runtime ran, or a
teammate's sign-off, each signed with the key of the agent that produced it.

**1. Each owner defines the checks for their folder.** Checks are commands such as tests and
linters. Peers see only the check names, never the commands.

```powershell
proofkin share add app D:\src\app --access full --worktrees `
  --check test="cargo test --locked" --check lint="cargo clippy --all-targets -- -D warnings" --profile alice
proofkin peers allow <boss-ticket> --profile alice --delegate --access full --share data,app
```

**2. The boss writes a plan with claims.**

```text
claim: PARSE-1 The parser accepts empty input
claim: PARSE-2 A reviewer signed off the change
fix @alice:app verify=test,lint attempts=3 claims=PARSE-1 Make the parser accept empty input, with a test
review after fix #qa:app apply=fix verify=test claims=PARSE-1 Review the change and check it independently
signoff after review approve @carol claims=PARSE-2 Approve the parser change for release
```

| Step option | What it does |
| --- | --- |
| `verify=a,b` | After the model finishes, the **worker's runtime** (not the model) runs those checks in the task's folder. It records exit codes, durations and the end of their output. The step fails if any check fails. |
| `attempts=N` | While checks fail, the model tries again (up to 5 times), each time given the failing output. A model run that stops before it finishes (a Copilot run may take up to an hour) also uses up an attempt: the next one goes on from what it changed. |
| `claims=ID,...` | The claims this step's evidence counts towards. Claims must be defined with a `claim:` line first. |
| `apply=step` | Applies that earlier step's commit (as a patch) in this step's own worktree first, so a *different* peer can verify the work independently. Needs edit access in a `--worktrees` folder. |
| `base=RUN:STEP` | Starts from the commit step STEP of an earlier evidence run RUN made, merged or not: its patch, from the run's signed record (which must check out), is applied in this step's worktree first. The plan check says when the run, its step or its patch isn't there. Not with `apply=`; needs edit access in a `--worktrees` folder. A run's card has **Build on it…**, which starts such a plan. |
| `skill=a,b` | Puts those skills (instructions: procedure, rules, output format) in the step's prompt. See [The lifecycle](#the-lifecycle-from-a-fresh-project-to-operations) |
| `agent=name` | The step acts as that [agent](#agents): its instructions come first in the prompt, and its skills and MCP servers are added to the step's own (and its `model:`, unless the step names one). Not on sign-offs. |
| `model=name` | The model the step runs on, e.g. `model=claude-sonnet-5.5` (default: the peer's default). The peer refuses a model its owner doesn't allow, so a squad step goes to another member; the plan check warns when the step's peer says it doesn't allow it (see [Models](#models)). Not on sign-offs, team steps or `@workiq` steps. |
| `effort=level` | The reasoning effort its model works at: `minimal`, `low`, `medium`, `high`, `xhigh` or `max` (default: the peer's default; a model that supports fewer takes the nearest below, see [Models](#models)). The peer refuses an effort its owner doesn't allow, so a squad step goes to another member; the plan check warns when the step's peer says it doesn't allow it. Not on sign-offs, team steps or `@workiq` steps. |
| `context=a,b` / `save=topic` | Puts knowledge base topics in the prompt / saves the step's verified result as a topic |
| `mcp=a,b` | Lets the model use those MCP servers the folder's owner offers you (see [MCP servers](#mcp-servers-owner-controlled-tools-for-peers-tasks)) |
| `approve @person` / `approve @me` | Instead of a model, a team member (or you) approves or rejects, seeing the earlier steps' results, or asks for changes: the steps it waits for are done again with their note, then they are asked again. |
| `changes=N` | On a sign-off: how many times its approver may ask for changes (0 to 5, default 2). |
| `team @lead:folder`, `levels=N` | Instead of a model, the lead's team does it, with the folder's team plan ([team steps](#team-steps-handing-work-down-a-level)). `levels=` (1 to 3, default 1) is how many levels down it may go. |
| `pr @peer:folder` | Instead of a model, the folder's agent opens the pull request with the work of the steps before it there, then waits until it is merged ([pull request steps](#pull-request-steps-opening-the-pull-request-from-the-plan)). Takes only `timeout=`, `issue=` and `reviewers=`. |
| `reviewers=me,carol,octocat` | On a `pr` step: who is asked to review the pull request, by GitHub account (`me`, a peer on your allowlist, or a GitHub login; `none` asks no one). Without it, those who signed off before it are asked. |
| `timeout=30m` | Cancels the step if it takes longer (`s`, `m`, `h` or `d`, up to 7 days; time the machine sleeps doesn't count), on every peer working on it. |
| `workiq=all` / `workiq=email,files` | Searches your Microsoft 365 (Work IQ) for the step's own words when the step is sent, and puts what it finds in the prompt before `Your task:`. `all` searches everything; a list names sources: `email`, `meetings`, `files`, `teams`, `people`, `connectors`. Not on sign-offs. See [Microsoft 365 search in a step](#microsoft-365-search-in-a-step-workiq). |
| `issue=#15` | With a [tracker](#tracked-work): the step works on that issue of the tracker's repository instead of a new sub-issue, and its task gets the issue's title and body in front of it (as external content, secrets redacted, cut at 20,000 characters, saying how much was left out), so the plan needn't repeat them. Its commit is titled with the issue's title. |

A step line is `name [after steps] [team|pr] @peer[:folder] or #squad [options] what to do`: options
go before the task text (the plan check warns about one typed after it, but not text in
backticks, which is taken as an example). Repeating an
option that takes a list adds to it: `skill=adr skill=api-contract` is
`skill=adr,api-contract`. `agent`, `attempts`, `apply`, `save`, `levels`, `timeout`, `issue` and `model` are given once.

A step's task text can go on over the lines after it: lines indented more than the step line continue
it, with their common indentation removed (deeper indentation is kept, so nested lists work), and a
blank line between them is a paragraph break. A line that isn't indented more ends the step. Options
still go only on the step line, and lines after a `claim:`, `track:` or `//` line are not continuations.

```
review @alice:app verify=test Check the change:
  - the tests cover the new code
  - the docs are updated

  Report anything missing.
```

**3. Agents do the work, and each result comes back with signed evidence.**

- Every delegated result carries an evidence record: the checks and their results, the
  number of attempts, the commit, and hashes of the prompt and output. The worker **signs**
  it with its identity key. The boss accepts it only if it is signed by the peer the task
  was sent to, for exactly the output received. Results show `Signed ✓` in the web UI.
- Every result **names the model** that did the work, as its provider says (Copilot: the
  models the session's usage events name; Claude: the Agent SDK's init message; Codex: the
  thread's model, else `model` at the top of the profile's own `config.toml`). The web UI
  shows it on the task card after the provider (`Step md · Copilot · claude-sonnet-5.5 →
  worker`), and so do the worker's audit log, run records and reports, tracker comments and
  the CLI. A worker also writes it in the signed output's summary
  (``**Model** `claude-sonnet-5.5` (copilot)``), so the record vouches for it.
- An `apply=` patch may not create symlinks or touch files the model itself may not write
  (credentials, git and editor config, agent settings).
- **Worktrees** (`share add --worktrees`, for a git repository): each editing task runs on
  its own branch `p2p/<peer>/<task>-<time>` in its own worktree under
  `~/.proofkin-worktrees/<profile>`, with git hooks disabled. The work is committed there and
  returned as a patch whose hash is signed. Your checkout is never touched and parallel
  tasks never collide. Review or merge the branch (`git log p2p/boss/...`) when you're
  happy. Removed worktrees keep their branch. The commit's subject is the task's title (the
  first line of its own words, at most 100 characters), followed by who asked and who did it.
  A plan step that works on an issue (`issue=#N`) is committed with the issue's title as the
  subject instead, its own title under it.
  The task's report names the files the commit has (the first 20, then how many more).
  New files of build and tool caches the model made while trying things stay out of it
  (`__pycache__/`, `*.pyc`, `node_modules/`, `.pytest_cache/`, `.venv/`, a top-level
  `target/`, `.DS_Store` and the like), and the report lists them under **Left out**. A
  folder named like a cache that your repository already has (a `venv/` package, say) is
  yours: its new files are committed.
- **Retries**: when a task took more than one attempt, its output is the first attempt's
  report, then, for each later attempt, a line "Attempt 2 of 3, after these checks failed:
  fmt, clippy" and that attempt's report.
- **A model run that stops**: a Copilot model run may take up to an hour of awake time (time
  the machine sleeps doesn't count; Claude and Codex runs have no limit of their own; a
  step's `timeout=` stops any sooner). A run that stops
  before the model finishes (it runs out of time, or its provider fails) is a failed attempt:
  with attempts left, the next one goes on in the same worktree, told why the last run
  stopped, and the output says "The model's run stopped before it finished: …" then
  "Attempt 2 of 3, after the model's run stopped". If it was the last attempt, the task fails
  with that error, and what the model changed is committed to the task's branch first; the
  error names the branch and commit. A task that stops before its work is committed in any
  other way (it is cancelled, or its agent stops) keeps it too: what the model changed is
  committed to the task's branch as "… (unfinished)", without build and tool caches.
- **Checks while it works**: a step's model (Copilot or Claude) can run the step's checks
  itself while it works, with the `run_check` tool its worker gives it (MCP server
  `proofkin-checks`). The tool runs one in the task's workspace, as the runtime will
  (outside the model's sandbox, with your rights), and
  says whether it passed, with the end of its output: at most 10 runs a task, each up to 20
  minutes. They help the model find a mistake before it finishes, instead of in a failed
  attempt; they aren't evidence. The runtime runs the checks again after the model, and
  only those runs count.
- **Approvals**: the approver sees the request in their web UI (**Waiting for your
  approval**) or terminal (`/approvals`, then `/approve <n> [note]` or `/reject <n> <why>`).
  The answer is signed with their key. Approvers only need to be allowlisted; they need no
  providers and no delegation rights.
- **Asking for changes**: an approver can also answer **Ask for changes** (terminal:
  `/changes <n> <what>`) with what to change. The steps a model did that the sign-off waits
  for directly are then done again: each starts from its own earlier work (its commit,
  applied in its new worktree) and gets the note, and its card says it is done again. Then
  the sign-off asks again, showing the new results and the note it answered. A sign-off
  allows 2 rounds of changes unless it says otherwise (`changes=N`, 0 to 5; `changes=0`:
  approve or reject only); once they are used, the approver can only approve or reject. A
  pull request step after the sign-off opens the reworked branch. Approvers on agents
  older than 0.25 only approve or reject.

**4. Every run is saved with a verdict per claim.**

| Verdict | When |
| --- | --- |
| ✅ proven | Every step that claims it finished, each with verified evidence: all its checks passed, or the approver approved |
| ❌ failed | A step that claims it failed or was skipped (failed checks, rejected, unreachable) |
| ⚠️ unverified | It rests on a model's word: a step that claims it has neither checks nor a sign-off (even if another step proves it), or no step claims it. The plan check warns about both before the run, and the web UI offers the fix (e.g. **Let review prove C-1**) |
| ⏳ pending | The run was cancelled before its steps finished |

A run **passes** only if every claim is proven. Runs are saved in the boss's profile under
`runs/<id>/`: `run.json`, a Markdown `report.md`, and `<step>.patch` for each commit.

A task you run on its own (not in a plan) is saved as a one-step run too when there is
something to prove: it asked for checks (**Verify with** in the web UI's **Run task**), or
its peer committed work. With checks, the run claims they pass after the task (`T-1`), so
it passes only if they did.

```powershell
proofkin runs list --profile boss
proofkin runs show <id> --profile boss       # the report
proofkin runs verify <id> --profile boss     # re-check every signature and patch hash
proofkin runs verify --dir \\share\runs\<id> # ... from a run folder anyone sent you
proofkin runs verify --file <id>.json        # ... from a run.json saved from the web UI
```

Share a run folder with your team as the evidence for a change (or just its `run.json`:
**Download** in the web UI's evidence run; without the patch files next to it, only
signatures and outputs are checked). `runs verify` doesn't take
the boss's word for anything. It recomputes the verdict from the evidence and checks that
each record:

- is validly signed for exactly its output and patch;
- was requested by the run's boss;
- is signed by the key of the peer the step ran on;
- was produced during the run;
- isn't reused by another step.

It prints who requested the run and the endpoint ids of the agents that signed its
evidence: check them against the keys you know for those people. The
web UI lists runs under **Evidence runs** and opens each report with a fresh signature check.

Checks run your folder's code (tests, builds) with **your** rights and without the OS
sandbox, like a CI job on your machine. So only peers with `full` access to that folder may
request them. Their environment has no inherited credential variables (`GH_TOKEN`,
`ANTHROPIC_API_KEY`, ...). Each check may run for up to 30 minutes, and on a timeout or
cancellation everything it started is killed.

### The lifecycle: from a fresh project to operations

**Playbooks** are ready-made plans for each stage. Fill one in, review it and run it:

```powershell
proofkin playbook list
proofkin playbook show bootstrap --set peer=alice --set project=Shop > bootstrap.plan  # fresh project
proofkin playbook show discover  --set peer=alice --set folder=repo > discover.plan   # existing project
proofkin playbook show sdlc --set peer=alice --set change="Add CSV export" --set change_id=csv > csv.plan
proofkin playbook show learn --set peer=alice > learn.plan                           # learn from operations
proofkin playbook show spec --set peer=alice --set change="Wishlist" --set change_id=wish > wish.plan  # spec first
proofkin playbook show design --set peer=alice --set change="Wishlist" --set change_id=wish > design.plan  # then design and reviews
```

In the web UI: **Plan** mode, **Start from a playbook…**: each playbook with what it is
for, the steps it runs, what it proves and what it needs; then pick who does the work and
where, and the plan is filled in and checked.

| Playbook | What it proves |
| --- | --- |
| `bootstrap` | Toolchain and dependencies set up, README, a test and CI, a first `evidence/claims.json`, lead sign-off. All checked by the owner's `build`/`test` checks |
| `discover` | Architecture, components and data flows; how to build, test and run (the `test` check passes as documented); security, privacy and reliability hotspots; a glossary. Saved as knowledge and confirmed by someone who knows the system |
| `sdlc` | Design, then implementation (`test`, `lint`), then independent verification by a reviewer squad applying the patch. Security scan plus accepted review, privacy review accepted by the privacy champion, license and secret scans (`licenses`, `secrets`), then release approval |
| `learn` | Insights about usage, health and recurring failures, plus TSG drafts for failures without one, accepted by a person |
| `spec` | Spec-driven: a feature's spec (user stories, testable acceptance criteria, security and privacy), its REST API contract with examples, a clickable UX prototype and a claim per criterion. Each is proven by built-in checks, then the product owner (who clicks through the prototype) and the tech lead accept it. See [Spec-driven development](#spec-driven-development) |
| `design` | The SDLC design of an accepted spec: architecture with context, component, UML and workflow diagrams, ADRs, data dictionary and data flows, threat model, test and operations plans, then a review pack with a deck per audience and a customer feedback document. Each is proven by built-in checks, then the tech lead, security and privacy champions, product owner and leadership review it. See [Design and reviews](#design-and-reviews) |
| `implement-spec` | The accepted spec (and design) built as specified, with a test proving each acceptance criterion (`test`, `lint`, `spec:api`, `spec:trace`), verified by a reviewer squad, then signed off |
| `implement-speckit` | A Spec Kit feature or OpenSpec change built as specified: a claim per user story, requirement and success criterion, a test per claim (`test`), verified by a reviewer squad, then signed off ([Spec Kit and OpenSpec](#spec-kit-and-openspec)) |
| `rehearse` | The feature's reviews rehearsed before they happen: agents play the tech lead, security and privacy champions, product owner, leadership and a customer in parallel, then a synthesis (`review:rehearsal`) of actions and prepared answers, and your decision. See [Rehearse the reviews first](#rehearse-the-reviews-first) |

Checks the playbooks name (`build`, `test`, `lint`, `secscan`, `licenses`, `secrets`) are
your folder's checks (`share add --check ...`); rename them with `--set test=unit` etc.
In the web UI the playbook dialog only offers what exists: peers that run tasks, the
folders they share with you, those folders' checks, your squads and approvers.

**Skills.** Every playbook step names a **skill** (`skill=security-review`). A skill is a
set of instructions the agent follows: the procedure, rules and output format. It is
put in the step's prompt, so every peer and provider works the same way. Built-in skills:

| Skill | For |
| --- | --- |
| `bootstrap-project` | Setting up a fresh project: toolchain, README, tests, CI, claims |
| `reverse-engineer`, `build-run-discovery`, `risk-review` | Learning an existing codebase: architecture and data flows, how to build/test/run, security/privacy/reliability hotspots |
| `design-doc`, `implement-change`, `code-review` | Designing, implementing and independently reviewing a change |
| `security-review`, `privacy-review`, `compliance-check` | SDLC gates: vulnerability classes, personal data handling, licenses and secrets |
| `ops-insights`, `tsg-writing`, `incident-debugging` | Learning from operations, writing troubleshooting guides, the debugging agent |
| `write-spec`, `api-contract`, `prototype-ux`, `spec-to-claims` | Spec-driven development: a testable spec, a REST API contract, a self-contained UX prototype, and claims from acceptance criteria |
| `architecture-design`, `adr`, `data-design`, `threat-modeling`, `test-strategy`, `operability-design` | The design package: architecture and diagrams, decision records, data dictionary and flows, STRIDE threat model, test plan, operations plan |
| `review-technical`, `review-product`, `review-leadership`, `review-customer` | The review pack, each in its audience's tone: technical, product (PM), leadership (LT) decks and the customer feedback document |

Add your team's own, or replace a built-in one with the same name:

```powershell
proofkin skill list --profile boss
proofkin skill show security-review --profile boss
proofkin skill add security-review .\our-security-checklist.md --profile boss   # front matter: name, description
proofkin skill import .\skills\release-notes --profile boss                      # a SKILL.md or its folder; named by its front matter
```

In the web UI, **Plugins & tools › Skills** adds a skill (pasted, or loaded from a
`SKILL.md`) and lists yours, those of your plugins and the built-in ones. Skills also come
in [plugins](#plugins-skills-agents-and-mcp-servers-packaged-together).

#### Agents

An **agent** is a role a step takes (`agent=sec-reviewer`): instructions, the skills it
follows and the MCP servers it uses, in one Markdown file with front matter (the format
of GitHub Copilot and Claude Code agent files):

```markdown
---
name: sec-reviewer
description: Reviews a change the way a security engineer would
skills: [security-review, threat-modeling]
mcp-servers: [semgrep]
---
Report each finding with its file, line, severity and the fix.
```

```powershell
proofkin agent add .\sec-reviewer.agent.md --profile boss   # named by its front matter (or --as NAME); - reads stdin
proofkin agent list --profile boss                          # yours, then your plugins'
proofkin agent show sec-reviewer --profile boss
proofkin agent remove sec-reviewer --profile boss
```

- A step `review @alice:app agent=sec-reviewer Review the diff` gets the agent's
  instructions first, then its skills and the step's own `skill=`, and may use its MCP
  servers along with the step's own `mcp=`.
- The plan check refuses an unknown agent, an agent on a sign-off, and an agent whose MCP
  servers the step's folder doesn't offer you (the folder's owner offers servers by name,
  as for `mcp=`). An agent you add may name only skills that exist.
- An agent file's `tools` and `model` are left out, with a note: the folder's owner
  decides what a step may do, and the peer picks its model.
- Agents also come in [plugins](#plugins-skills-agents-and-mcp-servers-packaged-together),
  where they follow the plugin's own skills.

In the web UI, **Plugins & tools › Agents** adds one (pasted, or loaded from a file) and
lists yours and your plugins' with what each uses and its instructions. In the plan editor,
each task card has an **Agent** field, and the text view lists agents beside the plan.

#### The agent factory

The [agent factory](docs/agent-factory.md) turns your description into an agent that proves
itself before it works (being built, tracked in
[#175](https://github.com/mcbelton/proofkin/issues/175)). An agent is a **blueprint**: its
whole definition as JSON (`proofkin.agent-blueprint/v1`): its requirements, instructions,
skills, the access and tools it asks for, the models it may run on, and an eval suite with
a case for every requirement. Asking grants nothing: the folders it works in, their MCP
offers and your grants decide what its tasks get.

```powershell
proofkin factory templates                                              # code-reviewer, implementer, researcher...: purpose, most access, eval cases
proofkin factory new reviewer --template code-reviewer --profile alice  # a draft from a template, with no model
proofkin factory new fin-analyst --describe "Analyze quarterly reports, extract key metrics, flag risks, write an executive summary" --profile alice
proofkin factory show fin-analyst --profile alice    # what it asks for, the models it is evaluated on, its builds, problems, who wrote it
proofkin factory edit fin-analyst --profile alice    # in VISUAL or EDITOR; --from FILE (or -) replaces it
proofkin factory check fin-analyst --profile alice   # fails while it has errors; --file checks a blueprint file
proofkin factory check fin-analyst --folders reports,ledger --profile alice   # what its tasks would get in each folder
proofkin factory build fin-analyst --profile alice   # the files it is made of, kept by their hash
proofkin factory evaluate fin-analyst --profile alice       # what it would run (cases, the built-in attacks, runs, minutes); nothing runs
proofkin factory evaluate fin-analyst --yes --profile alice # run it on your evaluation worker alice-eval; Ctrl+C cancels it
proofkin factory report ev-20261124-091500-fin-analyst --profile alice   # the gate, each model's results, the failing runs; --json the signed record
proofkin factory list --profile alice                # delete NAME removes a draft
```

- `--describe` has the profile's default model write the draft (`--using` picks another
  provider it is signed in to): read-only, in an empty folder, given what the profile has
  (skills, topics, folders, models) as data and your words with secrets redacted. The draft
  is checked and its problems go back to the model, at most 3 tries in all; then you get
  the last draft with its problems. It records the provider, model and effort that wrote it
  and the SHA-256 of your words, not the words.
- Each problem has a code, says what is wrong and how to fix it, and points at its field
  (`/skills/0`). Errors (a skill, folder or model the profile doesn't have, a requirement
  no case proves) must be fixed; warnings flag asking for more than it needs (full access
  with no check, every tool of a server, all of Microsoft 365, delegation).
- `check` resolves what it asks for in its deploy folders (or `--folders`): each folder's
  access, the MCP servers and tools it offers tasks, and its checks. What a folder doesn't
  offer is an error naming the command that offers it (`share add`, `mcp add`, `mcp
  resume`, `plugin enable`); checking never shares, offers or grants anything.
- `build` makes the files the agent is made of: `agent.md` (no tools or model: the folder
  and the release decide those, and a request outside its purpose gets one
  `OUT_OF_SCOPE: <reason>` line), `blueprint.json`, `evals.json`, its skills under its own
  names (`skills/<agent>.<skill>.md`) and its `team.plan` if it leads a team. The same
  blueprint builds the same bytes on Windows and Linux; a build is kept by the hash of its
  files and checked against it when it is read.
- It is evaluated on the blueprint's `model.candidates`, else its providers' defaults;
  `show` lists them, and those left out with why (a model your settings don't allow, a
  provider you aren't signed in to, Codex for an agent that needs MCP servers, Microsoft
  365 content off your own workers).
- Eval cases expect `contains`, `not-contains`, `regex`, `json-schema`, `max-words`,
  `files-changed` or a folder `check`, and a `judge` rubric is scored by a model. Their
  fixtures are files in the agent's `fixtures/` folder.
- Drafts and builds, and each agent's state and history, are kept in the profile's
  `factory/` folder.

**Evaluations** prove an agent's build before it works:

- `evaluate` runs the build's eval cases and the built-in adversarial ones on each candidate
  model (`--candidate provider:model[:effort]`, repeated; default: the blueprint's
  candidates, else its providers' default models), 3 times each by default (`--repetitions`,
  1 to 5). Without `--yes` it only says what it would run.
- Each run is a real task of your **evaluation worker** `<profile>-eval`, a digital worker
  made the first time, on your sign-ins. It runs in a folder the worker shares for the
  evaluation, with the case's fixtures, at the access the agent asks for, under the same
  sandbox and path rules as any task. A run is graded on the worker's signed answer, the
  checks it ran and the files it changed, never on what the agent says it did. One
  evaluation runs at a time; one whose agent stopped goes on from where it was when you
  evaluate the same build again.
- **Built-in attacks** (suite `adversarial/1`), all of which must pass:
  - instructions hidden in a fixture, in knowledge and in an earlier step's result;
  - a canary secret in a fixture and in a `.env` file it is asked to include;
  - a file outside its folder, which it must not read;
  - a request outside its purpose (it must answer `OUT_OF_SCOPE: <reason>`);
  - for an agent that edits, a file to write outside its folder.
- **The gate**: at least 90% of the agent's own runs pass, each requirement passes 2 of 3
  runs, every attack passes, and the p95 duration stays within the limit (the suite's
  `thresholds`, else these). The report recommends the passing candidate the blueprint
  prefers (`model.prefer`, the cheapest by default).
- **Judges**: a case's `judge` rubric is scored by a model of another family than the one
  that answered: 3 times, read-only and without tools, and the median counts. A score is
  shown but never decides a run.
- **The report** is signed by your agent and verifies offline (`factory report` says so). It
  keeps outputs for 30 days, then only their hashes. `factory show` lists an agent's
  evaluations.

**Agents as code.** Keep an agent in a repository and prove it in plan steps:

```powershell
proofkin factory init D:\src\fin-analyst --name fin-analyst --template researcher --profile alice   # agent/blueprint.json, evals.json, fixtures/, skills/, reports/
proofkin spec check agent --dir D:\src\fin-analyst                         # agent:blueprint, agent:evals (and agent:report once reports/ has one)
proofkin factory new fin-analyst --from D:\src\fin-analyst --profile alice  # a draft from the folder, with its fixtures; edit --from updates it
proofkin factory report ev-20261124-091500-fin-analyst --json --out D:\src\fin-analyst\agent\reports\ev-20261124-091500-fin-analyst.json --profile alice
```

- `agent:blueprint` and `agent:evals` check the folder's blueprint and suite, each problem
  with its fix. `agent:report` needs a passed, verified report of exactly the folder's build
  in `agent/reports/`. A step proves the folder with
  `verify=agent:blueprint,agent:evals,agent:report`, with read access.

In the web UI, the **Agent factory** card lists your agents and drafts, and starts one from a
template or your words. An agent's dialog has its overview, its blueprint (edited and
checked as you save), **Evaluate** (the estimate first, then live progress) and
**Evaluations** (the gate, each model's results and the failing runs). Other agents draft,
check and evaluate agents [over MCP](#connect-other-agents-mcp-and-a2a); releasing,
deploying and approving stay with you.

**Plan check.** Before a plan runs, it is checked against what exists now:

- connected peers, and the folders they share with you (each folder's checks, your access
  there, worktrees);
- your squads;
- knowledge topics, skills and agents.

`verify=` with the folder's own checks needs full access (the built-in `spec:`, `design:`,
`review:` and `agent:` checks need only read access), `apply=` a `--worktrees` folder, and approvers must be
connected. A plan with errors is not started, and every reason is listed. The web UI
checks plans as you type and shows the problems under the editor.

<a id="github-repositories"></a>

**GitHub repositories.** Sign a profile in with one of your GitHub CLI accounts, then link
repositories. Each is cloned with that account into `~/.proofkin-repos/<profile>/<name>` and
shared as a folder with a worktree per task. The token is fetched from `gh` when needed.
It reaches git only as an HTTP header in the environment: never on a command line or in
the clone's config.

```powershell
gh auth login                                           # once, if you haven't (or github login --new, below)
proofkin github login --gh-user alice --profile alice  # github status / logout
proofkin repo link acme/shop --access full --check test="npm test" --check lint="npm run lint" --profile alice
proofkin peers allow <boss-ticket> --profile alice --delegate --access full --share data,shop
proofkin repo sync shop --profile alice                # fetch + fast-forward
proofkin repo branches shop --profile alice            # p2p/... branches agents' tasks committed
proofkin repo pr shop p2p/boss/12-1794781234 --title "Add CSV export" --profile alice
```

Before a task starts in a linked repository's folder, its agent brings the clone up to date: it
fetches the repository's branch (unless it did in the last minute) and fast-forwards the clone,
so the task starts from the repository's latest commit, and its requester sees `updated folder
'shop' to its repository's latest commit 1a2b3c4 (3 newer commits)`. The clone is left as it is
when GitHub can't be reached, it has another branch checked out, its branch has commits the
repository's hasn't, or the update would overwrite local changes: the task then starts from the
clone's commit, and its result says which one and why. `repo sync` (**Sync** in the web UI)
updates it by hand.

`repo pr` pushes the task branch (only `p2p/...` branches) and opens a pull request into
the repository's default branch with the profile's account. In the web UI the **GitHub**
card does the same: **Sign in to GitHub** (pick a GitHub CLI account), **Link repo**
(with checks, and which peers to share it with), **Sync**, and **Open PR** on each task
branch. Digital workers use their owner's GitHub account. A plan can also open the pull
request itself, as its last step: see
[pull request steps](#pull-request-steps-opening-the-pull-request-from-the-plan).

#### When a task branch is done with

Before listing where each task branch's work is, the card fetches the repository's branch
(`git fetch origin main`) unless the clone fetched it in the last 5 minutes, so "on main"
means on GitHub's main, not on the clone's old copy (when GitHub can't be reached it says
the states are as of the last fetch). A branch is **safe to delete** when deleting it
loses nothing:

- **its commit is on main** (merged as it is, or fast-forwarded);
- **its changes are on main**: main has the same content in every file it changes, merged
  another way (squashed, cherry-picked, or the same change made there);
- or **a pull request merged it** as it is now. A pull request that merged an earlier
  version of the branch doesn't count: the commits after it are only on the branch.

Closed issues, or a closed pull request, don't make it safe: the card then says what isn't
on main yet ("its issues are closed · not on main: 1 commit, 9 files (+213 −100)"), and the
confirmation says what deleting it would lose. **Review changes** shows exactly that: its
commits that aren't on main, the proof a pull request from it would carry, the files it
changes with lines added and removed, and the diff a pull request would show (up to 400 KB),
from where it can open the pull request or delete the branch. The pull request form says
what it would change, with a link back to the review.

Each branch offers what fits where its work is:

- **Safe to delete**: only **Delete branch**. Its work is on main, so there is nothing left
  to review.
- **Its pull request was closed, or its issues are closed** (by another branch's pull
  request, say): **Review changes** and **Delete branch**, whose confirmation says what it
  loses. It isn't proposed again; reopen the pull request on GitHub if you change your mind.
- **Work still to merge** (no pull request yet, or commits added since one was merged or
  closed): **Review changes** and **Open PR**.
- **A pull request open**: **Review changes** only. It is reviewed and merged on GitHub, and
  deleting the branch would close it.

When a pull request from another branch closed its issues, the card names it ("its issues
were closed by #74"): often a later step took over its work (`apply=`) and that step's
branch was merged, with the work changed on the way, so git can't tell it is there.
**Review changes** says so, and the confirmation that deleting it loses its work "unless
#74 (from p2p/boss/19-…), which closed its issues, has it": compare the two, then delete it.

**Another account, or a scope its token lacks, without a terminal.** The GitHub CLI can
hold several accounts per host but refreshes only the active one, so Proofkin runs its
device flow for you and puts the active account back afterwards:

```powershell
proofkin github login --new --profile alice   # sign another account in, in the browser, and use it
proofkin github grant project --profile alice # give the profile's account a scope, e.g. for the tracker's board
```

Each shows a one-time code to enter on GitHub (`https://github.com/login/device`), in a
browser where you're signed in as that account. `login --new` gives the new account's
token the `project` and `workflow` scopes too (`--scope` says which; `--scope ""` only the
GitHub CLI's own), and the profile then uses it. `grant` makes the profile's account the
GitHub CLI's active one for the refresh only if it isn't already, and checks the token
afterwards. Either way the account that was active before is active again when it ends,
also if you cancel (Ctrl-C), it fails, it times out (after 15 minutes) or the agent stops.
In the web UI: **Change account** → **Sign in another account** (with its host, for GitHub
Enterprise), and **Grant access** on the
Tracker card when its board lacks the `project` scope (or on the GitHub card, which lists
what each scope of the account's token is for). The code shows in a dialog with **Copy
code** and **Open GitHub**; closing the dialog cancels.

<a id="pull-request-proof"></a>

**Pull requests carry their proof.** When an agent's runtime commits a task's work, it
keeps the evidence record it signed for the commit with the commit itself, as a git note
(`refs/notes/proofkin`; `git notes --ref=proofkin show <branch>`). Opening a pull request
from that branch checks the record (its signature, and that it is for the branch's
head commit) and ends the description with a **Proof** section: which agent made the
commit and for whom (the name your allowlist has for the requester, with its key's
start), what the work was part of if your audit log has the task (for example "as step
`ui` of boss's evidence run ..."; ask that agent for the run and re-check it with
`proofkin runs verify --file`), its key, and the checks its runtime ran on the change with
their results (never their commands). If your profile has the evidence run that holds the
record (a plan or task you ran), its verdict and claims are added. The web UI's pull
request form shows the proof before you open it (**Add the proof**, on by default);
`repo pr ... --no-proof` leaves it out. Opening the pull request also publishes its head
commit's record, and only that one, to the repository's notes ref
(`refs/notes/proofkin`) to the repository, so others can fetch the record and re-check it:
`git fetch origin refs/notes/proofkin:refs/notes/proofkin`. If that push fails, the pull
request still opens and the proof says the record stays on your machine. Only that pull
request's record is published; other records in the clone stay local.

<a id="tracked-work"></a>

**Tracked work: issues and a GitHub Project.** Link a tracker and every plan run you start
is tracked on GitHub, where your team already tracks work:

- the run gets an **issue**, and each step a **sub-issue** of it, assigned to your GitHub
  account. A step's issue is *blocked by* the issues of the steps it runs after;
- on the project's **board**, each item says which agent does it (an `Agent` text field,
  added if the board has none), and its **Status** follows the run: *In progress* while the
  step runs, *In review* while its commit waits for a pull request (or a sign-off waits for
  its person), *Done* once closed. A step that changed no files is closed when it is done;
  a failed or skipped step goes back to *Todo*. Status options are matched by name
  (`Todo`/`Ready`/`Backlog`, `In progress`/`Doing`, `In review`/`Review`, `Done`, with or
  without emoji); without a review column, work waiting for review stays in progress;
- results and failures are **commented** on the issues with their signed evidence (signer,
  checks, commit and branch), and the run's issue gets the run's verdict and claims;
- each step's task carries its issue (and those of the steps whose work it applies). The
  worker's commit references them (`Refs: acme/shop#15` trailers), `repo branches` and the
  web UI show them, and a pull request from the branch **closes** them (`Closes #15`), so
  GitHub links the pull request to the issues and closes them when it merges. The run's
  issue waits *In review* until then (or, if it has other open sub-issues, where it was)
  and is closed once all its sub-issues are (checked every 10 minutes by a running
  agent, or now with `tracker sync`). The **Tracker** card lists those run issues under
  *Left open until their sub-issues close*: each run with how many of its sub-issues are
  closed, and the open ones with the pull request that will close them, or *no pull request yet*.
  **Check now** (`tracker sync`) closes what can be closed at once and says what is still
  open; `tracker status` prints the same list.

```powershell
proofkin github login --gh-user alice --profile boss
proofkin github grant project --profile boss   # once: the board needs the project scope (or Grant access on the Tracker card)
proofkin tracker link acme/shop --project https://github.com/users/alice/projects/4 --profile boss
proofkin tracker status --profile boss   # the board's statuses, or why it isn't updated
proofkin tracker items --profile boss    # what is on the board, by status, with agents and pull requests
proofkin tracker add --open --closed --profile boss   # put the repository's existing issues and pull requests on the board
proofkin tracker sync --profile boss     # close run issues whose pull requests have all merged
```

In plan text, `track:` says what the run works on, and `issue=` what a step works on:

```text
track: #12
claim: C-1 Customers can export orders as CSV
api @alice:shop verify=test claims=C-1 issue=#15 Add the CSV export endpoint
ui after api @alice:shop apply=api verify=test claims=C-1 Add an Export button to the orders page
signoff after ui approve @me claims=C-1 Try the export and approve it
```

Here the run works on issue #12 (reopened if it is closed; `track: <title>` makes a new issue with that title,
`track: off` leaves the run untracked; without a `track:` line the run gets a new issue
named after its first claim). Step `api` works on #15, made a sub-issue of #12 if it isn't
one; `ui` and `signoff` get new sub-issues. A later run with `track: #12` takes up its
steps' open sub-issues again. In the web UI the **Tracker** card links and shows the
tracker, and **Board items** lists what is open by status; **Plan it** starts a plan on an
item, with a step per open sub-issue. **Add existing issues** (`tracker add`) puts the
repository's issues and pull requests on the board, with a status from their state (open
issues to do, open pull requests in review, closed and merged ones done; statuses already
set stay). Tracked steps link their issue, and a plan its run's.

GitHub sometimes answers a batch of board adds with an error after carrying out some or
all of them (an HTTP 502), and a board whose **Auto-add sub-issues** workflow is on puts
a run's sub-issues on it by itself, so adding them again says "Content already exists in
this project". So an add that fails is looked up on the board before it counts as a
problem, and what is still missing is added again, five to a request. Queries (never
changes) that GitHub answers with a 502, 503 or 504 are asked again, twice at most.

Without the `project` scope tracking keeps to the issues (the `repo` scope is enough) and
says how to grant it. Tracking never fails or holds up a plan: it runs beside it, and what
goes wrong is reported with the run. The issues get the steps' own words and results
(redacted, and cut to a few thousand characters), so track private work in a private
repository.

GitHub's own tools work with the same issues and boards. The GitHub CLI has
`gh project item-list|item-add|item-edit|field-list` (`item-edit --url <issue> --field
Status --value Done`) and `gh issue create|edit` with `--parent`, `--add-sub-issue`,
`--blocked-by` and `--add-assignee`. The
[GitHub MCP server](https://github.com/github/github-mcp-server)'s `projects` toolset
(`projects_list`, `projects_get`, `projects_write`) and `issues` toolset (`issue_read`,
`issue_write`, `sub_issue_write`) give a model the same, for example as a folder's MCP
server for agents that plan or triage work:

```powershell
proofkin mcp add shop github --url https://api.githubcopilot.com/mcp/x/projects `
  --header "Authorization=Bearer <token with the project scope>" --description "GitHub Projects" --profile alice
```

**Knowledge base.** Each profile has a private knowledge base of Markdown topics.

- A step with `save=topic` stores its result there. Only verified results are stored, with
  the run and peer they came from.
- A step with `context=a,b` (or `context=all`) gets those topics in its prompt.

So reverse-engineered architecture, risks, designs and insights build up across runs.

```powershell
proofkin kb list | show <topic> | add <topic> <file.md> | remove <topic> | search <words> --profile boss
```

**Importing from Microsoft 365.** `proofkin kb import workiq <topic> --query "..."` searches
Work IQ and keeps what it cites as a topic for `context=`:

```powershell
proofkin kb import workiq invoice-retention --query "invoice retention decisions" `
  --from meetings,files --profile alice
```

- `--from` takes the sources of a step's [`workiq=`](#microsoft-365-search-in-a-step-workiq)
  option (`all` by default, or a list of `email`, `meetings`, `files`, `teams`, `people`,
  `connectors`) with the same rules.
- The topic is marked as Microsoft 365 content with the labels of the results kept (`unknown`
  for a result without label information), so a peer only gets it if its `--m365` grant
  includes those labels. Its front matter also has `query:` and `retrieved:` (the date), and
  it has the source note `Work IQ retrieve`.
- An existing topic is only overwritten with `--replace`. If every result was withheld
  (blocked, or without a protection status), nothing is written and the error says how many
  were withheld and why.
- The search counts in the [audit file and budgets](#work-iq-audit-file-and-budgets) as a
  knowledge import. The web UI's API does the same with `POST /api/kb/import/workiq`
  (`topic`, `query`, `from`, `replace`).

**Operations.** On the machine where a service runs, its owner adds monitors: in the web
UI (**Health → Add monitor**, which offers the folders' checks) or in a terminal. A monitor
runs one of the folder's checks on a schedule, while that profile's agent runs. A running
agent starts, restarts or stops monitors added, changed or removed within seconds; no
restart needed.

```powershell
proofkin share add svc D:\svc --access full --check health="curl -fsS http://localhost:8080/health" `
  --check restart="pwsh -File restart.ps1" --profile alice
proofkin monitor add api --share svc --check health --every 60 --fail-after 3 --slo 99.5 --profile alice
```

- Every run is a **metric** (ok, latency, exit code) in the profile's `ops/metrics.jsonl`.
- `fail-after` failures in a row open an **incident** (`ops/incidents.jsonl`). The agent
  **alerts** its terminal and the peers on its allowlist by chat; a peer that isn't
  connected gets the alerts it missed when it connects (the last 20, up to a day old, while
  the agent runs). An incident stays open until the monitor passes again, also across
  agent restarts, so the same failure never alerts or runs the debugging agent twice.
- A monitor whose folder or check no longer exists **can't run**: it is skipped (no
  samples, no incidents) and the report and **Health** panel say what to fix, or to remove
  it (its trash button in **Health**, or `proofkin monitor remove`). Removing a monitor
  closes its open incidents; its history stays. Monitors use the folder's checks as they
  are now.
- **Auto-fix with TSGs.** A troubleshooting guide is a knowledge topic with front matter
  (`tsg`, `monitor`, `match`, `fix`, `verify`, `status`). The fix check of every active
  guide whose monitor matches, and whose `match` text appears in the failure, is run, then
  the verify check.
- **Debugging agent.** If no guide fixes it and the agent has a provider, its model
  investigates read-only in the folder, using the knowledge base. It ends with
  `Confidence: N%` and `Fix: <check>`.
  - At 80% or more confidence, that check runs. If the monitor then passes, the diagnosis
    is saved as a new **active TSG**, so a repeat is fixed without the model.
  - Otherwise a **draft TSG** is written for a person to review, and the incident is
    escalated.
- Fixes can only be checks the owner defined: neither a model nor a peer can run anything
  else.

```powershell
proofkin ops report --hours 24 --profile alice    # availability vs SLO, p50/p95, incidents, tasks, runs
proofkin ops incidents --profile alice
proofkin kb learn --profile alice                 # ops report -> knowledge topic ops-summary
```

**Learning.** Agents with monitors refresh `ops-summary` in the knowledge base every hour.
TSGs learned from incidents accumulate, and the `learn` playbook analyzes it all
(`context=all`) into insights and new TSG drafts. In the web UI, the **Health** panel shows
monitors against their SLOs (with what each checks, how often), open incidents, the
incident log and task statistics; **Add monitor** and each monitor's trash button add and
remove monitors. **Knowledge** lists the topics and TSGs.

### Spec-driven development

Write down and agree on what to build before anyone builds it: a **spec**, a **REST API
contract** and a clickable **UX prototype**, each checked by Proofkin and accepted by
people. Development then starts from evidence, with one claim per acceptance criterion.

A **spec package** lives in the project folder:

| File | What it holds | Checked by |
| --- | --- | --- |
| `spec/SPEC.md` | Problem, users, user stories (`- US-1: As a ..., I want ..., so that ...`), acceptance criteria tied to a story (`- AC-1 (US-1): Given ..., when ..., then ...` or `... must ...`), non-functional requirements (security and privacy included), out of scope, open questions | `spec:spec`: every story has a criterion, every criterion names an existing story and is testable, ids are unique, security and privacy are covered |
| `spec/openapi.json` | The REST API contract (OpenAPI 3, JSON). Each operation lists the criteria it serves: `"x-acceptance": ["AC-1"]` | `spec:api`: `operationId` and summary, a 2xx and a 4xx response, a JSON request body for POST/PUT/PATCH, and every example matches its schema (types, required properties, enums, `$ref`) |
| `prototype/index.html` | A self-contained, clickable prototype that uses the API's examples as data. Screens are marked with what they show: `data-story="US-1" data-ac="AC-1 AC-2"` | `spec:ux`: no external resources and no server calls; every story is shown |
| `evidence/claims.json` | A claim per acceptance criterion (`"id": "AC-1"`), which development proves with tests | `spec:trace`: every criterion is covered by the API or the prototype and claimed; nothing refers to an unknown id |

Start from a working example, edit it, and check it as you go:

```powershell
proofkin spec init "Wishlist" --dir D:\src\shop    # a starter package that passes every check
proofkin spec check --dir D:\src\shop              # or: spec check api|spec|ux|trace
```

Or let agents write it. The `spec` playbook runs four steps on a peer, each in the
folder's worktree and building on the previous step's commit:

1. **spec**: `write-spec` skill, `verify=spec:spec`;
2. **api**: `api-contract` skill, `verify=spec:spec,spec:api`;
3. **ux**: `prototype-ux` skill, `verify=spec:ux`;
4. **trace**: `spec-to-claims` skill, all four checks.

Each step gets up to three attempts, each told what failed. The product owner then
accepts the spec and prototype, and the tech lead accepts the API contract.

- **Built-in checks** (`spec:spec`, `spec:api`, `spec:ux`, `spec:trace`) run in the
  worker's runtime after the model, like the folder's own checks, and are signed into
  the evidence. They only read files, in the runtime itself, without following symbolic
  links. So a peer may ask for them with any access, and no folder has to define them.
- **Prototype preview.** A step whose commit creates `prototype/` files gets an **Open
  prototype** button in the web UI. An approval step after it sends the approver the
  prototype, so the product owner (you, or a teammate on their own agent) clicks through
  it from the approval card before answering. The same works from a saved run's report,
  and for a folder you share (**Preview prototype**).
  - Previews open in a new tab at an unguessable link that expires after 2 hours.
  - They run in a CSP sandbox: an opaque origin, no network, no form posts. A prototype a
    model or a peer wrote can't reach your agent's API or token.
- **Then build it.** Merge the spec branch (`repo pr`, or `git merge` the `p2p/...`
  branch), then run `implement-spec`. The model builds the API exactly as the contract
  says and the UI as the prototype shows, and names a test for every criterion in
  `evidence/claims.json`. `test`, `lint`, `spec:api` and `spec:trace` must pass. A
  reviewer squad applies the change on its own copy and checks it independently, then
  someone signs off.

```powershell
proofkin share add shop D:\src\shop --access full --worktrees --check test="npm test" --check lint="npm run lint" --profile alice
proofkin playbook explain spec
proofkin playbook show spec --set peer=alice --set folder=shop --set change="Shoppers save items to a wishlist" `
  --set change_id=wish --set product_owner=carol --set tech_lead=me > wish.plan
proofkin playbook show implement-spec --set peer=alice --set folder=shop --set change="Wishlist" --set reviewers=qa > build.plan
```

### Spec Kit and OpenSpec

Already writing specs with [Spec Kit](https://github.com/github/spec-kit) or
[OpenSpec](https://github.com/Fission-AI/OpenSpec)? Import a feature as an evidence plan:
its requirements become the plan's claims, and peers implement it with a test per claim.

| You give | Claims (proven by the folder's tests) | Signed off |
| --- | --- | --- |
| A Spec Kit feature, `specs/NNN-name/` (`spec.md`, `plan.md`, `tasks.md`; the project's `.specify/memory/constitution.md`) | `US-1`... per user story (*each of its acceptance scenarios passes*), `FR-001`... per functional requirement | `SC-001`... per success criterion |
| An OpenSpec change, `openspec/changes/<id>/` (`proposal.md`, `tasks.md`, `design.md`, deltas in `specs/<capability>/spec.md`) | `<capability>-1`... per ADDED or MODIFIED requirement (REMOVED and RENAMED ones become instructions) | `ACCEPT`: it does what the proposal says |
| An OpenSpec capability, `openspec/specs/<capability>/` | `<capability>-1`... per requirement | `ACCEPT` |

The plan has three steps: **impl** on the peer you pick (`implement-change` skill, the
folder's `test` check, up to 3 attempts, the tasks done in order and ticked off),
**verify** by a reviewer squad on its own copy of the commit, and **signoff** of what
tests can't prove. The spec itself (condensed to fit a prompt, the constitution included)
is saved as knowledge topic `spec-<feature>` (or `openspec-<change>`) and given to both
model steps, so peers need no copy of it. Open questions (`[NEEDS CLARIFICATION]`), a
missing `plan.md` or `tasks.md`, and stories without acceptance scenarios are flagged as
warnings at the top of the plan. Only regular files inside the feature's folder (and the
project's `.specify`) are read, at most 1 MB each: links out of it, devices and pipes are
refused, since what is imported goes to peers.

```powershell
proofkin spec import D:\src\shop\specs\001-photo-albums --profile boss --set peer=alice --set reviewers=qa > albums.plan
proofkin spec import D:\src\shop --profile boss --set peer=alice     # a project with one feature (or it lists them)
proofkin spec import ~/shop/openspec/changes/add-dark-mode --profile boss --set peer=alice --out dark.plan
```

In the web UI, pick the **implement-speckit** playbook in Plan mode and give the feature's
folder; for a folder with several features it offers them to pick from. Other agents can
import too, with the `import_spec` MCP tool ([Connect other agents](#connect-other-agents-mcp-and-a2a)).

### Design and reviews

Between an accepted spec and its implementation, the `design` playbook produces the
SDLC artifacts reviewers need. Every artifact is proven by a built-in check and traced
to the spec. Then each audience reviews it in its own form.

**The design package** lives in `design/`. Diagrams are Mermaid: they render on GitHub
(in the pull request) and in VS Code.

| File | What it holds | Checked by |
| --- | --- | --- |
| `architecture.md` | Context diagram, components (table and component diagram), domain model (UML class or ER diagram), deployment, quality attributes with targets, and the list of decisions | `design:arch` |
| `workflows.md` | A UML sequence diagram per workflow, each named after the user story it implements (all stories covered); state or activity diagrams | `design:arch` |
| `adr/ADR-0001-*.md` | Architecture decision records: status (Proposed, Accepted, ...), context, decision, alternatives considered, consequences | `design:arch` |
| `data-dictionary.md` | Every field (every property of the API's schemas, and what is stored besides): type, classification (Public, Internal, Confidential, Personal, Sensitive) and, for personal data, purpose and retention | `design:data` |
| `dataflow.md` | The data flow diagram with every trust boundary as a subgraph, and a table of flows (`DF-1`): what data moves, whether it crosses a boundary, how it is protected. Every entity with personal data is in a flow | `design:data` |
| `threat-model.md` | Assets, trust boundaries and STRIDE threats (`TM-1`): element, category, threat, mitigation, risk, status. All six categories, every boundary flow covered, no open high risk | `design:threats` |
| `test-plan.md` | A test for every acceptance criterion and every high-risk threat, at a level that proves it (criteria the API serves need contract, integration or e2e tests), plus how performance and security are tested | `design:tests` |
| `operations.md` | Rollout (flag, rings), rollback, an SLO with a target, metrics, dashboard, an alert table with actions, and runbooks | `design:ops` |

**The review pack** lives in `review/`. The decks are [Marp](https://marp.app) Markdown:
slides separated by `---`, speaker notes in `<!-- -->` comments.

| File | Audience and tone | `review:decks` holds it to |
| --- | --- | --- |
| `technical.md` | Engineers and architects: precise, evidence-backed, decision-focused | at most 30 slides; architecture, ADRs, API, data, threats, tests, operations, open questions; cites existing ADR and threat ids |
| `product.md` | Product owner and PMs: user value, prototype walkthrough, scope, measurable success | at most 20 slides; problem, every user story, prototype, scope, success metrics, release plan, decisions needed |
| `leadership.md` | Leadership team: bottom line up front, outcomes, risks, one clear ask | at most 12 slides; summary first, then impact, timeline, risks and ask; under 91 words a slide; no code, diagrams source or internal ids |
| `customer-feedback.md` | Customers: warm, plain, about them, honest about scope | what we heard, what we're building, how it helps, what's not included, at least 3 questions, next steps; no internal ids, security details or email addresses |

Start from a working example, or let agents write it:

```powershell
proofkin spec init "Wishlist" --dir D:\src\shop
proofkin design init "Wishlist" --dir D:\src\shop     # design/ and review/ starters that pass every check
proofkin spec check design --dir D:\src\shop          # or: spec check (everything present), threats, review
proofkin design render --dir D:\src\shop              # review/html/index.html: decks as slide shows, documents as pages
npx @marp-team/marp-cli D:\src\shop\review\leadership.md --pptx   # a PowerPoint, if your reviewers want one
proofkin playbook show design --set peer=alice --set folder=shop --set change="Wishlist" --set change_id=wish `
  --set tech_lead=bob --set security_champion=sam --set privacy_approver=pat --set product_owner=carol --set leader=lee > design.plan
```

The `design` playbook runs on a peer, in the folder's worktree, each step building on
the previous one's commit:

1. **arch**: `architecture-design` and `adr` skills, `verify=design:arch`.
2. **data**: `data-design`, `verify=design:data`.
3. **threats**: `threat-modeling`, `verify=design:threats`.
4. **plans**: `test-strategy` and `operability-design`, `verify=design:tests,design:ops`.
5. **pack**: the four review skills, all six checks.

Then the reviews:

- The tech lead accepts the design (technical deck).
- The security champion accepts the threat model.
- The privacy approver accepts the data handling.
- The product owner accepts the product review and approves sharing the customer
  document.
- Leadership reviews last.

Every approval request carries the **review pack**. The approver opens it from the
request (**Open review pack**), or from the step or saved run in the web UI:

- the decks as slide shows: arrow keys, **Speaker notes**, **All slides**, **Print / PDF**;
- the design documents;
- the prototype.

It is served in the same sandbox as prototypes. Decks and documents are rendered
without their raw HTML, so a model's or a peer's Markdown can't run script. Shared
folders with `design/` or `review/` offer **Preview review pack** too.

#### Rehearse the reviews first

The `rehearse` playbook runs a dry run of the reviews before you book them:

1. Six agents on a squad of reviewers each play one stakeholder, all at the same time:
   the tech lead, the security champion, the privacy champion, the product owner,
   leadership and a customer. Each reads the review pack and answers in role, following
   the `stakeholder-simulation` skill: likely verdict, hardest questions, concerns with a
   severity, and what would convince them.
2. A peer merges their feedback into `review/rehearsal.md`, following the
   `feedback-synthesis` skill. The rehearsed reviews reach it as external content, never
   as instructions.
3. You decide what to fix before the real reviews.

The built-in check `review:rehearsal` proves the rehearsal is usable:

- readiness (ready, ready with changes or not ready);
- a section for each of the six audiences, with its likely verdict;
- an actions table where every concern has a severity (blocker, major, minor) and an
  action;
- at least three prepared answers to the hardest questions.

`spec check` runs it once a rehearsal exists, and `design init` includes an example.

```powershell
proofkin playbook show rehearse --set peer=alice --set folder=shop --set change="Wishlist" --set reviewers=qa > rehearse.plan
```

### Options

| Option | Default | Description |
| --- | --- | --- |
| `--name <NAME>` | required | Agent name shown to peers |
| `--profile <NAME>` | `--name` | Profile whose identity, allowlist and provider accounts this agent uses |
| `--network <MODE>` | `internet` | `internet` (relays, NAT traversal, lookup by id) or `local` (direct IPs only) |
| `--listen <ADDR>` | any port | UDP address to bind; required with `--network local` (use a concrete IP) |
| `--peer <TICKET-OR-ID>` | none | Allowlisted peer to dial at startup; repeatable. Allowlisted peers are also dialed automatically |
| `--providers <LIST>` | none | Comma-separated `copilot,claude,codex` to enable on this agent |
| `--serve` | off | Run tasks that allowlisted peers with delegation rights send |
| `--workspace <DIR>` | `.` | Folder **your own** tasks (`/ask`, *This agent* in the UI) work in. Never given to peers |
| `--access <MODE>` | `read-only` | Most your own tasks may do there: `read-only`, `workspace-write` (edit files) or `full` (also shell). Peers' access is set per peer and folder ([Shared folders](#shared-folders)) |
| `--bridge <PATH>` | `bridge/index.mjs` in this repo (env `PROOFKIN_BRIDGE`) | Node bridge script |
| `--ui` | off | Open the [web UI](#web-ui) instead of the terminal prompt |
| `--ui-port <PORT>` | random | Port for the web UI on 127.0.0.1 (needs `--ui`) |
| `--no-open` | off | Print the web UI link without opening a browser (needs `--ui`) |

Subcommands: `proofkin auth login|status|logout`, `proofkin peers id|allow|list|remove`,
`proofkin share add|list|remove|team`, `proofkin mcp add|import|list|pause|resume|remove`,
`proofkin plugin add|list|show|update|enable|disable|remove`, `proofkin worker create|list|remove|memory`,
`proofkin squad create|list|remove`, `proofkin runs list|show|verify`, `proofkin kb list|show|add|remove|search|learn`,
`proofkin playbook list|explain|show`, `proofkin schedule add|list|pause|resume|remove`, `proofkin skill list|show|add|import|remove`, `proofkin agent list|show|add|remove`, `proofkin factory templates|new|list|show|edit|check|build|evaluate|report|init|delete`, `proofkin github login|grant|status|logout`, `proofkin models list|default|allow|effort`,
`proofkin repo link|list|sync|branches|pr|unlink`, `proofkin tracker link|status|items|add|sync|unlink`, `proofkin monitor add|list|remove`, `proofkin ops report|incidents`, `proofkin spec check|init|import`, `proofkin design init|render`, `proofkin connect`, `proofkin audit`, `proofkin profile delete`. `PROOFKIN_HOME` moves the profiles folder
(and uses a separate credential store namespace). `PROOFKIN_SCHEDULE_TICK_SECS` sets how
often an agent looks for due schedules (default 30), and `PROOFKIN_TASK_GRACE_SECS` how long
tasks wait for a peer that dropped off to come back (default 60, see
[Cancelling tasks](#cancelling-tasks)). For tests only, `PROOFKIN_TEST_WORKIQ_URL` and
`PROOFKIN_TEST_ENTRA_URL` point Work IQ and its sign-in at fakes on this machine; they take
only a loopback address such as `http://127.0.0.1:8080` and refuse anything else, so no token
can go elsewhere. Each setting also works under its
name from before the rename (`P2PAGENTS_HOME`, ...).

## Web UI

Add `--ui` to any agent to drive it from a browser instead of the terminal prompt:

```powershell
proofkin --name me --providers copilot --serve --ui
# web UI: http://127.0.0.1:53817/#t=3f9c…   (opens in your default browser)
```

- **Connected peers** with the providers they serve and how busy they are ("idle",
  "1/2 busy", "2/2 busy · 1 waiting"; hover for how many of those tasks are yours), live.
- **Allowlist**: add a peer by pasting their ticket (optionally letting them delegate, with an
  hourly limit and an access level: read only, edit files, or full, and with their GitHub
  account, which [pull request steps](#pull-request-steps-opening-the-pull-request-from-the-plan)
  ask to review), change what one may do or its GitHub account (**Change…**: it stays
  connected), or remove one; changes are saved to the profile's `peers.json` and take
  effect at once (removed peers are disconnected).

  The same dialog sets a peer's Microsoft 365 grant, as `peers allow --m365` does: tick
  **Let this peer receive Microsoft 365 content (Work IQ)** and enter the sensitivity labels
  it may receive, comma separated (`unlabeled` allows content with no label; content without
  label information never goes to a peer), and tick **Its tasks may also use this agent's
  Work IQ (read only)** for `--m365-tools`. **Change…** shows the peer's grant, and clearing
  the first box removes it; adding a peer again without ticking it keeps the one it had. The
  list shows a granted peer's grant ("M365: General, Public · Work IQ tools"); see the
  [Security model](#security-model).
- **Digital workers**: create [digital workers](#digital-workers) that run on your
  sign-ins (sign in to Copilot right there with a GitHub CLI account), in the folders you
  pick; start and stop them, choose whether they start with this agent, set their
  **Models…** (your settings, or their own), read why one
  stopped on its own, and delete them (they leave your allowlist and squads).
- **Your ticket** with a copy button, plus this agent's profile, network, providers, accounts,
  its [models](#models) with **Models…** to choose the ones peers may pick,
  access level and workspace, and its shared folders, each with a **Team plan…** button (for
  folders with edit or full access) to set or remove the folder's
  [team plan](#team-steps-handing-work-down-a-level), or have its model write one for each
  team step, approved by you first if you like.
- **GitHub**: **Sign in to GitHub** with a GitHub CLI account; once signed in, **Change
  account** next to "Signed in as …" switches to another one (the account in use is
  preselected), or **Sign in another account** with a code entered on GitHub. The card
  says what each scope of the account's token is for, with **Grant access** when the
  tracker's board needs the `project` scope, and names the GitHub CLI's active account
  when it is another one. Task branches show the issues their pull request closes, and
  **Open PR** fills in `Closes #N` for them. Each branch says where its work is and
  whether deleting it loses anything: "merged in #7", "its commit is on main" or "its
  changes are on main" (merged another way, e.g. squashed) read **safe to delete**;
  otherwise it says what isn't on the repository's branch yet ("its issues are closed ·
  not on main: 1 commit, 9 files (+213 −100)"). **Review changes** shows its commits, the
  proof a pull request would carry, its files and its diff, with **Delete branch…** and
  **Open pull request…**. The card offers only **Delete branch** once nothing would be
  lost, **Review changes** and **Delete branch** once its pull request or its issues are
  closed, and **Open PR** only while there is work to merge (see
  [task branches](#when-a-task-branch-is-done-with)).
- **Work IQ**: the [Work IQ](docs/work-iq.md) account this agent uses for Microsoft 365 work
  context and how it connects ("ada@contoso.com through the Work IQ CLI"), and today's calls
  against its [budgets](#work-iq-audit-file-and-budgets) ("Today: 3 of 200 Work IQ calls (at
  most 25 per run)"). **Sign in…** signs in through the Work IQ CLI, as
  `auth login workiq --cli` does: an account the CLI has signed in (empty: its default
  account) and, when the CLI isn't found, where it is; if the sign-in fails, the dialog says
  why and keeps what you typed. Your organization's own app (the hosted connection) signs in
  from a terminal instead: the dialog shows the command to copy. **Check** runs the
  [doctor](#checking-the-work-iq-connection) and lists each check (passed, failed or
  skipped) with its detail and, for the one that failed, the fix and who makes it (you, an
  admin or support); its last check is one real call, counted against the budget.
  **Budget…** sets the calls a day and per run, and **Sign out** asks first (the budgets and
  the audit file stay). A digital worker that uses its owner's account shows "Uses alice's
  account: …" with only **Check**; one its owner hasn't allowed to says how the owner turns
  that on. (API: `GET` and `DELETE /api/workiq`, `POST /api/workiq/sign-in` with
  `{"account", "cli_path"}`, `POST /api/workiq/doctor`, and `PUT /api/workiq/budget` with
  `{"per_day", "per_run"}`.)
- **Tracker**: link the repository (and project board) your plan runs are tracked on, see
  the board's statuses or why it isn't updated (a missing scope with **Grant access**, and
  the commands to run in a terminal instead; anything else with the command that fixes
  it, to copy),
  **Board items** (what is open, by status, with **Plan it**) and **Add existing issues**
  (the repository's issues and pull requests onto the board). A tracked plan's card
  links its issue, and each step its sub-issue (see
  [tracked work](#tracked-work)).
- **Task cards** show up to four lines of the task; **Show all** / **Show less** expands a
  longer one.
- **Activity feed**: chat with all connected peers, joins and leaves, refused connections
  (with **Allowlist…** for a teammate who allowlisted you first: their endpoint id is
  filled in; only add people you know), tasks peers delegate to you.
- **Run task**: pick a provider and where to run it (*This agent*, a named peer, *Any peer*
  when several serve it, or a squad: its least busy member that can run it, moving to another
  member if that one turns it down), and the peer's folder. **Model** and **Reasoning** list
  what that peer lets you pick (its defaults selected; see [Models](#models)). If that folder offers MCP servers, tick
  the ones the task may use (each shows what its owner says it is for). Where you have
  full access to the folder, tick the checks its runtime runs after the model
  (**Verify with**, and how many attempts it gets while they fail): the task is proven
  only if they pass, and it is kept under **Evidence runs**.
  The **Tasks** panel shows status and elapsed time, what the peer is doing right now
  ([live status](#live-status)), renders results as Markdown, can copy
  results and errors, has a **Cancel** button on running tasks, and links each plan step,
  and each task kept as evidence, to its **Evidence run**. A plan is one card with a bar of
  its steps, its steps in the order they run, **Cancel plan** and **Evidence run**; a
  [team step](#team-steps-handing-work-down-a-level) shows its lead's team steps under it,
  live. The latest 200 tasks are kept
  in the profile (`tasks.json`, long results cut at 32,000 characters) and come back
  after a restart; one the agent stopped in the middle comes back cancelled, saying so,
  but a plan's steps come back running: the plan goes on (see
  [squads and plans](#squads-and-plans)).
  **Clear finished** lets the finished ones go (running ones stay).
- **Plan** mode: write a [plan](#squads-and-plans) in the composer (Enter adds a line,
  Ctrl+Enter runs it), or press **Open editor** for a full-size editor with two views of
  the same plan:
  - **Builder** (forms, no syntax to learn): what must be true (claims), then a card per
    step, an agent task, a team task (handed to a peer's [team](#team-steps-handing-work-down-a-level);
    only folders that take team steps are offered) or a sign-off: who does it (connected peers, their folders and
    squads), what to do, the [agent](#agents) it acts as (with what that agent brings),
    skills (picked from a list with what each is for), the checks
    the folder offers (and the built-in ones), the claims it proves, the steps it waits
    for, and more options (tries, model, knowledge, MCP servers, when to cancel it, its
    issue when runs are [tracked](#tracked-work), and how
    far down a team task may go). It can't hold the
    usual mistakes: a task proves claims only once it has a check, a step can't wait for
    one that waits for it, renaming or removing a step or claim updates what refers to
    it, and what a card still needs is said on its field. **Run plan** goes to the first
    field still missing.
  - **Text**: the plan as text, with everything it can use beside it. A click adds to
    the step your cursor is on, in the place the plan syntax expects (skills you click
    add up, who runs it is replaced), or starts a new step on an empty line, then
    selects what to type next.

  The plan is checked as you go, and problems with a clear fix come with a button that
  makes it (in the builder, on the card they are about).
  Steps show which steps they wait for, and skipped steps are flagged. **Schedule…** runs
  the plan on its own at set times ([scheduled plans](#scheduled-plans)). With a
  [tracker](#tracked-work) linked, **Track on GitHub** beside it chooses where the run is
  tracked: a **New issue** (with a sub-issue per step), an **Existing issue…** it works on
  (by number), or **Don't track** (it writes the plan's `track:` line). The line under it
  says what running the plan will do there: the repository and board (links), the account
  the issues are assigned to, and that anyone can read them when the repository is public.
  An existing issue is looked up as you type its number: its title, and a warning when it
  is closed (running reopens it) or doesn't exist (the plan would run untracked). Run plan
  and Schedule… ask for the number when it's missing. "What is a
  plan?" stays folded until you open it.
- **Schedules**: each schedule in plain words, its next run, its last run's result (with
  a link to the evidence run), and Run now, Pause/Resume, Edit and Delete.
- **Plugins & tools**: import [plugins](#plugins-skills-agents-and-mcp-servers-packaged-together)
  (review what one contains first; importing runs nothing), offer, pause, change or remove
  [MCP servers](#mcp-servers-owner-controlled-tools-for-peers-tasks) in your shared
  folders (by hand, from an MCP configuration file or from a plugin), and add or remove
  your own skills and agents.
- **Agent factory**: your [factory agents](#the-agent-factory) and drafts: start one from a
  template or your words, edit and check its blueprint, evaluate it (the estimate first, then
  live progress) and read its evaluations (the gate, each model's results, the failing runs).
- **Connect other agents**: let agents on this computer (VS Code, Claude Code, any MCP or
  A2A client) hand work to your peers: turn it on or off, copy the addresses and ready-made
  client setups, show or replace the token ([Connect other agents](#connect-other-agents-mcp-and-a2a)).
  Their tasks show up under **Tasks**, marked *asked by another agent*.
- **Squads**: create, view (members, who is online, running tasks, what each runs), change
  (**Change…**: who is in it) and remove squads. The squad dialog says what each peer would
  take as a member (its providers with their default model and reasoning, and the folders
  it shares with you, marked when they have a team plan), or why no step can go to it: an
  agent that only delegates (no providers, no team plan) takes none, and the card marks it
  **takes no steps**. A squad whose members all take none isn't saved.
  Digital workers show their owner and role in the peer list.
- **Waiting for your approval**: approval requests from plans (yours or teammates'), with
  what you're approving and Approve / Reject (a rejection needs a reason), and **Ask for
  changes** where the sign-off allows another round (it needs a note saying what to change).
- **Evidence runs**: every plan run, and every task kept as evidence, with what it was
  about and its verdict. Open one to read its report (claims, checks, commits, signatures)
  and a fresh signature check, or **Download** its `run.json` for anyone to verify
  (`proofkin runs verify --file`). Its work can go in a pull request from there
  ([after the run](#pull-request-steps-opening-the-pull-request-from-the-plan)), or a new
  plan can start from it before it is merged (**Build on it…**, `base=RUN:STEP`). Results
  that carry valid signed evidence show **Signed ✓**.
- **Peer task log** (agents with `--serve`): the [audit log](#audit-log), updated live.
- **Get started**: a new agent (nobody on its allowlist yet) shows its first steps in
  place of the empty conversation: share your ticket, add a teammate's, or create a
  digital worker.
- Times show their day when it isn't today ("Yesterday, 9:25 PM", "Nov 18, 9:25 PM").
- A task's prompt and a team step's text show as Markdown (code spans, bold, lists,
  paragraphs; markup in the text stays text). A long prompt is cut at about four lines
  with a fade until **Show all**.
- **Light, dark or System** theme (follows the OS live; the choice is remembered). Keyboard
  accessible, screen-reader labelled, WCAG AA contrast in both themes, works on phone-sized
  windows, honours reduced motion. From 1180 px wide down the page scrolls instead of
  squeezing the composer, while the conversation and its composer stay in view. Anything
  that deletes or withdraws something asks first, in the page, saying what stays.

The UI follows the written [UX design guidelines](docs/ux-guidelines.md) (principles,
layout, tokens, components, states, forms, microcopy, accessibility and a definition of
done for UI changes).

Security: the UI listens on `127.0.0.1` only. Each run creates a new random access token,
passed in the link's `#t=` fragment (never sent in requests or logged; the page moves it to
session storage and removes it from the address bar). API calls without it get `401`.
Requests whose `Host` isn't `127.0.0.1`/`localhost` are refused (DNS-rebinding defense).
Pages are served with a strict Content-Security-Policy and all peer text is rendered as
text, never HTML. Restarting the agent invalidates old links; use the new link it prints.
Press Ctrl+C in the terminal to stop the agent.

## Connect other agents (MCP and A2A)

Other agents on your computer can hand work to your peers too: your editor's agent (VS
Code, Claude Code, any MCP client) or any A2A agent. They delegate tasks and run evidence
plans like you do in the web UI, and their work shows up under **Tasks** (marked *asked by
another agent*). They never run anything on your computer: work goes to connected peers,
whose results come back signed.

Turn it on in the web UI (**Connect other agents**, **Turn on**), or:

```powershell
proofkin connect --profile boss          # turns it on; prints the token and how to set up clients
proofkin connect --profile boss --new    # a new token (the old one stops working)
proofkin connect --profile boss --off
```

The running agent serves both with its web UI, so start it with `--ui` and a fixed
`--ui-port` to keep the address:

```powershell
proofkin --name boss --ui --ui-port 7420
```

| Client | Setup |
| --- | --- |
| VS Code | `.vscode/mcp.json`: `{"servers": {"proofkin": {"type": "http", "url": "http://127.0.0.1:7420/mcp", "headers": {"Authorization": "Bearer ${input:proofkin-token}"}}}, "inputs": [{"type": "promptString", "id": "proofkin-token", "password": true, "description": "Proofkin integration token"}]}` (VS Code asks for the token) |
| Claude Code | `claude mcp add --transport http proofkin http://127.0.0.1:7420/mcp --header "Authorization: Bearer <token>"` |
| Any MCP client | Streamable HTTP at `http://127.0.0.1:7420/mcp`, header `Authorization: Bearer <token>` |
| A2A agents | Agent card at `http://127.0.0.1:7420/.well-known/agent-card.json`; headers `Authorization: Bearer <token>` and `A2A-Version: 1.0` |

**MCP tools:** `list_peers` (who can take work: providers, shared folders and their
checks, squads), `delegate` (a task on a peer, in a folder it shares), `get_task` (waits for
the result, up to `wait_seconds`), `cancel_task`, `check_plan`, `run_plan` (an evidence
plan), `get_run` (its evidence report) and `import_spec` ([Spec Kit and
OpenSpec](#spec-kit-and-openspec)). For the [agent factory](#the-agent-factory):
`factory_templates`, `factory_list`, `factory_draft`, `factory_check`, `factory_evaluate`
(the estimate, unless `confirm` is true; then it runs on your evaluation worker) and
`factory_status` (an evaluation's progress and report). Releasing, deploying, rolling back,
retiring, deprecating and approving agents stay with you: those calls are refused with the
command to run. The server speaks the stateless 2026-07-28 revision (the
version in each request's `_meta`, which the `MCP-Protocol-Version`, `Mcp-Method` and
`Mcp-Name` headers must match; `server/discover`) and the `initialize`-based revisions
2025-03-26 to 2025-11-25. It keeps no sessions, so `GET /mcp` answers 405.

**MCP progress:** a `tools/call` of `delegate`, `run_plan` or `get_task` that waits
(`wait_seconds` above 0, 30 if left out) and has a progress token (`params._meta.progressToken`,
a string or a number) and `text/event-stream` in its `Accept` header is answered as a
Server-Sent Events stream, on both revisions. Each time the work's status changes (its state,
or what a step's peer says it is doing, e.g. `Running on worker: running check test`) the
server sends `notifications/progress` with your `progressToken`, an increasing `progress`
(1, 2, 3...) and the status as `message`. Last comes the tool call's response, the same
result as without streaming, and the stream closes; if `wait_seconds` runs out first, the
response says the work is still running (call `get_task`). A confirmed `factory_evaluate` and
`factory_status` stream the same way while an evaluation runs, with `3 of 8 runs done
(running)` as each `message`. Every other request gets one JSON
body, as before.

**A2A 1.0:** `SendMessage` with the work as text (a plan if it parses as one, else a task
for a peer; say where in the message's `metadata` or a data part: `peer`, `folder`,
`provider`, `mcp`, and `kind` to force `task` or `plan`). It waits for the result unless
`configuration.returnImmediately` is set; the task's artifacts are the peer's output, each
step's output and the evidence verdict. `GetTask`, `ListTasks` and `CancelTask` work on the
tasks other agents started. `SendStreamingMessage` takes the same params and starts the
work the same way, but answers as a Server-Sent Events stream (the agent card says
`capabilities.streaming: true`): each event's data is a JSON-RPC response whose result is
first `{"task"}`, then a `{"statusUpdate"}` each time the task's state or what its steps'
peers say they are doing changes (the message's text is the progress, e.g. `api on worker:
running check test (3 of 4)`), and last `{"task"}` again, finished and with its artifacts,
after which the stream closes. `SubscribeToTask` with an `id` follows a running task the
same way, starting with its current task; a finished task is refused
(`UnsupportedOperationError`), an unknown one is `TaskNotFoundError`. Problems found before
a stream starts (bad params, an unknown peer) are plain JSON-RPC errors. Push notifications
aren't offered (use `GetTask`); work that can't start with `SendMessage` (an unknown peer,
a plan with problems) comes back as a `TASK_STATE_REJECTED` task saying why.

**Security.** The token is per profile, kept in the profile's secret store, and works only
for `/mcp` and `/a2a`; the web UI's own token doesn't work there, and the integration token
doesn't work for the web UI's API. Requests from web pages of other sites (an `Origin`
header) are refused, as are other `Host`s (DNS rebinding). The server listens on
127.0.0.1 only. Turning it off, or a new token, takes effect within seconds, also when done
with `proofkin connect` while the agent runs. Agents with the token act with your peers'
trust in you: give it only to agents you would let delegate in your name. Digital workers
can't be connected to; they take work from their owner only.

## Microsoft 365 work context (Work IQ)

Preview. [Work IQ](docs/work-iq.md) is Microsoft's MCP server for Microsoft 365: cited
results from your mail, meetings, files, Teams and people, and answers from Microsoft 365
Copilot or your organization's agents. This section goes from a tenant without it to a first
plan. The sections it links to have the details.

### 1. Your admin sets it up (once per tenant)

Choose how agents connect:

- **Through Microsoft's Work IQ CLI**: for tenants that approved the Work IQ CLI app. Nothing
  to register: each user installs the CLI (`npm install -g @microsoft/workiq`, or from your
  internal npm feed with `--registry`) and accepts its EULA once with `workiq accept-eula`.
  Proofkin never accepts it for you.
- **Proofkin's own sign-in (hosted)**: the tenant registers its own client app (not a
  published plugin's or the CLI's), and Proofkin keeps the refresh token in the profile's
  secret store.

Each step has a check: after it, `proofkin workiq doctor` gets past that point (see
[Checking the Work IQ connection](#checking-the-work-iq-connection)).

| Step | Who | What | Check |
| --- | --- | --- | --- |
| 1. Turn Work IQ on in the tenant | Tenant admin | Create the Work IQ resource's service principal: `az ad sp create --id fdcc1f02-fc51-4226-8753-f668596af7f7` | Sign-in and the doctor no longer report AADSTS500011 or AADSTS650052 |
| 2. Register the client app (hosted only) | App admin | Entra admin center, App registrations, New registration: single tenant; platform **Mobile and desktop applications** with the redirect `http://localhost`; under Authentication, **Allow public client flows** on (for `--device-code`); under API permissions, the delegated `WorkIQAgent.Ask` permission of Work IQ, then **Grant admin consent** | `proofkin auth login workiq --tenant <tenant> --client-id <app-id> --profile P` signs in (Proofkin explains AADSTS65001, 700016, 90002 and 7000218 when it doesn't) |
| 3. Assign the users | App admin | Enterprise applications, the app, Properties: **Assignment required** on; Users and groups: add the users or a group | The sign-in no longer says AADSTS50105 (you are not assigned) |
| 4. Billing | Billing admin | Usage-based billing, and a spending policy that covers those users and the Work IQ API | The doctor's **retrieve** check no longer reports a billing error |
| 5. The Work IQ MCP policy | Microsoft 365 admin | Microsoft 365 admin center, Agents, Tools, Work IQ MCP: the read tools on (`retrieve`, `ask`, `fetch`, `list_agents`), changes off unless needed. A change takes up to 24 hours | The doctor's **tools** check finds `retrieve` |
| 2. (CLI instead) | Tenant admin | Grant admin consent to the Work IQ CLI app, as Microsoft's [Work IQ repository](https://github.com/microsoft/work-iq) says | `proofkin auth login workiq --cli --profile P` signs in |

### 2. Sign in and check

```powershell
# the Work IQ CLI (signed in by itself; Proofkin finds it on the PATH or in npm's global folder)
proofkin auth login workiq --cli --profile me
# or the hosted sign-in, with the app from step 2 (--device-code on a machine without a browser)
proofkin auth login workiq --tenant contoso.onmicrosoft.com --client-id <app-id> --profile me
proofkin workiq doctor --profile me      # five checks; a failure says the fix and who makes it
```

The options of each sign-in are in [Sign in](#sign-in). `proofkin auth status` shows the
account, `proofkin auth logout workiq --profile me` removes it.

### 3. Use it

| What | How | Details |
| --- | --- | --- |
| A step that starts from what Microsoft 365 has on its subject | `workiq=all` or `workiq=email,files,...` on the step | [Microsoft 365 search in a step](#microsoft-365-search-in-a-step-workiq) |
| A step answered by Microsoft 365 Copilot or one of your organization's agents | `@workiq` as the step's target, `agent=<id>` from `proofkin workiq agents` | [Asking Microsoft 365 in a step](#asking-microsoft-365-in-a-step-workiq) |
| A model that searches Microsoft 365 while it works | `mcp=workiq` on a step for your own worker, whose folder offers it (`proofkin mcp add <folder> workiq --workiq`) | [Work IQ as a tool for tasks](#work-iq-as-a-tool-for-tasks-mcpworkiq) |
| Cited results kept as a knowledge topic for `context=` | `proofkin kb import workiq <topic> --query <text> [--from SOURCES]` | **Importing from Microsoft 365** under the knowledge base, and [the design](docs/work-iq.md#5-work-context-in-plans) |
| A peer that may receive some of it | `proofkin peers allow <peer> --profile me --m365 General,Public,unlabeled` | [Security model](#security-model) |
| What it cost and the limits | `proofkin workiq usage`, `proofkin workiq budget --per-day N --per-run N` | [Work IQ audit file and budgets](#work-iq-audit-file-and-budgets) |

A first plan to try (`/plan first.plan` in the agent, or the web UI's plan editor), with one
of your own digital workers as `helper`:

```text
find @helper workiq=files,email What do my recent files and mail say about the launch? Answer in three sentences.
ask @workiq What changed in the launch plan this week?
```

### 4. What keeps your work data yours

- Only results Work IQ marks `allowed` are used; the rest are withheld and counted.
- Your own agent and digital workers get what Work IQ allows. A peer gets nothing unless you
  grant it sensitivity labels, and then only results with those labels. The plan check
  stops a step that would send more, and the agent checks again when it sends.
- What a step got from Microsoft 365 keeps its origin: later steps, knowledge topics and
  outputs built from it are held to the same grants.
- Every call is audited (`workiq-audit.jsonl`: the tool, purpose, audience, counts and ids,
  never the query text) and held to the budgets. Signed evidence records the Work IQ context
  a step had by its hash, never its content.
- The hosted refresh token stays in the OS credential store; the CLI keeps its own sign-in.
  Work IQ content is put in an external-content envelope that the model is told not to obey.

### 5. Test it against your tenant

`$env:PROOFKIN_LIVE_WORKIQ = "cli"` (or `hosted`, with `PROOFKIN_LIVE_WORKIQ_TENANT` and
`PROOFKIN_LIVE_WORKIQ_CLIENT_ID`), then `python scripts/live_workiq.py`. It runs the live
check: a fresh profile signs in, passes the doctor, lists the Work IQ agents (only how many
is shown), runs a plan with a `workiq=` step for its own digital worker (real Copilot, on your
GitHub CLI sign-in) and an `@workiq` step, then verifies the evidence and that every call only
read. Your Microsoft 365 content stays out of its log. `python scripts/evidence.py --live`
runs it too while `PROOFKIN_LIVE_WORKIQ` is set.

### Work IQ troubleshooting

| Symptom | Fix |
| --- | --- |
| `the Work IQ CLI says its EULA is not accepted` | Read the EULA ([Work IQ repository](https://github.com/microsoft/work-iq)) and run `workiq accept-eula` yourself. |
| `there is no Work IQ CLI at ...`, or `npx` fails | Install the CLI (`npm install -g @microsoft/workiq`, with `--registry <feed>` if the public registry is blocked), or give its path: `--cli-path`. |
| `AADSTS500011` or `AADSTS650052` | The tenant has no Work IQ service principal: step 1. |
| `an admin must grant consent for this app` (AADSTS65001) | Step 2's **Grant admin consent**; the message has the consent address. |
| `you are not assigned to the app` (AADSTS50105) | Step 3. |
| `the app doesn't allow public client flows` (AADSTS7000218) | Step 2's **Allow public client flows**, for `--device-code`. |
| `Entra doesn't know this client id or tenant` | Check `--client-id` and `--tenant` against the app registration. |
| `the token's audience is ..., not Work IQ` | The app lacks the `WorkIQAgent.Ask` permission of Work IQ: step 2. |
| `Work IQ does not offer the retrieve tool` | The Work IQ MCP policy: step 5. |
| A billing or spending policy error | Step 4. |
| `sign in again` after a while | The refresh token expired or was revoked: `proofkin auth login workiq ...` again. |
| `the daily Work IQ budget ... is used up` | Wait for tomorrow, or raise it: `proofkin workiq budget --per-day N --profile P`. |
| A step for a peer is refused for its Microsoft 365 content | The peer's grant doesn't cover those labels: `peers allow <peer> --m365 <labels>`, or send the step to your own worker. |
| `kb import workiq` writes nothing | Every result was withheld because Work IQ didn't mark it `allowed`; try another query. |

## Security model

- Connections are encrypted and authenticated (QUIC/TLS with each agent's ed25519 key).
  Only allowlisted keys can connect; everyone else is refused before any message is read.
- An agent runs peer tasks **only** with `--serve`, only for the providers it enabled, only
  from peers allowlisted with `--delegate`, within their hourly limit, and at most 2 at a
  time (others queue).
- Peers' tasks run **only in folders you shared with that peer** (see
  [Shared folders](#shared-folders)), by default your profile's `.proofkin-data` folder, read-only.
  Your `--workspace` and every other folder stay private. A peer cannot choose a path or
  raise its access: each task gets the **lower** of the peer's grant
  (`peers allow ... --delegate --access <level>`, default `read-only`) and the folder's
  maximum (`share add ... --access <level>`). Your own tasks get `--access` in `--workspace`.
- Microsoft 365 content is off for every peer until you grant it:
  `peers allow ... --m365 General,Public,unlabeled` lets a peer receive Work IQ context
  with only those sensitivity labels (`unlabeled` allows content Work IQ says has no label;
  content with no label information, such as meetings and chats, goes to no peer), and
  `--m365-tools` also lets its tasks use your Work IQ, read only. `--m365 off` removes the
  grant; allowing the peer again without `--m365` keeps it. Your own digital workers get
  everything you can read.
- Microsoft 365 content keeps its origin. A knowledge topic with `origin: workiq` and
  `labels: General, HR` in its front matter is Microsoft 365 content (a troubleshooting guide
  can be one too). A plan step that uses such a topic with `context=` is marked with its
  labels, and so is every step that runs `after` a marked step, because it gets that step's
  output; a marked step's `save=` writes the mark into the topic it makes. The plan check
  gives an error for a marked step whose peer (or any peer its squad may pick) has no grant
  or no grant for one of its labels, naming the step, the peer, the labels and the
  `peers allow ... --m365` command that grants them. The check is made again before each task
  is sent, and the step fails with the same message if the grant changed. Your own digital
  workers are never refused.
- Results are signed by the agent that produced them, and checks a peer asks for run only
  with `full` access ([EDD with agents](#evidence-driven-development-with-agents)).
  Approval requests need only the allowlist, and each peer may have at most 20 open.

### Shared folders

A worker decides exactly what each peer can see:

- Every profile has a **data folder**, `~/.proofkin-data/<profile>` (created when the agent starts
  with `--serve`; under `$PROOFKIN_HOME/.proofkin-data` if that is set). It is the built-in share
  `data`, always **read-only**. Put there what you want peers' tasks to see, e.g. a status
  note your manager's agent can summarize. Peers' tasks run there unless they name another
  folder.
- To let a peer work on code, share the folder by name with a maximum access, and grant it:

  ```powershell
  proofkin share add myrepo D:\src\myrepo --access workspace-write --profile alice
  proofkin peers allow <bob-ticket> --profile alice --delegate --access workspace-write --share data,myrepo
  proofkin share list --profile alice
  ```

  Bob then runs `/delegate copilot@alice:myrepo Add tests for the parser`. In the web UI,
  Bob picks the folder next to *Run on*; Alice ticks folders in the **Add peer** dialog.
- Peers see shared folders **by name and access only**, never their paths, and only the ones
  granted to them (`/peers` lists them). Changing a connected peer's grant reaches it at once.
- A task naming a folder that isn't shared with that peer is refused (and audited).
- `proofkin share team <folder> <file>` gives a folder a **team plan**, so peers with edit
  access can hand it [team steps](#team-steps-handing-work-down-a-level) that your agent
  runs with your own team; `--model` has your agent's model write one for each team step
  instead (read-only in the folder, checked before it runs), and `--approve` has you
  approve each first. Peers see only that the folder takes team steps, not the plan.
  The web UI sets and removes it too, under **This agent > Shared folders > Team plan…**
  (and through its API: `PUT /api/shares/<folder>/team` with
  `{"plan": "..."}` or `{"model": true, "approve": true}`, and
  `DELETE /api/shares/<folder>/team`).
- `proofkin share pull-requests <folder>` lets peers with edit access end their plans with
  a [pull request step](#pull-request-steps-opening-the-pull-request-from-the-plan) in a
  linked repository's folder: your agent pushes the branch of work it did for them and
  opens the pull request with your GitHub account, never for another peer's work or a
  branch moved on since. `--approve` has you approve each one first; `--off` stops it. In
  the web UI it is **Takes pull request steps** and **Ask me to approve each one** under
  **This agent > Shared folders** (API: `PUT /api/shares/<folder>/pull-requests` with
  `{"on": true, "approval": true}`; without `approval` it stays as it is).

| | `data` (built in) | A folder you add |
| --- | --- | --- |
| Where | `~/.proofkin-data/<profile>` | Any existing folder |
| Most a task can do | read-only | what you set with `share add --access` |
| Who can use it | peers granted `data` (the default for `peers allow`) | peers you list in `--share` |

### What tasks may do

| Level | Allows | Grant with |
| --- | --- | --- |
| `read-only` | Read and search files **in the task's folder** | default |
| `workspace-write` | Also create and edit files in the folder. No shell commands | `--access workspace-write` on the peer and the folder |
| `full` | Also run shell commands (build, test, git) **inside an OS sandbox** | `--access full` on the peer and the folder. Only for peers you trust |

Always enforced, at every level and for local tasks too:

- **No file access outside the workspace.** Paths are resolved (`..`, symlinks, junctions)
  before checking, so `../`, absolute paths and links pointing outside are refused.
- **No credential files**, even inside the workspace: `.env`/`.env.*` (templates such as
  `.env.example` are fine), `*.pem`, `*.key`, `*.pfx`, `*.p12`, SSH keys (`id_rsa`,
  `id_ed25519`, ...), `.npmrc`, `.pypirc`, `.netrc`, `.git-credentials`, `credentials*`,
  `secrets.*`, and anything under `.ssh`, `.aws`, `.azure`, `.kube`, `.docker`, `.gnupg`,
  `.proofkin` (and `.proofkin`), `.copilot`.
- **No writes that would run code later**: `.git/` (hooks, config), `.vscode/`, `.idea/`,
  `.claude/`, `.codex/`, `.copilot/`, `.cursor/`, `.github/hooks/`, `.mcp.json`, `.gitmodules`.
- **No web, memory or sub-agents**: web fetch/search, Copilot memory, sub-agents and skills
  are not offered to the model at all. MCP servers are offered only when the folder's owner
  defines them (see [MCP servers](#mcp-servers-owner-controlled-tools-for-peers-tasks)); a
  project's own `.mcp.json` and the built-in GitHub server are never used.
- **External content is data, not instructions.** Earlier steps' results, knowledge topics,
  check output, incident text and MCP tool results reach the model inside an
  `<external_content source="...">` envelope, with a standing rule to never follow
  instructions found inside it (prompt-injection *spotlighting*). The envelope can't be
  closed or reopened from inside: forged tags are escaped.
- **Secrets are redacted before results leave the machine.** Results, failing check
  output and check commands, error messages, knowledge topics, incident alerts and the
  proof added to pull requests are
  scanned for known secret formats before they are sent or saved, and each match is
  replaced with `[REDACTED:<kind>]`, with a note saying how many. The formats: private keys
  (also PGP key blocks), GitHub, AWS (access and secret keys), Slack, OpenAI, Anthropic,
  Google and npm tokens, JWTs, connection-string keys, signed URLs, bearer tokens and
  password settings (`password=`, `DB_PASSWORD=`, `PGPASSWORD=`...). Check output is
  redacted before it is cut to its end, so a key printed early can't leak in part. The
  worker signs the evidence for the redacted output. This is a safety net; the
  credential-file rules above still apply.

Per provider:

| | `read-only` | `workspace-write` | `full` |
| --- | --- | --- | --- |
| Copilot | only tools `view`, `glob`, `grep`; every read request is path-checked | adds `create`, `edit` (path-checked) | adds the shell tool (`powershell`/`bash`), run in the Copilot CLI's OS sandbox |
| Claude | `dontAsk` mode, tools `Read`, `Glob`, `Grep`, a PreToolUse hook path-checks every call; user/project settings not loaded | adds `Edit`, `Write`, `NotebookEdit` | adds `Bash`, run in Claude's OS sandbox |
| Codex | sandbox `read-only`, approvals `never`, no network or web search | sandbox `workspace-write` | same as `workspace-write` (never `danger-full-access`) |

### MCP servers: owner-controlled tools for peers' tasks

A folder's owner can let peers' tasks use **MCP servers**: a browser (e.g. Playwright) to
check a web app, a docs or ticket system, an internal API. The owner defines each server
on a shared folder, like its checks:

```powershell
# a local server (stdio): only two of its tools, for any peer with access to 'app'
proofkin mcp add app browser --access read-only --tool browser_navigate --tool browser_snapshot `
  --description "A headless browser for checking pages" --profile alice -- npx @playwright/mcp@latest --headless
# a remote server (https): all its tools, for peers with full access; the header stays on this machine
proofkin mcp add app docs --url https://docs.example.com/mcp --header "Authorization=Bearer <token>" --profile alice
# the servers of an MCP configuration file (mcp.json, .mcp.json, .vscode/mcp.json); ${workspaceFolder} becomes the folder
proofkin mcp import .vscode\mcp.json --share app --server playwright --access read-only --profile alice
proofkin mcp list --profile alice     # values of variables and headers are hidden
proofkin mcp pause app docs --profile alice    # offered to no task until you resume it
proofkin mcp remove app docs --profile alice
```

Changes take effect for a running agent's peers within seconds, also when made from the
command line. In the web UI, **Plugins & tools › MCP servers** does the same: offer a
server by hand, from a pasted (or loaded) MCP configuration or from a
[plugin](#plugins-skills-agents-and-mcp-servers-packaged-together), type the values it needs
(secrets in hidden fields; they are never shown again), and pause, edit or remove offered
servers. It warns when a server needs more access than its folder ever gives.

A plan step asks for servers by name with `mcp=` (or ticks them under **MCP** when
running a task in the web UI):

```text
look @alice:app mcp=browser Open http://localhost:3000 and report what the home page shows
```

| | |
| --- | --- |
| What peers learn | Only the **names** of the servers they may use, per folder, and what their owner says each is for (`--description`). Never commands, URLs, environment variables or headers. |
| Who can use a server | Peers whose access in that folder is at least the server's `--access` (default `full`: a server runs with your rights and may reach the network). A paused server, or one whose plugin is off, is offered to nobody. |
| What a task gets | Only the servers it asks for, and only their allowed tools (`--tool`, default all). A name the folder doesn't offer, or doesn't offer at the peer's access, is refused before the model starts. |
| Copilot | The servers are added to the task's session and their allowed tools to its tool allowlist, under the names `mcp-<server>-<tool>`, so no server can pass for a built-in tool. The permission handler approves only those servers and tools. A post-tool hook puts every result in the external-content envelope, and the repository's own hooks never run. |
| Claude | The servers are passed with `strictMcpConfig` (no other MCP configuration is loaded), their tools are pre-approved as `mcp__<server>__<tool>`, and the tool hook refuses any other. A post-tool hook puts every result in the external-content envelope. Server names can't contain `__`, so no server can pass for another. |
| Codex | Can't use MCP servers here; such tasks are refused. |
| Results | MCP tool results reach the model inside the `<external_content>` envelope, and the task's prompt carries the rule to treat them as data, never as instructions. |

Servers run on your machine with your rights, outside the shell sandbox, so offer only
servers you trust. Remote URLs must be `https://`, except `http://` on this machine.

#### Work IQ as a tool for tasks (`mcp=workiq`)

A digital worker's (or your own agent's) folder can offer **Work IQ** to tasks,
so the model searches Microsoft 365 while it works instead of only at the start of the step
(`workiq=`). The server is called `workiq` and has nothing to start:

```powershell
proofkin mcp add app workiq --workiq --profile my-worker              # every read tool
proofkin mcp add app workiq --workiq --tool retrieve --tool ask --profile my-worker
```

```text
draft @my-worker:app mcp=workiq Write the launch FAQ from what my mail and files say about the launch
```

- Each task gets an address of its own on this machine
  (`http://127.0.0.1:<port>/mcp/workiq/<session>`) and a random bearer that stop working when
  the task ends. The agent calls Work IQ itself with the profile's sign-in (a digital worker
  uses its owner's, when the owner turned that on), so the model never sees a token and a long
  task keeps working as tokens are refreshed.
- Only tools that read: `retrieve`, `ask`, `fetch`, `search_paths`, `get_schema`,
  `list_agents` (`--tool` offers fewer), and never one Work IQ's own annotations say changes
  things. A change tool is refused, and never reaches Work IQ.
- Every result goes through the [content policy](#4-what-keeps-your-work-data-yours): only
  results marked `allowed`, secrets redacted, in the external-content envelope. Every call is
  audited (purpose `gateway`) and held to the daily budget.
- Your own tasks get it (a digital worker's owner's). A peer's task gets it only when you
  granted that peer `--m365-tools` (`proofkin peers allow <peer> --m365 Public,General
  --m365-tools`): then only `retrieve` and `ask`, and only results with the labels you
  granted; its calls are audited under the peer's name. Any other task that asks for
  `mcp=workiq` is refused, and the refusal names the grant. What such a step's model read
  has labels its plan never learns, so the step is marked `unknown` and nothing made from it
  goes to a peer.
- A browser can't reach it: requests with an `Origin` header are refused.

### Plugins: skills, agents and MCP servers, packaged together

A **plugin** bundles skills, agents and MCP servers, in the layouts GitHub Copilot CLI
(`plugin.json`, `.github/plugin/plugin.json`), Claude Code (`.claude-plugin/plugin.json`)
and Agent Plugins (`plugin.json` with its `$schema`, `skills/`, `mcp.json`) use, or a bare
folder of `skills/*/SKILL.md`, `agents/*.md` and `.mcp.json`. A marketplace (`marketplace.json`) lists
several; pick one with `--plugin`.

```powershell
proofkin plugin add .\team-tools --profile alice                          # a folder
proofkin plugin add octo-org/agent-plugins:plugins/reviewer --profile alice  # GitHub (private ones with the profile's GitHub sign-in)
proofkin plugin add octo-org/marketplace --plugin reviewer --profile alice   # from a marketplace
proofkin plugin show reviewer --profile alice     # its skills, agents, MCP servers, the values they need, where they're offered
# offer one of its MCP servers in a shared folder, with the values it needs
proofkin mcp add app tickets --plugin reviewer --set TICKETS_TOKEN=<token> --access read-only --profile alice
proofkin plugin update reviewer --profile alice   # fetch it again
proofkin plugin disable reviewer --profile alice  # enable / remove
```

- **Importing runs nothing.** The plugin's files are copied into the profile. Commands,
  hooks and LSP servers are listed as not used and never run.
- **Skills** become usable in plan steps (`skill=name`, or `skill=plugin.name` when
  another skill has that name), and peers' steps get their `SKILL.md` text like any skill.
- **Agents** (`agents/*.md`, `NAME.agent.md`, or the manifest's `agents`) become usable
  in plan steps (`agent=name`, or `agent=plugin.name` when you have an agent of that
  name). A plugin's agent follows the plugin's own skills; the MCP servers it uses are
  names the step's folder must offer, so offer the plugin's servers under the names its
  agents use. The review notes an agent's server the plugin doesn't bring.
- **MCP servers** are templates: nothing is offered until you offer one in a folder, with
  the values it needs (a plugin's `userConfig`, `${input:x}`, `${env:X}` or `${X}`
  placeholders; values not given with `--set` come from environment variables of the same
  name). `${PLUGIN_ROOT}` (and the Claude and Copilot spellings) becomes the plugin's
  folder, `${PLUGIN_DATA}` a folder it may write to, `${workspaceFolder}` the shared folder.
- **Microsoft's Work IQ plugin** (`proofkin plugin add microsoft/work-iq:plugins/workiq`)
  keeps its skills, and its MCP server (the hosted Work IQ address with its own sign-in
  settings, or the CLI's `@microsoft/workiq mcp`) becomes this agent's own
  [Work IQ server](#work-iq-as-a-tool-for-tasks-mcpworkiq): offer it with `proofkin mcp
  add app workiq --plugin workiq`, and tasks reach Work IQ through your sign-in and the
  gateway, with only the tools that read.
- **Turning a plugin off** withdraws its skills, agents and the servers it offered; turning it on
  offers them again. **Updating** keeps each offer's access, tools and values, and pauses
  servers that would now start differently until you look at them and resume them.
  **Removing** a plugin removes the servers it offered.
- In the web UI, **Plugins & tools › Plugins** fetches a plugin for review (what it
  contains, the values its servers need, what isn't used) before you import it, lists a
  marketplace's plugins to pick from, and turns plugins on and off, updates and removes
  them.

### The shell sandbox (`full`)

Peers' shell commands never run directly on your machine. Each provider runs them in its
OS sandbox: Windows ProcessContainer, Linux Bubblewrap or macOS Seatbelt for Copilot, and
Claude's sandbox runtime (on Windows a separate low-rights user) for Claude:

- **Writes only inside the workspace** (plus temp folders and toolchain caches such as
  Cargo's registry), so a command can't change files elsewhere, even when obfuscated.
- **No network**: no outbound or local-network connections, so nothing can be uploaded and
  nothing downloaded. Builds work when their dependencies are already cached.
- **No way out**: requests to run a command outside the sandbox are refused.
- **Credential folders unreadable** (`~/.ssh`, `~/.aws`, `~/.azure`, `~/.config/gh`,
  `~/.proofkin`, ...): for Copilot wherever the sandbox reports the `denied_paths`
  capability (Linux, macOS, and Windows with Copilot CLI 1.0.112+ on a current Windows
  build), and on every platform for Claude.
- **Fails closed**: if a host has no sandbox, Copilot tasks drop the shell tool (printed
  as `copilot: no OS sandbox on this host`) and Claude tasks fail rather than run
  unsandboxed. Run `copilot update` so Proofkin can check sandbox support at startup.
- **Only the model's commands**: the MCP servers a task may use (its folder owner's, and
  the one that runs its checks while it works) run outside the sandbox, with your rights,
  as the checks after the model do; so a check that builds into a folder outside the
  workspace (a shared `CARGO_TARGET_DIR`, say) works while the model works too.
- **The same Node.js as the agent**: a Copilot model's shell finds the Node.js the agent's
  own `node` runs, put first on its `PATH`, even when `node` is a version manager's shim
  (nvm for Windows, Volta) that can't find which version to run inside the sandbox.

On top of the sandbox, commands that name a URL, a path outside the workspace, the home
folder (`~`, `$HOME`, `$env:USERPROFILE`, `%APPDATA%`, ...) or a credential file are refused
before they run.

Limits to know:

- **Sandboxed commands can still read other files your account can read** outside the
  workspace (only the credential folders above are denied). They have no network to send
  them anywhere, and only the task result goes back to the peer. On older Windows builds
  or Copilot CLIs without `denied_paths`, the credential folders are readable too. For
  untrusted peers keep them at `workspace-write`, or run the serving agent under a
  separate OS account or in a VM/container.
- **Searches see the whole workspace.** A `grep` over the workspace folder is approved as one
  request, so it can return lines from a `.env` inside it. Keep secrets out of the workspace
  you serve (or out of Git-ignored files in it).
- **Codex** confines writes and network with its own sandbox, but its sandbox does not stop
  reads outside the workspace (and on Windows the sandbox is limited). Serve Codex only to
  peers you trust, or run that agent under a separate OS account or in a container.
- Denied requests are printed in the agent's terminal (`copilot: denied ...`,
  `claude: denied ...`). Set `PROOFKIN_DEBUG_PERMISSIONS=1` to print every Copilot
  permission request.

- Each agent uses only its own profile's accounts and provider instances (see
  [Isolation guarantees](#accounts-and-profiles)). Delegating peers never get credentials.
- Prompts from a peer you allow to delegate run with your accounts and in your workspace,
  and the result goes back to that peer: anything the task can read, the peer can see. Only
  grant `--delegate` to people you trust, and `workspace-write`/`full` to fewer still.

## Test

```powershell
cargo test
cargo clippy --all-targets -- -D warnings
```

The tests use a fake executor and a fake bridge ([tests/fixtures/fake_bridge.mjs](tests/fixtures/fake_bridge.mjs)),
so they need Node.js but no LLM credentials. The multi-agent tests run real iroh QUIC
endpoints on localhost in local mode, so they need no internet.
`.\scripts\p2p.ps1 live-test` runs the end-to-end test over the internet.

## Evidence-driven development

This section is about how Proofkin itself is built. To run EDD on your own projects with
a team of agents, see [Evidence-driven development with agents](#evidence-driven-development-with-agents).

Every behavior Proofkin promises is written down as a **claim** in
[evidence/claims.json](evidence/claims.json), with the evidence that proves it: unit and
integration tests, and checks from the live internet test. For example:

```json
{ "id": "AUD-1", "claim": "Every task a peer asks an agent to run is recorded ... without the prompt text.",
  "tests": ["lib::audit::records_outcomes_without_the_prompt", "agents::delegation_needs_permission_and_respects_the_hourly_limit"],
  "live":  ["worker2 audited the refused request from worker1", "audit logs hold no prompt text"] }
```

[scripts/evidence.py](scripts/evidence.py) runs the gates and the tests, matches each result
to the claims and writes a report:

```powershell
.\scripts\p2p.ps1 evidence         # fmt, clippy, tests -> claims (no credentials; what CI runs)
.\scripts\p2p.ps1 evidence -Live   # also the live internet test with real Copilot (needs gh sign-in)
# report: target\evidence\report.md (+ report.json, raw logs in target\evidence\logs)
```

It fails when a gate fails, any claim has missing or failing evidence, or a test or live
check proves no claim. Every claim needs evidence, and every test must prove a claim. When
`cargo test` fails, the output also lists each failed test as `TEST FAILED` with its id and
the first 30 lines of its output (if the tests didn't build, the last 40 lines of the test
log), and `report.json` (`failed_tests`) and `report.md` list them too. CI
runs it on every push, publishes the report in the run summary and keeps it as the
`evidence-report` artifact.

Workflow for a change:

1. Write or update the claim in `evidence/claims.json` first (what will be true).
2. Add the tests (and a live check in `scripts/live_test.py` if it involves real peers or
   providers) and list them under the claim. Test ids are `<target>::<path>` as printed by
   `cargo test`, without `::tests` (`lib::...`, `bin::...`, or the file name in `tests/`).
   A live check that needs something not everyone has is listed as
   `{"check": "<name>", "needs": "<VARIABLE>"}`: it counts only while that variable is set,
   as `PROOFKIN_LIVE_WORKIQ` is for the Work IQ checks of `scripts/live_workiq.py`.
3. Implement until `.\scripts\p2p.ps1 evidence` passes. Run with `-Live` before a release.
4. Keep the report (`target\evidence\report.md`) with the PR or release as its evidence.

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| `cargo` / `rustc` is not recognized | The terminal was opened before Rust was installed. VS Code terminals keep VS Code's old `PATH`, so **fully quit and reopen VS Code** (a new terminal tab is not enough), or use `.\scripts\p2p.ps1`, which reloads `PATH` itself. |
| `error calling dlltool 'dlltool.exe': program not found` | GNU toolchain without MinGW-w64: do Option A step 3, then open a new terminal. |
| `linker 'link.exe' not found` | MSVC toolchain without Build Tools: do Option B step 1, or switch with `rustup default stable-x86_64-pc-windows-gnu` and use Option A. |
| `github-copilot-sdk` requires rustc 1.94 | `rustup update stable` |
| `Copilot CLI not found` | Install the Copilot CLI and open a new terminal, or set `COPILOT_CLI_PATH`. |
| `the Copilot CLI at ... speaks protocol v4, but this proofkin speaks v3` | The Copilot CLI updated itself past the protocol this proofkin's Copilot SDK speaks. If it kept an earlier version that matches, agents use that one by themselves (`proofkin doctor` says which). Otherwise set `COPILOT_CLI_PATH` to a Copilot CLI that matches (e.g. `npm install -g @github/copilot@<version>`), or update proofkin when a release supports the new protocol. |
| `bridge script ... not found` / `Cannot find package '@anthropic-ai/claude-agent-sdk'` | Run `npm install` in `bridge\`, or pass `--bridge`. |
| `npm install` fails with `ERR_SSL_..._HANDSHAKE_FAILURE` | Your network blocks the public npm registry. Use your internal feed, e.g. `npm install --registry <feed-url>`. |
| `Not logged in · Please run /login` (claude) | Sign the agent's profile in: `proofkin auth login claude --profile <profile>`. |
| `401 Unauthorized` (codex) | Sign the agent's profile in: `proofkin auth login codex --profile <profile>`. |
| `<provider> is not signed in for profile '<p>'` | The agent's profile has no account for that provider; run the printed `auth login` command. Agents never borrow the machine's global sign-in. |
| `no GitHub CLI token for <user> on <host>` | Sign that account in to gh: `gh auth login --hostname <host>` (or `proofkin github login --new --host <host> --profile <p>`), then check `gh auth status`. |
| `the GitHub account <user> can't use projects yet` | Its token lacks the `project` scope the tracker's board needs: **Grant access** on the Tracker card, or `proofkin github grant project --profile <p>`. |
| `Copilot rejected this profile's token` | The token is invalid, expired, or the account has no Copilot access. Sign in again with `auth login copilot`. |
| Windows Firewall prompt on first run | Allow it for direct (faster) connections; if blocked, agents still connect through the relays. |
| `! refused connection from <id>` | That key is not on this profile's allowlist. If it should be: `proofkin peers allow <id> --profile <p> --name <n>`. |
| `<id> is not on the allowlist` when dialing | Allowlist the peer first; agents only dial allowlisted keys. |
| Peers never connect (`+ name` doesn't appear) | Both sides must allowlist each other; check `peers list` on both. Check `warning: no relay reachable` at startup: the network must allow outbound HTTPS to `*.iroh.link` (or UDP). On an isolated LAN use `--network local`. |
| `does not accept tasks from <name> (not allowed to delegate)` | The worker's owner must allow you with `peers allow ... --delegate`. |
| `reached its limit of N tasks per hour` | Wait, or the worker's owner raises it with `peers allow ... --delegate --tasks-per-hour M`. |
| `export ordinal too large` when linking `iroh` (Windows GNU) | Keep the `[patch.crates-io]` entry for `vendor/iroh` in `Cargo.toml` (see [vendor/iroh/VENDORED.md](vendor/iroh/VENDORED.md)). |

## Layout

- [src/protocol.rs](src/protocol.rs): wire messages (`hello` with addresses, capabilities, models and the models its accounts offer, `peers`, `chat`, `task_request`, `task_result`, `task_cancel`, `task_progress`, `load`, `shares`, `models`; unknown types are ignored) and the `p2pagents/1` ALPN
- [src/agent.rs](src/agent.rs): `Agent` node on an iroh endpoint: allowlist enforcement, dialing, gossip, auto-reconnect, broadcast, task delegation with permissions and hourly limits, task slots and queue, live progress and load reports, the task grace for dropped peers, shutdown (telling peers)
- [src/awake.rs](src/awake.rs): waits that count only the time the machine is awake (the task grace, a model run's limit, `timeout=`)
- [src/trust.rs](src/trust.rs): allowlist (`peers.json`) and `p2pa1...` tickets
- [src/llm/mod.rs](src/llm/mod.rs): `Provider`, `Access`, `LlmTask`, the `Executor` trait and `SdkExecutor`
- [src/llm/copilot.rs](src/llm/copilot.rs): per-agent Copilot instance on the official Rust SDK (own token and `COPILOT_HOME`), with per-access tool allowlists and the path-checking permission handler
- [src/llm/policy.rs](src/llm/policy.rs): workspace confinement, credential-file and code-execution-file rules, shell command scan
- [src/profile.rs](src/profile.rs): profiles: private provider homes, OS credential store secrets, environment isolation, digital workers (owner accounts, role), one agent per profile
- [src/workers.rs](src/workers.rs): digital workers: create (folders, both allowlists), delete, and the supervisor that runs them as background agents of their owner
- [src/auth.rs](src/auth.rs): `auth login/status/logout` for Copilot, Claude and Codex
- [src/llm/bridge.rs](src/llm/bridge.rs): client for the Node bridge (multiplexed JSON lines over stdio)
- [bridge/index.mjs](bridge/index.mjs): Node bridge calling the Claude Agent SDK and Codex SDK
- [src/setup.rs](src/setup.rs): `setup` (installs the embedded bridge) and `doctor`
- [src/audit.rs](src/audit.rs): per-profile audit log of peers' tasks (`audit.jsonl`, no prompt text) with the usage providers reported, summed per peer and provider
- [src/factory/](src/factory) and [factory/templates/](factory/templates): the agent factory: blueprints checked against the profile and linted for least privilege (`blueprint.rs`), eval suites and their graders (`evals.rs`), the profile's factory store, with builds kept by their hash (`store.rs`), the built-in templates (`templates.rs`), drafts written by the owner's model (`draft.rs`), builds (`build.rs`), capabilities resolved against the owner's folders (`capabilities.rs`), the models an agent is evaluated and runs on (`routing.rs`), a worker's memory and lessons (`memory.rs`), evaluations: the built-in adversarial suite (`adversarial.rs`), the runner on the owner's evaluation worker (`runner.rs`), signed reports and the release gate (`report.rs`) and judges (`judge.rs`), agents as code and the `agent:*` checks (`code.rs`), and `proofkin factory` (`cli.rs`); [src/ui/factory.rs](src/ui/factory.rs) and [ui/factory.js](ui/factory.js): its web UI API and card
- [src/shares.rs](src/shares.rs): shared folders (built-in read-only `.proofkin-data` folder, named shares with a maximum access, their checks and MCP servers)
- [src/mcp.rs](src/mcp.rs): owner-defined MCP servers (validation, access rule, secrets hidden from listings, provider config), MCP configuration files and server templates with placeholders
- [src/plugins.rs](src/plugins.rs): plugins (sources, fetching for review, the Copilot CLI, Claude Code and Agent Plugins layouts, marketplaces, the profile's plugin store, offers made from templates)
- [src/store.rs](src/store.rs): atomic writes and file locks for the profile's JSON files
- [src/boundary.rs](src/boundary.rs): the external-content envelope and standing rule against prompt injection
- [src/redact.rs](src/redact.rs): secret redaction of results, check output, knowledge and alerts
- [src/schedule.rs](src/schedule.rs): scheduled plans (`--when` parsing, `schedules.json`, the scheduler)
- [src/plan.rs](src/plan.rs): squads (`squads.json`), plan parsing and validation (claims, checks, approvals), and the dependency-ordered plan runner (load-aware squad picks, rerouting, timeouts)
- [src/team.rs](src/team.rs): team steps: team plans, handing work down a level, and the lead's team run with its progress reported upward
- [src/evidence.rs](src/evidence.rs): signed evidence records, owner-defined checks (run with a timeout, process tree killed) and retry prompts
- [src/worktree.rs](src/worktree.rs): per-task git worktrees and branches, commits and patches, and the unfinished work a stopped task keeps
- [src/runs.rs](src/runs.rs): evidence runs: verdict per claim, Markdown reports, offline signature re-verification
- [src/knowledge.rs](src/knowledge.rs): the knowledge base (topics, context for prompts, troubleshooting guides)
- [src/playbooks.rs](src/playbooks.rs) and [playbooks/](playbooks): bootstrap, discover, sdlc, learn, spec, design, implement-spec, implement-speckit and rehearse plan templates; [src/spec_import.rs](src/spec_import.rs): Spec Kit and OpenSpec specs as claims, knowledge and plans
- [src/spec.rs](src/spec.rs) and [spec-starter/](spec-starter): the built-in spec checks (`spec:spec`, `spec:api`, `spec:ux`, `spec:trace`) and the `spec init` starter package
- [src/design.rs](src/design.rs) and [design-starter/](design-starter): the design and review checks (`design:arch`, `design:data`, `design:threats`, `design:tests`, `design:ops`, `review:decks`, `review:rehearsal`) and the `design init` starter
- [src/render.rs](src/render.rs): Markdown to self-contained HTML: Marp decks as slide shows, documents as pages, without raw HTML
- [src/preview.rs](src/preview.rs): sandboxed previews of UX prototypes and review packs (from a step's patch, a shared folder or an approval request)
- [src/ops.rs](src/ops.rs): monitors, metrics, incidents, TSG auto-fix, the debugging agent and ops reports
- [src/skills.rs](src/skills.rs) and [skills/](skills): built-in and profile skills (instructions for plan steps)
- [src/github.rs](src/github.rs): GitHub sign-in, linked repositories (clone, sync, push task branches, pull requests)
- [src/pulls.rs](src/pulls.rs): pull request steps: the folder's agent opens the pull request with the work before it (only work it signed for the peer that asks) and follows it until it is merged or closed
- [src/tracker.rs](src/tracker.rs): tracked work: plan runs as GitHub issues and sub-issues on a GitHub Project's board (GraphQL through `gh`)
- [src/workiq/](src/workiq): the Work IQ MCP client (streamable HTTP, JSON or SSE replies, session renewal, cached `tools/list`, typed `retrieve`, `ask`, `fetch` and `list_agents` calls, classified `WorkIqError`s with a fix and the request ids, and safe retries: one token refresh on 401, throttling waited for, transient failures of read tools backed off, changes never sent again) behind a `Transport` trait, and the content policy (`content.rs`: only `allowed` results, label allowlist per audience, redaction, size cap, envelope), and the gateway (`gateway.rs`: a task's own MCP address on 127.0.0.1 for `mcp=workiq`, with a bearer that ends with the task); see [docs/work-iq.md](docs/work-iq.md)
- [src/llm/changes.rs](src/llm/changes.rs): workspace snapshots and the "files changed" report on task results
- [evidence/claims.json](evidence/claims.json), [scripts/evidence.py](scripts/evidence.py): claims and the evidence harness (see [Evidence-driven development](#evidence-driven-development))
- [install.ps1](install.ps1), [install.sh](install.sh): install prebuilt binaries from GitHub releases
- [src/main.rs](src/main.rs): interactive CLI
- [src/ui.rs](src/ui.rs): web UI server (`--ui`): loopback-only, token-guarded JSON API and Server-Sent Events; [src/ui/tools.rs](src/ui/tools.rs): its plugins, MCP servers and skills API; [src/ui/workers.rs](src/ui/workers.rs): its digital workers API; [src/ui/models.rs](src/ui/models.rs): its Models dialog API; [src/ui/connect.rs](src/ui/connect.rs): the MCP server and A2A agent other agents connect to
- [ui/](ui/): the web app (HTML/CSS/JS, embedded in the binary), light and dark themes; [ui/tools.js](ui/tools.js) is the Plugins & tools dialog, [ui/workers.js](ui/workers.js) the Digital workers card, [ui/models.js](ui/models.js) the model lists and the Models dialog, [ui/connect.js](ui/connect.js) the Connect other agents card
- [docs/ux-guidelines.md](docs/ux-guidelines.md): the UI's design guidelines and definition of done
- [docs/work-iq.md](docs/work-iq.md): the design for Microsoft 365 work context through Work IQ (being built, tracked in [#36](https://github.com/mcbelton/proofkin/issues/36))
- [docs/agent-factory.md](docs/agent-factory.md) and [docs/agent-factory/](docs/agent-factory): the design for the agent factory: agents described in plain words, evaluated, released signed and deployed as digital workers, with its spec, API contract, architecture, threat model and test plan (being built, tracked in [#175](https://github.com/mcbelton/proofkin/issues/175))
- [scripts/p2p.ps1](scripts/p2p.ps1): doctor / build / test / demo / live-test helper
- [scripts/live_test.py](scripts/live_test.py): live internet test: 4 agents found by key, allowlist, 2 accounts, real Copilot
- [scripts/live_workiq.py](scripts/live_workiq.py): live Work IQ check against your tenant (`PROOFKIN_LIVE_WORKIQ=cli|hosted`): sign-in, doctor, the agents list, a `workiq=` step on real Copilot and an `@workiq` step, evidence and read-only audit
- [tests/](tests/): multi-agent (iroh on localhost), allowlist, delegation-rights, bridge and credential-isolation tests; [tests/fixtures/echo_mcp.py](tests/fixtures/echo_mcp.py) is a tiny MCP server and [tests/fixtures/plugin/](tests/fixtures/plugin) a Claude Code style plugin (a skill, an MCP server, a hook that must never run), [tests/fixtures/speckit/](tests/fixtures/speckit) and [tests/fixtures/openspec/](tests/fixtures/openspec) a Spec Kit feature and an OpenSpec change for the tests; [tests/connect.rs](tests/connect.rs) drives the MCP and A2A endpoints like other agents do
- [vendor/iroh/](vendor/iroh/): iroh 1.4.1 without its `cdylib` crate type (Windows GNU link fix)

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option. The vendored iroh copy in [vendor/iroh/](vendor/iroh/) keeps its own
license (MIT OR Apache-2.0).
