//! Errors of the core crate.

use std::path::PathBuf;

/// Result alias for the core crate.
pub type Result<T, E = KiwiError> = std::result::Result<T, E>;

/// Everything that can fail in the core: I/O, a malformed pack, bad input.
#[derive(Debug, thiserror::Error)]
pub enum KiwiError {
    /// An I/O error with the path it concerns.
    #[error("{path}: {source}")]
    Io {
        /// The file or directory involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not a Kiwi Pack, or its format version is not supported.
    #[error("not a supported Kiwi Pack: {0}")]
    PackFormat(String),
    /// The pack failed validation or holds inconsistent data.
    #[error("corrupt pack: {0}")]
    Corrupt(String),
    /// The caller passed invalid input.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// A plugin failed.
    #[error("plugin {plugin}: {message}")]
    Plugin {
        /// Plugin name.
        plugin: String,
        /// What went wrong.
        message: String,
    },
}

impl KiwiError {
    /// Wraps an I/O error with its path.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// A plugin error.
    pub fn plugin(plugin: impl Into<String>, message: impl std::fmt::Display) -> Self {
        Self::Plugin {
            plugin: plugin.into(),
            message: message.to_string(),
        }
    }
}
