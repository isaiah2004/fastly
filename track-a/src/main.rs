// Speed Demon 2026, Track A (Ingest). std only, Linux, AVX2 (built for x86-64-v3).
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize as AU, Ordering::*};
use std::{fs::File, os::unix::fs::FileExt, sync::Condvar, sync::Mutex};

const H: usize = 72; // hours in the fixed window
const NE: usize = 2048; // max distinct endpoints (the catalogue has 1,842)
const TS: usize = 4096; // intern table slots
const CS: usize = 64 << 10; // input chunk, read into a buffer that stays in L2
const PAD: usize = 512; // read past a chunk: its last line (under 200 bytes) and the SIMD reads past a line start
const BLK: usize = 512; // records per block
const ESC: u32 = 0xfffff; // latency field value: the latency is in the next record
const OC: usize = NE * 256; // output bytes per hour, at most: a line per endpoint

// A record per line: g<<21 | error<<20 | latency, or ESC and then the latency; g = endpoint id + 1, so g = 0 (unused,
// or a latency stored past the last record) ends a block early. Threads append them to blocks of BLK records in the hour's region. Loc: per hour, a thread's write
// position and block end.
type Loc = [(u32, u32); H];

// endpoint intern table shared by all threads: slot = hash>>32<<32 | len<<16 | g; bytes in ent[slot]
#[repr(align(64))]
struct Ent([u8; 128]);
struct Tab {
	slot: Vec<AtomicU64>,
	ent: *mut Ent,
	nid: AU,
	gs: Vec<AtomicU32>, // g -> slot
}


// endpoint g: probe from the hash, insert if new
#[inline(always)]
unsafe fn find(tb: &Tab, e: *const u8, n: usize, hv: u64) -> usize {
	let tag = hv >> 32 << 32 | (n as u64) << 16;
	let mut s = hv as usize & (TS - 1);
	loop {
		let sl = tb.slot.get_unchecked(s);
		let t = sl.load(Acquire);
		if t >> 16 == tag >> 16 {
			if v::eq(e, n, (*tb.ent.add(s)).0.as_ptr()) {
				return (t & 0xffff) as usize;
			}
		} else if t <= 1 {
			// empty, or another thread is inserting here
			if t == 0 && sl.compare_exchange(0, 1, Acquire, Relaxed).is_ok() {
				std::ptr::copy_nonoverlapping(e, (*tb.ent.add(s)).0.as_mut_ptr(), n);
				let g = tb.nid.fetch_add(1, Relaxed) + 1;
				assert!(g < NE);
				tb.gs[g].store(s as u32, Relaxed);
				sl.store(tag | g as u64, Release);
				return g;
			}
			continue;
		}
		s = (s + 1) & (TS - 1);
	}
}

mod v {
	use std::arch::x86_64::*;
	#[inline(always)]
	pub unsafe fn mask(p: *const u8, c: u8) -> u32 { _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(p as _), _mm256_set1_epi8(c as i8))) as u32 }
	const IO: [i8; 32] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31];
	// 64 ones, then 64 zeros: from 64-n, the mask of the first n bytes
	static MK: [u8; 128] = {
		let mut m = [0u8; 128];
		let mut i = 0;
		while i < 64 {
			m[i] = 255;
			i += 1;
		}
		m
	};
	// the n bytes at p equal the n bytes at s
	#[inline(always)]
	pub unsafe fn eq(p: *const u8, n: usize, s: *const u8) -> bool {
		let mut k = 0;
		while k < n {
			let m = MK.as_ptr().add(64 - (n - k).min(64));
			let d = |o: usize| _mm256_testz_si256(_mm256_xor_si256(_mm256_loadu_si256(p.add(k + o) as _), _mm256_loadu_si256(s.add(k + o) as _)), _mm256_loadu_si256(m.add(o) as _));
			if d(0) & d(32) == 0 {
				return false;
			}
			k += 64;
		}
		true
	}
	// index of the last space in the 16 bytes at p
	#[inline(always)]
	pub unsafe fn lastsp(p: *const u8) -> usize { 31 - (_mm_movemask_epi8(_mm_cmpeq_epi8(_mm_loadu_si128(p as _), _mm_set1_epi8(32))) as u32).leading_zeros() as usize }
	// the n-digit decimal number that ends at p+16
	#[inline(always)]
	pub unsafe fn lat(p: *const u8, n: usize) -> u32 {
		let w = _mm_sub_epi8(_mm_loadu_si128(p as _), _mm_set1_epi8(48));
		let d = _mm_and_si128(w, _mm_cmpgt_epi8(_mm_loadu_si128(IO.as_ptr() as _), _mm_set1_epi8(15 - n as i8)));
		let t = _mm_madd_epi16(_mm_maddubs_epi16(d, _mm_set1_epi16(0x010a)), _mm_set1_epi32(0x10064));
		let t = _mm_madd_epi16(_mm_packus_epi32(t, t), _mm_set1_epi32(0x12710));
		(_mm_cvtsi128_si32(t) as u32 as u64 * 100_000_000 + _mm_extract_epi32(t, 1) as u32 as u64) as u32
	}
}

// hash of the n endpoint bytes at p from its first, middle and last 8 (find compares the bytes)
#[inline(always)]
unsafe fn hash(p: *const u8, n: usize) -> u64 {
	let r = |o: usize| (p.add(o) as *const u64).read_unaligned();
	let (a, b, c) = if n >= 8 { (r(0), r((n - 8) / 2), r(n - 8)) } else { (r(0) & !(u64::MAX << (8 * n)), 0, 0) };
	let h = (a ^ n as u64).wrapping_mul(0x9E3779B97F4A7C15) ^ b.wrapping_mul(0xC2B2AE3D27D4EB4F).rotate_left(21) ^ c.wrapping_mul(0x165667B19E3779F9).rotate_left(42);
	h ^ (h >> 29)
}

// endpoint start: the first '/' after the 27-byte timestamp and the method
#[inline(always)]
unsafe fn ep_start(d: *const u8, p: usize) -> usize {
	let x = (d.add(p + 32) as *const u64).read_unaligned() ^ 0x2f2f2f2f2f2f2f2f;
	p + 32 + ((x.wrapping_sub(0x0101010101010101) & !x & 0x8080808080808080).trailing_zeros() / 8) as usize
}

// hour index 0..71 from YYYY-MM-DDTHH (the window is 2026-10-30T18 .. 2026-11-02T17)
#[inline(always)]
unsafe fn hour(s: *const u8) -> usize {
	let b = |i| (*s.add(i) - b'0') as usize;
	let d = b(8) * 10 + b(9);
	(if *s.add(6) == b'1' { d + 1 } else { d - 30 }) * 24 + b(11) * 10 + b(12) - 18
}

// f(line start, newline) for each line starting in [s, end). The newlines are listed first, two per 64 bytes
// without branching (a line is over 32 bytes), so the line loop is predictable.
#[inline(always)]
unsafe fn lines(d: *const u8, mut s: usize, end: usize, mut f: impl FnMut(usize, usize)) {
	let (mut ix, mut n, mut b) = ([0u32; CS / 32 + 32], 0, s);
	while b < end + 256 {
		let mut m = v::mask(d.add(b), 10) as u64 | (v::mask(d.add(b + 32), 10) as u64) << 32;
		ix[n] = (b + m.trailing_zeros() as usize) as u32;
		m &= m.wrapping_sub(1);
		ix[n + 1] = (b + m.trailing_zeros() as usize) as u32;
		n += (m != 0) as usize + (ix[n] < (b + 64) as u32) as usize;
		b += 64;
	}
	for &nl in &ix[..n] {
		if s >= end {
			break;
		}
		f(s, nl as usize);
		s = nl as usize + 1;
	}
}

// a new block for hour h
#[cold]
unsafe fn nb(h: usize, cx: &Cx) -> (u32, u32) {
	let s = cx.top[h].fetch_add(BLK, Relaxed);
	assert!(s + BLK <= cx.cap);
	let s = h * cx.cap + s;
	(s as u32, (s + BLK) as u32)
}

// the single pass over lines starting in [p, end): intern the endpoint, append the line's record to its hour. A line
// of an hour already finished (in fin) is not recorded; it flags a rerun. Returns the latest hour seen.
unsafe fn pass(d: *const u8, p: usize, end: usize, l: &mut Loc, cx: &Cx, fin: u128) -> usize {
	let (r, tb, mut mx) = (cx.rec, &cx.tb, 0);
	lines(d, p, end, |p, nl| {
		let h = hour(d.add(p));
		mx = mx.max(h);
		if fin >> h & 1 != 0 {
			cx.late.store(true, Relaxed);
			return;
		}
		let es = ep_start(d, p);
		let ls = v::lastsp(d.add(nl - 16));
		let sp = nl - 16 + ls; // the space before the latency
		let n = sp - 4 - es;
		let id = find(tb, d.add(es), n, hash(d.add(es), n)) as u32;
		let lat = v::lat(d.add(nl - 16), 15 - ls);
		let mut c = *l.get_unchecked(h);
		if c.0 + 2 > c.1 {
			c = nb(h, cx);
		}
		*r.add(c.0 as usize) = id << 21 | ((*d.add(sp - 3) == b'5') as u32) << 20 | lat.min(ESC);
		*r.add(c.0 as usize + 1) = lat;
		c.0 += 1 + (lat >= ESC) as u32;
		*l.get_unchecked_mut(h) = c;
	});
	mx
}

// f(record, latency) for every line of hour h
unsafe fn each(c: &Cx, h: usize, mut f: impl FnMut(u32, u32)) {
	let (mut k, r) = (h * c.cap, c.rec);
	while k < h * c.cap + c.top[h].load(Relaxed) {
		let x = *r.add(k);
		if x >> 21 == 0 {
			k = (k | (BLK - 1)) + 1;
			continue;
		}
		let esc = x & ESC == ESC;
		f(x, if esc { *r.add(k + 1) } else { x & ESC });
		k += 1 + esc as usize;
	}
}

const LUT: [u8; 200] = {
	let mut t = [0u8; 200];
	let mut i = 0;
	while i < 200 {
		t[i] = b'0' + if i % 2 == 0 { i / 20 } else { i / 2 % 10 } as u8;
		i += 1;
	}
	t
};
// write " <v>" at q (and up to 15 bytes of scratch after it); returns the end. No branches.
unsafe fn num(q: *mut u8, mut v: u32) -> *mut u8 {
	let (n, mut t) = (v.max(1).ilog10() as usize + 1, [0u8; 32]);
	for i in 0..5 {
		std::ptr::copy_nonoverlapping(LUT.as_ptr().add(v as usize % 100 * 2), t.as_mut_ptr().add(14 - 2 * i), 2);
		v /= 100;
	}
	t[15 - n] = b' ';
	std::ptr::copy_nonoverlapping(t.as_ptr().add(15 - n), q, 16);
	q.add(n + 1)
}

extern "C" {
	fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
	fn madvise(a: *const u8, l: usize, adv: i32) -> i32;
	fn mallopt(p: i32, v: i32) -> i32;
}

// n bytes of address space; only the 4 KB pages written are charged
fn arena(n: usize) -> *mut u8 {
	unsafe {
		let p = mmap(std::ptr::null_mut(), n, 3, 0x4022, -1, 0); // RW, PRIVATE | ANONYMOUS | NORESERVE
		assert!(p as isize != -1);
		madvise(p, n, 15); // MADV_NOHUGEPAGE
		p
	}
}

// MADV_DONTNEED the pages of [p, p+n)
unsafe fn free(p: *const u8, n: usize) { madvise(p, (n + 4095) & !4095, 4); }

struct Cx {
	inp: File,
	len: usize,
	file: File,
	tb: Tab,
	rec: *mut u32, // H regions of cap records
	val: *mut u32, // per thread: cap latencies, an hour's in endpoint order
	txt: *mut u8,  // per hour: OC bytes of output
	cap: usize,
	top: Vec<AU>, // records used per region
	nx: AU,       // next input chunk
	nk: usize,    // input chunks
	early: bool,  // finish hours before the whole input is parsed
	st: Mutex<St>,
	cv: Condvar, // threads with nothing to do wait for a chunk parsed or an hour handed out
	fin: [AtomicU64; 2], // finished hours
	late: AtomicBool,    // a line came for a finished hour
	out: Mutex<(usize, usize, [usize; H])>, // next hour to place, its offset, formatted lengths (MAX: not yet)
}
// done: per chunk, 0 or 1 + its latest hour; f: chunks parsed in a row from the start, pm: the latest hour in them;
// fh: hours finished; g: per finished hour, the chunks handed out by then; nh: hours handed out
struct St {
	done: Vec<u8>,
	f: usize,
	pm: usize,
	fh: usize,
	g: [usize; H],
	nh: usize,
}
unsafe impl Sync for Cx {}

// Threads parse chunks and turn finished hours into output. Generated logs are interleaved shards that hold all lines
// of an hour within minutes after it, so an hour is finished once the chunks parsed in a row from the start reach
// two hours past it (in a rerun: all chunks). Its records are complete when the chunks handed out by then are
// parsed; a chunk handed out later skips its lines and flags a rerun.
unsafe fn work(t: usize, c: &Cx) {
	let l = &mut [(0, 0); H];
	let mut w = vec![0u8; CS + 2 * PAD];
	let (mut od, mut ino, mut rep) = (vec![], vec![false; NE], None); // od: endpoints seen by this thread, in byte order
	let (mut cnt, mut off) = (vec![0u64; NE], vec![0u32; NE]); // cnt: count | errors<<32
	loop {
		let h = loop {
			let mut s = c.st.lock().unwrap();
			if let Some((k, m)) = rep.take() {
				s.done[k] = m as u8 + 1;
				if c.nx.load(SeqCst) >= c.len {
					c.cv.notify_all();
				}
			}
			while s.f < c.nk && s.done[s.f] != 0 {
				s.pm = s.pm.max(s.done[s.f] as usize - 1);
				s.f += 1;
			}
			while s.fh < H && (s.f == c.nk || c.early && s.pm >= s.fh + 2) {
				let fh = s.fh;
				c.fin[fh / 64].fetch_or(1 << fh % 64, SeqCst);
				s.g[fh] = (c.nx.load(SeqCst) / CS).min(c.nk);
				s.fh += 1;
			}
			if s.nh == H {
				return;
			}
			let h = s.nh;
			if h < s.fh && s.f >= s.g[h] {
				s.nh += 1;
				c.cv.notify_all();
				break h;
			}
			if c.nx.load(SeqCst) < c.len {
				break H;
			}
			drop(c.cv.wait(s));
		};
		if h < H {
			hour_out(t, h, c, &mut od, &mut ino, &mut cnt, &mut off);
			continue;
		}
		// a chunk owns the lines that start in it. It is read from the byte before it to PAD past it.
		let a = c.nx.fetch_add(CS, SeqCst);
		if a >= c.len {
			continue;
		}
		let fin = c.fin[0].load(SeqCst) as u128 | (c.fin[1].load(SeqCst) as u128) << 64;
		let o = a.saturating_sub(1);
		let k = (a + CS + PAD).min(c.len) - o;
		c.inp.read_exact_at(&mut w[..k], o as u64).unwrap();
		let x = if a == 0 { 0 } else { w.iter().position(|&b| b == b'\n').unwrap() + 1 };
		rep = Some((a / CS, pass(w.as_ptr(), x, (a + CS).min(c.len) - o, l, c, fin)));
	}
}

// hour h: count, scatter by endpoint rank, release the records, exact nearest-rank percentiles, format, write
unsafe fn hour_out(t: usize, h: usize, c: &Cx, od: &mut Vec<usize>, ino: &mut [bool], cnt: &mut [u64], off: &mut [u32]) {
	let tb = &c.tb;
	let nm = |g: usize| {
		let s = tb.gs[g].load(Relaxed) as usize;
		&(&(*tb.ent.add(s)).0)[..(tb.slot[s].load(Relaxed) >> 16 & 255) as usize]
	};
	cnt.fill(0);
	each(c, h, |r, _| {
		*cnt.get_unchecked_mut((r >> 21) as usize) += 1 | ((r >> 20 & 1) as u64) << 32;
	});
	// add the hour's endpoints new to this thread
	let n0 = od.len();
	for g in 1..NE {
		if cnt[g] != 0 && !ino[g] {
			ino[g] = true;
			od.push(g);
		}
	}
	if od.len() > n0 {
		od.sort_unstable_by(|&p, &q| nm(p).cmp(nm(q)));
	}
	let mut tot = 0;
	for &id in od.iter() {
		off[id] = tot;
		tot += cnt[id] as u32;
	}
	let v = std::slice::from_raw_parts_mut(c.val.add(t * c.cap), tot as usize);
	each(c, h, |r, x| {
		let k = off.get_unchecked_mut((r >> 21) as usize);
		*v.get_unchecked_mut(*k as usize) = x;
		*k += 1;
	});
	free(c.rec.add(h * c.cap) as _, c.top[h].load(Relaxed) * 4); // the hour's records are done
	let ab = h + 18;
	let mut hs = *b"2026-10-30T00:00:00Z ";
	hs[..10].copy_from_slice([b"2026-10-30", b"2026-10-31", b"2026-11-01", b"2026-11-02"][ab / 24]);
	hs[11] = b'0' + (ab % 24 / 10) as u8;
	hs[12] = b'0' + (ab % 24 % 10) as u8;
	let o = c.txt.add(h * OC);
	let mut q = o;
	for &id in od.iter() {
		let n = cnt[id] as u32 as usize;
		if n == 0 {
			continue;
		}
		let v = &mut v[off[id] as usize - n..off[id] as usize];
		let (r50, r95, r99) = ((50 * n + 99) / 100, (95 * n + 99) / 100, (99 * n + 99) / 100);
		let ps = if n <= 20 {
			v.sort_unstable();
			[v[r50 - 1], v[r95 - 1], v[r99 - 1]]
		} else {
			let p50 = *v.select_nth_unstable(r50 - 1).1;
			let p95 = *v[r50..].select_nth_unstable(r95 - 1 - r50).1;
			[p50, p95, *v[r95..].select_nth_unstable(r99 - 1 - r95).1]
		};
		let e = cnt[id] >> 32;
		for b in [&hs[..], nm(id)] {
			std::ptr::copy_nonoverlapping(b.as_ptr(), q, b.len());
			q = q.add(b.len());
		}
		for x in [n as u32, ps[0], ps[1], ps[2], e as u32, (e * 1_000_000 / n as u64) as u32] {
			q = num(q, x);
		}
		*q = b'\n';
		q = q.add(1);
	}
	// offsets are assigned in hour order: a formatted hour waits until the hours before it are placed, and whoever places it writes it
	let mut w = vec![];
	{
		let mut g = c.out.lock().unwrap();
		g.2[h] = q.offset_from(o) as usize;
		while g.0 < H && g.2[g.0] != usize::MAX {
			w.push((g.0, g.1, g.2[g.0]));
			g.1 += g.2[g.0];
			g.0 += 1;
		}
	}
	for (k, at, n) in w {
		let b = c.txt.add(k * OC);
		c.file.write_all_at(std::slice::from_raw_parts(b, n), at as u64).unwrap();
		free(b, n);
	}
}

fn main() {
	// M_ARENA_MAX = 1: no per-thread malloc arenas, whose mapping takes the address-space lock and stalls faulting threads
	unsafe { mallopt(-8, 1) };
	let a: Vec<String> = std::env::args().collect();
	let arg = |k: &str, d: &str| a.iter().position(|x| x == k).map_or(d.to_string(), |i| a[i + 1].clone());
	let (inp, out) = (arg("--data", "/data") + "/requests.log", arg("--out", "/out") + "/summary.txt");
	// a line for a finished hour: run again, finishing hours only at the end
	for early in [true, false] {
		if !run(&inp, &out, early) {
			break;
		}
	}
}

fn run(inp: &str, out: &str, early: bool) -> bool {
	let inp = File::open(inp).unwrap();
	let len = inp.metadata().unwrap().len() as usize;
	let file = File::create(out).unwrap();
	let nt = std::thread::available_parallelism().map_or(1, |n| n.get());
	let cap = (2 * (len / 39 + nt) / (BLK - 1) + (H + 1) * nt + 1) / 2 * 2 * BLK; // records, at most 2 per line, plus partial blocks; whole pages
	let nk = len.div_ceil(CS);
	let c = Cx {
		inp,
		len,
		file,
		tb: Tab {
			slot: (0..TS).map(|_| AtomicU64::new(0)).collect(),
			ent: unsafe { std::alloc::alloc_zeroed(std::alloc::Layout::from_size_align(TS * 128, 64).unwrap()) as *mut Ent },
			nid: AU::new(0),
			gs: (0..NE).map(|_| AtomicU32::new(0)).collect(),
		},
		rec: arena(H * cap * 4) as _,
		val: arena(nt * cap * 4) as _,
		txt: arena(H * OC),
		cap,
		top: (0..H).map(|_| AU::new(0)).collect(),
		nx: AU::new(0),
		nk,
		early,
		st: Mutex::new(St { done: vec![0; nk], f: 0, pm: 0, fh: 0, g: [0; H], nh: 0 }),
		fin: [AtomicU64::new(0), AtomicU64::new(0)],
		late: AtomicBool::new(false),
		cv: Condvar::new(),
		out: Mutex::new((0, 0, [usize::MAX; H])),
	};
	let c = &c;
	std::thread::scope(|sc| {
		for t in 0..nt {
			sc.spawn(move || unsafe { work(t, c) });
		}
	});
	c.late.load(Relaxed)
}
