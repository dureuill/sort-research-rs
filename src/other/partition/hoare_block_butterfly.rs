//! See comment in [`partition`].

#![allow(unused)]

use core::cmp;
use core::mem::{self, MaybeUninit};
use core::ptr;

partition_impl!("butterfly_block_partition");

// Can the type have interior mutability, this is checked by testing if T is Freeze. If the type can
// have interior mutability it may alter itself during comparison in a way that must be observed
// after the sort operation concludes. Otherwise a type like Mutex<Option<Box<str>>> could lead to
// double free.
unsafe auto trait Freeze {}

impl<T: ?Sized> !Freeze for core::cell::UnsafeCell<T> {}
unsafe impl<T: ?Sized> Freeze for core::marker::PhantomData<T> {}
unsafe impl<T: ?Sized> Freeze for *const T {}
unsafe impl<T: ?Sized> Freeze for *mut T {}
unsafe impl<T: ?Sized> Freeze for &T {}
unsafe impl<T: ?Sized> Freeze for &mut T {}

// TODO update
// // This number is chosen to strike a balance between good perf handoff between only small_partition
// // and hybrid block_partition + small_partition. Avoiding stack buffers that are too large,
// // impacting efficiency negatively. And aromatizing the cost of runtime feature detection, mostly a
// // relaxed atomic load + non-inlined function call.
// const MAX_SMALL_PARTITION_LEN: usize = 255;
const MAX_SMALL_PARTITION_LEN: usize = 256;

// TODO update
// TODO explain more. Both AVX and NEON SIMD were analyzed for `u64` and `i32` element types,
// the inner pivot comparison loop should spend a bit less than a cycle per element doing the
// comparison and 1.5-2.5 cycles if no SIMD is available. TODO cycles per swapped elements.
const BLOCK_PARTITION_BLOCK_SIZE: usize = 32;

trait Partition: Sized {
    /// Takes the input slice `v` and re-arranges elements such that when the call returns normally
    /// all elements that compare true for `is_less(elem, pivot)` are on the left side of `v`
    /// followed by the other elements, notionally considered greater or equal to `pivot`.
    ///
    /// Returns the number of elements that are compared true for `is_less(elem, pivot)`.
    ///
    /// If `is_less` does not implement a total order the resulting order and return value are
    /// unspecified. All original elements will remain in `v` and any possible modifications via
    /// interior mutability will be observable. Same is true if `is_less` panics or `v.len()`
    /// exceeds [`MAX_SMALL_PARTITION_LEN`].
    fn small_partition<F>(v: &mut [Self], pivot: &Self, is_less: &mut F) -> usize
    where
        F: FnMut(&Self, &Self) -> bool;
}

impl<T> Partition for T {
    default fn small_partition<F>(v: &mut [Self], pivot: &Self, is_less: &mut F) -> usize
    where
        F: FnMut(&Self, &Self) -> bool,
    {
        small_partition_move_opt(v, pivot, is_less)
    }
}

impl<T: Freeze + Copy> Partition for T {
    fn small_partition<F>(v: &mut [Self], pivot: &Self, is_less: &mut F) -> usize
    where
        F: FnMut(&Self, &Self) -> bool,
    {
        if const { mem::size_of::<T>() <= mem::size_of::<[usize; 2]>() } {
            small_partition_int_opt(v, pivot, is_less)
        } else {
            small_partition_move_opt(v, pivot, is_less)
        }
    }
}

/// See [`Partition::small_partition`].
///
/// Optimized for integers like types. Not suitable for large types, because it stores temporary
/// copies in a stack buffer.
fn small_partition_int_opt<T, F>(v: &mut [T], pivot: &T, is_less: &mut F) -> usize
where
    F: FnMut(&T, &T) -> bool,
    T: Freeze,
{
    let len = v.len();
    let arr_ptr = v.as_mut_ptr();

    if len >= MAX_SMALL_PARTITION_LEN {
        debug_assert!(false);
        return 0;
    }

    // SAFETY: TODO
    unsafe {
        let mut scratch = MaybeUninit::<[T; MAX_SMALL_PARTITION_LEN]>::uninit();
        let scratch_ptr = scratch.as_mut_ptr() as *mut T;

        let mut lt_count = 0;
        let mut ge_out_ptr = scratch_ptr.add(len);

        // Loop manually unrolled to ensure good performance.
        // Example T == u64, on x86 LLVM unrolls this loop but not on Arm.
        // And it's very perf critical so this is done manually.
        // And surprisingly this can yield better code-gen and perf than the auto-unroll.
        macro_rules! loop_body {
            ($elem_ptr:expr) => {
                ge_out_ptr = ge_out_ptr.sub(1);

                let elem_ptr = $elem_ptr;

                let is_less_than_pivot = is_less(&*elem_ptr, pivot);

                // Benchmarks show that especially on Firestorm (apple-m1) for anything at
                // most the size of a u64 double storing is more efficient than conditional
                // store. It is also less at risk of having the compiler generating a branch
                // instead of conditional store.
                if const { mem::size_of::<T>() <= mem::size_of::<usize>() } {
                    ptr::copy_nonoverlapping(elem_ptr, scratch_ptr.add(lt_count), 1);
                    ptr::copy_nonoverlapping(elem_ptr, ge_out_ptr.add(lt_count), 1);
                } else {
                    let dest_ptr = if is_less_than_pivot {
                        scratch_ptr
                    } else {
                        ge_out_ptr
                    };
                    ptr::copy_nonoverlapping(elem_ptr, dest_ptr.add(lt_count), 1);
                }

                lt_count += is_less_than_pivot as usize;
            };
        }

        let mut i: usize = 0;
        let end = len.saturating_sub(1);

        while i < end {
            loop_body!(arr_ptr.add(i));
            loop_body!(arr_ptr.add(i + 1));
            i += 2;
        }

        if i != len {
            loop_body!(arr_ptr.add(i));
        }

        // SAFETY: swap now contains all elements that belong on the left side of the pivot.
        // All comparisons have been done if is_less would have panicked `v` would have
        // stayed untouched.
        ptr::copy_nonoverlapping(scratch_ptr, arr_ptr, len);

        lt_count
    }
}

macro_rules! dbg_print_2 {
    ($fmt_expr:expr $(,$extra_args:expr)*) => {{
        // use std::io::{self, Write};
        // io::stdout()
        //     .write_all(format!($fmt_expr, $($extra_args,)*).as_bytes())
        //     .unwrap();
        // io::stdout().flush().unwrap();

        // print!($fmt_expr, $($extra_args,)*);
        let _ = ($fmt_expr, $($extra_args,)*);
    }};
}

/// SAFETY: The caller must ensure that all provided expression are no-panic and may not modify the
/// values produced by `next_left` and `next_right`. And the produced pointers MUST NOT alias.
macro_rules! cyclic_permutation_swap_loop {
    ($continue_check:expr, $next_left:expr, $next_right:expr, $base_ptr:expr) => {
        let base_ptr = $base_ptr; // TODO remove

        // Instead of swapping one pair at the time, it is more efficient to perform a cyclic
        // permutation. This is not strictly equivalent to swapping, but produces a similar
        // result using fewer memory operations.
        //
        // Example cyclic permutation to swap A,B,C,D with W,X,Y,Z
        //
        // A -> TMP
        // Z -> A   | Z,B,C,D ___ W,X,Y,Z
        //
        // Loop iter 1
        // B -> Z   | Z,B,C,D ___ W,X,Y,B
        // Y -> B   | Z,Y,C,D ___ W,X,Y,B
        //
        // Loop iter 2
        // C -> Y   | Z,Y,C,D ___ W,X,C,B
        // X -> C   | Z,Y,X,D ___ W,X,C,B
        //
        // Loop iter 3
        // D -> X   | Z,Y,X,D ___ W,D,C,B
        // W -> D   | Z,Y,X,W ___ W,D,C,B
        //
        // TMP -> W | Z,Y,X,W ___ A,D,C,B

        if $continue_check {
            let mut left_ptr = $next_left;
            let mut right_ptr = $next_right;

            // SAFETY: The following code is both panic- and observation-safe, so it's ok to
            // create a temporary.
            let tmp = ptr::read(left_ptr);
            ptr::copy_nonoverlapping(right_ptr, left_ptr, 1);

            while $continue_check {
                left_ptr = $next_left;
                dbg_print_2!(
                    "{} -> {} | ",
                    left_ptr.sub_ptr(base_ptr),
                    right_ptr.sub_ptr(base_ptr)
                );
                ptr::copy_nonoverlapping(left_ptr, right_ptr, 1);
                right_ptr = $next_right;
                dbg_print_2!(
                    "{} -> {}\n",
                    right_ptr.sub_ptr(base_ptr),
                    left_ptr.sub_ptr(base_ptr)
                );
                ptr::copy_nonoverlapping(right_ptr, left_ptr, 1);
            }

            ptr::copy_nonoverlapping(&tmp, right_ptr, 1);
            mem::forget(tmp);
        }

        dbg_print_2!("\n");
    };
}

/// See [`Partition::small_partition`].
///
/// Optimized for minimal moves.
fn small_partition_move_opt<T, F>(v: &mut [T], pivot: &T, is_less: &mut F) -> usize
where
    F: FnMut(&T, &T) -> bool,
{
    let len = v.len();
    let arr_ptr = v.as_mut_ptr();

    if len >= MAX_SMALL_PARTITION_LEN {
        debug_assert!(false);
        return 0;
    }

    // Larger types are optimized for a minimal amount of moves and avoid stack arrays with a size
    // dependent on T. It's not crazy fast for something like `u64`, still 2x faster than a simple
    // branchy version. But for things like `String` it's as fast if not faster and it saves on
    // compile-time to only instantiate the other version for types that are likely to benefit.

    // SAFETY: TODO
    unsafe {
        let mut ge_idx_buffer = MaybeUninit::<[u8; MAX_SMALL_PARTITION_LEN]>::uninit();
        let ge_idx_ptr = ge_idx_buffer.as_mut_ptr() as *mut u8;

        let mut lt_idx_buffer = MaybeUninit::<[u8; MAX_SMALL_PARTITION_LEN]>::uninit();
        let mut lt_idx_ptr = (lt_idx_buffer.as_mut_ptr() as *mut u8).add(len);

        let mut ge_count = 0;

        for i in 0..len {
            lt_idx_ptr = lt_idx_ptr.sub(1);

            *ge_idx_ptr.add(ge_count) = i as u8;
            *lt_idx_ptr.add(ge_count) = i as u8;

            let is_ge = !is_less(&*arr_ptr.add(i), pivot);
            ge_count += is_ge as usize;
        }

        let lt_count = len - ge_count;
        lt_idx_ptr = lt_idx_ptr.add(ge_count);

        let mut i = usize::MAX;
        cyclic_permutation_swap_loop!(
            {
                // continue_check
                i = i.wrapping_add(1);
                i < lt_count && (*ge_idx_ptr.add(i) as usize) < lt_count
            },
            {
                // next_left
                arr_ptr.add(*ge_idx_ptr.add(i) as usize)
            },
            {
                // next_right
                arr_ptr.add(*lt_idx_ptr.add(i) as usize)
            },
            v.as_ptr()
        );

        lt_count
    }
}

/// Scan elements `base_ptr[..block_len]` up and build a bitset that has the corresponding bit
/// toggled depending on `is_swap_elem`.
///
/// Written in a way that enables reliable auto-vectorization by the compiler if wide enough SIMD is
/// available.
///
/// SAFETY: The caller must ensure that `base_ptr[..block_len]` is valid to read.TODO update
#[inline(always)]
unsafe fn fill_offset_block_up<const BLOCK: usize, T>(
    base_ptr: *const T,
    offset_out_ptr: *mut u8,
    is_swap_elem: &mut impl FnMut(&T) -> bool,
) -> (*mut u8, *mut u8) {
    // This tries to exploit ILP by filling a block up and down simultaneously allowing for better
    // efficiency on some micro-architectures, compared to a simple fixed size loop that is
    // unrolled.
    //
    // Scans upwards suited for left side block generation.

    // TODO explain.
    const SUB_BLOCK: usize = 8;
    debug_assert!(BLOCK % SUB_BLOCK == 0);
    debug_assert!(BLOCK >= SUB_BLOCK);

    let mut up_ptr = offset_out_ptr.add(BLOCK / 2);
    let mut down_ptr = offset_out_ptr.add((BLOCK / 2) - 1);

    for i in 0..(BLOCK / 2) {
        let up_i = i + (BLOCK / 2);
        *up_ptr = up_i as u8;
        let is_se = is_swap_elem(&*base_ptr.add(up_i));
        up_ptr = up_ptr.add(is_se as usize);

        let down_i = ((BLOCK / 2) - 1) - i;
        *down_ptr = down_i as u8;
        let is_se = is_swap_elem(&*base_ptr.add(down_i));
        down_ptr = down_ptr.sub(is_se as usize);
    }

    // for s_i in 0..(BLOCK / SUB_BLOCK) {
    //     let sub_block_offset = s_i * SUB_BLOCK;
    //     for i in 0..(SUB_BLOCK / 2) {
    //         let up_i = sub_block_offset + (i + (SUB_BLOCK / 2));
    //         *up_ptr = up_i as u8;
    //         let is_se = is_swap_elem(&*base_ptr.add(up_i));
    //         up_ptr = up_ptr.add(is_se as usize);

    //         let down_i = sub_block_offset + (((SUB_BLOCK / 2) - 1) - i);
    //         *down_ptr = down_i as u8;
    //         let is_se = is_swap_elem(&*base_ptr.add(down_i));
    //         down_ptr = down_ptr.sub(is_se as usize);
    //     }
    // }

    (down_ptr.add(1), up_ptr)
}

/// Scan elements `base_ptr[..block_len]` down and build a bitset that has the corresponding bit
/// toggled depending on `is_swap_elem`.
///
/// Written in a way that enables reliable auto-vectorization by the compiler if wide enough SIMD is
/// available.
///
/// SAFETY: The caller must ensure that `base_ptr[..block_len]` is valid to read.TODO update
#[inline(always)]
unsafe fn fill_offset_block_down<const BLOCK: usize, T>(
    base_ptr: *const T,
    mut offset_out_ptr: *mut u8,
    is_swap_elem: &mut impl FnMut(&T) -> bool,
) -> (*mut u8, *mut u8) {
    // This tries to exploit ILP by filling a block up and down simultaneously allowing for better
    // efficiency on some micro-architectures, compared to a simple fixed size loop that is
    // unrolled.
    //
    // Scans downwards suited for right side block generation, because on some micro-architectures
    // repeated access in one direction may prompt the prefetcher to do unnecessary work greatly
    // reducing efficiency. It's important that the saved offsets also go downwards.

    // TODO explain.
    const SUB_BLOCK: usize = 8;
    debug_assert!(BLOCK % SUB_BLOCK == 0);
    debug_assert!(BLOCK >= SUB_BLOCK);

    let mut up_ptr = offset_out_ptr.add(BLOCK / 2);
    let mut down_ptr = offset_out_ptr.add((BLOCK / 2) - 1);

    for s_i in (0..(BLOCK / SUB_BLOCK)).rev() {
        let sub_block_offset = s_i * SUB_BLOCK;
        for i in 0..(SUB_BLOCK / 2) {
            let up_i = sub_block_offset + (((SUB_BLOCK / 2) - 1) - i);
            *up_ptr = up_i as u8;
            let is_se = is_swap_elem(&*base_ptr.add(up_i));
            up_ptr = up_ptr.add(is_se as usize);

            let down_i = sub_block_offset + (i + (SUB_BLOCK / 2));
            *down_ptr = down_i as u8;
            let is_se = is_swap_elem(&*base_ptr.add(down_i));
            down_ptr = down_ptr.sub(is_se as usize);
        }
    }

    (down_ptr.add(1), up_ptr)

    // let offset_base_ptr = offset_out_ptr;
    // let mut elem_ptr = base_ptr.add(BLOCK);

    // for i in 0..BLOCK {
    //     elem_ptr = elem_ptr.sub(1);
    //     *offset_out_ptr = ((BLOCK - 1) - i) as u8;
    //     let is_se = is_swap_elem(&*elem_ptr);
    //     offset_out_ptr = offset_out_ptr.add(is_se as usize);
    // }

    // dbg_print!(
    //     "{:?}\n",
    //     &*ptr::slice_from_raw_parts(offset_out_ptr, offset_out_ptr.sub_ptr(offset_base_ptr))
    // );

    // (offset_base_ptr, offset_out_ptr)
}

#[inline(always)]
unsafe fn fill_offset_block_up_simple<const BLOCK: usize, T>(
    base_ptr: *const T,
    mut offset_out_ptr: *mut u8,
    is_swap_elem: &mut impl FnMut(&T) -> bool,
) -> (*mut u8, *mut u8) {
    let offset_base_ptr = offset_out_ptr;

    const UNROLL: usize = 8; // TODO type dependent.
    debug_assert!(BLOCK % UNROLL == 0);
    debug_assert!(BLOCK >= UNROLL);

    for unroll_i in 0..(BLOCK / UNROLL) {
        let unroll_offset = unroll_i * UNROLL;

        for i in 0..UNROLL {
            let up_i = unroll_offset + i;
            *offset_out_ptr = up_i as u8;
            let is_se = is_swap_elem(&*base_ptr.add(up_i));
            offset_out_ptr = offset_out_ptr.add(is_se as usize);
        }
    }

    (offset_base_ptr, offset_out_ptr)
}

#[inline(always)]
unsafe fn fill_offset_block_down_simple<const BLOCK: usize, T>(
    base_ptr: *const T,
    mut offset_out_ptr: *mut u8,
    is_swap_elem: &mut impl FnMut(&T) -> bool,
) -> (*mut u8, *mut u8) {
    let offset_base_ptr = offset_out_ptr;

    const UNROLL: usize = 8; // TODO type dependent.
    debug_assert!(BLOCK % UNROLL == 0);
    debug_assert!(BLOCK >= UNROLL);

    // TODO use better code-gen for debug instead of rev.
    for unroll_i in (0..(BLOCK / UNROLL)).rev() {
        let unroll_offset = unroll_i * UNROLL;

        for i in 0..UNROLL {
            let down_i = unroll_offset + ((UNROLL - 1) - i);
            *offset_out_ptr = down_i as u8;
            let is_se = is_swap_elem(&*base_ptr.add(down_i));
            offset_out_ptr = offset_out_ptr.add(is_se as usize);
        }
    }

    (offset_base_ptr, offset_out_ptr)
}

// TODO remove
macro_rules! dbg_print {
    ($fmt_expr:expr $(,$extra_args:expr)*) => {{
        // use std::io::{self, Write};
        // io::stdout()
        //     .write_all(format!($fmt_expr, $($extra_args,)*).as_bytes())
        //     .unwrap();
        // io::stdout().flush().unwrap();

        // println!($fmt_expr, $($extra_args,)*);
        let _ = ($fmt_expr, $($extra_args,)*);
    }};
}

/// TODO doc
fn block_partition<'a, T, F>(v: &'a mut [T], pivot: &T, is_less: &mut F) -> &'a mut [T]
where
    F: FnMut(&T, &T) -> bool,
{
    const BLOCK: usize = BLOCK_PARTITION_BLOCK_SIZE;

    let len = v.len();
    let arr_ptr = v.as_mut_ptr();

    // TODO unify
    assert!(MAX_SMALL_PARTITION_LEN >= BLOCK * 2);
    if len < MAX_SMALL_PARTITION_LEN {
        return v;
    }

    dbg_print!("");

    // SAFETY: TODO
    unsafe {
        let mut l_offset_storate = MaybeUninit::<[u8; BLOCK]>::uninit();
        let l_offset_base_ptr = l_offset_storate.as_mut_ptr() as *mut u8;
        let mut l_offset_start_ptr = l_offset_base_ptr;
        let mut l_offset_end_ptr = l_offset_base_ptr;

        let mut r_offset_storate = MaybeUninit::<[u8; BLOCK]>::uninit();
        let r_offset_base_ptr = r_offset_storate.as_mut_ptr() as *mut u8;
        let mut r_offset_start_ptr = r_offset_base_ptr;
        let mut r_offset_end_ptr = r_offset_base_ptr;

        let mut l_ptr = arr_ptr;
        let mut r_ptr = arr_ptr.add(len - BLOCK);

        // It's crucial for reliable auto-vectorization that BLOCK always stays the same. Which
        // means we handle the rest of the input size separately later.

        // If the region we will look at during this loop iteration overlaps we are done.
        while l_ptr.add(BLOCK) <= r_ptr {
            // loop {
            // While interleaving left and right side access would be possible, experiments show
            // that on Zen3 this has significantly worse performance, and the CPU prefers working on
            // one region of memory followed by another.
            if l_offset_start_ptr == l_offset_end_ptr {
                (l_offset_start_ptr, l_offset_end_ptr) =
                    fill_offset_block_up::<BLOCK, T>(l_ptr, l_offset_base_ptr, &mut |elem| {
                        !is_less(elem, pivot)
                    });
            }

            if r_offset_start_ptr == r_offset_end_ptr {
                (r_offset_start_ptr, r_offset_end_ptr) =
                    fill_offset_block_down::<BLOCK, T>(r_ptr, r_offset_base_ptr, &mut |elem| {
                        is_less(elem, pivot)
                    });
            }

            let swap_count = cmp::min(
                l_offset_end_ptr.sub_ptr(l_offset_start_ptr),
                r_offset_end_ptr.sub_ptr(r_offset_start_ptr),
            );

            // type DebugT = i32;
            // dbg_print!("{:?}", mem::transmute::<&[T], &[DebugT]>(v));
            dbg_print!("{:?}", swap_count);
            dbg_print!(
                "{:?}",
                &*ptr::slice_from_raw_parts(l_offset_base_ptr as *const u8, swap_count,)
            );
            dbg_print!(
                "{:?}",
                &*ptr::slice_from_raw_parts(r_offset_base_ptr as *const u8, swap_count,)
            );

            // TODO try out version that is manually unrolled to two.
            let mut i = usize::MAX;
            cyclic_permutation_swap_loop!(
                {
                    // continue_check
                    i = i.wrapping_add(1);
                    i < swap_count
                },
                {
                    // next_left
                    l_ptr.add(*l_offset_start_ptr.add(i) as usize)
                },
                {
                    // next_right
                    r_ptr.add(*r_offset_start_ptr.add(i) as usize)
                },
                v.as_ptr()
            );

            l_offset_start_ptr = l_offset_start_ptr.add(swap_count);
            r_offset_start_ptr = r_offset_start_ptr.add(swap_count);

            l_ptr = l_ptr.add((l_offset_start_ptr == l_offset_end_ptr) as usize * BLOCK);
            r_ptr = r_ptr.sub((r_offset_start_ptr == r_offset_end_ptr) as usize * BLOCK);

            // dbg_print!(
            //     "l_ptr offset: {} r_ptr offset: {}",
            //     l_ptr.sub_ptr(arr_ptr),
            //     r_ptr.sub_ptr(arr_ptr)
            // );
        }

        let l_adjusted_ptr = l_ptr;
        let r_end_ptr = r_ptr.add(BLOCK);
        let un_partitioned_count = r_end_ptr.sub_ptr(l_adjusted_ptr);

        // TODO usage, fuse with small partitions.

        &mut *ptr::slice_from_raw_parts_mut(l_adjusted_ptr, un_partitioned_count)
    }
}

/// Takes the input slice `v` and re-arranges elements such that when the call returns normally
/// all elements that compare true for `is_less(elem, pivot)` are on the left side of `v`
/// followed by the other elements, notionally considered greater or equal to `pivot`.
///
/// Returns the number of elements that are compared true for `is_less(elem, pivot)`.
///
/// If `is_less` does not implement a total order the resulting order and return value are
/// unspecified. All original elements will remain in `v` and any possible modifications via
/// interior mutability will be observable. Same is true if `is_less` panics.
#[cfg_attr(feature = "no_inline_sub_functions", inline(never))]
fn partition<T, F: FnMut(&T, &T) -> bool>(v: &mut [T], pivot: &T, is_less: &mut F) -> usize {
    // This partition implementation combines various ideas to strike a good balance optimizing for
    // all the following:
    //
    // - performance/efficiency
    // - compile-time
    // - binary-size
    // - wide range of input lengths
    // - diverse types (integers, Strings, big stack arrays, etc.)
    // - various ISAs (x86, Arm, RISC-V, etc.)

    // High level overview and motivation:
    //
    // There are two main components, a small_partition implementation that is optimized for small
    // input sizes and can only handle up to `MAX_SMALL_PARTITION_LEN` elements. A block_partition
    // implementation optimized for consistent high throughput for larger sizes that may leave a
    // small region in the middle of the input slice un-partitioned. Either the input slice length
    // is small enough to be handled entirely by the small_partition, or it first handles most of
    // the input with the block_partition and the remaining hole with the small_partition. This
    // allows both components to be specialized and limits binary-size as well as branching overhead
    // to handle various scenarios commonly involved when handling the remainder of some block based
    // partition scheme. This scheme also allows the block_partition to use runtime feature
    // detection to leverage SIMD to speed up fixed block size processing, while only having to
    // double instantiate the block processing part and not the remainder handling which doesn't
    // benefit from it. Further, only calling block_partition for larger input length amortizes the
    // cost of runtime feature detection and last block handling. The implementations use heuristics
    // based on properties like input type size as well as Freeze and Copy traits to choose between
    // implementation strategies as appropriate, this can be seen as a form of type introspection
    // based programming. Using a block based partition scheme combined with a cyclic permutation is
    // a good fit for generic Rust implementation because it's trivial to prove panic- and
    // observation-safe as it disconnects, calling the user-provided comparison function which may
    // panic and or modify the values that are being compared, with creating temporary copies. In
    // addition using a cyclic permutation and only swapping values that need to be swapped is
    // efficient for both small types like integers and arbitrarily large user-defined types, as
    // well as cases where the input is already fully or nearly partitioned as may happen when
    // filtering out common values in a pdqsort style equal partition.

    // Influences:
    //
    // Many of the component ideas at play here are not novel and some were researched and
    // discovered independently to prior art.
    //
    // block_partition is fundamentally a Hoare partition, which Stefan Edelkamp and Armin Weiß used
    // in their paper "BlockQuicksort: How Branch Mispredictions don’t affect Quicksort"
    // [https://arxiv.org/pdf/1604.06697.pdf] and added unrolled block level processing, branchless
    // offset generation and cyclic permutation based swapping. Orson Peters used this in his paper
    // "Pattern-defeating Quicksort" [https://arxiv.org/pdf/2106.05123.pdf] and refined the
    // code-gen. The work on pdqsort was then used as the starting point for the work done by
    // Min-Jae Hwang in Bitset Sort [https://github.com/minjaehwang/bitsetsort] which changes the
    // block offset calculation in a way that allows for reliable compiler auto-vectorization, but
    // requires wider SIMD to be available than the default x86 and Arm LLVM targets include by
    // default, to benefit from auto-vectorization. This then formed the basis for the work by Lukas
    // Bergdoll on this block_partition which refines code-gen and adds double instantiation, one
    // default one and one with for example x86 AVX code-gen enabled paired with type introspection
    // heuristics to avoid generating the double instantiation for types like String which will not
    // auto-vectorize anyway. As well as a way to avoid the double instantiation and runtime feature
    // detection entirely if compiled with flags that allow wide enough SIMD anyway, allowing for a
    // kind of static dispatch. TODO update.
    //
    // small_partition is two entirely different partition implementations that use type
    // introspection to choose between them at compile time. One is focused on integer like types
    // and is based on research in sort-research-rs
    // [https://github.com/Voultapher/sort-research-rs/blob/c9f5ce28ff5705f119e0fab0626792304f36eecd/src/other/partition/small_fast.rs]
    // later refined in driftsort by Orson Peters and Lukas Bergdoll [TODO link]. The other version
    // focused on minimal moves is a novel design by the author that does a single scan with
    // code-gen influenced by driftsort followed by a cyclic permutation with an early exit, doing
    // the bare minimum moves.

    let arr_ptr = v.as_ptr();

    // TODO remove
    type DebugT = i32;
    dbg_print!("v before:     {:?}", unsafe {
        mem::transmute::<&[T], &[DebugT]>(v)
    });
    let remaining_v = block_partition(v, pivot, is_less);

    // dbg_print!("v after:      {:?}", unsafe {
    //     mem::transmute::<&[T], &[DebugT]>(v)
    // });
    dbg_print!("remaining_v: {:?}", unsafe {
        mem::transmute::<&[T], &[DebugT]>(remaining_v)
    });

    // SAFETY: block_partition is assumed to return a sub-slice of v.
    let lt_block_count = unsafe { remaining_v.as_ptr().sub_ptr(arr_ptr) };

    lt_block_count + <T as Partition>::small_partition(remaining_v, pivot, is_less)
}
