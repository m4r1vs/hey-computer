//! hey-computer: a voice assistant daemon that keeps one ACP agent warm and
//! answers one-shot requests.
//!
//! `environment` prepares the agent's private directory, `present` shows
//! answers, `policy` holds the deny list, `protocol` is the socket protocol.

pub mod config;
pub mod environment;
pub mod lines;
pub mod policy;
pub mod present;
pub mod protocol;
