#!/usr/bin/env bash
# Run under heavy.sh: these checks build binaries and launch real browsers.
set -u
logs=${STORAGE_VALIDATION_LOGS:-qa/storage_mainthread/results/validation}
mkdir -p "$logs"
: > "$logs/status.tsv"
failed=0
run_check() {
    local name=$1
    shift
    printf '%s start %s\n' "$(date -u +%FT%TZ)" "$name"
    timeout 3600 "$@" > "$logs/$name.log" 2>&1
    local status=$?
    printf '%s\t%s\n' "$name" "$status" >> "$logs/status.tsv"
    printf '%s end %s: %s\n' "$(date -u +%FT%TZ)" "$name" "$status"
    if [ "$status" -ne 0 ]; then
        failed=1
        tail -40 "$logs/$name.log"
    fi
}
run_check fmt just fmt
run_check ci just ci
run_check named-final bash qa/storage_mainthread/build.sh "${STORAGE_NAMED_LABEL:-final-named}"
run_check wasm-check cargo check -p mistlib-wasm --target wasm32-unknown-unknown --locked
run_check wasm-check-tests cargo check -p mistlib-wasm --target wasm32-unknown-unknown --tests --locked
run_check wasm-adapter-host cargo test -p mistlib-wasm --test storage_resolver --locked
run_check wasm-node wasm-pack test --node mistlib-wasm
run_check wasm-browser node qa/wasm_tests.mjs
run_check package python3 -m unittest discover -s qa -p test_package.py -v
run_check harness node --test qa/storage_mainthread/metrics.test.mjs qa/storage_mainthread/owned.test.mjs
run_check diff git diff --check
exit "$failed"
