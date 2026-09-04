#!/usr/bin/env bash
set -euo pipefail

# Keep one entry point for local use; normalization and process handling live
# in the Rust runner so this wrapper cannot silently compare different inputs.
exec cargo run -q -p doltlite_oracle -- run "$@"
