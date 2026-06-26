//! Schema-driven, root-relative mod install routing.
//!
//! Given a package's extracted contents and a game's
//! [`GameProfile`](crate::ecosystem::GameProfile) from the Thunderstore ecosystem
//! schema, this module computes an [`InstallPlan`](crate::install::InstallPlan): a
//! list of source files mapped to destination paths *relative to a profile/loader
//! root*. The caller supplies the absolute root and applies the plan with
//! [`apply_install`](crate::install::apply_install), which records a
//! [`FileManifest`](crate::install::FileManifest) of every written path so
//! uninstall is exact and reversible.
//!
//! Routing is driven entirely by the schema's install rules (no per-game code):
//! a package's override folders route by matching a rule's route leaf, loose files
//! route by longest-matching extension, and anything else falls to the rule flagged
//! as the default location. A package that appears in the ecosystem's mod-loader
//! registry ([`Ecosystem::modloader_package`](crate::ecosystem::Ecosystem::modloader_package))
//! is instead unpacked to the root.
//!
//! Deliberately, this module does NOT resolve the absolute game directory, do
//! Steam/Epic discovery, or place the loader proxy next to the executable — those
//! are game-specific concerns left to the caller.

#![deny(missing_docs)]

use crate::ecosystem::{Ecosystem, GameProfile, InstallRule, ModloaderPackage, TrackingMethod};
use crate::error::{Error, Result};
use crate::util::io_context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Records the files a `State`-tracked mod installed, so enable/disable can undo
/// and redo them. Mirrors r2modman's `_state/<mod>-state.yml` `ModFileTracker`:
/// each pair is `(cache source path, profile-relative install path)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ModFileTracker {
  /// The mod's `Owner-Name` identifier.
  pub mod_name: String,
  /// `(source, install-relative)` pairs, in plan order.
  pub files: Vec<(String, String)>,
}

/// Collects the `State`-tracked entries of `plan` into a [`ModFileTracker`], or
/// `None` when the plan installs no `State` files. Pure — no I/O.
pub fn state_tracker_from_plan(plan: &InstallPlan, mod_name: &str) -> Option<ModFileTracker> {
  let files: Vec<(String, String)> = plan
    .entries
    .iter()
    .filter(|e| e.tracking == TrackingMethod::State)
    .map(|e| {
      (
        e.source.to_string_lossy().to_string(),
        e.dest_relative.to_string_lossy().replace('\\', "/"),
      )
    })
    .collect();

  if files.is_empty() {
    return None;
  }

  Some(ModFileTracker {
    mod_name: mod_name.to_string(),
    files,
  })
}

/// The `_state/<mod_name>-state.yml` path within a profile directory.
pub fn state_file_path(profile_dir: &Path, mod_name: &str) -> PathBuf {
  profile_dir
    .join("_state")
    .join(format!("{mod_name}-state.yml"))
}

/// Writes a mod's state tracker to `<profile_dir>/_state/<mod>-state.yml`.
pub fn write_state_file(profile_dir: &Path, tracker: &ModFileTracker) -> Result<()> {
  let path = state_file_path(profile_dir, &tracker.mod_name);

  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)
      .map_err(|e| io_context(format!("creating {}", parent.display()), e))?;
  }

  let text = serde_yaml_ng::to_string(tracker)
    .map_err(|e| Error::Install(format!("serializing state file: {e}")))?;

  fs::write(&path, text).map_err(|e| io_context(format!("writing {}", path.display()), e))
}

/// Reads a mod's state tracker, or `None` if the file does not exist.
pub fn read_state_file(profile_dir: &Path, mod_name: &str) -> Result<Option<ModFileTracker>> {
  let path = state_file_path(profile_dir, mod_name);

  if !path.exists() {
    return Ok(None);
  }

  let text =
    fs::read_to_string(&path).map_err(|e| io_context(format!("reading {}", path.display()), e))?;
  let tracker = serde_yaml_ng::from_str(&text)
    .map_err(|e| Error::Install(format!("parsing {}: {e}", path.display())))?;

  Ok(Some(tracker))
}

/// Removes a mod's `State`-tracked payload files and then its `_state` file, so
/// an uninstall leaves nothing behind for a mod whose files live under
/// non-namespaced routes (which [`remove_delisted`] does not reconcile).
///
/// A missing `_state` file is a no-op — the mod tracked no `State` files. A
/// payload file that is already gone (e.g. the mod was disabled first) is
/// skipped. Unlike [`disable_mod`], this also deletes the tracker itself, since
/// after an uninstall there is nothing left to restore.
pub fn remove_state_payload(profile_dir: &Path, mod_name: &str) -> Result<()> {
  let tracker = match read_state_file(profile_dir, mod_name)? {
    Some(tracker) => tracker,
    None => return Ok(()),
  };

  remove_state_files(profile_dir, &tracker)?;

  let path = state_file_path(profile_dir, mod_name);

  fs::remove_file(&path).map_err(|e| io_context(format!("removing {}", path.display()), e))
}

/// Validates that `dest` is a safe root-relative path: it must not be absolute,
/// carry a path prefix/root, or use `..` to climb out of the install root.
///
/// This guards every destination computed from untrusted inputs — a package's
/// `Owner-Name` identifier and the ecosystem schema's route `target` — so a
/// malicious or malformed package cannot read or write outside the caller's root.
fn ensure_safe_relative(dest: &Path) -> Result<()> {
  for component in dest.components() {
    match component {
      Component::Normal(_) | Component::CurDir => {}
      Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
        return Err(Error::Install(format!(
          "unsafe install path escapes the install root: {}",
          dest.display()
        )));
      }
    }
  }

  Ok(())
}

/// A single source -> destination mapping within an install plan.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InstallEntry {
  /// Absolute path to the source file within the extracted package (or, for a
  /// `package-zip` install, the package archive itself).
  pub source: PathBuf,
  /// Destination path relative to the profile/loader root.
  pub dest_relative: PathBuf,
  /// How this file is tracked, which governs whether it is namespaced, recorded
  /// for uninstall, and whether an existing file is preserved.
  pub tracking: TrackingMethod,
}

/// A computed, root-relative plan for installing one package.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct InstallPlan {
  /// The ordered source -> destination mappings for this package's files.
  pub entries: Vec<InstallEntry>,
}

/// Records every path written by [`apply_install`], for exact uninstall.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct FileManifest {
  /// Paths written, relative to the root passed to [`apply_install`].
  pub written: Vec<PathBuf>,
}

/// Builds an [`InstallPlan`] for a package.
///
/// If `package_full_name` is in the ecosystem's mod-loader registry, the package
/// is a loader pack and unpacks to the root; otherwise its files are routed by the
/// profile's [`InstallRule`]s. `archive_path` is the package's original zip, needed
/// only for `package-zip` loaders (e.g. GodotML); pass `None` when unavailable.
pub fn plan_install(
  ecosystem: &Ecosystem,
  profile: &GameProfile,
  package_full_name: &str,
  extracted_root: &Path,
  archive_path: Option<&Path>,
) -> Result<InstallPlan> {
  if let Some(pack) = ecosystem.modloader_package(package_full_name) {
    return plan_loader_pack(pack, profile, extracted_root);
  }

  plan_regular_mod(profile, package_full_name, extracted_root, archive_path)
}

/// Plans a loader pack: unpack its payload to the profile root.
///
/// Many packs nest their payload under a single subfolder;
/// [`ModloaderPackage::root_folder`](crate::ecosystem::ModloaderPackage::root_folder)
/// names it so the folder's *contents* land at the root. Package-root metadata
/// (`manifest.json`, `icon.png`, `README.md`) is dropped. Files landing under an
/// untracked route (e.g. the pack's own `BepInEx/config/`) are preserved across
/// reinstalls; the rest are recorded so the loader can be uninstalled exactly.
fn plan_loader_pack(
  pack: &ModloaderPackage,
  profile: &GameProfile,
  extracted_root: &Path,
) -> Result<InstallPlan> {
  let base = match extracted_root.join(&pack.root_folder) {
    nested if !pack.root_folder.is_empty() && nested.is_dir() => nested,
    _ => extracted_root.to_path_buf(),
  };

  let mut untracked = Vec::new();

  collect_routes(
    &profile.install_rules,
    Path::new(""),
    &|rule| rule.tracking_method == TrackingMethod::None,
    &mut untracked,
  );

  let mut entries = Vec::new();

  for file in walk_files(&base)? {
    let rel = file.strip_prefix(&base).unwrap_or(&file).to_path_buf();

    if let Some(tracking) =
      classify_loader_file(&rel, &untracked, &profile.relative_file_exclusions)?
    {
      entries.push(InstallEntry {
        source: file,
        dest_relative: rel,
        tracking,
      });
    }
  }

  Ok(InstallPlan { entries })
}

/// Decides whether and how a loader-pack file (path `rel`, relative to the pack
/// payload root) is installed.
///
/// Returns `None` for package-root metadata (`manifest.json`, `icon.png`,
/// `README.md`) and for a file whose name is in `exclusions` (e.g. `LICENCE`).
/// Otherwise it returns the tracking method: `none` (shared config, preserved
/// across reinstalls) when the file lands under one of the pack's own `untracked`
/// routes, else `state` (recorded for exact uninstall). The safety check on `rel`
/// is surfaced as an `Err`. Pure — no filesystem access.
fn classify_loader_file(
  rel: &Path,
  untracked: &[PathBuf],
  exclusions: &[String],
) -> Result<Option<TrackingMethod>> {
  if is_package_metadata(rel) {
    return Ok(None);
  }

  if let Some(name) = rel.file_name().and_then(|n| n.to_str())
    && exclusions.iter().any(|e| e == name)
  {
    return Ok(None);
  }

  ensure_safe_relative(rel)?;

  let tracking = if untracked.iter().any(|route| rel.starts_with(route)) {
    TrackingMethod::None
  } else {
    TrackingMethod::State
  };

  Ok(Some(tracking))
}

/// Plans a regular mod install by routing each file through the profile's rules.
fn plan_regular_mod(
  profile: &GameProfile,
  ident: &str,
  extracted_root: &Path,
  archive_path: Option<&Path>,
) -> Result<InstallPlan> {
  let mut flat = Vec::new();

  flatten_rules(&profile.install_rules, Path::new(""), &mut flat);

  // Whole-package zip loaders (e.g. GodotML): the catch-all rule is `package-zip`,
  // so the entire archive is installed as a single file rather than per-file.
  if let Some(rule) = flat
    .iter()
    .find(|r| r.rule.is_default_location && r.rule.tracking_method == TrackingMethod::PackageZip)
  {
    return plan_package_zip(&rule.full_route, ident, archive_path);
  }

  let mut entries = Vec::new();

  for file in walk_files(extracted_root)? {
    let rel = file
      .strip_prefix(extracted_root)
      .unwrap_or(&file)
      .to_path_buf();

    match route_file(profile, &flat, ident, &rel)? {
      RouteOutcome::Route {
        dest_relative,
        tracking,
      } => entries.push(InstallEntry {
        source: file,
        dest_relative,
        tracking,
      }),
      RouteOutcome::Skip(reason) => log_route_skip(&rel, reason),
    }
  }

  Ok(InstallPlan { entries })
}

/// The outcome of routing a single extracted file through a profile's rules.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RouteOutcome {
  /// Route the file to `dest_relative` (relative to the profile root) tracked
  /// as `tracking`.
  Route {
    dest_relative: PathBuf,
    tracking: TrackingMethod,
  },
  /// The file is not installed, for the given reason.
  Skip(SkipReason),
}

/// Why [`route_file`] declined to install a file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SkipReason {
  /// The path has no usable file name.
  NoName,
  /// The file matched the profile's file exclusions (e.g. `LICENCE`).
  Excluded,
  /// No install rule claimed the file.
  NoRoute,
  /// The file routed to a rule whose tracking method has no per-file placement
  /// (e.g. `package-zip`).
  UnsupportedTracking(TrackingMethod),
}

/// Routes one extracted file (path `rel`, relative to the package root) through
/// the profile's rules, returning where it should land or why it is skipped.
///
/// This is the whole per-file routing decision, lifted out of the filesystem
/// walk in [`plan_regular_mod`]: it touches no disk, so the entire routing table
/// — override-folder matching, longest-extension routing, the default location,
/// exclusions, and `<ident>` namespacing — is unit-testable against string paths.
/// The safety check on the computed destination is still enforced, surfaced as an
/// `Err`.
fn route_file(
  profile: &GameProfile,
  flat: &[FlatRule<'_>],
  ident: &str,
  rel: &Path,
) -> Result<RouteOutcome> {
  let name = match rel.file_name().and_then(|n| n.to_str()) {
    Some(n) => n,
    None => return Ok(RouteOutcome::Skip(SkipReason::NoName)),
  };

  if profile.relative_file_exclusions.iter().any(|e| e == name) {
    return Ok(RouteOutcome::Skip(SkipReason::Excluded));
  }

  let resolution = resolve_dir(&profile.install_rules, Path::new(""), rel)
    .or_else(|| loose_file_route(flat, name))
    .or_else(|| default_route(flat, rel));

  let RouteResolution {
    route,
    tracking,
    remainder,
  } = match resolution {
    Some(resolved) => resolved,
    None => return Ok(RouteOutcome::Skip(SkipReason::NoRoute)),
  };

  let dest = match build_dest(&route, tracking, ident, &remainder) {
    Some(dest) => dest,
    None => {
      return Ok(RouteOutcome::Skip(SkipReason::UnsupportedTracking(
        tracking,
      )));
    }
  };

  ensure_safe_relative(&dest)?;

  Ok(RouteOutcome::Route {
    dest_relative: dest,
    tracking,
  })
}

/// Emits the same skip warnings the inline routing loop used to, for the reasons
/// worth surfacing (a file with no route, or an unsupported tracking method).
fn log_route_skip(rel: &Path, reason: SkipReason) {
  match reason {
    SkipReason::NoRoute => tracing::warn!("no install route for {}; skipping", rel.display()),
    SkipReason::UnsupportedTracking(tracking) => tracing::warn!(
      "unsupported tracking {:?} for {}; skipping",
      tracking,
      rel.display()
    ),
    SkipReason::Excluded | SkipReason::NoName => {}
  }
}

/// Plans a `package-zip` install: copy the original archive into the route.
fn plan_package_zip(route: &Path, ident: &str, archive_path: Option<&Path>) -> Result<InstallPlan> {
  let archive = match archive_path {
    Some(archive) => archive,
    None => {
      tracing::warn!("package-zip loader requires the original archive; skipping install");

      return Ok(InstallPlan::default());
    }
  };

  let dest = route.join(format!("{ident}.zip"));

  ensure_safe_relative(&dest)?;

  Ok(InstallPlan {
    entries: vec![InstallEntry {
      source: archive.to_path_buf(),
      dest_relative: dest,
      tracking: TrackingMethod::PackageZip,
    }],
  })
}

/// A single rule with its route resolved to a full profile-root-relative path.
struct FlatRule<'a> {
  full_route: PathBuf,
  rule: &'a InstallRule,
}

/// A resolved route for one file: the destination route, how it is tracked, and
/// the remaining sub-path to place under the route.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteResolution {
  route: PathBuf,
  tracking: TrackingMethod,
  remainder: PathBuf,
}

/// Flattens the rule forest into `(full_route, rule)` pairs, resolving each nested
/// rule's route against its parent so subroute routes become absolute.
fn flatten_rules<'a>(rules: &'a [InstallRule], parent: &Path, out: &mut Vec<FlatRule<'a>>) {
  for rule in rules {
    let full = join_route(parent, &rule.route);

    out.push(FlatRule {
      full_route: full.clone(),
      rule,
    });

    if !rule.sub_routes.is_empty() {
      flatten_rules(&rule.sub_routes, &full, out);
    }
  }
}

/// Resolves a file whose top component names an override folder (recursively into
/// subroutes), returning the destination route, its tracking, and the remaining
/// path to place under it.
fn resolve_dir(rules: &[InstallRule], parent_full: &Path, rel: &Path) -> Option<RouteResolution> {
  let mut comps = rel.components();
  let first = comps.next()?.as_os_str().to_str()?;
  let rest = comps.as_path();

  for rule in rules {
    if !route_leaf(&rule.route).eq_ignore_ascii_case(first) {
      continue;
    }

    let full = join_route(parent_full, &rule.route);

    if !rule.sub_routes.is_empty() && rest.components().next().is_some() {
      if let Some(found) = resolve_dir(&rule.sub_routes, &full, rest) {
        return Some(found);
      }

      // A loose file directly inside this folder routes by subroute extension.
      if rest.components().count() == 1
        && let Some(name) = rest.file_name().and_then(|n| n.to_str())
      {
        let mut sub_flat = Vec::new();

        flatten_rules(&rule.sub_routes, &full, &mut sub_flat);

        if let Some(matched) = best_extension(&sub_flat, name) {
          return Some(RouteResolution {
            route: matched.full_route.clone(),
            tracking: matched.rule.tracking_method,
            remainder: PathBuf::from(name),
          });
        }
      }
    }

    return Some(RouteResolution {
      route: full,
      tracking: rule.tracking_method,
      remainder: rest.to_path_buf(),
    });
  }

  None
}

/// Routes a file not claimed by an override folder by longest-matching extension.
///
/// This applies to any file — a root file or one inside an ordinary subfolder
/// (e.g. `Wrapper/Hook.mm.dll` still routes to the `.mm.dll` rule) — so a matched
/// file lands in its extension's route by base name rather than falling through to
/// the default location.
fn loose_file_route(flat: &[FlatRule<'_>], name: &str) -> Option<RouteResolution> {
  let matched = best_extension(flat, name)?;

  Some(RouteResolution {
    route: matched.full_route.clone(),
    tracking: matched.rule.tracking_method,
    remainder: PathBuf::from(name),
  })
}

/// Routes a file that matched nothing to the profile's default-location rule,
/// preserving its full relative path under that route.
fn default_route(flat: &[FlatRule<'_>], rel: &Path) -> Option<RouteResolution> {
  let matched = flat.iter().find(|r| r.rule.is_default_location)?;

  Some(RouteResolution {
    route: matched.full_route.clone(),
    tracking: matched.rule.tracking_method,
    remainder: rel.to_path_buf(),
  })
}

/// Finds the flattened rule whose longest `default_file_extensions` suffix matches
/// `name`, so a more specific extension (e.g. `.lib.dll`) wins over `.dll`.
fn best_extension<'a>(flat: &'a [FlatRule<'a>], name: &str) -> Option<&'a FlatRule<'a>> {
  flat
    .iter()
    .filter_map(|fr| {
      fr.rule
        .default_file_extensions
        .iter()
        .filter(|ext| name.ends_with(ext.as_str()))
        .map(|ext| ext.len())
        .max()
        .map(|len| (len, fr))
    })
    .max_by_key(|(len, _)| *len)
    .map(|(_, fr)| fr)
}

/// Computes the destination for a routed file, applying `<ident>` namespacing for
/// per-mod tracking methods. Returns `None` for methods with no per-file placement
/// (`package-zip`, unknown) and for a flattened `subdir` remainder that has no file
/// name, all of which the caller skips.
fn build_dest(
  route: &Path,
  tracking: TrackingMethod,
  ident: &str,
  remainder: &Path,
) -> Option<PathBuf> {
  match tracking {
    // `subdir` flattens: the matched file lands at `route/<ident>/<basename>`,
    // dropping any intermediate directories (mirrors r2modman's installSubDir).
    // A remainder with no file name (defensive) has no placement, so skip it.
    TrackingMethod::Subdir => remainder
      .file_name()
      .map(|name| route.join(ident).join(name)),
    // `subdir-no-flatten` namespaces the file but preserves its relative sub-path.
    TrackingMethod::SubdirNoFlatten => Some(route.join(ident).join(remainder)),
    TrackingMethod::None | TrackingMethod::State => Some(route.join(remainder)),
    TrackingMethod::PackageZip | TrackingMethod::Other => None,
  }
}

/// The final path segment of a route, i.e. the override-folder name matched at a
/// package's root (`"BepInEx/plugins"` -> `"plugins"`).
fn route_leaf(route: &str) -> &str {
  route.rsplit(['/', '\\']).next().unwrap_or(route)
}

/// Joins a parent full route with a (possibly slash-separated) child route.
fn join_route(parent_full: &Path, child_route: &str) -> PathBuf {
  let child = PathBuf::from(child_route);

  if parent_full.as_os_str().is_empty() {
    child
  } else {
    parent_full.join(child)
  }
}

/// Whether `rel` is package-root metadata that a loader pack should not install.
fn is_package_metadata(rel: &Path) -> bool {
  if rel.components().count() != 1 {
    return false;
  }

  match rel.file_name().and_then(|n| n.to_str()) {
    Some(name) => {
      let lower = name.to_ascii_lowercase();

      lower == "manifest.json" || lower == "icon.png" || lower == "readme.md"
    }
    None => false,
  }
}

/// Collects the full routes of every rule (recursively) matching `pred`.
///
/// Used with a `none`-tracking predicate to find a loader pack's shared/untracked
/// routes (preserved on reinstall), and with a namespaced predicate to find the
/// per-mod `<Owner-Name>/` routes reconciled by [`remove_delisted`].
fn collect_routes<F: Fn(&InstallRule) -> bool>(
  rules: &[InstallRule],
  parent: &Path,
  pred: &F,
  out: &mut Vec<PathBuf>,
) {
  for rule in rules {
    let full = join_route(parent, &rule.route);

    if pred(rule) {
      out.push(full.clone());
    }

    if !rule.sub_routes.is_empty() {
      collect_routes(&rule.sub_routes, &full, pred, out);
    }
  }
}

/// Applies an install plan into `root`, copying files and recording what was
/// written.
///
/// `root` is the caller-supplied profile/loader root. Parent directories are
/// created as needed. Files tracked as `none` (shared config) are written only if
/// absent and are never recorded, so a user's edits survive reinstalls and they are
/// never auto-removed; all other files are (over)written and recorded in the
/// returned [`FileManifest`] (paths relative to `root`).
///
/// The apply is all-or-nothing: if any entry fails, every file this call created
/// is removed before the error is returned — along with any directory tree this
/// call newly created (now left empty) — so a partial install leaves `root` as it
/// found it, with no untracked files or empty directories behind.
///
/// For a `none`-tracked (shared config) destination that already exists — a
/// regular file or a symlink the user planted — the entry is skipped and left
/// untouched, so a user's edits (or their symlinked config) survive. For a
/// tracked destination, a symlink already sitting there is refused rather than
/// followed, so a pre-planted leaf link cannot redirect the write. This is
/// best-effort: it inspects the final path component only (not symlinked parent
/// directories) and is not race-free against a concurrent attacker mutating
/// `root` mid-install.
pub fn apply_install(plan: &InstallPlan, root: &Path) -> Result<FileManifest> {
  let mut written = Vec::with_capacity(plan.entries.len());
  let mut created: Vec<PathBuf> = Vec::new();
  let mut created_dirs: Vec<PathBuf> = Vec::new();

  for entry in &plan.entries {
    if let Err(err) = ensure_safe_relative(&entry.dest_relative) {
      rollback_partial_install(&created, &created_dirs);

      return Err(err);
    }

    let dest = root.join(&entry.dest_relative);

    let dest_is_symlink = fs::symlink_metadata(&dest)
      .map(|metadata| metadata.file_type().is_symlink())
      .unwrap_or(false);

    match classify_entry(entry.tracking, dest.exists(), dest_is_symlink) {
      // Shared config already present (a file or a symlink the user planted) is
      // left untouched so a user's edits survive.
      ApplyAction::SkipExistingConfig => continue,
      // A pre-planted symlink at a tracked destination is refused rather than
      // followed, so it cannot redirect the write elsewhere.
      ApplyAction::RefuseSymlink => {
        rollback_partial_install(&created, &created_dirs);

        return Err(Error::Install(format!(
          "refusing to install through a symlink at {}",
          dest.display()
        )));
      }
      ApplyAction::Write { record } => {
        if let Err(err) = copy_entry(entry, &dest, root, &mut created_dirs) {
          rollback_partial_install(&created, &created_dirs);

          return Err(err);
        }

        created.push(dest);

        if record {
          written.push(entry.dest_relative.clone());
        }
      }
    }
  }

  Ok(FileManifest { written })
}

/// What [`apply_install`] should do with one entry, given the state at its
/// destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyAction {
  /// Copy the source into place; `record` says whether to add it to the
  /// [`FileManifest`] (tracked files are recorded, shared config is not).
  Write { record: bool },
  /// Leave an existing shared-config (`none`-tracked) file/symlink untouched.
  SkipExistingConfig,
  /// Refuse to write through a pre-existing symlink at a tracked destination.
  RefuseSymlink,
}

/// Decides what to do with an entry from its tracking method and whether the
/// destination already exists / is a symlink. Pure — no filesystem access — so
/// the write/skip/refuse policy is exhaustively table-testable.
fn classify_entry(tracking: TrackingMethod, exists: bool, is_symlink: bool) -> ApplyAction {
  if tracking == TrackingMethod::None && (exists || is_symlink) {
    return ApplyAction::SkipExistingConfig;
  }

  if is_symlink {
    return ApplyAction::RefuseSymlink;
  }

  ApplyAction::Write {
    record: tracking != TrackingMethod::None,
  }
}

/// Copies one entry's source to `dest`, creating parent directories as needed and
/// recording any it newly created in `created_dirs` so a failed apply can undo them.
fn copy_entry(
  entry: &InstallEntry,
  dest: &Path,
  root: &Path,
  created_dirs: &mut Vec<PathBuf>,
) -> Result<()> {
  if let Some(parent) = dest.parent() {
    record_created_dirs(parent, root, created_dirs);

    fs::create_dir_all(parent).map_err(|e| {
      Error::Install(format!(
        "failed to create install directory {}: {}",
        parent.display(),
        e
      ))
    })?;
  }

  fs::copy(&entry.source, dest).map_err(|e| {
    Error::Install(format!(
      "failed to copy {} to {}: {}",
      entry.source.display(),
      dest.display(),
      e
    ))
  })?;

  Ok(())
}

/// Records, into `created_dirs`, each not-yet-existing ancestor of `dir` (down to
/// and including `dir`) that lies strictly below `root`, ordered shallowest first.
///
/// Called before `create_dir_all` so a failed apply can remove exactly the
/// directory trees this call introduced — never `root` itself and never a
/// directory that already existed. Already-recorded (now-existing) directories are
/// skipped, so entries sharing a parent do not double-record it.
fn record_created_dirs(dir: &Path, root: &Path, created_dirs: &mut Vec<PathBuf>) {
  let mut missing = Vec::new();
  let mut current = dir;

  while current != root && current.starts_with(root) && !current.exists() {
    missing.push(current.to_path_buf());

    match current.parent() {
      Some(parent) => current = parent,
      None => break,
    }
  }

  for path in missing.into_iter().rev() {
    created_dirs.push(path);
  }
}

/// Best-effort removal of the files an [`apply_install`] created before it
/// failed, then the now-empty directory trees it newly created, so a failed apply
/// leaves `root` as it found it rather than half applied. Directories are removed
/// deepest-first (children before parents) and only if empty, so a pre-existing or
/// still-populated directory is never taken. Cleanup errors are logged, not
/// propagated: the caller needs to see the original failure, not a secondary one.
fn rollback_partial_install(created: &[PathBuf], created_dirs: &[PathBuf]) {
  for path in created.iter().rev() {
    if let Err(err) = fs::remove_file(path) {
      tracing::warn!(
        "failed to roll back partially-installed file {}: {}",
        path.display(),
        err
      );
    }
  }

  for dir in created_dirs.iter().rev() {
    if let Err(err) = fs::remove_dir(dir) {
      tracing::warn!(
        "failed to roll back partially-created directory {}: {}",
        dir.display(),
        err
      );
    }
  }
}

/// The `<Owner-Name>`-shaped mod folders under `profile`'s namespaced install
/// routes that `keep` does not name, as root-relative paths. These are exactly
/// the directories [`remove_delisted`] would delete.
///
/// Each regular mod installs into its own `<Owner-Name>` folder under a
/// namespaced route (e.g. `BepInEx/plugins/<Owner-Name>/`), so the folders
/// present under those routes *are* the record of what is installed.
/// Non-namespaced areas are never considered, which is why loader files at the
/// root and shared/`none` routes like `BepInEx/config` never appear here.
///
/// Public so a caller can ask what a sweep would take before taking it, which is
/// what lets a CLI warn about folders it does not manage. Sharing this with
/// [`remove_delisted`] is the point: a second implementation of "what does the
/// sweep touch" can drift from the one that actually deletes.
///
/// Sorted and deduplicated, which `read_dir` order alone does not give, so a
/// caller listing these one per line reads the same list every run.
pub fn plan_delisted(
  profile: &GameProfile,
  root: &Path,
  keep: &BTreeSet<String>,
) -> Result<Vec<PathBuf>> {
  let mut routes = Vec::new();

  collect_routes(
    &profile.install_rules,
    Path::new(""),
    &|rule| rule.tracking_method.is_namespaced(),
    &mut routes,
  );

  let mut planned = BTreeSet::new();

  for route in routes {
    ensure_safe_relative(&route)?;

    let dir = root.join(&route);

    if !dir.is_dir() {
      continue;
    }

    let mut listing = Vec::new();

    for entry in fs::read_dir(&dir)
      .map_err(|e| io_context(format!("reading install route {}", dir.display()), e))?
    {
      let path = entry
        .map_err(|e| io_context(format!("reading an entry under {}", dir.display()), e))?
        .path();
      let metadata = fs::symlink_metadata(&path)
        .map_err(|e| io_context(format!("inspecting {}", path.display()), e))?;

      listing.push((path, metadata.is_dir(), metadata.file_type().is_symlink()));
    }

    for path in plan_reconcile(&listing, keep) {
      planned.insert(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
    }
  }

  Ok(planned.into_iter().collect())
}

/// Reconciles the on-disk install to a desired set, removing the folders of mods
/// that are no longer wanted, without any sidecar state file.
///
/// This is [`plan_delisted`] followed by the removal, so there is one enumeration
/// and one decision about what the sweep covers. Returns the removed directories
/// (relative to `root`), which is exactly what `plan_delisted` returned.
pub fn remove_delisted(
  profile: &GameProfile,
  root: &Path,
  keep: &BTreeSet<String>,
) -> Result<Vec<PathBuf>> {
  let planned = plan_delisted(profile, root, keep)?;

  for relative in &planned {
    let path = root.join(relative);

    fs::remove_dir_all(&path)
      .map_err(|e| io_context(format!("removing delisted mod {}", path.display()), e))?;
  }

  Ok(planned)
}

/// Decides which directories under a namespaced route should be removed, given a
/// directory `listing` and the set of mod idents to `keep`.
///
/// Each tuple is `(path, is_dir, is_symlink)`. A symlink or non-directory is
/// ignored — never followed or removed. A real directory whose name has the
/// `<Owner-Name>` shape ([`looks_like_mod_ident`]) and is not in `keep` is
/// selected for removal; a loader-shipped subfolder (e.g. `MMHOOK`) or a
/// user-created folder that does not match that shape is left alone. Pure — no
/// filesystem access — so the reconcile decision is testable without planting
/// real directory trees.
fn plan_reconcile(listing: &[(PathBuf, bool, bool)], keep: &BTreeSet<String>) -> Vec<PathBuf> {
  listing
    .iter()
    .filter(|(path, is_dir, is_symlink)| {
      if *is_symlink || !*is_dir {
        return false;
      }

      match path.file_name().and_then(|n| n.to_str()) {
        Some(ident) => looks_like_mod_ident(ident) && !keep.contains(ident),
        None => false,
      }
    })
    .map(|(path, _, _)| path.clone())
    .collect()
}

/// Whether a directory name has the `<Owner-Name>` shape a mod folder uses: at
/// least one `-` separating segments of `[A-Za-z0-9_]`. This is the identifier
/// [`build_dest`] namespaces under, so it distinguishes per-mod folders from
/// loader-shipped or user-created folders under the same route.
fn looks_like_mod_ident(name: &str) -> bool {
  name.contains('-')
    && name
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Recursively collects all file paths under `dir` (skipping directories).
fn walk_files(dir: &Path) -> Result<Vec<PathBuf>> {
  let mut files = Vec::new();
  let mut stack = vec![dir.to_path_buf()];

  while let Some(current) = stack.pop() {
    for entry in fs::read_dir(&current)
      .map_err(|e| io_context(format!("reading package dir {}", current.display()), e))?
    {
      let entry = entry
        .map_err(|e| io_context(format!("reading an entry under {}", current.display()), e))?;
      let path = entry.path();
      let metadata = fs::symlink_metadata(&path)
        .map_err(|e| io_context(format!("inspecting {}", path.display()), e))?;

      if metadata.file_type().is_symlink() {
        // Never follow symlinks: a symlinked directory could let an extracted
        // package escape its root or create a traversal cycle.
        tracing::warn!("Skipping symlink in package contents: {}", path.display());

        continue;
      }

      if metadata.is_dir() {
        stack.push(path);
      } else {
        files.push(path);
      }
    }
  }

  files.sort();

  Ok(files)
}

/// Computes the rename each file needs to toggle its enabled state, omitting
/// files already in the desired state. Disabling appends `.old` to a file that
/// does not already end in `.old`; enabling strips a trailing `.old`. Pure — no
/// I/O — so the policy is table-testable (mirrors `classify_entry`).
pub fn plan_suffix_renames(files: &[PathBuf], disable: bool) -> Vec<(PathBuf, PathBuf)> {
  let mut renames = Vec::new();

  for file in files {
    let name = file.to_string_lossy();
    let is_old = name.to_lowercase().ends_with(".old");

    if disable && !is_old {
      renames.push((file.clone(), PathBuf::from(format!("{name}.old"))));
    } else if !disable && is_old {
      renames.push((file.clone(), PathBuf::from(&name[..name.len() - 4])));
    }
  }

  renames
}

/// Disables an installed mod: renames its `Subdir`-tracked files to `.old` and
/// removes its `State`-tracked files (recoverable via the `_state` file on
/// enable). Leaves shared/`none` config and the loader untouched.
pub fn disable_mod(profile: &GameProfile, profile_dir: &Path, mod_name: &str) -> Result<()> {
  let tracker = read_state_file(profile_dir, mod_name)?;

  toggle_subdir_files(profile, profile_dir, mod_name, true)?;

  toggle_state_files(profile_dir, tracker, true)
}

/// Enables an installed mod: strips `.old` from its `Subdir`-tracked files and
/// restores its `State`-tracked files by copying them back from the cache
/// sources recorded in the `_state` file.
pub fn enable_mod(profile: &GameProfile, profile_dir: &Path, mod_name: &str) -> Result<()> {
  let tracker = read_state_file(profile_dir, mod_name)?;

  toggle_subdir_files(profile, profile_dir, mod_name, false)?;

  toggle_state_files(profile_dir, tracker, false)
}

/// Applies the `.old` rename policy to every file under each namespaced route's
/// `<route>/<mod_name>/` folder.
fn toggle_subdir_files(
  profile: &GameProfile,
  profile_dir: &Path,
  mod_name: &str,
  disable: bool,
) -> Result<()> {
  let mut routes = Vec::new();

  collect_routes(
    &profile.install_rules,
    Path::new(""),
    &|rule| rule.tracking_method.is_namespaced(),
    &mut routes,
  );

  for route in routes {
    let mod_dir = profile_dir.join(&route).join(mod_name);

    if !mod_dir.is_dir() {
      continue;
    }

    let files = walk_files(&mod_dir)?;

    for (from, to) in plan_suffix_renames(&files, disable) {
      fs::rename(&from, &to).map_err(|e| io_context(format!("toggling {}", from.display()), e))?;
    }
  }

  Ok(())
}

/// Removes (disable) or restores from cache (enable) the `State`-tracked files
/// recorded in `tracker`. `None` is a no-op: the mod tracked no `State` files.
///
/// Takes an already-read tracker rather than reading one itself so its caller can
/// do that read before any other mutation. Reading it here instead meant an
/// unreadable record failed only after [`toggle_subdir_files`] had renamed the
/// mod's files, leaving them toggled on disk while the caller's `mods.yml` still
/// recorded the old state.
fn toggle_state_files(
  profile_dir: &Path,
  tracker: Option<ModFileTracker>,
  disable: bool,
) -> Result<()> {
  let Some(tracker) = tracker else {
    return Ok(());
  };

  if disable {
    return remove_state_files(profile_dir, &tracker);
  }

  restore_state_files(profile_dir, &tracker)
}

/// Removes each `State`-tracked destination that still exists, leaving the
/// `_state` file itself in place so [`restore_state_files`] can undo this.
///
/// Each recorded path is re-validated before use: the `_state` file sits in a
/// caller-owned directory and could be hand-edited, so a tampered entry must not
/// delete a file outside the profile root.
fn remove_state_files(profile_dir: &Path, tracker: &ModFileTracker) -> Result<()> {
  for (_, install_rel) in &tracker.files {
    ensure_safe_relative(Path::new(install_rel))?;

    let dest = profile_dir.join(install_rel);

    if !dest.exists() {
      continue;
    }

    fs::remove_file(&dest).map_err(|e| io_context(format!("removing {}", dest.display()), e))?;
  }

  Ok(())
}

/// Copies each `State`-tracked file back from its recorded cache source,
/// creating any missing destination directories.
fn restore_state_files(profile_dir: &Path, tracker: &ModFileTracker) -> Result<()> {
  for (source, install_rel) in &tracker.files {
    let dest = profile_dir.join(install_rel);

    if let Some(parent) = dest.parent() {
      fs::create_dir_all(parent)
        .map_err(|e| io_context(format!("creating {}", parent.display()), e))?;
    }

    fs::copy(source, &dest)
      .map_err(|e| io_context(format!("restoring {} from {}", dest.display(), source), e))?;
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ecosystem::{Ecosystem, GameProfile};
  use std::fs::File;
  use std::io::Write;
  use tempfile::tempdir;

  fn eco() -> Ecosystem {
    Ecosystem::bundled()
  }

  fn profile(eco: &Ecosystem, slug: &str) -> GameProfile {
    eco.game(slug).unwrap().profile().unwrap().clone()
  }

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
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let pkg = tempdir().unwrap();
    let root = pkg.path();

    write(&root.join("MyMod.dll"), b"dll");
    write(&root.join("manifest.json"), b"{}");
    write(&root.join("config/MyMod.cfg"), b"cfg");
    write(&root.join("patchers/MyPatcher.dll"), b"patch");
    write(&root.join("MyHook.mm.dll"), b"hook");

    let plan = plan_install(&eco, &profile, "Author-MyMod", root, None).unwrap();
    let dests = dest_set(&plan);

    assert!(dests.contains(&"BepInEx/plugins/Author-MyMod/MyMod.dll".to_string()));
    // BepInEx defines no exclusions, so metadata routes to the plugin folder.
    assert!(dests.contains(&"BepInEx/plugins/Author-MyMod/manifest.json".to_string()));
    // config is shared/untracked (not namespaced).
    assert!(dests.contains(&"BepInEx/config/MyMod.cfg".to_string()));
    // override patchers folder routes (namespaced), contents only.
    assert!(dests.contains(&"BepInEx/patchers/Author-MyMod/MyPatcher.dll".to_string()));
    // .mm.dll extension wins over .dll and routes to monomod.
    assert!(dests.contains(&"BepInEx/monomod/Author-MyMod/MyHook.mm.dll".to_string()));
  }

  #[test]
  fn route_file_routes_by_table_without_disk() {
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let mut flat = Vec::new();

    flatten_rules(&profile.install_rules, Path::new(""), &mut flat);

    let ident = "Author-MyMod";

    // A loose DLL routes to the (subdir-tracked, namespaced) plugins folder.
    match route_file(&profile, &flat, ident, Path::new("MyMod.dll")).unwrap() {
      RouteOutcome::Route {
        dest_relative,
        tracking,
      } => {
        assert_eq!(
          dest_relative,
          PathBuf::from("BepInEx/plugins/Author-MyMod/MyMod.dll")
        );
        assert_eq!(tracking, TrackingMethod::Subdir);
      }
      other => panic!("expected a route, got {other:?}"),
    }

    // The most-specific extension wins: `.mm.dll` beats `.dll` and routes to monomod.
    match route_file(&profile, &flat, ident, Path::new("MyHook.mm.dll")).unwrap() {
      RouteOutcome::Route { dest_relative, .. } => assert_eq!(
        dest_relative,
        PathBuf::from("BepInEx/monomod/Author-MyMod/MyHook.mm.dll")
      ),
      other => panic!("expected a route, got {other:?}"),
    }

    // A config file is shared/untracked (`none`), not namespaced.
    match route_file(&profile, &flat, ident, Path::new("config/MyMod.cfg")).unwrap() {
      RouteOutcome::Route {
        dest_relative,
        tracking,
      } => {
        assert_eq!(dest_relative, PathBuf::from("BepInEx/config/MyMod.cfg"));
        assert_eq!(tracking, TrackingMethod::None);
      }
      other => panic!("expected a route, got {other:?}"),
    }

    // A path with no file name is skipped rather than routed.
    assert_eq!(
      route_file(&profile, &flat, ident, Path::new("")).unwrap(),
      RouteOutcome::Skip(SkipReason::NoName)
    );
  }

  #[test]
  fn route_leaf_handles_both_separators() {
    assert_eq!(route_leaf("BepInEx/plugins"), "plugins");
    assert_eq!(route_leaf("BepInEx\\plugins"), "plugins");
    assert_eq!(route_leaf("plugins"), "plugins");
  }

  #[test]
  fn classify_entry_truth_table() {
    // Shared config (`none`): skip if a file or a symlink already sits there;
    // otherwise write it, but never record it in the manifest.
    assert_eq!(
      classify_entry(TrackingMethod::None, true, false),
      ApplyAction::SkipExistingConfig
    );
    assert_eq!(
      classify_entry(TrackingMethod::None, false, true),
      ApplyAction::SkipExistingConfig
    );
    assert_eq!(
      classify_entry(TrackingMethod::None, false, false),
      ApplyAction::Write { record: false }
    );

    // Tracked: refuse a pre-existing symlink; otherwise write and record it.
    assert_eq!(
      classify_entry(TrackingMethod::Subdir, false, true),
      ApplyAction::RefuseSymlink
    );
    assert_eq!(
      classify_entry(TrackingMethod::State, true, false),
      ApplyAction::Write { record: true }
    );
    assert_eq!(
      classify_entry(TrackingMethod::SubdirNoFlatten, false, false),
      ApplyAction::Write { record: true }
    );
  }

  #[test]
  fn plan_reconcile_selects_only_unwanted_mod_dirs() {
    let keep: BTreeSet<String> = ["Author-Keep".to_string()].into_iter().collect();

    // (path, is_dir, is_symlink)
    let listing = vec![
      (PathBuf::from("Author-Keep"), true, false),  // kept
      (PathBuf::from("Author-Drop"), true, false),  // removed
      (PathBuf::from("MMHOOK"), true, false),       // not an ident -> ignored
      (PathBuf::from("Author-Link"), true, true),   // symlink -> ignored
      (PathBuf::from("Author-File"), false, false), // not a dir -> ignored
    ];

    let removable = plan_reconcile(&listing, &keep);

    assert_eq!(removable, vec![PathBuf::from("Author-Drop")]);
  }

  #[test]
  fn classify_loader_file_drops_metadata_excluded_and_routes_rest() {
    let untracked = vec![PathBuf::from("BepInEx/config")];
    let exclusions = vec!["LICENCE".to_string()];

    // Package-root metadata is dropped.
    assert_eq!(
      classify_loader_file(Path::new("manifest.json"), &untracked, &exclusions).unwrap(),
      None
    );

    // An excluded file is dropped.
    assert_eq!(
      classify_loader_file(Path::new("LICENCE"), &untracked, &exclusions).unwrap(),
      None
    );

    // A file under an untracked route is preserved (`none` tracking).
    assert_eq!(
      classify_loader_file(Path::new("BepInEx/config/x.cfg"), &untracked, &exclusions).unwrap(),
      Some(TrackingMethod::None)
    );

    // Anything else is recorded (`state` tracking).
    assert_eq!(
      classify_loader_file(
        Path::new("BepInEx/core/loader.dll"),
        &untracked,
        &exclusions
      )
      .unwrap(),
      Some(TrackingMethod::State)
    );

    // A traversal path is rejected.
    assert!(classify_loader_file(Path::new("../evil"), &untracked, &exclusions).is_err());
  }

  #[test]
  fn apply_install_empty_plan_writes_nothing() {
    let root = tempdir().unwrap();

    let manifest = apply_install(&InstallPlan::default(), root.path()).unwrap();

    assert!(manifest.written.is_empty());
  }

  #[test]
  fn test_subdir_tracking_flattens_nested_file() {
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let pkg = tempdir().unwrap();
    let root = pkg.path();

    // A file nested inside the subdir-tracked plugins override folder is flattened
    // into the mod's namespaced folder, dropping the intermediate directory.
    write(&root.join("plugins/nested/Deep.dll"), b"deep");

    let plan = plan_install(&eco, &profile, "Author-MyMod", root, None).unwrap();
    let dests = dest_set(&plan);

    assert!(dests.contains(&"BepInEx/plugins/Author-MyMod/Deep.dll".to_string()));
    assert!(!dests.iter().any(|d| d.contains("nested")));
  }

  #[test]
  fn test_subdir_no_flatten_preserves_subpath() {
    let eco = eco();
    let profile = profile(&eco, "rumble");

    let pkg = tempdir().unwrap();
    let root = pkg.path();

    // rumble's default location (UserData/ModManager) is subdir-no-flatten, so a
    // nested file keeps its relative sub-path under the mod's namespaced folder.
    write(&root.join("data/nested/asset.bin"), b"asset");

    let plan = plan_install(&eco, &profile, "Author-Mod", root, None).unwrap();
    let dests = dest_set(&plan);

    assert!(dests.contains(&"UserData/ModManager/Author-Mod/data/nested/asset.bin".to_string()));
  }

  #[test]
  fn test_extension_routing_applies_to_nested_files() {
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let pkg = tempdir().unwrap();
    let root = pkg.path();

    // A .mm.dll inside an ordinary (non-override) subfolder still routes by its
    // extension to monomod rather than falling to the default plugins route; the
    // subdir flatten then drops the wrapper directory.
    write(&root.join("Wrapper/Hook.mm.dll"), b"hook");

    let plan = plan_install(&eco, &profile, "Author-MyMod", root, None).unwrap();
    let dests = dest_set(&plan);

    assert!(dests.contains(&"BepInEx/monomod/Author-MyMod/Hook.mm.dll".to_string()));
    assert!(!dests.iter().any(|d| d.contains("Wrapper")));
  }

  #[test]
  fn test_loader_pack_unpacks_to_root_stripping_nested_subfolder() {
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let pkg = tempdir().unwrap();
    let root = pkg.path();

    write(&root.join("BepInExPack_Valheim/winhttp.dll"), b"proxy");
    write(
      &root.join("BepInExPack_Valheim/BepInEx/core/BepInEx.dll"),
      b"core",
    );
    write(&root.join("manifest.json"), b"{}");
    write(&root.join("icon.png"), b"png");

    let plan = plan_install(&eco, &profile, "denikson-BepInExPack_Valheim", root, None).unwrap();
    let dests = dest_set(&plan);

    assert!(dests.contains(&"winhttp.dll".to_string()));
    assert!(dests.contains(&"BepInEx/core/BepInEx.dll".to_string()));
    assert!(!dests.iter().any(|d| d.contains("manifest.json")));
    assert!(!dests.iter().any(|d| d.contains("BepInExPack_Valheim")));
    assert!(!dests.iter().any(|d| d.contains("icon.png")));
  }

  #[test]
  fn test_loader_pack_preserves_config_on_reinstall() {
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let pkg = tempdir().unwrap();
    let pkg_root = pkg.path();
    write(&pkg_root.join("BepInExPack_Valheim/winhttp.dll"), b"proxy");
    write(
      &pkg_root.join("BepInExPack_Valheim/BepInEx/config/BepInEx.cfg"),
      b"default",
    );

    let profile_dir = tempdir().unwrap();
    let root = profile_dir.path();

    let plan = plan_install(
      &eco,
      &profile,
      "denikson-BepInExPack_Valheim",
      pkg_root,
      None,
    )
    .unwrap();
    apply_install(&plan, root).unwrap();

    let cfg = root.join("BepInEx/config/BepInEx.cfg");
    write(&cfg, b"user-edited");

    let plan = plan_install(
      &eco,
      &profile,
      "denikson-BepInExPack_Valheim",
      pkg_root,
      None,
    )
    .unwrap();
    apply_install(&plan, root).unwrap();

    assert_eq!(fs::read_to_string(&cfg).unwrap(), "user-edited");
    assert!(root.join("winhttp.dll").exists());
  }

  #[test]
  fn test_loader_pack_without_nesting_falls_back_to_root() {
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let pkg = tempdir().unwrap();
    let root = pkg.path();

    // The pack declares a nested root folder, but it is absent here: copy the
    // package root directly.
    write(&root.join("winhttp.dll"), b"proxy");

    let plan = plan_install(&eco, &profile, "denikson-BepInExPack_Valheim", root, None).unwrap();
    let dests = dest_set(&plan);

    assert!(dests.contains(&"winhttp.dll".to_string()));
  }

  #[test]
  fn test_loader_pack_skips_relative_file_exclusions() {
    let eco = eco();
    // The boneworks profile lists LICENCE in relativeFileExclusions.
    let profile = profile(&eco, "boneworks");

    let pkg = tempdir().unwrap();
    let root = pkg.path();

    // A loader pack from the bundled registry whose declared root folder is absent,
    // so its own root is copied directly.
    write(&root.join("winhttp.dll"), b"proxy");
    write(&root.join("LICENCE"), b"license text");

    let plan = plan_install(&eco, &profile, "denikson-BepInExPack_Valheim", root, None).unwrap();
    let dests = dest_set(&plan);

    assert!(dests.contains(&"winhttp.dll".to_string()));
    // The profile-excluded LICENCE file is neither installed nor recorded, just
    // like plan_regular_mod honors the same exclusions.
    assert!(!dests.iter().any(|d| d.contains("LICENCE")));
  }

  #[test]
  fn test_config_is_preserved_across_reinstall() {
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let pkg = tempdir().unwrap();
    let pkg_root = pkg.path();
    write(&pkg_root.join("MyMod.dll"), b"dll");
    write(&pkg_root.join("config/MyMod.cfg"), b"default-config");

    let profile_dir = tempdir().unwrap();
    let root = profile_dir.path();

    let plan = plan_install(&eco, &profile, "Author-MyMod", pkg_root, None).unwrap();
    let first = apply_install(&plan, root).unwrap();

    assert_eq!(first.written.len(), 1);
    assert!(root.join("BepInEx/config/MyMod.cfg").exists());

    let cfg = root.join("BepInEx/config/MyMod.cfg");
    write(&cfg, b"user-edited");

    let plan = plan_install(&eco, &profile, "Author-MyMod", pkg_root, None).unwrap();
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
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let pkg = tempdir().unwrap();
    let pkg_root = pkg.path();
    write(&pkg_root.join("MyMod.dll"), b"dll");
    write(&pkg_root.join("config/MyMod.cfg"), b"cfg");

    let plan = plan_install(&eco, &profile, "Author-MyMod", pkg_root, None).unwrap();

    let profile_dir = tempdir().unwrap();
    let root = profile_dir.path();
    let manifest = apply_install(&plan, root).unwrap();

    assert_eq!(manifest.written.len(), 1);
    assert!(root.join("BepInEx/plugins/Author-MyMod/MyMod.dll").exists());
    assert!(root.join("BepInEx/config/MyMod.cfg").exists());

    for rel in &manifest.written {
      fs::remove_file(root.join(rel)).unwrap();
    }

    assert!(!root.join("BepInEx/plugins/Author-MyMod/MyMod.dll").exists());
  }

  #[test]
  fn test_apply_install_rolls_back_on_failure() {
    let src = tempdir().unwrap();

    let good = src.path().join("good.dll");
    write(&good, b"good");

    let plan = InstallPlan {
      entries: vec![
        InstallEntry {
          source: good,
          dest_relative: PathBuf::from("BepInEx/plugins/Author-Mod/good.dll"),
          tracking: TrackingMethod::State,
        },
        InstallEntry {
          source: src.path().join("does_not_exist.dll"),
          dest_relative: PathBuf::from("BepInEx/plugins/Author-Mod/missing.dll"),
          tracking: TrackingMethod::State,
        },
      ],
    };

    let profile_dir = tempdir().unwrap();
    let root = profile_dir.path();

    let result = apply_install(&plan, root);

    // The second entry fails to copy (its source is absent); the first,
    // already-written file is rolled back so no untracked file is left behind.
    assert!(result.is_err());
    assert!(!root.join("BepInEx/plugins/Author-Mod/good.dll").exists());
    // The directory tree this apply newly created is removed too, so no empty
    // directories are left behind.
    assert!(!root.join("BepInEx/plugins/Author-Mod").exists());
    assert!(!root.join("BepInEx/plugins").exists());
    assert!(!root.join("BepInEx").exists());
    // The caller-supplied root itself is left in place.
    assert!(root.exists());
  }

  #[test]
  #[cfg(unix)]
  fn test_apply_install_preserves_symlinked_shared_config() {
    use std::os::unix::fs::symlink;

    let src = tempdir().unwrap();
    let source = src.path().join("mod.cfg");
    write(&source, b"packaged-default");

    let outside = tempdir().unwrap();
    let user_config = outside.path().join("user.cfg");
    write(&user_config, b"user-edited");

    let profile_dir = tempdir().unwrap();
    let root = profile_dir.path();

    // The user symlinked their shared config to a file they maintain elsewhere.
    let dest_relative = PathBuf::from("BepInEx/config/mod.cfg");
    let dest = root.join(&dest_relative);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    symlink(&user_config, &dest).unwrap();

    let plan = InstallPlan {
      entries: vec![InstallEntry {
        source,
        dest_relative,
        tracking: TrackingMethod::None,
      }],
    };

    let manifest = apply_install(&plan, root).unwrap();

    // The install succeeds, the None entry is not recorded, and the user's
    // symlinked config is left exactly as it was.
    assert!(manifest.written.is_empty());
    assert_eq!(fs::read_to_string(&user_config).unwrap(), "user-edited");
  }

  #[test]
  #[cfg(unix)]
  fn test_apply_install_refuses_to_write_through_symlink() {
    use std::os::unix::fs::symlink;

    let src = tempdir().unwrap();
    let source = src.path().join("mod.cfg");
    write(&source, b"new-content");

    let outside = tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    write(&secret, b"original");

    let profile_dir = tempdir().unwrap();
    let root = profile_dir.path();

    // Plant a symlink at the destination pointing outside the root.
    let dest_relative = PathBuf::from("BepInEx/config/mod.cfg");
    let dest = root.join(&dest_relative);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    symlink(&secret, &dest).unwrap();

    let plan = InstallPlan {
      entries: vec![InstallEntry {
        source,
        dest_relative,
        tracking: TrackingMethod::State,
      }],
    };

    let result = apply_install(&plan, root);

    // The write is refused and the symlink target outside the root is untouched.
    assert!(result.is_err());
    assert_eq!(fs::read_to_string(&secret).unwrap(), "original");
  }

  #[test]
  fn test_remove_delisted_removes_only_unwanted_mod_folders() {
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let profile_dir = tempdir().unwrap();
    let root = profile_dir.path();

    write(&root.join("BepInEx/plugins/Owner-ModA/A.dll"), b"a");
    write(&root.join("BepInEx/plugins/Owner-ModB/B.dll"), b"b");
    write(&root.join("BepInEx/core/BepInEx.dll"), b"loader");
    write(&root.join("BepInEx/config/BepInEx.cfg"), b"cfg");

    let keep: BTreeSet<String> = ["Owner-ModA".to_string()].into_iter().collect();
    let removed = remove_delisted(&profile, root, &keep).unwrap();

    assert!(root.join("BepInEx/plugins/Owner-ModA/A.dll").exists());
    assert!(!root.join("BepInEx/plugins/Owner-ModB").exists());
    // A loose loader file in core (not a per-mod folder) and user config survive.
    assert!(root.join("BepInEx/core/BepInEx.dll").exists());
    assert!(root.join("BepInEx/config/BepInEx.cfg").exists());

    assert_eq!(removed.len(), 1);
  }

  #[test]
  fn plan_delisted_lists_what_remove_delisted_would_take_without_taking_it() {
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let profile_dir = tempdir().unwrap();
    let root = profile_dir.path();

    write(&root.join("BepInEx/plugins/Owner-ModA/A.dll"), b"a");
    write(&root.join("BepInEx/plugins/Owner-ModB/B.dll"), b"b");
    write(&root.join("BepInEx/plugins/Owner-ModC/C.dll"), b"c");
    // Not `<Owner-Name>`-shaped, so the sweep never takes it.
    write(&root.join("BepInEx/plugins/MMHOOK/hook.dll"), b"hook");

    let keep: BTreeSet<String> = ["Owner-ModA".to_string()].into_iter().collect();
    let planned = plan_delisted(&profile, root, &keep).unwrap();

    // Root-relative and sorted, so a caller listing these one per line gets the
    // same order every run rather than whatever `read_dir` happened to yield.
    assert_eq!(
      planned,
      vec![
        PathBuf::from("BepInEx/plugins/Owner-ModB"),
        PathBuf::from("BepInEx/plugins/Owner-ModC"),
      ]
    );
    // Planning is not applying.
    assert!(root.join("BepInEx/plugins/Owner-ModB/B.dll").exists());
    assert!(root.join("BepInEx/plugins/Owner-ModC/C.dll").exists());

    // What was planned is exactly what the apply takes, because they share the
    // one enumeration rather than each deciding for themselves.
    let removed = remove_delisted(&profile, root, &keep).unwrap();

    assert_eq!(removed, planned);
    assert!(!root.join("BepInEx/plugins/Owner-ModB").exists());
    assert!(!root.join("BepInEx/plugins/Owner-ModC").exists());
    assert!(root.join("BepInEx/plugins/Owner-ModA/A.dll").exists());
    assert!(root.join("BepInEx/plugins/MMHOOK/hook.dll").exists());
  }

  #[test]
  fn test_remove_delisted_ignores_non_mod_directories() {
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let profile_dir = tempdir().unwrap();
    let root = profile_dir.path();

    // A per-mod folder (has the `Owner-Name` shape) and a loader-shipped folder
    // with no hyphen sitting directly under the namespaced plugins route.
    write(&root.join("BepInEx/plugins/Owner-Delete/x.dll"), b"x");
    write(&root.join("BepInEx/plugins/MMHOOK/hook.dll"), b"hook");

    let keep: BTreeSet<String> = BTreeSet::new();
    let removed = remove_delisted(&profile, root, &keep).unwrap();

    // The mod folder is reconciled away; the non-ident loader folder is left.
    assert!(!root.join("BepInEx/plugins/Owner-Delete").exists());
    assert!(root.join("BepInEx/plugins/MMHOOK/hook.dll").exists());
    assert_eq!(removed.len(), 1);
  }

  #[test]
  fn test_melonloader_flat_routing() {
    let eco = eco();
    let profile = profile(&eco, "boneworks");

    let pkg = tempdir().unwrap();
    let root = pkg.path();
    write(&root.join("MyMod.dll"), b"dll");
    write(&root.join("manifest.json"), b"{}");

    let plan = plan_install(&eco, &profile, "Author-MyMod", root, None).unwrap();
    let dests = dest_set(&plan);

    // .dll routes to Mods (state tracking, not namespaced).
    assert!(dests.contains(&"Mods/MyMod.dll".to_string()));
    // manifest.json is excluded by the profile's relativeFileExclusions.
    assert!(!dests.iter().any(|d| d.contains("manifest.json")));
  }

  #[test]
  fn test_melonloader_specific_extension_wins_over_generic() {
    let eco = eco();
    let profile = profile(&eco, "boneworks");

    let pkg = tempdir().unwrap();
    let root = pkg.path();

    write(&root.join("MyLib.lib.dll"), b"lib");
    write(&root.join("MyMod.dll"), b"dll");

    let plan = plan_install(&eco, &profile, "Author-MyMod", root, None).unwrap();
    let dests = dest_set(&plan);

    assert!(dests.contains(&"UserLibs/MyLib.lib.dll".to_string()));
    assert!(dests.contains(&"Mods/MyMod.dll".to_string()));
  }

  #[test]
  fn test_melonloader_subroute_extension_routing() {
    let eco = eco();
    let profile = profile(&eco, "bonelab");

    let pkg = tempdir().unwrap();
    let root = pkg.path();

    // A loose .melon routes to the UserData/CustomItems subroute by extension.
    write(&root.join("cool.melon"), b"item");
    // A file already under UserData routes via folder + subroute extension.
    write(&root.join("UserData/other.melon"), b"item2");

    let plan = plan_install(&eco, &profile, "Author-MyMod", root, None).unwrap();
    let dests = dest_set(&plan);

    assert!(dests.contains(&"UserData/CustomItems/cool.melon".to_string()));
    assert!(dests.contains(&"UserData/CustomItems/other.melon".to_string()));
  }

  #[test]
  fn test_northstar_state_routing_and_exclusions() {
    let eco = eco();
    let profile = profile(&eco, "northstar");

    let pkg = tempdir().unwrap();
    let root = pkg.path();

    write(&root.join("mods/Author.Mod/mod.json"), b"{}");
    write(&root.join("manifest.json"), b"{}");

    let plan = plan_install(&eco, &profile, "Author-Mod", root, None).unwrap();
    let dests = dest_set(&plan);

    // mods/ matches the R2Northstar/mods route (state, structure preserved).
    assert!(dests.contains(&"R2Northstar/mods/Author.Mod/mod.json".to_string()));
    // manifest.json is excluded.
    assert!(!dests.iter().any(|d| d.contains("manifest.json")));
  }

  #[test]
  fn test_godotml_package_zip() {
    let eco = eco();
    let profile = profile(&eco, "brotato");

    let pkg = tempdir().unwrap();
    let root = pkg.path();
    write(&root.join("something.pck"), b"pck");

    let archive = pkg.path().join("Author-Mod-1.0.0.zip");
    write(&archive, b"zipbytes");

    let plan = plan_install(&eco, &profile, "Author-Mod", root, Some(&archive)).unwrap();
    let dests = dest_set(&plan);

    assert_eq!(dests, vec!["mods/Author-Mod.zip".to_string()]);
    assert_eq!(plan.entries[0].tracking, TrackingMethod::PackageZip);

    // Without the archive, the package-zip install is skipped rather than wrong.
    let empty = plan_install(&eco, &profile, "Author-Mod", root, None).unwrap();
    assert!(empty.entries.is_empty());
  }

  #[test]
  fn test_plan_install_rejects_traversal_in_package_name() {
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let pkg = tempdir().unwrap();
    let root = pkg.path();

    write(&root.join("MyMod.dll"), b"dll");

    let result = plan_install(&eco, &profile, "../../etc/Author-Evil", root, None);

    assert!(result.is_err());
  }

  #[test]
  fn test_ensure_safe_relative_rejects_unsafe_paths() {
    assert!(ensure_safe_relative(Path::new("BepInEx/plugins/Author-Mod/x.dll")).is_ok());
    assert!(ensure_safe_relative(Path::new("../escape")).is_err());
    assert!(ensure_safe_relative(Path::new("a/../../escape")).is_err());
    assert!(ensure_safe_relative(Path::new("/abs/path")).is_err());
  }

  #[cfg(unix)]
  #[test]
  fn test_walk_files_skips_symlinks() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let root = dir.path();

    write(&root.join("real.txt"), b"real");
    symlink("/etc/passwd", root.join("link.txt")).unwrap();

    let files = walk_files(root).unwrap();

    assert!(files.iter().any(|p| p.ends_with("real.txt")));
    assert!(!files.iter().any(|p| p.ends_with("link.txt")));
  }

  #[test]
  fn state_tracker_collects_only_state_entries() {
    let plan = InstallPlan {
      entries: vec![
        InstallEntry {
          source: PathBuf::from("/cache/Author-Mod/1.0.0/loader.dll"),
          dest_relative: PathBuf::from("BepInEx/core/loader.dll"),
          tracking: TrackingMethod::State,
        },
        InstallEntry {
          source: PathBuf::from("/cache/Author-Mod/1.0.0/plugin.dll"),
          dest_relative: PathBuf::from("BepInEx/plugins/Author-Mod/plugin.dll"),
          tracking: TrackingMethod::Subdir,
        },
      ],
    };

    let tracker = state_tracker_from_plan(&plan, "Author-Mod").unwrap();

    assert_eq!(tracker.mod_name, "Author-Mod");
    assert_eq!(tracker.files.len(), 1);
    assert_eq!(tracker.files[0].1, "BepInEx/core/loader.dll");
  }

  #[test]
  fn state_tracker_none_when_no_state_entries() {
    let plan = InstallPlan {
      entries: vec![InstallEntry {
        source: PathBuf::from("/cache/x/plugin.dll"),
        dest_relative: PathBuf::from("BepInEx/plugins/Author-Mod/plugin.dll"),
        tracking: TrackingMethod::Subdir,
      }],
    };

    assert!(state_tracker_from_plan(&plan, "Author-Mod").is_none());
  }

  #[test]
  fn remove_state_payload_deletes_files_then_tracker() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    write(&root.join("BepInEx/core/loader.dll"), b"loader");

    let tracker = ModFileTracker {
      mod_name: "Author-Loader".to_string(),
      files: vec![
        (
          "/cache/loader.dll".to_string(),
          "BepInEx/core/loader.dll".to_string(),
        ),
        // An already-removed payload (e.g. the mod was disabled first) is skipped.
        (
          "/cache/gone.dll".to_string(),
          "BepInEx/core/gone.dll".to_string(),
        ),
      ],
    };

    write_state_file(root, &tracker).unwrap();
    remove_state_payload(root, "Author-Loader").unwrap();

    assert!(!root.join("BepInEx/core/loader.dll").exists());
    assert!(!state_file_path(root, "Author-Loader").exists());

    // A mod that tracked no `State` files has no `_state` file: a no-op, not an error.
    assert!(remove_state_payload(root, "Author-Untracked").is_ok());
  }

  #[test]
  fn remove_state_payload_rejects_traversal_in_tracker() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("profile");

    let outside = dir.path().join("secret.txt");
    write(&outside, b"secret");

    let tracker = ModFileTracker {
      mod_name: "Author-Evil".to_string(),
      files: vec![("/cache/x".to_string(), "../secret.txt".to_string())],
    };

    write_state_file(&root, &tracker).unwrap();

    // A hand-edited tracker cannot delete a file outside the profile root.
    assert!(remove_state_payload(&root, "Author-Evil").is_err());
    assert!(outside.exists());
  }

  #[test]
  fn state_file_round_trips() {
    let dir = tempdir().unwrap();
    let tracker = ModFileTracker {
      mod_name: "Author-Mod".to_string(),
      files: vec![("/cache/a".to_string(), "BepInEx/core/a.dll".to_string())],
    };

    write_state_file(dir.path(), &tracker).unwrap();

    assert!(dir.path().join("_state/Author-Mod-state.yml").exists());

    let back = read_state_file(dir.path(), "Author-Mod").unwrap().unwrap();

    assert_eq!(back.mod_name, "Author-Mod");
    assert_eq!(back.files, tracker.files);
    assert!(read_state_file(dir.path(), "Missing").unwrap().is_none());
  }

  #[test]
  fn plan_suffix_renames_adds_and_strips_old() {
    let files = vec![
      PathBuf::from("a/b/plugin.dll"),
      PathBuf::from("a/b/data.cfg.old"),
    ];

    let disable = plan_suffix_renames(&files, true);

    // plugin.dll -> plugin.dll.old ; the already-.old file is left alone (omitted).
    assert_eq!(
      disable,
      vec![(
        PathBuf::from("a/b/plugin.dll"),
        PathBuf::from("a/b/plugin.dll.old"),
      )]
    );

    let enable = plan_suffix_renames(&files, false);

    // data.cfg.old -> data.cfg ; the non-.old file is left alone.
    assert_eq!(
      enable,
      vec![(
        PathBuf::from("a/b/data.cfg.old"),
        PathBuf::from("a/b/data.cfg"),
      )]
    );
  }

  #[test]
  fn disable_enable_subdir_renames_files() {
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let profile_dir = tempdir().unwrap();
    let root = profile_dir.path();
    write(&root.join("BepInEx/plugins/Author-Mod/plugin.dll"), b"x");

    disable_mod(&profile, root, "Author-Mod").unwrap();

    assert!(!root.join("BepInEx/plugins/Author-Mod/plugin.dll").exists());
    assert!(
      root
        .join("BepInEx/plugins/Author-Mod/plugin.dll.old")
        .exists()
    );

    enable_mod(&profile, root, "Author-Mod").unwrap();

    assert!(root.join("BepInEx/plugins/Author-Mod/plugin.dll").exists());
  }

  #[test]
  fn disable_enable_state_removes_and_restores_from_cache() {
    let eco = eco();
    let profile = profile(&eco, "valheim");

    let profile_dir = tempdir().unwrap();
    let root = profile_dir.path();

    // A cached source file and its installed copy under a State route.
    let cache = tempdir().unwrap();
    let src = cache.path().join("loader.dll");
    write(&src, b"loader");
    write(&root.join("BepInEx/core/loader.dll"), b"loader");

    let tracker = ModFileTracker {
      mod_name: "Author-Loader".to_string(),
      files: vec![(
        src.to_string_lossy().to_string(),
        "BepInEx/core/loader.dll".to_string(),
      )],
    };
    write_state_file(root, &tracker).unwrap();

    disable_mod(&profile, root, "Author-Loader").unwrap();

    assert!(!root.join("BepInEx/core/loader.dll").exists());

    enable_mod(&profile, root, "Author-Loader").unwrap();

    assert!(root.join("BepInEx/core/loader.dll").exists());
  }
}
