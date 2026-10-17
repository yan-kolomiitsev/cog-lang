# Repository Guidelines

## Project Structure & Architecture

Cog is a Rust 2024 compiler and stack VM. `src/lib.rs` exposes the library; `src/main.rs` implements the CLI. Frontend files include `lexer.rs`, `hlparser.rs`, `ast.rs`, and `analysis.rs`; `resolver.rs` handles names/modules. `codegen.rs` and `backend.rs` emit instructions directly; `bytecode.rs` serializes `.cogb`, and `interpreter.rs` executes it. `parser.rs` is the standalone diagnostic assembler.

`core/` contains Cog standard-library sources. `tests/` contains Rust integration tests and `.asm` fixtures; unit tests live beside implementations. `benches/` and `src/bin/bench_*.rs` contain performance tooling. Root `.cog` files and demo directories provide examples. Consult `docs/` and `ROADMAP.md` for current decisions.

## Build, Test, and Development Commands

- `cargo build --locked`: build the compiler/VM.
- `cargo test --locked`: run unit, integration, and documentation tests.
- `cargo test --locked --test semantics`: run language-behavior regressions.
- `cargo clippy --locked --all-targets`: lint library, binaries, and tests.
- `cargo run -- check match_smoke.cog`: check source without writing artifacts.
- `cargo run -- run match_smoke.cog`: build/cache bytecode and execute it.
- `cargo install --path .`: refresh the installed CLI after validated compiler builds.

Add `--offline` to Cargo commands when dependencies are already cached.

## Coding Style & Naming Conventions

Use four-space indentation and Rust conventions: `snake_case` functions/modules, `PascalCase` types/variants, and `SCREAMING_SNAKE_CASE` constants. Format touched Rust code with rustfmt; avoid unrelated mass-formatting. Keep semantic checking separate from emission. Ordinary compile errors must produce recoverable diagnostics, not panic.

## Publication Language

All publishable material must be in English: source comments, user-facing text, diagnostics, examples, tests, documentation, and release material. Match the user's preferred language in conversation. Preserve non-ASCII coverage in Unicode regression fixtures without introducing Russian prose.

## Testing Guidelines

Use Rust `#[test]` with descriptive `snake_case` behavior names. Add focused regressions for fixes. Semantic tests should verify evaluation order, side effects, errors, and cleanup through bytecode encode/decode and execution. Check project/configuration changes in `tests/projects.rs`; prefer observable behavior over disassembly-only assertions.

## Configuration & Design Changes

User configuration uses `package.kdl`, `dependencies.kdl`, and optional `build.kdl`. Generated resolution belongs in `cog.lock`; disposable artifacts/cache belong in `target/`. Verify documentation against code and current owner decisions. Breaking changes are acceptable when justified; update tests/examples/docs together and avoid compatibility scaffolding.

## Commit & Pull Request Guidelines

Prefer concise imperative commit subjects describing the change. PRs should explain the problem, resulting behavior, validation, and breaking changes; link issues when relevant. Preserve unrelated working-tree edits and ignored local material. The Git repository is rooted in this project: scope Git operations to this directory.
