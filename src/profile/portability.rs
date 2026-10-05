//! r2modman-compatible profile portability: `.r2z` export/import and shareable
//! Thunderstore profile codes.
//!
//! A `.r2z` is a zip holding an `export.r2x` manifest (the mod list) plus the
//! target's config files. Note the manifest names the version field `version`,
//! while `mods.yml` names it `versionNumber` — same shape, different key.
//!
//! Every operation is target-directory generic (`*_in`), with an r2modman-layout
//! wrapper, so a game server can export its tracked mod set just as a desktop
//! client exports a named profile.
//!
//! **Import installs each mod's latest version, not the exported one.** The
//! manifest's `version` field is recorded and re-exported faithfully, but on
//! import every mod resolves through
//! [`crate::models::DependencyGraph::resolve`], which is version-agnostic: the
//! version suffix is discarded and the *latest* available version is
//! installed instead. There is no version pinning, so a recipient importing an
//! export pinned to `version: 1.2.3` may receive `2.0.0` installed, with no
//! warning.

#![deny(missing_docs)]

use crate::client::ThunderstoreClient;
use crate::error::{Error, Result};
use crate::profile::modlist::{ProfileMod, SemVer};
use crate::util::{canonicalize_missing, same_file};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use zip::write::{FileOptions, ZipWriter};

/// One entry of an `export.r2x` mod list.
///
/// `#[non_exhaustive]`: fields may be added as the format grows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ExportMod {
  /// The `Owner-Name` identifier.
  pub name: String,
  /// The version. Note: `version`, not `versionNumber` as in `mods.yml`.
  pub version: SemVer,
  /// Whether the mod was enabled. Absent upstream means enabled.
  #[serde(default = "default_true")]
  pub enabled: bool,
}

/// serde default for [`ExportMod::enabled`]: r2modman treats an absent `enabled`
/// as `true`.
fn default_true() -> bool {
  true
}

/// The `export.r2x` manifest inside a `.r2z`.
///
/// `#[non_exhaustive]`: fields may be added as the format grows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ExportFormat {
  /// The exported profile's display name.
  pub profile_name: String,
  /// The exported mod list.
  pub mods: Vec<ExportMod>,
}

/// Builds an [`ExportFormat`] from a target's `mods.yml` entries.
pub fn export_format_from_mods(profile_name: &str, mods: &[ProfileMod]) -> ExportFormat {
  let mods = mods
    .iter()
    .map(|entry| ExportMod {
      name: entry.name.clone(),
      version: entry.version_number,
      enabled: entry.enabled,
    })
    .collect();

  ExportFormat {
    profile_name: profile_name.to_string(),
    mods,
  }
}

/// Serializes an [`ExportFormat`] to `export.r2x` YAML.
pub fn export_r2x_to_string(export: &ExportFormat) -> Result<String> {
  serde_yaml_ng::to_string(export)
    .map_err(|e| Error::Profile(format!("serializing export.r2x: {e}")))
}

/// Parses `export.r2x` YAML.
pub fn parse_export_r2x(yaml: &str) -> Result<ExportFormat> {
  serde_yaml_ng::from_str(yaml).map_err(|e| Error::Profile(format!("parsing export.r2x: {e}")))
}

/// Filename suffixes r2modman treats as config files worth exporting.
const CONFIG_EXTENSIONS: [&str; 6] = [".cfg", ".txt", ".json", ".yml", ".yaml", ".ini"];

/// Whether a target-relative path is excluded from an export.
///
/// Mirrors r2modman's export prune list: manager state (`_state/`, `mods.yml`),
/// the runtime it never ships (`dotnet/`, `MelonLoader/`, `GDWeave/core|mods`),
/// per-mod package metadata, and `BepInEx/config/` (added separately under the
/// zip's `config/` folder). Pure — no filesystem access.
fn is_pruned(rel: &Path) -> bool {
  let normalized = rel.to_string_lossy().replace('\\', "/");

  let prefixes = [
    "dotnet/",
    "_state/",
    "MelonLoader/",
    "BepInEx/config/",
    "GDWeave/core/",
    "GDWeave/mods/",
  ];

  if prefixes.iter().any(|p| normalized.starts_with(p)) {
    return true;
  }

  if normalized == "mods.yml" || normalized == "GDWeave/GDWeave.log" {
    return true;
  }

  is_plugin_manifest(&normalized)
}

/// Whether a path is a per-mod `BepInEx/plugins/<Owner-Name>/manifest.json`,
/// which r2modman strips from an export.
fn is_plugin_manifest(normalized: &str) -> bool {
  let parts: Vec<&str> = normalized.split('/').collect();

  parts.len() == 4
    && parts[0] == "BepInEx"
    && parts[1] == "plugins"
    && parts[3].eq_ignore_ascii_case("manifest.json")
}

/// Whether a filename ends with one of [`CONFIG_EXTENSIONS`] (case-insensitive).
/// Pure — no filesystem access.
fn is_config_file(rel: &Path) -> bool {
  let name = match rel.file_name().and_then(|n| n.to_str()) {
    Some(name) => name.to_ascii_lowercase(),
    None => return false,
  };

  CONFIG_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
}

/// Collects the target-relative paths an export should carry: config files that
/// survive the prune list, sorted for a deterministic archive.
fn collect_export_paths(target_dir: &Path) -> Result<Vec<PathBuf>> {
  let mut found = Vec::new();

  collect_files(target_dir, target_dir, &mut found)?;

  let mut kept: Vec<PathBuf> = found
    .into_iter()
    .filter(|rel| is_config_file(rel) && !is_pruned(rel))
    .collect();

  kept.sort();

  Ok(kept)
}

/// Recursively collects file paths under `dir`, relative to `root`, skipping
/// symlinks so an export cannot escape the target (mirrors `install::walk_files`).
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
  if !dir.is_dir() {
    return Ok(());
  }

  for entry in
    fs::read_dir(dir).map_err(|e| Error::Profile(format!("reading {}: {}", dir.display(), e)))?
  {
    let entry = entry.map_err(|e| Error::Profile(format!("reading an entry: {e}")))?;
    let path = entry.path();
    let metadata = fs::symlink_metadata(&path)
      .map_err(|e| Error::Profile(format!("inspecting {}: {}", path.display(), e)))?;

    if metadata.file_type().is_symlink() {
      continue;
    }

    if metadata.is_dir() {
      collect_files(root, &path, out)?;
    } else if let Ok(rel) = path.strip_prefix(root) {
      out.push(rel.to_path_buf());
    }
  }

  Ok(())
}

/// The manifest filename inside a `.r2z`.
const EXPORT_MANIFEST: &str = "export.r2x";

/// The archive folder that a target's `BepInEx/config/` tree is re-rooted into.
const ARCHIVE_CONFIG_DIR: &str = "config";

/// The target-relative path of the shared BepInEx config tree.
const BEPINEX_CONFIG_DIR: &str = "BepInEx/config";

/// Builds the `.r2z` bytes for `target_dir`.
///
/// The archive carries `EXPORT_MANIFEST` (the mod list from `mods.yml`), the
/// target's `BepInEx/config/` tree re-rooted under `config/`, and every other
/// surviving config file at its target-relative path. Binaries and manager state
/// are never exported (see `is_pruned`).
///
/// `profile_name` is recorded in the manifest as the exported profile's name and
/// need not match any directory on disk.
///
/// # Errors
///
/// - [`Error::Profile`] if `target_dir` has no readable
///   `mods.yml`, or the mod list cannot be serialized.
/// - [`Error::Io`] if a config file cannot be read.
/// - [`Error::Zip`] if the archive cannot be written.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use thunderstore_engine::profile::portability::build_r2z_in;
///
/// let bytes = build_r2z_in(Path::new("/srv/valheim"), "my-profile")?;
///
/// std::fs::write("my-profile.r2z", &bytes)?;
/// # Ok::<(), thunderstore_engine::Error>(())
/// ```
pub fn build_r2z_in(target_dir: &Path, profile_name: &str) -> Result<Vec<u8>> {
  let mods = crate::profile::modlist::read(target_dir)?;
  let manifest = export_r2x_to_string(&export_format_from_mods(profile_name, &mods))?;

  let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
  let options: FileOptions<'_, ()> =
    FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

  zip
    .start_file(EXPORT_MANIFEST, options)
    .map_err(|e| Error::Profile(format!("writing {EXPORT_MANIFEST}: {e}")))?;
  zip
    .write_all(manifest.as_bytes())
    .map_err(|e| Error::Profile(format!("writing {EXPORT_MANIFEST}: {e}")))?;

  let config_root = target_dir.join(BEPINEX_CONFIG_DIR);
  let mut shared = Vec::new();

  collect_files(&config_root, &config_root, &mut shared)?;
  shared.sort();

  for rel in shared {
    let archive_path = format!(
      "{ARCHIVE_CONFIG_DIR}/{}",
      rel.to_string_lossy().replace('\\', "/")
    );

    add_zip_file(&mut zip, options, &archive_path, &config_root.join(&rel))?;
  }

  for rel in collect_export_paths(target_dir)? {
    let archive_path = rel.to_string_lossy().replace('\\', "/");

    add_zip_file(&mut zip, options, &archive_path, &target_dir.join(&rel))?;
  }

  let cursor = zip
    .finish()
    .map_err(|e| Error::Profile(format!("finishing export archive: {e}")))?;

  Ok(cursor.into_inner())
}

/// Adds one file to the archive under `archive_path`.
fn add_zip_file(
  zip: &mut ZipWriter<Cursor<Vec<u8>>>,
  options: FileOptions<'_, ()>,
  archive_path: &str,
  source: &Path,
) -> Result<()> {
  let bytes =
    fs::read(source).map_err(|e| Error::Profile(format!("reading {}: {}", source.display(), e)))?;

  zip
    .start_file(archive_path, options)
    .map_err(|e| Error::Profile(format!("writing {archive_path}: {e}")))?;
  zip
    .write_all(&bytes)
    .map_err(|e| Error::Profile(format!("writing {archive_path}: {e}")))?;

  Ok(())
}

/// Writes `target_dir`'s export to `<out_dir>/<profile_name>_<timestamp_millis>.r2z`
/// and returns the path. `timestamp_millis` is supplied by the caller so the
/// filename is deterministic and testable. `profile_name` is validated (see
/// [`crate::profile::layout::validate_name`]) because it becomes part of the
/// output filename; an unchecked name could otherwise write outside `out_dir`.
pub fn export_to_file_in(
  target_dir: &Path,
  profile_name: &str,
  out_dir: &Path,
  timestamp_millis: u64,
) -> Result<PathBuf> {
  crate::profile::layout::validate_name(profile_name)?;

  let bytes = build_r2z_in(target_dir, profile_name)?;

  fs::create_dir_all(out_dir)
    .map_err(|e| Error::Profile(format!("creating {}: {}", out_dir.display(), e)))?;

  let path = out_dir.join(format!("{profile_name}_{timestamp_millis}.r2z"));

  fs::write(&path, bytes)
    .map_err(|e| Error::Profile(format!("writing {}: {}", path.display(), e)))?;

  Ok(path)
}

/// Exports an r2modman-layout profile into `<base>/<game>/exports/`.
/// See [`export_to_file_in`].
pub fn export_to_file(
  base: &Path,
  game: &str,
  profile_name: &str,
  timestamp_millis: u64,
) -> Result<PathBuf> {
  export_to_file_in(
    &crate::profile::layout::profile_dir(base, game, profile_name),
    profile_name,
    &base.join(game).join("exports"),
    timestamp_millis,
  )
}

/// Upstream ceiling on a shared profile's zip, before base64.
const MAX_EXPORT_CODE_BYTES: usize = 20_000_000;

/// The literal prefix Thunderstore's profile-code payload carries.
const PROFILE_DATA_PREFIX: &str = "#r2modman";

/// Wraps `.r2z` bytes in the profile-code payload: the `#r2modman` prefix, a
/// newline, then base64 of the zip. Rejects a zip above the upstream 20 MB cap
/// before any upload is attempted.
fn encode_profile_payload(zip_bytes: &[u8]) -> Result<Vec<u8>> {
  if zip_bytes.len() > MAX_EXPORT_CODE_BYTES {
    return Err(Error::SizeLimitExceeded {
      context: "profile export".to_string(),
      limit: MAX_EXPORT_CODE_BYTES as u64,
      actual: Some(zip_bytes.len() as u64),
    });
  }

  let mut payload = format!("{PROFILE_DATA_PREFIX}\n").into_bytes();

  payload.extend_from_slice(STANDARD.encode(zip_bytes).as_bytes());

  Ok(payload)
}

/// Exports `target_dir` as a shareable Thunderstore profile code.
///
/// `profile_name` is not validated here: unlike [`export_to_file_in`], it never
/// becomes part of a filesystem path — it only rides inside the `export.r2x`
/// manifest carried by the uploaded payload.
pub async fn export_code_in(
  target_dir: &Path,
  profile_name: &str,
  client: &ThunderstoreClient,
) -> Result<String> {
  let payload = encode_profile_payload(&build_r2z_in(target_dir, profile_name)?)?;

  client.create_profile_code(&payload).await
}

/// Exports an r2modman-layout profile as a shareable code. See [`export_code_in`].
pub async fn export_code(
  base: &Path,
  game: &str,
  profile_name: &str,
  client: &ThunderstoreClient,
) -> Result<String> {
  export_code_in(
    &crate::profile::layout::profile_dir(base, game, profile_name),
    profile_name,
    client,
  )
  .await
}

/// Extensions never written by an import: a shared profile carries config, not
/// executable code, so a hostile `.r2z` cannot plant a binary.
const BLOCKED_IMPORT_EXTENSIONS: [&str; 12] = [
  ".dll", ".exe", ".bat", ".cmd", ".com", ".ps1", ".sh", ".js", ".vbs", ".msi", ".scr", ".jar",
];

/// Unwraps a profile-code response body into `.r2z` bytes: validates the
/// `#r2modman` prefix, then base64-decodes the remainder.
fn decode_profile_payload(body: &[u8]) -> Result<Vec<u8>> {
  let text = std::str::from_utf8(body)
    .map_err(|e| Error::Profile(format!("profile code is not valid UTF-8: {e}")))?;

  let encoded = text
    .strip_prefix(PROFILE_DATA_PREFIX)
    .ok_or_else(|| Error::Profile("profile code is missing its #r2modman prefix".to_string()))?;

  STANDARD
    .decode(encoded.trim())
    .map_err(|e| Error::Profile(format!("decoding profile code: {e}")))
}

/// Ceiling on a single inflated archive entry, and on the cumulative inflated
/// size of an import. A shared profile carries text config, so this sits far
/// above any legitimate export while rejecting a decompression bomb (DEFLATE
/// can reach ~1032:1, so a tiny `.r2z` can otherwise inflate to gigabytes).
const MAX_IMPORT_ENTRY_BYTES: u64 = 16 * 1024 * 1024;

/// See `MAX_IMPORT_ENTRY_BYTES`.
const MAX_IMPORT_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Reads one archive entry fully into memory, capped at `MAX_IMPORT_ENTRY_BYTES`.
///
/// The cap is checked against the bytes actually streamed out of the
/// decompressor, not any size the archive declares, so a crafted high-ratio
/// entry cannot forge its way past it. Returns [`Error::ExtractLimitExceeded`]
/// on breach — the same variant [`crate::extract`] uses for this class of guard.
fn read_capped_entry(entry: &mut impl Read, label: &str) -> Result<Vec<u8>> {
  let mut bytes = Vec::new();

  entry
    .take(MAX_IMPORT_ENTRY_BYTES + 1)
    .read_to_end(&mut bytes)
    .map_err(|e| Error::Profile(format!("reading {label}: {e}")))?;

  if bytes.len() as u64 > MAX_IMPORT_ENTRY_BYTES {
    return Err(Error::ExtractLimitExceeded(format!(
      "{label} exceeds the {MAX_IMPORT_ENTRY_BYTES}-byte per-entry import cap"
    )));
  }

  Ok(bytes)
}

/// Reads an export manifest from either a `.r2z` archive or bare `.r2x` YAML.
pub fn read_export(bytes: &[u8]) -> Result<ExportFormat> {
  match zip::ZipArchive::new(Cursor::new(bytes.to_vec())) {
    Ok(mut archive) => {
      let mut entry = archive
        .by_name(EXPORT_MANIFEST)
        .map_err(|e| Error::Profile(format!("archive has no {EXPORT_MANIFEST}: {e}")))?;

      let manifest_bytes = read_capped_entry(&mut entry, EXPORT_MANIFEST)?;
      let manifest = std::str::from_utf8(&manifest_bytes)
        .map_err(|e| Error::Profile(format!("{EXPORT_MANIFEST} is not valid UTF-8: {e}")))?;

      parse_export_r2x(manifest)
    }
    Err(_) => {
      let text = std::str::from_utf8(bytes)
        .map_err(|e| Error::Profile(format!("export is neither a zip nor UTF-8 YAML: {e}")))?;

      parse_export_r2x(text)
    }
  }
}

/// Maps an archive entry name to its target-relative destination, or `None` when
/// the entry must not be written.
///
/// The manifest and `mods.yml` are skipped (the mod list is applied by installing,
/// not by copying), a `config/…` entry is re-rooted back under `BepInEx/`, and any
/// executable extension or unsafe path is refused. Pure — no filesystem access.
///
/// The blocklist check is matched against the name with trailing dots and
/// spaces stripped: Windows silently drops those from the final path component
/// on create, so an entry named e.g. `Evil.dll.` would otherwise land on disk as
/// `Evil.dll` and bypass the filter on the platform that matters most here.
fn import_config_key(name: &str) -> Option<PathBuf> {
  let normalized = name.replace('\\', "/");

  if normalized.is_empty() || normalized.ends_with('/') {
    return None;
  }

  if normalized == EXPORT_MANIFEST || normalized == "mods.yml" {
    return None;
  }

  let lower = normalized.to_ascii_lowercase();
  let probe = lower.trim_end_matches(['.', ' ']);

  if BLOCKED_IMPORT_EXTENSIONS
    .iter()
    .any(|ext| probe.ends_with(ext))
  {
    return None;
  }

  let remapped = match normalized.strip_prefix("config/") {
    Some(rest) => format!("{BEPINEX_CONFIG_DIR}/{rest}"),
    None => normalized,
  };

  let path = PathBuf::from(remapped);

  for component in path.components() {
    match component {
      Component::Normal(_) | Component::CurDir => {}
      Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
    }
  }

  Some(path)
}

/// Whether `bytes` open with a zip archive's `PK` magic number, rather than
/// being a bare `.r2x` YAML manifest (see [`read_export`]).
///
/// A zip's first local-file header, or the end-of-central-directory record of
/// an empty zip, always starts with the two bytes `PK`; a YAML manifest never
/// does. This distinguishes "not a zip at all" from "a zip that failed to
/// parse" — the latter is a real corruption and must still surface as an
/// error rather than be swallowed as "no configs".
fn looks_like_zip(bytes: &[u8]) -> bool {
  bytes.len() >= 2 && &bytes[0..2] == b"PK"
}

/// Refuses to write through a pre-existing symlink at `dest`.
///
/// Mirrors [`crate::install::apply_install`]'s `RefuseSymlink` handling: a
/// symlink already sitting at the destination is refused rather than
/// followed, so a pre-planted leaf link cannot redirect the write outside the
/// target. This is best-effort: it inspects the final path component only
/// (not symlinked parent directories) and is not race-free against a
/// concurrent attacker mutating the target mid-import.
fn refuse_symlink_dest(dest: &Path) -> Result<()> {
  let is_symlink = fs::symlink_metadata(dest)
    .map(|metadata| metadata.file_type().is_symlink())
    .unwrap_or(false);

  if is_symlink {
    return Err(Error::Profile(format!(
      "refusing to write through a symlink at {}",
      dest.display()
    )));
  }

  Ok(())
}

/// Extracts an export's config files into `target_dir`, returning the
/// target-relative paths written. Entries `import_config_key` rejects are
/// skipped.
///
/// A bare `.r2x` manifest (see [`read_export`]) carries no config files, so
/// `zip_bytes` that don't even look like a zip (see `looks_like_zip`) are
/// treated as "nothing to extract" rather than an error; a payload that does
/// look like a zip but fails to parse is a genuine corruption and still
/// surfaces as an error.
///
/// A pre-existing symlink at an entry's destination is refused rather than
/// followed (see `refuse_symlink_dest`), so a planted leaf link cannot
/// redirect a write outside `target_dir`.
///
/// Each entry is bounded by `MAX_IMPORT_ENTRY_BYTES`, and the running total
/// across the whole import by `MAX_IMPORT_TOTAL_BYTES`, so a decompression
/// bomb hiding in an otherwise-valid config entry cannot exhaust memory.
pub fn extract_configs_in(target_dir: &Path, zip_bytes: &[u8]) -> Result<Vec<PathBuf>> {
  if !looks_like_zip(zip_bytes) {
    return Ok(Vec::new());
  }

  let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes.to_vec()))
    .map_err(|e| Error::Profile(format!("reading export archive: {e}")))?;

  let mut written = Vec::new();
  let mut total_written: u64 = 0;

  for index in 0..archive.len() {
    let mut entry = archive
      .by_index(index)
      .map_err(|e| Error::Profile(format!("reading archive entry {index}: {e}")))?;

    let rel = match import_config_key(entry.name()) {
      Some(rel) => rel,
      None => continue,
    };

    let bytes = read_capped_entry(&mut entry, &rel.display().to_string())?;

    total_written += bytes.len() as u64;

    if total_written > MAX_IMPORT_TOTAL_BYTES {
      return Err(Error::ExtractLimitExceeded(format!(
        "import exceeds the {MAX_IMPORT_TOTAL_BYTES}-byte cumulative import cap"
      )));
    }

    let dest = target_dir.join(&rel);

    if let Some(parent) = dest.parent() {
      fs::create_dir_all(parent)
        .map_err(|e| Error::Profile(format!("creating {}: {}", parent.display(), e)))?;
    }

    refuse_symlink_dest(&dest)?;
    fs::write(&dest, &bytes)
      .map_err(|e| Error::Profile(format!("writing {}: {}", dest.display(), e)))?;

    written.push(rel);
  }

  Ok(written)
}

/// Imports a `.r2z` (or a bare `.r2x` manifest, which carries no configs) into
/// `target_dir`: installs every mod the manifest lists (each with its
/// dependency closure) and restores the recorded `enabled` state, then
/// extracts the archive's config files.
///
/// These three steps run in this order deliberately:
///
/// 1. **Install first, extract configs second.** Most configs live in the
///    shared `BepInEx/config/`, which an install never removes. A few mods
///    keep settings inside their own `BepInEx/plugins/<Owner-Name>/` folder
///    instead, and installing (or updating) a mod deletes that folder and
///    writes the package's files fresh, as r2modman does. Extracting configs
///    before installing would lose those exported files the moment their
///    owning mod is installed; extracting after ensures the exported config
///    is what's left on disk. A mod skipped as already current is not
///    touched.
/// 2. **Disable pass runs after every install, inside the batch.** It still
///    runs only once every mod in the batch is installed, which is what
///    makes it immune to a dependency installed later re-enabling an entry
///    the export marked disabled. It now happens inside `install_batch`
///    rather than as a loop here, and running before config extraction is
///    harmless because disabling renames a mod's files rather than
///    rewriting its config.
///
/// **Installs the latest version, not the exported one.** Each entry's
/// `version` is not honored on import: resolution goes through
/// [`crate::models::DependencyGraph::resolve`], which is version-agnostic and
/// always installs a mod's latest available version. There is no version
/// pinning — importing an export pinned to `1.2.3` can install `2.0.0` with no
/// warning.
///
/// `cache_base` is the package cache root and must not be `target_dir` — see
/// [`crate::profile::install_mod_in`]. Returns the sorted identifiers the
/// import covers, dependencies included: those installed and those skipped as
/// already current, so importing a profile the target already matches still
/// lists every mod.
#[allow(clippy::too_many_arguments)]
pub async fn import_r2z_in(
  target_dir: &Path,
  cache_base: &Path,
  eco: &crate::ecosystem::Ecosystem,
  index: &crate::models::PackageIndex,
  client: &ThunderstoreClient,
  game: &str,
  zip_bytes: &[u8],
  installed_at_time: u64,
) -> Result<Vec<String>> {
  let export = read_export(zip_bytes)?;

  let desired: Vec<String> = export.mods.iter().map(|entry| entry.name.clone()).collect();
  let protect: Vec<String> = export
    .mods
    .iter()
    .filter(|entry| !entry.enabled)
    .map(|entry| entry.name.clone())
    .collect();
  let mut batch = crate::profile::InstallBatch::new(desired, protect);

  // A mod already recorded at its latest version is skipped rather than
  // reinstalled, so one the export has enabled but the target has disabled
  // would otherwise stay disabled.
  batch.enable_requested = export
    .mods
    .iter()
    .filter(|entry| entry.enabled)
    .map(|entry| entry.name.clone())
    .collect();

  let outcome = crate::profile::install_batch(
    target_dir,
    cache_base,
    eco,
    index,
    client,
    game,
    &batch,
    installed_at_time,
  )
  .await?;

  // Configs are not extracted after a failure, so the import is unfinished.
  // A retry skips the mods already installed and completes it.
  if let Some((name, error)) = outcome.failed.into_iter().next() {
    return Err(Error::Profile(format!(
      "importing {name}: {error}; the import is unfinished (its config files were \
       not restored), retry the import to finish it"
    )));
  }

  // Configs are extracted after every install, so an exported per-mod config
  // wins over the package default the install just wrote.
  extract_configs_in(target_dir, zip_bytes)?;

  // A mod skipped as already current is still part of the imported profile.
  let mut imported = outcome.succeeded;

  imported.extend(outcome.unchanged);
  imported.sort();
  imported.dedup();

  Ok(imported)
}

/// Imports a shared profile by its Thunderstore code. See [`import_r2z_in`].
#[allow(clippy::too_many_arguments)]
pub async fn import_code_in(
  target_dir: &Path,
  cache_base: &Path,
  eco: &crate::ecosystem::Ecosystem,
  index: &crate::models::PackageIndex,
  client: &ThunderstoreClient,
  game: &str,
  code: &str,
  installed_at_time: u64,
) -> Result<Vec<String>> {
  let body = client.fetch_profile_code(code).await?;
  let zip_bytes = decode_profile_payload(&body)?;

  import_r2z_in(
    target_dir,
    cache_base,
    eco,
    index,
    client,
    game,
    &zip_bytes,
    installed_at_time,
  )
  .await
}

/// Copies an existing r2modman profile directory into `target_dir`, adopting its
/// `mods.yml`, installed files, and config as-is.
///
/// This is a raw directory adoption, not an install: nothing is downloaded and no
/// package cache is populated, so re-enabling a `State`-tracked mod afterwards
/// requires that mod to be reinstalled first.
///
/// **The result may not be uninstallable.** r2modman never wrote this engine's
/// `_state` tracker, so an adopted mod loader ends up with a `mods.yml` entry and
/// no install record, which is exactly what
/// [`ensure_removable`](crate::profile::ensure_removable) refuses and therefore
/// what [`uninstall_mod_in`](crate::profile::uninstall_mod_in) refuses. Use
/// [`adopt_r2modman_dir_in`] instead unless the offline raw copy is what you
/// actually want.
///
/// # Errors
///
/// - [`Error::Profile`] if `source_profile_dir` is not a
///   directory, or if it and `target_dir` are nested in one another (a copy
///   would otherwise recurse into its own output; see
///   [`ensure_disjoint_trees`]).
/// - [`Error::Io`] if a file cannot be read or written.
///
/// Failure can leave `target_dir` partially populated: the copy is not
/// transactional, so a caller that must not keep a half-imported profile should
/// import into a scratch directory and move it into place on success.
pub fn import_r2modman_dir(source_profile_dir: &Path, target_dir: &Path) -> Result<()> {
  if !source_profile_dir.is_dir() {
    return Err(Error::Profile(format!(
      "{} is not a profile directory",
      source_profile_dir.display()
    )));
  }

  ensure_disjoint_trees(source_profile_dir, target_dir)?;

  copy_tree(source_profile_dir, target_dir)
}

/// What [`adopt_r2modman_dir_in`] adopted, and what it had to reinstall to leave
/// the profile coherent.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct AdoptedProfile {
  /// Every identifier the adopted `mods.yml` names, in the order it named them.
  pub adopted: Vec<String>,
  /// The subset reinstalled from Thunderstore so it would have an install
  /// record, in the order they were reinstalled.
  ///
  /// Each came from the package's **latest** version, so a version the source
  /// profile had pinned is not preserved. A caller that shows this to a user
  /// should say so.
  pub reinstalled: Vec<String>,
}

/// Copies an existing r2modman profile directory into `target_dir` and then
/// reinstalls whatever the copy alone would have left un-uninstallable, so the
/// result is a profile this engine can manage.
///
/// [`import_r2modman_dir`] on its own cannot produce one: r2modman never wrote
/// this engine's `_state` tracker, so an adopted mod loader gets a `mods.yml`
/// entry and no install record, and
/// [`ensure_removable`](crate::profile::ensure_removable) then refuses to remove
/// it. This reinstalls each such loader from its package, which is the *only* way
/// to get an exact file list for one.
///
/// Reconstructing the tracker from the copied files instead is deliberately not
/// attempted. A loader installs into `BepInEx/core`, which is itself a declared
/// route, so a loader file sitting there is shape-indistinguishable from one a
/// user dropped in by hand; attributing files by shape would let a later sweep
/// delete a file the loader never placed.
///
/// **Reinstalls the latest version, not the adopted one.** Resolution goes
/// through [`crate::models::DependencyGraph`], which is version-agnostic, so a
/// version the source profile had pinned is not preserved. The returned
/// [`AdoptedProfile::reinstalled`] names exactly which packages this happened to,
/// so a caller can disclose it.
///
/// Takes a `client` rather than a prefetched
/// [`PackageIndex`](crate::models::PackageIndex), unlike [`import_r2z_in`]: the
/// index is fetched only once something actually needs reinstalling, so adopting
/// a profile that names no unrecorded loader stays fully offline. Requiring the
/// caller to pass an index would cost them a network round trip to find out they
/// did not need one.
///
/// `cache_base` is the package cache root and must not be `target_dir`; see
/// [`crate::profile::install_mod_in`].
#[allow(clippy::too_many_arguments)]
pub async fn adopt_r2modman_dir_in(
  source_profile_dir: &Path,
  target_dir: &Path,
  cache_base: &Path,
  eco: &crate::ecosystem::Ecosystem,
  client: &ThunderstoreClient,
  game: &str,
  installed_at_time: u64,
) -> Result<AdoptedProfile> {
  import_r2modman_dir(source_profile_dir, target_dir)?;

  let adopted: Vec<String> = crate::profile::modlist::read(target_dir)?
    .iter()
    .map(|entry| entry.name.clone())
    .collect();

  // Every recognised loader, not merely the first: the registry is keyed on
  // package identity rather than on a game, so nothing rules out an adopted
  // profile naming two.
  //
  // Presence is tested rather than parsing the record, so a target that already
  // holds an *unreadable* one is left exactly as it was. Overwriting it would be
  // a destructive guess, and `ensure_removable` reports it plainly later.
  let unrecorded: Vec<String> = adopted
    .iter()
    .filter(|name| eco.modloader_package(name).is_some())
    .filter(|name| !crate::install::state_file_path(target_dir, name).exists())
    .cloned()
    .collect();

  if unrecorded.is_empty() {
    return Ok(AdoptedProfile {
      adopted,
      reinstalled: unrecorded,
    });
  }

  let index = client.get_manifest().await?;

  // Forced: the copied mods.yml already records each loader, usually at the
  // version that would be installed, which an ordinary install skips. The
  // reinstall is the point, since it is what writes the record.
  for full_name in &unrecorded {
    crate::profile::install_named_in(
      target_dir,
      cache_base,
      eco,
      &index,
      client,
      game,
      full_name,
      true,
      installed_at_time,
    )
    .await?;
  }

  Ok(AdoptedProfile {
    adopted,
    reinstalled: unrecorded,
  })
}

/// What an adoption installed, what it could not, and what it swept.
///
/// Does not derive `Clone`: `failed` carries an [`Error`] per item, and `Error`
/// itself does not implement `Clone` (it wraps `io::Error` and `reqwest::Error`,
/// neither of which do either).
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct AdoptOutcome {
  /// Identifiers now recorded, sorted.
  pub adopted: Vec<String>,
  /// Identifiers still missing from the record, sorted. Their files may be on
  /// disk from an older installer, unrecorded, where a reconcile would sweep
  /// them as delisted.
  pub remaining: Vec<String>,
  /// Root-relative folders the completing sweep removed. Empty unless every
  /// desired name was adopted.
  pub swept: Vec<PathBuf>,
  /// Each desired name that errored while installing, with the reason
  /// (unresolvable dependency, network error, checksum mismatch, and so on).
  ///
  /// This answers a different question than `remaining`: a name lands here only
  /// when its own install attempt failed, whereas `remaining` is derived from the
  /// record afterwards and also holds a name that resolved to nothing without
  /// erroring. A name can therefore be in `remaining` without being in `failed`.
  pub failed: Vec<(String, Error)>,
}

/// Whether `desired` names anything the record does not yet hold.
///
/// Keyed on missing entries rather than a missing record: the record is written
/// after each package installs, so an adoption that failed partway leaves the
/// file present but incomplete. Keying on the file's existence would both hide
/// that and refuse the retry, stranding the remaining mods on disk where a later
/// reconcile would sweep them.
pub fn needs_adoption(target_dir: &Path, desired: &[String]) -> Result<bool> {
  if desired.is_empty() {
    return Ok(false);
  }

  let recorded = crate::profile::modlist::read(target_dir).unwrap_or_default();

  Ok(
    desired
      .iter()
      .any(|name| crate::profile::modlist::find(&recorded, name).is_none()),
  )
}

/// Adopts `desired` into `target_dir`'s record, then reconciles once complete.
///
/// Installing clears and rewrites whatever an older folder-based installer
/// placed, so this is safe over an existing install and safe to retry after a
/// partial failure: an entry already recorded at its latest version is skipped.
///
/// The delisted sweep runs **only** when every desired name is adopted. Running
/// it against a partial record would delete mods that have not been reached yet.
#[allow(clippy::too_many_arguments)]
pub async fn adopt_names_in(
  target_dir: &Path,
  cache_base: &Path,
  eco: &crate::ecosystem::Ecosystem,
  index: &crate::models::PackageIndex,
  client: &ThunderstoreClient,
  game: &str,
  desired: &[String],
  installed_at_time: u64,
) -> Result<AdoptOutcome> {
  let batch = crate::profile::plan_install_batch(target_dir, desired, &[])?;

  let batch_outcome = crate::profile::install_batch(
    target_dir,
    cache_base,
    eco,
    index,
    client,
    game,
    &batch,
    installed_at_time,
  )
  .await?;

  let failed = batch_outcome.failed;

  // Which names made it is read back from the record rather than from the batch
  // outcome: a name can be missing because its own install failed or because it
  // resolved to nothing, and the record is the authority either way.
  let recorded = crate::profile::modlist::read(target_dir).unwrap_or_default();

  let mut adopted: Vec<String> = Vec::new();
  let mut remaining: Vec<String> = Vec::new();

  for name in desired {
    match crate::profile::modlist::find(&recorded, name) {
      Some(_) => adopted.push(name.clone()),
      None => remaining.push(name.clone()),
    }
  }

  adopted.sort();
  remaining.sort();

  if !remaining.is_empty() {
    return Ok(AdoptOutcome {
      adopted,
      remaining,
      swept: Vec::new(),
      failed,
    });
  }

  let game_profile = eco
    .game(game)
    .and_then(|g| g.profile())
    .ok_or_else(|| Error::Profile(format!("no ecosystem profile for game {game:?}")))?;
  let keep: std::collections::BTreeSet<String> =
    recorded.iter().map(|entry| entry.name.clone()).collect();
  let swept = crate::install::remove_delisted(game_profile, target_dir, &keep)?;

  Ok(AdoptOutcome {
    adopted,
    remaining,
    swept,
    failed,
  })
}

/// Refuses a copy whose source is, contains, or sits inside `target`.
///
/// `fs::copy` opens the destination with `O_TRUNC` before reading the source, so
/// copying a file onto itself zeroes it and returns success. Importing a profile
/// into itself would therefore empty the game executable, the loader proxy, every
/// plugin assembly, and every hand-edited config, and only then fail parsing the
/// record it had just zeroed.
///
/// The two nesting cases are refused for the same reason rather than for being
/// merely odd: a source inside the target has its own tree as the copy
/// destination, so a repeated directory name copies into the directory being
/// read; a source containing the target has the copy walking into its own
/// destination, growing a nested tree until the filesystem gives out.
///
/// Paths are canonicalized before comparison, so a relative spelling or one
/// reached through a symlinked directory still compares equal to what it names. A
/// path that cannot be resolved is compared as written rather than assumed
/// disjoint.
pub fn ensure_disjoint_trees(source: &Path, target: &Path) -> Result<()> {
  let source_at = resolve_as_far_as_it_exists(source);
  let target_at = resolve_as_far_as_it_exists(target);

  if source_at == target_at {
    return Err(Error::Profile(format!(
      "refusing to copy {} onto itself: every file would be truncated to zero \
       bytes",
      source.display()
    )));
  }

  if source_at.starts_with(&target_at) {
    return Err(Error::Profile(format!(
      "refusing to copy {} into {}: the source lives inside the destination, so \
       the copy would overwrite the tree it is reading",
      source.display(),
      target.display()
    )));
  }

  if target_at.starts_with(&source_at) {
    return Err(Error::Profile(format!(
      "refusing to copy {} into {}: the destination lives inside the source, so \
       the copy would recurse into itself",
      source.display(),
      target.display()
    )));
  }

  Ok(())
}

/// Canonicalizes as much of `path` as exists, for [`ensure_disjoint_trees`].
///
/// Tries `path.canonicalize()` first, then [`canonicalize_missing`] for a leaf
/// that does not exist yet. Neither is enough on its own for every caller of
/// `ensure_disjoint_trees`: `target_dir` is commonly not yet created at all
/// when `import_r2modman_dir` calls this, before `copy_tree` has made anything
/// under it, so more than one trailing path component can be missing, and
/// `canonicalize_missing` only resolves the immediate parent. So when both of
/// those fail, this walks up past every missing level in turn, using the
/// first ancestor that does canonicalize and rejoining the missing remainder
/// onto it. That resolves a symlink anywhere in the existing prefix, not only
/// in the immediate parent, which matters here: a symlinked ancestor two or
/// more levels above a not-yet-created leaf would otherwise compare as its
/// own unresolved spelling and could wrongly look disjoint from a source it
/// really nests inside or contains.
///
/// Falls back to `path` itself, unresolved, if no ancestor down to the path's
/// root can be canonicalized (for example a relative path with no existing
/// prefix, or a permissions error). That fallback is intentional and matches
/// `ensure_disjoint_trees`'s documented behavior: compare something rather
/// than assume disjoint.
fn resolve_as_far_as_it_exists(path: &Path) -> PathBuf {
  if let Ok(canonical) = path.canonicalize() {
    return canonical;
  }

  if let Ok(canonical) = canonicalize_missing(path) {
    return canonical;
  }

  for ancestor in path.ancestors().skip(1) {
    let Ok(canonical_ancestor) = ancestor.canonicalize() else {
      continue;
    };
    let Ok(remainder) = path.strip_prefix(ancestor) else {
      continue;
    };

    return canonical_ancestor.join(remainder);
  }

  path.to_path_buf()
}

/// Recursively copies a directory tree, skipping symlinks in `src`.
///
/// A pre-existing symlink at a file's destination in `dst` is refused rather
/// than followed (see `refuse_symlink_dest`), so a planted leaf link cannot
/// redirect a copy outside `dst`.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
  fs::create_dir_all(dst)
    .map_err(|e| Error::Profile(format!("creating {}: {}", dst.display(), e)))?;

  for entry in
    fs::read_dir(src).map_err(|e| Error::Profile(format!("reading {}: {}", src.display(), e)))?
  {
    let entry = entry.map_err(|e| Error::Profile(format!("reading an entry: {e}")))?;
    let path = entry.path();
    let metadata = fs::symlink_metadata(&path)
      .map_err(|e| Error::Profile(format!("inspecting {}: {}", path.display(), e)))?;

    if metadata.file_type().is_symlink() {
      continue;
    }

    let target = dst.join(entry.file_name());

    if metadata.is_dir() {
      copy_tree(&path, &target)?;
    } else {
      refuse_symlink_dest(&target)?;

      // A belt-and-braces second line of defence behind `ensure_disjoint_trees`:
      // a same-file copy truncates, so it is skipped rather than attempted even
      // if a caller reached here another way. Mirrors `stage_one_file`, including
      // running after the symlink refusal above, so a symlink planted at the
      // destination is still reported as a symlink even in the edge case where it
      // happens to resolve back to the source path.
      if same_file(&path, &target) {
        continue;
      }

      fs::copy(&path, &target)
        .map_err(|e| Error::Profile(format!("copying to {}: {}", target.display(), e)))?;
    }
  }

  Ok(())
}

/// What an import source string names.
///
/// A user or a UI hands over one string for "import this", and which of the
/// three import routes it means is decided by looking at the filesystem. That
/// decision is made here, once, so no caller re-derives it: a caller that wants
/// to word the routes differently, or preview what a pasted string would do,
/// matches on this instead of stating its own `is_dir`/`is_file` rules.
///
/// Deliberately **not** `#[non_exhaustive]`, matching
/// [`BadProfile`](crate::profile::layout::BadProfile) and
/// [`NotRemovable`](crate::profile::NotRemovable): a caller that words each route
/// should stop compiling when a fourth appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportSource {
  /// An existing directory, treated as an r2modman profile to adopt in place.
  R2modmanDir(PathBuf),
  /// An existing file, treated as a `.r2z` export.
  Archive(PathBuf),
  /// Anything else, treated as a shareable Thunderstore profile code.
  Code(String),
}

impl ImportSource {
  /// Classifies one import source string.
  ///
  /// A path that exists but is neither a directory nor a regular file (a
  /// dangling symlink, a socket) falls through to [`Self::Code`], where the
  /// attempt fails naming the code endpoint. `is_dir`/`is_file` follow symlinks,
  /// so a link to either is classified as its destination.
  pub fn classify(source: &str) -> Self {
    let path = Path::new(source);

    if path.is_dir() {
      return Self::R2modmanDir(path.to_path_buf());
    }

    if path.is_file() {
      return Self::Archive(path.to_path_buf());
    }

    Self::Code(source.to_string())
  }

  /// Whether this route keeps the versions the source recorded.
  ///
  /// True only for [`Self::R2modmanDir`], which copies files as they are. The
  /// other two resolve every entry through
  /// [`DependencyGraph`](crate::models::DependencyGraph) and install each mod's
  /// latest version instead. Even an adoption reinstalls an unrecorded loader at
  /// latest, which is why [`ImportOutcome::reinstalled`] is reported separately.
  ///
  /// Exposed because it is the one thing a caller has to tell a user before they
  /// wonder why a pinned version changed, and deriving it from the variant is
  /// the sort of rule that goes stale in a client.
  pub fn preserves_versions(&self) -> bool {
    matches!(self, Self::R2modmanDir(_))
  }
}

/// What an import installed, by which route.
///
/// Carries `source` so a caller can word the outcome per route, e.g. disclosing
/// that versions were not preserved, without classifying the source string a
/// second time.
#[derive(Debug)]
#[non_exhaustive]
pub struct ImportOutcome {
  /// How `source` was classified, and therefore which route ran.
  pub source: ImportSource,
  /// Identifiers now recorded in the target.
  pub installed: Vec<String>,
  /// Identifiers reinstalled at their latest version so they would have an
  /// install record. Only ever non-empty for [`ImportSource::R2modmanDir`]; see
  /// [`AdoptedProfile::reinstalled`].
  pub reinstalled: Vec<String>,
}

/// Imports whatever `source` names into `target_dir`: an r2modman profile
/// directory, a `.r2z` file, or a Thunderstore profile code.
///
/// The single entry point for "import this string". [`ImportSource::classify`]
/// picks the route and the returned [`ImportOutcome`] says which one ran, so a
/// caller neither inspects the filesystem itself nor reads the archive to hand
/// over its bytes.
///
/// The package index is fetched only on the routes that need it, so adopting a
/// directory that names no unrecorded loader stays fully offline. That is why
/// this takes a `client` rather than a prefetched
/// [`PackageIndex`](crate::models::PackageIndex), matching
/// [`adopt_r2modman_dir_in`].
///
/// `cache_base` is the package cache root and must not be `target_dir`; see
/// [`crate::profile::install_mod_in`].
pub async fn import_in(
  target_dir: &Path,
  cache_base: &Path,
  eco: &crate::ecosystem::Ecosystem,
  client: &ThunderstoreClient,
  game: &str,
  source: &str,
  installed_at_time: u64,
) -> Result<ImportOutcome> {
  let source = ImportSource::classify(source);

  if let ImportSource::R2modmanDir(dir) = &source {
    let adopted = adopt_r2modman_dir_in(
      dir,
      target_dir,
      cache_base,
      eco,
      client,
      game,
      installed_at_time,
    )
    .await?;

    return Ok(ImportOutcome {
      source,
      installed: adopted.adopted,
      reinstalled: adopted.reinstalled,
    });
  }

  let index = client.get_manifest().await?;

  let installed = match &source {
    ImportSource::Archive(path) => {
      let bytes =
        fs::read(path).map_err(|e| Error::Profile(format!("reading {}: {}", path.display(), e)))?;

      import_r2z_in(
        target_dir,
        cache_base,
        eco,
        &index,
        client,
        game,
        &bytes,
        installed_at_time,
      )
      .await?
    }
    ImportSource::Code(code) => {
      import_code_in(
        target_dir,
        cache_base,
        eco,
        &index,
        client,
        game,
        code,
        installed_at_time,
      )
      .await?
    }
    // Unreachable: the `if let ImportSource::R2modmanDir(dir)` above returns
    // before this match, so the only way here is a variant added to
    // `ImportSource` without a corresponding arm, which the compiler catches
    // first. Not reachable by any caller input, so it is not a documented panic.
    ImportSource::R2modmanDir(_) => {
      unreachable!("R2modmanDir returns from the early branch above this match")
    }
  };

  Ok(ImportOutcome {
    source,
    installed,
    reinstalled: Vec::new(),
  })
}

/// Imports a `.r2z` into an r2modman-layout profile. See [`import_r2z_in`].
#[allow(clippy::too_many_arguments)]
pub async fn import_r2z(
  base: &Path,
  game: &str,
  profile_name: &str,
  eco: &crate::ecosystem::Ecosystem,
  index: &crate::models::PackageIndex,
  client: &ThunderstoreClient,
  zip_bytes: &[u8],
  installed_at_time: u64,
) -> Result<Vec<String>> {
  import_r2z_in(
    &crate::profile::layout::profile_dir(base, game, profile_name),
    base,
    eco,
    index,
    client,
    game,
    zip_bytes,
    installed_at_time,
  )
  .await
}

/// Imports a shared profile code into an r2modman-layout profile.
/// See [`import_code_in`].
#[allow(clippy::too_many_arguments)]
pub async fn import_code(
  base: &Path,
  game: &str,
  profile_name: &str,
  eco: &crate::ecosystem::Ecosystem,
  index: &crate::models::PackageIndex,
  client: &ThunderstoreClient,
  code: &str,
  installed_at_time: u64,
) -> Result<Vec<String>> {
  import_code_in(
    &crate::profile::layout::profile_dir(base, game, profile_name),
    base,
    eco,
    index,
    client,
    game,
    code,
    installed_at_time,
  )
  .await
}

#[cfg(test)]
mod tests {
  use super::*;

  fn profile_mod(name: &str, enabled: bool) -> ProfileMod {
    let mut entry = crate::profile::mod_entry_from_version(name, "Author", &sample_version(), 1);
    entry.enabled = enabled;

    entry
  }

  fn sample_version() -> crate::models::Version {
    // Built through the index so no `#[non_exhaustive]` literal is needed here.
    let json = r#"{"name":"Mod","full_name":"Author-Mod","description":"d","icon":"i",
      "version_number":"1.2.3","dependencies":[],"download_url":"u","downloads":1,
      "date_created":"2024-01-01T12:00:00Z","website_url":"w","is_active":true,
      "uuid4":"u","file_size":1}"#;

    serde_json::from_str(json).unwrap()
  }

  #[test]
  fn import_source_classifies_a_directory_a_file_and_a_code() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("SomeProfile");
    let archive = dir.path().join("export.r2z");

    std::fs::create_dir_all(&profile).unwrap();
    write_test_file(&archive, b"not really a zip");

    assert!(matches!(
      ImportSource::classify(profile.to_str().unwrap()),
      ImportSource::R2modmanDir(path) if path == profile
    ));
    assert!(matches!(
      ImportSource::classify(archive.to_str().unwrap()),
      ImportSource::Archive(path) if path == archive
    ));

    // Anything that is not on disk is a Thunderstore profile code. A
    // path-shaped string that does not exist counts too: there is nothing to
    // read, and the code endpoint is the only remaining thing it could name.
    assert!(matches!(
      ImportSource::classify("abc123-def456"),
      ImportSource::Code(code) if code == "abc123-def456"
    ));
    assert!(matches!(
      ImportSource::classify("/no/such/path.r2z"),
      ImportSource::Code(_)
    ));
  }

  #[test]
  fn import_source_reports_whether_versions_survive() {
    let dir = tempfile::tempdir().unwrap();

    // The distinction a caller has to disclose: adoption keeps the source's
    // versions apart from a reinstalled loader, while the other two always
    // install each mod's latest.
    assert!(ImportSource::R2modmanDir(dir.path().to_path_buf()).preserves_versions());
    assert!(!ImportSource::Archive(dir.path().join("e.r2z")).preserves_versions());
    assert!(!ImportSource::Code("abc123".to_string()).preserves_versions());
  }

  #[test]
  fn export_format_maps_mods_and_preserves_enabled() {
    let mods = vec![
      profile_mod("Author-A", true),
      profile_mod("Author-B", false),
    ];

    let export = export_format_from_mods("MyProfile", &mods);

    assert_eq!(export.profile_name, "MyProfile");
    assert_eq!(export.mods.len(), 2);
    assert_eq!(export.mods[0].name, "Author-A");
    assert_eq!(
      export.mods[0].version,
      SemVer {
        major: 1,
        minor: 2,
        patch: 3
      }
    );
    assert!(export.mods[0].enabled);
    assert!(!export.mods[1].enabled);
  }

  #[test]
  fn serialized_r2x_uses_version_not_version_number() {
    let mods = vec![profile_mod("Author-A", true)];
    let yaml = export_r2x_to_string(&export_format_from_mods("MyProfile", &mods)).unwrap();

    assert!(yaml.contains("profileName: MyProfile"));
    assert!(yaml.contains("name: Author-A"));
    assert!(yaml.contains("version:"));
    assert!(yaml.contains("major: 1"));
    // `mods.yml` uses `versionNumber`; `export.r2x` must NOT.
    assert!(!yaml.contains("versionNumber"));
  }

  #[test]
  fn parses_real_r2x_and_defaults_missing_enabled_to_true() {
    let yaml = r#"profileName: Shared
mods:
  - name: Author-A
    version:
      major: 2
      minor: 0
      patch: 1
    enabled: false
  - name: Author-B
    version:
      major: 1
      minor: 0
      patch: 0
"#;

    let export = parse_export_r2x(yaml).unwrap();

    assert_eq!(export.profile_name, "Shared");
    assert_eq!(export.mods.len(), 2);
    assert!(!export.mods[0].enabled);
    // r2modman treats an absent `enabled` as true.
    assert!(export.mods[1].enabled);
  }

  #[test]
  fn parse_export_r2x_rejects_malformed() {
    assert!(parse_export_r2x("not: a valid export").is_err());
  }

  fn write_test_file(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
  }

  #[test]
  fn is_pruned_matches_r2modman_rules() {
    // Dropped.
    assert!(is_pruned(Path::new("dotnet/x.json")));
    assert!(is_pruned(Path::new("_state/Author-Mod-state.yml")));
    assert!(is_pruned(Path::new("BepInEx/config/BepInEx.cfg")));
    assert!(is_pruned(Path::new(
      "BepInEx/plugins/Author-Mod/manifest.json"
    )));
    assert!(is_pruned(Path::new("MelonLoader/x.cfg")));
    assert!(is_pruned(Path::new("GDWeave/core/a.json")));
    assert!(is_pruned(Path::new("GDWeave/mods/b.json")));
    assert!(is_pruned(Path::new("GDWeave/GDWeave.log")));
    assert!(is_pruned(Path::new("mods.yml")));

    // Kept.
    assert!(!is_pruned(Path::new(
      "BepInEx/plugins/Author-Mod/settings.cfg"
    )));
    assert!(!is_pruned(Path::new("GDWeave/other.json")));
    assert!(!is_pruned(Path::new("notes.txt")));
  }

  #[test]
  fn is_config_file_matches_supported_extensions() {
    assert!(is_config_file(Path::new("a/b.cfg")));
    assert!(is_config_file(Path::new("a/b.YAML")));
    assert!(is_config_file(Path::new("b.ini")));
    assert!(!is_config_file(Path::new("a/b.dll")));
    assert!(!is_config_file(Path::new("a/b")));
  }

  #[test]
  fn collect_export_paths_selects_configs_and_skips_pruned() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write_test_file(&root.join("mods.yml"), b"[]");
    write_test_file(&root.join("BepInEx/config/BepInEx.cfg"), b"cfg");
    write_test_file(&root.join("BepInEx/plugins/Author-Mod/settings.cfg"), b"s");
    write_test_file(
      &root.join("BepInEx/plugins/Author-Mod/manifest.json"),
      b"{}",
    );
    write_test_file(&root.join("BepInEx/plugins/Author-Mod/Mod.dll"), b"dll");
    write_test_file(&root.join("_state/Author-Mod-state.yml"), b"x");

    let paths = collect_export_paths(root).unwrap();
    let as_str: Vec<String> = paths
      .iter()
      .map(|p| p.to_string_lossy().replace('\\', "/"))
      .collect();

    assert_eq!(
      as_str,
      vec!["BepInEx/plugins/Author-Mod/settings.cfg".to_string()]
    );
  }

  #[test]
  fn build_r2z_contains_manifest_and_configs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    crate::profile::modlist::write(root, &[profile_mod("Author-A", true)]).unwrap();
    write_test_file(&root.join("BepInEx/config/BepInEx.cfg"), b"shared");
    write_test_file(&root.join("BepInEx/plugins/Author-A/settings.cfg"), b"mine");
    write_test_file(&root.join("BepInEx/plugins/Author-A/Mod.dll"), b"dll");

    let bytes = build_r2z_in(root, "MyProfile").unwrap();

    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let names: Vec<String> = (0..zip.len())
      .map(|i| zip.by_index(i).unwrap().name().replace('\\', "/"))
      .collect();

    assert!(names.contains(&"export.r2x".to_string()));
    // BepInEx/config/ is re-rooted under config/ in the archive.
    assert!(names.contains(&"config/BepInEx.cfg".to_string()));
    // Other config files keep their target-relative path.
    assert!(names.contains(&"BepInEx/plugins/Author-A/settings.cfg".to_string()));
    // Binaries and mods.yml are never exported.
    assert!(!names.iter().any(|n| n.ends_with("Mod.dll")));
    assert!(!names.contains(&"mods.yml".to_string()));

    let mut manifest = String::new();
    std::io::Read::read_to_string(&mut zip.by_name("export.r2x").unwrap(), &mut manifest).unwrap();

    let export = parse_export_r2x(&manifest).unwrap();

    assert_eq!(export.profile_name, "MyProfile");
    assert_eq!(export.mods[0].name, "Author-A");
  }

  #[test]
  fn export_to_file_uses_name_and_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("profile");
    let out = dir.path().join("exports");

    crate::profile::modlist::write(&root, &[profile_mod("Author-A", true)]).unwrap();

    let path = export_to_file_in(&root, "MyProfile", &out, 1_700_000_000_000).unwrap();

    assert_eq!(
      path.file_name().unwrap().to_string_lossy(),
      "MyProfile_1700000000000.r2z"
    );
    assert!(path.exists());
  }

  #[test]
  fn export_to_file_rejects_unsafe_profile_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("profile");
    let out = dir.path().join("exports");

    crate::profile::modlist::write(&root, &[profile_mod("Author-A", true)]).unwrap();

    assert!(export_to_file_in(&root, "../evil", &out, 1).is_err());
    assert!(!dir.path().join("evil_1.r2z").exists());
  }

  #[test]
  fn encode_profile_payload_prefixes_and_base64s() {
    let payload = encode_profile_payload(b"ABC").unwrap();

    assert_eq!(payload, b"#r2modman\nQUJD");
  }

  #[test]
  fn encode_profile_payload_rejects_oversize() {
    let big = vec![0u8; MAX_EXPORT_CODE_BYTES + 1];

    assert!(matches!(
      encode_profile_payload(&big),
      Err(Error::SizeLimitExceeded { .. })
    ));
  }

  #[test]
  fn decode_profile_payload_round_trips_and_rejects_bad_prefix() {
    let payload = encode_profile_payload(b"ABC").unwrap();

    assert_eq!(decode_profile_payload(&payload).unwrap(), b"ABC");
    assert!(decode_profile_payload(b"nope\nQUJD").is_err());
  }

  #[test]
  fn import_config_key_remaps_config_and_blocks_unsafe() {
    // `config/` is re-rooted back under BepInEx/.
    assert_eq!(
      import_config_key("config/BepInEx.cfg"),
      Some(PathBuf::from("BepInEx/config/BepInEx.cfg"))
    );
    // Other entries keep their path.
    assert_eq!(
      import_config_key("BepInEx/plugins/Author-A/settings.cfg"),
      Some(PathBuf::from("BepInEx/plugins/Author-A/settings.cfg"))
    );
    // Manifest and manager state are skipped.
    assert_eq!(import_config_key("export.r2x"), None);
    assert_eq!(import_config_key("mods.yml"), None);
    // Executables are refused even inside an otherwise valid path.
    assert_eq!(import_config_key("BepInEx/plugins/evil.dll"), None);
    // Traversal is refused.
    assert_eq!(import_config_key("../escape.cfg"), None);
  }

  #[test]
  fn import_config_key_blocks_trailing_dot_and_space_bypass() {
    // Windows strips trailing dots/spaces from the final path component on
    // create, so these must be blocked even though they don't literally end
    // with a blocked extension.
    let blocked = [
      "evil.dll.",
      "evil.dll ",
      "evil.dll. . ",
      "evil.DLL",
      "evil.cfg.dll",
    ];

    for name in blocked {
      assert_eq!(
        import_config_key(name),
        None,
        "{name} should have been blocked"
      );
    }

    // An ordinary config entry is unaffected.
    assert_eq!(import_config_key("a.cfg"), Some(PathBuf::from("a.cfg")));
  }

  #[test]
  fn read_export_accepts_r2z_and_bare_r2x() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    crate::profile::modlist::write(root, &[profile_mod("Author-A", true)]).unwrap();

    let zip_bytes = build_r2z_in(root, "MyProfile").unwrap();

    assert_eq!(read_export(&zip_bytes).unwrap().profile_name, "MyProfile");

    let yaml = export_r2x_to_string(&export_format_from_mods("Bare", &[])).unwrap();

    assert_eq!(read_export(yaml.as_bytes()).unwrap().profile_name, "Bare");
  }

  #[test]
  fn extract_configs_writes_remapped_files_only() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let target = dir.path().join("target");

    crate::profile::modlist::write(&source, &[profile_mod("Author-A", true)]).unwrap();
    write_test_file(&source.join("BepInEx/config/BepInEx.cfg"), b"shared");
    write_test_file(
      &source.join("BepInEx/plugins/Author-A/settings.cfg"),
      b"mine",
    );

    let zip_bytes = build_r2z_in(&source, "MyProfile").unwrap();
    let written = extract_configs_in(&target, &zip_bytes).unwrap();

    assert!(target.join("BepInEx/config/BepInEx.cfg").exists());
    assert!(
      target
        .join("BepInEx/plugins/Author-A/settings.cfg")
        .exists()
    );
    // The manifest itself is never written into the target.
    assert!(!target.join("export.r2x").exists());
    assert_eq!(written.len(), 2);
  }

  #[test]
  fn extract_configs_treats_non_zip_input_as_no_configs() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");

    // A bare `.r2x` manifest — no zip container at all.
    let yaml = export_r2x_to_string(&export_format_from_mods("Bare", &[])).unwrap();

    let written = extract_configs_in(&target, yaml.as_bytes()).unwrap();

    assert!(written.is_empty());
  }

  #[test]
  fn extract_configs_still_rejects_a_genuinely_corrupt_zip() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");

    // Looks like a zip (starts with the `PK` magic) but is truncated garbage.
    let corrupt = b"PK\x03\x04not a real zip".to_vec();

    assert!(extract_configs_in(&target, &corrupt).is_err());
  }

  #[test]
  #[cfg(unix)]
  fn extract_configs_refuses_to_write_through_a_symlink() {
    use std::os::unix::fs::symlink;

    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.cfg");
    std::fs::write(&secret, b"original").unwrap();

    let source = tempfile::tempdir().unwrap();

    crate::profile::modlist::write(source.path(), &[profile_mod("Author-A", true)]).unwrap();
    write_test_file(
      &source.path().join("BepInEx/config/BepInEx.cfg"),
      b"new-content",
    );

    let zip_bytes = build_r2z_in(source.path(), "MyProfile").unwrap();

    // Plant a symlink at the destination the archive's `config/BepInEx.cfg`
    // entry remaps to, pointing outside the target.
    let target = tempfile::tempdir().unwrap();
    let dest = target.path().join("BepInEx/config/BepInEx.cfg");

    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    symlink(&secret, &dest).unwrap();

    let err = extract_configs_in(target.path(), &zip_bytes).unwrap_err();

    assert!(matches!(err, Error::Profile(_)));
    // The write is refused and the symlink target outside the target dir is untouched.
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "original");
  }

  /// Builds an in-memory zip with a single highly-compressible entry, so the
  /// archive itself stays tiny while its inflated content can dwarf the caps.
  fn zip_with_one_zero_filled_entry(name: &str, inflated_len: usize) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options: FileOptions<'_, ()> =
      FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    zip.start_file(name, options).unwrap();
    zip.write_all(&vec![0u8; inflated_len]).unwrap();

    zip.finish().unwrap().into_inner()
  }

  #[test]
  fn extract_configs_rejects_entry_exceeding_the_per_entry_cap() {
    let bytes = zip_with_one_zero_filled_entry(
      "BepInEx/config/Big.cfg",
      (MAX_IMPORT_ENTRY_BYTES + 1) as usize,
    );

    let dir = tempfile::tempdir().unwrap();
    let err = extract_configs_in(dir.path(), &bytes).unwrap_err();

    assert!(matches!(err, Error::ExtractLimitExceeded(_)));
  }

  #[test]
  fn extract_configs_rejects_import_exceeding_the_cumulative_cap() {
    // Each entry sits just under the per-entry cap individually, but enough of
    // them together must exceed the cumulative cap.
    let per_entry_len = (MAX_IMPORT_ENTRY_BYTES - 1024) as usize;
    let entries_needed = (MAX_IMPORT_TOTAL_BYTES / per_entry_len as u64) + 1;

    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options: FileOptions<'_, ()> =
      FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let chunk = vec![0u8; per_entry_len];

    for i in 0..entries_needed {
      zip
        .start_file(format!("BepInEx/config/{i}.cfg"), options)
        .unwrap();
      zip.write_all(&chunk).unwrap();
    }

    let bytes = zip.finish().unwrap().into_inner();

    let dir = tempfile::tempdir().unwrap();
    let err = extract_configs_in(dir.path(), &bytes).unwrap_err();

    assert!(matches!(err, Error::ExtractLimitExceeded(_)));
  }

  #[test]
  fn import_r2modman_dir_adopts_an_existing_profile() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let target = dir.path().join("target");

    crate::profile::modlist::write(&source, &[profile_mod("Author-A", true)]).unwrap();
    write_test_file(&source.join("BepInEx/plugins/Author-A/Mod.dll"), b"dll");

    import_r2modman_dir(&source, &target).unwrap();

    assert!(target.join("BepInEx/plugins/Author-A/Mod.dll").exists());
    assert_eq!(crate::profile::modlist::read(&target).unwrap().len(), 1);
    assert!(import_r2modman_dir(&dir.path().join("missing"), &target).is_err());
  }

  #[test]
  #[cfg(unix)]
  fn import_r2modman_dir_refuses_to_copy_through_a_symlink() {
    use std::os::unix::fs::symlink;

    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.cfg");
    std::fs::write(&secret, b"original").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let target = dir.path().join("target");

    crate::profile::modlist::write(&source, &[profile_mod("Author-A", true)]).unwrap();
    write_test_file(
      &source.join("BepInEx/plugins/Author-A/settings.cfg"),
      b"new-content",
    );

    // Plant a symlink at the destination the source's config file would copy to.
    let dest = target.join("BepInEx/plugins/Author-A/settings.cfg");

    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    symlink(&secret, &dest).unwrap();

    let err = import_r2modman_dir(&source, &target).unwrap_err();

    assert!(matches!(err, Error::Profile(_)));
    // The copy is refused and the symlink target outside the target dir is untouched.
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "original");
  }

  #[test]
  fn copy_tree_skips_a_same_file_copy_instead_of_truncating_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write_test_file(&root.join("sub/file.txt"), b"original bytes");

    // `ensure_disjoint_trees` refuses an overlapping source and target on
    // every public path (see `importing_a_directory_into_itself_is_refused_before_anything_is_truncated`
    // in the integration suite), so `copy_tree`'s own `same_file` skip is
    // otherwise never reached there. Calling it directly, the way a future
    // caller reaching `copy_tree` another way might, is the only way to
    // exercise this second line of defence.
    copy_tree(root, root).unwrap();

    let bytes = std::fs::read(root.join("sub/file.txt")).unwrap();

    assert_eq!(
      bytes, b"original bytes",
      "a same-file copy must be skipped, not truncate the file to zero bytes"
    );
  }
}
