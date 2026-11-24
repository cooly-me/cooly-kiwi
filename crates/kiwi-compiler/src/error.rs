//! Errors of the compiler.

use std::path::PathBuf;

/// Result alias for the compiler.
pub type Result<T, E = CompileError> = std::result::Result<T, E>;

/// Everything that can stop a build.
#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    /// Reading the corpus failed.
    #[error("{path}: {message}")]
    Source {
        /// The path concerned.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },
    /// Git could not be read or written.
    #[error("git: {0}")]
    Git(String),
    /// A document is malformed.
    #[error("{path}:{line}: {message}")]
    Document {
        /// Corpus-relative path.
        path: String,
        /// 1-based line.
        line: usize,
        /// What is wrong.
        message: String,
    },
    /// The configuration is invalid.
    #[error("config: {0}")]
    Config(String),
    /// An access declaration is invalid.
    #[error(transparent)]
    Access(#[from] kiwi_policy::AccessError),
    /// The core failed (pack writing, a plugin).
    #[error(transparent)]
    Core(#[from] kiwi_core::KiwiError),
}

impl CompileError {
    /// A source error.
    pub fn source(path: impl Into<PathBuf>, message: impl std::fmt::Display) -> Self {
        Self::Source {
            path: path.into(),
            message: message.to_string(),
        }
    }

    /// A git error.
    pub fn git(message: impl std::fmt::Display) -> Self {
        Self::Git(message.to_string())
    }
}
