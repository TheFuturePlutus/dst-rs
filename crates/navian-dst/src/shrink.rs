//! Delta-debug shrinking for DST failure traces.
//!
//! When a seeded DST run fails, the seed may drive dozens of candidates, events,
//! or fault injections — but usually only a handful actually matter to the bug.
//! [`ddmin`] is Zeller & Hildebrandt's minimizing delta-debugging algorithm: given
//! a failing input and a reproduction predicate, it returns a **1-minimal** failing
//! subset — one where removing any single element makes the failure disappear.
//! That turns "a 40-element trace fails" into "these 2 elements fail", which is the
//! difference between a triageable and an untriageable DST report.
//!
//! Reference: A. Zeller & R. Hildebrandt, "Simplifying and Isolating
//! Failure-Inducing Input", IEEE TSE 2002 (the `ddmin` algorithm).

/// Shrink a failing input to a 1-minimal failing subset.
///
/// `reproduces(subset)` must return `true` when `subset` still triggers the
/// failure, and must be **side-effect-free / idempotent** — it is called many
/// times on overlapping subsets (and once extra in debug builds for the
/// precondition check below), so any observable side effect would make the result
/// build-profile-dependent. The caller guarantees `reproduces(input) == true`
/// (checked in debug builds). The returned subset preserves input order, still
/// reproduces, and is
/// 1-minimal: removing any single element would make `reproduces` return `false`.
///
/// Deterministic: the same input and (deterministic) predicate always yield the
/// same minimal set. Complexity is polynomial in the input length, not exponential
/// — worst case O(n²) predicate evaluations.
///
/// # Examples
///
/// ```
/// use navian_dst::ddmin;
///
/// // The failure reproduces only when the poison value 42 is present.
/// let input: Vec<i32> = (30..50).collect();
/// let minimal = ddmin(&input, |subset| subset.contains(&42));
/// assert_eq!(minimal, vec![42]);
/// ```
#[must_use = "ddmin returns the minimized failing subset; dropping it discards the shrink result"]
pub fn ddmin<T, F>(input: &[T], mut reproduces: F) -> Vec<T>
where
    T: Clone,
    F: FnMut(&[T]) -> bool,
{
    debug_assert!(
        reproduces(input),
        "ddmin precondition: the full input must reproduce the failure"
    );
    // Degenerate case: if the failure needs nothing at all (the empty input already
    // reproduces), the 1-minimal failing subset is empty. The loop below stops at
    // length 1 and never tries the empty complement, so without this it would return
    // a non-1-minimal singleton. Textbook ddmin assumes the empty input passes; we
    // handle it explicitly so the contract holds unconditionally.
    if reproduces(&[]) {
        return Vec::new();
    }
    let mut current: Vec<T> = input.to_vec();
    let mut granularity = 2usize;

    while current.len() >= 2 {
        let len = current.len();
        let chunk_size = len.div_ceil(granularity);
        // Owned `[start, end)` chunk boundaries so no borrow of `current` is held
        // across the reassignments below.
        let ranges: Vec<(usize, usize)> = (0..len)
            .step_by(chunk_size)
            .map(|start| (start, (start + chunk_size).min(len)))
            .collect();
        let mut reduced = false;

        // 1. Does any single chunk reproduce on its own? (fast, aggressive cut)
        for &(s, e) in &ranges {
            let chunk = current[s..e].to_vec();
            if reproduces(&chunk) {
                current = chunk;
                granularity = 2;
                reduced = true;
                break;
            }
        }

        // 2. Otherwise, does removing any single chunk (its complement) reproduce?
        if !reduced {
            for &(s, e) in &ranges {
                let complement: Vec<T> =
                    current[..s].iter().chain(&current[e..]).cloned().collect();
                if !complement.is_empty() && reproduces(&complement) {
                    current = complement;
                    // Removing one chunk drops granularity by one, floored at 2.
                    granularity = (granularity - 1).max(2);
                    reduced = true;
                    break;
                }
            }
        }

        // 3. Neither worked — refine granularity, or stop when it can't grow.
        if !reduced {
            if granularity >= current.len() {
                break;
            }
            granularity = (granularity * 2).min(current.len());
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrinks_to_the_single_poison_element() {
        // Failure reproduces iff the poison value 42 is present. Input 30..50 holds
        // it; ddmin must peel 20 elements down to exactly [42].
        let input: Vec<i32> = (30..50).collect();
        let minimal = ddmin(&input, |s| s.contains(&42));
        assert_eq!(
            minimal,
            vec![42],
            "must shrink to the lone reproducing element"
        );
    }

    #[test]
    fn finds_a_minimal_two_element_conjunction() {
        // Failure needs BOTH 3 and 17 present (neither alone reproduces). ddmin must
        // return a 2-element, 1-minimal set containing exactly those.
        let input: Vec<i32> = (0..24).collect();
        let repro = |s: &[i32]| s.contains(&3) && s.contains(&17);
        let minimal = ddmin(&input, repro);
        assert_eq!(minimal.len(), 2, "irreducible conjunction has size 2");
        assert!(minimal.contains(&3) && minimal.contains(&17));
        // 1-minimality: dropping either element must make the predicate pass.
        for i in 0..minimal.len() {
            let mut without = minimal.clone();
            without.remove(i);
            assert!(
                !repro(&without),
                "result is not 1-minimal: {without:?} still reproduces"
            );
        }
    }

    #[test]
    fn is_deterministic() {
        let input: Vec<i32> = (0..40).collect();
        let repro = |s: &[i32]| s.contains(&11) && s.contains(&29);
        assert_eq!(ddmin(&input, repro), ddmin(&input, repro));
    }

    #[test]
    fn shrinks_to_empty_when_the_empty_input_already_reproduces() {
        // Degenerate predicate: everything — including the empty set — reproduces.
        // The 1-minimal failing subset is therefore empty, not a leftover singleton.
        let input: Vec<i32> = (0..8).collect();
        let minimal = ddmin(&input, |_s| true);
        assert!(
            minimal.is_empty(),
            "empty input reproduces => minimal set is empty"
        );
    }

    #[test]
    fn keeps_the_whole_input_when_every_element_is_needed() {
        // Reproduces only when ALL elements are present (size-sensitive predicate) —
        // an all-or-nothing failure cannot be shrunk.
        let input: Vec<i32> = (0..6).collect();
        let n = input.len();
        let minimal = ddmin(&input, move |s| s.len() == n);
        assert_eq!(minimal.len(), n);
    }

    #[test]
    fn preserves_input_order() {
        // The 1-minimal set must appear in the same relative order as the input.
        let input: Vec<i32> = vec![9, 2, 7, 5, 3, 8, 1];
        let repro = |s: &[i32]| s.contains(&7) && s.contains(&3);
        let minimal = ddmin(&input, repro);
        assert_eq!(minimal, vec![7, 3], "order of 7 before 3 is preserved");
    }

    #[test]
    fn evaluation_count_is_bounded_not_exponential() {
        let input: Vec<i32> = (0..32).collect();
        let mut evals = 0usize;
        let _ = ddmin(&input, |s| {
            evals += 1;
            s.contains(&7)
        });
        assert!(
            evals < 200,
            "ddmin used {evals} evals — expected well under 200"
        );
    }
}
