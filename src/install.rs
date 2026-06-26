//! Schema-driven, root-relative mod install routing.
//!
//! Given a package's extracted contents and a game's [`ModLoader`] rules, this
//! module computes an [`InstallPlan`]: a list of source files mapped to
//! destination paths *relative to a profile/loader root*. The caller supplies
//! the absolute root and applies the plan with [`apply_install`], which records
//! a [`FileManifest`] of every written path so uninstall is exact and
//! reversible.
//!
//! Deliberately, this module does NOT resolve the absolute game directory, do
//! Steam/Epic discovery, or place the loader proxy next to the executable —
//! those are game-specific concerns left to the caller.
//!
//! Routing rules mirror r2modman / Gale (see [`crate::ecosystem`]): a regular
//! mod's override folders (`plugins`, `patchers`, `monomod`, `core`, `config`,
//! …) route to their targets; everything else falls to the default subdir; and
//! the loader's own package unpacks to the root.

use crate::ecosystem::{ModLoader, Subdir};
use crate::error::Result;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// A single source -> destination mapping within an install plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallEntry {
  /// Absolute path to the source file within the extracted package.
  pub source: PathBuf,
  /// Destination path relative to the profile/loader root.
  pub dest_relative: PathBuf,
  /// Whether this is a user-mutable file (e.g. config) that must not be
  /// overwritten if it already exists at the destination.
  pub mutable: bool,
}

/// A computed, root-relative plan for installing one package.
#[derive(Debug, Clone, Default)]
pub struct InstallPlan {
  pub entries: Vec<InstallEntry>,
}

/// Records every path written by [`apply_install`], for exact uninstall.
#[derive(Debug, Clone, Default)]
pub struct FileManifest {
  /// Paths written, relative to the root passed to [`apply_install`].
  pub written: Vec<PathBuf>,
}

/// Builds an [`InstallPlan`] for a package given its loader and extracted root.
///
/// `package_full_name` is the `Owner-Name` identifier; it is used both to detect
/// the loader's own package (which unpacks to the root) and to namespace a
/// regular mod's files under `<Owner-Name>/`.
pub fn plan_install(
  loader: &ModLoader,
  package_full_name: &str,
  extracted_root: &Path,
) -> Result<InstallPlan> {
  let mut entries = Vec::new();

  if loader.is_loader_package(package_full_name) {
    // Loader pack: unpack to the root. Many packs nest their payload under a
    // subfolder (e.g. `BepInExPack_Valheim/`); strip it via `loader_root` so the
    // folder's *contents* land at the root.
    let base = match &loader.loader_root {
      Some(sub) if extracted_root.join(sub).is_dir() => extracted_root.join(sub),
      _ => extracted_root.to_path_buf(),
    };

    for file in walk_files(&base)? {
      let rel = file.strip_prefix(&base).unwrap_or(&file).to_path_buf();

      // Preserve files that land under a mutable subdir target (e.g. the pack's
      // own `BepInEx/config/`) so a user's edits survive loader reinstalls.
      let mutable = loader
        .subdirs
        .iter()
        .any(|s| s.mutable && rel.starts_with(&s.target));

      entries.push(InstallEntry {
        source: file,
        dest_relative: rel,
        mutable,
      });
    }

    return Ok(InstallPlan { entries });
  }

  for file in walk_files(extracted_root)? {
    let rel = file.strip_prefix(extracted_root).unwrap_or(&file);
    let file_name = match rel.file_name().and_then(|n| n.to_str()) {
      Some(n) => n,
      None => continue,
    };

    if loader.ignored_files.iter().any(|f| f == file_name) {
      continue;
    }

    let (dest_relative, mutable) = route_file(loader, package_full_name, rel, file_name);

    entries.push(InstallEntry {
      source: file,
      dest_relative,
      mutable,
    });
  }

  Ok(InstallPlan { entries })
}

/// Computes the destination (relative to root) for a single regular-mod file,
/// along with whether the target subdir holds user-mutable files.
fn route_file(loader: &ModLoader, ident: &str, rel: &Path, file_name: &str) -> (PathBuf, bool) {
  let mut components = rel.components();
  let top = components
    .next()
    .map(|c| c.as_os_str().to_string_lossy().to_string());

  // If the top-level component names an override subdir, route its contents there.
  if let Some(top) = &top
    && let Some(subdir) = loader.subdir_named(top)
  {
    let remainder: PathBuf = components.as_path().to_path_buf();

    return (target_base(subdir, ident).join(remainder), subdir.mutable);
  }

  let is_loose_root_file = rel.components().count() == 1;

  if is_loose_root_file {
    // Extension routing only applies to loose files at the package root.
    if let Some(subdir) = loader
      .subdirs
      .iter()
      .find(|s| matches!(&s.extension, Some(ext) if file_name.ends_with(ext.as_str())))
    {
      return (target_base(subdir, ident).join(file_name), subdir.mutable);
    }
  }

  // Everything else falls to the default subdir, preserving relative structure.
  match loader.default_subdir() {
    Some(default) => (target_base(default, ident).join(rel), default.mutable),
    None => (PathBuf::from(rel), false),
  }
}

/// Computes a subdir's base destination, applying `<ident>` namespacing per mode.
fn target_base(subdir: &Subdir, ident: &str) -> PathBuf {
  let base = PathBuf::from(&subdir.target);

  if subdir.mode.is_namespaced() {
    base.join(ident)
  } else {
    base
  }
}

/// Applies an install plan into `root`, copying files and recording what was
/// written.
///
/// `root` is the caller-supplied profile/loader root. Parent directories are
/// created as needed. The returned [`FileManifest`] lists each written path
/// relative to `root`.
pub fn apply_install(plan: &InstallPlan, root: &Path) -> Result<FileManifest> {
  let mut written = Vec::with_capacity(plan.entries.len());

  for entry in &plan.entries {
    let dest = root.join(&entry.dest_relative);

    if entry.mutable {
      // User-mutable files (e.g. config) are written only if absent and are
      // never recorded, so a user's edits survive reinstalls and they are never
      // auto-removed on cleanup/uninstall.
      if dest.exists() {
        continue;
      }

      if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
      }

      fs::copy(&entry.source, &dest)?;

      continue;
    }

    if let Some(parent) = dest.parent() {
      fs::create_dir_all(parent)?;
    }

    fs::copy(&entry.source, &dest)?;
    written.push(entry.dest_relative.clone());
  }

  Ok(FileManifest { written })
}

/// Reconciles the on-disk install to a desired set, removing the folders of mods
/// that are no longer wanted — without any sidecar state file.
///
/// Each regular mod installs into its own `<Owner-Name>` folder under a
/// namespaced target (e.g. `BepInEx/plugins/<Owner-Name>/`), so the folders
/// present under those targets *are* the record of what is installed. This
/// removes every such folder whose name is not in `keep`. The `keep` set is
/// authoritative: anything in those targets that is not requested is removed.
///
/// Non-namespaced areas are never touched — loader files unpacked at the root and
/// flat/`mutable` targets like `BepInEx/config` — so a user's config and the
/// loader itself are left alone. Returns the removed directories (relative to
/// `root`).
pub fn remove_delisted(
  loader: &ModLoader,
  root: &Path,
  keep: &BTreeSet<String>,
) -> Result<Vec<PathBuf>> {
  let mut removed = Vec::new();

  for subdir in loader.subdirs.iter().filter(|s| s.mode.is_namespaced()) {
    let dir = root.join(&subdir.target);

    if !dir.is_dir() {
      continue;
    }

    for entry in fs::read_dir(&dir)? {
      let path = entry?.path();

      if !path.is_dir() {
        continue;
      }

      let ident = match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name,
        None => continue,
      };

      if keep.contains(ident) {
        continue;
      }

      fs::remove_dir_all(&path)?;
      removed.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
    }
  }

  Ok(removed)
}

/// Recursively collects all file paths under `dir` (skipping directories).
fn walk_files(dir: &Path) -> Result<Vec<PathBuf>> {
  let mut files = Vec::new();
  let mut stack = vec![dir.to_path_buf()];

  while let Some(current) = stack.pop() {
    for entry in fs::read_dir(&current)? {
      let entry = entry?;
      let path = entry.path();

      if path.is_dir() {
        stack.push(path);
      } else {
        files.push(path);
      }
    }
  }

  files.sort();

  Ok(files)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ecosystem::Ecosystem;
  use std::fs::File;
  use std::io::Write;
  use tempfile::tempdir;

  fn write(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
      fs::create_dir_all(parent).unwrap();
    }

    let mut f = File::create(path).unwrap();
    f.write_all(contents).unwrap();
  }

  fn dest_set(plan: &InstallPlan) -> Vec<String> {
    let mut v: Vec<String> = plan
      .entries
      .iter()
      .map(|e| e.dest_relative.to_string_lossy().replace('\\', "/"))
      .collect();
    v.sort();

    v
  }

  #[test]
  fn test_bepinex_regular_mod_routing() {
    let eco = Ecosystem::bundled();
    let loader = &eco.game("valheim").unwrap().mod_loader;

    let pkg = tempdir().unwrap();
    let root = pkg.path();

    // A loose plugin DLL, a config file under config/, a patcher, an .mm.dll, and metadata.
    write(&root.join("MyMod.dll"), b"dll");
    write(&root.join("manifest.json"), b"{}");
    write(&root.join("config/MyMod.cfg"), b"cfg");
    write(&root.join("patchers/MyPatcher.dll"), b"patch");
    write(&root.join("MyHook.mm.dll"), b"hook");

    let plan = plan_install(loader, "Author-MyMod", root).unwrap();
    let dests = dest_set(&plan);

    assert!(dests.contains(&"BepInEx/plugins/Author-MyMod/MyMod.dll".to_string()));
    assert!(dests.contains(&"BepInEx/plugins/Author-MyMod/manifest.json".to_string()));
    // config is not namespaced
    assert!(dests.contains(&"BepInEx/config/MyMod.cfg".to_string()));
    // override patchers folder routes (namespaced), contents only
    assert!(dests.contains(&"BepInEx/patchers/Author-MyMod/MyPatcher.dll".to_string()));
    // .mm.dll extension routes to monomod
    assert!(dests.contains(&"BepInEx/monomod/Author-MyMod/MyHook.mm.dll".to_string()));
  }

  #[test]
  fn test_loader_pack_unpacks_to_root_stripping_nested_subfolder() {
    let eco = Ecosystem::bundled();
    let loader = &eco.game("valheim").unwrap().mod_loader;

    let pkg = tempdir().unwrap();
    let root = pkg.path();

    // The BepInEx pack nests its payload under `BepInExPack_Valheim/`, plus some
    // package-root metadata that must NOT be installed.
    write(&root.join("BepInExPack_Valheim/winhttp.dll"), b"proxy");
    write(
      &root.join("BepInExPack_Valheim/BepInEx/core/BepInEx.dll"),
      b"core",
    );
    write(&root.join("manifest.json"), b"{}");
    write(&root.join("icon.png"), b"png");

    let plan = plan_install(loader, "denikson-BepInExPack_Valheim", root).unwrap();
    let dests = dest_set(&plan);

    // The nested subfolder prefix is stripped: contents land at the root.
    assert!(dests.contains(&"winhttp.dll".to_string()));
    assert!(dests.contains(&"BepInEx/core/BepInEx.dll".to_string()));
    // Package-root metadata outside the loader_root subfolder is excluded.
    assert!(!dests.iter().any(|d| d.contains("manifest.json")));
    assert!(!dests.iter().any(|d| d.contains("BepInExPack_Valheim")));
  }

  #[test]
  fn test_loader_pack_preserves_config_on_reinstall() {
    let eco = Ecosystem::bundled();
    let loader = &eco.game("valheim").unwrap().mod_loader;

    let pkg = tempdir().unwrap();
    let pkg_root = pkg.path();
    write(&pkg_root.join("BepInExPack_Valheim/winhttp.dll"), b"proxy");
    write(
      &pkg_root.join("BepInExPack_Valheim/BepInEx/config/BepInEx.cfg"),
      b"default",
    );

    let profile = tempdir().unwrap();
    let root = profile.path();

    let plan = plan_install(loader, "denikson-BepInExPack_Valheim", pkg_root).unwrap();
    apply_install(&plan, root).unwrap();

    // User edits the loader config.
    let cfg = root.join("BepInEx/config/BepInEx.cfg");
    write(&cfg, b"user-edited");

    // Reinstalling the loader pack must not clobber the edited config.
    let plan = plan_install(loader, "denikson-BepInExPack_Valheim", pkg_root).unwrap();
    apply_install(&plan, root).unwrap();

    assert_eq!(fs::read_to_string(&cfg).unwrap(), "user-edited");
    // But core files are still (re)written.
    assert!(root.join("winhttp.dll").exists());
  }

  #[test]
  fn test_loader_pack_without_nesting_falls_back_to_root() {
    let eco = Ecosystem::bundled();
    let loader = &eco.game("valheim").unwrap().mod_loader;

    let pkg = tempdir().unwrap();
    let root = pkg.path();

    // No `BepInExPack_Valheim/` folder present: copy the package root directly.
    write(&root.join("winhttp.dll"), b"proxy");

    let plan = plan_install(loader, "denikson-BepInExPack_Valheim", root).unwrap();
    let dests = dest_set(&plan);

    assert!(dests.contains(&"winhttp.dll".to_string()));
  }

  #[test]
  fn test_config_is_preserved_across_reinstall() {
    let eco = Ecosystem::bundled();
    let loader = &eco.game("valheim").unwrap().mod_loader;

    let pkg = tempdir().unwrap();
    let pkg_root = pkg.path();
    write(&pkg_root.join("MyMod.dll"), b"dll");
    write(&pkg_root.join("config/MyMod.cfg"), b"default-config");

    let profile = tempdir().unwrap();
    let root = profile.path();

    // First install writes both files, but only the non-mutable plugin DLL is
    // tracked in the manifest.
    let plan = plan_install(loader, "Author-MyMod", pkg_root).unwrap();
    let first = apply_install(&plan, root).unwrap();
    assert_eq!(first.written.len(), 1);
    assert!(root.join("BepInEx/config/MyMod.cfg").exists());

    // The user edits their config.
    let cfg = root.join("BepInEx/config/MyMod.cfg");
    write(&cfg, b"user-edited");

    // Reinstall: the plugin DLL is rewritten, but the edited config is preserved
    // and is NOT claimed in the manifest (so uninstall won't remove it).
    let plan = plan_install(loader, "Author-MyMod", pkg_root).unwrap();
    let second = apply_install(&plan, root).unwrap();

    assert_eq!(fs::read_to_string(&cfg).unwrap(), "user-edited");
    assert!(second.written.iter().any(|p| p.ends_with("MyMod.dll")));
    assert!(
      !second
        .written
        .iter()
        .any(|p| p.to_string_lossy().contains("MyMod.cfg"))
    );
  }

  #[test]
  fn test_apply_and_manifest_roundtrip() {
    let eco = Ecosystem::bundled();
    let loader = &eco.game("valheim").unwrap().mod_loader;

    let pkg = tempdir().unwrap();
    let pkg_root = pkg.path();
    write(&pkg_root.join("MyMod.dll"), b"dll");
    write(&pkg_root.join("config/MyMod.cfg"), b"cfg");

    let plan = plan_install(loader, "Author-MyMod", pkg_root).unwrap();

    let profile = tempdir().unwrap();
    let root = profile.path();
    let manifest = apply_install(&plan, root).unwrap();

    // The plugin DLL is tracked; the config file is written but not tracked.
    assert_eq!(manifest.written.len(), 1);
    assert!(root.join("BepInEx/plugins/Author-MyMod/MyMod.dll").exists());
    assert!(root.join("BepInEx/config/MyMod.cfg").exists());

    // Uninstall using the recorded manifest leaves no orphan files.
    for rel in &manifest.written {
      fs::remove_file(root.join(rel)).unwrap();
    }

    assert!(!root.join("BepInEx/plugins/Author-MyMod/MyMod.dll").exists());
  }

  #[test]
  fn test_remove_delisted_removes_only_unwanted_mod_folders() {
    let eco = Ecosystem::bundled();
    let loader = &eco.game("valheim").unwrap().mod_loader;

    let profile = tempdir().unwrap();
    let root = profile.path();

    // Two installed mods (namespaced), a loader file in core (not a mod folder),
    // and a user config (non-namespaced).
    write(&root.join("BepInEx/plugins/Owner-ModA/A.dll"), b"a");
    write(&root.join("BepInEx/plugins/Owner-ModB/B.dll"), b"b");
    write(&root.join("BepInEx/core/BepInEx.dll"), b"loader");
    write(&root.join("BepInEx/config/BepInEx.cfg"), b"cfg");

    let keep: BTreeSet<String> = ["Owner-ModA".to_string()].into_iter().collect();
    let removed = remove_delisted(loader, root, &keep).unwrap();

    assert!(root.join("BepInEx/plugins/Owner-ModA/A.dll").exists());
    assert!(!root.join("BepInEx/plugins/Owner-ModB").exists());
    // Loader files and user config are untouched.
    assert!(root.join("BepInEx/core/BepInEx.dll").exists());
    assert!(root.join("BepInEx/config/BepInEx.cfg").exists());

    assert_eq!(removed.len(), 1);
  }

  #[test]
  fn test_melonloader_flat_routing() {
    let eco = Ecosystem::bundled();
    let loader = &eco.game("boneworks").unwrap().mod_loader;

    let pkg = tempdir().unwrap();
    let root = pkg.path();
    write(&root.join("MyMod.dll"), b"dll");
    write(&root.join("manifest.json"), b"{}");

    let plan = plan_install(loader, "Author-MyMod", root).unwrap();
    let dests = dest_set(&plan);

    // .dll loose file routes to Mods, flat (not namespaced)
    assert!(dests.contains(&"Mods/MyMod.dll".to_string()));
    // ignored metadata is dropped
    assert!(!dests.iter().any(|d| d.contains("manifest.json")));
  }
}
