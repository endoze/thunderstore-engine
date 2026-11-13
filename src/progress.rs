//! UI-agnostic progress reporting.
//!
//! The engine performs long-running network and IO work (manifest downloads,
//! concurrent package downloads) but must not depend on any particular UI
//! toolkit. Callers provide a [`ProgressReporter`]; a CLI can wire it to
//! `indicatif`, a GUI can wire it to reactive signals, and tests can use the
//! default [`NoopProgress`].
//!
//! A [`ProgressReporter`] belongs to a client and sees every download it makes.
//! An [`Operation`] belongs to one call: it receives a [`ProgressEvent`] for
//! each package and [`Stage`] of that call, and carries the [`CancelToken`]
//! the caller uses to stop it. Neither type needs the `io` feature, so a
//! client that only stores events can use them without it.

#![deny(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Identifies one caller-started operation in the events it emits.
///
/// The engine never invents one: the caller picks it, so events from several
/// operations can share one channel and still be told apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct OperationId(pub u64);

/// The stage of work a [`ProgressEvent`] describes for one package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Stage {
  /// Fetching the package archive. Units are bytes.
  Downloading,
  /// Unpacking the archive into the package cache. Units are archive entries.
  Extracting,
  /// Copying the package's files into the target. Units are files.
  Installing,
  /// Removing the package's files and record from the target. Has no units:
  /// it starts with an unknown total and finishes.
  Uninstalling,
  /// Restoring a disabled package's files. Has no units.
  Enabling,
  /// Setting a package's files aside so the game skips them. Has no units.
  Disabling,
}

/// What happened to a [`Stage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StageUpdate {
  /// The stage began.
  Started {
    /// Expected units, or `None` when unknown (render as a spinner).
    total: Option<u64>,
  },
  /// Part of the stage is complete.
  Advanced {
    /// Units complete so far.
    done: u64,
    /// Expected units, or `None` when unknown.
    total: Option<u64>,
  },
  /// The stage ended, whether or not it succeeded. Sent exactly once per
  /// `Started`; a failure is reported in the operation's result.
  Finished,
}

/// One progress report from an [`Operation`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProgressEvent {
  /// The operation this event belongs to.
  pub operation: OperationId,
  /// The package, as `Owner-Name`.
  pub package: String,
  /// The stage of work on `package`.
  pub stage: Stage,
  /// What happened to the stage.
  pub update: StageUpdate,
}

/// Receives the [`ProgressEvent`]s of one [`Operation`].
///
/// Called from whichever thread is doing the work, including blocking-pool
/// threads, so it should be cheap and should not block: forward to a channel.
/// Any `Fn(ProgressEvent) + Send + Sync` closure is a sink.
pub trait ProgressSink: Send + Sync {
  /// Handles one event.
  fn report(&self, event: ProgressEvent);
}

impl<F> ProgressSink for F
where
  F: Fn(ProgressEvent) + Send + Sync,
{
  fn report(&self, event: ProgressEvent) {
    self(event)
  }
}

/// A shared flag a caller sets to stop an [`Operation`].
///
/// Clones share the flag. Cancelling is a request: a batch checks it between
/// packages, so the package in progress always finishes.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
  /// A token that has not been cancelled.
  pub fn new() -> Self {
    Self::default()
  }

  /// Requests cancellation. Calling it again does nothing.
  pub fn cancel(&self) {
    self.0.store(true, Ordering::SeqCst);
  }

  /// Whether cancellation was requested.
  pub fn is_cancelled(&self) -> bool {
    self.0.load(Ordering::SeqCst)
  }
}

/// The caller's handle on one engine call: where its progress goes and how to
/// cancel it.
///
/// Clones share the sink and the cancel flag. [`Operation::default`] has id
/// `0`, discards progress, and has a fresh token.
///
/// ```
/// use std::sync::mpsc;
/// use thunderstore_engine::progress::{Operation, OperationId};
///
/// let (tx, rx) = mpsc::channel();
/// let operation = Operation::new(OperationId(1)).with_progress(move |event| {
///   let _ = tx.send(event);
/// });
/// let cancel = operation.cancel_token();
///
/// cancel.cancel();
///
/// assert!(operation.is_cancelled());
/// assert!(rx.try_recv().is_err());
/// ```
#[derive(Clone, Default)]
pub struct Operation {
  id: OperationId,
  sink: Option<Arc<dyn ProgressSink>>,
  cancel: CancelToken,
}

impl std::fmt::Debug for Operation {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Operation")
      .field("id", &self.id)
      .field("reports_progress", &self.sink.is_some())
      .field("cancel", &self.cancel)
      .finish()
  }
}

impl Operation {
  /// An operation with id `id` that discards progress, with a fresh token.
  pub fn new(id: OperationId) -> Self {
    Self {
      id,
      ..Self::default()
    }
  }

  /// Sends this operation's progress to `sink`.
  pub fn with_progress(mut self, sink: impl ProgressSink + 'static) -> Self {
    self.sink = Some(Arc::new(sink));

    self
  }

  /// Cancels this operation through `token`, for a caller that made the token
  /// first.
  pub fn with_cancel(mut self, token: CancelToken) -> Self {
    self.cancel = token;

    self
  }

  /// The id every event from this operation carries.
  pub fn id(&self) -> OperationId {
    self.id
  }

  /// A clone of this operation's cancel token.
  pub fn cancel_token(&self) -> CancelToken {
    self.cancel.clone()
  }

  /// Whether cancellation was requested.
  pub fn is_cancelled(&self) -> bool {
    self.cancel.is_cancelled()
  }

  /// Starts reporting `stage` of `package`. The guard sends
  /// [`StageUpdate::Finished`] when dropped, so the stage ends exactly once on
  /// every path.
  #[cfg(feature = "io")]
  pub(crate) fn stage(&self, package: &str, stage: Stage, total: Option<u64>) -> StageProgress {
    let progress = StageProgress {
      operation: self.clone(),
      package: package.to_string(),
      stage,
      total,
    };

    progress.send(StageUpdate::Started { total });

    progress
  }

  #[cfg(feature = "io")]
  fn send(&self, package: &str, stage: Stage, update: StageUpdate) {
    if let Some(sink) = &self.sink {
      sink.report(ProgressEvent {
        operation: self.id,
        package: package.to_string(),
        stage,
        update,
      });
    }
  }
}

/// One running stage of an [`Operation`]. Sends `Finished` on drop.
#[cfg(feature = "io")]
pub(crate) struct StageProgress {
  operation: Operation,
  package: String,
  stage: Stage,
  total: Option<u64>,
}

#[cfg(feature = "io")]
impl StageProgress {
  /// Reports `done` units complete.
  pub(crate) fn advance(&self, done: u64) {
    self.send(StageUpdate::Advanced {
      done,
      total: self.total,
    });
  }

  /// Records the total once it becomes known.
  pub(crate) fn set_total(&mut self, total: u64) {
    self.total = Some(total);
  }

  fn send(&self, update: StageUpdate) {
    self.operation.send(&self.package, self.stage, update);
  }
}

#[cfg(feature = "io")]
impl Drop for StageProgress {
  fn drop(&mut self) {
    self.send(StageUpdate::Finished);
  }
}

/// Forwards a client's download handles to an [`Operation`] as well as to the
/// client's own reporter, naming each by the package its archive holds.
#[cfg(feature = "io")]
pub(crate) struct OperationDownloads {
  operation: Operation,
  inner: Arc<dyn ProgressReporter>,
}

#[cfg(feature = "io")]
impl OperationDownloads {
  pub(crate) fn new(operation: Operation, inner: Arc<dyn ProgressReporter>) -> Self {
    Self { operation, inner }
  }
}

#[cfg(feature = "io")]
impl ProgressReporter for OperationDownloads {
  fn begin(&self, label: &str, total: Option<u64>) -> Box<dyn ProgressHandle> {
    let package = crate::extract::parse_package_filename(label)
      .map(|(full_name, _)| full_name)
      .unwrap_or_else(|| label.to_string());

    Box::new(OperationDownload {
      inner: self.inner.begin(label, total),
      stage: std::sync::Mutex::new(Some(DownloadStage {
        progress: self.operation.stage(&package, Stage::Downloading, total),
        done: 0,
        sent: (0, total),
      })),
    })
  }
}

/// How far a download of unknown length moves between two `Advanced` events.
#[cfg(feature = "io")]
const UNSIZED_DOWNLOAD_STEP: u64 = 256 * 1024;

/// A download handle for [`OperationDownloads`]. The stage is dropped, which
/// sends `Finished`, by [`ProgressHandle::finish`].
///
/// A download streams in many small chunks, so `Advanced` is sent only when
/// the whole percentage changes, or every [`UNSIZED_DOWNLOAD_STEP`] bytes when
/// the length is unknown, and once more with the final position.
#[cfg(feature = "io")]
struct OperationDownload {
  inner: Box<dyn ProgressHandle>,
  stage: std::sync::Mutex<Option<DownloadStage>>,
}

#[cfg(feature = "io")]
struct DownloadStage {
  progress: StageProgress,
  done: u64,
  /// The `done` and total last sent as `Advanced`.
  sent: (u64, Option<u64>),
}

#[cfg(feature = "io")]
impl DownloadStage {
  fn send_if_moved(&mut self) {
    let (sent_done, sent_total) = self.sent;
    let total = self.progress.total;
    let moved = total != sent_total
      || self.done < sent_done
      || match total {
        Some(total) => percent(self.done, total) != percent(sent_done, total),
        None => self.done - sent_done >= UNSIZED_DOWNLOAD_STEP,
      };

    if moved {
      self.send();
    }
  }

  fn send(&mut self) {
    self.progress.advance(self.done);
    self.sent = (self.done, self.progress.total);
  }
}

#[cfg(feature = "io")]
fn percent(done: u64, total: u64) -> u128 {
  match total {
    0 => 100,
    total => u128::from(done) * 100 / u128::from(total),
  }
}

#[cfg(feature = "io")]
impl OperationDownload {
  fn update(&self, change: impl FnOnce(&mut DownloadStage)) {
    let mut guard = self
      .stage
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner);

    if let Some(stage) = guard.as_mut() {
      change(stage);
      stage.send_if_moved();
    }
  }
}

#[cfg(feature = "io")]
impl ProgressHandle for OperationDownload {
  fn inc(&self, delta: u64) {
    self.inner.inc(delta);
    self.update(|stage| stage.done += delta);
  }

  fn set_length(&self, total: u64) {
    self.inner.set_length(total);
    self.update(|stage| stage.progress.set_total(total));
  }

  fn set_position(&self, position: u64) {
    self.inner.set_position(position);
    self.update(|stage| stage.done = position);
  }

  fn set_message(&self, msg: &str) {
    self.inner.set_message(msg);
  }

  fn finish(&self, msg: Option<&str>) {
    self.inner.finish(msg);

    let stage = self
      .stage
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
      .take();

    if let Some(mut stage) = stage
      && stage.sent.0 != stage.done
    {
      stage.send();
    }
  }
}

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

#[cfg(all(test, feature = "io"))]
mod tests {
  use super::*;
  use std::sync::Mutex;

  /// Streams `chunks` of `chunk` bytes through one download handle, telling it
  /// the length up front when `total` is set, and returns every `Advanced`
  /// event's `done`.
  fn advanced_for(total: Option<u64>, chunks: u64, chunk: u64) -> Vec<u64> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let operation = Operation::new(OperationId(1))
      .with_progress(move |event: ProgressEvent| sink.lock().unwrap().push(event));
    let downloads = OperationDownloads::new(operation, Arc::new(NoopProgress));
    let handle = downloads.begin("Owner-ModA-1.0.0.zip", None);

    if let Some(total) = total {
      handle.set_length(total);
    }

    for _ in 0..chunks {
      handle.inc(chunk);
    }

    handle.finish(None);

    let events = events.lock().unwrap();

    events
      .iter()
      .filter_map(|event| match event.update {
        StageUpdate::Advanced { done, .. } => Some(done),
        _ => None,
      })
      .collect()
  }

  #[test]
  fn a_download_of_known_length_reports_each_percent_once() {
    let advanced = advanced_for(Some(10 * 1024 * 1024), 10 * 1024, 1024);

    assert_eq!(advanced.len(), 101, "the length, then 1% through 100%");
    assert_eq!(advanced.first(), Some(&0));
    assert_eq!(advanced.last(), Some(&(10 * 1024 * 1024)));
  }

  #[test]
  fn a_download_of_unknown_length_reports_every_step_and_the_end() {
    let advanced = advanced_for(None, 1000, 1024);

    assert_eq!(
      advanced,
      [256 * 1024, 512 * 1024, 768 * 1024, 1000 * 1024],
      "every 256 KiB, then the final position"
    );
  }
}
