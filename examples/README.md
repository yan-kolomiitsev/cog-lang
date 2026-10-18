# Cog examples

Run these commands from the repository root after installing Cog. Alternatively,
replace `cog` with `cargo run --locked --offline --bin cog --`.

```sh
cog run examples/hello.cog
cog run examples/command_line_args.cog -- first "two words"
cog run examples/functions.cog
cog run examples/comptime_factorial.cog
cog run examples/comptime_types.cog
cog run examples/comptime_structs.cog
cog run examples/comptime_errors.cog
cog run examples/extensions.cog
cog run examples/generics_traits.cog
cog run examples/package_include
cog run examples/number_detective.cog
cog run examples/todo.cog
```

The todo example reads and writes `txt/todo.txt` relative to the process
working directory. Run it from the repository root. Each line stores one note
with a `[ ]` or `[x]` completion marker. The example data file is included in
the repository and starts empty.

## Learning path

| Example | What it demonstrates |
| --- | --- |
| [hello.cog](hello.cog) | Imports, main, output, and explicit I/O propagation |
| [command_line_args.cog](command_line_args.cog) | `main(args: str[])` and forwarding arguments after `--` |
| [bindings.cog](bindings.cog) | const/var, normal types, reassignment, and interpolation |
| [functions.cog](functions.cog) | Tails, defaults, named arguments, structs, enum constructors, and trailing commas |
| [comptime_factorial.cog](comptime_factorial.cog) | Compile-time functions, recursion, and a materialized scalar constant |
| [comptime_types.cog](comptime_types.cog) | Type values passed through a compile-time function and substituted into nested annotations |
| [comptime_structs.cog](comptime_structs.cog) | Compile-time struct constructors, nested results, and field defaults |
| [comptime_errors.cog](comptime_errors.cog) | Compile-time nil/Error fallbacks and propagation across function boundaries |
| [extensions.cog](extensions.cog) | Type aliases, inherent methods, and compile-time registered extensions |
| [generics_traits.cog](generics_traits.cog) | Comptime type parameters, trait implementations, and generic standard-library operations |
| [package_include](package_include/) | Compile-time package inclusion and explicit package selection for duplicate module names |
| [control_flow.cog](control_flow.cog) | Ranges, while, break/continue, guards, when, and match |
| [strings.cog](strings.cog) | Literal braces, full interpolation, nesting, and Error handling |
| [tails.cog](tails.cog) | Normal block results, early return, and cleanup snapshots |
| [structs.cog](structs.cog) | Typed/defaulted fields, nullable fields, and reordered constructor arguments |
| [collections.cog](collections.cog) | Shared arrays, read-only aliases, mutation, and sorting |
| [fallbacks.cog](fallbacks.cog) | Distinct lazy nil/Error fallbacks and evaluation counts |
| [propagation.cog](propagation.cog) | !! bypassing local catch, caller handling, and primary/secondary cleanup failures |
| [formatting.cog](formatting.cog) | Shared builders, runtime templates, and immutable snapshots |
| [mathematics.cog](mathematics.cog) | Powers, square roots, rounding, and recoverable domain errors |
| [text_processing.cog](text_processing.cog) | Split, trim, replacement, join, array search, and fresh reversed arrays |
| [byte_encoding.cog](byte_encoding.cog) | ASCII byte conversion, hexadecimal encoding/decoding, and invalid input |
| [base64_encoding.cog](base64_encoding.cog) | Padded Base64 encoding/decoding, UTF-8 conversion, and malformed input |
| [streams.cog](streams.cog) | Byte buffers, managed stdout, write_all, and flush |

These examples complete without input. Host output can still fail; the examples
use explicit propagation or lexical handling where appropriate.

## Interactive programs

| Example | Input and behavior |
| --- | --- |
| [read_number.cog](read_number.cog) | Enter an integer with optional surrounding whitespace; conversion/I/O errors propagate |
| [pyramid.cog](pyramid.cog) | Enter a positive height; build and print a centered star pyramid |
| [number_detective.cog](number_detective.cog) | Guess a secret number in 1..100; commands: stats and quit |
| [todo.cog](todo.cog) | Add, complete, remove, list, and count notes; load and save them as plain text in `todo.txt` |

Number detective accepts an empty name/attempt limit as Player/6. Invalid guesses
do not consume attempts. It demonstrates shared history, patterns/guards, named
arguments, nullable defaults, fallbacks, lexical handlers, propagation, and cleanup.
The statistics helper takes its array through an unannotated parameter, so it casts
element reads at that dynamic boundary. The command parser is also dynamic, so its
match keeps a wildcard arm.

## Projects and host processes

- [Modules](modules/README.md): two source modules and public visibility.
- [Multiple targets](targets/README.md): select separate client/server entry points.
- [processes.cog](processes.cog): run the host `echo` command and decode captured bytes.
  It requires `echo` on PATH; it uses no shell. The full shell API is Unix-specific.

```sh
cog run examples/modules
cog run examples/targets --target server
cog run examples/processes.cog
```

Local modules take precedence over standard modules with the same name. This is
why the process example is named `processes.cog`. See [projects](../docs/PROJECT_FORMATS.md).

## Validation

The [build-input project](build_inputs/main.cog) embeds text and binary data from
its package root. Run `cog run examples/build_inputs`; its bytecode continues to
work after the input file is removed.

```sh
cargo test --locked --offline --test examples --test documentation
```

Tests compile the programs, round-trip their bytecode, execute deterministic and
interactive cases, and check both project examples. Process checks are platform-aware
in the project/runtime suites. See the [language reference](../docs/LANGUAGE.md)
and [error contracts](../docs/ERRORS.md) for the implemented semantics.
