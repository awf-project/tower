//! Shared-daemon hub: one daemon per workspace owns the engine; `tower mcp`
//! clients relay stdio over a Unix socket. Pure infrastructure (sockets,
//! processes, framing) — the `domain/` crate is never imported here.
#![forbid(unsafe_code)]

pub mod client;
pub mod engine;
pub mod server;
pub mod session;
pub mod socket;
pub mod wire;
