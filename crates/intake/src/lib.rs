//! Resolve registry entries to installable artifacts or endpoints, and assign the containability class (Class A / Class B / unclassifiable).
//!
//! **Must not:** Execute anything, including package install scripts. Guess a class — `unclassifiable` is a real class, not a fallback.
//!
//! P0-03 lands catalogue ingest ([`catalogue`]): resolving one already-fetched `server.json`
//! registry entry into installable artifacts and/or remote endpoints. The containability
//! classifier (P0-04) is a separate, later piece of this crate's contract.
//!
//! Contract: [architecture.md §3.1].

pub mod catalogue;
