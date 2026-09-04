# doltlite-oracle — differential harness vs DoltLite

The harness feeds each checked-in SQL scenario to both `tursodb` and the
DoltLite shell, then compares normalized output and errors. It does not use
golden output, so a scenario must be valid for both engines.

## Buckets

- `refs-workspace` — branches, tags, checkout, and workspace state
- `diff-history-data` — row diffs and commit history
- `merge-replay-schema` — merges, replay, and schema changes
- `feature-interaction` — savepoints and combined features
- `remotes-recovery` — reserved for O5-owned scenarios; O6 leaves its manifest empty

Every scenario is listed exactly once in its bucket's `manifest.txt`. The
`check-buckets` action checks that manifests do not escape their bucket and
that every O6-owned bucket is populated.

## Run in the pinned environment

Set `DOLTLITE_BIN` to the reference binary and `TURSODB_BIN` to the debug
Turso shell. The runner defaults to `target/debug/tursodb` and `doltlite` when
the variables are absent.

```bash
nix develop -c cargo run -p doltlite_oracle -- check-buckets
nix develop -c cargo run -p doltlite_oracle -- run --batch
nix develop -c cargo run -p doltlite_oracle -- run --filter branch_tags
nix develop -c cargo run -p doltlite_oracle -- run --time --filter branch_tags --reps 30
```

For the reference build used by O6:

```bash
nix develop -c sh -c \
  'DOLTLITE_BIN=/private/tmp/doltlite-research/build/doltlite \
   TURSODB_BIN=$PWD/target/debug/tursodb \
   cargo run -p doltlite_oracle -- run --batch'
```

The deterministic generated sweep accepts an inclusive `START:END` range.
This is useful for the 10,000-seed pass without adding generated files to the
corpus:

```bash
nix develop -c sh -c \
  'DOLTLITE_BIN=/private/tmp/doltlite-research/build/doltlite \
   TURSODB_BIN=$PWD/target/debug/tursodb \
   cargo run -p doltlite_oracle -- run --seeds 1:10000'
```

The runner prepends `.headers off` and `.mode list`, folds engine-specific
hashes/booleans/error decoration, and prints a structured line diff on the
first mismatch. Timing reports the median process wall time over `--reps`
interleaved repetitions; it fails if the compared outputs differ.
