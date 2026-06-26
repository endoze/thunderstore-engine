//! Compute how to launch a game with a target directory's mods applied.
//!
//! The engine produces a [`LaunchPlan`](crate::profile::launch::LaunchPlan) — the program to start, its argv, its
//! environment, and a working directory — and the **caller** spawns it. The engine
//! never discovers a game directory, never reads the host platform, and never
//! starts a process; platform facts arrive as explicit [`LaunchContext`](crate::profile::launch::LaunchContext) fields so
//! every combination is testable.
//!
//! Injection is command-line arguments, not config files: a loader package ships
//! its own `doorstop_config.ini` / proxy DLL / `run_bepinex.sh` into the target at
//! install time, and the arguments computed here override that ini at runtime.
//!
//! Argv is returned as a real argv vector and is **never shell-quoted** — pass it
//! to `Command::args`, not to a shell.

#![deny(missing_docs)]

use crate::ecosystem::{Ecosystem, GameProfile, InstanceType, LoaderKind};
use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// The operating system the game will run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostOs {
  /// Windows. Loader paths handed to the game are Windows-shaped natively.
  Windows,
  /// Linux. The only host where [`LaunchPlan::steam_wrapper`] can apply, and the
  /// only one where [`Runtime::Proton`] is meaningful.
  Linux,
  /// macOS. Rejected by [`launch_plan_in`], which cannot compute a correct plan
  /// because r2modman injects through a compiled proxy binary with no available
  /// source.
  MacOs,
}

impl HostOs {
  /// The operating system this build is running on, or `None` where launching is
  /// not supported.
  ///
  /// Reads the compile-time target rather than a runtime probe, because a build
  /// cannot run on a platform it was not compiled for.
  pub fn detect() -> Option<Self> {
    if cfg!(target_os = "windows") {
      Some(Self::Windows)
    } else if cfg!(target_os = "linux") {
      Some(Self::Linux)
    } else if cfg!(target_os = "macos") {
      Some(Self::MacOs)
    } else {
      None
    }
  }
}

/// How the game is distributed, which decides what process is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StorePlatform {
  /// Launch through Steam (`-applaunch <id>`).
  Steam,
  /// A Steam copy launched by executing the game binary directly.
  SteamDirect,
  /// Any other store: execute the game binary directly.
  Other,
}

impl std::str::FromStr for StorePlatform {
  type Err = Error;

  /// Parses the spelling a client's configuration uses.
  fn from_str(value: &str) -> Result<Self> {
    match value {
      "steam" => Ok(Self::Steam),
      "steam-direct" => Ok(Self::SteamDirect),
      "other" => Ok(Self::Other),
      other => Err(Error::Profile(format!(
        "{other:?} is not a known store platform; expected steam, steam-direct, \
         or other"
      ))),
    }
  }
}

/// Whether the game runs natively or under Proton/Wine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Runtime {
  /// The game runs as a native binary for [`HostOs`].
  Native,
  /// The game is a Windows build running under Proton or Wine, so paths handed
  /// to it are Windows-shaped.
  Proton,
}

impl std::str::FromStr for Runtime {
  type Err = Error;

  /// Parses the spelling a client's configuration uses.
  fn from_str(value: &str) -> Result<Self> {
    match value {
      "native" => Ok(Self::Native),
      "proton" => Ok(Self::Proton),
      other => Err(Error::Profile(format!(
        "{other:?} is not a known runtime; expected native or proton"
      ))),
    }
  }
}

/// Launch with mods applied, or without.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LaunchMode {
  /// Inject the loader so the target directory's mods are applied.
  Modded,
  /// Start the game with the loader explicitly disabled.
  ///
  /// Installed files stay on disk untouched, so this is a per-launch toggle
  /// rather than an uninstall. Most loaders take an explicit off switch here
  /// (`--vanilla`, `-vanilla`, `--gdweave-disable`) rather than simply omitting
  /// the on switch, because a loader already staged into the game directory
  /// would otherwise still initialise.
  Vanilla,
}

/// The process the caller should start.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LaunchProgram {
  /// Execute the game binary. The caller resolves which of `exe_names` exists in
  /// its game directory.
  GameExe {
    /// Candidate binary names, in preference order.
    exe_names: Vec<String>,
  },
  /// Execute Steam with `-applaunch <steam_app_id>`.
  Steam {
    /// The Steam application id to hand to `-applaunch`.
    steam_app_id: u32,
  },
  /// Execute a script the loader package shipped into the target directory (one
  /// of [`LOADER_SCRIPTS`]), passing the game binary and then [`LaunchPlan::args`]
  /// after it. The caller resolves which of `exe_names` exists in its game
  /// directory, exactly as for [`LaunchProgram::GameExe`].
  ProfileScript {
    /// The loader script inside the target directory to execute.
    path: PathBuf,
    /// Candidate game binary names, in preference order, passed to the script
    /// ahead of [`LaunchPlan::args`].
    exe_names: Vec<String>,
  },
}

/// What a caller needs to install the wrapper script for a Linux native Steam
/// launch, carried by [`LaunchPlan::steam_wrapper`] when that combination applies.
///
/// Steam's launch options are a single fixed `%command%` string, so a file must sit
/// between Steam and the game; [`LaunchPlan::program`] alone cannot express that.
/// Neither [`write_wrapper_script`] nor [`steam_launch_options`] needs anything
/// from here — both take only a path, because the script names no target and the
/// options string names no loader. This type is what tells a caller the wrapper is
/// required at all.
///
/// The wrapper is written **once** and never rewritten: it resolves its target at
/// run time from [`WRAPPER_TARGET_FLAG`], which [`launch_plan_in`] has already
/// appended to [`LaunchPlan::args`] whenever it populates this field. A caller
/// spawns the plan as computed and the correct target follows automatically, so
/// the launch-options string a user pasted stays valid across every target.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SteamWrapper {
  /// The target directory the wrapper chainloads from, which reaches it as a
  /// run-time argument already present in [`LaunchPlan::args`]. Reported here so
  /// a caller can display or log it, not because the wrapper needs writing for it.
  pub target_dir: PathBuf,
  /// The loader this target injects through, reported for display and logging.
  ///
  /// No longer feeds [`steam_launch_options`], which dropped its `WINEDLLOVERRIDES`
  /// and with it the need for a loader. [`proxy_dll`] still maps this to a DLL name
  /// for a caller setting up a Wine prefix by hand.
  pub loader: LoaderKind,
}

/// A fully computed launch invocation.
///
/// `#[non_exhaustive]`: fields may be added as more platforms are supported.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LaunchPlan {
  /// What to execute.
  pub program: LaunchProgram,
  /// Arguments, in order, already resolved. Never shell-quoted.
  pub args: Vec<String>,
  /// Environment variables to set for the child process.
  ///
  /// **Only honoured for a program the caller spawns directly.** When
  /// [`Self::program`] is [`LaunchProgram::Steam`], these are set on the
  /// short-lived `steam` process, and `steam -applaunch <id>` merely messages an
  /// already-running Steam client, which starts the game with *its own*
  /// environment. Nothing here reaches the game in that case. The visible
  /// consequence is a Proton + Steam launch silently starting unmodded: without
  /// `WINEDLLOVERRIDES`, Wine loads its builtin proxy instead of the loader's and
  /// BepInEx never initialises, leaving no log to explain why. A caller needing the
  /// override under Steam must put it in the Wine prefix ([`proxy_dll`] gives the
  /// name), not here.
  pub env: Vec<(String, String)>,
  /// Working directory, when it matters.
  pub working_dir: Option<PathBuf>,
  /// `Some` when starting [`Self::program`] is not sufficient on its own: a Linux
  /// native Steam launch injects through a loader script that Steam will not run
  /// unless a wrapper script is installed in its launch options. See
  /// [`SteamWrapper`], [`write_wrapper_script`], and [`steam_launch_options`].
  pub steam_wrapper: Option<SteamWrapper>,
}

/// The platform facts a launch computation needs, supplied by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LaunchContext {
  /// The operating system the game will run on. Supplied rather than detected so
  /// every combination stays testable; [`HostOs::detect`] is the caller's opt-in.
  pub os: HostOs,
  /// How the game is distributed, which decides what process is started and
  /// whether the environment in [`LaunchPlan::env`] can reach the game at all.
  pub store: StorePlatform,
  /// Whether the game runs natively or under Proton/Wine, which decides whether
  /// paths in the computed argv are Unix- or Windows-shaped.
  pub runtime: Runtime,
  /// Whether to inject the loader or start the game vanilla.
  pub mode: LaunchMode,
  /// Caller/user-supplied arguments, appended after the loader's own.
  pub extra_args: Vec<String>,
}

impl LaunchContext {
  /// Builds a [`LaunchContext`] with an empty `extra_args`.
  ///
  /// `LaunchContext` is `#[non_exhaustive]`, so outside this crate it cannot be
  /// built with a struct literal, nor updated with functional-update (`..`)
  /// syntax — both are struct-literal forms and `#[non_exhaustive]` forbids both
  /// from other crates. This constructor is the supported way for a downstream
  /// crate (including this crate's own `tests/*.rs`, which compile as separate
  /// crates) to construct one; use [`Self::with_extra_args`] to add extras.
  pub fn new(os: HostOs, store: StorePlatform, runtime: Runtime, mode: LaunchMode) -> Self {
    Self {
      os,
      store,
      runtime,
      mode,
      extra_args: Vec::new(),
    }
  }

  /// Returns `self` with `extra_args` replaced, for chaining after [`Self::new`].
  pub fn with_extra_args(self, extra_args: Vec<String>) -> Self {
    Self { extra_args, ..self }
  }
}

/// Preloader assembly names a BepInEx package may ship, in the order r2modman
/// lists them. The first one present in `<target>/BepInEx/core` wins.
pub const PRELOADER_CANDIDATES: [&str; 5] = [
  "BepInEx.Unity.Mono.Preloader.dll",
  "BepInEx.Unity.IL2CPP.dll",
  "BepInEx.Preloader.dll",
  "BepInEx.IL2CPP.dll",
  "BepInEx.NET.CoreCLR.dll",
];

/// The loader scripts a BepInEx/BepisLoader package may ship into the target, in
/// precedence order. Both the resolver behind [`launch_plan_in`] and the wrapper
/// script from [`wrapper_script_contents`] chainload the first one present, so the
/// two agree by construction.
///
/// `start_server_bepinex.sh` is only ever selected for an
/// [`InstanceType::Server`] profile — running it for a game instance would start a
/// dedicated server. The wrapper, which has no ecosystem profile to consult at run
/// time, gates it on the `TS_START_SERVER` environment variable instead.
pub const LOADER_SCRIPTS: [&str; 3] = [
  "start_server_bepinex.sh",
  "run_bepinex.sh",
  "start_game_bepinex.sh",
];

/// The dedicated-server entry of [`LOADER_SCRIPTS`].
const SERVER_SCRIPT: &str = LOADER_SCRIPTS[0];

/// The refusal both [`launch_plan_in`] and [`stage_for_launch_in`] return for macOS:
/// no plan can be computed, so nothing may be written into a game directory either.
const MACOS_UNSUPPORTED: &str =
  "macOS launch is not supported: the injection chain requires r2modman's closed-source proxy";

/// Reads the Doorstop major version from `<target_dir>/.doorstop_version`,
/// defaulting to `3` when the file is missing or unparseable.
///
/// A major of `4` or higher selects the v4 argument spellings. r2modman instead
/// falls back to v3 for anything above 4, which mis-launches a future Doorstop;
/// we deliberately do not reproduce that.
pub fn doorstop_version(target_dir: &Path) -> u32 {
  let path = target_dir.join(".doorstop_version");

  let text = match std::fs::read_to_string(&path) {
    Ok(text) => text,
    Err(_) => return 3,
  };

  let major = text
    .trim()
    .split('.')
    .next()
    .and_then(|part| part.parse::<u32>().ok());

  match major {
    Some(version) if version >= 3 => version,
    _ => 3,
  }
}

/// Renders `path` for a Wine/Proton prefix: forward slashes, `Z:` root mapping.
fn wine_path(path: &Path) -> String {
  format!("Z:{}", path.to_string_lossy().replace('\\', "/"))
}

/// Locates the BepInEx preloader assembly inside `<target_dir>/BepInEx/core`.
///
/// Under [`Runtime::Proton`] the path is rendered for the Wine prefix (`Z:` +
/// forward slashes), which is how the Windows-side Doorstop resolves a Linux path.
pub fn bepinex_preloader(target_dir: &Path, runtime: Runtime) -> Result<String> {
  let core = target_dir.join("BepInEx").join("core");

  let found = PRELOADER_CANDIDATES
    .iter()
    .map(|name| core.join(name))
    .find(|candidate| candidate.is_file())
    .ok_or_else(|| Error::Profile(format!("no BepInEx preloader found in {}", core.display())))?;

  let rendered = match runtime {
    Runtime::Proton => wine_path(&found),
    Runtime::Native => found.to_string_lossy().replace('\\', "/"),
  };

  Ok(rendered)
}

/// Whether the target ships unstripped corlibs, which need a Doorstop search-path
/// override.
pub fn has_corlibs(target_dir: &Path) -> bool {
  target_dir.join("unstripped_corlib").is_dir()
}

/// Renders a target-relative path as an argument value, using forward slashes so
/// a Windows-side loader reads it consistently.
fn arg_path(target_dir: &Path, relative: &str) -> String {
  target_dir
    .join(relative)
    .to_string_lossy()
    .replace('\\', "/")
}

/// The loader's own arguments for the requested mode, before any caller-supplied
/// extras. An unrecognized loader contributes nothing rather than erroring, so a
/// newer ecosystem schema degrades to a vanilla launch.
///
/// For [`LoaderKind::BepInEx`] and [`LoaderKind::Umm`], the Doorstop flag
/// spellings chosen depend on more than `.doorstop_version`: this function also
/// stats `target_dir` (via `is_script_injected`) to check whether a loader
/// script is present, because a script-injected launch forces the v4 spellings
/// regardless of the detected version (see `doorstop_args`'s doc comment).
/// `launch_plan_in` separately computes the same `ScriptInjection` fact for
/// `resolve_program`/[`LaunchPlan::steam_wrapper`], so a single call to
/// [`launch_plan_in`] reads that presence off disk twice — same rule, two
/// moments, harmless for a synchronous, non-hot-path computation.
pub fn loader_args(
  target_dir: &Path,
  profile: &GameProfile,
  ctx: &LaunchContext,
) -> Result<Vec<String>> {
  let target = target_dir.to_string_lossy().replace('\\', "/");

  let args = match profile.package_loader {
    LoaderKind::BepInEx => {
      let script_injected = is_script_injected(target_dir, profile, ctx);

      doorstop_args(
        target_dir,
        profile,
        ctx,
        DoorstopTarget::BepInEx,
        script_injected,
      )?
    }
    LoaderKind::BepisLoader => bepis_args(target_dir, profile, ctx)?,
    LoaderKind::Umm => {
      let script_injected = is_script_injected(target_dir, profile, ctx);

      doorstop_args(
        target_dir,
        profile,
        ctx,
        DoorstopTarget::Umm,
        script_injected,
      )?
    }
    LoaderKind::MelonLoader | LoaderKind::RecursiveMelonLoader => {
      melonloader_args(target_dir, ctx, &target)
    }
    LoaderKind::GdWeave => match ctx.mode {
      LaunchMode::Modded => vec![format!(
        "--gdweave-folder-override={}",
        arg_path(target_dir, "GDWeave")
      )],
      LaunchMode::Vanilla => vec!["--gdweave-disable".to_string()],
    },
    LoaderKind::Lovely => match ctx.mode {
      LaunchMode::Modded => vec!["--mod-dir".to_string(), arg_path(target_dir, "mods")],
      LaunchMode::Vanilla => vec!["--vanilla".to_string()],
    },
    LoaderKind::Northstar => match ctx.mode {
      LaunchMode::Modded => vec![
        "-northstar".to_string(),
        format!("-profile={}", arg_path(target_dir, "R2Northstar")),
      ],
      LaunchMode::Vanilla => vec!["-vanilla".to_string()],
    },
    LoaderKind::GodotMl => match ctx.mode {
      LaunchMode::Modded => vec![
        "--script".to_string(),
        "res://addons/mod_loader/mod_loader_setup.gd".to_string(),
        "--enable-mods".to_string(),
        "--mods-path".to_string(),
        arg_path(target_dir, "mods"),
      ],
      LaunchMode::Vanilla => Vec::new(),
    },
    LoaderKind::Shimloader => match ctx.mode {
      LaunchMode::Modded => vec![
        "--mod-dir".to_string(),
        arg_path(target_dir, "shimloader/mod"),
        "--pak-dir".to_string(),
        arg_path(target_dir, "shimloader/pak"),
        "--cfg-dir".to_string(),
        arg_path(target_dir, "shimloader/cfg"),
        "--overlay-dir".to_string(),
        arg_path(target_dir, "shimloader/overlay"),
      ],
      LaunchMode::Vanilla => Vec::new(),
    },
    LoaderKind::ReturnOfModding => match ctx.mode {
      LaunchMode::Modded => vec!["--rom_modding_root_folder".to_string(), target.clone()],
      LaunchMode::Vanilla => vec!["--rom_enabled".to_string(), "false".to_string()],
    },
    LoaderKind::Rivet => match ctx.mode {
      LaunchMode::Modded => vec![
        "-rivetEnable".to_string(),
        "true".to_string(),
        "-rivetTarget".to_string(),
        arg_path(target_dir, "Rivet/Loader.dll"),
        "-rivetDirectory".to_string(),
        arg_path(target_dir, "Rivet/Mods"),
      ],
      LaunchMode::Vanilla => vec!["-rivetEnable".to_string(), "false".to_string()],
    },
    // `None`, `Other`, and any loader a newer schema adds inject nothing: their
    // files simply sit in the game directory.
    _ => Vec::new(),
  };

  Ok(args)
}

/// Which assembly a Doorstop-based loader targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoorstopTarget {
  BepInEx,
  Umm,
}

/// Builds Doorstop arguments for BepInEx or UMM, using the flag spellings of the
/// version detected in the target.
///
/// Unlike r2modman, the *vanilla* flag matches the detected version too (r2modman
/// emits the v3 spelling even under v4).
///
/// `script_injected` overrides that version match: when this argv is headed for
/// the loader script (`run_bepinex.sh` / `start_game_bepinex.sh` /
/// `start_server_bepinex.sh`) rather than straight to Doorstop's own
/// command-line parser, the v4 spellings are used unconditionally, regardless
/// of what `.doorstop_version` says. That script is a Doorstop-v4-style front
/// end no matter which Doorstop generation it wraps: verified against BepInEx's
/// source, it recognizes only the v4 flags (`--doorstop-enabled`,
/// `--doorstop-target-assembly`, `--doorstop-mono-dll-search-path-override`,
/// …) and silently drops the v3 spellings (`--doorstop-enable`,
/// `--doorstop-target`, `--doorstop-dll-search-override`) as unmatched
/// passthrough arguments before `exec`-ing the game. Keeping the version-matched
/// v3 spelling here for a v3-detected target would leave the script's own
/// `enabled` default (`"1"`) in effect and its own default target assembly
/// unset by us — silently launching modded when vanilla was requested, and the
/// wrong assembly when modded was requested. This exception does not apply
/// off the script path (Windows, Proton, or a Linux native direct-exe launch),
/// where the version match above is intentional and must be preserved.
fn doorstop_args(
  target_dir: &Path,
  profile: &GameProfile,
  ctx: &LaunchContext,
  target: DoorstopTarget,
  script_injected: bool,
) -> Result<Vec<String>> {
  let version = doorstop_version(target_dir);
  let v4 = script_injected || version >= 4;

  let (enable_flag, target_flag, search_flag) = if v4 {
    (
      "--doorstop-enabled",
      "--doorstop-target-assembly",
      "--doorstop-mono-dll-search-path-override",
    )
  } else {
    (
      "--doorstop-enable",
      "--doorstop-target",
      "--doorstop-dll-search-override",
    )
  };

  if ctx.mode == LaunchMode::Vanilla {
    return Ok(vec![enable_flag.to_string(), "false".to_string()]);
  }

  let assembly = match target {
    DoorstopTarget::BepInEx => bepinex_preloader(target_dir, ctx.runtime)?,
    DoorstopTarget::Umm => arg_path(target_dir, "UMM/Core/UnityModManager.dll"),
  };

  let mut args = vec![
    enable_flag.to_string(),
    "true".to_string(),
    target_flag.to_string(),
    assembly,
  ];

  if target == DoorstopTarget::BepInEx && profile.instance_type == InstanceType::Server {
    args.push("--server".to_string());
  }

  if has_corlibs(target_dir) {
    args.push(search_flag.to_string());
    args.push(arg_path(target_dir, "unstripped_corlib"));
  }

  Ok(args)
}

/// BepisLoader (Resonite): a hookfxr hook plus a hardcoded Doorstop v4 target.
fn bepis_args(
  target_dir: &Path,
  profile: &GameProfile,
  ctx: &LaunchContext,
) -> Result<Vec<String>> {
  if ctx.mode == LaunchMode::Vanilla {
    return Ok(vec![
      "--hookfxr-disable".to_string(),
      "--doorstop-enabled".to_string(),
      "false".to_string(),
    ]);
  }

  let mut args = vec![
    "--hookfxr-enable".to_string(),
    "--bepinex-target".to_string(),
    arg_path(target_dir, "BepInEx"),
    "--doorstop-enabled".to_string(),
    "true".to_string(),
    "--doorstop-target-assembly".to_string(),
    bepinex_preloader(target_dir, ctx.runtime)?,
  ];

  if profile.instance_type == InstanceType::Server {
    args.push("--server".to_string());
  }

  if has_corlibs(target_dir) {
    args.push("--doorstop-mono-dll-search-path-override".to_string());
    args.push(arg_path(target_dir, "unstripped_corlib"));
  }

  Ok(args)
}

/// MelonLoader: point it at the target, and ask it to regenerate its assembly
/// cache when neither the ML 0.5 nor the ML 0.6 assembly layout is present.
fn melonloader_args(target_dir: &Path, ctx: &LaunchContext, target: &str) -> Vec<String> {
  if ctx.mode == LaunchMode::Vanilla {
    return vec!["--no-mods".to_string()];
  }

  let mut args = vec!["--melonloader.basedir".to_string(), target.to_string()];

  let ml_five = target_dir
    .join("MelonLoader/Managed/Assembly-CSharp.dll")
    .is_file();
  let ml_six = target_dir
    .join("MelonLoader/Il2CppAssemblies/Assembly-CSharp.dll")
    .is_file();

  if !ml_five && !ml_six {
    args.push("--melonloader.agfregenerate".to_string());
  }

  args
}

/// The Wine proxy DLL a loader hooks through under Proton.
pub fn proxy_dll(loader: LoaderKind) -> &'static str {
  match loader {
    LoaderKind::GdWeave => "winmm",
    _ => "winhttp",
  }
}

/// Whether this launch injects through a loader script, and which one.
///
/// The single fact both `resolve_program` and [`stage_for_launch_in`] decide
/// from, so a plan can never assume script injection while staging assumes the
/// proxy-DLL path, or the reverse.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ScriptInjection {
  /// Not a script-injected combination: the proxy DLL beside the executable does
  /// the injecting, and staging must put it there.
  NotApplicable,
  /// Script injection applies and the target ships this script.
  Found(PathBuf),
  /// Script injection applies but the target ships none of the scripts looked for.
  Missing,
}

impl ScriptInjection {
  /// Whether script injection applies at all — true even for [`Self::Missing`],
  /// because a Linux native script-injected launch never wants proxy files copied
  /// into its game directory regardless of which scripts the target happens to
  /// ship.
  fn applies(&self) -> bool {
    !matches!(self, Self::NotApplicable)
  }
}

/// The scripts to look for, in precedence order, for this profile's instance type.
fn script_candidates(profile: &GameProfile) -> Vec<&'static str> {
  match profile.instance_type {
    InstanceType::Server => LOADER_SCRIPTS.to_vec(),
    _ => LOADER_SCRIPTS
      .iter()
      .copied()
      .filter(|name| *name != SERVER_SCRIPT)
      .collect(),
  }
}

/// Resolves whether this loader injects through a shell script the package ships
/// into the target, rather than through a proxy DLL next to the executable, and
/// which script that is.
///
/// The store is deliberately *not* part of this decision: under Steam the same
/// script is chainloaded, just by the wrapper Steam invokes rather than by the
/// caller (see [`SteamWrapper`]).
fn script_injection(
  target_dir: &Path,
  profile: &GameProfile,
  ctx: &LaunchContext,
) -> ScriptInjection {
  let script_loader = matches!(
    profile.package_loader,
    LoaderKind::BepInEx | LoaderKind::BepisLoader
  );

  if !script_loader || ctx.os != HostOs::Linux || ctx.runtime != Runtime::Native {
    return ScriptInjection::NotApplicable;
  }

  let found = script_candidates(profile)
    .into_iter()
    .map(|name| target_dir.join(name))
    .find(|candidate| candidate.is_file());

  match found {
    Some(path) => ScriptInjection::Found(path),
    None => ScriptInjection::Missing,
  }
}

/// Whether `doorstop_args` must use the v4 flag spellings unconditionally
/// because a loader script — not Doorstop's own parser — is what will read
/// this argv. See `doorstop_args`'s doc comment for why that overrides the
/// version match.
///
/// `false` for [`ScriptInjection::Missing`] as well as [`ScriptInjection::NotApplicable`]:
/// when the target ships none of [`LOADER_SCRIPTS`], the argv reaches the game
/// (or, under Steam, the wrapper's `exec "$@"` fallthrough) directly, so the
/// version-matched spelling is the correct one there too.
fn is_script_injected(target_dir: &Path, profile: &GameProfile, ctx: &LaunchContext) -> bool {
  matches!(
    script_injection(target_dir, profile, ctx),
    ScriptInjection::Found(_)
  )
}

/// Computes the launch invocation for `target_dir`.
///
/// The caller supplies the platform facts in `ctx` and spawns the returned
/// [`LaunchPlan`]; the engine neither detects the host nor starts a process.
///
/// A Linux native Steam launch cannot be expressed by [`LaunchPlan::program`]
/// alone — that plan carries a [`LaunchPlan::steam_wrapper`] the user must install
/// into Steam's launch options first, and the caller need not re-derive when.
/// Whenever that field is populated, [`wrapper_target_args`] is already appended
/// to [`LaunchPlan::args`], so the once-pasted wrapper resolves this call's
/// `target_dir` and not whichever one was current when it was written.
///
/// Under [`LaunchMode::Modded`], a combination that injects through a loader script
/// whose target ships none of [`LOADER_SCRIPTS`] is an error, not a silent
/// fallthrough to a proxy-DLL plan that would launch the game unmodded.
///
/// macOS is not supported: r2modman injects through a compiled proxy binary with
/// no available source, so a correct plan cannot be computed (see the design
/// spec's non-goals).
///
/// # Errors
///
/// - [`Error::Profile`] if `ctx.os` is
///   [`HostOs::MacOs`], if `game` is unknown to `eco`, or if a
///   [`LaunchMode::Modded`] plan needs a loader script that `target_dir` does
///   not contain.
///
/// # Examples
///
/// The engine computes the invocation; the caller spawns it:
///
/// ```no_run
/// use std::path::Path;
/// use std::process::Command;
/// use thunderstore_engine::ecosystem::Ecosystem;
/// use thunderstore_engine::profile::launch::{
///   HostOs, LaunchContext, LaunchMode, Runtime, StorePlatform, launch_plan_in,
/// };
///
/// let eco = Ecosystem::bundled();
///
/// let ctx = LaunchContext::new(
///   HostOs::Linux,
///   StorePlatform::SteamDirect,
///   Runtime::Proton,
///   LaunchMode::Modded,
/// );
///
/// let plan = launch_plan_in(Path::new("/games/valheim/profile"), &eco, "valheim", &ctx)?;
///
/// let mut command = plan.to_command(Path::new("/games/valheim"))?;
///
/// command.spawn()?;
/// # Ok::<(), thunderstore_engine::Error>(())
/// ```
pub fn launch_plan_in(
  target_dir: &Path,
  eco: &Ecosystem,
  game: &str,
  ctx: &LaunchContext,
) -> Result<LaunchPlan> {
  if ctx.os == HostOs::MacOs {
    return Err(Error::Profile(MACOS_UNSUPPORTED.to_string()));
  }

  let profile = eco
    .game(game)
    .and_then(|g| g.profile())
    .ok_or_else(|| Error::Profile(format!("no ecosystem profile for game {game:?}")))?;

  // Computed before the argv so the loader-script decision is available for
  // `loader_args` to fold into its Doorstop flag spellings (see
  // `is_script_injected`), not just for `resolve_program` below.
  let injection = script_injection(target_dir, profile, ctx);
  let mut args = loader_args(target_dir, profile, ctx)?;

  args.extend(ctx.extra_args.iter().cloned());

  let program = resolve_program(&injection, target_dir, profile, ctx)?;
  let needs_wrapper = injection.applies() && ctx.store == StorePlatform::Steam;

  // The wrapper is written once and never rewritten, so it learns its target from
  // this argv rather than from its own contents. Appending here — rather than
  // documenting that the caller must — is what keeps a stale paste in Steam's
  // launch options from silently launching the previous target's mods.
  if needs_wrapper {
    args.extend(wrapper_target_args(target_dir));
  }

  let steam_wrapper = needs_wrapper.then(|| SteamWrapper {
    target_dir: target_dir.to_path_buf(),
    loader: profile.package_loader,
  });

  let mut env = Vec::new();

  if ctx.runtime == Runtime::Proton {
    env.push((
      "WINEDLLOVERRIDES".to_string(),
      format!("{}=n,b", proxy_dll(profile.package_loader)),
    ));
  }

  Ok(LaunchPlan {
    program,
    args,
    env,
    working_dir: None,
    steam_wrapper,
  })
}

/// Chooses what process to start for this store/platform combination.
///
/// Under Steam the loader script is not started directly even when one is present:
/// Steam's launch options are a fixed `%command%` string, so the wrapper script
/// reported through [`LaunchPlan::steam_wrapper`] is what chainloads it.
fn resolve_program(
  injection: &ScriptInjection,
  target_dir: &Path,
  profile: &GameProfile,
  ctx: &LaunchContext,
) -> Result<LaunchProgram> {
  if *injection == ScriptInjection::Missing && ctx.mode == LaunchMode::Modded {
    return Err(Error::Profile(format!(
      "modded launch injects through a loader script, but none of {} exist in {}",
      script_candidates(profile).join(", "),
      target_dir.display(),
    )));
  }

  if let ScriptInjection::Found(path) = injection
    && ctx.store != StorePlatform::Steam
  {
    return Ok(LaunchProgram::ProfileScript {
      path: path.clone(),
      exe_names: profile.exe_names.clone(),
    });
  }

  if ctx.store == StorePlatform::Steam {
    let steam_app_id = profile.steam_app_id().ok_or_else(|| {
      Error::Profile("Steam launch requested but the game has no Steam app id".to_string())
    })?;

    return Ok(LaunchProgram::Steam { steam_app_id });
  }

  Ok(LaunchProgram::GameExe {
    exe_names: profile.exe_names.clone(),
  })
}

/// Computes the launch invocation for an r2modman-layout profile.
/// See [`launch_plan_in`].
pub fn launch_plan(
  base: &Path,
  game: &str,
  profile_name: &str,
  eco: &Ecosystem,
  ctx: &LaunchContext,
) -> Result<LaunchPlan> {
  launch_plan_in(
    &crate::profile::layout::profile_dir(base, game, profile_name),
    eco,
    game,
    ctx,
  )
}

/// The flag that tells a generated wrapper which target directory to chainload.
///
/// Appended, with the target directory after it, to the argv a caller sends
/// through Steam — see [`wrapper_target_args`]. The wrapper consumes both words
/// and forwards everything else untouched, so the game never sees this flag.
pub const WRAPPER_TARGET_FLAG: &str = "--ts-target";

/// The arguments that make a generated wrapper chainload `target_dir`.
///
/// [`launch_plan_in`] already appends these to [`LaunchPlan::args`] whenever it
/// populates [`LaunchPlan::steam_wrapper`], so a caller spawning a computed plan
/// never needs to call this itself. It is public for callers assembling an argv
/// by hand, and for tests.
pub fn wrapper_target_args(target_dir: &Path) -> Vec<String> {
  vec![
    WRAPPER_TARGET_FLAG.to_string(),
    target_dir.to_string_lossy().replace('\\', "/"),
  ]
}

/// Builds the POSIX shell wrapper that Steam invokes on Linux native launches.
///
/// Steam launch options are a single fixed `%command%` string, so a file must sit
/// between Steam and the game. Steam also stores exactly **one** such string per
/// game, so this script is written once and must stay correct for every target
/// afterwards. It therefore carries no target of its own — nothing about it is
/// specific to a profile, which is why it never needs regenerating:
///
/// - `--ts-target <dir>` ([`WRAPPER_TARGET_FLAG`]) anywhere in the argv Steam
///   forwards selects that directory. [`launch_plan_in`] appends it for the
///   caller, so switching targets needs no change to the script or to Steam.
/// - Absent that flag — a bare launch straight from Steam, with no manager
///   involved — the game starts **unmodded**. There is no default profile: a
///   wrapper that quietly loaded one would make "launch from Steam" mean
///   whichever target happened to be baked in, which is the ambiguity this whole
///   mechanism exists to remove. A value that names something other than a
///   directory is likewise treated as no target at all rather than chainloaded
///   blindly, so a deleted profile yields a vanilla launch, not a broken one.
///
/// This matches r2modman's `linux_wrapper.sh` — which reads `--r2profile` from
/// argv and launches vanilla without it — with one deliberate difference: it
/// passes a profile *name* and rebuilds `<root>/profiles/<name>`, assuming the
/// r2modman layout, whereas this takes a resolved absolute directory and so also
/// serves a bare target such as a dedicated server's game directory.
///
/// The flag and its value are consumed here, never forwarded; every other
/// argument reaches the loader script in its original order, quoted, so a target
/// or game argument containing spaces survives.
///
/// The branches are generated from [`LOADER_SCRIPTS`], the same list the resolver
/// behind [`launch_plan_in`] searches, so the wrapper and the plan cannot disagree
/// about which scripts count. Falls through to `exec "$@"` — a vanilla launch —
/// when the resolved target ships none of them.
pub fn wrapper_script_contents() -> String {
  let mut branches = String::new();

  for (index, name) in LOADER_SCRIPTS.iter().enumerate() {
    let keyword = if index == 0 { "if" } else { "elif" };
    // The wrapper has no ecosystem profile at run time, so an env var stands in
    // for `InstanceType::Server`.
    let server_gate = if *name == SERVER_SCRIPT {
      r#" && [ -n "$TS_START_SERVER" ]"#
    } else {
      ""
    };

    branches.push_str(&format!(
      "{keyword} [ -f \"$TARGET/{name}\" ]{server_gate}; then\n  exec \"$TARGET/{name}\" \"$@\"\n"
    ));
  }

  // The loop rotates the positional parameters exactly `$#` times: each argument
  // is shifted off the front and, unless it is the flag or the flag's value,
  // appended to the back. `remaining` counts only the not-yet-inspected ones, so
  // already-appended arguments are never re-inspected and the surviving argv ends
  // up in its original order. This is the POSIX way to filter "$@" without
  // arrays and without `eval`.
  format!(
    r#"#!/bin/sh
# Generated by thunderstore-engine. Chainloads the mod loader for a target.
#
# Paste this into Steam's launch options once; it never needs regenerating, and
# it names no profile. The target directory is read from
# `{WRAPPER_TARGET_FLAG} <dir>` when a manager appends it to the launch
# arguments. Without it — launching straight from Steam — the game starts
# unmodded. The flag is consumed here and never reaches the game.
TARGET=""

remaining=$#

while [ "$remaining" -gt 0 ]; do
  argument="$1"
  shift
  remaining=$((remaining - 1))

  if [ "$argument" = "{WRAPPER_TARGET_FLAG}" ]; then
    if [ "$remaining" -gt 0 ]; then
      candidate="$1"
      shift
      remaining=$((remaining - 1))

      if [ -d "$candidate" ]; then
        TARGET="$candidate"
      fi
    fi

    continue
  fi

  set -- "$@" "$argument"
done

if [ -z "$TARGET" ]; then
  exec "$@"
fi

{branches}else
  exec "$@"
fi
"#
  )
}

/// Writes the wrapper script to `script_path`, marking it executable on Unix (the
/// loader scripts fail to chainload otherwise).
///
/// The script names no target, so this is written once per game and never
/// rewritten — see [`wrapper_script_contents`].
pub fn write_wrapper_script(script_path: &Path) -> Result<()> {
  if let Some(parent) = script_path.parent() {
    std::fs::create_dir_all(parent)
      .map_err(|e| Error::Profile(format!("creating {}: {}", parent.display(), e)))?;
  }

  std::fs::write(script_path, wrapper_script_contents())
    .map_err(|e| Error::Profile(format!("writing {}: {}", script_path.display(), e)))?;

  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;

    let permissions = std::fs::Permissions::from_mode(0o755);

    std::fs::set_permissions(script_path, permissions)
      .map_err(|e| Error::Profile(format!("chmod {}: {}", script_path.display(), e)))?;
  }

  Ok(())
}

/// Marks every [`LOADER_SCRIPTS`] entry present in `target_dir` executable,
/// returning the paths whose mode this call actually changed.
///
/// Thunderstore loader packages ship these scripts **without** an execute bit:
/// `BepInExPack_Valheim` 5.4.2333 records `start_game_bepinex.sh` as `0666` in its
/// archive. Both [`LaunchProgram::ProfileScript`] and the wrapper from
/// [`wrapper_script_contents`] `exec` one of them directly, so without this that
/// `exec` fails with `Permission denied`, and under Steam that error reaches only
/// Steam's own console log: the launch presents to the user as the game silently
/// never starting. The bit therefore has to come from the manager, exactly as
/// r2modman sets it. Preserving the archive's recorded mode is no substitute,
/// because there is no execute bit in the archive to preserve.
///
/// Applied at launch time rather than at install time so a target that arrived by
/// any route is covered, whether that is a fresh install, an import, an export
/// round-trip, a restored backup, or one already on disk from an older version,
/// instead of only freshly installed ones. Kept out of [`launch_plan_in`], which
/// stays a pure computation, so the caller decides when the target is written to.
/// Keyed on the same [`LOADER_SCRIPTS`] list the plan and the wrapper both resolve
/// from, so all three agree about which files have to be runnable.
///
/// A script that is already executable is left untouched, so repeated launches do
/// not rewrite modes.
pub fn ensure_loader_scripts_executable(target_dir: &Path) -> Result<Vec<PathBuf>> {
  let mut changed = Vec::new();

  for name in LOADER_SCRIPTS {
    let path = target_dir.join(name);

    if !path.is_file() {
      continue;
    }

    if make_executable(&path)? {
      changed.push(path);
    }
  }

  Ok(changed)
}

/// Grants the execute bit to exactly those classes that can already read `path`,
/// returning whether the mode changed. A no-op off Unix, where there is no such
/// bit to set.
///
/// Mirroring the read bits rather than forcing `0o755` keeps a deliberately
/// private script private: `0o640` becomes `0o750`, not world-readable. It can only
/// ever *add* execute bits, never widen read or write access.
fn make_executable(path: &Path) -> Result<bool> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::metadata(path)
      .map_err(|e| Error::Profile(format!("inspecting {}: {}", path.display(), e)))?;
    let mode = metadata.permissions().mode();
    let wanted = mode | ((mode & 0o444) >> 2);

    if wanted == mode {
      return Ok(false);
    }

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(wanted))
      .map_err(|e| Error::Profile(format!("chmod {}: {}", path.display(), e)))?;

    Ok(true)
  }

  #[cfg(not(unix))]
  {
    let _ = path;

    Ok(false)
  }
}

/// Files Shimloader stages, and only these, into the UE binaries folder.
const SHIMLOADER_STAGED: [&str; 3] = ["ue4ss.dll", "dwmapi.dll", "ue4ss-settings.ini"];

/// Where a profile-root file belongs in the game directory, or `None` when it is
/// not staged at all. Pure — no filesystem access.
pub fn stage_destination(
  loader: LoaderKind,
  file_name: &str,
  game_dir: &Path,
  data_folder: Option<&str>,
) -> Option<PathBuf> {
  let lowercased = file_name.to_ascii_lowercase();

  if lowercased == "mods.yml" {
    return None;
  }

  match loader {
    LoaderKind::Shimloader => {
      if !SHIMLOADER_STAGED.iter().any(|name| *name == lowercased) {
        return None;
      }

      let data = data_folder?;

      Some(
        game_dir
          .join(data)
          .join("Binaries")
          .join("Win64")
          .join(file_name),
      )
    }
    LoaderKind::Rivet => {
      if lowercased != "version.dll" {
        return None;
      }

      Some(game_dir.join("Release").join(file_name))
    }
    _ => Some(game_dir.join(file_name)),
  }
}

/// Copies the target's root-level loader files into `game_dir` so the proxy sits
/// beside the executable, returning the absolute paths written.
///
/// This is the only place the engine writes into a game directory, and it does so
/// only against an explicitly supplied `game_dir` — never one it discovered. Only
/// root-level *files* are staged; subdirectories (`BepInEx/`, `mods/`, …) stay in
/// the target, which is where the loader reads them from. Staging is skipped
/// entirely for a Linux native script-injected loader, whose game directory must
/// stay untouched.
///
/// `target_dir` and `game_dir` are frequently the same directory — a consumer
/// with no r2modman profile layout (e.g. one installing mods straight into the
/// game directory) always calls this with the two equal — and for most loaders
/// that makes a file's computed destination the exact file already sitting in
/// the target. `stage_one_file` detects that case by filesystem identity, not
/// path text, so it survives a `.`/`..`-relative `game_dir` or one reached
/// through a symlinked directory too, and leaves the file untouched rather than
/// copying it onto itself (which would truncate it to zero bytes). A file left
/// in place this way is **not** included in the returned `Vec<PathBuf>`: this
/// call did not write it, so reporting it as written would let a caller that
/// later deletes everything in that list destroy a file it never created. Note
/// that this per-file check is what actually matters — Shimloader routes into
/// `<game_dir>/<dataFolderName>/Binaries/Win64` and Rivet into
/// `<game_dir>/Release`, so even with `target_dir == game_dir` those
/// destinations genuinely differ from their sources and must still be copied.
///
/// A destination that is already a symlink is refused rather than followed, so a
/// pre-planted link cannot redirect a write outside `game_dir` (final path
/// component only, matching [`crate::install::apply_install`]) — including when
/// the symlink happens to point back at the source file itself; that refusal is
/// checked before the same-file identity check, so it always wins.
///
/// macOS is rejected here for the same reason [`launch_plan_in`] rejects it: no
/// correct launch can be computed for it, so the game directory must not be
/// touched on its behalf either.
///
/// This does not itself detect "nothing to stage": when `game_dir` does not
/// exist yet, listing it fails and that `read_dir` error is returned as is,
/// even for a loader whose destinations are identical to their sources. A
/// caller should check [`crate::profile::target::InstallTarget::needs_staging`]
/// first rather than rely on this function to turn that case into an empty
/// list.
pub fn stage_for_launch_in(
  target_dir: &Path,
  eco: &Ecosystem,
  game: &str,
  game_dir: &Path,
  ctx: &LaunchContext,
) -> Result<Vec<PathBuf>> {
  if ctx.os == HostOs::MacOs {
    return Err(Error::Profile(MACOS_UNSUPPORTED.to_string()));
  }

  let profile = eco
    .game(game)
    .and_then(|g| g.profile())
    .ok_or_else(|| Error::Profile(format!("no ecosystem profile for game {game:?}")))?;

  if script_injection(target_dir, profile, ctx).applies() {
    return Ok(Vec::new());
  }

  let mut written = Vec::new();

  for entry in std::fs::read_dir(target_dir)
    .map_err(|e| Error::Profile(format!("reading {}: {}", target_dir.display(), e)))?
  {
    let entry = entry.map_err(|e| Error::Profile(format!("reading an entry: {e}")))?;
    let metadata = std::fs::symlink_metadata(entry.path())
      .map_err(|e| Error::Profile(format!("inspecting {}: {}", entry.path().display(), e)))?;

    if !metadata.is_file() {
      continue;
    }

    let file_name = match entry.file_name().to_str() {
      Some(name) => name.to_string(),
      None => continue,
    };

    let dest = match stage_destination(
      profile.package_loader,
      &file_name,
      game_dir,
      profile.data_folder_name.as_deref(),
    ) {
      Some(dest) => dest,
      None => continue,
    };

    let staged = stage_one_file(&entry.path(), &dest)?;

    if staged {
      written.push(dest);
    }
  }

  written.sort();

  Ok(written)
}

/// Stages an r2modman-layout profile's loader files into `game_dir`.
/// See [`stage_for_launch_in`].
pub fn stage_for_launch(
  base: &Path,
  game: &str,
  profile_name: &str,
  eco: &Ecosystem,
  game_dir: &Path,
  ctx: &LaunchContext,
) -> Result<Vec<PathBuf>> {
  stage_for_launch_in(
    &crate::profile::layout::profile_dir(base, game, profile_name),
    eco,
    game,
    game_dir,
    ctx,
  )
}

/// Copies one staged file, creating parent directories and refusing to write
/// through a pre-existing symlink at the destination.
///
/// Returns whether a copy actually happened: `false` means `source` and `dest`
/// were already [`same_file`], so nothing was written. [`stage_for_launch_in`]
/// uses that to decide whether `dest` belongs in its returned `Vec<PathBuf>` —
/// see that function's doc comment for why an already-in-place file must not be
/// reported as written.
///
/// The symlink refusal is checked first and unconditionally, before the
/// same-file check: a destination symlink that happens to point at `source`
/// must still be refused rather than treated as "already in place", so a
/// pre-planted link can never be silently followed.
fn stage_one_file(source: &Path, dest: &Path) -> Result<bool> {
  let dest_is_symlink = std::fs::symlink_metadata(dest)
    .map(|metadata| metadata.file_type().is_symlink())
    .unwrap_or(false);

  if dest_is_symlink {
    return Err(Error::Profile(format!(
      "refusing to stage through a symlink at {}",
      dest.display()
    )));
  }

  if crate::util::same_file(source, dest) {
    return Ok(false);
  }

  if let Some(parent) = dest.parent() {
    std::fs::create_dir_all(parent)
      .map_err(|e| Error::Profile(format!("creating {}: {}", parent.display(), e)))?;
  }

  std::fs::copy(source, dest)
    .map_err(|e| Error::Profile(format!("staging {}: {}", dest.display(), e)))?;

  Ok(true)
}

/// The string a user pastes into Steam's launch options for a Linux native launch.
///
/// The engine only computes this: Steam stores launch options in `localconfig.vdf`,
/// which no manager writes (r2modman reads it to verify, never writes it), so
/// installing this is necessarily a user action.
///
/// This is the **Linux native** string, and only that, matching r2modman's
/// `WrapperArguments.ts` exactly. It carries no `WINEDLLOVERRIDES`: that is a Wine
/// setting, and no Wine is in this launch path. Nor could adding it buy Proton
/// coverage — what `script_path` names is a POSIX shell script, which is not a
/// valid `%command%` front end for a Windows binary in a Wine prefix regardless of
/// what environment variables precede it. A Proton launch cannot reach this
/// function anyway: [`LaunchPlan::steam_wrapper`] is populated only when
/// `script_injection` applies, which requires [`Runtime::Native`].
///
/// Under Proton the override *is* required — Wine prefers its own builtin
/// `winhttp` over the loader's, so the proxy never loads without it — but launch
/// options are the wrong channel for it. [`LaunchPlan::env`] carries it for the
/// direct-spawn path, and [`proxy_dll`] exposes the name for a caller writing it
/// into a prefix itself, as r2modman does via `user.reg`.
pub fn steam_launch_options(script_path: &Path) -> String {
  format!(
    r#""{script}" %command%"#,
    script = script_path.to_string_lossy().replace('\\', "/"),
  )
}

/// The first of `exe_names` present in `game_dir`.
pub fn resolve_game_exe(game_dir: &Path, exe_names: &[String]) -> Result<PathBuf> {
  exe_names
    .iter()
    .map(|name| game_dir.join(name))
    .find(|candidate| candidate.is_file())
    .ok_or_else(|| {
      Error::Profile(format!(
        "no game executable found in {}: none of {} exist",
        game_dir.display(),
        exe_names.join(", ")
      ))
    })
}

/// Where a game's Steam wrapper script is written.
///
/// One shared file for every target rather than one per profile: the script
/// resolves which directory to chainload from its argv at run time, and Steam
/// stores exactly one launch-options string per game naming this path. A
/// per-profile path would go stale the moment a user switched profiles.
pub fn wrapper_path(base: &Path, game: &str) -> PathBuf {
  base.join(game).join("steam-wrapper.sh")
}

impl LaunchPlan {
  /// Translates this plan into the process to spawn.
  ///
  /// Always an argv array, never a shell, so a path containing spaces or quotes
  /// cannot become an injection. Separate from spawning so the program,
  /// arguments, and environment can be asserted without starting anything.
  ///
  /// Every [`LaunchProgram`] variant is handled, with no wildcard arm.
  /// `LaunchProgram` is `#[non_exhaustive]`, but that only constrains other
  /// crates, so a wildcard here would be an unreachable pattern and fail
  /// `-D warnings`. Adding a variant therefore breaks this build deliberately,
  /// which is the point: a new launch shape must be translated, not silently
  /// rejected at run time.
  pub fn to_command(&self, game_dir: &Path) -> Result<std::process::Command> {
    let mut command = match &self.program {
      LaunchProgram::GameExe { exe_names } => {
        std::process::Command::new(resolve_game_exe(game_dir, exe_names)?)
      }
      LaunchProgram::Steam { steam_app_id } => {
        let mut command = std::process::Command::new("steam");

        command.arg("-applaunch").arg(steam_app_id.to_string());

        command
      }
      LaunchProgram::ProfileScript { path, exe_names } => {
        let mut command = std::process::Command::new(path);

        command.arg(resolve_game_exe(game_dir, exe_names)?);

        command
      }
    };

    command.args(&self.args);

    for (key, value) in &self.env {
      command.env(key, value);
    }

    if let Some(dir) = &self.working_dir {
      command.current_dir(dir);
    }

    Ok(command)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn doorstop_version_reads_major_and_defaults_to_three() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Missing file => v3.
    assert_eq!(doorstop_version(root), 3);

    std::fs::write(root.join(".doorstop_version"), "4.0.0").unwrap();
    assert_eq!(doorstop_version(root), 4);

    std::fs::write(root.join(".doorstop_version"), "3.7.1").unwrap();
    assert_eq!(doorstop_version(root), 3);

    // r2modman falls back to v3 for major >= 5; we treat it as v4 (its own bug).
    std::fs::write(root.join(".doorstop_version"), "5.1.0").unwrap();
    assert_eq!(doorstop_version(root), 5);

    // Unparseable => v3.
    std::fs::write(root.join(".doorstop_version"), "garbage").unwrap();
    assert_eq!(doorstop_version(root), 3);
  }

  #[test]
  fn bepinex_preloader_finds_candidate_and_prefixes_under_proton() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = root.join("BepInEx/core");

    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("BepInEx.Preloader.dll"), b"x").unwrap();
    std::fs::write(core.join("Unrelated.dll"), b"x").unwrap();

    let native = bepinex_preloader(root, Runtime::Native).unwrap();

    assert!(native.ends_with("BepInEx/core/BepInEx.Preloader.dll"));
    assert!(!native.starts_with("Z:"));

    let proton = bepinex_preloader(root, Runtime::Proton).unwrap();

    assert!(proton.starts_with("Z:"));
    assert!(proton.ends_with("BepInEx/core/BepInEx.Preloader.dll"));
  }

  #[test]
  fn bepinex_preloader_errors_when_absent() {
    let dir = tempfile::tempdir().unwrap();

    assert!(bepinex_preloader(dir.path(), Runtime::Native).is_err());
  }

  #[test]
  fn has_corlibs_detects_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    assert!(!has_corlibs(root));

    std::fs::create_dir_all(root.join("unstripped_corlib")).unwrap();

    assert!(has_corlibs(root));
  }

  fn ctx(mode: LaunchMode) -> LaunchContext {
    LaunchContext {
      os: HostOs::Windows,
      store: StorePlatform::Steam,
      runtime: Runtime::Native,
      mode,
      extra_args: Vec::new(),
    }
  }

  fn profile_with(loader: LoaderKind) -> GameProfile {
    let eco = Ecosystem::bundled();
    let mut profile = eco.game("valheim").unwrap().profile().unwrap().clone();
    profile.package_loader = loader;

    profile
  }

  #[test]
  fn bepinex_v3_and_v4_use_the_right_flag_spellings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = root.join("BepInEx/core");

    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("BepInEx.Preloader.dll"), b"x").unwrap();

    let profile = profile_with(LoaderKind::BepInEx);

    let v3 = loader_args(root, &profile, &ctx(LaunchMode::Modded)).unwrap();

    assert_eq!(v3[0], "--doorstop-enable");
    assert_eq!(v3[1], "true");
    assert_eq!(v3[2], "--doorstop-target");

    std::fs::write(root.join(".doorstop_version"), "4.0.0").unwrap();

    let v4 = loader_args(root, &profile, &ctx(LaunchMode::Modded)).unwrap();

    assert_eq!(v4[0], "--doorstop-enabled");
    assert_eq!(v4[2], "--doorstop-target-assembly");

    // Vanilla uses the matching spelling for the detected version — r2modman
    // emits the v3 spelling here even on v4, which we deliberately correct.
    let vanilla = loader_args(root, &profile, &ctx(LaunchMode::Vanilla)).unwrap();

    assert_eq!(
      vanilla,
      vec!["--doorstop-enabled".to_string(), "false".to_string()]
    );
  }

  #[test]
  fn melonloader_adds_regenerate_only_when_assemblies_are_absent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let profile = profile_with(LoaderKind::MelonLoader);

    let bare = loader_args(root, &profile, &ctx(LaunchMode::Modded)).unwrap();

    assert_eq!(bare[0], "--melonloader.basedir");
    assert!(bare.contains(&"--melonloader.agfregenerate".to_string()));

    let managed = root.join("MelonLoader/Managed");
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::write(managed.join("Assembly-CSharp.dll"), b"x").unwrap();

    let with_assembly = loader_args(root, &profile, &ctx(LaunchMode::Modded)).unwrap();

    assert!(!with_assembly.contains(&"--melonloader.agfregenerate".to_string()));
    assert_eq!(
      loader_args(root, &profile, &ctx(LaunchMode::Vanilla)).unwrap(),
      vec!["--no-mods".to_string()]
    );
  }

  #[test]
  fn single_element_flags_stay_single_elements() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let gdweave = loader_args(
      root,
      &profile_with(LoaderKind::GdWeave),
      &ctx(LaunchMode::Modded),
    )
    .unwrap();

    assert_eq!(gdweave.len(), 1);
    assert!(gdweave[0].starts_with("--gdweave-folder-override="));
    assert!(gdweave[0].ends_with("/GDWeave"));

    let northstar = loader_args(
      root,
      &profile_with(LoaderKind::Northstar),
      &ctx(LaunchMode::Modded),
    )
    .unwrap();

    assert_eq!(northstar[0], "-northstar");
    assert!(northstar[1].starts_with("-profile="));
  }

  #[test]
  fn shimloader_and_rivet_and_rom_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let shim = loader_args(
      root,
      &profile_with(LoaderKind::Shimloader),
      &ctx(LaunchMode::Modded),
    )
    .unwrap();

    assert_eq!(shim.len(), 8);
    assert_eq!(shim[0], "--mod-dir");
    assert!(shim.contains(&"--overlay-dir".to_string()));

    let rivet = loader_args(
      root,
      &profile_with(LoaderKind::Rivet),
      &ctx(LaunchMode::Modded),
    )
    .unwrap();

    assert_eq!(rivet[0], "-rivetEnable");
    assert_eq!(rivet[1], "true");
    assert!(rivet.contains(&"-rivetDirectory".to_string()));
    assert_eq!(
      loader_args(
        root,
        &profile_with(LoaderKind::Rivet),
        &ctx(LaunchMode::Vanilla)
      )
      .unwrap(),
      vec!["-rivetEnable".to_string(), "false".to_string()]
    );

    let rom = loader_args(
      root,
      &profile_with(LoaderKind::ReturnOfModding),
      &ctx(LaunchMode::Modded),
    )
    .unwrap();

    assert_eq!(rom[0], "--rom_modding_root_folder");
  }

  #[test]
  fn unknown_and_none_loaders_produce_no_args() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    assert!(
      loader_args(
        root,
        &profile_with(LoaderKind::None),
        &ctx(LaunchMode::Modded)
      )
      .unwrap()
      .is_empty()
    );
    assert!(
      loader_args(
        root,
        &profile_with(LoaderKind::Other),
        &ctx(LaunchMode::Modded)
      )
      .unwrap()
      .is_empty()
    );
  }

  #[test]
  fn server_instance_appends_server_flag_for_bepinex() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = root.join("BepInEx/core");

    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("BepInEx.Preloader.dll"), b"x").unwrap();

    let mut profile = profile_with(LoaderKind::BepInEx);
    profile.instance_type = InstanceType::Server;

    let args = loader_args(root, &profile, &ctx(LaunchMode::Modded)).unwrap();

    assert!(args.contains(&"--server".to_string()));
  }

  #[test]
  fn umm_uses_doorstop_flags_and_never_gets_server_flag() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let profile = profile_with(LoaderKind::Umm);

    let v3 = loader_args(root, &profile, &ctx(LaunchMode::Modded)).unwrap();

    assert_eq!(v3[0], "--doorstop-enable");
    assert_eq!(v3[1], "true");
    assert_eq!(v3[2], "--doorstop-target");
    assert!(v3[3].ends_with("UMM/Core/UnityModManager.dll"));

    std::fs::write(root.join(".doorstop_version"), "4.0.0").unwrap();

    let v4 = loader_args(root, &profile, &ctx(LaunchMode::Modded)).unwrap();

    assert_eq!(v4[0], "--doorstop-enabled");
    assert_eq!(v4[2], "--doorstop-target-assembly");
    assert!(v4[3].ends_with("UMM/Core/UnityModManager.dll"));

    let vanilla = loader_args(root, &profile, &ctx(LaunchMode::Vanilla)).unwrap();

    assert_eq!(
      vanilla,
      vec!["--doorstop-enabled".to_string(), "false".to_string()]
    );

    // Unlike BepInEx, UMM's `DoorstopTarget` arm never appends `--server`, even
    // when the profile's instance type is `Server`.
    let mut server_profile = profile_with(LoaderKind::Umm);
    server_profile.instance_type = InstanceType::Server;

    let server_args = loader_args(root, &server_profile, &ctx(LaunchMode::Modded)).unwrap();

    assert!(!server_args.contains(&"--server".to_string()));
  }

  #[test]
  fn bepis_loader_modded_and_vanilla_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = root.join("BepInEx/core");

    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("BepInEx.Preloader.dll"), b"x").unwrap();

    let profile = profile_with(LoaderKind::BepisLoader);

    let modded = loader_args(root, &profile, &ctx(LaunchMode::Modded)).unwrap();

    assert_eq!(modded[0], "--hookfxr-enable");
    assert_eq!(modded[1], "--bepinex-target");
    assert!(modded.contains(&"--doorstop-enabled".to_string()));
    assert!(modded.contains(&"--doorstop-target-assembly".to_string()));

    let vanilla = loader_args(root, &profile, &ctx(LaunchMode::Vanilla)).unwrap();

    assert_eq!(
      vanilla,
      vec![
        "--hookfxr-disable".to_string(),
        "--doorstop-enabled".to_string(),
        "false".to_string()
      ]
    );
  }

  #[test]
  fn lovely_modded_and_vanilla_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let modded = loader_args(
      root,
      &profile_with(LoaderKind::Lovely),
      &ctx(LaunchMode::Modded),
    )
    .unwrap();

    assert_eq!(modded[0], "--mod-dir");
    assert!(modded[1].ends_with("/mods"));

    let vanilla = loader_args(
      root,
      &profile_with(LoaderKind::Lovely),
      &ctx(LaunchMode::Vanilla),
    )
    .unwrap();

    assert_eq!(vanilla, vec!["--vanilla".to_string()]);
  }

  #[test]
  fn godot_ml_modded_and_vanilla_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let modded = loader_args(
      root,
      &profile_with(LoaderKind::GodotMl),
      &ctx(LaunchMode::Modded),
    )
    .unwrap();

    assert_eq!(modded[0], "--script");
    assert_eq!(modded[1], "res://addons/mod_loader/mod_loader_setup.gd");
    assert!(modded.contains(&"--mods-path".to_string()));

    let vanilla = loader_args(
      root,
      &profile_with(LoaderKind::GodotMl),
      &ctx(LaunchMode::Vanilla),
    )
    .unwrap();

    assert!(vanilla.is_empty());
  }

  #[test]
  fn corlib_override_appends_search_flag_pair_matching_version() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = root.join("BepInEx/core");

    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("BepInEx.Preloader.dll"), b"x").unwrap();
    std::fs::create_dir_all(root.join("unstripped_corlib")).unwrap();

    let profile = profile_with(LoaderKind::BepInEx);

    let v3 = loader_args(root, &profile, &ctx(LaunchMode::Modded)).unwrap();
    let v3_idx = v3
      .iter()
      .position(|a| a == "--doorstop-dll-search-override")
      .unwrap();

    assert!(v3[v3_idx + 1].ends_with("/unstripped_corlib"));

    std::fs::write(root.join(".doorstop_version"), "4.0.0").unwrap();

    let v4 = loader_args(root, &profile, &ctx(LaunchMode::Modded)).unwrap();
    let v4_idx = v4
      .iter()
      .position(|a| a == "--doorstop-mono-dll-search-path-override")
      .unwrap();

    assert!(v4[v4_idx + 1].ends_with("/unstripped_corlib"));

    // BepisLoader hardcodes the v4 search-flag spelling regardless of the
    // detected `.doorstop_version`.
    std::fs::remove_file(root.join(".doorstop_version")).unwrap();

    let bepis = loader_args(
      root,
      &profile_with(LoaderKind::BepisLoader),
      &ctx(LaunchMode::Modded),
    )
    .unwrap();
    let bepis_idx = bepis
      .iter()
      .position(|a| a == "--doorstop-mono-dll-search-path-override")
      .unwrap();

    assert!(bepis[bepis_idx + 1].ends_with("/unstripped_corlib"));
  }

  #[test]
  fn windows_steam_plan_targets_steam_with_loader_args() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = root.join("BepInEx/core");

    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("BepInEx.Preloader.dll"), b"x").unwrap();

    let eco = Ecosystem::bundled();
    let mut context = ctx(LaunchMode::Modded);
    context.extra_args = vec!["-nolog".to_string()];

    let plan = launch_plan_in(root, &eco, "valheim", &context).unwrap();

    match &plan.program {
      LaunchProgram::Steam { steam_app_id } => assert_eq!(*steam_app_id, 892970),
      other => panic!("expected Steam, got {other:?}"),
    }

    assert_eq!(plan.args[0], "--doorstop-enable");
    // User args come last.
    assert_eq!(plan.args.last().unwrap(), "-nolog");
    assert!(plan.env.is_empty());
    // Windows injects through the proxy DLL, so Steam needs no wrapper file.
    assert_eq!(plan.steam_wrapper, None);
  }

  #[test]
  fn proton_sets_wine_dll_overrides_and_prefixes_the_preloader() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = root.join("BepInEx/core");

    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("BepInEx.Preloader.dll"), b"x").unwrap();

    let eco = Ecosystem::bundled();
    let mut context = ctx(LaunchMode::Modded);
    context.os = HostOs::Linux;
    context.runtime = Runtime::Proton;

    let plan = launch_plan_in(root, &eco, "valheim", &context).unwrap();

    assert_eq!(
      plan.env,
      vec![("WINEDLLOVERRIDES".to_string(), "winhttp=n,b".to_string())]
    );
    assert!(plan.args.iter().any(|a| a.starts_with("Z:")));
    // Proton injects through the proxy DLL too: no script, so no wrapper.
    assert_eq!(plan.steam_wrapper, None);
  }

  #[test]
  fn linux_native_bepinex_runs_the_profile_script() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = root.join("BepInEx/core");

    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("BepInEx.Preloader.dll"), b"x").unwrap();
    std::fs::write(root.join("run_bepinex.sh"), b"#!/bin/sh\n").unwrap();

    let eco = Ecosystem::bundled();
    let mut context = ctx(LaunchMode::Modded);
    context.os = HostOs::Linux;
    context.store = StorePlatform::SteamDirect;
    context.runtime = Runtime::Native;

    let plan = launch_plan_in(root, &eco, "valheim", &context).unwrap();

    match &plan.program {
      // `exe_names` travels with the script: the caller execs the script *with*
      // the game binary, so it needs the same candidates `GameExe` hands over.
      LaunchProgram::ProfileScript { path, exe_names } => {
        assert!(path.ends_with("run_bepinex.sh"));
        assert_eq!(
          *exe_names,
          eco.game("valheim").unwrap().profile().unwrap().exe_names
        );
        assert!(!exe_names.is_empty());
      }
      other => panic!("expected ProfileScript, got {other:?}"),
    }

    // A directly launched script is not the Steam wrapper case.
    assert_eq!(plan.steam_wrapper, None);
  }

  #[test]
  fn macos_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = root.join("BepInEx/core");

    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("BepInEx.Preloader.dll"), b"x").unwrap();

    let eco = Ecosystem::bundled();
    let mut context = ctx(LaunchMode::Modded);
    context.os = HostOs::MacOs;

    let err = launch_plan_in(root, &eco, "valheim", &context).unwrap_err();

    assert!(err.to_string().contains("macOS launch is not supported"));

    // Control: the exact same directory and profile succeed once `os` is not
    // macOS, proving the error above comes from the macOS branch itself and
    // not, say, a missing BepInEx preloader fixture.
    context.os = HostOs::Windows;

    assert!(launch_plan_in(root, &eco, "valheim", &context).is_ok());
  }

  #[test]
  fn unknown_game_errors() {
    let dir = tempfile::tempdir().unwrap();
    let eco = Ecosystem::bundled();

    let err = launch_plan_in(dir.path(), &eco, "not-a-game", &ctx(LaunchMode::Modded)).unwrap_err();

    assert!(err.to_string().contains("no ecosystem profile"));
  }

  #[test]
  fn proxy_dll_is_winmm_for_gdweave() {
    assert_eq!(proxy_dll(LoaderKind::GdWeave), "winmm");
    assert_eq!(proxy_dll(LoaderKind::BepInEx), "winhttp");
  }

  #[test]
  fn wrapper_script_names_no_target_and_execs_the_loader() {
    let script = wrapper_script_contents();

    assert!(script.starts_with("#!/bin/sh"));

    // No target of any kind is baked in — that is what makes one script correct
    // for every profile, and what makes a bare launch unmodded rather than
    // whichever profile was current when it was written.
    assert!(script.contains(r#"TARGET="""#));
    assert!(script.contains(r#"[ -z "$TARGET" ]"#));

    // The run-time flag is parsed, not `eval`-ed, and its value is only adopted
    // when it names a directory.
    assert!(script.contains(&format!(r#"[ "$argument" = "{WRAPPER_TARGET_FLAG}" ]"#)));
    assert!(script.contains(r#"[ -d "$candidate" ]"#));
    assert!(!script.contains("eval"));

    // Verify all three branches are present in the correct order.
    let pos_server = script
      .find("start_server_bepinex.sh")
      .expect("server branch missing");
    let pos_run = script.find("run_bepinex.sh").expect("run branch missing");
    let pos_game = script
      .find("start_game_bepinex.sh")
      .expect("game branch missing");

    assert!(pos_server < pos_run, "server branch must come before run");
    assert!(pos_run < pos_game, "run branch must come before game");

    // The server branch must gate on TS_START_SERVER.
    assert!(script.contains("$TS_START_SERVER"));

    // The script must have the else fallthrough and be well-formed.
    assert!(script.contains("else"));
    assert!(script.contains("fi"));

    // The game command is forwarded, not swallowed.
    assert!(script.contains("\"$@\""));
  }

  #[test]
  fn write_wrapper_script_creates_an_executable_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let script_path = root.join("launch_wrapper.sh");

    write_wrapper_script(&script_path).unwrap();

    assert!(script_path.is_file());

    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;

      let mode = std::fs::metadata(&script_path)
        .unwrap()
        .permissions()
        .mode();

      assert_eq!(mode & 0o111, 0o111);
    }
  }

  /// The regression this exists for: a Thunderstore loader package ships its
  /// scripts non-executable, so a freshly installed profile cannot be launched at
  /// all until something sets the bit. Under Steam the failure is silent, so the
  /// only defense is that this runs before the wrapper `exec`s anything.
  #[test]
  #[cfg(unix)]
  fn ensure_loader_scripts_executable_adds_the_bit_and_reports_what_changed() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let game = root.join("start_game_bepinex.sh");
    let server = root.join("start_server_bepinex.sh");

    std::fs::write(&game, b"#!/bin/sh\n").unwrap();
    std::fs::write(&server, b"#!/bin/sh\n").unwrap();

    // Exactly what the package ships: readable, not executable.
    std::fs::set_permissions(&game, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o644)).unwrap();

    let changed = ensure_loader_scripts_executable(root).unwrap();

    assert_eq!(changed.len(), 2, "got: {changed:?}");

    for name in ["start_game_bepinex.sh", "start_server_bepinex.sh"] {
      let mode = std::fs::metadata(root.join(name))
        .unwrap()
        .permissions()
        .mode();

      assert_eq!(mode & 0o777, 0o755, "{name} got mode {mode:o}");
    }

    // Idempotent: a second launch must not report a change, so repeated launches
    // do not keep rewriting modes.
    assert!(ensure_loader_scripts_executable(root).unwrap().is_empty());
  }

  /// Read access is mirrored rather than forced to `0o755`, so a target whose
  /// scripts are deliberately owner-only does not become world-readable just to
  /// be launchable.
  #[test]
  #[cfg(unix)]
  fn ensure_loader_scripts_executable_mirrors_read_access() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let script = root.join("run_bepinex.sh");

    std::fs::write(&script, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o640)).unwrap();

    ensure_loader_scripts_executable(root).unwrap();

    let mode = std::fs::metadata(&script).unwrap().permissions().mode();

    assert_eq!(mode & 0o777, 0o750, "got mode {mode:o}");
  }

  /// A target with no loader scripts at all, such as a Proton profile or one
  /// whose loader injects through a proxy DLL, is not an error.
  #[test]
  fn ensure_loader_scripts_executable_ignores_a_target_with_no_scripts() {
    let dir = tempfile::tempdir().unwrap();

    assert!(
      ensure_loader_scripts_executable(dir.path())
        .unwrap()
        .is_empty()
    );
  }

  #[test]
  fn wrapper_target_args_name_the_flag_and_the_directory() {
    assert_eq!(
      wrapper_target_args(Path::new("/data/profiles/Main")),
      vec![
        WRAPPER_TARGET_FLAG.to_string(),
        "/data/profiles/Main".to_string()
      ]
    );
  }

  // --- Executing the generated wrapper.
  //
  // The wrapper is a shell script, and the bugs that matter in one — a lost
  // quote, a flag leaking through to the game, an argument reordered by the
  // filtering loop — are invisible to assertions about its text. These tests run
  // `/bin/sh` against it for real and inspect the argv the chainloaded script
  // actually received. ---

  /// Marks `path` executable, as [`write_wrapper_script`] does for the wrapper
  /// itself and as the loader scripts must be to be `exec`-ed.
  #[cfg(unix)]
  fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
  }

  /// Writes a stand-in loader script at `path` that records the argv it was
  /// invoked with into `log`, one argument per line, prefixed by its own `$0`.
  ///
  /// One line per argument is what makes the assertions meaningful: a lost quote
  /// shows up as an argument split across two lines rather than as a string that
  /// merely looks similar.
  #[cfg(unix)]
  fn recording_loader(path: &Path, log: &Path) {
    let contents = format!(
      "#!/bin/sh\nprintf '%s\\n' \"$0\" > \"{log}\"\nfor argument in \"$@\"; do\n  printf '%s\\n' \"$argument\" >> \"{log}\"\ndone\n",
      log = log.display()
    );

    std::fs::write(path, contents).unwrap();
    make_executable(path);
  }

  /// The lines `recording_loader` wrote: the chainloaded script's own path, then
  /// each argument it received.
  #[cfg(unix)]
  fn recorded(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
      .unwrap()
      .lines()
      .map(str::to_string)
      .collect()
  }

  /// Runs `wrapper` with `args`, asserting it exited successfully.
  #[cfg(unix)]
  fn run_wrapper(wrapper: &Path, args: &[&str], server: bool) {
    let mut command = std::process::Command::new(wrapper);

    command.args(args);

    if server {
      command.env("TS_START_SERVER", "1");
    } else {
      command.env_remove("TS_START_SERVER");
    }

    let output = command.output().unwrap();

    assert!(
      output.status.success(),
      "wrapper failed: {}",
      String::from_utf8_lossy(&output.stderr)
    );
  }

  #[cfg(unix)]
  #[test]
  fn wrapper_chainloads_the_target_given_at_run_time_and_consumes_only_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let fallback = dir.path().join("fallback");
    // A space in the target directory is the case that catches quoting bugs, and
    // it is a realistic profile name.
    let selected = dir.path().join("My Profile 2");
    let log = dir.path().join("argv.log");
    let wrapper = dir.path().join("steam-wrapper.sh");

    std::fs::create_dir_all(&fallback).unwrap();
    std::fs::create_dir_all(&selected).unwrap();
    recording_loader(&selected.join("run_bepinex.sh"), &log);
    write_wrapper_script(&wrapper).unwrap();

    let target = selected.to_string_lossy().to_string();

    run_wrapper(
      &wrapper,
      &[
        "/games/valheim/valheim.x86_64",
        "--doorstop-enabled",
        "true",
        WRAPPER_TARGET_FLAG,
        &target,
        "-name",
        "a server with spaces",
      ],
      false,
    );

    let lines = recorded(&log);

    // The script inside the run-time target ran — not the fallback's.
    assert_eq!(lines[0], selected.join("run_bepinex.sh").to_string_lossy());
    // Every non-flag argument, in its original order, with spaces intact; the
    // flag and its value are gone.
    assert_eq!(
      &lines[1..],
      [
        "/games/valheim/valheim.x86_64",
        "--doorstop-enabled",
        "true",
        "-name",
        "a server with spaces",
      ]
    );
  }

  #[cfg(unix)]
  #[test]
  fn wrapper_without_the_flag_launches_the_game_unmodded() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let wrapper = dir.path().join("steam-wrapper.sh");
    let game_exe = dir.path().join("valheim.x86_64");
    // A profile sitting right beside the wrapper, complete with a loader script.
    // Nothing may reach for it: no flag means no mods, not "whichever profile
    // happens to be there".
    let unrequested = dir.path().join("profile");
    let unrequested_log = dir.path().join("must-not-run.log");

    std::fs::create_dir_all(&unrequested).unwrap();
    recording_loader(&unrequested.join("run_bepinex.sh"), &unrequested_log);
    recording_loader(&game_exe, &log);
    write_wrapper_script(&wrapper).unwrap();

    // A launch straight from Steam, with no manager appending anything: exactly
    // what `%command%` expands to on its own.
    run_wrapper(&wrapper, &[&game_exe.to_string_lossy()], false);

    assert!(
      !unrequested_log.exists(),
      "a bare launch must not load any profile's mods"
    );

    let lines = recorded(&log);

    assert_eq!(lines[0], game_exe.to_string_lossy());
    assert_eq!(lines.len(), 1, "the game got extra arguments: {lines:?}");
  }

  #[cfg(unix)]
  #[test]
  fn wrapper_launches_unmodded_when_the_flags_value_is_not_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let wrapper = dir.path().join("steam-wrapper.sh");
    let game_exe = dir.path().join("valheim.x86_64");

    recording_loader(&game_exe, &log);
    write_wrapper_script(&wrapper).unwrap();

    // A profile deleted between the plan and the launch must not defeat the
    // launch, and must not be chainloaded as if it existed.
    let missing = dir.path().join("deleted-profile");

    run_wrapper(
      &wrapper,
      &[
        &game_exe.to_string_lossy(),
        WRAPPER_TARGET_FLAG,
        &missing.to_string_lossy(),
      ],
      false,
    );

    let lines = recorded(&log);

    // The flag is consumed even when its value is rejected: the game must never
    // see it.
    assert_eq!(lines[0], game_exe.to_string_lossy());
    assert_eq!(lines.len(), 1, "the game got extra arguments: {lines:?}");
  }

  #[cfg(unix)]
  #[test]
  fn wrapper_consumes_a_trailing_flag_that_has_no_value() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let wrapper = dir.path().join("steam-wrapper.sh");
    let game_exe = dir.path().join("valheim.x86_64");

    recording_loader(&game_exe, &log);
    write_wrapper_script(&wrapper).unwrap();

    run_wrapper(
      &wrapper,
      &[&game_exe.to_string_lossy(), WRAPPER_TARGET_FLAG],
      false,
    );

    let lines = recorded(&log);

    // A valueless flag names no target, so this is a vanilla launch — and the
    // dangling flag is still swallowed rather than handed to the game.
    assert_eq!(lines[0], game_exe.to_string_lossy());
    assert_eq!(lines.len(), 1, "the game got extra arguments: {lines:?}");
  }

  #[cfg(unix)]
  #[test]
  fn wrapper_execs_the_game_directly_when_the_target_ships_no_loader_script() {
    let dir = tempfile::tempdir().unwrap();
    let selected = dir.path().join("profile");
    let log = dir.path().join("argv.log");
    let wrapper = dir.path().join("steam-wrapper.sh");
    // Stand in for the game binary Steam puts at the head of `%command%`.
    let game_exe = dir.path().join("valheim.x86_64");

    std::fs::create_dir_all(&selected).unwrap();
    recording_loader(&game_exe, &log);
    write_wrapper_script(&wrapper).unwrap();

    run_wrapper(
      &wrapper,
      &[
        &game_exe.to_string_lossy(),
        WRAPPER_TARGET_FLAG,
        &selected.to_string_lossy(),
        "-window-mode",
      ],
      false,
    );

    let lines = recorded(&log);

    // `exec "$@"` reached the game itself, and the flag still did not survive
    // into its argv.
    assert_eq!(lines[0], game_exe.to_string_lossy());
    assert_eq!(&lines[1..], ["-window-mode"]);
  }

  #[cfg(unix)]
  #[test]
  fn wrapper_selects_the_server_script_only_when_ts_start_server_is_set() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let server_log = dir.path().join("server.log");
    let game_log = dir.path().join("game.log");
    let wrapper = dir.path().join("steam-wrapper.sh");

    std::fs::create_dir_all(&target).unwrap();
    // Both scripts present, which is the only way the gate is observable: the
    // server script has the higher precedence in `LOADER_SCRIPTS`, so without
    // the gate it would always win.
    recording_loader(&target.join(SERVER_SCRIPT), &server_log);
    recording_loader(&target.join("run_bepinex.sh"), &game_log);
    write_wrapper_script(&wrapper).unwrap();

    let selected = target.to_string_lossy().to_string();
    let argv = [
      "/games/valheim/valheim.x86_64",
      WRAPPER_TARGET_FLAG,
      &selected,
    ];

    run_wrapper(&wrapper, &argv, false);

    assert!(
      !server_log.exists(),
      "the server script must not run for a game instance"
    );
    assert_eq!(
      recorded(&game_log)[0],
      target.join("run_bepinex.sh").to_string_lossy()
    );

    run_wrapper(&wrapper, &argv, true);

    assert_eq!(
      recorded(&server_log)[0],
      target.join(SERVER_SCRIPT).to_string_lossy()
    );
  }

  #[test]
  fn steam_launch_options_are_only_the_wrapper_and_command_placeholder() {
    let options = steam_launch_options(Path::new("/data/launch_wrapper.sh"));

    // Byte-for-byte what r2modman's `WrapperArguments.ts` emits: a quoted path
    // and `%command%`, and nothing else.
    assert_eq!(options, r#""/data/launch_wrapper.sh" %command%"#);

    // No Wine setting rides along. This string is only ever produced for a Linux
    // native launch, where nothing in the path touches Wine, and it could not buy
    // Proton coverage anyway — see `steam_launch_options`'s doc comment.
    assert!(!options.contains("WINEDLLOVERRIDES"));
    assert!(!options.contains("winhttp"));

    // A space in the script path stays inside the quotes.
    assert_eq!(
      steam_launch_options(Path::new("/data/My Games/wrapper.sh")),
      r#""/data/My Games/wrapper.sh" %command%"#
    );
  }

  #[test]
  fn stage_destination_routes_per_loader() {
    let game = Path::new("/game");

    assert_eq!(
      stage_destination(LoaderKind::BepInEx, "winhttp.dll", game, None),
      Some(PathBuf::from("/game/winhttp.dll"))
    );
    // mods.yml is manager state, never staged.
    assert_eq!(
      stage_destination(LoaderKind::BepInEx, "mods.yml", game, None),
      None
    );

    // Shimloader: only its three files, into the UE binaries folder.
    assert_eq!(
      stage_destination(LoaderKind::Shimloader, "ue4ss.dll", game, Some("Palworld")),
      Some(PathBuf::from("/game/Palworld/Binaries/Win64/ue4ss.dll"))
    );
    assert_eq!(
      stage_destination(LoaderKind::Shimloader, "other.dll", game, Some("Palworld")),
      None
    );
    // No `dataFolderName` at all => nothing is staged.
    assert_eq!(
      stage_destination(LoaderKind::Shimloader, "ue4ss.dll", game, None),
      None
    );

    // Rivet: only version.dll, into Release.
    assert_eq!(
      stage_destination(LoaderKind::Rivet, "version.dll", game, None),
      Some(PathBuf::from("/game/Release/version.dll"))
    );
    assert_eq!(
      stage_destination(LoaderKind::Rivet, "winhttp.dll", game, None),
      None
    );
  }

  #[test]
  fn stage_for_launch_copies_root_files_only() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let game_dir = dir.path().join("game");

    std::fs::create_dir_all(target.join("BepInEx/core")).unwrap();
    std::fs::create_dir_all(&game_dir).unwrap();
    std::fs::write(target.join("winhttp.dll"), b"proxy").unwrap();
    std::fs::write(target.join("doorstop_config.ini"), b"[UnityDoorstop]").unwrap();
    std::fs::write(target.join("mods.yml"), b"[]").unwrap();
    std::fs::write(target.join("BepInEx/core/BepInEx.Preloader.dll"), b"x").unwrap();

    let eco = Ecosystem::bundled();
    let written = stage_for_launch_in(
      &target,
      &eco,
      "valheim",
      &game_dir,
      &ctx(LaunchMode::Modded),
    )
    .unwrap();

    assert!(game_dir.join("winhttp.dll").exists());
    assert!(game_dir.join("doorstop_config.ini").exists());
    // Manager state and subdirectories are not staged.
    assert!(!game_dir.join("mods.yml").exists());
    assert!(!game_dir.join("BepInEx").exists());
    assert_eq!(written.len(), 2);

    // The returned paths are exactly what was written, and sorted.
    assert!(written.contains(&game_dir.join("winhttp.dll")));
    assert!(written.contains(&game_dir.join("doorstop_config.ini")));

    let mut sorted = written.clone();
    sorted.sort();

    assert_eq!(written, sorted);
  }

  #[test]
  fn stage_for_launch_stages_shimloader_files_into_nested_binaries_dir() {
    // Palworld is a bundled Shimloader game (`dataFolderName` = "Pal"), so this
    // exercises `stage_for_launch_in` end to end through a nested destination
    // that does not exist yet, forcing `stage_one_file`'s `create_dir_all`.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let game_dir = dir.path().join("game");

    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(&game_dir).unwrap();
    std::fs::write(target.join("ue4ss.dll"), b"loader").unwrap();
    std::fs::write(target.join("dwmapi.dll"), b"proxy").unwrap();
    std::fs::write(target.join("ue4ss-settings.ini"), b"[UE4SS]").unwrap();
    std::fs::write(target.join("other.dll"), b"unrelated").unwrap();

    let eco = Ecosystem::bundled();
    let written = stage_for_launch_in(
      &target,
      &eco,
      "palworld",
      &game_dir,
      &ctx(LaunchMode::Modded),
    )
    .unwrap();

    let nested = game_dir.join("Pal").join("Binaries").join("Win64");

    assert_eq!(std::fs::read(nested.join("ue4ss.dll")).unwrap(), b"loader");
    assert!(nested.join("dwmapi.dll").exists());
    assert!(nested.join("ue4ss-settings.ini").exists());
    // Every other root file is skipped for Shimloader.
    assert!(!game_dir.join("other.dll").exists());
    assert!(!nested.join("other.dll").exists());

    assert_eq!(written.len(), 3);
    assert!(written.contains(&nested.join("ue4ss.dll")));
    assert!(written.contains(&nested.join("dwmapi.dll")));
    assert!(written.contains(&nested.join("ue4ss-settings.ini")));

    let mut sorted = written.clone();
    sorted.sort();

    assert_eq!(written, sorted);
  }

  #[test]
  fn stage_for_launch_stages_rivet_version_dll_into_release_dir() {
    // Scrap Mechanic is a bundled Rivet game, so this exercises
    // `stage_for_launch_in` end to end through the `<game>/Release` destination,
    // forcing `stage_one_file`'s `create_dir_all` for that nested directory too.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let game_dir = dir.path().join("game");

    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(&game_dir).unwrap();
    std::fs::write(target.join("version.dll"), b"rivet-loader").unwrap();
    std::fs::write(target.join("other.dll"), b"unrelated").unwrap();

    let eco = Ecosystem::bundled();
    let written = stage_for_launch_in(
      &target,
      &eco,
      "scrap-mechanic",
      &game_dir,
      &ctx(LaunchMode::Modded),
    )
    .unwrap();

    let dest = game_dir.join("Release").join("version.dll");

    assert_eq!(std::fs::read(&dest).unwrap(), b"rivet-loader");
    // Every other root file is skipped for Rivet.
    assert!(!game_dir.join("other.dll").exists());
    assert!(!game_dir.join("Release").join("other.dll").exists());

    assert_eq!(written, vec![dest]);
  }

  #[test]
  fn stage_for_launch_skips_linux_native_bepinex() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let game_dir = dir.path().join("game");

    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(&game_dir).unwrap();
    std::fs::write(target.join("winhttp.dll"), b"proxy").unwrap();

    let eco = Ecosystem::bundled();
    let mut context = ctx(LaunchMode::Modded);
    context.os = HostOs::Linux;
    context.runtime = Runtime::Native;

    let written = stage_for_launch_in(&target, &eco, "valheim", &game_dir, &context).unwrap();

    assert!(written.is_empty());
    assert!(!game_dir.join("winhttp.dll").exists());
  }

  #[cfg(unix)]
  #[test]
  fn stage_for_launch_refuses_to_write_through_a_symlink() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let game_dir = dir.path().join("game");
    let outside = dir.path().join("secret.dll");

    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(&game_dir).unwrap();
    std::fs::write(&outside, b"original").unwrap();
    std::fs::write(target.join("winhttp.dll"), b"proxy").unwrap();
    symlink(&outside, game_dir.join("winhttp.dll")).unwrap();

    let eco = Ecosystem::bundled();

    assert!(
      stage_for_launch_in(
        &target,
        &eco,
        "valheim",
        &game_dir,
        &ctx(LaunchMode::Modded)
      )
      .is_err()
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"original");
  }

  #[test]
  fn stage_for_launch_in_place_leaves_root_files_byte_for_byte_untouched() {
    // `target_dir == game_dir` is the layout a consumer with no r2modman
    // profile uses when it installs mods straight into the game directory
    // (see `stage_for_launch_in`'s doc comment). Before the fix, `stage_one_file`
    // called `std::fs::copy(path, path)` here, which truncates the destination
    // to zero bytes: `std::fs::copy` opens the destination with `O_TRUNC`
    // before it ever reads the source.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("game");

    std::fs::create_dir_all(root.join("BepInEx/core")).unwrap();

    let winhttp_contents = b"MZ-fake-proxy-dll-18b";
    let ini_contents = b"[UnityDoorstop]\nenabled=true\n";

    std::fs::write(root.join("winhttp.dll"), winhttp_contents).unwrap();
    std::fs::write(root.join("doorstop_config.ini"), ini_contents).unwrap();
    std::fs::write(root.join("mods.yml"), b"[]").unwrap();
    std::fs::write(root.join("BepInEx/core/BepInEx.Preloader.dll"), b"x").unwrap();

    let eco = Ecosystem::bundled();
    let written =
      stage_for_launch_in(&root, &eco, "valheim", &root, &ctx(LaunchMode::Modded)).unwrap();

    // Contents, not mere existence — a zero-byte file still "exists", which is
    // precisely how the truncation bug hid behind every prior test.
    assert_eq!(
      std::fs::read(root.join("winhttp.dll")).unwrap(),
      winhttp_contents
    );
    assert_eq!(
      std::fs::read(root.join("doorstop_config.ini")).unwrap(),
      ini_contents
    );

    // Neither file was actually written by this call — both were already
    // correctly in place — so neither is reported as written. See
    // `stage_for_launch_in`'s doc comment: over-reporting here would let a
    // caller that deletes everything in this list destroy a file it never
    // created.
    assert!(written.is_empty());
  }

  #[test]
  fn stage_for_launch_in_place_survives_a_dot_dot_aliased_game_dir() {
    // `target/BepInEx/..` is not lexically equal to `target`, but resolves to
    // the exact same directory. The identity check must recognize this via
    // canonicalization rather than string comparison, or this differently
    // spelled `game_dir` would reintroduce the truncation bug.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");

    std::fs::create_dir_all(target.join("BepInEx/core")).unwrap();

    let contents = b"proxy-bytes-not-truncated";

    std::fs::write(target.join("winhttp.dll"), contents).unwrap();

    let aliased_game_dir = target.join("BepInEx").join("..");
    let eco = Ecosystem::bundled();

    let written = stage_for_launch_in(
      &target,
      &eco,
      "valheim",
      &aliased_game_dir,
      &ctx(LaunchMode::Modded),
    )
    .unwrap();

    assert_eq!(std::fs::read(target.join("winhttp.dll")).unwrap(), contents);
    assert!(written.is_empty());
  }

  #[cfg(unix)]
  #[test]
  fn stage_for_launch_in_place_survives_a_symlinked_game_dir() {
    use std::os::unix::fs::symlink;

    // A `game_dir` reached through a symlinked directory must alias `target_dir`
    // exactly as the `..`-relative case above does, even though it is not
    // string-comparable to `target_dir` at all.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let link = dir.path().join("game-link");

    std::fs::create_dir_all(target.join("BepInEx/core")).unwrap();

    let contents = b"proxy-bytes-via-symlink";

    std::fs::write(target.join("winhttp.dll"), contents).unwrap();
    symlink(&target, &link).unwrap();

    let eco = Ecosystem::bundled();
    let written =
      stage_for_launch_in(&target, &eco, "valheim", &link, &ctx(LaunchMode::Modded)).unwrap();

    assert_eq!(std::fs::read(target.join("winhttp.dll")).unwrap(), contents);
    assert!(written.is_empty());
  }

  #[test]
  fn stage_for_launch_in_place_still_copies_shimloader_files_to_a_distinct_destination() {
    // Palworld is a bundled Shimloader game (`dataFolderName` = "Pal"). Even
    // with `target_dir == game_dir`, Shimloader's destination
    // (`<game_dir>/Pal/Binaries/Win64/<file>`) genuinely differs from its
    // source, so the identity check must not skip these — only a destination
    // identical to its source is exempted from copying.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("game");

    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("ue4ss.dll"), b"loader").unwrap();
    std::fs::write(root.join("dwmapi.dll"), b"proxy").unwrap();
    std::fs::write(root.join("ue4ss-settings.ini"), b"[UE4SS]").unwrap();

    let eco = Ecosystem::bundled();
    let written =
      stage_for_launch_in(&root, &eco, "palworld", &root, &ctx(LaunchMode::Modded)).unwrap();

    let nested = root.join("Pal").join("Binaries").join("Win64");

    assert_eq!(std::fs::read(nested.join("ue4ss.dll")).unwrap(), b"loader");
    assert_eq!(std::fs::read(nested.join("dwmapi.dll")).unwrap(), b"proxy");
    // The source files are untouched by their own copy, and still present at
    // the target root (which is also `game_dir` here).
    assert_eq!(std::fs::read(root.join("ue4ss.dll")).unwrap(), b"loader");

    assert_eq!(written.len(), 3);
    assert!(written.contains(&nested.join("ue4ss.dll")));
    assert!(written.contains(&nested.join("dwmapi.dll")));
    assert!(written.contains(&nested.join("ue4ss-settings.ini")));
  }

  #[test]
  fn stage_for_launch_in_place_still_copies_rivet_version_dll_to_a_distinct_destination() {
    // Scrap Mechanic is a bundled Rivet game. Its destination
    // (`<game_dir>/Release/version.dll`) differs from its source even when
    // `target_dir == game_dir`, so it must still be copied rather than skipped.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("game");

    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("version.dll"), b"rivet-loader").unwrap();

    let eco = Ecosystem::bundled();
    let written = stage_for_launch_in(
      &root,
      &eco,
      "scrap-mechanic",
      &root,
      &ctx(LaunchMode::Modded),
    )
    .unwrap();

    let dest = root.join("Release").join("version.dll");

    assert_eq!(std::fs::read(&dest).unwrap(), b"rivet-loader");
    assert_eq!(
      std::fs::read(root.join("version.dll")).unwrap(),
      b"rivet-loader"
    );
    assert_eq!(written, vec![dest]);
  }

  #[test]
  fn stage_for_launch_wraps_stage_for_launch_in_over_a_layout_profile() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base");
    let game_dir = dir.path().join("game");

    let profile_dir = crate::profile::layout::create(&base, "valheim", "Main").unwrap();
    let core = profile_dir.join("BepInEx/core");

    std::fs::create_dir_all(&core).unwrap();
    std::fs::create_dir_all(&game_dir).unwrap();
    std::fs::write(core.join("BepInEx.Preloader.dll"), b"x").unwrap();
    std::fs::write(profile_dir.join("winhttp.dll"), b"proxy").unwrap();
    std::fs::write(profile_dir.join("mods.yml"), b"[]").unwrap();

    let eco = Ecosystem::bundled();
    let written = stage_for_launch(
      &base,
      "valheim",
      "Main",
      &eco,
      &game_dir,
      &ctx(LaunchMode::Modded),
    )
    .unwrap();

    assert!(game_dir.join("winhttp.dll").exists());
    assert!(!game_dir.join("mods.yml").exists());
    assert_eq!(written, vec![game_dir.join("winhttp.dll")]);
  }

  /// A Linux native `LaunchContext` for a directly executed (non-Steam) game.
  fn linux_native(mode: LaunchMode) -> LaunchContext {
    let mut context = ctx(mode);

    context.os = HostOs::Linux;
    context.store = StorePlatform::SteamDirect;
    context.runtime = Runtime::Native;

    context
  }

  /// A target that looks like a real Linux BepInEx install, minus any loader
  /// script: preloader, proxy DLL, and Doorstop ini.
  fn linux_bepinex_target(root: &Path) {
    let core = root.join("BepInEx/core");

    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("BepInEx.Preloader.dll"), b"x").unwrap();
    std::fs::write(root.join("winhttp.dll"), b"proxy").unwrap();
    std::fs::write(root.join("doorstop_config.ini"), b"[UnityDoorstop]").unwrap();
  }

  #[test]
  fn linux_native_modded_selects_any_shipped_script_not_only_run_bepinex() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let game_dir = dir.path().join("game");

    linux_bepinex_target(&target);
    std::fs::create_dir_all(&game_dir).unwrap();
    // This package ships `start_game_bepinex.sh`; there is no `run_bepinex.sh`.
    std::fs::write(target.join("start_game_bepinex.sh"), b"#!/bin/sh\n").unwrap();

    let eco = Ecosystem::bundled();
    let context = linux_native(LaunchMode::Modded);

    let plan = launch_plan_in(&target, &eco, "valheim", &context).unwrap();

    match &plan.program {
      LaunchProgram::ProfileScript { path, .. } => {
        assert!(path.ends_with("start_game_bepinex.sh"))
      }
      // Falling through to `GameExe` here is the silent-vanilla bug: Doorstop
      // argv nothing reads, and no proxy staged into the game directory.
      other => panic!("expected ProfileScript, got {other:?}"),
    }

    // Staging must agree with the plan rather than decide independently.
    let written = stage_for_launch_in(&target, &eco, "valheim", &game_dir, &context).unwrap();

    assert!(written.is_empty());
    assert!(!game_dir.join("winhttp.dll").exists());
  }

  #[test]
  fn server_instance_prefers_the_server_script_and_a_game_instance_ignores_it() {
    let dir = tempfile::tempdir().unwrap();
    let all_three = dir.path().join("all-three");
    let server_only = dir.path().join("server-only");

    linux_bepinex_target(&all_three);
    linux_bepinex_target(&server_only);

    for name in LOADER_SCRIPTS {
      std::fs::write(all_three.join(name), b"#!/bin/sh\n").unwrap();
    }

    std::fs::write(server_only.join(SERVER_SCRIPT), b"#!/bin/sh\n").unwrap();

    let context = linux_native(LaunchMode::Modded);
    let game_profile = profile_with(LoaderKind::BepInEx);
    let mut server_profile = profile_with(LoaderKind::BepInEx);

    server_profile.instance_type = InstanceType::Server;

    let server = resolve_program(
      &script_injection(&all_three, &server_profile, &context),
      &all_three,
      &server_profile,
      &context,
    )
    .unwrap();

    match &server {
      LaunchProgram::ProfileScript { path, .. } => {
        assert!(path.ends_with("start_server_bepinex.sh"))
      }
      other => panic!("expected ProfileScript, got {other:?}"),
    }

    // A game instance skips the server script even when it is present and first
    // in precedence — exec'ing it would start a dedicated server.
    let game = resolve_program(
      &script_injection(&all_three, &game_profile, &context),
      &all_three,
      &game_profile,
      &context,
    )
    .unwrap();

    match &game {
      LaunchProgram::ProfileScript { path, .. } => assert!(path.ends_with("run_bepinex.sh")),
      other => panic!("expected ProfileScript, got {other:?}"),
    }

    // With only the server script on disk, a game instance has no script at all.
    let err = resolve_program(
      &script_injection(&server_only, &game_profile, &context),
      &server_only,
      &game_profile,
      &context,
    )
    .unwrap_err();

    assert!(err.to_string().contains("run_bepinex.sh"));
  }

  #[test]
  fn modded_script_injection_without_a_script_errors_but_vanilla_falls_through() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");

    linux_bepinex_target(&target);

    let eco = Ecosystem::bundled();
    let modded = linux_native(LaunchMode::Modded);

    let err = launch_plan_in(&target, &eco, "valheim", &modded).unwrap_err();
    let message = err.to_string();

    assert!(message.contains("run_bepinex.sh"), "{message}");
    assert!(message.contains("start_game_bepinex.sh"), "{message}");
    assert!(message.contains(&target.display().to_string()), "{message}");

    // Vanilla asks for no injection at all, so a missing injector is not a
    // failure: the game binary runs with the loader's disable flag.
    let vanilla =
      launch_plan_in(&target, &eco, "valheim", &linux_native(LaunchMode::Vanilla)).unwrap();

    assert!(matches!(vanilla.program, LaunchProgram::GameExe { .. }));
    assert_eq!(
      vanilla.args,
      vec!["--doorstop-enable".to_string(), "false".to_string()]
    );
  }

  #[test]
  fn linux_native_steam_launches_steam_and_reports_the_wrapper_requirement() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let game_dir = dir.path().join("game");
    let script_path = dir.path().join("launch_wrapper.sh");

    linux_bepinex_target(&target);
    std::fs::create_dir_all(&game_dir).unwrap();
    std::fs::write(target.join("run_bepinex.sh"), b"#!/bin/sh\n").unwrap();

    let eco = Ecosystem::bundled();
    let mut context = linux_native(LaunchMode::Modded);

    context.store = StorePlatform::Steam;

    let plan = launch_plan_in(&target, &eco, "valheim", &context).unwrap();

    // Steam cannot be handed a script as its program: it takes `-applaunch`.
    match &plan.program {
      LaunchProgram::Steam { steam_app_id } => assert_eq!(*steam_app_id, 892970),
      other => panic!("expected Steam, got {other:?}"),
    }

    // The wrapper requirement is discoverable from the plan itself, and carries
    // everything the two wrapper functions need — no reimplementing the rule and
    // no second ecosystem lookup.
    let wrapper = plan.steam_wrapper.clone().expect("wrapper requirement");

    assert_eq!(wrapper.target_dir, target);
    assert_eq!(wrapper.loader, LoaderKind::BepInEx);

    write_wrapper_script(&script_path).unwrap();

    let options = steam_launch_options(&script_path);

    assert!(options.contains(&script_path.display().to_string()));
    assert!(!options.contains("WINEDLLOVERRIDES"));
    assert!(
      std::fs::read_to_string(&script_path)
        .unwrap()
        .contains("run_bepinex.sh")
    );

    // The plan already carries the target flag, so a caller that spawns it
    // verbatim reaches this target through a wrapper written for any other one.
    assert!(
      plan
        .args
        .windows(2)
        .any(|pair| pair == wrapper_target_args(&target).as_slice()),
      "plan args must carry the wrapper target flag: {:?}",
      plan.args
    );

    // Injection is still script-based, so the game directory stays untouched.
    let written = stage_for_launch_in(&target, &eco, "valheim", &game_dir, &context).unwrap();

    assert!(written.is_empty());
    assert!(!game_dir.join("winhttp.dll").exists());
  }

  #[test]
  fn stage_for_launch_rejects_macos() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let game_dir = dir.path().join("game");

    linux_bepinex_target(&target);
    std::fs::create_dir_all(&game_dir).unwrap();

    let eco = Ecosystem::bundled();
    let mut context = ctx(LaunchMode::Modded);

    context.os = HostOs::MacOs;

    let err = stage_for_launch_in(&target, &eco, "valheim", &game_dir, &context).unwrap_err();

    assert!(err.to_string().contains("macOS launch is not supported"));
    // Nothing was written: `launch_plan_in` refuses macOS, so staging must not
    // dirty the game directory on its behalf.
    assert_eq!(std::fs::read_dir(&game_dir).unwrap().count(), 0);

    // Control: the same target and game directory stage fine once `os` is not
    // macOS, proving the error comes from the macOS branch itself.
    context.os = HostOs::Windows;

    let written = stage_for_launch_in(&target, &eco, "valheim", &game_dir, &context).unwrap();

    assert_eq!(written.len(), 2);
    assert!(game_dir.join("winhttp.dll").exists());
  }

  #[test]
  fn only_a_wrapper_bound_plan_carries_the_target_flag() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");

    linux_bepinex_target(&target);
    std::fs::write(target.join("run_bepinex.sh"), b"#!/bin/sh\n").unwrap();

    let eco = Ecosystem::bundled();

    // Linux native, but launched by executing the script directly: no wrapper
    // stands between the caller and the loader, so the flag would be a stray
    // argument the game has to tolerate.
    let direct =
      launch_plan_in(&target, &eco, "valheim", &linux_native(LaunchMode::Modded)).unwrap();

    assert_eq!(direct.steam_wrapper, None);
    assert!(!direct.args.contains(&WRAPPER_TARGET_FLAG.to_string()));

    // Steam, but injecting through the proxy DLL rather than a script: still no
    // wrapper, still no flag.
    let mut proton = linux_native(LaunchMode::Modded);

    proton.store = StorePlatform::Steam;
    proton.runtime = Runtime::Proton;

    let proton_plan = launch_plan_in(&target, &eco, "valheim", &proton).unwrap();

    assert_eq!(proton_plan.steam_wrapper, None);
    assert!(!proton_plan.args.contains(&WRAPPER_TARGET_FLAG.to_string()));
  }

  #[test]
  fn wrapper_branches_are_generated_from_the_shared_script_list() {
    let script = wrapper_script_contents();

    let mut previous = 0;

    for name in LOADER_SCRIPTS {
      let position = script
        .find(&format!("[ -f \"$TARGET/{name}\" ]"))
        .unwrap_or_else(|| panic!("wrapper is missing a branch for {name}"));

      assert!(position >= previous, "{name} is out of precedence order");

      previous = position;
    }
  }

  // --- Script-path Doorstop spelling (v3-detected target injected via a loader
  // script must still use v4 flags, because the script itself only understands
  // v4 spellings; see `doorstop_args`'s doc comment) ---

  #[test]
  fn linux_native_script_present_vanilla_runs_the_script_with_v4_enabled_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");

    linux_bepinex_target(&target);
    std::fs::write(target.join("run_bepinex.sh"), b"#!/bin/sh\n").unwrap();
    // No `.doorstop_version` file: `doorstop_version` defaults to 3, but the
    // script only recognizes the v4 spelling — see `doorstop_args`'s doc comment.

    let eco = Ecosystem::bundled();
    let plan =
      launch_plan_in(&target, &eco, "valheim", &linux_native(LaunchMode::Vanilla)).unwrap();

    // Vanilla still runs the script: the script itself honours the disable
    // flag, so there is no reason to fall back to the bare game binary.
    match &plan.program {
      LaunchProgram::ProfileScript { path, .. } => assert!(path.ends_with("run_bepinex.sh")),
      other => panic!("expected ProfileScript, got {other:?}"),
    }

    assert_eq!(
      plan.args,
      vec!["--doorstop-enabled".to_string(), "false".to_string()]
    );
  }

  #[test]
  fn linux_native_script_present_modded_uses_v4_target_assembly_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");

    linux_bepinex_target(&target);
    std::fs::write(target.join("run_bepinex.sh"), b"#!/bin/sh\n").unwrap();
    // No `.doorstop_version` file here either: same v3-defaults-but-script-wants-v4
    // situation as the vanilla case above.

    let eco = Ecosystem::bundled();
    let plan = launch_plan_in(&target, &eco, "valheim", &linux_native(LaunchMode::Modded)).unwrap();

    assert!(matches!(plan.program, LaunchProgram::ProfileScript { .. }));
    assert_eq!(plan.args[0], "--doorstop-enabled");
    assert_eq!(plan.args[1], "true");
    assert_eq!(plan.args[2], "--doorstop-target-assembly");
    assert!(plan.args[3].ends_with("BepInEx/core/BepInEx.Preloader.dll"));
  }

  #[test]
  fn linux_native_script_present_v3_detected_corlib_override_uses_v4_search_flag_spelling() {
    // The third of the three doorstop-flag pairs, and the one the fix's other
    // script-path tests don't touch: `linux_bepinex_target` never creates
    // `unstripped_corlib`, so without this test the corlib search-path flag on
    // the script-injected + v3-detected path is only correct by inference from
    // the shared `if v4` tuple in `doorstop_args`, not by a direct assertion.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");

    linux_bepinex_target(&target);
    std::fs::write(target.join("run_bepinex.sh"), b"#!/bin/sh\n").unwrap();
    std::fs::create_dir_all(target.join("unstripped_corlib")).unwrap();
    // No `.doorstop_version` file: detected version defaults to v3, but the
    // script on disk forces the v4 spellings — see `doorstop_args`'s doc comment.

    let eco = Ecosystem::bundled();
    let plan = launch_plan_in(&target, &eco, "valheim", &linux_native(LaunchMode::Modded)).unwrap();

    assert!(matches!(plan.program, LaunchProgram::ProfileScript { .. }));

    let idx = plan
      .args
      .iter()
      .position(|a| a == "--doorstop-mono-dll-search-path-override")
      .expect("v4 corlib search-path flag missing");

    assert!(plan.args[idx + 1].ends_with("/unstripped_corlib"));
    assert!(
      !plan
        .args
        .iter()
        .any(|a| a == "--doorstop-dll-search-override"),
      "v3 corlib search-path spelling must not appear on the script-injected path: {:?}",
      plan.args
    );
  }

  #[test]
  fn linux_native_script_present_v4_detected_target_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");

    linux_bepinex_target(&target);
    std::fs::write(target.join("run_bepinex.sh"), b"#!/bin/sh\n").unwrap();
    std::fs::write(target.join(".doorstop_version"), "4.0.0").unwrap();

    let eco = Ecosystem::bundled();

    let vanilla =
      launch_plan_in(&target, &eco, "valheim", &linux_native(LaunchMode::Vanilla)).unwrap();

    assert_eq!(
      vanilla.args,
      vec!["--doorstop-enabled".to_string(), "false".to_string()]
    );

    let modded =
      launch_plan_in(&target, &eco, "valheim", &linux_native(LaunchMode::Modded)).unwrap();

    assert_eq!(modded.args[0], "--doorstop-enabled");
    assert_eq!(modded.args[2], "--doorstop-target-assembly");
  }

  #[test]
  fn proton_v3_detected_target_still_uses_v3_spellings_even_with_a_script_on_disk() {
    // Regression guard: script-path spelling must key off actually running
    // through the script (Linux + native + a found script), not merely off
    // `HostOs::Linux` or the script file's mere presence on disk. A Proton
    // (Wine) launch never goes through the loader script — `script_injection`
    // requires `Runtime::Native` — so even with `run_bepinex.sh` sitting in the
    // target and no `.doorstop_version` file, the argv must still get the
    // version-matched (v3) spelling, because it is Doorstop's own parser, not
    // the script, that will read it under Proton.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");

    linux_bepinex_target(&target);
    std::fs::write(target.join("run_bepinex.sh"), b"#!/bin/sh\n").unwrap();

    let eco = Ecosystem::bundled();
    let mut context = linux_native(LaunchMode::Modded);

    context.runtime = Runtime::Proton;

    let plan = launch_plan_in(&target, &eco, "valheim", &context).unwrap();

    assert_eq!(plan.args[0], "--doorstop-enable");
    assert_eq!(plan.args[2], "--doorstop-target");

    let mut vanilla_context = linux_native(LaunchMode::Vanilla);

    vanilla_context.runtime = Runtime::Proton;

    let vanilla = launch_plan_in(&target, &eco, "valheim", &vanilla_context).unwrap();

    assert_eq!(
      vanilla.args,
      vec!["--doorstop-enable".to_string(), "false".to_string()]
    );
  }

  #[test]
  fn windows_v3_detected_target_still_uses_v3_spellings_regression_guard() {
    // Regression guard for the non-script paths named in the fix: a
    // v3-detected target launched via argv (not a script) must keep the v3
    // spellings, on Windows exactly as before this fix.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = root.join("BepInEx/core");

    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("BepInEx.Preloader.dll"), b"x").unwrap();

    let eco = Ecosystem::bundled();
    let plan = launch_plan_in(root, &eco, "valheim", &ctx(LaunchMode::Modded)).unwrap();

    assert_eq!(plan.args[0], "--doorstop-enable");
    assert_eq!(plan.args[2], "--doorstop-target");

    let vanilla = launch_plan_in(root, &eco, "valheim", &ctx(LaunchMode::Vanilla)).unwrap();

    assert_eq!(
      vanilla.args,
      vec!["--doorstop-enable".to_string(), "false".to_string()]
    );
  }

  #[test]
  fn store_and_runtime_parse_from_their_config_spellings() {
    assert_eq!(
      "steam".parse::<StorePlatform>().unwrap(),
      StorePlatform::Steam
    );
    assert_eq!(
      "steam-direct".parse::<StorePlatform>().unwrap(),
      StorePlatform::SteamDirect
    );
    assert_eq!(
      "other".parse::<StorePlatform>().unwrap(),
      StorePlatform::Other
    );
    assert_eq!("native".parse::<Runtime>().unwrap(), Runtime::Native);
    assert_eq!("proton".parse::<Runtime>().unwrap(), Runtime::Proton);

    // An unknown value names itself, so a caller can quote it back.
    let error = "wat".parse::<StorePlatform>().unwrap_err().to_string();

    assert!(error.contains("wat"), "got: {error}");
  }

  #[test]
  fn resolve_game_exe_returns_the_first_present_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path();

    std::fs::write(game_dir.join("second.x86_64"), b"elf").unwrap();

    let names = vec!["first.exe".to_string(), "second.x86_64".to_string()];
    let found = resolve_game_exe(game_dir, &names).unwrap();

    assert_eq!(found, game_dir.join("second.x86_64"));

    // Nothing present names every candidate it looked for.
    let error = resolve_game_exe(game_dir, &["absent.exe".to_string()])
      .unwrap_err()
      .to_string();

    assert!(error.contains("absent.exe"), "got: {error}");
  }

  #[test]
  fn a_plan_becomes_a_spawnable_command() {
    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path();

    std::fs::write(game_dir.join("valheim.x86_64"), b"elf").unwrap();

    // `LaunchPlan` derives no `Default`, so every field is named. It is
    // `#[non_exhaustive]`, which only restricts other crates, so a struct literal
    // is fine inside this module.
    let plan = LaunchPlan {
      program: LaunchProgram::GameExe {
        exe_names: vec!["valheim.x86_64".to_string()],
      },
      args: vec!["-console".to_string()],
      env: vec![("KEY".to_string(), "VALUE".to_string())],
      working_dir: Some(game_dir.to_path_buf()),
      steam_wrapper: None,
    };

    let command = plan.to_command(game_dir).unwrap();

    assert_eq!(command.get_program(), game_dir.join("valheim.x86_64"));
    assert_eq!(
      command.get_args().collect::<Vec<_>>(),
      vec![std::ffi::OsStr::new("-console")]
    );
    assert_eq!(command.get_current_dir(), Some(game_dir));
    assert!(
      command
        .get_envs()
        .any(|(key, value)| key == std::ffi::OsStr::new("KEY")
          && value == Some(std::ffi::OsStr::new("VALUE")))
    );
  }
}
