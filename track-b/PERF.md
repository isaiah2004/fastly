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
| 5 | AVX2 key load (one 32-byte load, OR 0x20, AND mask); newline mask from the same pass, so the doc-id check needs no extra load (Ir 559M to 498M) | | ~0.32 | ~85 | |
