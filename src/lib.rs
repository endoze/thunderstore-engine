//!
#![cfg_attr(feature = "io", doc = include_str!("../README.md"))]
#![cfg_attr(
  not(feature = "io"),
  doc = "Game-agnostic Thunderstore engine, built without the `io` feature: the data models, dependency resolver, game definitions, plan types, pure planners, and pure queries. See the crate README for the full feature list."
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]

#[cfg(all(
  feature = "io",
  not(any(feature = "rustls-tls", feature = "native-tls"))
))]
compile_error!(
  "thunderstore-engine requires exactly one TLS backend: enable rustls-tls (default) or native-tls"
);

#[cfg(all(feature = "rustls-tls", feature = "native-tls"))]
compile_error!(
  "thunderstore-engine: enable only one TLS backend, rustls-tls OR native-tls, not both"
);

/// Async Thunderstore API client and package-index cache.
#[cfg(feature = "io")]
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
/// Pure queries over the package index and a profile's installed packages.
pub mod query;
/// Dependency-graph resolution.
pub mod resolve;
/// Bounded retry policy for transient network failures.
mod retry;
/// Whether the running Steam client can resolve a path.
pub mod steam;
/// Mock Thunderstore server, package index, and archives for tests.
#[cfg(feature = "test-support")]
pub mod test_support;
/// Small internal utilities shared across the engine.
pub(crate) mod util;

pub use error::{Error, Result};
pub use progress::{NoopProgress, ProgressHandle, ProgressReporter};
pub use retry::RetryPolicy;
