# Track D (Traverse) - Rust, std only

No dependencies. Reads `/data/graph.txt` and `/data/queries.txt`, writes `/out/distances.txt`.

## Approach
1. **Parse** (12 threads): mmap the input; each thread parses a line-aligned chunk. AVX2: one
   32-byte compare + movemask finds a line's two spaces and newline; digits are converted with
   SWAR multiplies (scalar fallback elsewhere). Self-loops are dropped. Each edge goes into a
   bucket for the cache-sized slot (8 per thread) that owns its tail, as a packed 6-byte entry
   (tail offset, head, weight; heavier weights go to a side list). Buckets are chains of
   512-entry blocks cut from per-thread slabs.
2. **First graph** (old ids): 4 bytes per edge, `head << (32 - bits(n)) | weight`, the all-ones
   weight meaning "look it up in a sorted side table". Built slot by slot, so writes stay in cache.
3. **Renumber for locality**: parallel BFS region growing (regions of up to 16k nodes, claimed
   by CAS on a per-node tag, two-stage prefetch). Each thread copies the rows of the nodes it
   places while they are hot, already encoded in the final format; the new id is a prefix sum
   plus the index in the thread's list, so there is no permutation array.
4. **Final graphs** (forward and reverse): 4 bytes per edge, `(head - tail + 2^15) << 15 | weight`
   when both fit, else an escape into a flat `(head, weight)` table (~4% of edges after
   renumbering). Row offsets: a u32 base per 16 nodes plus a u16 each. The reverse graph is built
   by owner ranges with cross-thread hand-over lists.
5. **Queries** (12 threads, work queue, longest |s - t| first, duplicate lines answered once):
   bidirectional Dijkstra, always expanding the side with the smaller heap (so a source whose only
   out-edges weigh ~2^32 does not flood the graph backwards), stop when `top_f + top_b >= best`,
   and a node is pushed only if `dist + top_other < best`. 4-ary heap of packed
   `(dist << bits(n)) | node` keys with a branchless sift-down. Labels live in 64-id pages per
   direction, allocated on first touch (memory follows the search, not n), stored as u32 with a
   u64 rerun when a distance reaches 2^32 - 1 (and a plain wide-key Dijkstra as a last resort).
6. Answers are written in query order in one buffer (`-1` for unreachable).

Linux details: big arrays come from mmap (2 MiB aligned, `MADV_HUGEPAGE`), one malloc arena,
a persistent parked thread pool with 1 MiB stacks.

## Build and run
`docker build -t d . && docker run -v <data>:/data:ro -v <out>:/out d`

Native: `cargo build --release && target/release/solution --data DIR --out DIR`
