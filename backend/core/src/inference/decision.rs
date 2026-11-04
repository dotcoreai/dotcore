//! Derived fields of decision answers, as the decision contract defines them.
//!
//! A decision answer's probabilities are its primary data; its selected answer and expected score
//! derive from them. Every codec and engine fills a missing derived field through these rules, so
//! the same probabilities always give the same derived answer.

/// Whether a predicate answer is true: its true probability is at least one half.
pub(crate) fn predicate_value(true_probability: f64) -> bool {
    true_probability >= 0.5
}

/// Zero-based index of the most probable option or level, the first one on a tie; none when there
/// are no probabilities.
pub(crate) fn most_probable(probabilities: &[f64]) -> Option<usize> {
    let mut best = None::<(usize, f64)>;
    for (index, probability) in probabilities.iter().enumerate() {
        if best.is_none_or(|(_, value)| *probability > value) {
            best = Some((index, *probability));
        }
    }
    best.map(|(index, _)| index)
}

/// Probability-weighted mean of the zero-based level indexes.
pub(crate) fn expected_level(level_probabilities: &[f64]) -> f64 {
    level_probabilities
        .iter()
        .enumerate()
        .map(|(index, probability)| index as f64 * probability)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_predicate_is_true_from_one_half() {
        assert!(predicate_value(0.5));
        assert!(!predicate_value(0.499));
    }

    #[test]
    fn the_first_most_probable_entry_is_selected() {
        assert_eq!(most_probable(&[0.25, 0.5, 0.25]), Some(1));
        assert_eq!(most_probable(&[0.5, 0.5]), Some(0));
        assert_eq!(most_probable(&[]), None);
    }

    #[test]
    fn the_expected_level_weights_zero_based_indexes() {
        assert_eq!(expected_level(&[0.25, 0.25, 0.5]), 1.25);
    }
}
