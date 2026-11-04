#!/usr/bin/env bash
set -euo pipefail
label=${1:-head}
wasm-pack build mistlib-wasm --target web --profiling --no-opt \
    --out-dir "../qa/storage_mainthread/artifacts/$label" -- --locked
optimizer=${WASM_OPT:-}
if [ -z "$optimizer" ]; then
    optimizer=$(command -v wasm-opt || true)
fi
if [ -z "$optimizer" ]; then
    for cached in "$HOME"/.cache/.wasm-pack/wasm-opt-*/bin/wasm-opt; do
        if [ -x "$cached" ]; then optimizer=$cached; break; fi
    done
fi
if [ -z "$optimizer" ]; then
    printf 'Set WASM_OPT to the wasm-opt executable.\n' >&2
    exit 1
fi
artifact="qa/storage_mainthread/artifacts/$label/mistlib_wasm_bg.wasm"
"$optimizer" -O -g "$artifact" -o "$artifact.named"
mv "$artifact.named" "$artifact"
node --input-type=module - "$artifact" <<'JS'
import { readFileSync } from 'node:fs';
const module = new WebAssembly.Module(readFileSync(process.argv[2]));
if (!WebAssembly.Module.customSections(module, 'name').length) {
  throw new Error('profiling artifact has no name section');
}
JS
