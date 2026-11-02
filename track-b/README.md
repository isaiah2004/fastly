# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

One Rust file (`src/main.rs`), std only.

1. **Queries first.** `queries.txt` is tokenized and hashed in parallel. Every distinct query term goes into an open-addressing hash table, and only those terms are ever indexed.
2. **Sample.** About 1 MB spread across `docs.txt` is read to estimate each term's document frequency. BOOL terms that appear in at least 1 in 40 docs get a dense bitmap, and the other BOOL terms get sorted posting lists. Ids are assigned in this order: bitmap terms, then list terms, then phrase-only terms, each group by frequency, so the hot per-term state stays compact. Each phrase gets a "key" term, its rarest one.
3. **Rounds.** Docs are processed in rounds of 65,536 docs. Each round is indexed, its BOOL queries are answered, and the count, the id sum and the first ids are added to each query's running total. Only one round's index is alive at a time, and within a round a doc id fits in 16 bits.
4. **Index a round.** `docs.txt` is memory-mapped. Each round is split across all cores into equal byte ranges, cut at doc ids that are multiples of 64, so every bitmap word has exactly one writer.
   - Tokenizing works line by line (one doc per line). One AVX2 pass per 64-byte block gives the masks of alphanumeric bytes and of newlines. The token boundaries (bit transitions) are flattened into a position array with an unrolled loop, so tokens come out as start/end pairs.
   - Each token of up to 12 bytes becomes 8+4 lowercased key bytes from one SIMD load, then is hashed. A single pass per doc handles each token, prefetching the table slot of the token 16 places ahead: look it up, set the bitmap bit, record the first occurrence of a list term in a stream of 2-byte entries, and note phrase key positions. The bitmap bits of the current 64-doc block go into a small per-thread array (one word per bitmap term) that stays in L1, and each word is written to its row once, when the block ends. Rows are padded by 64 bytes, because with a power-of-two stride every row's current word lands in the same L1 set.
   - Phrases are checked in place, at the positions of their key term.
   - At the end of its range, a worker counting-sorts its stream into per-term 16-bit doc lists.
5. **Answer a round.** The workers' lists are merged into one array per round, in parallel by term range. The same threads then answer the BOOL queries in parallel blocks, folded left to right over sorted lists and bitmaps. A leading run of ANDs commutes, so evaluation starts from its shortest list. AND and NOT of a list filter it in place against the borrowed posting list (galloping when the sizes differ a lot), and an empty result skips the rest of an AND/NOT chain. OR switches to a bitmap when the result gets large. Queries made only of bitmap terms are folded 128 words at a time in a stack buffer, with no allocation. Counts and id sums come from popcounts. Phrase matches are added up per query in the same way. The output is formatted in parallel at the end.

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

- Overlap the queries of one round with the indexing of the next.
- Use dynamic work distribution inside a round, so slow cores don't hold up the barriers (on a laptop with mixed core types, 8-15% of each round is spent waiting).
- Keep one thread pool for the whole run instead of starting threads for each phase.
