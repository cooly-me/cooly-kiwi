//! Kiwi core: the knowledge model, the Kiwi Pack format, the query planner and
//! the plugin traits.
//!
//! A pack is compiled once per commit by `kiwi-compiler` and read here through a memory
//! map. The planner answers the read tools (`map`, `search`, `read`, `trace`) on the
//! [`engine::View`] the policy gate allows, so no tier ever sees a hidden atom.

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod api;
pub mod date;
pub mod engine;
pub mod error;
pub mod export;
pub mod model;
pub mod pack;
pub mod plugin;
pub mod rerank;
pub mod text;

pub use engine::{
    BudgetFit, DEFAULT_MAP_BUDGET, DEFAULT_READ_BUDGET, DEFAULT_SEARCH_BUDGET, DEFAULT_SEARCH_LIMIT, Engine,
    EngineConfig, HiddenMode, MAX_BUDGET, MAX_QUERIES, MAX_READ_IDS, MAX_SEARCH_LIMIT, MIN_BUDGET, QueryMask,
    RankedAtom, Ranking, ReadBudget, ReadCut, ReadStep, View, check_read_ids, cover_queries, fit_budget,
};
pub use error::{KiwiError, Result};
pub use pack::{DEFAULT_KEEP_PACKS, GcReport, Pack, PackBytes, PackStore, StoreLock, StoredPack};
