//! # thunderstore-engine
//!
//! A game-agnostic engine for building Thunderstore mod managers. It provides:
//!
//! - [`client`]: an async Thunderstore API client with a `zstd` + `bincode`
//!   package-index cache.
//! - [`models`]: the package/version data model (struct-of-arrays + string
//!   interning) and the on-disk cache wire format.
//! - [`resolve`]: dependency-graph resolution over the index.
//! - [`manifest`]: parsing of a package's `manifest.json`.
//! - [`extract`]: CRC-verified zip extraction primitives.
//! - [`ecosystem`]: the Thunderstore ecosystem-schema (games + mod loaders).
//! - [`install`]: schema-driven, root-relative mod install routing.
//!
//! The engine is UI-agnostic: long-running operations report progress through
//! the [`progress`] traits rather than depending on a UI toolkit.

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
pub mod intern;
/// Per-mod `manifest.json` parsing.
pub mod manifest;
/// Package data model and the cache wire format.
pub mod models;
/// UI-agnostic progress reporting.
pub mod progress;
/// Dependency-graph resolution.
pub mod resolve;

pub use error::{Error, Result};
pub use progress::{NoopProgress, ProgressHandle, ProgressReporter};
