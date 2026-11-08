//! The SQLite backend.
//!
//! Schema notes:
//! * `note_updates` is `WITHOUT ROWID` keyed by `(note_id, seq)` — the oplog is
//!   read as a range scan and trimmed from the front.
//! * `notes_fts` is an FTS5 **external content** table over `notes`: the text
//!   lives once, in `notes.plain`, and the index stores postings only.
//! * `note_edits` keeps one tiny row per save. It is the history that never gets
//!   compacted away, which is why "how much has this note been worked on" stays
//!   answerable years later.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

use crate::codec;
use crate::vault::{NoteSummary, SearchHit, VaultStats};
use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteMeta {
    pub id: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Version vector covered by the oplog.
    pub vv: Vec<u8>,
    /// Highest oplog seq folded into the latest snapshot (0 when none).
    pub snapshot_seq: i64,
    pub title: String,
    pub plain: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSnapshot {
    pub seq: i64,
    pub ts: i64,
    pub vv: Vec<u8>,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init()?;
        Ok(store)
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn init(&self) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS vault_meta (
              key   TEXT PRIMARY KEY,
              value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS notes (
              id           TEXT PRIMARY KEY,
              created_at   INTEGER NOT NULL,
              updated_at   INTEGER NOT NULL,
              vv           BLOB    NOT NULL,
              snapshot_seq INTEGER NOT NULL DEFAULT 0,
              seq_hi       INTEGER NOT NULL DEFAULT 0,
              title        TEXT    NOT NULL DEFAULT '',
              plain        TEXT    NOT NULL DEFAULT '',
              indexed      INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS note_updates (
              note_id TEXT    NOT NULL,
              seq     INTEGER NOT NULL,
              ts      INTEGER NOT NULL,
              bytes   BLOB    NOT NULL,
              PRIMARY KEY (note_id, seq)
            ) WITHOUT ROWID;

            CREATE TABLE IF NOT EXISTS note_snapshots (
              note_id TEXT    NOT NULL,
              seq     INTEGER NOT NULL,
              ts      INTEGER NOT NULL,
              vv      BLOB    NOT NULL,
              bytes   BLOB    NOT NULL,
              PRIMARY KEY (note_id, seq)
            ) WITHOUT ROWID;

            CREATE TABLE IF NOT EXISTS blobs (
              hash  TEXT PRIMARY KEY,
              size  INTEGER NOT NULL,
              ts    INTEGER NOT NULL,
              bytes BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS note_edits (
              id      INTEGER PRIMARY KEY,
              note_id TEXT    NOT NULL,
              ts      INTEGER NOT NULL,
              chars   INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS note_edits_by_note ON note_edits(note_id, ts);

            CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
              title,
              plain,
              content = 'notes',
              content_rowid = 'rowid',
              tokenize = 'trigram',
              columnsize = 0
            );

            DROP TRIGGER IF EXISTS notes_ai;
            DROP TRIGGER IF EXISTS notes_ad;
            DROP TRIGGER IF EXISTS notes_au;
            "#,
        )?;
        let _ = conn.execute(
            "ALTER TABLE notes ADD COLUMN indexed INTEGER NOT NULL DEFAULT 0",
            [],
        );
        Ok(())
    }

    // ------------------------------------------------------------ vault meta

    pub fn vault_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT value FROM vault_meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_vault_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO vault_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---------------------------------------------------------------- notes

    pub fn note(&self, id: &str) -> Result<Option<NoteMeta>> {
        let conn = self.conn();
        let row = conn
            .query_row(
                "SELECT id, created_at, updated_at, vv, snapshot_seq, title, plain
                 FROM notes WHERE id = ?1",
                params![id],
                |row| {
                    Ok(NoteMeta {
                        id: row.get(0)?,
                        created_at: row.get(1)?,
                        updated_at: row.get(2)?,
                        vv: row.get(3)?,
                        snapshot_seq: row.get(4)?,
                        title: row.get(5)?,
                        plain: row.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn upsert_note(&self, meta: &NoteMeta) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO notes (id, created_at, updated_at, vv, snapshot_seq, seq_hi, title, plain)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, '', '')
             ON CONFLICT(id) DO UPDATE SET
               updated_at = excluded.updated_at,
               vv = excluded.vv,
               snapshot_seq = excluded.snapshot_seq",
            params![
                meta.id,
                meta.created_at,
                meta.updated_at,
                meta.vv,
                meta.snapshot_seq
            ],
        )?;
        Ok(())
    }

    /// Refresh the searchable projection and its index for one note.
    ///
    /// Kept out of the save path on purpose: re-tokenising a 200 KB body is
    /// milliseconds of work that must not sit between a keystroke and durability.
    /// Editors call this when the note goes idle or closes.
    pub fn index_note(&self, id: &str, title: &str, plain: &str) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let old: Option<(String, String, i64, i64)> = tx
            .query_row(
                "SELECT title, plain, rowid, indexed FROM notes WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((old_title, old_plain, rowid, indexed)) = old else {
            return Err(crate::Error::NotFound(id.to_string()));
        };
        if indexed != 0 && old_title == title && old_plain == plain {
            return Ok(());
        }
        // External-content FTS5 requires the old row to be deleted with exactly
        // the values it was inserted with; doing that twice corrupts the index.
        if indexed != 0 {
            tx.execute(
                "INSERT INTO notes_fts(notes_fts, rowid, title, plain) VALUES ('delete', ?1, ?2, ?3)",
                params![rowid, old_title, old_plain],
            )?;
        }
        tx.execute(
            "INSERT INTO notes_fts(rowid, title, plain) VALUES (?1, ?2, ?3)",
            params![rowid, title, plain],
        )?;
        tx.execute(
            "UPDATE notes SET title = ?2, plain = ?3, indexed = 1 WHERE id = ?1",
            params![id, title, plain],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Drop a note's index entry without touching the note itself.
    pub fn unindex_note(&self, id: &str) -> Result<()> {
        let conn = self.conn();
        if let Some((title, plain, rowid)) = conn
            .query_row(
                "SELECT title, plain, rowid FROM notes WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?
        {
            conn.execute(
                "INSERT INTO notes_fts(notes_fts, rowid, title, plain) VALUES ('delete', ?1, ?2, ?3)",
                params![rowid, title, plain],
            )?;
        }
        Ok(())
    }

    pub fn delete_note(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM note_updates WHERE note_id = ?1", params![id])?;
        tx.execute("DELETE FROM note_snapshots WHERE note_id = ?1", params![id])?;
        tx.execute("DELETE FROM note_edits WHERE note_id = ?1", params![id])?;
        if let Some((title, plain, rowid)) = tx
            .query_row(
                "SELECT title, plain, rowid FROM notes WHERE id = ?1 AND indexed != 0",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?
        {
            tx.execute(
                "INSERT INTO notes_fts(notes_fts, rowid, title, plain) VALUES ('delete', ?1, ?2, ?3)",
                params![rowid, title, plain],
            )?;
        }
        let removed = tx.execute("DELETE FROM notes WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(removed > 0)
    }

    pub fn list_notes(&self, limit: usize, offset: usize) -> Result<Vec<NoteSummary>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT n.id, n.title, n.created_at, n.updated_at,
                    (SELECT COUNT(*) FROM note_edits e WHERE e.note_id = n.id)
             FROM notes n
             ORDER BY n.updated_at DESC
             LIMIT ?1 OFFSET ?2",
        )?;
        let rows = stmt
            .query_map(params![limit as i64, offset as i64], |row| {
                Ok(NoteSummary {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    created_at: row.get(2)?,
                    updated_at: row.get(3)?,
                    saves: row.get::<_, i64>(4)? as usize,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---------------------------------------------------------------- oplog

    pub fn max_update_seq(&self, note_id: &str) -> Result<i64> {
        let conn = self.conn();
        let seq: Option<i64> = conn
            .query_row(
                "SELECT MAX(seq) FROM note_updates WHERE note_id = ?1",
                params![note_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(seq.unwrap_or(0))
    }

    /// Append one encoded update. Returns the seq it was stored under.
    ///
    /// The sequence is monotonic for the lifetime of the note, not for the
    /// lifetime of the log: trimming old rows must never make a number reusable,
    /// or a snapshot would claim to cover updates it has never seen.
    pub fn append_update(&self, note_id: &str, vv: &[u8], bytes: &[u8], ts: i64) -> Result<i64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let hi: Option<i64> = tx
            .query_row(
                "SELECT seq_hi FROM notes WHERE id = ?1",
                params![note_id],
                |row| row.get(0),
            )
            .optional()?;
        let seq = match hi {
            Some(hi) => {
                let seq = hi + 1;
                tx.execute(
                    "UPDATE notes SET seq_hi = ?2 WHERE id = ?1",
                    params![note_id, seq],
                )?;
                seq
            }
            None => {
                tx.query_row(
                    "SELECT COALESCE(MAX(seq), 0) + 1 FROM note_updates WHERE note_id = ?1",
                    params![note_id],
                    |row| row.get::<_, i64>(0),
                )? + 1
            }
        };
        tx.execute(
            "INSERT INTO note_updates (note_id, seq, ts, bytes) VALUES (?1, ?2, ?3, ?4)",
            params![note_id, seq, ts, codec::pack(bytes)],
        )?;
        tx.commit()?;
        let _ = vv;
        Ok(seq)
    }

    /// Highest sequence number ever allocated for this note.
    pub fn seq_hi(&self, note_id: &str) -> Result<i64> {
        let conn = self.conn();
        let hi: Option<i64> = conn
            .query_row(
                "SELECT seq_hi FROM notes WHERE id = ?1",
                params![note_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(hi.unwrap_or(0))
    }

    pub fn updates_after(&self, note_id: &str, seq: i64) -> Result<Vec<Vec<u8>>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT bytes FROM note_updates WHERE note_id = ?1 AND seq > ?2 ORDER BY seq ASC",
        )?;
        let rows = stmt
            .query_map(params![note_id, seq], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|packed| codec::unpack(&packed))
            .collect()
    }

    pub fn update_count_after(&self, note_id: &str, seq: i64) -> Result<usize> {
        let conn = self.conn();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM note_updates WHERE note_id = ?1 AND seq > ?2",
            params![note_id, seq],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Bytes of log sitting on top of a snapshot.
    pub fn update_bytes_after(&self, note_id: &str, seq: i64) -> Result<usize> {
        let conn = self.conn();
        let bytes: i64 = conn.query_row(
            "SELECT COALESCE(SUM(LENGTH(bytes)), 0) FROM note_updates
             WHERE note_id = ?1 AND seq > ?2",
            params![note_id, seq],
            |row| row.get(0),
        )?;
        Ok(bytes as usize)
    }

    pub fn trim_updates(&self, note_id: &str, up_to_seq: i64) -> Result<usize> {
        let conn = self.conn();
        let removed = conn.execute(
            "DELETE FROM note_updates WHERE note_id = ?1 AND seq <= ?2",
            params![note_id, up_to_seq],
        )?;
        Ok(removed)
    }

    // ------------------------------------------------------------- snapshots

    pub fn put_snapshot(
        &self,
        note_id: &str,
        seq: i64,
        vv: &[u8],
        bytes: &[u8],
        ts: i64,
    ) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO note_snapshots (note_id, seq, ts, vv, bytes)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(note_id, seq) DO UPDATE SET
               ts = excluded.ts, vv = excluded.vv, bytes = excluded.bytes",
            params![note_id, seq, ts, vv, codec::pack(bytes)],
        )?;
        Ok(())
    }

    pub fn latest_snapshot(&self, note_id: &str) -> Result<Option<StoredSnapshot>> {
        let conn = self.conn();
        let row = conn
            .query_row(
                "SELECT seq, ts, vv, bytes FROM note_snapshots
                 WHERE note_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![note_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )
            .optional()?;
        match row {
            Some((seq, ts, vv, packed)) => Ok(Some(StoredSnapshot {
                seq,
                ts,
                vv,
                bytes: codec::unpack(&packed)?,
            })),
            None => Ok(None),
        }
    }

    pub fn snapshot_count(&self, note_id: &str) -> Result<usize> {
        let conn = self.conn();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM note_snapshots WHERE note_id = ?1",
            params![note_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Keep the newest `keep` snapshots, delete the rest.
    pub fn prune_snapshots(&self, note_id: &str, keep: usize) -> Result<usize> {
        let conn = self.conn();
        let removed = conn.execute(
            "DELETE FROM note_snapshots
             WHERE note_id = ?1
               AND seq NOT IN (
                 SELECT seq FROM note_snapshots WHERE note_id = ?1 ORDER BY seq DESC LIMIT ?2
               )",
            params![note_id, keep as i64],
        )?;
        Ok(removed)
    }

    // ----------------------------------------------------------------- blobs

    pub fn put_blob(&self, raw: &[u8], ts: i64) -> Result<String> {
        let hash = codec::content_hash(raw);
        let conn = self.conn();
        conn.execute(
            "INSERT OR IGNORE INTO blobs (hash, size, ts, bytes) VALUES (?1, ?2, ?3, ?4)",
            params![hash, raw.len() as i64, ts, codec::pack(raw)],
        )?;
        Ok(hash)
    }

    pub fn get_blob(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.conn();
        let packed: Option<Vec<u8>> = conn
            .query_row(
                "SELECT bytes FROM blobs WHERE hash = ?1",
                params![hash],
                |row| row.get(0),
            )
            .optional()?;
        match packed {
            Some(packed) => Ok(Some(codec::unpack(&packed)?)),
            None => Ok(None),
        }
    }

    pub fn blob_count(&self) -> Result<usize> {
        let conn = self.conn();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    // ------------------------------------------------------------------ edits

    /// One row per save that changed something. Tiny on purpose: this is the
    /// history that survives compaction.
    pub fn record_edit(&self, note_id: &str, ts: i64, chars: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO note_edits (note_id, ts, chars) VALUES (?1, ?2, ?3)",
            params![note_id, ts, chars],
        )?;
        Ok(())
    }

    pub fn clear_edits(&self, note_id: &str) -> Result<usize> {
        let conn = self.conn();
        Ok(conn.execute(
            "DELETE FROM note_edits WHERE note_id = ?1",
            params![note_id],
        )?)
    }

    /// Edit timestamps for a page of notes in one query, ordered per note.
    ///
    /// A note page must not load 100 CRDT documents to answer "how much has this
    /// been worked on"; the answer is already in `note_edits`.
    pub fn edit_timestamps_for(&self, note_ids: &[String]) -> Result<Vec<(String, i64)>> {
        if note_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; note_ids.len()].join(",");
        let sql = format!(
            "SELECT note_id, ts FROM note_edits WHERE note_id IN ({placeholders}) ORDER BY note_id, ts"
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        for (index, id) in note_ids.iter().enumerate() {
            stmt.raw_bind_parameter(index + 1, id)?;
        }
        let mut rows = stmt.raw_query();
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((row.get::<_, String>(0)?, row.get::<_, i64>(1)?));
        }
        Ok(out)
    }

    pub fn edit_timestamps(&self, note_id: &str) -> Result<Vec<i64>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT ts FROM note_edits WHERE note_id = ?1 ORDER BY ts ASC")?;
        let rows = stmt
            .query_map(params![note_id], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ----------------------------------------------------------------- search

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn();
        // The trigram tokenizer indexes every substring of length >= 3. Shorter
        // queries (a single CJK character is a whole word) fall back to LIKE:
        // correct beats fast, and a personal vault is small.
        if query.chars().count() < 3 {
            let pattern = format!("%{}%", escape_like(query));
            let mut stmt = conn.prepare(
                "SELECT id, title, substr(plain, 1, 160)
                 FROM notes
                 WHERE title LIKE ?1 ESCAPE '\\' OR plain LIKE ?1 ESCAPE '\\'
                 ORDER BY updated_at DESC
                 LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![pattern, limit as i64], |row| {
                    Ok(SearchHit {
                        note_id: row.get(0)?,
                        title: row.get(1)?,
                        snippet: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            return Ok(rows);
        }

        let pattern = format!("\"{}\"", query.replace('"', "\"\""));
        let mut stmt = conn.prepare(
            "SELECT n.id, n.title, snippet(notes_fts, 1, '[', ']', '…', 16)
             FROM notes_fts
             JOIN notes n ON n.rowid = notes_fts.rowid
             WHERE notes_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![pattern, limit as i64], |row| {
                Ok(SearchHit {
                    note_id: row.get(0)?,
                    title: row.get(1)?,
                    snippet: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ------------------------------------------------------------------ stats

    pub fn stats(&self) -> Result<VaultStats> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT
               (SELECT COUNT(*) FROM notes),
               (SELECT COUNT(*) FROM note_updates),
               (SELECT COALESCE(SUM(LENGTH(bytes)), 0) FROM note_updates),
               (SELECT COUNT(*) FROM note_snapshots),
               (SELECT COALESCE(SUM(LENGTH(bytes)), 0) FROM note_snapshots),
               (SELECT COUNT(*) FROM note_edits),
               (SELECT COUNT(*) FROM blobs),
               (SELECT COALESCE(SUM(LENGTH(bytes)), 0) FROM blobs)",
        )?;
        let stats = stmt.query_row([], |row| {
            Ok(VaultStats {
                notes: row.get::<_, i64>(0)? as usize,
                updates: row.get::<_, i64>(1)? as usize,
                update_bytes: row.get::<_, i64>(2)? as usize,
                snapshots: row.get::<_, i64>(3)? as usize,
                snapshot_bytes: row.get::<_, i64>(4)? as usize,
                edits: row.get::<_, i64>(5)? as usize,
                blobs: row.get::<_, i64>(6)? as usize,
                blob_bytes: row.get::<_, i64>(7)? as usize,
            })
        })?;
        Ok(stats)
    }

    /// Reclaim free pages. Compaction deletes data; without this the file never
    /// shrinks.
    pub fn vacuum(&self) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA incremental_vacuum;")?;
        Ok(())
    }

    /// Write a single-file, transactionally consistent copy of the vault.
    ///
    /// `VACUUM INTO` is the only copy that is safe while the app is running: a
    /// plain file copy can catch a half-written WAL. This is the primitive a
    /// backup job should call instead of copying `*.db`, `*.db-wal`, `*.db-shm`.
    pub fn vacuum_into(&self, path: &std::path::Path) -> Result<()> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = self.conn();
        conn.execute("VACUUM INTO ?1", params![path.to_string_lossy()])?;
        Ok(())
    }

    /// Run `VACUUM` from scratch (slow; the compaction escape hatch).
    pub fn vacuum_full(&self) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch("VACUUM;")?;
        Ok(())
    }
}

fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Note ids are opaque to the engine but must be filename-safe and stable.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}
