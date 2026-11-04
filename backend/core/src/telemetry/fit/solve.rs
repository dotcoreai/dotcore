//! Solves a sizing fit's linear system by least squares and explains the systems it cannot solve.
//!
//! Each fit point contributes one row: the value its unknowns must explain and their
//! coefficients. Before solving, a diagnosis names why points cannot determine every unknown (an
//! unknown no point exercises, too few distinct points, two unknowns no point tells apart, or a
//! rank deficiency) and a subset of the unknowns the same points would determine.

use super::FitError;

/// Relative size under which a scaled pivot or proportionality gap counts as zero.
const TOLERANCE: f64 = 1e-9;

/// Largest float noise, in units, that rounding up still treats as an exact integer.
const MAXIMUM_ROUNDING_NOISE: f64 = 1e-3;

/// One fit point's equation row.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Row {
    /// The observed value minus the equation's known terms.
    pub(super) target: f64,

    /// One coefficient per unknown.
    pub(super) coefficients: Vec<f64>,
}

/// Why a fit's points cannot determine its parameters.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum UnsolvableCause {
    #[error("parameter {0:?} has no data: its coefficient is zero on every fit point")]
    NoData(String),

    #[error("{points} distinct fit points cannot determine {parameters} parameters")]
    TooFewPoints { points: usize, parameters: usize },

    #[error(
        "parameters {first:?} and {second:?} are indistinguishable: their coefficients are proportional on every fit point"
    )]
    Indistinguishable { first: String, second: String },

    #[error("the fit points determine only {rank} of {parameters} parameters")]
    Rank { rank: usize, parameters: usize },
}

/// Solves every unknown over the rows, rounded up to whole units; a negative value is an error,
/// since sizing constants are unsigned.
pub(super) fn solve(unknowns: &[String], rows: &[Row]) -> Result<Vec<u64>, FitError> {
    diagnose(unknowns, rows)?;
    let values = least_squares(rows).ok_or_else(|| unsolvable(unknowns, rows))?;
    unknowns
        .iter()
        .zip(values)
        .map(|(name, value)| {
            let rounded = round_up(value);
            if rounded < 0.0 {
                return Err(FitError::NegativeParameter {
                    parameter: name.clone(),
                    value,
                });
            }
            Ok(rounded as u64)
        })
        .collect()
}

/// Fails with the first reason the rows cannot determine every unknown, if any.
pub(super) fn diagnose(unknowns: &[String], rows: &[Row]) -> Result<(), FitError> {
    match cause(unknowns, rows) {
        Some(cause) => Err(FitError::Unsolvable {
            cause,
            solvable: solvable_subset(unknowns, rows),
        }),
        None => Ok(()),
    }
}

/// Least-squares values of every column, or none when a column is a combination of earlier ones.
/// The same factorization decides the rank, so a system the diagnosis accepts always solves.
pub(super) fn least_squares(rows: &[Row]) -> Option<Vec<f64>> {
    let columns = rows.first().map_or(0, |row| row.coefficients.len());
    let factorization = Factorization::of(rows, &(0..columns).collect::<Vec<_>>());
    if factorization.basis.len() < columns {
        return None;
    }
    // Back substitution on R·x = Qᵀ·y.
    let projected = factorization
        .basis
        .iter()
        .map(|basis| {
            basis
                .iter()
                .zip(rows)
                .map(|(value, row)| value * row.target)
                .sum::<f64>()
        })
        .collect::<Vec<_>>();
    let mut values = vec![0.0; columns];
    for index in (0..columns).rev() {
        let known = (index + 1..columns)
            .map(|later| factorization.r[index][later] * values[later])
            .sum::<f64>();
        values[index] = (projected[index] - known) / factorization.r[index][index];
    }
    Some(values)
}

/// Rounds a fitted value up to a whole unit, tolerating float noise around an exact integer.
pub(super) fn round_up(value: f64) -> f64 {
    // A relative tolerance alone would swallow whole bytes of a base in the tens of gigabytes.
    (value - (TOLERANCE * value.abs().max(1.0)).min(MAXIMUM_ROUNDING_NOISE)).ceil()
}

fn unsolvable(unknowns: &[String], rows: &[Row]) -> FitError {
    FitError::Unsolvable {
        cause: UnsolvableCause::Rank {
            rank: rank(rows, &(0..unknowns.len()).collect::<Vec<_>>()),
            parameters: unknowns.len(),
        },
        solvable: solvable_subset(unknowns, rows),
    }
}

fn cause(unknowns: &[String], rows: &[Row]) -> Option<UnsolvableCause> {
    let scales = column_scales(rows, unknowns.len());
    if let Some(column) = scales.iter().position(|scale| *scale == 0.0) {
        return Some(UnsolvableCause::NoData(unknowns[column].clone()));
    }
    let distinct = rows
        .iter()
        .map(|row| {
            row.coefficients
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>()
        })
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if distinct < unknowns.len() {
        return Some(UnsolvableCause::TooFewPoints {
            points: distinct,
            parameters: unknowns.len(),
        });
    }
    for first in 0..unknowns.len() {
        for second in first + 1..unknowns.len() {
            if proportional(rows, first, second) {
                return Some(UnsolvableCause::Indistinguishable {
                    first: unknowns[first].clone(),
                    second: unknowns[second].clone(),
                });
            }
        }
    }
    let rank = rank(rows, &(0..unknowns.len()).collect::<Vec<_>>());
    (rank < unknowns.len()).then_some(UnsolvableCause::Rank {
        rank,
        parameters: unknowns.len(),
    })
}

/// The unknowns, in order, that add rank when kept: a set the same rows determine.
fn solvable_subset(unknowns: &[String], rows: &[Row]) -> Vec<String> {
    let mut kept = Vec::new();
    for column in 0..unknowns.len() {
        let mut candidate = kept.clone();
        candidate.push(column);
        if rank(rows, &candidate) == candidate.len() {
            kept = candidate;
        }
    }
    kept.into_iter()
        .map(|column| unknowns[column].clone())
        .collect()
}

/// Whether two non-zero columns are proportional on every row.
fn proportional(rows: &[Row], first: usize, second: usize) -> bool {
    let Some(ratio) = rows.iter().find_map(|row| {
        (row.coefficients[first] != 0.0).then(|| row.coefficients[second] / row.coefficients[first])
    }) else {
        return false;
    };
    rows.iter().all(|row| {
        let expected = ratio * row.coefficients[first];
        (row.coefficients[second] - expected).abs()
            <= TOLERANCE * row.coefficients[second].abs().max(expected.abs()).max(1.0)
    })
}

/// Rank of the selected columns.
fn rank(rows: &[Row], columns: &[usize]) -> usize {
    Factorization::of(rows, columns).basis.len()
}

/// Gram-Schmidt factorization of selected columns, in order: an orthonormal basis of the columns
/// that earlier ones do not explain, and the coefficients of every column on that basis.
struct Factorization {
    basis: Vec<Vec<f64>>,
    r: Vec<Vec<f64>>,
}

impl Factorization {
    /// A column joins the basis when what earlier columns leave of it exceeds `TOLERANCE` of its
    /// own norm. Orthogonalizing twice keeps the basis orthogonal at the precision sizing needs.
    fn of(rows: &[Row], columns: &[usize]) -> Self {
        let mut basis: Vec<Vec<f64>> = Vec::new();
        let mut r = vec![vec![0.0; columns.len()]; columns.len()];
        for (position, &column) in columns.iter().enumerate() {
            let mut remainder = rows
                .iter()
                .map(|row| row.coefficients[column])
                .collect::<Vec<_>>();
            let norm = euclidean_norm(&remainder);
            for _ in 0..2 {
                for (index, vector) in basis.iter().enumerate() {
                    let coefficient = vector
                        .iter()
                        .zip(&remainder)
                        .map(|(a, b)| a * b)
                        .sum::<f64>();
                    for (value, basis_value) in remainder.iter_mut().zip(vector) {
                        *value -= coefficient * basis_value;
                    }
                    r[index][position] += coefficient;
                }
            }
            let left = euclidean_norm(&remainder);
            if norm == 0.0 || left <= TOLERANCE * norm {
                continue;
            }
            r[basis.len()][position] = left;
            basis.push(remainder.into_iter().map(|value| value / left).collect());
        }
        Self { basis, r }
    }
}

fn euclidean_norm(values: &[f64]) -> f64 {
    values.iter().map(|value| value * value).sum::<f64>().sqrt()
}

/// Largest absolute coefficient of each column, zero for a column no row exercises.
fn column_scales(rows: &[Row], columns: usize) -> Vec<f64> {
    (0..columns)
        .map(|column| {
            rows.iter()
                .map(|row| row.coefficients[column].abs())
                .fold(0.0_f64, f64::max)
        })
        .collect()
}
