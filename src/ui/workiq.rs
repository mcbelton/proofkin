//! The web UI's Work IQ account (see [`crate::workiq`] and docs/work-iq.md): the account the
//! agent uses and today's calls against its budgets, the doctor's checks, a sign-in through
//! Microsoft's Work IQ CLI (as `proofkin auth login workiq --cli`), signing out, the
//! budgets, and the declarative agents a `@workiq` step can ask. Each handler reads the
//! profile as it is now, so a sign-in made in another process counts.

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, AppState, bad_request, internal};
use crate::profile::{Profile, WorkIqConnection};
use crate::runs::one_line;
use crate::workiq::account::{account_line, logout};
use crate::workiq::audit::{Audit, set_budget};
use crate::workiq::content::read_agents;
use crate::workiq::context::client_for_profile;
use crate::workiq::doctor::Doctor;
use crate::workiq::login::CliLogin;
use crate::workiq::notice::Setting;

fn refused(e: anyhow::Error) -> ApiError {
    bad_request(format!("{e:#}"))
}

/// Refuses to change the account of a digital worker that uses its owner's: `instead` says
/// what to do.
fn refuse_owners(profile: &Profile, instead: &str) -> Result<(), ApiError> {
    match profile.workiq_profile() {
        Ok(holder) if holder.name != profile.name => Err(bad_request(format!(
            "{} uses {}'s Work IQ account; {instead}",
            profile.name, holder.name
        ))),
        _ => Ok(()),
    }
}

/// What the page sent, unless it is empty.
fn given(text: Option<String>) -> Option<String> {
    let text = text?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// The account the agent uses (a digital worker may use its owner's), how it connects, and
/// today's calls against the budgets.
pub(super) async fn state(State(s): State<AppState>) -> Json<Value> {
    let profile = s.profile_now();
    let (holder, account, not_used) = match profile.workiq_profile() {
        Ok(holder) => {
            let account = holder.config.workiq.clone();
            (holder, account, None)
        }
        Err(e) => (profile.clone(), None, Some(format!("{e:#}"))),
    };
    let connection = account.as_ref().map(|account| match account.connection {
        WorkIqConnection::Hosted { .. } => "hosted",
        WorkIqConnection::Cli { .. } => "cli",
    });
    let cli = match account.as_ref().map(|account| &account.connection) {
        Some(WorkIqConnection::Cli {
            account: cli_account,
            program,
        }) => json!({
            "account": cli_account,
            "path": program.as_ref().map(|path| path.display().to_string()),
        }),
        _ => Value::Null,
    };
    let usage = match Audit::for_profile(&holder).report() {
        Ok(report) => json!({
            "day": report.day.to_string(),
            "today": report.today,
            "per_day": report.budget.per_day(),
            "per_run": report.budget.per_run(),
            "runs": report.runs,
        }),
        Err(_) => Value::Null,
    };
    let notices = Setting::of(&profile);
    Json(json!({
        "signed_in": account.is_some(),
        "line": account.as_ref().map(account_line),
        "connection": connection,
        "user": account.as_ref().map(|account| &account.upn),
        "cli": cli,
        "holder": holder.name,
        "own": holder.name == profile.name,
        "not_used": not_used,
        "hint": holder.workiq_login_hint(),
        "usage": usage,
        // Plans of this profile may make the Microsoft 365 changes you sign off, and its
        // digital workers' tasks for you each change you sign off, waiting this long for you.
        "changes": profile.config.workiq_changes.is_some(),
        "change_wait_minutes": profile
            .config
            .workiq_changes
            .as_ref()
            .map(|changes| changes.wait_minutes()),
        // Where the sign-off notices of this profile go (its holder's setting), or null when
        // they are off.
        "notices": notices.to_json(),
        "notices_line": notices.to.is_some().then(|| notices.describe()),
        "profile": profile.name,
    }))
}

/// Checks the Work IQ connection as `proofkin workiq doctor` does. A check that fails is part
/// of the answer, with who fixes it and how.
pub(super) async fn doctor(State(s): State<AppState>) -> Json<Value> {
    let profile = s.profile_now();
    let report = Doctor::new(&profile).run().await;
    Json(report.to_json())
}

#[derive(Deserialize)]
pub(super) struct SignInBody {
    /// The account the CLI signs in with; none: the CLI's default account.
    account: Option<String>,
    /// Where the CLI is; none: looked for.
    cli_path: Option<String>,
}

/// Signs this profile in through Microsoft's Work IQ CLI, as `proofkin auth login workiq
/// --cli` does: the account is kept only once Work IQ answered through it.
pub(super) async fn sign_in(
    State(s): State<AppState>,
    Json(body): Json<SignInBody>,
) -> Result<Json<Value>, ApiError> {
    let mut profile = s.profile_now();
    refuse_owners(&profile, "sign in there")?;
    profile.check_can_sign_in().map_err(refused)?;
    let account = given(body.account);
    let mut login = CliLogin::new(account.as_deref());
    if let Some(path) = given(body.cli_path) {
        login = login.with_cli_path(path);
    }
    let who = login.run(&mut profile).await.map_err(refused)?;
    s.system(
        "info",
        format!("Work IQ signed in through the Work IQ CLI as {who}"),
    );
    let line = profile.config.workiq.as_ref().map(account_line);
    Ok(Json(json!({ "who": who, "line": line })))
}

/// Signs this profile out of Work IQ, as `proofkin auth logout workiq` does.
pub(super) async fn sign_out(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let mut profile = s.profile_now();
    refuse_owners(&profile, "sign out there")?;
    if profile.config.workiq.is_none() {
        return Err(bad_request(format!(
            "Work IQ is not signed in for profile '{}'",
            profile.name
        )));
    }
    logout(&mut profile).map_err(internal)?;
    s.system("info", "Work IQ signed out");
    Ok(Json(json!({ "signed_in": false })))
}

#[derive(Deserialize)]
pub(super) struct BudgetBody {
    per_day: Option<u32>,
    per_run: Option<u32>,
}

/// Changes this profile's Work IQ call budgets, as `proofkin workiq budget` does: a limit
/// left out keeps its value.
pub(super) async fn budget(
    State(s): State<AppState>,
    Json(body): Json<BudgetBody>,
) -> Result<Json<Value>, ApiError> {
    let mut profile = s.profile_now();
    refuse_owners(&profile, "set its budget there")?;
    let budget = set_budget(&mut profile, body.per_day, body.per_run).map_err(refused)?;
    Ok(Json(json!({
        "per_day": budget.per_day(),
        "per_run": budget.per_run(),
    })))
}

/// The declarative agents Work IQ offers the account this profile uses, by name and the id a
/// `@workiq` step's `agent=` takes, as `proofkin workiq agents` lists them. Not signed in,
/// or a Work IQ error, is a 400 with its message on one line: the plan builder shows it as a
/// hint under its Agent list.
pub(super) async fn agents(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let failed = |e: anyhow::Error| bad_request(one_line(&format!("{e:#}")));
    let client = client_for_profile(&s.profile_now()).map_err(failed)?;
    let listed = client.list_agents().await.map_err(failed)?;
    let agents: Vec<Value> = read_agents(&listed)
        .map_err(failed)?
        .into_iter()
        .map(|agent| json!({ "name": agent.name, "id": agent.id }))
        .collect();
    Ok(Json(json!({ "agents": agents })))
}
