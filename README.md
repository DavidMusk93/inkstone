# notebase

**A note base**: the storage and sync layer a note app is built on. One CRDT
document per note, a block tree with rich text, an append-only oplog, and a sync
protocol that only moves what the other side is missing.

Markdown is the projection, not the truth. Search, warmth, statistics and word
counts are projections too — delete them and the notes still load.

```text
notebase          the document: block tree, rich text, markdown, versions
notebase-store    SQLite: oplog, snapshots, blobs, FTS5, compaction
notebase-sync     version vectors, chunked deltas, transport-agnostic sessions
notebase-ffi      UniFFI bindings: Swift, Kotlin, Python
```

## Name

`notebase` — notes + base. The crate family reads as what it is:
`notebase` (the document), `notebase-store` (the disk), `notebase-sync` (the
wire), `notebase-ffi` (the bindings).

## Why it exists

Saving a note by writing the whole body again, every time, keeping every copy
forever, is how a personal vault turns into a 500 MB file with 0.2 MB of live
notes in it. This engine is the opposite shape:

* a save appends the **delta**, not the document;
* history is a **log that gets folded**, not an ever-growing pile of copies;
* the **fact** that you edited is kept forever (a 40-byte row), the **bytes** are not;
* sync asks "what do you have?" and answers with the difference;
* two devices editing at once converge without a server and without losing edits.

## Measured, not claimed

`cargo run --release -p notebase-store --example scale` — one 202 KB note,
2 000 keystroke-sized edits, on an M-series Mac:

| | |
|---|---|
| save latency | **0.40 ms** per save |
| bytes written per save | **126 B** |
| same workload, whole-document writes | 394.8 MB |
| vault file after 2 000 edits | **0.70 MB** |
| compaction | 14.0 KB → 11.9 KB of history, 44 ms |

## Quick start

```rust
use notebase::{BlockKind, Mark};
use notebase_store::Vault;

let vault = Vault::open("/tmp/notes.db")?;

let note = vault.create("Weekly review")?;
let intro = note.append_block(None, BlockKind::Paragraph, "What moved this week?")?;
note.append_block(Some(intro), BlockKind::BulletedListItem, "shipped the sync engine")?;
note.mark_block_text(intro, 0..4, Mark::Bold)?;

vault.save_and_index(&note)?;

for hit in vault.search("sync engine", 10)? {
    println!("{} — {}", hit.title, hit.snippet);
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

Editing is CRDT-level, so a web editor, a Swift app and a CLI can all drive it:

```rust
let block = note.blocks()[1].id;
note.insert_block_text(block, 0, "still ")?;      // one keystroke, one operation
vault.save(&note)?;                                // durability, 126 bytes
let past = vault.at_version(&note.id(), &vv)?;     // time travel, when history is kept
```

## Sync

```rust
use notebase_sync::{SyncConfig, SyncSession};

let mut session = SyncSession::new(note.clone(), SyncConfig::default());
let hello = session.hello();                    // "I have this much"
// transport: WebSocket, HTTP, iCloud, a USB stick, a mail attachment
let step = session.receive(their_message)?;     // Step { outbound, changed, converged }
```

* `Hello { note, vv }` → `Updates { chunk }` × n → `Ack { vv }`.
* Payloads are chunked (128 KiB default), reassembled out of order, idempotent.
* One note = one session = one small delta. Editing note A never ships note B.
* No clock trust, no last-writer-wins, no conflict markers.

## Native apps (UniFFI)

```bash
./scripts/gen-bindings.sh swift build/bindings      # or kotlin
```

```swift
let vault = try VaultHandle.open(path: "notes.db")
let note = try vault.create(title: "Weekly review")
let intro = try note.appendBlock(parent: nil, kind: "paragraph", text: "What moved?")
try note.insertText(blockId: intro, pos: 0, text: "→ ")   // one keystroke, one op
try note.markText(blockId: intro, start: 0, end: 1, mark: "bold", value: nil)
_ = try vault.saveAndIndex(note: note)                    // durability + search
let hits = try vault.search(query: "moved", limit: 20)
```

Handles mirror the engine 1:1 (`VaultHandle` ↔ `Vault`, `NoteHandle` ↔ `NoteDoc`).
Typed records for everything, JSON only where a host already speaks JSON (block
trees for a web view). `save` is durability; `index` is the search projection —
an app decides when to pay for the second one.

## What is deliberately not here

No server, no accounts, no UI, no attachments pipeline beyond a content-addressed
blob table, no encryption layer yet. Those belong to the applications built on
top; the engine only owns what has to be exactly right.

## Verify

```bash
./scripts/verify.sh    # cargo test + generated fixture import + 19 checks + scale
```

Backups are `Vault::backup_to(path)`: a `VACUUM INTO` copy, which is the only
kind that is safe while the app is writing. A plain file copy can catch a torn
WAL, and restoring one is how "we have backups" turns into lost notes.

## Status

`0.1.0` — the model, storage, search and sync are implemented and tested
(34 tests: convergence, log folding, CJK search, chunked sync, end-to-end two
vaults). See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the invariants and
the prior art behind every mechanism.

## License

MIT.
