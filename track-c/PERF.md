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
| 7 | Whole name lookup delegated to the slot owner: parsers send 32-byte (name, country) messages through lock-free batch stacks, owners probe their own slot range (one 64-byte line per name: key + country set) with prefetch, no atomics in the table; threads short of batches help drain; region table without huge-page rounding; 256 KiB thread stacks (a 2 MiB stack can be backed by a whole huge page) | run-c7aab8f5c5af | 0.0445 | 13.0 | 69.7 |
| 8 | Name-word and digit masks from small constant tables instead of computed shifts and branches: instructions per record 683 -> 657 (callgrind, `sample`, 1 thread) | | within local noise | same | |
| 9 | Scanner yields separator positions one 64-byte block at a time, with per-field quote counts from the block's quote mask; PCLMUL prefix-xor for the in-quotes mask; unchecked byte reads where the scan already proves the bounds; a 128-record stage (region entry prefetched at parse time) replaces the 8-record ring. Instructions per record on `full`, 1 thread: 603 (step 7) -> 505 | | not resolvable locally (host load 5-9) | same | |
| 10 | Source size: non-Linux fallbacks removed (the binary only targets Linux x86-64 with AVX2/BMI2/PCLMUL), cold code (CLI, output, regions, sequential path) written compactly. Code lines after rustfmt 1,103 -> 810 (our count); instructions per record 512 | | same as step 9 | same | |
| 11 | `names_distinct` counted while the owner sets a group bit (a bit that was 0 is a new pair) instead of a popcount pass over the table at the end: instructions per record 512 -> 509, simulated branch misses on `sample` -28% | | same | same | |
| 12 | Each finished chunk's pages are dropped with `MADV_DONTNEED` by the thread that parsed it, so the page-table teardown runs in parallel instead of in `exit`: exit cost 3.1 -> 1.8 ms locally (time from the final write to the parent seeing the exit, min of 15) | | same | same | |

## Tried and rejected (local measurements)

- **Shared name table with `fetch_or` group bits** (no messages; 478 instructions per record): 20% slower than step 9 at 12 threads, and slower at 4 threads. Lines written by `lock or` on every record bounce between cores.
- **Shared intern table giving dense ids + 4-byte (id, group) messages over SPSC rings to bitmap owners**: 529 instructions per record and the slowest variant (83 ms vs 56 ms at 12 threads, 97 vs 76 at 4). The owner model keeps each slot range in its owner's L2; the shared table does not.
- **Two-pass scan** (all separator positions of a 4 KiB window written with unconditional stores, then 5 per record): 0.9 fewer simulated branch misses per record but more instructions, and slower in every timing session.
- **PREFETCHW** for the name line, **non-temporal stream prefetch**: no gain.
- Ablation (step 9, 4 threads, laptop): parsing only 33 ms, plus message passing 44 ms, plus owner table updates 73 ms. The name table is the memory-bound part.
