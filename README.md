# thunderstore-engine

A game-agnostic Rust engine for building [Thunderstore](https://thunderstore.io)
mod managers. Extracted from
[`valheim-mod-manager`](https://github.com/endoze/valheim-mod-manager) so it can
be shared across projects.

## What it provides

- **`client`** — async Thunderstore API client with a `zstd` + `bincode`
  package-index cache (per community), keyed by a `Last-Modified` freshness
  check. UI-agnostic progress via the `progress` traits.
- **`models`** — the package/version data model (struct-of-arrays + `lasso`
  string interning) and the on-disk cache wire format (v1/v2/v3 with migration).
- **`resolve`** — dependency-graph resolution over the package index.
- **`manifest`** — parsing of a package's `manifest.json`.
- **`extract`** — CRC-verified, traversal-safe zip extraction primitives.
- **`ecosystem`** — the Thunderstore ecosystem-schema (games + mod loaders),
  with a bundled snapshot and network refresh.
- **`install`** — schema-driven, *root-relative* mod install routing
  (override-folder routing, `<Author-ModName>` namespacing, loader-pack
  handling) plus a recorded file manifest for exact uninstall.

The engine never resolves the absolute game directory, performs platform
discovery, or launches games — those are game-specific concerns left to the
caller.

## Quick start

```rust
use thunderstore_engine::client::ThunderstoreClient;

let client = ThunderstoreClient::builder()
    .base_url("https://stacklands.thunderstore.io")
    .community("valheim")
    .cache_dir("/path/to/cache")
    .build()?;

let index = client.get_manifest().await?;
```

## Benchmarks

Generate fixtures from live data, then run benches:

```sh
cargo run --example generate-fixtures
cargo bench --features bench
```

## Credits / references

- Thunderstore API: <https://thunderstore.io/api/docs/>
- Ecosystem schema: <https://github.com/thunderstore-io/ecosystem-schema>
- Install-rule modeling mirrors Gale: <https://github.com/Kesomannen/gale>

## License

MIT
