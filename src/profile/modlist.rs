//! The `mods.yml` model and its read/write.
//!
//! `mods.yml` is a YAML array of [`ProfileMod`] and is the authoritative record
//! of what is installed in a profile. The shape mirrors r2modman's `ManifestV2`
//! so r2modman can read a profile this engine produced: keys are camelCase, the
//! `icon` field is never written, and `versionNumber` is a nested object.

#![deny(missing_docs)]

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// The `major.minor.patch` version object, serialized as a nested mapping to
/// match r2modman's `VersionNumber`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemVer {
  /// The major component, or `0` if the source string had none.
  pub major: u32,
  /// The minor component, or `0` if the source string had none.
  pub minor: u32,
  /// The patch component, or `0` if the source string had none. A prerelease
  /// suffix is discarded, so `1.2.3-beta` parses to a patch of `3`.
  pub patch: u32,
}

impl SemVer {
  /// Parses the first three dot-separated integer components of `s`; a missing or
  /// non-numeric component becomes `0` (so `"1.2"` -> `1.2.0`, `"1.2.3-beta"` ->
  /// `1.2.3`).
  ///
  /// Never fails and never panics: an unparseable or overflowing component
  /// degrades to `0` rather than rejecting a `mods.yml` r2modman itself accepts.
  ///
  /// # Examples
  ///
  /// ```
  /// use thunderstore_engine::profile::modlist::SemVer;
  ///
  /// assert_eq!(SemVer::parse("1.2.3").to_string(), "1.2.3");
  /// assert_eq!(SemVer::parse("1.2").to_string(), "1.2.0");
  /// assert_eq!(SemVer::parse("1.2.3-beta").to_string(), "1.2.3");
  /// assert_eq!(SemVer::parse("not-a-version").to_string(), "0.0.0");
  /// ```
  pub fn parse(s: &str) -> SemVer {
    let mut parts = s.split('.');
    let major = parse_leading_u32(parts.next());
    let minor = parse_leading_u32(parts.next());
    let patch = parse_leading_u32(parts.next());

    SemVer {
      major,
      minor,
      patch,
    }
  }
}

impl std::fmt::Display for SemVer {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
  }
}

/// Parses the leading run of ASCII digits of an optional component into a `u32`,
/// yielding `0` for `None`, an empty run, or overflow. This tolerates a
/// prerelease suffix (`"3-beta"` -> `3`) the way r2modman's parser does.
fn parse_leading_u32(component: Option<&str>) -> u32 {
  let text = component.unwrap_or("");
  let digits: String = text.chars().take_while(|c| c.is_ascii_digit()).collect();

  digits.parse().unwrap_or(0)
}

/// One installed mod, mirroring r2modman's `ManifestV2`. Field order matches
/// r2modman's declaration order; `icon` is never serialized.
///
/// `#[non_exhaustive]`: fields may be added as the schema grows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ProfileMod {
  /// The `manifest.json` schema version the package declared.
  pub manifest_version: u32,
  /// The package name, without its owner prefix.
  pub name: String,
  /// The package owner, as Thunderstore spells it.
  pub author_name: String,
  /// The package's homepage, or empty when it declared none.
  pub website_url: String,
  /// The name to show a user, which may differ from [`Self::name`].
  pub display_name: String,
  /// The package's own description, shown verbatim. Not validated or truncated,
  /// so a UI must treat it as untrusted text from an arbitrary uploader.
  pub description: String,
  /// The game version the package declared, carried through verbatim.
  pub game_version: String,
  /// r2modman's network-mode marker, carried through verbatim.
  pub network_mode: String,
  /// r2modman's package-type marker, carried through verbatim.
  pub package_type: String,
  /// r2modman's install-mode marker, carried through verbatim.
  pub install_mode: String,
  /// When the mod was installed, as a Unix timestamp in milliseconds.
  pub installed_at_time: u64,
  /// Mod loaders the package declared support for.
  pub loaders: Vec<String>,
  /// Full `Owner-Name-Version` dependency strings.
  pub dependencies: Vec<String>,
  /// Packages this one declares it cannot coexist with.
  pub incompatibilities: Vec<String>,
  /// Dependencies the package treats as optional.
  pub optional_dependencies: Vec<String>,
  /// The installed version.
  pub version_number: SemVer,
  /// Whether the mod is currently enabled in the profile.
  pub enabled: bool,
  /// Never written to `mods.yml` (r2modman strips it); ignored on read.
  #[serde(skip)]
  pub icon: String,
  /// Whether the mod came from Thunderstore rather than a local import.
  pub online_source: bool,
  /// Absent from the `mods.yml` that installed r2modmanPlus versions actually
  /// write, so it must not be required: demanding it made importing a real
  /// profile fail with `missing field `trustedPackage`` *after* every file had
  /// already been copied into place.
  ///
  /// Defaulting to `false` cannot change behavior, because nothing in the engine
  /// branches on this flag; it is carried purely so a round-trip through
  /// [`read()`]/[`write()`] does not drop a key a newer r2modman does write. `false`
  /// is also the conservative reading of "is this package trusted".
  #[serde(default)]
  pub trusted_package: bool,
}

/// The `mods.yml` filename within a profile directory.
const MODS_YML: &str = "mods.yml";

/// Reads `<profile_dir>/mods.yml`. A missing file is an empty list (a fresh
/// profile), matching r2modman treating an absent list as `[]`.
///
/// A read or parse failure names the file in its message, so a caller does not
/// need to prepend the path itself.
pub fn read(profile_dir: &Path) -> Result<Vec<ProfileMod>> {
  let path = profile_dir.join(MODS_YML);

  if !path.exists() {
    return Ok(Vec::new());
  }

  let text = fs::read_to_string(&path)
    .map_err(|e| Error::Profile(format!("reading {}: {}", path.display(), e)))?;

  serde_yaml_ng::from_str(&text)
    .map_err(|e| Error::Profile(format!("parsing {}: {}", path.display(), e)))
}

/// Writes `mods` to `<profile_dir>/mods.yml`, creating the directory if needed.
pub fn write(profile_dir: &Path, mods: &[ProfileMod]) -> Result<()> {
  fs::create_dir_all(profile_dir)
    .map_err(|e| Error::Profile(format!("creating {}: {}", profile_dir.display(), e)))?;

  let text = serde_yaml_ng::to_string(mods)
    .map_err(|e| Error::Profile(format!("serializing mods.yml: {}", e)))?;

  let path = profile_dir.join(MODS_YML);

  fs::write(&path, text).map_err(|e| Error::Profile(format!("writing {}: {}", path.display(), e)))
}

/// Finds an installed mod by its `Owner-Name` identifier.
pub fn find<'a>(mods: &'a [ProfileMod], full_name: &str) -> Option<&'a ProfileMod> {
  mods.iter().find(|m| m.name == full_name)
}

/// Inserts `entry`, replacing any existing entry with the same `name`.
pub fn upsert(mods: &mut Vec<ProfileMod>, entry: ProfileMod) {
  if let Some(existing) = mods.iter_mut().find(|m| m.name == entry.name) {
    *existing = entry;

    return;
  }

  mods.push(entry);
}

/// The current time in the epoch-millisecond encoding [`ProfileMod::installed_at_time`]
/// records, matching r2modman's `installedAtTime`.
///
/// A clock reading before the Unix epoch (a misconfigured system clock) falls
/// back to `0` rather than failing an install over it: nothing in `mods.yml`
/// or the install pipeline treats this timestamp as anything but display
/// metadata, so there is no correctness reason to refuse the install.
pub fn now_millis() -> u64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|elapsed| elapsed.as_millis() as u64)
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::tempdir;

  fn sample() -> ProfileMod {
    ProfileMod {
      manifest_version: 1,
      name: "bbepis-BepInExPack".to_string(),
      author_name: "bbepis".to_string(),
      website_url: "https://example.com".to_string(),
      display_name: "BepInExPack".to_string(),
      description: "pack".to_string(),
      game_version: "0".to_string(),
      network_mode: "both".to_string(),
      package_type: "other".to_string(),
      install_mode: "managed".to_string(),
      installed_at_time: 1_699_999_999_999,
      loaders: vec![],
      dependencies: vec![],
      incompatibilities: vec![],
      optional_dependencies: vec![],
      version_number: SemVer {
        major: 5,
        minor: 4,
        patch: 2100,
      },
      enabled: true,
      icon: String::new(),
      online_source: false,
      trusted_package: false,
    }
  }

  #[test]
  fn semver_parse_takes_first_three_numeric_components() {
    assert_eq!(
      SemVer::parse("5.4.2100"),
      SemVer {
        major: 5,
        minor: 4,
        patch: 2100
      }
    );
    assert_eq!(
      SemVer::parse("1.2"),
      SemVer {
        major: 1,
        minor: 2,
        patch: 0
      }
    );
    assert_eq!(
      SemVer::parse("1.2.3-beta"),
      SemVer {
        major: 1,
        minor: 2,
        patch: 3
      }
    );
    assert_eq!(
      SemVer::parse("garbage"),
      SemVer {
        major: 0,
        minor: 0,
        patch: 0
      }
    );
  }

  #[test]
  fn serialized_yaml_is_r2modman_readable() {
    let yaml = serde_yaml_ng::to_string(&vec![sample()]).unwrap();

    // camelCase keys, no `icon`, nested versionNumber.
    assert!(yaml.contains("manifestVersion:"));
    assert!(yaml.contains("name: bbepis-BepInExPack"));
    assert!(yaml.contains("versionNumber:"));
    assert!(yaml.contains("major: 5"));
    assert!(yaml.contains("networkMode: both"));
    assert!(!yaml.contains("icon"));
    assert!(!yaml.contains("version_number"));
  }

  #[test]
  fn read_write_round_trips_and_missing_is_empty() {
    let dir = tempdir().unwrap();

    assert_eq!(read(dir.path()).unwrap().len(), 0);

    write(dir.path(), &[sample()]).unwrap();
    let back = read(dir.path()).unwrap();

    assert_eq!(back.len(), 1);
    assert_eq!(back[0].name, "bbepis-BepInExPack");
    assert_eq!(
      back[0].version_number,
      SemVer {
        major: 5,
        minor: 4,
        patch: 2100
      }
    );
    assert!(back[0].enabled);
  }

  #[test]
  fn parses_real_r2modman_sample_ignoring_icon() {
    // A real-shaped entry, including an `icon` key r2modman would have stripped
    // but a hand-edited file might carry: it must be ignored, not rejected.
    let yaml = r#"- manifestVersion: 1
  name: Author-Mod
  authorName: Author
  websiteUrl: ''
  displayName: Mod
  description: ''
  gameVersion: '0'
  networkMode: both
  packageType: other
  installMode: managed
  installedAtTime: 1699999999999
  loaders: []
  dependencies:
    - Other-Dep-1.2.3
  incompatibilities: []
  optionalDependencies: []
  versionNumber:
    major: 1
    minor: 0
    patch: 0
  enabled: false
  icon: /some/path/icon.png
  onlineSource: true
  trustedPackage: false
"#;
    let mods: Vec<ProfileMod> = serde_yaml_ng::from_str(yaml).unwrap();

    assert_eq!(mods.len(), 1);
    assert_eq!(mods[0].dependencies, vec!["Other-Dep-1.2.3".to_string()]);
    assert!(!mods[0].enabled);
    assert!(mods[0].online_source);
  }

  /// The shape r2modman actually writes on disk, verbatim from an installed
  /// r2modmanPlus profile: no `trustedPackage` key at all. Requiring it made
  /// importing a real profile fail with `missing field `trustedPackage``, after
  /// the import had already copied every file into place.
  #[test]
  fn parses_an_r2modman_entry_with_no_trusted_package_key() {
    let yaml = r#"- manifestVersion: 1
  name: denikson-BepInExPack_Valheim
  authorName: denikson
  websiteUrl: https://thunderstore.io/c/valheim/p/denikson/BepInExPack_Valheim/
  displayName: BepInExPack_Valheim
  description: BepInEx pack for Valheim.
  gameVersion: '0'
  networkMode: both
  packageType: other
  installMode: managed
  installedAtTime: 1771107231154
  loaders: []
  dependencies: []
  incompatibilities: []
  optionalDependencies: []
  versionNumber:
    major: 5
    minor: 4
    patch: 2333
  enabled: true
  onlineSource: false
"#;

    let mods: Vec<ProfileMod> = serde_yaml_ng::from_str(yaml).unwrap();

    assert_eq!(mods.len(), 1);
    assert_eq!(mods[0].name, "denikson-BepInExPack_Valheim");
    assert!(mods[0].enabled);

    // Absent means false: the conservative reading of "is this package trusted",
    // and nothing in the engine branches on it, so a default cannot change behavior.
    assert!(!mods[0].trusted_package);
  }

  #[test]
  fn upsert_replaces_by_name_and_find_locates() {
    let mut mods = vec![sample()];
    let mut updated = sample();
    updated.enabled = false;

    upsert(&mut mods, updated);

    assert_eq!(mods.len(), 1);
    assert!(!find(&mods, "bbepis-BepInExPack").unwrap().enabled);
    assert!(find(&mods, "nope").is_none());
  }

  #[test]
  fn semver_renders_as_major_minor_patch() {
    let version = SemVer {
      major: 5,
      minor: 4,
      patch: 2200,
    };

    assert_eq!(version.to_string(), "5.4.2200");
    assert_eq!(format!("{version}"), "5.4.2200");
  }

  #[test]
  fn an_unreadable_record_names_its_path() {
    let dir = tempdir().unwrap();

    std::fs::write(dir.path().join("mods.yml"), "not: [valid").unwrap();

    let message = read(dir.path()).unwrap_err().to_string();

    assert!(
      message.contains(&dir.path().join("mods.yml").display().to_string()),
      "a read failure must name the file, so a caller need not add it; got: {message}"
    );
  }
}
