# Standard library

## Declared build inputs

`include std; use build;` exposes comptime-only `build::include_str(path)` and
`build::include_bytes(path)`. The `try_include_str` and `try_include_bytes` variants
return nil only when the file is absent. Read/UTF-8 failures are catchable Error
values; unhandled Errors at mandatory comptime roots stop compilation.

Paths are relative to the project root in project mode, invocation CWD in explicit
file mode, or the dependency's root in dependency code. Absolute paths and escapes
through `..` or symlinks are compilation errors. This policy applies to compiler
inputs; runtime filesystem APIs retain normal process permissions. Embedded inputs
remain usable after their source files are removed. See the
[build-input contracts](SEMANTIC_CONTRACTS.md#implemented-declared-build-inputs).

The project entry point must include the standard package with
`include std;` before any source imports standard-library modules. Then
import modules explicitly: `use io;`, `use str;`, or `use math::PI;`.
The `clone`, `eq`, and `ord` modules define traits for generic code.
Selective and wildcard imports work for public members. The ordinary prelude
contains `len(value)` and `exclusive(value)`. Imports do not grant host permissions.
`exclusive` inspects runtime reference counts for supported shared values; it is
not an ownership or mutation-capability guarantee.
All functions can fail computationally; see [errors](ERRORS.md).
Runtime byte and resource quotas are optional embedder settings on `RuntimeLimits`;
`max_buffer_bytes` and `max_resources` default to `None`. Their documented limits
apply when an embedder explicitly sets `Some(n)`. This does not cap total process
memory or make OS allocation failures recoverable.

Source modules in `core/` and native functions share the resolver. The tables below
describe successful results, not additional source annotation syntax. Native calls
take positional arguments only. Source functions support named/default arguments.

## Text and output

| Call | Result / behavior |
| --- | --- |
| `io::print(value)` | Render, write to stdout, and flush; normal nil |
| `io::println(value)` | As print, followed by a newline; normal nil |
| `io::read_line()` | Read stdin, removing trailing CR/LF; EOF is IoReadFailed |
| `io::write_text(handle, text)` | UTF-8 encode and write all text to the selected stream; does not flush |
| `str::trim(text)` | Remove Unicode whitespace from both edges |
| `str::trim_start(text)` / `str::trim_end(text)` | Remove leading / trailing whitespace |
| `str::starts_with(text, prefix)` / `str::ends_with(text, suffix)` | Exact prefix / suffix test; an empty pattern matches |
| `str::contains(text, part)` | Exact substring test; an empty pattern matches |
| `str::replace(text, pattern, replacement)` | Replace all exact non-overlapping matches |
| `str::split(text, separator)` | New array of strings; separator must be nonempty |
| `str::join(parts, separator = "")` | Join an array of strings; empty array produces `""` |
| `str::to_int(value)` / `str::to_float(value)` | Numeric conversion, also available through `as` and receiver methods |
| `str::to_bool(value)` / `str::to_str(value)` | Boolean / string conversion |
| `str::compare(left, right)` | Lexicographic comparison; negative, zero, or positive int |
| `str::builder()` | A new shared mutable string builder |
| `fmt::write(builder, template, value)` | Append formatted text; return the same builder |
| `str::build(builder)` | Immutable string snapshot of a builder |

```cog
include std;
use io;
use str;
use fmt;

fn main() -> nil {
    var text = str::builder();
    fmt::write(text, "count={}; ", 3)!!;
    const snapshot = str::build(text)!!;
    fmt::write(text, "{}", "done")!!;
    const completed = str::build(text)!!;
    io::println(snapshot)!!;
    io::println(completed)!!;
}
```


`fmt::write` replaces the first `{}` in its template; later placeholders remain
literal. Without a placeholder, it appends the template without rendering an
unused value, but an Error value still propagates. This runtime formatting API is
separate from language interpolation. Failure preserves the builder's existing
prefix through all aliases, including self-formatting. Buffer limits count UTF-8
bytes; rendering has a traversal-depth limit of 64.

Conversions propagate Error. `bytes::render` and library printing deliberately
render errors for diagnostics. Float-to-int checks finite/i64 bounds; int-to-float
can lose precision. Numeric string parsing uses the current Rust numeric parser;
it is distinct from Cog's source literal grammar. Nonfinite float strings can be
parsed, but converting their result to int produces Overflow.

Trim is implemented in Cog and recognizes the 25 Unicode White_Space codepoints:
U+0009..U+000D, U+0020, U+0085, U+00A0, U+1680, U+2000..U+200A, U+2028, U+2029,
U+202F, U+205F, U+3000. Interior text, NUL, U+FEFF BOM, and U+200B ZERO WIDTH SPACE
are preserved. It copies input into a temporary byte buffer, so buffer limits apply
even when no trimming is needed. Runtime is linear in input size.

Search, replace, and split compare complete UTF-8 sequences without case folding or
Unicode normalization. Replace and split use non-overlapping matches from left to
right. Replace rejects an empty pattern with InvalidArgument and returns the original
text when there are no matches. Split preserves empty fields:
`str::split(",a,,", ",")` is `["", "a", "", ""]`.
An empty separator produces InvalidArgument. Join requires string elements and
propagates an Error element; it does not implicitly render other values.

These helpers copy their input strings into byte buffers; join uses a string builder.
Existing buffer limits apply to these allocations and the joined/replaced result.
Prefix and suffix tests are linear in the pattern size after copying. Contains,
replace, and split use a simple scan, with worst-case time proportional to text size
times pattern size.

```cog
include std;
use io;
use str;

fn main() -> nil {
    const fields = str::split("red,green,blue", ",")!!;
    io::println(str::join(fields, " / ")!!)!!;
}
```


A prompt printed to stdout is separate from the input returned by `io::read_line`. Use
`str::trim(io::read_line()) as int` to accept whitespace around a number.

## Arrays

| Call | Successful result / behavior |
| --- | --- |
| `array::is_empty(items)` | bool; inspect length only |
| `array::first(items)` / `array::last(items)` | First / last element, or nil for an empty array |
| `array::index_of(items, value)` | First matching zero-based index, or nil when absent |
| `array::contains(items, value)` | bool; stop at the first match |
| `array::position(items, value)` | Typed equality search; inferred element type `T` must implement `Eq[T]` |
| `items.swap(left, right)` | Mutates one array, or returns IndexOutOfBounds without changing it |
| `array::min(items)` / `array::max(items)` | Select the least / greatest element using `Ord`; empty input returns InvalidArgument |
| `array::slice(items, offset, count)` | Fresh outer array for `[offset, offset + count)` |
| `array::reversed(items)` | Fresh outer array in reverse order |
| `array::concat(left, right)` | Fresh outer array containing both inputs in order |

These functions accept arrays and leave their inputs unchanged. A nil element is
indistinguishable from an empty array through first/last alone; use is_empty when
that distinction matters. Slice accepts an empty range at the end; negative or
out-of-bounds ranges produce InvalidArgument rather than clamping.

`index_of` and `contains` use Cog's `==`: mixed int/float comparisons compare
the exact integer with the floating value without rounding the integer first,
different normal types compare unequal, and equality between arrays or records is
not implemented and can produce TypeMismatch. Search propagates an Error argument
or an Error encountered before a match. First/last return the selected element,
including Error. Slice,
reversed, and concat propagate Error as they consume elements; an unselected Error
does not affect slice, and is_empty inspects no elements.

Fresh outer arrays retain the element type of their inputs, including when the result
is empty. They retain shared references to nested arrays, records, and other shared
values; they do not perform a deep copy or grant writable access to elements obtained
through a read-only path. The existing conservative alias analysis applies.
`.empty_like()` returns a fresh empty array with the receiver's recursive element
type; it is used by the array utilities to preserve type information for empty results.
`position` instead requires a statically known element type that implements `Eq[T]`;
the built-in implementations cover int, float, bool, nil, and str. Programs can add
`Eq[T]` implementations for their own types. `min` and `max` propagate errors found
while reading the array and return the original selected element, preserving aliases
for shared values. Structural `.clone()` creates an
independent array container and recursively copies nested arrays and tuples when all
elements are supported primitive or nested container values. It does not share the
mutability of the source container or its nested containers, while repeated nested
references within the source remain shared within the copy. Clone recursion is limited
to 64 nested arrays/tuples; deeper values return ResourceLimit.

```cog
include std;
use io;
use array;

fn main() -> nil {
    const original = [1, 2, 3];
    var result = array::reversed(original)!!;
    result << 4;
    io::println(original)!!; // [1, 2, 3]
    io::println(result)!!;   // [3, 2, 1, 4]
}
```

## Collections

`collections` exposes one insertion-ordered hash table, `Map[K, V]`. Its
constructor is `map_new[K, V]()`. `Map` supports `int`, `str`, and `bool` keys;
custom key types implement `collections::Hashable`:

```cog
include std;
use collections;

struct UserId { value: int }

extend UserId with collections::Hashable {
    fn hash_code(self) -> int { self.value }
    fn equals(self, other: UserId) -> bool {
        self.value == other.value
    }
}

fn main() {
    var users = collections::map_new[UserId, str]();
    users.set(UserId(value: 7), "Ada");
}
```

Equal keys must return the same `hash_code`, and a key's hash and equality
behavior must remain stable while it is stored. Negative hash codes are valid.
Built-in keys use process-keyed hashes, so their numeric hash codes are not
stable across processes and must not be persisted.

| API | Behavior |
| --- | --- |
| `.len()`, `.is_empty()`, `.capacity()` | Number of live entries, emptiness, and bucket count |
| `.contains(key)` | Tests key presence, including when the stored value is nil |
| `.get(key)` | Returns the value or `Error::MissingKey` |
| `.get_or(key, default)` | Returns the value or the default after one lookup |
| `.set(key, value)` | Inserts or replaces; returns true only for a new key |
| `.remove(key)` | Removes and returns the value, or `Error::MissingKey` |
| `.clear()` | Removes all entries while retaining bucket capacity |
| `.entries()`, `.keys()`, `.values()` | Insertion-ordered snapshots |

For a nilable value type, `contains` reports presence independently of the value:
declare a comptime alias such as `comptime MaybeInt: type = int?;`, then use
`map_new[str, MaybeInt]()` and store `nil` under a key. `get` returns nil for that
present key and `Error::MissingKey` for an absent key.

`set`, `remove`, and `clear` declare `mut self`, so they require a mutable
`var` binding. Updating a key preserves its original position; removing a key
and inserting it again places it at the end. Removal unlinks a bucket entry and
uses tombstones in the ordered slot array. The array is compacted during resize
or after tombstones exceed half its slots, preserving the order of live entries.
Lookup and removal are expected O(1) with well-distributed hashes; collisions
can make a bucket chain linear. Removal is amortized O(1), including occasional
compaction.

`Map` implements `Clone` when its key and value types implement `Clone`. It
creates independent map storage, copies keys through `Clone`, clones each live
value, and preserves insertion order. Snapshot arrays have independent outer
storage; values otherwise follow normal Cog aliasing rules.

```cog
include std;
use collections;
use io;

fn main() -> nil {
    var scores = collections::map_new[str, int]();
    scores.set("Ada", 10);
    scores.set("Lin", 8);
    scores.set("Ada", 12);
    io::println(scores.get("Ada"))!!;
}
```

## JSON

The native `json` module provides `json::parse(text)` and
`json::stringify(value)`. Parsing maps JSON `null` to `nil`, booleans and strings
to their Cog counterparts, signed 64-bit integer tokens to `int`, and finite
fractional/exponent tokens to `float`. Integer tokens outside the `int` range
produce `Error::Overflow` instead of silently losing precision. The JSON token
`-0` is kept as floating negative zero to preserve its sign. JSON arrays
become mutable heterogeneous Cog arrays; objects become records with string
field names. Duplicate object names are rejected as `Error::ParseFailure`. Decimal
numbers are represented by IEEE 754 `float` values; integers outside `int` range
are rejected rather than rounded into floats.

Stringification supports `nil`, bool, int, finite float, string, arrays, and
records. Record fields are emitted in lexicographic key order, so equal records
produce deterministic text. Strings use JSON escaping and preserve Unicode.
Top-level Cog `Error` values propagate when passed directly; unsupported values
(including nested Errors, bytes, handles, functions, variants, and iterators) produce
`Error::TypeMismatch`. A non-finite float, cyclic array/record graph, or conflicting
host borrow produces a catchable `Error` instead of panicking.

Records serialize as JSON objects using their stored field names, and parsing
does not preserve a nominal Cog struct type. JSON text is a data representation,
not a lossless encoding of Cog type identity or array type constraints.

Both directions use the runtime `max_buffer_bytes` limit. Decoding also charges a
conservative allocation budget for the created Cog value graph and limits nested
values to 64 levels. Encoding is streamed through a bounded writer and uses the
same depth limit. These limits bound this API's input/output and temporary JSON
work; they are not a process-wide memory quota.

```cog
include std;
use io;
use json;

fn main() -> nil {
    const value = json::parse("{\"name\":\"Ada\",\"active\":true}")!!;
    io::println(json::stringify(value)!!)!!;
}
```


## Math, sorting, and random values

| Module | Public API |
| --- | --- |
| `math` | `abs`, `min`, `max`, `floor`, `ceil`, `round`, `pow`, `sqrt`, `ln`, `log2`, `log10` |
| `math` constants | `PI`, `E`, `TAU`, `SQRT_2`, `SQRT_3`, `LN_2`, `LN_10`, `LOG2_E`, `LOG10_E`, `DEG_TO_RAD`, `RAD_TO_DEG` |
| `sort` | `quick`, `stable`, `sorted`, `is_sorted`; `insertion`, `selection`, and `bubble` remain educational algorithms |
| `search` | `binary`, `lower_bound`, `upper_bound`; inferred comptime type `T` must implement `Ord` |
| `random` | `next()`, `range(lo, hi)`, `boolean()` |
| `entropy` | `bytes(count)`, bytes from OS entropy |

All sorting functions require elements implementing `Ord { compare(self, other: Self) -> int }`.
`quick`, `stable`, `insertion`, `selection`, and `bubble` mutate their input and
return `nil`; these in-place calls require a mutable binding. `quick` and
`selection` are unstable, while `stable`, `insertion`, and `bubble` preserve the
order of equal elements. `bubble` and `selection` are
quadratic teaching examples. `quick` uses median-of-three, three-way partitioning,
an insertion-sort cutoff, smaller-side recursion, and a heapsort fallback to bound
worst-case time and stack depth. `sorted(items)` returns an independently cloned,
sorted array and therefore requires both `Ord` and `Clone`; it leaves the input
unchanged. `is_sorted(items)` checks ascending order without mutation.
`quick` and `stable` use Rust's native sort for int, float, bool, and str arrays;
other `Ord` implementations use the Cog algorithms.

`Ord.compare` uses a negative int for less-than, zero for equality, and a positive
int for greater-than. It may return an Error; sorting, searching, `min`, and `max`
propagate that Error and do not lose elements. A failure can leave an in-place
array partially reordered, but it remains a permutation of the original array.
Floating-point comparisons involving NaN return `Error::Unordered`; NaN is not
assigned an arbitrary place in the ordering.

The `search` functions require an array sorted in ascending order according to
`Ord`. `binary` returns an index of a matching element or `-1`; when matches
are duplicated, it may return any matching index. `lower_bound` returns the first
index whose element is greater than or equal to the target, and `upper_bound`
returns the first index whose element is greater than the target. Both bounds can
equal the array length when no element meets the condition. Empty arrays return
`-1` from `binary` and `0` from both bound functions. These functions do not mutate
the input.

`Ord` is implemented for int, float, bool, and str. String comparison is lexicographic
by UTF-8 bytes; valid UTF-8 preserves Unicode scalar ordering. User types can
implement `Ord` to use sorting and selection functions. See the [language
reference](LANGUAGE.md) for compile-time type parameters and trait syntax.
`clone::Clone`, `eq::Eq[T]`, and `ord::Ord` are traits written in Cog; their
implementations are checked statically.

Math accepts int/float arguments; other normal values produce TypeMismatch. Existing
Error values propagate unchanged. Primitive sorting uses Rust's standard slice
sorts; custom `Ord` implementations use the Cog fallback algorithms.

| Call | Result and limits |
| --- | --- |
| `math::abs(x)`, `math::min(a, b)`, `math::max(a, b)` | Numeric result; min/max retain the selected argument's type |
| `math::floor(x)`, `math::ceil(x)` | int rounded toward negative/positive infinity |
| `math::round(x)` | Nearest int; ties go toward positive infinity, so round(-1.5) is -1 |
| `math::pow(base, exponent)` | Checked integer or floating power; exponent must be a nonnegative int |
| `math::sqrt(x)` | float square root; negative arguments produce InvalidArgument |
| `math::ln(x)`, `math::log2(x)`, `math::log10(x)` | float logarithm; zero/negative arguments produce InvalidArgument |

Integer arithmetic overflow and nonfinite/out-of-range integer rounding produce
Overflow. Integer rounding preserves int inputs without converting them to float.
`pow` uses exponentiation by squaring; a negative exponent produces InvalidArgument,
and a non-int exponent produces TypeMismatch. Its result has the base's numeric type,
including exponent zero; `pow(0, 0)` is 1. Square root and logarithms preserve NaN and
positive infinity. Their finite calculations cover very small and very large inputs,
including subnormal floats. Logarithm domain errors identify the `math::ln` operation. Logarithms are
approximations; even exact powers can differ from an integer by floating-point
rounding. `abs` normalizes floating negative zero to positive zero.

Random uses a 31-bit pseudorandom generator seeded once from OS entropy. `next()`
returns an int in `[0, 2147483648)`; `boolean()` returns bool. `range(lo, hi)` takes
int bounds and samples `[lo, hi)`, rejecting the incomplete remainder interval to
avoid modulo bias. The range must be nonempty and its width at most `2147483648`;
otherwise it produces InvalidArgument. A width that overflows int produces Overflow.
Bounds are checked before requesting entropy. Entropy failures remain recoverable
Error. This generator is not cryptographic. Seed/partition/recursive helpers are private.

## Byte buffers

| Call | Successful result |
| --- | --- |
| `bytes::new(size)` | Zero-filled, fixed-length mutable buffer |
| `bytes::copy(buffer)` | Independent copy of the full buffer |
| `bytes::slice(buffer, offset, count)` | Independent copy of a checked byte range |
| `bytes::concat(left, right)` | New buffer containing both inputs |
| `bytes::from_str(text)` | New buffer containing UTF-8 bytes |
| `bytes::render(value)` | New buffer containing diagnostic display text |
| `bytes::len(buffer)` | Byte length |
| `bytes::get(buffer, index)` | Integer in 0..255 |
| `bytes::set(buffer, index, value)` | Checked mutation; nil |
| `bytes::to_str(buffer, offset, count)` | Strict UTF-8 decoding of a byte range |

Buffers share storage through assignment. `bytes::set` requires writable access.
Copy, slice, and concat allocate independent storage and obey the runtime buffer
limit; concat accepts aliases, including the same buffer for both arguments.
Invalid byte values, negative/out-of-bounds ranges, and invalid UTF-8 produce Error.
`len(str)` counts Unicode scalars; use `bytes::len(bytes::from_str(text))` for bytes.

## ASCII and hexadecimal text

| Call | Successful result / behavior |
| --- | --- |
| `ascii::is_ascii(value)` | Whether a byte is in 0..127 |
| `ascii::is_alpha(value)` / `ascii::is_digit(value)` | ASCII letter / decimal digit test |
| `ascii::is_alphanumeric(value)` | ASCII letter or decimal digit test |
| `ascii::is_lowercase(value)` / `ascii::is_uppercase(value)` | ASCII lowercase / uppercase letter test |
| `ascii::is_whitespace(value)` | Space or a byte in 9..13, including vertical tab |
| `ascii::to_lowercase(value)` / `ascii::to_uppercase(value)` | Convert ASCII letters; leave other bytes unchanged |
| `hex::encode(data, uppercase = false)` | Encode a byte buffer as hexadecimal str, two digits per byte |
| `hex::decode(text)` | New writable byte buffer decoded from hexadecimal str |
| `base64::encode(data)` | Encode arbitrary bytes as padded Base64 str |
| `base64::decode(text)` | Decode strict padded Base64 into a new writable byte buffer |

ASCII functions take int byte values in 0..255; values outside that range produce
InvalidArgument. Bytes 128..255 return false from all classifiers and are unchanged
by case conversion. These functions do not perform Unicode character conversion.

Hex encoding uses lowercase by default; decoding accepts either case. Empty input
is valid in both directions. Odd digit counts, non-hexadecimal characters, prefixes
such as `0x`, and whitespace produce ParseFailure with `from: "hex", to: "bytes"`.
Decoded bytes need not be valid UTF-8; use bytes::to_str explicitly when text is
expected. Encoding and decoding are linear in input size. Existing buffer limits
apply to copied input and output buffers, including the doubled size when encoding.

Base64 uses the standard alphabet (A-Z, a-z, 0-9, +, /) and = padding. The empty
string is valid. Decoding requires a length divisible by four, and rejects
whitespace, URL-safe substitutions, misplaced padding, and nonzero unused bits in
the final padded sextet. Failures produce ParseFailure with from: "base64" and
to: "bytes". Decoding returns arbitrary bytes; valid UTF-8 is not implied.
Encoding and decoding are linear in input size. Decoding copies its text into a
byte buffer; both that copy and the resulting buffer use the existing byte limit.
Encoding allocates output text using the same limit.

```cog
include std;
use io;
use bytes;
use hex;

fn main() -> nil {
    const encoded = hex::encode(bytes::from_str("Cog")!!)!!;
    const decoded = hex::decode(encoded)!!;
    io::println(encoded)!!; // 436f67
    io::println(bytes::to_str(decoded, 0, bytes::len(decoded)!!)!!)!!;
}
```


## Streams and files

| Call | Successful result / behavior |
| --- | --- |
| `io::stdout()` / `io::stdin()` | Managed standard stream handles |
| `io::open(path, mode, buffer_size)` | File handle; modes `r`, `r+`, `w`, `w+`, `a`, `a+` |
| `io::read_into(handle, buffer, offset, count)` | Number of bytes read; 0 at EOF or for an empty range |
| `io::write(handle, buffer, offset, count)` | Number of bytes accepted; partial writes and 0 are allowed |
| `io::write_all(handle, buffer, offset, count)` | Retry partial/interrupted writes; nil, or Error on WriteZero |
| `io::seek(handle, offset, origin)` | New absolute position; origin is `"start"`, `"current"`, or `"end"` |
| `io::flush(handle)` / `io::close(handle)` | nil |

Stream transfer calls take the handle first, then the byte buffer, offset, and count.
Ranges are half-open `[offset, offset + count)`, including length/zero. Bounds and
permissions are checked before I/O. `read_into` mutates its second argument; immutable
handle tokens can be used without making their bindings var. Closing a handle
invalidates all aliases, even if flushing fails. Stale/double-close/foreign-context
handles produce InvalidResource; closed standard streams are not reopened.

Zero buffer size disables file output buffering. A positive size enables buffering
for output, not read-ahead. Read/seek on a read-write file first flushes pending
output. Flush is not fsync or a durability guarantee. `read_line` and `read_into` on
stdin share buffered input; a failed read can have consumed a prefix already.

Use explicit/deferred close for deterministic lifetime. The VM also finalizes
managed resources at the end of execution. An automatically finalized stream does
not silently retry a previously reported write/flush failure; explicit operations
may retry. See [runtime](RUNTIME.md) for embedding limits and permissions.

`io::open` interprets relative paths from the process working directory, with the
same operating-system path rules used by Rust. Runtime file APIs are not rooted at
the package directory. Compile-time `build::include_*` paths use their separately
documented project/dependency bases.

## Filesystem operations

| Call | Result / behavior |
| --- | --- |
| `fs::metadata(path)` | Record with `is_file`, `is_dir`, `is_readonly`, and OS-reported `len` fields |
| `fs::read_dir(path)` | Array of immediate entry names, sorted lexically as UTF-8 strings |
| `fs::create_dir(path)` / `fs::create_dir_all(path)` | Create a directory, or the directory tree |
| `fs::remove_file(path)` | Remove one file entry using host OS semantics |
| `fs::remove_dir(path)` | Remove an empty directory |

Paths are interpreted from the process working directory. Metadata follows
symbolic links; `is_readonly` reports the OS readonly attribute and is not a
portable access check. Directory listing fails with `ParseFailure` if an entry
name cannot be represented as UTF-8, and does not return a partial result after an
OS or resource error. Entries are sorted by UTF-8 byte order for deterministic
results. The runtime buffer limit accounts for encoded entry-name bytes plus a
fixed allowance per Cog value/string reference; it does not bound all allocator or
process overhead. Filesystem failures produce `IoFailure` with the operation name and OS
message. `allow_fs_read` gates metadata/listing, and `allow_fs_write` gates
directory creation and removal. The CLI's ordinary runtime policy grants both;
the restricted policy denies both.

## Paths

| Call | Result / behavior |
| --- | --- |
| `path::join(base, child)` | Join using the host platform's path rules |
| `path::is_absolute(path)` | Whether the path is absolute on the host platform |
| `path::parent(path)` | Parent path as str, or nil when there is no parent |
| `path::file_name(path)` | Final normal component as str, or nil when absent |
| `path::extension(path)` | Extension without the leading dot, or nil when absent |
| `path::with_extension(path, extension)` | Return a path with its extension replaced |

These are lexical path operations: they do not access the filesystem, resolve
symbolic links, normalize `.`/`..`, or guarantee that a path exists. Their separator,
root, prefix, and extension behavior follows the host OS. `join` and
`with_extension` return Error if the resulting path cannot be represented as Cog
UTF-8 text, and return ResourceLimit when the result exceeds the runtime buffer
limit. File opening still uses the process working directory and OS permissions.
`with_extension` returns `InvalidArgument` when the extension contains a path
separator according to the host OS.

## Time and processes

| Call | Successful result / behavior |
| --- | --- |
| `time::sleep(seconds)` | nil; int/float duration with subsecond precision |
| `time::now()` | Unix epoch seconds as int |
| `time::now_nanos()` | Unix epoch nanoseconds as int |
| `time::now_millis()` | Unix epoch milliseconds as int |
| `time::monotonic_nanos()` | Monotonic nanoseconds with an unspecified origin; compare differences only |
| `process::args()` | `str[]` containing program arguments, excluding the Cog executable |
| `process::env(name)` | Environment value as str, or nil when the name is absent |
| `process::run(program, arguments)` | Captured process result; argument array of strings, no shell expansion |
| `process::shell(command)` | Explicit `/bin/sh -c` on Unix; unsupported on other targets |

Wall-clock results are not monotonic; use `monotonic_nanos` for elapsed-time
measurements. Negative sleep yields NegativeSleep; nonfinite or unrepresentable
durations and pre-epoch clocks fail, and i64 overflow is reported. On Windows,
durations that round to the OS `INFINITE` wait sentinel are rejected instead of
blocking forever. OS scheduling does not guarantee precise wakeups.

`process::args()` and `process::env(name)` read the current process state. Cog
arguments and environment values must be valid UTF-8; otherwise the call returns
`Error::ParseFailure`. The argument array and returned environment value obey the
runtime buffer limit. An empty environment name, or one containing `=` or NUL,
returns `Error::InvalidArgument`. A missing environment variable returns nil.
Both calls require the `allow_process_info` embedding capability; the CLI's
`--restricted` mode denies them. The ordinary CLI inherits the environment of
the process that launched it.

Process results contain `exit_code` (int, or nil after signal termination), `success`
(bool), `stdout`, and `stderr` (byte buffers). A nonzero exit is a normal result;
spawn/capture/wait failure is `Error::IoFailure(op, message)` with the OS cause.
Output exceeding the configured cap produces `Error::ResourceLimit`. Shell
script text is not included in host-generated launch errors. Decode output
explicitly with `bytes::to_str`.

Run is synchronous, with null stdin and inherited cwd/environment. The argument
array is validated before spawn. Output streams are captured concurrently, with
a separate buffer limit for each. On capture failure/limit, the direct child is
killed and reaped without joining the other reader. A descendant retaining that
pipe can keep the detached reader thread and pipe open until the descendant exits.
On successful capture, descendants retaining pipes may delay EOF indefinitely.
Timeouts, process-tree termination, streaming capture, and launch options are not
implemented.

## Error names

`err::Error` provides TypeMismatch, DivisionByZero, Overflow, NegativeSleep,
ParseFailure, IndexOutOfBounds, MissingField, MissingKey, Unordered, PermissionDenied, ProcessFailed,
IoReadFailed, IoWriteFailed, EntropyUnavailable, InvalidRangeStep, InvalidResource,
InvalidArgument, ResourceLimit, and IoFailure. Their payload declarations are in
[core/err.cog](../core/err.cog). `err::kind(value)` and `value.error_kind()` return
the primary error kind as str, or `"ok"` for a normal value. `.unwrap()`
and `.expect(message)` crash on Error. Prefer patterns and explicit recovery or
propagation when failure is recoverable.
