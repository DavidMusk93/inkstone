//! The persisted container layout of a note document.
//!
//! Container names are part of the on-disk format: changing one is a format
//! change and must bump [`LAYOUT`] with a migration. Block kinds and property
//! keys are deliberately the Notion block vocabulary, so readers (web, Swift,
//! Android, a diff tool) can reason about them without a translation table.

/// Bumped when the container layout or the meaning of a property changes.
pub const LAYOUT: u32 = 1;

/// Root map: document identity and timestamps.
pub const META: &str = "meta";
/// Root text: the note title.
pub const TITLE: &str = "title";
/// Root tree: the block tree. Children order is the reading order.
pub const BLOCKS: &str = "blocks";

/// Keys of the [`META`] map.
pub mod meta {
    /// Stable note id (UUID string). Set once by the store on create.
    pub const ID: &str = "id";
    /// Unix milliseconds.
    pub const CREATED_AT: &str = "created_at";
    /// Unix milliseconds, bumped by every edit.
    pub const UPDATED_AT: &str = "updated_at";
    /// [`super::LAYOUT`] of the writer.
    pub const LAYOUT: &str = "layout";
}

/// Keys of a block node's metadata map.
pub mod block {
    /// [`super::super::BlockKind::as_str`] value.
    pub const KIND: &str = "kind";
    /// Child [`loro::LoroText`] container holding the block's rich text.
    pub const TEXT: &str = "text";
    /// `todo` checkbox state.
    pub const CHECKED: &str = "checked";
    /// `code` fence language.
    pub const LANG: &str = "lang";
    /// `image` source URL.
    pub const SRC: &str = "src";
    /// `image` alternative text.
    pub const ALT: &str = "alt";
}

/// Inline mark keys stored on text ranges (Quill delta attribute names).
pub mod mark {
    pub const BOLD: &str = "bold";
    pub const ITALIC: &str = "italic";
    pub const STRIKE: &str = "strike";
    pub const CODE: &str = "code";
    pub const LINK: &str = "link";
}
