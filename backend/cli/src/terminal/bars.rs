//! Labelled stacked bars, for example how full each memory of a machine would be with one more
//! workload added.
//!
//! Each row draws its segments in order on one scale, the rest of its capacity as free space and
//! an optional marker line across the bar, then a note in the row's tone. Shades tell the
//! segments apart without colors, so a line terminal shows the same bars as plain text; capable
//! terminals also color them. Anything past a row's capacity is drawn as an error.

use super::render::{Piece, RenderedLine, Role, Symbols};

/// Cells one bar spans, without its marker.
const BAR_WIDTH: usize = 24;

/// Rows of stacked bars, with a legend naming the fills and the marker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BarChart {
    /// The bars, top first.
    pub rows: Vec<BarRow>,

    /// What each fill stands for, in legend order; an empty legend draws none.
    pub legend: Vec<(BarFill, String)>,

    /// What the marker line stands for, shown last in the legend.
    pub marker_legend: Option<String>,
}

/// One labelled bar and the note after it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BarRow {
    /// The name shown before the bar, aligned with the other rows'.
    pub label: String,

    /// The amounts the bar stacks, in drawing order.
    pub segments: Vec<BarSegment>,

    /// The amount a full bar stands for; unknown draws no bar, only the note.
    pub capacity: Option<u64>,

    /// The amount a marker line is drawn at.
    pub marker: Option<u64>,

    /// The words after the bar.
    pub note: String,

    /// The tone of the note.
    pub tone: BarTone,
}

/// One stacked part of a bar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BarSegment {
    /// How the part is drawn.
    pub fill: BarFill,

    /// The amount it stands for, on the row's scale.
    pub amount: u64,
}

/// How a segment is drawn, each with its own shade.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BarFill {
    /// What is already there.
    Used,

    /// What would be added.
    Added,
}

/// The tone of a row's note.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BarTone {
    /// A plain statement.
    Neutral,

    /// A good outcome.
    Good,

    /// An outcome to act on.
    Bad,
}

impl BarChart {
    /// Lays the chart out: one line per row, then the legend.
    pub(super) fn lines(&self, symbols: Symbols) -> Vec<RenderedLine> {
        let width = self
            .rows
            .iter()
            .map(|row| super::render::text_width(&row.label))
            .max()
            .unwrap_or(0);
        let marked = self.rows.iter().any(|row| row.marker.is_some());
        let mut lines = self
            .rows
            .iter()
            .map(|row| row.line(width, marked, symbols))
            .collect::<Vec<_>>();
        if let Some(legend) = self.legend_line(symbols) {
            lines.push(legend);
        }
        lines
    }

    fn legend_line(&self, symbols: Symbols) -> Option<RenderedLine> {
        if self.legend.is_empty() && self.marker_legend.is_none() {
            return None;
        }
        let mut line = Vec::new();
        let mut entry = |role, mark: &str, text: &str| {
            line.push(Piece {
                role: Role::Plain,
                text: "  ".to_owned(),
            });
            line.push(Piece {
                role,
                text: mark.to_owned(),
            });
            line.push(Piece {
                role: Role::Hint,
                text: format!(" {text}"),
            });
        };
        for (fill, text) in &self.legend {
            entry(fill_role(*fill), fill_shade(*fill, symbols), text);
        }
        if let Some(text) = &self.marker_legend {
            entry(Role::Label, symbols.marker(), text);
        }
        Some(line)
    }
}

impl BarRow {
    /// The row's line, its label padded to `width` and its bar column reserving a marker cell
    /// when any row of the chart has one.
    fn line(&self, width: usize, marked: bool, symbols: Symbols) -> RenderedLine {
        let padding = width - super::render::text_width(&self.label);
        let mut line = vec![Piece {
            role: Role::Label,
            text: format!("  {}{}  ", self.label, " ".repeat(padding)),
        }];
        let column = BAR_WIDTH + usize::from(marked);
        match self.capacity {
            Some(capacity) => {
                let cells = self.cells(capacity, symbols);
                let drawn = cells.len();
                push_merged(&mut line, cells);
                line.push(Piece {
                    role: Role::Plain,
                    text: " ".repeat(column - drawn + 2),
                });
            }
            None => line.push(Piece {
                role: Role::Plain,
                text: " ".repeat(column + 2),
            }),
        }
        line.push(Piece {
            role: tone_role(self.tone),
            text: self.note.clone(),
        });
        line
    }

    /// The bar's cells: each takes the fill of the segment under its middle, else free space,
    /// with the marker inserted where it falls.
    fn cells(&self, capacity: u64, symbols: Symbols) -> Vec<Piece> {
        let stacked: u64 = self.segments.iter().map(|segment| segment.amount).sum();
        let scale = u128::from(capacity.max(stacked).max(1));
        let width = BAR_WIDTH as u128;

        let mut cells = (0..width)
            .map(|index| {
                // The middle of the cell on the row's scale, doubled to stay in whole numbers.
                let middle = (2 * index + 1) * scale;
                let mut end = 0u128;
                let fill = self.segments.iter().find_map(|segment| {
                    end += u128::from(segment.amount);
                    (middle < 2 * end * width).then_some(segment.fill)
                });
                let over = middle >= 2 * u128::from(capacity) * width;
                let (role, text) = match fill {
                    Some(fill) => (fill_role(fill), fill_shade(fill, symbols)),
                    None => (Role::Hint, symbols.shade(2)),
                };
                Piece {
                    role: if over { Role::Error } else { role },
                    text: text.to_owned(),
                }
            })
            .collect::<Vec<_>>();
        if let Some(marker) = self.marker {
            let position = (u128::from(marker) * width + scale / 2) / scale;
            let position = usize::try_from(position.min(width)).unwrap_or(BAR_WIDTH);
            cells.insert(
                position,
                Piece {
                    role: Role::Label,
                    text: symbols.marker().to_owned(),
                },
            );
        }
        cells
    }
}

/// Appends the cells, merging neighbours of one role into one piece.
fn push_merged(line: &mut RenderedLine, cells: Vec<Piece>) {
    let start = line.len();
    for cell in cells {
        let merges = line.len() > start;
        match line.last_mut() {
            Some(last) if merges && last.role == cell.role => {
                last.text.push_str(&cell.text);
            }
            _ => line.push(cell),
        }
    }
}

fn fill_shade(fill: BarFill, symbols: Symbols) -> &'static str {
    match fill {
        BarFill::Used => symbols.shade(0),
        BarFill::Added => symbols.shade(1),
    }
}

fn fill_role(fill: BarFill) -> Role {
    match fill {
        BarFill::Used => Role::Plain,
        BarFill::Added => Role::Accent,
    }
}

fn tone_role(tone: BarTone) -> Role {
    match tone {
        BarTone::Neutral => Role::Plain,
        BarTone::Good => Role::Success,
        BarTone::Bad => Role::Warning,
    }
}

#[cfg(test)]
mod tests {
    use super::{BarChart, BarFill, BarRow, BarSegment, BarTone};
    use crate::terminal::{
        Message,
        render::{Role, Symbols, plain_text},
    };

    fn plain(chart: &BarChart, symbols: Symbols) -> Vec<String> {
        chart.lines(symbols).iter().map(plain_text).collect()
    }

    fn chart() -> BarChart {
        BarChart {
            rows: vec![
                BarRow {
                    label: "GPU 0".to_owned(),
                    segments: vec![
                        BarSegment {
                            fill: BarFill::Used,
                            amount: 6,
                        },
                        BarSegment {
                            fill: BarFill::Added,
                            amount: 6,
                        },
                    ],
                    capacity: Some(24),
                    marker: Some(18),
                    note: "fits".to_owned(),
                    tone: BarTone::Good,
                },
                BarRow {
                    label: "Host memory".to_owned(),
                    segments: vec![BarSegment {
                        fill: BarFill::Added,
                        amount: 36,
                    }],
                    capacity: Some(24),
                    marker: None,
                    note: "over by 12".to_owned(),
                    tone: BarTone::Bad,
                },
                BarRow {
                    label: "GPU 1".to_owned(),
                    segments: Vec::new(),
                    capacity: None,
                    marker: None,
                    note: "total unknown".to_owned(),
                    tone: BarTone::Neutral,
                },
            ],
            legend: vec![
                (BarFill::Used, "in use".to_owned()),
                (BarFill::Added, "added".to_owned()),
            ],
            marker_legend: Some("limit".to_owned()),
        }
    }

    #[test]
    fn bars_stack_segments_on_their_scale_with_a_marker_and_a_legend() {
        assert_eq!(
            plain(&chart(), Symbols::UNICODE),
            [
                "  GPU 0        ██████▓▓▓▓▓▓░░░░░░│░░░░░░  fits",
                "  Host memory  ▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓   over by 12",
                "  GPU 1                                   total unknown",
                "  █ in use  ▓ added  │ limit",
            ]
        );
        assert_eq!(
            plain(&chart(), Symbols::ASCII),
            [
                "  GPU 0        ######++++++......|......  fits",
                "  Host memory  ++++++++++++++++++++++++   over by 12",
                "  GPU 1                                   total unknown",
                "  # in use  + added  | limit",
            ]
        );
    }

    #[test]
    fn cells_past_the_capacity_are_errors_and_notes_take_their_tone() {
        let lines = chart().lines(Symbols::ASCII);

        assert_eq!(lines[1][1].role, Role::Accent);
        assert_eq!(lines[1][1].text, "+".repeat(16));
        assert_eq!(lines[1][2].role, Role::Error);
        assert_eq!(lines[1][2].text, "+".repeat(8));
        assert_eq!(lines[1].last().map(|piece| piece.role), Some(Role::Warning));
        assert_eq!(lines[0].last().map(|piece| piece.role), Some(Role::Success));
    }

    #[test]
    fn a_chart_is_a_message_attached_to_what_precedes_it() {
        let message = Message::Bars(chart());

        assert!(!message.render(Symbols::UNICODE).blank_before);
        assert_eq!(message.plain_lines().len(), 4);
    }
}
