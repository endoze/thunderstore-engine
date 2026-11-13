# thunderstore-engine

![Build Status](https://github.com/endoze/thunderstore-engine/actions/workflows/ci.yml/badge.svg?branch=master)
[![Coverage Status](https://coveralls.io/repos/github/endoze/thunderstore-engine/badge.svg?branch=master)](https://coveralls.io/github/endoze/thunderstore-engine?branch=master)
[![Crate](https://img.shields.io/crates/v/thunderstore-engine.svg)](https://crates.io/crates/thunderstore-engine)
[![Docs](https://docs.rs/thunderstore-engine/badge.svg)](https://docs.rs/thunderstore-engine)

A game-agnostic Rust engine for building [Thunderstore](https://thunderstore.io)
mod managers. Extracted from
[`valheim-mod-manager`](https://github.com/endoze/valheim-mod-manager) so it can
be shared across projects.

## What it provides

- **`client`**: async Thunderstore API client with a `zstd` + `bincode`
  package-index cache (per community), keyed by a `Last-Modified` freshness
  check. UI-agnostic progress via the `progress` traits, and per-operation
  progress events and cancellation via `progress::Operation`.
- **`models`**: the package/version data model (struct-of-arrays + `lasso`
  string interning) and the on-disk cache wire format.
- **`resolve`**: dependency-graph resolution over the package index.
- **`manifest`**: parsing of a package's `manifest.json`.
- **`extract`**: CRC-verified, traversal-safe zip extraction primitives.
- **`ecosystem`**: Thunderstore's upstream ecosystem schema (games, mod
  loaders, install rules, and the mod-loader package registry), consumed
  directly so routing works for every Thunderstore game with no per-game
  maintenance. Ships a compressed `zstd` snapshot as an offline fallback and
  refreshes over the network.
- **`install`**: schema-driven, *root-relative* mod install routing over the
  ecosystem's install rules (route-leaf matching, longest-extension precedence,
  recursive subroutes, all tracking methods, `<Owner-Name>` namespacing, and
  registry-based loader-pack handling) plus a recorded file manifest for exact
  uninstall.
- **`profile`**: r2modman-compatible profiles, covering directory layout, the
  `mods.yml` model, a per-game package cache, `.r2z` export/import, and
  computing a `LaunchPlan` that the caller spawns.
- **`steam`**: asks a running Steam client whether it can resolve a path, so a
  sandboxed Steam's silent launch failure surfaces before a launch instead of
  only in Steam's own console log.

The engine never resolves the absolute game directory, performs platform
discovery, or launches games. Those are game-specific concerns left to the
caller.

## Installation

As a dependency of a Rust project:

```sh
cargo add thunderstore-engine
```

## Quick start

```rust,no_run
use thunderstore_engine::client::ThunderstoreClient;

#[tokio::main]
async fn main() -> thunderstore_engine::Result<()> {
  let client = ThunderstoreClient::builder()
    .base_url("https://thunderstore.io")
    .community("valheim")
    .cache_dir("/path/to/cache")
    .build()?;

  let index = client.get_manifest().await?;

  println!("{} packages", index.len());

  Ok(())
}
```

## Cargo features

### `io` (default)

Everything that touches disk or network: the `client`, the index cache, zip
extraction, and every profile operation that reads or writes a target. With
`io` off, the crate builds without an HTTP client, an async runtime, or an
archive library, and keeps what does no IO:

- the package, version, and ecosystem models (including
  `Ecosystem::require_profile`), and the dependency resolver;
- the game to community mapping (`GameProfile::community`,
  `GameDef::community`), read from each game's package index location;
- the `Error` type, which is `Clone` so a client can keep one in its state;
- the plan types (`InstallPlan`, `InstallBatch`, `UninstallBatch`,
  `ToggleBatch`, `LaunchPlan`) and the `mods.yml` and export models;
- profile name validation, `profile::layout::validate_name`, which refuses
  names Windows cannot hold on every OS and gives the reason as a `BadName`;
- pure planners that take what the `io` functions would read as arguments:
  `install::plan_install_from` (a package's file list),
  `install::plan_delisted_from`, `profile::plan_install_batch_from`,
  `profile::plan_set_enabled_batch_from`,
  `profile::plan_uninstall_batch_from`, and
  `profile::launch::launch_plan_from` (a target's `LaunchFacts`);
- queries over installed packages: `query::dependants` (every installed
  package that requires a given one, directly or transitively) and
  `query::outdated` (installed packages the index has a newer version of),
  with "newer" defined by the total ordering on `profile::modlist::SemVer`;
- browsing the package index: `query::browse` searches, filters, and sorts
  it into package indices per a `query::BrowseQuery` (r2modman's defaults:
  NSFW and deprecated hidden, community-pinned first, newest update first,
  or `query::SortKey::BestMatch` to rank the closest match to the search
  first), and `query::PackageRow` reads one package's list fields without building
  its versions;
- the per-operation progress and cancel types in `progress` (`Operation`,
  `OperationId`, `ProgressEvent`, `Stage`, `StageUpdate`, `CancelToken`) and
  `profile::BatchOutcome`, so a client can store an operation's events and
  result.

Each `io` function with a pure counterpart reads its inputs and calls that
counterpart, so the two decide identically. To depend on the pure part only:

```toml
thunderstore-engine = { version = "0.3", default-features = false }
```

Cargo merges features across everything built together, so a crate that must
not reach the `io` functions is only held to that when it is built on its own.

### TLS backend

The HTTP/TLS backend is selectable so downstream builds can avoid a backend's
build dependency. Either one turns on `io`. Pick exactly one:

- `rustls-tls` *(default)*: rustls with the `aws-lc-rs` provider (needs a C
  toolchain to build).
- `native-tls`: links the platform's TLS stack instead:

  ```toml
  thunderstore-engine = { version = "0.1", default-features = false, features = ["native-tls"] }
  ```

### `test-support`

Off by default. Adds `test_support`, a mock Thunderstore built on `mockito`
that serves a small package index, real package archives, and their readmes,
so a client's tests can run the install pipeline with no network access.
`MockThunderstore::start` (or `start_async` in an async test) starts it, and
`client_builder` returns a client pointed at it. Turn it on for tests only:

```toml
[dev-dependencies]
thunderstore-engine = { version = "0.3", features = ["test-support"] }
```

`test-support` turns on `io`, so it needs a TLS backend like any `io` build.
A client on `native-tls` repeats its own features, so the test build doesn't
also get the default `rustls-tls`:

```toml
[dev-dependencies]
thunderstore-engine = { version = "0.3", default-features = false, features = ["native-tls", "test-support"] }
```

## Refreshing the bundled ecosystem snapshot

The crate embeds a compressed snapshot of Thunderstore's ecosystem schema as an
offline fallback (`assets/ecosystem-schema.json.zst`); `Ecosystem::fetch_latest`
pulls a fresh copy at runtime. Regenerate the embedded fallback with:

```sh
curl -s https://thunderstore.io/api/experimental/schema/dev/latest/ \
  | zstd -19 -o assets/ecosystem-schema.json.zst
```

## Credits / references

- Thunderstore API: <https://thunderstore.io/api/docs/>
- Ecosystem schema: <https://github.com/thunderstore-io/ecosystem-schema>
- Install routing follows the model proven in r2modman
  (<https://github.com/ebkr/r2modmanPlus>) and Gale
  (<https://github.com/Kesomannen/gale>).

## License

MIT
