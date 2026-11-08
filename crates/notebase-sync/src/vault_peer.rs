//! Sync that owns its durability: a session bound to a stored note.
//!
//! The plain [`SyncSession`](crate::SyncSession) works on a document and leaves
//! persistence to the caller. That is right for a pure transport, and wrong for
//! the one thing an app always gets wrong: forgetting to save after importing a
//! peer's updates. `VaultPeer` loads the note from a vault, feeds the session,
//! and writes the delta back before returning — so "the message arrived" and
//! "the change is on disk" are the same event.

use crate::{Message, Result, Step, SyncConfig, SyncSession};
use notebase::NoteDoc;
use notebase_store::Vault;

#[derive(Debug)]
pub struct VaultPeer {
    vault: Vault,
    note: String,
    session: SyncSession,
    persisted: bool,
}

impl VaultPeer {
    /// Open a sync session for a stored note. The note must exist.
    pub fn open(vault: Vault, note: &str, config: SyncConfig) -> Result<Self> {
        let doc = vault
            .load(note)?
            .ok_or_else(|| crate::Error::Wire(format!("note {note} is not in this vault")))?;
        Ok(Self {
            vault,
            note: note.to_string(),
            session: SyncSession::new(doc, config),
            persisted: true,
        })
    }

    pub fn note(&self) -> &str {
        &self.note
    }

    /// Did the last `receive` leave changes that have not been persisted?
    pub fn dirty(&self) -> bool {
        !self.persisted
    }

    pub fn hello(&self) -> Message {
        self.session.hello()
    }

    pub fn receive(&mut self, message: Message) -> Result<Step> {
        let step = self.session.receive(message)?;
        if step.changed {
            // Persist immediately: an imported update that is not stored is a
            // note that will silently roll back on the next load.
            self.vault.save_and_index(self.session.doc())?;
            self.persisted = true;
        }
        Ok(step)
    }

    /// Payload a peer is missing, for pull transports.
    pub fn updates_for_peer(&self) -> Result<Vec<u8>> {
        self.session.updates_for_peer()
    }

    pub fn doc(&self) -> &NoteDoc {
        self.session.doc()
    }
}
