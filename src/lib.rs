//! Sylva: graphs and trees that remember.
//!
//! - [`graph::Graph`] and [`tree::Tree`] are general-purpose structures with
//!   stable keys, dense storage and indexes, iterating in key order so every
//!   replay of the same changes gives the same result.
//! - [`forest::Forest`] is a tree for hot paths, such as a UI's retained tree
//!   walked every frame: generational ids, intrusive links, a flat preorder
//!   index and per-node data in columns.
//! - [`log`] (feature `log`) is the layer that makes state remember: changes
//!   are ops in batches, state is the replay of the log, batches order
//!   causally so branches merge, an op that no longer applies is kept as a
//!   conflict, and any past step can be rebuilt. Batches can be signed
//!   (`sign`) and logs kept as files or as a hash chain in SQLite (`sqlite`).
//! - [`knowledge`] (feature `knowledge`) is a typed property graph and a
//!   catalog tree built on the two: schemas, facts that record whether they
//!   were stated or observed, references to entities elsewhere, queries and
//!   diagrams.

//!
//! Without the default `std` feature the structures and the forest need only
//! `alloc`, so they build for no_std and wasm targets.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod error;
pub mod forest;
pub mod graph;
pub mod tree;

#[cfg(feature = "log")]
pub mod log;

#[cfg(feature = "knowledge")]
pub mod knowledge;

pub use error::{Error, Result};

/// A key for nodes and edges: small, copyable and totally ordered (iteration follows it).
pub trait Key: Copy + Ord + core::hash::Hash + core::fmt::Debug + core::fmt::Display {}

impl<T: Copy + Ord + core::hash::Hash + core::fmt::Debug + core::fmt::Display> Key for T {}
