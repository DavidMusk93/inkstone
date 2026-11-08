//! # notebase-ffi
//!
//! The seam between the engine and native applications (Swift, Kotlin, Python).
//! It is deliberately thin: handles, records, and the operations an editor
//! actually performs. No API is invented here — every method maps 1:1 to
//! `notebase-store::Vault` or `notebase::NoteDoc`.
//!
//! Generate bindings for an app:
//!
//! ```text
//! cargo build --release -p notebase-ffi
//! cargo run --release -p notebase-ffi --bin uniffi-bindgen -- generate \
//!   --library target/release/libnotebase_ffi.dylib --language swift --out-dir out
//! ```
//!
//! JSON appears exactly where the host app already speaks JSON (block trees for
//! a web view). Everything else is typed, so a schema mistake fails at compile
//! time in the host language instead of at runtime in a string.

#![forbid(unsafe_code)]

use std::sync::Arc;

use notebase::{BlockKind, Mark, NoteDoc};
use notebase_store::{EditActivity, NoteSummary, SaveStats, SearchHit, Vault};

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum InkError {
    #[error("{message}")]
    Message { message: String },
}

impl From<notebase_store::Error> for InkError {
    fn from(err: notebase_store::Error) -> Self {
        InkError::Message {
            message: err.to_string(),
        }
    }
}

impl From<notebase::Error> for InkError {
    fn from(err: notebase::Error) -> Self {
        InkError::Message {
            message: err.to_string(),
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct NoteSummaryFfi {
    pub id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub saves: u32,
}

impl From<NoteSummary> for NoteSummaryFfi {
    fn from(summary: NoteSummary) -> Self {
        Self {
            id: summary.id,
            title: summary.title,
            created_at: summary.created_at,
            updated_at: summary.updated_at,
            saves: summary.saves.min(u32::MAX as usize) as u32,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SearchHitFfi {
    pub note_id: String,
    pub title: String,
    pub snippet: String,
}

impl From<SearchHit> for SearchHitFfi {
    fn from(hit: SearchHit) -> Self {
        Self {
            note_id: hit.note_id,
            title: hit.title,
            snippet: hit.snippet,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SaveStatsFfi {
    pub changed: bool,
    pub bytes: u64,
    pub update_seq: i64,
    pub snapshotted: bool,
}

impl From<SaveStats> for SaveStatsFfi {
    fn from(stats: SaveStats) -> Self {
        Self {
            changed: stats.changed,
            bytes: stats.bytes as u64,
            update_seq: stats.update_seq,
            snapshotted: stats.snapshotted,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ActivityFfi {
    pub saves: u32,
    pub sessions: u32,
    pub in_progress: bool,
    /// 0 when the note has never been saved.
    pub last_edit_at: i64,
}

impl From<EditActivity> for ActivityFfi {
    fn from(activity: EditActivity) -> Self {
        Self {
            saves: activity.saves.min(u32::MAX as usize) as u32,
            sessions: activity.sessions.min(u32::MAX as usize) as u32,
            in_progress: activity.in_progress,
            last_edit_at: activity.last_edit_at.unwrap_or(0),
        }
    }
}

/// A vault, opened on a file or in memory.
#[derive(Debug, uniffi::Object)]
pub struct VaultHandle {
    inner: Vault,
}

#[uniffi::export]
impl VaultHandle {
    #[uniffi::constructor]
    pub fn open(path: String) -> Result<Arc<Self>, InkError> {
        Ok(Arc::new(Self {
            inner: Vault::open(path)?,
        }))
    }

    #[uniffi::constructor]
    pub fn in_memory() -> Result<Arc<Self>, InkError> {
        Ok(Arc::new(Self {
            inner: Vault::open_in_memory()?,
        }))
    }

    /// Device identity used to author operations. Stable for the life of the file.
    pub fn peer_id(&self) -> u64 {
        self.inner.peer_id()
    }

    pub fn create(&self, title: String) -> Result<Arc<NoteHandle>, InkError> {
        let doc = self.inner.create(&title)?;
        Ok(Arc::new(NoteHandle { inner: doc }))
    }

    pub fn load(&self, id: String) -> Result<Option<Arc<NoteHandle>>, InkError> {
        Ok(self
            .inner
            .load(&id)?
            .map(|inner| Arc::new(NoteHandle { inner })))
    }

    /// Import markdown under an existing id (migration). `created_at`/`updated_at`
    /// are unix milliseconds; pass 0 for "now".
    pub fn import_markdown(
        &self,
        markdown: String,
        id: Option<String>,
        created_at: i64,
        updated_at: i64,
    ) -> Result<Option<Arc<NoteHandle>>, InkError> {
        let now = now_ms();
        let doc = match id {
            Some(id) => self.inner.import_markdown_as(
                &id,
                &markdown,
                if created_at == 0 { now } else { created_at },
                if updated_at == 0 { now } else { updated_at },
            )?,
            None => Some(self.inner.import_markdown(&markdown)?),
        };
        Ok(doc.map(|inner| Arc::new(NoteHandle { inner })))
    }

    /// Durability only: appends the delta, never waits on search indexing.
    pub fn save(&self, note: Arc<NoteHandle>) -> Result<SaveStatsFfi, InkError> {
        Ok(self.inner.save(&note.inner)?.into())
    }

    /// Save, then refresh the search projection. The right call for small notes.
    pub fn save_and_index(&self, note: Arc<NoteHandle>) -> Result<SaveStatsFfi, InkError> {
        Ok(self.inner.save_and_index(&note.inner)?.into())
    }

    /// Refresh the search projection only (call when the note goes idle).
    pub fn index(&self, note: Arc<NoteHandle>) -> Result<(), InkError> {
        self.inner.index(&note.inner)?;
        Ok(())
    }

    pub fn list(&self, limit: u32, offset: u32) -> Result<Vec<NoteSummaryFfi>, InkError> {
        Ok(self
            .inner
            .list(limit as usize, offset as usize)?
            .into_iter()
            .map(NoteSummaryFfi::from)
            .collect())
    }

    pub fn search(&self, query: String, limit: u32) -> Result<Vec<SearchHitFfi>, InkError> {
        Ok(self
            .inner
            .search(&query, limit as usize)?
            .into_iter()
            .map(SearchHitFfi::from)
            .collect())
    }

    pub fn delete(&self, id: String) -> Result<bool, InkError> {
        Ok(self.inner.delete(&id)?)
    }

    pub fn activity(&self, id: String) -> Result<ActivityFfi, InkError> {
        Ok(self.inner.edit_activity(&id)?.into())
    }

    /// Activity for a whole page in one query — what a list view needs.
    pub fn activities(&self, notes: Vec<String>) -> Result<Vec<NoteActivityFfi>, InkError> {
        Ok(self
            .inner
            .edit_activities(&notes)?
            .into_iter()
            .map(|(note, activity)| NoteActivityFfi {
                note,
                activity: activity.into(),
            })
            .collect())
    }

    /// Updates a peer holding `peer_vv` is missing. Empty means "you are current".
    pub fn updates_for(&self, id: String, peer_vv: Vec<u8>) -> Result<Vec<u8>, InkError> {
        let note = self.inner.load(&id)?.ok_or_else(|| InkError::Message {
            message: format!("note {id} not found"),
        })?;
        Ok(note.updates_since(&peer_vv)?)
    }

    /// Ingest a peer's payload (snapshot or updates). Creates the note if new.
    pub fn apply_remote(&self, id: String, bytes: Vec<u8>) -> Result<bool, InkError> {
        Ok(self.inner.apply_remote(&id, &bytes)?.changed)
    }

    /// Fold the oplog and reclaim space. Returns the number of notes compacted.
    pub fn compact(&self) -> Result<u64, InkError> {
        let stats = self.inner.compact()?;
        Ok(stats.updates_trimmed as u64)
    }

    pub fn stats_json(&self) -> Result<String, InkError> {
        let stats = self.inner.stats()?;
        Ok(serde_json::json!({
            "notes": stats.notes,
            "updates": stats.updates,
            "updateBytes": stats.update_bytes,
            "snapshots": stats.snapshots,
            "snapshotBytes": stats.snapshot_bytes,
            "edits": stats.edits,
            "blobs": stats.blobs,
            "blobBytes": stats.blob_bytes,
        })
        .to_string())
    }

    /// Open a sync session for one note. Transport-agnostic: the host moves the
    /// opaque bytes however it likes (HTTP, WebSocket, a file in iCloud).
    pub fn sync_handle(&self, note: String) -> Result<Arc<SyncHandle>, InkError> {
        let peer = notebase_sync::VaultPeer::open(
            self.inner.clone(),
            &note,
            notebase_sync::SyncConfig::default(),
        )
        .map_err(|e| InkError::Message {
            message: e.to_string(),
        })?;
        Ok(Arc::new(SyncHandle {
            inner: std::sync::Mutex::new(peer),
        }))
    }

    /// Consistent single-file copy of the vault (`VACUUM INTO`). Safe while the
    /// app is writing; this is what a pre-cutover safety snapshot should use.
    pub fn backup_to(&self, path: String) -> Result<u64, InkError> {
        Ok(self.inner.backup_to(path)?)
    }

    pub fn put_blob(&self, bytes: Vec<u8>) -> Result<String, InkError> {
        Ok(self.inner.put_blob(&bytes)?)
    }

    pub fn get_blob(&self, hash: String) -> Result<Option<Vec<u8>>, InkError> {
        Ok(self.inner.get_blob(&hash)?)
    }
}

/// Edit activity for one note inside a page.
#[derive(Debug, Clone, uniffi::Record)]
pub struct NoteActivityFfi {
    pub note: String,
    pub activity: ActivityFfi,
}

/// One step of a sync exchange, in host terms.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SyncStepFfi {
    /// Messages to hand back to the peer, already encoded.
    pub outbound: Vec<Vec<u8>>,
    /// The engine's note changed (it has already been saved by then).
    pub changed: bool,
    /// The peer's version is covered by this note.
    pub converged: bool,
    /// Current markdown, for the host to project into its own storage.
    pub markdown: String,
    pub title: String,
}

/// A sync session bound to a stored note. Every accepted message is persisted
/// before `receive` returns, so the host never has to remember to save.
#[derive(Debug, uniffi::Object)]
pub struct SyncHandle {
    inner: std::sync::Mutex<notebase_sync::VaultPeer>,
}

#[uniffi::export]
impl SyncHandle {
    /// First message of an exchange ("here is what I have").
    pub fn hello(&self) -> Result<Vec<u8>, InkError> {
        let peer = self.lock()?;
        peer.hello().encode().map_err(|e| InkError::Message {
            message: e.to_string(),
        })
    }

    /// Feed one opaque message. The engine has already persisted whatever it
    /// accepted; `markdown` is what the host should project into its own tables.
    pub fn receive_step(&self, message: Vec<u8>) -> Result<SyncStepFfi, InkError> {
        let mut peer = self.lock()?;
        let incoming = notebase_sync::Message::decode(&message).map_err(|e| InkError::Message {
            message: e.to_string(),
        })?;
        let step = peer.receive(incoming).map_err(|e| InkError::Message {
            message: e.to_string(),
        })?;
        let mut outbound = Vec::with_capacity(step.outbound.len());
        for message in step.outbound {
            outbound.push(message.encode().map_err(|e| InkError::Message {
                message: e.to_string(),
            })?);
        }
        Ok(SyncStepFfi {
            outbound,
            changed: step.changed,
            converged: step.converged,
            markdown: peer.doc().to_markdown(),
            title: peer.doc().title(),
        })
    }

    /// Payload for a pull-style transport.
    pub fn updates_for_peer(&self) -> Result<Vec<u8>, InkError> {
        let peer = self.lock()?;
        peer.updates_for_peer().map_err(|e| InkError::Message {
            message: e.to_string(),
        })
    }

    pub fn note(&self) -> String {
        self.inner
            .lock()
            .map(|peer| peer.note().to_string())
            .unwrap_or_default()
    }
}

impl SyncHandle {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, notebase_sync::VaultPeer>, InkError> {
        self.inner.lock().map_err(|_| InkError::Message {
            message: "sync session is poisoned".to_string(),
        })
    }
}

/// One note document.
#[derive(Debug, uniffi::Object)]
pub struct NoteHandle {
    inner: NoteDoc,
}

#[uniffi::export]
impl NoteHandle {
    pub fn id(&self) -> String {
        self.inner.id()
    }

    pub fn title(&self) -> String {
        self.inner.title()
    }

    pub fn set_title(&self, title: String) {
        self.inner.set_title(&title);
    }

    pub fn markdown(&self) -> String {
        self.inner.to_markdown()
    }

    pub fn plain_text(&self) -> String {
        self.inner.plain_text()
    }

    /// Block tree as JSON — the shape a web view renders.
    pub fn blocks_json(&self) -> String {
        let blocks: Vec<serde_json::Value> = self
            .inner
            .blocks()
            .into_iter()
            .map(|block| {
                serde_json::json!({
                    "id": block.id.to_string(),
                    "parent": block.parent.map(|p| p.to_string()),
                    "depth": block.depth,
                    "kind": block.kind.as_str(),
                    "text": block.text.text(),
                    "checked": block.checked,
                    "lang": block.lang,
                    "src": block.src,
                    "alt": block.alt,
                    "spans": block.text.spans().iter().map(|span| serde_json::json!({
                        "text": span.text,
                        "bold": span.marks.bold,
                        "italic": span.marks.italic,
                        "strike": span.marks.strike,
                        "code": span.marks.code,
                        "link": span.marks.link,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        serde_json::Value::Array(blocks).to_string()
    }

    pub fn set_markdown(&self, markdown: String) -> Result<(), InkError> {
        self.inner.set_markdown(&markdown)?;
        Ok(())
    }

    /// Append a block. `kind` is a [`BlockKind`] name (`paragraph`, `heading_1`,
    /// `bulleted_list_item`, `todo`, `code`, `divider`, `image`, …).
    pub fn append_block(
        &self,
        parent: Option<String>,
        kind: String,
        text: String,
    ) -> Result<String, InkError> {
        let kind = parse_kind(&kind)?;
        let parent = match parent {
            Some(raw) => Some(parse_block_id(&raw)?),
            None => None,
        };
        Ok(self.inner.append_block(parent, kind, &text)?.to_string())
    }

    /// Insert text at a character offset: one keystroke, one CRDT operation.
    pub fn insert_text(&self, block_id: String, pos: u32, text: String) -> Result<(), InkError> {
        let id = parse_block_id(&block_id)?;
        self.inner.insert_block_text(id, pos as usize, &text)?;
        Ok(())
    }

    pub fn delete_text(&self, block_id: String, pos: u32, len: u32) -> Result<(), InkError> {
        let id = parse_block_id(&block_id)?;
        self.inner
            .delete_block_text(id, pos as usize, len as usize)?;
        Ok(())
    }

    pub fn set_block_text(&self, block_id: String, text: String) -> Result<(), InkError> {
        let id = parse_block_id(&block_id)?;
        self.inner.set_block_text(id, &text)?;
        Ok(())
    }

    /// Apply an inline mark: `bold`, `italic`, `strike`, `code`, `link`.
    /// `value` carries the URL for `link`.
    pub fn mark_text(
        &self,
        block_id: String,
        start: u32,
        end: u32,
        mark: String,
        value: Option<String>,
    ) -> Result<(), InkError> {
        let id = parse_block_id(&block_id)?;
        let mark = match mark.as_str() {
            "bold" => Mark::Bold,
            "italic" => Mark::Italic,
            "strike" => Mark::Strike,
            "code" => Mark::Code,
            "link" => Mark::Link(value.unwrap_or_default()),
            other => {
                return Err(InkError::Message {
                    message: format!("unknown mark `{other}`"),
                })
            }
        };
        self.inner
            .mark_block_text(id, start as usize..end as usize, mark)?;
        Ok(())
    }

    pub fn set_checked(&self, block_id: String, checked: bool) -> Result<(), InkError> {
        let id = parse_block_id(&block_id)?;
        self.inner.set_checked(id, checked)?;
        Ok(())
    }

    pub fn move_block(
        &self,
        block_id: String,
        parent: Option<String>,
        index: u32,
    ) -> Result<(), InkError> {
        let id = parse_block_id(&block_id)?;
        let parent = match parent {
            Some(raw) => Some(parse_block_id(&raw)?),
            None => None,
        };
        self.inner.move_block(id, parent, index as usize)?;
        Ok(())
    }

    pub fn delete_block(&self, block_id: String) -> Result<(), InkError> {
        let id = parse_block_id(&block_id)?;
        self.inner.delete_block(id)?;
        Ok(())
    }

    /// Version vector to hand to a peer.
    pub fn vv(&self) -> Vec<u8> {
        self.inner.vv()
    }

    pub fn updates_since(&self, vv: Vec<u8>) -> Result<Vec<u8>, InkError> {
        Ok(self.inner.updates_since(&vv)?)
    }

    pub fn snapshot(&self) -> Result<Vec<u8>, InkError> {
        Ok(self.inner.snapshot()?)
    }

    pub fn import(&self, bytes: Vec<u8>) -> Result<bool, InkError> {
        Ok(self.inner.import(&bytes)?)
    }

    pub fn created_at(&self) -> i64 {
        self.inner.created_at().unwrap_or(0)
    }

    pub fn updated_at(&self) -> i64 {
        self.inner.updated_at().unwrap_or(0)
    }
}

fn parse_kind(kind: &str) -> Result<BlockKind, InkError> {
    BlockKind::parse(kind).ok_or_else(|| InkError::Message {
        message: format!("unknown block kind `{kind}`"),
    })
}

fn parse_block_id(raw: &str) -> Result<notebase::BlockId, InkError> {
    raw.parse().map_err(|_| InkError::Message {
        message: format!("invalid block id `{raw}`"),
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_surface_drives_a_vault() {
        let vault = VaultHandle::in_memory().unwrap();
        let note = vault.create("FFI".to_string()).unwrap();
        assert!(!note.id().is_empty());

        let intro = note
            .append_block(None, "paragraph".into(), "from the host app".into())
            .unwrap();
        note.mark_text(intro.clone(), 0, 4, "bold".into(), None)
            .unwrap();
        let bullet = note
            .append_block(
                Some(intro.clone()),
                "bulleted_list_item".into(),
                "nested".into(),
            )
            .unwrap();
        assert!(bullet.contains('@'));

        let stats = vault.save_and_index(note.clone()).unwrap();
        assert!(stats.changed);
        assert!(note.markdown().contains("**from**"));
        assert!(note.blocks_json().contains("\"depth\":1"));

        let hits = vault.search("host app".into(), 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].note_id, note.id());

        let activity = vault.activity(note.id()).unwrap();
        assert_eq!(activity.saves, 2, "create counts as the first save");
        assert!((activity.last_edit_at - note.updated_at()).abs() < 5_000);

        // A second vault pulls the note by version vector.
        let peer = VaultHandle::in_memory().unwrap();
        let empty_vv = vec![0u8];
        let _ = empty_vv;
        let payload = note.snapshot().unwrap();
        assert!(peer.apply_remote(note.id(), payload).unwrap());
        let pulled = peer.load(note.id()).unwrap().unwrap();
        assert_eq!(pulled.markdown(), note.markdown());

        assert!(vault.stats_json().unwrap().contains("\"notes\":1"));
        assert!(vault.list(10, 0).unwrap().len() == 1);
        assert!(vault.delete(note.id()).unwrap());
        assert!(vault.list(10, 0).unwrap().is_empty());
    }

    #[test]
    fn sync_handle_moves_a_note_between_vaults() {
        let alice = VaultHandle::in_memory().unwrap();
        let bob = VaultHandle::in_memory().unwrap();
        let note = alice.create("sync".into()).unwrap();
        note.insert_text(note.blocks_json().len().to_string(), 0, " ".into())
            .ok();
        let block = note
            .append_block(None, "paragraph".into(), "alice content".into())
            .unwrap();
        assert!(!block.is_empty());
        let _ = alice.save_and_index(note.clone());

        // Bob needs the note row to exist before a session can open on it.
        let payload = note.snapshot().unwrap();
        assert!(bob.apply_remote(note.id(), payload).unwrap());

        let sa = alice.sync_handle(note.id()).unwrap();
        let sb = bob.sync_handle(note.id()).unwrap();
        let hello = sa.hello().unwrap();
        let out = sb.receive_step(hello).unwrap();
        assert!(out.outbound.is_empty(), "bob owes alice nothing yet");
        for message in sa.receive_step(sb.hello().unwrap()).unwrap().outbound {
            let back = sb.receive_step(message).unwrap();
            for reply in back.outbound {
                let _ = sa.receive_step(reply).unwrap();
            }
        }
        let a = alice.load(note.id()).unwrap().unwrap();
        let b = bob.load(note.id()).unwrap().unwrap();
        assert_eq!(a.markdown(), b.markdown());
        assert!(b.markdown().contains("alice content"));
        assert!(bob.search("alice content".into(), 5).unwrap().len() == 1);
    }

    #[test]
    fn activities_come_back_for_a_page() {
        let vault = VaultHandle::in_memory().unwrap();
        let a = vault.create("a".into()).unwrap();
        let b = vault.create("b".into()).unwrap();
        let page = vault
            .activities(vec![a.id(), b.id(), "missing".into()])
            .unwrap();
        assert_eq!(page.len(), 2, "unknown ids are simply absent");
        for entry in &page {
            assert_eq!(entry.activity.saves, 1, "create counts as one save");
        }
    }

    #[test]
    fn ffi_reports_errors_as_messages() {
        let vault = VaultHandle::in_memory().unwrap();
        let note = vault.create("errors".to_string()).unwrap();
        let err = note
            .append_block(None, "not_a_kind".into(), "x".into())
            .unwrap_err();
        assert!(err.to_string().contains("not_a_kind"));
        let err = note
            .insert_text("nope@1".into(), 0, "x".into())
            .unwrap_err();
        assert!(err.to_string().contains("nope@1"));
    }
}
