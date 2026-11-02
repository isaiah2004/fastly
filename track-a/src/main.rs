// Speed Demon 2026, Track A (Ingest). std only, Linux, AVX2 (built for x86-64-v3).
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize as AU, Ordering::*};
use std::{fs::File, os::unix::fs::FileExt, sync::Barrier, sync::Mutex};

const H: usize = 72; // hours in the fixed window
const NE: usize = 2048; // max distinct endpoints (the catalogue has 1,842)
const TS: usize = 4096; // intern table slots
const CS: usize = 64 << 10; // input chunk, read into a buffer that stays in L2
const PAD: usize = 512; // read past a chunk: its last line (under 200 bytes) and the SIMD reads past a line start
const BLK: usize = 512; // records per block
const ESC: u32 = 0xfffff; // latency field value: the latency is in the next record
const FILL: u32 = u32::MAX; // unused tail of a block
const OC: usize = NE * 256; // output bytes per hour, at most: a line per endpoint

// A record per line: id<<21 | error<<20 | latency, or ESC and then the latency. Threads append them to blocks of
// BLK records in the hour's region; FILL ends a block early. Loc: per hour, a thread's write position and block end.
type Loc = [(u32, u32); H];

// endpoint intern table shared by all threads: slot = hash<<32 | len<<16 | id+1; bytes in ent[slot]
#[repr(align(64))]
struct Ent([u8; 128]);
struct Tab {
	slot: Vec<AtomicU64>,
	ent: *mut Ent,
	nid: AU,
	gs: Vec<AtomicU32>, // id -> slot
}


// endpoint id: probe from the hash, insert if new
#[inline(always)]
unsafe fn find(tb: &Tab, e: *const u8, n: usize, hv: u64) -> usize {
	let tag = hv << 32 | (n as u64) << 16;
	let mut s = hv as usize & (TS - 1);
	loop {
		let sl = tb.slot.get_unchecked(s);
		let t = sl.load(Acquire);
		if t >> 16 == tag >> 16 {
			if v::eq(e, n, (*tb.ent.add(s)).0.as_ptr()) {
				return (t & 0xffff) as usize - 1;
			}
		} else if t <= 1 {
			// empty, or another thread is inserting here
			if t == 0 && sl.compare_exchange(0, 1, Acquire, Relaxed).is_ok() {
				std::ptr::copy_nonoverlapping(e, (*tb.ent.add(s)).0.as_mut_ptr(), n);
				let id = tb.nid.fetch_add(1, Relaxed);
				assert!(id < NE - 1);
				tb.gs[id].store(s as u32, Relaxed);
				sl.store(tag | (id as u64 + 1), Release);
				return id;
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
	// 128 ones, then 128 zeros: from 128-n, the mask of the first n bytes
	static MK: [u8; 256] = {
		let mut m = [0u8; 256];
		let mut i = 0;
		while i < 128 {
			m[i] = 255;
			i += 1;
		}
		m
	};
	// the n bytes (at most 128) at p equal the n bytes at s
	#[inline(always)]
	pub unsafe fn eq(p: *const u8, n: usize, s: *const u8) -> bool {
		let m = MK.as_ptr().add(128 - n);
		let d = |o: usize| _mm256_testz_si256(_mm256_xor_si256(_mm256_loadu_si256(p.add(o) as _), _mm256_loadu_si256(s.add(o) as _)), _mm256_loadu_si256(m.add(o) as _));
		d(0) & d(32) != 0 && (n <= 64 || d(64) & d(96) != 0)
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

// 32-bit CRC of the n endpoint bytes at p: its first, middle and last 8 (find compares the bytes)
#[inline(always)]
unsafe fn hash(p: *const u8, n: usize) -> u64 {
	use std::arch::x86_64::_mm_crc32_u64 as crc;
	let (m, o) = (!0u64 >> (64 - 8 * n.min(8)), n.max(8) - 8);
	let r = |o: usize| (p.add(o) as *const u64).read_unaligned() & m;
	crc(crc(crc(n as u64, r(0)), r(o / 2)), r(o))
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

// f(line start, newline) for each line starting in [s, end). Skip the fixed minimum prefix.
#[inline(always)]
unsafe fn lines(d: *const u8, mut s: usize, end: usize, mut f: impl FnMut(usize, usize)) {
	while s < end {
		let mut b = s + 39; // the shortest legal line has its LF at byte 39
		let nl = loop {
			let m = v::mask(d.add(b), 10);
			if m != 0 { break b + m.trailing_zeros() as usize; }
			b += 32;
		};
		f(s, nl);
		s = nl + 1;
	}
}

// a new block for hour h
#[cold]
unsafe fn nb(h: usize, c: (u32, u32), cx: &Cx) -> (u32, u32) {
	if c.0 < c.1 {
		*cx.rec.add(c.0 as usize) = FILL;
	}
	let s = cx.top[h].fetch_add(BLK, Relaxed);
	assert!(s + BLK <= cx.cap);
	let s = h * cx.cap + s;
	(s as u32, (s + BLK) as u32)
}

// the single pass: intern the endpoint, append the line's record to its hour
unsafe fn pass(d: *const u8, p: usize, end: usize, l: &mut Loc, cx: &Cx) {
	let (r, tb, mut hk, mut h) = (cx.rec, &cx.tb, 0, 0);
	lines(d, p, end, |p, nl| {
		// the hour of the line before, unless the month, day or hour bytes differ
		let k = (d.add(p + 5) as *const u64).read_unaligned();
		if k != hk {
			(hk, h) = (k, hour(d.add(p)));
		}
		let es = ep_start(d, p);
		let ls = v::lastsp(d.add(nl - 16));
		let sp = nl - 16 + ls; // the space before the latency
		let n = sp - 4 - es;
		let id = find(tb, d.add(es), n, hash(d.add(es), n)) as u32;
		let lat = v::lat(d.add(nl - 16), 15 - ls);
		let mut c = *l.get_unchecked(h);
		if c.0 + 2 > c.1 {
			c = nb(h, c, cx);
		}
		*r.add(c.0 as usize) = id << 21 | ((*d.add(sp - 3) == b'5') as u32) << 20 | lat.min(ESC);
		*r.add(c.0 as usize + 1) = lat;
		c.0 += 1 + (lat >= ESC) as u32;
		*l.get_unchecked_mut(h) = c;
	})
}

// f(record, latency) for every line of hour h
unsafe fn each(c: &Cx, h: usize, mut f: impl FnMut(u32, u32)) {
	let (mut k, r) = (h * c.cap, c.rec);
	let e = k + c.top[h].load(Relaxed);
	while k < e {
		let x = *r.add(k);
		if x == FILL {
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
	bar: Barrier,
	tb: Tab,
	rec: *mut u32, // H regions of cap records
	val: *mut u32, // per thread: cap latencies, an hour's in endpoint order
	txt: *mut u8,  // per hour: OC bytes of output
	cap: usize,
	top: Vec<AU>, // records used per region
	nx: [AU; 2],                  // next hour, next input chunk
	out: Mutex<(usize, usize, [usize; H])>, // next hour to place, its offset, formatted lengths (MAX: not yet)
}
unsafe impl Sync for Cx {}

unsafe fn work(t: usize, c: &Cx) {
	let (len, tb) = (c.len, &c.tb);
	let l = &mut [(0, 0); H];
	// chunks of CS bytes handed out dynamically; a chunk owns the lines that start in it. It is read from the
	// byte before it to PAD past it.
	let mut w = vec![0u8; CS + 2 * PAD];
	loop {
		let a = c.nx[1].fetch_add(CS, Relaxed);
		if a >= len {
			break;
		}
		let o = a.saturating_sub(1);
		let k = (a + CS + PAD).min(len) - o;
		c.inp.read_exact_at(&mut w[..k], o as u64).unwrap();
		let x = (a > 0) as usize * (w.iter().position(|&b| b == b'\n').unwrap() + 1);
		pass(w.as_ptr(), x, (a + CS).min(len) - o, l, c);
	}
	for &(p, e) in l.iter() {
		if p < e {
			*c.rec.add(p as usize) = FILL;
		}
	}
	c.bar.wait();
	// endpoint ids in byte order (each thread sorts its own copy)
	let nm = |g: usize| {
		let s = tb.gs[g].load(Relaxed) as usize;
		&(&(*tb.ent.add(s)).0)[..(tb.slot[s].load(Relaxed) >> 16 & 255) as usize]
	};
	let mut od: Vec<usize> = (0..tb.nid.load(Relaxed)).collect();
	od.sort_unstable_by(|&p, &q| nm(p).cmp(nm(q)));
	// per hour: count, scatter by endpoint rank, release the records, exact nearest-rank percentiles, format, write
	let (mut cnt, mut off) = (vec![0u64; NE], vec![0u32; NE]); // cnt: count | errors<<32
	loop {
		let h = c.nx[0].fetch_add(1, Relaxed);
		if h >= H {
			break;
		}
		cnt.fill(0);
		each(c, h, |r, _| {
			*cnt.get_unchecked_mut((r >> 21) as usize) += 1 | ((r >> 20 & 1) as u64) << 32;
		});
		let mut tot = 0;
		for &id in &od {
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
		for &id in &od {
			let (n, e) = (cnt[id] as u32 as usize, (cnt[id] >> 32) as u32);
			if n == 0 {
				continue;
			}
			let v = &mut v[off[id] as usize - n..off[id] as usize];
			let r = |p: usize| (p * n + 99) / 100;
			let (r50, r95, r99) = (r(50), r(95), r(99));
			let ps = if n <= 20 {
				v.sort_unstable();
				[v[r50 - 1], v[r95 - 1], v[r99 - 1]]
			} else {
				let p50 = *v.select_nth_unstable(r50 - 1).1;
				let p95 = *v[r50..].select_nth_unstable(r95 - 1 - r50).1;
				[p50, p95, *v[r95..].select_nth_unstable(r99 - 1 - r95).1]
			};
			for b in [&hs[..], nm(id)] {
				std::ptr::copy_nonoverlapping(b.as_ptr(), q, b.len());
				q = q.add(b.len());
			}
			let pm = (e as u64 * 1_000_000 / n as u64) as u32;
			for x in [n as u32, ps[0], ps[1], ps[2], e, pm] {
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
			let b = std::slice::from_raw_parts(c.txt.add(k * OC), n);
			c.file.write_all_at(b, at as u64).unwrap();
			free(b.as_ptr(), n);
		}
	}
}

fn main() {
	// M_ARENA_MAX = 1: no per-thread malloc arenas, whose mapping takes the address-space lock and stalls faulting threads
	unsafe { mallopt(-8, 1) };
	let a: Vec<String> = std::env::args().collect();
	let arg = |k: &str, d: &str| a.iter().position(|x| x == k).map_or(d.to_string(), |i| a[i + 1].clone());
	let inp = File::open(arg("--data", "/data") + "/requests.log").unwrap();
	let len = inp.metadata().unwrap().len() as usize;
	let file = File::create(arg("--out", "/out") + "/summary.txt").unwrap();
	let nt = std::thread::available_parallelism().map_or(1, |n| n.get());
	let cap = (2 * (len / 39 + nt) / (BLK - 1) + (H + 1) * nt + 1) / 2 * 2 * BLK; // records, at most 2 per line, plus partial blocks; whole pages
	let c = Cx {
		inp,
		len,
		file,
		bar: Barrier::new(nt),
		tb: Tab {
			slot: (0..TS).map(|_| AtomicU64::new(0)).collect(),
			ent: arena(TS * 128) as _,
			nid: AU::new(0),
			gs: (0..NE).map(|_| AtomicU32::new(0)).collect(),
		},
		rec: arena(H * cap * 4) as _,
		val: arena(nt * cap * 4) as _,
		txt: arena(H * OC),
		cap,
		top: (0..H).map(|_| AU::new(0)).collect(),
		nx: [AU::new(0), AU::new(0)],
		out: Mutex::new((0, 0, [usize::MAX; H])),
	};
	let c = &c;
	std::thread::scope(|sc| {
		for t in 1..nt {
			sc.spawn(move || unsafe { work(t, c) });
		}
		unsafe { work(0, c) };
	});
}
