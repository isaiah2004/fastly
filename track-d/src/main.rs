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

#[derive(Clone, Copy)]
struct P<T>(*mut T);
impl<T> P<T> {
    /// The i-th element; callers write disjoint elements from different threads.
    #[allow(clippy::mut_from_ref)]
    fn at(&self, i: usize) -> &mut T {
        unsafe { &mut *self.0.add(i) }
    }
}
unsafe impl<T> Send for P<T> {}
unsafe impl<T> Sync for P<T> {}

fn atomics(v: &mut [u32]) -> &[AtomicU32] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const AtomicU32, v.len()) }
}

/// Address space for n Ts; pages are committed only where written. No huge pages,
/// so a window's partly used tail costs at most one small page.
fn reserve<T>(n: usize) -> *mut T {
    #[cfg(target_os = "linux")]
    unsafe {
        extern "C" {
            fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
            fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
        }
        let l = n * std::mem::size_of::<T>();
        let p = mmap(std::ptr::null_mut(), l, 3, 0x4022, -1, 0); // MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE
        madvise(p, l, 15); // MADV_NOHUGEPAGE
        p as *mut T
    }
    #[cfg(not(target_os = "linux"))]
    Vec::with_capacity(n).leak().as_mut_ptr()
}

fn uninit<T>(m: usize) -> Vec<T> {
    let mut v = Vec::with_capacity(m);
    unsafe { v.set_len(m) };
    v
}

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
    let mut nx = || num(b, &mut i) as u32;
    let (u, v, w) = (nx(), nx(), nx());
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
    fn hd(&self, x: u32) -> u32 {
        x >> self.gw
    }
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
    off: Off,
    e: &'static [u32],
    ext: &'static [(u32, u32)],
}

const ESC: u32 = 1 << 31;

/// The packed form of an edge u -> v of weight w, if it fits.
fn enc(u: u32, v: u32, w: u32) -> Option<u32> {
    let d = (v as i64 - u as i64 + 32768) as u64;
    (d < 65536 && w < 32768).then(|| (d as u32) << 15 | w)
}

/// Row offsets, compact: a u32 base per 16 nodes plus a u16 per node relative to
/// it; 0xFFFF means "in `big`" (only for rows far from their block start).
struct Off {
    base: Vec<u32>,
    rel: Vec<u16>,
    big: Vec<(u32, u32)>,
}

impl Off {
    fn new(off: &[u32], nt: usize) -> Off {
        let n = off.len();
        let base: Vec<u32> = (0..n.div_ceil(16)).map(|b| off[b * 16]).collect();
        let mut rel: Vec<u16> = uninit(n);
        let rp = P(rel.as_mut_ptr());
        let big = par(nt, |t| {
            let mut big = Vec::new();
            let (a, b) = (n * t / nt, n * (t + 1) / nt);
            for i in a..b {
                let r = off[i] - base[i >> 4];
                *rp.at(i) = r.min(0xFFFF) as u16;
                if r >= 0xFFFF {
                    big.push((i as u32, off[i]));
                }
            }
            big
        })
        .concat();
        Off { base, rel, big }
    }
    fn get(&self, i: usize) -> usize {
        let r = unsafe { *self.rel.get_unchecked(i) };
        if r != 0xFFFF {
            unsafe { *self.base.get_unchecked(i >> 4) as usize + r as usize }
        } else {
            self.big[self.big.binary_search_by_key(&(i as u32), |x| x.0).unwrap()].1 as usize
        }
    }
    fn pf(&self, i: usize) {
        pf(unsafe { self.rel.as_ptr().add(i) });
    }
}

impl Cg {
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
        self.off.get(u)..self.off.get(u + 1)
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
    let op = out.as_mut_ptr() as usize;
    let job = move |t: usize| {
        unsafe { *(op as *mut Option<R>).add(t) = Some(f(t)) };
    };
    let jr: &(dyn Fn(usize) + Sync) = &job;
    // the job outlives its use: par returns only after every worker is done
    unsafe { *POOL.job.get() = Some(std::mem::transmute(jr)) };
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

/// Per-thread edge buckets: each bucket is a chain of BLK-entry blocks cut from a
/// few big slabs, so thousands of buckets cost a handful of allocations.
const BLK: usize = 512;
/// A 48-bit bucket entry.
#[derive(Clone, Copy)]
#[repr(C, packed)]
struct B6(u32, u16);
struct Buckets {
    slabs: Vec<Vec<B6>>,
    used: usize,
    full: Vec<Vec<*mut B6>>,
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
        let s = self.slabs.last_mut().unwrap();
        unsafe { s.as_mut_ptr().add(self.used - BLK) }
    }
    fn push(&mut self, j: usize, x: u64) {
        let c = unsafe { self.cur.get_unchecked_mut(j) };
        if c.1 == BLK {
            let p = c.0;
            self.full[j].push(p);
            let q = self.block();
            self.cur[j] = (q, 0);
        }
        let c = unsafe { self.cur.get_unchecked_mut(j) };
        unsafe { *c.0.add(c.1) = B6(x as u32, (x >> 32) as u16) };
        c.1 += 1;
    }
    fn len(&self, j: usize) -> usize {
        self.full[j].len() * BLK + self.cur[j].1
    }
    fn each(&self, j: usize, mut f: impl FnMut(u64)) {
        for (p, l) in self.full[j].iter().map(|&p| (p, BLK)).chain([self.cur[j]]) {
            for &x in unsafe { std::slice::from_raw_parts(p, l) } {
                f(x.0 as u64 | (x.1 as u64) << 32);
            }
        }
    }
}

/// Rows of one thread, in the new order, packed like `Cg` relative to the thread's
/// own numbering; `ext` keys are a tag, or ESC | old id for a head that had no tag yet.
struct Rows {
    len: usize,
    deg: Vec<u8>,
    bigdeg: Vec<(u32, u32)>, // (node, degree) where deg says 255
    e: Vec<u32>,
    ext: Vec<(u32, u32)>,
}

/// Locality order: parallel region growing (BFS from seeds, regions capped).
/// Each thread copies the rows of the nodes it placed while they are hot.
/// `tag` gets (thread << 26) | (index in that thread's list) for every node.
fn order(g: &Csr, n: usize, nt: usize, tag: &[AtomicU32], win: (P<u32>, P<(u32, u32)>, usize)) -> Vec<Rows> {
    const CAP: usize = 16384;
    const D1: usize = 16;
    const D2: usize = 8;
    let claim = |v: usize, x: usize| tag[v].load(Relaxed) == NONE && tag[v].compare_exchange(NONE, x as u32, Relaxed, Relaxed).is_ok();
    let rg = |u: usize| g.row(u);
    // append the row (still hot in cache) of u, the thread's node number iu
    let copy = |r: &mut Rows, t: usize, iu: usize, u: u32, row: &[u32]| {
        if row.len() < 255 {
            r.deg.push(row.len() as u8);
        } else {
            r.bigdeg.push((r.deg.len() as u32, row.len() as u32));
            r.deg.push(255);
        }
        for (j, &x) in row.iter().enumerate() {
            let w = g.wt(g.off[u as usize] as usize + j) as u32;
            let hv = g.hd(x);
            let tv = tag[hv as usize].load(Relaxed);
            if tv >> 26 == t as u32 {
                if let Some(c) = enc(iu as u32, tv & ((1 << 26) - 1), w) {
                    r.e.push(c);
                    continue;
                }
            }
            r.e.push(ESC | (t * win.2 + r.ext.len()) as u32);
            r.ext.push((if tv == NONE { ESC | hv } else { tv }, w));
        }
    };
    par(nt, |t| {
        // the BFS queue of the current region; earlier regions' nodes number 0..done
        let mut list: Vec<u32> = Vec::with_capacity(2 * CAP);
        let mut done = 0;
        // e and ext live in this thread's windows of the shared forward arrays (never reallocated)
        let (e, ext) = unsafe { (Vec::from_raw_parts(win.0.at(t * win.2), 0, win.2), Vec::from_raw_parts(win.1.at(t * win.2), 0, win.2)) };
        let mut r = Rows { len: 0, deg: Vec::with_capacity(n / nt * 5 / 4), bigdeg: Vec::new(), e, ext };
        for s in (n * t / nt)..(n * (t + 1) / nt) {
            if !claim(s, t << 26 | done) {
                continue;
            }
            list.push(s as u32);
            let mut q = 0;
            while q < list.len() && list.len() < CAP {
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
    dir: [Vec<u32>; 2],
    pool: [Vec<L>; 2],
    pages: [Vec<u32>; 2],
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
    fn r(self) -> u64 {
        self as u64 | ((self == u32::MAX) as u64).wrapping_neg()
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
        let d = vec![NONE; (n >> PB) + 1];
        St { dir: [d.clone(), d], pool: [Vec::new(), Vec::new()], pages: [Vec::new(), Vec::new()], heap: [Vec::new(), Vec::new()] }
    }
    fn get(&self, v: u32, s: usize) -> u64 {
        let p = unsafe { *self.dir[s].get_unchecked((v >> PB) as usize) };
        if p == NONE {
            return INF;
        }
        unsafe { self.pool[s].get_unchecked(((p << PB) | (v & PM)) as usize) }.r()
    }
    fn pf(&self, v: u32, s: usize) {
        let p = unsafe { *self.dir[s].get_unchecked((v >> PB) as usize) };
        if p != NONE {
            pf(unsafe { self.pool[s].as_ptr().add(((p << PB) | (v & PM)) as usize) });
        }
    }
    #[cold]
    fn alloc(&mut self, s: usize, v: u32) -> u32 {
        let p = self.pages[s].len() as u32;
        self.pages[s].push(v >> PB);
        let lo = (p as usize) << PB;
        let pool = &mut self.pool[s];
        if pool.len() < lo + (1 << PB) {
            pool.resize(lo + (1 << PB), L::MAX);
        } else {
            pool[lo..lo + (1 << PB)].fill(L::MAX);
        }
        self.dir[s][(v >> PB) as usize] = p;
        p
    }
    /// Lower v's label on side s to d. If it improved, return the other side's label of v.
    fn relax(&mut self, s: usize, v: u32, d: u64) -> Option<u64> {
        let mut p = unsafe { *self.dir[s].get_unchecked((v >> PB) as usize) };
        if p == NONE {
            p = self.alloc(s, v);
        }
        let l = unsafe { self.pool[s].get_unchecked_mut(((p << PB) | (v & PM)) as usize) };
        if d >= l.r() {
            return None;
        }
        *l = L::w(d);
        Some(self.get(v, s ^ 1))
    }
    fn reset(&mut self) {
        for s in 0..2 {
            for &q in &self.pages[s] {
                self.dir[s][q as usize] = NONE;
            }
            self.pages[s].clear();
        }
        self.heap[0].clear();
        self.heap[1].clear();
    }
}

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

fn pop(h: &mut Vec<u64>) -> u64 {
    let top = h[0];
    let last = h.pop().unwrap();
    let len = h.len();
    let mut i = 0;
    while 4 * i + 1 < len {
        // branchless minimum of the (up to) four children
        let g = |j: usize| if j < len { unsafe { *h.get_unchecked(j) } } else { u64::MAX };
        let c = 4 * i + 1;
        let (m1, i1) = if g(c + 1) < g(c) { (g(c + 1), c + 1) } else { (g(c), c) };
        let (m2, i2) = if g(c + 3) < g(c + 2) { (g(c + 3), c + 3) } else { (g(c + 2), c + 2) };
        let (mv, mc) = if m2 < m1 { (m2, i2) } else { (m1, i1) };
        if mv >= last {
            break;
        }
        h[i] = mv;
        i = mc;
    }
    if len > 0 {
        h[i] = last;
    }
    top
}

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
            let k = gr.off.get(y as usize);
            pf(unsafe { gr.e.as_ptr().add(k) });
            st.pf(y, sd);
        }
        let (lo, hi) = (gr.off.get(u as usize), gr.off.get(u as usize + 1));
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
                        gr.off.pf(v as usize);
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
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str, d: &str| args.iter().position(|a| a == k).map_or(d.into(), |i| args[i + 1].clone());
    let (data, out) = (arg("--data", "/data"), arg("--out", "/out"));
    #[cfg(target_os = "linux")]
    unsafe {
        extern "C" {
            fn mallopt(p: i32, v: i32) -> i32;
        }
        mallopt(-8, 1); // M_ARENA_MAX: one arena, no per-thread heaps (they cost RSS)
        mallopt(-3, 1 << 17); // M_MMAP_THRESHOLD fixed: blocks >= 128 KiB go back to the OS on free
    }
    let nt = std::thread::available_parallelism().map_or(12, |x| x.get()).min(32);
    start_pool(nt);
    let gb = load(&format!("{data}/graph.txt"));
    let qb = load(&format!("{data}/queries.txt"));
    let mut p = 0;
    let n = num(gb, &mut p) as usize;
    let _m = num(gb, &mut p);
    let ch = chunks(gb, p, nt);
    // g0 is built from buckets of ns = nt * SUB row slots (thread t handles slots
    // t * SUB .. (t + 1) * SUB one at a time, so its scattered writes stay in cache).
    // parts[p], bucket j: edges parsed by thread p whose tail is in slot j, packed as
    // 48 bits: (tail - slos[j]) << (wb + nb) | head << wb | weight; heavier edges go to `huge`
    const SUB: usize = 8;
    let ns = nt * SUB;
    let smul = ((ns as u64) << 32) / n as u64;
    let sown = |x: u32| ((x as u64 * smul) >> 32) as usize;
    let slos: Vec<usize> = (0..=ns).map(|j| (((j as u64) << 32).div_ceil(smul) as usize).min(n)).collect();
    let nb = (usize::BITS - n.leading_zeros()).max(1);
    let ub = (0..ns).map(|j| usize::BITS - (slos[j + 1] - slos[j]).leading_zeros()).max().unwrap();
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
                bk.push(o, ((u as usize - slos[o]) as u64) << vs | (v as u64) << wb | w as u64);
            } else {
                huge.push([u, v, w]);
            }
        }
        (bk, huge)
    });
    let huge: Vec<[u32; 3]> = res.iter().flat_map(|r| r.1.iter().copied()).collect();
    let parts: Vec<Buckets> = res.into_iter().map(|r| r.0).collect();
    // first graph: head << gw | weight, weights >= gm escaped to `big`. Slot by slot:
    // count, prefix and scatter while the slot's buckets are still in cache.
    let gw = 32 - nb;
    let gm = (1u32 << gw) - 1;
    let mut sb = vec![0u32];
    for j in 0..ns {
        let c = parts.iter().map(|p| p.len(j)).sum::<usize>() + huge.iter().filter(|e| sown(e[0]) == j).count();
        sb.push(sb[j] + c as u32);
    }
    let m = sb[ns] as usize;
    let mut off0: Vec<u32> = uninit(n + 1);
    let mut e0: Vec<u32> = uninit(m);
    let (op, ep) = (P(off0.as_mut_ptr()), P(e0.as_mut_ptr()));
    let mut big: Vec<(u32, u32)> = par(nt, |t| {
        let (mut big, mut cur) = (Vec::new(), Vec::new());
        for j in t * SUB..(t + 1) * SUB {
            let (a, mine) = (slos[j], || huge.iter().filter(|e| sown(e[0]) == j));
            cur.clear();
            cur.resize(slos[j + 1] - a, 0u32);
            for p in &parts {
                p.each(j, |x| unsafe { *cur.get_unchecked_mut((x >> vs) as usize) += 1 });
            }
            for e in mine() {
                cur[e[0] as usize - a] += 1;
            }
            let mut s = sb[j];
            for (i, c) in cur.iter_mut().enumerate() {
                (*c, s) = (s, s + *c);
                *op.at(a + i) = *c;
            }
            for p in &parts {
                p.each(j, |x| {
                    let k = unsafe { cur.get_unchecked_mut((x >> vs) as usize) };
                    let (v, w) = ((x >> wb & vm) as u32, (x & wm as u64) as u32);
                    *ep.at(*k as usize) = v << gw | w.min(gm);
                    if w >= gm {
                        big.push((*k, w));
                    }
                    *k += 1;
                });
            }
            for e in mine() {
                let k = &mut cur[e[0] as usize - a];
                *ep.at(*k as usize) = e[1] << gw | gm;
                big.push((*k, e[2]));
                *k += 1;
            }
        }
        big
    })
    .concat();
    off0[n] = m as u32;
    big.sort_unstable();
    drop(parts);
    let g0 = Csr { off: off0, e: e0, big, gw, gm };
    let mut tag = vec![NONE; n];
    // thread t's rows go to window [t * m, t * m + its edges) of the forward arrays,
    // written in place by the BFS: no concatenation copy
    let win = (P(reserve::<u32>(nt * m)), P(reserve::<(u32, u32)>(nt * m)), m);
    let parts = order(&g0, n, nt, atomics(&mut tag), win);
    drop(g0);
    // node ids: thread t's i-th node is nb0[t] + i; a dummy node closes each thread's
    // segment (its row is the unused rest of the window, it is never reached)
    let mut nb0 = vec![0usize];
    for p in &parts {
        nb0.push(nb0.last().unwrap() + p.len + 1);
    }
    let n2 = nb0[nt];
    let id = |x: u32| (nb0[x as usize >> 26] + (x as usize & ((1 << 26) - 1))) as u32;
    let nid = |old: u32| id(tag[old as usize]);
    let mut off: Vec<u32> = uninit(n2 + 1);
    let op = P(off.as_mut_ptr());
    par(nt, |t| {
        let r = &parts[t];
        let (mut o, mut bd) = (t * m, r.bigdeg.iter());
        for (i, &d) in r.deg.iter().enumerate() {
            *op.at(nb0[t] + i) = o as u32;
            o += if d < 255 { d as usize } else { bd.next().unwrap().1 as usize };
        }
        *op.at(nb0[t + 1] - 1) = o as u32;
        for j in t * m..t * m + r.ext.len() {
            let (key, w) = *win.1.at(j);
            *win.1.at(j) = (if key < ESC { id(key) } else { nid(key - ESC) }, w);
        }
    });
    off[n2] = (nt * m) as u32;
    for p in parts {
        std::mem::forget((p.e, p.ext));
    }
    let fwd = unsafe { Cg { off: Off::new(&off, nt), e: std::slice::from_raw_parts(win.0 .0, nt * m), ext: std::slice::from_raw_parts(win.1 .0, nt * m) } };
    drop(off);
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
    // reverse graph, thread-local: thread t owns its own BFS segment nb0[t]..nb0[t + 1].
    // Packed edges never leave their segment, so only escaped edges (~4%) can cross;
    // those go over in lists of (head, tail, weight). Each thread counts into, prefixes
    // and fills its own window of roff; escapes are counted first so their indices are final.
    let seg = |v: u32| nb0.partition_point(|&b| b <= v as usize) - 1;
    let mut roff = vec![0u32; n2 + 1];
    let rp = P(roff.as_mut_ptr());
    let cross = par(nt, |t| {
        let (mut cr, mut xc) = (vec![Vec::new(); nt], vec![0u32; nt]);
        for u in nb0[t]..nb0[t + 1] - 1 {
            for k in fwd.row(u) {
                let (v, w) = fwd.edge(u as u32, k);
                let o = if fwd.e[k] & ESC == 0 { t } else { seg(v) };
                xc[o] += enc(v, u as u32, w as u32).is_none() as u32;
                if o == t {
                    *rp.at(v as usize) += 1;
                } else {
                    cr[o].push([v, u as u32, w as u32]);
                }
            }
        }
        (cr, xc)
    });
    let (mut rb, mut xb) = (vec![0u32], vec![0u32]);
    for t in 0..nt {
        let inc = cross.iter().map(|x| x.0[t].len()).sum::<usize>() as u32;
        rb.push(rb[t] + inc + roff[nb0[t]..nb0[t + 1]].iter().sum::<u32>());
        xb.push(xb[t] + cross.iter().map(|x| x.1[t]).sum::<u32>());
    }
    let mut re: Vec<u32> = uninit(m);
    let ep = P(re.as_mut_ptr());
    let rx = par(nt, |t| {
        let mut ext = Vec::new();
        for x in cross.iter().flat_map(|c| &c.0[t]) {
            *rp.at(x[0] as usize) += 1;
        }
        // inclusive prefix: each row is then filled from its end down to its start
        let mut s = rb[t];
        for v in nb0[t]..nb0[t + 1] {
            s += *rp.at(v);
            *rp.at(v) = s;
        }
        let mut put = |v: u32, u: u32, w: u32| {
            let c = rp.at(v as usize);
            *c -= 1;
            *ep.at(*c as usize) = enc(v, u, w).unwrap_or_else(|| {
                ext.push((u, w));
                ESC | (xb[t] + ext.len() as u32 - 1)
            });
        };
        for u in nb0[t]..nb0[t + 1] - 1 {
            for k in fwd.row(u) {
                let (v, w) = fwd.edge(u as u32, k);
                if fwd.e[k] & ESC == 0 || seg(v) == t {
                    put(v, u as u32, w as u32);
                }
            }
        }
        for x in cross.iter().flat_map(|c| &c.0[t]) {
            put(x[0], x[1], x[2]);
        }
        ext
    });
    roff[n2] = m as u32;
    drop(cross);
    let bwd = Cg { off: Off::new(&roff, nt), e: re.leak(), ext: rx.concat().leak() };
    drop(roff);

    // likely long searches first (ids far apart), so the phase does not end on one
    let mut qo: Vec<usize> = (0..qs.len()).collect();
    qo.sort_by_key(|&k| std::cmp::Reverse(qs[k].0.abs_diff(qs[k].1)));
    let nq = usize::BITS - n2.leading_zeros();
    let ans: Vec<AtomicU64> = (0..qs.len()).map(|_| AtomicU64::new(0)).collect();
    let next = AtomicUsize::new(0);
    par(nt, |_| {
        let (mut st, mut sw) = (St::<u32>::new(n2), St::<u64>::new(n2));
        while let Some(&k) = qo.get(next.fetch_add(1, Relaxed)) {
            let (s, t) = qs[k];
            let d = query([&fwd, &bwd], &mut st, s, t, nq)
                .or_else(|| query([&fwd, &bwd], &mut sw, s, t, nq))
                .unwrap_or_else(|| slow(&fwd, s, t));
            ans[k].store(d, Relaxed);
        }
    });
    let mut o = String::with_capacity(qi.len() * 12);
    for &k in &qi {
        match ans[k].load(Relaxed) {
            INF => o.push_str("-1\n"),
            d => o += &format!("{d}\n"),
        }
    }
    std::fs::write(format!("{out}/distances.txt"), o).unwrap();
    std::process::exit(0); // skip freeing the graph
}
