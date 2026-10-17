# Developing Cog

The [fuzz workspace](../fuzz/README.md) contains frontend, decoder and verifier
targets. Run instrumented smoke checks when changing their input boundaries;
record sustained campaigns separately from unit and integration test results.

A guide to the source tree, local checks, and performance tools.

## Source layout

| Location | Responsibility |
| --- | --- |
| `src/lexer.rs`, `string_literals.rs`, `hlparser.rs`, `ast.rs` | Frontend and source representation |
| `src/resolver.rs`, `project.rs`, `config.rs`, `lockfile.rs` | Modules, package discovery, KDL, and resolution |
| `src/analysis.rs`, `arguments.rs`, `mutability.rs` | Semantic checking, argument binding, capabilities/effects |
| `src/codegen.rs`, `backend.rs` | Direct instruction emission |
| `src/bytecode.rs`, `build.rs` | Artifact codec and build cache |
| `src/interpreter.rs`, `runtime.rs`, `runtime/`, `value.rs` | VM, native ABI, resources, and values |
| `src/main.rs`, `cli_output.rs` | CLI and diagnostics/status presentation |
| `src/parser.rs` | Standalone diagnostic assembler |
| `core/` | Embedded Cog standard-library sources |
| `tests/` | Feature regressions, project/CLI tests, and ASM fixtures |
| `examples/` | Runnable learning examples and small projects |
| `web/` | Interactive tutorial and local TypeScript server |
| `benches/`, `src/bin/bench_*.rs` | Performance tools |

## Validation

```sh
cargo build --locked --offline
cargo test --locked --offline --no-fail-fast
cargo clippy --locked --offline --all-targets -- -D warnings
cargo test --locked --offline --test examples --test documentation
cargo bench --locked --offline --bench bench_interpreter --no-run
```

Omit `--offline` when dependencies are not cached. Rust formatting uses rustfmt. The optional tutorial backend is tested with
`node --test web/server.test.mts` (Node.js 24.12+).

## Tests

Feature regressions cover source compilation, bytecode round trips, VM execution,
error paths, and cleanup. Project tests cover discovery, manifests, dependencies,
locks, caching, and CLI behavior. Assembler, loader, and runtime tests exercise the
lower-level interfaces directly.

`tests/documentation.rs` compiles the standalone Cog examples in Markdown and HTML.
`tests/examples.rs` runs the learning examples and interactive input/EOF cases,
including both small example projects. Process tests in `tests/projects.rs` and
`tests/runtime.rs` account for platform differences.
`tests/standard_library.rs` checks math domains and numeric boundaries, random
range validation, sorting aliases, and error inspection through the bytecode codec.

Ignored tests in `tests/semantic_contracts.rs` and `tests/build_input_contracts.rs`
describe features that are not implemented yet. Their ignore reasons and the
[semantic contracts](SEMANTIC_CONTRACTS.md) explain the expected behavior.

## Performance tools

```sh
cargo bench --locked --offline --bench bench_interpreter
cargo run --locked --offline --release --bin bench_runtime
cargo run --locked --offline --release --bin bench_strings -- 2000 5
cargo run --locked --offline --release --bin bench_harness
cargo run --locked --offline --release --bin bench_repeated -- 10000
```

The Criterion suite measures arithmetic and array indexing. `bench_runtime`
compares native calls, formatting, I/O, loops, and cold decode/link; `bench_strings`
compares identical interpolation/addition outputs without VM stdout and excludes
compilation/linking/codec/warmup. Its arguments select strings per sample and sample
count. The workloads stay within runtime limits so they compare successful results.
`bench_harness` and `bench_repeated` isolate VM workloads and repeated invocation.

Results depend on the host, build mode, workload, sample count, and timing scope.
These tools help compare changes locally; they do not establish a portable speedup.

## License and contributions

Cog is dual-licensed under MPL-2.0 OR Apache-2.0, at the recipient's option.
Contributions are accepted under both licenses. See [LICENSE](../LICENSE) and the
full [MPL-2.0](../LICENSE-MPL-2.0) and [Apache-2.0](../LICENSE-APACHE-2.0) texts.
