//! Two replicas, concurrent edits, one converged document. This is the property
//! the whole engine exists for: no lock, no server, no last-writer-wins loss.

use notebase::{BlockKind, Mark, NoteDoc};

fn two_peers() -> (NoteDoc, NoteDoc) {
    let a = NoteDoc::with_peer(1);
    a.set_title("Plan");
    a.append_block(None, BlockKind::Paragraph, "draft").unwrap();
    let b = NoteDoc::with_peer(2);
    b.import(&a.snapshot().unwrap()).unwrap();
    (a, b)
}

fn sync(from: &NoteDoc, to: &NoteDoc) {
    let updates = from.updates_since(&to.vv()).unwrap();
    to.import(&updates).unwrap();
}

#[test]
fn concurrent_edits_converge() {
    let (a, b) = two_peers();
    let blocks = a.blocks();
    let first = blocks[0].id;

    a.insert_block_text(first, 5, " A").unwrap(); // after B sees nothing
    b.insert_block_text(first, 0, "B ").unwrap();
    a.append_block(None, BlockKind::Paragraph, "from A")
        .unwrap();
    b.append_block(None, BlockKind::Paragraph, "from B")
        .unwrap();

    sync(&a, &b);
    sync(&b, &a);

    assert_eq!(a.to_markdown(), b.to_markdown());
    assert_eq!(a.blocks().len(), b.blocks().len());
    assert_eq!(
        notebase::VersionVector::decode(&a.vv()).unwrap(),
        notebase::VersionVector::decode(&b.vv()).unwrap()
    );
    let text = a.blocks()[0].plain();
    assert!(
        text.contains('A') && text.contains('B'),
        "both edits survive: {text}"
    );
}

#[test]
fn sync_is_incremental_and_idempotent() {
    let (a, b) = two_peers();
    let first = a.blocks()[0].id;
    a.insert_block_text(first, 0, "hello ").unwrap();

    let first_batch = a.updates_since(&b.vv()).unwrap();
    assert!(b.import(&first_batch).unwrap());
    assert!(
        !b.import(&first_batch).unwrap(),
        "replaying the same updates is a no-op"
    );

    let empty = a.updates_since(&b.vv()).unwrap();
    assert!(empty.is_empty(), "a current peer needs no bytes at all");
    assert!(a.covers(&b.vv()).unwrap());

    a.insert_block_text(first, 0, "hi ").unwrap();
    let second = a.updates_since(&b.vv()).unwrap();
    assert!(second.len() < first_batch.len() * 8, "delta stays small");
    assert!(b.import(&second).unwrap());
    assert_eq!(a.to_markdown(), b.to_markdown());
}

#[test]
fn blocks_move_and_delete_replicate() {
    let (a, b) = two_peers();
    let parent = a
        .append_block(None, BlockKind::BulletedListItem, "parent")
        .unwrap();
    let child = a
        .append_block(Some(parent), BlockKind::BulletedListItem, "child")
        .unwrap();
    sync(&a, &b);
    assert_eq!(b.blocks().len(), 3);

    b.move_block(child, None, 0).unwrap();
    sync(&b, &a);
    assert_eq!(a.blocks()[0].id, child);
    assert_eq!(a.block(child).unwrap().depth, 0);

    a.delete_block(parent).unwrap();
    sync(&a, &b);
    assert_eq!(
        b.blocks().len(),
        2,
        "subtree delete removes the parent only"
    );
    assert!(a.block(parent).is_none());
}

#[test]
fn rich_text_marks_survive_the_round_trip() {
    let (a, b) = two_peers();
    let id = a
        .append_block(None, BlockKind::Paragraph, "plain bold italic link")
        .unwrap();
    a.mark_block_text(id, 6..10, Mark::Bold).unwrap();
    a.mark_block_text(id, 11..17, Mark::Italic).unwrap();
    a.mark_block_text(id, 18..22, Mark::Link("https://example.com".into()))
        .unwrap();

    sync(&a, &b);
    assert_eq!(a.to_markdown(), b.to_markdown());
    let markdown = a.to_markdown();
    assert!(markdown.contains("**bold**"), "{markdown}");
    assert!(markdown.contains("*italic*"), "{markdown}");
    assert!(
        markdown.contains("[link](https://example.com)"),
        "{markdown}"
    );

    let restored = NoteDoc::with_peer(3);
    restored.set_markdown(&a.to_markdown()).unwrap();
    let spans = &restored.blocks()[1].text;
    assert!(spans
        .spans()
        .iter()
        .any(|s| s.marks.bold && s.text == "bold"));
    assert!(spans
        .spans()
        .iter()
        .any(|s| s.marks.link.as_deref() == Some("https://example.com")));
}

#[test]
fn snapshot_at_a_version_is_time_travel() {
    let (a, _b) = two_peers();
    let id = a.blocks()[0].id;
    a.insert_block_text(id, 0, "v1 ").unwrap();
    let v1 = a.vv();
    a.insert_block_text(id, 0, "v2 ").unwrap();

    let past = a.fork_at(&v1).unwrap();
    assert!(past.blocks()[0].plain().starts_with("v1 "));
    assert!(a.blocks()[0].plain().starts_with("v2 "));

    let restored = NoteDoc::with_peer(9);
    restored.import(&past.snapshot().unwrap()).unwrap();
    assert!(restored.blocks()[0].plain().starts_with("v1 "));
}

#[test]
fn shallow_snapshot_rehydrates_current_state() {
    let (a, _b) = two_peers();
    let id = a.blocks()[0].id;
    for i in 0..20 {
        a.insert_block_text(id, 0, &format!("{i} ")).unwrap();
    }
    let state_only = a.state_only_snapshot().unwrap();
    assert!(a.snapshot().unwrap().len() > state_only.len());

    let fresh = NoteDoc::load(7, Some(&state_only), &[]).unwrap();
    assert_eq!(fresh.blocks()[0].plain(), a.blocks()[0].plain());
    assert_eq!(fresh.title(), a.title());

    // The production path: a compacted note keeps receiving live updates.
    a.insert_block_text(id, 0, "after compaction ").unwrap();
    let delta = a.updates_since(&fresh.vv()).unwrap();
    assert!(fresh.import(&delta).unwrap());
    assert_eq!(fresh.to_markdown(), a.to_markdown());
}

#[test]
fn block_ids_round_trip_through_strings() {
    let note = NoteDoc::new();
    let id = note.append_block(None, BlockKind::Paragraph, "x").unwrap();
    let parsed: notebase::BlockId = id.to_string().parse().unwrap();
    assert_eq!(parsed, id);
}

#[test]
fn rejects_nonsense_moves() {
    let note = NoteDoc::new();
    let parent = note
        .append_block(None, BlockKind::BulletedListItem, "p")
        .unwrap();
    let child = note
        .append_block(Some(parent), BlockKind::BulletedListItem, "c")
        .unwrap();
    assert!(
        note.move_block(parent, Some(child), 0).is_err(),
        "no cycles"
    );
    let code = note.append_block(None, BlockKind::Code, "x").unwrap();
    assert!(note
        .append_block(Some(code), BlockKind::Paragraph, "y")
        .is_err());
}
