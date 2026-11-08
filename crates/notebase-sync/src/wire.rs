//! Messages on the wire.
//!
//! Three message kinds, and no fourth: a peer announces what it has
//! ([`Message::Hello`]), the other answers with what it is missing
//! ([`Message::Updates`]), and the receiver confirms ([`Message::Ack`]).
//! That is the y-protocols / automerge sync handshake, verbatim in spirit.
//!
//! Large payloads are cut into [`Chunk`]s so a transport can move them one
//! socket write at a time without buffering the whole note twice.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Default chunk payload: 128 KiB. Small enough for a WebSocket frame, large
/// enough that a million-keystroke note is ~a dozen frames.
pub const DEFAULT_CHUNK_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// "Here is the version vector of what I already have."
    Hello { note: String, vv: Vec<u8> },
    /// "Here are the operations you are missing."
    Updates { note: String, chunk: Chunk },
    /// "I now have this much. Send me anything else you have."
    Ack { note: String, vv: Vec<u8> },
}

impl Message {
    pub fn note(&self) -> &str {
        match self {
            Message::Hello { note, .. }
            | Message::Updates { note, .. }
            | Message::Ack { note, .. } => note,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        bincode::serialize(self).map_err(|e| Error::Wire(e.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        bincode::deserialize(bytes).map_err(|e| Error::Wire(e.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// Zero-based index of this chunk.
    pub index: u32,
    /// Total number of chunks in this payload (>= 1).
    pub total: u32,
    pub body: Vec<u8>,
}

impl Chunk {
    pub fn is_last(&self) -> bool {
        self.index + 1 == self.total
    }
}

/// Cut a payload into chunks. An empty payload yields no chunks at all: "I have
/// nothing you need" is expressed by silence, not by an empty frame.
pub fn split(payload: &[u8], chunk_bytes: usize) -> Vec<Chunk> {
    if payload.is_empty() {
        return Vec::new();
    }
    let size = chunk_bytes.max(1);
    let total = payload.len().div_ceil(size);
    (0..total)
        .map(|index| {
            let start = index * size;
            let end = (start + size).min(payload.len());
            Chunk {
                index: index as u32,
                total: total as u32,
                body: payload[start..end].to_vec(),
            }
        })
        .collect()
}

/// Reassembles an outgoing payload from chunks that may arrive out of order,
/// twice, or not at all.
#[derive(Debug, Default, Clone)]
pub struct Reassembler {
    total: u32,
    parts: Vec<Option<Vec<u8>>>,
    received: u32,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one chunk. Returns the payload once every chunk has arrived.
    pub fn push(&mut self, chunk: Chunk) -> Result<Option<Vec<u8>>> {
        if chunk.total == 0 || chunk.index >= chunk.total {
            return Err(Error::Wire(format!(
                "chunk {} of {} is out of range",
                chunk.index, chunk.total
            )));
        }
        if self.total == 0 {
            self.total = chunk.total;
            self.parts = vec![None; chunk.total as usize];
        } else if self.total != chunk.total {
            return Err(Error::Wire(format!(
                "chunk set changed size: {} -> {}",
                self.total, chunk.total
            )));
        }
        let slot = &mut self.parts[chunk.index as usize];
        if slot.is_none() {
            self.received += 1;
            *slot = Some(chunk.body);
        }
        if self.received < self.total {
            return Ok(None);
        }
        let mut out = Vec::new();
        for part in self.parts.drain(..) {
            out.extend_from_slice(&part.ok_or_else(|| Error::Wire("missing chunk".into()))?);
        }
        let total = self.total;
        self.total = 0;
        self.received = 0;
        self.parts = Vec::new();
        debug_assert!(!out.is_empty() || total == 0);
        Ok(Some(out))
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// How many chunks are still missing.
    pub fn missing(&self) -> u32 {
        self.total.saturating_sub(self.received)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_and_reassembles() {
        let payload: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let chunks = split(&payload, 64);
        assert_eq!(chunks.len(), 16);
        let mut reassembler = Reassembler::new();
        let mut done = None;
        for chunk in chunks {
            done = reassembler.push(chunk).unwrap();
        }
        assert_eq!(done.unwrap(), payload);
    }

    #[test]
    fn out_of_order_and_duplicate_chunks() {
        let payload: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let chunks = split(&payload, 32);
        let mut reversed: Vec<Chunk> = chunks.clone();
        reversed.reverse();
        let mut reassembler = Reassembler::new();
        let mut done = None;
        for chunk in reversed {
            done = reassembler.push(chunk).unwrap();
        }
        assert_eq!(done.unwrap(), payload, "order does not matter");

        // A duplicate must not complete a set that is still missing a chunk.
        let mut reassembler = Reassembler::new();
        assert!(reassembler.push(chunks[0].clone()).unwrap().is_none());
        assert!(reassembler.push(chunks[0].clone()).unwrap().is_none());
        assert_eq!(reassembler.missing(), (chunks.len() - 1) as u32);
        let mut done = None;
        for chunk in chunks.into_iter().skip(1) {
            done = reassembler.push(chunk).unwrap();
        }
        assert_eq!(done.unwrap(), payload);
    }

    #[test]
    fn empty_payload_produces_no_chunks() {
        assert!(split(&[], 128).is_empty());
    }

    #[test]
    fn messages_round_trip() {
        let message = Message::Updates {
            note: "n1".into(),
            chunk: Chunk {
                index: 0,
                total: 1,
                body: vec![1, 2, 3],
            },
        };
        assert_eq!(
            Message::decode(&message.encode().unwrap()).unwrap(),
            message
        );
        assert!(Message::decode(b"garbage")
            .unwrap_err()
            .to_string()
            .contains("wire"));
    }
}
