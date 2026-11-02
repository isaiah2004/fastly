# Track C: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

Local numbers (T, M) come from `docker run` on our dev laptop (i5-12450H, WSL2, shared with other jobs, so
±20% noise; best of 11-25 runs), on `full`. M is the process's anonymous RSS (steps 1-4 read at exit; steps 5-6 the peak sampled during the run). Board numbers replace them
once measured.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 14.44 | 367 | 0 |
| 1 | Rust rewrite: mmap, 12 threads, speculative chunk starts checked by quote parity, AVX2 bitmask CSV scan, lock-free name interning verified against the file, per-country bitsets | | 0.150 (local) | ~12 (local) | |
| 2 | Name bytes copied to a compact arena next to the interning table (fewer cache misses per record) | | 0.072 (local) | ~12 (local) | |
| 3 | One 64-byte line per distinct name (inline bytes + 256-bit country set), hash + line + region entry software-prefetched 8 records ahead, SWAR number parsing, stream prefetch, regions parsed in parallel | | 0.065 (local) | 11.2 (local) | |
| 4 | Name entries and country bitmaps split into separate tables; region pieces claimed dynamically (no start barrier); 4x smaller record chunks (end-of-run imbalance) | | 0.061-0.069 (local) | 11.3 (local) | |
| 5 | Country bits routed in batches to the thread that owns the name slot and set with plain stores (no `lock or` per record); total CPU time -8 to -13% locally | | 0.065-0.075 (local, noisy) | 13-15 peak (local) | |
| 6 | One glibc malloc arena (`mallopt`), so worker allocations do not create per-thread heaps | run-87334b81831e | 0.0572 | 24.6 | 64.7 |
| 7 | Whole name lookup delegated to the slot owner: parsers send 32-byte (name, country) messages through lock-free batch stacks, owners probe their own slot range (one 64-byte line per name: key + country set) with prefetch, no atomics in the table; threads short of batches help drain; region table without huge-page rounding; 256 KiB thread stacks (a 2 MiB stack can be backed by a whole huge page) | | 0.052-0.063 (local, vs 0.064-0.068 for step 6 in the same session) | 10.7-12.7 peak (local) | |
