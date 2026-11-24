//! Kiwi governance: access declarations (`KIWIOWNERS`, frontmatter, heading
//! attributes), the Cedar policy gate that turns a principal into permitted classes,
//! policy unit tests, the tamper-evident audit log and its sinks.

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod access;
pub mod audit;
pub mod directory;
pub mod gate;
pub mod policytest;
pub mod sinks;

pub use access::{AccessError, FrontmatterAccess, KiwiOwners, PathDefaults};
pub use audit::{AuditError, AuditEvent, AuditLog, AuditStatus};
pub use directory::{DefaultAgent, Directory, DirectoryError, Identity, UNIDENTIFIED_AGENT};
pub use gate::{Action, PolicyError, PolicyGate};
