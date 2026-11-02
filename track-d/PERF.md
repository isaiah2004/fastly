# Track D: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

Notes in the Change column give local numbers: full scale, Docker on a loaded 12-thread laptop (i5-12450H), so only the trend is meaningful; the board box is different.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 72.69 | 893 | 0 |
| 1 | Rust: parallel parse, fwd+rev CSR, parallel bidirectional Dijkstra with hash-map labels (local native: 1.67 s) | | | | |
| 2 | Locality renumbering (BFS region growing) + 4-ary packed-key heap + paged labels instead of hash maps (local: ~0.9 s, 224 MB) | | | | |
| 3 | Expand the side with the smaller heap (was: smaller key), so "from no-in source" queries stop flooding the graph backwards; settled nodes 14.3 M -> 7.1 M (local: ~0.34 s, 150 MB) | | | | |
| 4 | Setup without atomics (edges bucketed by owner thread while parsing), forward CSR copied during the BFS ordering, both directions' labels side by side in one page, prefetch of the next row, u32 labels with u64 fallback, 256-id pages (local: ~0.31 s, 126 MB) | | | | |
| 5 | Linux memory hygiene: big arrays straight from mmap (2 MiB aligned, MADV_HUGEPAGE, unmapped on free), one malloc arena, 1 MiB thread stacks (local: ~0.30 s, 124 MB) | | | | |
| 6 | AVX2 line parser (one 32-byte compare finds the delimiters, SWAR digit conversion), duplicate queries answered once, parse buckets freed as consumed, persistent parked thread pool instead of spawning per phase, prune pushes with `nd + top_other >= best` (local: ~0.30 s, 116 MB) | | | | |
| 7 | Compressed edges: CSRs store u32 head + u16 weight (rare weights >= 65535 in a sorted side table), parse buckets packed to one u64 per edge (local docker: ~0.30 s, 90 MB) | | | | |
| 8 | Renumbering measured: without it queries take 299 ms instead of ~127 ms and the reverse CSR 113 ms instead of 22 ms, so it stays. First CSR scattered per cache-sized slot (8 per thread) instead of one 2.5 MB range per thread, parse buckets as block chains in per-thread slabs (csr0 61 -> 20 ms; local docker: ~0.26 s, 89 MB) | | | | |
