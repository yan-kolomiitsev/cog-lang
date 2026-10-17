# Runtime and bytecode

The compiler emits instructions directly; textual assembly is a separate diagnostic
tool. Cog currently executes on an interpreter with a stack VM. JIT is planned.

The frontend applies finite input budgets: 16 MiB per source, 64 MiB for project
source snapshots, 262,144 tokens per source (including interpolation), 100 frontend
diagnostics, 64 recursive parser levels and 128 expression-tree levels. Package
discovery limits packages to 256, modules to 4096 and namespace nesting to 64.
The Rust API exposes `limits::CompilerLimits` and `frontend::parse_with_limits`;
recursive depth settings can be lowered but cannot exceed the safety ceilings.
These compiler limits are independent of optional runtime execution settings.
Loaded/generated syntax and comptime allocation have conservative 64 MiB accounting
budgets. KDL configuration is limited to 256 KiB and 64 nesting levels, including
nested comments and slashdash prefixes, before entering the KDL parser. These
budgets do not establish a whole-process memory guarantee.

```text
UTF-8 source → recovering lexer/parser → module/name resolution → semantic checking
             → direct instruction emission → .cogb → validated loader/linker → VM
```

## Executable artifacts

One `.cogb` links the chosen entry point and imported module/package code. It runs
without sources, manifests, or recompilation. Artifacts are experimental: rebuild
after incompatible format changes; older versions are rejected.

The current format is v10: magic `COG\0`, little-endian `u16` version and flags,
`u32` import count and imports, a `u32` callable count and callable descriptors,
then `u32` instruction count and explicit instruction encodings, followed by a
source-map table. Each map entry stores a half-open instruction range and a
length-prefixed UTF-8 file name with one-based line and column coordinates. Each
callable descriptor stores its entry, parameter count, capture count, an optional
parameter contract table, and an optional result contract. When present, the table
contains one optional runtime type per parameter; it is omitted when every parameter
is dynamic. Entries are unique and sorted; they partition startup and
function regions. Rebuild v9 and earlier artifacts. Native imports store a length-prefixed UTF-8 symbolic name,
ABI version, parameter count/types, and successful-result type. Imports are sorted
by identity and linked once. Host pointers and Rust enum layout are not serialized.

ABI v1 native tags are any=0, int=1, str=2, bytes=3, handle=4, nil=5, number=6,
array=7, record=8, float=9, bool=10, strbuf=11. These are runtime descriptors,
not the available source annotation grammar. Array values carry recursive element
type metadata, and runtime mutations enforce that invariant. The format limits
imports to 4096 and parameters per import to 64. Files are limited to 128 MiB,
aggregate static strings to 64 MiB, and constant/type nodes to 1,000,000;
instruction/constant/slot bounds are also enforced by the codec.

`CallNative(index)` consumes its arguments and produces one result. Calls borrow
arguments from the operand stack and dispatch through a linked descriptor/function
pointer; builtins need no per-call symbolic lookup or argument-vector allocation.
SDK callbacks retain owned argument roots, and dynamic plugins use a per-call
handle arena through [C ABI v1](SDK.md).
Recoverable native failure becomes Error; invalid VM state and crash reach the host
as terminal failures. The compiler-private cleanup merge uses this same ABI.

The loader checks tags, sizes, UTF-8, constant nesting, native indices, import
signatures, callable entries, direct-call/capture arities, function-region branches,
operand heights at control-flow joins, definite local initialization, and descriptor
shape. Verification has bounded analysis work. The VM checks annotated arguments
and results at direct and indirect call boundaries; mismatches produce the language's
recoverable `Error` value, and existing `Error` values continue to propagate through
typed boundaries. Full static value-type verification remains planned. Source
maps carry file/line/column metadata without embedding source text. The verifier
also tracks bounded value, callable, capture, aggregate-field, and exact tagged
variant facts; this partial analysis does not prove general type safety. The CLI adds
the mapped location to terminal runtime errors, and embedders can inspect
`RuntimeContext::last_source_location`; catchable Errors handled by Cog are not
annotated. This does not provide OS isolation or a total memory guarantee. Runtime buffers, handles,
reference graphs, cells, Error values, and open builders cannot be serialized as
constants. Artifacts are unsigned; no publisher authenticity is implied by loading.

## Embedding

Use the owned [Rust SDK](SDK.md) for embedding and native registration. The
lower-level modules below remain compiler/VM implementation interfaces.

The Rust library exposes `project::compile_source`, `bytecode::encode/decode/read/validate`,
`RuntimeContext`, `VmCapabilities`, `RuntimeLimits`, and
`interpreter::run_with_context`. See [tests/runtime.rs](../tests/runtime.rs) for
custom inputs/writers, failure injection, and restricted execution examples.

`project::compile_source` compiles an in-memory source and supports the embedded
`include std;` declaration. Other package includes require filesystem
loading through `project::load` or `project::prepare`; unsupported declarations
produce located diagnostics.

Use `Loaded::compile()` after project loading to retain package/file-mode
build-input bases. In-memory embedders can supply `build_inputs::BuildInputs` to
`project::compile_source_with_inputs`; the ordinary source API captures process CWD.

RuntimeContext owns managed resources and input buffering and borrows the output
writer. It is intended for one execution. Handles contain context identity, a slot,
and generation; they are not OS descriptors or pointers. Slot reuse does not revive
stale handles. Runtime quotas default to `None`, including `max_buffer_bytes` and
`max_resources`. When configured, the former caps buffer-related operations and
the latter caps live resource slots, including standard streams. These are VM
settings, not a language-level budget or a total VM memory quota.

Embedders can optionally set `max_buffer_bytes`, `max_resources`, `max_steps`,
`max_operand_values`, `max_call_frames`, combined local/global `max_slots`, and
`max_heap_objects` on `RuntimeLimits`; all default to `None`. The object limit
applies to live managed objects in the current thread's shared Cog heap; `Value`
handles retained by an embedder are roots.
The VM traces cycles at allocation checkpoints and execution exit, and releases
unreachable graph edges iteratively. Bulk array constants are checked against the
remaining object quota before construction, so one instruction cannot exceed that
configured count. The count is not a byte quota and does not
bound all host allocations or blocking calls. Exhaustion terminates execution and
finishes host resources; it does not run Cog defer blocks or interrupt blocking
native calls. In-memory programs with calls must supply `CompiledProgram::callables`;
execution rechecks call signatures and frame/branch boundaries even when decoding
is bypassed.

Rust code constructing graph values uses `cog::heap::HeapRef::new` for managed
storage, for example `Value::Array(HeapRef::new(ArrayValue::new(values)?))`.
Keeping a cloned `HeapRef` or `Value` keeps that object graph alive. Embedders
that create/drop cycles outside `run_with_context` can call
`cog::heap::collect_cycles()` to reclaim unreachable cycles on that thread.
Public mutable values use checked interior borrows at VM and native API boundaries.
If Rust code holds an active borrow while Cog accesses that value, the operation
returns a catchable `InvalidArgument` Error instead of panicking the host process;
release host borrows before calling back into Cog when the operation must succeed.

| Permission | Host operation |
| --- | --- |
| `allow_print` / `allow_read` | Stdout / stdin |
| `allow_fs_read` / `allow_fs_write` | `io::open`, `fs::metadata`/`read_dir`, and FS mutations; read-write open requires both |
| `allow_sleep` / `allow_time` | Sleeping / clocks |
| `allow_random` / `allow_exec` | Entropy / process execution |

Permissions are checked for each effect. The CLI uses the default permissive host
policy. `cog run --restricted` allows stdin/stdout and denies other runtime host
effects; restricted embedding can disable all host effects. Imports do not grant authority.
This is a host boundary, not a completed untrusted-package delegation model.

`run_with_context` calls `finish` on every VM exit, including terminal failure.
Finish closes resources, flushes eligible output, invalidates handles, and is
idempotent. A finish failure becomes primary after successful execution or secondary
when a primary failure is active. Drop releases resources without a hidden retry.
Embedding code making native calls directly must call finish itself.

## Assembly and diagnostics

`cog run file.asm --allow-asm` enables the diagnostic assembler. Its calls include
`call target argc`, `callv argc`, and symbolic `call_native io::write_all`.
Direct calls supply callable arities automatically. Indirect functions and closures
require `.function label argc captures` declarations before bytecode encoding.
`is_type error` consumes a value and produces bool; use `dup` to retain it. The
`record n` primitive consumes key/value pairs. Typed-array construction and runtime
type constraints have dedicated instructions. Printing, conversions, formatting,
time, entropy, and processes use native calls rather than dedicated opcodes.

Source diagnostics retain file/line/column origins through frontend linking and
recover independent errors. Saved v10 artifacts carry validated serialized source
maps. Public runtime APIs are listed in [standard library](STANDARD_LIBRARY.md);
cleanup and error boundaries are defined in [errors](ERRORS.md).
