# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Rust, standard library only, Linux and AVX2 (`target-cpu=x86-64-v3`). One scoped pool of `available_parallelism()` threads that sync on a `Barrier` between phases.

1. **Single pass (read, parse, intern, record).** Threads take 64 KB chunks of the log from an atomic counter and `pread` each chunk (plus up to 512 bytes, to finish the last line) into a buffer that stays in L2. A chunk owns the lines that start in it. Each line scans for LF in 64-byte steps (two 32-byte AVX2 compares) beginning at byte 39, the earliest legal LF position; 93% of lines end within the first step, so the loop exit is predictable. This removes the per-chunk newline index and its stores/loads. For each line:
   - The hour index (0..71) comes straight from the day and hour digits, and is reused while a line's month, day and hour bytes (one 8-byte compare) equal the line before's. All timestamps fall in one fixed 72-hour window, so there is no date parsing.
   - The endpoint starts at the first `/` (SWAR) and ends 4 bytes before the last space, which an SSE compare finds. The latency is parsed by an SSE `maddubs`/`madd` routine that has no branches.
   - The endpoint is hashed (three `crc32` steps) from its first, middle and last 8 bytes and interned in one lock-free open-addressing table that all threads share. Each slot is one 8-byte word (32-bit hash tag, length, id). The bytes are kept zero-padded beside it and are always compared (two masked 32-byte AVX2 compares for endpoints up to 64 bytes, four up to 128), so the result is exact. Ids are global, so no merge step is needed.
   - The line becomes one `u32` record, `id<<21 | error<<20 | latency`, appended to the thread's current block for that hour. Every line is exactly one record: a latency of 2^20-1 or more keeps 2^20-1 in the record, and its full value goes to a per-hour side list (`id<<32 | latency`, about 1% of lines) applied after the scatter. Unused block tails are filled with records of a reserved id that sorts after every endpoint, so the per-hour walks are plain branch-free slice loops. Each hour has its own region of a reserved (`MAP_NORESERVE`), small-page arena, and threads take 512-record blocks from it with an atomic counter.
2. **Per hour (dynamic).** After a barrier, each thread sorts the ~1,842 endpoint ids by bytes for itself (no second barrier). Then each worker takes the next hour:
   - It walks the hour's region twice (count per endpoint, then scatter the latencies by endpoint rank into a per-thread array of about 170 KB that stays in L2), then releases the region with `MADV_DONTNEED`.
   - It takes exact nearest-rank p50, p95 and p99 per group with `select_nth_unstable`: p50 on the whole group, p95 on the part above it, p99 on the part above that. Groups of 64 or fewer use no sort and no branches: the group is read straight from the scatter array with AVX2 masked loads (lanes past the group count as `u32::MAX`), each value's count of smaller values is built with one broadcast compare per value, and the k-th smallest is the largest value whose count is at most k. This removed about 40% of the run's branch mispredictions.
   - It formats the hour's lines (branch-free 2-digit LUT itoa) into the hour's slot of a reserved output area.
   - Offsets are assigned in hour order under a mutex. Whoever completes the next run of hours `pwrite`s them at their offsets and releases their pages, so threads write in parallel and the 10 MB output is never held in one buffer.

`mallopt(M_ARENA_MAX, 1)` keeps glibc from mapping per-thread arenas, which would take the address-space lock while other threads fault pages in.

Memory: the records take 4 bytes per line (12.5 MB for `full`), which is the peak. Everything else is a few MB of per-thread buffers. The input is never mapped.

## Dependencies

None (std only; `mmap`, `madvise` and `mallopt` are declared via `extern "C"`).

## Build and run

    cargo build --release          # Linux; .cargo/config.toml sets target-cpu=x86-64-v3 (AVX2/BMI2)
    ./target/release/solution --data /data --out /out

Defaults are `/data/requests.log` and `/out/summary.txt`.

The build is profile-guided: `.cargo/config.toml` passes `-C profile-use=pgo.profdata`. PGO profile trained on generator seed 7 (not the scored data); profile is a build input and contains no answers. It was made with `spec/generators/a_ingest.py --seed 7` at the `sample` and `tiny` scales: build in `halloumee/speed-demon-rust:1.99.1-trixie-llvm-tools` with `RUSTFLAGS="-C target-cpu=x86-64-v3 -C profile-generate=/pgo"`, run it 3 times on `sample` and once on `tiny`, then run `llvm-profdata merge -o pgo.profdata /pgo/*.profraw`. The board's build image has the same rustc 1.99.1, so the Dockerfile is unchanged. A stale profile only costs speed: rustc warns and still builds.

## With another 24 hours

- Hide the intern table's L2 latency at full thread count (prefetching helped one thread but not twelve).
- A cheaper selection for the groups of 65 to 200 lines.

## November 2 direct-LF experiment

The fixed grammar permits skipping 39 bytes before searching for LF. All seven methods, the one-byte endpoint, ten-digit latencies and 125-byte endpoints still use the same exact parser. The search may read past LF into the existing padded chunk buffer. No endpoint catalogue or latency distribution is assumed.

Dense exact latency histograms are unsuitable: the range is all of u32, requiring about 2.07 PiB at 72 × 1,842 × 2^32 four-byte counters. Even 256 coarse bins per group require about 130 MiB and do not determine exact percentiles. Removing the barrier needs a correctness mechanism for arbitrary disorder; the spec supplies no safe time watermark.
