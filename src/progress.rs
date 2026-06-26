//! UI-agnostic progress reporting.
//!
//! The engine performs long-running network and IO work (manifest downloads,
//! concurrent package downloads) but must not depend on any particular UI
//! toolkit. Callers provide a [`ProgressReporter`]; a CLI can wire it to
//! `indicatif`, a GUI can wire it to reactive signals, and tests can use the
//! default [`NoopProgress`].

#![deny(missing_docs)]

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
///
/// The engine calls [`finish`](ProgressHandle::finish) exactly once when the work
/// ends — whether it succeeded or failed — so an implementation can rely on
/// `finish` to clear or finalize its display and should not leave a bar dangling
/// if it is never called again.
///
/// Handles are `Send + Sync`: a single handle is shared by reference across the
/// retried attempts of one download, and that download future must stay `Send` so
/// callers can drive it from a multi-threaded runtime (e.g. `tokio::spawn`).
pub trait ProgressHandle: Send + Sync {
  /// Advances progress by `delta` units.
  fn inc(&self, delta: u64);

  /// Sets (or updates) the total number of units when it only becomes known
  /// after the handle was created — e.g. a download's `Content-Length` arriving
  /// with the response. Implementations that cannot re-total may ignore this;
  /// the default does nothing.
  fn set_length(&self, _total: u64) {}

  /// Resets (overrides) the absolute progress position to `position` units.
  ///
  /// Unlike [`inc`](ProgressHandle::inc), which advances relative to the current
  /// position, this sets it directly — used to rewind to zero when a download is
  /// retried so re-downloaded bytes are not double-counted and the bar cannot
  /// overshoot its total. Implementations that cannot re-position may ignore this;
  /// the default does nothing.
  fn set_position(&self, _position: u64) {}

  /// Updates the displayed message for this unit of work.
  fn set_message(&self, msg: &str);

  /// Marks this unit of work complete, optionally with a final message. Called
  /// exactly once per handle, on both the success and failure paths.
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
