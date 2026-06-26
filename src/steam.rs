//! Whether the running Steam client can resolve a path a caller hands it.
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
