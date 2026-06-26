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
  check. UI-agnostic progress via the `progress` traits.
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

The HTTP/TLS backend is selectable so downstream builds can avoid a backend's
build dependency. Pick exactly one:

- `rustls-tls` *(default)*: rustls with the `aws-lc-rs` provider (needs a C
  toolchain to build).
- `native-tls`: links the platform's TLS stack instead:

  ```toml
  thunderstore-engine = { version = "0.1", default-features = false, features = ["native-tls"] }
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
