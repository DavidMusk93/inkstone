//! What the engine costs per keystroke, measured instead of claimed.
//!
//! ```text
//! cargo run --release -p notebase-store --example scale
//! ```
//!
//! It writes a long note, keeps editing it, and reports how many bytes actually
//! reach the disk — the number a "save the whole document" design cannot match.

use std::time::Instant;

use notebase_store::Vault;

fn now() -> Instant {
    Instant::now()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!("notebase-scale-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("scale.db");
    let _ = std::fs::remove_file(&path);

    let vault = Vault::open(&path)?;
    let note = vault.create("Scale")?;
    let block = note.blocks()[0].id;

    // A long note: ~200 KB of prose.
    let paragraph = "The quick brown fox jumps over the lazy dog. ".repeat(4_600);
    let t0 = now();
    note.insert_block_text(block, 0, &paragraph)?;
    vault.save_and_index(&note)?;
    let seed_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let seed_bytes = paragraph.len();

    // 2,000 keystroke-sized edits at the front of the note.
    const EDITS: usize = 2_000;
    let mut appended = 0usize;
    let t0 = now();
    for i in 0..EDITS {
        note.insert_block_text(block, 0, &format!("[{i}]"))?;
        appended += vault.save(&note)?.bytes;
    }
    let edit_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let loaded = vault.load(&note.id())?.expect("note reloads");
    assert_eq!(
        loaded.blocks()[0].plain().chars().count(),
        note.blocks()[0].plain().chars().count()
    );

    let before = vault.stats()?;
    let t0 = now();
    let compacted = vault.compact()?;
    let compact_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let after = vault.stats()?;

    println!("note body            {:.1} KB", seed_bytes as f64 / 1024.0);
    println!("seed save            {seed_ms:.1} ms");
    println!(
        "incremental edits    {EDITS} saves in {edit_ms:.0} ms ({:.2} ms/save)",
        edit_ms / EDITS as f64
    );
    println!(
        "bytes on disk        {:.1} KB appended for {EDITS} edits = {:.0} B per save",
        appended as f64 / 1024.0,
        appended as f64 / EDITS as f64
    );
    println!(
        "whole-document write would have been {:.1} MB",
        (seed_bytes * EDITS) as f64 / (1024.0 * 1024.0)
    );
    println!(
        "history before fold  {:.1} KB in {} updates / {} snapshots",
        before.history_bytes() as f64 / 1024.0,
        before.updates,
        before.snapshots
    );
    println!(
        "history after fold   {:.1} KB in {} updates / {} snapshots ({compact_ms:.0} ms)",
        after.history_bytes() as f64 / 1024.0,
        after.updates,
        after.snapshots
    );
    println!(
        "note file            {:.2} MB",
        std::fs::metadata(&path)?.len() as f64 / (1024.0 * 1024.0)
    );
    println!(
        "compacted            {} notes, {} updates trimmed",
        compacted.notes, compacted.updates_trimmed
    );
    let _ = std::fs::remove_file(&path);
    Ok(())
}
