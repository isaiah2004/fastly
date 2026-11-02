use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::{AcqRel, Acquire, Relaxed, Release}};

const INF: u64 = u64::MAX;
const NONE: u32 = u32::MAX;

#[cfg(target_os = "linux")]
fn load(path: &str) -> &'static [u8] {
    extern "C" {
        fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    }
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::open(path).unwrap();
    let len = f.metadata().unwrap().len() as usize;
    if len == 0 {
        return &[];
    }
    // PROT_READ, MAP_PRIVATE | MAP_POPULATE
    let p = unsafe { mmap(std::ptr::null_mut(), len, 1, 2 | 0x8000, f.as_raw_fd(), 0) };
    unsafe { std::slice::from_raw_parts(p, len) }
}
#[cfg(not(target_os = "linux"))]
fn load(path: &str) -> &'static [u8] {
    Box::leak(std::fs::read(path).unwrap().into_boxed_slice())
}

/// Big allocations come straight from mmap, 2 MiB aligned with MADV_HUGEPAGE
/// (fewer page faults and TLB misses) and go straight back on free.
#[cfg(target_os = "linux")]
mod huge {
    use std::alloc::{GlobalAlloc, Layout, System};
    extern "C" {
        fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
        fn munmap(a: *mut u8, l: usize) -> i32;
        fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    }
    pub struct A;
    const H: usize = 1 << 21;
    const BIG: usize = 1 << 22;
    unsafe impl GlobalAlloc for A {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            if l.size() < BIG {
                return System.alloc(l);
            }
            let len = (l.size() + 4095) & !4095;
            let raw = mmap(std::ptr::null_mut(), len + H, 3, 0x22, -1, 0);
            if raw as isize == -1 {
                return std::ptr::null_mut();
            }
            let a = ((raw as usize + H - 1) & !(H - 1)) as *mut u8;
            if a > raw {
                munmap(raw, a as usize - raw as usize);
            }
            munmap(a.add(len), raw as usize + H - a as usize);
            madvise(a, len, 14);
            a
        }
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            if l.size() < BIG { System.alloc_zeroed(l) } else { self.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            if l.size() < BIG {
                System.dealloc(p, l)
            } else {
                munmap(p, (l.size() + 4095) & !4095);
            }
        }
    }
}
#[cfg(target_os = "linux")]
#[global_allocator]
static GA: huge::A = huge::A;

struct P<T>(*mut T);
impl<T> Clone for P<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for P<T> {}
unsafe impl<T> Send for P<T> {}
unsafe impl<T> Sync for P<T> {}

fn atomics(v: &mut [u32]) -> &[AtomicU32] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const AtomicU32, v.len()) }
}

fn uninit<T>(m: usize) -> Vec<T> {
    let mut v = Vec::with_capacity(m);
    unsafe { v.set_len(m) };
    v
}

#[inline(always)]
fn num(b: &[u8], i: &mut usize) -> u64 {
    let mut x = 0u64;
    loop {
        let c = unsafe { *b.get_unchecked(*i) };
        *i += 1;
        if c < b'0' {
            return x;
        }
        x = x * 10 + (c - b'0') as u64;
    }
}

/// The decimal number in b[e - l..e] (l <= 16, e >= 8): SWAR, 8 digits at a time.
#[inline(always)]
fn dig(b: &[u8], e: usize, l: usize) -> u64 {
    let d8 = |e: usize, l: usize| {
        let raw = unsafe { (b.as_ptr().add(e - 8) as *const u64).read_unaligned() };
        let m = !0u64 << ((8 - l) * 8); // keep the last l bytes (the digits)
        let s = (raw & m) - (0x3030_3030_3030_3030 & m);
        let s = ((s & 0x0f00_0f00_0f00_0f00) >> 8) + (s & 0x000f_000f_000f_000f) * 10;
        let s = ((s & 0x00ff_0000_00ff_0000) >> 16) + (s & 0x0000_00ff_0000_00ff) * 100;
        ((s & 0x0000_ffff_0000_0000) >> 32) + (s & 0x0000_0000_0000_ffff) * 10000
    };
    if l <= 8 {
        d8(e, l)
    } else {
        d8(e - 8, l - 8) * 100_000_000 + d8(e, 8)
    }
}

/// Parse the edge line at b[i..]: (u, v, w, start of the next line).
#[inline(always)]
fn edge(b: &[u8], i: usize) -> (u32, u32, u32, usize) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    if i >= 8 && i + 32 <= b.len() {
        use std::arch::x86_64::*;
        // one 32-byte compare finds the two spaces and the newline (bytes < '0')
        let mut m = unsafe {
            let x = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
            _mm256_movemask_epi8(_mm256_cmpgt_epi8(_mm256_set1_epi8(b'0' as i8), x)) as u32
        };
        if m.count_ones() < 3 {
            return edge_scalar(b, i); // a line longer than 32 bytes
        }
        let p1 = m.trailing_zeros() as usize;
        m &= m - 1;
        let p2 = m.trailing_zeros() as usize;
        m &= m - 1;
        let p3 = m.trailing_zeros() as usize;
        return (dig(b, i + p1, p1) as u32, dig(b, i + p2, p2 - p1 - 1) as u32, dig(b, i + p3, p3 - p2 - 1) as u32, i + p3 + 1);
    }
    edge_scalar(b, i)
}

fn edge_scalar(b: &[u8], mut i: usize) -> (u32, u32, u32, usize) {
    let u = num(b, &mut i) as u32;
    let v = num(b, &mut i) as u32;
    let w = num(b, &mut i) as u32;
    (u, v, w, i)
}

/// Split `b[start..]` into `k` line-aligned chunks.
fn chunks(b: &[u8], start: usize, k: usize) -> Vec<(usize, usize)> {
    let len = b.len() - start;
    let mut cuts = vec![start];
    for j in 1..k {
        let last = *cuts.last().unwrap();
        let mut p = (start + len * j / k).max(last);
        while p > last && p < b.len() && b[p - 1] != b'\n' {
            p += 1;
        }
        cuts.push(p);
    }
    cuts.push(b.len());
    cuts.windows(2).map(|w| (w[0], w[1])).collect()
}

/// The first graph (old ids), 4 bytes per edge: head << gw | weight, where the
/// weight field gm (all ones) means "look the weight up in `big`" (sorted by edge index).
struct Csr {
    off: Vec<u32>,
    e: Vec<u32>,
    big: Vec<(u32, u32)>,
    gw: u32,
    gm: u32,
}

impl Csr {
    #[inline(always)]
    fn hd(&self, x: u32) -> u32 {
        x >> self.gw
    }
    #[inline(always)]
    fn wt(&self, k: usize) -> u64 {
        let w = unsafe { *self.e.get_unchecked(k) } & self.gm;
        if w != self.gm { w as u64 } else { self.big[self.big.binary_search_by_key(&(k as u32), |x| x.0).unwrap()].1 as u64 }
    }
    fn row(&self, u: usize) -> std::ops::Range<usize> {
        self.off[u] as usize..self.off[u + 1] as usize
    }
}

/// Final graph rows, 4 bytes per edge: (head - tail + 2^15) << 15 | weight when both
/// fit (bit 31 clear), else 2^31 | index into `ext` = (head, weight). After the
/// locality renumbering nearly every edge fits.
struct Cg {
    off: Vec<u32>,
    e: Vec<u32>,
    ext: Vec<(u32, u32)>,
}

const ESC: u32 = 1 << 31;

/// The packed form of an edge u -> v of weight w, if it fits.
#[inline(always)]
fn enc(u: u32, v: u32, w: u32) -> Option<u32> {
    let d = (v as i64 - u as i64 + 32768) as u64;
    (d < 65536 && w < 32768).then(|| (d as u32) << 15 | w)
}

impl Cg {
    #[inline(always)]
    fn edge(&self, u: u32, k: usize) -> (u32, u64) {
        let x = unsafe { *self.e.get_unchecked(k) };
        if x & ESC == 0 {
            ((u + (x >> 15)).wrapping_sub(32768), (x & 0x7FFF) as u64)
        } else {
            let (v, w) = unsafe { *self.ext.get_unchecked((x & !ESC) as usize) };
            (v, w as u64)
        }
    }
    fn row(&self, u: usize) -> std::ops::Range<usize> {
        self.off[u] as usize..self.off[u + 1] as usize
    }
}

/// Persistent worker pool: threads 1..nt park until the generation counter moves,
/// run the current job, and the last one to finish wakes the caller, which runs
/// the job as thread 0. Cheaper than spawning threads for every phase.
struct Pool {
    gen: AtomicUsize,
    done: AtomicUsize,
    job: std::cell::UnsafeCell<Option<&'static (dyn Fn(usize) + Sync)>>,
    th: std::sync::OnceLock<(std::thread::Thread, Vec<std::thread::Thread>)>,
}
unsafe impl Sync for Pool {}
static POOL: Pool = Pool { gen: AtomicUsize::new(0), done: AtomicUsize::new(0), job: std::cell::UnsafeCell::new(None), th: std::sync::OnceLock::new() };

fn start_pool(nt: usize) {
    let ws = (1..nt)
        .map(|t| {
            // 1 MiB stacks cannot hold a 2 MiB transparent huge page
            let h = std::thread::Builder::new().stack_size(1 << 20).spawn(move || {
                let mut g = 0;
                loop {
                    while POOL.gen.load(Acquire) == g {
                        std::thread::park();
                    }
                    g += 1;
                    (unsafe { *POOL.job.get() }.unwrap())(t);
                    if POOL.done.fetch_add(1, AcqRel) + 2 == nt {
                        POOL.th.get().unwrap().0.unpark();
                    }
                }
            });
            h.unwrap().thread().clone()
        })
        .collect();
    POOL.th.set((std::thread::current(), ws)).unwrap();
}

/// Run f(t) for t in 0..nt on the pool and collect the results.
fn par<R: Send>(nt: usize, f: impl Fn(usize) -> R + Sync) -> Vec<R> {
    let mut out: Vec<Option<R>> = (0..nt).map(|_| None).collect();
    let op = P(out.as_mut_ptr());
    let job = move |t: usize| {
        let op = op;
        unsafe { *op.0.add(t) = Some(f(t)) };
    };
    let jr: &(dyn Fn(usize) + Sync) = &job;
    // the job outlives its use: par returns only after every worker is done
    unsafe { *POOL.job.get() = Some(std::mem::transmute::<&(dyn Fn(usize) + Sync), &'static (dyn Fn(usize) + Sync)>(jr)) };
    POOL.done.store(0, Relaxed);
    POOL.gen.fetch_add(1, Release);
    for w in &POOL.th.get().unwrap().1 {
        w.unpark();
    }
    jr(0);
    while POOL.done.load(Acquire) + 1 < nt {
        std::thread::park();
    }
    out.into_iter().map(|x| x.unwrap()).collect()
}

/// Counts -> exclusive prefix sums, in place (c has n + 1 slots, the last one 0).
fn prefix(c: &mut [u32], nt: usize) {
    let n = c.len();
    let cp = P(c.as_mut_ptr());
    let rg = |t: usize| (n * t / nt, n * (t + 1) / nt);
    let sums = par(nt, |t| {
        let cp = cp;
        let (lo, hi) = rg(t);
        let mut s = 0u32;
        for i in lo..hi {
            s += unsafe { *cp.0.add(i) };
        }
        s
    });
    par(nt, |t| {
        let cp = cp;
        let (lo, hi) = rg(t);
        let mut s: u32 = sums[..t].iter().sum();
        for i in lo..hi {
            unsafe {
                let x = *cp.0.add(i);
                *cp.0.add(i) = s;
                s += x;
            }
        }
    });
}

/// Per-thread edge buckets: each bucket is a chain of BLK-entry blocks cut from a
/// few big slabs, so thousands of buckets cost a handful of allocations.
const BLK: usize = 512;
/// A 48-bit bucket entry.
#[derive(Clone, Copy)]
#[repr(C, packed)]
struct B6 {
    lo: u32,
    hi: u16,
}
struct Buckets {
    slabs: Vec<Vec<B6>>,
    used: usize,
    full: Vec<Vec<*const B6>>,
    cur: Vec<(*mut B6, usize)>,
}
unsafe impl Send for Buckets {}
unsafe impl Sync for Buckets {}
impl Buckets {
    fn new(k: usize, expect: usize) -> Buckets {
        let mut b = Buckets { slabs: vec![Vec::with_capacity(expect + (k + 1) * BLK)], used: 0, full: vec![Vec::new(); k], cur: Vec::with_capacity(k) };
        for _ in 0..k {
            let p = b.block();
            b.cur.push((p, 0));
        }
        b
    }
    fn block(&mut self) -> *mut B6 {
        if self.used + BLK > self.slabs.last().unwrap().capacity() {
            self.slabs.push(Vec::with_capacity(64 * BLK));
            self.used = 0;
        }
        self.used += BLK;
        unsafe { self.slabs.last_mut().unwrap().as_mut_ptr().add(self.used - BLK) }
    }
    #[inline(always)]
    fn push(&mut self, j: usize, x: u64) {
        let c = unsafe { self.cur.get_unchecked_mut(j) };
        if c.1 == BLK {
            let p = c.0;
            self.full[j].push(p);
            let q = self.block();
            self.cur[j] = (q, 0);
        }
        let c = unsafe { self.cur.get_unchecked_mut(j) };
        unsafe { *c.0.add(c.1) = B6 { lo: x as u32, hi: (x >> 32) as u16 } };
        c.1 += 1;
    }
    fn each(&self, j: usize, mut f: impl FnMut(u64)) {
        for &p in &self.full[j] {
            for &x in unsafe { std::slice::from_raw_parts(p, BLK) } {
                f(x.lo as u64 | (x.hi as u64) << 32);
            }
        }
        let (p, l) = self.cur[j];
        for &x in unsafe { std::slice::from_raw_parts(p, l) } {
            f(x.lo as u64 | (x.hi as u64) << 32);
        }
    }
}

/// Rows of one thread, in the new order, packed like `Cg` relative to the thread's
/// own numbering; `ext` keys are a tag, or ESC | old id for a head that had no tag yet.
struct Rows {
    len: usize,
    off: Vec<u32>,
    e: Vec<u32>,
    ext: Vec<(u32, u32)>,
}

/// Locality order: parallel region growing (BFS from seeds, regions capped).
/// Each thread copies the rows of the nodes it placed while they are hot.
/// `tag` gets (thread << 26) | (index in that thread's list) for every node.
fn order(g: &Csr, n: usize, nt: usize, tag: &[AtomicU32]) -> Vec<Rows> {
    const CAP: usize = 16384;
    const D1: usize = 16;
    const D2: usize = 8;
    let claim = |v: usize, x: usize| tag[v].load(Relaxed) == NONE && tag[v].compare_exchange(NONE, x as u32, Relaxed, Relaxed).is_ok();
    let rg = |u: usize| g.row(u);
    // append the row (still hot in cache) of u, the thread's node number iu
    let copy = |r: &mut Rows, t: usize, iu: usize, u: u32, row: &[u32]| {
        r.off.push(r.e.len() as u32);
        for (j, &x) in row.iter().enumerate() {
            let w = if x & g.gm == g.gm { g.wt(g.off[u as usize] as usize + j) as u32 } else { x & g.gm };
            let hv = g.hd(x);
            let tv = tag[hv as usize].load(Relaxed);
            if tv >> 26 == t as u32 {
                if let Some(c) = enc(iu as u32, tv & ((1 << 26) - 1), w) {
                    r.e.push(c);
                    continue;
                }
            }
            r.e.push(ESC | r.ext.len() as u32);
            r.ext.push((if tv == NONE { ESC | hv } else { tv }, w));
        }
    };
    par(nt, |t| {
        // the BFS queue of the current region; earlier regions' nodes number 0..done
        let mut list: Vec<u32> = Vec::with_capacity(2 * CAP);
        let mut done = 0;
        let c = g.e.len() / nt * 5 / 4;
        let mut r = Rows { len: 0, off: Vec::with_capacity(n / nt * 5 / 4), e: Vec::with_capacity(c), ext: Vec::new() };
        for s in (n * t / nt)..(n * (t + 1) / nt) {
            if !claim(s, t << 26 | done) {
                continue;
            }
            let start = 0;
            list.push(s as u32);
            let mut q = start;
            while q < list.len() && list.len() - start < CAP {
                // two-stage prefetch: rows of the node D1 ahead, tags of the heads of the node D2 ahead
                if let Some(&a) = list.get(q + D1) {
                    let k = g.off[a as usize] as usize;
                    pf(unsafe { g.e.as_ptr().add(k) });
                }
                if let Some(&a) = list.get(q + D2) {
                    for &x in &g.e[rg(a as usize)] {
                        let v = g.hd(x);
                        pf(&tag[v as usize]);
                        pf(&g.off[v as usize]);
                    }
                }
                let row = &g.e[rg(list[q] as usize)];
                for &x in row {
                    let v = g.hd(x);
                    if claim(v as usize, t << 26 | (done + list.len())) {
                        list.push(v);
                    }
                }
                copy(&mut r, t, done + q, list[q], row);
                q += 1;
            }
            // the rows of the nodes left in the queue when the region filled up
            for i in q..list.len() {
                copy(&mut r, t, done + i, list[i], &g.e[rg(list[i] as usize)]);
            }
            done += list.len();
            list.clear();
        }
        r.len = done;
        r
    })
}

const PB: u32 = 6;
const PM: u32 = (1 << PB) - 1;

/// Per-thread search state. Distance labels of both directions sit side by side
/// in pages of 2^PB ids, allocated on first touch (memory follows the search,
/// not n). One 4-ary min-heap of (dist << nb) | node per direction.
struct St<L> {
    dir: Vec<u32>,
    pool: Vec<[L; 2]>,
    pages: Vec<u32>,
    heap: [Vec<u64>; 2],
}

/// Label storage type: u32 (compact, tried first) or u64.
trait Lab: Copy + PartialEq {
    const MAX: Self;
    const LIM: u64; // stored distances must be below this
    fn w(d: u64) -> Self;
    fn r(self) -> u64;
}
impl Lab for u32 {
    const MAX: u32 = u32::MAX;
    const LIM: u64 = u32::MAX as u64;
    fn w(d: u64) -> u32 {
        d as u32
    }
    #[inline(always)]
    fn r(self) -> u64 {
        if self == u32::MAX { INF } else { self as u64 }
    }
}
impl Lab for u64 {
    const MAX: u64 = INF;
    const LIM: u64 = INF;
    fn w(d: u64) -> u64 {
        d
    }
    fn r(self) -> u64 {
        self
    }
}

impl<L: Lab> St<L> {
    fn new(n: usize) -> St<L> {
        St { dir: vec![NONE; (n >> PB) + 1], pool: Vec::new(), pages: Vec::new(), heap: [Vec::new(), Vec::new()] }
    }
    #[inline(always)]
    fn get(&self, v: u32, s: usize) -> u64 {
        let p = unsafe { *self.dir.get_unchecked((v >> PB) as usize) };
        if p == NONE {
            INF
        } else {
            unsafe { self.pool.get_unchecked(((p << PB) | (v & PM)) as usize)[s].r() }
        }
    }
    #[inline(always)]
    fn pf(&self, v: u32) {
        let p = unsafe { *self.dir.get_unchecked((v >> PB) as usize) };
        if p != NONE {
            pf(unsafe { self.pool.as_ptr().add(((p << PB) | (v & PM)) as usize) });
        }
    }
    #[cold]
    fn alloc(&mut self, v: u32) -> u32 {
        let p = self.pages.len() as u32;
        self.pages.push(v >> PB);
        let lo = (p as usize) << PB;
        if self.pool.len() < lo + (1 << PB) {
            self.pool.resize(lo + (1 << PB), [L::MAX; 2]);
        } else {
            self.pool[lo..lo + (1 << PB)].fill([L::MAX; 2]);
        }
        self.dir[(v >> PB) as usize] = p;
        p
    }
    /// Lower v's label on side s to d. If it improved, return the other side's label of v.
    #[inline(always)]
    fn relax(&mut self, s: usize, v: u32, d: u64) -> Option<u64> {
        let mut p = unsafe { *self.dir.get_unchecked((v >> PB) as usize) };
        if p == NONE {
            p = self.alloc(v);
        }
        let l = unsafe { self.pool.get_unchecked_mut(((p << PB) | (v & PM)) as usize) };
        if d >= l[s].r() {
            return None;
        }
        l[s] = L::w(d);
        Some(l[s ^ 1].r())
    }
    fn reset(&mut self) {
        for &q in &self.pages {
            self.dir[q as usize] = NONE;
        }
        self.pages.clear();
        self.heap[0].clear();
        self.heap[1].clear();
    }
}

#[inline(always)]
fn push(h: &mut Vec<u64>, x: u64) {
    h.push(x);
    let mut i = h.len() - 1;
    while i > 0 {
        let p = (i - 1) / 4;
        let hp = unsafe { *h.get_unchecked(p) };
        if hp <= x {
            break;
        }
        unsafe { *h.get_unchecked_mut(i) = hp };
        i = p;
    }
    unsafe { *h.get_unchecked_mut(i) = x };
}

#[inline(always)]
fn pop(h: &mut Vec<u64>) -> u64 {
    let top = h[0];
    let last = h.pop().unwrap();
    let len = h.len();
    if len == 0 {
        return top;
    }
    let mut i = 0;
    loop {
        let c = 4 * i + 1;
        if c >= len {
            break;
        }
        let (mut mc, mut mv);
        if c + 4 <= len {
            // branchless minimum of the four children
            let g = |j: usize| unsafe { *h.get_unchecked(j) };
            let (a, b, x, y) = (g(c), g(c + 1), g(c + 2), g(c + 3));
            let (m1, i1) = if b < a { (b, c + 1) } else { (a, c) };
            let (m2, i2) = if y < x { (y, c + 3) } else { (x, c + 2) };
            (mv, mc) = if m2 < m1 { (m2, i2) } else { (m1, i1) };
        } else {
            mc = c;
            mv = unsafe { *h.get_unchecked(c) };
            for j in c + 1..len {
                let x = unsafe { *h.get_unchecked(j) };
                if x < mv {
                    mv = x;
                    mc = j;
                }
            }
        }
        if mv >= last {
            break;
        }
        unsafe { *h.get_unchecked_mut(i) = mv };
        i = mc;
    }
    unsafe { *h.get_unchecked_mut(i) = last };
    top
}

#[inline(always)]
fn pf<T>(p: *const T) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch(p as *const i8, std::arch::x86_64::_MM_HINT_T0)
    };
}

/// Bidirectional Dijkstra; always expands the side with the smaller heap.
/// None if a distance does not fit the packed heap key.
fn query<L: Lab>(g: [&Cg; 2], st: &mut St<L>, s: u32, t: u32, nb: u32) -> Option<u64> {
    if s == t {
        return Some(0);
    }
    let lim = (1u64 << (64 - nb)).min(L::LIM);
    let mask = (1u64 << nb) - 1;
    let mut mu = INF;
    st.relax(0, s, 0);
    st.relax(1, t, 0);
    st.heap[0].push(s as u64);
    st.heap[1].push(t as u64);
    let mut ok = true;
    'outer: while let (Some(&xf), Some(&xb)) = (st.heap[0].first(), st.heap[1].first()) {
        if (xf >> nb) + (xb >> nb) >= mu {
            break;
        }
        let sd = (st.heap[1].len() < st.heap[0].len()) as usize;
        let to = [xf, xb][sd ^ 1] >> nb; // lower bound on the other side's unsettled labels
        let x = pop(&mut st.heap[sd]);
        let d = x >> nb;
        let u = (x & mask) as u32;
        if d > st.get(u, sd) {
            continue;
        }
        let gr = g[sd];
        if let Some(&y) = st.heap[sd].first() {
            let y = (y & mask) as u32;
            let k = unsafe { *gr.off.get_unchecked(y as usize) as usize };
            pf(unsafe { gr.e.as_ptr().add(k) });
            st.pf(y);
        }
        let (lo, hi) = unsafe { (*gr.off.get_unchecked(u as usize) as usize, *gr.off.get_unchecked(u as usize + 1) as usize) };
        for k in lo..hi {
            let (v, w) = gr.edge(u, k);
            let nd = d + w;
            if nd < mu {
                if nd >= lim {
                    ok = false;
                    break 'outer;
                }
                if let Some(o) = st.relax(sd, v, nd) {
                    if o != INF && nd + o < mu {
                        mu = nd + o;
                    }
                    // a path through v costs >= nd + to unless v is settled on the
                    // other side, and then mu already holds it: push only if useful
                    if nd + to < mu {
                        push(&mut st.heap[sd], (nd << nb) | v as u64);
                        pf(unsafe { gr.off.as_ptr().add(v as usize) });
                    }
                }
            }
        }
    }
    st.reset();
    ok.then_some(mu)
}

/// Plain Dijkstra with wide keys, for the (never seen) case of huge distances.
#[cold]
fn slow(g: &Cg, s: u32, t: u32) -> u64 {
    use std::{cmp::Reverse, collections::*};
    let mut dist: HashMap<u32, u64> = HashMap::new();
    let mut h = BinaryHeap::new();
    dist.insert(s, 0);
    h.push(Reverse((0u64, s)));
    while let Some(Reverse((d, u))) = h.pop() {
        if d > dist[&u] {
            continue;
        }
        if u == t {
            return d;
        }
        for k in g.row(u as usize) {
            let (v, w) = g.edge(u, k);
            let nd = d + w;
            if dist.get(&v).map_or(true, |&o| nd < o) {
                dist.insert(v, nd);
                h.push(Reverse((nd, v)));
            }
        }
    }
    INF
}

fn main() {
    let mut data = "/data".to_string();
    let mut out = "/out".to_string();
    let args: Vec<String> = std::env::args().collect();
    for i in 1..args.len().saturating_sub(1) {
        match args[i].as_str() {
            "--data" => data = args[i + 1].clone(),
            "--out" => out = args[i + 1].clone(),
            _ => {}
        }
    }
    #[cfg(target_os = "linux")]
    unsafe {
        extern "C" {
            fn mallopt(p: i32, v: i32) -> i32;
        }
        mallopt(-8, 1); // M_ARENA_MAX: one arena, no per-thread heaps (they cost RSS)
        mallopt(-3, 1 << 17); // M_MMAP_THRESHOLD fixed: blocks >= 128 KiB go back to the OS on free
    }
    let t0 = std::time::Instant::now();
    let tm = std::env::var("DT_TIME").is_ok();
    let lap = |what: &str| {
        if tm {
            eprintln!("{what}: {:?} {}", t0.elapsed(), std::fs::read_to_string("/proc/self/status").unwrap_or_default().lines().filter(|l| l.starts_with("VmHWM") || l.starts_with("VmRSS")).collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" "))
        }
    };
    let nt = std::thread::available_parallelism().map_or(12, |x| x.get()).min(32);
    start_pool(nt);
    let gb = load(&format!("{data}/graph.txt"));
    let qb = load(&format!("{data}/queries.txt"));
    lap("load");
    let mut p = 0;
    let n = num(gb, &mut p) as usize;
    let _m = num(gb, &mut p);
    // Rows are split into nt equal ranges; thread t owns rows lo(t)..lo(t + 1), so
    // counting and scattering need no atomics.
    let mul = ((nt as u64) << 32) / n as u64;
    let own = |x: u32| ((x as u64 * mul) >> 32) as usize;
    let los: Vec<usize> = (0..=nt).map(|t| (((t as u64) << 32).div_ceil(mul) as usize).min(n)).collect();
    let lo = |t: usize| los[t];
    let ch = chunks(gb, p, nt);
    // g0 is built from buckets of ns = nt * SUB row slots (thread t handles slots
    // t * SUB .. (t + 1) * SUB one at a time, so its scattered writes stay in cache).
    // parts[p], bucket j: edges parsed by thread p whose tail is in slot j, packed as
    // 48 bits: (tail - slo(j)) << (wb + nb) | head << wb | weight; heavier edges go to `huge`
    const SUB: usize = 8;
    let ns = nt * SUB;
    let smul = ((ns as u64) << 32) / n as u64;
    let sown = |x: u32| ((x as u64 * smul) >> 32) as usize;
    let slos: Vec<usize> = (0..=ns).map(|j| (((j as u64) << 32).div_ceil(smul) as usize).min(n)).collect();
    let slo = |j: usize| slos[j];
    let nb = (usize::BITS - n.leading_zeros()).max(1);
    let ub = (0..ns).map(|j| usize::BITS - (slo(j + 1) - slo(j)).leading_zeros()).max().unwrap();
    let wb = 48u32.saturating_sub(nb + ub).min(16);
    let (vs, vm, wm) = (wb + nb, (1u64 << nb) - 1, (1u32 << wb) - 1);
    let res = par(nt, |t| {
        let (a, b) = ch[t];
        let mut bk = Buckets::new(ns, (b - a) / 18 * 21 / 20);
        let mut huge = Vec::new();
        let mut i = a;
        while i < b {
            let (u, v, w, j) = edge(gb, i);
            i = j;
            if u == v {
            } else if w < wm {
                let o = sown(u);
                bk.push(o, ((u as usize - slo(o)) as u64) << vs | (v as u64) << wb | w as u64);
            } else {
                huge.push([u, v, w]);
            }
        }
        (bk, huge)
    });
    let huge: Vec<[u32; 3]> = res.iter().flat_map(|r| r.1.iter().copied()).collect();
    let parts: Vec<Buckets> = res.into_iter().map(|r| r.0).collect();
    lap("parse");
    let mut deg = vec![0u32; n + 1];
    let dp = P(deg.as_mut_ptr());
    par(nt, |t| {
        let dp = dp;
        for j in t * SUB..(t + 1) * SUB {
            let d = unsafe { dp.0.add(slo(j)) };
            for p in &parts {
                p.each(j, |x| unsafe { *d.add((x >> vs) as usize) += 1 });
            }
        }
        for e in huge.iter().filter(|e| sown(e[0]) / SUB == t) {
            unsafe { *dp.0.add(e[0] as usize) += 1 };
        }
    });
    let mut off0 = deg;
    prefix(&mut off0, nt);
    let m = off0[n] as usize;
    let mut e0: Vec<u32> = uninit(m);
    // first graph: head << gw | weight, weights >= gm escaped to `big`
    let gw = 32 - nb;
    let gm = (1u32 << gw) - 1;
    let ep = P(e0.as_mut_ptr());
    let mut big: Vec<(u32, u32)> = par(nt, |t| {
        let ep = ep;
        let mut big = Vec::new();
        let mut cur = Vec::new();
        for j in t * SUB..(t + 1) * SUB {
            cur.clear();
            cur.extend_from_slice(&off0[slo(j)..slo(j + 1)]);
            for p in &parts {
                p.each(j, |x| {
                    let k = unsafe { cur.get_unchecked_mut((x >> vs) as usize) };
                    let (v, w) = ((x >> wb & vm) as u32, (x & wm as u64) as u32);
                    unsafe { *ep.0.add(*k as usize) = v << gw | w.min(gm) };
                    if w >= gm {
                        big.push((*k, w));
                    }
                    *k += 1;
                });
            }
            for e in huge.iter().filter(|e| sown(e[0]) == j) {
                let k = &mut cur[e[0] as usize - slo(j)];
                unsafe { *ep.0.add(*k as usize) = e[1] << gw | gm };
                big.push((*k, e[2]));
                *k += 1;
            }
        }
        big
    })
    .concat();
    big.sort_unstable();
    drop(parts);
    let g0 = Csr { off: off0, e: e0, big, gw, gm };
    lap("csr0");
    let mut tag = vec![NONE; n];
    let parts = order(&g0, n, nt, atomics(&mut tag));
    drop(g0);
    let (mut nb0, mut eb0, mut xb0) = (vec![0usize], vec![0usize], vec![0usize]);
    for p in &parts {
        nb0.push(nb0.last().unwrap() + p.len);
        eb0.push(eb0.last().unwrap() + p.e.len());
        xb0.push(xb0.last().unwrap() + p.ext.len());
    }
    // new id from a tag: thread t's i-th node is nb0[t] + i
    let newid = |x: u32| (nb0[x as usize >> 26] + (x as usize & ((1 << 26) - 1))) as u32;
    let nid = |old: u32| newid(tag[old as usize]);
    lap("order");
    // forward graph: the threads' rows side by side, escapes resolved
    let mut off: Vec<u32> = uninit(n + 1);
    let mut fe: Vec<u32> = uninit(m);
    let mut fx: Vec<(u32, u32)> = uninit(*xb0.last().unwrap());
    let (op, ep, xp) = (P(off.as_mut_ptr()), P(fe.as_mut_ptr()), P(fx.as_mut_ptr()));
    par(nt, |t| {
        let (op, ep, xp) = (op, ep, xp);
        let r = &parts[t];
        for (i, &o) in r.off.iter().enumerate() {
            unsafe { *op.0.add(nb0[t] + i) = (eb0[t] + o as usize) as u32 };
        }
        for (j, &x) in r.e.iter().enumerate() {
            unsafe { *ep.0.add(eb0[t] + j) = if x & ESC != 0 { x + xb0[t] as u32 } else { x } };
        }
        for (j, &(key, w)) in r.ext.iter().enumerate() {
            let v = if key & ESC != 0 { nid(key & !ESC) } else { newid(key) };
            unsafe { *xp.0.add(xb0[t] + j) = (v, w) };
        }
    });
    off[n] = m as u32;
    drop(parts);
    let fwd = Cg { off, e: fe, ext: fx };
    // distinct queries (duplicate lines are answered once)
    let (mut qs, mut qi, mut seen) = (Vec::new(), Vec::new(), std::collections::HashMap::new());
    let mut i = 0;
    while i < qb.len() {
        let s = nid(num(qb, &mut i) as u32);
        let t = nid(num(qb, &mut i) as u32);
        qi.push(*seen.entry((s, t)).or_insert_with(|| {
            qs.push((s, t));
            qs.len() - 1
        }));
    }
    drop(tag);
    lap("fwd");
    // reverse graph: edges whose head another thread owns are handed over in lists
    let mut roff = vec![0u32; n + 1];
    let rp = P(roff.as_mut_ptr());
    // cross[t][o]: (head, tail, fwd edge index) for edges whose head thread o owns
    let cross: Vec<Vec<Vec<[u32; 3]>>> = par(nt, |t| {
        let rp = rp;
        let mut cr = vec![Vec::new(); nt];
        for u in lo(t)..lo(t + 1) {
            for k in fwd.row(u) {
                let v = fwd.edge(u as u32, k).0;
                if own(v) == t {
                    unsafe { *rp.0.add(v as usize) += 1 };
                } else {
                    cr[own(v)].push([v, u as u32, k as u32]);
                }
            }
        }
        cr
    });
    par(nt, |t| {
        let rp = rp;
        for x in cross.iter().flat_map(|c| &c[t]) {
            unsafe { *rp.0.add(x[0] as usize) += 1 };
        }
    });
    prefix(&mut roff, nt);
    let mut re: Vec<u32> = uninit(m);
    let ep = P(re.as_mut_ptr());
    let rx = par(nt, |t| {
        let ep = ep;
        let base = lo(t);
        let mut cur = roff[base..lo(t + 1)].to_vec();
        let mut ext = Vec::new();
        let mut put = |v: u32, u: u32, k: usize| {
            let c = &mut cur[v as usize - base];
            let w = fwd.edge(u, k).1 as u32;
            let x = enc(v, u, w).unwrap_or_else(|| {
                ext.push((u, w));
                ESC | (ext.len() - 1) as u32
            });
            unsafe { *ep.0.add(*c as usize) = x };
            *c += 1;
        };
        for u in base..lo(t + 1) {
            for k in fwd.row(u) {
                let v = fwd.edge(u as u32, k).0;
                if own(v) == t {
                    put(v, u as u32, k);
                }
            }
        }
        for x in cross.iter().flat_map(|c| &c[t]) {
            put(x[0], x[1], x[2] as usize);
        }
        ext
    });
    drop(cross);
    // thread-local escape indices -> global
    let mut xb = vec![0u32];
    for x in &rx {
        xb.push(xb.last().unwrap() + x.len() as u32);
    }
    par(nt, |t| {
        let ep = ep;
        for k in roff[lo(t)] as usize..roff[lo(t + 1)] as usize {
            let x = unsafe { &mut *ep.0.add(k) };
            if *x & ESC != 0 {
                *x += xb[t];
            }
        }
    });
    if tm {
        eprintln!("escapes fwd {} bwd {} of {m}", fwd.ext.len(), xb[nt]);
    }
    let bwd = Cg { off: roff, e: re, ext: rx.concat() };
    lap("csr");

    let ans: Vec<AtomicU64> = (0..qs.len()).map(|_| AtomicU64::new(0)).collect();
    let next = AtomicUsize::new(0);
    par(nt, |_| {
        let (mut st, mut sw) = (St::<u32>::new(n), St::<u64>::new(n));
        loop {
            let k = next.fetch_add(1, Relaxed);
            if k >= qs.len() {
                break;
            }
            let (s, t) = qs[k];
            let d = query([&fwd, &bwd], &mut st, s, t, nb)
                .or_else(|| query([&fwd, &bwd], &mut sw, s, t, nb))
                .unwrap_or_else(|| slow(&fwd, s, t));
            ans[k].store(d, Relaxed);
        }
    });
    lap("queries");
    let mut o = String::with_capacity(qi.len() * 12);
    for &k in &qi {
        let d = ans[k].load(Relaxed);
        if d == INF {
            o.push_str("-1\n");
        } else {
            o.push_str(&d.to_string());
            o.push('\n');
        }
    }
    std::fs::write(format!("{out}/distances.txt"), o).unwrap();
    std::process::exit(0); // skip freeing the graph
}
