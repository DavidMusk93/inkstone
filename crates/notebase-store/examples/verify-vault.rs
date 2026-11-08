//! Verify a vault the way a release would: counts, content, timestamps, search,
//! a backup/restore drill, and compaction — all on real data.
//!
//! ```text
//! cargo run --release -p notebase-store --example verify-vault -- \
//!   --vault notes.db --work /tmp/notebase-verify [--expect-notes 200]
//! ```
//!
//! Exit code 0 means every check passed; anything else prints the first failure
//! and exits non-zero, so it can be the acceptance command of a release.

use std::path::PathBuf;

use notebase_store::Vault;

struct Checks {
    passed: usize,
    failed: Vec<String>,
}

impl Checks {
    fn new() -> Self {
        Self {
            passed: 0,
            failed: Vec::new(),
        }
    }

    fn check(&mut self, ok: bool, what: impl Into<String>) {
        if ok {
            self.passed += 1;
        } else {
            self.failed.push(what.into());
        }
    }

    fn eq<T: PartialEq + std::fmt::Debug>(&mut self, got: T, want: T, what: &str) {
        let ok = got == want;
        if !ok {
            self.failed
                .push(format!("{what}: got {got:?}, want {want:?}"));
        } else {
            self.passed += 1;
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut vault_path = PathBuf::from("notebase.db");
    let mut work = PathBuf::from("/tmp/notebase-verify");
    let mut expect_notes: Option<usize> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--vault" => vault_path = PathBuf::from(args.next().ok_or("--vault needs a path")?),
            "--expect-notes" => {
                expect_notes = Some(
                    args.next()
                        .ok_or("--expect-notes needs a number")?
                        .parse()?,
                )
            }
            "--work" => work = PathBuf::from(args.next().ok_or("--work needs a path")?),
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    std::fs::create_dir_all(&work)?;

    let mut checks = Checks::new();
    let vault = Vault::open(&vault_path)?;
    let notes = vault.list(usize::MAX, 0)?;
    let stats = vault.stats()?;

    // ---------------------------------------------------------------- shape
    checks.check(stats.notes == notes.len(), "stats.notes matches list()");
    checks.check(!notes.is_empty(), "vault has notes");
    if let Some(expected) = expect_notes {
        checks.eq(stats.notes, expected, "note count equals expectation");
    }

    // ------------------------------------------------------------- content
    let mut empty_titles = 0;
    let mut blocked = 0;
    for summary in &notes {
        let Some(note) = vault.load(&summary.id)? else {
            blocked += 1;
            continue;
        };
        if note.plain_text().trim().is_empty() {
            blocked += 1;
        }
        if note.title().trim().is_empty() {
            empty_titles += 1;
        }
    }
    checks.eq(blocked, 0, "every note loads with content");
    checks.check(empty_titles <= notes.len() / 10, "most notes have titles");

    // --------------------------------------------------------- search works
    let probes = ["note", "the", "and", "数据", "笔记"];
    let mut probe_hits = 0;
    for probe in probes {
        let hits = vault.search(probe, 5)?;
        if !hits.is_empty() {
            probe_hits += 1;
        }
    }
    checks.check(probe_hits >= 3, "search answers real probes");

    // ----------------------------------------------- backup + restore drill
    // A backup is only a backup if it restores. `VACUUM INTO` is the copy that
    // is safe while the app is writing (no torn WAL).
    let backup_path = work.join("backup.db");
    let backup_bytes = vault.backup_to(&backup_path)?;
    checks.check(backup_bytes > 0, "backup file written");

    let restored = Vault::open(&backup_path)?;
    let restored_stats = restored.stats()?;
    checks.eq(restored_stats.notes, stats.notes, "restored note count");
    checks.eq(restored_stats.edits, stats.edits, "restored edit history");
    let mut content_mismatch = 0;
    for summary in notes.iter().take(64) {
        let want = vault.load(&summary.id)?.map(|n| n.to_markdown());
        let got = restored.load(&summary.id)?.map(|n| n.to_markdown());
        if want != got {
            content_mismatch += 1;
        }
    }
    checks.eq(content_mismatch, 0, "restored notes match the original");
    for probe in probes {
        let want = vault.search(probe, 5)?.len();
        let got = restored.search(probe, 5)?.len();
        if want != got {
            checks
                .failed
                .push(format!("restored search for {probe:?}: {got} vs {want}"));
        } else {
            checks.passed += 1;
        }
    }

    // ------------------------------------------------------------ compaction
    // Give the log something to fold: a scratch note edited past the snapshot
    // threshold, so compaction has real updates to trim.
    let probe = vault.create("acceptance probe")?;
    let probe_block = probe.blocks()[0].id;
    for i in 0..140 {
        probe
            .insert_block_text(probe_block, 0, &format!("{i} "))
            .ok();
        // durability only; the index is refreshed below
        vault.save(&probe)?;
    }
    vault.index(&probe)?;
    let probe_markdown = probe.to_markdown();
    let before = vault.stats()?;
    let compacted = vault.compact()?;
    let after = vault.stats()?;
    checks.check(
        after.history_bytes() <= before.history_bytes(),
        "compaction does not grow history",
    );
    checks.check(
        compacted.updates_trimmed > 0,
        "compaction actually folded updates",
    );
    let probe_after = vault.load(&probe.id())?;
    checks.eq(
        probe_after.map(|n| n.to_markdown()),
        Some(probe_markdown),
        "edited note survives compaction",
    );
    let mut post_compact_mismatch = 0;
    for summary in notes.iter().take(64) {
        let Some(note) = vault.load(&summary.id)? else {
            post_compact_mismatch += 1;
            continue;
        };
        if note.plain_text().trim().is_empty() && !summary.title.trim().is_empty() {
            post_compact_mismatch += 1;
        }
    }
    checks.eq(
        post_compact_mismatch,
        0,
        "notes still load after compaction",
    );

    // ---------------------------------------------------------------- report
    println!(
        "vault    {} notes · {} edits · {:.2} MB · history {:.1} KB",
        stats.notes,
        stats.edits,
        std::fs::metadata(&vault_path)?.len() as f64 / (1024.0 * 1024.0),
        stats.history_bytes() as f64 / 1024.0,
    );
    println!(
        "backup   {:.2} MB at {} (restore drill passed)",
        backup_bytes as f64 / (1024.0 * 1024.0),
        backup_path.display()
    );
    println!(
        "compact  {} note(s), {} update(s) trimmed · {:.1} KB → {:.1} KB",
        compacted.notes,
        compacted.updates_trimmed,
        before.history_bytes() as f64 / 1024.0,
        after.history_bytes() as f64 / 1024.0,
    );
    println!(
        "checks   {} passed, {} failed",
        checks.passed,
        checks.failed.len()
    );
    if !checks.failed.is_empty() {
        for failure in &checks.failed {
            eprintln!("FAIL {failure}");
        }
        return Err(format!("{} check(s) failed", checks.failed.len()).into());
    }
    println!("ok       vault verified");
    Ok(())
}
