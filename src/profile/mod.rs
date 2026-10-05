//! Profile installs on top of [`crate::install`]: five high-level operations
//! (install an extracted package, uninstall, enable, disable, and the full
//! download-resolve-install pipeline) that work against an arbitrary target
//! directory. `mods.yml` ([`modlist`](crate::profile::modlist)) is the
//! authoritative install record in every mode.
//!
//! Two layers share the same primitives:
//!
//! - **Client mode** (named r2modman profiles):
//!   [`install_extracted`](crate::profile::install_extracted),
//!   [`uninstall_mod`](crate::profile::uninstall_mod),
//!   [`set_enabled`](crate::profile::set_enabled) (and
//!   [`enable_mod`](crate::profile::enable_mod)/[`disable_mod`](crate::profile::disable_mod)),
//!   and [`install_mod`](crate::profile::install_mod) take `base` +
//!   `profile_name`, compute `layout::profile_dir(base, game, profile_name)`,
//!   and delegate to the `_in` variant below. The profile name is validated by
//!   [`layout::validate_name`](crate::profile::layout::validate_name).
//! - **Server mode**: the `_in` variants
//!   ([`install_extracted_in`](crate::profile::install_extracted_in),
//!   [`uninstall_mod_in`](crate::profile::uninstall_mod_in),
//!   [`set_enabled_in`](crate::profile::set_enabled_in)/[`enable_mod_in`](crate::profile::enable_mod_in)/
//!   [`disable_mod_in`](crate::profile::disable_mod_in),
//!   [`install_mod_in`](crate::profile::install_mod_in)) take a
//!   `target_dir: &Path` directly — there is no named r2modman profile layout;
//!   `mods.yml` is the source of truth for a single mod set installed straight
//!   into the game directory.
//!
//! A caller-supplied `target_dir` is caller-owned and expected to already be
//! absolute; unlike a layout profile name it is **not** name-validated here —
//! there is no [`layout::validate_name`](crate::profile::layout::validate_name)-equivalent
//! check on it, the caller is trusted to pass a real directory. Package
//! *contents* are still traversal-guarded regardless of mode:
//! [`crate::install`]'s `ensure_safe_relative` rejects any archive entry or
//! computed destination that would escape `target_dir`.
//!
//! [`install_mod_in`](crate::profile::install_mod_in)/[`install_mod`](crate::profile::install_mod)
//! additionally take a `cache_base` independent of the install target:
//! extracted packages land under
//! [`cache::ensure_extracted`](crate::profile::cache::ensure_extracted)`(cache_base, ...)`,
//! never under `target_dir`.
//! `cache_base` must persist across installs — it is also the restore source
//! [`crate::install::enable_mod`] copies from when re-enabling a `State`-
//! tracked mod — and in server mode it must not be `target_dir` itself, or
//! cache contents would sit inside the installed mod tree.

#![deny(missing_docs)]

/// Extract packages into the per-game package cache.
pub mod cache;
/// Compute a launch invocation and stage loader files.
pub mod launch;
/// Directory layout and profile lifecycle (named r2modman profiles; client mode).
pub mod layout;
/// The `mods.yml` model and its read/write.
pub mod modlist;
/// `.r2z` export/import and Thunderstore profile codes.
pub mod portability;
/// Where an install goes: a named profile, or the game directory itself.
pub mod target;

use crate::client::ThunderstoreClient;
use crate::ecosystem::Ecosystem;
use crate::error::{Error, Result};
use crate::extract::parse_package_filename;
use crate::install::{
  apply_install, clear_state_files, plan_install, remove_delisted, remove_mod_folders,
  remove_state_file, remove_state_payload, state_tracker_from_plan, write_state_file,
};
use crate::models::{DependencyGraph, PackageIndex, Version};
use modlist::{ProfileMod, SemVer};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Builds a [`ProfileMod`] from a Thunderstore [`Version`], applying the same
/// defaults r2modman uses for a Thunderstore install (`fromThunderstoreCombo`):
/// `manifestVersion=1`, `gameVersion="0"`, `networkMode="both"`,
/// `packageType="other"`, `installMode="managed"`.
pub fn mod_entry_from_version(
  full_name: &str,
  owner: &str,
  version: &Version,
  installed_at_time: u64,
) -> ProfileMod {
  let version_number = version
    .version_number
    .as_deref()
    .map(SemVer::parse)
    .unwrap_or(SemVer {
      major: 0,
      minor: 0,
      patch: 0,
    });

  ProfileMod {
    manifest_version: 1,
    name: full_name.to_string(),
    author_name: owner.to_string(),
    website_url: version.website_url.clone().unwrap_or_default(),
    display_name: version
      .name
      .clone()
      .unwrap_or_else(|| full_name.to_string()),
    description: version.description.clone().unwrap_or_default(),
    game_version: "0".to_string(),
    network_mode: "both".to_string(),
    package_type: "other".to_string(),
    install_mode: "managed".to_string(),
    installed_at_time,
    loaders: Vec::new(),
    dependencies: version.dependencies.clone(),
    incompatibilities: Vec::new(),
    optional_dependencies: Vec::new(),
    version_number,
    enabled: true,
    icon: version.icon.clone().unwrap_or_default(),
    online_source: true,
    trusted_package: false,
  }
}

/// The per-item result of a batch operation.
///
/// A batch does not stop at the first failure: an operation that already mutated
/// earlier items has to report which ones, so `failed` carries the reason per
/// item rather than collapsing to a single error.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct BatchOutcome {
  /// Identifiers the operation completed.
  ///
  /// For [`uninstall_batch`] and [`set_enabled_batch`], this is exactly the
  /// requested identifiers, in the order each completed, matching this
  /// field's name at face value. For [`install_batch`] it does not: that
  /// function sorts and deduplicates it, and fills it with the whole
  /// dependency closure that installed rather than only what the caller
  /// asked for. A caller zipping `batch.desired` against this positionally,
  /// or reading `succeeded.len()` as "how many of my requested items
  /// worked", is wrong for install by however many dependencies were pulled
  /// in.
  pub succeeded: Vec<String>,
  /// Identifiers left alone because the version already recorded in `mods.yml`
  /// is the one that would be installed. Only [`install_batch`] fills this;
  /// like `succeeded` it covers the dependency closure, sorted and
  /// deduplicated, and never names anything `succeeded` or `failed` also names.
  pub unchanged: Vec<String>,
  /// Identifiers that failed, each with the reason.
  pub failed: Vec<(String, Error)>,
}

impl BatchOutcome {
  /// Whether every item succeeded.
  pub fn is_ok(&self) -> bool {
    self.failed.is_empty()
  }
}

/// What an install batch will install, and whose disabled state it must restore
/// afterwards.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct InstallBatch {
  /// Every identifier to install. Dependency closures are resolved per item by
  /// the install itself, so this is what the caller asked for, not the closure.
  pub desired: Vec<String>,
  /// Recorded mods whose `enabled: false` must be restored after the batch.
  ///
  /// An install upserts every dependency-closure member with `enabled: true` and
  /// `modlist::upsert` replaces the entry wholesale, so without this a mod the
  /// user disabled is silently re-enabled by a reinstall.
  pub protect_disabled: Vec<String>,
  /// Mods the caller wants enabled after the batch (e.g. the ones it named).
  ///
  /// A reinstall already leaves them enabled, but one skipped because its
  /// recorded version is current is not reinstalled at all, so without this
  /// naming a disabled mod would leave it disabled. A name that is already
  /// enabled, or was not skipped, is left alone.
  pub enable_requested: Vec<String>,
  /// Reinstall each `desired` mod even when `mods.yml` already records the
  /// version that would be installed. Applies to the `desired` names only, so
  /// their dependencies are still skipped at a recorded version.
  pub force: bool,
}

impl InstallBatch {
  /// Builds a batch directly, for a caller that knows its own protect set rather
  /// than deriving it from the target (e.g. an import applying an export's
  /// recorded disabled state).
  pub fn new(desired: Vec<String>, protect_disabled: Vec<String>) -> Self {
    Self {
      desired,
      protect_disabled,
      ..Self::default()
    }
  }
}

/// Plans an install batch against `target_dir`'s current record.
///
/// `desired` is everything to install. `explicit` is the subset the caller named
/// directly: asking for a mod by name is a request to have it active, so a
/// disabled one goes to `enable_requested` rather than `protect_disabled`. Pass
/// an empty `explicit` when the batch is run on the user's behalf rather than at
/// their request, so every disabled mod is restored.
pub fn plan_install_batch(
  target_dir: &Path,
  desired: &[String],
  explicit: &[String],
) -> Result<InstallBatch> {
  let recorded = modlist::read(target_dir)?;

  let (enable_requested, protect_disabled): (Vec<String>, Vec<String>) = recorded
    .iter()
    .filter(|entry| !entry.enabled)
    .map(|entry| entry.name.clone())
    .partition(|name| explicit.contains(name));

  Ok(InstallBatch {
    desired: desired.to_vec(),
    protect_disabled,
    enable_requested,
    force: false,
  })
}

/// Re-disables everything `batch` protects, after the installs have run.
///
/// Must run after the whole batch, never interleaved: a mod later in a batch can
/// pull an earlier one back in as a dependency, so only a final pass catches
/// every re-enable. Runs even when the batch failed partway, since items
/// installed before the failure are already enabled.
pub fn reapply_batch_disabled(
  target_dir: &Path,
  eco: &Ecosystem,
  game: &str,
  batch: &InstallBatch,
) -> Result<()> {
  for full_name in &batch.protect_disabled {
    set_enabled_in(target_dir, eco, game, full_name, false)?;
  }

  Ok(())
}

/// Installs an already-extracted package into `target_dir`: plans the file copy,
/// removes any copy already installed (its namespaced `<route>/<ident>/` folders
/// and `State`-tracked files, as r2modman uninstalls before installing), applies
/// the copy, writes the `_state` tracker for any `State`-tracked files, and
/// upserts the `mods.yml` entry.
///
/// Shared `none`-tracked files (config) are left in place, and an existing one is
/// not overwritten. A failure after the removal leaves the mod without files
/// while `mods.yml` and its `_state` record still name it; reinstalling repairs
/// that, and uninstalling still works.
///
/// This is the apply-only, target-directory primitive; the full download
/// pipeline ([`install_mod_in`]) is layered on top of it.
#[allow(clippy::too_many_arguments)]
pub fn install_extracted_in(
  target_dir: &Path,
  eco: &Ecosystem,
  game: &str,
  full_name: &str,
  owner: &str,
  version: &Version,
  extracted_dir: &Path,
  archive: Option<&Path>,
  installed_at_time: u64,
) -> Result<()> {
  let game_profile = eco
    .game(game)
    .and_then(|g| g.profile())
    .ok_or_else(|| Error::Profile(format!("no ecosystem profile for game {game:?}")))?;

  let plan = plan_install(eco, game_profile, full_name, extracted_dir, archive)?;

  // Planned first so a package that cannot be routed fails before anything of
  // the installed copy is removed. Shared `none` routes are never named here,
  // so user config survives. The `_state` record outlives its payload until the
  // copy lands, so a failed apply still leaves the mod uninstallable, which
  // `ensure_removable` refuses for a loader with no record.
  clear_state_files(target_dir, full_name)?;
  remove_mod_folders(game_profile, target_dir, full_name)?;
  apply_install(&plan, target_dir)?;

  match state_tracker_from_plan(&plan, full_name) {
    Some(tracker) => write_state_file(target_dir, &tracker)?,
    None => remove_state_file(target_dir, full_name)?,
  }

  let mut mods = modlist::read(target_dir)?;

  modlist::upsert(
    &mut mods,
    mod_entry_from_version(full_name, owner, version, installed_at_time),
  );

  modlist::write(target_dir, &mods)
}

/// Installs an already-extracted package into an r2modman-layout profile
/// (`<base>/<game>/profiles/<profile_name>`). See [`install_extracted_in`].
#[allow(clippy::too_many_arguments)]
pub fn install_extracted(
  base: &Path,
  eco: &Ecosystem,
  game: &str,
  profile_name: &str,
  full_name: &str,
  owner: &str,
  version: &Version,
  extracted_dir: &Path,
  archive: Option<&Path>,
  installed_at_time: u64,
) -> Result<()> {
  install_extracted_in(
    &layout::profile_dir(base, game, profile_name),
    eco,
    game,
    full_name,
    owner,
    version,
    extracted_dir,
    archive,
    installed_at_time,
  )
}

/// Why a recorded mod's files cannot be removed exactly.
///
/// Returned rather than flattened into a message because the distinction drives
/// what a caller should advise: an absent record can be rebuilt by reinstalling,
/// while an unreadable one is a file the engine will not guess the contents of.
/// Only the wording of that advice belongs to the caller, and a caller that
/// words it itself should not have to re-derive the reason by re-reading the
/// record the engine has already read.
///
/// Deliberately **not** `#[non_exhaustive]`, unlike the rest of this crate's
/// public types, for the same reason [`launch::LaunchPlan::to_command`] gives: a
/// caller that words each refusal should stop compiling when a new reason
/// appears, rather than silently printing a generic fallback for a case nobody
/// worded. Designing that fallback is the thing this type exists to avoid.
#[derive(Debug)]
pub enum NotRemovable {
  /// A recognised mod loader with no install record at `path`, so which files it
  /// placed is unknown. Reinstalling the loader rebuilds the record.
  RecordAbsent {
    /// The mod that cannot be removed.
    name: String,
    /// Where the record was expected.
    path: PathBuf,
  },
  /// An install record exists at `path` but could not be read or parsed, so the
  /// engine does not know which files it tracked and will not guess.
  RecordUnreadable {
    /// The mod that cannot be removed.
    name: String,
    /// The record that could not be read.
    path: PathBuf,
    /// Why it could not be read.
    source: Error,
  },
}

impl std::fmt::Display for NotRemovable {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::RecordAbsent { name, path } => write!(
        f,
        "{name} is a mod loader with no install record at {}, so its files \
         cannot be removed exactly. Reinstall it, then uninstall it again, or \
         remove its files by hand.",
        path.display()
      ),
      Self::RecordUnreadable { name, path, source } => write!(
        f,
        "{name} has an install record at {} that cannot be read, so its files \
         cannot be removed exactly: {source}",
        path.display()
      ),
    }
  }
}

impl From<NotRemovable> for Error {
  fn from(value: NotRemovable) -> Self {
    Error::Profile(value.to_string())
  }
}

/// Whether `full_name`'s files can be removed exactly, reporting the reason
/// when they cannot.
///
/// A *missing* record is only a problem for a package whose files are `State`
/// tracked. A recognised mod loader always gets one when this engine installs
/// it, so a missing record means the target was adopted from disk rather than
/// installed and there is no way to know which files the loader placed;
/// uninstalling anyway would drop the `mods.yml` entry and leave every one of
/// its files orphaned. An ordinary mod, by contrast, has no record by design
/// under install rules with no `state` route, and `remove_delisted` reconciles
/// its folders by presence instead.
///
/// An *unreadable* record is always a problem, loader or not: the file exists,
/// so something tracked files, and guessing which is how an uninstall orphans or
/// over-deletes. That case arrives as the `Err` from
/// [`crate::install::read_state_file`], which is carried as
/// [`NotRemovable::RecordUnreadable`]'s `source` rather than propagated on its
/// own, so the two reasons reach a caller as one type it can match.
///
/// Returns [`NotRemovable`] rather than [`Error`] because the two reasons want
/// different advice and a caller wording that advice itself would otherwise have
/// to re-read the record to tell them apart. `?` still converts it for a caller
/// that does not care, via [`From<NotRemovable>`](NotRemovable).
///
/// Public and separate from [`uninstall_mod_in`] so a caller can pre-flight a
/// whole batch and refuse before touching the first mod. `uninstall_mod_in`
/// calls it too, so a single uninstall is protected whether or not the caller
/// does.
///
/// Takes no `game`: the only schema consulted is the mod-loader registry, which
/// [`Ecosystem::modloader_package`] keys on package identity rather than on a
/// game, so a `game` argument would go unread.
pub fn ensure_removable(
  target_dir: &Path,
  eco: &Ecosystem,
  full_name: &str,
) -> std::result::Result<(), NotRemovable> {
  match crate::install::read_state_file(target_dir, full_name) {
    Ok(Some(_)) => return Ok(()),
    Ok(None) => {}
    Err(source) => {
      return Err(NotRemovable::RecordUnreadable {
        name: full_name.to_string(),
        path: crate::install::state_file_path(target_dir, full_name),
        source,
      });
    }
  }

  if eco.modloader_package(full_name).is_none() {
    return Ok(());
  }

  Err(NotRemovable::RecordAbsent {
    name: full_name.to_string(),
    path: crate::install::state_file_path(target_dir, full_name),
  })
}

/// Uninstalls a mod from `target_dir`: removes its `mods.yml` entry,
/// reconciles the on-disk namespaced folders against the remaining set (so only
/// this mod's folders are removed), and removes its `State`-tracked payload
/// files along with its `_state` file.
///
/// [`ensure_removable`] gates the whole thing before anything is touched, so a
/// mod whose files cannot be removed exactly is refused rather than half
/// removed. See its doc for which records qualify.
///
/// Both removal paths are needed: `remove_delisted` reconciles the per-mod
/// `<Owner-Name>/` folders under namespaced routes, while `State`-tracked files
/// sit directly under a shared route (e.g. `BepInEx/core/`) and are only
/// recoverable from the `_state` record. This function does not special-case
/// the mod loader beyond the refusal above: naming it as `full_name` removes
/// it through these same two paths like any other mod, which is how callers
/// such as `vmm uninstall --all` uninstall it. Only shared config
/// (`none`-tracked) is unconditionally left untouched, since nothing this
/// function does ever names it for removal.
pub fn uninstall_mod_in(
  target_dir: &Path,
  eco: &Ecosystem,
  game: &str,
  full_name: &str,
) -> Result<()> {
  let game_profile = eco
    .game(game)
    .and_then(|g| g.profile())
    .ok_or_else(|| Error::Profile(format!("no ecosystem profile for game {game:?}")))?;

  // Before any mutation. `remove_state_payload` below reaches an unreadable
  // record on its own, but only after `remove_delisted` has already deleted this
  // mod's folders and before `mods.yml` is rewritten, so the entry would be left
  // naming files that are gone. Refusing here is what keeps both sides intact.
  ensure_removable(target_dir, eco, full_name)?;

  let mut mods = modlist::read(target_dir)?;

  mods.retain(|m| m.name != full_name);

  let keep: BTreeSet<String> = mods.iter().map(|m| m.name.clone()).collect();

  remove_delisted(game_profile, target_dir, &keep)?;
  remove_state_payload(target_dir, full_name)?;

  modlist::write(target_dir, &mods)
}

/// Uninstalls a mod from an r2modman-layout profile. See [`uninstall_mod_in`].
pub fn uninstall_mod(
  base: &Path,
  eco: &Ecosystem,
  game: &str,
  profile_name: &str,
  full_name: &str,
) -> Result<()> {
  uninstall_mod_in(
    &layout::profile_dir(base, game, profile_name),
    eco,
    game,
    full_name,
  )
}

/// Enables or disables an installed mod in `target_dir`: applies the on-disk
/// file toggle through the install layer, then flips `enabled` in `mods.yml`.
/// Errors if the mod is not present in `target_dir`.
///
/// A mod-loader pack (see [`Ecosystem::modloader_package`]) is a no-op here,
/// matching r2modman: loaders can't be disabled, so neither the on-disk files
/// nor its `enabled` flag are touched.
pub fn set_enabled_in(
  target_dir: &Path,
  eco: &Ecosystem,
  game: &str,
  full_name: &str,
  enabled: bool,
) -> Result<()> {
  let game_profile = eco
    .game(game)
    .and_then(|g| g.profile())
    .ok_or_else(|| Error::Profile(format!("no ecosystem profile for game {game:?}")))?;

  let mut mods = modlist::read(target_dir)?;

  let entry = mods
    .iter_mut()
    .find(|m| m.name == full_name)
    .ok_or_else(|| Error::Profile(format!("{full_name} is not installed")))?;

  if eco.modloader_package(full_name).is_some() {
    return Ok(());
  }

  if enabled {
    crate::install::enable_mod(game_profile, target_dir, full_name)?;
  } else {
    crate::install::disable_mod(game_profile, target_dir, full_name)?;
  }

  entry.enabled = enabled;

  modlist::write(target_dir, &mods)
}

/// Enables or disables an installed mod in an r2modman-layout profile. See
/// [`set_enabled_in`].
pub fn set_enabled(
  base: &Path,
  eco: &Ecosystem,
  game: &str,
  profile_name: &str,
  full_name: &str,
  enabled: bool,
) -> Result<()> {
  set_enabled_in(
    &layout::profile_dir(base, game, profile_name),
    eco,
    game,
    full_name,
    enabled,
  )
}

/// Enables an installed mod in `target_dir`. See [`set_enabled_in`].
pub fn enable_mod_in(
  target_dir: &Path,
  eco: &Ecosystem,
  game: &str,
  full_name: &str,
) -> Result<()> {
  set_enabled_in(target_dir, eco, game, full_name, true)
}

/// Disables an installed mod in `target_dir`. See [`set_enabled_in`].
pub fn disable_mod_in(
  target_dir: &Path,
  eco: &Ecosystem,
  game: &str,
  full_name: &str,
) -> Result<()> {
  set_enabled_in(target_dir, eco, game, full_name, false)
}

/// Enables an installed mod. See [`set_enabled`].
pub fn enable_mod(
  base: &Path,
  eco: &Ecosystem,
  game: &str,
  profile_name: &str,
  full_name: &str,
) -> Result<()> {
  set_enabled(base, eco, game, profile_name, full_name, true)
}

/// Disables an installed mod. See [`set_enabled`].
pub fn disable_mod(
  base: &Path,
  eco: &Ecosystem,
  game: &str,
  profile_name: &str,
  full_name: &str,
) -> Result<()> {
  set_enabled(base, eco, game, profile_name, full_name, false)
}

/// Installs a mod and its full dependency closure into `target_dir`: resolves
/// the download set against `index`, downloads the archives via `client`,
/// extracts each into `cache_base`'s per-game package cache, and installs
/// each into `target_dir` (updating `mods.yml`).
///
/// A package whose version `mods.yml` already records, and whose files that
/// version installs are still on disk, is skipped before it is downloaded, as
/// r2modman does, unless it is `full_name` itself and `force` is set. A recorded
/// package with missing files is reinstalled, which repairs a failed install or
/// a hand-deleted folder. Returns the sorted `Owner-Name` identifiers installed
/// and those skipped.
///
/// `cache_base` is deliberately independent of `target_dir`: packages are
/// extracted *from* it, never installed *into* it. See the module docs for
/// why it must persist across installs and, in server mode, must not be
/// `target_dir` itself.
#[allow(clippy::too_many_arguments)]
async fn install_one(
  target_dir: &Path,
  cache_base: &Path,
  eco: &Ecosystem,
  index: &PackageIndex,
  client: &ThunderstoreClient,
  game: &str,
  full_name: &str,
  force: bool,
  installed_at_time: u64,
) -> Result<(Vec<String>, Vec<String>)> {
  let resolution = DependencyGraph::new(vec![full_name.to_string()]).resolve_report(index);

  if !resolution.unresolved.is_empty() {
    return Err(Error::Profile(format!(
      "cannot install {full_name}: unresolved dependencies: {}",
      resolution.unresolved.join(", ")
    )));
  }

  let recorded = modlist::read(target_dir)?;
  let forced = force.then(|| crate::util::full_name_prefix(full_name));
  let downloads = client.cache_dir().join("downloads");
  let mut urls = HashMap::new();
  let mut unchanged = Vec::new();

  for (filename, url) in resolution.urls {
    let (pkg_full_name, version_str) = parse_package_filename(&filename)
      .ok_or_else(|| Error::Profile(format!("unparseable package filename {filename:?}")))?;

    let version = SemVer::parse(&version_str);
    let current = forced != Some(pkg_full_name.as_str())
      && recorded
        .iter()
        .find(|entry| entry.name == pkg_full_name && entry.version_number == version)
        .is_some_and(|entry| {
          recorded_files_present(
            target_dir,
            cache_base,
            eco,
            game,
            entry,
            &version_str,
            &downloads.join(&filename),
          )
        });

    if current {
      unchanged.push(pkg_full_name);
    } else {
      urls.insert(filename, url);
    }
  }

  client.download_files(urls.clone()).await?;

  let mut installed = Vec::new();

  for filename in urls.keys() {
    let (pkg_full_name, version_str) = parse_package_filename(filename)
      .ok_or_else(|| Error::Profile(format!("unparseable package filename {filename:?}")))?;

    let archive = downloads.join(filename);

    let extracted =
      cache::ensure_extracted(cache_base, game, &pkg_full_name, &version_str, &archive)?;

    let package = index
      .get_package_by_full_name(&pkg_full_name)
      .ok_or_else(|| Error::Profile(format!("{pkg_full_name} missing from index")))?;

    let owner = package.owner.clone().unwrap_or_default();
    let version = package
      .versions
      .iter()
      .find(|v| v.version_number.as_deref() == Some(version_str.as_str()))
      .or_else(|| package.latest_version())
      .ok_or_else(|| Error::Profile(format!("{pkg_full_name} has no usable version")))?;

    install_extracted_in(
      target_dir,
      eco,
      game,
      &pkg_full_name,
      &owner,
      version,
      &extracted,
      Some(&archive),
      installed_at_time,
    )?;

    installed.push(pkg_full_name);
  }

  installed.sort();
  unchanged.sort();

  Ok((installed, unchanged))
}

/// Whether the files `entry` installed at `version` are still in place, judged
/// from the package's cached extraction. A missing cache, an unknown game or a
/// package that no longer plans counts as not in place, so the install runs and
/// either repairs the mod or reports why it cannot.
fn recorded_files_present(
  target_dir: &Path,
  cache_base: &Path,
  eco: &Ecosystem,
  game: &str,
  entry: &ProfileMod,
  version: &str,
  archive: &Path,
) -> bool {
  let extracted = cache::package_cache_dir(cache_base, game, &entry.name, version);

  if !extracted.is_dir() {
    return false;
  }

  let Some(game_profile) = eco.game(game).and_then(|g| g.profile()) else {
    return false;
  };

  let archive = archive.exists().then_some(archive);

  plan_install(eco, game_profile, &entry.name, &extracted, archive).is_ok_and(|plan| {
    crate::install::plan_is_on_disk(&plan, target_dir, &entry.name, entry.enabled)
  })
}

/// Installs every identifier in `batch`, restoring protected disabled state
/// afterwards, and reports per-item outcomes.
///
/// Failures do not abort the batch: each item is attempted and its reason
/// recorded, because an operation that already installed earlier items has to
/// say which ones. The disabled-state restore runs regardless, since items
/// installed before a failure are already enabled. A requested enable that
/// fails is recorded the same way.
#[allow(clippy::too_many_arguments)]
pub async fn install_batch(
  target_dir: &Path,
  cache_base: &Path,
  eco: &Ecosystem,
  index: &PackageIndex,
  client: &ThunderstoreClient,
  game: &str,
  batch: &InstallBatch,
  installed_at_time: u64,
) -> Result<BatchOutcome> {
  let mut outcome = BatchOutcome::default();

  for full_name in &batch.desired {
    match install_one(
      target_dir,
      cache_base,
      eco,
      index,
      client,
      game,
      full_name,
      batch.force,
      installed_at_time,
    )
    .await
    {
      Ok((installed, unchanged)) => {
        outcome.succeeded.extend(installed);
        outcome.unchanged.extend(unchanged);
      }
      Err(error) => outcome.failed.push((full_name.clone(), error)),
    }
  }

  reapply_batch_disabled(target_dir, eco, game, batch)?;

  outcome.succeeded.sort();
  outcome.succeeded.dedup();
  outcome.unchanged.sort();
  outcome.unchanged.dedup();
  outcome
    .unchanged
    .retain(|name| outcome.succeeded.binary_search(name).is_err());

  // A reinstalled mod is already enabled; only one skipped as current can still
  // carry the disabled state the caller asked to clear.
  let recorded = modlist::read(target_dir)?;
  let disabled: BTreeSet<&str> = recorded
    .iter()
    .filter(|entry| !entry.enabled)
    .map(|entry| entry.name.as_str())
    .collect();

  for full_name in &batch.enable_requested {
    if disabled.contains(full_name.as_str())
      && outcome.unchanged.binary_search(full_name).is_ok()
      && let Err(error) = set_enabled_in(target_dir, eco, game, full_name, true)
    {
      outcome.failed.push((full_name.clone(), error));
    }
  }

  // A mod whose enable failed is reported as failed, not also as unchanged.
  let failed: BTreeSet<&String> = outcome.failed.iter().map(|(name, _)| name).collect();

  outcome.unchanged.retain(|name| !failed.contains(name));

  Ok(outcome)
}

/// Installs one mod and its dependency closure, protecting the disabled state of
/// every recorded mod it did not name. See [`install_batch`].
///
/// Returns every `Owner-Name` actually installed, which is the named package
/// plus its transitive dependencies. A package already recorded at the version
/// that would be installed, with its files still on disk, is skipped and not
/// listed, so this is empty when the whole closure is current; use
/// [`install_batch`] with [`InstallBatch::force`] to reinstall regardless.
///
/// `cache_base` must not be `target_dir` or a directory inside it, or extracted
/// packages would land inside the installed mod tree.
///
/// # Errors
///
/// - [`Error::Http`] or
///   [`Error::Download`] if fetching the package or a
///   dependency fails.
/// - [`Error::Install`] if the ecosystem has no install
///   rules for `game`, or a package's files cannot be routed.
/// - [`Error::Io`] if `target_dir` or `cache_base` cannot be
///   written.
///
/// A package that is absent from `index` is skipped rather than reported here;
/// see [`crate::models::DependencyGraph::resolve_report`] to detect that.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use thunderstore_engine::client::ThunderstoreClient;
/// use thunderstore_engine::ecosystem::Ecosystem;
/// use thunderstore_engine::profile::{install_mod_in, modlist};
///
/// # async fn run() -> thunderstore_engine::Result<()> {
/// let client = ThunderstoreClient::builder()
///   .community("valheim")
///   .cache_dir("/var/cache/my-mod-manager")
///   .build()?;
///
/// let index = client.get_manifest().await?;
/// let eco = Ecosystem::bundled();
///
/// let installed = install_mod_in(
///   Path::new("/srv/valheim"),
///   Path::new("/var/cache/my-mod-manager"),
///   &eco,
///   &index,
///   &client,
///   "valheim",
///   "denikson-BepInExPack_Valheim",
///   modlist::now_millis(),
/// )
/// .await?;
///
/// println!("installed {} packages", installed.len());
/// # Ok(())
/// # }
/// ```
#[allow(clippy::too_many_arguments)]
pub async fn install_mod_in(
  target_dir: &Path,
  cache_base: &Path,
  eco: &Ecosystem,
  index: &PackageIndex,
  client: &ThunderstoreClient,
  game: &str,
  full_name: &str,
  installed_at_time: u64,
) -> Result<Vec<String>> {
  install_named_in(
    target_dir,
    cache_base,
    eco,
    index,
    client,
    game,
    full_name,
    false,
    installed_at_time,
  )
  .await
}

/// Installs one named mod and its dependency closure, surfacing the first
/// per-item failure as the error. With `force`, the named mod is reinstalled
/// even when `mods.yml` already records its version. Shared by
/// [`install_mod_in`] and adoption, which forces the loader reinstall.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn install_named_in(
  target_dir: &Path,
  cache_base: &Path,
  eco: &Ecosystem,
  index: &PackageIndex,
  client: &ThunderstoreClient,
  game: &str,
  full_name: &str,
  force: bool,
  installed_at_time: u64,
) -> Result<Vec<String>> {
  let named = [full_name.to_string()];
  let mut batch = plan_install_batch(target_dir, &named, &named)?;

  batch.force = force;

  let outcome = install_batch(
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

  match outcome.failed.into_iter().next() {
    Some((_, error)) => Err(error),
    None => Ok(outcome.succeeded),
  }
}

/// Installs a mod and its full dependency closure into an r2modman-layout
/// profile, using `base` as both the profile base and the package cache
/// base (matching this pipeline's pre-existing behavior). See
/// [`install_mod_in`].
#[allow(clippy::too_many_arguments)]
pub async fn install_mod(
  base: &Path,
  eco: &Ecosystem,
  index: &PackageIndex,
  client: &ThunderstoreClient,
  game: &str,
  profile_name: &str,
  full_name: &str,
  installed_at_time: u64,
) -> Result<Vec<String>> {
  install_mod_in(
    &layout::profile_dir(base, game, profile_name),
    base,
    eco,
    index,
    client,
    game,
    full_name,
    installed_at_time,
  )
  .await
}

/// What an uninstall batch will remove.
///
/// Mods and loaders are held apart because a caller showing this to a user needs
/// to say the loader goes too, and because removing loaders last keeps every
/// intermediate state one a user could have reached by hand. Ordinarily there is
/// at most one loader, but the registry is keyed on package identity rather than
/// game, so nothing rules out a second.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct UninstallBatch {
  /// Every non-loader identifier to remove, sorted.
  pub mods: Vec<String>,
  /// Every recognised mod loader to remove, sorted, removed after `mods`.
  pub loaders: Vec<String>,
  /// Root-relative mod folders the record does not account for, which the
  /// reconcile sweep removes along with the named mods.
  pub untracked: Vec<PathBuf>,
}

/// Every one of `names` that cannot be removed exactly, with the reason.
///
/// Separate from [`plan_uninstall_batch`], which refuses outright, so a caller
/// can list every problem at once instead of discovering them one at a time. The
/// reason is a [`NotRemovable`] rather than an [`Error`] so a caller can word
/// each kind of refusal without re-deriving which kind it is.
pub fn check_removable_batch(
  target_dir: &Path,
  eco: &Ecosystem,
  names: &[String],
) -> Vec<(String, NotRemovable)> {
  names
    .iter()
    .filter_map(|name| match ensure_removable(target_dir, eco, name) {
      Ok(()) => None,
      Err(refusal) => Some((name.clone(), refusal)),
    })
    .collect()
}

/// Plans an uninstall, refusing before anything is mutated.
///
/// `names` of `None` means every recorded mod. A name that is not installed, or
/// any mod that cannot be removed exactly, is an error rather than a per-item
/// failure: this runs before any mutation, so refusing the whole request is what
/// keeps a batch from failing partway through.
pub fn plan_uninstall_batch(
  target_dir: &Path,
  eco: &Ecosystem,
  game: &str,
  names: Option<&[String]>,
) -> Result<UninstallBatch> {
  let game_profile = eco
    .game(game)
    .and_then(|g| g.profile())
    .ok_or_else(|| Error::Profile(format!("no ecosystem profile for game {game:?}")))?;

  let recorded = modlist::read(target_dir)?;

  let subjects: Vec<String> = match names {
    Some(names) => {
      for name in names {
        if modlist::find(&recorded, name).is_none() {
          return Err(Error::Profile(format!("{name} is not installed")));
        }
      }

      names.to_vec()
    }
    None => recorded.iter().map(|entry| entry.name.clone()).collect(),
  };

  if let Some((name, refusal)) = check_removable_batch(target_dir, eco, &subjects)
    .into_iter()
    .next()
  {
    return Err(Error::Profile(format!(
      "{name} cannot be removed: {refusal}"
    )));
  }

  let mut mods: Vec<String> = subjects
    .iter()
    .filter(|name| eco.modloader_package(name).is_none())
    .cloned()
    .collect();
  let mut loaders: Vec<String> = subjects
    .iter()
    .filter(|name| eco.modloader_package(name).is_some())
    .cloned()
    .collect();

  mods.sort();
  loaders.sort();

  let keep: BTreeSet<String> = recorded.iter().map(|entry| entry.name.clone()).collect();
  let untracked = crate::install::plan_delisted(game_profile, target_dir, &keep)?;

  Ok(UninstallBatch {
    mods,
    loaders,
    untracked,
  })
}

/// Removes everything `batch` names, loaders last, reporting per-item outcomes.
///
/// Each removal re-derives its keep-set from the remaining record, so ordering is
/// not required for correctness; loaders go last so every intermediate state is
/// coherent.
pub fn uninstall_batch(
  target_dir: &Path,
  eco: &Ecosystem,
  game: &str,
  batch: &UninstallBatch,
) -> Result<BatchOutcome> {
  let mut outcome = BatchOutcome::default();

  for full_name in batch.mods.iter().chain(batch.loaders.iter()) {
    match uninstall_mod_in(target_dir, eco, game, full_name) {
      Ok(()) => outcome.succeeded.push(full_name.clone()),
      Err(error) => outcome.failed.push((full_name.clone(), error)),
    }
  }

  Ok(outcome)
}

/// What a bulk enable or disable will do.
///
/// Loaders are excluded rather than merely skipped. `set_enabled_in` already
/// no-ops a loader, so including one would be harmless, but it would be counted
/// as a mod the operation acted on and any summary built from this would overstate
/// what happened.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ToggleBatch {
  /// Identifiers whose state will change, sorted.
  pub changing: Vec<String>,
  /// Identifiers already in the requested state, sorted.
  pub already: Vec<String>,
  /// Recognised loaders, excluded from the toggle entirely, sorted.
  pub loaders: Vec<String>,
  /// The sense this batch was planned for: `true` to enable, `false` to
  /// disable. `set_enabled_batch` applies this instead of taking its own
  /// `enabled` argument, so a caller cannot execute a batch against a sense it
  /// was not planned for.
  pub enabled: bool,
}

/// Plans a bulk enable or disable. `names` of `None` means every recorded mod.
///
/// A name in `names` that is not recorded at all is refused outright, in the
/// same shape as [`plan_uninstall_batch`]'s refusal, rather than silently
/// producing an empty plan: iterating the record and filtering would let an
/// unrecorded name simply never appear, indistinguishable from "nothing to
/// do".
pub fn plan_set_enabled_batch(
  target_dir: &Path,
  eco: &Ecosystem,
  names: Option<&[String]>,
  enabled: bool,
) -> Result<ToggleBatch> {
  let recorded = modlist::read(target_dir)?;

  if let Some(names) = names {
    for name in names {
      if modlist::find(&recorded, name).is_none() {
        return Err(Error::Profile(format!("{name} is not installed")));
      }
    }
  }

  let mut batch = ToggleBatch {
    enabled,
    ..ToggleBatch::default()
  };

  for entry in &recorded {
    if let Some(names) = names
      && !names.iter().any(|name| name == &entry.name)
    {
      continue;
    }

    if eco.modloader_package(&entry.name).is_some() {
      batch.loaders.push(entry.name.clone());
    } else if entry.enabled == enabled {
      batch.already.push(entry.name.clone());
    } else {
      batch.changing.push(entry.name.clone());
    }
  }

  batch.changing.sort();
  batch.already.sort();
  batch.loaders.sort();

  Ok(batch)
}

/// Applies `batch`, reporting per-item outcomes.
///
/// One mod's failure does not abort the rest: returning early would leave a
/// half-applied toggle with nothing reported. Applies `batch.enabled`, the
/// sense it was planned for, rather than taking its own `enabled` argument, so
/// the executor cannot disagree with the plan it is executing.
pub fn set_enabled_batch(
  target_dir: &Path,
  eco: &Ecosystem,
  game: &str,
  batch: &ToggleBatch,
) -> Result<BatchOutcome> {
  let mut outcome = BatchOutcome::default();

  for full_name in &batch.changing {
    match set_enabled_in(target_dir, eco, game, full_name, batch.enabled) {
      Ok(()) => outcome.succeeded.push(full_name.clone()),
      Err(error) => outcome.failed.push((full_name.clone(), error)),
    }
  }

  Ok(outcome)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ecosystem::Ecosystem;
  use crate::install::state_file_path;
  use crate::models::Version;
  use std::io::{Cursor, Write};
  use tempfile::tempdir;
  use time::OffsetDateTime;
  use zip::write::{FileOptions, ZipWriter};

  fn write_file(path: &std::path::Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::File::create(path)
      .unwrap()
      .write_all(bytes)
      .unwrap();
  }

  fn version(full_name: &str) -> Version {
    Version {
      name: Some("Mod".to_string()),
      full_name: Some(full_name.to_string()),
      description: Some("desc".to_string()),
      icon: Some("icon.png".to_string()),
      version_number: Some("1.2.3".to_string()),
      dependencies: vec!["Other-Dep-1.0.0".to_string()],
      download_url: Some("https://example.com/dl".to_string()),
      downloads: Some(1),
      date_created: OffsetDateTime::UNIX_EPOCH,
      website_url: Some("https://example.com".to_string()),
      is_active: Some(true),
      uuid4: Some("u".to_string()),
      file_size: Some(1),
    }
  }

  #[test]
  fn mod_entry_maps_version_with_r2modman_defaults() {
    let entry = mod_entry_from_version("Author-Mod", "Author", &version("Author-Mod"), 123);

    assert_eq!(entry.name, "Author-Mod");
    assert_eq!(entry.author_name, "Author");
    assert_eq!(entry.network_mode, "both");
    assert_eq!(entry.package_type, "other");
    assert_eq!(entry.install_mode, "managed");
    assert_eq!(entry.game_version, "0");
    assert_eq!(entry.manifest_version, 1);
    assert_eq!(
      entry.version_number,
      crate::profile::modlist::SemVer {
        major: 1,
        minor: 2,
        patch: 3
      }
    );
    assert_eq!(entry.dependencies, vec!["Other-Dep-1.0.0".to_string()]);
    assert_eq!(entry.installed_at_time, 123);
    assert!(entry.enabled);
  }

  #[test]
  fn install_extracted_places_files_and_records_mods_yml() {
    let eco = Ecosystem::bundled();

    let dir = tempdir().unwrap();
    let base = dir.path();

    let extracted = tempdir().unwrap();
    write_file(&extracted.path().join("plugins/Mod.dll"), b"dll");
    write_file(&extracted.path().join("manifest.json"), b"{}");

    crate::profile::layout::create(base, "valheim", "Main").unwrap();

    install_extracted(
      base,
      &eco,
      "valheim",
      "Main",
      "Author-Mod",
      "Author",
      &version("Author-Mod"),
      extracted.path(),
      None,
      123,
    )
    .unwrap();

    let profile_dir = crate::profile::layout::profile_dir(base, "valheim", "Main");

    assert!(
      profile_dir
        .join("BepInEx/plugins/Author-Mod/Mod.dll")
        .exists()
    );

    let mods = crate::profile::modlist::read(&profile_dir).unwrap();

    assert_eq!(mods.len(), 1);
    assert_eq!(mods[0].name, "Author-Mod");
    assert!(mods[0].enabled);
  }

  #[test]
  fn uninstall_removes_files_and_mods_yml_entry() {
    let eco = Ecosystem::bundled();

    let dir = tempdir().unwrap();
    let base = dir.path();

    let extracted = tempdir().unwrap();
    write_file(&extracted.path().join("plugins/Mod.dll"), b"dll");

    crate::profile::layout::create(base, "valheim", "Main").unwrap();

    install_extracted(
      base,
      &eco,
      "valheim",
      "Main",
      "Author-Mod",
      "Author",
      &version("Author-Mod"),
      extracted.path(),
      None,
      1,
    )
    .unwrap();

    let profile_dir = crate::profile::layout::profile_dir(base, "valheim", "Main");

    assert!(
      profile_dir
        .join("BepInEx/plugins/Author-Mod/Mod.dll")
        .exists()
    );

    uninstall_mod(base, &eco, "valheim", "Main", "Author-Mod").unwrap();

    assert!(!profile_dir.join("BepInEx/plugins/Author-Mod").exists());
    assert_eq!(
      crate::profile::modlist::read(&profile_dir).unwrap().len(),
      0
    );
  }

  #[test]
  fn uninstall_refuses_a_mod_whose_install_record_cannot_be_read() {
    let eco = Ecosystem::bundled();

    let dir = tempdir().unwrap();
    let base = dir.path();

    let extracted = tempdir().unwrap();
    write_file(&extracted.path().join("plugins/Mod.dll"), b"dll");

    crate::profile::layout::create(base, "valheim", "Main").unwrap();
    install_extracted(
      base,
      &eco,
      "valheim",
      "Main",
      "Author-Mod",
      "Author",
      &version("Author-Mod"),
      extracted.path(),
      None,
      1,
    )
    .unwrap();

    let profile_dir = crate::profile::layout::profile_dir(base, "valheim", "Main");

    // An ordinary mod routes entirely through its namespaced folder, so it gets
    // no tracker of its own. Nothing stops one existing anyway (a stray
    // leftover, or one written by hand), so an unparseable one is planted here.
    write_file(&state_file_path(&profile_dir, "Author-Mod"), b"not: [valid");

    let error = uninstall_mod(base, &eco, "valheim", "Main", "Author-Mod").unwrap_err();

    // `remove_state_payload` reaches the same unparseable record on its own, but
    // only after `remove_delisted` has deleted the folder and before `mods.yml`
    // is rewritten, which leaves the entry naming files that are already gone.
    // Refusing up front is what keeps both sides intact.
    assert!(
      profile_dir
        .join("BepInEx/plugins/Author-Mod/Mod.dll")
        .exists(),
      "the mod's files must survive a refusal; got error: {error}"
    );
    assert_eq!(
      crate::profile::modlist::read(&profile_dir).unwrap().len(),
      1
    );
  }

  #[test]
  fn disable_then_enable_updates_disk_and_mods_yml() {
    let eco = Ecosystem::bundled();

    let dir = tempdir().unwrap();
    let base = dir.path();

    let extracted = tempdir().unwrap();
    write_file(&extracted.path().join("plugins/Mod.dll"), b"dll");

    crate::profile::layout::create(base, "valheim", "Main").unwrap();
    install_extracted(
      base,
      &eco,
      "valheim",
      "Main",
      "Author-Mod",
      "Author",
      &version("Author-Mod"),
      extracted.path(),
      None,
      1,
    )
    .unwrap();

    let profile_dir = crate::profile::layout::profile_dir(base, "valheim", "Main");

    disable_mod(base, &eco, "valheim", "Main", "Author-Mod").unwrap();

    assert!(
      profile_dir
        .join("BepInEx/plugins/Author-Mod/Mod.dll.old")
        .exists()
    );
    assert!(!crate::profile::modlist::read(&profile_dir).unwrap()[0].enabled);

    enable_mod(base, &eco, "valheim", "Main", "Author-Mod").unwrap();

    assert!(
      profile_dir
        .join("BepInEx/plugins/Author-Mod/Mod.dll")
        .exists()
    );
    assert!(crate::profile::modlist::read(&profile_dir).unwrap()[0].enabled);
  }

  #[test]
  fn disable_leaves_files_alone_when_the_install_record_cannot_be_read() {
    let eco = Ecosystem::bundled();

    let dir = tempdir().unwrap();
    let base = dir.path();

    let extracted = tempdir().unwrap();
    write_file(&extracted.path().join("plugins/Mod.dll"), b"dll");

    crate::profile::layout::create(base, "valheim", "Main").unwrap();
    install_extracted(
      base,
      &eco,
      "valheim",
      "Main",
      "Author-Mod",
      "Author",
      &version("Author-Mod"),
      extracted.path(),
      None,
      1,
    )
    .unwrap();

    let profile_dir = crate::profile::layout::profile_dir(base, "valheim", "Main");

    write_file(&state_file_path(&profile_dir, "Author-Mod"), b"not: [valid");

    let error = disable_mod(base, &eco, "valheim", "Main", "Author-Mod").unwrap_err();

    // An unreadable record already refused a disable, but only once the `.old`
    // rename pass had run, and `mods.yml` is written last of all. That left the
    // files disabled on disk and the entry still claiming `enabled`. Reading the
    // record up front is what keeps the two in step.
    assert!(
      profile_dir
        .join("BepInEx/plugins/Author-Mod/Mod.dll")
        .exists(),
      "the mod's files must be left as they were; got error: {error}"
    );
    assert!(
      !profile_dir
        .join("BepInEx/plugins/Author-Mod/Mod.dll.old")
        .exists()
    );
    assert!(crate::profile::modlist::read(&profile_dir).unwrap()[0].enabled);
  }

  #[test]
  fn disable_mod_is_noop_for_modloader_pack() {
    let eco = Ecosystem::bundled();

    let dir = tempdir().unwrap();
    let base = dir.path();
    let full_name = "denikson-BepInExPack_Valheim";

    crate::profile::layout::create(base, "valheim", "Main").unwrap();

    let profile_dir = crate::profile::layout::profile_dir(base, "valheim", "Main");
    let loader_file = profile_dir.join("winhttp.dll");

    write_file(&loader_file, b"dll");

    let mut mods = Vec::new();

    modlist::upsert(
      &mut mods,
      mod_entry_from_version(full_name, "denikson", &version(full_name), 1),
    );
    modlist::write(&profile_dir, &mods).unwrap();

    let tracker = crate::install::ModFileTracker {
      mod_name: full_name.to_string(),
      files: vec![("unused-cache-source".to_string(), "winhttp.dll".to_string())],
    };

    write_state_file(&profile_dir, &tracker).unwrap();

    disable_mod(base, &eco, "valheim", "Main", full_name).unwrap();

    assert!(loader_file.exists());
    assert!(crate::profile::modlist::read(&profile_dir).unwrap()[0].enabled);
  }

  #[test]
  fn install_mod_fails_on_unresolved_dependency() {
    let eco = Ecosystem::bundled();

    let package = crate::models::Package {
      name: Some("Mod".to_string()),
      full_name: Some("Author-Mod".to_string()),
      owner: Some("Author".to_string()),
      package_url: Some("https://example.com/Author-Mod".to_string()),
      date_created: OffsetDateTime::UNIX_EPOCH,
      date_updated: OffsetDateTime::UNIX_EPOCH,
      uuid4: Some("u".to_string()),
      rating_score: Some(0),
      is_pinned: Some(false),
      is_deprecated: Some(false),
      has_nsfw_content: Some(false),
      categories: vec![],
      versions: vec![Version {
        dependencies: vec!["Missing-Dep-1.0.0".to_string()],
        ..version("Author-Mod")
      }],
    };

    let index: PackageIndex = vec![package].into();

    let dir = tempdir().unwrap();
    let base = dir.path();
    let cache_dir = tempdir().unwrap();

    let client = ThunderstoreClient::builder()
      .package_index_url("http://localhost/pkg/")
      .cache_dir(cache_dir.path())
      .build()
      .unwrap();

    crate::profile::layout::create(base, "valheim", "Main").unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(install_mod(
      base,
      &eco,
      &index,
      &client,
      "valheim",
      "Main",
      "Author-Mod",
      1,
    ));

    let err = result.unwrap_err().to_string();

    assert!(err.contains("Missing-Dep"), "unexpected error: {err}");
  }

  #[test]
  fn server_mode_lifecycle_against_bare_target_dir() {
    let eco = Ecosystem::bundled();

    let dir = tempdir().unwrap();
    let target_dir = dir.path();

    let extracted = tempdir().unwrap();
    write_file(&extracted.path().join("plugins/Mod.dll"), b"dll");

    install_extracted_in(
      target_dir,
      &eco,
      "valheim",
      "Author-Mod",
      "Author",
      &version("Author-Mod"),
      extracted.path(),
      None,
      123,
    )
    .unwrap();

    assert!(
      target_dir
        .join("BepInEx/plugins/Author-Mod/Mod.dll")
        .exists()
    );
    assert!(target_dir.join("mods.yml").exists());

    let mods = modlist::read(target_dir).unwrap();

    assert_eq!(mods.len(), 1);
    assert_eq!(mods[0].name, "Author-Mod");
    assert!(mods[0].enabled);

    // Simulate a `State`-tracked file alongside the `Subdir`-tracked plugin
    // above (e.g. a MelonLoader-style loose `.dll`), proving `_state` I/O
    // resolves entirely against `target_dir` with no r2modman profile layout
    // involved: the source is a real file so enable's restore-from-cache
    // copy succeeds, and the dest starts out present so disable's removal is
    // a real effect, not a no-op.
    let state_source = extracted.path().join("state-source.txt");

    write_file(&state_source, b"state-data");
    write_file(&target_dir.join("state-dest.txt"), b"state-data");

    let tracker = crate::install::ModFileTracker {
      mod_name: "Author-Mod".to_string(),
      files: vec![(
        state_source.to_string_lossy().to_string(),
        "state-dest.txt".to_string(),
      )],
    };

    write_state_file(target_dir, &tracker).unwrap();

    assert!(state_file_path(target_dir, "Author-Mod").exists());

    disable_mod_in(target_dir, &eco, "valheim", "Author-Mod").unwrap();

    assert!(
      target_dir
        .join("BepInEx/plugins/Author-Mod/Mod.dll.old")
        .exists()
    );
    assert!(!target_dir.join("state-dest.txt").exists());
    assert!(!modlist::read(target_dir).unwrap()[0].enabled);

    enable_mod_in(target_dir, &eco, "valheim", "Author-Mod").unwrap();

    assert!(
      target_dir
        .join("BepInEx/plugins/Author-Mod/Mod.dll")
        .exists()
    );
    assert!(target_dir.join("state-dest.txt").exists());
    assert!(modlist::read(target_dir).unwrap()[0].enabled);

    uninstall_mod_in(target_dir, &eco, "valheim", "Author-Mod").unwrap();

    assert!(!target_dir.join("BepInEx/plugins/Author-Mod").exists());
    // The `State`-tracked payload lives under a shared route, so `remove_delisted`
    // never reconciles it — uninstall must remove it from the `_state` record
    // before discarding that record, or the file would be orphaned untracked.
    assert!(!target_dir.join("state-dest.txt").exists());
    assert!(!state_file_path(target_dir, "Author-Mod").exists());
    assert_eq!(modlist::read(target_dir).unwrap().len(), 0);
  }

  fn build_mod_zip() -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let opts: FileOptions<'_, ()> =
      FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    for (name, body) in [
      ("manifest.json", &b"{\"version_number\":\"1.0.0\"}"[..]),
      ("plugins/CoolMod.dll", &b"dll-bytes"[..]),
    ] {
      zip.start_file(name, opts).unwrap();
      zip.write_all(body).unwrap();
    }

    zip.finish().unwrap().into_inner()
  }

  fn index_json(server_url: &str) -> String {
    format!(
      r#"[{{"name":"CoolMod","full_name":"Author-CoolMod","owner":"Author","package_url":"https://example.com/CoolMod","date_created":"2024-01-01T12:00:00Z","date_updated":"2024-01-02T12:00:00Z","uuid4":"pkg","rating_score":1,"is_pinned":false,"is_deprecated":false,"has_nsfw_content":false,"categories":[],"versions":[{{"name":"CoolMod","full_name":"Author-CoolMod","description":"A cool mod","icon":"icon.png","version_number":"1.0.0","dependencies":[],"download_url":"{server_url}/dl/mod.zip","downloads":1,"date_created":"2024-01-01T12:00:00Z","website_url":"","is_active":true,"uuid4":"ver","file_size":1024}}]}}]"#
    )
  }

  #[test]
  fn install_mod_in_extracts_into_cache_base_not_target_dir() {
    let mut server = mockito::Server::new();
    let zip_bytes = build_mod_zip();

    let dl = server
      .mock("GET", "/dl/mod.zip")
      .with_status(200)
      .with_header("Content-Type", "application/zip")
      .with_body(zip_bytes)
      .create();
    let idx = server
      .mock("GET", "/pkg/")
      .with_status(200)
      .with_header("Content-Type", "application/json")
      .with_header("Last-Modified", "Wed, 21 Feb 2024 15:30:45 GMT")
      .with_body(index_json(&server.url()))
      .create();

    let http_cache = tempdir().unwrap();
    let cache_base = tempdir().unwrap();
    let target = tempdir().unwrap();

    let client = ThunderstoreClient::builder()
      .package_index_url(format!("{}/pkg/", server.url()))
      .cache_dir(http_cache.path())
      .build()
      .unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let eco = Ecosystem::bundled();
    let index = rt.block_on(client.get_manifest()).unwrap();

    let installed = rt
      .block_on(install_mod_in(
        target.path(),
        cache_base.path(),
        &eco,
        &index,
        &client,
        "valheim",
        "Author-CoolMod",
        123,
      ))
      .unwrap();

    assert_eq!(installed, vec!["Author-CoolMod".to_string()]);

    assert!(
      target
        .path()
        .join("BepInEx/plugins/Author-CoolMod/CoolMod.dll")
        .exists()
    );

    let cached = cache::package_cache_dir(cache_base.path(), "valheim", "Author-CoolMod", "1.0.0");

    assert!(cached.join("plugins/CoolMod.dll").exists());
    assert!(!target.path().join("cache").exists());

    dl.assert();
    idx.assert();
  }

  /// Builds a minimal [`ProfileMod`] for a hand-built `mods.yml` entry.
  /// `ProfileMod` does not derive `Default`, so every field is filled with a
  /// plausible value.
  fn profile_mod(name: &str) -> ProfileMod {
    ProfileMod {
      manifest_version: 1,
      name: name.to_string(),
      author_name: String::new(),
      website_url: String::new(),
      display_name: name.to_string(),
      description: String::new(),
      game_version: "0".to_string(),
      network_mode: "both".to_string(),
      package_type: "other".to_string(),
      install_mode: "managed".to_string(),
      installed_at_time: 1,
      loaders: Vec::new(),
      dependencies: Vec::new(),
      incompatibilities: Vec::new(),
      optional_dependencies: Vec::new(),
      version_number: SemVer::parse("1.0.0"),
      enabled: true,
      icon: String::new(),
      online_source: false,
      trusted_package: false,
    }
  }

  #[test]
  fn uninstalling_a_loader_pack_without_a_state_file_is_refused() {
    let dir = tempdir().unwrap();
    let target_dir = dir.path();
    let eco = crate::ecosystem::Ecosystem::bundled();

    // A `mods.yml` naming the loader, with no `_state` tracker beside it: the
    // shape `import_r2modman_dir` leaves behind, since r2modman never wrote
    // this engine's tracker.
    modlist::write(target_dir, &[profile_mod("denikson-BepInExPack_Valheim")]).unwrap();

    let result = uninstall_mod_in(target_dir, &eco, "valheim", "denikson-BepInExPack_Valheim");

    assert!(result.is_err());

    let message = result.unwrap_err().to_string();

    assert!(
      message.contains("denikson-BepInExPack_Valheim"),
      "the error must name the loader; got: {message}"
    );
    // Refusing must leave the record intact rather than dropping it, which is
    // the orphaning this guard exists to prevent.
    assert_eq!(modlist::read(target_dir).unwrap().len(), 1);
  }

  #[test]
  fn uninstalling_a_regular_mod_without_a_state_file_still_works() {
    let dir = tempdir().unwrap();
    let target_dir = dir.path();
    let eco = crate::ecosystem::Ecosystem::bundled();

    // Valheim's install rules are `subdir` and `none` only, so a regular mod
    // never has a `_state` file. That must stay a normal uninstall.
    modlist::write(target_dir, &[profile_mod("Owner-Regular")]).unwrap();

    uninstall_mod_in(target_dir, &eco, "valheim", "Owner-Regular").unwrap();

    assert!(modlist::read(target_dir).unwrap().is_empty());
  }

  #[test]
  fn check_removable_batch_reports_every_unremovable_name_with_its_reason() {
    let dir = tempdir().unwrap();
    let target_dir = dir.path();
    let eco = crate::ecosystem::Ecosystem::bundled();

    modlist::write(
      target_dir,
      &[
        profile_mod("denikson-BepInExPack_Valheim"),
        profile_mod("Owner-Regular"),
      ],
    )
    .unwrap();

    // The loader has no `_state` tracker at all: adopted from disk rather than
    // installed, so which files it placed is unknown.
    //
    // The regular mod's `_state` file is present but unparseable: a stray
    // leftover or a hand edit, so its contents are unreadable rather than
    // absent. Different reason, same "cannot be removed exactly" outcome.
    let state_path = crate::install::state_file_path(target_dir, "Owner-Regular");

    std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    std::fs::write(&state_path, "not: [valid").unwrap();

    let names = vec![
      "denikson-BepInExPack_Valheim".to_string(),
      "Owner-Regular".to_string(),
    ];

    let blocked = check_removable_batch(target_dir, &eco, &names);

    assert_eq!(
      blocked.len(),
      2,
      "both un-removable names must be reported, not just the first; got {blocked:?}"
    );

    let loader_refusal = blocked
      .iter()
      .find(|(name, _)| name == "denikson-BepInExPack_Valheim")
      .map(|(_, refusal)| refusal)
      .unwrap();
    let loader_path = state_file_path(target_dir, "denikson-BepInExPack_Valheim");

    // The reason is returned rather than flattened into a sentence, so a caller
    // decides what to advise from the variant instead of re-statting the record.
    assert!(
      matches!(loader_refusal, NotRemovable::RecordAbsent { path, .. } if path == &loader_path),
      "expected an absent record carrying its expected path; got: {loader_refusal:?}"
    );

    let regular_refusal = blocked
      .iter()
      .find(|(name, _)| name == "Owner-Regular")
      .map(|(_, refusal)| refusal)
      .unwrap();

    assert!(
      matches!(regular_refusal, NotRemovable::RecordUnreadable { path, .. } if path == &state_path),
      "expected an unreadable record carrying the path it could not read; got: {regular_refusal:?}"
    );
  }

  #[test]
  fn a_removability_refusal_keeps_todays_wording_when_it_becomes_an_error() {
    let dir = tempdir().unwrap();
    let target_dir = dir.path();
    let eco = crate::ecosystem::Ecosystem::bundled();

    modlist::write(target_dir, &[profile_mod("denikson-BepInExPack_Valheim")]).unwrap();

    let refusal = ensure_removable(target_dir, &eco, "denikson-BepInExPack_Valheim").unwrap_err();
    let path = state_file_path(target_dir, "denikson-BepInExPack_Valheim");

    // `uninstall_mod_in` propagates this refusal with `?`, so the sentence a
    // caller without its own wording sees has to survive the conversion intact.
    assert_eq!(
      Error::from(refusal).to_string(),
      format!(
        "Profile error: denikson-BepInExPack_Valheim is a mod loader with no \
         install record at {}, so its files cannot be removed exactly. Reinstall \
         it, then uninstall it again, or remove its files by hand.",
        path.display()
      )
    );
  }

  #[test]
  fn a_reinstall_the_caller_did_not_name_keeps_its_disabled_state() {
    let eco = Ecosystem::bundled();
    let dir = tempdir().unwrap();
    let base = dir.path();

    crate::profile::layout::create(base, "valheim", "Main").unwrap();

    let profile_dir = crate::profile::layout::profile_dir(base, "valheim", "Main");
    let extracted = tempdir().unwrap();

    write_file(&extracted.path().join("plugins/Base.dll"), b"dll");

    install_extracted(
      base,
      &eco,
      "valheim",
      "Main",
      "Author-Base",
      "Author",
      &version("Author-Base"),
      extracted.path(),
      None,
      1,
    )
    .unwrap();

    disable_mod(base, &eco, "valheim", "Main", "Author-Base").unwrap();

    assert!(!modlist::read(&profile_dir).unwrap()[0].enabled);
    assert!(
      profile_dir
        .join("BepInEx/plugins/Author-Base/Base.dll.old")
        .exists(),
      "the disable must have suffixed the file, or this test proves nothing"
    );

    // Reinstalling Author-Base the way a dependency closure would: not named by
    // the user, so its disabled state must survive.
    let batch = plan_install_batch(&profile_dir, &["Author-Base".to_string()], &[]).unwrap();

    assert_eq!(batch.protect_disabled, vec!["Author-Base".to_string()]);

    install_extracted(
      base,
      &eco,
      "valheim",
      "Main",
      "Author-Base",
      "Author",
      &version("Author-Base"),
      extracted.path(),
      None,
      2,
    )
    .unwrap();

    reapply_batch_disabled(&profile_dir, &eco, "valheim", &batch).unwrap();

    let mods = modlist::read(&profile_dir).unwrap();

    assert!(
      !mods[0].enabled,
      "a disabled dependency must not be re-enabled by an incidental reinstall"
    );
    assert!(
      profile_dir
        .join("BepInEx/plugins/Author-Base/Base.dll.old")
        .exists(),
      "the file must be left suffixed, not rewritten un-suffixed and orphaned"
    );
  }

  #[test]
  fn planning_an_uninstall_batch_partitions_loaders_and_lists_untracked() {
    let eco = Ecosystem::bundled();
    let dir = tempdir().unwrap();
    let base = dir.path();

    crate::profile::layout::create(base, "valheim", "Main").unwrap();

    let profile_dir = crate::profile::layout::profile_dir(base, "valheim", "Main");
    let extracted = tempdir().unwrap();

    write_file(&extracted.path().join("plugins/Mod.dll"), b"dll");
    install_extracted(
      base,
      &eco,
      "valheim",
      "Main",
      "Author-Mod",
      "Author",
      &version("Author-Mod"),
      extracted.path(),
      None,
      1,
    )
    .unwrap();

    // A folder mods.yml never recorded, which the sweep would take along.
    write_file(
      &profile_dir.join("BepInEx/plugins/Other-ByHand/x.dll"),
      b"dll",
    );
    // Not `<Owner-Name>`-shaped, so the sweep ignores it and the plan must too.
    write_file(&profile_dir.join("BepInEx/plugins/MMHOOK/h.dll"), b"dll");

    let batch = plan_uninstall_batch(&profile_dir, &eco, "valheim", None).unwrap();

    assert_eq!(batch.mods, vec!["Author-Mod".to_string()]);
    assert!(batch.loaders.is_empty());
    assert_eq!(
      batch.untracked,
      vec![std::path::PathBuf::from("BepInEx/plugins/Other-ByHand")]
    );

    let outcome = uninstall_batch(&profile_dir, &eco, "valheim", &batch).unwrap();

    assert!(outcome.is_ok(), "failed: {:?}", outcome.failed);
    assert_eq!(outcome.succeeded, vec!["Author-Mod".to_string()]);
    assert!(modlist::read(&profile_dir).unwrap().is_empty());
    assert!(profile_dir.join("BepInEx/plugins/MMHOOK/h.dll").exists());
  }

  #[test]
  fn uninstalling_a_batch_removes_the_loader_after_ordinary_mods_and_its_own_files() {
    let eco = Ecosystem::bundled();
    let dir = tempdir().unwrap();
    let base = dir.path();
    let loader = "denikson-BepInExPack_Valheim";

    crate::profile::layout::create(base, "valheim", "Main").unwrap();

    let profile_dir = crate::profile::layout::profile_dir(base, "valheim", "Main");
    let extracted = tempdir().unwrap();

    write_file(&extracted.path().join("plugins/Mod.dll"), b"dll");
    install_extracted(
      base,
      &eco,
      "valheim",
      "Main",
      "Author-Mod",
      "Author",
      &version("Author-Mod"),
      extracted.path(),
      None,
      1,
    )
    .unwrap();

    // A recorded loader, fabricated the same way `disable_mod_is_noop_for_modloader_pack`
    // does: its files land directly under the profile root, not through
    // `install_extracted`, since a real loader package's layout (a
    // `rootFolder` wrapper stripped by the download pipeline) is not what
    // that helper exercises.
    let winhttp = profile_dir.join("winhttp.dll");
    let core_dll = profile_dir.join("BepInEx/core/BepInEx.dll");

    write_file(&winhttp, b"proxy");
    write_file(&core_dll, b"core");

    let mut mods = modlist::read(&profile_dir).unwrap();

    modlist::upsert(
      &mut mods,
      mod_entry_from_version(loader, "denikson", &version(loader), 1),
    );
    modlist::write(&profile_dir, &mods).unwrap();

    let tracker = crate::install::ModFileTracker {
      mod_name: loader.to_string(),
      files: vec![
        (
          "unused-cache-source-1".to_string(),
          "winhttp.dll".to_string(),
        ),
        (
          "unused-cache-source-2".to_string(),
          "BepInEx/core/BepInEx.dll".to_string(),
        ),
      ],
    };

    write_state_file(&profile_dir, &tracker).unwrap();

    let batch = plan_uninstall_batch(&profile_dir, &eco, "valheim", None).unwrap();

    assert_eq!(batch.mods, vec!["Author-Mod".to_string()]);
    assert_eq!(batch.loaders, vec![loader.to_string()]);

    let outcome = uninstall_batch(&profile_dir, &eco, "valheim", &batch).unwrap();

    assert!(outcome.is_ok(), "failed: {:?}", outcome.failed);
    // `uninstall_batch` chains `batch.mods` before `batch.loaders`, so the
    // order items were attempted in is what `succeeded` records them in. Two
    // items give no way to observe the timing of one removal relative to the
    // other beyond this: there is no shared file or intermediate read that
    // would tell mods-then-loader apart from an interleaved or reversed order
    // except the order they were pushed onto `succeeded` in, which is exactly
    // what this pins.
    assert_eq!(
      outcome.succeeded,
      vec!["Author-Mod".to_string(), loader.to_string()]
    );
    assert!(modlist::read(&profile_dir).unwrap().is_empty());
    // The loader's own `State`-tracked files are gone too, not just its
    // `mods.yml` entry.
    assert!(!winhttp.exists());
    assert!(!core_dll.exists());
  }

  #[test]
  fn planning_an_uninstall_batch_sorts_and_removes_two_recorded_loaders() {
    let eco = Ecosystem::bundled();
    let dir = tempdir().unwrap();
    let base = dir.path();
    let first_loader = "denikson-BepInExPack_Valheim";
    // A second, unrelated loader pack recognised by the same registry:
    // `modloader_package` is a global lookup keyed on package identity, not
    // scoped to a game, so nothing about a Valheim profile rules out a second
    // recognised loader ending up in its `mods.yml` (a real entry in the
    // bundled registry).
    let second_loader = "1F31A-BepInEx_Valheim_Full";

    crate::profile::layout::create(base, "valheim", "Main").unwrap();

    let profile_dir = crate::profile::layout::profile_dir(base, "valheim", "Main");

    let mut mods = Vec::new();

    for loader in [first_loader, second_loader] {
      modlist::upsert(
        &mut mods,
        mod_entry_from_version(loader, "owner", &version(loader), 1),
      );

      // An empty tracker satisfies the install-record precondition without
      // claiming ownership of any real file, so removing both loaders cannot
      // conflict over shared files.
      write_state_file(
        &profile_dir,
        &crate::install::ModFileTracker {
          mod_name: loader.to_string(),
          files: Vec::new(),
        },
      )
      .unwrap();
    }

    modlist::write(&profile_dir, &mods).unwrap();

    let batch = plan_uninstall_batch(&profile_dir, &eco, "valheim", None).unwrap();

    assert!(batch.mods.is_empty());
    // Sorted, so neither loader is silently dropped by the other overwriting it.
    assert_eq!(
      batch.loaders,
      vec![second_loader.to_string(), first_loader.to_string()]
    );

    let outcome = uninstall_batch(&profile_dir, &eco, "valheim", &batch).unwrap();

    assert!(outcome.is_ok(), "failed: {:?}", outcome.failed);
    assert!(modlist::read(&profile_dir).unwrap().is_empty());
  }

  #[test]
  fn planning_a_toggle_batch_splits_changing_from_already_and_excludes_loaders() {
    let eco = Ecosystem::bundled();
    let dir = tempdir().unwrap();
    let base = dir.path();
    let loader = "denikson-BepInExPack_Valheim";

    crate::profile::layout::create(base, "valheim", "Main").unwrap();

    let profile_dir = crate::profile::layout::profile_dir(base, "valheim", "Main");
    let extracted = tempdir().unwrap();

    write_file(&extracted.path().join("plugins/Mod.dll"), b"dll");

    for name in ["Author-One", "Author-Two"] {
      install_extracted(
        base,
        &eco,
        "valheim",
        "Main",
        name,
        "Author",
        &version(name),
        extracted.path(),
        None,
        1,
      )
      .unwrap();
    }

    // A recorded loader, which has no disabled state and must be excluded.
    let mut mods = modlist::read(&profile_dir).unwrap();

    mods.push(mod_entry_from_version(
      loader,
      "denikson",
      &version(loader),
      1,
    ));
    modlist::write(&profile_dir, &mods).unwrap();

    disable_mod(base, &eco, "valheim", "Main", "Author-One").unwrap();

    let batch = plan_set_enabled_batch(&profile_dir, &eco, None, false).unwrap();

    assert_eq!(batch.changing, vec!["Author-Two".to_string()]);
    assert_eq!(batch.already, vec!["Author-One".to_string()]);
    assert_eq!(batch.loaders, vec![loader.to_string()]);

    let outcome = set_enabled_batch(&profile_dir, &eco, "valheim", &batch).unwrap();

    assert!(outcome.is_ok(), "failed: {:?}", outcome.failed);
    assert_eq!(outcome.succeeded, vec!["Author-Two".to_string()]);

    let after = modlist::read(&profile_dir).unwrap();

    assert!(
      !after
        .iter()
        .find(|m| m.name == "Author-Two")
        .unwrap()
        .enabled
    );
    // The loader was never touched, so its record still reads enabled.
    assert!(after.iter().find(|m| m.name == loader).unwrap().enabled);
  }

  #[test]
  fn plan_set_enabled_batch_refuses_an_unrecorded_name() {
    let eco = Ecosystem::bundled();
    let dir = tempdir().unwrap();
    let base = dir.path();

    crate::profile::layout::create(base, "valheim", "Main").unwrap();

    let profile_dir = crate::profile::layout::profile_dir(base, "valheim", "Main");
    let extracted = tempdir().unwrap();

    write_file(&extracted.path().join("plugins/Mod.dll"), b"dll");

    install_extracted(
      base,
      &eco,
      "valheim",
      "Main",
      "Author-One",
      "Author",
      &version("Author-One"),
      extracted.path(),
      None,
      1,
    )
    .unwrap();

    let names = vec!["Author-Gone".to_string()];
    let error = plan_set_enabled_batch(&profile_dir, &eco, Some(&names), false).unwrap_err();

    assert!(error.to_string().contains("Author-Gone"));
    assert!(error.to_string().contains("is not installed"));
  }

  /// Install batches run end to end against a mock Thunderstore: real downloads,
  /// extraction, routing and `mods.yml`, so skip, clean and force are checked
  /// on disk rather than against a stub.
  mod reinstall {
    use super::*;
    use crate::models::Package;

    const MOD: &str = "Author-Mod";
    const DEP: &str = "Author-Dep";
    const LOADER: &str = "denikson-BepInExPack_Valheim";

    /// One published version of a package.
    struct Pkg<'a> {
      full_name: &'a str,
      version: &'a str,
      deps: &'a [&'a str],
      files: &'a [(&'a str, &'a [u8])],
    }

    struct Repo {
      server: mockito::ServerGuard,
      mocks: Vec<mockito::Mock>,
      client: ThunderstoreClient,
      _http_cache: tempfile::TempDir,
      cache_base: tempfile::TempDir,
      target: tempfile::TempDir,
      eco: Ecosystem,
      rt: tokio::runtime::Runtime,
      clock: u64,
    }

    impl Repo {
      fn new() -> Self {
        let server = mockito::Server::new();
        let http_cache = tempdir().unwrap();
        let client = ThunderstoreClient::builder()
          .package_index_url(format!("{}/pkg/", server.url()))
          .cache_dir(http_cache.path())
          .build()
          .unwrap();

        Self {
          server,
          mocks: Vec::new(),
          client,
          _http_cache: http_cache,
          cache_base: tempdir().unwrap(),
          target: tempdir().unwrap(),
          eco: Ecosystem::bundled(),
          rt: tokio::runtime::Runtime::new().unwrap(),
          clock: 0,
        }
      }

      /// Serves each package's archive and returns an index in which that
      /// version is the only, and so the latest, one.
      fn publish(&mut self, pkgs: &[Pkg<'_>]) -> PackageIndex {
        let mut packages = Vec::new();

        for pkg in pkgs {
          let path = format!("/dl/{}-{}.zip", pkg.full_name, pkg.version);
          let mock = self
            .server
            .mock("GET", path.as_str())
            .with_status(200)
            .with_header("Content-Type", "application/zip")
            .with_body(zip_of(pkg.files))
            .create();

          self.mocks.push(mock);

          let (owner, name) = pkg.full_name.split_once('-').unwrap();
          let json = serde_json::json!({
            "name": name, "full_name": pkg.full_name, "owner": owner,
            "package_url": "https://example.com", "date_created": "2024-01-01T12:00:00Z",
            "date_updated": "2024-01-02T12:00:00Z", "uuid4": "pkg", "rating_score": 1,
            "is_pinned": false, "is_deprecated": false, "has_nsfw_content": false,
            "categories": [],
            "versions": [{
              "name": name, "full_name": pkg.full_name, "description": "d", "icon": "i",
              "version_number": pkg.version, "dependencies": pkg.deps,
              "download_url": format!("{}{path}", self.server.url()), "downloads": 1,
              "date_created": "2024-01-01T12:00:00Z", "website_url": "", "is_active": true,
              "uuid4": "ver", "file_size": 1
            }]
          });

          packages.push(serde_json::from_value::<Package>(json).unwrap());
        }

        PackageIndex::from(packages)
      }

      /// Plans a batch for `desired` (naming `explicit`) and runs it.
      fn install(
        &mut self,
        index: &PackageIndex,
        desired: &[&str],
        explicit: &[&str],
        force: bool,
      ) -> BatchOutcome {
        let desired: Vec<String> = desired.iter().map(|s| s.to_string()).collect();
        let explicit: Vec<String> = explicit.iter().map(|s| s.to_string()).collect();
        let mut batch = plan_install_batch(self.target.path(), &desired, &explicit).unwrap();

        batch.force = force;
        self.clock += 1;

        let outcome = self
          .rt
          .block_on(install_batch(
            self.target.path(),
            self.cache_base.path(),
            &self.eco,
            index,
            &self.client,
            "valheim",
            &batch,
            self.clock,
          ))
          .unwrap();

        assert!(outcome.is_ok(), "batch failed: {:?}", outcome.failed);

        outcome
      }

      fn path(&self, rel: &str) -> PathBuf {
        self.target.path().join(rel)
      }

      fn entry(&self, name: &str) -> ProfileMod {
        modlist::read(self.target.path())
          .unwrap()
          .into_iter()
          .find(|m| m.name == name)
          .unwrap()
      }
    }

    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
      let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
      let opts: FileOptions<'_, ()> =
        FileOptions::default().compression_method(zip::CompressionMethod::Stored);

      for (name, body) in files {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(body).unwrap();
      }

      zip.finish().unwrap().into_inner()
    }

    fn names(list: &[&str]) -> Vec<String> {
      list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_install_at_the_recorded_version_is_skipped() {
      let mut repo = Repo::new();
      let index = repo.publish(&[Pkg {
        full_name: MOD,
        version: "1.0.0",
        deps: &[],
        files: &[("plugins/Mod.dll", b"v1")],
      }]);

      repo.install(&index, &[MOD], &[MOD], false);

      let user_file = repo.path("BepInEx/plugins/Author-Mod/user.txt");

      write_file(&user_file, b"mine");

      let installed_at = repo.entry(MOD).installed_at_time;
      let outcome = repo.install(&index, &[MOD], &[MOD], false);

      assert!(outcome.succeeded.is_empty());
      assert_eq!(outcome.unchanged, names(&[MOD]));
      assert!(user_file.exists(), "a skipped install must not touch files");
      assert_eq!(repo.entry(MOD).installed_at_time, installed_at);
    }

    #[test]
    fn a_version_change_removes_files_only_the_old_version_had() {
      let mut repo = Repo::new();
      let v1 = repo.publish(&[Pkg {
        full_name: MOD,
        version: "1.0.0",
        deps: &[],
        files: &[
          ("plugins/Old.dll", b"old"),
          ("plugins/german.json", b"flat"),
        ],
      }]);

      repo.install(&v1, &[MOD], &[MOD], false);

      assert!(repo.path("BepInEx/plugins/Author-Mod/german.json").exists());

      let v2 = repo.publish(&[Pkg {
        full_name: MOD,
        version: "2.0.0",
        deps: &[],
        files: &[
          ("plugins/Mod.dll", b"new"),
          ("plugins/Translations/German/german.json", b"nested"),
        ],
      }]);
      let outcome = repo.install(&v2, &[MOD], &[MOD], false);

      assert_eq!(outcome.succeeded, names(&[MOD]));
      assert!(!repo.path("BepInEx/plugins/Author-Mod/Old.dll").exists());
      assert!(!repo.path("BepInEx/plugins/Author-Mod/german.json").exists());
      assert!(
        repo
          .path("BepInEx/plugins/Author-Mod/Translations/German/german.json")
          .exists()
      );
      assert_eq!(repo.entry(MOD).version_number.to_string(), "2.0.0");
    }

    #[test]
    fn force_reinstalls_the_recorded_version_from_a_clean_folder() {
      let mut repo = Repo::new();
      let index = repo.publish(&[Pkg {
        full_name: MOD,
        version: "1.0.0",
        deps: &[],
        files: &[("plugins/Translations/German/german.json", b"nested")],
      }]);

      repo.install(&index, &[MOD], &[MOD], false);

      // What an install made before nested folders were kept left behind.
      let stale = repo.path("BepInEx/plugins/Author-Mod/german.json");

      write_file(&stale, b"flat");

      let outcome = repo.install(&index, &[MOD], &[MOD], true);

      assert_eq!(outcome.succeeded, names(&[MOD]));
      assert!(outcome.unchanged.is_empty());
      assert!(!stale.exists());
      assert!(
        repo
          .path("BepInEx/plugins/Author-Mod/Translations/German/german.json")
          .exists()
      );
    }

    #[test]
    fn force_reinstalls_only_the_named_mod_not_its_dependencies() {
      let mut repo = Repo::new();
      let index = repo.publish(&[
        Pkg {
          full_name: MOD,
          version: "1.0.0",
          deps: &["Author-Dep-1.0.0"],
          files: &[("plugins/Mod.dll", b"mod")],
        },
        Pkg {
          full_name: DEP,
          version: "1.0.0",
          deps: &[],
          files: &[("plugins/Dep.dll", b"dep")],
        },
      ]);

      repo.install(&index, &[MOD], &[MOD], false);

      let dep_marker = repo.path("BepInEx/plugins/Author-Dep/user.txt");

      write_file(&dep_marker, b"mine");

      let outcome = repo.install(&index, &[MOD], &[MOD], true);

      assert_eq!(outcome.succeeded, names(&[MOD]));
      assert_eq!(outcome.unchanged, names(&[DEP]));
      assert!(
        dep_marker.exists(),
        "the dependency must not be reinstalled"
      );
    }

    #[test]
    fn a_disabled_mod_stays_disabled_through_force_and_an_update() {
      let mut repo = Repo::new();
      let v1 = repo.publish(&[Pkg {
        full_name: MOD,
        version: "1.0.0",
        deps: &[],
        files: &[("plugins/Mod.dll", b"v1"), ("plugins/Old.dll", b"v1")],
      }]);

      repo.install(&v1, &[MOD], &[MOD], false);

      set_enabled_in(repo.target.path(), &repo.eco, "valheim", MOD, false).unwrap();

      // Not named, as an update on the user's behalf runs it.
      repo.install(&v1, &[MOD], &[], true);

      assert!(!repo.entry(MOD).enabled);
      assert!(repo.path("BepInEx/plugins/Author-Mod/Mod.dll.old").exists());
      assert!(!repo.path("BepInEx/plugins/Author-Mod/Mod.dll").exists());

      let v2 = repo.publish(&[Pkg {
        full_name: MOD,
        version: "2.0.0",
        deps: &[],
        files: &[("plugins/Mod.dll", b"v2")],
      }]);

      repo.install(&v2, &[MOD], &[], false);

      assert!(!repo.entry(MOD).enabled);
      assert!(repo.path("BepInEx/plugins/Author-Mod/Mod.dll.old").exists());
      assert!(!repo.path("BepInEx/plugins/Author-Mod/Mod.dll").exists());
      assert!(!repo.path("BepInEx/plugins/Author-Mod/Old.dll.old").exists());
    }

    #[test]
    fn naming_a_disabled_mod_at_its_recorded_version_enables_it() {
      let mut repo = Repo::new();
      let index = repo.publish(&[Pkg {
        full_name: MOD,
        version: "1.0.0",
        deps: &[],
        files: &[("plugins/Mod.dll", b"v1")],
      }]);

      repo.install(&index, &[MOD], &[MOD], false);

      set_enabled_in(repo.target.path(), &repo.eco, "valheim", MOD, false).unwrap();

      let outcome = repo.install(&index, &[MOD], &[MOD], false);

      assert_eq!(outcome.unchanged, names(&[MOD]));
      assert!(repo.entry(MOD).enabled);
      assert!(repo.path("BepInEx/plugins/Author-Mod/Mod.dll").exists());
      assert!(!repo.path("BepInEx/plugins/Author-Mod/Mod.dll.old").exists());
    }

    #[test]
    fn shared_config_survives_force_and_an_update() {
      let mut repo = Repo::new();
      let v1 = repo.publish(&[Pkg {
        full_name: MOD,
        version: "1.0.0",
        deps: &[],
        files: &[("plugins/Mod.dll", b"v1"), ("config/Mod.cfg", b"default")],
      }]);

      repo.install(&v1, &[MOD], &[MOD], false);

      let cfg = repo.path("BepInEx/config/Mod.cfg");

      write_file(&cfg, b"edited");
      repo.install(&v1, &[MOD], &[MOD], true);

      assert_eq!(std::fs::read(&cfg).unwrap(), b"edited");

      let v2 = repo.publish(&[Pkg {
        full_name: MOD,
        version: "2.0.0",
        deps: &[],
        files: &[("plugins/Mod.dll", b"v2"), ("config/Mod.cfg", b"default")],
      }]);

      repo.install(&v2, &[MOD], &[MOD], false);

      assert_eq!(std::fs::read(&cfg).unwrap(), b"edited");
    }

    #[test]
    fn force_reinstalls_a_loader_pack_and_keeps_its_config() {
      let mut repo = Repo::new();
      let index = repo.publish(&[Pkg {
        full_name: LOADER,
        version: "5.4.2333",
        deps: &[],
        files: &[
          ("BepInExPack_Valheim/winhttp.dll", b"proxy"),
          ("BepInExPack_Valheim/BepInEx/core/BepInEx.dll", b"core"),
          ("BepInExPack_Valheim/BepInEx/config/BepInEx.cfg", b"default"),
        ],
      }]);

      repo.install(&index, &[LOADER], &[LOADER], false);

      let cfg = repo.path("BepInEx/config/BepInEx.cfg");
      let core = repo.path("BepInEx/core/BepInEx.dll");

      write_file(&cfg, b"edited");
      write_file(&core, b"tampered");

      let outcome = repo.install(&index, &[LOADER], &[LOADER], true);

      assert_eq!(outcome.succeeded, names(&[LOADER]));
      assert_eq!(std::fs::read(&core).unwrap(), b"core");
      assert!(repo.path("winhttp.dll").exists());
      assert_eq!(std::fs::read(&cfg).unwrap(), b"edited");
      assert!(state_file_path(repo.target.path(), LOADER).exists());
    }

    #[test]
    fn a_recorded_mod_whose_folder_is_gone_is_reinstalled() {
      let mut repo = Repo::new();
      let index = repo.publish(&[Pkg {
        full_name: MOD,
        version: "1.0.0",
        deps: &[],
        files: &[("plugins/Mod.dll", b"v1")],
      }]);

      repo.install(&index, &[MOD], &[MOD], false);

      std::fs::remove_dir_all(repo.path("BepInEx/plugins/Author-Mod")).unwrap();

      let outcome = repo.install(&index, &[MOD], &[MOD], false);

      assert_eq!(outcome.succeeded, names(&[MOD]));
      assert!(outcome.unchanged.is_empty());
      assert!(repo.path("BepInEx/plugins/Author-Mod/Mod.dll").exists());
    }

    #[test]
    fn a_loader_pack_missing_a_tracked_file_is_reinstalled() {
      let mut repo = Repo::new();
      let index = repo.publish(&[Pkg {
        full_name: LOADER,
        version: "5.4.2333",
        deps: &[],
        files: &[
          ("BepInExPack_Valheim/winhttp.dll", b"proxy"),
          ("BepInExPack_Valheim/BepInEx/core/BepInEx.dll", b"core"),
        ],
      }]);

      repo.install(&index, &[LOADER], &[LOADER], false);

      std::fs::remove_file(repo.path("winhttp.dll")).unwrap();

      let outcome = repo.install(&index, &[LOADER], &[LOADER], false);

      assert_eq!(outcome.succeeded, names(&[LOADER]));
      assert!(repo.path("winhttp.dll").exists());
    }

    #[test]
    fn a_recorded_mod_with_no_files_to_place_is_skipped() {
      let mut repo = Repo::new();
      let index = repo.publish(&[Pkg {
        full_name: MOD,
        version: "1.0.0",
        deps: &[],
        files: &[("manifest.json", b"{}"), ("README.md", b"modpack")],
      }]);

      repo.install(&index, &[MOD], &[MOD], false);

      let outcome = repo.install(&index, &[MOD], &[MOD], false);

      assert!(outcome.succeeded.is_empty());
      assert_eq!(outcome.unchanged, names(&[MOD]));
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_enable_is_reported_per_item_without_losing_the_batch() {
      use std::os::unix::fs::PermissionsExt;

      let mut repo = Repo::new();
      let index = repo.publish(&[
        Pkg {
          full_name: MOD,
          version: "1.0.0",
          deps: &[],
          files: &[("plugins/Mod.dll", b"v1")],
        },
        Pkg {
          full_name: DEP,
          version: "1.0.0",
          deps: &[],
          files: &[("plugins/Dep.dll", b"dep")],
        },
      ]);

      repo.install(&index, &[MOD], &[MOD], false);

      set_enabled_in(repo.target.path(), &repo.eco, "valheim", MOD, false).unwrap();

      // A read-only folder makes the `.old` rename fail, as a locked file would.
      let folder = repo.path("BepInEx/plugins/Author-Mod");

      std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();

      if std::fs::write(folder.join("probe"), b"").is_ok() {
        // Running as root: permissions cannot force the failure.
        return;
      }

      let desired = names(&[MOD, DEP]);
      let batch = plan_install_batch(repo.target.path(), &desired, &desired).unwrap();
      let result = repo.rt.block_on(install_batch(
        repo.target.path(),
        repo.cache_base.path(),
        &repo.eco,
        &index,
        &repo.client,
        "valheim",
        &batch,
        1,
      ));

      std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();

      let outcome = result.expect("a failed enable must not discard the batch");
      let failed: Vec<&str> = outcome.failed.iter().map(|(n, _)| n.as_str()).collect();

      assert_eq!(failed, vec![MOD]);
      assert_eq!(outcome.succeeded, names(&[DEP]));
      assert!(outcome.unchanged.is_empty());
    }

    #[test]
    fn a_disabled_mod_not_named_is_still_skipped() {
      let mut repo = Repo::new();
      let index = repo.publish(&[Pkg {
        full_name: MOD,
        version: "1.0.0",
        deps: &[],
        files: &[("plugins/Mod.dll", b"v1")],
      }]);

      repo.install(&index, &[MOD], &[MOD], false);

      set_enabled_in(repo.target.path(), &repo.eco, "valheim", MOD, false).unwrap();

      let outcome = repo.install(&index, &[MOD], &[], false);

      assert_eq!(outcome.unchanged, names(&[MOD]));
      assert!(!repo.entry(MOD).enabled);
      assert!(repo.path("BepInEx/plugins/Author-Mod/Mod.dll.old").exists());
    }

    #[test]
    fn a_mod_left_half_disabled_is_reinstalled() {
      let mut repo = Repo::new();
      let index = repo.publish(&[Pkg {
        full_name: MOD,
        version: "1.0.0",
        deps: &[],
        files: &[("plugins/Mod.dll", b"v1"), ("plugins/Extra.dll", b"v1")],
      }]);

      repo.install(&index, &[MOD], &[MOD], false);

      // A disable that failed after its first rename: one file toggled, the
      // other not, and `mods.yml` still recording the mod as enabled.
      let extra = repo.path("BepInEx/plugins/Author-Mod/Extra.dll");

      std::fs::rename(
        &extra,
        repo.path("BepInEx/plugins/Author-Mod/Extra.dll.old"),
      )
      .unwrap();

      let outcome = repo.install(&index, &[MOD], &[MOD], false);

      assert_eq!(outcome.succeeded, names(&[MOD]));
      assert!(outcome.unchanged.is_empty());
      assert!(extra.exists());
      assert!(
        !repo
          .path("BepInEx/plugins/Author-Mod/Extra.dll.old")
          .exists()
      );
      assert!(repo.path("BepInEx/plugins/Author-Mod/Mod.dll").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_loader_reinstall_keeps_it_uninstallable() {
      let mut repo = Repo::new();
      let v1 = repo.publish(&[Pkg {
        full_name: LOADER,
        version: "5.4.2333",
        deps: &[],
        files: &[
          ("BepInExPack_Valheim/winhttp.dll", b"proxy"),
          ("BepInExPack_Valheim/BepInEx/core/BepInEx.dll", b"core"),
        ],
      }]);

      repo.install(&v1, &[LOADER], &[LOADER], false);

      let v2 = repo.publish(&[Pkg {
        full_name: LOADER,
        version: "5.4.2334",
        deps: &[],
        files: &[
          ("BepInExPack_Valheim/winhttp.dll", b"proxy"),
          ("BepInExPack_Valheim/BepInEx/core/BepInEx.dll", b"core"),
          ("BepInExPack_Valheim/BepInEx/core/Extra.dll", b"extra"),
        ],
      }]);

      // A symlink at a destination only the new version writes makes the apply
      // refuse after the old payload is already gone.
      std::os::unix::fs::symlink(
        repo.path("elsewhere.dll"),
        repo.path("BepInEx/core/Extra.dll"),
      )
      .unwrap();

      let desired = names(&[LOADER]);
      let batch = plan_install_batch(repo.target.path(), &desired, &desired).unwrap();
      let outcome = repo
        .rt
        .block_on(install_batch(
          repo.target.path(),
          repo.cache_base.path(),
          &repo.eco,
          &v2,
          &repo.client,
          "valheim",
          &batch,
          2,
        ))
        .unwrap();
      let failed: Vec<&str> = outcome.failed.iter().map(|(n, _)| n.as_str()).collect();

      assert_eq!(failed, vec![LOADER]);
      assert!(state_file_path(repo.target.path(), LOADER).exists());

      uninstall_mod_in(repo.target.path(), &repo.eco, "valheim", LOADER).unwrap();

      assert!(modlist::read(repo.target.path()).unwrap().is_empty());
      assert!(!state_file_path(repo.target.path(), LOADER).exists());
    }
  }
}
