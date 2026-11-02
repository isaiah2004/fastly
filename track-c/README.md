# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Rust, std only, one file (`src/main.rs`), for Linux on x86-64 with AVX2, BMI2 and PCLMUL
(`target-cpu=x86-64-v3` plus `+pclmulqdq` in `.cargo/config.toml`).

- **Input.** `records.csv` is `mmap`ed (declared via `extern "C"`).
  `regions.csv` is split into 64 pieces that the threads claim and parse in parallel into a dense
  `region_id -> country+1` byte table (0 = unmatched).
- **Parallel CSV without a sequential pass.** By the 2.1 grammar `"` only occurs inside quoted fields, so a
  byte is inside quotes exactly when an odd number of `"` precede it. The file is cut into 32 chunks per
  thread (work-stealing). Each chunk guesses its first record start (an LF followed by a digit or a BOM)
  and assumes it is outside quotes. Every chunk also counts its quotes, and after all threads finish the
  guesses are checked exactly against the prefix parity. A wrong guess (impossible for generator output,
  possible for other 2.1-legal files) makes the program discard everything and rerun a simple sequential
  parser, so the output is always exact.
- **Parsing.** AVX2 compares per 64-byte block give quote/comma/LF bitmasks; a prefix-XOR of the quote
  mask (one carry-less multiply) gives the inside-quotes mask, so commas and LFs outside quotes are the
  field/record boundaries. A small scanner hands out these positions block by block, five per record.
  Unescaped name/note lengths come from field length and quote counts (`""` counts 1, delimiters 0).
  `id`, `region_id` and `amount` are parsed with SWAR (8 digits at a time), amounts in integer cents.
  BOMs are skipped only at record starts; a final record without terminator is handled at EOF.
- **Distinct names.** One 64-byte line per distinct name: a hash tag, the length, the raw name bytes
  inline (exact byte comparison, never hash-only; names over 24 bytes are compared against the file) and
  a 256-bit country set. Raw inner bytes of a quoted field map 1:1 to unescaped values, and the empty
  unquoted and `""` names share one key. The table is split into one slot range per thread, chosen by
  the name's hash, and only the range's owner touches it: parsers send each record's (name, country) as
  a 32-byte message in batches over lock-free stacks, and the owner probes its range (prefetching a few
  messages ahead) with plain loads and stores. Each thread may have at most 4 batches per owner in
  flight; a thread that runs out applies waiting batches (taking the range's lock), so memory stays
  bounded and late or descheduled threads do not stall the rest. `names_distinct` is a popcount per
  country. Parsed records wait in a 128-record stage while their region entries are prefetched, then are
  aggregated and sent.
- **Memory.** ~12 MB peak anonymous memory locally: 8 MB name table (huge pages requested), 1 MB region
  table, message batches, the rest runtime. One glibc malloc arena (`mallopt`); 256 KiB thread stacks.
- **Fallbacks.** More than ~114k distinct names, more than 255 countries, or an unexpected `regions.csv`
  shape also fall back to the exact sequential path. `--slow` forces it (used for cross-checking).

## Dependencies

None (Rust std only).

## Build and run

```
cargo build --release
target/release/solution [--data /data] [--out /out] [--threads N] [--slow] [--chunks N]
```

The Docker image builds the same binary (`target-cpu=x86-64-v3`) and runs it with the defaults.
`--chunks` only exists for boundary testing.

## Testing

Besides the published hashes (tiny, sample, full) and the spec's worked and rules examples, the fast path
was fuzzed against the Python reference on ~2,900 random 2.1-legal files (quoted LFs followed by digits,
BOMs inside quotes, unquoted names, leading zeros, no final terminator) with 1 to `len` chunks, and
`full` gives the same hash with up to 3 million chunks (a chunk boundary at nearly every record) without
ever needing the fallback.

## With another 24 hours

Measure on the real box (our timings come from a busy laptop), tune chunk size and prefetch distances,
and shrink the name tables (memory).
