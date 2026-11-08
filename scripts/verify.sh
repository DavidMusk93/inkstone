#!/usr/bin/env bash
# Self-contained acceptance run: build, test, import, verify, scale.
#
#   ./scripts/verify.sh [work-dir]
#
# Nothing outside this repository is read: the fixture notes are generated here,
# imported, then verified (counts, content, timestamps, search, a VACUUM INTO
# backup and restore drill, and compaction).
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"

WORK="${1:-${TMPDIR:-/tmp}/notebase-verify}"
mkdir -p "$WORK"
FIXTURE="$WORK/fixture"
VAULT="$WORK/notebase.db"
rm -rf "$FIXTURE" "$VAULT" "$VAULT-wal" "$VAULT-shm"

echo "[1/4] cargo test --all"
cargo test --all -q

echo "[2/4] generate a fixture and import it"
mkdir -p "$FIXTURE/nested"
for i in 1 2 3 4 5; do
  cat > "$FIXTURE/note-$i.md" <<MD
# Fixture note $i

Body of note $i with a list:

- one
- two

\`\`\`rust
fn main() { println!("$i"); }
\`\`\`
MD
done
cat > "$FIXTURE/nested/cjk.md" <<'MD'
# 中文笔记

这是一篇用来验证检索的中文笔记：数据、笔记本、同步。

- 列表一
- 列表二
MD
cargo run --release -q -p notebase-store --example import-markdown -- --in "$FIXTURE" --out "$VAULT"

echo "[3/4] verify"
cargo run --release -q -p notebase-store --example verify-vault -- \
  --vault "$VAULT" --work "$WORK/verify" --expect-notes 6

echo "[4/4] scale example (cost per save)"
cargo run --release -q -p notebase-store --example scale

echo "VERIFY OK  vault=$VAULT"
