# Source package inclusion

Run from the repository root:

```sh
cog run examples/package_include
```

The entry point includes the `game` source package and the embedded standard
library. Both packages provide a `random` module, so the imports select their
providers with `from`. The `game::numbers` module also defines a compile-time
type alias and registers an integer extension method.

Expected output:

```text
game package
18
```

See [project configuration](../../docs/PROJECT_FORMATS.md) and the
[language reference](../../docs/LANGUAGE.md).
