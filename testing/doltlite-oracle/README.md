# doltlite-oracle — differential harness vs doltlite

Runs the same scenario on `build/doltlite` (C, prolly-tree) and on Turso
`extensions/versioning` (Rust, overlay) and diffs normalized output.

## Buckets (mirror doltlite oracle-buckets)

- `refs-workspace` — branches, tags, checkout, detached, add/status/reset/clean
- `diff-history-data` — diff, history, at, blame, patch, schemas
- `merge-replay-schema` — merge, conflicts, constraints, cherry-pick, revert, rebase
- `feature-interaction` — savepoints, cross-op conflicts, rowid, triggers
- `remotes-recovery` — remotes, lazy source, gc, compat

Every `vc_oracle_*` scenario must appear in exactly one bucket; `check_buckets.sh`
guards the total (like doltlite `check_oracle_buckets.sh`).

## Run

```bash
# build both sides
./configure && make -C build doltlite        # doltlite binary
cargo build -p turso_versioning

# deterministic sqltests (preferred)
make -C sqlite/conformance run-rust ARGS='--snapshot-filter __never__'

# differential sweep (10k fixed seeds, like doltlite PR)
cargo run -p doltlite-oracle -- --seeds 1:10000 --groups all

# nightly 100k random + scale
cargo run -p doltlite-oracle -- --random 100000 --groups all
```

Output is normalized `|`-separated, `NULL` literal, sorted where `unordered`,
hashes replaced with `<HASH>` unless testing `hashof` determinism.
