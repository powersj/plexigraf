# Working on Plexigraf

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
  `examples/random-rust`, `crates/plexigraf/tests/plugins/*`, and
  `crates/plexigraf/fuzz`) each copy that table into their own `[lints]`; keep
  the copies in sync.
- Every public item in `sdk/rust` needs a `///` doc comment: it is the API
  plugin authors read.

## Panics and errors

- The agent uses `anyhow` for errors. Return errors instead of panicking on
  bad config, input, or plugin output; plugins and the network are untrusted.
- `unwrap` and `expect` are for invariants the code guarantees, such as
  `mutex.lock()`. An `expect` message states the invariant.
- Panics unwind (see `[profile.release]`), and the hook in `lib.rs` logs them.

## Tests

- Encoders and parsers get property tests (`proptest`) as well as examples.
  See `encoding/influx.rs` for the pattern.
- Snapshot tests use `insta`. When output changes on purpose, run
  `cargo insta review` and check every diff before accepting it.
- `make coverage` shows untested lines.
- Parsers of untrusted input get a fuzz target in `crates/plexigraf/fuzz/`.
- `make bench` before and after changes to `src/encoding/`.
- After changing `plexigraf-sdk`'s public API, run `make public-api` and commit
  the listing.
