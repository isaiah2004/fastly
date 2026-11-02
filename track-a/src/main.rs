// Speed Demon 2026, Track A (Ingest). std only; AVX2 helpers with a scalar fallback.
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize as AU, Ordering::*};
use std::{cell::UnsafeCell, fs::File, sync::Barrier};

const H: usize = 72; // hours in the fixed window
const NE: usize = 2048; // max distinct endpoints (the catalogue has 1,842)
const TS: usize = 4096; // intern table slots
const PAD: usize = 256; // readable bytes needed past any line start

// shared mutable state; phases are separated by barriers
struct S<T>(UnsafeCell<T>);
unsafe impl<T> Sync for S<T> {}
impl<T> S<T> {
	#[allow(clippy::mut_from_ref)]
	fn g(&self) -> &mut T { unsafe { &mut *self.0.get() } }
}
fn s<T>(v: T) -> S<T> { S(UnsafeCell::new(v)) }

#[derive(Default)]
#[repr(align(128))] // no false sharing between threads
struct Loc {
	cnt: Vec<u64>, // [hour*NE + id]: count | errors<<32, later the scatter cursor
	hm: u128,      // hours seen
}

// endpoint intern table shared by all threads: slot = hash>>32<<32 | len<<16 | id+1; bytes in ent[slot]
#[repr(align(64))]
struct Ent([u8; 128]);
struct Tab {
	slot: Vec<AtomicU64>,
	ent: *mut Ent,
	nid: AU,
	gs: Vec<AtomicU32>, // id -> slot
	hs: Vec<AtomicU64>, // slot -> full hash
	dup: AtomicBool,    // two different endpoints share a full hash and length
}

// endpoint id: probe from the hash; byte-compare when verifying (pass 1) or if hashes are not unique
#[inline(always)]
unsafe fn find(tb: &Tab, e: *const u8, n: usize, hv: u64, verify: bool) -> usize {
	let tag = hv >> 32 << 32 | (n as u64) << 16;
	let mut s = hv as usize & (TS - 1);
	loop {
		let sl = tb.slot.get_unchecked(s);
		let t = sl.load(Acquire);
		if t >> 16 == tag >> 16 && tb.hs.get_unchecked(s).load(Relaxed) == hv {
			if !verify || v::eq(e, n, (*tb.ent.add(s)).0.as_ptr()) {
				return (t & 0xffff) as usize - 1;
			}
			tb.dup.store(true, Relaxed);
		} else if t == 0 {
			if sl.compare_exchange(0, 1, Acquire, Relaxed).is_ok() {
				std::ptr::copy_nonoverlapping(e, (*tb.ent.add(s)).0.as_mut_ptr(), n);
				let id = tb.nid.fetch_add(1, Relaxed);
				assert!(id < NE);
				tb.gs[id].store(s as u32, Relaxed);
				tb.hs[s].store(hv, Relaxed);
				sl.store(tag | (id as u64 + 1), Release);
				return id;
			}
			continue;
		} else if t == 1 {
			continue; // another thread is inserting here
		}
		s = (s + 1) & (TS - 1);
	}
}

#[cfg(target_feature = "avx2")]
mod v {
	use std::arch::x86_64::*;
	#[inline(always)]
	pub unsafe fn mask(p: *const u8, c: u8) -> u32 { _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(p as _), _mm256_set1_epi8(c as i8))) as u32 }
	const IO: [i8; 32] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31];
	// 32 bytes at p, bytes from index rem on zeroed
	#[inline(always)]
	unsafe fn block(p: *const u8, rem: usize) -> __m256i {
		_mm256_and_si256(_mm256_loadu_si256(p as _), _mm256_cmpgt_epi8(_mm256_set1_epi8(rem.min(32) as i8), _mm256_loadu_si256(IO.as_ptr() as _)))
	}
	#[inline(always)]
	pub unsafe fn hash(p: *const u8, n: usize) -> u64 {
		let t = _mm256_and_si256(_mm256_loadu_si256(p.add(n).sub(32) as _), _mm256_cmpgt_epi8(_mm256_loadu_si256(IO.as_ptr() as _), _mm256_set1_epi8(31 - n as i8)));
		let x = _mm256_xor_si256(block(p, n), _mm256_slli_epi64(t, 1));
		let (a, b, c, d) = (_mm256_extract_epi64(x, 0) as u64, _mm256_extract_epi64(x, 1) as u64, _mm256_extract_epi64(x, 2) as u64, _mm256_extract_epi64(x, 3) as u64);
		let h = (a ^ n as u64).wrapping_mul(0x9E3779B97F4A7C15)
			^ b.wrapping_mul(0xC2B2AE3D27D4EB4F).rotate_left(21)
			^ (c.wrapping_mul(0x165667B19E3779F9) ^ d.wrapping_mul(0xD6E8FEB86659FD93)).rotate_left(42);
		h ^ (h >> 31)
	}
	// the n bytes at p equal the zero-padded bytes at s
	#[inline(always)]
	pub unsafe fn eq(p: *const u8, n: usize, s: *const u8) -> bool {
		let mut k = 0;
		while k < n.max(64) {
			let m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(block(p.add(k), n.saturating_sub(k)), _mm256_loadu_si256(s.add(k) as _)))
				& _mm256_movemask_epi8(_mm256_cmpeq_epi8(block(p.add(k + 32), n.saturating_sub(k + 32)), _mm256_loadu_si256(s.add(k + 32) as _)));
			if m != -1 {
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

#[cfg(not(target_feature = "avx2"))]
mod v {
	pub unsafe fn mask(p: *const u8, c: u8) -> u32 { (0..32).fold(0, |m, i| m | ((*p.add(i) == c) as u32) << i) }
	pub unsafe fn hash(p: *const u8, n: usize) -> u64 { (0..n).fold(n as u64, |h, i| (h ^ *p.add(i) as u64).wrapping_mul(0x100000001B3)).wrapping_mul(0x9E3779B97F4A7C15) }
	pub unsafe fn eq(p: *const u8, n: usize, s: *const u8) -> bool { std::slice::from_raw_parts(p, n) == std::slice::from_raw_parts(s, n) }
	pub unsafe fn lastsp(p: *const u8) -> usize { (0..16).rfind(|&i| *p.add(i) == b' ').unwrap() }
	pub unsafe fn lat(p: *const u8, n: usize) -> u32 { (16 - n..16).fold(0, |v, k| v * 10 + (*p.add(k) - b'0') as u32) }
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

// f(line start, newline) for each line starting in [s, end), walking a 64-byte newline bitmask; returns the next line start
#[inline(always)]
unsafe fn lines(d: *const u8, mut s: usize, end: usize, mut f: impl FnMut(usize, usize)) -> usize {
	let mut b = s;
	while s < end {
		let mut m = v::mask(d.add(b), 10) as u64 | (v::mask(d.add(b + 32), 10) as u64) << 32;
		while m != 0 {
			let nl = b + m.trailing_zeros() as usize;
			m &= m - 1;
			f(s, nl);
			s = nl + 1;
			if s >= end {
				return s;
			}
		}
		b += 64;
	}
	s
}

// pass 1: intern endpoints, count lines and errors per (hour, endpoint)
unsafe fn pass1(d: *const u8, p: usize, end: usize, l: &mut Loc, tb: &Tab) -> usize {
	lines(d, p, end, |p, nl| {
		let h = hour(d.add(p));
		let es = ep_start(d, p);
		let sp = nl - 16 + v::lastsp(d.add(nl - 16)); // the space before the latency
		let n = sp - 4 - es;
		let id = find(tb, d.add(es), n, v::hash(d.add(es), n), true);
		*l.cnt.get_unchecked_mut(h * NE + id) += 1 | ((*d.add(sp - 3) == b'5') as u64) << 32;
		l.hm |= 1 << h;
	})
}

// pass 2: find the endpoint again (read-only table) and scatter the latency into its group's slot
unsafe fn pass2(d: *const u8, p: usize, end: usize, l: &mut Loc, tb: &Tab, lat: *mut u32) -> usize {
	let dup = tb.dup.load(Relaxed);
	lines(d, p, end, |p, nl| {
		let es = ep_start(d, p);
		let ls = v::lastsp(d.add(nl - 16));
		let n = nl - 20 + ls - es;
		let id = find(tb, d.add(es), n, v::hash(d.add(es), n), dup);
		let c = l.cnt.get_unchecked_mut(hour(d.add(p)) * NE + id);
		*lat.add(*c as usize) = v::lat(d.add(nl - 16), 15 - ls);
		*c += 1;
	})
}

// run f over the lines in [x, y); lines starting within PAD of the end of buf run on a padded copy
fn chunk(buf: &[u8], x: usize, y: usize, mut f: impl FnMut(*const u8, usize, usize) -> usize) {
	let p = f(buf.as_ptr(), x, y.min(buf.len().saturating_sub(PAD)));
	if p < y {
		let mut t = buf[p..y].to_vec();
		t.resize(y - p + PAD, 0);
		f(t.as_ptr(), 0, y - p);
	}
}

fn digits(v: u64) -> usize { (v.max(1).ilog10() + 1) as usize }

const LUT: [u8; 200] = {
	let mut t = [0u8; 200];
	let mut i = 0;
	while i < 200 {
		t[i] = b'0' + if i % 2 == 0 { i / 20 } else { i / 2 % 10 } as u8;
		i += 1;
	}
	t
};
// append " <v>"
fn num(o: &mut Vec<u8>, mut v: u64) {
	let (mut t, mut i) = ([b' '; 22], 22);
	while v >= 10 {
		let r = (v % 100) as usize * 2;
		i -= 2;
		t[i..i + 2].copy_from_slice(&LUT[r..r + 2]);
		v /= 100;
	}
	if i == 22 || v > 0 {
		i -= 1;
		t[i] = b'0' + v as u8;
	}
	o.extend_from_slice(&t[i - 1..]);
}

#[cfg(target_os = "linux")]
extern "C" {
	fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
	fn madvise(a: *const u8, l: usize, adv: i32) -> i32;
}

fn load(path: String) -> &'static [u8] {
	#[cfg(target_os = "linux")]
	unsafe {
		let f = File::open(path).unwrap();
		let n = f.metadata().unwrap().len() as usize;
		if n == 0 {
			return &[];
		}
		return std::slice::from_raw_parts(mmap(std::ptr::null_mut(), n, 1, 2, std::os::fd::AsRawFd::as_raw_fd(&f), 0), n);
	}
	#[cfg(not(target_os = "linux"))]
	Box::leak(std::fs::read(path).unwrap().into_boxed_slice())
}

// the output file sized to n bytes and mapped for writing; else a buffer that ofin writes out
static BUFD: AtomicBool = AtomicBool::new(false);
fn omap(f: &File, n: usize) -> *mut u8 {
	#[cfg(target_os = "linux")]
	unsafe {
		if n > 0 && f.set_len(n as u64).is_ok() {
			let p = mmap(std::ptr::null_mut(), n, 3, 1, std::os::fd::AsRawFd::as_raw_fd(f), 0);
			if p as isize != -1 {
				return p;
			}
			f.set_len(0).unwrap();
		}
	}
	let _ = f;
	BUFD.store(true, Relaxed);
	Box::leak(vec![0u8; n].into_boxed_slice()).as_mut_ptr()
}
fn ofin(mut f: &File, p: *mut u8, n: usize) {
	if BUFD.load(Relaxed) {
		std::io::Write::write_all(&mut f, unsafe { std::slice::from_raw_parts(p, n) }).unwrap();
	}
}

struct Cx {
	buf: &'static [u8],
	file: File,
	nt: usize,
	b: Vec<usize>,     // chunk bounds, at line starts
	locs: Vec<S<Loc>>, // per thread
	bar: Barrier,
	tb: Tab,
	ord: S<Vec<usize>>,           // rank -> endpoint id
	names: S<Vec<&'static [u8]>>, // rank -> endpoint
	goff: S<Vec<u32>>,            // group g = hour*E + rank -> first slot in lat; output order
	gerr: S<Vec<u32>>,
	st: S<Vec<[u32; 3]>>, // percentiles per group
	lat: S<Vec<u32>>,
	om: S<*mut u8>,
	tot: Vec<AU>, // latencies per thread's hour range
	hl: Vec<AU>,  // output bytes per hour, then offsets
	nx: [AU; 2],  // next hour to hand out
}
unsafe impl Sync for Cx {}

unsafe fn work(t: usize, c: &Cx) {
	let (buf, nt, tb) = (c.buf, c.nt, &c.tb);
	let (x, y) = (c.b[t], c.b[t + 1]);
	let l = c.locs[t].g();
	l.cnt = vec![0; NE * H];
	#[cfg(target_os = "linux")]
	madvise(buf.as_ptr().add(x & !4095), y - (x & !4095), 22); // MADV_POPULATE_READ, page tables in parallel
	chunk(buf, x, y, |d, p, e| pass1(d, p, e, l, tb));
	c.bar.wait();
	if t == 0 {
		// endpoints in byte order
		let e = tb.nid.load(Relaxed);
		let nm = |g: usize| {
			let s = tb.gs[g].load(Relaxed) as usize;
			&(&(*tb.ent.add(s)).0)[..(tb.slot[s].load(Relaxed) >> 16 & 255) as usize]
		};
		let o = c.ord.g();
		*o = (0..e).collect();
		o.sort_unstable_by(|&p, &q| nm(p).cmp(nm(q)));
		*c.names.g() = o.iter().map(|&g| nm(g)).collect();
		*c.goff.g() = vec![0; e * H + 1];
		*c.gerr.g() = vec![0; e * H];
		*c.st.g() = vec![[0; 3]; e * H];
	}
	c.bar.wait();
	// group offsets, two-phase prefix sum; this thread owns hours [h0, h1)
	let e = c.names.g().len();
	let (h0, h1) = (t * H / nt, (t + 1) * H / nt);
	let act = |h: usize| (0..nt).filter(move |&u| c.locs[u].g().hm >> h & 1 != 0);
	let sum = (h0..h1).flat_map(|h| act(h).flat_map(move |u| c.locs[u].g().cnt[h * NE..h * NE + e].iter())).map(|&v| v as u32 as usize).sum();
	c.tot[t].store(sum, Relaxed);
	c.bar.wait();
	let mut run = (0..t).map(|u| c.tot[u].load(Relaxed)).sum::<usize>() as u64;
	let (go, ge, od) = (c.goff.g(), c.gerr.g(), &c.ord.g()[..]);
	for h in h0..h1 {
		let us: Vec<usize> = act(h).collect();
		for r in 0..e {
			go[h * e + r] = run as u32;
			let mut er = 0;
			for &u in &us {
				let v = c.locs[u].g().cnt.get_unchecked_mut(h * NE + od[r]);
				er += *v >> 32;
				(*v, run) = (run, run + (*v as u32 as u64));
			}
			ge[h * e + r] = er as u32;
		}
	}
	if t == nt - 1 {
		go[e * H] = run as u32;
		let lv = c.lat.g();
		*lv = Vec::with_capacity(run as usize);
		lv.set_len(run as usize);
	}
	c.bar.wait();
	let lp = c.lat.g().as_mut_ptr();
	let l = c.locs[t].g();
	chunk(buf, x, y, |d, p, e| pass2(d, p, e, l, tb, lp));
	c.bar.wait();
	// exact nearest-rank percentiles per group, and output bytes per hour
	let (go, ge, nm, st) = (&c.goff.g()[..], &c.gerr.g()[..], &c.names.g()[..], c.st.g());
	loop {
		let h = c.nx[0].fetch_add(1, Relaxed);
		if h >= H {
			break;
		}
		let mut bytes = 0;
		for r in 0..e {
			let g = h * e + r;
			let (x, n) = (go[g] as usize, (go[g + 1] - go[g]) as usize);
			if n == 0 {
				continue;
			}
			let v = std::slice::from_raw_parts_mut(lp.add(x), n);
			let (r50, r95, r99) = ((50 * n + 99) / 100, (95 * n + 99) / 100, (99 * n + 99) / 100);
			let ps = if n <= 24 {
				v.sort_unstable();
				[v[r50 - 1], v[r95 - 1], v[r99 - 1]]
			} else {
				let p50 = *v.select_nth_unstable(r50 - 1).1;
				let p95 = *v[r50..].select_nth_unstable(r95 - 1 - r50).1;
				[p50, p95, *v[r95..].select_nth_unstable(r99 - 1 - r95).1]
			};
			st[g] = ps;
			let er = ge[g] as u64;
			bytes += 28 + nm[r].len() + digits(n as u64) + ps.iter().map(|&p| digits(p as u64)).sum::<usize>() + digits(er) + digits(er * 1_000_000 / n as u64);
		}
		c.hl[h + 1].store(bytes, Relaxed);
	}
	c.bar.wait();
	if t == 0 {
		*c.lat.g() = Vec::new();
		for h in 0..H {
			c.hl[h + 1].fetch_add(c.hl[h].load(Relaxed), Relaxed);
		}
		*c.om.g() = omap(&c.file, c.hl[H].load(Relaxed));
	}
	c.bar.wait();
	// format each hour straight into the output at its offset
	let mut o = Vec::with_capacity(1 << 18);
	loop {
		let h = c.nx[1].fetch_add(1, Relaxed);
		if h >= H {
			break;
		}
		o.clear();
		let ab = h + 18;
		let mut hs = *b"2026-10-30T00:00:00Z ";
		hs[..10].copy_from_slice([b"2026-10-30", b"2026-10-31", b"2026-11-01", b"2026-11-02"][ab / 24]);
		hs[11] = b'0' + (ab % 24 / 10) as u8;
		hs[12] = b'0' + (ab % 24 % 10) as u8;
		for r in 0..e {
			let g = h * e + r;
			let n = (go[g + 1] - go[g]) as u64;
			if n == 0 {
				continue;
			}
			let er = ge[g] as u64;
			o.extend_from_slice(&hs);
			o.extend_from_slice(nm[r]);
			for v in [n, st[g][0] as u64, st[g][1] as u64, st[g][2] as u64, er, er * 1_000_000 / n] {
				num(&mut o, v);
			}
			o.push(b'\n');
		}
		std::ptr::copy_nonoverlapping(o.as_ptr(), c.om.g().add(c.hl[h].load(Relaxed)), o.len());
	}
	c.bar.wait();
	if t == 0 {
		ofin(&c.file, *c.om.g(), c.hl[H].load(Relaxed));
	}
}

fn main() {
	let (mut data, mut out) = ("/data".to_string(), "/out".to_string());
	let a: Vec<String> = std::env::args().collect();
	for i in 1..a.len() - 1 {
		match a[i].as_str() {
			"--data" => data = a[i + 1].clone(),
			"--out" => out = a[i + 1].clone(),
			_ => {}
		}
	}
	let buf = load(data + "/requests.log");
	let file = File::options().read(true).write(true).create(true).truncate(true).open(out + "/summary.txt").unwrap();
	let nt = std::thread::available_parallelism().map_or(1, |n| n.get());
	let mut b = vec![0; nt + 1];
	b[nt] = buf.len();
	for i in 1..nt {
		let mut x = (buf.len() * i / nt).max(b[i - 1]);
		while x > 0 && x < buf.len() && buf[x - 1] != b'\n' {
			x += 1;
		}
		b[i] = x;
	}
	let c = Cx {
		buf,
		file,
		nt,
		b,
		locs: (0..nt).map(|_| s(Loc::default())).collect(),
		bar: Barrier::new(nt),
		tb: Tab {
			slot: (0..TS).map(|_| AtomicU64::new(0)).collect(),
			ent: unsafe { std::alloc::alloc_zeroed(std::alloc::Layout::from_size_align(TS * 128, 64).unwrap()) as *mut Ent },
			nid: AU::new(0),
			gs: (0..NE).map(|_| AtomicU32::new(0)).collect(),
			hs: (0..TS).map(|_| AtomicU64::new(0)).collect(),
			dup: AtomicBool::new(false),
		},
		ord: s(vec![]),
		names: s(vec![]),
		goff: s(vec![]),
		gerr: s(vec![]),
		st: s(vec![]),
		lat: s(vec![]),
		om: s(std::ptr::null_mut()),
		tot: (0..nt).map(|_| AU::new(0)).collect(),
		hl: (0..=H).map(|_| AU::new(0)).collect(),
		nx: [AU::new(0), AU::new(0)],
	};
	let c = &c;
	std::thread::scope(|sc| {
		for t in 0..nt {
			sc.spawn(move || unsafe { work(t, c) });
		}
	});
}
