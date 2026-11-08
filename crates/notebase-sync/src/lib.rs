//! # notebase-sync
//!
//! Sync without a server and without a conflict log: each note is a CRDT, so
//! two peers need only tell each other **what they already have** (a version
//! vector) and **what the other is missing** (the delta since it).
//!
//! ```no_run
//! use notebase::NoteDoc;
//! use notebase_sync::SyncSession;
//!
//! let a = NoteDoc::with_peer(1);
//! let b = NoteDoc::with_peer(2);
//! let mut sa = SyncSession::new(a.clone(), Default::default());
//! let mut sb = SyncSession::new(b.clone(), Default::default());
//!
//! // Transport is yours: WebSocket, HTTP, iCloud, a USB stick.
//! let hello = sa.hello();
//! let out = sb.receive(hello).unwrap();
//! for message in out.outbound {
//!     let reply = sa.receive(message).unwrap();
//!     for message in reply.outbound {
//!         sb.receive(message).unwrap();
//!     }
//! }
//! ```
//!
//! The session is transport-agnostic and holds no timers: it is the same code
//! path for a live socket, a nightly iCloud push, or a batch import.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations, rust_2018_idioms)]

pub mod wire;

use notebase::{NoteDoc, VersionVector};

pub use wire::{Chunk, Message, Reassembler, DEFAULT_CHUNK_BYTES};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("note: {0}")]
    Note(#[from] notebase::Error),
    #[error("wire: {0}")]
    Wire(String),
    #[error("message is for note {got}, session is on {expected}")]
    WrongNote { expected: String, got: String },
    #[cfg(feature = "store")]
    #[error("store: {0}")]
    Store(#[from] notebase_store::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncConfig {
    pub chunk_bytes: usize,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            chunk_bytes: DEFAULT_CHUNK_BYTES,
        }
    }
}

/// What to do after receiving a message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Step {
    /// Messages to hand to the transport, in order.
    pub outbound: Vec<Message>,
    /// The document changed locally (so the caller must persist it).
    pub changed: bool,
    /// The peer's version is now fully known and covered by this document.
    pub converged: bool,
}

/// One peer's half of a note sync.
#[derive(Debug)]
pub struct SyncSession {
    note: String,
    doc: NoteDoc,
    config: SyncConfig,
    inbound: Reassembler,
    peer_vv: Option<Vec<u8>>,
    sent_updates: bool,
}

impl SyncSession {
    pub fn new(doc: NoteDoc, config: SyncConfig) -> Self {
        let note = doc.id();
        Self {
            note,
            doc,
            config,
            inbound: Reassembler::new(),
            peer_vv: None,
            sent_updates: false,
        }
    }

    pub fn note(&self) -> &str {
        &self.note
    }

    pub fn doc(&self) -> &NoteDoc {
        &self.doc
    }

    fn vv(&self) -> Vec<u8> {
        self.doc.vv()
    }

    /// First message of an exchange.
    pub fn hello(&self) -> Message {
        Message::Hello {
            note: self.note.clone(),
            vv: self.vv(),
        }
    }

    /// Handle one message from the peer.
    pub fn receive(&mut self, message: Message) -> Result<Step> {
        let note = message.note().to_string();
        if !self.note.is_empty() && note != self.note {
            return Err(Error::WrongNote {
                expected: self.note.clone(),
                got: note,
            });
        }
        if self.note.is_empty() {
            // A session opened on a document that has never been persisted adopts
            // the identity of the first note it hears about.
            self.note = note;
            if self.doc.id().is_empty() {
                self.doc.set_id(&self.note)?;
            }
        }
        match message {
            Message::Hello { vv, .. } => {
                self.peer_vv = Some(vv.clone());
                self.sent_updates = false;
                let payload = self.doc.updates_since(&vv)?;
                let mut outbound = Vec::new();
                if !payload.is_empty() {
                    self.sent_updates = true;
                    for chunk in wire::split(&payload, self.config.chunk_bytes) {
                        outbound.push(Message::Updates {
                            note: self.note.clone(),
                            chunk,
                        });
                    }
                }
                Ok(Step {
                    outbound,
                    changed: false,
                    converged: payload.is_empty(),
                })
            }
            Message::Updates { chunk, .. } => {
                let Some(payload) = self.inbound.push(chunk)? else {
                    return Ok(Step::default());
                };
                let changed = self.doc.import(&payload)?;
                Ok(Step {
                    outbound: vec![Message::Ack {
                        note: self.note.clone(),
                        vv: self.vv(),
                    }],
                    changed,
                    converged: false,
                })
            }
            Message::Ack { vv, .. } => {
                let theirs = VersionVector::decode(&vv)
                    .map_err(|e| Error::Wire(format!("version vector: {e}")))?;
                self.peer_vv = Some(vv);
                let mine = self.vv();
                let mine_vv = VersionVector::decode(&mine)
                    .map_err(|e| Error::Wire(format!("version vector: {e}")))?;
                let covered = theirs >= mine_vv.clone() && !self.sent_updates;
                Ok(Step {
                    outbound: Vec::new(),
                    changed: false,
                    converged: covered,
                })
            }
        }
    }

    /// Ask for what is missing: the answer to an [`Message::Ack`] that was not
    /// covered.
    pub fn hello_for_missing(&self) -> Message {
        self.hello()
    }

    pub fn peer_vv(&self) -> Option<&[u8]> {
        self.peer_vv.as_deref()
    }

    /// Updates the peer is missing, for transports that prefer to pull.
    pub fn updates_for_peer(&self) -> Result<Vec<u8>> {
        let Some(vv) = self.peer_vv.as_ref() else {
            return Ok(Vec::new());
        };
        Ok(self.doc.updates_since(vv)?)
    }
}

#[cfg(feature = "store")]
pub mod vault_peer;

#[cfg(feature = "store")]
pub use vault_peer::VaultPeer;
