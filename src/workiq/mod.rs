//! Work IQ: Microsoft 365 work context for plans and tasks (see docs/work-iq.md).
//!
//! [`client`] is the MCP client. It talks to a [`Transport`] (streamable HTTP here; a stdio
//! transport for Microsoft's CLI can reuse the client) with a bearer from a [`TokenSource`].

pub mod account;
pub mod client;
pub mod login;

pub use client::*;
