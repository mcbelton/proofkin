//! The agent factory (docs/agent-factory.md): an owner describes an agent, and the
//! factory turns it into one that proves itself before it works. An agent is a
//! [`blueprint`]: its whole definition as data, checked against the profile it is for
//! and linted for least privilege. Its eval suite ([`evals`]) has a case for every
//! requirement, graded deterministically. The profile's factory [`store`] keeps drafts,
//! builds and each agent's lifecycle. Drafts come from built-in [`templates`] or are
//! written by the owner's model from plain words ([`draft`]), which never grants
//! anything.
//!
//! A blueprint [`build`]s into the files an agent is made of, the same bytes everywhere.
//! What it asks for is resolved against what its owner offers where it works
//! ([`capabilities`]); its model policy says which models it may run on and which one a
//! release takes ([`routing`]); its [`memory`] says what knowledge it reads and where
//! the lessons of its verified work go. [`cli`] is `proofkin factory`; an agent may also be
//! kept as code, in a folder under git ([`code`]).
//!
//! An evaluation runs a build's cases, with the built-in [`adversarial`] ones, on each
//! candidate model, a few times each, on the owner's evaluation worker, and grades the
//! runs deterministically; [`judge`]s of another model family score rubrics, never
//! deciding. Its signed [`report`] says whether it passed the release gate and which
//! candidate a release takes.
//!
//! A build that passed is [`release`]d: a signed, immutable manifest of its files' hashes,
//! with a semantic version, accepted by the approvers its blueprint names ([`signoff`]).
//! Plans use an agent's active release (or one by version), and a [`deploy`]ment runs it
//! as a digital worker pinned to it, after a canary, within its budgets; it can be rolled
//! back, deprecated and retired ([`lifecycle`]), and its [`metrics`] come from its
//! workers' audits. [`manage`] is the command line for all of it.

use std::fmt;

pub mod adversarial;
pub mod blueprint;
pub mod build;
pub mod capabilities;
pub mod cli;
pub mod code;
pub mod deploy;
pub mod draft;
pub mod evals;
pub mod judge;
pub mod lifecycle;
pub mod manage;
pub mod memory;
pub mod metrics;
pub mod release;
pub mod report;
pub mod routing;
pub mod runner;
pub mod signoff;
pub mod store;
pub mod templates;

pub use blueprint::{Blueprint, Catalog, Problem};
pub use store::FactoryStore;

/// Why the factory won't do what it was asked: what is wrong, and which kind of wrong, so
/// the web UI's API answers with the status code its contract gives (404, 400 or 409).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub kind: RefusalKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalKind {
    /// There is no such agent, release or evaluation.
    NotFound,
    /// The request itself is wrong: a version, a value out of range, a move the state
    /// doesn't allow.
    Invalid,
    /// It can't be done as things are: an evaluation that didn't pass, a capability the
    /// folders don't offer, an agent that is retired.
    Conflict,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refusal {}

pub fn not_found(message: impl Into<String>) -> anyhow::Error {
    Refusal {
        kind: RefusalKind::NotFound,
        message: message.into(),
    }
    .into()
}

pub fn invalid(message: impl Into<String>) -> anyhow::Error {
    Refusal {
        kind: RefusalKind::Invalid,
        message: message.into(),
    }
    .into()
}

pub fn conflict(message: impl Into<String>) -> anyhow::Error {
    Refusal {
        kind: RefusalKind::Conflict,
        message: message.into(),
    }
    .into()
}

/// The kind of refusal `error` is, if it is one (anywhere in its chain).
pub fn refusal_kind(error: &anyhow::Error) -> Option<RefusalKind> {
    error
        .chain()
        .find_map(|e| e.downcast_ref::<Refusal>())
        .map(|r| r.kind)
}
