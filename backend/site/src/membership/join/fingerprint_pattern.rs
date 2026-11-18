//! The fingerprint pattern: a small symmetric grid of colored cells derived from a key
//! fingerprint, which the approver and the joining machine both show.
//!
//! Matching two patterns is a quick visual check that both screens show the same key, before the
//! operator approves a join. It carries about 37 bits, so it never replaces the fingerprint,
//! which is always shown next to it. Every view gets its pattern from this one derivation.

use crate::node::folder::CertificateFingerprint;

/// Rows and columns of a fingerprint pattern.
pub const FINGERPRINT_PATTERN_SIZE: usize = 5;

/// Columns derived from the fingerprint; the others mirror them.
const DERIVED_COLUMNS: usize = 3;

/// A 5 by 5 grid of cells, symmetric left to right, in three of the six standard terminal
/// colors, derived from a key fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FingerprintPattern {
    /// The three distinct colors the cells use.
    pub colors: [FingerprintColor; 3],

    /// Rows of cells, top first: 0 is empty, 1 to 3 the color at that position in `colors`,
    /// counting from 1.
    pub cells: [[u8; FINGERPRINT_PATTERN_SIZE]; FINGERPRINT_PATTERN_SIZE],
}

impl FingerprintPattern {
    /// Derives the pattern of `fingerprint`. The first three digest bytes pick the colors in
    /// turn, each from the colors not picked yet; the next bytes, two bits per cell with the most
    /// significant bits first, fill the first three columns row by row, and the last two columns
    /// mirror the first two.
    pub fn of(fingerprint: &CertificateFingerprint) -> Self {
        let digest = fingerprint.digest();

        let mut remaining = FingerprintColor::ALL.to_vec();
        let colors = [digest[0], digest[1], digest[2]]
            .map(|byte| remaining.remove(usize::from(byte) % remaining.len()));

        let mut cells = [[0; FINGERPRINT_PATTERN_SIZE]; FINGERPRINT_PATTERN_SIZE];
        for index in 0..FINGERPRINT_PATTERN_SIZE * DERIVED_COLUMNS {
            let bit = 2 * index;
            let slot = (digest[3 + bit / 8] >> (6 - bit % 8)) & 0b11;
            let (row, column) = (index / DERIVED_COLUMNS, index % DERIVED_COLUMNS);
            cells[row][column] = slot;
            cells[row][FINGERPRINT_PATTERN_SIZE - 1 - column] = slot;
        }

        Self { colors, cells }
    }
}

/// One of the six standard terminal colors, so a terminal and the console show the same ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FingerprintColor {
    Red,

    Green,

    Yellow,

    Blue,

    Magenta,

    Cyan,
}

impl FingerprintColor {
    /// Every color, in the order the derivation picks from.
    const ALL: [Self; 6] = [
        Self::Red,
        Self::Green,
        Self::Yellow,
        Self::Blue,
        Self::Magenta,
        Self::Cyan,
    ];
}

#[cfg(test)]
mod tests {
    use super::{FingerprintColor::*, FingerprintPattern};
    use crate::node::folder::CertificateFingerprint;

    #[test]
    fn golden_fingerprints_give_their_exact_colors_and_cells() {
        let cases = [
            (
                "87524a4a3c1f9e0b5d26e8f1a7c4093b6e2d51f8c0a97b34e1d6f2085ab9c7e3",
                FingerprintPattern {
                    colors: [Blue, Yellow, Magenta],
                    cells: [
                        [1, 0, 2, 0, 1],
                        [2, 0, 3, 0, 2],
                        [3, 0, 0, 0, 3],
                        [1, 3, 3, 3, 1],
                        [2, 1, 3, 1, 2],
                    ],
                },
            ),
            (
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                FingerprintPattern {
                    colors: [Green, Red, Blue],
                    cells: [
                        [1, 2, 1, 2, 1],
                        [3, 2, 0, 2, 3],
                        [2, 1, 2, 1, 2],
                        [2, 2, 3, 2, 2],
                        [3, 0, 3, 0, 3],
                    ],
                },
            ),
            (
                "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                FingerprintPattern {
                    colors: [Blue, Red, Cyan],
                    cells: [[3; 5]; 5],
                },
            ),
        ];

        for (fingerprint, expected) in cases {
            let fingerprint = CertificateFingerprint::parse(fingerprint).expect("fingerprint");
            assert_eq!(
                FingerprintPattern::of(&fingerprint),
                expected,
                "{fingerprint:?}"
            );
        }
    }

    #[test]
    fn fingerprints_group_in_two_lines_of_eight() {
        let fingerprint = CertificateFingerprint::parse(
            "87524a4a3c1f9e0b5d26e8f1a7c4093b6e2d51f8c0a97b34e1d6f2085ab9c7e3",
        )
        .expect("fingerprint");

        assert_eq!(
            fingerprint.grouped_lines(),
            [
                "8752 4a4a 3c1f 9e0b 5d26 e8f1 a7c4 093b",
                "6e2d 51f8 c0a9 7b34 e1d6 f208 5ab9 c7e3",
            ]
        );
    }
}
