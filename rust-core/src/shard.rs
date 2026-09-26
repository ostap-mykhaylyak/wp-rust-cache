//! One shard: hash index, LRU list, TinyLFU sketch and a TLSF allocator over
//! the shard's heap. A `Ctx` exists only while the shard lock is held.
//!
//! Every offset read from shared memory is checked before it is followed and
//! every list walk is bounded: corrupted data yields `Err(Corrupt)`, which
//! the caller turns into a shard reset, never into a wild write or a hang.

use crate::layout::*;
use crate::value::{self, Number};
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

#[derive(Debug)]
pub struct Corrupt;
type R<T> = Result<T, Corrupt>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetMode {
    Set,
    Add,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOutcome {
    Stored,
    /// `add` on an existing key.
    Exists,
    /// `replace` on a missing key.
    Missing,
    /// Larger than `max_item_size`; any older copy was deleted.
    TooLarge,
    /// Nothing left to evict (the value is larger than the free heap).
    NoMemory,
    /// TinyLFU admission refused a new key colder than the eviction victim.
    Rejected,
}

// TLSF block flags, in the low bits of the size word.
const FREE: u32 = 1;
const PREV_FREE: u32 = 2;
const MIN_BLOCK: u32 = 32;
const SKETCH_ROWS: u32 = 4;

#[inline]
fn inc(c: &AtomicU64) {
    c.store(c.load(Relaxed).wrapping_add(1), Relaxed);
}
#[inline]
fn add(c: &AtomicU64, v: u64) {
    c.store(c.load(Relaxed).wrapping_add(v), Relaxed);
}
#[inline]
fn sub(c: &AtomicU64, v: u64) {
    c.store(c.load(Relaxed).saturating_sub(v), Relaxed);
}

/// Computes the static part of a shard's state for a shard of `shard_size`
/// bytes (buckets, sketch and heap placement, limits).
/// Immutable placement of a shard's regions and its limits. Computed by every
/// process from the validated header and never read back from shared memory,
/// so corrupted shared data cannot move the bounds that checks rely on.
#[derive(Debug, Clone, Copy)]
pub struct Plan {
    pub bucket_mask: u32,
    pub buckets_offset: u32,
    pub sketch_offset: u32,
    pub sketch_mask: u32,
    pub heap_offset: u32,
    pub heap_end: u32,
    pub max_item: u32,
    pub soft_limit: u32,
}

pub fn plan(shard_size: u32, tinylfu: bool, max_item: u64, soft_pct: u32) -> Plan {
    let mut st = Plan {
        bucket_mask: 0,
        buckets_offset: 0,
        sketch_offset: 0,
        sketch_mask: 0,
        heap_offset: 0,
        heap_end: 0,
        max_item: 0,
        soft_limit: 0,
    };
    let align = |v: u32| (v + 63) & !63;
    let wanted = (shard_size / 256).max(64);
    let buckets = 1u32 << (31 - wanted.leading_zeros());
    st.bucket_mask = buckets - 1;
    st.buckets_offset = align(SHARD_HEADER_SIZE as u32);
    let mut off = align(st.buckets_offset + buckets * 4);
    if tinylfu {
        st.sketch_offset = off;
        st.sketch_mask = buckets - 1;
        off = align(off + buckets * SKETCH_ROWS);
    } else {
        st.sketch_offset = 0;
        st.sketch_mask = 0;
    }
    st.heap_offset = off;
    st.heap_end = shard_size;
    let heap = (st.heap_end - st.heap_offset) as u64;
    st.max_item = max_item.min(heap / 2) as u32;
    st.soft_limit = (heap * soft_pct as u64 / 100) as u32;
    st
}

pub struct Ctx<'a> {
    /// Start of the shard (its `ShardHeader`).
    pub base: *mut u8,
    pub stats: &'a ShardStats,
    pub st: &'a mut ShardState,
    pub plan: Plan,
    pub tinylfu: bool,
}

pub struct Found {
    pub tag: u8,
    pub expires: u32,
    pub alloc: u32,
}

pub struct Walked<'k> {
    pub key: &'k [u8],
    pub alloc: u32,
}

impl Ctx<'_> {
    // ---- raw access (callers validate offsets first) ---------------------

    #[inline]
    fn ptr(&self, off: u32) -> *mut u8 {
        // SAFETY: offsets are validated against heap bounds (or are fixed
        // header/bucket offsets computed by `plan`) before being used.
        unsafe { self.base.add(off as usize) }
    }
    #[inline]
    fn r32(&self, off: u32) -> u32 {
        // SAFETY: see `ptr`; all u32 fields sit at 4-aligned offsets.
        unsafe { (self.ptr(off) as *const u32).read() }
    }
    #[inline]
    fn w32(&mut self, off: u32, v: u32) {
        // SAFETY: see `ptr`; we hold the shard lock.
        unsafe { (self.ptr(off) as *mut u32).write(v) }
    }
    #[inline]
    fn r64(&self, off: u32) -> u64 {
        // SAFETY: entry hashes sit at block+8, and blocks are 8-aligned.
        unsafe { (self.ptr(off) as *const u64).read() }
    }
    #[inline]
    fn w64(&mut self, off: u32, v: u64) {
        // SAFETY: as `r64`; we hold the shard lock.
        unsafe { (self.ptr(off) as *mut u64).write(v) }
    }
    #[inline]
    fn bytes(&self, off: u32, len: u32) -> &[u8] {
        // SAFETY: callers pass ranges validated by `dims`.
        unsafe { std::slice::from_raw_parts(self.ptr(off), len as usize) }
    }
    #[inline]
    fn put(&mut self, off: u32, src: &[u8]) {
        // SAFETY: destination validated by the caller; `src` never aliases
        // the destination (it is caller memory or a disjoint entry).
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr(off), src.len()) }
    }

    /// A block that can hold an entry header.
    #[inline]
    fn check(&self, b: u32) -> R<()> {
        if b < self.plan.heap_offset
            || b % 8 != 0
            || b as u64 + E_DATA as u64 > self.plan.heap_end as u64
        {
            return Err(Corrupt);
        }
        Ok(())
    }

    /// Any physical block position, including the end sentinel.
    #[inline]
    fn check_phys(&self, b: u32) -> R<()> {
        if b < self.plan.heap_offset || b % 8 != 0 || b > self.plan.heap_end - 8 {
            return Err(Corrupt);
        }
        Ok(())
    }

    /// Key length, value length, block size â€” validated against the block.
    #[inline]
    fn dims(&self, e: u32) -> R<(u32, u32, u32)> {
        self.check(e)?;
        let size = self.bsize(e);
        let klen = self.r32(e + E_KLEN);
        let vlen = self.r32(e + E_VLEN);
        if size < MIN_BLOCK
            || e as u64 + size as u64 > self.plan.heap_end as u64
            || E_DATA as u64 + klen as u64 + vlen as u64 > size as u64
        {
            return Err(Corrupt);
        }
        Ok((klen, vlen, size))
    }

    #[inline]
    fn tag(&self, e: u32) -> u8 {
        // SAFETY: e was validated by `dims`.
        unsafe { self.ptr(e + E_TAG).read() }
    }

    #[inline]
    fn bucket_off(&self, hash: u64) -> u32 {
        self.plan.buckets_offset + ((hash as u32) & self.plan.bucket_mask) * 4
    }

    // ---- reset -----------------------------------------------------------

    /// Empties the shard. Always a correct state for a cache.
    pub fn reset(&mut self) {
        let buckets = (self.plan.bucket_mask + 1) as usize * 4;
        // SAFETY: bucket and sketch regions were laid out by `plan` inside
        // the shard.
        unsafe {
            std::ptr::write_bytes(self.ptr(self.plan.buckets_offset), 0, buckets);
            if self.plan.sketch_offset != 0 {
                let len = (self.plan.sketch_mask + 1) as usize * SKETCH_ROWS as usize;
                std::ptr::write_bytes(self.ptr(self.plan.sketch_offset), 0, len);
            }
        }
        self.st.lru_head = 0;
        self.st.lru_tail = 0;
        self.st.sketch_ops = 0;
        self.tlsf_init();
        self.stats.entries.store(0, Relaxed);
        self.stats.payload_bytes.store(0, Relaxed);
        self.stats.alloc_bytes.store(0, Relaxed);
        self.st.dirty = 0;
    }

    // ---- TLSF ------------------------------------------------------------

    #[inline]
    fn bsize(&self, b: u32) -> u32 {
        self.r32(b + 4) & !3
    }
    #[inline]
    fn bflags(&self, b: u32) -> u32 {
        self.r32(b + 4) & 3
    }
    #[inline]
    fn set_bsize(&mut self, b: u32, size: u32) {
        let f = self.bflags(b);
        self.w32(b + 4, size | f);
    }
    #[inline]
    fn set_bflags(&mut self, b: u32, f: u32) {
        let s = self.bsize(b);
        self.w32(b + 4, s | f);
    }

    #[inline]
    fn mapping(size: u32) -> (usize, usize) {
        if size < (1 << FL_SHIFT) {
            (0, (size >> 3) as usize)
        } else {
            let f = 31 - size.leading_zeros();
            let sl = (size >> (f - SL_LOG2)) ^ (1 << SL_LOG2);
            ((f - (FL_SHIFT - 1)) as usize, sl as usize)
        }
    }

    /// Class whose every block is at least `size` bytes.
    #[inline]
    fn mapping_search(size: u32) -> Option<(usize, usize)> {
        let mut s = size as u64;
        if size >= (1 << FL_SHIFT) {
            let f = 63 - s.leading_zeros();
            s += (1u64 << (f - SL_LOG2)) - 1;
        }
        if s > u32::MAX as u64 {
            return None;
        }
        let (fl, sl) = Self::mapping(s as u32);
        (fl < FL_COUNT).then_some((fl, sl))
    }

    fn insert_free(&mut self, b: u32) -> R<()> {
        let (fl, sl) = Self::mapping(self.bsize(b));
        if fl >= FL_COUNT || sl >= SL_COUNT {
            return Err(Corrupt);
        }
        let head = self.st.free_heads[fl][sl];
        if head != 0 {
            self.check_phys(head)?;
        }
        self.w32(b + 8, head);
        self.w32(b + 12, 0);
        if head != 0 {
            self.w32(head + 12, b);
        }
        self.st.free_heads[fl][sl] = b;
        self.st.fl_bitmap |= 1 << fl;
        self.st.sl_bitmap[fl] |= 1 << sl;
        Ok(())
    }

    fn remove_free(&mut self, b: u32) -> R<()> {
        let (fl, sl) = Self::mapping(self.bsize(b));
        if fl >= FL_COUNT || sl >= SL_COUNT {
            return Err(Corrupt);
        }
        let next = self.r32(b + 8);
        let prev = self.r32(b + 12);
        if next != 0 {
            self.check_phys(next)?;
            self.w32(next + 12, prev);
        }
        if prev != 0 {
            self.check_phys(prev)?;
            self.w32(prev + 8, next);
        } else if self.st.free_heads[fl][sl] != b {
            return Err(Corrupt);
        }
        if self.st.free_heads[fl][sl] == b {
            self.st.free_heads[fl][sl] = next;
            if next == 0 {
                self.st.sl_bitmap[fl] &= !(1 << sl);
                if self.st.sl_bitmap[fl] == 0 {
                    self.st.fl_bitmap &= !(1 << fl);
                }
            }
        }
        Ok(())
    }

    fn tlsf_init(&mut self) {
        self.st.fl_bitmap = 0;
        self.st.sl_bitmap = [0; FL_COUNT];
        self.st.free_heads = [[0; SL_COUNT]; FL_COUNT];
        let start = self.plan.heap_offset;
        let sentinel = self.plan.heap_end - 8;
        self.w32(start, 0);
        self.w32(start + 4, (sentinel - start) | FREE);
        self.w32(sentinel, start);
        self.w32(sentinel + 4, PREV_FREE);
        // The free lists were just emptied: this cannot fail.
        let _ = self.insert_free(start);
    }

    /// Allocates a block with at least `payload` bytes after its header.
    fn alloc(&mut self, payload: usize) -> R<Option<u32>> {
        let size = ((payload as u64 + 8 + 7) & !7).max(MIN_BLOCK as u64);
        if size > u32::MAX as u64 {
            return Ok(None);
        }
        let size = size as u32;
        let Some((mut fl, sl)) = Self::mapping_search(size) else {
            return Ok(None);
        };
        let mut sl_map = self.st.sl_bitmap[fl] & (!0u32 << sl);
        if sl_map == 0 {
            let fl_map = if fl + 1 >= 32 {
                0
            } else {
                self.st.fl_bitmap & (!0u32 << (fl + 1))
            };
            if fl_map == 0 {
                return Ok(None);
            }
            fl = fl_map.trailing_zeros() as usize;
            if fl >= FL_COUNT {
                return Err(Corrupt);
            }
            sl_map = self.st.sl_bitmap[fl];
            if sl_map == 0 {
                return Err(Corrupt);
            }
        }
        let sl = sl_map.trailing_zeros() as usize;
        if sl >= SL_COUNT {
            return Err(Corrupt);
        }
        let b = self.st.free_heads[fl][sl];
        self.check_phys(b)?;
        if self.bflags(b) & FREE == 0 {
            return Err(Corrupt);
        }
        self.remove_free(b)?;
        let bs = self.bsize(b);
        if bs < size || b as u64 + bs as u64 > self.plan.heap_end as u64 - 8 {
            return Err(Corrupt);
        }
        if bs - size >= MIN_BLOCK {
            let rem = b + size;
            self.w32(rem, b);
            self.w32(rem + 4, (bs - size) | FREE);
            self.set_bsize(b, size);
            let next = rem + (bs - size);
            self.check_phys(next)?;
            self.w32(next, rem);
            self.insert_free(rem)?;
        } else {
            let next = b + bs;
            self.check_phys(next)?;
            let f = self.bflags(next) & !PREV_FREE;
            self.set_bflags(next, f);
        }
        let f = self.bflags(b) & !FREE;
        self.set_bflags(b, f);
        Ok(Some(b))
    }

    fn free(&mut self, b: u32) -> R<()> {
        self.check_phys(b)?;
        let mut b = b;
        let mut size = self.bsize(b);
        let f = self.bflags(b) | FREE;
        self.set_bflags(b, f);
        if self.bflags(b) & PREV_FREE != 0 {
            let p = self.r32(b);
            self.check_phys(p)?;
            if self.bflags(p) & FREE == 0 || p as u64 + self.bsize(p) as u64 != b as u64 {
                return Err(Corrupt);
            }
            self.remove_free(p)?;
            size += self.bsize(p);
            b = p;
            self.set_bsize(b, size);
        }
        let next = b.checked_add(size).ok_or(Corrupt)?;
        self.check_phys(next)?;
        if self.bflags(next) & FREE != 0 {
            self.remove_free(next)?;
            size = size.checked_add(self.bsize(next)).ok_or(Corrupt)?;
            self.set_bsize(b, size);
        }
        let next = b.checked_add(size).ok_or(Corrupt)?;
        self.check_phys(next)?;
        self.w32(next, b);
        let f = self.bflags(next) | PREV_FREE;
        self.set_bflags(next, f);
        self.insert_free(b)
    }

    // ---- LRU -------------------------------------------------------------

    fn lru_unlink(&mut self, e: u32) -> R<()> {
        let p = self.r32(e + E_LPREV);
        let n = self.r32(e + E_LNEXT);
        if p != 0 {
            self.check(p)?;
            self.w32(p + E_LNEXT, n);
        } else if self.st.lru_head == e {
            self.st.lru_head = n;
        } else {
            return Err(Corrupt);
        }
        if n != 0 {
            self.check(n)?;
            self.w32(n + E_LPREV, p);
        } else if self.st.lru_tail == e {
            self.st.lru_tail = p;
        } else {
            return Err(Corrupt);
        }
        Ok(())
    }

    fn lru_push(&mut self, e: u32) -> R<()> {
        let h = self.st.lru_head;
        if h != 0 {
            self.check(h)?;
        }
        self.w32(e + E_LPREV, 0);
        self.w32(e + E_LNEXT, h);
        if h != 0 {
            self.w32(h + E_LPREV, e);
        } else {
            self.st.lru_tail = e;
        }
        self.st.lru_head = e;
        Ok(())
    }

    #[inline]
    fn lru_touch(&mut self, e: u32) -> R<()> {
        if self.st.lru_head != e {
            self.lru_unlink(e)?;
            self.lru_push(e)?;
        }
        Ok(())
    }

    // ---- TinyLFU sketch --------------------------------------------------

    #[inline]
    fn sketch_slot(&self, hash: u64, row: u32) -> u32 {
        let h = (hash ^ (row as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
            .wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        let w = self.plan.sketch_mask + 1;
        self.plan.sketch_offset + row * w + (((h >> 32) as u32) & self.plan.sketch_mask)
    }

    fn sketch_incr(&mut self, hash: u64) {
        for row in 0..SKETCH_ROWS {
            let p = self.ptr(self.sketch_slot(hash, row));
            // SAFETY: slot is inside the sketch region laid out by `plan`.
            unsafe {
                if *p < 15 {
                    *p += 1;
                }
            }
        }
        self.st.sketch_ops += 1;
        let w = self.plan.sketch_mask + 1;
        if self.st.sketch_ops >= w.saturating_mul(10) {
            // Aging: halve every counter so old popularity fades.
            let len = (w * SKETCH_ROWS) as usize;
            // SAFETY: the whole sketch region belongs to this shard.
            let s =
                unsafe { std::slice::from_raw_parts_mut(self.ptr(self.plan.sketch_offset), len) };
            for c in s {
                *c >>= 1;
            }
            self.st.sketch_ops = 0;
        }
    }

    fn sketch_estimate(&self, hash: u64) -> u8 {
        (0..SKETCH_ROWS)
            // SAFETY: slot is inside the sketch region.
            .map(|row| unsafe { *self.ptr(self.sketch_slot(hash, row)) })
            .min()
            .unwrap_or(0)
    }

    // ---- index -----------------------------------------------------------

    fn find(&self, hash: u64, key: &[u8]) -> R<Option<u32>> {
        let mut cur = self.r32(self.bucket_off(hash));
        let limit = self.stats.entries.load(Relaxed) + 8;
        let mut steps = 0u64;
        while cur != 0 {
            self.check(cur)?;
            if self.r64(cur + E_HASH) == hash {
                let (klen, _, _) = self.dims(cur)?;
                if klen as usize == key.len() && self.bytes(cur + E_DATA, klen) == key {
                    return Ok(Some(cur));
                }
            }
            cur = self.r32(cur + E_NEXT);
            steps += 1;
            if steps > limit {
                return Err(Corrupt);
            }
        }
        Ok(None)
    }

    fn chain_unlink(&mut self, e: u32, hash: u64) -> R<()> {
        let bo = self.bucket_off(hash);
        let mut prev = 0;
        let mut cur = self.r32(bo);
        let limit = self.stats.entries.load(Relaxed) + 8;
        let mut steps = 0u64;
        while cur != 0 {
            if cur == e {
                let next = self.r32(e + E_NEXT);
                if prev == 0 {
                    self.w32(bo, next);
                } else {
                    self.w32(prev + E_NEXT, next);
                }
                return Ok(());
            }
            self.check(cur)?;
            prev = cur;
            cur = self.r32(cur + E_NEXT);
            steps += 1;
            if steps > limit {
                return Err(Corrupt);
            }
        }
        Err(Corrupt)
    }

    fn remove(&mut self, e: u32) -> R<()> {
        let (klen, vlen, size) = self.dims(e)?;
        let hash = self.r64(e + E_HASH);
        self.chain_unlink(e, hash)?;
        self.lru_unlink(e)?;
        sub(&self.stats.entries, 1);
        sub(&self.stats.payload_bytes, (klen + vlen) as u64);
        sub(&self.stats.alloc_bytes, size as u64);
        self.free(e)
    }

    #[inline]
    fn is_expired(&self, e: u32, now: u32) -> bool {
        let exp = self.r32(e + E_EXPIRES);
        exp != 0 && exp <= now
    }

    /// Frees up to two LRU-tail entries that are expired or stale (written
    /// under a generation that a flush has since bumped). This is the only
    /// "garbage collection": amortised over sets, no global sweep.
    fn sweep_tail(&mut self, now: u32, is_live: &dyn Fn(&[u8]) -> bool) -> R<()> {
        for _ in 0..2 {
            let v = self.st.lru_tail;
            if v == 0 {
                break;
            }
            let (klen, _, _) = self.dims(v)?;
            if self.is_expired(v, now) {
                self.remove(v)?;
                inc(&self.stats.expired);
            } else if !is_live(self.bytes(v + E_DATA, klen)) {
                self.remove(v)?;
                inc(&self.stats.stale);
            } else {
                break;
            }
        }
        Ok(())
    }

    fn write_entry(
        &mut self,
        b: u32,
        hash: u64,
        key: &[u8],
        tag: u8,
        val: &[u8],
        expires: u32,
    ) -> R<()> {
        self.w64(b + E_HASH, hash);
        self.w32(b + E_EXPIRES, expires);
        self.w32(b + E_KLEN, key.len() as u32);
        self.w32(b + E_VLEN, val.len() as u32);
        self.w64(b + E_TAG, tag as u64);
        self.put(b + E_DATA, key);
        self.put(b + E_DATA + key.len() as u32, val);
        let bo = self.bucket_off(hash);
        let head = self.r32(bo);
        self.w32(b + E_NEXT, head);
        self.w32(bo, b);
        self.lru_push(b)?;
        inc(&self.stats.entries);
        add(&self.stats.payload_bytes, (key.len() + val.len()) as u64);
        let size = self.bsize(b) as u64;
        add(&self.stats.alloc_bytes, size);
        Ok(())
    }

    fn insert_new(
        &mut self,
        hash: u64,
        key: &[u8],
        tag: u8,
        val: &[u8],
        expires: u32,
        admit: bool,
    ) -> R<SetOutcome> {
        let payload = ENTRY_OVERHEAD + key.len() + val.len();
        // TinyLFU admission: a new key that would push the shard past its
        // soft limit (so something must be evicted) only gets in if it has
        // been asked for more often than the victim it would displace.
        if admit
            && self.tinylfu
            && self.stats.alloc_bytes.load(Relaxed) + payload as u64 + 16
                > self.plan.soft_limit as u64
        {
            let v = self.st.lru_tail;
            if v != 0 {
                self.check(v)?;
                let victim = self.r64(v + E_HASH);
                if self.sketch_estimate(hash) <= self.sketch_estimate(victim) {
                    inc(&self.stats.rejected);
                    return Ok(SetOutcome::Rejected);
                }
            }
        }
        let b = match self.alloc(payload)? {
            Some(b) => b,
            None => loop {
                let v = self.st.lru_tail;
                if v == 0 {
                    inc(&self.stats.no_memory);
                    return Ok(SetOutcome::NoMemory);
                }
                self.remove(v)?;
                inc(&self.stats.evictions);
                if let Some(b) = self.alloc(payload)? {
                    break b;
                }
            },
        };
        self.write_entry(b, hash, key, tag, val, expires)?;
        // Soft limit: keep headroom so large values still find room.
        let mut n = 0;
        while n < 2 && self.stats.alloc_bytes.load(Relaxed) > self.plan.soft_limit as u64 {
            let v = self.st.lru_tail;
            if v == 0 || v == b {
                break;
            }
            self.remove(v)?;
            inc(&self.stats.evictions);
            n += 1;
        }
        Ok(SetOutcome::Stored)
    }

    // ---- operations ------------------------------------------------------

    /// Copies the value into `out`. With `touch == false` (diagnostics) no
    /// statistic, LRU position or expiry changes.
    pub fn get(
        &mut self,
        hash: u64,
        key: &[u8],
        now: u32,
        out: &mut Vec<u8>,
        touch: bool,
    ) -> R<Option<Found>> {
        if self.tinylfu && touch {
            self.sketch_incr(hash);
        }
        let Some(e) = self.find(hash, key)? else {
            if touch {
                inc(&self.stats.misses);
            }
            return Ok(None);
        };
        if self.is_expired(e, now) {
            if touch {
                self.remove(e)?;
                inc(&self.stats.expired);
                inc(&self.stats.misses);
            }
            return Ok(None);
        }
        let (klen, vlen, size) = self.dims(e)?;
        out.clear();
        out.extend_from_slice(self.bytes(e + E_DATA + klen, vlen));
        let found = Found {
            tag: self.tag(e),
            expires: self.r32(e + E_EXPIRES),
            alloc: size,
        };
        if touch {
            self.lru_touch(e)?;
            inc(&self.stats.hits);
        }
        Ok(Some(found))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set(
        &mut self,
        hash: u64,
        key: &[u8],
        tag: u8,
        val: &[u8],
        expires: u32,
        mode: SetMode,
        now: u32,
        is_live: &dyn Fn(&[u8]) -> bool,
    ) -> R<SetOutcome> {
        let mut existing = self.find(hash, key)?;
        if let Some(e) = existing {
            if self.is_expired(e, now) {
                self.remove(e)?;
                inc(&self.stats.expired);
                existing = None;
            }
        }
        match (mode, existing) {
            (SetMode::Add, Some(_)) => return Ok(SetOutcome::Exists),
            (SetMode::Replace, None) => return Ok(SetOutcome::Missing),
            _ => {}
        }
        let payload = ENTRY_OVERHEAD + key.len() + val.len();
        if payload > self.plan.max_item as usize {
            // Never leave an older value behind a failed write.
            if let Some(e) = existing {
                self.remove(e)?;
            }
            inc(&self.stats.too_large);
            return Ok(SetOutcome::TooLarge);
        }
        if let Some(e) = existing {
            let (klen, vlen, size) = self.dims(e)?;
            let cap = (size - 8) as usize;
            if cap >= payload && cap <= payload * 2 + 64 {
                self.w32(e + E_VLEN, val.len() as u32);
                self.w32(e + E_EXPIRES, expires);
                self.w64(e + E_TAG, tag as u64);
                self.put(e + E_DATA + klen, val);
                sub(&self.stats.payload_bytes, vlen as u64);
                add(&self.stats.payload_bytes, val.len() as u64);
                self.lru_touch(e)?;
                inc(&self.stats.sets);
                return Ok(SetOutcome::Stored);
            }
            self.remove(e)?;
        }
        self.sweep_tail(now, is_live)?;
        let r = self.insert_new(hash, key, tag, val, expires, existing.is_none())?;
        if r == SetOutcome::Stored {
            inc(&self.stats.sets);
        }
        Ok(r)
    }

    pub fn delete(&mut self, hash: u64, key: &[u8], now: u32) -> R<bool> {
        let Some(e) = self.find(hash, key)? else {
            return Ok(false);
        };
        let expired = self.is_expired(e, now);
        self.remove(e)?;
        if expired {
            inc(&self.stats.expired);
            return Ok(false);
        }
        inc(&self.stats.deletes);
        Ok(true)
    }

    /// Atomic increment (negative `offset` = decrement) with core semantics.
    pub fn incr(&mut self, hash: u64, key: &[u8], offset: i64, now: u32) -> R<Option<Number>> {
        let Some(e) = self.find(hash, key)? else {
            inc(&self.stats.misses);
            return Ok(None);
        };
        if self.is_expired(e, now) {
            self.remove(e)?;
            inc(&self.stats.expired);
            inc(&self.stats.misses);
            return Ok(None);
        }
        let (klen, vlen, size) = self.dims(e)?;
        let n = value::incr(self.tag(e), self.bytes(e + E_DATA + klen, vlen), offset);
        let (tag, bytes) = n.encode();
        let payload = ENTRY_OVERHEAD + klen as usize + 8;
        if (size - 8) as usize >= payload {
            self.w32(e + E_VLEN, 8);
            self.w64(e + E_TAG, tag as u64);
            self.put(e + E_DATA + klen, &bytes);
            sub(&self.stats.payload_bytes, vlen as u64);
            add(&self.stats.payload_bytes, 8);
            self.lru_touch(e)?;
        } else {
            let key = self.bytes(e + E_DATA, klen).to_vec();
            let expires = self.r32(e + E_EXPIRES);
            let hash = self.r64(e + E_HASH);
            self.remove(e)?;
            if self.insert_new(hash, &key, tag, &bytes, expires, false)? != SetOutcome::Stored {
                return Ok(None);
            }
        }
        inc(&self.stats.hits);
        inc(&self.stats.sets);
        Ok(Some(n))
    }

    /// Visits every entry from most to least recently used.
    pub fn walk(&self, mut f: impl FnMut(Walked<'_>)) -> R<()> {
        let mut cur = self.st.lru_head;
        let limit = self.stats.entries.load(Relaxed) + 8;
        let mut steps = 0u64;
        while cur != 0 {
            let (klen, vlen, size) = self.dims(cur)?;
            let _ = vlen;
            f(Walked {
                key: self.bytes(cur + E_DATA, klen),
                alloc: size,
            });
            cur = self.r32(cur + E_LNEXT);
            steps += 1;
            if steps > limit {
                return Err(Corrupt);
            }
        }
        Ok(())
    }

    /// Full structural check, for `wp-rust-cache verify`.
    pub fn verify(&self) -> Result<(), String> {
        let st = &*self.st;
        // 1. Physical blocks tile the heap exactly.
        let mut b = self.plan.heap_offset;
        let mut prev = 0u32;
        let mut prev_free = false;
        let (mut used, mut free_blocks, mut used_bytes) = (0u64, 0u64, 0u64);
        let sentinel = self.plan.heap_end - 8;
        while b < sentinel {
            let size = self.bsize(b);
            let flags = self.bflags(b);
            if size < MIN_BLOCK || size % 8 != 0 || b as u64 + size as u64 > sentinel as u64 {
                return Err(format!("block {b}: bad size {size}"));
            }
            if (flags & PREV_FREE != 0) != prev_free {
                return Err(format!(
                    "block {b}: prev-free flag disagrees with neighbour"
                ));
            }
            if prev != 0 && prev_free && self.r32(b) != prev {
                return Err(format!("block {b}: wrong prev-physical link"));
            }
            let is_free = flags & FREE != 0;
            if is_free && prev_free {
                return Err(format!("block {b}: two adjacent free blocks"));
            }
            if is_free {
                free_blocks += 1;
            } else {
                used += 1;
                used_bytes += size as u64;
            }
            prev = b;
            prev_free = is_free;
            b += size;
        }
        if b != sentinel {
            return Err("blocks do not end at the sentinel".into());
        }
        // 2. Free lists hold exactly the free blocks.
        let mut listed = 0u64;
        for fl in 0..FL_COUNT {
            for sl in 0..SL_COUNT {
                let mut f = st.free_heads[fl][sl];
                let has_bit = st.sl_bitmap[fl] & (1 << sl) != 0;
                if (f != 0) != has_bit {
                    return Err(format!("free list {fl}/{sl}: bitmap disagrees"));
                }
                while f != 0 {
                    self.check_phys(f)
                        .map_err(|_| format!("free list {fl}/{sl}: bad offset"))?;
                    if self.bflags(f) & FREE == 0 || Self::mapping(self.bsize(f)) != (fl, sl) {
                        return Err(format!("free list {fl}/{sl}: wrong block {f}"));
                    }
                    listed += 1;
                    if listed > free_blocks {
                        return Err("free lists longer than the free blocks".into());
                    }
                    f = self.r32(f + 8);
                }
            }
        }
        if listed != free_blocks {
            return Err(format!("{free_blocks} free blocks, {listed} listed"));
        }
        // 3. LRU and index agree with the counters.
        let entries = self.stats.entries.load(Relaxed);
        if used != entries {
            return Err(format!("{used} used blocks, {entries} entries counted"));
        }
        if used_bytes != self.stats.alloc_bytes.load(Relaxed) {
            return Err("allocated bytes counter disagrees with blocks".into());
        }
        let mut n = 0u64;
        let mut cur = st.lru_head;
        let mut back = 0u32;
        while cur != 0 {
            self.dims(cur)
                .map_err(|_| format!("LRU: bad entry {cur}"))?;
            if self.r32(cur + E_LPREV) != back {
                return Err(format!("LRU: broken back link at {cur}"));
            }
            let (klen, _, _) = self.dims(cur).unwrap();
            let hash = self.r64(cur + E_HASH);
            match self.find(hash, self.bytes(cur + E_DATA, klen)) {
                Ok(Some(x)) if x == cur => {}
                _ => return Err(format!("entry {cur} not reachable from its bucket")),
            }
            n += 1;
            if n > entries {
                return Err("LRU longer than entry count".into());
            }
            back = cur;
            cur = self.r32(cur + E_LNEXT);
        }
        if back != st.lru_tail || n != entries {
            return Err(format!("LRU holds {n} entries, {entries} counted"));
        }
        Ok(())
    }
}
