//!
#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

#[cfg(not(any(feature = "rustls-tls", feature = "native-tls")))]
compile_error!(
  "thunderstore-engine requires exactly one TLS backend: enable rustls-tls (default) or native-tls"
);

#[cfg(all(feature = "rustls-tls", feature = "native-tls"))]
compile_error!(
  "thunderstore-engine: enable only one TLS backend, rustls-tls OR native-tls, not both"
);

/// Async Thunderstore API client and package-index cache.
pub mod client;
/// Ecosystem-schema: games, mod loaders, and install rules.
pub mod ecosystem;
/// Error types and the engine `Result` alias.
pub mod error;
/// CRC-verified zip extraction primitives.
pub mod extract;
/// Schema-driven, root-relative mod install routing.
pub mod install;
/// String interning utilities for memory-efficient string storage.
pub(crate) mod intern;
/// Per-mod `manifest.json` parsing.
pub mod manifest;
/// Package data model and the cache wire format.
pub mod models;
/// r2modman-compatible profile management.
pub mod profile;
/// UI-agnostic progress reporting.
pub mod progress;
/// Dependency-graph resolution.
pub mod resolve;
/// Bounded retry policy for transient network failures.
mod retry;
/// Whether the running Steam client can resolve a path.
pub mod steam;
/// Small internal utilities shared across the engine.
pub(crate) mod util;

pub use error::{Error, Result};
pub use progress::{NoopProgress, ProgressHandle, ProgressReporter};
pub use retry::RetryPolicy;
