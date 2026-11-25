//! The agent factory (docs/agent-factory.md): an owner describes an agent, and the
//! factory turns it into one that proves itself before it works. An agent is a
//! [`blueprint`]: its whole definition as data, checked against the profile it is for
//! and linted for least privilege. Its eval suite ([`evals`]) has a case for every
//! requirement, graded deterministically. The profile's factory [`store`] keeps drafts
//! and each agent's lifecycle. Drafts come from built-in [`templates`] or are written
//! by the owner's model from plain words ([`draft`]), which never grants anything.
//! [`cli`] is `proofkin factory`.

pub mod blueprint;
pub mod cli;
pub mod draft;
pub mod evals;
pub mod store;
pub mod templates;

pub use blueprint::{Blueprint, Catalog, Problem};
pub use store::FactoryStore;
