// Speed Demon 2026, Track B (Index). std only.
// One parallel pass over docs.txt: tokenize (AVX2 byte classification), look up each token in a
// table of the query terms only, build posting lists (u32 lists or bitmaps) for BOOL terms and
// match every phrase inline at its rarest term. Then answer the queries in parallel.
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

const MISS: u32 = u32::MAX;
const PD: usize = 16;
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
    long: Vec<(u32, Key)>, // after keys is dropped: the terms longer than 12 bytes, by id
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
        Tab {
            slots: hvec(1 << bits, (0, 0, 0)),
            sh: 64 - bits,
            keys: Vec::new(),
            long: Vec::new(),
        }
    }
    fn intern_h(&mut self, k: &Key, h: u64) -> u32 {
        let id = self.get_h(k, h);
        if id != MISS {
            return id;
        }
        if self.keys.len() * 2 >= self.slots.len() {
            let keys = std::mem::take(&mut self.keys);
            *self = Tab::new(self.slots.len().trailing_zeros() + 1);
            keys.iter().for_each(|k| _ = self.intern_h(k, hk(k)));
        }
        let m = self.slots.len() - 1;
        let mut i = (h >> self.sh) as usize;
        while self.slots[i].0 != 0 {
            i = (i + 1) & m;
        }
        self.slots[i] = (k[0], k1(k), self.keys.len() as u32);
        self.keys.push(*k);
        self.keys.len() as u32 - 1
    }
    #[cold]
    fn key_eq(&self, id: u32, k: &Key) -> bool {
        if self.keys.is_empty() {
            self.long
                .binary_search_by_key(&id, |x| x.0)
                .map_or(false, |i| self.long[i].1 == *k)
        } else {
            self.keys[id as usize] == *k
        }
    }
    // Keeps only the keys of terms longer than 12 bytes (the only ones lookups compare in full).
    fn drop_keys(&mut self) {
        self.long = self
            .keys
            .iter()
            .enumerate()
            .filter(|(_, k)| k[1] >> 32 != 0)
            .map(|(i, k)| (i as u32, *k))
            .collect();
        self.keys = Vec::new();
    }
    // New ids (nid[old]) with their flags (f[new]) in the top byte.
    fn renumber(&mut self, nid: &[u32], f: &[u8]) {
        for s in self.slots.iter_mut().filter(|s| s.0 != 0) {
            s.2 = nid[s.2 as usize] | (f[nid[s.2 as usize] as usize] as u32) << 24;
        }
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
            _mm_prefetch(
                self.slots.as_ptr().add((h >> (self.sh - 32)) as usize) as *const i8,
                _MM_HINT_T0,
            )
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
            if s.0 == k[0] && s.1 == b && (b >> 31 == 0 || self.key_eq(s.2 & 0xFF_FFFF, k)) {
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
    unsafe { std::mem::transmute(b) }
}

static MK: [[u8; 32]; 2] = [[0xFF; 32], [0; 32]]; // MK[32 - len..][..32]: len bytes of 0xFF
                                                  // Token bytes [s, s+len) lowercased and zero padded to 32 bytes (len <= 32).
#[inline(always)]
fn load_key(buf: &[u8], s: usize, len: usize) -> Key {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    if s + 32 <= buf.len() {
        unsafe {
            use std::arch::x86_64::*;
            let v = _mm256_or_si256(
                _mm256_loadu_si256(buf.as_ptr().add(s) as *const __m256i),
                _mm256_set1_epi8(0x20),
            );
            return std::mem::transmute(_mm256_and_si256(
                v,
                _mm256_loadu_si256((MK.as_ptr() as *const u8).add(32 - len) as *const __m256i),
            ));
        }
    }
    let mut b = [0u8; 32];
    let n = (buf.len() - s).min(32);
    b[..n].copy_from_slice(&buf[s..s + n]);
    for i in 0..32 {
        b[i] = (b[i] | 0x20) & if i < len { 0xFF } else { 0 };
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
            let v = _mm_or_si128(
                _mm_loadu_si128(buf.as_ptr().add(s) as *const __m128i),
                _mm_set1_epi8(0x20),
            );
            let v = _mm_and_si128(
                v,
                _mm_loadu_si128((MK.as_ptr() as *const u8).add(32 - len) as *const __m128i),
            );
            return (
                _mm_cvtsi128_si64(v) as u64,
                _mm_extract_epi32::<2>(v) as u32,
            );
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
        let l = _mm256_cmpgt_epi8(
            _mm256_set1_epi8(-102),
            _mm256_add_epi8(lo, _mm256_set1_epi8(0x1F)),
        );
        let d = _mm256_cmpgt_epi8(
            _mm256_set1_epi8(-118),
            _mm256_add_epi8(v, _mm256_set1_epi8(0x50)),
        );
        let n = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(10));
        (
            _mm256_movemask_epi8(_mm256_or_si256(l, d)) as u32 as u64,
            _mm256_movemask_epi8(n) as u32 as u64,
        )
    };
    let (a, b) = (
        f(_mm256_loadu_si256(p as *const __m256i)),
        f(_mm256_loadu_si256(p.add(32) as *const __m256i)),
    );
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

// Tokens of the line that starts at p: pos gets the start and end of every maximal [A-Za-z0-9] run,
// relative to p. Returns the length of the line with its newline.
#[inline(always)]
fn line_tokens(buf: &[u8], p: usize, pos: &mut Vec<u32>) -> usize {
    pos.clear();
    let (mut q, mut carry) = (p, 0u64);
    loop {
        pos.reserve(72);
        let (mut m, nl) = if q + 64 <= buf.len() {
            unsafe { mask64(buf.as_ptr().add(q)) }
        } else {
            let mut t = [b'\n'; 64];
            t[..buf.len() - q].copy_from_slice(&buf[q..]);
            unsafe { mask64(t.as_ptr()) }
        };
        let lim = nl.trailing_zeros();
        if lim < 64 {
            m &= (1u64 << lim) - 1;
        }
        // bits of t: token starts and ends (a run cut at the newline ends there)
        let mut t = m ^ (m << 1 | carry);
        carry = m >> 63;
        let (off, cnt, n0) = ((q - p) as u32, t.count_ones() as usize, pos.len());
        unsafe {
            let o = pos.as_mut_ptr().add(n0);
            let mut k = 0;
            while k < cnt {
                for u in 0..8 {
                    *o.add(k + u) = off + t.trailing_zeros();
                    t &= t.wrapping_sub(1);
                }
                k += 8;
            }
            pos.set_len(n0 + cnt);
        }
        if lim < 64 {
            return q - p + lim as usize + 1;
        }
        q += 64;
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
            let p = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, f.as_raw_fd(), 0) };
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
        let (a, e) = (
            x.as_ptr() as usize,
            x.as_ptr() as usize + n * std::mem::size_of::<T>(),
        );
        let (a, e) = ((a + (1 << 21) - 1) & !((1 << 21) - 1), e & !((1 << 21) - 1));
        if e > a {
            unsafe { madvise(a as *mut u8, e - a, 14) };
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
    lo: usize, // list terms are lo..
    ew: usize, // bytes per stream entry: 2, or 3 when there are 65535 list terms or morelo + nl
    nl: usize,
    buf: &'a [u8],
    tab: Tab,
    bm: Ptr<u64>, // bitmaps of the current round: row t (w words) has bit d - d0 for doc d
    d0: u32,
    w: usize,
    kl_off: Vec<u32>,
    kl: Vec<(u32, u32)>, // (phrase, offset of key term in phrase)
    p_off: Vec<u32>,
    p_terms: Vec<u32>,
}

// f(0), ..., f(n - 1) on n threads.
fn par<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    std::thread::scope(|sc| {
        let f = &f;
        let hs: Vec<_> = (0..n).map(|i| sc.spawn(move || f(i))).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

struct Ptr<T>(*mut T);
unsafe impl<T> Sync for Ptr<T> {}
impl<T> Ptr<T> {
    fn at(&self, i: usize) -> *mut T {
        unsafe { self.0.add(i) }
    }
}

// One output (at most 32768 docs, one 65536-doc partition): postings of the list terms as
// the low 16 bits of the doc ids, list term lo + t at post[offs[t]..offs[t + 1]]; phrase matches.
struct WOut {
    offs: Vec<u32>,
    post: Vec<u16>,
    pm: Vec<(u32, u32)>,
}

fn finish(
    c: &Ctx,
    doc: u32,
    ids: &[u32],
    kp: &[u32],
    pm: &mut Vec<(u32, u32)>,
    lastpm: &mut [u32],
) {
    for &p in kp {
        let t = ids[p as usize] as usize;
        for &(ph, off) in &c.kl[c.kl_off[t] as usize..c.kl_off[t + 1] as usize] {
            if p < off || lastpm[ph as usize] == doc {
                continue;
            }
            let st = (p - off) as usize;
            let terms =
                &c.p_terms[c.p_off[ph as usize] as usize..c.p_off[ph as usize + 1] as usize];
            if st + terms.len() <= ids.len() && terms.iter().zip(&ids[st..]).all(|(a, b)| a == b) {
                lastpm[ph as usize] = doc;
                pm.push((ph, doc));
            }
        }
    }
}

// Walks a worker stream of ew-byte entries: f(low 16 bits of the doc id, term).
#[inline(always)]
fn decode(st: &[u8], ew: usize, mut f: impl FnMut(u16, usize)) {
    let (mut i, mut d, m) = (0, 0u16, (1u32 << (8 * ew)) - 1);
    while i < st.len() {
        let v = unsafe { (st.as_ptr().add(i) as *const u32).read_unaligned() } & m;
        i += ew;
        if v == 0 {
            d = u16::from_le_bytes([st[i], st[i + 1]]);
            i += 4;
        } else {
            f(d, v as usize - 1);
        }
    }
}

#[derive(Default)]
struct W {
    lc: Vec<[u16; 2]>, // per list term: last doc seen (relative to base; outputs hold at most 32768 docs), docs
    base: u32,
    lastpm: Vec<u32>,
    stream: Vec<u8>, // per doc with list terms: 0, doc (4 bytes LE), then term + 1 each; ew-byte entries (+3 slack bytes)
    pm: Vec<(u32, u32)>,
    ids: Vec<u32>,
    kp: Vec<u32>,
    pos: Vec<u32>,
    sb: Vec<u64>, // the current 64-doc block's bitmap words (one per bitmap term), kept in L1
    blk: usize,   // that block's word index in the rows (MAX: none)
    ks: Vec<(u64, u32, u32)>, // per token: (bytes 0..8, bytes 8..12, h12), or (term lookup result, MAX, 0) when longer
}

impl W {
    // Writes the current block's bitmap words into the rows (each block has one writer).
    fn flush(&mut self, c: &Ctx) {
        if self.blk != usize::MAX {
            let d = unsafe { c.bm.0.add(self.blk) };
            for (t, x) in self.sb.iter_mut().enumerate() {
                unsafe { *d.add(t * c.w) = *x };
                *x = 0;
            }
        }
    }
    // All tokens of one document are in ks: hash and prefetch first, then look up and index.
    fn doc(&mut self, c: &Ctx, doc: u32) {
        let n = self.ks.len();
        self.ids.clear();
        self.ids.reserve(n);
        self.kp.clear();
        self.kp.reserve(n);
        self.stream.reserve(n * 3 + 12);
        for &(_, _, h) in self.ks.iter().take(PD) {
            c.tab.prefetch32(h);
        }
        // One pass: look up, set the bitmap bit of bitmap terms, record the first occurrence of a
        // list term in the stream, note phrase key positions. Term-major bitmaps: chunks start at
        // doc ids that are multiples of 64, so each word has one writer.
        let b = ((doc - c.d0) >> 6) as usize;
        if b != self.blk {
            self.flush(c);
            self.blk = b;
        }
        let sb = self.sb.as_mut_ptr();
        let bit = 1u64 << (doc & 63);
        let rel = (doc - self.base) as u16;
        let (l0, st, lc) = (self.stream.len(), self.stream.as_mut_ptr(), unsafe {
            self.lc.as_mut_ptr().sub(c.lo)
        });
        unsafe {
            (st.add(l0) as *mut u32).write_unaligned(0);
            (st.add(l0 + c.ew) as *mut u32).write_unaligned(doc);
        }
        let (mut sl, mut kl) = (l0 + c.ew + 4, 0);
        for (pos, &(k0, k1, h)) in self.ks.iter().enumerate() {
            if let Some(x) = self.ks.get(pos + PD) {
                c.tab.prefetch32(x.2);
            }
            let r = if k1 == u32::MAX {
                k0 as u32
            } else {
                c.tab.get12(k0, k1, h)
            };
            let r = if r == MISS { c.dummy } else { r };
            let ti = (r & 0xFF_FFFF) as usize;
            let fl = r >> 24; // 1 list, 2 bitmap (bitmap index = id), 4 phrase key
            unsafe {
                *self.ids.as_mut_ptr().add(pos) = ti as u32;
                if fl & 2 != 0 {
                    *sb.add(ti) |= bit;
                }
                if fl & 1 != 0 {
                    let e = &mut *lc.add(ti);
                    if e[0] != rel {
                        e[0] = rel;
                        e[1] += 1;
                        (st.add(sl) as *mut u32).write_unaligned((ti - c.lo) as u32 + 1);
                        sl += c.ew;
                    }
                }
                *self.kp.as_mut_ptr().add(kl) = pos as u32;
                kl += ((fl >> 2) & 1) as usize;
            }
        }
        unsafe {
            self.ids.set_len(n);
            self.kp.set_len(kl);
            self.stream
                .set_len(if sl == l0 + c.ew + 4 { l0 } else { sl });
        }
        self.ks.clear();
        finish(c, doc, &self.ids, &self.kp, &mut self.pm, &mut self.lastpm);
    }
}

// Index the docs from bounds[0] to the last bound, one output per pair of bounds (at most 32768 docs).
fn worker(c: &Ctx, bounds: &[usize]) -> Vec<WOut> {
    let buf = c.buf;
    let nt = c.nl;
    let mut w = W {
        lc: hvec(nt, [0, 0]),
        lastpm: vec![MISS; c.p_off.len() - 1],
        sb: vec![0; c.lo],
        blk: usize::MAX,
        ..Default::default()
    };
    let mut outs = Vec::new();
    for r in bounds.windows(2) {
        let (a, b) = (r[0], r[1]);
        if a == b {
            continue;
        }
        let mut doc;
        w.lc.fill([u16::MAX, 0]);
        w.base = parse_u32(&buf[a..a + buf[a..].iter().position(|&c| c == b'\t').unwrap()]);
        let mut p = a;
        while p < b {
            let len = line_tokens(buf, p, &mut w.pos);
            let np = w.pos.len() / 2;
            if np > 0 {
                let pos = &w.pos;
                doc = parse_u32(&buf[p + pos[0] as usize..p + pos[1] as usize]);
                w.ks.clear();
                w.ks.reserve(np);
                let ks = w.ks.as_mut_ptr();
                for j in 1..np {
                    let (s, e) = unsafe {
                        (
                            p + *pos.get_unchecked(2 * j) as usize,
                            p + *pos.get_unchecked(2 * j + 1) as usize,
                        )
                    };
                    let len = e - s;
                    let ent = if len <= 12 {
                        let (k0, k1) = load12(buf, s, len);
                        (k0, k1, h12(k0, k1))
                    } else {
                        (
                            if len <= 32 {
                                c.tab.get(&load_key(buf, s, len))
                            } else {
                                MISS
                            } as u64,
                            u32::MAX,
                            0,
                        )
                    };
                    unsafe { ks.add(j - 1).write(ent) };
                }
                unsafe { w.ks.set_len(np - 1) };
                w.doc(c, doc);
            }
            p += len;
        }
        // local counting sort of the stream (small enough to stay in cache)
        let mut offs = Vec::with_capacity(nt + 1);
        offs.push(0);
        let mut acc = 0;
        for e in &w.lc {
            offs.push(acc); // offs[t + 1] is the write cursor of t, then its end
            acc += e[1] as u32;
        }
        let mut post = vec![0u16; acc as usize];
        decode(&w.stream, c.ew, |d, t| unsafe {
            let o = offs.get_unchecked_mut(t + 1);
            *post.get_unchecked_mut(*o as usize) = d;
            *o += 1;
        });
        w.stream.clear();
        outs.push(WOut {
            offs,
            post,
            pm: std::mem::take(&mut w.pm),
        });
    }
    w.flush(c);
    outs
}

// A set of doc ids: a sorted list, an owned bitmap or a borrowed (index) bitmap.
enum Set<'a> {
    L(Vec<u32>),
    B(Vec<u64>),
    R(&'a [u64]),
}

fn union(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut r: Vec<u32> = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j, mut k, d) = (0, 0, 0, r.as_mut_ptr());
    unsafe {
        while i < a.len() && j < b.len() {
            let (x, y) = (*a.get_unchecked(i), *b.get_unchecked(j));
            *d.add(k) = x.min(y);
            k += 1;
            i += (x <= y) as usize;
            j += (y <= x) as usize;
        }
        r.set_len(k);
    }
    r.extend_from_slice(&a[i..]);
    r.extend_from_slice(&b[j..]);
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
            a.iter()
                .for_each(|&x| v[(x >> 6) as usize] |= 1 << (x & 63));
            B(v)
        }
        (_, L(mut a)) => {
            a.retain(|&x| !tb(sb, x));
            L(a)
        }
        (b'A', R(a)) if !sbits => L(sl.iter().copied().filter(|&x| tb(a, x)).collect()),
        (b'A', B(a)) if !sbits => L(sl.iter().copied().filter(|&x| tb(&a, x)).collect()),
        (op, R(a)) if sbits => B(match op {
            b'A' => a.iter().zip(sb).map(|(x, y)| x & y).collect(),
            b'O' => a.iter().zip(sb).map(|(x, y)| x | y).collect(),
            _ => a.iter().zip(sb).map(|(x, y)| x & !y).collect(),
        }),
        (op, r) => {
            let mut a = own(r);
            match (op, sbits) {
                (b'A', _) => a.iter_mut().zip(sb).for_each(|(x, y)| *x &= y),
                (b'O', false) => sl
                    .iter()
                    .for_each(|&x| a[(x >> 6) as usize] |= 1 << (x & 63)),
                (b'O', true) => a.iter_mut().zip(sb).for_each(|(x, y)| *x |= y),
                (_, false) => sl
                    .iter()
                    .for_each(|&x| a[(x >> 6) as usize] &= !(1 << (x & 63))),
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

// Result of a query summed over the rounds: count, sum of ids, the first (up to) 10 ids.
#[derive(Clone, Copy, Default)]
struct Acc {
    s: u64,
    n: u32,
    k: u32,
    ids: [u32; 10],
}

fn add(r: &mut Acc, x: u32) {
    r.n += 1;
    r.s += x as u64;
    if r.k < 10 {
        r.ids[r.k as usize] = x;
        r.k += 1;
    }
}

// Adds a round's result, ids relative to d0 (a sorted list).
fn acc_list(r: &mut Acc, a: &[u32], d0: u32) {
    r.n += a.len() as u32;
    r.s += a.iter().map(|&x| x as u64).sum::<u64>() + a.len() as u64 * d0 as u64;
    for &x in &a[..a.len().min(10 - r.k as usize)] {
        r.ids[r.k as usize] = x + d0;
        r.k += 1;
    }
}

// Adds a round's result, ids relative to d0 (a bitmap).
fn acc_bits(r: &mut Acc, b: &[u64], d0: u32) {
    let (mut n, mut s) = (0u64, 0u64);
    for (i, &w) in b.iter().enumerate() {
        let c = w.count_ones() as u64;
        n += c;
        s += c * (i as u64 * 64)
            + (0..6)
                .map(|k| ((w & !(!0 / ((1u64 << (1 << k)) + 1))).count_ones() as u64) << k)
                .sum::<u64>();
    }
    r.n += n as u32;
    r.s += s + n * d0 as u64;
    for (i, &w) in b.iter().enumerate() {
        if r.k == 10 {
            break;
        }
        let mut w = w;
        while w != 0 && r.k < 10 {
            r.ids[r.k as usize] = d0 + (i * 64) as u32 + w.trailing_zeros();
            r.k += 1;
            w &= w - 1;
        }
    }
}

fn emit_acc(o: &mut Vec<u8>, r: &Acc) {
    put(o, r.n as u64);
    o.push(b' ');
    put(o, r.s);
    for &x in &r.ids[..r.k as usize] {
        o.push(b' ');
        put(o, x as u64);
    }
    o.push(b'\n');
}

enum Q {
    Bool(usize), // its terms: ops[s..] up to the next op 0
    Phrase,
}

fn main() {
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
    let qb = load(&format!("{data}/queries.txt"));
    let buf = load(&format!("{data}/docs.txt"));

    // ---- parse queries, intern terms
    let nth = std::env::var("NT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, |n| n.get()));
    let (qt, st) = (nth.min(4), nth.min(4)); // small jobs: more threads cost more (spawns, page faults) than they save
    let mut tab = Tab::new((qb.len() / 12).next_power_of_two().trailing_zeros().max(4));
    // In parallel: every query term as (B first BOOL term / A, O, N operator / P first phrase term / p, key, hash).
    let toks = par(qt, |i| {
        let (a, b) = (
            line_start(qb, qb.len() * i / qt),
            line_start(qb, qb.len() * (i + 1) / qt),
        );
        let mut v = Vec::with_capacity((b - a) / 5);
        let mut add = |op: u8, w: &[u8]| {
            v.push((
                hk(&key_of(w)),
                (w.as_ptr() as usize - qb.as_ptr() as usize) as u32,
                w.len() as u8,
                op,
            ))
        };
        for line in qb[a..b].split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
            if line.starts_with(b"BOOL ") {
                let mut op = b'B';
                for (i, wd) in line[5..].split(|&c| c == b' ').enumerate() {
                    if i % 2 == 1 {
                        op = wd[0];
                    } else {
                        add(op, wd);
                    }
                }
            } else {
                line[8..line.len() - 1]
                    .split(|&c| c == b' ')
                    .enumerate()
                    .for_each(|(i, w)| add(if i == 0 { b'P' } else { b'p' }, w));
            }
        }
        v
    });
    let mut qs = Vec::new();
    let mut ops: Vec<(u8, u32)> = Vec::new();
    let mut is_bool_term: Vec<bool> = Vec::new();
    let (mut p_terms, mut p_off, mut pq) = (Vec::new(), Vec::new(), Vec::new()); // phrase terms, starts, query
    for v in &toks {
        for (j, &(h, o, l, op)) in v.iter().enumerate() {
            if let Some(x) = v.get(j + 16) {
                tab.prefetch32((x.0 >> 32) as u32);
            }
            let t = tab.intern_h(&key_of(&qb[o as usize..o as usize + l as usize]), h);
            match op {
                b'B' => {
                    qs.push(Q::Bool(ops.len()));
                    ops.push((0, t));
                }
                b'P' => {
                    pq.push(qs.len() as u32);
                    qs.push(Q::Phrase);
                    p_off.push(p_terms.len() as u32);
                    p_terms.push(t);
                }
                b'p' => p_terms.push(t),
                _ => ops.push((op, t)),
            }
        }
    }
    p_off.push(p_terms.len() as u32);
    drop(toks);
    let nt = tab.keys.len();
    is_bool_term.resize(nt, false);
    for &(_, t) in &ops {
        is_bool_term[t as usize] = true;
    }

    // ---- document count from the last line
    let n_docs = if buf.is_empty() {
        0
    } else {
        let e = buf.len() - 1;
        let s = buf[..e]
            .iter()
            .rposition(|&c| c == b'\n')
            .map_or(0, |p| p + 1);
        let t = s + buf[s..].iter().position(|&c| c == b'\t').unwrap();
        parse_u32(&buf[s..t]) as usize + 1
    };
    let w = (n_docs + 63) / 64;

    // ---- sample 1 MB spread over the corpus (in parallel) to estimate document frequencies
    let (mut sdf, mut sdocs) = (vec![0u32; nt], 0u32);
    for (d, n) in par(st, |i| {
        let a = line_start(buf, buf.len() * i / st);
        let b = line_start(buf, (a + (1 << 20) / st).min(buf.len()));
        let (mut df, mut last, mut docs) = (vec![0u32; nt], vec![MISS; nt], 0u32);
        let (mut p, mut pos, mut ks) = (a, Vec::new(), Vec::new());
        let mut count = |t: u32, d: u32| {
            if t != MISS && last[t as usize] != d {
                last[t as usize] = d;
                df[t as usize] += 1;
            }
        };
        // per doc: hash and prefetch every token, then look them up
        while p < b {
            let l = line_tokens(buf, p, &mut pos);
            ks.clear();
            for j in 1..pos.len() / 2 {
                let (s, len) = (
                    p + pos[2 * j] as usize,
                    (pos[2 * j + 1] - pos[2 * j]) as usize,
                );
                if len <= 12 {
                    let (k0, k1) = load12(buf, s, len);
                    let h = h12(k0, k1);
                    tab.prefetch32(h);
                    ks.push((k0, k1, h));
                } else if len <= 32 {
                    count(tab.get(&load_key(buf, s, len)), docs);
                }
            }
            ks.iter()
                .for_each(|&(k0, k1, h)| count(tab.get12(k0, k1, h), docs));
            docs += 1;
            p += l;
        }
        (df, docs)
    }) {
        sdf.iter_mut().zip(&d).for_each(|(x, y)| *x += y);
        sdocs += n;
    }
    let sdocs = sdocs.max(1);
    // BOOL terms in more than 1/40 of the sample get a bitmap, the other ones a sorted list.
    // Renumber: bitmap terms first (bitmap index = id), then by sampled frequency so hot data shares cache lines.
    let isbm = |t: usize| is_bool_term[t] && sdf[t] as u64 * 40 >= sdocs as u64;
    let mut pk: Vec<u64> = (0..nt)
        .map(|t| {
            ((!isbm(t) as u64) << 63
                | (!is_bool_term[t] as u64) << 62
                | ((u32::MAX - sdf[t]) as u64) << 24)
                | t as u64
        })
        .collect();
    pk.sort_unstable();
    let perm: Vec<u32> = pk.iter().map(|&x| (x & 0xFF_FFFF) as u32).collect();
    let nbm = (0..nt).filter(|&t| isbm(t)).count();
    let mut nid = vec![0u32; nt];
    perm.iter()
        .enumerate()
        .for_each(|(n, &o)| nid[o as usize] = n as u32);
    tab.keys = perm.iter().map(|&o| tab.keys[o as usize]).collect();
    let lens: Vec<u32> = tab
        .keys
        .iter()
        .map(|k| k.iter().map(|w| 8 - w.leading_zeros() / 8).sum())
        .collect();
    let is_bool_term: Vec<bool> = perm.iter().map(|&o| is_bool_term[o as usize]).collect();
    let sdf: Vec<u32> = perm.iter().map(|&o| sdf[o as usize]).collect();
    ops.iter_mut().for_each(|o| o.1 = nid[o.1 as usize]);
    p_terms.iter_mut().for_each(|t| *t = nid[*t as usize]);
    let np = p_off.len() - 1;
    let plist = |i: usize| &p_terms[p_off[i] as usize..p_off[i + 1] as usize];

    let mut flags: Vec<u8> = (0..nt)
        .map(|t| if t < nbm { 2 } else { is_bool_term[t] as u8 })
        .collect();
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
    tab.renumber(&nid, &flags);
    tab.drop_keys();
    // Rounds of 65536 docs: index a round, answer every BOOL query on it, add up the results. Only
    // one round's index is alive at a time; within a round doc ids are their low 16 bits.
    let rd = 1 << 16; // docs per round
    let wr = w.min(rd / 64);
    // row stride padded by 64 bytes: power-of-two strides put every row's current word in one L1 set
    let stride = wr + 8;
    let mut bmv = hvec((nbm + 1) * stride, 0u64);
    let nbool = is_bool_term.iter().filter(|&&b| b).count();
    let mut ctx = Ctx {
        dummy: nt as u32,
        lo: nbm,
        ew: if nbool - nbm < 65535 { 2 } else { 3 },
        nl: nbool - nbm,
        buf,
        tab,
        bm: Ptr(bmv.as_mut_ptr()),
        d0: 0,
        w: stride,
        kl_off,
        kl,
        p_off,
        p_terms,
    };
    let id_at = |p: usize| {
        if p >= buf.len() {
            u32::MAX
        } else {
            parse_u32(&buf[p..p + buf[p..].iter().position(|&c| c == b'\t').unwrap()])
        }
    };
    // line start of doc d (binary search: ids ascend with the offset)
    let at_doc = |d: usize| {
        let (mut lo, mut hi) = (0, buf.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            let p = line_start(buf, mid);
            if p < buf.len() && id_at(p) < d as u32 {
                lo = mid + 1
            } else {
                hi = mid
            }
        }
        line_start(buf, lo)
    };
    let nr = (n_docs + rd - 1) / rd;
    let mut accs = vec![Acc::default(); qs.len()];
    for r in 0..nr {
        let d0 = (r * rd) as u32;
        if r > 0 && r + 1 == nr {
            bmv.fill(0); // every block of a full round is written whole (ids are dense); the last may be short
        }
        ctx.d0 = d0;
        let ctx = &ctx;
        // ---- parallel index pass over the round; chunks start at doc ids that are multiples of 64,
        // outputs of at most 32768 docs
        let outs: Vec<WOut> = par(nth, |i| {
            // equal bytes per thread, cut at docs that are multiples of 64
            let (rs, re) = (at_doc(d0 as usize), at_doc(d0 as usize + rd));
            let cut = |i: usize| match i {
                0 => d0 as usize,
                _ if i == nth => d0 as usize + rd,
                _ => {
                    id_at(line_start(buf, rs + (re - rs) * i / nth)).min(n_docs as u32) as usize
                        & !63
                }
            };
            let (a, b) = (cut(i), cut(i + 1));
            let mut ds: Vec<usize> = (a..b).step_by(1 << 15).collect();
            ds.push(b);
            if a < b.min(n_docs) {
                worker(ctx, &ds.into_iter().map(|d| at_doc(d)).collect::<Vec<_>>())
            } else {
                vec![]
            }
        })
        .into_iter()
        .flatten()
        .collect();
        for o in &outs {
            for &(p, d) in &o.pm {
                add(&mut accs[pq[p as usize] as usize], d);
            }
        }
        // merge the outs into one CSR for the round (in parallel by term ranges)
        let nl = nbool - nbm;
        let mut goff = vec![0u32; nl + 1];
        let n: usize = outs.iter().map(|o| o.post.len()).sum();
        let mut post: Vec<u16> = Vec::with_capacity(n);
        unsafe { post.set_len(n) }; // filled below
        goff[nl] = n as u32;
        let bmw: &[u64] = &bmv;
        let (pp, gp, bar) = (
            Ptr(post.as_mut_ptr()),
            Ptr(goff.as_mut_ptr()),
            std::sync::Barrier::new(nth),
        );
        let sums: Vec<AtomicUsize> = (0..nth).map(|_| AtomicUsize::new(0)).collect();
        let next = AtomicUsize::new(0);
        const BLK: usize = 128;
        let nblk = (qs.len() + BLK - 1) / BLK;
        let ap = Ptr(accs.as_mut_ptr());
        par(nth, |i| {
            let (t0, t1) = (nl * i / nth, nl * (i + 1) / nth);
            sums[i].store(
                outs.iter()
                    .map(|o| (o.offs[t1] - o.offs[t0]) as usize)
                    .sum(),
                Relaxed,
            );
            bar.wait();
            let s0 = sums[..i].iter().map(|x| x.load(Relaxed)).sum::<usize>();
            let mut d = pp.at(s0);
            for t in t0..t1 {
                unsafe { *gp.at(t) = d.offset_from(pp.at(0)) as u32 };
                for o in &outs {
                    let x = &o.post[o.offs[t] as usize..o.offs[t + 1] as usize];
                    unsafe {
                        std::ptr::copy_nonoverlapping(x.as_ptr(), d, x.len());
                        d = d.add(x.len());
                    }
                }
            }
            // ---- then answer the BOOL queries on this round
            bar.wait();
            let (post, goff): (&[u16], &[u32]) = unsafe {
                (
                    std::slice::from_raw_parts(pp.at(0), n),
                    std::slice::from_raw_parts(gp.at(0), nl + 1),
                )
            };
            let src = |t: u32| -> Set {
                let t = t as usize;
                if t < nbm {
                    let i = t * stride;
                    Set::R(&bmw[i..i + wr])
                } else {
                    let t = t - nbm;
                    Set::L(
                        post[goff[t] as usize..goff[t + 1] as usize]
                            .iter()
                            .map(|&x| x.wrapping_sub(d0 as u16) as u32)
                            .collect(),
                    )
                }
            };
            loop {
                let b = next.fetch_add(1, Relaxed);
                if b >= nblk {
                    break;
                }
                for qi in b * BLK..((b + 1) * BLK).min(qs.len()) {
                    if let Q::Bool(s) = qs[qi] {
                        let e = s + 1 + ops[s + 1..].iter().take_while(|o| o.0 != 0).count();
                        let acc = unsafe { &mut *ap.at(qi) };
                        if ops[s..e].iter().all(|o| (o.1 as usize) < nbm) {
                            // only bitmaps: fold 128 words at a time on the stack, no allocation
                            const CH: usize = 128;
                            let mut x = [0u64; CH];
                            for c0 in (0..wr).step_by(CH) {
                                let n = CH.min(wr - c0);
                                let row = |t: u32| {
                                    &bmw[t as usize * stride + c0..t as usize * stride + c0 + n]
                                };
                                x[..n].copy_from_slice(row(ops[s].1));
                                for &(op, t) in &ops[s + 1..e] {
                                    let y = row(t);
                                    match op {
                                        b'A' => x[..n].iter_mut().zip(y).for_each(|(a, b)| *a &= b),
                                        b'O' => x[..n].iter_mut().zip(y).for_each(|(a, b)| *a |= b),
                                        _ => x[..n].iter_mut().zip(y).for_each(|(a, b)| *a &= !b),
                                    }
                                }
                                acc_bits(acc, &x[..n], d0 + (c0 * 64) as u32);
                            }
                            continue;
                        }
                        // the leading run of ANDs commutes: start from its shortest list
                        let k = 1 + ops[s + 1..e].iter().take_while(|o| o.0 == b'A').count();
                        let len = |t: u32| {
                            if (t as usize) < nbm {
                                u32::MAX
                            } else {
                                goff[t as usize - nbm + 1] - goff[t as usize - nbm]
                            }
                        };
                        let f = (s..s + k).min_by_key(|&i| len(ops[i].1)).unwrap();
                        let mut r = src(ops[f].1);
                        for (op, t) in (s..e)
                            .filter(|&i| i != f)
                            .map(|i| (if i < s + k { b'A' } else { ops[i].0 }, ops[i].1))
                        {
                            if matches!(&r, Set::L(a) if a.is_empty()) {
                                // AND / NOT of nothing stays empty; nothing OR S is S
                                if op == b'O' {
                                    r = src(t);
                                }
                                continue;
                            }
                            if let (Set::L(a), true) = (&mut r, op != b'O' && t as usize >= nbm) {
                                // filter in place against the borrowed list (no copy)
                                let t = t as usize - nbm;
                                let l = &post[goff[t] as usize..goff[t + 1] as usize];
                                let keep = op == b'A';
                                let (mut j, b0) = (0, d0 as u16);
                                let at = |j: usize| l[j].wrapping_sub(b0) as u32;
                                let gal = a.len() * 16 < l.len();
                                a.retain(|&x| {
                                    if gal {
                                        let mut step = 1;
                                        while j + step < l.len() && at(j + step) < x {
                                            j += step;
                                            step <<= 1;
                                        }
                                        let h = (j + step).min(l.len());
                                        j += l[j..h]
                                            .partition_point(|&y| (y.wrapping_sub(b0) as u32) < x);
                                    }
                                    while j < l.len() && at(j) < x {
                                        j += 1;
                                    }
                                    (j < l.len() && at(j) == x) == keep
                                });
                                continue;
                            }
                            r = apply(r, op, src(t), wr);
                        }
                        match r {
                            Set::L(a) => acc_list(acc, &a, d0),
                            Set::B(a) => acc_bits(acc, &a, d0),
                            Set::R(a) => acc_bits(acc, a, d0),
                        }
                    }
                }
            }
        });
        drop(outs);
    }
    drop(bmv);
    let per = (qs.len() + nth - 1) / nth.max(1);
    let o = par(nth, |i| {
        let mut o = Vec::with_capacity(per * 40);
        for q in i * per..((i + 1) * per).min(qs.len()) {
            emit_acc(&mut o, &accs[q]);
        }
        o
    })
    .concat();
    let _ = std::fs::create_dir_all(&out);
    std::fs::write(format!("{out}/results.txt"), &o).unwrap();
}
