//! The note document: a CRDT block tree with a title, plus the operations a
//! text editor needs.
//!
//! One [`NoteDoc`] is one note. Notes are small documents on purpose: a peer
//! exchanges version vectors per note, so editing note A never ships note B.

use std::ops::Range;

use loro::{
    Container, ExportMode, LoroDoc, LoroMap, LoroText, LoroTree, LoroValue, TextDelta, TreeID,
    TreeParentId, ValueOrContainer, VersionVector,
};

use crate::blocks::{Block, BlockId, BlockKind, Mark, Marks, RichText};
use crate::schema;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("crdt: {0}")]
    Loro(#[from] loro::LoroError),
    #[error("crdt encode: {0}")]
    Encode(#[from] loro::LoroEncodeError),
    #[error("version vector is not readable: {0}")]
    VersionVector(String),
    #[error("block {0} does not exist")]
    UnknownBlock(BlockId),
    #[error("block {0} cannot hold child blocks")]
    NotNestable(BlockId),
    #[error("a block cannot be moved into itself")]
    CyclicMove,
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A note document.
#[derive(Clone)]
pub struct NoteDoc {
    doc: LoroDoc,
}

impl Default for NoteDoc {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for NoteDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoteDoc")
            .field("id", &self.id())
            .field("blocks", &self.blocks().len())
            .finish()
    }
}

impl NoteDoc {
    /// A new, empty note with a random peer id (fine for one process; pass a
    /// stable peer id from the store in a real app).
    pub fn new() -> Self {
        Self::with_peer(loro::LoroDoc::new().peer_id())
    }

    /// A document with no local content yet — the form to import a snapshot
    /// into. State-only snapshots cannot attach to a document that already
    /// committed its own ops on the same containers, so loading must start here.
    pub fn bare(peer: u64) -> Self {
        let doc = LoroDoc::new();
        let _ = doc.set_peer_id(peer);
        let out = Self { doc };
        out.configure_styles();
        out
    }

    /// A new, empty note that will author operations under `peer`.
    ///
    /// The peer id must be stable per device, otherwise two devices can look
    /// like one peer and their concurrent edits stop being distinguishable.
    pub fn with_peer(peer: u64) -> Self {
        let out = Self::bare(peer);
        out.ensure_layout();
        out
    }

    /// Loro needs to know how each mark behaves when text is inserted at its
    /// boundaries; without a declared style a mark call fails. Declaring them in
    /// one place keeps two clients from disagreeing about the same key.
    fn configure_styles(&self) {
        use loro::{ExpandType, StyleConfig, StyleConfigMap};
        let mut styles = StyleConfigMap::new();
        for key in [
            schema::mark::BOLD,
            schema::mark::ITALIC,
            schema::mark::STRIKE,
            schema::mark::CODE,
            schema::mark::LINK,
        ] {
            styles.insert(
                key.into(),
                StyleConfig {
                    expand: ExpandType::After,
                },
            );
        }
        self.doc.config_text_style(styles);
    }

    /// Load a note from a stored snapshot (and/or updates).
    ///
    /// Order matters: import first, create missing containers second. The other
    /// order makes a state-only snapshot unattachable — see [`Self::bare`].
    pub fn load(peer: u64, snapshot: Option<&[u8]>, updates: &[Vec<u8>]) -> Result<Self> {
        let out = Self::bare(peer);
        if let Some(bytes) = snapshot {
            out.import(bytes)?;
        }
        if !updates.is_empty() {
            out.import_updates(updates)?;
        }
        out.ensure_layout();
        Ok(out)
    }

    /// Create the containers a note needs if they are missing. Safe to call on
    /// an imported document: it never overwrites imported values.
    pub fn ensure_layout(&self) {
        let meta = self.doc.get_map(schema::META);
        if meta.get(schema::meta::LAYOUT).is_none() {
            let _ = meta.insert(schema::meta::LAYOUT, schema::LAYOUT as i64);
        }
        if meta.get(schema::meta::CREATED_AT).is_none() {
            let _ = meta.insert(schema::meta::CREATED_AT, now_ms());
        }
        if meta.get(schema::meta::UPDATED_AT).is_none() {
            let _ = meta.insert(schema::meta::UPDATED_AT, now_ms());
        }
        let _ = self.doc.get_text(schema::TITLE);
        let _ = self.doc.get_tree(schema::BLOCKS);
        self.doc.commit();
    }

    /// The underlying CRDT document (sync engine, store, tests).
    pub fn crdt(&self) -> &LoroDoc {
        &self.doc
    }

    // ----------------------------------------------------------------- identity

    pub fn id(&self) -> String {
        self.meta_string(schema::meta::ID).unwrap_or_default()
    }

    /// Set once by the store. Idempotent.
    pub fn set_id(&self, id: &str) -> Result<()> {
        if self.id() == id {
            return Ok(());
        }
        self.doc
            .get_map(schema::META)
            .insert(schema::meta::ID, id)?;
        self.doc.commit();
        Ok(())
    }

    pub fn created_at(&self) -> Option<i64> {
        self.meta_i64(schema::meta::CREATED_AT)
    }

    pub fn updated_at(&self) -> Option<i64> {
        self.meta_i64(schema::meta::UPDATED_AT)
    }

    /// Import path: keep the timestamps a note had before it entered this
    /// vault, so migration does not rewrite history.
    pub fn set_times(&self, created_at: i64, updated_at: i64) -> Result<()> {
        let meta = self.doc.get_map(schema::META);
        meta.insert(schema::meta::CREATED_AT, created_at)?;
        meta.insert(schema::meta::UPDATED_AT, updated_at)?;
        self.doc.commit();
        Ok(())
    }

    pub fn layout(&self) -> u32 {
        self.meta_i64(schema::meta::LAYOUT).unwrap_or(0) as u32
    }

    fn meta_string(&self, key: &str) -> Option<String> {
        value_string(&self.doc.get_map(schema::META).get(key)?)
    }

    fn meta_i64(&self, key: &str) -> Option<i64> {
        match self.doc.get_map(schema::META).get(key)? {
            ValueOrContainer::Value(LoroValue::I64(v)) => Some(v),
            ValueOrContainer::Value(LoroValue::Double(v)) => Some(v as i64),
            _ => None,
        }
    }

    fn touch(&self) {
        let meta = self.doc.get_map(schema::META);
        let _ = meta.insert(schema::meta::UPDATED_AT, now_ms());
    }

    fn commit(&self) {
        self.touch();
        self.doc.commit();
    }

    // -------------------------------------------------------------------- title

    pub fn title(&self) -> String {
        self.doc.get_text(schema::TITLE).to_string()
    }

    pub fn set_title(&self, title: &str) {
        let text = self.doc.get_text(schema::TITLE);
        if text.to_string() == title {
            return;
        }
        let len = text.len_unicode();
        if len > 0 {
            let _ = text.delete(0, len);
        }
        if !title.is_empty() {
            let _ = text.insert(0, title);
        }
        self.commit();
    }

    // ------------------------------------------------------------------- blocks

    /// All live blocks in reading order, depth-first.
    pub fn blocks(&self) -> Vec<Block> {
        let tree = self.doc.get_tree(schema::BLOCKS);
        let mut out = Vec::new();
        self.walk(&tree, TreeParentId::Root, None, 0, &mut out);
        out
    }

    fn walk(
        &self,
        tree: &LoroTree,
        parent: TreeParentId,
        parent_id: Option<BlockId>,
        depth: usize,
        out: &mut Vec<Block>,
    ) {
        let Some(children) = tree.children(parent) else {
            return;
        };
        for id in children {
            let block = self.read_block(tree, id, parent_id, depth);
            let nested = block.is_some();
            if let Some(block) = block {
                out.push(block);
            }
            if nested {
                self.walk(
                    tree,
                    TreeParentId::Node(id),
                    Some(BlockId(id)),
                    depth + 1,
                    out,
                );
            }
        }
    }

    fn read_block(
        &self,
        tree: &LoroTree,
        id: TreeID,
        parent: Option<BlockId>,
        depth: usize,
    ) -> Option<Block> {
        let meta = tree.get_meta(id).ok()?;
        let kind = meta
            .get(schema::block::KIND)
            .and_then(|v| value_string(&v))
            .and_then(|s| BlockKind::parse(&s))
            .unwrap_or(BlockKind::Paragraph);
        Some(Block {
            id: BlockId(id),
            parent,
            depth,
            kind,
            text: text_of(&meta),
            checked: matches!(
                meta.get(schema::block::CHECKED),
                Some(ValueOrContainer::Value(LoroValue::Bool(true)))
            ),
            lang: meta_string_of(&meta, schema::block::LANG),
            src: meta_string_of(&meta, schema::block::SRC),
            alt: meta_string_of(&meta, schema::block::ALT),
        })
    }

    pub fn block(&self, id: BlockId) -> Option<Block> {
        let tree = self.doc.get_tree(schema::BLOCKS);
        if tree.is_node_deleted(&id.0).unwrap_or(false) {
            return None;
        }
        let parent = tree.parent(id.0).and_then(|p| match p {
            TreeParentId::Node(p) => Some(BlockId(p)),
            _ => None,
        });
        let depth = self.depth_of(id);
        self.read_block(&tree, id.0, parent, depth)
    }

    fn depth_of(&self, id: BlockId) -> usize {
        let tree = self.doc.get_tree(schema::BLOCKS);
        let mut depth = 0;
        let mut cursor = id.0;
        while let Some(TreeParentId::Node(parent)) = tree.parent(cursor) {
            depth += 1;
            cursor = parent;
            if depth > 128 {
                break;
            }
        }
        depth
    }

    /// Position of `id` among its siblings.
    pub fn index_of(&self, id: BlockId) -> Option<usize> {
        let tree = self.doc.get_tree(schema::BLOCKS);
        let parent = tree.parent(id.0)?;
        tree.children(parent)?.iter().position(|c| *c == id.0)
    }

    pub fn child_count(&self, id: BlockId) -> usize {
        let tree = self.doc.get_tree(schema::BLOCKS);
        tree.children(TreeParentId::Node(id.0))
            .map(|c| c.len())
            .unwrap_or(0)
    }

    /// Append a block to the end of the document (or to `parent`'s children).
    pub fn append_block(
        &self,
        parent: Option<BlockId>,
        kind: BlockKind,
        text: &str,
    ) -> Result<BlockId> {
        let index = match parent {
            None => self
                .doc
                .get_tree(schema::BLOCKS)
                .children(TreeParentId::Root)
                .map(|c| c.len())
                .unwrap_or(0),
            Some(p) => self.child_count(p),
        };
        self.insert_block(parent, index, kind, text)
    }

    pub fn insert_block(
        &self,
        parent: Option<BlockId>,
        index: usize,
        kind: BlockKind,
        text: &str,
    ) -> Result<BlockId> {
        let tree = self.doc.get_tree(schema::BLOCKS);
        if let Some(parent) = parent {
            let parent_kind = self
                .block(parent)
                .map(|b| b.kind)
                .ok_or(Error::UnknownBlock(parent))?;
            if !parent_kind.can_nest() {
                return Err(Error::NotNestable(parent));
            }
        }
        let parent_id = match parent {
            None => TreeParentId::Root,
            Some(p) => TreeParentId::Node(p.0),
        };
        let id = tree.create_at(parent_id, index)?;
        let meta = tree.get_meta(id)?;
        meta.insert(schema::block::KIND, kind.as_str())?;
        match kind {
            BlockKind::ToDo => {
                meta.insert(schema::block::CHECKED, false)?;
            }
            BlockKind::Code => {
                meta.insert(schema::block::LANG, "")?;
            }
            BlockKind::Image => {
                meta.insert(schema::block::SRC, "")?;
                meta.insert(schema::block::ALT, "")?;
            }
            _ => {}
        }
        if kind.has_text() {
            let container = meta.ensure_mergeable_text(schema::block::TEXT)?;
            if !text.is_empty() {
                container.insert(0, text)?;
            }
        }
        self.commit();
        Ok(BlockId(id))
    }

    /// Replace a block's whole text. Editors that support collaborative
    /// keystrokes should use [`Self::insert_block_text`] instead.
    pub fn set_block_text(&self, id: BlockId, text: &str) -> Result<()> {
        let container = self.text_container(id)?;
        if container.to_string() == text {
            return Ok(());
        }
        let len = container.len_unicode();
        if len > 0 {
            container.delete(0, len)?;
        }
        if !text.is_empty() {
            container.insert(0, text)?;
        }
        self.commit();
        Ok(())
    }

    /// Insert text at a character offset — one keystroke, one CRDT operation.
    pub fn insert_block_text(&self, id: BlockId, pos: usize, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let container = self.text_container(id)?;
        container.insert(pos, text)?;
        self.commit();
        Ok(())
    }

    /// Delete `len` characters at a character offset.
    pub fn delete_block_text(&self, id: BlockId, pos: usize, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let container = self.text_container(id)?;
        container.delete(pos, len)?;
        self.commit();
        Ok(())
    }

    /// Apply an inline mark to a character range.
    pub fn mark_block_text(&self, id: BlockId, range: Range<usize>, mark: Mark) -> Result<()> {
        let container = self.text_container(id)?;
        let value = match &mark {
            Mark::Link(url) => LoroValue::String(url.clone().into()),
            _ => LoroValue::Bool(true),
        };
        container.mark(range, mark.key(), value)?;
        self.commit();
        Ok(())
    }

    pub fn unmark_block_text(&self, id: BlockId, range: Range<usize>, key: &str) -> Result<()> {
        let container = self.text_container(id)?;
        container.unmark(range, key)?;
        self.commit();
        Ok(())
    }

    fn text_container(&self, id: BlockId) -> Result<LoroText> {
        let tree = self.doc.get_tree(schema::BLOCKS);
        if !tree.contains(id.0) || tree.is_node_deleted(&id.0).unwrap_or(false) {
            return Err(Error::UnknownBlock(id));
        }
        let meta = tree.get_meta(id.0)?;
        Ok(meta.ensure_mergeable_text(schema::block::TEXT)?)
    }

    pub fn set_block_kind(&self, id: BlockId, kind: BlockKind) -> Result<()> {
        let tree = self.doc.get_tree(schema::BLOCKS);
        let meta = tree.get_meta(id.0)?;
        meta.insert(schema::block::KIND, kind.as_str())?;
        match kind {
            BlockKind::ToDo if meta.get(schema::block::CHECKED).is_none() => {
                meta.insert(schema::block::CHECKED, false)?;
            }
            BlockKind::Code if meta.get(schema::block::LANG).is_none() => {
                meta.insert(schema::block::LANG, "")?;
            }
            _ => {}
        }
        if !kind.has_text() {
            let _ = meta.delete(schema::block::TEXT);
        }
        self.commit();
        Ok(())
    }

    pub fn set_checked(&self, id: BlockId, checked: bool) -> Result<()> {
        let tree = self.doc.get_tree(schema::BLOCKS);
        tree.get_meta(id.0)?
            .insert(schema::block::CHECKED, checked)?;
        self.commit();
        Ok(())
    }

    pub fn set_code_lang(&self, id: BlockId, lang: &str) -> Result<()> {
        let tree = self.doc.get_tree(schema::BLOCKS);
        tree.get_meta(id.0)?.insert(schema::block::LANG, lang)?;
        self.commit();
        Ok(())
    }

    pub fn set_image(&self, id: BlockId, src: &str, alt: &str) -> Result<()> {
        let tree = self.doc.get_tree(schema::BLOCKS);
        let meta = tree.get_meta(id.0)?;
        meta.insert(schema::block::SRC, src)?;
        meta.insert(schema::block::ALT, alt)?;
        self.commit();
        Ok(())
    }

    /// Move `id` under `parent` at `index`. Rejects cycles and non-nestable
    /// parents; the CRDT resolves concurrent moves deterministically.
    pub fn move_block(&self, id: BlockId, parent: Option<BlockId>, index: usize) -> Result<()> {
        let tree = self.doc.get_tree(schema::BLOCKS);
        if let Some(parent) = parent {
            if parent == id || self.is_ancestor(id, parent) {
                return Err(Error::CyclicMove);
            }
            let kind = self
                .block(parent)
                .map(|b| b.kind)
                .ok_or(Error::UnknownBlock(parent))?;
            if !kind.can_nest() {
                return Err(Error::NotNestable(parent));
            }
        }
        let parent_id = match parent {
            None => TreeParentId::Root,
            Some(p) => TreeParentId::Node(p.0),
        };
        tree.mov_to(id.0, parent_id, index)?;
        self.commit();
        Ok(())
    }

    fn is_ancestor(&self, maybe_ancestor: BlockId, of: BlockId) -> bool {
        let tree = self.doc.get_tree(schema::BLOCKS);
        let mut cursor = of.0;
        for _ in 0..128 {
            match tree.parent(cursor) {
                Some(TreeParentId::Node(parent)) => {
                    if parent == maybe_ancestor.0 {
                        return true;
                    }
                    cursor = parent;
                }
                _ => return false,
            }
        }
        false
    }

    /// Delete a block and its subtree.
    pub fn delete_block(&self, id: BlockId) -> Result<()> {
        let tree = self.doc.get_tree(schema::BLOCKS);
        if !tree.contains(id.0) {
            return Err(Error::UnknownBlock(id));
        }
        tree.delete(id.0)?;
        self.commit();
        Ok(())
    }

    /// Delete every block, keeping identity and title.
    pub fn clear_blocks(&self) {
        let tree = self.doc.get_tree(schema::BLOCKS);
        for root in tree.roots() {
            let _ = tree.delete(root);
        }
        self.commit();
    }

    // --------------------------------------------------------------- versions

    /// The local version vector, the handle peers exchange.
    ///
    /// Commits first: an uncommitted operation is part of the document, so a
    /// version vector that ignored it would let a store write payload bytes
    /// under a version that does not cover them.
    pub fn vv(&self) -> Vec<u8> {
        self.doc.commit();
        self.doc.state_vv().encode()
    }

    /// Seal pending operations into one change (a save boundary).
    pub fn seal(&self) {
        self.doc.commit();
    }

    /// Everything a peer that already has `their_vv` is missing.
    ///
    /// Empty when the peer is already current — an encoder may still emit a
    /// header for an empty bundle, and shipping that header would make every
    /// sync look like it moved data.
    pub fn updates_since(&self, their_vv: &[u8]) -> Result<Vec<u8>> {
        let vv =
            VersionVector::decode(their_vv).map_err(|e| Error::VersionVector(e.to_string()))?;
        self.doc.commit();
        if vv >= self.doc.state_vv() {
            return Ok(Vec::new());
        }
        Ok(self.doc.export(ExportMode::updates(&vv))?)
    }

    /// Is `their_vv` exactly this document's version?
    ///
    /// Version vectors are compared semantically: the same version can encode to
    /// different byte orders, so bytes are never the equality test.
    pub fn same_version(&self, their_vv: &[u8]) -> Result<bool> {
        let theirs =
            VersionVector::decode(their_vv).map_err(|e| Error::VersionVector(e.to_string()))?;
        self.doc.commit();
        Ok(theirs == self.doc.state_vv())
    }

    /// Does a peer holding `their_vv` already have everything this note has?
    pub fn covers(&self, their_vv: &[u8]) -> Result<bool> {
        let theirs =
            VersionVector::decode(their_vv).map_err(|e| Error::VersionVector(e.to_string()))?;
        self.doc.commit();
        Ok(theirs >= self.doc.state_vv())
    }

    /// Full snapshot: current state plus the ops that produced it.
    pub fn snapshot(&self) -> Result<Vec<u8>> {
        self.doc.commit();
        Ok(self.doc.export(ExportMode::Snapshot)?)
    }

    /// State-only snapshot: full current content, history dropped to the
    /// minimum needed to keep identity and convergence (the compaction form).
    pub fn state_only_snapshot(&self) -> Result<Vec<u8>> {
        self.doc.commit();
        Ok(self.doc.export(ExportMode::StateOnly(None))?)
    }

    /// Shallow snapshot whose history starts at `vv`: everything before it is
    /// dropped, so the payload stays small while peers still converge forward.
    pub fn shallow_snapshot(&self, vv: &[u8]) -> Result<Vec<u8>> {
        self.doc.commit();
        let vv = VersionVector::decode(vv).map_err(|e| Error::VersionVector(e.to_string()))?;
        let frontiers = self.doc.vv_to_frontiers(&vv);
        Ok(self
            .doc
            .export(ExportMode::ShallowSnapshot(std::borrow::Cow::Owned(
                frontiers,
            )))?)
    }

    /// A read-only copy of the document as of `vv` (time travel / restore).
    pub fn fork_at(&self, vv: &[u8]) -> Result<Self> {
        let vv = VersionVector::decode(vv).map_err(|e| Error::VersionVector(e.to_string()))?;
        let frontiers = self.doc.vv_to_frontiers(&vv);
        let doc = self.doc.fork_at(&frontiers)?;
        Ok(Self { doc })
    }

    /// Import a snapshot or update bundle. Returns whether anything was new.
    pub fn import(&self, bytes: &[u8]) -> Result<bool> {
        let before = self.doc.state_vv();
        self.doc.import(bytes)?;
        self.doc.commit();
        Ok(before != self.doc.state_vv())
    }

    pub fn import_updates(&self, updates: &[Vec<u8>]) -> Result<bool> {
        let before = self.doc.state_vv();
        let refs: Vec<Vec<u8>> = updates.to_vec();
        self.doc.import_batch(&refs)?;
        self.doc.commit();
        Ok(before != self.doc.state_vv())
    }

    // ------------------------------------------------------------- projections

    /// Markdown rendering of the note (`# title`, then the block tree).
    pub fn to_markdown(&self) -> String {
        crate::markdown::to_markdown(self)
    }

    /// Plain text projection used for search indexing.
    pub fn plain_text(&self) -> String {
        let mut out = self.title();
        for block in self.blocks() {
            if !block.kind.has_text() {
                continue;
            }
            out.push('\n');
            out.push_str(&block.plain());
        }
        out
    }

    /// Replace the document's content with `markdown` (import / paste path).
    pub fn set_markdown(&self, markdown: &str) -> Result<()> {
        crate::markdown::apply_markdown(self, markdown)
    }
}

fn text_of(meta: &LoroMap) -> RichText {
    match meta.get(schema::block::TEXT) {
        Some(ValueOrContainer::Container(Container::Text(text))) => {
            let mut out = RichText::new();
            for delta in text.to_delta() {
                if let TextDelta::Insert { insert, attributes } = delta {
                    out.push(&insert, marks_of(attributes.as_ref()));
                }
            }
            out
        }
        _ => RichText::new(),
    }
}

fn marks_of<S: std::hash::BuildHasher>(
    attrs: Option<&std::collections::HashMap<String, LoroValue, S>>,
) -> Marks {
    let mut marks = Marks::default();
    let Some(attrs) = attrs else {
        return marks;
    };
    let flag = |key: &str| {
        matches!(
            attrs.get(key),
            Some(LoroValue::Bool(true)) | Some(LoroValue::I64(_)) | Some(LoroValue::Double(_))
        )
    };
    marks.bold = flag(schema::mark::BOLD);
    marks.italic = flag(schema::mark::ITALIC);
    marks.strike = flag(schema::mark::STRIKE);
    marks.code = flag(schema::mark::CODE);
    marks.link = match attrs.get(schema::mark::LINK) {
        Some(LoroValue::String(url)) => Some(url.to_string()),
        _ => None,
    };
    marks
}

fn value_string(v: &ValueOrContainer) -> Option<String> {
    match v {
        ValueOrContainer::Value(LoroValue::String(s)) => Some(s.to_string()),
        ValueOrContainer::Value(LoroValue::I64(i)) => Some(i.to_string()),
        ValueOrContainer::Value(LoroValue::Double(d)) => Some(d.to_string()),
        _ => None,
    }
}

fn meta_string_of(meta: &LoroMap, key: &str) -> Option<String> {
    meta.get(key).and_then(|v| value_string(&v))
}
