//! # notebase
//!
//! A note engine's core: one CRDT document per note, a block tree with rich
//! text, and markdown as a projection.
//!
//! ```no_run
//! use notebase::{BlockKind, NoteDoc};
//!
//! let note = NoteDoc::new();
//! note.set_title("Weekly review");
//! let intro = note.append_block(None, BlockKind::Paragraph, "What moved?").unwrap();
//! note.insert_block(Some(intro), 0, BlockKind::BulletedListItem, "shipped the sync engine").unwrap();
//! assert!(note.to_markdown().starts_with("# Weekly review"));
//! ```
//!
//! Layering: this crate is pure document semantics — no disk, no network, no
//! clock beyond `updated_at`. Storage lives in `notebase-store`, transport in
//! `notebase-sync`.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations, rust_2018_idioms)]

pub mod blocks;
pub mod doc;
pub mod markdown;
pub mod schema;

pub use blocks::{Block, BlockId, BlockKind, Mark, Marks, RichText, Span};
pub use doc::{Error, NoteDoc, Result};

/// Re-export so callers can work with version vectors without adding a
/// dependency on the CRDT crate themselves.
pub use loro::{Frontiers, VersionVector};
