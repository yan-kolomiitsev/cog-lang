# Cog roadmap

Cog is preparing its first Beta release. This roadmap summarizes implemented
capabilities, release work still in progress, and the next language goals. It is
not a release schedule. See the [language reference](docs/LANGUAGE.md),
[standard library reference](docs/STANDARD_LIBRARY.md), and
[semantic contracts](docs/SEMANTIC_CONTRACTS.md) for current behavior.

## Implemented

- A recovering lexer/parser, separate semantic analysis, located diagnostics,
  module resolution, and direct stack-bytecode generation.
- A stack VM with checked frames, control flow, locals, callable boundaries,
  source maps, a generational graph heap, iterative cycle collection, and
  optional execution limits. The optional object-count limit is not a byte or
  whole-process memory limit.
- Portable `.cogb` v10 artifacts with bounded decoding and structural
  verification. The VM checks declared callable argument/result types at call
  boundaries; static value-flow verification is still in progress.
- Packages, KDL configuration, path dependencies, `cog.lock`, explicit imports,
  build targets, cache invalidation, and contained compile-time text/binary
  inputs.
- Structs, enums, typed arrays/tuples, generic functions and structs, trait
  bounds, generic receiver methods, nullable/Error flow analysis, and compile-
  time functions with error handling.
- Arrays, Unicode-scalar string traversal, insertion-ordered hashed `Map`,
  bounded JSON, process arguments/environment, path and filesystem
  APIs, byte streams, and Cog implementations of common search, sort, string,
  and encoding utilities.
- A tutorial and CI/fuzz infrastructure. Short fuzz smoke runs and local Linux
  validation have been recorded; sustained fuzz campaigns remain outstanding.
  CI covers Linux x86_64, Windows x86_64, and macOS arm64 on Rust 1.95 and stable.
- A Rust SDK for embedding and native registration, plus CLI plugins across a
  versioned C ABI v1 with call-scoped handles and retained library lifetimes.

## Before the first Beta release

- Complete static bytecode value-flow checks while preserving dynamic type
  failures as recoverable `Error` values.
- Finish the contract review of the Rust native SDK and CLI plugin interface,
  including ownership, error paths, panic containment, and library lifetimes.
- Complete the broader compiler/runtime resource audit. Keep ordinary host
  authority and runtime limits optional; do not claim whole-process memory
  isolation.
- Run the agreed sanitizer/fuzz campaigns and validate the final revision on Linux x86_64, Windows
  x86_64, and macOS arm64 with Rust 1.95 and stable, and verify clean packaging
  and installation.
- Close release-blocking defects and document remaining limitations before
  preparing 0.1.0 release artifacts.

## After Beta: language completeness

1. Add first-class function values and source-level closures, including capture,
   aliasing, and error semantics. The runtime has closure-related support, but
   the source grammar does not yet expose closures or indirect calls.
2. Extend compile-time programming with value parameters, canonical
   specialization, and materialization of arrays, tuples, and enum values.
3. Resolve trait implementation coherence/visibility rules and user-defined
   iteration before expanding generic abstractions further.
4. Continue measured compiler/runtime optimization. JIT remains a longer-term
   goal and must match interpreter behavior on representative workloads.

## Readiness

The current assessment is **Alpha**. Beta requires the release work above and
evidence for each acceptance gate. A passing local test suite does not establish
cross-platform readiness, complete bytecode type safety, or production
suitability. Detailed status and historical audit evidence are kept in ignored
`dev_docs/` files and are not included in Cargo packages.
