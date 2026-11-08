//! Export every note as markdown — the rollback artifact before any destructive
//! cleanup of a legacy store.
//!
//! ```text
//! cargo run -q -p notebase-store --example export-markdown -- --vault x.db --out dir
//! ```
//!
//! Writes `<note-id>.md` per note plus `MANIFEST.tsv` (`id<TAB>title<TAB>bytes`),
//! so a restore is a directory walk and nothing has to parse JSON to be readable.

use std::io::Write;
use std::path::PathBuf;

use notebase_store::Vault;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut vault_path = PathBuf::new();
    let mut out_dir = PathBuf::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--vault" => vault_path = PathBuf::from(args.next().ok_or("--vault needs a path")?),
            "--out" => out_dir = PathBuf::from(args.next().ok_or("--out needs a dir")?),
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    std::fs::create_dir_all(&out_dir)?;

    let vault = Vault::open(&vault_path)?;
    let notes = vault.list(usize::MAX, 0)?;
    let mut manifest = std::fs::File::create(out_dir.join("MANIFEST.tsv"))?;
    writeln!(manifest, "id\ttitle\tbytes\tedits")?;

    let mut bytes = 0usize;
    let mut written = 0usize;
    for summary in &notes {
        let Some(note) = vault.load(&summary.id)? else {
            continue;
        };
        let markdown = note.to_markdown();
        bytes += markdown.len();
        written += 1;
        std::fs::write(out_dir.join(format!("{}.md", summary.id)), &markdown)?;
        writeln!(
            manifest,
            "{}\t{}\t{}\t{}",
            summary.id,
            summary.title.replace('\t', " "),
            markdown.len(),
            summary.saves
        )?;
    }
    println!(
        "exported {written}/{} note(s), {:.1} KB of markdown -> {}",
        notes.len(),
        bytes as f64 / 1024.0,
        out_dir.display()
    );
    Ok(())
}
