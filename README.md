# Team 3pi4: Speed Demon 2026

This repo is our entry in all four tracks. Each track is one Rust file with only the standard library: zero dependencies, nothing vendored. Each `track-*` folder has the full write-up (README.md) and the step-by-step log with board run IDs (PERF.md).

Board numbers below are **as of commit fee1215**.

| Track | Board run | T | M | K | Details |
|---|---|---|---|---|---|
| A: Ingest | run-c56b72a97a8f | 25.3 ms | 20.4 MB | 42.2 | [README](track-a/README.md) · [PERF](track-a/PERF.md) |
| B: Index | run-0cb071d721ba | 171.7 ms | 37.9 MB | 27.1 | [README](track-b/README.md) · [PERF](track-b/PERF.md) |
| C: Join | run-9206d49f43e1 | 40.1 ms | 13.3 MB | 26.9 | [README](track-c/README.md) · [PERF](track-c/PERF.md) |
| D: Traverse | run-7213cfc0f5c2 | 151.4 ms | 71.3 MB | 22.6 | [README](track-d/README.md) · [PERF](track-d/PERF.md) |

## Track A: Ingest (request log to per-hour percentiles)

- Single pass. Threads `pread` 64 KB chunks into an L2-sized buffer. Newline positions come from a branch-free bitmask walk, and the latency is parsed by an SSE routine with no branches.
- Endpoints are interned in one lock-free open-addressing table that all threads share (8-byte slot words, always verified on the bytes), so there is no merge step.
- Each line becomes a 4-byte record in per-hour regions of a reserved arena. Per hour: count, scatter by endpoint rank into an L2-sized array, then exact p50/p95/p99 with `select_nth_unstable` on shrinking upper parts.
- Hours are formatted and `pwrite`n in parallel at their final offsets, and each region is released with `MADV_DONTNEED` when done. Peak memory is about 4 bytes per line.

## Track B: Index (boolean and phrase queries over documents)

- Queries are parsed first, and only their terms are ever indexed. A 1 MB sample picks dense bitmaps for frequent BOOL terms and sorted 16-bit posting lists for the rest.
- Documents are processed in rounds of 65,536, so only one round's index is alive at a time and doc ids fit in 16 bits.
- The AVX2 tokenizer turns 64-byte blocks into token boundary masks. Table slots are prefetched per document, and each 64-doc block's bitmap words build up in L1 and are written once.
- Phrases are checked in place at their rarest term. BOOL queries are folded over lists and bitmaps: galloping AND/NOT, OR switching to a bitmap, popcounts for counts.

## Track C: Join (CSV records against regions, distinct names per country)

- The CSV is parsed in parallel without a sequential pass. Each chunk guesses that it starts outside quotes, and the guesses are checked afterwards against the exact quote-parity prefix. A wrong guess reruns an exact sequential parser.
- AVX2 masks for quotes, commas and newlines; a carry-less multiply (PCLMUL) gives the inside-quotes mask; SWAR number parsing; amounts in integer cents.
- Distinct names live in one 64-byte line each. The table is split into hash ranges that only their owner thread touches; other threads send records over lock-free batch stacks.
- Peak memory is about 12 MB: an 8 MB name table, a 1 MB region table, and the rest runtime.

## Track D: Traverse (shortest paths on a road network)

- Parallel AVX2 parse into per-slot buckets, then a first graph built slot by slot while the buckets are in cache.
- Parallel BFS region growing renumbers the nodes for locality. Without it, the queries take 299 ms instead of 127 ms. The renumbering also lets 96% of edges fit in 4 bytes (16-bit id delta + 15-bit weight).
- Bidirectional Dijkstra that expands the smaller heap: 2.8× fewer settled nodes than one-way search, and half as many as choosing the side by smaller key. 4-ary packed-key heap; paged u32 labels with u64 and wide-key fallbacks.
- Queries are handed out from one atomic counter, longest |s - t| first, and all 12 threads finish within 3.4 ms of each other.

## Shared engineering principles

- **Std-only Rust, zero dependencies.** System calls (`mmap`, `madvise`, `mallopt`, `pwrite`) are declared through `extern "C"` under `cfg(target_os = "linux")`, with a portable `std::fs` fallback. D = 1 on every track.
- **How each track reads its input.** It depends on the access pattern:
  - `mmap` where many threads read the input in parallel, sometimes more than once (B, C, D; D adds `MAP_POPULATE`).
  - `pread` into small L2-resident buffers where one streaming pass suffices (A); this avoids page-table work and keeps memory flat.
  - D's big arrays come straight from 2 MiB-aligned `mmap` with `MADV_HUGEPAGE` and are unmapped on free, and C asks for huge pages for its name table.
  - Tracks A, C and D set `M_ARENA_MAX=1`, a single glibc malloc arena, so per-thread heaps do not inflate M.
- **AVX2 where it removes branches.** `target-cpu=x86-64-v3` (plus PCLMUL in C). Delimiter finding is a compare plus movemask, digits use SWAR, and masks are walked with `tzcnt`. Every SIMD path has a scalar fallback for lines it cannot handle.
- **Measuring on a noisy host.** Local work ran on a shared laptop with mixed P/E cores.
  - Every change was judged by interleaved A/B runs and the minimum over many runs, and changes inside the noise were dropped.
  - Deterministic counters (settled nodes, probes, records) were used wherever time was too noisy.
  - Board run IDs are the ground truth. Where they disagree with local numbers, the PERF logs say so (for example, track D's steps 11–13).
- **Holdout safety.** Every program is exact for any input the spec allows, not just seed 1:
  - Fallbacks: C's speculative parse is re-verified and can rerun sequentially; D's u32 labels rerun as u64, then as a wide-key Dijkstra; A writes rare large latencies as escape records.
  - Interning and joins always compare bytes, never hashes alone.
  - Every output was hash-checked on the tiny, sample and full datasets plus extra seeds, and with `check.py` in Docker.

## Layout

`track-a` … `track-d` each have their own Dockerfile (the organisers' pinned two-stage Rust build), `.speed-demon.toml`, README and PERF log. `check.py` is the organisers' local checker, unchanged.
