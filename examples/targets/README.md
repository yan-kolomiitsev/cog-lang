# Multiple build targets

Run from the repository root:

```sh
cog check examples/targets --target client
cog run examples/targets --target client
cog run examples/targets --target server
```

Each target is a separate entry point selected in `build.kdl`. The programs print
`Client target` or `Server target`.
There is no conventional main and two targets are declared, so selection is required.
Building server writes `examples/targets/target/server.cogb`.

See [projects and configuration](../../docs/PROJECT_FORMATS.md).
