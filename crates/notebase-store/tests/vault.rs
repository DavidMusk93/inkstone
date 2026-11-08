//! What a storage engine has to get right: content survives a restart, the log
//! stays small, history metadata survives compaction, and search finds CJK.

use notebase::{BlockKind, Mark};
use notebase_store::{Retention, Vault, SESSION_QUIET_MS};

fn temp_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("notebase-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.db"));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
    path
}

#[test]
fn note_survives_reopen() {
    let path = temp_path("reopen");
    let id = {
        let vault = Vault::open(&path).unwrap();
        let note = vault.create("Weekly review").unwrap();
        note.append_block(None, BlockKind::BulletedListItem, "shipped sync")
            .unwrap();
        let block = note
            .append_block(None, BlockKind::Paragraph, "next: storage compaction")
            .unwrap();
        note.mark_block_text(block, 6..13, Mark::Bold).unwrap();
        vault.save(&note).unwrap();
        note.id()
    };

    let vault = Vault::open(&path).unwrap();
    let note = vault.load(&id).unwrap().unwrap();
    assert_eq!(note.title(), "Weekly review");
    let blocks = note.blocks();
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[2].plain(), "next: storage compaction");
    assert!(blocks[2].text.spans().iter().any(|s| s.marks.bold));
    assert!(note.to_markdown().contains("**storage**"));
}

#[test]
fn saving_unchanged_note_writes_nothing() {
    let vault = Vault::open_in_memory().unwrap();
    let note = vault.create("quiet").unwrap();
    let first = vault.save(&note).unwrap();
    assert!(!first.changed, "create already stored this version");

    let second = vault.save(&note).unwrap();
    assert!(!second.changed);
    let stats = vault.stats().unwrap();
    assert_eq!(stats.updates, 0, "no oplog rows for a no-op save");
    assert_eq!(stats.notes, 1);

    note.append_block(None, BlockKind::Paragraph, "typed")
        .unwrap();
    let third = vault.save(&note).unwrap();
    assert!(third.changed);
    assert!(third.bytes > 0);
    assert_eq!(vault.stats().unwrap().updates, 1);
}

#[test]
fn oplog_is_folded_into_snapshots_and_stays_small() {
    let vault = Vault::open_in_memory().unwrap().with_retention(Retention {
        snapshot_after_updates: 8,
        snapshot_after_bytes: 1 << 20,
        keep_snapshots: 2,
        truncate_history: true,
    });
    let note = vault.create("long note").unwrap();
    let body = note.blocks()[0].id;
    let paragraph = "一段很长的正文，用来证明每次保存都不会把整篇抄一遍。".repeat(20);
    note.insert_block_text(body, 0, &paragraph).unwrap();
    vault.save(&note).unwrap();

    let mut total_bytes = 0usize;
    for i in 0..400 {
        note.insert_block_text(body, 0, &format!("[{i}]")).unwrap();
        let stats = vault.save(&note).unwrap();
        total_bytes += stats.bytes;
    }

    let stats = vault.stats().unwrap();
    assert!(
        stats.updates <= 8,
        "oplay stays bounded by the snapshot threshold, got {}",
        stats.updates
    );
    assert!(stats.snapshots <= 2, "snapshot retention holds");
    assert!(
        total_bytes < 400 * 1024,
        "incremental saves are small, got {total_bytes} bytes for 400 edits"
    );

    // Compaction must not change what the user sees.
    let before = vault.load(&note.id()).unwrap().unwrap();
    let compacted = vault.compact().unwrap();
    assert!(compacted.bytes_after <= compacted.bytes_before);
    let after = vault.load(&note.id()).unwrap().unwrap();
    assert_eq!(after.to_markdown(), before.to_markdown());

    // And the note keeps working after its history was truncated.
    after
        .insert_block_text(body, 0, "after compaction ")
        .unwrap();
    vault.save(&after).unwrap();
    let reloaded = vault.load(&note.id()).unwrap().unwrap();
    assert!(reloaded.blocks()[0]
        .plain()
        .starts_with("after compaction "));
}

#[test]
fn edit_history_survives_compaction() {
    let vault = Vault::open_in_memory().unwrap();
    let note = vault.create("activity").unwrap();
    let block = note.blocks()[0].id;
    for i in 0..5 {
        note.insert_block_text(block, 0, &format!("{i}")).unwrap();
        vault.save(&note).unwrap();
    }
    let quiet_now = 4_000_000_000_000;
    let before = vault
        .edit_activity_with(&note.id(), SESSION_QUIET_MS, quiet_now)
        .unwrap();
    vault.compact().unwrap();
    let after = vault
        .edit_activity_with(&note.id(), SESSION_QUIET_MS, quiet_now)
        .unwrap();
    assert_eq!(before.saves, after.saves, "compaction keeps the edit log");
    assert_eq!(after.saves, 6, "create + five edits");
    assert_eq!(after.sessions, 1, "all inside one writing session");
    assert!(!after.in_progress);
}

#[test]
fn sessions_are_debounced_not_counted_per_save() {
    let vault = Vault::open_in_memory().unwrap();
    let note = vault.create("bursts").unwrap();
    let id = note.id();
    let store = vault.store();
    store.clear_edits(&id).unwrap(); // synthetic timeline, not this second
                                     // Two sittings six minutes apart; each sitting has many saves a few seconds
                                     // apart. Timestamps are unix milliseconds, the same unit as the quiet window.
    for ts in [1_000_000, 1_003_000, 1_006_000, 1_009_000] {
        store.record_edit(&id, ts, 10).unwrap();
    }
    for ts in [1_400_000, 1_403_000] {
        store.record_edit(&id, ts, 10).unwrap();
    }
    let settled = vault
        .edit_activity_with(&id, SESSION_QUIET_MS, 1_403_000 + SESSION_QUIET_MS)
        .unwrap();
    assert_eq!(settled.saves, 6);
    assert_eq!(settled.sessions, 2);
    assert!(!settled.in_progress);

    let typing = vault
        .edit_activity_with(&id, SESSION_QUIET_MS, 1_403_100)
        .unwrap();
    assert_eq!(typing.sessions, 1, "the newest session is not counted yet");
    assert!(typing.in_progress);
}

#[test]
fn activity_for_a_page_is_one_query_and_matches_single_note_reads() {
    let vault = Vault::open_in_memory().unwrap();
    let store = vault.store();
    let mut ids = Vec::new();
    for n in 0..3 {
        let note = vault.create(&format!("note {n}")).unwrap();
        store.clear_edits(&note.id()).unwrap();
        for ts in [1_000_000, 1_003_000, 1_400_000] {
            store.record_edit(&note.id(), ts + n * 10, 10).unwrap();
        }
        ids.push(note.id());
    }
    let now = 1_500_000 + SESSION_QUIET_MS; // well past the newest save
    let batch = vault
        .edit_activities_with(&ids, SESSION_QUIET_MS, now)
        .unwrap();
    assert_eq!(batch.len(), 3);
    for (id, activity) in &batch {
        let single = vault.edit_activity_with(id, SESSION_QUIET_MS, now).unwrap();
        assert_eq!(activity, &single, "batch and single reads agree for {id}");
        assert_eq!(activity.saves, 3);
        assert_eq!(activity.sessions, 2, "two sittings, three minutes apart");
    }
    // A page of unknown ids is empty, not an error.
    assert!(vault
        .edit_activities_with(&[], SESSION_QUIET_MS, now)
        .unwrap()
        .is_empty());
}

#[test]
fn search_handles_cjk_and_latin() {
    let vault = Vault::open_in_memory().unwrap();
    let note = vault
        .import_markdown("# 集群机器分布报告\n\n任务都应该运行在 A 类型机器上。\n\n## Rollout\n\nship the sync engine first\n")
        .unwrap();

    let hits = vault.search("机器", 10).unwrap();
    assert_eq!(hits.len(), 1, "two-character CJK query uses the LIKE path");
    assert_eq!(hits[0].note_id, note.id());

    let hits = vault.search("sync engine", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].snippet.contains("sync engine"),
        "{}",
        hits[0].snippet
    );

    assert!(vault.search("nothing here", 10).unwrap().is_empty());
    assert!(vault.search("", 10).unwrap().is_empty());
}

#[test]
fn search_follows_updates_and_deletes() {
    let vault = Vault::open_in_memory().unwrap();
    let note = vault.create("draft").unwrap();
    let block = note.blocks()[0].id;
    note.insert_block_text(block, 0, "kangaroo").unwrap();
    vault.save_and_index(&note).unwrap();
    assert_eq!(vault.search("kangaroo", 10).unwrap().len(), 1);

    note.set_block_text(block, "wallaby").unwrap();
    vault.save_and_index(&note).unwrap();
    assert!(vault.search("kangaroo", 10).unwrap().is_empty());
    assert_eq!(vault.search("wallaby", 10).unwrap().len(), 1);

    vault.delete(&note.id()).unwrap();
    assert!(vault.search("wallaby", 10).unwrap().is_empty());
    assert!(vault.load(&note.id()).unwrap().is_none());
}

#[test]
fn blobs_are_content_addressed() {
    let vault = Vault::open_in_memory().unwrap();
    let png = b"\x89PNG\r\n\x1a\nnot really a png".to_vec();
    let a = vault.put_blob(&png).unwrap();
    let b = vault.put_blob(&png).unwrap();
    assert_eq!(a, b);
    assert_eq!(vault.store().blob_count().unwrap(), 1);
    assert_eq!(vault.get_blob(&a).unwrap().unwrap(), png);
    assert!(vault.get_blob("nope").unwrap().is_none());
}

#[test]
fn remote_note_lands_and_converges() {
    let alice = Vault::open_in_memory().unwrap();
    let bob = Vault::open_in_memory().unwrap();

    let note = alice.create("shared").unwrap();
    let block = note.blocks()[0].id;
    note.insert_block_text(block, 0, "from alice ").unwrap();
    alice.save(&note).unwrap();

    let payload = note.snapshot().unwrap();
    let applied = bob.apply_remote(&note.id(), &payload).unwrap();
    assert!(applied.changed);
    let bob_copy = bob.load(&note.id()).unwrap().unwrap();
    assert_eq!(bob_copy.to_markdown(), note.to_markdown());

    // Bidirectional: bob edits, alice pulls the delta.
    bob_copy.insert_block_text(block, 0, "bob ").unwrap();
    bob.save(&bob_copy).unwrap();
    let delta = bob_copy.updates_since(&note.vv()).unwrap();
    let applied = alice.apply_remote(&note.id(), &delta).unwrap();
    assert!(applied.changed);
    let merged = alice.load(&note.id()).unwrap().unwrap();
    assert_eq!(
        merged.to_markdown(),
        bob.load(&note.id()).unwrap().unwrap().to_markdown()
    );
    assert!(merged.blocks()[0].plain().contains("alice"));
    assert!(merged.blocks()[0].plain().contains("bob"));

    // Replaying the same delta changes nothing.
    let replay = alice.apply_remote(&note.id(), &delta).unwrap();
    assert!(!replay.changed);
}

#[test]
fn time_travel_reads_past_versions() {
    let vault = Vault::open_in_memory().unwrap().with_retention(Retention {
        snapshot_after_updates: 1_000,
        snapshot_after_bytes: usize::MAX,
        keep_snapshots: 8,
        truncate_history: false,
    });
    let note = vault.create("history").unwrap();
    let block = note.blocks()[0].id;
    note.set_block_text(block, "one").unwrap();
    vault.save(&note).unwrap();
    let v1 = note.vv();
    note.set_block_text(block, "two").unwrap();
    vault.save(&note).unwrap();

    let past = vault.at_version(&note.id(), &v1).unwrap().unwrap();
    assert_eq!(past.blocks()[0].plain(), "one");
    assert_eq!(
        vault.load(&note.id()).unwrap().unwrap().blocks()[0].plain(),
        "two"
    );
}

#[test]
fn migrating_a_note_is_idempotent_and_keeps_timestamps() {
    let vault = Vault::open_in_memory().unwrap();
    let md = "# 集群机器分布\n\n原始时间要保留。\n\n- 一台\n- 另一台\n";
    let first = vault
        .import_markdown_as("legacy-note-1", md, 1_700_000_000_000, 1_700_000_900_000)
        .unwrap()
        .expect("first import lands");
    assert_eq!(first.title(), "集群机器分布");
    assert_eq!(first.created_at(), Some(1_700_000_000_000));
    assert_eq!(first.updated_at(), Some(1_700_000_900_000));

    let summary = vault.list(10, 0).unwrap();
    assert_eq!(summary[0].updated_at, 1_700_000_900_000);

    let again = vault
        .import_markdown_as("legacy-note-1", md, 1_700_000_000_000, 1_700_000_900_000)
        .unwrap();
    assert!(
        again.is_none(),
        "re-running the importer must not duplicate"
    );
    assert_eq!(vault.stats().unwrap().notes, 1);
    let loaded = vault.load("legacy-note-1").unwrap().unwrap();
    assert_eq!(loaded.to_markdown().trim_end(), md.trim_end());
}

#[test]
fn backup_copies_a_consistent_vault_while_writing() {
    let vault = Vault::open_in_memory().unwrap();
    let note = vault.create("backup").unwrap();
    note.insert_block_text(note.blocks()[0].id, 0, "before the copy")
        .unwrap();
    vault.save_and_index(&note).unwrap();

    let dir = std::env::temp_dir().join(format!("notebase-backup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("copy.db");
    let bytes = vault.backup_to(&path).unwrap();
    assert!(bytes > 0);

    let restored = Vault::open(&path).unwrap();
    let copy = restored.load(&note.id()).unwrap().unwrap();
    assert_eq!(copy.to_markdown(), note.to_markdown());
    assert_eq!(restored.search("before the copy", 5).unwrap().len(), 1);
    assert!(restored.stats().unwrap().edits >= 1);

    // Overwriting an existing backup is normal (nightly job).
    let again = vault.backup_to(&path).unwrap();
    assert!(again > 0);
}

#[test]
fn list_is_newest_first() {
    let vault = Vault::open_in_memory().unwrap();
    let a = vault.create("a").unwrap();
    let b = vault.create("b").unwrap();
    b.append_block(None, BlockKind::Paragraph, "newest")
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    vault.save(&b).unwrap();
    let list = vault.list(10, 0).unwrap();
    assert_eq!(list[0].id, b.id());
    assert!(list.iter().any(|n| n.id == a.id()));
    assert_eq!(vault.list(1, 0).unwrap().len(), 1);
    assert_eq!(vault.list(10, 2).unwrap().len(), 0);
}
