//! hey-computer: a voice assistant daemon that keeps one ACP agent warm and
//! answers one-shot requests.
//!
//! `daemon` owns the socket, `conductor` the agent and the conversation,
//! `acp` the wire protocol to the agent, `environment` the agent's private
//! directory, `context` opt-in ambient time/desktop headers, `store` persistent
//! state (`last.json`), `present` notifications and the viewer, `policy` the
//! deny list, `protocol` and `ipc` the socket protocol, `mock` a scripted agent
//! for the tests.

pub mod acp;
pub mod attachments;
pub mod conductor;
pub mod config;
pub mod context;
pub mod daemon;
pub mod debug;
pub mod distill;
pub mod environment;
pub mod ipc;
pub mod lines;
pub mod mcp;
pub mod mock;
pub mod policy;
pub mod present;
pub mod protocol;
pub mod routing;
pub mod scheduler;
pub mod store;
