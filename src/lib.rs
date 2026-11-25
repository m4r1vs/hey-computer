//! hey-computer: a voice assistant daemon that keeps one ACP agent warm and
//! answers one-shot requests.
//!
//! `daemon` owns the socket, `conductor` the agent and the conversation,
//! `acp` the wire protocol to the agent, `environment` the agent's private
//! directory, `context` opt-in ambient time/desktop headers, `store` persistent
//! SQLite state (`history.db`), `present` notifications, TTS and the viewer,
//! `render` and `highlight` pure-Rust Markdown/MathML/Flexoki rendering,
//! `policy` the deny list, `protocol` and `ipc` the socket protocol, `mock` a
//! scripted agent for the tests.

pub mod acp;
pub mod attachments;
pub mod bridge;
pub mod conductor;
pub mod config;
pub mod context;
pub mod daemon;
pub mod debug;
pub mod distill;
pub mod environment;
pub mod highlight;
pub mod ipc;
pub mod lines;
pub mod mcp;
pub mod mock;
pub mod policy;
pub mod present;
pub mod protocol;
pub mod render;
pub mod routing;
pub mod scheduler;
pub mod store;
