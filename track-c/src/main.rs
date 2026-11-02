// Speed Demon 2026, Track C (Join). std only; Linux on x86-64 with AVX2, BMI2 and PCLMUL.
use std::arch::x86_64::*;
use std::collections::{HashMap, HashSet};
use std::ptr::null_mut;
use std::sync::atomic::Ordering::*;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize};

const UNM: usize = 255; // group index of _UNMATCHED; countries are 0..255
const NB: u32 = 17; // name table: 2^17 slots
const NS: usize = 1 << NB;

/// Per group: count, sum of ids, sum of cents, min, max, name bytes, note bytes.
#[derive(Clone, Copy)]
struct Agg(u64, u64, i64, i64, i64, u64, u64);
const AGG0: Agg = Agg(0, 0, 0, i64::MAX, i64::MIN, 0, 0);
impl Agg {
    #[inline(always)]
    fn add(&mut self, o: &Agg) {
        (self.0, self.1, self.2) = (self.0 + o.0, self.1.wrapping_add(o.1), self.2.wrapping_add(o.2));
        (self.3, self.4, self.5, self.6) = (self.3.min(o.3), self.4.max(o.4), self.5 + o.5, self.6 + o.6);
    }
}

// ---------- memory ----------
extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    #[cfg(target_env = "gnu")]
    fn mallopt(p: i32, v: i32) -> i32;
}
fn map(path: &str) -> &'static [u8] {
    let f = std::fs::File::open(path).expect("open");
    let n = f.metadata().unwrap().len() as usize;
    let fd = std::os::fd::AsRawFd::as_raw_fd(&f);
    let p = unsafe { mmap(null_mut(), n.max(1), 1, 2, fd, 0) };
    assert!(p as isize != -1, "mmap");
    unsafe { std::slice::from_raw_parts(p, n) }
}
/// Zeroed memory; if `huge`, 2 MiB aligned with transparent huge pages requested (fewer TLB misses
/// on random access; only worth it for tables that fill whole huge pages).
fn zalloc(n: usize, huge: bool) -> *mut u8 {
    let m = if huge { (1 << 21) - 1 } else { 0 };
    let p = unsafe { mmap(null_mut(), n + m, 3, 0x22, -1, 0) };
    assert!(p as isize != -1, "mmap");
    let a = ((p as usize + m) & !m) as *mut u8;
    unsafe { madvise(a, n, if huge { 14 } else { 0 }) };
    a
}

// ---------- small parsers ----------
fn uint(s: &[u8]) -> u64 {
    s.iter().fold(0, |v: u64, &b| v.wrapping_mul(10).wrapping_add(b.wrapping_sub(b'0') as u64))
}
/// Saturates: more than 18 significant digits give u64::MAX, which matches no region.
fn uint_sat(s: &[u8]) -> u64 {
    let t = s.iter().position(|&b| b != b'0').unwrap_or(s.len());
    (s.len() - t <= 18).then(|| uint(&s[t..])).unwrap_or(u64::MAX)
}
/// Decimal digits in d[s..e]: SWAR for up to 8 digits, saturating loop otherwise.
#[inline(always)]
fn num(d: &[u8], s: usize, e: usize) -> u64 {
    let n = e.wrapping_sub(s);
    if n > 8 || e < 8 {
        return uint_sat(d.get(s..e).unwrap_or(&[]));
    }
    let m = HIGH[n];
    let mut x = (rd8(d, e - 8) & m).wrapping_sub(0x3030303030303030 & m);
    x = (x.wrapping_mul(10) + (x >> 8)) & 0x00FF00FF00FF00FF;
    x = (x.wrapping_mul(100) + (x >> 16)) & 0x0000FFFF0000FFFF;
    (x.wrapping_mul(10000) + (x >> 32)) & 0xFFFFFFFF
}
fn cents(s: &[u8]) -> i64 {
    let (neg, s) = s.strip_prefix(b"-").map_or((false, s), |t| (true, t));
    let v = uint(&s.iter().copied().filter(|&b| b != b'.').collect::<Vec<_>>()) as i64;
    v.wrapping_mul(if neg { -1 } else { 1 })
}

// ---------- regions: parsed in parallel into table[id] = country index + 1 (0 = unmatched) ----------
struct Reg<'a> {
    rd: &'a [u8],
    tbl: &'static [AtomicU8],
    cidx: Vec<AtomicU8>,
    codes: Vec<AtomicU32>,
    n: AtomicUsize,
    bad: AtomicBool,
}
fn reg_new(rd: &[u8]) -> Option<Reg<'_>> {
    // ids are ascending (2.3), so the last line holds the largest
    let body = rd.strip_suffix(b"\n")?;
    let last = &body[body.iter().rposition(|&b| b == b'\n')? + 1..];
    let max = Some(uint_sat(&last[..last.iter().position(|&b| b == b',')?]) as usize).filter(|&m| m < 1 << 28)?;
    let tbl = unsafe { std::slice::from_raw_parts(zalloc(max + 1, false) as *const AtomicU8, max + 1) };
    let cidx = (0..26 * 26 * 26).map(|_| AtomicU8::new(0)).collect();
    let codes = (0..256).map(|_| AtomicU32::new(0)).collect();
    let (n, bad) = (AtomicUsize::new(0), AtomicBool::new(false));
    Some(Reg { rd, tbl, cidx, codes, n, bad })
}
impl Reg<'_> {
    /// Parse the lines that start in the ti-th of nt slices; any line not of the form
    /// `digits,ABC\n` sets `bad`.
    fn part(&self, ti: usize, nt: usize) -> Option<()> {
        let d = self.rd;
        let h = d.iter().position(|&b| b == b'\n')? + 1;
        let w = d.len() - h;
        let (mut p, e) = (h + w * ti / nt, h + w * (ti + 1) / nt);
        while p > h && p < e && d[p - 1] != b'\n' {
            p += 1;
        }
        while p < e {
            let mut id = 0;
            while p < d.len() && d[p] != b',' && id < 1 << 28 {
                (id, p) = (id * 10 + (d[p].wrapping_sub(b'0')) as usize, p + 1);
            }
            let c = d.get(p + 1..p + 5).filter(|c| c[..3].iter().all(u8::is_ascii_uppercase) && c[3] == b'\n');
            let c = c.filter(|_| id < self.tbl.len() && d[p] == b',')?;
            let k = ((c[0] - b'A') as usize * 26 + (c[1] - b'A') as usize) * 26 + (c[2] - b'A') as usize;
            let mut v = self.cidx[k].load(Relaxed);
            if v == 0 {
                let n = self.n.fetch_add(1, Relaxed);
                self.codes.get(n).filter(|_| n < UNM)?.store(u32::from_le_bytes([c[0], c[1], c[2], 0]), Relaxed);
                v = self.cidx[k].compare_exchange(0, n as u8 + 1, Relaxed, Relaxed).map_or_else(|x| x, |_| n as u8 + 1);
            }
            self.tbl[id].store(v, Relaxed);
            p += 5;
        }
        Some(())
    }
}

// ---------- name table: one 64-byte slot per distinct name ----------
// The slots are split into nt contiguous ranges. A name's home slot (from its hash) picks the range,
// and only that range's owner thread ever touches it: parsers send each record's (name, group) to the
// owner as a 32-byte message, so the table needs no atomic read-modify-write and stays in the owner's
// caches. slot (one cache line): w0 header (0 empty, else hash32<<32 | len<<10 | 3), w1..w3 name bytes
// (zero padded) or, for names over 24 bytes, w1 = file offset; w4..w7 the 256-bit set of the name's groups.
// message: [header | group<<2, w1, w2, w3]
type Msg = [u64; 4];
#[inline(always)]
fn prefetch<T>(p: &T) {
    unsafe { _mm_prefetch::<_MM_HINT_T0>(p as *const T as *const i8) }
}
#[repr(align(64))]
struct Line([AtomicU64; 8]);
struct Names<'a> {
    d: &'a [u8],
    t: &'static [Line],
    tags: &'static [AtomicU8], // per slot: 0 empty, else 8 hash bits (probed in L1 before the line)
    nt: usize,
    overflow: AtomicBool,
}
#[inline(always)]
fn mum(a: u64, b: u64) -> u64 {
    let r = a as u128 * b as u128;
    (r as u64) ^ ((r >> 64) as u64)
}
/// The 8 bytes at s[i..]; every caller checks i + 8 <= len first.
#[inline(always)]
fn rd8(s: &[u8], i: usize) -> u64 {
    debug_assert!(i + 8 <= s.len());
    unsafe { (s.as_ptr().add(i) as *const u64).read_unaligned() }
}
/// d[i] where the caller has established i < d.len().
#[inline(always)]
fn at(d: &[u8], i: usize) -> u8 {
    debug_assert!(i < d.len());
    unsafe { *d.get_unchecked(i) }
}
fn hash_long(s: &[u8]) -> u64 {
    let (n, mut h, mut i) = (s.len(), 0x243F6A8885A308D3u64, 0);
    while i + 16 < n {
        (h, i) = (mum(h ^ rd8(s, i), rd8(s, i + 8) ^ 0x13198A2E03707344), i + 16);
    }
    mum(h ^ rd8(s, n - 16), rd8(s, n - 8) ^ 0xA4093822299F31D0)
}
/// KEEP[l]: masks keeping the first l bytes of three words; HIGH[n]: mask of the top n bytes of a word.
const KEEP: [[u64; 3]; 25] = {
    let (mut t, mut i) = ([[0u64; 3]; 25], 0);
    while i < 75 {
        let r = (i / 3usize).saturating_sub(8 * (i % 3));
        t[i / 3][i % 3] = if r >= 8 { !0 } else { (1u64 << (8 * r)) - 1 };
        i += 1;
    }
    t
};
const HIGH: [u64; 9] = {
    let (mut t, mut n) = ([0u64; 9], 1);
    while n <= 8 {
        (t[n], n) = (!0u64 << (8 * (8 - n)), n + 1);
    }
    t
};
#[inline(always)]
fn home(m0: u64) -> usize {
    (m0 >> 32) as usize & (NS - 1)
}
#[inline(always)]
fn tag(m0: u64) -> u8 {
    (m0 >> 56) as u8 | 1
}
const LMAX: usize = 1 << 22;
impl<'a> Names<'a> {
    /// The message (group not yet set) for the name at d[o..o + l].
    #[inline(always)]
    fn msg(&self, o: usize, l: usize) -> Msg {
        let d = self.d;
        if l > 24 {
            if l >= LMAX {
                self.overflow.store(true, Relaxed);
            }
            let hd = hash_long(&d[o..o + l]) >> 32 << 32 | ((l % LMAX) as u64) << 10 | 3;
            return [hd, o as u64, 0, 0];
        }
        let w = if o + 24 <= d.len() {
            std::array::from_fn(|i| rd8(d, o + 8 * i) & KEEP[l][i])
        } else {
            let mut b = [0u8; 24];
            b[..l].copy_from_slice(&d[o..o + l]);
            [rd8(&b, 0), rd8(&b, 8), rd8(&b, 16)]
        };
        let a = mum(w[0] ^ 0x9E3779B97F4A7C15, w[1] ^ 0xD6E8FEB86659FD93);
        let h = mum(a ^ w[2], l as u64 ^ 0xA0761D6478BD642F);
        [h >> 32 << 32 | (l as u64) << 10 | 3, w[0], w[1], w[2]]
    }
    #[inline(always)]
    fn owner(&self, m0: u64) -> usize {
        home(m0) * self.nt >> NB
    }
    /// The first slot from m0's home on that is empty or carries its tag. The slots before it hold
    /// other names, and slots are never cleared, so a later probe may start there.
    #[inline(always)]
    fn guess(&self, m0: u64, o: &Own) -> usize {
        let (tg, mut i) = (tag(m0), home(m0));
        while !matches!(self.tags[i].load(Relaxed), x if x == 0 || x == tg) {
            i = if i + 1 == o.hi { o.lo } else { i + 1 };
        }
        i
    }
    /// Owner side: find or insert each message's name in the owner's range and set its group bit.
    fn apply(&self, ms: &[Msg], o: &mut Own) {
        if self.overflow.load(Relaxed) {
            return;
        }
        const PF: usize = 8;
        let mut ring = [0; PF];
        for (j, m) in ms.iter().enumerate().take(PF) {
            ring[j] = self.guess(m[0], o);
            prefetch(&self.t[ring[j]]);
        }
        for (j, m) in ms.iter().enumerate() {
            let mut i = ring[j % PF];
            if let Some(n) = ms.get(j + PF) {
                ring[j % PF] = self.guess(n[0], o);
                prefetch(&self.t[ring[j % PF]]);
            }
            let (g, hdr) = ((m[0] >> 2) as usize & 255, m[0] & !(255 << 2));
            let l = (m[0] >> 10) as usize & (LMAX - 1);
            let tg = tag(m[0]);
            loop {
                let sl = &self.t[i].0;
                let t = self.tags[i].load(Relaxed);
                if t == 0 {
                    o.n += 1;
                    if o.n > (o.hi - o.lo) / 8 * 7 {
                        return self.overflow.store(true, Relaxed);
                    }
                    (1..4).for_each(|k| sl[k].store(m[k], Relaxed));
                    sl[0].store(hdr, Relaxed);
                    self.tags[i].store(tg, Relaxed);
                    break;
                }
                let hd = if t == tg { sl[0].load(Relaxed) } else { 0 };
                let same = |k: usize| sl[k].load(Relaxed) == m[k];
                let long = |p: u64, q: u64| self.d[p as usize..][..l] == self.d[q as usize..][..l];
                if hd == hdr && if l <= 24 { same(1) && same(2) && same(3) } else { long(sl[1].load(Relaxed), m[1]) } {
                    break;
                }
                i += 1;
                if i == o.hi {
                    i = o.lo;
                }
            }
            let (wd, b) = (&self.t[i].0[4 + g / 64], g % 64);
            let w = wd.load(Relaxed);
            o.dist[g] += (!w >> b) as u32 & 1; // a new (name, country) pair
            wd.store(w | 1 << b, Relaxed);
        }
    }
}
/// An owner's slot range [lo, hi), how many of its slots are used, and its distinct names per group.
struct Own {
    lo: usize,
    hi: usize,
    n: usize,
    dist: [u32; 256],
}

// ---------- SIMD block classification ----------
/// Bit masks of '"', ',' and LF in the 64 bytes at p.
#[inline(always)]
unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
    let (a, b) = (_mm256_loadu_si256(p as *const __m256i), _mm256_loadu_si256(p.add(32) as *const __m256i));
    let m = |c: u8| {
        let s = _mm256_set1_epi8(c as i8);
        let (x, y) = (_mm256_movemask_epi8(_mm256_cmpeq_epi8(a, s)), _mm256_movemask_epi8(_mm256_cmpeq_epi8(b, s)));
        x as u32 as u64 | (y as u32 as u64) << 32
    };
    (m(b'"'), m(b','), m(b'\n'))
}
/// Bit i set iff an odd number of bits 0..=i of x are set (carry-less multiply by all ones).
#[inline(always)]
fn prefix_xor(x: u64) -> u64 {
    unsafe { _mm_cvtsi128_si64(_mm_clmulepi64_si128::<0>(_mm_set_epi64x(0, x as i64), _mm_set1_epi8(-1))) as u64 }
}
#[inline(always)]
fn low(n: usize) -> u64 {
    if n >= 64 { !0 } else { (1u64 << n) - 1 }
}

const STAGE: usize = 128;
const PD: usize = 1024; // stream prefetch distance
/// A parsed record, staged (while its region entry is prefetched) until STAGE of them are aggregated.
#[derive(Clone, Copy, Default)]
struct Rec {
    id: u64,
    c: i64,
    ko: usize,
    kl: u32,
    nb: u32,
    tb: u32,
    reg: u32,
}
struct Stage {
    r: [Rec; STAGE],
    n: usize,
}
const BATCH: usize = 64;
/// Messages for one owner; `from` is the producer, to which the owner hands the batch back for reuse.
struct Batch {
    next: *mut Batch,
    from: usize,
    n: usize,
    m: [Msg; BATCH],
}
/// Lock-free stack of batches: many pushers, one taker that always takes everything (so no ABA).
#[repr(align(128))]
struct Stack(AtomicPtr<Batch>);
impl Stack {
    fn push(&self, b: *mut Batch) {
        let mut h = self.0.load(Relaxed);
        loop {
            unsafe { (*b).next = h };
            match self.0.compare_exchange_weak(h, b, Release, Relaxed) {
                Ok(_) => return,
                Err(x) => h = x,
            }
        }
    }
    fn take(&self) -> *mut Batch {
        if self.0.load(Relaxed).is_null() { null_mut() } else { self.0.swap(null_mut(), Acquire) }
    }
}
/// A slot range with its fill count. Whoever holds `lock` may write the range: normally its owner
/// thread, but a thread that waits for batches helps by applying another owner's backlog.
#[repr(align(128))]
struct Part {
    lock: AtomicBool,
    own: std::cell::UnsafeCell<Own>,
}
unsafe impl Sync for Part {}
struct Ctx<'a> {
    d: &'a [u8],
    table: &'a [AtomicU8],
    names: &'a Names<'a>,
    inbox: &'a [Stack],
    spare: &'a [Stack],
    parts: &'a [Part],
}
/// Per-thread state: aggregates, the open batch for each slot owner, free batches.
struct St {
    agg: Box<[Agg; 256]>,
    me: usize,
    out: Vec<*mut Batch>,
    free: Vec<*mut Batch>,
    nb: usize,
}
impl Ctx<'_> {
    /// An empty batch. A thread has at most 4 per owner (bounded memory); when all are in flight it
    /// helps apply waiting batches until some come back.
    fn fresh(&self, st: &mut St) -> *mut Batch {
        loop {
            if let Some(b) = st.free.pop() {
                return b;
            }
            let mut b = self.spare[st.me].take();
            while !b.is_null() {
                st.free.push(b);
                b = unsafe { (*b).next };
            }
            if !st.free.is_empty() {
                continue;
            }
            if st.nb < 4 * self.parts.len() {
                st.nb += 1;
                return Box::into_raw(Box::new(Batch { next: null_mut(), from: st.me, n: 0, m: [[0; 4]; BATCH] }));
            }
            if !self.help() {
                std::thread::yield_now();
            }
        }
    }
    /// Hand the open batch for owner o over and start a new one.
    fn post(&self, st: &mut St, o: usize) {
        if unsafe { (*st.out[o]).n } > 0 {
            self.inbox[o].push(st.out[o]);
            st.out[o] = self.fresh(st);
        }
    }
    /// Apply the batches waiting for owner o, unless another thread is doing so; true if any were.
    fn drain(&self, o: usize, wait: bool) -> bool {
        let pt = &self.parts[o];
        while self.inbox[o].0.load(Relaxed).is_null() && !wait || pt.lock.swap(true, Acquire) {
            if !wait {
                return false;
            }
            std::thread::yield_now();
        }
        let mut b = self.inbox[o].take();
        let any = !b.is_null();
        while let Some(x) = unsafe { b.as_mut() } {
            self.names.apply(&x.m[..x.n], unsafe { &mut *pt.own.get() });
            (x.n, b) = (0, x.next);
            self.spare[x.from].push(x);
        }
        pt.lock.store(false, Release);
        any
    }
    fn help(&self) -> bool {
        (0..self.parts.len()).fold(false, |a, o| self.drain(o, false) | a)
    }
}

/// Aggregate the staged records and send their (name, country) messages to the slot owners.
fn flush(cx: &Ctx, st: &mut St, sg: &mut Stage) {
    for p in &sg.r[..sg.n] {
        let r = p.reg as usize;
        let g = if r < cx.table.len() { cx.table[r].load(Relaxed).wrapping_sub(1) } else { UNM as u8 };
        st.agg[g as usize].add(&Agg(1, p.id, p.c, p.c, p.c, p.nb as u64, p.tb as u64));
        let m = cx.names.msg(p.ko, p.kl as usize);
        let o = cx.names.owner(m[0]); // < nt = out.len()
        let b = unsafe { &mut **st.out.get_unchecked(o) };
        // b.n < BATCH: a full batch is posted at once
        unsafe { *b.m.get_unchecked_mut(b.n) = [m[0] | (g as u64) << 2, m[1], m[2], m[3]] };
        b.n += 1;
        if b.n == BATCH {
            cx.post(st, o);
            cx.drain(st.me, false); // keeps the backlog small
        }
    }
    sg.n = 0;
}

/// One record: start rs, field separators f[0..4], terminator (LF or EOF) f[4]; q and tq are the
/// numbers of '"' in the name and note fields.
#[inline(always)]
fn record(cx: &Ctx, st: &mut St, sg: &mut Stage, rs: usize, f: &[usize; 5], q: usize, tq: usize) {
    let d = cx.d;
    // rs <= f[0] < f[1] < .. < f[4] <= len (separator positions from the scan)
    let a = rs + if at(d, rs) == 0xEF { 3 } else { 0 };
    let id = num(d, a, f[0]);
    // value length = raw length - delimiting quotes - one per "" pair
    let nl = f[1] - f[0] - 1;
    let (ko, kl) = if q > 0 { (f[0] + 2, nl.saturating_sub(2)) } else { (f[0] + 1, nl) };
    let nb = kl.saturating_sub(q.saturating_sub(2) / 2) as u64;
    let tl = f[2] - f[1] - 1;
    let tb = if tq > 0 { tl.saturating_sub(2 + (tq - 2) / 2) } else { tl } as u64;
    let reg = num(d, f[2] + 1, f[3]).min(u32::MAX as u64) as usize;
    if reg < cx.table.len() {
        prefetch(&cx.table[reg]);
    }
    let (end, s) = (f[4], f[3] + 1);
    let e = end - (at(d, end - 1) == 13) as usize;
    let neg = s < e && at(d, s) == b'-';
    let c = if e >= s + 4 + neg as usize && at(d, e - 3) == b'.' {
        let x = at(d, e - 2).wrapping_sub(b'0') as u64;
        let y = at(d, e - 1).wrapping_sub(b'0') as u64;
        let v = (num(d, s + neg as usize, e - 3) * 100 + x * 10 + y) as i64;
        if neg { -v } else { v }
    } else {
        cents(d.get(s..e).unwrap_or(&[]))
    };
    if kl >= LMAX {
        cx.names.overflow.store(true, Relaxed);
    }
    sg.r[sg.n] = Rec { id, c, ko, kl: kl.min(LMAX) as u32, nb: nb as u32, tb: tb as u32, reg: reg as u32 };
    sg.n += 1;
    if sg.n == STAGE {
        flush(cx, st, sg);
    }
}

/// Separator (comma or LF outside quotes) positions, block by block, from a record start r0.
struct Scan<'a> {
    d: &'a [u8],
    e: usize,
    base: usize,
    s: u64,
    q: u64,
    qb: u64,
    cumq: u64,
    carry: u64,
    qe: u64,
}
impl Scan<'_> {
    #[inline(always)]
    fn block(&mut self) {
        let (d, base, len) = (self.d, self.base, self.d.len());
        let (q, c, l) = if base + 64 <= len {
            if base + PD < len {
                prefetch(&d[base + PD]);
            }
            unsafe { masks(d.as_ptr().add(base)) }
        } else {
            let mut buf = [0u8; 64];
            buf[..len - base].copy_from_slice(&d[base..]);
            unsafe { masks(buf.as_ptr()) }
        };
        self.qb = self.cumq;
        if self.e <= base + 64 && self.e > base {
            self.qe = self.qb + (q & low(self.e - base)).count_ones() as u64;
        }
        self.cumq += q.count_ones() as u64;
        let inq = prefix_xor(q) ^ self.carry;
        (self.carry, self.q, self.s) = (((inq as i64) >> 63) as u64, q, (c | l) & !inq);
    }
    #[inline(always)]
    fn next(&mut self) -> Option<usize> {
        while self.s == 0 {
            self.base += 64;
            if self.base >= self.d.len() {
                return None;
            }
            self.block();
        }
        let t = self.s.trailing_zeros() as usize;
        self.s &= self.s - 1;
        Some(self.base + t)
    }
    /// Quotes before position p (in the current block), counted from r0.
    #[inline(always)]
    fn quotes(&self, p: usize) -> usize {
        (self.qb + (self.q & low(p - self.base)).count_ones() as u64) as usize
    }
}

/// Parse every record whose start lies in [r0, e), r0 being a record start outside quotes.
/// Returns the number of quote bytes in [r0, e).
fn parse_from(cx: &Ctx, st: &mut St, r0: usize, e: usize) -> u64 {
    let (d, len) = (cx.d, cx.d.len());
    let mut sc = Scan { d, e, base: r0, s: 0, q: 0, qb: 0, cumq: 0, carry: 0, qe: 0 };
    sc.block();
    let (mut rs, mut sg) = (r0, Stage { r: [Rec::default(); STAGE], n: 0 });
    while let Some(f0) = sc.next() {
        let q0 = sc.quotes(f0);
        let Some(f1) = sc.next() else { break };
        let q1 = sc.quotes(f1);
        let Some(f2) = sc.next() else { break };
        let q2 = sc.quotes(f2);
        let Some(f3) = sc.next() else { break };
        let Some(f4) = sc.next() else {
            // a final record without terminator ends at EOF (only if EOF is outside quotes)
            if rs < e && rs < len && d[len - 1] != b'\n' && sc.carry == 0 {
                record(cx, st, &mut sg, rs, &[f0, f1, f2, f3, len], q1 - q0, q2 - q1);
            }
            break;
        };
        record(cx, st, &mut sg, rs, &[f0, f1, f2, f3, f4], q1 - q0, q2 - q1);
        rs = f4 + 1;
        if rs >= e {
            break;
        }
    }
    flush(cx, st, &mut sg);
    sc.qe
}

// ---------- fast parallel path ----------
// Outside quotes <=> an even number of '"' precede the byte (by the grammar '"' only occurs in quoted
// fields). Each chunk guesses its first record start (an LF followed by a digit or a BOM) and assumes
// it is outside quotes; afterwards the guess is checked exactly against the prefix quote parity.
// Any wrong guess, or a name table overflow, falls back to the sequential exact parser.
type Rows = Vec<(Vec<u8>, Agg, u64)>;
fn fast(d: &[u8], rd: &[u8], nt: usize, nch: usize) -> Option<Rows> {
    let len = d.len();
    let rg = reg_new(rd).filter(|_| len < u32::MAX as usize)?; // staged lengths are 32-bit
    let t = unsafe { std::slice::from_raw_parts(zalloc(NS * 64, true) as *const Line, NS) };
    let overflow = AtomicBool::new(false);
    let tags = unsafe { std::slice::from_raw_parts(zalloc(NS, false) as *const AtomicU8, NS) };
    let names = Names { d, t, tags, nt, overflow };
    let stk = || Vec::from_iter((0..nt).map(|_| Stack(AtomicPtr::default())));
    let (inbox, spare) = (stk(), stk());
    // owner o has exactly the slots i with i * nt >> NB == o
    let range = |o: usize| ((o * NS).div_ceil(nt), ((o + 1) * NS).div_ceil(nt));
    let own = |(lo, hi)| Part { lock: AtomicBool::new(false), own: Own { lo, hi, n: 0, dist: [0; 256] }.into() };
    let parts: Vec<Part> = (0..nt).map(|o| own(range(o))).collect();
    let cx = Ctx { d, table: rg.tbl, names: &names, inbox: &inbox, spare: &spare, parts: &parts };
    let auto = (nt * 32).min(len / 4096 + 1);
    let nch = if nch > 0 { nch } else { auto }.min(len.max(1));
    // per chunk: quotes in chunk << 2 | checked << 1 | implied parity
    let res: Vec<AtomicU64> = (0..nch).map(|_| AtomicU64::new(0)).collect();
    let hdr = d.iter().position(|&b| b == b'\n').map_or(len, |x| x + 1);
    const RP: usize = 64;
    let z = || AtomicUsize::new(0);
    let (next, done, rnext, rdone) = (z(), z(), z(), z());
    let work = |ti: usize| {
        // region pieces are claimed dynamically, so early threads do not wait for late spawns
        for i in std::iter::from_fn(|| Some(rnext.fetch_add(1, Relaxed))).take_while(|&i| i < RP) {
            rg.bad.fetch_or(rg.part(i, RP).is_none(), Relaxed);
            rdone.fetch_add(1, Release);
        }
        while rdone.load(Acquire) < RP {
            std::hint::spin_loop();
        }
        if ti == 0 {
            unsafe { madvise(rd.as_ptr() as *mut u8, rd.len(), 4) }; // the regions file is parsed
        }
        let mut st = St { agg: Box::new([AGG0; 256]), me: ti, out: vec![], free: vec![], nb: 0 };
        st.out = (0..nt).map(|_| cx.fresh(&mut st)).collect();
        for i in std::iter::from_fn(|| Some(next.fetch_add(1, Relaxed))).take_while(|&i| i < nch) {
            let (s, e) = (i * len / nch, (i + 1) * len / nch);
            let (mut qpre, mut r, mut chk) = (0u64, s, 0);
            if i == 0 {
                (qpre, r) = (d[..hdr].iter().filter(|&&b| b == b'"').count() as u64, hdr);
            }
            while i > 0 && r < e {
                if d[r - 1] == b'\n' && (d[r].is_ascii_digit() || d[r] == 0xEF) {
                    chk = 2 | (qpre & 1);
                    break;
                }
                (qpre, r) = (qpre + (d[r] == b'"') as u64, r + 1);
            }
            let qt = qpre + (r < e).then(|| parse_from(&cx, &mut st, r, e)).unwrap_or(0);
            res[i].store(qt << 2 | chk, Relaxed);
            // unmap the chunk's pages now, in parallel, instead of all at exit (MADV_DONTNEED; a later
            // read, e.g. of a record running past e, just maps the page again)
            let (a, b) = ((s + 4095) & !4095, e & !4095);
            unsafe { madvise(d.as_ptr().wrapping_add(a) as *mut u8, b.saturating_sub(a), 4) };
            cx.drain(ti, false);
        }
        (0..nt).for_each(|o| cx.post(&mut st, o));
        // keep serving the mailbox until every thread has sent everything
        done.fetch_add(1, Release);
        while done.load(Acquire) < nt {
            if !cx.help() {
                std::thread::yield_now();
            }
        }
        // everything is posted now; after this drain the range is final
        cx.drain(ti, true);
        st.agg
    };
    let all: Vec<Box<[Agg; 256]>> = std::thread::scope(|sc| {
        // small stacks: a 2 MiB stack could be backed by a whole huge page
        let spawn = |ti| std::thread::Builder::new().stack_size(256 << 10).spawn_scoped(sc, move || work(ti));
        let hs: Vec<_> = (0..nt).map(|ti| spawn(ti).expect("spawn")).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut p = 0u64;
    for v in res.iter().map(|r| r.load(Relaxed)) {
        if v & 2 != 0 && (v & 1) != p || names.overflow.load(Relaxed) || rg.bad.load(Relaxed) {
            return None;
        }
        p ^= (v >> 2) & 1;
    }
    let code = |g: usize| rg.codes[g].load(Relaxed).to_le_bytes()[..3].to_vec();
    let mut rows: Rows = (0..256).map(|g| (code(g), AGG0, 0)).collect();
    rows[UNM].0 = b"_UNMATCHED".to_vec();
    for (g, row) in rows.iter_mut().enumerate() {
        all.iter().for_each(|a| row.1.add(&a[g]));
        row.2 = parts.iter().map(|p| unsafe { &*p.own.get() }.dist[g] as u64).sum();
    }
    Some(rows)
}

// ---------- slow, simple, exact sequential path (fallback and cross-check) ----------
fn slow(d: &[u8], rd: &[u8]) -> Rows {
    let mut reg: HashMap<u64, &[u8]> = HashMap::new();
    for line in rd.split(|&b| b == b'\n').skip(1) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(c) = line.iter().position(|&b| b == b',') {
            reg.insert(uint_sat(&line[..c]), &line[c + 1..]);
        }
    }
    let mut groups: HashMap<&[u8], (Agg, HashSet<&[u8]>)> = HashMap::new();
    let (len, mut p, mut n) = (d.len(), 0, 0);
    while p < len {
        p += 3 * d[p..].starts_with(&[0xEF, 0xBB, 0xBF]) as usize;
        let mut fl: [(&[u8], usize); 5] = [(&[], 0); 5]; // (raw inner bytes, unescaped length)
        for f in &mut fl {
            let st = p;
            if p < len && d[p] == b'"' {
                // a quoted field ends at a '"' not followed by another; "" pairs are escapes
                let (mut i, mut dq) = (p + 1, 0);
                while i < len && (d[i] != b'"' || i + 1 < len && d[i + 1] == b'"') {
                    let x = (d[i] == b'"') as usize;
                    (i, dq) = (i + 1 + x, dq + x);
                }
                (*f, p) = ((&d[st + 1..i.min(len)], i.min(len) - st - 1 - dq), i + 1);
            } else {
                while p < len && !matches!(d[p], b',' | b'\r' | b'\n') {
                    p += 1;
                }
                *f = (&d[st..p], p - st);
            }
            p += 1;
        }
        // p is one past the amount's terminator: step back over it unless it is CR LF or LF
        p += (p <= len && d[p - 1] == b'\r' && p < len && d[p] == b'\n') as usize;
        p -= (p <= len && !matches!(d[p - 1], b'\n' | b'\r')) as usize;
        n += 1;
        if n == 1 {
            continue; // the header
        }
        let key: &[u8] = reg.get(&uint_sat(fl[3].0)).map_or(b"_UNMATCHED", |k| k);
        let e = groups.entry(key).or_insert_with(|| (AGG0, HashSet::new()));
        let c = cents(fl[4].0);
        let (id, nb, tb) = (uint(fl[0].0), fl[1].1 as u64, fl[2].1 as u64);
        e.0.add(&Agg(1, id, c, c, c, nb, tb));
        e.1.insert(fl[1].0);
    }
    groups.into_iter().map(|(k, (a, s))| (k.to_vec(), a, s.len() as u64)).collect()
}

fn main() {
    // one malloc arena: worker threads allocate little, and per-thread arenas would inflate peak memory
    #[cfg(target_env = "gnu")]
    unsafe { mallopt(-8, 1) };
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| Some(args[args.iter().position(|a| a == k)? + 1].clone());
    let data = arg("--data").unwrap_or("/data".into());
    let out = arg("--out").unwrap_or("/out".into());
    let cpus = std::thread::available_parallelism().map_or(12, |n| n.get());
    let nt = arg("--threads").map_or(cpus, |t| t.parse().unwrap()).clamp(1, 256);
    let nch = arg("--chunks").map_or(0, |c| c.parse().unwrap());
    let (d, rd) = (map(&format!("{data}/records.csv")), map(&format!("{data}/regions.csv")));
    let slow_only = args.iter().any(|a| a == "--slow");
    let mut rows = (!slow_only).then(|| fast(d, rd, nt, nch)).flatten().unwrap_or_else(|| {
        eprintln!("note: exact sequential path");
        slow(d, rd)
    });
    rows.retain(|r| r.1 .0 > 0);
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let money = |c: i64| {
        let (s, u) = (if c < 0 { "-" } else { "" }, c.unsigned_abs());
        format!("{s}{}.{:02}", u / 100, u % 100)
    };
    let mut o = String::new();
    for (k, Agg(c, ids, s, lo, hi, nb, tb), n) in rows {
        let (k, s, lo, hi) = (String::from_utf8_lossy(&k), money(s), money(lo), money(hi));
        o += &format!("{k} {c} {ids} {s} {lo} {hi} {n} {nb} {tb}\n");
    }
    std::fs::write(format!("{out}/aggregates.txt"), o).expect("write");
}
