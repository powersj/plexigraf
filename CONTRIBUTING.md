# Contributing to Tern

## Set up

Run `make setup` once after cloning. It installs the pinned toolchain (from
`rust-toolchain.toml`), nightly for fuzzing and API listings, the cargo tools
the Makefile uses, and the git pre-commit hook.

## Make a change

1. Run `make lint` and `make test` before every commit. The pre-commit hook
   and CI both run `make lint`, and CI also checks MSRV, the SDK's public
   API, the binary size budget, and a short fuzz run.
2. Start commit subjects with an imperative verb: "Add", "Fix", "Remove",
   "Deprecate", or another verb for changes. The verb picks the CHANGELOG
   section (see `cliff.toml`).
3. If you change `tern-sdk`'s public API, run `make public-api` and commit the
   updated `sdk/rust/public-api/*.txt`, so reviewers see the API diff.

[AGENTS.md](AGENTS.md) has the lint and test conventions.

## Versioning and deprecation

The agent, `tern-sdk`, and the `tern:*` WIT packages each follow
[Semantic Versioning](https://semver.org/). Before 1.0, a minor version bump
(0.x to 0.(x+1)) may break things and a patch bump may not.

A breaking change is anything that can make a working setup fail after an
upgrade:

- **Agent:** removing or renaming a config key, CLI flag, or metric name, or
  changing a default in a way that changes output.
- **SDK:** any change `make semver` reports, and raising the SDK's
  `rust-version`.
- **WIT:** any change to a published `tern:*` package version. Changes go
  into a new package version, and the host keeps supporting the previous one.

Deprecated items keep working for at least one minor release, log a warning
that names the replacement, and are listed under "Deprecated" in the
CHANGELOG. They are removed no earlier than the next minor release after that.

## Toolchain and MSRV

- **Toolchain:** bump `rust-toolchain.toml` within two weeks of each Rust
  stable release (every six weeks), in a PR that also fixes any new lints.
  The weekly CI job runs beta, so new lints show up early.
- **Agent MSRV:** `rust-version` in the root `Cargo.toml`. It follows
  wasmtime's MSRV, and raising it is not a breaking change.
- **SDK MSRV:** `rust-version` in `sdk/rust/Cargo.toml`. Plugin authors build
  with their own toolchains, so keep it as low as dependencies allow, and
  treat raising it as a breaking change.

## Adding a dependency

Every crate becomes code we ship, review, and update. Before adding one,
check, and say in the PR:

- **Need:** whether the standard library or an existing dependency covers it.
- **Maintenance:** recent releases, and an owner who responds to issues.
- **Weight:** how many crates it adds (`cargo tree -p tern -i <crate>` after
  adding it) and how much it grows the binary (`make size`).
- **License:** `make lint` runs cargo-deny, which rejects licenses not listed
  in `deny.toml`.
- **Features:** set `default-features = false` and enable only what you use.

Keep our own crates' features additive: enabling a feature must never remove
an API or change behavior that existing code relies on. (The SDK's world
features are the deliberate exception: exactly one must be enabled.)

## Testing tools

- **Property tests** (`proptest`) for parsers, encoders, and invariants.
- **Snapshot tests** (`insta`). Review changes with `cargo insta review`.
- **Fuzzing:** `make fuzz` for the line protocol decoder. Add a target in
  `crates/tern/fuzz/` for any new parser of untrusted input.
- **Benchmarks:** `make bench`. Run before and after touching
  `src/encoding/`, and include the numbers in the PR.
- **Mutation testing:** `cargo mutants -p tern -f <file>` shows changes to
  the code that no test catches. Run it on code you add tests for.

## Releases

1. Run `make changelog` and edit the result into release notes.
2. Bump the version in `[workspace.package]` and commit.
3. Tag `vX.Y.Z` and push the tag. The release workflow (cargo-dist) builds
   the binaries, an SBOM, and build provenance attestations, and publishes a
   GitHub release.

## Repository settings

Once the repo is on GitHub, protect `main` with a ruleset that requires pull
requests and these status checks: `lint`, `test`, `msrv`, `api`, `size`, and
`fuzz`. Enable private vulnerability reporting for [SECURITY.md](SECURITY.md).

## Quarterly health review

Each quarter, run `make health`, `cargo build --release -p tern --timings`,
and `make bench`, and open an issue titled "Health review YYYY-QN" that
compares these with the last review:

- release build time and CI duration
- direct and total dependency counts
- open advisories
- binary size against the budget in the Makefile
- benchmark results

Then pick at most one thing to improve.
