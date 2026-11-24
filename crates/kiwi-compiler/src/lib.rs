//! Kiwi compiler: reads Markdown from a Git commit (or a directory), splits it
//! into heading-bounded atoms with three zoom levels, assigns permission classes,
//! resolves the typed graph and writes one immutable Kiwi Pack.
//!
//! Builds are incremental: a compiled document is cached by the BLAKE3 of its bytes,
//! its resolved access and the configuration, so an unchanged corpus compiles no atom.

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod build;
pub mod compile;
pub mod config;
pub mod error;
pub mod link;
pub mod lint;
pub mod parse;
pub mod propose;
pub mod source;

pub use build::{BuildOptions, BuildPlugins, BuildReport, BuildSession, build};
pub use config::KiwiConfig;
pub use error::{CompileError, Result};
pub use kiwi_core::date;
pub use source::{Snapshot, SourceMode};
