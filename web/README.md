# Interactive Cog tutorial

Requires Rust 1.95 or newer, Cargo, and Node.js 24.12 or newer. From the repository root:

```sh
node web/server.mts
```

Open **http://127.0.0.1:4173/web/get_started.html**. Edit an example, optionally add
stdin input, and choose Run. Output and compiler/runtime diagnostics appear below
the editor. Reset restores the original example; Ctrl+Enter or Command+Enter runs it.

The server builds Cog with locked, offline Cargo, then invokes the real compiler
and runs its `.cogb` artifact. If dependencies are not cached, first run
`cargo build --locked --bin cog` with network access. There are no npm dependencies
or JavaScript implementations of Cog semantics.

`PORT` changes the local port; `COG_BIN` selects an already-built Cog executable
that supports `--restricted`. The default binds only to loopback. Runs allow stdin
and stdout, deny other runtime host effects, and have time/output limits. This is
a local learning tool, not a service for hosting untrusted code. CLI runtime limits
do not impose a total VM memory quota.

The HTML remains readable when opened directly or without JavaScript; execution
requires the local server. Language documentation is in [docs/](../docs/README.md).

```sh
node --test web/server.test.mts
cargo test --locked --offline --test documentation --test projects
```
