//! Bounded retry with exponential backoff for transient HTTP failures.
//!
//! Retries are applied only to idempotent network operations that failed for a
//! *transient* reason (connection/timeout/body errors, `5xx`, or `429`) — never
//! to a deterministic failure such as a `4xx` response or one of the crate's own
//! terminal errors (a size-cap breach, a checksum mismatch, an unsafe filename).
//! The whole download path is already retry-safe: archives stream to a temp file
//! that is only renamed into place on success, so a retried attempt restarts
//! cleanly.

#![deny(missing_docs)]

use crate::error::{Error, Result};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Policy for retrying transient network failures.
///
/// The delay before the retry following a (0-based) attempt is
/// `base_delay * 2^attempt`, capped at `max_delay`, plus a little jitter.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
  /// Maximum number of retries *after* the initial attempt (so total attempts is
  /// `max_retries + 1`). Zero disables retrying.
  pub max_retries: u32,
  /// The base backoff delay, doubled on each successive retry.
  pub base_delay: Duration,
  /// The ceiling on any single backoff delay.
  pub max_delay: Duration,
}

impl Default for RetryPolicy {
  fn default() -> Self {
    Self {
      max_retries: 3,
      base_delay: Duration::from_millis(250),
      max_delay: Duration::from_secs(10),
    }
  }
}

impl RetryPolicy {
  /// A policy that performs no retries (a single attempt).
  pub fn none() -> Self {
    Self {
      max_retries: 0,
      ..Self::default()
    }
  }

  /// The backoff delay before the retry following `attempt` (0-based).
  fn backoff(&self, attempt: u32) -> Duration {
    let factor = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let capped = self.base_delay.saturating_mul(factor).min(self.max_delay);
    let jitter_ceiling_ms = (capped.as_millis() / 4) as u64;

    if jitter_ceiling_ms == 0 {
      return capped;
    }

    capped + Duration::from_millis(jitter_source() % (jitter_ceiling_ms + 1))
  }
}

/// A cheap, dependency-free varying value used only to desynchronize concurrent
/// retries. Not cryptographic and not for any security purpose.
fn jitter_source() -> u64 {
  static COUNTER: AtomicU64 = AtomicU64::new(0);

  COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
}

/// Runs `op`, retrying per `policy` while `is_retryable` accepts the error.
///
/// Returns the first `Ok`, or the last `Err` once retries are exhausted or the
/// error is non-retryable.
///
/// `op` is a plain `FnMut` returning a future (call sites pass `|| async { .. }`)
/// rather than an `AsyncFnMut`. This is deliberate: an async closure's returned
/// future carries a higher-ranked lifetime that the compiler cannot prove `Send`
/// "for any lifetime", which would make every retried operation — and therefore
/// the public `get_manifest`/`download_files` futures — `!Send`. A closure
/// returning a single concrete future type has no such higher-ranked lifetime, so
/// the futures stay `Send` and can be driven from a multi-threaded runtime.
pub(crate) async fn retry_with<T, F>(
  policy: &RetryPolicy,
  is_retryable: impl Fn(&Error) -> bool,
  mut op: impl FnMut() -> F,
) -> Result<T>
where
  F: Future<Output = Result<T>>,
{
  let mut attempt = 0u32;

  loop {
    match op().await {
      Ok(value) => return Ok(value),
      Err(err) => {
        if attempt >= policy.max_retries || !is_retryable(&err) {
          return Err(err);
        }

        let delay = policy.backoff(attempt);

        tracing::warn!(
          "transient network error ({}); retry {}/{}",
          err,
          attempt + 1,
          policy.max_retries
        );

        if !delay.is_zero() {
          tokio::time::sleep(delay).await;
        }

        attempt += 1;
      }
    }
  }
}

/// [`retry_with`] using [`is_retryable_http`] as the classifier.
pub(crate) async fn retry_http<T, F>(policy: &RetryPolicy, op: impl FnMut() -> F) -> Result<T>
where
  F: Future<Output = Result<T>>,
{
  retry_with(policy, is_retryable_http, op).await
}

/// Whether an error is a transient HTTP failure worth retrying: a connection,
/// timeout, request, or body error, or a `5xx`/`429` status. A `4xx` (other than
/// `429`) and every non-HTTP error are treated as terminal.
pub(crate) fn is_retryable_http(err: &Error) -> bool {
  match err {
    Error::Http(e) => {
      if let Some(status) = e.status() {
        status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
      } else {
        e.is_timeout() || e.is_connect() || e.is_request() || e.is_body()
      }
    }
    _ => false,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::cell::Cell;

  fn zero_delay(max_retries: u32) -> RetryPolicy {
    RetryPolicy {
      max_retries,
      base_delay: Duration::ZERO,
      max_delay: Duration::ZERO,
    }
  }

  #[tokio::test]
  async fn retries_until_success() {
    let calls = Cell::new(0u32);

    let result: Result<u32> = retry_with(
      &zero_delay(3),
      |_| true,
      || async {
        calls.set(calls.get() + 1);

        if calls.get() < 3 {
          Err(Error::Cache("transient".to_string()))
        } else {
          Ok(42)
        }
      },
    )
    .await;

    assert_eq!(result.unwrap(), 42);
    assert_eq!(calls.get(), 3);
  }

  #[tokio::test]
  async fn gives_up_after_max_retries() {
    let calls = Cell::new(0u32);

    let result: Result<u32> = retry_with(
      &zero_delay(2),
      |_| true,
      || async {
        calls.set(calls.get() + 1);

        Err(Error::Cache("always".to_string()))
      },
    )
    .await;

    assert!(result.is_err());
    assert_eq!(calls.get(), 3); // initial attempt + 2 retries
  }

  #[tokio::test]
  async fn does_not_retry_non_retryable() {
    let calls = Cell::new(0u32);

    let result: Result<u32> = retry_with(
      &zero_delay(5),
      |_| false,
      || async {
        calls.set(calls.get() + 1);

        Err(Error::Cache("terminal".to_string()))
      },
    )
    .await;

    assert!(result.is_err());
    assert_eq!(calls.get(), 1);
  }

  #[test]
  fn crate_terminal_errors_are_not_retryable() {
    assert!(!is_retryable_http(&Error::SizeLimitExceeded {
      context: "download body".to_string(),
      limit: 1,
      actual: None,
    }));
    assert!(!is_retryable_http(&Error::Cache("bad cache".to_string())));
    assert!(!is_retryable_http(&Error::ChecksumMismatch {
      path: "x".to_string(),
      expected: 1,
      actual: 2,
    }));
  }
}
