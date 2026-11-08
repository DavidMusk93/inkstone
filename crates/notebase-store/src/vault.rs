//! The vault: the facade an application talks to.
//!
//! ```no_run
//! use notebase::{BlockKind, NoteDoc};
//! use notebase_store::Vault;
//!
//! let vault = Vault::open("/tmp/notes.db").unwrap();
//! let note = vault.create("Reading list").unwrap();
//! let block = note.append_block(None, BlockKind::Paragraph, "start here").unwrap();
//! vault.save(&note).unwrap();
//!
//! let reopened = vault.load(&note.id()).unwrap().unwrap();
//! assert_eq!(reopened.blocks().len(), 2);
//! assert_eq!(vault.search("start here", 10).unwrap().len(), 1);
//! ```

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use notebase::{BlockKind, NoteDoc};

use crate::{NoteMeta, Result, SqliteStore, StoredSnapshot};

/// When to fold the oplog into a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retention {
    /// Write a snapshot once this many updates are sitting on top of the newest one.
    pub snapshot_after_updates: usize,
    /// …or once the log above the newest snapshot is this large. A handful of
    /// huge edits must not be able to bloat the oplog indefinitely.
    pub snapshot_after_bytes: usize,
    /// Keep at most this many snapshots per note.
    pub keep_snapshots: usize,
    /// Snapshot as a state-only payload (history dropped) instead of a full
    /// snapshot.
    ///
    /// Off by default, and that default is load-bearing: a state-only snapshot
    /// only carries the ops of the target version, so the loaded document's
    /// version vector **understates what it has**. A peer then hears "I have
    /// nothing" and never receives the content. Enable this only for vaults that
    /// never sync; a syncing vault folds its oplog into full snapshots (still
    /// bounded, because the log above the newest snapshot is trimmed).
    pub truncate_history: bool,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            snapshot_after_updates: 64,
            snapshot_after_bytes: 1 << 20,
            keep_snapshots: 3,
            truncate_history: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteSummary {
    pub id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Saves recorded for this note (the compacted, permanent history).
    pub saves: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub note_id: String,
    pub title: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SaveStats {
    pub changed: bool,
    pub bytes: usize,
    pub update_seq: i64,
    pub snapshotted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyStats {
    pub note_id: String,
    pub changed: bool,
    pub bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactStats {
    pub notes: usize,
    pub snapshots_written: usize,
    pub updates_trimmed: usize,
    pub snapshots_pruned: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

/// How much a note has actually been worked on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditActivity {
    /// Every recorded save.
    pub saves: usize,
    /// Saves grouped into writing sessions (a session = saves closer together
    /// than the quiet window). A session counts once it has gone quiet.
    pub sessions: usize,
    /// Newest session has not gone quiet yet, so it is not counted above.
    pub in_progress: bool,
    pub last_edit_at: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VaultStats {
    pub notes: usize,
    pub updates: usize,
    pub update_bytes: usize,
    pub snapshots: usize,
    pub snapshot_bytes: usize,
    pub edits: usize,
    pub blobs: usize,
    pub blob_bytes: usize,
}

impl VaultStats {
    pub fn history_bytes(&self) -> usize {
        self.update_bytes + self.snapshot_bytes
    }
}

/// Default: two writing sessions separated by 3 minutes of silence.
pub const SESSION_QUIET_MS: i64 = 180_000;

/// A vault handle. Cheap to clone: every clone shares the same store, which is
/// what a UI thread, a background saver and a sync session each need.
#[derive(Debug, Clone)]
pub struct Vault {
    store: std::sync::Arc<SqliteStore>,
    peer: u64,
    retention: Retention,
}

impl Vault {
    /// Open (or create) a vault file. The device peer id lives in the file, so
    /// two devices never author under the same identity.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let store = std::sync::Arc::new(SqliteStore::open(path)?);
        let peer = match store.vault_meta("peer_id")? {
            Some(raw) => raw.parse::<u64>().unwrap_or_else(|_| fresh_peer()),
            None => {
                let peer = fresh_peer();
                store.set_vault_meta("peer_id", &peer.to_string())?;
                peer
            }
        };
        Ok(Self {
            store,
            peer,
            retention: Retention::default(),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let store = std::sync::Arc::new(SqliteStore::open_in_memory()?);
        let peer = fresh_peer();
        store.set_vault_meta("peer_id", &peer.to_string())?;
        Ok(Self {
            store,
            peer,
            retention: Retention::default(),
        })
    }

    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    pub fn peer_id(&self) -> u64 {
        self.peer
    }

    pub fn set_peer_id(&mut self, peer: u64) -> Result<()> {
        self.peer = peer;
        self.store.set_vault_meta("peer_id", &peer.to_string())
    }

    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    pub fn retention(&self) -> &Retention {
        &self.retention
    }

    // ------------------------------------------------------------------ write

    /// Create a note and persist its first snapshot.
    pub fn create(&self, title: &str) -> Result<NoteDoc> {
        let doc = NoteDoc::with_peer(self.peer);
        doc.set_id(&uuid::Uuid::new_v4().to_string())?;
        doc.set_title(title);
        doc.append_block(None, BlockKind::Paragraph, "")?;
        self.insert(&doc)?;
        Ok(doc)
    }

    /// Create a note from markdown (import path for existing vaults).
    pub fn import_markdown(&self, markdown: &str) -> Result<NoteDoc> {
        let doc = NoteDoc::with_peer(self.peer);
        doc.set_id(&uuid::Uuid::new_v4().to_string())?;
        doc.set_markdown(markdown)?;
        if doc.blocks().is_empty() {
            doc.append_block(None, BlockKind::Paragraph, "")?;
        }
        self.insert(&doc)?;
        Ok(doc)
    }

    /// Import one note under an id the caller already owns (migration from an
    /// existing store). No-op if the id is already present, so re-running the
    /// importer is safe.
    pub fn import_markdown_as(
        &self,
        id: &str,
        markdown: &str,
        created_at: i64,
        updated_at: i64,
    ) -> Result<Option<NoteDoc>> {
        if self.store.note(id)?.is_some() {
            return Ok(None);
        }
        let doc = NoteDoc::with_peer(self.peer);
        doc.set_id(id)?;
        doc.set_markdown(markdown)?;
        if doc.blocks().is_empty() {
            doc.append_block(None, BlockKind::Paragraph, "")?;
        }
        doc.set_times(created_at, updated_at)?;
        self.insert(&doc)?;
        Ok(Some(doc))
    }

    /// Persist a document that is not in the vault yet.
    pub fn insert(&self, doc: &NoteDoc) -> Result<()> {
        if doc.id().is_empty() {
            doc.set_id(&uuid::Uuid::new_v4().to_string())?;
        }
        let id = doc.id();
        if !crate::sqlite::is_valid_id(&id) {
            return Err(crate::Error::NotFound(format!("invalid note id `{id}`")));
        }
        let ts = now_ms();
        let vv = doc.vv();
        // A note entering the vault does not need the history of how it was
        // built; the state-only payload is the same content at a fraction of
        // the bytes. Full history is a `Retention` choice, not a default.
        let snapshot = if self.retention.truncate_history {
            doc.state_only_snapshot()?
        } else {
            doc.snapshot()?
        };
        // Seq 0 covers no log rows: the first update is seq 1.
        self.store.put_snapshot(&id, 0, &vv, &snapshot, ts)?;
        self.store.upsert_note(&NoteMeta {
            id: id.clone(),
            created_at: doc.created_at().unwrap_or(ts),
            updated_at: doc.updated_at().unwrap_or(ts),
            vv,
            snapshot_seq: 0,
            title: String::new(),
            plain: String::new(),
        })?;
        self.store
            .index_note(&id, &doc.title(), &doc.plain_text())?;
        self.store
            .record_edit(&id, ts, doc.plain_text().chars().count() as i64)?;
        Ok(())
    }

    /// Append the document's new operations to the oplog and refresh the
    /// derived projections. Cheap when nothing changed: it compares version
    /// vectors first, so a save with no edits writes nothing.
    pub fn save(&self, doc: &NoteDoc) -> Result<SaveStats> {
        let id = doc.id();
        let meta = self
            .store
            .note(&id)?
            .ok_or_else(|| crate::Error::NotFound(format!("{id} (call Vault::insert first)")))?;
        let vv = doc.vv();
        if doc.same_version(&meta.vv)? {
            return Ok(SaveStats {
                changed: false,
                ..SaveStats::default()
            });
        }
        let bytes = doc.updates_since(&meta.vv)?;
        if bytes.is_empty() {
            return Ok(SaveStats {
                changed: false,
                ..SaveStats::default()
            });
        }
        let ts = now_ms();
        let update_seq = self.store.append_update(&id, &vv, &bytes, ts)?;
        self.store
            .record_edit(&id, ts, doc.plain_text().chars().count() as i64)?;
        self.store.upsert_note(&NoteMeta {
            id: id.clone(),
            created_at: meta.created_at,
            updated_at: ts,
            vv,
            snapshot_seq: meta.snapshot_seq,
            title: String::new(),
            plain: String::new(),
        })?;

        let snapshotted = self.maybe_snapshot(doc, &id, ts)?;
        Ok(SaveStats {
            changed: true,
            bytes: bytes.len(),
            update_seq,
            snapshotted,
        })
    }

    /// Refresh the search projection and index for this note.
    ///
    /// Deliberately separate from [`Self::save`]: durability must not wait on
    /// tokenising a long body. Call it when the note goes idle, on close, or from
    /// [`Self::save_and_index`] for small notes.
    pub fn index(&self, doc: &NoteDoc) -> Result<()> {
        let id = doc.id();
        self.store.index_note(&id, &doc.title(), &doc.plain_text())
    }

    /// Save, then index. The right call for a note that is not huge.
    pub fn save_and_index(&self, doc: &NoteDoc) -> Result<SaveStats> {
        let stats = self.save(doc)?;
        self.index(doc)?;
        Ok(stats)
    }

    fn maybe_snapshot(&self, doc: &NoteDoc, id: &str, ts: i64) -> Result<bool> {
        let meta = self
            .store
            .note(id)?
            .ok_or_else(|| crate::Error::NotFound(id.to_string()))?;
        let pending = self.store.update_count_after(id, meta.snapshot_seq)?;
        let pending_bytes = self.store.update_bytes_after(id, meta.snapshot_seq)?;
        if pending < self.retention.snapshot_after_updates
            && pending_bytes < self.retention.snapshot_after_bytes
        {
            return Ok(false);
        }
        self.write_snapshot(doc, id, ts)?;
        Ok(true)
    }

    fn write_snapshot(&self, doc: &NoteDoc, id: &str, ts: i64) -> Result<()> {
        let seq = self.store.seq_hi(id)?;
        let bytes = if self.retention.truncate_history {
            doc.state_only_snapshot()?
        } else {
            doc.snapshot()?
        };
        let vv = doc.vv();
        self.store.put_snapshot(id, seq, &vv, &bytes, ts)?;
        self.store.trim_updates(id, seq)?;
        self.store
            .prune_snapshots(id, self.retention.keep_snapshots)?;
        let meta = self
            .store
            .note(id)?
            .ok_or_else(|| crate::Error::NotFound(id.to_string()))?;
        self.store.upsert_note(&NoteMeta {
            snapshot_seq: seq,
            ..meta
        })?;
        Ok(())
    }

    /// Fold every note's oplog into a snapshot and reclaim the space.
    pub fn compact(&self) -> Result<CompactStats> {
        let mut stats = CompactStats::default();
        let before = self.store.stats()?;
        for summary in self.store.list_notes(usize::MAX, 0)? {
            let Some(doc) = self.load(&summary.id)? else {
                continue;
            };
            let meta = self
                .store
                .note(&summary.id)?
                .ok_or_else(|| crate::Error::NotFound(summary.id.clone()))?;
            let pending = self
                .store
                .update_count_after(&summary.id, meta.snapshot_seq)?;
            if pending == 0 {
                continue;
            }
            stats.notes += 1;
            stats.updates_trimmed += pending;
            let snapshots = self.store.snapshot_count(&summary.id)?;
            self.write_snapshot(&doc, &summary.id, now_ms())?;
            let kept = self.retention.keep_snapshots.min(snapshots + 1);
            stats.snapshots_written += 1;
            stats.snapshots_pruned += snapshots.saturating_sub(kept - 1);
        }
        self.store.vacuum()?;
        let after = self.store.stats()?;
        stats.bytes_before = before.history_bytes();
        stats.bytes_after = after.history_bytes();
        Ok(stats)
    }

    /// Consistent single-file backup of the whole vault (safe while running).
    /// Returns the size of the backup in bytes.
    pub fn backup_to(&self, path: impl AsRef<Path>) -> Result<u64> {
        let path = path.as_ref();
        self.store.vacuum_into(path)?;
        Ok(std::fs::metadata(path)?.len())
    }

    pub fn delete(&self, id: &str) -> Result<bool> {
        self.store.delete_note(id)
    }

    // ------------------------------------------------------------------- read

    pub fn load(&self, id: &str) -> Result<Option<NoteDoc>> {
        let Some(meta) = self.store.note(id)? else {
            return Ok(None);
        };
        let StoredSnapshot { seq, bytes, .. } = match self.store.latest_snapshot(id)? {
            Some(snapshot) => snapshot,
            None => StoredSnapshot {
                seq: 0,
                ts: 0,
                vv: Vec::new(),
                bytes: Vec::new(),
            },
        };
        let updates = self.store.updates_after(id, seq)?;
        let snapshot = if bytes.is_empty() {
            None
        } else {
            Some(bytes.as_slice())
        };
        let doc = NoteDoc::load(self.peer, snapshot, &updates)?;
        debug_assert!(
            doc.same_version(&meta.vv)?,
            "loaded note is at the stored version"
        );
        Ok(Some(doc))
    }

    pub fn list(&self, limit: usize, offset: usize) -> Result<Vec<NoteSummary>> {
        self.store.list_notes(limit, offset)
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        self.store.search(query, limit)
    }

    /// Time travel: the note as of `vv` (read-only copy).
    pub fn at_version(&self, id: &str, vv: &[u8]) -> Result<Option<NoteDoc>> {
        let Some(doc) = self.load(id)? else {
            return Ok(None);
        };
        Ok(Some(doc.fork_at(vv)?))
    }

    /// Save history that survives compaction: one row per save.
    pub fn edit_activity(&self, id: &str) -> Result<EditActivity> {
        self.edit_activity_with(id, SESSION_QUIET_MS, now_ms())
    }

    pub fn edit_activity_with(&self, id: &str, quiet_ms: i64, now: i64) -> Result<EditActivity> {
        Ok(summarize(&self.store.edit_timestamps(id)?, quiet_ms, now))
    }

    /// Edit activity for a whole page in one query (see [`Self::edit_activity`]).
    pub fn edit_activities(&self, ids: &[String]) -> Result<Vec<(String, EditActivity)>> {
        self.edit_activities_with(ids, SESSION_QUIET_MS, now_ms())
    }

    pub fn edit_activities_with(
        &self,
        ids: &[String],
        quiet_ms: i64,
        now: i64,
    ) -> Result<Vec<(String, EditActivity)>> {
        let rows = self.store.edit_timestamps_for(ids)?;
        let mut out: Vec<(String, EditActivity)> = Vec::new();
        let mut current: Option<(String, Vec<i64>)> = None;
        for (note, ts) in rows {
            match &mut current {
                Some((id, list)) if *id == note => list.push(ts),
                _ => {
                    if let Some((id, list)) = current.take() {
                        out.push((id, summarize(&list, quiet_ms, now)));
                    }
                    current = Some((note, vec![ts]));
                }
            }
        }
        if let Some((id, list)) = current {
            out.push((id, summarize(&list, quiet_ms, now)));
        }
        Ok(out)
    }

    /// Ingest updates from a peer. Creates the note when it is new here.
    pub fn apply_remote(&self, id: &str, bytes: &[u8]) -> Result<ApplyStats> {
        match self.load(id)? {
            Some(doc) => {
                let changed = doc.import(bytes)?;
                let stats = self.save(&doc)?;
                Ok(ApplyStats {
                    note_id: id.to_string(),
                    changed,
                    bytes: stats.bytes,
                })
            }
            None => {
                let doc = NoteDoc::load(self.peer, Some(bytes), &[])?;
                let incoming = doc.id();
                let id = if incoming.is_empty() {
                    id.to_string()
                } else {
                    incoming
                };
                doc.set_id(&id)?;
                self.insert(&doc)?;
                Ok(ApplyStats {
                    note_id: id,
                    changed: true,
                    bytes: bytes.len(),
                })
            }
        }
    }

    // ------------------------------------------------------------------ blobs

    pub fn put_blob(&self, raw: &[u8]) -> Result<String> {
        self.store.put_blob(raw, now_ms())
    }

    pub fn get_blob(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        self.store.get_blob(hash)
    }

    // ------------------------------------------------------------------ stats

    pub fn stats(&self) -> Result<VaultStats> {
        self.store.stats()
    }

    /// The file size on disk, when the vault is backed by a file.
    pub fn file_size(&self, path: impl AsRef<Path>) -> Result<u64> {
        Ok(std::fs::metadata(path)?.len())
    }
}

fn summarize(timestamps: &[i64], quiet_ms: i64, now: i64) -> EditActivity {
    let Some(&last) = timestamps.last() else {
        return EditActivity::default();
    };
    let mut sessions = 0usize;
    let mut prev: Option<i64> = None;
    for ts in timestamps {
        if prev.map(|p| ts - p > quiet_ms).unwrap_or(true) {
            sessions += 1;
        }
        prev = Some(*ts);
    }
    let in_progress = now - last < quiet_ms;
    EditActivity {
        saves: timestamps.len(),
        sessions: sessions - usize::from(in_progress && sessions > 0),
        in_progress,
        last_edit_at: Some(last),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn fresh_peer() -> u64 {
    // Peers only need to be unique, not secret. Derive from entropy we already
    // have: uuid v4 bytes.
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    let mut peer = u64::from_le_bytes(bytes[..8].try_into().unwrap_or([0; 8]));
    if peer == 0 {
        peer = 1;
    }
    peer
}
