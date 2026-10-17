# Rust embedding and native plugins

Cog 0.1.0 is experimental. `cog::sdk` is the owned embedding API; other compiler
and VM modules remain implementation interfaces. The dynamic plugin boundary is
**C ABI v1**, defined by [cog_plugin.h](../include/cog_plugin.h) and
`cog::sdk::abi`. No Rust ABI, allocator, `String`, `Value`, trait object or panic
crosses that boundary. Plugins must match the host OS, architecture and C calling
convention. Loading a library executes trusted code with process authority.

## Embedding and registration

```rust
use cog::sdk::{Engine, Signature, Type, Value};

let mut engine = Engine::new();
engine.register("host::double", Signature::new(vec![Type::Int], Type::Int),
    |_, arguments| {
        arguments[0].as_int()?.checked_mul(2).map(Value::int)
            .ok_or_else(|| cog::sdk::Error::new("integer overflow"))
    })?;
let program = engine.compile("embedded.cog",
    "include std; use host; use io; fn main() { io::println(host::double(21)); }")?;
let program = engine.decode(&program.encode()?)?;
drop(engine);
program.run(&mut std::io::stdout())?;
# Ok::<(), cog::sdk::Error>(())
```

Run the complete example with `cargo run --locked --offline --example embedding`.
`compile_project` resolves filesystem projects; `build_project` also writes the
artifact and resolution lock and uses the normal build cache. `read` bounds and
validates a bytecode reader. `Program::write` writes an encoded artifact atomically.
Source check/build supports metadata-only registrations and never needs a loaded
library. Registrations are engine scoped; compiled/decoded programs own their
resolved imports and may outlive their engine. Cache fingerprints include native
metadata. A decoded extension import must match its exact canonical metadata
fingerprint (name, ABI version, parameter/result types, effects and mutation
indices). This identifies a contract; it provides no publisher authentication.
Locally built unsigned artifacts remain supported.

Register names as `module::function` with ASCII identifiers. Standard-library
modules are reserved. Each function accepts at most 64 positional parameters.
`Signature::with_mutating_parameters` takes strictly increasing argument indices;
the compiler rejects calls that expose const aliases to these mutations. Trusted
callbacks must honor the declared mutations and effects, including changes through
captured aliases. Parameter tags are the coarse native types in the C header,
not nominal Cog struct identities or the complete source annotation grammar.
All typed boundaries retain runtime checks. A typed Error argument bypasses the
callback and propagates; `Any` can inspect/return it. A wrong successful return
type, callback error or caught panic becomes the catchable `err::Error::NativeFailure`.

## Ownership, contexts and limits

Engine, Program and Value are thread confined (`!Send`/`!Sync`). Callbacks are
synchronous and owned (`'static` captures, including `Rc` shared state). Cloning
a Value preserves aliases and roots its graph. The SDK provides primitive/string
constructors and checked array creation/access/mutation. Other VM categories can
be retained and passed back unchanged; v1 does not expose raw resource pointers,
arbitrary record construction or custom host-pointer values. Release retained
graphs when no longer needed. Unreachable Cog graph cycles are traced by the VM.

`RunContext` borrows its output writer, owns its input/resources, and is intended
for one run. The VM finishes resources on all exits; resource values retained by
callbacks cannot revive a closed slot or transfer it into another context.
Dropping callback captures releases ordinary Rust resources. `NativeContext`
borrows one call's I/O, with checked output/read helpers; it cannot escape a safe
Rust callback. SDK v1 rejects compiler/VM reentry from native callbacks. Async
callbacks and cross-thread calls into a borrowed context are unsupported.

Default permissions provide normal process authority. Optional `Permissions` and
`Limits` configure host policy and VM counters. All `Limits` fields default to
`None`, including `max_buffer_bytes` and `max_resources`; set `Some(n)` to opt into
those VM quotas. The resource count includes standard stream slots. Declared
plugin effects are checked before invocation; checked host I/O also checks its
permissions. Native code can
make OS calls independently: these flags are not an OS sandbox. Limits do not
bound all process memory, plugin allocations or blocking native/system calls.
Rust `Rc`/string payload allocation can abort on system OOM. Catching unwind does
not catch `panic=abort`, signals, foreign exceptions or native memory corruption.
Rust's panic hook may print even when a panic is caught. User capture destructors
run under normal Rust Drop rules, outside the callback panic guard.

Apply explicit limits to one run with `RunContext::with_limits`:

```rust
use cog::sdk::{Limits, RunContext};

let limits = Limits {
    max_buffer_bytes: Some(8 * 1024 * 1024),
    ..Limits::default()
};
let mut output = Vec::new();
let mut context = RunContext::new(&mut output).with_limits(limits);
program.run_in(&mut context)?;
```

## Independently built CLI plugins

The CLI takes explicit, repeatable `--plugin path/to/plugin.kdl` arguments:

```kdl
plugin version=1 library="./libnative_plugin.so"
function "host::double" result="int" { param "int"; }
function "host::fail" result="nil"
```

Library paths are relative to the manifest file; use `.dylib` on macOS and `.dll`
on Windows. Function children accept `param "type"`, `effect "name"`, and
`mutates 0` nodes. Effects are `output`, `input`, `fs_read`, `fs_write`, `process`,
`process_info`, `clock`, `sleep`, `entropy`. Unknown/duplicate metadata, unsupported
versions, excessive arity, oversized/deep manifests and descriptor mismatches are
rejected. A library's descriptor must match the manifest's entire function set.
Only `run` loads the library; `check` and `build` use its metadata without loading
it, including when the library is missing. Deployment must provide matching
manifests/libraries explicitly; source-free bytecode does not bundle native code.
`--restricted` is optional host policy and cannot contain a trusted plugin.

```sh
cog check main.cog --plugin plugin.kdl
cog build main.cog --plugin plugin.kdl -o program.cogb
cog run program.cogb --plugin plugin.kdl
```

The Rust [cdylib example](../examples/native_plugin.rs) uses C descriptors and
`abi::dispatch`, which catches unwinding callback panics before returning through C.
Build it against a separately compiled Cog library (Linux example):

```sh
cargo build --locked --offline --lib
rustc --edition=2024 --crate-type=cdylib --crate-name=native_plugin \
  examples/native_plugin.rs --extern cog=target/debug/libcog.rlib \
  -L dependency=target/debug/deps -o target/libnative_plugin.so
```

If Cargo uses an explicit `--target`, adjust the library/dependency paths to
`target/<triple>/debug` and also supply `-L dependency=target/debug/deps` for host
proc macros. Rust dependencies must use the same toolchain; this is a build-time
requirement for the wrapper, not the dynamic ABI. C/C++ plugins need only the
header and their own compiler. The test fixture in
[tests/fixtures/native_plugin.c](../tests/fixtures/native_plugin.c) builds and loads
independently on Unix. `cargo test --locked --offline --test sdk --test sdk_plugins`
checks embedding, metadata-only CLI builds, separately built Rust/C libraries,
errors, panic containment, resource invalidation and library/state lifetime.

## C ABI lifetime and failure contract

- Export `cog_plugin_v1` with the C calling convention. Its immutable descriptor
  and function metadata remain valid until unload. `size` must equal `sizeof`
  the v1 structure and `version` must equal 1. Reserved numeric tags are rejected;
  incompatible layout changes require another ABI version.
- Views are length-delimited, without a NUL requirement. Empty views may be null;
  nonempty views and typed arrays must be valid/aligned for their complete length.
  Names and strings are UTF-8. The host copies descriptor metadata before use.
  Checks detect compatibility mistakes; they cannot make malicious pointers safe.
- `create`/`destroy` are either both absent or both present. `create` writes a
  plugin-owned instance pointer (initially null); the host never dereferences or
  frees it. On any subsequent registration failure or final release, `destroy`
  runs once, including for null/partially initialized state after create failure.
  Initialize that state so destruction is safe. No call overlaps destruction.
  Rust authors may use `abi::create_state`/`destroy_state` in C wrappers to contain
  unwinding initialization/destruction panics. Never exchange Rust allocations.
- Every registered callback owns a shared reference to the instance and library.
  Engines and compiled/decoded programs retain callbacks; the last release calls
  destroy before unloading. External threads, async work, borrowed library data
  retained by the host and callbacks after return are unsupported in ABI v1.
- During invoke, the host table, call context, argument handles and borrowed
  string views are valid only until return, on the calling thread. All host
  functions are synchronous. Retain copies in plugin-owned memory if needed.
  Opaque handles have per-call generations and are rejected in other calls;
  they are not addresses. At most 4096 values are rooted per call; new string/array
  data consumes the configured buffer budget. Call-generation exhaustion fails
  without wrapping. Arbitrary nested/opaque Cog values can be forwarded unchanged.
- `inspect` supports primitives, strings and array length; other values are opaque
  (Errors have a separate kind). `make` supports nil/int/float/bool/string/array;
  array access/mutation is checked. No VM internals or Rust layouts are exposed.
  Inspected string storage stays rooted throughout invoke. A successful callback
  writes a valid result handle. Nonzero status, set_error or an invalid result
  becomes NativeFailure. Error text is copied with a 64 KiB limit. Never unwind
  through C or throw C++ exceptions into the host. A failed host operation returns
  status and must be handled by the plugin.

Cross-platform support and release readiness require actual target CI results;
successful local Linux tests do not establish Windows/macOS/MSRV support.
