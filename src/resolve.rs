//! Dependency-graph resolution.
//!
//! The resolver operates purely over the [`crate::models`] types and is
//! game-agnostic. See [`DependencyGraph`](crate::models::DependencyGraph) for the algorithm.
//!
//! Resolution is version-agnostic: dependency version suffixes are ignored and
//! every package resolves to its latest available version. See
//! [`DependencyGraph::resolve`](crate::models::DependencyGraph::resolve) for the
//! full limitation.

#![deny(missing_docs)]

pub use crate::models::{DependencyGraph, Resolution};
