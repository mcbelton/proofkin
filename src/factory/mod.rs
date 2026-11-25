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
//! the lessons of its verified work go. [`cli`] is `proofkin factory`.

pub mod blueprint;
pub mod build;
pub mod capabilities;
pub mod cli;
pub mod draft;
pub mod evals;
pub mod memory;
pub mod routing;
pub mod store;
pub mod templates;

pub use blueprint::{Blueprint, Catalog, Problem};
pub use store::FactoryStore;
