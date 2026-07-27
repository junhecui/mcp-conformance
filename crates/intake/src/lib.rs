//! Resolve registry entries to installable artifacts or endpoints, and assign the containability class (Class A / Class B / unclassifiable).
//!
//! **Must not:** Execute anything, including package install scripts. Guess a class — `unclassifiable` is a real class, not a fallback.
//!
//! P0-03 lands catalogue ingest ([`catalogue`]): resolving one already-fetched `server.json`
//! registry entry into installable artifacts and/or remote endpoints. P0-04 lands
//! containability classification ([`classify`]) on top of it.
//!
//! Contract: [architecture.md §3.1].

pub mod catalogue;
pub mod classify;
