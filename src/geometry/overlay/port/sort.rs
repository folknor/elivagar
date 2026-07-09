//! Serial bin sort, collapsed and pruned from i_key_sort 0.10.3.
//!
//! Upstream is a multi-file generic crate (u8..u64 / i8..i64 / usize keys,
//! optional rayon). The overlay engine only keys on i32 coordinates and usize
//! segment indices, serially, so this consolidates the used surface - the
//! two-keys, two-keys-plus-comparator, and one-key-plus-comparator entry
//! points, all buffer-reusing - into one module, with a `SortKey` trait
//! carrying only the i32 and usize impls and the parallel paths dropped. The
//! bin layout, spread, and per-chunk recursion are byte-for-byte the upstream
//! serial algorithm (`< 64` elements fall back to `sort_unstable_by` exactly as
//! upstream), so the sort order is identical; the pristine reference stays under
//! `research/i_key_sort`.

use alloc::vec::Vec;
use core::cmp::Ordering;
use core::mem::MaybeUninit;
use core::ops::Range;
use core::ptr;

const BIN_SORT_MIN: usize = 64;
const MAX_BINS_POWER: u32 = 8;
const MAX_BINS_COUNT: usize = 1 << MAX_BINS_POWER;

/// Integer sort key. i_key_sort supports u8..u64 / i8..i64 / usize; the overlay
/// engine only ever keys on i32 coordinates and usize segment indices, so only
/// those two impls are retained.
pub(crate) trait SortKey: Copy + Ord {
    fn difference(self, other: Self) -> usize;
}

impl SortKey for i32 {
    #[inline(always)]
    fn difference(self, other: Self) -> usize {
        debug_assert!(self >= other, "difference requires self >= other");
        (self - other) as usize
    }
}

impl SortKey for usize {
    #[inline(always)]
    fn difference(self, other: Self) -> usize {
        debug_assert!(self >= other, "difference requires self >= other");
        self - other
    }
}

/// `Fn(&T) -> K` key extractor (i_key_sort's `KeyFn`).
pub(crate) trait KeyFn<T, K>: Fn(&T) -> K + Copy {}
impl<T, K, F: Fn(&T) -> K + Copy> KeyFn<T, K> for F {}

/// `Fn(&T, &T) -> Ordering` tiebreak comparator.
pub(crate) trait CmpFn<T>: Fn(&T, &T) -> Ordering + Copy {}
impl<T, F: Fn(&T, &T) -> Ordering + Copy> CmpFn<T> for F {}

// --- buffer helpers (i_key_sort sort/buffer.rs) -----------------------------

trait CopyFromNotOverlap<T> {
    fn copy_from_not_overlap(&mut self, buffer: &[T]);
    fn copy_to_range_from_not_overlap(&mut self, buffer: &[T], range: Range<usize>);
}

trait CopyNotOverlapValue<T> {
    fn copy_value_from(&mut self, src: &[T], index: usize);
}

impl<T: Copy> CopyNotOverlapValue<T> for [T] {
    #[inline(always)]
    fn copy_value_from(&mut self, src: &[T], index: usize) {
        unsafe {
            ptr::copy_nonoverlapping(src.as_ptr().add(index), self.as_mut_ptr().add(index), 1);
        }
    }
}

impl<T> CopyFromNotOverlap<T> for [T] {
    #[inline(always)]
    fn copy_from_not_overlap(&mut self, buffer: &[T]) {
        unsafe {
            ptr::copy_nonoverlapping(buffer.as_ptr(), self.as_mut_ptr(), self.len());
        }
    }

    #[inline(always)]
    fn copy_to_range_from_not_overlap(&mut self, buffer: &[T], range: Range<usize>) {
        debug_assert_eq!(range.len(), buffer.len());
        let dst = unsafe { self.get_unchecked_mut(range) };
        dst.copy_from_not_overlap(buffer);
    }
}

trait DoubleRangeSlices<T> {
    fn mut_slices<'a>(
        &self,
        slice1: &'a mut [T],
        slice2: &'a mut [T],
    ) -> (&'a mut [T], &'a mut [T]);
}

impl<T> DoubleRangeSlices<T> for Range<usize> {
    #[inline(always)]
    fn mut_slices<'a>(
        &self,
        slice1: &'a mut [T],
        slice2: &'a mut [T],
    ) -> (&'a mut [T], &'a mut [T]) {
        unsafe {
            let sub1 = slice1.get_unchecked_mut(self.clone());
            let sub2 = slice2.get_unchecked_mut(self.clone());
            (sub1, sub2)
        }
    }
}

#[inline(always)]
fn min_max<T, K: SortKey, F: KeyFn<T, K>>(array: &[T], key: F) -> (K, K) {
    debug_assert!(!array.is_empty());
    let mut min_key = key(&array[0]);
    let mut max_key = min_key;
    for val in array.iter().skip(1) {
        let k = key(val);
        min_key = min_key.min(k);
        max_key = max_key.max(k);
    }
    (min_key, max_key)
}

// --- bin layout (i_key_sort sort/bin_layout.rs) -----------------------------

struct BinLayout<K> {
    min_key: K,
    max_key: K,
    power: usize,
    bin_width_is_one: bool,
}

#[inline(always)]
fn ilog2_ceil(v: usize) -> u32 {
    let floor = v.ilog2();
    if v.is_power_of_two() {
        floor
    } else {
        floor + 1
    }
}

impl<K: SortKey> BinLayout<K> {
    #[inline(always)]
    fn bin_width_is_one(&self) -> bool {
        self.bin_width_is_one
    }

    #[inline(always)]
    fn index(&self, value: K) -> usize {
        value.difference(self.min_key) >> self.power
    }

    #[inline(always)]
    fn count(&self) -> usize {
        self.index(self.max_key) + 1
    }

    fn with_constraints(min_key: K, max_key: K) -> BinLayout<K> {
        let length = max_key.difference(min_key);
        if length < MAX_BINS_COUNT {
            return Self {
                min_key,
                max_key,
                power: 0,
                bin_width_is_one: true,
            };
        }
        let scale = ilog2_ceil(length.saturating_add(1));
        let power = scale.saturating_sub(MAX_BINS_COUNT.ilog2()) as usize;
        Self {
            min_key,
            max_key,
            power,
            bin_width_is_one: false,
        }
    }

    #[inline(always)]
    fn with_keys<T, F: KeyFn<T, K>>(array: &[T], key: F) -> Option<Self> {
        if array.is_empty() {
            return None;
        }
        let (min_key, max_key) = min_max(array, key);
        if min_key == max_key {
            return None;
        }
        Some(Self::with_constraints(min_key, max_key))
    }

    #[inline(always)]
    fn spread_with_uninit_buffer<T: Copy, F: KeyFn<T, K>>(
        &self,
        src: &mut [T],
        buf: &mut Vec<T>,
        key: F,
    ) -> Mapper {
        buf.clear();
        let need = src.len();
        if buf.capacity() < need {
            buf.reserve(need);
        }
        let scratch: &mut [MaybeUninit<T>] = &mut buf.spare_capacity_mut()[..need];

        let mut mapper = Mapper::new(self.count());
        for a in src.iter() {
            mapper.inc_bin_count(self.index(key(a)));
        }
        mapper.init_indices();

        for val in src.iter() {
            let index = mapper.next_index(self.index(key(val)));
            unsafe {
                scratch.get_unchecked_mut(index).write(*val);
            }
        }

        #[allow(clippy::uninit_vec)]
        unsafe {
            buf.set_len(need);
        }

        mapper
    }

    #[inline(always)]
    fn spread_with_buffer<T: Copy, F: KeyFn<T, K>>(
        &self,
        src: &mut [T],
        buf: &mut [T],
        key: F,
    ) -> Mapper {
        let mut mapper = Mapper::new(self.count());
        for a in src.iter() {
            mapper.inc_bin_count(self.index(key(a)));
        }
        mapper.init_indices();
        for val in src.iter() {
            let index = mapper.next_index(self.index(key(val)));
            unsafe {
                *buf.get_unchecked_mut(index) = *val;
            }
        }
        mapper
    }
}

// --- mapper (i_key_sort sort/mapper.rs) -------------------------------------

#[derive(Clone, Copy, Default)]
struct Chunk {
    index: usize,
    count: usize,
}

impl Chunk {
    #[inline(always)]
    fn as_range(&self) -> Range<usize> {
        let end = self.index;
        let start = self.index - self.count;
        start..end
    }
}

struct Mapper {
    count: usize,
    chunks: [Chunk; MAX_BINS_COUNT],
}

impl Mapper {
    #[inline(always)]
    fn new(count: usize) -> Self {
        debug_assert!(count <= MAX_BINS_COUNT);
        Self {
            count,
            chunks: [Chunk::default(); MAX_BINS_COUNT],
        }
    }

    #[inline(always)]
    fn inc_bin_count(&mut self, chunk_index: usize) {
        unsafe { self.chunks.get_unchecked_mut(chunk_index).count += 1 };
    }

    #[inline(always)]
    fn next_index(&mut self, chunk_index: usize) -> usize {
        let chunk = unsafe { self.chunks.get_unchecked_mut(chunk_index) };
        let index = chunk.index;
        chunk.index += 1;
        index
    }

    #[inline(always)]
    fn init_indices(&mut self) {
        let mut offset = 0;
        for chunk in &mut self.chunks[..self.count] {
            chunk.index = offset;
            offset += chunk.count;
        }
    }

    #[inline(always)]
    fn iter(&self) -> core::slice::Iter<'_, Chunk> {
        unsafe { self.chunks.get_unchecked(..self.count) }.iter()
    }
}

// --- unstable fallbacks (used below 64 elements and for tiny bins) ----------

#[inline]
fn sort_unstable_by_two_keys<T, K1: SortKey, K2: SortKey, F1: KeyFn<T, K1>, F2: KeyFn<T, K2>>(
    slice: &mut [T],
    key1: F1,
    key2: F2,
) {
    slice.sort_unstable_by(|a, b| key1(a).cmp(&key1(b)).then(key2(a).cmp(&key2(b))));
}

#[inline]
fn sort_unstable_by_two_keys_then_by<
    T,
    K1: SortKey,
    K2: SortKey,
    F1: KeyFn<T, K1>,
    F2: KeyFn<T, K2>,
    F3: CmpFn<T>,
>(
    slice: &mut [T],
    key1: F1,
    key2: F2,
    compare: F3,
) {
    slice.sort_unstable_by(|a, b| {
        key1(a)
            .cmp(&key1(b))
            .then(key2(a).cmp(&key2(b)))
            .then(compare(a, b))
    });
}

#[inline]
fn sort_unstable_by_one_key_then_by<T, K: SortKey, F1: KeyFn<T, K>, F2: CmpFn<T>>(
    slice: &mut [T],
    key: F1,
    compare: F2,
) {
    slice.sort_unstable_by(|a, b| key(a).cmp(&key(b)).then(compare(a, b)));
}

// --- public entry points (parallel dropped) --------------------------------

pub(crate) trait TwoKeysSort<T> {
    fn sort_by_two_keys_and_buffer<K1: SortKey, K2: SortKey, F1: KeyFn<T, K1>, F2: KeyFn<T, K2>>(
        &mut self,
        reusable_buffer: &mut Vec<T>,
        key1: F1,
        key2: F2,
    );
}

impl<T: Copy> TwoKeysSort<T> for [T] {
    #[inline]
    fn sort_by_two_keys_and_buffer<K1: SortKey, K2: SortKey, F1: KeyFn<T, K1>, F2: KeyFn<T, K2>>(
        &mut self,
        reusable_buffer: &mut Vec<T>,
        key1: F1,
        key2: F2,
    ) {
        if self.len() < BIN_SORT_MIN {
            sort_unstable_by_two_keys(self, key1, key2);
            return;
        }
        ser_sort_by_two_keys_and_uninit_buffer(self, reusable_buffer, key1, key2);
    }
}

pub(crate) trait TwoKeysAndCmpSort<T> {
    fn sort_by_two_keys_then_by_and_buffer<
        K1: SortKey,
        K2: SortKey,
        F1: KeyFn<T, K1>,
        F2: KeyFn<T, K2>,
        F3: CmpFn<T>,
    >(
        &mut self,
        reusable_buffer: &mut Vec<T>,
        key1: F1,
        key2: F2,
        compare: F3,
    );
}

impl<T: Copy> TwoKeysAndCmpSort<T> for [T] {
    #[inline]
    fn sort_by_two_keys_then_by_and_buffer<
        K1: SortKey,
        K2: SortKey,
        F1: KeyFn<T, K1>,
        F2: KeyFn<T, K2>,
        F3: CmpFn<T>,
    >(
        &mut self,
        reusable_buffer: &mut Vec<T>,
        key1: F1,
        key2: F2,
        compare: F3,
    ) {
        if self.len() < BIN_SORT_MIN {
            sort_unstable_by_two_keys_then_by(self, key1, key2, compare);
            return;
        }
        ser_sort_by_two_keys_then_by_and_uninit_buffer(self, reusable_buffer, key1, key2, compare);
    }
}

pub(crate) trait OneKeyAndCmpSort<T> {
    fn sort_by_one_key_then_by_and_buffer<K: SortKey, F1: KeyFn<T, K>, F2: CmpFn<T>>(
        &mut self,
        reusable_buffer: &mut Vec<T>,
        key: F1,
        compare: F2,
    );
}

impl<T: Copy> OneKeyAndCmpSort<T> for [T] {
    #[inline]
    fn sort_by_one_key_then_by_and_buffer<K: SortKey, F1: KeyFn<T, K>, F2: CmpFn<T>>(
        &mut self,
        reusable_buffer: &mut Vec<T>,
        key: F1,
        compare: F2,
    ) {
        if self.len() < BIN_SORT_MIN {
            sort_unstable_by_one_key_then_by(self, key, compare);
            return;
        }
        ser_sort_by_one_key_then_by_and_uninit_buffer(self, reusable_buffer, key, compare);
    }
}

// --- serial slice recursion (i_key_sort sort/serial/slice_*.rs) -------------

fn ser_sort_by_two_keys_and_uninit_buffer<
    T: Copy,
    K1: SortKey,
    K2: SortKey,
    F1: KeyFn<T, K1>,
    F2: KeyFn<T, K2>,
>(
    src: &mut [T],
    buf: &mut Vec<T>,
    key1: F1,
    key2: F2,
) {
    if let Some(layout) = BinLayout::with_keys(src, key1) {
        layout.sort_by_two_keys_and_uninit_buffer(src, buf, key1, key2);
    } else {
        ser_sort_by_one_key_and_uninit_buffer(src, buf, key2);
    }
}

fn ser_sort_by_two_keys_and_buffer<
    T: Copy,
    K1: SortKey,
    K2: SortKey,
    F1: KeyFn<T, K1>,
    F2: KeyFn<T, K2>,
>(
    src: &mut [T],
    buf: &mut [T],
    key1: F1,
    key2: F2,
    copy_to_src: bool,
) {
    debug_assert_eq!(src.len(), buf.len());
    if let Some(layout) = BinLayout::with_keys(src, key1) {
        layout.sort_by_two_keys_and_buffer(src, buf, key1, key2, copy_to_src);
    } else {
        ser_sort_by_one_key_and_buffer(src, buf, key2, copy_to_src);
    }
}

fn ser_sort_by_two_keys_then_by_and_uninit_buffer<
    T: Copy,
    K1: SortKey,
    K2: SortKey,
    F1: KeyFn<T, K1>,
    F2: KeyFn<T, K2>,
    F3: CmpFn<T>,
>(
    src: &mut [T],
    buf: &mut Vec<T>,
    key1: F1,
    key2: F2,
    compare: F3,
) {
    if let Some(layout) = BinLayout::with_keys(src, key1) {
        layout.sort_by_two_keys_then_by_and_uninit_buffer(src, buf, key1, key2, compare);
    } else {
        ser_sort_by_one_key_then_by_and_uninit_buffer(src, buf, key2, compare);
    }
}

fn ser_sort_by_two_keys_then_by_and_buffer<
    T: Copy,
    K1: SortKey,
    K2: SortKey,
    F1: KeyFn<T, K1>,
    F2: KeyFn<T, K2>,
    F3: CmpFn<T>,
>(
    src: &mut [T],
    buf: &mut [T],
    key1: F1,
    key2: F2,
    compare: F3,
    copy_to_src: bool,
) {
    debug_assert_eq!(src.len(), buf.len());
    if let Some(layout) = BinLayout::with_keys(src, key1) {
        layout.sort_by_two_keys_then_by_and_buffer(src, buf, key1, key2, compare, copy_to_src);
    } else {
        ser_sort_by_one_key_then_by_and_buffer(src, buf, key2, compare, copy_to_src);
    }
}

fn ser_sort_by_one_key_and_uninit_buffer<T: Copy, K: SortKey, F: KeyFn<T, K>>(
    src: &mut [T],
    buf: &mut Vec<T>,
    key: F,
) {
    if let Some(layout) = BinLayout::with_keys(src, key) {
        layout.sort_by_one_key_and_uninit_buffer(src, buf, key);
    }
}

fn ser_sort_by_one_key_and_buffer<T: Copy, K: SortKey, F: KeyFn<T, K>>(
    src: &mut [T],
    buf: &mut [T],
    key: F,
    copy_to_src: bool,
) {
    debug_assert_eq!(src.len(), buf.len());
    if let Some(layout) = BinLayout::with_keys(src, key) {
        layout.sort_by_one_key_and_buffer(src, buf, key, copy_to_src);
    } else if !copy_to_src {
        buf.copy_from_not_overlap(src);
    }
}

fn ser_sort_by_one_key_then_by_and_uninit_buffer<
    T: Copy,
    K: SortKey,
    F1: KeyFn<T, K>,
    F2: CmpFn<T>,
>(
    src: &mut [T],
    buf: &mut Vec<T>,
    key: F1,
    compare: F2,
) {
    if let Some(layout) = BinLayout::with_keys(src, key) {
        layout.sort_by_one_key_then_by_and_uninit_buffer(src, buf, key, compare);
    } else {
        src.sort_unstable_by(compare);
    }
}

fn ser_sort_by_one_key_then_by_and_buffer<T: Copy, K: SortKey, F1: KeyFn<T, K>, F2: CmpFn<T>>(
    src: &mut [T],
    buf: &mut [T],
    key: F1,
    compare: F2,
    copy_to_src: bool,
) {
    debug_assert_eq!(src.len(), buf.len());
    if let Some(layout) = BinLayout::with_keys(src, key) {
        layout.sort_by_one_key_then_by_and_buffer(src, buf, key, compare, copy_to_src);
    } else {
        src.sort_unstable_by(compare);
        if !copy_to_src {
            buf.copy_from_not_overlap(src);
        }
    }
}

// --- per-layout dispatch (i_key_sort sort/serial/layout_*.rs) ---------------

impl<K1: SortKey> BinLayout<K1> {
    fn sort_by_two_keys_and_uninit_buffer<
        T: Copy,
        K2: SortKey,
        F1: KeyFn<T, K1>,
        F2: KeyFn<T, K2>,
    >(
        &self,
        src: &mut [T],
        buf: &mut Vec<T>,
        key1: F1,
        key2: F2,
    ) {
        let mapper = self.spread_with_uninit_buffer(src, buf, key1);
        if self.bin_width_is_one() {
            mapper.sort_chunks_by_one_key(src, buf, key2, true);
        } else {
            mapper.sort_chunks_by_two_keys(src, buf, key1, key2, true);
        }
    }

    fn sort_by_two_keys_and_buffer<T: Copy, K2: SortKey, F1: KeyFn<T, K1>, F2: KeyFn<T, K2>>(
        &self,
        src: &mut [T],
        buf: &mut [T],
        key1: F1,
        key2: F2,
        copy_to_src: bool,
    ) {
        let mapper = self.spread_with_buffer(src, buf, key1);
        if self.bin_width_is_one() {
            mapper.sort_chunks_by_one_key(src, buf, key2, copy_to_src);
        } else {
            mapper.sort_chunks_by_two_keys(src, buf, key1, key2, copy_to_src);
        }
    }

    fn sort_by_two_keys_then_by_and_uninit_buffer<
        T: Copy,
        K2: SortKey,
        F1: KeyFn<T, K1>,
        F2: KeyFn<T, K2>,
        F3: CmpFn<T>,
    >(
        &self,
        src: &mut [T],
        buf: &mut Vec<T>,
        key1: F1,
        key2: F2,
        compare: F3,
    ) {
        let mapper = self.spread_with_uninit_buffer(src, buf, key1);
        if self.bin_width_is_one() {
            mapper.sort_chunks_by_one_key_then_by(src, buf, key2, compare, true);
        } else {
            mapper.sort_chunks_by_two_keys_then_by(src, buf, key1, key2, compare, true);
        }
    }

    fn sort_by_two_keys_then_by_and_buffer<
        T: Copy,
        K2: SortKey,
        F1: KeyFn<T, K1>,
        F2: KeyFn<T, K2>,
        F3: CmpFn<T>,
    >(
        &self,
        src: &mut [T],
        buf: &mut [T],
        key1: F1,
        key2: F2,
        compare: F3,
        copy_to_src: bool,
    ) {
        let mapper = self.spread_with_buffer(src, buf, key1);
        if self.bin_width_is_one() {
            mapper.sort_chunks_by_one_key_then_by(src, buf, key2, compare, copy_to_src);
        } else {
            mapper.sort_chunks_by_two_keys_then_by(src, buf, key1, key2, compare, copy_to_src);
        }
    }

    fn sort_by_one_key_and_uninit_buffer<T: Copy, F: KeyFn<T, K1>>(
        &self,
        src: &mut [T],
        buf: &mut Vec<T>,
        key: F,
    ) {
        let mapper = self.spread_with_uninit_buffer(src, buf, key);
        if self.bin_width_is_one() {
            src.copy_from_not_overlap(buf);
        } else {
            mapper.sort_chunks_by_one_key(src, buf, key, true);
        }
    }

    fn sort_by_one_key_and_buffer<T: Copy, F: KeyFn<T, K1>>(
        &self,
        src: &mut [T],
        buf: &mut [T],
        key: F,
        copy_to_src: bool,
    ) {
        let mapper = self.spread_with_buffer(src, buf, key);
        if self.bin_width_is_one() {
            if copy_to_src {
                src.copy_from_not_overlap(buf);
            }
        } else {
            mapper.sort_chunks_by_one_key(src, buf, key, copy_to_src);
        }
    }

    fn sort_by_one_key_then_by_and_uninit_buffer<T: Copy, F1: KeyFn<T, K1>, F2: CmpFn<T>>(
        &self,
        src: &mut [T],
        buf: &mut Vec<T>,
        key: F1,
        compare: F2,
    ) {
        let mapper = self.spread_with_uninit_buffer(src, buf, key);
        if self.bin_width_is_one() {
            mapper.sort_chunks_by(src, buf, compare, true);
        } else {
            mapper.sort_chunks_by_one_key_then_by(src, buf, key, compare, true);
        }
    }

    fn sort_by_one_key_then_by_and_buffer<T: Copy, F1: KeyFn<T, K1>, F2: CmpFn<T>>(
        &self,
        src: &mut [T],
        buf: &mut [T],
        key: F1,
        compare: F2,
        copy_to_src: bool,
    ) {
        let mapper = self.spread_with_buffer(src, buf, key);
        if self.bin_width_is_one() {
            mapper.sort_chunks_by(src, buf, compare, copy_to_src);
        } else {
            mapper.sort_chunks_by_one_key_then_by(src, buf, key, compare, copy_to_src);
        }
    }
}

// --- per-chunk recursion (i_key_sort sort/serial/mapper_*.rs) ---------------

impl Mapper {
    fn sort_chunks_by_two_keys<
        T: Copy,
        K1: SortKey,
        K2: SortKey,
        F1: KeyFn<T, K1>,
        F2: KeyFn<T, K2>,
    >(
        &self,
        src: &mut [T],
        buf: &mut [T],
        key1: F1,
        key2: F2,
        copy_to_src: bool,
    ) {
        for chunk in self.iter() {
            let range = chunk.as_range();
            match range.len() {
                0 => continue,
                1 => {
                    if copy_to_src {
                        src.copy_value_from(buf, range.start);
                    }
                }
                2..BIN_SORT_MIN => {
                    let sub_buf = unsafe { buf.get_unchecked_mut(range.clone()) };
                    sort_unstable_by_two_keys(sub_buf, key1, key2);
                    if copy_to_src {
                        src.copy_to_range_from_not_overlap(sub_buf, range);
                    }
                }
                _ => {
                    let (sub_src, sub_buf) = range.mut_slices(src, buf);
                    ser_sort_by_two_keys_and_buffer(sub_buf, sub_src, key1, key2, !copy_to_src);
                }
            }
        }
    }

    fn sort_chunks_by_two_keys_then_by<
        T: Copy,
        K1: SortKey,
        K2: SortKey,
        F1: KeyFn<T, K1>,
        F2: KeyFn<T, K2>,
        F3: CmpFn<T>,
    >(
        &self,
        src: &mut [T],
        buf: &mut [T],
        key1: F1,
        key2: F2,
        compare: F3,
        copy_to_src: bool,
    ) {
        for chunk in self.iter() {
            let range = chunk.as_range();
            match range.len() {
                0 => continue,
                1 => {
                    if copy_to_src {
                        src.copy_value_from(buf, range.start);
                    }
                }
                2..BIN_SORT_MIN => {
                    let sub_buf = unsafe { buf.get_unchecked_mut(range.clone()) };
                    sort_unstable_by_two_keys_then_by(sub_buf, key1, key2, compare);
                    if copy_to_src {
                        src.copy_to_range_from_not_overlap(sub_buf, range);
                    }
                }
                _ => {
                    let (sub_src, sub_buf) = range.mut_slices(src, buf);
                    ser_sort_by_two_keys_then_by_and_buffer(
                        sub_buf,
                        sub_src,
                        key1,
                        key2,
                        compare,
                        !copy_to_src,
                    );
                }
            }
        }
    }

    fn sort_chunks_by_one_key<T: Copy, K: SortKey, F: KeyFn<T, K>>(
        &self,
        src: &mut [T],
        buf: &mut [T],
        key: F,
        copy_to_src: bool,
    ) {
        for chunk in self.iter() {
            let range = chunk.as_range();
            match range.len() {
                0 => continue,
                1 => {
                    if copy_to_src {
                        src.copy_value_from(buf, range.start);
                    }
                }
                2..BIN_SORT_MIN => {
                    let sub_buf = unsafe { buf.get_unchecked_mut(range.clone()) };
                    sub_buf.sort_unstable_by_key(key);
                    if copy_to_src {
                        src.copy_to_range_from_not_overlap(sub_buf, range);
                    }
                }
                _ => {
                    let (sub_src, sub_buf) = range.mut_slices(src, buf);
                    ser_sort_by_one_key_and_buffer(sub_buf, sub_src, key, !copy_to_src);
                }
            }
        }
    }

    fn sort_chunks_by_one_key_then_by<T: Copy, K: SortKey, F1: KeyFn<T, K>, F2: CmpFn<T>>(
        &self,
        src: &mut [T],
        buf: &mut [T],
        key: F1,
        compare: F2,
        copy_to_src: bool,
    ) {
        for chunk in self.iter() {
            let range = chunk.as_range();
            match range.len() {
                0 => continue,
                1 => {
                    if copy_to_src {
                        src.copy_value_from(buf, range.start);
                    }
                }
                2..BIN_SORT_MIN => {
                    let sub_buf = unsafe { buf.get_unchecked_mut(range.clone()) };
                    sort_unstable_by_one_key_then_by(sub_buf, key, compare);
                    if copy_to_src {
                        src.copy_to_range_from_not_overlap(sub_buf, range);
                    }
                }
                _ => {
                    let (sub_src, sub_buf) = range.mut_slices(src, buf);
                    ser_sort_by_one_key_then_by_and_buffer(
                        sub_buf,
                        sub_src,
                        key,
                        compare,
                        !copy_to_src,
                    );
                }
            }
        }
    }

    fn sort_chunks_by<T: Copy, F: CmpFn<T>>(
        &self,
        src: &mut [T],
        buf: &mut [T],
        compare: F,
        copy_to_src: bool,
    ) {
        for chunk in self.iter() {
            let range = chunk.as_range();
            match range.len() {
                0 => continue,
                1 => {
                    if copy_to_src {
                        src.copy_value_from(buf, range.start);
                    }
                }
                _ => {
                    let sub_buf = unsafe { buf.get_unchecked_mut(range.clone()) };
                    sub_buf.sort_unstable_by(compare);
                    if copy_to_src {
                        src.copy_to_range_from_not_overlap(sub_buf, range);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn reversed(count: i32) -> Vec<(i32, i32, i32)> {
        let mut arr = Vec::new();
        for x in (0..count).rev() {
            for y in (0..count).rev() {
                for z in (0..3).rev() {
                    arr.push((x, y, z));
                }
            }
        }
        arr
    }

    #[test]
    fn two_keys_then_by_matches_std() {
        for count in [2i32, 5, 20, 40, 100] {
            let mut org = reversed(count);
            let mut arr = org.clone();
            arr.sort_by_two_keys_then_by_and_buffer(
                &mut Vec::new(),
                |a| a.0,
                |a| a.1,
                |a, b| a.2.cmp(&b.2),
            );
            org.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
            assert_eq!(arr, org);
        }
    }

    #[test]
    fn one_key_then_by_usize_matches_std() {
        // one-key-cmp on a usize key, mirroring the LineMark sort.
        for count in [2usize, 5, 20, 40, 100] {
            let mut org: Vec<(usize, i32)> = (0..count)
                .rev()
                .flat_map(|x| (0..3).rev().map(move |y| (x, y)))
                .collect();
            let mut arr = org.clone();
            arr.sort_by_one_key_then_by_and_buffer(&mut Vec::new(), |a| a.0, |a, b| a.1.cmp(&b.1));
            org.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            assert_eq!(arr, org);
        }
    }
}
