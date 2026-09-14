use eyre::{Result, eyre};

pub type EntryId = i64;

/// What to do with a batch that failed its gate.
///
/// A batch of one that fails identifies its culprit exactly. Any larger batch
/// says only that the culprit is somewhere inside, so it is split and both
/// halves are retested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Culprit(EntryId),
    Bisect {
        left: Vec<EntryId>,
        right: Vec<EntryId>,
    },
}

/// Decide the next step after a failed candidate.
///
/// Merge order is preserved in both halves, because a batch is only meaningful
/// as an ordered sequence applied to the base.
pub fn bisect(entries: &[EntryId]) -> Result<Verdict> {
    match entries {
        [] => Err(eyre!("cannot bisect an empty candidate")),
        [only] => Ok(Verdict::Culprit(*only)),
        _ => {
            let (left, right) = entries.split_at(entries.len() / 2);
            Ok(Verdict::Bisect {
                left: left.to_vec(),
                right: right.to_vec(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_candidate_cannot_be_bisected() {
        assert!(bisect(&[]).is_err());
    }

    #[test]
    fn a_batch_of_one_names_its_culprit() {
        assert_eq!(bisect(&[42]).unwrap(), Verdict::Culprit(42));
    }

    #[test]
    fn bisection_preserves_merge_order() {
        let Verdict::Bisect { left, right } = bisect(&[1, 2, 3, 4, 5]).unwrap() else {
            panic!("expected a split");
        };
        assert_eq!(left, vec![1, 2]);
        assert_eq!(right, vec![3, 4, 5]);
        let mut rejoined = left;
        rejoined.extend(right);
        assert_eq!(rejoined, vec![1, 2, 3, 4, 5]);
    }

    /// Worst case is every entry failing, which is the serial case plus the
    /// splits. The point of the batch is that the common case is one run.
    #[test]
    fn bisection_terminates_and_stays_logarithmic() {
        fn rounds_to_isolate(entries: &[EntryId]) -> u32 {
            match bisect(entries).unwrap() {
                Verdict::Culprit(_) => 1,
                Verdict::Bisect { left, .. } => 1 + rounds_to_isolate(&left),
            }
        }

        for n in [1usize, 2, 4, 8, 16, 64] {
            let entries: Vec<EntryId> = (1..=n as i64).collect();
            let expected = n.ilog2() + 1;
            assert_eq!(
                rounds_to_isolate(&entries),
                expected,
                "isolating one culprit in a batch of {n}"
            );
        }
    }
}
