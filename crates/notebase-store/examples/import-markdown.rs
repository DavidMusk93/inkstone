//! Import a directory of markdown files into a vault.
//!
//! ```text
//! cargo run --release -p notebase-store --example import-markdown -- \
//!   --in notes/ --out notes.db [--limit 20]
//! ```
//!
//! * every `*.md` (recursively) becomes one note;
//! * the file's mtime becomes the note's `updated_at`, so an import does not
//!   rewrite history;
//! * re-running it imports nothing twice.
//!
//! The importer is deliberately file-based: a vault is built from text, not from
//! another product's schema.

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use notebase_store::Vault;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = PathBuf::new();
    let mut out = PathBuf::from("notebase.db");
    let mut limit: Option<usize> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--in" => input = PathBuf::from(args.next().ok_or("--in needs a directory")?),
            "--out" => out = PathBuf::from(args.next().ok_or("--out needs a path")?),
            "--limit" => limit = Some(args.next().ok_or("--limit needs a number")?.parse()?),
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    if !input.is_dir() {
        return Err(format!("{} is not a directory", input.display()).into());
    }

    let mut files = Vec::new();
    collect(&input, &mut files)?;
    files.sort();
    if let Some(limit) = limit {
        files.truncate(limit);
    }

    let started = Instant::now();
    let vault = Vault::open(&out)?;
    let (mut imported, mut skipped, mut failed, mut bytes) = (0usize, 0usize, 0usize, 0usize);

    for file in &files {
        let markdown = std::fs::read_to_string(file)?;
        if markdown.trim().is_empty() {
            skipped += 1;
            continue;
        }
        bytes += markdown.len();
        let id = note_id(file);
        let mtime = file
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        match vault.import_markdown_as(&id, &markdown, mtime, mtime) {
            Ok(Some(_)) => imported += 1,
            Ok(None) => skipped += 1,
            Err(err) => {
                failed += 1;
                eprintln!("{}: {err}", file.display());
            }
        }
    }

    let stats = vault.stats()?;
    println!(
        "imported {imported}, skipped {skipped}, failed {failed} in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    println!(
        "content {:.1} KB · vault {} notes · {:.2} MB",
        bytes as f64 / 1024.0,
        stats.notes,
        std::fs::metadata(&out)?.len() as f64 / (1024.0 * 1024.0)
    );
    if failed > 0 {
        return Err(format!("{failed} file(s) failed").into());
    }
    Ok(())
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
}

/// Note id from the file path: stable across machines, readable in the vault.
fn note_id(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("note");
    let mut id: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if id.is_empty() {
        id.push_str("note");
    }
    id
}
