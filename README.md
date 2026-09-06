<!--
SPDX-License-Identifier: MIT OR Apache-2.0
SPDX-FileCopyrightText: Copyright © the fdio contributors
-->

# fdio

A capability-based filesystem library for Linux.

fdio provides sandboxed filesystem access using O_PATH file descriptors and
`openat2` with `RESOLVE_IN_ROOT`/`RESOLVE_BENEATH` for directory confinement.
It is designed for use in security-sensitive programs like Flatpak where
filesystem operations must be confined to a specific directory tree.

## Design goals

- **Compile-time file type safety.** `FileRef<Dir>`, `FileRef<Regular>`,
  `FileRef<Socket>`, `FileRef<Symlink>` — the type system prevents calling
  directory operations on a regular file or reopening a symlink through procfs.
- **Sandbox by default.** Path resolution goes through a chase engine that
  enforces `RESOLVE_IN_ROOT` or `RESOLVE_BENEATH`. Single-component kernel
  operations use the `Name` type to prevent path traversal.
- **Layered API.** The `Chase` builder provides composable, hookable path
  resolution. The low-level `chaseat` engine is also public for direct use.
- **Minimal dependencies.** Only depends on rustix and bitflags.

## Development

### Building and testing

```
cargo build
make test
```

### Checks

`make check` runs the same checks as CI: formatting, clippy lints, and doc warnings.

```
make check
```

Individual targets: `make fmt`, `make clippy`, `make doc`.

### Coverage

Generate an HTML coverage report and serve it locally:

```
make coverage-report
```

This requires [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) and serves
the report at `http://localhost:8080`.

### CI

GitHub Actions runs formatting, clippy, doc, and test checks on every push to `main`
and on pull requests. It also verifies the MSRV and minimum dependency versions.

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSES/Apache-2.0.txt)
- [MIT License](LICENSES/MIT.txt)

at your option.
