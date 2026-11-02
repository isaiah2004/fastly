// Speed Demon 2026, Track C (Join). std only.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering::*};

const UNM: usize = 255; // group index of _UNMATCHED; countries are 0..255
const NB: u32 = 17; // name table: 2^17 slots
const NS: usize = 1 << NB;

#[derive(Clone, Copy)]
struct Agg { cnt: u64, ids: u64, sum: i64, min: i64, max: i64, nb: u64, tb: u64 }
const AGG0: Agg = Agg { cnt: 0, ids: 0, sum: 0, min: i64::MAX, max: i64::MIN, nb: 0, tb: 0 };
impl Agg {
    #[inline(always)]
    fn add(&mut self, id: u64, c: i64, nb: u64, tb: u64) {
        self.cnt += 1; self.ids = self.ids.wrapping_add(id); self.sum = self.sum.wrapping_add(c);
        self.min = self.min.min(c); self.max = self.max.max(c); self.nb += nb; self.tb += tb;
    }
    fn merge(&mut self, o: &Agg) {
        self.cnt += o.cnt; self.ids = self.ids.wrapping_add(o.ids); self.sum = self.sum.wrapping_add(o.sum);
        self.min = self.min.min(o.min); self.max = self.max.max(o.max); self.nb += o.nb; self.tb += o.tb;
    }
}

// ---------- input ----------
#[cfg(target_os = "linux")]
fn map(path: &str) -> &'static [u8] {
    use std::os::unix::io::AsRawFd;
    extern "C" { fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8; }
    let f = std::fs::File::open(path).expect("open");
    let n = f.metadata().unwrap().len() as usize;
    if n == 0 { return &[]; }
    let p = unsafe { mmap(std::ptr::null_mut(), n, 1, 2, f.as_raw_fd(), 0) };
    assert!(p as isize != -1, "mmap");
    unsafe { std::slice::from_raw_parts(p, n) }
}
/// Zeroed memory; if `huge`, 2 MiB aligned with transparent huge pages requested (fewer TLB misses
/// on random access; only worth it for tables that fill whole huge pages).
#[cfg(target_os = "linux")]
fn zalloc(n: usize, huge: bool) -> *mut u8 {
    extern "C" {
        fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
        fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    }
    const H: usize = 1 << 21;
    if !huge {
        let p = unsafe { mmap(std::ptr::null_mut(), n.max(1), 3, 0x22, -1, 0) };
        assert!(p as isize != -1, "mmap");
        return p;
    }
    let n = (n + H - 1) & !(H - 1);
    unsafe {
        let p = mmap(std::ptr::null_mut(), n + H, 3, 0x22, -1, 0);
        assert!(p as isize != -1, "mmap");
        let a = ((p as usize + H - 1) & !(H - 1)) as *mut u8;
        madvise(a, n, 14);
        a
    }
}
#[cfg(not(target_os = "linux"))]
fn zalloc(n: usize, _huge: bool) -> *mut u8 {
    let l = std::alloc::Layout::from_size_align(n.max(1), 64).unwrap();
    let p = unsafe { std::alloc::alloc_zeroed(l) };
    if p.is_null() { std::alloc::handle_alloc_error(l) }
    p
}
#[cfg(not(target_os = "linux"))]
fn map(path: &str) -> &'static [u8] { Box::leak(std::fs::read(path).expect("read").into_boxed_slice()) }

// ---------- small parsers ----------
#[inline(always)]
fn uint(s: &[u8]) -> u64 {
    let mut v = 0u64;
    for &b in s { v = v.wrapping_mul(10).wrapping_add((b.wrapping_sub(b'0')) as u64); }
    v
}
#[inline(always)]
fn uint_sat(s: &[u8]) -> u64 {
    if s.len() > 18 {
        let t = s.iter().position(|&b| b != b'0').unwrap_or(s.len());
        if s.len() - t > 18 { return u64::MAX; }
        return uint(&s[t..]);
    }
    uint(s)
}
/// Decimal digits in d[s..e]: SWAR for up to 8 digits, saturating loop otherwise.
#[inline(always)]
fn num(d: &[u8], s: usize, e: usize) -> u64 {
    let n = e.wrapping_sub(s);
    if n <= 8 && e >= 8 {
        let m = !bytemask(8 - n);
        let mut x = (rd8(d, e - 8) & m).wrapping_sub(0x3030303030303030 & m);
        x = (x.wrapping_mul(10) + (x >> 8)) & 0x00FF00FF00FF00FF;
        x = (x.wrapping_mul(100) + (x >> 16)) & 0x0000FFFF0000FFFF;
        (x.wrapping_mul(10000) + (x >> 32)) & 0xFFFFFFFF
    } else { uint_sat(d.get(s..e).unwrap_or(&[])) }
}
#[inline(always)]
fn cents(s: &[u8]) -> i64 {
    let (neg, s) = match s.first() { Some(b'-') => (true, &s[1..]), _ => (false, s) };
    let mut v = 0i64;
    for &b in s { if b != b'.' { v = v.wrapping_mul(10).wrapping_add((b.wrapping_sub(b'0')) as i64); } }
    if neg { -v } else { v }
}

// ---------- regions: parsed in parallel into table[id] = country index + 1 (0 = unmatched) ----------
struct Reg<'a> { rd: &'a [u8], tbl: &'static [AtomicU8], cidx: Vec<AtomicU8>, codes: Vec<AtomicU32>, n: AtomicUsize, bad: AtomicBool }
fn reg_new(rd: &[u8]) -> Option<Reg<'_>> {
    // ids are ascending (2.3), so the last line holds the largest
    let body = rd.strip_suffix(b"\n")?;
    let last = &body[body.iter().rposition(|&b| b == b'\n')? + 1..];
    let max = uint_sat(&last[..last.iter().position(|&b| b == b',')?]) as usize;
    if max >= 1 << 28 { return None; }
    let tbl = unsafe { std::slice::from_raw_parts(zalloc(max + 1, false) as *const AtomicU8, max + 1) };
    Some(Reg { rd, tbl, cidx: (0..26 * 26 * 26).map(|_| AtomicU8::new(0)).collect(), codes: (0..256).map(|_| AtomicU32::new(0)).collect(), n: AtomicUsize::new(0), bad: AtomicBool::new(false) })
}
impl Reg<'_> {
    /// Parse the lines that start in the ti-th of nt slices.
    fn part(&self, ti: usize, nt: usize) {
        let d = self.rd;
        let h = d.iter().position(|&b| b == b'\n').map_or(d.len(), |x| x + 1);
        let (s, e) = (h + (d.len() - h) * ti / nt, h + (d.len() - h) * (ti + 1) / nt);
        let mut p = s;
        while p > h && p < e && d[p - 1] != b'\n' { p += 1; }
        while p < e {
            let mut id = 0usize;
            while p < d.len() && d[p] != b',' { id = id * 10 + (d[p].wrapping_sub(b'0')) as usize; p += 1; if id >= 1 << 28 { break; } }
            let c = match d.get(p + 1..p + 4) { Some(c) => c, None => { self.bad.store(true, Relaxed); return; } };
            if id >= self.tbl.len() || !c.iter().all(|b| b.is_ascii_uppercase()) || d.get(p + 4) != Some(&b'\n') { self.bad.store(true, Relaxed); return; }
            p += 5;
            let k = ((c[0] - b'A') as usize * 26 + (c[1] - b'A') as usize) * 26 + (c[2] - b'A') as usize;
            let mut v = self.cidx[k].load(Relaxed);
            if v == 0 {
                let n = self.n.fetch_add(1, Relaxed);
                if n >= UNM { self.bad.store(true, Relaxed); return; }
                self.codes[n].store(u32::from_le_bytes([c[0], c[1], c[2], 0]), Relaxed);
                v = match self.cidx[k].compare_exchange(0, n as u8 + 1, Relaxed, Relaxed) { Ok(_) => n as u8 + 1, Err(cur) => cur };
            }
            self.tbl[id].store(v, Relaxed);
        }
    }
}

// ---------- name table: one 32-byte slot per distinct name ----------
// The slots are split into nt contiguous ranges. A name's home slot (from its hash) picks the range,
// and only that range's owner thread ever touches it: parsers send each record's (name, group) to the
// owner as a 32-byte message, so the table needs no atomic read-modify-write and stays in the owner's
// caches. slot (one cache line): w0 header (0 empty, else hash32<<32 | len<<10 | 3), w1..w3 name bytes (zero padded)
// or, for names over 24 bytes, w1 = file offset; w4..w7 the 256-bit set of the name's groups.
// message: [header | group<<2, w1, w2, w3]
type Msg = [u64; 4];
#[inline(always)]
fn prefetch<T>(p: &T) {
    #[cfg(target_arch = "x86_64")]
    unsafe { std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(p as *const T as *const i8) }
}
#[repr(align(64))]
struct Line([AtomicU64; 8]);
struct Names<'a> { d: &'a [u8], t: &'static [Line], nt: usize, overflow: AtomicBool }
#[inline(always)]
fn mum(a: u64, b: u64) -> u64 { let r = a as u128 * b as u128; (r as u64) ^ ((r >> 64) as u64) }
#[inline(always)]
fn rd8(s: &[u8], i: usize) -> u64 { u64::from_le_bytes(s[i..i + 8].try_into().unwrap()) }
fn hash_long(s: &[u8]) -> u64 {
    let n = s.len();
    let mut h = 0x243F6A8885A308D3u64; let mut i = 0;
    while i + 16 < n { h = mum(h ^ rd8(s, i), rd8(s, i + 8) ^ 0x13198A2E03707344); i += 16; }
    mum(h ^ rd8(s, n - 16), rd8(s, n - 8) ^ 0xA4093822299F31D0)
}
#[inline(always)]
fn bytemask(r: usize) -> u64 { if r >= 8 { !0 } else { (1u64 << (8 * r)) - 1 } }
#[inline(always)]
fn home(m0: u64) -> usize { (m0 >> 32) as usize & (NS - 1) }
const LMAX: usize = 1 << 22;
impl<'a> Names<'a> {
    fn new(d: &'a [u8], nt: usize) -> Self {
        let t = unsafe { std::slice::from_raw_parts(zalloc(NS * 64, true) as *const Line, NS) };
        Names { d, t, nt, overflow: AtomicBool::new(false) }
    }
    /// The message (group not yet set) for the name at d[o..o + l].
    #[inline(always)]
    fn msg(&self, o: usize, l: usize) -> Msg {
        let d = self.d;
        if l <= 24 {
            let w = if o + 24 <= d.len() {
                [rd8(d, o) & bytemask(l), rd8(d, o + 8) & bytemask(l.saturating_sub(8)), rd8(d, o + 16) & bytemask(l.saturating_sub(16))]
            } else {
                let mut b = [0u8; 24];
                b[..l].copy_from_slice(&d[o..o + l]);
                [rd8(&b, 0), rd8(&b, 8), rd8(&b, 16)]
            };
            let h = mum(mum(w[0] ^ 0x9E3779B97F4A7C15, w[1] ^ 0xD6E8FEB86659FD93) ^ w[2], l as u64 ^ 0xA0761D6478BD642F);
            [h >> 32 << 32 | (l as u64) << 10 | 3, w[0], w[1], w[2]]
        } else {
            if l >= LMAX { self.overflow.store(true, Relaxed); }
            let h = hash_long(&d[o..o + l]);
            [h >> 32 << 32 | ((l % LMAX) as u64) << 10 | 3, o as u64, 0, 0]
        }
    }
    #[inline(always)]
    fn owner(&self, m0: u64) -> usize { home(m0) * self.nt >> NB }
    /// Slot range [lo, hi) owned by thread o: exactly the slots i with i * nt >> NB == o.
    fn range(&self, o: usize) -> (usize, usize) { ((o * NS).div_ceil(self.nt), ((o + 1) * NS).div_ceil(self.nt)) }
    /// Owner side: find or insert each message's name in the owner's range and set its group bit.
    fn apply(&self, ms: &[Msg], o: &mut Own) {
        if self.overflow.load(Relaxed) { return; }
        const PF: usize = 8;
        for (j, m) in ms.iter().enumerate() {
            if let Some(n) = ms.get(j + PF) { let i = home(n[0]); prefetch(&self.t[i]); }
            let g = (m[0] >> 2) as usize & 255;
            let hdr = m[0] & !(255 << 2);
            let l = (m[0] >> 10) as usize & (LMAX - 1);
            let mut i = home(m[0]);
            loop {
                let sl = &self.t[i].0;
                let hd = sl[0].load(Relaxed);
                if hd == 0 {
                    o.n += 1;
                    if o.n > (o.hi - o.lo) / 8 * 7 { self.overflow.store(true, Relaxed); return; }
                    for k in 1..4 { sl[k].store(m[k], Relaxed); }
                    sl[0].store(hdr, Relaxed);
                    break;
                }
                if hd == hdr {
                    if l <= 24 {
                        if sl[1].load(Relaxed) == m[1] && sl[2].load(Relaxed) == m[2] && sl[3].load(Relaxed) == m[3] { break; }
                    } else {
                        let (p, q) = (sl[1].load(Relaxed) as usize, m[1] as usize);
                        if self.d[p..p + l] == self.d[q..q + l] { break; }
                    }
                }
                i += 1;
                if i == o.hi { i = o.lo; }
            }
            let wd = &self.t[i].0[4 + g / 64];
            wd.store(wd.load(Relaxed) | 1 << (g % 64), Relaxed);
        }
    }
}
/// An owner's slot range and how many of its slots are used.
struct Own { lo: usize, hi: usize, n: usize }

// ---------- SIMD block classification ----------
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    let a = _mm256_loadu_si256(p as *const __m256i);
    let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let m = |c: u8| {
        let s = _mm256_set1_epi8(c as i8);
        (_mm256_movemask_epi8(_mm256_cmpeq_epi8(a, s)) as u32 as u64) | (_mm256_movemask_epi8(_mm256_cmpeq_epi8(b, s)) as u32 as u64) << 32
    };
    (m(b'"'), m(b','), m(b'\n'))
}
#[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
#[inline(always)]
unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
    let (mut q, mut c, mut l) = (0u64, 0u64, 0u64);
    for i in 0..64 { let b = *p.add(i); q |= ((b == b'"') as u64) << i; c |= ((b == b',') as u64) << i; l |= ((b == b'\n') as u64) << i; }
    (q, c, l)
}
#[inline(always)]
fn prefix_xor(mut x: u64) -> u64 { x ^= x << 1; x ^= x << 2; x ^= x << 4; x ^= x << 8; x ^= x << 16; x ^= x << 32; x }
#[inline(always)]
fn low(n: usize) -> u64 { if n >= 64 { !0 } else { (1u64 << n) - 1 } }

const RING: usize = 8;
const PD: usize = 1024; // stream prefetch distance
/// A parsed record waiting (while its region entry is prefetched) to be aggregated.
#[derive(Clone, Copy, Default)]
struct Pend { reg: usize, id: u64, c: i64, nb: u64, tb: u64, m: Msg }
struct Ring([Pend; RING], usize);
const BATCH: usize = 64;
/// Messages for one owner; `from` is the producer, to which the owner hands the batch back for reuse.
struct Batch { next: *mut Batch, from: usize, n: usize, m: [Msg; BATCH] }
/// Lock-free stack of batches: many pushers, one taker that always takes everything (so no ABA).
#[repr(align(128))]
struct Stack(AtomicPtr<Batch>);
impl Stack {
    fn push(&self, b: *mut Batch) {
        let mut h = self.0.load(Relaxed);
        loop {
            unsafe { (*b).next = h; }
            match self.0.compare_exchange_weak(h, b, Release, Relaxed) { Ok(_) => return, Err(x) => h = x }
        }
    }
    fn take(&self) -> *mut Batch {
        if self.0.load(Relaxed).is_null() { std::ptr::null_mut() } else { self.0.swap(std::ptr::null_mut(), Acquire) }
    }
}
/// A slot range with its fill count. Whoever holds `lock` may write the range: normally its owner
/// thread, but a thread that waits for batches helps by applying another owner's backlog.
#[repr(align(128))]
struct Part { lock: AtomicBool, own: std::cell::UnsafeCell<Own> }
unsafe impl Sync for Part {}
struct Ctx<'a> { d: &'a [u8], table: &'a [AtomicU8], names: &'a Names<'a>, inbox: &'a [Stack], spare: &'a [Stack], parts: &'a [Part] }
/// Per-thread state: aggregates, the open batch for each slot owner, free batches.
struct St { agg: Vec<Agg>, me: usize, out: Vec<*mut Batch>, free: Vec<*mut Batch>, nb: usize }
impl Ctx<'_> {
    /// An empty batch. A thread has at most 4 per owner (bounded memory); when all are in flight it
    /// helps apply waiting batches until some come back.
    fn fresh(&self, st: &mut St) -> *mut Batch {
        loop {
            if let Some(b) = st.free.pop() { return b; }
            let mut b = self.spare[st.me].take();
            while !b.is_null() { st.free.push(b); b = unsafe { (*b).next }; }
            if !st.free.is_empty() { continue; }
            if st.nb < 4 * self.parts.len() {
                st.nb += 1;
                return Box::into_raw(Box::new(Batch { next: std::ptr::null_mut(), from: st.me, n: 0, m: [[0; 4]; BATCH] }));
            }
            if !self.help() { std::thread::yield_now(); }
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
        loop {
            if !wait && self.inbox[o].0.load(Relaxed).is_null() { return false; }
            if !pt.lock.swap(true, Acquire) { break; }
            if !wait { return false; }
            std::thread::yield_now();
        }
        let own = unsafe { &mut *pt.own.get() };
        let mut b = self.inbox[o].take();
        let any = !b.is_null();
        while !b.is_null() {
            let x = unsafe { &mut *b };
            let nx = x.next;
            self.names.apply(&x.m[..x.n], own);
            x.n = 0;
            self.spare[x.from].push(b);
            b = nx;
        }
        pt.lock.store(false, Release);
        any
    }
    fn help(&self) -> bool {
        let mut any = false;
        for o in 0..self.parts.len() { any |= self.drain(o, false); }
        any
    }
}

#[inline(always)]
fn flush(cx: &Ctx, st: &mut St, p: &Pend) {
    let g = if p.reg < cx.table.len() { cx.table[p.reg].load(Relaxed).wrapping_sub(1) as usize } else { UNM };
    st.agg[g].add(p.id, p.c, p.nb, p.tb);
    let o = cx.names.owner(p.m[0]);
    let b = unsafe { &mut *st.out[o] };
    b.m[b.n] = [p.m[0] | (g as u64) << 2, p.m[1], p.m[2], p.m[3]];
    b.n += 1;
    if b.n == BATCH {
        cx.post(st, o);
        cx.drain(st.me, false); // keeps the backlog small
    }
}

/// One record: start rs, commas at f, quote counts (running) at those commas, terminator LF at end.
#[inline(always)]
fn record(cx: &Ctx, st: &mut St, ring: &mut Ring, rs: usize, f: &[usize; 4], fq: &[u64; 4], end: usize) {
    let d = cx.d;
    let a = rs + if d[rs] == 0xEF { 3 } else { 0 };
    let id = num(d, a, f[0]);
    // value length = raw length - delimiting quotes - one per "" pair
    let q = (fq[1] - fq[0]) as usize;
    let nl = f[1] - f[0] - 1;
    let (mut ko, mut kl) = if q > 0 { (f[0] + 2, nl.saturating_sub(2)) } else { (f[0] + 1, nl) };
    if ko + kl > d.len() { (ko, kl) = (0, 0); } // only on malformed input
    let nb = kl.saturating_sub(q.saturating_sub(2) / 2) as u64;
    let tq = (fq[2] - fq[1]) as usize;
    let tl = f[2] - f[1] - 1;
    let tb = if tq > 0 { tl.saturating_sub(2 + (tq - 2) / 2) } else { tl } as u64;
    let reg = num(d, f[2] + 1, f[3]) as usize;
    if reg < cx.table.len() { prefetch(&cx.table[reg]); }
    let e = end - (d[end - 1] == 13) as usize;
    let s = f[3] + 1;
    let neg = d.get(s) == Some(&b'-');
    let c = if e >= s + 4 + neg as usize && d[e - 3] == b'.' {
        let v = (num(d, s + neg as usize, e - 3) * 100 + (d[e - 2].wrapping_sub(b'0') as u64) * 10 + d[e - 1].wrapping_sub(b'0') as u64) as i64;
        if neg { -v } else { v }
    } else { cents(d.get(s..e).unwrap_or(&[])) };
    let m = cx.names.msg(ko, kl);
    let slot = &mut ring.0[ring.1 % RING];
    if ring.1 >= RING { flush(cx, st, slot); }
    *slot = Pend { reg, id, c, nb, tb, m };
    ring.1 += 1;
}

/// Parse every record whose start lies in [r0, e), r0 being a record start outside quotes.
/// Returns the number of quote bytes in [r0, e).
fn parse_from(cx: &Ctx, st: &mut St, r0: usize, e: usize) -> u64 {
    let d = cx.d;
    let len = d.len();
    let mut base = r0;
    let mut carry = 0u64;
    let mut cumq = 0u64;
    let mut qe = 0u64;
    let mut rs = r0;
    let mut f = [0usize; 4];
    let mut fq = [0u64; 4];
    let mut k = 0usize;
    let mut ring = Ring([Pend::default(); RING], 0);
    while base < len {
        let (q, c, l) = if base + 64 <= len {
            if base + PD < len { prefetch(&d[base + PD]); }
            unsafe { masks(d.as_ptr().add(base)) }
        } else {
            let n = len - base;
            let mut buf = [0u8; 64];
            buf[..n].copy_from_slice(&d[base..]);
            unsafe { masks(buf.as_ptr()) }
        };
        if e <= base + 64 && e > base { qe = cumq + (q & low(e - base)).count_ones() as u64; }
        let inq = prefix_xor(q) ^ carry;
        carry = ((inq as i64) >> 63) as u64;
        let mut s = (c | l) & !inq;
        while s != 0 {
            let t = s.trailing_zeros() as usize;
            s &= s - 1;
            let pos = base + t;
            let cq = cumq + (q & low(t)).count_ones() as u64;
            if k < 4 { f[k] = pos; fq[k] = cq; k += 1; } else {
                record(cx, st, &mut ring, rs, &f, &fq, pos);
                k = 0;
                rs = pos + 1;
                if rs >= e { break; }
            }
        }
        if rs >= e { break; }
        cumq += q.count_ones() as u64;
        base += 64;
    }
    // a final record without terminator ends at EOF (only if EOF is outside quotes)
    if base >= len && k == 4 && rs < e && rs < len && d[len - 1] != b'\n' && carry == 0 { record(cx, st, &mut ring, rs, &f, &fq, len); }
    for p in &ring.0[..ring.1.min(RING)] { flush(cx, st, p); }
    qe
}

// ---------- fast parallel path ----------
// Outside quotes <=> an even number of '"' precede the byte (by the grammar '"' only occurs in quoted
// fields). Each chunk guesses its first record start (an LF followed by a digit or a BOM) and assumes
// it is outside quotes; afterwards the guess is checked exactly against the prefix quote parity.
// Any wrong guess, or a name table overflow, falls back to the sequential exact parser.
fn fast(d: &[u8], rd: &[u8], nt: usize, nch: usize) -> Option<Vec<(Vec<u8>, Agg, u64)>> {
    let len = d.len();
    let rg = reg_new(rd)?;
    let names = Names::new(d, nt);
    let stk = || (0..nt).map(|_| Stack(AtomicPtr::new(std::ptr::null_mut()))).collect::<Vec<_>>();
    let (inbox, spare) = (stk(), stk());
    let parts: Vec<Part> = (0..nt).map(|o| { let (lo, hi) = names.range(o); Part { lock: AtomicBool::new(false), own: Own { lo, hi, n: 0 }.into() } }).collect();
    let cx = Ctx { d, table: rg.tbl, names: &names, inbox: &inbox, spare: &spare, parts: &parts };
    let nch = if nch > 0 { nch.min(len.max(1)) } else { (nt * 32).min(len / 4096 + 1) };
    let next = AtomicUsize::new(0);
    // per chunk: quotes in chunk << 2 | checked << 1 | implied parity
    let res: Vec<AtomicU64> = (0..nch).map(|_| AtomicU64::new(0)).collect();
    let hdr = d.iter().position(|&b| b == b'\n').map_or(len, |x| x + 1);
    let done = AtomicUsize::new(0);
    const RP: usize = 64;
    let (rnext, rdone) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let res2: Vec<(Vec<Agg>, Vec<u64>)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nt).map(|ti| {
            let (cx, next, res, done, names, rg, rnext, rdone) = (&cx, &next, &res, &done, &names, &rg, &rnext, &rdone);
            // small stacks: a 2 MiB stack could be backed by a whole huge page
            std::thread::Builder::new().stack_size(256 << 10).spawn_scoped(sc, move || {
                // region pieces are claimed dynamically, so early threads do not wait for late spawns
                loop {
                    let i = rnext.fetch_add(1, Relaxed);
                    if i >= RP { break; }
                    rg.part(i, RP);
                    rdone.fetch_add(1, Release);
                }
                while rdone.load(Acquire) < RP { std::hint::spin_loop(); }
                let (lo, hi) = names.range(ti);
                let mut st = St { agg: vec![AGG0; 256], me: ti, out: Vec::with_capacity(nt), free: Vec::new(), nb: 0 };
                for _ in 0..nt { let b = cx.fresh(&mut st); st.out.push(b); }
                loop {
                    let i = next.fetch_add(1, Relaxed);
                    if i >= nch { break; }
                    let (s, e) = (i * len / nch, (i + 1) * len / nch);
                    let (mut qpre, mut r) = (0u64, s);
                    let mut chk = 0;
                    if i == 0 {
                        qpre = d[..hdr].iter().filter(|&&b| b == b'"').count() as u64;
                        r = hdr;
                    } else {
                        while r < e {
                            if d[r - 1] == b'\n' && (d[r].is_ascii_digit() || d[r] == 0xEF) { chk = 2 | (qpre & 1); break; }
                            qpre += (d[r] == b'"') as u64;
                            r += 1;
                        }
                    }
                    let qt = if r < e { qpre + parse_from(cx, &mut st, r, e) } else { qpre };
                    res[i].store(qt << 2 | chk, Relaxed);
                    cx.drain(ti, false);
                }
                for o in 0..nt { cx.post(&mut st, o); }
                // keep serving the mailbox until every thread has sent everything
                done.fetch_add(1, Release);
                while done.load(Acquire) < nt { if !cx.help() { std::thread::yield_now(); } }
                // everything is posted now; after this drain the range is final
                cx.drain(ti, true);
                let mut dist = vec![0u64; 256];
                for i in lo..hi {
                    for k in 0..4 {
                        let mut b = names.t[i].0[4 + k].load(Relaxed);
                        while b != 0 { dist[k * 64 + b.trailing_zeros() as usize] += 1; b &= b - 1; }
                    }
                }
                (st.agg, dist)
            }).expect("spawn")
        }).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    if names.overflow.load(Relaxed) || rg.bad.load(Relaxed) { return None; }
    let mut p = 0u64;
    for r in &res {
        let v = r.load(Relaxed);
        if v & 2 != 0 && (v & 1) != p { return None; }
        p ^= (v >> 2) & 1;
    }
    let mut tot = vec![AGG0; 256];
    let mut dist = vec![0u64; 256];
    for (a, ds) in &res2 {
        for g in 0..256 { tot[g].merge(&a[g]); dist[g] += ds[g]; }
    }
    Some((0..256).map(|g| (if g == UNM { b"_UNMATCHED".to_vec() } else { rg.codes[g].load(Relaxed).to_le_bytes()[..3].to_vec() }, tot[g], dist[g])).collect())
}

// ---------- slow, simple, exact sequential path (fallback and cross-check) ----------
fn slow(d: &[u8], rd: &[u8]) -> Vec<(Vec<u8>, Agg, u64)> {
    let mut reg: HashMap<u64, &[u8]> = HashMap::new();
    for line in rd.split(|&b| b == b'\n').skip(1) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(c) = line.iter().position(|&b| b == b',') { reg.insert(uint_sat(&line[..c]), &line[c + 1..]); }
    }
    let mut groups: HashMap<&[u8], (Agg, HashSet<&[u8]>)> = HashMap::new();
    let len = d.len();
    let mut p = 0;
    let mut first = true;
    while p < len {
        if d[p..].starts_with(&[0xEF, 0xBB, 0xBF]) { p += 3; }
        let mut fl: [(&[u8], usize); 5] = [(&[], 0); 5]; // (raw inner bytes, unescaped length)
        for k in 0..5 {
            if p < len && d[p] == b'"' {
                let mut i = p + 1;
                let mut dq = 0;
                while i < len {
                    if d[i] == b'"' { if i + 1 < len && d[i + 1] == b'"' { i += 2; dq += 1; continue; } break; }
                    i += 1;
                }
                let i = i.min(len);
                fl[k] = (&d[p + 1..i], i - p - 1 - dq);
                p = i + 1;
            } else {
                let st = p;
                while p < len && !matches!(d[p], b',' | b'\r' | b'\n') { p += 1; }
                fl[k] = (&d[st..p], p - st);
            }
            if k < 4 { p += 1; }
        }
        if p < len && d[p] == b'\r' { p += 1; }
        if p < len && d[p] == b'\n' { p += 1; }
        if first { first = false; continue; }
        let key: &[u8] = reg.get(&uint_sat(fl[3].0)).copied().unwrap_or(b"_UNMATCHED");
        let e = groups.entry(key).or_insert_with(|| (AGG0, HashSet::new()));
        e.0.add(uint(fl[0].0), cents(fl[4].0), fl[1].1 as u64, fl[2].1 as u64);
        e.1.insert(fl[1].0);
    }
    groups.into_iter().map(|(k, (a, s))| (k.to_vec(), a, s.len() as u64)).collect()
}

fn main() {
    // one malloc arena: worker threads allocate little, and per-thread arenas would inflate peak memory
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe { extern "C" { fn mallopt(p: i32, v: i32) -> i32; } mallopt(-8, 1); }
    let mut data = "/data".to_string();
    let mut out = "/out".to_string();
    let mut slow_only = false;
    let mut nt = std::thread::available_parallelism().map_or(12, |n| n.get());
    let mut nch = 0;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--data" => { data = args[i + 1].clone(); i += 1; }
            "--out" => { out = args[i + 1].clone(); i += 1; }
            "--threads" => { nt = args[i + 1].parse().unwrap(); i += 1; }
            "--chunks" => { nch = args[i + 1].parse().unwrap(); i += 1; }
            "--slow" => slow_only = true,
            _ => {}
        }
        i += 1;
    }
    let nt = nt.clamp(1, 256);
    let d = map(&format!("{data}/records.csv"));
    let rd = map(&format!("{data}/regions.csv"));
    let mut rows = match if slow_only { None } else { fast(d, rd, nt, nch) } {
        Some(r) => r,
        None => { eprintln!("note: exact sequential path"); slow(d, rd) }
    };
    rows.retain(|r| r.1.cnt > 0);
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut o = String::new();
    let money = |c: i64| format!("{}{}.{:02}", if c < 0 { "-" } else { "" }, c.unsigned_abs() / 100, c.unsigned_abs() % 100);
    for (k, a, n) in rows {
        o += &format!("{} {} {} {} {} {} {} {} {}\n", String::from_utf8_lossy(&k), a.cnt, a.ids, money(a.sum), money(a.min), money(a.max), n, a.nb, a.tb);
    }
    std::fs::write(format!("{out}/aggregates.txt"), o).expect("write");
}
