#!/usr/bin/env bash
set -euo pipefail
# Minimal differential driver: runs a .sql scenario on both engines and diffs.
# Usage: ./run.sh scenario.sql
SCENARIO="${1:-}"
if [[ -z "$SCENARIO" ]]; then
  echo "usage: $0 <scenario.sql>"
  exit 1
fi
TURSO_CLI="${TURSO_CLI:-./target/debug/tursodb}"
DOLTLITE_CLI="${DOLTLITE_CLI:-./build/doltlite}"
normalize() { sed -E 's/[0-9a-f]{40}/<HASH>/g' | sort; }
echo "== turso =="
"$TURSO_CLI" :memory: < "$SCENARIO" | normalize > /tmp/turso.out
echo "== doltlite =="
"$DOLTLITE_CLI" :memory: < "$SCENARIO" | normalize > /tmp/doltlite.out
diff -u /tmp/doltlite.out /tmp/turso.out && echo "PASS: $SCENARIO" || echo "FAIL: $SCENARIO"
