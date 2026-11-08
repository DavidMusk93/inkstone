//! Two vaults, two peers, one truth on disk after every message.
//!
//! This is the shape a multi-device app uses: a transport only moves opaque
//! bytes, and the store is consistent the moment a message is accepted.

#![cfg(feature = "store")]

use notebase::{BlockKind, NoteDoc};
use notebase_store::Vault;
use notebase_sync::{Message, SyncConfig, VaultPeer};

fn delivered(a: &mut VaultPeer, b: &mut VaultPeer) -> (bool, bool) {
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

#[test]
fn peers_converge_and_persist_without_the_caller_saving() {
    let alice = Vault::open_in_memory().unwrap();
    let bob = Vault::open_in_memory().unwrap();

    let note = alice.create("shared").unwrap();
    let block = note.blocks()[0].id;
    note.insert_block_text(block, 0, "from alice").unwrap();
    alice.save(&note).unwrap();

    // Bob starts with the same note id and no content of his own.
    let bob_note = NoteDoc::with_peer(bob.peer_id());
    bob_note.set_id(&note.id()).unwrap();
    bob_note
        .append_block(None, BlockKind::Paragraph, "bob was here")
        .unwrap();
    bob.insert(&bob_note).unwrap();

    let mut pa =
        VaultPeer::open(alice.clone(), &note.id(), SyncConfig { chunk_bytes: 1024 }).unwrap();
    let mut pb =
        VaultPeer::open(bob.clone(), &note.id(), SyncConfig { chunk_bytes: 1024 }).unwrap();
    let (a_changed, b_changed) = delivered(&mut pa, &mut pb);
    assert!(a_changed && b_changed, "both sides learned something");
    assert!(!pa.dirty() && !pb.dirty(), "sync persists before returning");

    // Read back through the vault, not the session: the bytes have to be on disk.
    let a_view = alice.load(&note.id()).unwrap().unwrap();
    let b_view = bob.load(&note.id()).unwrap().unwrap();
    assert_eq!(a_view.to_markdown(), b_view.to_markdown());
    assert!(a_view.to_markdown().contains("from alice"));
    assert!(a_view.to_markdown().contains("bob was here"));

    // Search index followed the sync too.
    assert_eq!(alice.search("bob was here", 5).unwrap().len(), 1);
    assert_eq!(bob.search("from alice", 5).unwrap().len(), 1);
}

#[test]
fn a_second_round_is_a_no_op() {
    let alice = Vault::open_in_memory().unwrap();
    let bob = Vault::open_in_memory().unwrap();
    let note = alice.create("quiet").unwrap();
    note.insert_block_text(note.blocks()[0].id, 0, "body")
        .unwrap();
    alice.save(&note).unwrap();
    bob.insert(&NoteDoc::load(bob.peer_id(), Some(&note.snapshot().unwrap()), &[]).unwrap())
        .unwrap();

    let mut pa = VaultPeer::open(alice.clone(), &note.id(), SyncConfig::default()).unwrap();
    let mut pb = VaultPeer::open(bob.clone(), &note.id(), SyncConfig::default()).unwrap();
    let _ = delivered(&mut pa, &mut pb);
    let (again_a, again_b) = delivered(&mut pa, &mut pb);
    assert!(
        !again_a && !again_b,
        "already converged peers exchange nothing"
    );
    assert_eq!(alice.stats().unwrap().notes, 1);
}

#[test]
fn a_missing_note_is_an_error_not_a_silent_empty_document() {
    let vault = Vault::open_in_memory().unwrap();
    let err = VaultPeer::open(vault, "does-not-exist", SyncConfig::default()).unwrap_err();
    assert!(err.to_string().contains("not in this vault"), "{err}");
}
