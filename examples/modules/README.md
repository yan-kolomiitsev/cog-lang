# Modules example

Run from the repository root:

```sh
cog check examples/modules
cog run examples/modules
```

The program prints `Hello, Cog!`. `main.cog` imports `greeting.cog`, whose public
function provides the greeting. The package manifest fixes the artifact name:
`cog build examples/modules` writes `examples/modules/target/modules-demo.cogb`.
No dependency manifest is needed because both modules belong to one package.

See [projects and configuration](../../docs/PROJECT_FORMATS.md).
