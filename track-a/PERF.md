# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

Local numbers come from a laptop: i5-12450H, Docker Desktop on WSL2, a host shared with other jobs, `full` data in a Docker volume. T is the best of 10 to 15 runs, wall time from exec to exit. M is the cgroup `max_usage_in_bytes` minus the container baseline. These local numbers are noisy (±15%) and only show direction. Board numbers go in the Run ID rows.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | Rust v1: mmap, 12 threads, scalar parse, per-thread intern table, `(group,latency)` u64 records, counting-sort scatter, `select_nth_unstable` | | 0.138 (local) | ~44 (local) | |
| 2 | Two-pass design (pass 1 keeps a u16 endpoint id per line, pass 2 re-reads and scatters latencies) to cut memory; AVX2 endpoint scan/hash/compare; SSE latency parse; barrier-synced thread pool; parallel offsets; pwrite output | | 0.075 (local) | ~25 (local) | |
| 3 | Offsets only visit hours each thread saw (u128 hour mask); newline-bitmask line iteration; latency digit count stored in the id's spare bits so pass 2 needs no search | | 0.063 (local) | ~24 (local) | |
| 4 | Output via ftruncate+mmap, threads format straight into the page cache; LUT itoa | | 0.066 (local) | ~23 (local) | |
| 5 | One lock-free intern table shared by all threads (no merge); p50, then p95, then p99 selected on shrinking upper parts; per-thread state aligned to 128 B (removes false sharing); parallel MADV_POPULATE_READ | | 0.053 (local) | ~19 (local) | |
| 6 | Source compacted (thread body in one function, shared context struct), same algorithm; src 21.7 KB to 16.6 KB | | ~0.055 (local, host loaded) | ~20 (local) | |
