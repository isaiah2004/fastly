// Speed Demon 2026, Track B (Index). std only.
// One parallel pass over docs.txt: tokenize (AVX2 byte classification), look up each token in a
// table of the query terms only, build posting lists (u32 lists or bitmaps) for BOOL terms and
// match every phrase inline at its rarest term. Then answer the queries in parallel.
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

const MISS: u32 = u32::MAX;
type Key = [u64; 4];

#[inline(always)]
fn hk(k: &Key) -> u64 {
    (k[0] ^ k[1].rotate_left(29)).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}
// Open addressing over the query terms. A 16-byte slot holds bytes 0..12 of the term and its id, so
// terms of up to 12 bytes are verified without touching the key array; longer ones set bit 31 of
// the second word (term bytes never have it) and are checked against the full key.
struct Tab {
    slots: Vec<(u64, u32, u32)>,
    sh: u32,
    keys: Vec<Key>,
}
// Hash of a term of at most 12 bytes from its first 8 bytes and the next 4 (same as hk), top 32 bits.
#[inline(always)]
fn h12(k0: u64, k1: u32) -> u32 {
    ((k0 ^ (k1 as u64).rotate_left(29)).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as u32
}
#[inline(always)]
fn k1(k: &Key) -> u32 {
    k[1] as u32 | ((k[1] >> 32 != 0) as u32) << 31
}
impl Tab {
    fn new(bits: u32) -> Tab {
        Tab { slots: hvec(1 << bits, (0, 0, 0)), sh: 64 - bits, keys: Vec::new() }
    }
    fn of(keys: Vec<Key>) -> Tab {
        let mut t = Tab::new((keys.len() * 4 + 4).next_power_of_two().trailing_zeros().max(4));
        keys.iter().for_each(|k| _ = t.intern(k));
        t
    }
    fn intern(&mut self, k: &Key) -> u32 {
        let id = self.get(k);
        if id != MISS {
            return id;
        }
        if self.keys.len() * 2 >= self.slots.len() {
            *self = Tab::of(std::mem::take(&mut self.keys));
        }
        let m = self.slots.len() - 1;
        let mut i = (hk(k) >> self.sh) as usize;
        while self.slots[i].0 != 0 {
            i = (i + 1) & m;
        }
        self.slots[i] = (k[0], k1(k), self.keys.len() as u32);
        self.keys.push(*k);
        self.keys.len() as u32 - 1
    }
    fn set_flags(&mut self, f: &[u8]) {
        self.slots.iter_mut().filter(|s| s.0 != 0).for_each(|s| s.2 |= (f[s.2 as usize] as u32) << 24);
    }
    #[inline(always)]
    fn get(&self, k: &Key) -> u32 {
        self.get_h(k, hk(k))
    }
    #[inline(always)]
    fn prefetch32(&self, h: u32) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::*;
            _mm_prefetch(self.slots.as_ptr().add((h >> (self.sh - 32)) as usize) as *const i8, _MM_HINT_T0)
        }
    }
    // Lookup of a term of at most 12 bytes (k0, k1 as in the slots) with h = h12(k0, k1).
    #[inline(always)]
    fn get12(&self, k0: u64, k1: u32, h: u32) -> u32 {
        let m = self.slots.len() - 1;
        let mut i = (h >> (self.sh - 32)) as usize;
        loop {
            let s = unsafe { *self.slots.get_unchecked(i) };
            if s.0 == k0 && s.1 == k1 {
                return s.2;
            }
            if s.0 == 0 {
                return MISS;
            }
            i = (i + 1) & m;
        }
    }
    #[inline(always)]
    fn get_h(&self, k: &Key, h: u64) -> u32 {
        let m = self.slots.len() - 1;
        let mut i = (h >> self.sh) as usize;
        let b = k1(k);
        loop {
            let s = unsafe { *self.slots.get_unchecked(i) };
            if s.0 == k[0] && s.1 == b && (b >> 31 == 0 || self.keys[(s.2 & 0xFF_FFFF) as usize] == *k) {
                return s.2;
            }
            if s.0 == 0 {
                return MISS;
            }
            i = (i + 1) & m;
        }
    }
}

fn key_of(t: &[u8]) -> Key {
    let mut b = [0u8; 32];
    b[..t.len()].copy_from_slice(t);
    let mut k = [0u64; 4];
    for i in 0..4 {
        k[i] = u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    }
    k
}

static MK: [u8; 64] = {
    let mut m = [0u8; 64];
    let mut i = 0;
    while i < 32 {
        m[i] = 0xFF;
        i += 1;
    }
    m
};
// Token bytes [s, s+len) lowercased and zero padded to 32 bytes (len <= 32).
#[inline(always)]
fn load_key(buf: &[u8], s: usize, len: usize) -> Key {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    if s + 32 <= buf.len() {
        unsafe {
            use std::arch::x86_64::*;
            let v = _mm256_or_si256(_mm256_loadu_si256(buf.as_ptr().add(s) as *const __m256i), _mm256_set1_epi8(0x20));
            return std::mem::transmute(_mm256_and_si256(v, _mm256_loadu_si256(MK.as_ptr().add(32 - len) as *const __m256i)));
        }
    }
    let mut b = [0u8; 32];
    let n = (buf.len() - s).min(32);
    b[..n].copy_from_slice(&buf[s..s + n]);
    for i in 0..32 {
        b[i] = (b[i] | 0x20) & MK[32 - len + i];
    }
    unsafe { std::mem::transmute(b) }
}

// Token bytes [s, s+len) lowercased, len <= 12: (bytes 0..8, bytes 8..12) zero padded.
#[inline(always)]
fn load12(buf: &[u8], s: usize, len: usize) -> (u64, u32) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    if s + 16 <= buf.len() {
        unsafe {
            use std::arch::x86_64::*;
            let v = _mm_or_si128(_mm_loadu_si128(buf.as_ptr().add(s) as *const __m128i), _mm_set1_epi8(0x20));
            let v = _mm_and_si128(v, _mm_loadu_si128(MK.as_ptr().add(32 - len) as *const __m128i));
            return (_mm_cvtsi128_si64(v) as u64, _mm_extract_epi32::<2>(v) as u32);
        }
    }
    let k = load_key(buf, s, len);
    (k[0], k[1] as u32)
}

// Bit i of the first mask: p[i] is [A-Za-z0-9]; of the second: p[i] is a newline.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
unsafe fn mask64(p: *const u8) -> (u64, u64) {
    use std::arch::x86_64::*;
    let f = |v: __m256i| -> (u64, u64) {
        let lo = _mm256_or_si256(v, _mm256_set1_epi8(0x20));
        let l = _mm256_cmpgt_epi8(_mm256_set1_epi8(-102), _mm256_add_epi8(lo, _mm256_set1_epi8(0x1F)));
        let d = _mm256_cmpgt_epi8(_mm256_set1_epi8(-118), _mm256_add_epi8(v, _mm256_set1_epi8(0x50)));
        let n = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(10));
        (_mm256_movemask_epi8(_mm256_or_si256(l, d)) as u32 as u64, _mm256_movemask_epi8(n) as u32 as u64)
    };
    let (a, b) = (f(_mm256_loadu_si256(p as *const __m256i)), f(_mm256_loadu_si256(p.add(32) as *const __m256i)));
    let (mut m, mut n) = (a.0 | b.0 << 32, a.1 | b.1 << 32);
    // Opaque to LLVM: otherwise it turns the masks into 64 separate bits (~350 instructions per block).
    std::arch::asm!("/* {0} {1} */", inout(reg) m, inout(reg) n, options(pure, nomem, nostack));
    (m, n)
}
#[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
#[inline(always)]
unsafe fn mask64(p: *const u8) -> (u64, u64) {
    let (mut m, mut n) = (0u64, 0u64);
    for i in 0..64 {
        let c = *p.add(i);
        m |= ((c | 0x20).wrapping_sub(b'a') < 26 || c.wrapping_sub(b'0') < 10) as u64 * (1 << i);
        n |= (c == b'\n') as u64 * (1 << i);
    }
    (m, n)
}

// Calls f(start, end, first) for every maximal [A-Za-z0-9] run in buf[a..b], where first says the
// run starts a line (the doc id). buf[a] starts a line, buf[b-1] is a newline.
#[inline(always)]
fn scan(buf: &[u8], a: usize, b: usize, mut f: impl FnMut(usize, usize, bool)) {
    let (mut pos, mut carry, mut nlc, mut ts, mut first, mut in_tok) = (a, 0u64, 1u64, 0usize, false, false);
    while pos < b {
        let (mut m, nl) = if pos + 64 <= buf.len() {
            unsafe { mask64(buf.as_ptr().add(pos)) }
        } else {
            let mut t = [0u8; 64];
            t[..buf.len() - pos].copy_from_slice(&buf[pos..]);
            unsafe { mask64(t.as_ptr()) }
        };
        if b - pos < 64 {
            m &= (1u64 << (b - pos)) - 1;
        }
        let pn = nl << 1 | nlc; // bit i: byte i-1 is a newline
        nlc = nl >> 63;
        let mut t = m ^ ((m << 1) | carry);
        carry = m >> 63;
        while t != 0 {
            let i = t.trailing_zeros();
            t &= t - 1;
            if in_tok {
                f(ts, pos + i as usize, first)
            } else {
                ts = pos + i as usize;
                first = pn >> i & 1 != 0;
            }
            in_tok = !in_tok;
        }
        pos += 64;
    }
}

fn load(path: &str) -> &'static [u8] {
    #[cfg(target_os = "linux")]
    {
        extern "C" {
            fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
        }
        use std::os::unix::io::AsRawFd;
        let f = std::fs::File::open(path).unwrap();
        let len = f.metadata().unwrap().len() as usize;
        if len > 0 {
            let p = unsafe { mmap(std::ptr::null_mut(), len, 1, if std::env::var("POP").is_ok() { 0x8002 } else { 2 }, f.as_raw_fd(), 0) };
            if p as isize != -1 {
                return unsafe { std::slice::from_raw_parts(p, len) };
            }
        }
    }
    Box::leak(std::fs::read(path).unwrap().into_boxed_slice())
}

// vec![v; n] whose big buffers ask for huge pages (fewer TLB misses on random access).
fn hvec<T: Clone>(n: usize, v: T) -> Vec<T> {
    let mut x = Vec::with_capacity(n);
    #[cfg(target_os = "linux")]
    {
        extern "C" {
            fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
        }
        let (a, e) = (x.as_ptr() as usize, x.as_ptr() as usize + n * std::mem::size_of::<T>());
        let (a, e) = ((a + (1 << 21) - 1) & !((1 << 21) - 1), e & !((1 << 21) - 1));
        if e > a {
            unsafe { madvise(a as *mut u8, e - a, if std::env::var("NOHP").is_ok() { 15 } else { 14 }) };
        }
    }
    x.resize(n, v);
    x
}

// First line start at or after p.
fn line_start(buf: &[u8], mut p: usize) -> usize {
    while p < buf.len() && p > 0 && buf[p - 1] != b'\n' {
        p += 1;
    }
    p
}

fn parse_u32(b: &[u8]) -> u32 {
    b.iter().fold(0u32, |a, &c| a * 10 + (c - b'0') as u32)
}

struct Ctx<'a> {
    dummy: u32,
    buf: &'a [u8],
    tab: Tab,
    bm: Ptr<u64>,
    w: usize,
    kl_off: Vec<u32>,
    kl: Vec<(u32, u32)>, // (phrase, offset of key term in phrase)
    p_off: Vec<u32>,
    p_terms: Vec<u32>,
}

struct Ptr<T>(*mut T);
unsafe impl<T> Sync for Ptr<T> {}

// One sub-chunk (at most 32768 docs, one 65536-doc partition): postings of the list terms as
// the low 16 bits of the doc ids, term t at post[offs[t]..offs[t + 1]]; phrase matches.
struct WOut {
    part: u32,
    offs: Vec<u32>,
    post: Vec<u16>,
    pm: Vec<(u32, u32)>,
}

fn finish(c: &Ctx, doc: u32, ids: &[u32], kp: &[u32], pm: &mut Vec<(u32, u32)>, lastpm: &mut [u32]) {
    for &p in kp {
        let t = ids[p as usize] as usize;
        for &(ph, off) in &c.kl[c.kl_off[t] as usize..c.kl_off[t + 1] as usize] {
            if p < off || lastpm[ph as usize] == doc {
                continue;
            }
            let st = (p - off) as usize;
            let terms = &c.p_terms[c.p_off[ph as usize] as usize..c.p_off[ph as usize + 1] as usize];
            if st + terms.len() <= ids.len() && &ids[st..st + terms.len()] == terms {
                lastpm[ph as usize] = doc;
                pm.push((ph, doc));
            }
        }
    }
}

// Walks a worker stream: f(low 16 bits of the doc id, term).
#[inline(always)]
fn decode(st: &[u8], mut f: impl FnMut(u16, usize)) {
    let (mut i, mut d) = (0, 0u16);
    while i < st.len() {
        let b0 = st[i] as usize;
        if b0 == 0 {
            d = u16::from_le_bytes([st[i + 1], st[i + 2]]);
            i += 5;
            continue;
        }
        let mut v = b0 & 0x7F;
        i += 1;
        if b0 & 0x80 != 0 {
            let b1 = st[i] as usize;
            v |= (b1 & 0x7F) << 7;
            i += 1;
            if b1 & 0x80 != 0 {
                v |= (st[i] as usize) << 14;
                i += 1;
            }
        }
        f(d, v - 1);
    }
}

#[derive(Default)]
struct W {
    lc: Vec<[u16; 2]>, // per term: last doc seen (relative to base; chunks hold at most 32768 docs), docs
    base: u32,
    lastpm: Vec<u32>,
    stream: Vec<u8>, // per doc with list terms: 0, doc (4 bytes LE), then LEB128(term + 1) each
    pm: Vec<(u32, u32)>,
    ids: Vec<u32>,
    kp: Vec<u32>,
    bms: Vec<u32>,
    lst: Vec<u32>,
    ks: Vec<(u64, u32, u32)>, // per token: (bytes 0..8, bytes 8..12, h12), or (term lookup result, MAX, 0) when longer
}

impl W {
    // All tokens of one document are in ks: hash and prefetch first, then look up and index.
    fn doc(&mut self, c: &Ctx, doc: u32) {
        let n = self.ks.len();
        for v in [&mut self.ids, &mut self.kp, &mut self.bms, &mut self.lst] {
            v.clear();
            v.reserve(n);
        }
        for &(_, _, h) in &self.ks {
            c.tab.prefetch32(h);
        }
        // Look up every token and sort the hits by kind with branch-free appends (a miss is the
        // dummy term nt, which has no flags).
        let (mut nb, mut nl, mut kl) = (0, 0, 0);
        for (pos, &(k0, k1, h)) in self.ks.iter().enumerate() {
            let r = if k1 == u32::MAX { k0 as u32 } else { c.tab.get12(k0, k1, h) };
            let r = if r == MISS { c.dummy } else { r };
            let ti = r & 0xFF_FFFF;
            let fl = r >> 24; // 1 list, 2 bitmap (bitmap index = id), 4 phrase key
            unsafe {
                *self.ids.as_mut_ptr().add(pos) = ti;
                *self.bms.as_mut_ptr().add(nb) = ti;
                nb += ((fl >> 1) & 1) as usize;
                *self.lst.as_mut_ptr().add(nl) = ti;
                nl += (fl & 1) as usize;
                *self.kp.as_mut_ptr().add(kl) = pos as u32;
                kl += ((fl >> 2) & 1) as usize;
            }
        }
        unsafe {
            self.ids.set_len(n);
            self.bms.set_len(nb);
            self.lst.set_len(nl);
            self.kp.set_len(kl);
        }
        self.ks.clear();
        // term-major bitmaps; chunks start at doc ids that are multiples of 64, so each word has
        // one writer, and 64 docs in a row reuse the same cache lines
        let col = unsafe { c.bm.0.add((doc >> 6) as usize) };
        for &t in &self.bms {
            unsafe { *col.add(t as usize * c.w) |= 1u64 << (doc & 63) };
        }
        // list terms: first occurrence in the doc goes to the stream
        if nl > 0 {
            self.stream.reserve(nl * 3 + 9);
            let (l0, st) = (self.stream.len(), self.stream.as_mut_ptr());
            unsafe { (st.add(l0) as *mut [u8; 5]).write_unaligned([0, doc as u8, (doc >> 8) as u8, (doc >> 16) as u8, (doc >> 24) as u8]) };
            let mut sl = l0 + 5;
            let rel = (doc - self.base) as u16;
            for &t in &self.lst {
                unsafe {
                    let e = self.lc.get_unchecked_mut(t as usize);
                    let new = (e[0] != rel) as u32;
                    e[0] = rel;
                    e[1] += new as u16;
                    let v = t + 1;
                    let (m1, m2) = ((v >= 1 << 7) as u32, (v >= 1 << 14) as u32);
                    (st.add(sl) as *mut u32).write_unaligned((v & 0x7F | m1 << 7) | ((v >> 7) & 0x7F | m2 << 7) << 8 | (v >> 14) << 16);
                    sl += (new * (1 + m1 + m2)) as usize;
                }
            }
            unsafe { self.stream.set_len(sl) };
        }
        finish(c, doc, &self.ids, &self.kp, &mut self.pm, &mut self.lastpm);
    }
}

// Index buf[a..b); a new output starts at each cut (every 32768 docs) so every output lies in one
// 65536-doc partition and holds at most 32768 docs.
fn worker(c: &Ctx, a: usize, b: usize, cuts: &[usize]) -> Vec<WOut> {
    let nt = c.tab.keys.len();
    let buf = c.buf;
    let mut w = W { lc: hvec(nt + 1, [0, 0]), lastpm: vec![MISS; c.p_off.len() - 1], ..Default::default() };
    let mut outs = Vec::new();
    let mut bounds: Vec<usize> = cuts.iter().copied().filter(|&x| x > a && x < b).collect();
    bounds.insert(0, a);
    bounds.push(b);
    for r in bounds.windows(2) {
        let (a, b) = (r[0], r[1]);
        let mut doc = MISS;
        w.lc.fill([u16::MAX, 0]);
        w.base = parse_u32(&buf[a..a + buf[a..].iter().position(|&c| c == b'\t').unwrap()]);
        w.ks.reserve(1 << 12);
        scan(buf, a, b, |s, e, first| {
            if first {
                if doc != MISS {
                    w.doc(c, doc);
                }
                doc = parse_u32(&buf[s..e]);
            } else {
                let len = e - s;
                let ent = if len <= 12 {
                    let (k0, k1) = load12(buf, s, len);
                    (k0, k1, h12(k0, k1))
                } else {
                    (if len <= 32 { c.tab.get(&load_key(buf, s, len)) } else { MISS } as u64, u32::MAX, 0)
                };
                if w.ks.len() == w.ks.capacity() {
                    w.ks.reserve(w.ks.len());
                }
                unsafe { w.ks.as_mut_ptr().add(w.ks.len()).write(ent) };
                unsafe { w.ks.set_len(w.ks.len() + 1) };
            }
        });
        if doc != MISS {
            w.doc(c, doc);
        }
        // local counting sort of the stream (small enough to stay in cache)
        let mut offs = Vec::with_capacity(nt + 1);
        offs.push(0);
        let mut acc = 0;
        for e in &w.lc[..nt] {
            offs.push(acc); // offs[t + 1] is the write cursor of t, then its end
            acc += e[1] as u32;
        }
        let mut post = vec![0u16; acc as usize];
        decode(&w.stream, |d, t| unsafe {
            let o = offs.get_unchecked_mut(t + 1);
            *post.get_unchecked_mut(*o as usize) = d;
            *o += 1;
        });
        w.stream.clear();
        outs.push(WOut { offs, post, pm: std::mem::take(&mut w.pm), part: doc >> 16 });
    }
    outs
}

// A set of doc ids: a sorted list, an owned bitmap or a borrowed (index) bitmap.
enum Set<'a> {
    L(Vec<u32>),
    B(Vec<u64>),
    R(&'a [u64]),
}

fn gallop_ge(b: &[u32], mut lo: usize, x: u32) -> usize {
    let mut step = 1;
    let mut hi = lo;
    while hi < b.len() && b[hi] < x {
        lo = hi + 1;
        hi += step;
        step <<= 1;
    }
    let hi = hi.min(b.len());
    lo + b[lo..hi].partition_point(|&y| y < x)
}

fn inter(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (a, b) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut r = Vec::with_capacity(a.len());
    if a.len() * 16 < b.len() {
        let mut j = 0;
        for &x in a {
            j = gallop_ge(b, j, x);
            if j == b.len() {
                break;
            }
            if b[j] == x {
                r.push(x);
            }
        }
    } else {
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            let (x, y) = (a[i], b[j]);
            if x == y {
                r.push(x);
            }
            i += (x <= y) as usize;
            j += (y <= x) as usize;
        }
    }
    r
}

fn union(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut r = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        r.push(x.min(y));
        i += (x <= y) as usize;
        j += (y <= x) as usize;
    }
    r.extend_from_slice(&a[i..]);
    r.extend_from_slice(&b[j..]);
    r
}

fn diff(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut r = Vec::with_capacity(a.len());
    let mut j = 0;
    for &x in a {
        j = gallop_ge(b, j, x);
        if j == b.len() || b[j] != x {
            r.push(x);
        }
    }
    r
}

#[inline(always)]
fn tb(b: &[u64], x: u32) -> bool {
    b[(x >> 6) as usize] >> (x & 63) & 1 != 0
}

fn to_bits(a: &[u32], w: usize) -> Vec<u64> {
    let mut b = vec![0u64; w];
    for &x in a {
        b[(x >> 6) as usize] |= 1 << (x & 63);
    }
    b
}

fn apply<'a>(r: Set<'a>, op: u8, s: Set<'a>, w: usize) -> Set<'a> {
    use Set::*;
    let own = |r: Set| match r {
        B(v) => v,
        R(v) => v.to_vec(),
        L(_) => unreachable!(),
    };
    let (sl, sb): (&[u32], &[u64]) = match &s {
        L(v) => (v, &[]),
        B(v) => (&[], v),
        R(v) => (&[], v),
    };
    let sbits = !matches!(s, L(_));
    match (op, r) {
        (b'A', L(a)) if !sbits => L(inter(&a, sl)),
        (b'A', L(mut a)) => {
            a.retain(|&x| tb(sb, x));
            L(a)
        }
        (b'O', L(a)) if !sbits => {
            let u = union(&a, sl);
            if u.len() > w * 2 {
                B(to_bits(&u, w))
            } else {
                L(u)
            }
        }
        (b'O', L(a)) => {
            let mut v = sb.to_vec();
            a.iter().for_each(|&x| v[(x >> 6) as usize] |= 1 << (x & 63));
            B(v)
        }
        (_, L(a)) if !sbits => L(diff(&a, sl)),
        (_, L(mut a)) => {
            a.retain(|&x| !tb(sb, x));
            L(a)
        }
        (b'A', R(a)) if !sbits => L(sl.iter().copied().filter(|&x| tb(a, x)).collect()),
        (b'A', B(a)) if !sbits => L(sl.iter().copied().filter(|&x| tb(&a, x)).collect()),
        (op, r) => {
            let mut a = own(r);
            match (op, sbits) {
                (b'A', _) => a.iter_mut().zip(sb).for_each(|(x, y)| *x &= y),
                (b'O', false) => sl.iter().for_each(|&x| a[(x >> 6) as usize] |= 1 << (x & 63)),
                (b'O', true) => a.iter_mut().zip(sb).for_each(|(x, y)| *x |= y),
                (_, false) => sl.iter().for_each(|&x| a[(x >> 6) as usize] &= !(1 << (x & 63))),
                (_, true) => a.iter_mut().zip(sb).for_each(|(x, y)| *x &= !y),
            }
            B(a)
        }
    }
}

fn put(o: &mut Vec<u8>, mut v: u64) {
    let mut t = [0u8; 20];
    let mut i = 20;
    loop {
        i -= 1;
        t[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    o.extend_from_slice(&t[i..]);
}

fn emit_list(o: &mut Vec<u8>, a: &[u32]) {
    put(o, a.len() as u64);
    o.push(b' ');
    put(o, a.iter().map(|&x| x as u64).sum());
    for &x in &a[..a.len().min(10)] {
        o.push(b' ');
        put(o, x as u64);
    }
    o.push(b'\n');
}

fn emit_bits(o: &mut Vec<u8>, b: &[u64]) {
    const M: [u64; 6] = [0xAAAA_AAAA_AAAA_AAAA, 0xCCCC_CCCC_CCCC_CCCC, 0xF0F0_F0F0_F0F0_F0F0, 0xFF00_FF00_FF00_FF00, 0xFFFF_0000_FFFF_0000, 0xFFFF_FFFF_0000_0000];
    let (mut n, mut s) = (0u64, 0u64);
    for (i, &w) in b.iter().enumerate() {
        let c = w.count_ones() as u64;
        n += c;
        s += c * (i as u64 * 64) + (0..6).map(|k| ((w & M[k]).count_ones() as u64) << k).sum::<u64>();
    }
    put(o, n);
    o.push(b' ');
    put(o, s);
    let mut k = 0;
    for (i, &w) in b.iter().enumerate() {
        let mut w = w;
        while w != 0 && k < 10 {
            o.push(b' ');
            put(o, (i * 64) as u64 + w.trailing_zeros() as u64);
            w &= w - 1;
            k += 1;
        }
        if k == 10 {
            break;
        }
    }
    o.push(b'\n');
}

enum Q {
    Bool(usize, usize), // range into ops
    Phrase(u32),
}

fn main() {
    let t0 = std::time::Instant::now();
    let dbg = std::env::var("BT").is_ok();
    let lap = |m: &str| {
        if dbg {
            let st = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
            let ra = st.lines().find(|l| l.starts_with("RssAnon")).unwrap_or("").split_whitespace().nth(1).unwrap_or("0").to_string();
            eprintln!("{m}: {:.1} ms [anon {} MB]", t0.elapsed().as_secs_f64() * 1e3, ra.parse::<u64>().unwrap_or(0) / 1024)
        }
    };
    let args: Vec<String> = std::env::args().collect();
    let mut data = "/data".to_string();
    let mut out = "/out".to_string();
    for i in 1..args.len() {
        if args[i] == "--data" {
            data = args[i + 1].clone();
        } else if args[i] == "--out" {
            out = args[i + 1].clone();
        }
    }
    #[cfg(target_os = "linux")]
    {
        extern "C" {
            fn mallopt(p: i32, v: i32) -> i32;
        }
        // fixed mmap threshold: big buffers go back to the OS when freed, which keeps the peak low
        unsafe { mallopt(-3, 1 << 17) };
    }
    let qb = std::fs::read(format!("{data}/queries.txt")).unwrap();
    lap("qread");
    let buf = load(&format!("{data}/docs.txt"));
    lap("mapped");

    // ---- parse queries, intern terms
    let mut tab = Tab::new((qb.len() / 12).next_power_of_two().trailing_zeros().max(4));
    let mut intern = |t: &[u8]| tab.intern(&key_of(t));
    let qb: &'static [u8] = Box::leak(qb.into_boxed_slice());
    let mut qs = Vec::new();
    let mut ops: Vec<(u8, u32)> = Vec::new();
    let mut is_bool_term: Vec<bool> = Vec::new();
    let (mut p_terms, mut p_off) = (Vec::new(), vec![0u32]);
    for line in qb.split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
        if line.starts_with(b"BOOL ") {
            let st = ops.len();
            let mut op = 0u8;
            for (i, wd) in line[5..].split(|&c| c == b' ').enumerate() {
                if i % 2 == 1 {
                    op = wd[0];
                } else {
                    let t = intern(wd);
                    ops.push((op, t));
                }
            }
            qs.push(Q::Bool(st, ops.len()));
        } else {
            line[8..line.len() - 1].split(|&c| c == b' ').for_each(|w| p_terms.push(intern(w)));
            qs.push(Q::Phrase(p_off.len() as u32 - 1));
            p_off.push(p_terms.len() as u32);
        }
    }
    lap("qloop");
    let nt = tab.keys.len();
    is_bool_term.resize(nt, false);
    for &(_, t) in &ops {
        is_bool_term[t as usize] = true;
    }
    lap("parsed");

    // ---- document count from the last line
    let n_docs = if buf.is_empty() {
        0
    } else {
        let e = buf.len() - 1;
        let s = buf[..e].iter().rposition(|&c| c == b'\n').map_or(0, |p| p + 1);
        let t = s + buf[s..].iter().position(|&c| c == b'\t').unwrap();
        parse_u32(&buf[s..t]) as usize + 1
    };
    let w = (n_docs + 63) / 64;

    // ---- sample 2 MB spread over the corpus (in parallel) to estimate document frequencies
    let nth = std::env::var("NT").ok().and_then(|v| v.parse().ok()).unwrap_or_else(|| std::thread::available_parallelism().map_or(8, |n| n.get()));
    let (mut sdf, mut sdocs) = (vec![0u32; nt], 0u32);
    for (d, n) in std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nth)
            .map(|i| {
                let tab = &tab;
                sc.spawn(move || {
                    let a = line_start(buf, buf.len() * i / nth);
                    let b = line_start(buf, (a + (2 << 20) / nth).min(buf.len()));
                    let (mut df, mut last, mut docs) = (vec![0u32; nt], vec![MISS; nt], 0u32);
                    scan(buf, a, b, |s, e, first| {
                        if first {
                            docs += 1;
                        } else if e - s <= 32 {
                            let t = tab.get(&load_key(buf, s, e - s)) as usize;
                            if t != MISS as usize && last[t] != docs {
                                last[t] = docs;
                                df[t] += 1;
                            }
                        }
                    });
                    (df, docs)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
    }) {
        sdf.iter_mut().zip(&d).for_each(|(x, y)| *x += y);
        sdocs += n;
    }
    let sdocs = sdocs.max(1);
    lap("sampscan");
    // BOOL terms in more than 1/40 of the sample get a bitmap, the other ones a sorted list.
    // Renumber: bitmap terms first (bitmap index = id), then by sampled frequency so hot data shares cache lines.
    let isbm = |t: usize| is_bool_term[t] && sdf[t] as u64 * 40 >= sdocs as u64;
    let mut perm: Vec<u32> = (0..nt as u32).collect();
    perm.sort_by_key(|&t| (!isbm(t as usize), std::cmp::Reverse(sdf[t as usize])));
    let nbm = (0..nt).filter(|&t| isbm(t)).count();
    let mut nid = vec![0u32; nt];
    perm.iter().enumerate().for_each(|(n, &o)| nid[o as usize] = n as u32);
    lap("perm");
    tab.keys = perm.iter().map(|&o| tab.keys[o as usize]).collect();
    tab.slots.iter_mut().filter(|s| s.0 != 0).for_each(|s| s.2 = nid[s.2 as usize]);
    lap("retab");
    let lens: Vec<u32> = tab.keys.iter().map(|k| k.iter().map(|w| 8 - w.leading_zeros() / 8).sum()).collect();
    let is_bool_term: Vec<bool> = perm.iter().map(|&o| is_bool_term[o as usize]).collect();
    let sdf: Vec<u32> = perm.iter().map(|&o| sdf[o as usize]).collect();
    ops.iter_mut().for_each(|o| o.1 = nid[o.1 as usize]);
    p_terms.iter_mut().for_each(|t| *t = nid[*t as usize]);
    let np = p_off.len() - 1;
    let plist = |i: usize| &p_terms[p_off[i] as usize..p_off[i + 1] as usize];
    lap("sampled");

    let mut flags: Vec<u8> = (0..nt).map(|t| if t < nbm { 2 } else { is_bool_term[t] as u8 }).collect();
    // phrase keys: the rarest term of each phrase
    let mut kcnt = vec![0u32; nt + 1];
    let mut pkey = Vec::with_capacity(np);
    for p in (0..np).map(plist) {
        let mut best = 0;
        for i in 1..p.len() {
            let (a, b) = (p[i] as usize, p[best] as usize);
            if sdf[a] < sdf[b] || (sdf[a] == sdf[b] && lens[a] > lens[b]) {
                best = i;
            }
        }
        let t = p[best] as usize;
        flags[t] |= 4;
        kcnt[t + 1] += 1;
        pkey.push(best as u32);
    }
    for t in 0..nt {
        kcnt[t + 1] += kcnt[t];
    }
    let kl_off = kcnt.clone();
    let mut kl = vec![(0u32, 0u32); np];
    for i in 0..np {
        let t = plist(i)[pkey[i] as usize] as usize;
        kl[kcnt[t] as usize] = (i as u32, pkey[i]);
        kcnt[t] += 1;
    }
    tab.set_flags(&flags);
    let mut bmv = hvec((nbm + 1) * w, 0u64);
    let ctx = Ctx { dummy: nt as u32, buf, tab, bm: Ptr(bmv.as_mut_ptr()), w, kl_off, kl, p_off, p_terms };

    // ---- parallel index pass
    let id_at = |p: usize| if p >= buf.len() { u32::MAX } else { parse_u32(&buf[p..p + buf[p..].iter().position(|&c| c == b'\t').unwrap()]) };
    let mut cuts = vec![0usize];
    for i in 1..nth {
        let mut p = line_start(buf, (buf.len() * i / nth).max(*cuts.last().unwrap()));
        while id_at(p) % 64 != 0 {
            p += buf[p..].iter().position(|&c| c == b'\n').unwrap() + 1;
        }
        cuts.push(p);
    }
    cuts.push(buf.len());
    // line starts of docs 32768, 65536, ... (binary search: ids ascend with the offset)
    let pcuts: Vec<usize> = (1..(n_docs + 32767) >> 15)
        .map(|k| {
            let (mut lo, mut hi) = (0, buf.len());
            while lo < hi {
                let mid = (lo + hi) / 2;
                if id_at(line_start(buf, mid)) < (k << 15) as u32 { lo = mid + 1 } else { hi = mid }
            }
            line_start(buf, lo)
        })
        .collect();
    let outs: Vec<WOut> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nth)
            .map(|i| {
                let (a, b, c, pcuts) = (cuts[i], cuts[i + 1], &ctx, &pcuts);
                sc.spawn(move || {
                    let r = if a < b { worker(c, a, b, pcuts) } else { vec![] };
                    if dbg { eprint!("w{i}:{:.0} ", t0.elapsed().as_secs_f64() * 1e3) }
                    r
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });

    lap("indexed");
    let total: usize = outs.iter().map(|o| o.post.len()).sum();
    let mut pres: Vec<Vec<u32>> = vec![Vec::new(); np];
    for o in &outs {
        for &(p, d) in &o.pm {
            pres[p as usize].push(d);
        }
    }
    lap("csr");
    if dbg { eprintln!("nt {nt} nbm {nbm} list postings {total} phrases {np}"); }
    let bmw: &[u64] = &bmv;
    let src = |t: u32| -> Set {
        let t = t as usize;
        if t < nbm {
            let i = t * w;
            Set::R(&bmw[i..i + w])
        } else {
            let mut v = Vec::with_capacity(outs.iter().map(|o| (o.offs[t + 1] - o.offs[t]) as usize).sum());
            for o in &outs {
                let r = o.offs[t] as usize..o.offs[t + 1] as usize;
                v.extend(o.post[r].iter().map(|&x| o.part << 16 | x as u32));
            }
            Set::L(v)
        }
    };

    // ---- answer queries in parallel
    let next = AtomicUsize::new(0);
    const BLK: usize = 128;
    let nblk = (qs.len() + BLK - 1) / BLK;
    let mut parts: Vec<(usize, Vec<u8>)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nth)
            .map(|_| {
                let (next, qs, ops, src, pres) = (&next, &qs, &ops, &src, &pres);
                sc.spawn(move || {
                    let mut res = Vec::new();
                    loop {
                        let b = next.fetch_add(1, Relaxed);
                        if b >= nblk {
                            break;
                        }
                        let mut o = Vec::with_capacity(BLK * 32);
                        for q in &qs[b * BLK..((b + 1) * BLK).min(qs.len())] {
                            match *q {
                                Q::Phrase(p) => emit_list(&mut o, &pres[p as usize]),
                                Q::Bool(s, e) => {
                                    let mut r = src(ops[s].1);
                                    for &(op, t) in &ops[s + 1..e] {
                                        r = apply(r, op, src(t), w);
                                    }
                                    match r {
                                        Set::L(a) => emit_list(&mut o, &a),
                                        Set::B(a) => emit_bits(&mut o, &a),
                                        Set::R(a) => emit_bits(&mut o, a),
                                    }
                                }
                            }
                        }
                        res.push((b, o));
                    }
                    res
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    lap("queries");
    parts.sort_unstable_by_key(|p| p.0);
    let mut o = Vec::with_capacity(parts.iter().map(|p| p.1.len()).sum());
    for p in parts {
        o.extend_from_slice(&p.1);
    }
    let _ = std::fs::create_dir_all(&out);
    std::fs::write(format!("{out}/results.txt"), &o).unwrap();
    lap("written");
    if dbg {
        eprintln!("{}", std::fs::read_to_string("/proc/self/status").unwrap_or_default().lines().filter(|l| l.starts_with("VmHWM") || l.starts_with("Rss")).collect::<Vec<_>>().join(" | "));
    }
}
