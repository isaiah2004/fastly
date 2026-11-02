# Track D: performance log

Every step from the reference to the final version. Steps that reached the board carry the run ID that measured them.

Local numbers come from a laptop (i5-12450H: 4 P-cores with HT plus 4 E-cores, Docker Desktop on WSL2) that other jobs shared, using the `full` seed-1 data. They are best-of-N wall times and only show direction. The board box (i5-13500, six P-cores, 12 threads) is about 1.4× faster, and local gains did not always carry over (see steps 11–13). Phase times in brackets are local.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 72.69 | 893 | 0 |
| 1 | Rust, std only: parallel parse, forward and reverse CSR, parallel bidirectional Dijkstra with hash-map labels (local native 1.67 s) | | | | |
| 2 | Locality renumbering (parallel BFS region growing), 4-ary heap of packed `(dist << bits(n)) \| node` keys, paged distance labels instead of hash maps (local ~0.9 s, 224 MB) | | | | |
| 3 | Expand the side with the smaller heap, not the smaller key: sources whose only out-edges weigh ~2^32 stopped flooding the graph backwards. Settled nodes 14.3 M -> 7.1 M (local ~0.34 s, 150 MB) | | | | |
| 4 | No atomics in setup (edges bucketed by owner thread while parsing); forward CSR copied during the BFS; both directions' labels in one page; prefetch of the next heap top's row; u32 labels with a u64 rerun (local ~0.31 s, 126 MB) | | | | |
| 5 | Linux memory: big arrays straight from mmap (2 MiB aligned, `MADV_HUGEPAGE`, unmapped on free), one malloc arena, 1 MiB thread stacks (local ~0.30 s, 124 MB) | | | | |
| 6 | AVX2 line parser (one 32-byte compare finds a line's delimiters, SWAR digits); duplicate queries answered once; persistent parked thread pool; push pruning with `dist + top_other >= best` (local ~0.30 s, 116 MB) | | | | |
| 7 | Compressed CSRs: u32 head + u16 weight, rare weights >= 65535 in a sorted side table; parse buckets as one u64 per edge (local Docker ~0.30 s, 90 MB). Commit 7c0701a (steps 1-7) | | | | |
| 8 | Renumbering checked: without it the queries take 299 ms instead of ~127 ms and the reverse CSR 113 ms instead of 22 ms, so it stays. First CSR scattered per cache-sized slot (8 per thread); parse buckets as block chains in per-thread slabs [csr0 61 -> 20 ms] (local Docker ~0.26 s, 89 MB). Commit f650481 | | | | |
| 9 | Packed 6-byte edges (one stream per row); two-stage prefetch in the BFS (rows 16 ahead, neighbour tags 8 ahead); branchless min-of-4 in the heap's sift-down [order 70 -> 47 ms] (local Docker ~0.225 s, 91 MB). Commit b70d0cd, K 20.90 | run-d76821feeaa1 | 0.1616 | 90.5 | 66.01 |
| 10 | Rows copied inside the BFS while still hot [order ~47 -> ~32 ms]. Final graphs at 4 bytes per edge (16-bit head delta + 15-bit weight, ~4% escaped to a side table), encoded during the BFS from the tags, so there is no permutation array. First graph at 4 bytes per edge; parse buckets 6 bytes per edge; 64-id label pages (local Docker ~0.20 s, 72 MB). Commit a4807dd, measured in the board run of c8bd349, K 22.29 | run-531dd6e3c4a9 | 0.1511 | 72.7 | 67.41 |
| 11 | Labels split per direction (64-id pages each); queries scheduled longest \|s - t\| first; u8 row degrees; row offsets compacted to a u32 base per 16 nodes plus a u16 each [queries ~93 -> ~88 ms] (local Docker ~0.185 s, 67 MB) | | | | |
| 12 | Code diet: instrumentation removed, helpers folded, `#[inline(always)]` dropped (fat LTO inlines the same). No time change in interleaved native and Docker A/B; rustfmt'd code lines 1083 -> 955 | | | | |
| 13 | First graph built in one pass per slot (count, prefix and scatter while the slot's buckets are in cache; slot sizes come from bucket lengths), so the n-sized degree array is gone [csr0 ~14.5 -> ~12.5 ms native]. Commit fee1215 (steps 11-13), K 22.55 | run-7213cfc0f5c2 | 0.1514 | 71.3 | 67.65 |

**Board versus local.** Steps 11–13 measured about 7% faster in local Docker, but on the board they moved T from 151.1 to 151.4 ms (no change) and M from 72.7 to 71.3 MB. The query-side changes in step 11 (split labels, compact offsets) seem to matter on the laptop's smaller caches and E-cores, not on the board's P-cores.

## Measured and dropped

All local, none pushed:

- **Radix heap** instead of the 4-ary heap, tried twice: slower both times. The heaps stay small (median query settles 128 nodes), so the 4-ary heap's sift-down stays in L1.
- **Choosing the side by smaller key** instead of smaller heap size: 15.31 M settled instead of 7.54 M, and a query phase of 359 ms instead of 87 ms.
- **Two queries interleaved per thread:** no gain. The next heap top's row and labels are already prefetched one settle ahead.
- **Query-phase threads:** 6 threads 128.6 ms, 8 threads 100.5 ms, 12 threads 86.6 ms (best of 5). 12 stays.
- **Probe-then-requeue for expensive queries:** not built. With dynamic scheduling, all 12 threads finish within 3.4 ms of each other. A perfect longest-first order would save about 3 ms, and restarting unfinished probes would cost about 13% more settles.
- **Reverse in-degrees counted inside the forward copy:** the fused loop cost about 6 ms more than the plain copy, cancelling the ~5 ms count pass it removed.
- **Reverse escapes counted in advance** (no fix-up pass): no change.
- **First graph's arrays recycled for the forward graph:** no change, because transparent huge pages make the fresh page faults cheap.
- **Flat labels**, labels for both directions on the same lines vs split, ordering the regions a second time (BLOB), sorting queries by source, prefetching heap children, a label pre-pass prefetch: all neutral.
- **PGO:** held back, because it was unclear whether build-time profiling is allowed under the rules.
