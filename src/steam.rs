//! Host facts about the Steam client: where its executable lives, and whether it
//! can resolve a path a caller hands it.
//!
//! # Locating the executable
//!
//! [`crate::steam::steam_exe`] answers "what program starts Steam on this
//! machine". Steam is on `PATH` on Linux, so a bare `steam` is correct there,
//! but its Windows installer adds nothing to `PATH`: `CreateProcess` appends
//! `.exe` to a bare program name yet still has nowhere to look, so the spawn
//! fails outright and the ordinary Steam configuration cannot launch at all.
//!
//! # Namespace visibility
//!
//! Steam's launch options are a string the caller never opens itself: it names
//! a wrapper script and a target directory, and *Steam* is what has to resolve
//! them. On a sandboxed Steam those are resolved in a different mount
//! namespace, so a path that exists perfectly well for the caller can be
//! absent for Steam. When that happens nothing on the caller's side fails,
//! since `steam -applaunch` returns 0 either way, and the only record is a
//! line in Steam's own console log. The launch simply appears to do nothing.
//!
//! This module makes that failure loud *before* a launch, by asking the running
//! Steam client's own mount namespace whether it can see the path.
//!
//! This is host inspection, shared here in the engine rather than kept private
//! to one client, because every client on Linux hits sandboxed Steam (Flatpak,
//! Snap, containerized), and the failure mode is a launch that appears to do
//! nothing, with the only record in Steam's own console log. Sharing the probe
//! is worth the impurity of reading the host machine.

#![deny(missing_docs)]

use std::path::{Path, PathBuf};

/// Path prefixes a sandboxed Steam is known to replace with a private mount, used
/// only when no running Steam client is available to ask directly.
///
/// These are per-namespace or per-boot scratch locations that no persistent game
/// data belongs in regardless of Steam, so flagging them is honest even against an
/// unsandboxed Steam that could in fact read them.
const SUSPECT_PREFIXES: [&str; 4] = ["/tmp", "/var/tmp", "/dev/shm", "/run/user"];

/// What was determined about Steam's ability to resolve a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
  /// Present in the running Steam client's mount namespace.
  Reachable,
  /// Proven absent from the running Steam client's mount namespace. A launch using
  /// this path would fail inside Steam and report nowhere the caller can see.
  Unreachable {
    /// PID of the running Steam client whose mount namespace was inspected.
    steam_pid: u32,
  },
  /// No running Steam client could be inspected, so nothing was proven. `suspect`
  /// marks a path under a `SUSPECT_PREFIXES` entry, which is the shape of path
  /// that fails once Steam *is* sandboxed.
  Unknown {
    /// Whether the path sits under a prefix a sandboxed Steam is known to
    /// replace with a private mount.
    suspect: bool,
  },
}

/// How a caller should treat a [`Visibility`] verdict.
///
/// The mapping lives here so every client agrees on refuse-versus-warn; the
/// wording of each refusal or warning is each client's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
  /// Nothing to report.
  Ok,
  /// Report a warning but proceed: the suspicion is unconfirmed.
  Warn,
  /// Refuse: a live Steam client was inspected and genuinely lacks the path.
  Refuse,
}

impl Visibility {
  /// How a caller should treat this verdict.
  ///
  /// Only a proven absence refuses. An unproven suspicion warns, because
  /// reporting it as a failure would be a claim the evidence does not support.
  pub fn severity(&self) -> Severity {
    match self {
      Self::Reachable | Self::Unknown { suspect: false } => Severity::Ok,
      Self::Unknown { suspect: true } => Severity::Warn,
      Self::Unreachable { .. } => Severity::Refuse,
    }
  }
}

/// Whether `path` starts with a `SUSPECT_PREFIXES` entry.
///
/// Compared component-wise rather than as a string prefix, so `/tmpfoo` is not
/// mistaken for something under `/tmp`.
pub fn suspect_prefix(path: &Path) -> bool {
  SUSPECT_PREFIXES
    .iter()
    .any(|prefix| starts_with_components(path, Path::new(prefix)))
}

/// Whether every component of `prefix`, in order, opens `path`.
fn starts_with_components(path: &Path, prefix: &Path) -> bool {
  let mut components = path.components();

  for expected in prefix.components() {
    match components.next() {
      Some(actual) if actual == expected => {}
      _ => return false,
    }
  }

  true
}

/// Whether `path` resolves inside the filesystem rooted at `namespace_root`.
///
/// `namespace_root` is `/proc/<pid>/root` in real use, which the kernel resolves
/// against that process's root, including absolute symlinks encountered along the
/// way, so this genuinely answers "can that process open this path", not merely
/// "does a similarly named file exist here". Taking the root as a parameter is also
/// what makes the resolution testable against an ordinary directory tree.
///
/// `path`'s leading `/` is stripped before joining, since [`Path::join`] with an
/// absolute path discards the root it is joined onto.
fn visible_in(namespace_root: &Path, path: &Path) -> bool {
  let relative = path.strip_prefix("/").unwrap_or(path);

  namespace_root.join(relative).exists()
}

/// The running Steam client's pid, as Steam itself records it.
///
/// Read from `~/.steam/steam.pid`, which the client writes on startup, and
/// accepted only if that pid currently belongs to a process named `steam`. A pid
/// file outlives the process that wrote it, and a recycled pid would otherwise
/// have us inspecting an unrelated namespace and reporting nonsense about it.
pub fn steam_client_pid() -> Option<u32> {
  let home = std::env::var_os("HOME")?;
  let pid_file = PathBuf::from(home).join(".steam").join("steam.pid");
  let pid: u32 = std::fs::read_to_string(pid_file)
    .ok()?
    .trim()
    .parse()
    .ok()?;

  let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;

  (comm.trim() == "steam").then_some(pid)
}

/// Whether the running Steam client can resolve `path`.
///
/// Returns [`Visibility::Unknown`] whenever no Steam client can be inspected, be
/// that Steam not running, a platform without `/proc`, or an unreadable pid file,
/// because an unproven suspicion must not be reported as a proven failure.
pub fn visibility(path: &Path) -> Visibility {
  let Some(steam_pid) = steam_client_pid() else {
    return Visibility::Unknown {
      suspect: suspect_prefix(path),
    };
  };

  let namespace_root = PathBuf::from(format!("/proc/{steam_pid}/root"));

  // An unreadable namespace root proves nothing either way; only a readable one
  // that lacks the path is evidence.
  if !namespace_root.is_dir() {
    return Visibility::Unknown {
      suspect: suspect_prefix(path),
    };
  }

  if visible_in(&namespace_root, path) {
    return Visibility::Reachable;
  }

  Visibility::Unreachable { steam_pid }
}

/// The registry key Steam's Windows installer writes its own location into.
const STEAM_REGISTRY_KEY: &str = r"Software\Valve\Steam";

/// The value under [`STEAM_REGISTRY_KEY`] holding the full path to `steam.exe`.
const STEAM_REGISTRY_VALUE: &str = "SteamExe";

/// Environment variables naming the roots Steam installs under, in the order its
/// installer prefers: a 32-bit application on 64-bit Windows lands in
/// `Program Files (x86)`, and Steam is one.
const PROGRAM_FILES_VARS: [&str; 2] = ["ProgramFiles(x86)", "ProgramFiles"];

/// The directory Steam creates under a Program Files root.
const STEAM_INSTALL_DIR: &str = "Steam";

/// The Steam client executable's file name on Windows.
const STEAM_EXE_NAME: &str = "steam.exe";

/// The program name used when no executable can be located.
///
/// Correct on Linux and macOS, where the client is on `PATH`, and no worse than a
/// bare `steam` on a Windows host that has had Steam put on `PATH` by hand.
const STEAM_FALLBACK: &str = "steam";

/// `steam.exe` under each root's `Steam` directory, in the order given.
///
/// Pure: proposes paths without touching the filesystem, so the preference order
/// can be asserted on any host.
fn steam_exe_candidates(roots: &[PathBuf]) -> Vec<PathBuf> {
  roots
    .iter()
    .map(|root| root.join(STEAM_INSTALL_DIR).join(STEAM_EXE_NAME))
    .collect()
}

/// The first candidate that is a file, or `None` when none is.
///
/// Split out from where the candidates come from so the "which one wins"
/// decision is testable against an ordinary directory tree, with no Windows and
/// no Steam.
fn first_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
  candidates
    .iter()
    .find(|candidate| candidate.is_file())
    .cloned()
}

/// The Program Files roots to search, read from the environment.
///
/// Guarded at run time rather than with `#[cfg(windows)]` so the resolution path
/// stays compiled and unit-tested on a Linux CI, which is the only place this can
/// be tested at all. The guard is load-bearing beyond that: Wine and some CI
/// images export `ProgramFiles`, and honouring it off Windows would divert a
/// working launch to a path that cannot be executed.
fn install_roots() -> Vec<PathBuf> {
  if !cfg!(windows) {
    return Vec::new();
  }

  PROGRAM_FILES_VARS
    .iter()
    .filter_map(std::env::var_os)
    .map(PathBuf::from)
    .collect()
}

/// The executable path Steam's installer recorded in the registry.
///
/// Preferred over the standard roots because it is the only source that survives
/// a Steam installed to another drive. Steam writes this value with forward
/// slashes, which Windows accepts.
#[cfg(windows)]
fn registry_steam_exe() -> Option<PathBuf> {
  let key = windows_registry::CURRENT_USER
    .open(STEAM_REGISTRY_KEY)
    .ok()?;
  let recorded = key.get_string(STEAM_REGISTRY_VALUE).ok()?;

  (!recorded.trim().is_empty()).then(|| PathBuf::from(recorded))
}

/// No registry to read. See the Windows implementation above.
#[cfg(not(windows))]
fn registry_steam_exe() -> Option<PathBuf> {
  // Referenced so the names stay live off Windows and cannot rot unnoticed.
  let _ = (STEAM_REGISTRY_KEY, STEAM_REGISTRY_VALUE);

  None
}

/// Chooses the Steam program from host facts already gathered.
///
/// The entire precedence rule, taking the registry value and the install roots as
/// parameters rather than reading them, so every branch — registry hit, stale
/// registry, roots order, nothing found at all — is exercised on a Linux CI. That
/// matters more here than usual: the bug this resolution exists to fix only
/// reproduces on Windows, which cannot be tested directly.
///
/// The registry path is re-checked against the filesystem rather than trusted, so
/// a stale entry left behind by a moved or uninstalled Steam falls through to the
/// standard roots instead of yielding a path that cannot be spawned.
fn resolve_steam_exe(registry: Option<PathBuf>, roots: &[PathBuf]) -> PathBuf {
  registry
    .filter(|path| path.is_file())
    .or_else(|| first_existing(&steam_exe_candidates(roots)))
    .unwrap_or_else(|| PathBuf::from(STEAM_FALLBACK))
}

/// The program that starts the Steam client on this machine.
///
/// Tries the registry, then the standard install roots, then falls back to a bare
/// `STEAM_FALLBACK`. Resolution never fails: that fallback is exactly what this
/// crate passed unconditionally before, so a machine this cannot resolve is left
/// no worse off than it was.
///
/// [`crate::profile::launch::LaunchPlan::to_command`] calls this for
/// [`crate::profile::launch::LaunchProgram::Steam`], so a caller spawning a
/// computed plan gets it without asking. It is public for callers assembling a
/// command themselves.
pub fn steam_exe() -> PathBuf {
  resolve_steam_exe(registry_steam_exe(), &install_roots())
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::tempdir;

  #[test]
  fn suspect_prefix_matches_whole_components_only() {
    assert!(suspect_prefix(Path::new(
      "/tmp/t/d/valheim/steam-wrapper.sh"
    )));
    assert!(suspect_prefix(Path::new("/var/tmp/x")));
    assert!(suspect_prefix(Path::new("/dev/shm/x")));
    assert!(suspect_prefix(Path::new("/run/user/1000/x")));

    assert!(!suspect_prefix(Path::new("/home/u/.local/share/vmm")));
    assert!(!suspect_prefix(Path::new("/mnt/games/vmm")));

    // A string-prefix check would wrongly flag these: neither is under /tmp.
    assert!(!suspect_prefix(Path::new("/tmpfoo/x")));
    assert!(!suspect_prefix(Path::new("/var/tmpfoo/x")));

    // `/tmp` itself, and a relative path, are handled without panicking.
    assert!(suspect_prefix(Path::new("/tmp")));
    assert!(!suspect_prefix(Path::new("relative/path")));
  }

  #[test]
  fn visible_in_resolves_an_absolute_path_under_the_namespace_root() {
    let namespace = tempdir().unwrap();
    let root = namespace.path();

    std::fs::create_dir_all(root.join("home/u/.local/share/vmm/valheim")).unwrap();
    std::fs::write(
      root.join("home/u/.local/share/vmm/valheim/steam-wrapper.sh"),
      b"#!/bin/sh\n",
    )
    .unwrap();

    assert!(visible_in(
      root,
      Path::new("/home/u/.local/share/vmm/valheim/steam-wrapper.sh")
    ));

    // The namespace has no `/tmp` at all, exactly like a sandboxed Steam's.
    assert!(!visible_in(
      root,
      Path::new("/tmp/t/d/valheim/steam-wrapper.sh")
    ));

    // A directory counts as visible: the wrapper's own check on the target is
    // `[ -d ]`, not `[ -f ]`.
    assert!(visible_in(root, Path::new("/home/u/.local/share/vmm")));
  }

  /// The leading `/` has to be stripped: `Path::join` with an absolute path
  /// discards the root it is joined onto, which would silently check the *host*
  /// filesystem and call every path reachable.
  #[test]
  fn visible_in_does_not_fall_through_to_the_host_filesystem() {
    let namespace = tempdir().unwrap();
    let real = tempdir().unwrap();
    let outside = real.path().join("wrapper.sh");

    std::fs::write(&outside, b"#!/bin/sh\n").unwrap();

    assert!(outside.exists(), "the file must exist on the host");
    assert!(
      !visible_in(namespace.path(), &outside),
      "a host path must not be reported visible in a namespace that lacks it"
    );
  }

  /// Whatever this machine's Steam state, a verdict is always produced and never
  /// panics, and a home-directory path is never reported as proven unreachable.
  #[test]
  fn visibility_returns_a_verdict_without_panicking() {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home".to_string());
    let verdict = visibility(Path::new(&home));

    assert!(
      !matches!(verdict, Visibility::Unreachable { .. }),
      "a home path should never be proven unreachable, got: {verdict:?}"
    );
  }

  #[test]
  fn steam_exe_candidates_names_steam_exe_under_each_root_in_order() {
    let roots = vec![
      PathBuf::from(r"C:\Program Files (x86)"),
      PathBuf::from(r"C:\Program Files"),
    ];

    assert_eq!(
      steam_exe_candidates(&roots),
      vec![
        PathBuf::from(r"C:\Program Files (x86)")
          .join("Steam")
          .join("steam.exe"),
        PathBuf::from(r"C:\Program Files")
          .join("Steam")
          .join("steam.exe"),
      ]
    );

    // No roots to search is not an error; it simply proposes nothing, which is
    // what the caller sees on every non-Windows host.
    assert!(steam_exe_candidates(&[]).is_empty());
  }

  #[test]
  fn first_existing_returns_the_earliest_candidate_present_on_disk() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let absent = root.join("absent.exe");
    let present = root.join("present.exe");
    let also_present = root.join("also_present.exe");

    std::fs::write(&present, b"MZ").unwrap();
    std::fs::write(&also_present, b"MZ").unwrap();

    // Preference order decides, not disk order: the first *present* candidate
    // wins even though an earlier one was looked for and missing.
    assert_eq!(
      first_existing(&[absent.clone(), present.clone(), also_present]),
      Some(present)
    );

    assert_eq!(first_existing(&[absent]), None);

    // A directory is not an executable, so it must not satisfy a candidate.
    assert_eq!(first_existing(&[root.to_path_buf()]), None);
  }

  /// The precedence the Windows fix turns on, asserted end to end on a host that
  /// has neither a registry nor a Steam. Each branch is reached by varying only
  /// the two inputs.
  #[test]
  fn resolve_steam_exe_prefers_the_registry_then_the_roots_then_the_fallback() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    // A Steam relocated to another drive: only the registry knows about it, and
    // it must win even when a standard root would also have resolved.
    let relocated = root.join("D_drive").join("Steam").join("steam.exe");
    let standard = root.join("ProgramFilesX86");
    let secondary = root.join("ProgramFiles");

    std::fs::create_dir_all(relocated.parent().unwrap()).unwrap();
    std::fs::create_dir_all(standard.join("Steam")).unwrap();
    std::fs::create_dir_all(secondary.join("Steam")).unwrap();
    std::fs::write(&relocated, b"MZ").unwrap();
    std::fs::write(standard.join("Steam").join("steam.exe"), b"MZ").unwrap();
    std::fs::write(secondary.join("Steam").join("steam.exe"), b"MZ").unwrap();

    let roots = vec![standard.clone(), secondary.clone()];

    assert_eq!(
      resolve_steam_exe(Some(relocated.clone()), &roots),
      relocated,
      "a registry-recorded Steam must win over a standard root"
    );

    assert_eq!(
      resolve_steam_exe(None, &roots),
      standard.join("Steam").join("steam.exe"),
      "with no registry value the first root wins"
    );

    // A registry entry left behind by a moved or uninstalled Steam must not be
    // handed back: spawning it would fail where the standard root would work.
    assert_eq!(
      resolve_steam_exe(Some(root.join("gone").join("steam.exe")), &roots),
      standard.join("Steam").join("steam.exe"),
      "a stale registry entry must fall through to the roots"
    );

    assert_eq!(
      resolve_steam_exe(None, std::slice::from_ref(&secondary)),
      secondary.join("Steam").join("steam.exe"),
      "the second root resolves when it is the only one offered"
    );

    // Nothing resolvable is not an error. The bare name is what this crate always
    // passed, and it still works wherever Steam is on PATH.
    assert_eq!(
      resolve_steam_exe(None, &[root.join("empty")]),
      PathBuf::from("steam")
    );
    assert_eq!(resolve_steam_exe(None, &[]), PathBuf::from("steam"));
  }

  /// The whole resolution path stays compiled and exercised off Windows, so the
  /// runtime guard is what keeps it inert there. `install_roots` is the only
  /// route the environment takes into resolution, so a host that happens to
  /// export `ProgramFiles` — Wine and some CI images do — cannot divert a launch
  /// as long as this stays empty.
  ///
  /// Asserted by calling the shim rather than by setting the variable, because
  /// the crate is `#![forbid(unsafe_code)]` and `std::env::set_var` is `unsafe`.
  #[test]
  fn resolution_is_inert_off_windows() {
    if cfg!(windows) {
      return;
    }

    assert!(
      install_roots().is_empty(),
      "no install roots may be proposed off Windows"
    );
    assert_eq!(
      registry_steam_exe(),
      None,
      "there is no registry to read off Windows"
    );
    assert_eq!(
      steam_exe(),
      PathBuf::from("steam"),
      "off Windows the bare program name is correct and must be preserved"
    );
  }

  #[test]
  fn only_a_proven_absence_refuses() {
    assert_eq!(Visibility::Reachable.severity(), Severity::Ok);
    assert_eq!(
      Visibility::Unknown { suspect: false }.severity(),
      Severity::Ok
    );
    assert_eq!(
      Visibility::Unknown { suspect: true }.severity(),
      Severity::Warn
    );
    assert_eq!(
      Visibility::Unreachable { steam_pid: 1234 }.severity(),
      Severity::Refuse
    );
  }
}
