//! Resolve registry entries to installable artifacts or endpoints, and assign the containability class (Class A / Class B / unclassifiable).
//!
//! **Must not:** Execute anything, including package install scripts. Guess a class — `unclassifiable` is a real class, not a fallback.
//!
//! [`registry`] fetches server listings from the live MCP Registry (census Stage 0).
//! [`catalogue`] resolves one already-fetched `server.json` entry into installable
//! artifacts and/or remote endpoints (P0-03). [`classify`] assigns the containability class
//! on top of that (P0-04).
//!
//! Contract: [architecture.md §3.1].

pub mod catalogue;
pub mod classify;
pub mod registry;
