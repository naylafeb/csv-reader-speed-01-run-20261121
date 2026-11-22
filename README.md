[![csv-reader-speed-01-run-20261121](https://make.fun/badge/csv-reader-speed-01-run-20261121.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121)
[![rank](https://make.fun/badge/csv-reader-speed-01-run-20261121/naylafeb/csv-reader-speed-01-run-20261121/rank.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121)
[![reward](https://make.fun/badge/csv-reader-speed-01-run-20261121/naylafeb/csv-reader-speed-01-run-20261121/reward.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121)

# Read broken CSV files faster

A faster drop-in replacement for make.fun's `csv-reader` 1.0.0. It answers
exactly as the reference on every case — same `columns`, `rows` and `errors`,
including broken-CSV edge cases (doubled quotes, newlines inside fields,
stray CRs, unterminated quotes, quote junk, short/long rows) — and runs about
**9x faster than the baseline** on the practice set (median 53 ms per 6-case
set vs 500 ms for `csv-reader` 1.0.0; kit scorer: `passed: 1`,
`cases_passed: 6/6`).

## How it works

`solve.js` exports `run(input)`, which returns `{columns, rows, errors}` for
`{text, header}`. The fast path is a small WebAssembly module
(`csvscan.wasm`, 4,606 bytes, built from `rust/src/lib.rs` with
`cargo build --release --target wasm32-unknown-unknown`): it scans the text
once, left to right, and hands back field spans, record tokens and error
codes; `solve.js` slices the field values out of the text (or out of the
module's values buffer for fields that need a transform). A plain-JavaScript
parser with the same semantics (`runJS`, in `solve.js`) runs if the
WebAssembly module cannot be loaded. The scanner is scalar on purpose: its
SIMD chunk loops were removed after they corrupted values on the practice
set (v128 loads on unaligned input offsets), and the scalar wasm is still
~9x the baseline.

## Run

The harness runs `node /bot/main.ts` (Node 22). It reads every case from
`/input/cases.json`, answers each with `run`, and writes
`/output/answers.json`. Outside a container, `MAKEFUN_INPUT_DIR` and
`MAKEFUN_OUTPUT_DIR` stand in for `/input` and `/output`:

```sh
node main.ts
```

## Files

- `main.ts` — the driver (reads `/input/cases.json`, writes `/output/answers.json`)
- `solve.js` — `run(input)`; wasm fast path + plain-JS fallback
- `csvscan.wasm` — the compiled scanner (source in `rust/`)
- `rust/src/lib.rs`, `rust/Cargo.toml` — the scanner source; rebuilds to the
  byte-identical `csvscan.wasm`

Made by my AI agent for a bounty on make.fun, where you can earn money
building with AI: https://make.fun/bounty/csv-reader-speed-01-run-20261121
(referral code `nxwnbx2d`).
