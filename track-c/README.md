# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Rust, std only, one file (`src/main.rs`).

- **Input.** `records.csv` is `mmap`ed (declared via `extern "C"`, Linux only; `std::fs::read` elsewhere).
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
  mask gives the inside-quotes mask, so commas and LFs outside quotes are the field/record boundaries.
  Unescaped name/note lengths come from field length and quote counts (`""` counts 1, delimiters 0).
  `id`, `region_id` and `amount` are parsed with SWAR (8 digits at a time), amounts in integer cents.
  BOMs are skipped only at record starts; a final record without terminator is handled at EOF.
- **Distinct names.** A lock-free hash table shared by all threads: one 32-byte entry per distinct name
  holding a hash tag, the length and the raw name bytes inline (exact byte comparison, never hash-only;
  names over 24 bytes are compared against the file). Raw inner bytes of a quoted field map 1:1 to
  unescaped values, and the empty unquoted and `""` names share one key. A parallel table holds a 256-bit
  country set per name. Each slot range is owned by one thread; other threads send `(slot, country)` pairs
  to the owner in batches, so bits are set with plain stores. `names_distinct` is a popcount per country.
  Name hashing, the table line and the region entry are software-prefetched and each record is
  aggregated 8 records after it is parsed.
- **Memory.** ~12.5 MB peak anonymous memory: 4 MB name slots + 4 MB country bitmaps (huge pages
  requested), 1 MB region table, the rest runtime. One glibc malloc arena (`mallopt`).
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
