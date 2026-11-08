# Architecture

The engine is three crates with one rule between them: **the document is the
only truth, everything else is derived.**

```text
                ┌──────────────────────────────────────────────┐
   editor/UI ──▶│ notebase            (pure, no IO)            │
                │   NoteDoc: block tree + rich text + versions │
                └───────────────┬──────────────────────────────┘
                                │ vv / updates_since / snapshot / import
              ┌─────────────────┴───────────────────┐
              ▼                                     ▼
   ┌────────────────────────┐            ┌──────────────────────────┐
   │ notebase-store         │            │ notebase-sync            │
   │  oplog · snapshots     │            │  Hello/Updates/Ack       │
   │  blobs · FTS5          │            │  chunking · sessions     │
   └────────────────────────┘            └──────────────────────────┘
```

## Invariants

1. **Durability never waits on derivation.** A save appends an encoded CRDT
   delta and updates one row. Re-tokenising the body for search happens in
   `index()`, on idle or on close. Measured: 0.40 ms and 126 bytes per save for a
   202 KB note.
2. **Derived data can be thrown away.** `notes.title`, `notes.plain`,
   `notes_fts` and every statistic are recomputable from the oplog + snapshots.
3. **History is metadata.** `note_edits` keeps one tiny row per save forever, so
   "how much has this note been worked on" survives compaction. Bodies are what
   gets folded away.
4. **Equality of versions is semantic.** Two encodings of the same version vector
   can differ byte-for-byte; comparing raw bytes produces phantom "changed"
   results and phantom merges. Always compare decoded vectors
   (`NoteDoc::same_version`, `covers`).
5. **One note, one document, one delta.** Peers exchange per-note version
   vectors, so syncing note A never ships note B, and a note that did not change
   costs zero bytes.
6. **Peer ids are stable per device and stored in the vault**, never generated
   per document, or two devices look like one author.
7. **Container layout is a format.** Adding a block kind is additive; renaming a
   container is a migration and bumps `schema::LAYOUT`.

## Data model

One `LoroDoc` per note. Containers:

| container | type | contents |
| --- | --- | --- |
| `meta` | map | `id`, `created_at`, `updated_at`, `layout` |
| `title` | text | the note title (CRDT text, so two typos converge) |
| `blocks` | tree | the block forest; child order is reading order |

Each tree node carries a metadata map with `kind`, a mergeable `text` child
container, and the properties its kind needs (`checked`, `lang`, `src`, `alt`).
`text` uses Loro's mergeable-container API so a peer that creates the container
concurrently converges instead of forking a second one.

Block kinds are the Notion vocabulary, cut to what a text note needs:
`paragraph`, `heading_1..3`, `bulleted_list_item`, `numbered_list_item`, `todo`,
`quote`, `code`, `divider`, `image`.

Rich text is a Quill-style delta: an ordered list of `(text, marks)` spans with
`bold`, `italic`, `strike`, `code`, `link`.

Markdown is bidirectional but not byte-exact: rendering covers the whole model,
parsing covers the same subset (ATX headings, lists, todos, quotes, fences,
images, dividers, inline marks). Unmatched markers stay literal. A document that
must survive exactly is a document, not markdown — which is why the truth is the
CRDT, not the file.

## Storage (SQLite)

| table | job |
| --- | --- |
| `vault_meta` | device peer id, future settings |
| `notes` | one row per note: vv, `seq_hi`, projection + `indexed` flag |
| `note_updates` | append-only oplog, `(note_id, seq)`, zstd, `seq` monotonic forever |
| `note_snapshots` | full or state-only snapshots, pruned to `keep_snapshots` |
| `note_edits` | one row per save (`ts`, `chars`) — survives compaction |
| `blobs` | BLAKE3-addressed attachments, zstd, deduplicated |
| `notes_fts` | FTS5 **external content** over `notes`, trigram tokenizer |

Details that are load-bearing:

* **Sequence numbers never reset.** Allocating `MAX(seq)+1` after trimming
  silently reuses numbers a snapshot already claims to cover; `notes.seq_hi` is
  the allocator.
* **FTS5 external content is maintained explicitly**, guarded by `notes.indexed`.
  Deleting a row that was never inserted corrupts the index; the flag is what
  makes `index()` idempotent.
* **No FTS triggers.** A trigger on `notes` would re-index the whole body on
  every save — the exact cost this design removes.
* **Trigram for CJK, LIKE for 1–2 characters.** A single CJK character is a whole
  word; correctness first, and a personal vault is small.
* **WAL + `auto_vacuum=INCREMENTAL`**, `vacuum()` after compaction so the file
  actually shrinks.

## Write path

```text
edit (one keystroke)        NoteDoc::insert_block_text   → one CRDT op
save                        vv compare (semantic)       → nothing if unchanged
                            updates_since(stored vv)    → bytes to append
                            append_update               → oplog row + seq_hi++
                            record_edit                 → one tiny history row
                            upsert_note                 → vv, timestamps (no FTS)
                            maybe_snapshot              → every N updates
index (idle / close)        index_note                  → projection + FTS row
compact (background)        state_only snapshot         → trim log, prune snapshots
```

A save with no edits writes nothing at all: the version vector is compared
before any bytes are produced.

## Snapshots, compaction, time travel

Three payload shapes, three different promises:

| mode | contains | use |
| --- | --- | --- |
| `Snapshot` | state + full history | export, debugging, restore everything |
| `StateOnly` | state + minimal history | non-syncing vaults only (vv understates content) |
| `ShallowSnapshot(vv)` | state + history since `vv` | keep a bounded window of the past |

`truncate_history` defaults to **false**, and that is deliberate: a state-only
payload carries only the ops of the target version, so the loaded document's
version vector understates what it holds — a syncing peer would then hear "I
have nothing" and never receive the content. Fold the oplog into full snapshots
instead; the log above the newest snapshot is still trimmed, so growth stays
bounded. `truncate_history: true` is for vaults that never sync, and it costs
both time travel and sync provenance.

Consequence, stated plainly: time travel reaches back `keep_snapshots` deep. Keeping every byte of history forever
is possible (`keep_snapshots` high, snapshots full) but it is a choice, not a
default — the same trade-off Joplin makes with `revisionService.ttlDays` and
Loro with shallow snapshots.

The oplog is folded on two triggers, whichever comes first: 64 updates or 1 MiB
of log above the newest snapshot. A note with a few enormous pastes must not be
able to bloat the log just because the update count stayed low.

A state-only snapshot can only attach to a document that has not already
committed its own operations, so loading is always *import first, create missing
containers second* (`NoteDoc::load`). Getting that order wrong silently yields an
empty note.

## Sync

```text
A → B   Hello  { note, vv }
B → A   Updates{ chunk i/n } …          (empty when A is already current)
A → B   Ack    { vv }
```

* Missing updates come from `export(updates since peer vv)`; an empty result is
  "you are current", not an empty frame.
* Chunks are `(index, total, body)`; the reassembler accepts out-of-order and
  duplicate frames and rejects a set whose size changes mid-stream.
* Sessions adopt the note id of the first message, so a receiver can open blind
  (HTTP `POST /sync/:id`, a fresh device) and still land the note.
* Idempotent by construction: importing the same update twice changes nothing,
  because CRDT import is a set union.
* Transports: anything that can carry `Vec<u8>`. The pull form
  (`updates_for_peer`) exists for request/response transports.

## Host bindings

`notebase-ffi` is a UniFFI surface over the same objects, not a second API:
`VaultHandle` wraps `Vault`, `NoteHandle` wraps `NoteDoc`, and every method is a
direct call. Rules that keep it honest:

* typed records and errors everywhere; **JSON only where the host already speaks
  JSON** (the block tree a web view renders);
* `save` (durability) and `index` (search projection) stay two calls in the host
  too, for the same reason they are two calls here;
* block ids cross the boundary as `"peer@counter"` strings — the same identity
  the UI already uses for a caret or a drag handle;
* mutating methods take `&self`: the host may hold one handle from several
  threads, which is exactly what a UI + a background saver need.

```bash
./scripts/gen-bindings.sh swift build/bindings
```

Verified on macOS by compiling a Swift program against the generated module and
calling into the engine (create → edit → save → search → reload).

## Prior art, explicitly

Every mechanism here is borrowed; nothing is invented for its own sake.

| mechanism | where it comes from |
| --- | --- |
| block tree as the unit of storage | Notion's block model |
| delta-shaped rich text with marks | Quill delta, ProseMirror marks |
| CRDT text/tree, state-only and shallow snapshots | Loro (`ExportMode`), Yjs (`mergeUpdates`, GC) |
| per-note document + version-vector handshake | automerge-repo, y-protocols |
| append-only log + periodic snapshots + pruning | write-ahead logs, Joplin `revisions` with TTL |
| content-addressed blob store | Git, restic |
| FTS5 external content index | SQLite FTS5 §4.4.3 |
| zstd payloads, BLAKE3 addresses | standard practice |
| one tiny row per edit session | Joplin `revisionService.intervalBetweenRevisions` / Obsidian file history, reduced to metadata |
| UniFFI object/record surface over a native core | Mozilla application-services, uniffi examples |

## Non-goals (for now)

Server-side collaboration, presence, comments, permissions, encryption at rest,
and a rich media pipeline. The engine's job is that a note never gets lost, never
gets slower as history grows, and syncs without a coordinator. Everything else is
an application.
