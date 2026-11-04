# Storage main-thread measurement

Research harness created 2026-11-04. Run from the repository root. Requires the
pinned `qa/node_modules` Playwright installation, its Chromium, wasm-pack, and
Linux `flock`. There is no external signaling: the relay and HTTP fixture bind
loopback, `iceServers` is empty, and page requests outside the fixture are blocked.
Each peer runs in its own browser process with a fresh context/OPFS.

```sh
just qa storage-mainthread-build head
just qa storage-mainthread --package old="$OLD_PACKAGE" \
  --package head=qa/storage_mainthread/artifacts/head \
  --rates 1,4 --repeats 4 --out qa/storage_mainthread/results/baseline.jsonl
just qa storage-mainthread-report qa/storage_mainthread/results/baseline.jsonl
```

`OLD_PACKAGE` points to the unpacked public 0.7.1 npm artifact, **never a rebuilt
0.7.1**. Build candidates into a distinct artifact directory; do not overwrite
baseline artifacts. Both recipes acquire `/tmp/mistlib-heavy.lock` with a 900 s
wait, retrying acquisition up to eight times. No release/control files are used.
Exit code 75 is reserved for lock acquisition timeout. Do not run builds or other
measurements concurrently, including builds used by test suites.

The default operation is `storage_add_pinned` followed by a remote `storage_get`
of 20 MiB of random data. `--mode add-pin` instead exercises `storage_add` followed
by `storage_pin`. `--mixed` runs both directions between exactly two package
arguments. `--rates 1,4`, `--repeats 4`, `--bytes 20971520`, and `--timeout 180`
are configurable. Package order alternates AB/BA between repetitions. A transfer
has its own deadline; each attempt additionally has a hard process-group deadline
of transfer timeout plus 90 s. Failed attempts remain in JSONL and make the command
exit unsuccessfully; they are not silently retried or included as successes.

`--mode add-owned` uses `storage_add_pinned_owned`; `--mode add-owned-pin` uses
`storage_add_owned` followed by `storage_pin`. Both assert synchronous detachment.
`--package-mode label=add-owned` overrides the mode for one sender package, allowing
AB/BA comparisons with the snapshot API and mixed runs with old receivers:

```sh
just qa storage-mainthread --package old="$OLD_PACKAGE" \
  --package owned=qa/storage_mainthread/artifacts/round2-packed \
  --package-mode owned=add-owned --mixed --rates 1,4 --repeats 4
```

For a three-way comparison, pass the final artifact twice with distinct labels
(`final` and `owned`) and override only `owned`. Modes follow the sender in mixed runs.
`--block-workers` adds a restrictive worker CSP to exercise the digest fallback.
`--trace-wasm` enables the browser's available wasm/V8 trace categories for cold
compilation diagnosis. These diagnostic runs have extra instrumentation and must
be kept separate from ordinary performance comparisons.
The fixture also serves wasm-bindgen's packaged JS snippets from each artifact.

Both pages run an approximately 8 ms wall-clock busy loop every animation frame
at both throttle rates. Payload generation and SHA-256 integrity checks happen
outside measurement. A 1.5 s idle window establishes rendering-only behavior.
Addition and transfer are separate windows, with CPU profiling throughout and
500 microsecond requested sampling. CPU throttling affects both renderers.

Artifacts contain CPU profiles, Chromium traces, rAF intervals, Long Tasks API
entries and errors. `blink.user_timing` marks align each phase with the profile's
monotonic clock. Main renderer scheduler tasks (`RunTask` or
`ThreadControllerImpl::RunTask`), including their following sibling
`BlinkScheduler_PerformMicrotaskCheckpoint`, provide task boundaries, including tasks shorter
than the Long Tasks API's 50 ms threshold. Missing task boundaries are a failure.
`reanalyze.mjs input.jsonl output.jsonl` recomputes results from saved traces;
analysis v2 includes those post-task checkpoints, which Chromium traces separately.
The build recipe uses `--profiling --no-opt` followed by `wasm-opt -O -g` and
verifies that the name section survived. Set `WASM_OPT` to override the cached binary.

A sample is attributed to mistlib when any stack frame is wasm or the mistlib
JS glue. This includes native children of a glue call (e.g. typed-array copies).
`mistTaskCpuMaxMs` is the largest **sampled CPU contribution inside one real
scheduler task**. `mistContainingTaskWallMaxMs` is the full wall duration of a task
with an attributed sample, potentially including render work and scheduling
pauses. Neither sampling nor a passing maximum proves a strict 16 ms upper bound;
inspect the traces, sampling gaps and containing task maximum too. CPU time here
is sample-weighted elapsed time, not OS thread CPU accounting; descheduling and
profiling overhead can inflate it. `mistPercent` uses the whole phase as its
denominator. Hot self samples and stacks are also reported, including the subset
inside tasks at least 50 ms long. Framework rendering is not mistlib work.

The report uses per-run medians for durations/counts/CPU totals and the worst
observed maximum across successful runs. It preserves failure counts. Linux
Chromium measurements do not replace Windows application reproduction. A named
profiling build may differ from a release build; preserve artifact hashes and
check a final release build before making release performance claims.

```sh
node --test qa/storage_mainthread/metrics.test.mjs
just qa storage-mainthread-check
just qa storage-mainthread --package old="$OLD_PACKAGE" \
  --package candidate=qa/storage_mainthread/artifacts/candidate --mixed \
  --rates 1,4 --repeats 4 --out qa/storage_mainthread/results/mixed.jsonl
```

The validation recipe runs fmt, local CI, wasm target checks (including tests),
Node WASM tests, the real Chromium WASM suite, the npm snippet packaging test,
and metrics tests. Per-command logs and exit codes remain under
`results/validation/`. Browser storage tests check digest compatibility, concurrent
worker jobs and idle restart, copy boundaries, and snapshot-at-call behavior.
They also cover ownership transfer, alias detachment, subarray/empty input, pin
semantics, rejected inputs, resolver task yields and cancellation. Node tests in
`owned.test.mjs` cover shared/non-transferable buffers and missing transfer support.
Set `STORAGE_VALIDATION_LOGS` and `STORAGE_NAMED_LABEL` to preserve earlier validation
logs and named artifacts during another round of work.
