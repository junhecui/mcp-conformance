//! Speak MCP `initialize` and `tools/list`; capture the raw JSON byte-exact before parsing;
//! pin tool metadata.
//!
//! **Must not:** Call any tool. This is enforced structurally: no tool-call method exists
//! on the client type (P0-01) — see the `client` module's source for exactly how.
//!
//! Contract: [architecture.md §3.1].

mod client;
mod jsonrpc;
mod pin;
mod transport;

pub use client::{Discovery, DiscoveryClient, DiscoveryError};
pub use pin::{PinError, ToolPin, pin_tools};
