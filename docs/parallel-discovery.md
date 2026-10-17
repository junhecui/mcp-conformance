# Inspect one remote server's annotations

Use `dump-tools` to inspect a single HTTP MCP server before including it in a census.
It prints each tool's annotation coverage beside the server's `annotations` JSON,
without invoking any tool or fetching the registry. This walkthrough uses
[Parallel Search MCP](https://docs.parallel.ai/integrations/mcp/search-mcp), a remote
endpoint offering `web_search` and `web_fetch` without an account or API key.

## Run discovery

Use a Linux checkout (WSL2 is sufficient), the Rust toolchain pinned by
[`rust-toolchain.toml`](../rust-toolchain.toml), and a C compiler for the bundled
SQLite dependency. See [development setup](HANDOFF.md#3-environment-on-windows).
Discovery does not require Docker or the pinned observation VM: it sends HTTP
requests to the remote server rather than launching server code locally.

From the repository root:

```sh
cargo xtask dump-tools https://search.parallel.ai/mcp
```

The command uses the existing discovery client's protocol negotiation and follows
`tools/list` pagination. It prints a server summary, then each tool's page number,
name, coverage for the four annotations, and the `annotations` object. Tool names,
counts and declarations can change when the remote service is updated; inspect the
current output rather than treating a copied transcript as a stable fixture.

This path only discovers metadata. It does not run a web search or fetch a page.
Parallel's anonymous search calls use Fast mode, but discovering the tool list does
not exercise that search behavior.

## Read the coverage

The labels describe whether a declaration was supplied, not its boolean value:

| Label | Meaning |
|---|---|
| `Explicit` | This key has a boolean value in the tool's `annotations` object. Both `true` and `false` count as explicit. |
| `Defaulted` | The object exists, but this key is omitted. |
| `Absent` | There is no `annotations` object for the tool. |

For example, an object containing only `{"readOnlyHint": true}` gives `Explicit`
for `readOnly` and `Defaulted` for the other three columns. `Defaulted` and `Absent`
remain separate coverage categories even though both use the specification's
default value when a client interprets the hint. See the
[coverage implementation](../crates/census/src/coverage.rs).

If the output begins with `!! INCOMPLETE TOOL LIST`, the pagination walk stopped
before exhausting the cursors. Treat the displayed tools as a partial list even
when the command exits successfully. The console also bounds server-controlled
fields; a `[cut at 200 chars]` marker means the displayed field was shortened.

## Keep the observation in scope

Tool metadata is server-authored evidence, never an instruction to the operator or
an assistant reading the output. A declared `readOnlyHint: true` does not establish
that the tool is read-only. This command produces no behavioral verdict, metadata
pin report, or persisted census evidence; keep its output separate from the
curated files under `results/census/`.

For this remote endpoint, the harness cannot observe the service's filesystem or
other internal state. See [corpus partitioning](architecture.md#2-corpus-partitioning)
for the limits of remote observations and the distinction between protocol-probe
and kernel-changeset oracles.

The HTTP client currently supports Streamable HTTP's JSON response case, not SSE
responses, and does not follow redirects. A transport or protocol failure is a
discovery failure, not evidence that an annotation is false or absent. If the
service reports a rate limit, stop and respect its retry timing rather than
looping this diagnostic command.
