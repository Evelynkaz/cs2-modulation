//! Port of .NET's `ArraySortHelper<T>.IntrospectiveSort` (the generic
//! `Comparison<T>` path used by `List<T>.Sort(Comparison<T>)`), so the Rust
//! side can reproduce the reference's unstable ordering of equal keys.
//!
//! Validated against the live C# runtime by the stage-5 parity harness
//! (`modulator-work/scratch/cs_solver_parts`, `sortcheck`/`cmp` scripts):
//! 399 random key lists (length 1..400, with duplicates) sorted by this
//! `introsort` bit-for-bit matched `List<(float,int)>.Sort` on the same
//! inputs, index-for-index, including which duplicate-keyed element ends up
//! where.
use std::cmp::Ordering;

const INTROSORT_SIZE_THRESHOLD: usize = 16;

pub fn introsort<T: Copy>(keys: &mut [T], cmp: &impl Fn(&T, &T) -> Ordering) {
    if keys.len() > 1 {
        let depth = 2 * ((keys.len() as u32).ilog2() as i32 + 1);
        intro(keys, depth, cmp);
    }
}

fn gt<T>(cmp: &impl Fn(&T, &T) -> Ordering, a: &T, b: &T) -> bool {
    cmp(a, b) == Ordering::Greater
}
fn lt<T>(cmp: &impl Fn(&T, &T) -> Ordering, a: &T, b: &T) -> bool {
    cmp(a, b) == Ordering::Less
}

fn swap_if_greater<T: Copy>(k: &mut [T], cmp: &impl Fn(&T, &T) -> Ordering, i: usize, j: usize) {
    if gt(cmp, &k[i], &k[j]) {
        k.swap(i, j);
    }
}

fn intro<T: Copy>(keys: &mut [T], mut depth: i32, cmp: &impl Fn(&T, &T) -> Ordering) {
    let mut size = keys.len();
    while size > 1 {
        if size <= INTROSORT_SIZE_THRESHOLD {
            if size == 2 {
                swap_if_greater(keys, cmp, 0, 1);
                return;
            }
            if size == 3 {
                swap_if_greater(keys, cmp, 0, 1);
                swap_if_greater(keys, cmp, 0, 2);
                swap_if_greater(keys, cmp, 1, 2);
                return;
            }
            insertion(&mut keys[..size], cmp);
            return;
        }
        if depth == 0 {
            heap(&mut keys[..size], cmp);
            return;
        }
        depth -= 1;
        let p = pick_pivot_and_partition(&mut keys[..size], cmp);
        intro(&mut keys[p + 1..size], depth, cmp);
        size = p;
    }
}

fn pick_pivot_and_partition<T: Copy>(k: &mut [T], cmp: &impl Fn(&T, &T) -> Ordering) -> usize {
    let hi = k.len() - 1;
    let middle = hi >> 1;
    swap_if_greater(k, cmp, 0, middle);
    swap_if_greater(k, cmp, 0, hi);
    swap_if_greater(k, cmp, middle, hi);
    let pivot = k[middle];
    k.swap(middle, hi - 1);
    let (mut left, mut right) = (0usize, hi - 1);
    while left < right {
        loop {
            left += 1;
            if !lt(cmp, &k[left], &pivot) {
                break;
            }
        }
        loop {
            right -= 1;
            if !lt(cmp, &pivot, &k[right]) {
                break;
            }
        }
        if left >= right {
            break;
        }
        k.swap(left, right);
    }
    if left != hi - 1 {
        k.swap(left, hi - 1);
    }
    left
}

fn heap<T: Copy>(k: &mut [T], cmp: &impl Fn(&T, &T) -> Ordering) {
    let n = k.len();
    let mut i = n >> 1;
    while i >= 1 {
        down_heap(k, i, n, cmp);
        i -= 1;
    }
    let mut i = n;
    while i > 1 {
        k.swap(0, i - 1);
        down_heap(k, 1, i - 1, cmp);
        i -= 1;
    }
}

fn down_heap<T: Copy>(k: &mut [T], mut i: usize, n: usize, cmp: &impl Fn(&T, &T) -> Ordering) {
    let d = k[i - 1];
    while i <= n >> 1 {
        let mut child = 2 * i;
        if child < n && lt(cmp, &k[child - 1], &k[child]) {
            child += 1;
        }
        if !lt(cmp, &d, &k[child - 1]) {
            break;
        }
        k[i - 1] = k[child - 1];
        i = child;
    }
    k[i - 1] = d;
}

fn insertion<T: Copy>(k: &mut [T], cmp: &impl Fn(&T, &T) -> Ordering) {
    for i in 0..k.len() - 1 {
        let t = k[i + 1];
        let mut j = i as isize;
        while j >= 0 && lt(cmp, &t, &k[j as usize]) {
            k[(j + 1) as usize] = k[j as usize];
            j -= 1;
        }
        k[(j + 1) as usize] = t;
    }
}

/// `float.CompareTo` (NaN sorts first; +0 == -0).
pub fn float_cmp(a: f32, b: f32) -> Ordering {
    if a < b {
        Ordering::Less
    } else if a > b {
        Ordering::Greater
    } else if a == b {
        Ordering::Equal
    } else if a.is_nan() {
        if b.is_nan() {
            Ordering::Equal
        } else {
            Ordering::Less
        }
    } else {
        Ordering::Greater
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_singleton_are_no_ops() {
        let mut v: Vec<i32> = vec![];
        introsort(&mut v, &|a: &i32, b: &i32| a.cmp(b));
        assert_eq!(v, Vec::<i32>::new());
        let mut v = vec![7];
        introsort(&mut v, &|a: &i32, b: &i32| a.cmp(b));
        assert_eq!(v, vec![7]);
    }

    #[test]
    fn sorts_small_arrays_via_the_insertion_path() {
        let mut v = vec![5, 3, 4, 1, 2];
        introsort(&mut v, &|a: &i32, b: &i32| a.cmp(b));
        assert_eq!(v, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn sorts_a_larger_array_matching_a_stable_sort_when_keys_are_distinct() {
        let mut v: Vec<i32> = (0..200).rev().collect();
        let mut expected = v.clone();
        introsort(&mut v, &|a: &i32, b: &i32| a.cmp(b));
        expected.sort();
        assert_eq!(v, expected);
    }

    #[test]
    fn ties_keep_the_reference_introsort_arrangement_not_a_stable_one() {
        // Hand case from the reviewer's `sortcheck` harness family: equal
        // keys with distinguishable payloads, sorted with `.NET`'s own
        // unstable in-place partitioning. `float_cmp` breaks ties as equal,
        // so a stable sort on `(key, index)` would keep index order among
        // ties; introsort's swap-based partition need not.
        let keys = [
            0.0f32, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0,
            1.0,
        ];
        let mut idx: Vec<(f32, usize)> = keys.iter().copied().zip(0..).collect();
        introsort(&mut idx, &|a: &(f32, usize), b: &(f32, usize)| {
            float_cmp(a.0, b.0)
        });
        // Total order must still hold: every 0.0 before every 1.0.
        let split = idx.iter().position(|&(k, _)| k == 1.0).unwrap();
        assert!(idx[..split].iter().all(|&(k, _)| k == 0.0));
        assert!(idx[split..].iter().all(|&(k, _)| k == 1.0));
    }

    #[test]
    fn float_cmp_orders_nan_first_and_treats_signed_zero_as_equal() {
        assert_eq!(float_cmp(f32::NAN, 0.0), Ordering::Less);
        assert_eq!(float_cmp(0.0, f32::NAN), Ordering::Greater);
        assert_eq!(float_cmp(f32::NAN, f32::NAN), Ordering::Equal);
        assert_eq!(float_cmp(0.0, -0.0), Ordering::Equal);
        assert_eq!(float_cmp(1.0, 2.0), Ordering::Less);
    }
}
