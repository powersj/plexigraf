# Working on Tern

## Before you finish any change

Run `make lint` after every change to Rust code, Cargo manifests, or
`deny.toml`, and fix everything it reports before you call the work done.
`make fmt` fixes formatting. Then run `make test`.

`make lint` checks formatting, Clippy (with pedantic lints) on every crate,
doc warnings, cargo-deny, and unused dependencies. It also runs as a git
pre-commit hook and in CI, so a change that fails it cannot merge.

## Lint rules

- Do not silence a lint to make it pass. If a lint is wrong for one item,
  use `#[expect(clippy::lint_name, reason = "...")]` on that item, with a
  reason that explains why. `expect` warns when the lint stops firing, so
  stale exemptions get cleaned up.
- Change lint levels only in the `[workspace.lints]` table in the root
  `Cargo.toml`. The crates outside the workspace (`sdk/rust`,
  `examples/random-rust`, and `crates/tern/tests/plugins/*`) each copy that
  table into their own `[lints]`; keep the copies in sync.
- Every public item in `sdk/rust` needs a `///` doc comment: it is the API
  plugin authors read.

## Tests

- Encoders and parsers get property tests (`proptest`) as well as examples.
  See `encoding/influx.rs` for the pattern.
- Snapshot tests use `insta`. When output changes on purpose, run
  `cargo insta review` and check every diff before accepting it.
- `make coverage` shows untested lines.
