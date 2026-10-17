# Projects and configuration

Cog uses KDL 2 for user configuration and a separate generated `cog.lock` format.
Current dependency support is local/path only. A standalone program needs no
configuration; create configuration files only when their settings are needed.

## Start a project

```sh
cog init my-app
cog check my-app
cog run my-app
cog build my-app
```

Init requires an explicit directory and creates `src/main.cog` plus commented
`package.kdl`, `dependencies.kdl`, and `build.kdl` templates. The generated `app`
target works immediately: `cog run my-app --target app`. Dependency examples stay
commented until you create their source packages.
Use `cog init .` to initialize the current directory. The package name is derived
from the selected directory. Once created, the manifest name
survives directory renames. Package names must be nonempty and cannot be `.`/`..`
or contain path separators/control characters.

| File | Purpose |
| --- | --- |
| `package.kdl` | Package name and version |
| `dependencies.kdl` | Explicit local dependency aliases and paths |
| `build.kdl` | Optional named targets with nonstandard entry points |
| `cog.lock` | Generated dependency resolution; keep it with the project |
| `target/` | Disposable artifacts and build cache |

```kdl
// package.kdl
package "my-app" version="0.1.0"
```

## KDL syntax and supported settings

A declaration has a node name, positional string arguments, and `key="value"`
properties. In the example above, `package` is the node, `"my-app"` is its name
argument, and `version` is a property of that same node. A newline ends a node.
`//` introduces a comment. These follow [KDL 2 syntax](https://kdl.dev/spec/).

Cog supports the following complete configuration schema:

| File | Declaration | Effect |
| --- | --- | --- |
| `package.kdl` | `package "name" version="value"` | Package identity and default artifact name; version recorded in dependency resolution and lockfiles |
| `dependencies.kdl` | `alias path="directory"` | Connects a local source package for imports; paths are relative to this file |
| `build.kdl` | `target "name" entry="source.cog"` | Selects a named executable with `--target name` |

All values are strings. These schemas use flat declarations; child blocks,
additional properties and duplicate declarations are rejected with diagnostics.
Version is metadata, without semver solving or version constraints. Compiler
optimization profiles, runtime permissions, registry dependencies and arbitrary
build scripts are not currently KDL settings. Configuration is read and checked
during source project preparation, including cache hits; saved bytecode runs
without it.

```kdl
// dependencies.kdl
geometry path="./packages/geometry"
```

The node name is the dependency alias in the consuming package. Its path is
relative to the directory containing `dependencies.kdl`; the dependency needs its
own `package.kdl`. Aliases can differ from actual package names. Transitive
packages declare their own dependencies; consumers do not repeat them.

## Compile-time source packages

The package entry point can include a source directory during compilation. Its
`.cog` files become modules in that package; executable module code still runs in
the resulting program. Inclusion declarations in other modules are rejected.
An include declares a package connection; its package name does not need a prior
`use`. It does not import module names into source scope: that is the role of `use`.

```cog
include game;
use random from game as game_random;
```

Without an explicit path, `game` resolves to `src/game/` when it exists, otherwise
to `game/` at the package root. A directory can use the regular package manifests
or can simply contain source modules. The standard library is also an explicit
source package: add `include std;` to the project entry point before
importing its modules. This makes the `std` alias available for package-qualified
imports and includes standard modules in ambiguity checks alongside other packages:

```cog
include game;
include std;

use random from game as game_random;
use random from std as std_random;
```

Existing `dependencies.kdl` dependencies and `cog.lock` remain supported.

`use module from package as alias;` selects one package explicitly. A plain
`use module;` works when the module has a single visible provider; if multiple
included packages provide that namespace, compilation reports an ambiguity and
requires `from`. A package inclusion can also name a directory explicitly:

```cog
include game from ../../game;
use random from game as rand;
```

An explicit include path is relative to the compiler's current working directory.
It can be written as a quoted path or as a relative path beginning with `./` or
`../`. Resolved package paths are represented by the existing `cog.lock` format.

The version is nonempty metadata, not a semver solver. Registry/git sources and
version constraints are not supported yet. KDL syntax parsing is separate from
strict schema validation. Unknown nodes/properties, wrong types, duplicate
singleton nodes/properties, and conflicting aliases produce located diagnostics;
independent schema errors accumulate. Cog rejects duplicate properties even though
KDL itself permits last-value semantics. There is no alternate-format fallback.

## Modules and imports

Sources are discovered under `src/` when it exists, otherwise under the package
root. Root `lib.cog` is the package API; `a/b.cog` defines `a::b`. Directories form
namespaces; nested `lib.cog` is an ordinary module named `lib`.

```text
package.kdl
src/
  main.cog
  greeting.cog
  utils/
    math.cog
```

A public function in `greeting.cog` can be imported with `use greeting;` and called
as `greeting::greet(...)`. See the runnable [modules project](../examples/modules/README.md).

Supported imports include `use a::b;`, `use a::b::item as alias;`,
`use a::b::{x, y as z};`, and `use a::b::*;`, globally or inside blocks.
`self`/`super` refer to the current/parent module. From main, use `super` or
`super::item` to reach the root `lib.cog` explicitly.

Explicit `self`/`super` and dependency aliases select a module or package directly.
Unqualified paths collect visible providers from the current package, its
dependencies, and the included standard library. Multiple providers are ambiguous,
including for nested paths. A dependency alias cannot
collide with a local/standard module or a reserved name. Declaring a dependency
does not replace `use`; imports are required for standard modules too.

Declarations are private unless marked `public`. Private helpers can call each
other directly or through their module name within that module, including
recursion. Defaults/globals resolve in the declaration environment. Wildcards
import only public declarations. Package/module identity distinguishes equal
names in different files/packages. Import cycles are allowed; package dependency
cycles and statically detected global initialization cycles are rejected.

Global initializers currently execute before main and can call functions/native
operations. They are ordered by references to globals, including references through
functions/defaults; independent initializations preserve declaration order. Cycle
analysis is conservative and may reject dependencies through an unexecuted branch.

Nested directories containing `package.kdl` are separate package boundaries and
need dependency aliases. Symlinks, hidden directories, `target/`, `tests/`, and
`examples/` are excluded from ordinary discovery. Module filenames/directories
must be Cog identifiers; `a.cog` together with `a/` is ambiguous. Package paths
must be UTF-8. Only entry/import-reached modules are parsed/checked/emitted; all
of their declarations are retained, without per-function stripping.

## Targets and artifacts

Default entry selection tries `src/main.cog`, then `main.cog`, then the single
configured target. Multiple targets without a conventional main require selection.

```kdl
// build.kdl
target "client" entry="src/client.cog"
target "server" entry="src/server.cog"
```

```sh
cog build . --target server
cog run . --target server
```

The artifact is `target/server.cogb` for an explicitly selected target, otherwise
`target/<package-name>.cogb` (directory name without a package manifest). A single
nonstandard target selected implicitly still uses the package artifact name.
Entry paths must be discovered sources inside the package source tree; absolute
paths and `..` are forbidden in `entry`. See [multiple targets](../examples/targets/README.md).

One artifact includes the entry and linked modules/dependencies; there are no
separate library `.cogb` artifacts. A source passed explicitly produces a `.cogb`
beside it. `cog build ... -o output.cogb` overrides the destination. An explicit
`cog run file.cogb` uses that file without source freshness checks.

## Dependency lockfile

Version 1 is a small UTF-8, line-oriented format:

```text
COGLOCK 1
root 0
pkg 0 "my-app" "0.1.0" path "."
pkg 1 "geometry" "0.2.0" path "packages/geometry"
dep 0 "geometry" 1
```

The header is exactly `COGLOCK 1`. A single root references an existing package.
A pkg record contains ID, quoted name, quoted version, `path`, and quoted path;
a dep record contains owner ID, quoted alias, and target ID. IDs are decimal u32
without leading zeros except `0`. Identity includes name, version, and source;
matching names from different sources are not merged.

Paths are normalized relative to the lockfile, using `/` and removing redundant
`./`. Relative `..` is allowed for explicitly declared external local dependencies;
absolute host paths are not written. Fields are space/tab separated; blank lines
are allowed, records cannot span lines, and strings are always double-quoted.
String escapes are `\"`, `\\`, `\n`, `\r`, `\t`; unknown escapes, unescaped control
characters, and unterminated strings are invalid. There are no comments or
executable expressions. The reader accepts LF/CRLF; the writer uses LF and a final newline.

Canonical writing assigns root ID 0, sorts remaining packages by name/version/source
kind/normalized source in UTF-8 order, and writes pkg records before dep records.
Dependencies sort by owner/alias, with one ASCII space between fields. Only reachable
packages are emitted; identical resolution gives identical bytes, without timestamps.
The reader validates versions, fields, duplicate IDs/identities/aliases, and references;
the resolver separately checks the package graph and agreement with manifests.

Build/run validate resolution even on a cache hit. Without `--locked`, a missing or
stale valid lock is prepared for update and written after artifact preparation,
before execution. Runtime failure does not undo it. `--locked` rejects missing/stale
locks. Corrupt locks are diagnosed and preserved regardless of that flag. Check
writes no lock. No dependency graph requires no lock, but an existing lock is still
validated when dependencies are removed.

Mutable path sources remain live directories, not frozen snapshots. Source changes
invalidate builds even with `--locked`. Reproducibility requires the same available
source contents. Immutable sources and revision/content hashes need future work.

## Build cache and CLI output

Build/run share a whole-program cache. BLAKE3 fingerprints cover compiler binary,
configuration/resolution, entry and package/module identities, discovered catalog
and source contents (including unused modules and embedded core), and artifact bytes.
The source snapshot passed to compilation matches the hashed bytes; mtime/size alone
are insufficient. Preparation parses the entry point to resolve package inclusion
before checking the cache. Cache hits skip further module parsing, semantic checking,
and emission, while still validating configuration, resolution, artifact hash, and
bytecode.

Cache records live in `<package-root>/target/.cog-cache/`, keyed by output path.
The current internal 74-byte record contains `COGCACHE`, little-endian u16 version 2,
32-byte input hash, and 32-byte artifact hash. A companion `.deps` KDL record stores
build-input bases, normalized paths, BLAKE3 content hashes or absence states. Its
contents participate in the input hash; current dependencies are reread before
reuse. This includes creation/deletion of optional inputs. Both files are disposable and not a substitute
for `cog.lock`. Missing/corrupt cache or artifacts cause rebuilds. Compile errors stop
execution instead of running a stale artifact. Artifact replacement uses a temporary
file and rename. Deleting target does not lose dependency resolution.

Check always analyzes source and writes no artifact/lock/cache. Explicit bytecode
execution needs no source/configuration/cache. Builds report `built` or `finished`;
check reports `checked`; a cached run is silent. Preparation timing excludes VM
execution. Status/diagnostics go to stderr, program output and requested `--debug`
disassembly to stdout. `--color auto|always|never` controls color, and `NO_COLOR`
disables automatic coloring. Failures distinguish preparation from runtime errors.

Caching is conservative and still reads/discovers inputs. This is not file-level
incremental compilation. Compiler-aware build inputs and their materialization
contracts are in [semantic contracts](SEMANTIC_CONTRACTS.md). Project mode locates
the nearest enclosing package.kdl when invoked from a subdirectory. Runtime CWD
is not changed.
