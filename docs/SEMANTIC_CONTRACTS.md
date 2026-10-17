# Semantic contracts and future boundaries

This is the canonical distinction between enforced behavior, accepted future
requirements, and unresolved design questions. Implemented syntax is documented in
[language](LANGUAGE.md) and [errors](ERRORS.md). A proposed example below does not
imply compiler support.

Executable contracts are in [tests/semantic_contracts.rs](../tests/semantic_contracts.rs)
and [tests/build_input_contracts.rs](../tests/build_input_contracts.rs). Test-name
prefixes are stable contract IDs. There are 100 active and 7 pending tests in these
two suites; feature regressions also exist in separate suites. Pending tests
have `#[ignore = "spec: ..."]` reasons and are not passing coverage. A negative
semantic test must parse first: missing syntax is not a successful type/phase check.
Output observations pass through bytecode encode/decode and VM execution.

## Implemented invariants

| Contract IDs | Enforced behavior |
| --- | --- |
| try_01..07 | First unhandled protected Error stops lexical try; no dynamic handler inheritance; existing Error loads and arguments are observations; ordinary return observes locally; patterns inspect without clearing stored Error |
| cleanup_01..05 | Return snapshot before LIFO cleanup; normal cleanup failure becomes primary; active primary and secondary failures survive propagation; remaining defers run |
| poison_01..06 | Normal type and Error state are orthogonal; strict eager operations preserve the first cause; aggregate leaves do not recursively poison containers; typed consumers observe needed components |
| poison_07..09 | Lazy nil/Error fallbacks are distinct; a selected RHS Error uses the surrounding lexical handler |
| propagate_01..05 | !! evaluates once, passes normal/nil unchanged, bypasses local catches, restores caller operands, performs cleanup, and requires an enclosing runtime function; main reaches the host |
| nil_01..02, tail_01..08 | Singleton nil, unified tails/discard, every normal result exit checked, cleanup snapshots, no loop/defer result collection |
| binding_01..05, sequence_01 | Reachable mutating use for local var, available static inference, const access restrictions, transitive mutating calls, operator/method capability checks |
| flow_01 | Excluding Error does not remove nullable from the declared normal type |
| nullable_01/04/05/07/08 | Nullable boundaries remain declared; nonterminal arms/writes do not exclude nil; Error exclusion and !! do not unwrap nil; ?? can produce a nonnullable normal result |
| phase_01..07, phase_10..17, comptime_01..03 | Scalar values and nominal struct construction evaluate before runtime lowering; type-valued bindings substitute recursively into annotations and are erased; arithmetic Error values can be inspected by patterns or diagnosed at the comptime root; `try`/`catch`, `!!`, and lazy `??`/`?!` preserve computational error boundaries; runtime inputs/calls and forbidden runtime effects are rejected, and comptime callables are omitted from runtime artifacts |

Error is a computational state of an expression with normal type T; no mandatory
Result/union/throws annotation is introduced. Compiler recovery poison is a
separate invalid-source marker. Static violations cannot be caught. A computational
failure does not automatically stop every statement outside an observing boundary.

Conceptual facts are `normal_type + error_state + normal_control_flow`, independent
of mutability. Inferred may-error metadata is an accepted direction, not a complete
implemented fallibility system. A normal array/struct/enum can contain Error leaves.
Typed consumption must not mistake a poisoned component for its normal type.

Try restores the lexical operand-stack baseline and runs exited cleanup without
rolling back side effects. Explicit inspection handles its operand before outer
observation. Ordinary return still observes locally; !! is a distinct control-flow
exit. Its normal type is unchanged, and cleanup failures retain primary/secondary
information. Complete source provenance and a public suppressed-error API are pending.
Fatal cleanup precedence and diagnostic storage bounds remain unresolved.

Const forbids mutation through its access path; var does not imply dynamic typing.
Reference-like assignment shares without moves, and aliases cannot increase access
capability. A const alias observes an existing mutable alias. The mutability pass
tracks paths, returned aliases, containers, and inferred recursive/transitive effects.
Joins, unknown indices, repeated allocation sites, and paths deeper than four selectors
are conservative; acceptance of every safe program is not guaranteed. Resource handle
use mutates a host resource, not its immutable Cog token. Bytes/builders are mutable
Cog heap values. Const alone does not prove all reachable memory immutable for a JIT.

Poisoned writes/storage remain partly open: indexed writes currently propagate
Error without writing it, while constructors can store Error components. This does
not authorize silently changing all mutations. Streaming serialization may have
written a prefix; transactional output, wire Error representation, and cycle policies
are not selected by the aggregate contracts.

## Implemented: normal-type and Error flow facts

The checker keeps each binding's declared normal type separate from its
path-sensitive effective normal type and independent Error-exclusion fact.
Non-nil initialization does not narrow `T?`. A terminating nil arm narrows its
normal successor to T; a positive type pattern narrows inside its matched arm.
Reads use the effective type while writes remain checked against the declared
contract. No cast is needed after a valid proof.

The following is accepted and checked through successor refinement:

```text
fn required(value: int?) -> int {
    when value is nil { return 0; }
    value
}
```

| Contract ID | Behavior |
| --- | --- |
| nullable_02 | Returning nil arm narrows the normal successor |
| nullable_03 | Terminal crash in nil arm narrows the normal successor |
| nullable_06 | Nil exclusion preserves independent Error identity/payload/state |
| nullable_09 | Positive type arm narrows return/tail; later writes invalidate the fact |
| flow_02 | A terminating Error arm excludes Error on its normal successor |
| flow_03 | Calls invalidate mutable refinements when their effects are unknown |

A nonterminating arm joins with unmatched paths and does not exclude nil/Error.
Reassignment invalidates the binding's facts. Calls and method calls conservatively
invalidate facts for mutable bindings because effect summaries are not available.
Conditional/match joins retain only facts common to every normally reachable exit;
shadow bindings have separate identities. Excluding nil does not exclude Error,
and excluding Error does not exclude nil. Refinement is knowledge, not mutation
of a stored value or an additional observing boundary.

`flow_02` and `nullable_06` are active regressions. A checker unit test verifies
that normal-type and Error-exclusion facts join independently; the end-to-end
contracts also verify preserved Error identity/payload and that excluding Error
does not permit a nullable value at a nonnullable return.

## Implemented: typed arrays and tuples

- `T[]` is a homogeneous dynamic-length array. Nonempty literals infer their element
  type recursively, including empty nested arrays when a sibling makes the type
  unambiguous. An empty literal needs an annotation, a typed parameter/result, a
  compatible assignment, or another known type context; otherwise it is diagnosed.
  There is no implicit `any[]`.
- Element contracts are invariant and checked for construction, indexing, assignment,
  append/prepend, nested arrays, aliases, and explicitly typed function parameters,
  results, and fields. Unannotated parameters/results remain dynamic, while the
  shared array object retains its runtime element type. An incompatible mutation
  through dynamic code returns `TypeMismatch` without changing the array.
- `(int, str)` is a heterogeneous fixed-shape tuple. Tuple literals have two or more
  elements, numeric positions such as `.0`/`.1`, and fixed per-position types. Empty
  and single-element tuple forms and destructuring are unsupported. `()` remains an
  empty argument list; nil retains the unit-like role.
- Tuple assignment and array assignment share reference-like storage. Mutating one
  alias is visible through the others; `const` restricts mutation through its path.

Contracts collection_01..12, `collection_error_elements_use_the_error_nominal_type`,
tuple_01..03, and clone_01..02 are active regressions. `.clone()` creates independent
array and tuple containers and recursively clones supported primitive/nested
container elements. It preserves array and tuple type contracts. Custom nominal
`Clone` implementations work as generic values, but are not composed into automatic
cloning of arrays or tuples yet. Repeated nested references inside a cloned graph
remain aliases of the same independent copied object.

## Pending: fixed-size arrays and associated types

- `T[N]` waits for real comptime N values/expressions rather than a temporary size
  evaluator. Layout/materialization/size limits still need implementation.
- Associated types are not implemented. Generic extension methods use static
  receiver matching and monomorphization; generic trait implementations remain
  unsupported. No implicit dynamic type or runtime trait object is introduced.
- Clone does not transfer ownership, provide a borrow checker, or add lifetime
  syntax. Copy-on-write or reference counting may optimize observable independence.

Contracts clone_01..02 are active regressions.

## Implemented: comptime evaluation and functions

Global and local `comptime name[: T] = expression;` bindings are evaluated after
name resolution and before runtime lowering. Scalar literals, earlier comptime
bindings, arithmetic, comparisons, boolean operators, unary operators, string
concatenation, and nominal struct constructors are supported. Constructors can be
nested and use named/default arguments. Compile-time functions support scalar and
struct parameters, named/default arguments, local const/var/comptime bindings,
returns, `if`, and recursion within deterministic evaluator limits. Scalar results
are materialized as runtime literals; struct results are materialized through
their resolved constructors. Comptime-only function definitions are omitted from
runtime code. Top-level `comptime T: type = ...;` bindings can name primitive,
nominal, array, tuple, and nullable types used by later annotations. These compiler
type values are substituted recursively and removed before runtime lowering; they
cannot be stored as runtime values. Compile-time evaluator functions still do not
accept type parameters. Runtime functions may declare inferred compile-time type
parameters, which are specialized and erased before runtime lowering as described
below.

Runtime parameters/locals/globals cannot flow into comptime evaluation, and ordinary
functions remain runtime-only even when pure. Phase checks visit dead branches.
Native calls inside comptime functions are rejected. Recoverable division-by-zero
and integer-overflow results flow as Error values through scalar operations and
comptime calls. `when`/`match` patterns can inspect these errors inside a comptime
function; an unhandled Error at a `comptime` binding root is a compile diagnostic.
The evaluator cannot inspect struct fields. `try`/`catch` and `!!` work in
comptime functions: `!!` bypasses a catch in its own function and reaches the
caller's lexical handler, while using it at an initializer root is rejected.
Lazy `??` and `?!` select nil and Error fallbacks in compile-time expressions;
`!!` propagation bypasses either fallback. Mutation statements, loops, arrays,
tuples, and enum values remain pending as comptime data. Evaluator limits produce
compile diagnostics.
`comptime const` is invalid syntax.

## Pending: Error semantics, indirect callables, and richer compile-time data

The broader comptime foundation remains incomplete. Accepted boundaries:

- `comptime name[: T] = expression;` is a standalone local/global binding alongside
  const/var, never `comptime const`. Its computed result is not reassigned.
- Phase is Runtime or Comptime and is checked before dead-code elimination.
  Runtime information cannot flow into Comptime. Runtime-representable computed
  values may flow from Comptime to Runtime after materialization.
- Ordinary fn is runtime-only in v1, even when pure; comptime fn is compile-time-only.
  Direct calls enforce this boundary. Function values and indirect calls are not
  supported; a comptime-only function cannot be used as a runtime value or passed
  through an alias/container. Runtime artifacts have no CT-only entries.
- Ordinary local const/var inside a comptime function execute in its current phase
  without a modifier on every statement. Mutation statements remain unimplemented.
  Compiler-only TypeValue/reflection handles need not have a runtime representation;
  representability is checked at runtime use.
- Arithmetic Error values and explicit Error-pattern inspection work in comptime
  functions. `try`/`catch` handles computational errors, and `!!` bypasses a local
  handler and propagates to the caller's lexical handler. Lazy `??` and `?!` follow
  their runtime selection rules; `!!` propagation bypasses fallback selection.
  Constructed Error payloads and the complete cross-phase operational contract
  remain pending. Unhandled Error at a mandatory evaluation root is a compile error.
  Static phase/effect violations cannot be recovered by catch or hidden in dead code.
- !! requires a callable in the current execution context. A comptime initializer
  nested lexically in a runtime function does not provide a comptime callable;
  its root cannot return Error from that runtime function during compilation.
- Runtime filesystem, processes, network, clock/sleep, entropy/random, and environment
  effects are forbidden in comptime v1. Compiler-aware declared build inputs are
  a separate effect category, not general runtime I/O authority.

Cross-phase references to comptime-only functions are rejected before they can be
stored or passed through aliases, callbacks, arrays, or record fields. Cog v1 does
not support function-valued expressions or indirect calls. Closure-expression
syntax is not in the grammar, so closure phase/capture behavior has no source
contract yet. Compiler quota exhaustion must be distinguished from a
program's computational ResourceLimit; the evaluator's precise quota contract is
open.

Conceptually main is the first user Runtime function after compilation, evaluation,
specialization/generation, and materialization. **Current code executes global
initializers before main**, including function/native calls. A guarantee banning
user runtime initializers before main is not finalized. Ordinary const initialization
currently happens at runtime.

## Pending: comptime value parameters and canonical type identity

Runtime functions support inferred compile-time type parameters. A declaration such
as `fn identity[T: type](value: T)` remains a runtime function; `T` is
inferred from static argument types without evaluating runtime expressions and is
erased from the runtime ABI. Parameters must precede ordinary runtime parameters.
Ambiguous binding/inference diagnoses rather than choosing an arbitrary solution or
substituting dynamic typing. Compile-time value parameters are not implemented; a
runtime integer's current value does not infer a compile-time value parameter.

Canonical specialization identity must use compiler structural identity rather than
user equality, runtime addresses, hash iteration order, or RefCell identity. Cache
correctness includes semantic revisions, compiler/configuration, transitive evaluation,
type/layout, visibility/negative lookup, reflection, and generation dependencies.
A hash accelerates lookup; canonical values still resolve collisions. Evaluation
cycles and expanding specialization chains need diagnostics and resource bounds.
Reuse and deterministic emission are required; persistent fine-grained caches can
follow a conservative correctness boundary.

TypeValue uses semantic type identity, including package/module/nominal declaration
and generic struct specialization arguments, not display names or field shape.
Inference from result context, float value specialization
keys, CT availability of ordinary runtime const, and aggregate materialization remain
open.
Suggested internal data structures and cache encodings are implementation proposals,
not finalized public syntax or guarantees.

## Implemented: declared build inputs

The `build` namespace separates declared compilation inputs from runtime
filesystem effects. It requires `include std; use build;` and is comptime-only:

| API | Normal result | Missing file | Other computational failures |
| --- | --- | --- | --- |
| build::include_str(path) | str | Error | Read/UTF-8 Error |
| build::include_bytes(path) | bytes | Error | Read Error; no text decoding |
| build::try_include_str(path) | str? | nil | Read/UTF-8 Error |
| build::try_include_bytes(path) | bytes? | nil | Read Error; no text decoding |

Optional means absence only. An existing empty input is a value; permission denied,
a directory, or invalid UTF-8 does not become nil. Ordinary comptime Error handling
may recover computational failures; forbidden static effects are not catchable.

Paths must be comptime-known. The compiler must register dependency identity/content
hash matching the exact bytes supplied to evaluation. Optional absence is also a
dependency state: creation/deletion invalidates dependent results. Mtime/size alone
cannot establish reuse. Source/compiler/configuration and transitive specialization
inputs continue to participate; unchanged independent queries should become reusable
when incremental infrastructure exists. A changed required input failure must never
launch a stale successful artifact.

Typed comptime may parse/transform input. Runtime use checks representability and
materializes the resulting data into `.cogb`; evaluator heap serialization is not the
contract. The artifact contains neither build input handles nor runtime include
imports or synthesized filesystem calls. It runs after sources/inputs are removed,
without runtime filesystem permissions unless explicit independent runtime I/O exists.

Binary inputs become fresh runtime byte-buffer initializers backed by an embedded
compiler constant and a checked allocation intrinsic. No runtime file read is
synthesized. General aggregate freezing, cycles, and materialization remain future work.

Project/directory mode resolves input paths from the discovered package root, even
when invoked in a subdirectory. Explicit source-file mode captures the invocation
CWD. Dependency modules use their own package root. Absolute paths and escaping
parent/symlink traversals are static errors and cannot be caught. A pinned directory
capability contains file access. Inputs must be regular files; each read is limited
to 1,000,000 bytes, all reads to 16 MiB and 4096 dependency identities. Compiler
budget failures are diagnostics. Read and UTF-8 errors remain catchable comptime
Errors; only unhandled mandatory-root errors prevent compilation.

Normalized paths, content hashes and absence states are persisted under target in
cache dependency records, separate from package resolution in cog.lock. Runtime FS
retains process authority. KDL paths remain relative to their manifest; explicit
package inclusion paths remain relative to compiler CWD.

Contracts build_01..10 are active. Additional tests cover subdirectory/file modes,
dependency bases, absolute/parent/symlink escape, cache failure and runtime FS scope.

## Implemented: comptime type parameters and traits

Runtime functions declare inferred type parameters with `[T: type]`, for
example `fn first[T: type](values: T[]) -> T`. A type parameter may have
trait bounds after `+`, such as `[T: type + Eq[T] + Ord]`. Argument types
infer parameters recursively through arrays and tuples; incompatible repeated
inferences and uninferable parameters are diagnostics. The compiler creates
deterministic per-compilation specializations, checks each body with concrete
annotations, and rejects more than 256 instances. Explicit arguments use
`first[int](values)` or `Box[int](value)` and must supply the full parameter list.
Result-only inference and generic `comptime fn`s remain unsupported. Generic
extensions use `extend[T: type] Box[T] { ... }` or `extend[T: type] T[] { ... }`;
extension and method type
parameters are inferred from the receiver and arguments, then specialized with the
same per-compilation limit as generic functions.

Structs declare compile-time type parameters in the declaration header, for
example `struct Box[T: type] { value: T }`. Constructor arguments infer
the parameters using the same recursive type matching as generic functions.
Type annotations apply arguments with `Box[int]` and can nest, such as
`Box[Box[int]]`. The compiler specializes each concrete struct once, substitutes
its field types, and checks field reads, writes, constructor arguments, function
parameters, and trait bounds against the concrete types. Struct and function
specializations share the 256-instance compilation limit. Struct type parameters
cannot be inferred when no supplied constructor argument constrains them.

Traits declare type parameters as compile-time parameters, for example
`trait Convert[To: type]`. `extend Type with Trait[Arguments] { ... }`
binds concrete type values and substitutes them into method signatures. A
`comptime extend` may implement a trait for a compile-time-resolved target type.
Implementations are checked for duplicates, required methods, parameter names and
types, and result types. Function bounds can refer to other inferred type
parameters. Only one implementation of a trait is accepted per target type;
generic target implementations are not supported. `Self` names the implementing
type. Bounds are checked at specialization sites, and methods resolve statically
against the concrete receiver. There is no runtime trait object or dynamic dispatch.
The current cross-module visibility rules remain limited.

The `clone`, `eq`, and `ord` standard modules provide core traits. Structural clone
is intrinsic for arrays and tuples whose leaves are supported primitive values or
nested cloneable containers. Other types can implement `Clone` for generic use, but
their custom implementations are not automatically composed through arrays/tuples.

## Implemented: source package inclusion and extensions

The package entry point supports `include name;` for a source directory
and `include name from path;` for a directory relative to the compiler's
current working directory. The included directory contributes modules to the
compile-time source graph; their executable code remains runtime code. Existing
`dependencies.kdl` aliases remain available. `use module from package as alias;`
selects a provider explicitly. A plain module import resolves only when there is
one provider; duplicate providers produce an ambiguity diagnostic. `std` selects
the embedded standard library after `include std;`. Lock data uses the
existing `cog.lock` format.

`extend NamedType { ... }` declares inherent methods for a struct or enum.
`comptime extend TypeValue { ... }` resolves a compile-time type binding and can
register runtime methods for primitive, array, or tuple types. It may also implement
a trait for the resolved type. Ordinary `fn` methods are statically dispatched by
receiver type and lower as runtime functions. Compile-time type parameters and
trait bounds are described above.

Compile-time method calls and controlled generation from comptime functions remain
pending; `comptime extend` registers runtime methods and does not make those methods
callable during compile-time evaluation.

The metaprogramming direction is typed values/specialization, read-only reflection,
then controlled typed implementation generation with semantic identities, hygiene,
source/request origins, conflict checks, and ordinary checking/lowering. Generation
must not be a hidden phase escape. Specific reflection/generator capabilities and
trusted registration mechanisms are proposals awaiting implementation decisions.
Separate token macros, derive/decorators, arbitrary AST rewriting, and text emission
are not current prerequisites.

Closure capture modes, escaping lifetime/mutable invocation/equality, resource/cycle
handling, raw-string delimiters/extended escapes, collection iteration/mutation,
sequence chaining changes, and the existing when-var redesign remain open.
