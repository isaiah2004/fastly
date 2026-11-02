# Track D: Traverse (Rust, std only)

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md). In short: a directed road-like graph (1,740,512 nodes, 5,000,000 edges, u32 weights including 0 and ~2^32), 2,000 point-to-point queries, and exact shortest distances or `-1`. Preprocessing counts toward the time.

Board: commit fee1215, run-7213cfc0f5c2, **T 151.4 ms, M 71.3 MB, K 22.55** (reference 72.69 s, 893 MB).

## Approach

The run is one pipeline on a persistent pool of 12 parked threads. Phase times are best-of-N in local Docker; the board box is about 1.4× faster.

| Phase | What it does | Local |
|---|---|---|
| Parse | `mmap` (`MAP_POPULATE`); each thread takes a line-aligned chunk. One AVX2 32-byte compare and movemask finds a line's two spaces and newline, and SWAR multiplies convert the digits. Each edge becomes a packed 6-byte entry `(tail offset, head, weight)` in a bucket for the cache-sized slot (8 per thread) that owns its tail. Buckets are chains of 512-entry blocks cut from per-thread slabs. | 18 ms |
| First graph | Per slot: count degrees, prefix-sum and scatter while the slot's buckets are still in L2. 4 bytes per edge: `head << (32 - bits(n)) \| weight`, with heavy weights in a sorted side table. | 15 ms |
| Renumber | Parallel BFS region growing: regions of up to 16k nodes, nodes claimed by CAS on a per-node tag, two-stage prefetch (rows 16 ahead, neighbour tags 8 ahead). Each thread copies the rows of the nodes it places while they are hot, already in the final encoding. A new id is a prefix sum plus the index in the thread's list, so no permutation array is ever built. | 33 ms |
| Forward graph | The threads' rows laid side by side. 4 bytes per edge: `(head - tail + 2^15) << 15 \| weight` when both fit, else an escape into a flat `(head, weight)` table (~4% of edges). Row offsets: a u32 base per 16 nodes plus a u16 each. | 10 ms |
| Reverse graph | Threads own equal node ranges: count in-edges (edges into another thread's range go over in hand-over lists), prefix, fill. Same encoding. | 22 ms |
| Queries | Bidirectional Dijkstra, one atomic work counter over the distinct queries, longest \|s - t\| first. | 90 ms |

**The query loop:**
- It always expands the side with the smaller heap, and stops when `top_f + top_b >= best`.
- It pushes a node only if `dist + top_other < best`.
- The heap is 4-ary, with packed `(dist << bits(n)) | node` keys and a branchless min-of-4 sift-down.
- Labels are u32 in 64-id pages per direction, allocated on first touch (memory follows the search, not n).
- It reruns with u64 labels when a distance reaches 2^32 - 1, and as a last resort runs a plain wide-key Dijkstra.

## Why it is fast (measured)

- **Locality renumbering pays for itself several times over.** Without it, the queries take 299 ms instead of 127 ms and the reverse build 113 ms instead of 22 ms (step 8 of PERF.md). It also lets 96% of edges fit in 4 bytes as 16-bit id deltas.
- **Bidirectional search with the smaller-heap rule:**
  - A one-way Dijkstra settles 2.8× more nodes (median and geometric mean over 198 sampled queries, 2.9× for far targets), as expected for a near-planar graph.
  - Choosing the side by key instead of by heap size doubles the settled nodes (7.54 M vs 15.31 M) and makes the phase 4× slower (359 vs 87 ms). Sources whose only out-edges weigh ~2^32 would otherwise flood the graph backwards.
- **Balanced query phase:**
  - The work is very skewed. The slowest 1% of queries take 33% of the query time and the slowest 10% take 83%; the median query settles 128 nodes and the largest 80k.
  - Dynamic scheduling keeps all 12 threads within 3.4 ms of each other at the end.
  - Thread sweep: 6 threads 128.6 ms, 8 threads 100.5 ms, 12 threads 86.6 ms.
- **Few, cache-friendly passes over the edges:**
  - The first graph is built slot by slot.
  - Rows are copied during the BFS instead of in a separate permutation pass.
  - The first and final graphs both use 4 bytes per edge, so the peak stays at about 62 MB of our own memory (about 71 MB with the container's baseline).

## Rejected, with reasons

- **Contraction hierarchies:** contracting a 1.7M-node road network takes on the order of seconds, against a whole run of about 0.15 s.
- **ALT (landmarks):** not built. Estimated cost per landmark: a forward and a backward full Dijkstra (at ~150 ns per settled node, ~0.26 s each on one thread) plus 14 MB of distances. Even 6 landmarks run in parallel would cost more time than the 2,000 queries take now, and push memory past 150 MB.
- **Radix heap:** tried twice, slower both times. The heaps are small, so the 4-ary heap stays in L1.
- **Two queries interleaved per thread:** no gain, because the next settle's row and labels are already prefetched.
- **PGO:** not used, because it was unclear whether the rules allow profile-guided builds.

Smaller dead ends are listed in PERF.md.

## Limits

- Ids are tagged as `thread << 26 | index`, so up to 64 threads and 2^26 nodes per thread.
- Any u32 weights are supported; distances are exact up to u64. The common case uses u32 labels, with u64 and wide-key fallbacks.
- Linux gets `mmap`, `mallopt` and `MADV_HUGEPAGE` through `extern "C"`. Other systems fall back to `std::fs::read` and the system allocator.
- The pipeline's peak is the renumbering phase, where the first graph (27 MB), the node tags (7 MB) and the copied rows (24 MB) are alive together.
- Local measurements did not always carry over to the board: steps 11–13 were 7% faster locally and flat on the board.

## With another 24 hours

- Profile on the board's CPU instead of the laptop, since our last three steps moved there but not on the board.
- Lower the peak: run the BFS on a heads-only graph and build the final rows from a second, sequential read of the buckets, so the 4-byte first graph and the copied rows are never alive together.
- Cut the cost per settled node (~150 ns, mostly label misses on both sides): store each node's two labels next to its row offset, or use a bucket queue keyed by distance band.
- Overlap phases instead of separating them with barriers: renumbering threads end within a few ms of each other, but each phase change still costs a full join.

## Dependencies

None (std only).

## Build and run

```
docker build -t d . && docker run -v <data>:/data:ro -v <out>:/out d
cargo build --release && target/release/solution --data DIR --out DIR
```

`.cargo/config.toml` sets `target-cpu=x86-64-v3` (AVX2). Without AVX2, a scalar parser takes over.
