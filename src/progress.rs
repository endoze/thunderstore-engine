//! UI-agnostic progress reporting.
//!
//! The engine performs long-running network and IO work (manifest downloads,
//! concurrent package downloads) but must not depend on any particular UI
//! toolkit. Callers provide a [`ProgressReporter`]; a CLI can wire it to
//! `indicatif`, a GUI can wire it to reactive signals, and tests can use the
//! default [`NoopProgress`].

/// Factory for per-unit-of-work progress handles.
///
/// Implementations are shared across concurrent tasks, so this trait requires
/// `Send + Sync`.
pub trait ProgressReporter: Send + Sync {
  /// Begins reporting a new unit of work.
  ///
  /// `total` is the expected number of units (e.g. bytes); `None` indicates an
  /// indeterminate amount of work (render as a spinner).
  fn begin(&self, label: &str, total: Option<u64>) -> Box<dyn ProgressHandle>;
}

/// A handle to a single in-flight unit of work.
pub trait ProgressHandle: Send {
  /// Advances progress by `delta` units.
  fn inc(&self, delta: u64);

  /// Updates the displayed message for this unit of work.
  fn set_message(&self, msg: &str);

  /// Marks this unit of work complete, optionally with a final message.
  fn finish(&self, msg: Option<&str>);
}

/// A [`ProgressReporter`] that discards all progress updates.
///
/// This is the default used by [`crate::client::ThunderstoreClientBuilder`] when
/// no reporter is supplied.
pub struct NoopProgress;

impl ProgressReporter for NoopProgress {
  fn begin(&self, _label: &str, _total: Option<u64>) -> Box<dyn ProgressHandle> {
    Box::new(NoopHandle)
  }
}

/// A no-op [`ProgressHandle`] returned by [`NoopProgress`].
struct NoopHandle;

impl ProgressHandle for NoopHandle {
  fn inc(&self, _delta: u64) {}
  fn set_message(&self, _msg: &str) {}
  fn finish(&self, _msg: Option<&str>) {}
}
