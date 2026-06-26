//! The Thunderstore ecosystem schema: games, mod loaders, and install rules.
//!
//! This mirrors Thunderstore's upstream ecosystem schema — the same data that
//! drives r2modman — rather than a bespoke normalized copy. Each game carries
//! one or more profiles (`r2modman[]`) with a [`LoaderKind`](crate::ecosystem::LoaderKind),
//! recursive [`InstallRule`](crate::ecosystem::InstallRule)s that route a package's
//! files, and metadata (executables, store distributions). A top-level
//! [`ModloaderPackage`](crate::ecosystem::ModloaderPackage) registry names every
//! package that *is* a loader pack (and the subfolder of it that unpacks to the
//! root), so the engine never hard-codes per-game loader facts.
//!
//! The canonical source is the Thunderstore ecosystem schema, published as JSON
//! at [`DEFAULT_ECOSYSTEM_URL`](crate::ecosystem::DEFAULT_ECOSYSTEM_URL). A
//! compressed snapshot is embedded at compile time as a fallback
//! ([`Ecosystem::bundled`](crate::ecosystem::Ecosystem::bundled));
//! [`Ecosystem::fetch_latest`](crate::ecosystem::Ecosystem::fetch_latest) refreshes
//! it from the network. Parsing is intentionally defensive (`#[serde(default)]`,
//! unknown loaders/tracking methods fall back to `Other`) because this is an
//! external, evolving contract.

#![deny(missing_docs)]

use crate::error::{Error, Result};
use crate::retry::{RetryPolicy, retry_http};
use crate::util::SizeCap;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;

/// Compressed (`zstd`) snapshot of the ecosystem schema embedded at compile time
/// as an offline fallback. Regenerate with the command documented in `README.md`.
const BUNDLED_ECOSYSTEM: &[u8] = include_bytes!("../assets/ecosystem-schema.json.zst");

/// The default URL used by [`Ecosystem::fetch_latest`] to refresh the snapshot.
///
/// # Trust
///
/// This is Thunderstore's official (experimental) schema endpoint — the same
/// one r2modman consumes. Because the ecosystem drives install *routing* (where
/// a package's files land on disk), whatever this endpoint returns is fully
/// trusted; only refresh from a source you trust. Being an *experimental*
/// endpoint, its shape may change over time, so parsing is defensive.
///
/// [`Ecosystem::fetch_latest`] returns an `Err` on any refresh failure and does
/// *not* fall back on its own; a caller that wants stale-over-broken behavior
/// should use [`Ecosystem::bundled`] (or a previously fetched value) when the
/// refresh fails.
pub const DEFAULT_ECOSYSTEM_URL: &str =
  "https://thunderstore.io/api/experimental/schema/dev/latest/";

/// Maximum size of an ecosystem schema fetched over the network. The real schema
/// is small relative to this generous cap, which bounds the memory a misbehaving or
/// compromised endpoint can force [`Ecosystem::fetch_latest`] to allocate.
const MAX_ECOSYSTEM_BYTES: u64 = 64 * 1024 * 1024;

/// A parsed ecosystem schema: the known games plus the mod-loader package registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Ecosystem {
  /// The schema version string reported by the source (e.g. `"0.3.0"`).
  #[serde(
    rename = "schemaVersion",
    default,
    deserialize_with = "null_as_default"
  )]
  pub schema_version: String,
  /// All known games, keyed by their community slug.
  #[serde(default, deserialize_with = "null_as_default")]
  pub games: HashMap<String, GameDef>,
  /// Every package that is itself a mod-loader pack, with its unpack root.
  #[serde(
    rename = "modloaderPackages",
    default,
    deserialize_with = "null_as_default"
  )]
  pub modloader_packages: Vec<ModloaderPackage>,
}

impl Ecosystem {
  /// Loads the bundled ecosystem snapshot embedded in the binary.
  ///
  /// # Panics
  ///
  /// Panics if the bundled snapshot fails to decompress or parse, which is a
  /// build-time invariant. Use [`Ecosystem::try_bundled`] to handle the error.
  ///
  /// # Examples
  ///
  /// Routing works offline with no network call, which is the point of the
  /// bundled snapshot:
  ///
  /// ```
  /// use thunderstore_engine::ecosystem::Ecosystem;
  ///
  /// let eco = Ecosystem::bundled();
  ///
  /// assert!(eco.games().count() > 0);
  /// assert!(eco.game("valheim").is_some());
  /// ```
  pub fn bundled() -> Self {
    Self::try_bundled()
      .expect("bundled ecosystem snapshot must be valid; this is a build-time invariant")
  }

  /// Loads the bundled ecosystem snapshot, returning an error instead of
  /// panicking if it cannot be decompressed or parsed.
  pub fn try_bundled() -> Result<Self> {
    let json = zstd::decode_all(BUNDLED_ECOSYSTEM)
      .map_err(|e| Error::Ecosystem(format!("failed to decompress bundled schema: {}", e)))?;

    Self::from_json_slice(&json)
  }

  /// Parses an ecosystem snapshot from a JSON string.
  pub fn from_json(json: &str) -> Result<Self> {
    Self::from_json_slice(json.as_bytes())
  }

  /// Parses an ecosystem snapshot from JSON bytes.
  fn from_json_slice(bytes: &[u8]) -> Result<Self> {
    serde_json::from_slice(bytes).map_err(|e| Error::Ecosystem(format!("failed to parse: {}", e)))
  }

  /// Fetches the latest ecosystem snapshot from `url` and parses it.
  ///
  /// Use [`DEFAULT_ECOSYSTEM_URL`] for the default location. The response is
  /// fully trusted to define install routing, so only fetch from a source you
  /// trust — see the [`DEFAULT_ECOSYSTEM_URL`] trust note. The response body is
  /// size-bounded (see `MAX_ECOSYSTEM_BYTES`) so a misbehaving endpoint cannot
  /// force unbounded allocation.
  pub async fn fetch_latest(client: &reqwest::Client, url: &str) -> Result<Self> {
    Self::fetch_latest_bounded(client, url, MAX_ECOSYSTEM_BYTES, &RetryPolicy::default()).await
  }

  /// [`fetch_latest`](Self::fetch_latest) with an explicit body-size cap and
  /// retry policy. Transient network failures are retried; a size-cap breach or
  /// a parse failure is terminal.
  async fn fetch_latest_bounded(
    client: &reqwest::Client,
    url: &str,
    max_bytes: u64,
    retry: &RetryPolicy,
  ) -> Result<Self> {
    use futures::StreamExt;

    let body = retry_http(retry, async || {
      let response = client.get(url).send().await?.error_for_status()?;

      let mut cap = SizeCap::new("ecosystem schema", max_bytes);

      // Reject a declared-oversize body up front, before streaming a single byte,
      // then enforce the cap against the actual streamed bytes.
      cap.check_declared(response.content_length())?;

      let mut stream = response.bytes_stream();
      let mut body: Vec<u8> = Vec::new();

      while let Some(chunk) = stream.next().await {
        let chunk = chunk?;

        cap.add(chunk.len() as u64)?;

        body.extend_from_slice(&chunk);
      }

      Ok(body)
    })
    .await?;

    Self::from_json_slice(&body)
  }

  /// Returns the game definition for a community slug, if known.
  pub fn game(&self, slug: &str) -> Option<&GameDef> {
    self.games.get(slug)
  }

  /// Iterates over all known games.
  pub fn games(&self) -> impl Iterator<Item = &GameDef> {
    self.games.values()
  }

  /// Looks up a package in the mod-loader registry by its `Owner-Name`
  /// identifier, returning its entry if the package is a loader pack.
  ///
  /// This is the reverse lookup that replaces per-game loader configuration: a
  /// package that appears here is unpacked to the profile root (see
  /// [`ModloaderPackage::root_folder`]); anything else is routed as a regular
  /// mod via the game's [`InstallRule`]s.
  pub fn modloader_package(&self, package_full_name: &str) -> Option<&ModloaderPackage> {
    self
      .modloader_packages
      .iter()
      .find(|m| m.package_id == package_full_name)
  }
}

/// A single game's definition.
///
/// `#[non_exhaustive]`: fields may be added as the ecosystem schema grows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GameDef {
  /// The Thunderstore community slug.
  #[serde(rename = "label", default, deserialize_with = "null_as_default")]
  pub slug: String,
  /// The schema's stable identifier for this game.
  #[serde(default, deserialize_with = "null_as_default")]
  pub uuid: String,
  /// Display metadata (name, icon).
  #[serde(default)]
  pub meta: Meta,
  /// Store distributions (Steam/Epic/etc.) at the game level.
  #[serde(default, deserialize_with = "null_as_default")]
  pub distributions: Vec<Distribution>,
  /// Per-manager profiles. Most games have one; a few add a dedicated-server
  /// profile whose install rules match the primary profile's.
  #[serde(rename = "r2modman", default, deserialize_with = "null_as_default")]
  pub profiles: Vec<GameProfile>,
}

impl GameDef {
  /// The human-readable display name of the game.
  pub fn display_name(&self) -> &str {
    &self.meta.display_name
  }

  /// Returns the primary profile to route against: the first `game`-instance
  /// profile, or the first profile of any kind.
  pub fn profile(&self) -> Option<&GameProfile> {
    self
      .profiles
      .iter()
      .find(|p| p.instance_type == InstanceType::Game)
      .or_else(|| self.profiles.first())
  }
}

/// A per-manager profile for a game: its loader, install rules, and metadata.
///
/// `#[non_exhaustive]`: fields may be added as the ecosystem schema grows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GameProfile {
  /// Display metadata for this profile.
  #[serde(default)]
  pub meta: Meta,
  /// The mod-loader family this profile uses.
  #[serde(
    rename = "packageLoader",
    default,
    deserialize_with = "null_as_default"
  )]
  pub package_loader: LoaderKind,
  /// Whether this profile targets the game or a dedicated server.
  #[serde(
    rename = "gameInstanceType",
    default,
    deserialize_with = "null_as_default"
  )]
  pub instance_type: InstanceType,
  /// Candidate executable names for game-directory validation.
  #[serde(rename = "exeNames", default, deserialize_with = "null_as_default")]
  pub exe_names: Vec<String>,
  /// Store distributions for this profile.
  #[serde(default, deserialize_with = "null_as_default")]
  pub distributions: Vec<Distribution>,
  /// Filenames to skip when copying a package's contents (e.g. `manifest.json`).
  /// A JSON `null` is normalized to an empty list.
  #[serde(
    rename = "relativeFileExclusions",
    default,
    deserialize_with = "null_as_default"
  )]
  pub relative_file_exclusions: Vec<String>,
  /// The routing rules for a regular mod's files (a recursive forest).
  #[serde(rename = "installRules", default, deserialize_with = "null_as_default")]
  pub install_rules: Vec<InstallRule>,
  /// The loader-specific data subdirectory, when the schema records one.
  #[serde(rename = "dataFolderName", default)]
  pub data_folder_name: Option<String>,
  /// The manager's settings identifier for this profile.
  #[serde(rename = "settingsIdentifier", default)]
  pub settings_identifier: Option<String>,
  /// The package-index URL for this community.
  #[serde(rename = "packageIndex", default)]
  pub package_index: Option<String>,
}

impl GameProfile {
  /// The Steam application id, parsed from the profile's distributions, if any.
  pub fn steam_app_id(&self) -> Option<u32> {
    self
      .distributions
      .iter()
      .find(|d| d.platform == "steam")
      .and_then(|d| d.identifier.parse().ok())
  }
}

/// Display metadata (name and icon) attached to a game or profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Meta {
  /// Human-readable display name.
  #[serde(rename = "displayName", default, deserialize_with = "null_as_default")]
  pub display_name: String,
  /// Relative URL to the icon asset.
  #[serde(rename = "iconUrl", default, deserialize_with = "null_as_default")]
  pub icon_url: String,
}

/// A store distribution: which platform lists the game and under what id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Distribution {
  /// The platform, e.g. `"steam"`, `"epic-games-store"`, `"xbox-game-pass"`.
  #[serde(default, deserialize_with = "null_as_default")]
  pub platform: String,
  /// The platform-specific identifier (e.g. a Steam app id as a string). May be
  /// `null` upstream (normalized to an empty string).
  #[serde(default, deserialize_with = "null_as_default")]
  pub identifier: String,
}

/// A package that is itself a mod-loader pack.
///
/// Membership in the registry is how the engine decides a package unpacks to the
/// profile root rather than being routed as a regular mod.
///
/// `#[non_exhaustive]`: fields may be added as the ecosystem schema grows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModloaderPackage {
  /// The package's `Owner-Name` identifier.
  #[serde(rename = "packageId", default, deserialize_with = "null_as_default")]
  pub package_id: String,
  /// The subfolder within the pack whose *contents* unpack to the root, or an
  /// empty string when the pack's own root should be copied directly.
  #[serde(rename = "rootFolder", default, deserialize_with = "null_as_default")]
  pub root_folder: String,
  /// The loader family this pack provides.
  #[serde(default, deserialize_with = "null_as_default")]
  pub loader: LoaderKind,
}

/// How files matched by an [`InstallRule`] are placed and tracked under its route.
///
/// `#[non_exhaustive]`: new tracking methods may be added without a breaking
/// change, so downstream `match`es must include a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum TrackingMethod {
  /// Namespaced under `route/<Owner-Name>/`, flattened. The default.
  #[default]
  Subdir,
  /// Written directly under `route/`, shared and untracked (e.g. config).
  None,
  /// Written directly under `route/`, shared, but each file recorded for
  /// exact uninstall.
  State,
  /// Namespaced under `route/<Owner-Name>/`, preserving internal structure.
  SubdirNoFlatten,
  /// The package archive is copied into `route/` as a zip.
  PackageZip,
  /// An unrecognized tracking method from a newer schema.
  #[serde(other)]
  Other,
}

impl TrackingMethod {
  /// Whether files routed via this method live in a per-mod `<Owner-Name>/`
  /// folder (and are therefore reconciled by directory in [`crate::install`]).
  pub fn is_namespaced(self) -> bool {
    matches!(
      self,
      TrackingMethod::Subdir | TrackingMethod::SubdirNoFlatten
    )
  }
}

/// A single, possibly nested, routing rule for a regular mod's files.
///
/// `#[non_exhaustive]`: fields may be added as the ecosystem schema grows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct InstallRule {
  /// Destination path relative to the profile root, e.g. `"BepInEx/plugins"`.
  /// The final path segment is the override-folder name matched at a package's
  /// root (e.g. `plugins`).
  #[serde(default, deserialize_with = "null_as_default")]
  pub route: String,
  /// Extensions (matched as filename suffixes) that route a loose file here.
  /// When several rules match, the longest suffix wins.
  #[serde(
    rename = "defaultFileExtensions",
    default,
    deserialize_with = "null_as_default"
  )]
  pub default_file_extensions: Vec<String>,
  /// How matched files are placed and tracked under [`InstallRule::route`].
  #[serde(
    rename = "trackingMethod",
    default,
    deserialize_with = "null_as_default"
  )]
  pub tracking_method: TrackingMethod,
  /// Nested rules applied within this rule's route.
  #[serde(rename = "subRoutes", default, deserialize_with = "null_as_default")]
  pub sub_routes: Vec<InstallRule>,
  /// Whether this rule is the catch-all for files matching no other rule.
  /// At most one rule per profile is the default location; some have none.
  #[serde(
    rename = "isDefaultLocation",
    default,
    deserialize_with = "null_as_default"
  )]
  pub is_default_location: bool,
}

/// Whether a [`GameProfile`] targets the game itself or a dedicated server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum InstanceType {
  /// The playable game (the default).
  #[default]
  Game,
  /// A dedicated server build.
  Server,
  /// An unrecognized instance type from a newer schema.
  #[serde(other)]
  Other,
}

/// The mod-loader family. Unknown values deserialize to [`LoaderKind::Other`].
///
/// `#[non_exhaustive]`: new loader families may be added without a breaking
/// change, so downstream `match`es must include a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum LoaderKind {
  /// BepInEx (`bepinex`), injected through Unity Doorstop.
  #[serde(rename = "bepinex")]
  BepInEx,
  /// BepisLoader (`bepisloader`), the BepInEx successor, which takes its own
  /// argument set rather than Doorstop's.
  #[serde(rename = "bepisloader")]
  BepisLoader,
  /// GDWeave (`gdweave`), pointed at its mod folder with
  /// `--gdweave-folder-override`.
  #[serde(rename = "gdweave")]
  GdWeave,
  /// Godot Mod Loader (`godotml`), driven by a `--script` entry point plus
  /// `--mods-path`.
  #[serde(rename = "godotml")]
  GodotMl,
  /// Lovely (`lovely`), pointed at its mod folder with `--mod-dir`.
  #[serde(rename = "lovely")]
  Lovely,
  /// MelonLoader (`melonloader`).
  #[serde(rename = "melonloader")]
  MelonLoader,
  /// MelonLoader shipped in a nested package layout (`recursive-melonloader`).
  /// Injects identically to [`LoaderKind::MelonLoader`]; only the package's file
  /// layout differs.
  #[serde(rename = "recursive-melonloader")]
  RecursiveMelonLoader,
  /// Northstar (`northstar`), selected with `-northstar` and a `-profile=`
  /// directory.
  #[serde(rename = "northstar")]
  Northstar,
  /// ReturnOfModding (`return-of-modding`), pointed at its root with
  /// `--rom_modding_root_folder`.
  #[serde(rename = "return-of-modding")]
  ReturnOfModding,
  /// Rivet (`rivet`), enabled with `-rivetEnable` plus explicit loader and mod
  /// paths.
  #[serde(rename = "rivet")]
  Rivet,
  /// Shimloader (`shimloader`), which takes separate mod, pak, cfg, and overlay
  /// directories.
  #[serde(rename = "shimloader")]
  Shimloader,
  /// Unity Mod Manager (`umm`), injected through Unity Doorstop like
  /// [`LoaderKind::BepInEx`].
  #[serde(rename = "umm")]
  Umm,
  /// A community with no mod loader.
  #[serde(rename = "none")]
  None,
  /// An unrecognized loader family from a newer schema, or an absent value.
  #[default]
  #[serde(other)]
  Other,
}

/// Normalizes a JSON `null` (or absent value) to `T::default()` during
/// deserialization, so a nullable list field becomes an empty `Vec`.
fn null_as_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
  D: Deserializer<'de>,
  T: Deserialize<'de> + Default,
{
  let opt = Option::<T>::deserialize(deserializer)?;

  Ok(opt.unwrap_or_default())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_bundled_parses() {
    let eco = Ecosystem::bundled();

    assert!(!eco.games.is_empty());
    assert!(!eco.modloader_packages.is_empty());
    assert_eq!(eco.schema_version, "0.3.0");
  }

  #[test]
  fn test_valheim_bepinex() {
    let eco = Ecosystem::bundled();
    let valheim = eco.game("valheim").expect("valheim present");

    assert_eq!(valheim.slug, "valheim");
    assert_eq!(valheim.display_name(), "Valheim");

    let profile = valheim.profile().expect("a profile");

    assert_eq!(profile.package_loader, LoaderKind::BepInEx);
    assert_eq!(profile.instance_type, InstanceType::Game);
    assert_eq!(profile.steam_app_id(), Some(892970));
    assert!(!profile.install_rules.is_empty());

    let plugins = profile
      .install_rules
      .iter()
      .find(|r| r.route == "BepInEx/plugins")
      .expect("plugins rule");

    assert_eq!(plugins.tracking_method, TrackingMethod::Subdir);
    assert!(plugins.tracking_method.is_namespaced());

    let monomod = profile
      .install_rules
      .iter()
      .find(|r| r.route == "BepInEx/monomod")
      .expect("monomod rule");

    assert!(
      monomod
        .default_file_extensions
        .iter()
        .any(|e| e == ".mm.dll")
    );

    let config = profile
      .install_rules
      .iter()
      .find(|r| r.route == "BepInEx/config")
      .expect("config rule");

    assert!(!config.tracking_method.is_namespaced());
  }

  #[test]
  fn test_modloader_registry_lookup() {
    let eco = Ecosystem::bundled();

    let pack = eco
      .modloader_package("denikson-BepInExPack_Valheim")
      .expect("valheim loader pack present");

    assert_eq!(pack.root_folder, "BepInExPack_Valheim");
    assert_eq!(pack.loader, LoaderKind::BepInEx);

    assert!(eco.modloader_package("Author-SomeMod").is_none());
  }

  #[test]
  fn test_melonloader_game_present() {
    let eco = Ecosystem::bundled();

    let game = eco
      .games()
      .find(|g| {
        g.profile()
          .map(|p| p.package_loader == LoaderKind::MelonLoader)
          .unwrap_or(false)
      })
      .expect("at least one MelonLoader game");

    assert!(game.profile().is_some());
  }

  #[test]
  fn test_unknown_loader_falls_back_to_other() {
    let json = r#"{
      "schemaVersion": "9.9.9",
      "games": {
        "x": { "label": "x", "meta": {"displayName": "X"},
               "r2modman": [ { "packageLoader": "someNewLoader", "installRules": [] } ] }
      },
      "modloaderPackages": []
    }"#;

    let eco = Ecosystem::from_json(json).unwrap();
    let profile = eco.game("x").unwrap().profile().unwrap();

    assert_eq!(profile.package_loader, LoaderKind::Other);
  }

  #[test]
  fn test_null_relative_file_exclusions_normalizes_to_empty() {
    let json = r#"{
      "games": {
        "x": { "label": "x",
               "r2modman": [ { "packageLoader": "bepinex", "relativeFileExclusions": null, "installRules": [] } ] }
      }
    }"#;

    let eco = Ecosystem::from_json(json).unwrap();
    let profile = eco.game("x").unwrap().profile().unwrap();

    assert!(profile.relative_file_exclusions.is_empty());
  }

  #[test]
  fn test_null_enum_and_bool_fields_normalize_to_defaults() {
    let json = r#"{
      "games": {
        "x": { "label": "x",
               "r2modman": [ { "packageLoader": null, "gameInstanceType": null,
                 "installRules": [ { "route": "Mods", "trackingMethod": null, "isDefaultLocation": null } ] } ] }
      }
    }"#;

    let eco = Ecosystem::from_json(json).unwrap();
    let profile = eco.game("x").unwrap().profile().unwrap();

    assert_eq!(profile.package_loader, LoaderKind::Other);
    assert_eq!(profile.instance_type, InstanceType::Game);

    let rule = &profile.install_rules[0];

    assert_eq!(rule.tracking_method, TrackingMethod::Subdir);
    assert!(!rule.is_default_location);
  }

  #[test]
  fn test_unknown_tracking_method_falls_back_to_other() {
    let json = r#"{
      "games": {
        "x": { "label": "x",
               "r2modman": [ { "packageLoader": "bepinex",
                 "installRules": [ { "route": "Mods", "trackingMethod": "brand-new-method" } ] } ] }
      }
    }"#;

    let eco = Ecosystem::from_json(json).unwrap();
    let rule = &eco.game("x").unwrap().profile().unwrap().install_rules[0];

    assert_eq!(rule.tracking_method, TrackingMethod::Other);
    assert!(!rule.tracking_method.is_namespaced());
  }

  #[test]
  fn test_fetch_latest_rejects_declared_oversize_body() {
    let mut server = mockito::Server::new();
    let body = "x".repeat(1000);

    let mock = server
      .mock("GET", "/schema")
      .with_status(200)
      .with_body(&body)
      .create();

    let url = format!("{}/schema", server.url());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let client = reqwest::Client::new();

    let result = rt.block_on(Ecosystem::fetch_latest_bounded(
      &client,
      &url,
      16,
      &RetryPolicy::none(),
    ));

    assert!(matches!(result, Err(Error::SizeLimitExceeded { .. })));

    mock.assert();
  }

  #[test]
  fn test_fetch_latest_rejects_streamed_oversize_body() {
    let mut server = mockito::Server::new();

    // A chunked body carries no Content-Length, so the cap must be enforced from
    // the actual streamed bytes.
    let mock = server
      .mock("GET", "/schema")
      .with_status(200)
      .with_chunked_body(|w| w.write_all(&vec![b'x'; 1000]))
      .create();

    let url = format!("{}/schema", server.url());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let client = reqwest::Client::new();

    let result = rt.block_on(Ecosystem::fetch_latest_bounded(
      &client,
      &url,
      16,
      &RetryPolicy::none(),
    ));

    assert!(matches!(result, Err(Error::SizeLimitExceeded { .. })));

    mock.assert();
  }

  #[test]
  fn test_fetch_latest_parses_network_schema() {
    let mut server = mockito::Server::new();

    let body = r#"{
      "schemaVersion": "0.3.0",
      "games": {
        "valheim": { "label": "valheim", "meta": {"displayName": "Valheim"},
          "r2modman": [ { "packageLoader": "bepinex",
            "installRules": [ { "route": "BepInEx/plugins", "trackingMethod": "subdir",
              "isDefaultLocation": true, "defaultFileExtensions": [".dll"] } ] } ] }
      },
      "modloaderPackages": [
        { "packageId": "denikson-BepInExPack_Valheim", "rootFolder": "BepInExPack_Valheim", "loader": "bepinex" }
      ]
    }"#;

    let mock = server
      .mock("GET", "/schema")
      .with_status(200)
      .with_header("Content-Type", "application/json")
      .with_body(body)
      .create();

    let url = format!("{}/schema", server.url());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let client = reqwest::Client::new();
    let eco = rt.block_on(Ecosystem::fetch_latest(&client, &url)).unwrap();

    assert_eq!(eco.schema_version, "0.3.0");

    let profile = eco.game("valheim").unwrap().profile().unwrap();

    assert_eq!(profile.package_loader, LoaderKind::BepInEx);
    assert!(
      eco
        .modloader_package("denikson-BepInExPack_Valheim")
        .is_some()
    );

    mock.assert();
  }

  #[test]
  fn steam_app_id_none_when_missing_or_unparseable() {
    // No steam distribution => None.
    let no_steam: GameProfile = serde_json::from_str(
      r#"{"distributions":[{"platform":"epic-games-store","identifier":"abc"}]}"#,
    )
    .unwrap();

    assert_eq!(no_steam.steam_app_id(), None);

    // A steam distribution whose id is not numeric => None.
    let bad_steam: GameProfile =
      serde_json::from_str(r#"{"distributions":[{"platform":"steam","identifier":"nope"}]}"#)
        .unwrap();

    assert_eq!(bad_steam.steam_app_id(), None);

    // A steam distribution with a numeric id => Some.
    let steam: GameProfile =
      serde_json::from_str(r#"{"distributions":[{"platform":"steam","identifier":"892970"}]}"#)
        .unwrap();

    assert_eq!(steam.steam_app_id(), Some(892970));
  }

  #[test]
  fn game_profile_prefers_game_instance_then_falls_back() {
    // Only a server-instance profile: `profile()` falls back to the first one.
    let server_only: GameDef =
      serde_json::from_str(r#"{"r2modman":[{"gameInstanceType":"server"}]}"#).unwrap();

    assert_eq!(
      server_only.profile().unwrap().instance_type,
      InstanceType::Server
    );

    // A game-instance profile is preferred over a server one, whatever the order.
    let mixed: GameDef = serde_json::from_str(
      r#"{"r2modman":[{"gameInstanceType":"server"},{"gameInstanceType":"game"}]}"#,
    )
    .unwrap();

    assert_eq!(mixed.profile().unwrap().instance_type, InstanceType::Game);

    // No profiles at all => None.
    let empty: GameDef = serde_json::from_str(r#"{"r2modman":[]}"#).unwrap();

    assert!(empty.profile().is_none());
  }
}
