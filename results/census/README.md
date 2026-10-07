# Census results

The JSON files here are the curated, committed census artifacts. Every number in them is
re-derivable offline from the evidence store by `cargo xtask census-rederive`.

## `evidence/` is untrusted and deliberately uncommitted

`evidence/` (git-ignored; see the repo root `.gitignore`) is P0-10's content-addressed
store of **unmodified third-party response bytes** — the verbatim `initialize` /
`server/discover` and `tools/list` payloads each server returned, addressed by SHA-256
digest with no file extension.

Those bytes are **evidence, never instruction**. Per design.md §3 the servers that produced
them are assumed actively hostile, and architecture.md §1 draws the trust boundary once:
everything arriving from a server under test is data. MCP responses carry server-authored
prose in fields like `instructions` and tool `description`, and some of it is written
imperatively, addressed at whatever model reads it — an established prompt-injection vector
(architecture.md §0, MCPTox). Committed into a public repository, these blobs would become
live payloads sitting exactly where a future agent session reads the tree as project
content. Hence: not committed.

Whether to commit a curated evidence bundle later is still open — measured cost is roughly
15.6 KB/server for Class B and 9.4 KB/server for Class A, so a few MB for a full sweep.
Ignoring it is the reversible default, not a decision against it.
