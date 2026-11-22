//! Peer-to-peer agents that connect over the internet with iroh (QUIC, NAT
//! traversal, relays), authenticate each other by public key against an
//! allowlist, and run or delegate LLM coding tasks with the GitHub Copilot,
//! Claude Agent and Codex SDKs. Each agent uses its own provider instances and
//! accounts (see [`profile`]).

pub mod agent;
pub mod audit;
pub mod auth;
pub mod boundary;
pub mod design;
pub mod evidence;
pub mod github;
pub mod knowledge;
pub mod llm;
pub mod mcp;
pub mod ops;
pub mod plan;
pub mod playbooks;
pub mod plugins;
pub mod preview;
pub mod profile;
pub mod protocol;
pub mod redact;
pub mod render;
pub mod runs;
pub mod schedule;
pub mod setup;
pub mod shares;
pub mod skills;
pub mod spec;
pub mod spec_import;
pub mod store;
pub mod team;
pub mod tracker;
pub mod trust;
pub mod ui;
pub mod workers;
pub mod workiq;
pub mod worktree;

pub use agent::{Agent, AgentConfig, ApprovalInfo, Delivered, Event, Network, PeerInfo};
pub use iroh::{EndpointAddr, EndpointId, SecretKey};
pub use llm::{Access, Executor, ExecutorConfig, LlmTask, Provider, SdkExecutor, TaskContext};
pub use profile::Profile;
pub use protocol::Message;
pub use trust::{Ticket, TrustStore, TrustedPeer};
