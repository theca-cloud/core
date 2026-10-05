#![warn(missing_docs)]
//! Shared types and algorithms: chunking, hashing, compression, name rules.
//!
//! Everything here must behave identically on the server, in native clients and
//! in the browser (WASM), so this crate does no I/O of its own.

pub mod api;
pub mod chunking;
pub mod compression;
pub mod hash;
pub mod name;
