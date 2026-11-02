// Speed Demon 2026, Track C (Join). std only.
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering::*};

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
/// Zeroed, 2 MiB aligned memory with transparent huge pages requested (fewer TLB misses on random access).
#[cfg(target_os = "linux")]
fn zalloc(n: usize) -> *mut u8 {
    extern "C" {
        fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
        fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    }
    const H: usize = 1 << 21;
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
fn zalloc(n: usize) -> *mut u8 {
    let l = std::alloc::Layout::from_size_align(n, 64).unwrap();
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
    let tbl = unsafe { std::slice::from_raw_parts(Box::leak(vec![0u8; max + 1].into_boxed_slice()).as_ptr() as *const AtomicU8, max + 1) };
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
// slot: w0 header (0 empty, 1 being written, else hash32<<32 | len<<2 | 3), w1..w3 name bytes (zero
// padded) or, for names over 24 bytes, w1 = file offset. bits: 256-bit set of groups per slot.
#[derive(Clone, Copy, Default)]
struct Key { h: u64, w: [u64; 3], o: usize, l: usize }
#[inline(always)]
fn prefetch<T>(p: &T) {
    #[cfg(target_arch = "x86_64")]
    unsafe { std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(p as *const T as *const i8) }
}
#[repr(align(64))]
struct Line([AtomicU64; 8]); // two slots: [header, name w0..w2] x2
struct Names<'a> { d: &'a [u8], t: &'static [Line], bits: &'static [Line], n: AtomicUsize, overflow: AtomicBool }
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
impl<'a> Names<'a> {
    fn new(d: &'a [u8]) -> Self {
        let t = unsafe { std::slice::from_raw_parts(zalloc(NS * 32) as *const Line, NS / 2) };
        let bits = unsafe { std::slice::from_raw_parts(zalloc(NS * 32) as *const Line, NS / 2) };
        Names { d, t, bits, n: AtomicUsize::new(0), overflow: AtomicBool::new(false) }
    }
    /// Hash a name and prefetch its line; the insert happens a few records later (Names::add).
    #[inline(always)]
    fn key(&self, o: usize, l: usize) -> Key {
        let d = self.d;
        let mut w = [0u64; 3];
        let h = if l <= 24 {
            if o + 24 <= d.len() {
                w = [rd8(d, o) & bytemask(l), rd8(d, o + 8) & bytemask(l.saturating_sub(8)), rd8(d, o + 16) & bytemask(l.saturating_sub(16))];
            } else {
                let mut b = [0u8; 24];
                b[..l].copy_from_slice(&d[o..o + l]);
                w = [rd8(&b, 0), rd8(&b, 8), rd8(&b, 16)];
            }
            mum(mum(w[0] ^ 0x9E3779B97F4A7C15, w[1] ^ 0xD6E8FEB86659FD93) ^ w[2], l as u64 ^ 0xA0761D6478BD642F)
        } else { hash_long(&d[o..o + l]) };
        prefetch(&self.t[(h as usize & (NS - 1)) / 2]);
        Key { h, w, o, l }
    }
    #[inline(always)]
    fn add(&self, k: &Key) -> Option<usize> {
        let d = self.d;
        let (h, w, o, l) = (k.h, k.w, k.o, k.l);
        if l >= 1 << 30 { self.overflow.store(true, Relaxed); return None; }
        let hdr = (h >> 32) << 32 | (l as u64) << 2 | 3;
        let mut i = h as usize & (NS - 1);
        let mut j = 0;
        loop {
            let ln = &self.t[i / 2].0[(i & 1) * 4..(i & 1) * 4 + 4];
            let mut hd = ln[0].load(Acquire);
            if hd == 0 {
                match ln[0].compare_exchange(0, 1, Acquire, Acquire) {
                    Ok(_) => {
                        if self.n.fetch_add(1, Relaxed) >= NS / 8 * 7 { self.overflow.store(true, Relaxed); }
                        if l <= 24 { for k in 0..3 { ln[1 + k].store(w[k], Relaxed); } } else { ln[1].store(o as u64, Relaxed); }
                        ln[0].store(hdr, Release);
                        break;
                    }
                    Err(cur) => hd = cur,
                }
            }
            while hd == 1 { std::hint::spin_loop(); hd = ln[0].load(Acquire); }
            if hd == hdr {
                if l <= 24 {
                    if ln[1].load(Relaxed) == w[0] && ln[2].load(Relaxed) == w[1] && ln[3].load(Relaxed) == w[2] { break; }
                } else {
                    let p = ln[1].load(Relaxed) as usize;
                    if d[p..p + l] == d[o..o + l] { break; }
                }
            }
            i = (i + 1) & (NS - 1);
            j += 1;
            if j > NS { self.overflow.store(true, Relaxed); return None; }
        }
        Some(i)
    }
    /// Set bit (slot, group). Only the slot's owner thread calls this, so no locked RMW is needed.
    #[inline(always)]
    fn set(&self, v: u32) {
        let (i, g) = ((v >> 8) as usize, (v & 255) as usize);
        let wd = &self.bits[i / 2].0[(i & 1) * 4 + g / 64];
        wd.store(wd.load(Relaxed) | 1 << (g % 64), Relaxed);
    }
}

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
/// A parsed record waiting (while its region entry and name line are prefetched) to be aggregated.
#[derive(Clone, Copy, Default)]
struct Pend { reg: usize, id: u64, c: i64, nb: u64, tb: u64, key: Key }
struct Ring([Pend; RING], usize);
struct Ctx<'a> { d: &'a [u8], table: &'a [AtomicU8], names: &'a Names<'a>, nt: usize, mail: &'a [Mutex<Vec<Vec<u32>>>] }
/// Per-thread state: aggregates, and outgoing (slot << 8 | group) batches for each slot owner.
struct St { agg: Vec<Agg>, me: usize, out: Vec<Vec<u32>> }
const BATCH: usize = 256;
impl Ctx<'_> {
    fn post(&self, st: &mut St, o: usize) {
        let b = std::mem::replace(&mut st.out[o], Vec::with_capacity(BATCH));
        if !b.is_empty() { self.mail[o].lock().unwrap().push(b); }
    }
    fn drain(&self, me: usize) {
        let bs = std::mem::take(&mut *self.mail[me].lock().unwrap());
        for b in bs { for v in b { self.names.set(v); } }
    }
}

#[inline(always)]
fn flush(cx: &Ctx, st: &mut St, p: &Pend) {
    let g = if p.reg < cx.table.len() { cx.table[p.reg].load(Relaxed).wrapping_sub(1) as usize } else { UNM };
    st.agg[g].add(p.id, p.c, p.nb, p.tb);
    if let Some(i) = cx.names.add(&p.key) {
        let v = (i as u32) << 8 | g as u32;
        let o = i * cx.nt >> NB;
        if o == st.me { cx.names.set(v); } else {
            st.out[o].push(v);
            if st.out[o].len() >= BATCH { cx.post(st, o); }
        }
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
    let key = cx.names.key(ko, kl);
    let slot = &mut ring.0[ring.1 % RING];
    if ring.1 >= RING { flush(cx, st, slot); }
    *slot = Pend { reg, id, c, nb, tb, key };
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
    let names = Names::new(d);
    let mail: Vec<Mutex<Vec<Vec<u32>>>> = (0..nt).map(|_| Mutex::new(Vec::new())).collect();
    let cx = Ctx { d, table: rg.tbl, names: &names, nt, mail: &mail };
    let nch = if nch > 0 { nch.min(len.max(1)) } else { (nt * 32).min(len / 4096 + 1) };
    let next = AtomicUsize::new(0);
    // per chunk: quotes in chunk << 2 | checked << 1 | implied parity
    let res: Vec<AtomicU64> = (0..nch).map(|_| AtomicU64::new(0)).collect();
    let hdr = d.iter().position(|&b| b == b'\n').map_or(len, |x| x + 1);
    let bar = std::sync::Barrier::new(nt);
    const RP: usize = 64;
    let (rnext, rdone) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let parts: Vec<(Vec<Agg>, Vec<u64>)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nt).map(|ti| {
            let (cx, next, res, bar, names, rg, rnext, rdone) = (&cx, &next, &res, &bar, &names, &rg, &rnext, &rdone);
            sc.spawn(move || {
                // region pieces are claimed dynamically, so early threads do not wait for late spawns
                loop {
                    let i = rnext.fetch_add(1, Relaxed);
                    if i >= RP { break; }
                    rg.part(i, RP);
                    rdone.fetch_add(1, Release);
                }
                while rdone.load(Acquire) < RP { std::hint::spin_loop(); }
                let mut st = St { agg: vec![AGG0; 256], me: ti, out: (0..nt).map(|_| Vec::with_capacity(BATCH)).collect() };
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
                    cx.drain(ti);
                }
                for o in 0..nt { cx.post(&mut st, o); }
                bar.wait();
                cx.drain(ti);
                bar.wait();
                let mut dist = vec![0u64; 256];
                for ln in &names.bits[ti * NS / 2 / nt..(ti + 1) * NS / 2 / nt] {
                    for k in 0..8 {
                        let mut b = ln.0[k].load(Relaxed);
                        while b != 0 { dist[(k & 3) * 64 + b.trailing_zeros() as usize] += 1; b &= b - 1; }
                    }
                }
                (st.agg, dist)
            })
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
    for (a, ds) in &parts {
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
