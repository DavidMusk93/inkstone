//! Sync behaves like a protocol, not like a file copy: no server, no lock, no
//! lost edits, and a payload that is only what the peer is missing.

use notebase::{BlockKind, NoteDoc};
use notebase_store::Vault;
use notebase_sync::{Chunk, Message, SyncConfig, SyncSession};

/// Deliver messages in both directions until both sides go quiet.
fn exchange(a: &mut SyncSession, b: &mut SyncSession) -> (bool, bool) {
    // Both sides announce, the way two reconnecting peers always do.
    let mut to_b = vec![a.hello()];
    let mut to_a: Vec<Message> = vec![b.hello()];
    let (mut a_changed, mut b_changed) = (false, false);
    for _ in 0..64 {
        while let Some(message) = to_b.pop() {
            let step = b.receive(message).expect("b accepts");
            b_changed |= step.changed;
            to_a.extend(step.outbound);
        }
        while let Some(message) = to_a.pop() {
            let step = a.receive(message).expect("a accepts");
            a_changed |= step.changed;
            to_b.extend(step.outbound);
        }
        if to_a.is_empty() && to_b.is_empty() {
            return (a_changed, b_changed);
        }
    }
    panic!("sync did not settle");
}

fn session(doc: &NoteDoc, chunk_bytes: usize) -> SyncSession {
    SyncSession::new(doc.clone(), SyncConfig { chunk_bytes })
}

#[test]
fn concurrent_edits_converge_without_a_server() {
    let a = NoteDoc::with_peer(1);
    a.set_title("Shared");
    a.append_block(None, BlockKind::Paragraph, "line").unwrap();
    let b = NoteDoc::with_peer(2);
    b.import(&a.snapshot().unwrap()).unwrap();

    let first = a.blocks()[0].id;
    a.insert_block_text(first, 4, " from a").unwrap();
    b.insert_block_text(first, 0, "from b: ").unwrap();
    a.append_block(None, BlockKind::BulletedListItem, "a item")
        .unwrap();
    b.append_block(None, BlockKind::BulletedListItem, "b item")
        .unwrap();

    let (mut sa, mut sb) = (session(&a, 16 * 1024), session(&b, 16 * 1024));
    let (_, _) = exchange(&mut sa, &mut sb);

    assert_eq!(a.to_markdown(), b.to_markdown());
    assert!(a.to_markdown().contains("from a"));
    assert!(a.to_markdown().contains("from b: "));
    assert_eq!(a.blocks().len(), b.blocks().len());
}

#[test]
fn large_notes_move_in_chunks() {
    let a = NoteDoc::with_peer(1);
    a.set_title("Long");
    let block = a.append_block(None, BlockKind::Paragraph, "").unwrap();
    // Random-looking text: a repetitive body would fold into a few KB and the
    // chunker would have nothing to chunk.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let body: String = (0..120_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b'a' + (state % 26) as u8)
        })
        .collect();
    a.insert_block_text(block, 0, &body).unwrap();
    let b = NoteDoc::with_peer(2);

    let mut sa = session(&a, 8 * 1024);
    let mut sb = session(&b, 8 * 1024);
    // The sender answers the receiver's hello with the content it is missing.
    let step = sa.receive(sb.hello()).unwrap();
    let chunks: Vec<Chunk> = step
        .outbound
        .iter()
        .filter_map(|m| match m {
            Message::Updates { chunk, .. } => Some(chunk.clone()),
            _ => None,
        })
        .collect();
    assert!(
        chunks.len() > 10,
        "payload is split, got {} chunks",
        chunks.len()
    );
    assert!(chunks.iter().all(|c| c.total as usize == chunks.len()));

    // Deliver out of order, with a duplicate, then the rest.
    chunks[1..].iter().rev().for_each(|chunk| {
        sb.receive(Message::Updates {
            note: a.id(),
            chunk: chunk.clone(),
        })
        .unwrap();
    });
    let last = sb
        .receive(Message::Updates {
            note: a.id(),
            chunk: chunks[0].clone(),
        })
        .unwrap();
    assert!(last.changed);
    assert_eq!(b.to_markdown(), a.to_markdown());
}

#[test]
fn ack_that_already_covers_everything_converges() {
    let a = NoteDoc::with_peer(1);
    a.append_block(None, BlockKind::Paragraph, "x").unwrap();
    // b is loaded from a's snapshot, so it shares a's version exactly and has no
    // local operations of its own to offer.
    let b = NoteDoc::load(2, Some(&a.snapshot().unwrap()), &[]).unwrap();

    let mut sa = session(&a, 16 * 1024);
    let sb = session(&b, 16 * 1024);
    let step = sa.receive(sb.hello()).unwrap();
    assert!(
        step.outbound.is_empty(),
        "nothing to send when the peer is current"
    );
}

#[test]
fn messages_are_checked_against_the_session_note() {
    let a = NoteDoc::with_peer(1);
    a.set_id("note-a").unwrap();
    let b = NoteDoc::with_peer(2);
    b.set_id("note-b").unwrap();
    let sa = session(&a, 1024);
    let mut sb = session(&b, 1024);
    let err = sb.receive(sa.hello()).unwrap_err();
    assert!(err.to_string().contains("note-a"), "{err}");
}

#[test]
fn pull_only_transport_works() {
    let a = NoteDoc::with_peer(1);
    a.append_block(None, BlockKind::Paragraph, "pull me")
        .unwrap();
    let b = NoteDoc::with_peer(2);

    // One request in, one payload out — no push frames at all.
    let mut sa = session(&a, 4096);
    let sb = session(&b, 4096);
    sa.receive(sb.hello()).unwrap();
    let pull = sa.updates_for_peer().unwrap();
    assert!(!pull.is_empty(), "a knows b's vector and owes it a delta");
    b.import(&pull).unwrap();
    assert_eq!(a.to_markdown(), b.to_markdown());
    assert!(
        sb.updates_for_peer().unwrap().is_empty(),
        "b owes a nothing"
    );
}

#[test]
fn end_to_end_with_two_vaults() {
    let alice = Vault::open_in_memory().unwrap();
    let bob = Vault::open_in_memory().unwrap();

    let note = alice.create("Trip plan").unwrap();
    let block = note.blocks()[0].id;
    note.insert_block_text(block, 0, "kyoto in april").unwrap();
    alice.save(&note).unwrap();

    // Bob's device opens the session blind: it learns the note id from the wire.
    let bob_doc = NoteDoc::with_peer(2);
    bob_doc.set_id(&note.id()).unwrap(); // the transport names the note
    let mut sa = session(&note, 4096);
    let mut sb = SyncSession::new(bob_doc.clone(), SyncConfig { chunk_bytes: 4096 });
    exchange(&mut sa, &mut sb);

    assert_eq!(bob_doc.to_markdown(), note.to_markdown());
    bob.insert(&bob_doc).unwrap();
    assert_eq!(bob.search("kyoto", 10).unwrap().len(), 1);

    // The wire is real bytes, and they decode.
    let hello = sa.hello().encode().unwrap();
    assert_eq!(Message::decode(&hello).unwrap().note(), note.id());

    // Bob edits offline; next sync converges both vaults.
    let bob_note = bob.load(&bob_doc.id()).unwrap().unwrap();
    bob_note.insert_block_text(block, 0, "noted: ").unwrap();
    bob.save(&bob_note).unwrap();

    let mut sa = session(&note, 4096);
    let mut sb = session(&bob_note, 4096);
    exchange(&mut sa, &mut sb);
    assert_eq!(sa.doc().to_markdown(), sb.doc().to_markdown());
    assert!(sa.doc().to_markdown().contains("noted: kyoto in april"));
}
