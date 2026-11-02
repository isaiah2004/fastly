# Track D (Traverse) - Rust, std only

No dependencies. Reads `/data/graph.txt` and `/data/queries.txt`, writes `/out/distances.txt`.

## Approach
1. **Parse** (12 threads): mmap the input; each thread parses a line-aligned chunk. AVX2: one
   32-byte compare + movemask finds a line's two spaces and newline; digits are converted with
   SWAR multiplies (scalar fallback elsewhere). Self-loops are dropped. Each edge goes into a
   bucket for the thread that owns its tail (packed into one u64: tail offset, head, 16-bit
   weight), so CSR counting/scattering needs no atomics.
2. **Renumber for locality**: parallel BFS region growing (regions of up to 16k nodes) gives
   nodes that are close in the road network nearby ids; rows are copied while hot. Forward and
   reverse CSRs (u32 head + u16 weight per edge; weights >= 65535 are escaped to a small
   sorted side table) are built in the new numbering.
3. **Queries** (12 threads, work queue, duplicate lines answered once): bidirectional Dijkstra,
   always expanding the side with the smaller heap (so a source whose only out-edges weigh
   ~2^32 does not flood the graph backwards), stop when `top_f + top_b >= best`, and a node is
   pushed only if `dist + top_other < best`. 4-ary heap of packed `(dist << bits(n)) | node` keys.
   Labels of both directions sit side by side in 256-id pages allocated on first touch (memory
   follows the search, not n), stored as u32 with a u64 rerun when a distance reaches 2^32 - 1.
4. Answers are written in query order in one buffer (`-1` for unreachable).

Linux details: big arrays come from mmap (2 MiB aligned, `MADV_HUGEPAGE`), one malloc arena,
a persistent parked thread pool with 1 MiB stacks.

## Build and run
`docker build -t d . && docker run -v <data>:/data:ro -v <out>:/out d`

Native: `cargo build --release && target/release/solution --data DIR --out DIR`
(`DT_TIME=1` prints phase timings to stderr).
