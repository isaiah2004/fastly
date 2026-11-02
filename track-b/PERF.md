# Track B: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

T and M below come from a local laptop docker run (i5-12450H, shared with other jobs, so T is noisy), not from the board. The Run ID column is filled in once the board has measured the step. "Ir" is the cachegrind instruction count for the sample set on one thread, which is a steadier measure.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 56.35 | 2,200 | 0 |
| 1 | First correct Rust: mmap, parallel tokenizer, only query terms indexed, bitmaps for frequent terms, sorted lists for the rest, queries answered in parallel | | ~0.60 | ~165 | |
| 2 | Custom open-addressing term table (no HashMap); terms numbered by sampled doc frequency; per-doc batched lookups with prefetch; branch-free appends (Ir 848M to 688M) | | ~0.45 | ~120 | |
| 3 | Postings as a varint byte stream per worker, then a u16 CSR split into 65,536-doc partitions; bitmaps borrowed (not copied) by queries; large freed buffers go back to the OS (mallopt) | | ~0.33 | ~84 | |
| 4 | An asm barrier stops LLVM from scalarizing the 64-bit AVX2 byte-class mask (Ir 688M to 559M) | | ~0.33 | ~84 | |
| 6 | Postings sorted per sub-chunk by the index workers (in cache, no global count/prefix/scatter pass); 16-byte token entries (8+4 key bytes and a 32-bit hash; longer tokens resolved at once) instead of 40 (Ir 496M to 461M) | | ~0.33 | ~67 | |
| 7 | Query file mmap'd and tokenized/hashed in parallel (serial intern only, with prefetch); renumbering by a u64-key unstable sort; BOOL terms numbered before phrase-only terms (compact per-term state); bitmap AND/OR/NOT with a borrowed bitmap in one pass | | ~0.29 | ~68 | |
| 8 | Index tokenizer works per line: one SIMD pass finds the newline, token boundaries flattened into a position array (unrolled), tokens are pairs (no per-token state); one fused per-token pass (lookup, bitmap bit, list dedup and stream, phrase keys); list-term-only per-sub-chunk offsets and 16384-doc sub-chunks; prefetched lookups in the sampling pass (Ir 461M to 427M, single-thread index -7%) | | ~0.28 | ~62 | |
| 5 | AVX2 key load (one 32-byte load, OR 0x20, AND mask); newline mask from the same pass, so the doc-id check needs no extra load (Ir 559M to 498M) | | ~0.32 | ~85 | |
