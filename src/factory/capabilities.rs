//! Capabilities (docs/agent-factory.md, section 4): what an agent's blueprint asks for,
//! resolved against what its owner offers in each folder it is deployed to. A blueprint
//! asks; the owner grants. Resolving never offers a server, shares a folder or grants a
//! peer anything: what isn't offered is an error naming the command that offers it, and
//! the owner decides.
//!
//! [`increases`] says what one blueprint asks for beyond another (a draft beyond the
//! active release): a release that asks for more needs its owner's sign-off.

use std::collections::BTreeMap;

use crate::factory::blueprint::{Blueprint, Catalog, Folder, Problem};
use crate::llm::Access;
use crate::shares::DATA_SHARE;

/// What an agent's tasks would get in one of its folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub folder: String,
    /// What it asks for, at most what the folder gives.
    pub access: Access,
    /// The MCP servers its tasks may use, with the tools they may call (none listed: all
    /// the server offers).
    pub mcp: BTreeMap<String, Vec<String>>,
    pub checks: Vec<String>,
}

/// What resolving an agent's capabilities found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    /// What isn't offered (errors), and what is offered only in part (warnings).
    pub problems: Vec<Problem>,
    /// What its tasks would get, folder by folder.
    pub grants: Vec<Grant>,
    /// The Microsoft 365 sources its tasks may get context from: those it asks for, when
    /// the owner is signed in to Work IQ (its worker uses the owner's account).
    pub workiq: Vec<String>,
    /// Whether it may hand work to other agents: only if it asks to, and then only to the
    /// peers its owner allows on its worker's allowlist.
    pub delegate: bool,
}

/// What `bp` asks for, resolved against what `catalog`'s profile offers in each of
/// `folders`: errors for what isn't offered, each with the command that offers it.
pub fn resolve(bp: &Blueprint, folders: &[String], catalog: &Catalog) -> Resolution {
    let caps = &bp.capabilities;
    let profile = &catalog.profile;
    let mut res = Resolution::default();
    for name in folders {
        // A deploy folder's problem points at it there; one given on its own has no field.
        let at = bp
            .deploy
            .folders
            .iter()
            .position(|f| f == name)
            .map(|i| format!("/deploy/folders/{i}"))
            .unwrap_or_default();
        if name == DATA_SHARE {
            res.problems.push(
                Problem::error(
                    "data_folder",
                    &at,
                    "every deployed agent has its own data folder",
                )
                .fix("name folders you share (proofkin share list)"),
            );
            continue;
        }
        let Some(folder) = catalog.folders.get(name) else {
            res.problems.push(
                Problem::error(
                    "unknown_folder",
                    &at,
                    format!("you share no folder '{name}'"),
                )
                .fix(format!(
                    "proofkin share add {name} <path> --profile {profile}"
                )),
            );
            continue;
        };
        let path = format!("\"{}\"", folder.path.display());
        let mut grant = Grant {
            folder: name.clone(),
            access: caps.access.min(folder.access),
            mcp: BTreeMap::new(),
            checks: Vec::new(),
        };
        if caps.access > folder.access {
            res.problems.push(
                Problem::error(
                    "access_not_offered",
                    "/capabilities/access",
                    format!(
                        "'{name}' gives tasks at most {} access, and it asks for {}",
                        folder.access, caps.access
                    ),
                )
                .fix(format!(
                    "proofkin share add {name} {path} --access {}{} --profile {profile}{} (or ask for {} access)",
                    caps.access,
                    flags_again(folder),
                    checks_again(folder),
                    folder.access
                )),
            );
        }
        for (i, ask) in caps.mcp.iter().enumerate() {
            let at = format!("/capabilities/mcp/{i}");
            let server = &ask.server;
            let every = ask.tools.iter().any(|t| t.trim() == "*");
            let Some(offer) = folder.mcp.get(server) else {
                let tools: String = if every {
                    String::new()
                } else {
                    tool_flags(ask.tools.iter())
                };
                res.problems.push(
                    Problem::error(
                        "mcp_not_offered",
                        &at,
                        format!("'{name}' offers no MCP server '{server}'"),
                    )
                    .fix(format!(
                        "proofkin mcp add {name} {server} --access {}{tools} --profile {profile} -- <its command> (or --url <its URL>)",
                        grant.access
                    )),
                );
                continue;
            };
            let mut offered = true;
            if offer.paused {
                offered = false;
                res.problems.push(
                    Problem::error(
                        "mcp_paused",
                        &at,
                        format!("'{server}' in '{name}' is paused"),
                    )
                    .fix(format!(
                        "proofkin mcp resume {name} {server} --profile {profile}"
                    )),
                );
            }
            if let Some(plugin) = &offer.plugin_off {
                offered = false;
                res.problems.push(
                    Problem::error(
                        "mcp_plugin_off",
                        &at,
                        format!(
                            "'{server}' in '{name}' comes with plugin '{plugin}', which is turned off"
                        ),
                    )
                    .fix(format!("proofkin plugin enable {plugin} --profile {profile}")),
                );
            }
            if grant.access < offer.access {
                offered = false;
                res.problems.push(
                    Problem::error(
                        "mcp_needs_access",
                        &at,
                        format!(
                            "'{name}' offers '{server}' to tasks with {} access, and its tasks get {}",
                            offer.access, grant.access
                        ),
                    )
                    .fix(format!(
                        "proofkin mcp add {name} {server} --access {}{} --profile {profile} -- <its command> (or --url <its URL>; it replaces the offer), or ask for {} access",
                        grant.access,
                        tool_flags(offer.tools.iter()),
                        offer.access
                    )),
                );
            }
            let tools = if every {
                if !offer.tools.is_empty() {
                    res.problems.push(
                        Problem::warning(
                            "tools_limited",
                            &format!("{at}/tools"),
                            format!(
                                "it asks for every tool of '{server}', and '{name}' offers only {}",
                                offer.tools.join(", ")
                            ),
                        )
                        .fix(format!(
                            "list the tools it uses of {}",
                            offer.tools.join(", ")
                        )),
                    );
                }
                offer.tools.clone()
            } else {
                let missing: Vec<&String> =
                    ask.tools.iter().filter(|t| !offer.allows_tool(t)).collect();
                if !missing.is_empty() {
                    offered = false;
                    let names: Vec<&str> = missing.iter().map(|t| t.as_str()).collect();
                    // What it would offer then: the tools it offers now, and these.
                    let flags = tool_flags(offer.tools.iter().chain(missing.iter().copied()));
                    res.problems.push(
                        Problem::error(
                            "tool_not_offered",
                            &format!("{at}/tools"),
                            format!("'{server}' in '{name}' doesn't offer {}", names.join(", ")),
                        )
                        .fix(format!(
                            "proofkin mcp add {name} {server} --access {}{flags} --profile {profile} -- <its command> (or --url <its URL>; it replaces the offer)",
                            offer.access
                        )),
                    );
                }
                ask.tools.clone()
            };
            if offered {
                grant.mcp.insert(server.clone(), tools);
            }
        }
        for (i, check) in caps.checks.iter().enumerate() {
            if folder.checks.contains(check) {
                grant.checks.push(check.clone());
            } else {
                res.problems.push(
                    Problem::error(
                        "check_not_offered",
                        &format!("/capabilities/checks/{i}"),
                        format!("'{name}' has no check '{check}'"),
                    )
                    .fix(format!(
                        "proofkin share add {name} {path} --access full{} --check {check}=<command> --profile {profile}{}",
                        flags_again(folder),
                        checks_again(folder)
                    )),
                );
            }
        }
        res.grants.push(grant);
    }
    if catalog.workiq {
        res.workiq = caps.workiq.clone();
    }
    res.delegate = caps.delegate;
    res.problems.sort_by_key(|p| !p.error);
    res
}

/// What a fix that shares a folder again must keep: the checks it has.
fn checks_again(folder: &Folder) -> String {
    if folder.checks.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = folder.checks.iter().map(String::as_str).collect();
        format!(" (with its --check options again: {})", names.join(", "))
    }
}

/// The flags a fix that shares a folder again must keep: `share add` replaces the folder,
/// and without `--worktrees` its tasks lose their worktrees and its pull request steps.
fn flags_again(folder: &Folder) -> &'static str {
    if folder.worktrees { " --worktrees" } else { "" }
}

fn tool_flags<'a>(tools: impl Iterator<Item = &'a String>) -> String {
    let mut seen = Vec::new();
    for tool in tools {
        if !seen.contains(&tool) {
            seen.push(tool);
        }
    }
    seen.iter().map(|t| format!(" --tool {t}")).collect()
}

/// What `after` asks for that `before` doesn't: more access, MCP servers or tools, checks,
/// Microsoft 365 sources, delegation, a new or changed team plan. A release that asks for
/// any of them needs its owner's sign-off.
pub fn increases(before: &Blueprint, after: &Blueprint) -> Vec<String> {
    let (b, a) = (&before.capabilities, &after.capabilities);
    let every = |tools: &[String]| tools.iter().any(|t| t.trim() == "*");
    let mut found = Vec::new();
    if a.access > b.access {
        found.push(format!("access: {} → {}", b.access, a.access));
    }
    for ask in &a.mcp {
        match b.mcp.iter().find(|m| m.server == ask.server) {
            None if every(&ask.tools) => {
                found.push(format!("MCP server {}: every tool", ask.server))
            }
            None => found.push(format!(
                "MCP server {}: {}",
                ask.server,
                ask.tools.join(", ")
            )),
            Some(old) if every(&old.tools) => {}
            Some(_) if every(&ask.tools) => {
                found.push(format!("MCP server {}: every tool", ask.server));
            }
            Some(old) => {
                let new: Vec<&str> = ask
                    .tools
                    .iter()
                    .filter(|t| !old.tools.contains(t))
                    .map(String::as_str)
                    .collect();
                if !new.is_empty() {
                    found.push(format!("MCP server {}: {}", ask.server, new.join(", ")));
                }
            }
        }
    }
    for check in a.checks.iter().filter(|c| !b.checks.contains(c)) {
        found.push(format!("check {check}"));
    }
    let all = |sources: &[String]| sources.iter().any(|s| s == "all");
    if !all(&b.workiq) {
        if all(&a.workiq) {
            found.push("every Microsoft 365 source".to_owned());
        } else {
            let new: Vec<&str> = a
                .workiq
                .iter()
                .filter(|s| !b.workiq.contains(s))
                .map(String::as_str)
                .collect();
            if !new.is_empty() {
                found.push(format!("Microsoft 365: {}", new.join(", ")));
            }
        }
    }
    if a.delegate && !b.delegate {
        found.push("delegation to other agents".to_owned());
    }
    match (&before.team, &after.team) {
        (None, Some(_)) => found.push("a team plan".to_owned()),
        (Some(old), Some(new)) if old != new => found.push("a changed team plan".to_owned()),
        _ => {}
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::blueprint::tests::{catalog, sample};
    use crate::factory::blueprint::{Offer, from_value};
    use serde_json::{Value, json};

    fn blueprint(change: impl FnOnce(&mut Value)) -> Blueprint {
        let mut value = sample();
        change(&mut value);
        from_value(&value).unwrap()
    }

    fn offer(access: Access, tools: &[&str]) -> Offer {
        Offer {
            access,
            tools: tools.iter().map(|t| t.to_string()).collect(),
            paused: false,
            plugin_off: None,
        }
    }

    /// alice's folders: reports offers pdf (read_pdf, search_pdf) to read-only tasks; app
    /// gives full access, runs test, and offers pdf only to full-access tasks, paused;
    /// notes offers nothing.
    fn folders() -> Catalog {
        let mut catalog = catalog();
        let folder = |access, checks: &[&str], mcp: Vec<(&str, Offer)>| Folder {
            path: format!("/srv/{access}").into(),
            access,
            checks: checks.iter().map(|c| c.to_string()).collect(),
            mcp: mcp.into_iter().map(|(n, o)| (n.to_owned(), o)).collect(),
            worktrees: false,
        };
        let paused = Offer {
            paused: true,
            ..offer(Access::Full, &[])
        };
        let app = Folder {
            worktrees: true,
            ..folder(Access::Full, &["test"], vec![("pdf", paused)])
        };
        catalog.folders = [
            (
                "reports",
                folder(
                    Access::ReadOnly,
                    &[],
                    vec![("pdf", offer(Access::ReadOnly, &["read_pdf", "search_pdf"]))],
                ),
            ),
            ("app", app),
            ("notes", folder(Access::WorkspaceWrite, &[], vec![])),
        ]
        .into_iter()
        .map(|(n, f)| (n.to_owned(), f))
        .collect();
        catalog
    }

    fn codes(res: &Resolution) -> Vec<(&'static str, bool, String)> {
        res.problems
            .iter()
            .map(|p| (p.code, p.error, p.path.clone().unwrap_or_default()))
            .collect()
    }

    fn fix<'a>(res: &'a Resolution, code: &str) -> &'a str {
        res.problems
            .iter()
            .find(|p| p.code == code)
            .and_then(|p| p.fix.as_deref())
            .unwrap_or_else(|| panic!("no {code} in {:?}", res.problems))
    }

    #[test]
    fn an_agent_gets_what_each_folder_offers_and_no_more() {
        let res = resolve(&blueprint(|_| {}), &["reports".into()], &folders());
        assert_eq!(res.problems, Vec::new());
        assert_eq!(
            res.grants,
            [Grant {
                folder: "reports".into(),
                access: Access::ReadOnly,
                mcp: [("pdf".to_owned(), vec!["read_pdf".to_owned()])].into(),
                checks: Vec::new(),
            }]
        );
        assert!(!res.delegate);
        assert_eq!(res.workiq, Vec::<String>::new());
        // Every tool, where only some are offered: those, with a warning.
        let every = resolve(
            &blueprint(|v| v["capabilities"]["mcp"][0]["tools"] = json!(["*"])),
            &["reports".into()],
            &folders(),
        );
        assert_eq!(
            codes(&every),
            [("tools_limited", false, "/capabilities/mcp/0/tools".into())]
        );
        assert_eq!(
            every.grants[0].mcp["pdf"],
            ["read_pdf".to_owned(), "search_pdf".to_owned()]
        );
        // Delegation: only when it asks.
        let lead = resolve(
            &blueprint(|v| v["capabilities"]["delegate"] = json!(true)),
            &["reports".into()],
            &folders(),
        );
        assert!(lead.delegate);
    }

    #[test]
    fn what_isnt_offered_is_an_error_with_the_command_that_offers_it() {
        let notes = resolve(&blueprint(|_| {}), &["notes".into()], &folders());
        assert_eq!(
            codes(&notes),
            [("mcp_not_offered", true, "/capabilities/mcp/0".into())]
        );
        assert_eq!(
            fix(&notes, "mcp_not_offered"),
            "proofkin mcp add notes pdf --access read-only --tool read_pdf --profile alice -- <its command> (or --url <its URL>)"
        );
        assert!(notes.grants[0].mcp.is_empty(), "nothing is offered for it");

        let full = blueprint(|v| {
            v["capabilities"]["access"] = json!("full");
            v["capabilities"]["checks"] = json!(["test", "lint"]);
            v["capabilities"]["mcp"][0]["tools"] = json!(["read_pdf", "write_pdf"]);
        });
        let app = resolve(&full, &["app".into()], &folders());
        assert_eq!(
            codes(&app),
            [
                ("mcp_paused", true, "/capabilities/mcp/0".into()),
                ("check_not_offered", true, "/capabilities/checks/1".into()),
            ]
        );
        assert_eq!(
            fix(&app, "mcp_paused"),
            "proofkin mcp resume app pdf --profile alice"
        );
        assert_eq!(
            fix(&app, "check_not_offered"),
            "proofkin share add app \"/srv/full\" --access full --worktrees --check lint=<command> --profile alice (with its --check options again: test)"
        );
        assert_eq!(app.grants[0].checks, ["test"]);

        let reports = resolve(&full, &["reports".into()], &folders());
        assert_eq!(
            codes(&reports),
            [
                ("access_not_offered", true, "/capabilities/access".into()),
                ("tool_not_offered", true, "/capabilities/mcp/0/tools".into()),
                ("check_not_offered", true, "/capabilities/checks/0".into()),
                ("check_not_offered", true, "/capabilities/checks/1".into()),
            ]
        );
        assert!(fix(&reports, "access_not_offered").starts_with(
            "proofkin share add reports \"/srv/read-only\" --access full --profile alice"
        ));
        assert!(
            reports.problems[1]
                .message
                .contains("doesn't offer write_pdf")
        );
        assert_eq!(
            fix(&reports, "tool_not_offered"),
            "proofkin mcp add reports pdf --access read-only --tool read_pdf --tool search_pdf --tool write_pdf --profile alice -- <its command> (or --url <its URL>; it replaces the offer)"
        );
        assert_eq!(
            reports.grants[0].access,
            Access::ReadOnly,
            "at most the folder's"
        );

        // A server offered only to tasks with more access, or whose plugin is off.
        let mut catalog = folders();
        let pdf = catalog
            .folders
            .get_mut("reports")
            .unwrap()
            .mcp
            .get_mut("pdf")
            .unwrap();
        pdf.access = Access::WorkspaceWrite;
        pdf.plugin_off = Some("pdf-tools".into());
        let res = resolve(&blueprint(|_| {}), &["reports".into()], &catalog);
        assert_eq!(
            codes(&res),
            [
                ("mcp_plugin_off", true, "/capabilities/mcp/0".into()),
                ("mcp_needs_access", true, "/capabilities/mcp/0".into()),
            ]
        );
        assert_eq!(
            fix(&res, "mcp_plugin_off"),
            "proofkin plugin enable pdf-tools --profile alice"
        );
        assert_eq!(
            fix(&res, "mcp_needs_access"),
            "proofkin mcp add reports pdf --access read-only --tool read_pdf --tool search_pdf --profile alice -- <its command> (or --url <its URL>; it replaces the offer), or ask for workspace-write access"
        );

        // Folders that aren't shared, and the data folder, are refused by name.
        let res = resolve(
            &blueprint(|_| {}),
            &["nope".into(), "data".into()],
            &folders(),
        );
        assert_eq!(
            codes(&res),
            [
                ("unknown_folder", true, String::new()),
                ("data_folder", true, String::new())
            ]
        );
        let res = resolve(
            &blueprint(|_| {}),
            &["reports".into()],
            &catalog_without_folders(),
        );
        assert_eq!(
            codes(&res),
            [("unknown_folder", true, "/deploy/folders/0".into())]
        );
    }

    fn catalog_without_folders() -> Catalog {
        Catalog {
            folders: BTreeMap::new(),
            ..catalog()
        }
    }

    #[test]
    fn increases_are_what_a_blueprint_asks_beyond_another() {
        let before = blueprint(|_| {});
        assert_eq!(increases(&before, &before), Vec::<String>::new());
        let after = blueprint(|v| {
            v["capabilities"] = json!({
                "access": "full",
                "mcp": [
                    { "server": "pdf", "tools": ["read_pdf", "write_pdf"], "why": "PDFs" },
                    { "server": "browser", "tools": ["*"], "why": "Pages" }
                ],
                "checks": ["test"],
                "workiq": ["email", "files"],
                "delegate": true
            });
            v["team"] = json!("review @carol {task}");
        });
        assert_eq!(
            increases(&before, &after),
            [
                "access: read-only → full",
                "MCP server pdf: write_pdf",
                "MCP server browser: every tool",
                "check test",
                "Microsoft 365: email, files",
                "delegation to other agents",
                "a team plan",
            ]
        );
        assert_eq!(
            increases(&after, &before),
            Vec::<String>::new(),
            "less is no increase"
        );
        let every = blueprint(|v| v["capabilities"]["workiq"] = json!(["all"]));
        let some = blueprint(|v| v["capabilities"]["workiq"] = json!(["email"]));
        assert_eq!(increases(&some, &every), ["every Microsoft 365 source"]);
        assert_eq!(increases(&every, &some), Vec::<String>::new());
    }
}
