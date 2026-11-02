# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Rust, standard library only. One persistent pool of `available_parallelism()` threads that sync on a `Barrier` between phases; the input is split into equal chunks at line starts.

1. **Load.** `mmap` the log read-only. Each thread asks the kernel to populate the page tables for its own chunk (`MADV_POPULATE_READ`), so that work runs in parallel too.
2. **Pass 1 (parse, intern, count).** Each thread walks a 64-byte bitmask of newline positions (AVX2), so lines don't wait on each other. For each line:
   - The hour index (0..71) comes straight from the day and hour digits. All timestamps fall in one fixed 72-hour window, so there is no date parsing.
   - The endpoint starts at the first `/` (SWAR) and ends 4 bytes before the last space, which an SSE compare finds.
   - The endpoint is hashed from two masked 32-byte AVX2 blocks and interned in one lock-free open-addressing table that all threads share. The table stores the bytes zero-padded, so checking equality takes 2 vector compares. Each slot keeps the length, the id and the full 64-bit hash in one 16-byte record, so a probe touches one cache line. Ids are global, so no merge step is needed.
   - The thread adds to a per-thread `count | errors<<32` counter for (hour, endpoint). Nothing is stored per line.
3. **Offsets.** The ~1,842 endpoints are sorted once by bytes. Group order is hour-major and then endpoint rank, which is exactly the output order. A two-phase parallel prefix sum gives every group, and every thread inside a group, its slot range in one `u32` latency array. A bitmask of the hours each thread saw keeps this phase from touching unused counters.
4. **Pass 2 (scatter).** The threads re-walk their chunks and find each endpoint again with a read-only probe of the now-frozen table that matches the full 64-bit hash and the length, with no byte compare (if pass 1 ever saw two different endpoints share both, pass 2 compares bytes too). They parse each latency with an SSE `maddubs`/`madd` routine that has no branches, then write it straight to its slot.
5. **Percentiles.** Hours are handed out dynamically. Each group takes exact nearest-rank p50, p95 and p99 with `select_nth_unstable` in the order p50 on the whole group, p95 on the upper half, p99 on the top 5%. Small groups are sorted instead. The exact byte length of every hour's output is computed at the same time.
6. **Output.** The output file is sized with `ftruncate` and `mmap`ed shared, and each thread formats its hours (2-digit LUT itoa) directly at their final offsets. There is no single serial `write` of 10 MB, and the only memory the output costs is its page cache.

Memory stays low on purpose. Nothing per line is kept between passes. The latency array (12.4 MB) is the only large allocation, and it is freed before the output is written.

On non-Linux targets (local testing on Windows), the loader falls back to `std::fs::read` and a buffered write, and the code falls back to scalar versions of the SIMD helpers.

## Dependencies

None (std only; `mmap`/`madvise` are declared via `extern "C"`).

## Build and run

    cargo build --release          # .cargo/config.toml sets target-cpu=x86-64-v3 (AVX2/BMI2)
    ./target/release/solution --data /data --out /out

Defaults are `/data/requests.log` and `/out/summary.txt`.

## With another 24 hours

- Do it in one pass: keep `(id, latency)` per line, so the 240 MB input is never re-read. That costs about 12 MB more memory.
- Make the intern table one cache line per lookup, with the tag and the first 56 bytes inline.
- Use a radix-based selection for the few very large groups.
