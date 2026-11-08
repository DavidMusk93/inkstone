//! A transport-less peer: prints the opaque messages a host would put on the
//! wire, so a shell script (or curl) can be the network.
//!
//! ```text
//! # seed a peer vault with an old body under a known note id
//! sync-peer seed   --vault /tmp/peer.db --id <note> --markdown old.md
//! # what the peer has
//! sync-peer hello  --vault /tmp/peer.db --note <note>
//! # apply one message from the other side, print the replies
//! sync-peer receive --vault /tmp/peer.db --note <note> --message <base64>
//! ```
//!
//! Every message is base64 on stdout, one per line.

use base64::{engine::general_purpose::STANDARD, Engine};
use notebase_store::Vault;
use notebase_sync::{Message, SyncConfig, VaultPeer};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or("usage: seed|edit|hello|receive")?;
    let mut vault_path = String::new();
    let mut note = String::new();
    let mut id = String::new();
    let mut markdown_file = String::new();
    let mut message = String::new();
    let mut text = String::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--vault" => vault_path = args.next().ok_or("--vault needs a path")?,
            "--note" => note = args.next().ok_or("--note needs an id")?,
            "--id" => id = args.next().ok_or("--id needs an id")?,
            "--markdown" => markdown_file = args.next().ok_or("--markdown needs a path")?,
            "--message" => message = args.next().ok_or("--message needs base64")?,
            "--text" => text = args.next().ok_or("--text needs a string")?,
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let vault = Vault::open(&vault_path)?;

    match command.as_str() {
        "seed" => {
            let markdown = std::fs::read_to_string(&markdown_file)?;
            match vault.import_markdown_as(&id, &markdown, now_ms(), now_ms())? {
                Some(note) => println!("seeded {} ({})", note.id(), note.blocks().len()),
                None => println!("already present {}", id),
            }
        }
        "edit" => {
            // A peer edit: append a paragraph, then save. Nothing else changes.
            let note = vault.load(&note)?.ok_or("note not found")?;
            note.append_block(None, notebase::BlockKind::Paragraph, &text)?;
            vault.save_and_index(&note)?;
            println!("edited {}", note.id());
        }
        "hello" => {
            let peer = VaultPeer::open(vault, &note, SyncConfig::default())?;
            println!("{}", STANDARD.encode(peer.hello().encode()?));
        }
        "receive" => {
            let mut peer = VaultPeer::open(vault, &note, SyncConfig::default())?;
            let bytes = STANDARD.decode(message.trim())?;
            let step = peer.receive(Message::decode(&bytes)?)?;
            println!("changed={} converged={}", step.changed, step.converged);
            for out in step.outbound {
                println!("{}", STANDARD.encode(out.encode()?));
            }
        }
        other => return Err(format!("unknown command {other}").into()),
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
