# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

One Rust file (`src/main.rs`), std only.

1. **Queries first.** Parse `queries.txt` and put every distinct query term into an open-addressing hash table. Only those terms are ever indexed.
2. **Sample.** Read about 2 MB spread across `docs.txt` to estimate each term's document frequency. Terms that appear in at least 1 in 40 docs (and are used by BOOL queries) get a dense bitmap. Every other term gets a sorted posting list. Terms are numbered by frequency so the hot data stays compact in cache. Each phrase gets a "key" term: its rarest term.
3. **Index.** `docs.txt` is memory-mapped and split across all cores. Chunk boundaries fall on doc ids that are multiples of 64, so every bitmap word has exactly one writer.
   - Tokenizing uses AVX2. Each 64-byte block yields one mask of alphanumeric bytes and one of newlines. Token starts and ends are the bit transitions in the alphanumeric mask.
   - Each token is lowercased and padded to 32 bytes in a single SIMD load, then hashed.
   - Per document, all slot lookups are prefetched first, then resolved. Each hit is sorted without branches into bitmap terms, list terms and phrase keys.
   - Bitmap terms set their bit directly.
   - List terms are deduplicated per doc and appended to a per-worker varint byte stream.
   - Phrases are checked in place, at the positions of their key term.
4. **Postings.** The worker streams are counted and scattered in parallel into a compact array of 16-bit doc ids, split into 65,536-doc partitions.
5. **Answer.** Queries are answered in parallel blocks. BOOL queries are folded left to right over sorted lists and bitmaps: AND uses galloping intersection, OR switches to a bitmap when the result gets large, and NOT is a difference. Counts and id sums come from popcount on the bitmaps. Output blocks are written in order.

On Linux the program uses `mmap` and `mallopt` through `extern "C"`. Other platforms fall back to `std::fs::read`.

## Dependencies

None (std only).

## Build and run

```
docker build -t track-b .
docker run --rm -v /path/to/data:/data:ro -v /path/to/out:/out track-b
# native: cargo build --release && target/release/solution --data DIR --out DIR
```

## With another 24 hours

- Build the postings and answer the queries partition by partition. That would lower peak memory and keep each partition's data in cache.
- Parallelize the remaining serial prefix sums in the postings step.
- Make the query parse faster.
- Use a smaller hash slot, or a two-level filter, to cut cache misses on token lookups.
