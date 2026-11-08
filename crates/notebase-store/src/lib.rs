//! Storage: the durable half of the engine.
//!
//! Shape, borrowed from every serious local-first store rather than invented:
//!
//! * an **append-only log of CRDT updates** per note (the oplog),
//! * periodic **snapshots** that make the log truncatable,
//! * **content-addressed blobs** for anything that is not text,
//! * a **derived** full-text index and a **derived** plain-text projection,
//! * an append-only **edit log** of tiny rows (`ts`, `chars`) so usage history
//!   survives compaction — the body is what gets compressed away, never the
//!   fact that a human sat down and wrote.
//!
//! Nothing in here is authoritative: delete every derived table and the notes
//! still load.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations, rust_2018_idioms)]

mod codec;
mod sqlite;
mod vault;

pub use sqlite::{NoteMeta, SqliteStore, StoredSnapshot};
pub use vault::{
    ApplyStats, CompactStats, EditActivity, NoteSummary, Retention, SaveStats, SearchHit, Vault,
    VaultStats, SESSION_QUIET_MS,
};

pub use notebase;

/// Convenience alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("note: {0}")]
    Note(#[from] notebase::Error),
    #[error("note {0} not found")]
    NotFound(String),
    #[error("corrupt stored bytes: {0}")]
    Corrupt(String),
}
