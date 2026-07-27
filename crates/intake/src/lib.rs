//! Resolve registry entries to installable artifacts or endpoints, and assign the containability class (Class A / Class B / unclassifiable).
//!
//! **Must not:** Execute anything, including package install scripts. Guess a class — `unclassifiable` is a real class, not a fallback.
//!
//! Contract: [architecture.md §3.1]. Placeholder — see docs/tasks.md.
