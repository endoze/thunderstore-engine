//! The Thunderstore ecosystem-schema: games, mod loaders, and install rules.
//!
//! This models, in normalized form, the per-game data that drives install
//! routing: which mod loader a community uses, the loader's proxy DLL and own
//! Thunderstore package, and the set of override subdirectories that route files
//! into a profile/loader root.
//!
//! The canonical, drifting source is the Thunderstore ecosystem-schema
//! (<https://github.com/thunderstore-io/ecosystem-schema>, published as JSON to
//! the Thunderstore API). The loader/subdir modeling here mirrors the approach
//! proven in Gale (<https://github.com/Kesomannen/gale>,
//! `src-tauri/src/game/mod_loader.rs`). A normalized snapshot is embedded as a
//! bundled fallback; [`Ecosystem::fetch_latest`] refreshes it from the network.
//! Parsing is intentionally defensive (`#[serde(default)]`, unknown loaders fall
//! back to [`LoaderKind::Other`]) because this is an external contract.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Normalized ecosystem snapshot embedded at compile time as a fallback.
const BUNDLED_ECOSYSTEM: &str = include_str!("../assets/ecosystem.json");

/// The default URL used by [`Ecosystem::fetch_latest`] to refresh the snapshot.
pub const DEFAULT_ECOSYSTEM_URL: &str =
  "https://raw.githubusercontent.com/endoze/thunderstore-engine/master/assets/ecosystem.json";

/// A parsed ecosystem schema: the set of known games and their loaders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ecosystem {
  /// All known games, keyed in [`Ecosystem::game`] by their community slug.
  pub games: Vec<GameDef>,
}

impl Ecosystem {
  /// Loads the bundled ecosystem snapshot embedded in the binary.
  pub fn bundled() -> Self {
    serde_json::from_str(BUNDLED_ECOSYSTEM)
      .expect("bundled ecosystem.json must be valid; this is a build-time invariant")
  }

  /// Parses an ecosystem snapshot from a JSON string.
  pub fn from_json(json: &str) -> Result<Self> {
    serde_json::from_str(json).map_err(|e| Error::Ecosystem(format!("Failed to parse: {}", e)))
  }

  /// Fetches the latest ecosystem snapshot from `url` and parses it.
  ///
  /// Use [`DEFAULT_ECOSYSTEM_URL`] for the default location.
  pub async fn fetch_latest(client: &reqwest::Client, url: &str) -> Result<Self> {
    let body = client.get(url).send().await?.text().await?;

    Self::from_json(&body)
  }

  /// Returns the game definition for a community slug, if known.
  pub fn game(&self, slug: &str) -> Option<&GameDef> {
    self.games.iter().find(|g| g.slug == slug)
  }

  /// Iterates over all known games.
  pub fn games(&self) -> impl Iterator<Item = &GameDef> {
    self.games.iter()
  }

  /// Builds a slug -> index lookup for repeated queries.
  pub fn slug_index(&self) -> HashMap<String, usize> {
    self
      .games
      .iter()
      .enumerate()
      .map(|(idx, g)| (g.slug.clone(), idx))
      .collect()
  }
}

/// A single game's definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameDef {
  /// The Thunderstore community slug, e.g. `"valheim"`.
  pub slug: String,
  /// Human-readable name, e.g. `"Valheim"`.
  pub display_name: String,
  /// The mod loader this game uses.
  pub mod_loader: ModLoader,
  /// Steam application id, when distributed on Steam.
  #[serde(default)]
  pub steam_app_id: Option<u32>,
  /// Epic Games Store id, when distributed on EGS.
  #[serde(default)]
  pub epic_id: Option<String>,
  /// Candidate executable names for game-directory validation.
  #[serde(default)]
  pub exe_names: Vec<String>,
  /// Optional game data subdirectory (loader-specific).
  #[serde(default)]
  pub data_subdir: Option<String>,
}

/// A mod loader and its install-routing configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModLoader {
  /// The loader's own Thunderstore package full name (used to detect the loader
  /// pack so it unpacks to the root rather than being routed as a plugin).
  #[serde(default)]
  pub package_name: Option<String>,
  /// The proxy DLL placed next to the game executable, e.g. `winhttp.dll`.
  #[serde(default)]
  pub file_target: Option<String>,
  /// The loader family.
  pub kind: LoaderKind,
  /// Override subdirectories that route files into the root.
  #[serde(default)]
  pub subdirs: Vec<Subdir>,
  /// Name of the subdir (in `subdirs`) that catches loose/unmatched files.
  #[serde(default)]
  pub default_subdir: Option<String>,
  /// File names ignored when installing a regular mod (e.g. `manifest.json`).
  #[serde(default)]
  pub ignored_files: Vec<String>,
  /// Subfolder within the loader's own package whose contents unpack to the
  /// root. Many loader packs nest their payload (e.g. the BepInEx pack ships a
  /// `BepInExPack_Valheim/` folder containing `winhttp.dll` + `BepInEx/`); set
  /// this so that folder's *contents* land at the root rather than the folder
  /// itself. `None` copies the package root directly.
  #[serde(default)]
  pub loader_root: Option<String>,
}

impl ModLoader {
  /// Returns the subdir designated as the default, if configured and present.
  pub fn default_subdir(&self) -> Option<&Subdir> {
    let name = self.default_subdir.as_deref()?;

    self.subdirs.iter().find(|s| s.name == name)
  }

  /// Finds the override subdir whose `name` equals `name` (case-insensitive).
  pub fn subdir_named(&self, name: &str) -> Option<&Subdir> {
    self
      .subdirs
      .iter()
      .find(|s| s.name.eq_ignore_ascii_case(name))
  }

  /// Determines whether `package_full_name` is this loader's own package.
  ///
  /// The loader pack unpacks to the root (proxy DLL + loader core) rather than
  /// being routed as a regular mod. Mirrors Gale's `is_loader_package`.
  pub fn is_loader_package(&self, package_full_name: &str) -> bool {
    if let Some(pkg) = &self.package_name {
      return package_full_name == pkg;
    }

    match self.kind {
      LoaderKind::BepInEx => {
        package_full_name.starts_with("BepInEx-BepInExPack")
          || package_full_name.starts_with("denikson-BepInExPack")
      }
      LoaderKind::MelonLoader => package_full_name == "LavaGang-MelonLoader",
      LoaderKind::GdWeave => package_full_name == "NotNet-GDWeave",
      LoaderKind::Northstar => package_full_name == "northstar-Northstar",
      LoaderKind::Shimloader => package_full_name == "Thunderstore-unreal_shimloader",
      LoaderKind::Lovely => package_full_name == "Thunderstore-lovely",
      LoaderKind::ReturnOfModding => package_full_name == "ReturnOfModding-ReturnOfModding",
      LoaderKind::Other => false,
    }
  }
}

/// The mod-loader family. Unknown values deserialize to [`LoaderKind::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoaderKind {
  BepInEx,
  MelonLoader,
  GdWeave,
  Northstar,
  Shimloader,
  Lovely,
  ReturnOfModding,
  #[serde(other)]
  Other,
}

/// How files from an override subdirectory are placed under its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubdirMode {
  /// Namespace files under `target/<Author-ModName>/` (the default).
  #[default]
  Separated,
  /// Place files directly under `target/` without namespacing.
  Flat,
  /// Namespaced and tracked for uninstall (treated like `Separated` for routing).
  Tracked,
  /// Place files directly under `target/`, not namespaced (e.g. shared config).
  Untracked,
}

impl SubdirMode {
  /// Whether files routed via this mode are namespaced under `<Author-ModName>/`.
  pub fn is_namespaced(self) -> bool {
    matches!(self, SubdirMode::Separated | SubdirMode::Tracked)
  }
}

/// An override subdirectory routing rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subdir {
  /// The override-folder name as it appears at the package zip root, e.g.
  /// `"plugins"`.
  pub name: String,
  /// The destination relative to the profile/loader root, e.g.
  /// `"BepInEx/plugins"`.
  pub target: String,
  /// How files are placed under `target`.
  #[serde(default)]
  pub mode: SubdirMode,
  /// If set, route loose files whose name ends with this extension here, e.g.
  /// `".mm.dll"`.
  #[serde(default)]
  pub extension: Option<String>,
  /// Whether this target holds user-mutable files (e.g. config).
  #[serde(default)]
  pub mutable: bool,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_bundled_parses() {
    let eco = Ecosystem::bundled();

    assert!(!eco.games.is_empty());
  }

  #[test]
  fn test_valheim_bepinex() {
    let eco = Ecosystem::bundled();
    let valheim = eco.game("valheim").expect("valheim present");

    assert_eq!(valheim.mod_loader.kind, LoaderKind::BepInEx);
    assert_eq!(valheim.steam_app_id, Some(892970));
    assert_eq!(
      valheim.mod_loader.file_target.as_deref(),
      Some("winhttp.dll")
    );

    let plugins = valheim.mod_loader.subdir_named("plugins").unwrap();
    assert_eq!(plugins.target, "BepInEx/plugins");
    assert!(plugins.mode.is_namespaced());

    let monomod = valheim.mod_loader.subdir_named("monomod").unwrap();
    assert_eq!(monomod.extension.as_deref(), Some(".mm.dll"));

    let config = valheim.mod_loader.subdir_named("config").unwrap();
    assert!(!config.mode.is_namespaced());
  }

  #[test]
  fn test_a_melonloader_game() {
    let eco = Ecosystem::bundled();
    let game = eco
      .games()
      .find(|g| g.mod_loader.kind == LoaderKind::MelonLoader)
      .expect("at least one MelonLoader game");

    assert!(game.mod_loader.subdir_named("Mods").is_some());
  }

  #[test]
  fn test_unknown_loader_falls_back_to_other() {
    let json =
      r#"{"games":[{"slug":"x","display_name":"X","mod_loader":{"kind":"someNewLoader"}}]}"#;
    let eco = Ecosystem::from_json(json).unwrap();

    assert_eq!(eco.game("x").unwrap().mod_loader.kind, LoaderKind::Other);
  }

  #[test]
  fn test_loader_package_detection() {
    let eco = Ecosystem::bundled();
    let valheim = eco.game("valheim").unwrap();

    assert!(
      valheim
        .mod_loader
        .is_loader_package("denikson-BepInExPack_Valheim")
    );
    assert!(!valheim.mod_loader.is_loader_package("Author-SomeMod"));
  }
}
