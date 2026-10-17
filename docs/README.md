# Cog documentation

Cog is experimental. These pages describe the current compiler and VM unless a
section explicitly says **planned**. Code examples marked as Cog are checked by
the test suite; proposed syntax in the design contracts is marked as text.

Start with the [repository README](../README.md), the
[getting-started page](../web/get_started.html), or the [runnable examples](../examples/README.md).

| Reference | Contents |
| --- | --- |
| [Language](LANGUAGE.md) | Bindings, types, functions, control flow, patterns, strings, and collections |
| [Errors and cleanup](ERRORS.md) | Error values, lexical handlers, propagation, fallbacks, and defer |
| [Standard library](STANDARD_LIBRARY.md) | Generic array search and sorting, cloning, text, ASCII, hex, Base64, bytes, streams, time, and processes |
| [Projects and configuration](PROJECT_FORMATS.md) | Modules, KDL manifests, dependencies, lockfiles, targets, and caching |
| [Runtime and bytecode](RUNTIME.md) | Compilation pipeline, artifacts, native ABI, and embedding boundaries |
| [Rust SDK and plugins](SDK.md) | Owned embedding API, native registration, C ABI v1 and plugin lifecycle |
| [Semantic contracts](SEMANTIC_CONTRACTS.md) | Implemented invariants, accepted future specifications, and open decisions |
| [Contributing](CONTRIBUTING.md) | Source layout, validation, diagnostics, and performance tools |

The [roadmap](../ROADMAP.md) separates completed work, near-term priorities, and
longer-term goals. A design example or ignored specification test does not imply
that its syntax is implemented.
