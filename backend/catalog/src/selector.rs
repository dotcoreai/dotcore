//! Specificity between selectors, the typed conditions over execution target facts.
//!
//! Comparing two selectors tells whether every target one matches is also matched by the other,
//! so catalog validation can reject declarations that would compete for the same target and
//! resolution can pick the most specific of several matches. The
//! comparison is pure and reads only the dimensions selectors match on; the NVIDIA driver minimum
//! is a requirement checked after a match and never changes the result.

use std::collections::BTreeSet;

use dotcore_proto::dotcore::{
    catalog::v1::{OciPlatform, Selector},
    observation::v1::AcceleratorMemoryArchitecture,
};

/// How the targets one selector matches relate to the targets another matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectorRelation {
    /// Both match exactly the same targets.
    Equal,

    /// The first matches a strict subset of the second's targets, so it is more specific.
    Narrower,

    /// The first matches a strict superset of the second's targets, so it is less specific.
    Wider,

    /// No target matches both.
    Disjoint,

    /// Some target matches both, but neither selector contains the other.
    Overlapping,
}

impl SelectorRelation {
    /// Whether at least one target matches both selectors.
    pub fn can_match_same_target(self) -> bool {
        self != Self::Disjoint
    }
}

/// Compares the targets `first` matches with the targets `second` matches. A dimension a
/// selector leaves unset matches any value, so it is wider than any set value.
pub fn selector_relation(first: &Selector, second: &Selector) -> SelectorRelation {
    combine([
        platform_relation(first.platform.as_ref(), second.platform.as_ref()),
        accelerator_relation(&first.accelerators, &second.accelerators),
        memory_architecture_relation(first.memory_architecture(), second.memory_architecture()),
    ])
}

/// Combines per-dimension relations: selectors match a product of their dimensions, so one
/// contains the other only when it does so in every dimension.
fn combine(dimensions: [SelectorRelation; 3]) -> SelectorRelation {
    let has = |relation| dimensions.contains(&relation);
    if has(SelectorRelation::Disjoint) {
        SelectorRelation::Disjoint
    } else if has(SelectorRelation::Overlapping)
        || (has(SelectorRelation::Narrower) && has(SelectorRelation::Wider))
    {
        SelectorRelation::Overlapping
    } else if has(SelectorRelation::Narrower) {
        SelectorRelation::Narrower
    } else if has(SelectorRelation::Wider) {
        SelectorRelation::Wider
    } else {
        SelectorRelation::Equal
    }
}

fn platform_relation(
    first: Option<&OciPlatform>,
    second: Option<&OciPlatform>,
) -> SelectorRelation {
    match (first, second) {
        (None, None) => SelectorRelation::Equal,
        (None, Some(_)) => SelectorRelation::Wider,
        (Some(_), None) => SelectorRelation::Narrower,
        (Some(first), Some(second)) if platform_key(first) == platform_key(second) => {
            SelectorRelation::Equal
        }
        (Some(_), Some(_)) => SelectorRelation::Disjoint,
    }
}

fn platform_key(platform: &OciPlatform) -> (&str, &str, &str) {
    (
        &platform.operating_system,
        &platform.architecture,
        platform.variant.as_deref().unwrap_or_default(),
    )
}

/// Relates two accelerator vendor sets, where a target matches when it reports any listed vendor.
fn accelerator_relation(first: &[String], second: &[String]) -> SelectorRelation {
    let first = first.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let second = second.iter().map(String::as_str).collect::<BTreeSet<_>>();
    match (first.is_empty(), second.is_empty()) {
        (true, true) => SelectorRelation::Equal,
        (true, false) => SelectorRelation::Wider,
        (false, true) => SelectorRelation::Narrower,
        (false, false) if first == second => SelectorRelation::Equal,
        (false, false) if first.is_subset(&second) => SelectorRelation::Narrower,
        (false, false) if first.is_superset(&second) => SelectorRelation::Wider,
        (false, false) if first.is_disjoint(&second) => SelectorRelation::Disjoint,
        (false, false) => SelectorRelation::Overlapping,
    }
}

/// Relates two memory architecture conditions, where the unspecified value matches either one.
fn memory_architecture_relation(
    first: AcceleratorMemoryArchitecture,
    second: AcceleratorMemoryArchitecture,
) -> SelectorRelation {
    use AcceleratorMemoryArchitecture::Unspecified;
    match (first, second) {
        (first, second) if first == second => SelectorRelation::Equal,
        (Unspecified, _) => SelectorRelation::Wider,
        (_, Unspecified) => SelectorRelation::Narrower,
        _ => SelectorRelation::Disjoint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_dimension_relations() {
        let cases = [
            (
                "equal",
                Some(LINUX_AMD64),
                Some(LINUX_AMD64),
                SelectorRelation::Equal,
            ),
            (
                "narrower",
                Some(LINUX_AMD64),
                None,
                SelectorRelation::Narrower,
            ),
            ("wider", None, Some(LINUX_AMD64), SelectorRelation::Wider),
            (
                "disjoint",
                Some(LINUX_AMD64),
                Some(LINUX_ARM64),
                SelectorRelation::Disjoint,
            ),
            ("both unset", None, None, SelectorRelation::Equal),
        ];
        for (label, first, second, expected) in cases {
            assert_eq!(
                selector_relation(
                    &selector(first, &["nvidia"]),
                    &selector(second, &["nvidia"])
                ),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn platform_variant_is_part_of_the_platform() {
        let mut variant = selector(Some(LINUX_ARM64), &["nvidia"]);
        variant.platform.as_mut().expect("platform").variant = Some("v8".to_owned());
        let mut empty_variant = selector(Some(LINUX_ARM64), &["nvidia"]);
        empty_variant.platform.as_mut().expect("platform").variant = Some(String::new());

        assert_eq!(
            selector_relation(&variant, &selector(Some(LINUX_ARM64), &["nvidia"])),
            SelectorRelation::Disjoint
        );
        assert_eq!(
            selector_relation(&empty_variant, &selector(Some(LINUX_ARM64), &["nvidia"])),
            SelectorRelation::Equal
        );
    }

    #[test]
    fn accelerator_set_relations() {
        let cases: [(&str, &[&str], &[&str], SelectorRelation); 7] = [
            ("equal", &["nvidia"], &["nvidia"], SelectorRelation::Equal),
            (
                "equal in another order",
                &["nvidia", "apple"],
                &["apple", "nvidia"],
                SelectorRelation::Equal,
            ),
            (
                "narrower",
                &["nvidia"],
                &["nvidia", "apple"],
                SelectorRelation::Narrower,
            ),
            (
                "wider",
                &["nvidia", "apple"],
                &["nvidia"],
                SelectorRelation::Wider,
            ),
            (
                "disjoint",
                &["nvidia"],
                &["apple"],
                SelectorRelation::Disjoint,
            ),
            (
                "overlapping not nested",
                &["nvidia", "apple"],
                &["apple", "amd"],
                SelectorRelation::Overlapping,
            ),
            (
                "narrower than unset",
                &["nvidia"],
                &[],
                SelectorRelation::Narrower,
            ),
        ];
        for (label, first, second, expected) in cases {
            assert_eq!(
                selector_relation(
                    &selector(Some(LINUX_AMD64), first),
                    &selector(Some(LINUX_AMD64), second)
                ),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn dimensions_combine_into_one_relation() {
        let cases: [(&str, Selector, Selector, SelectorRelation); 5] = [
            (
                "narrower in both",
                selector(Some(LINUX_AMD64), &["nvidia"]),
                selector(None, &["nvidia", "apple"]),
                SelectorRelation::Narrower,
            ),
            (
                "wider in one, equal in the other",
                selector(None, &["nvidia"]),
                selector(Some(LINUX_AMD64), &["nvidia"]),
                SelectorRelation::Wider,
            ),
            (
                "platform narrower, accelerators wider",
                selector(Some(LINUX_AMD64), &["nvidia", "apple"]),
                selector(None, &["nvidia"]),
                SelectorRelation::Overlapping,
            ),
            (
                "platform equal, accelerators overlapping",
                selector(Some(LINUX_AMD64), &["nvidia", "apple"]),
                selector(Some(LINUX_AMD64), &["apple", "amd"]),
                SelectorRelation::Overlapping,
            ),
            (
                "disjoint platform wins over wider accelerators",
                selector(Some(LINUX_AMD64), &[]),
                selector(Some(LINUX_ARM64), &["nvidia"]),
                SelectorRelation::Disjoint,
            ),
        ];
        for (label, first, second, expected) in cases {
            assert_eq!(selector_relation(&first, &second), expected, "{label}");
        }
    }

    #[test]
    fn memory_architecture_relations() {
        use AcceleratorMemoryArchitecture::{Dedicated, Unified, Unspecified};
        let cases = [
            ("unified equal", Unified, Unified, SelectorRelation::Equal),
            (
                "dedicated equal",
                Dedicated,
                Dedicated,
                SelectorRelation::Equal,
            ),
            (
                "unset equal",
                Unspecified,
                Unspecified,
                SelectorRelation::Equal,
            ),
            (
                "unified narrower than unset",
                Unified,
                Unspecified,
                SelectorRelation::Narrower,
            ),
            (
                "dedicated narrower than unset",
                Dedicated,
                Unspecified,
                SelectorRelation::Narrower,
            ),
            (
                "unset wider than unified",
                Unspecified,
                Unified,
                SelectorRelation::Wider,
            ),
            (
                "unified and dedicated disjoint",
                Unified,
                Dedicated,
                SelectorRelation::Disjoint,
            ),
        ];
        for (label, first, second, expected) in cases {
            assert_eq!(
                selector_relation(
                    &with_memory(selector(None, &["nvidia"]), first),
                    &with_memory(selector(None, &["nvidia"]), second)
                ),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn memory_architecture_combines_with_the_other_dimensions() {
        use AcceleratorMemoryArchitecture::{Dedicated, Unified};
        let unified = with_memory(selector(None, &["nvidia"]), Unified);
        let dedicated = with_memory(selector(None, &["nvidia"]), Dedicated);

        assert_eq!(
            selector_relation(&unified, &selector(None, &["nvidia", "apple"])),
            SelectorRelation::Narrower
        );
        assert_eq!(
            selector_relation(
                &with_memory(selector(Some(LINUX_ARM64), &["nvidia"]), Dedicated),
                &unified
            ),
            SelectorRelation::Disjoint
        );
        assert_eq!(
            selector_relation(&dedicated, &selector(Some(LINUX_AMD64), &["nvidia"])),
            SelectorRelation::Overlapping
        );
    }

    #[test]
    fn driver_minimum_does_not_affect_the_relation() {
        let mut newer_driver = selector(Some(LINUX_AMD64), &["nvidia"]);
        newer_driver.minimum_nvidia_driver_major_version = 590;
        let mut no_driver = selector(None, &["nvidia"]);
        no_driver.minimum_nvidia_driver_major_version = 0;

        assert_eq!(
            selector_relation(&newer_driver, &selector(Some(LINUX_AMD64), &["nvidia"])),
            SelectorRelation::Equal
        );
        assert_eq!(
            selector_relation(&newer_driver, &no_driver),
            SelectorRelation::Narrower
        );
        assert!(!SelectorRelation::Disjoint.can_match_same_target());
        assert!(SelectorRelation::Narrower.can_match_same_target());
    }

    const LINUX_AMD64: (&str, &str) = ("linux", "amd64");

    const LINUX_ARM64: (&str, &str) = ("linux", "arm64");

    fn selector(platform: Option<(&str, &str)>, accelerators: &[&str]) -> Selector {
        Selector {
            platform: platform.map(|(operating_system, architecture)| OciPlatform {
                operating_system: operating_system.to_owned(),
                architecture: architecture.to_owned(),
                variant: None,
            }),
            accelerators: accelerators
                .iter()
                .map(|&vendor| vendor.to_owned())
                .collect(),
            minimum_nvidia_driver_major_version: 580,
            memory_architecture: AcceleratorMemoryArchitecture::Unspecified.into(),
        }
    }

    fn with_memory(
        mut selector: Selector,
        architecture: AcceleratorMemoryArchitecture,
    ) -> Selector {
        selector.set_memory_architecture(architecture);
        selector
    }
}
