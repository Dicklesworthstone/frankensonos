#!/bin/sh
# Run the FrankenSonos e2e scenarios (the real `fsonos` binary against
# fsonos-sim) and point at their logs. Extra arguments go to `cargo test`,
# e.g. `scripts/e2e.sh harness_self_test`.
#
# Logs: target/e2e-logs/<scenario>/<epoch-ms>/ (steps.jsonl, summary.json,
# soap.log, gena.log), or under $FSONOS_E2E_LOG_DIR. Under rch the logs stay
# on the worker; the per-scenario summary lines below are the record.
set -eu
cd "$(dirname "$0")/.."
exec cargo test -p fsonos-cli --test e2e_sim -- --nocapture --test-threads=1 "$@"
