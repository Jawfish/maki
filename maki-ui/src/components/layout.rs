//! Shared spacing scale, column geometry, and number alignment for history
//! rendering. Every gap between blocks and every numeric column in the status
//! surfaces comes from here, so the vertical rhythm stays consistent.

use crate::selection::Join;
use maki_markdown::render::{CODE_BAR, CODE_BAR_WRAP};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Blank rows between blocks: none, one inside a block, two between sections.
pub(crate) const SPACING_NONE: usize = 0;
pub(crate) const SPACING_BLOCK: usize = 1;
pub(crate) const SPACING_SECTION: usize = 2;

/// Column the message text starts at, after the marker/actor column. Wrapped
/// continuation rows indent to it.
pub(crate) const MESSAGE_INDENT: &str = "  ";

/// Stable widths for right-aligned numbers in the status bar and usage modal.
pub(crate) const TOKENS_COL: usize = 7;
pub(crate) const COST_COL: usize = 8;

pub(crate) fn blank_rows(rows: usize) -> Vec<Line<'static>> {
    vec![Line::default(); rows]
}

/// Rows separating a block from the one above it: none at the top of the
/// history, a section gap where the actor changes, one row otherwise.
pub(crate) fn separator_rows(first_block: bool, section: bool) -> usize {
    match (first_block, section) {
        (true, _) => SPACING_NONE,
        (false, true) => SPACING_SECTION,
        (false, false) => SPACING_BLOCK,
    }
}

pub(crate) fn right_align(text: &str, width: usize) -> String {
    let pad = width.saturating_sub(UnicodeWidthStr::width(text));
    let mut out = String::with_capacity(pad + text.len());
    out.extend(std::iter::repeat_n(' ', pad));
    out.push_str(text);
    out
}

/// Readable measure for prose: the terminal width, capped by the configured
/// maximum. Tool output, diffs, and tables keep the full viewport width.
pub(crate) fn prose_measure(viewport_width: u16, max_prose_width: u16) -> u16 {
    viewport_width.min(max_prose_width.max(1))
}
/// Wrapped lines plus how each row joins the row above it, so a copied
/// selection can rebuild the logical lines the wrap came from.
pub(crate) struct WrappedLines {
    pub lines: Vec<Line<'static>>,
    pub joins: Vec<Join>,
}

/// Pre-wraps prose lines to `width` so continuation rows start at the message
/// column. Lines already fitting `width` are left untouched, which keeps the
/// terminal renderer from wrapping them a second time. Gutter content (code
/// blocks) keeps its full width and its own continuation marks, and disables
/// the join map for the whole block so copying falls back to soft wrapping.
pub(crate) fn wrap_with_hanging_indent(lines: Vec<Line<'static>>, width: u16) -> WrappedLines {
    let limit = usize::from(width);
    let mut out = WrappedLines {
        lines: Vec::with_capacity(lines.len()),
        joins: Vec::with_capacity(lines.len()),
    };
    if limit <= MESSAGE_INDENT.len() {
        out.lines = lines;
        return out;
    }
    let mut soft_wrapped = false;
    for line in lines {
        if has_gutter(&line) {
            soft_wrapped = true;
            out.lines.push(line);
            out.joins.push(Join::NewLine);
            continue;
        }
        wrap_line(line, limit, &mut out);
    }
    if soft_wrapped {
        out.joins.clear();
    }
    out
}

fn has_gutter(line: &Line<'_>) -> bool {
    line.spans
        .first()
        .is_some_and(|s| s.content.starts_with(CODE_BAR) || s.content.starts_with(CODE_BAR_WRAP))
}

fn wrap_line(line: Line<'static>, limit: usize, out: &mut WrappedLines) {
    if line_width(&line) <= limit {
        out.lines.push(line);
        out.joins.push(Join::NewLine);
        return;
    }
    let continuation_limit = limit - MESSAGE_INDENT.len();
    let start = out.lines.len();
    let mut row = Row::new(line.style);
    let mut join = Join::NewLine;
    for span in line.spans {
        for word in split_words(&span.content) {
            if !row.fits(
                word.trim_end(),
                limit,
                continuation_limit,
                out.lines.len() == start,
            ) {
                row.flush(out, &mut join, Join::Word);
            }
            for chunk in hard_split(word, continuation_limit) {
                if !row.fits(chunk, limit, continuation_limit, out.lines.len() == start) {
                    row.flush(out, &mut join, Join::Char);
                }
                row.push(chunk, span.style);
            }
        }
    }
    row.flush(out, &mut join, Join::NewLine);
}

struct Row {
    spans: Vec<Span<'static>>,
    used: usize,
    style: Style,
}

impl Row {
    fn new(style: Style) -> Self {
        Self {
            spans: Vec::new(),
            used: 0,
            style,
        }
    }

    fn fits(&self, text: &str, limit: usize, continuation_limit: usize, first_row: bool) -> bool {
        let limit = if first_row { limit } else { continuation_limit };
        self.used + UnicodeWidthStr::width(text) <= limit
    }

    fn push(&mut self, text: &str, style: Style) {
        self.spans.push(Span::styled(text.to_owned(), style));
        self.used += UnicodeWidthStr::width(text);
    }

    /// Emits the row under the pending join and arms `join` for the next one.
    fn flush(&mut self, out: &mut WrappedLines, join: &mut Join, next: Join) {
        trim_trailing_spaces(&mut self.spans);
        self.used = 0;
        if self.spans.is_empty() {
            return;
        }
        let mut spans = std::mem::take(&mut self.spans);
        if *join != Join::NewLine {
            spans.insert(0, Span::raw(MESSAGE_INDENT));
        }
        out.lines.push(Line::from(spans).style(self.style));
        out.joins.push(*join);
        *join = next;
    }
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

/// Rows never end in padding: the break eats the space and the copy path
/// puts it back through `Join::Word`.
fn trim_trailing_spaces(spans: &mut Vec<Span<'static>>) {
    while let Some(last) = spans.last()
        && last.content.trim_end().is_empty()
    {
        spans.pop();
    }
    if let Some(last) = spans.last_mut()
        && last.content.ends_with(' ')
    {
        let trimmed = last.content.trim_end().to_owned();
        last.content = trimmed.into();
    }
}

/// Splits into words with their trailing spaces attached, so a break between
/// words never leaves padding at the end of a row.
fn split_words(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_space = false;
    for (i, ch) in text.char_indices() {
        let is_space = ch == ' ';
        if is_space {
            in_space = true;
        } else if in_space {
            out.push(&text[start..i]);
            start = i;
            in_space = false;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Breaks a word wider than a full row into row-sized chunks.
fn hard_split(word: &str, limit: usize) -> Vec<&str> {
    if UnicodeWidthStr::width(word) <= limit || limit == 0 {
        return vec![word];
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut used = 0usize;
    for (i, ch) in word.char_indices() {
        let w = UnicodeWidthStr::width(ch.encode_utf8(&mut [0u8; 4]) as &str);
        if used + w > limit {
            out.push(&word[start..i]);
            start = i;
            used = 0;
        }
        used += w;
    }
    out.push(&word[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const WIDTH: u16 = 12;
    const MAX_PROSE: u16 = 88;

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test_case(SPACING_NONE ; "none")]
    #[test_case(SPACING_BLOCK ; "block")]
    #[test_case(SPACING_SECTION ; "section")]
    fn blank_rows_matches_spacing_scale(rows: usize) {
        assert_eq!(blank_rows(rows).len(), rows);
    }

    #[test_case("42", TOKENS_COL, "     42" ; "pads_to_token_column")]
    #[test_case("$1.234", COST_COL, "  $1.234" ; "pads_to_cost_column")]
    #[test_case("overlong", 3, "overlong" ; "never_truncates")]
    fn right_align_pads_to_stable_width(text: &str, width: usize, expected: &str) {
        assert_eq!(right_align(text, width), expected);
    }

    #[test]
    fn continuation_rows_indent_to_message_column() {
        let wrapped = wrap_with_hanging_indent(vec![Line::raw("alpha beta gamma delta")], WIDTH);
        let rows = texts(&wrapped.lines);
        assert_eq!(rows[0], "alpha beta");
        assert_eq!(wrapped.joins[0], Join::NewLine);
        for (row, join) in rows[1..].iter().zip(&wrapped.joins[1..]) {
            assert!(
                row.starts_with(MESSAGE_INDENT),
                "continuation row {row:?} must indent to the message column"
            );
            assert_eq!(*join, Join::Word, "prose breaks join on word boundaries");
        }
    }

    #[test]
    fn wrapped_rows_fit_width_without_trailing_padding() {
        let wrapped = wrap_with_hanging_indent(
            vec![Line::raw("one two three four five six seven eight")],
            WIDTH,
        );
        for row in texts(&wrapped.lines) {
            assert!(row.width() <= usize::from(WIDTH), "row too wide: {row:?}");
            assert_eq!(row.trim_end(), row, "row has trailing padding: {row:?}");
        }
    }

    #[test]
    fn short_lines_are_left_untouched() {
        let wrapped = wrap_with_hanging_indent(vec![Line::raw("short")], WIDTH);
        assert_eq!(texts(&wrapped.lines), vec!["short".to_owned()]);
        assert_eq!(wrapped.joins, vec![Join::NewLine]);
    }

    #[test]
    fn words_wider_than_a_row_are_split() {
        let wrapped = wrap_with_hanging_indent(vec![Line::raw("a".repeat(24))], WIDTH);
        assert!(wrapped.lines.len() > 1);
        assert!(wrapped.joins[1..].iter().all(|j| *j == Join::Char));
        for row in texts(&wrapped.lines) {
            assert!(row.width() <= usize::from(WIDTH), "row too wide: {row:?}");
        }
    }

    #[test_case(200, MAX_PROSE, MAX_PROSE ; "wide_terminal_caps_at_measure")]
    #[test_case(60, MAX_PROSE, 60 ; "narrow_terminal_keeps_full_width")]
    #[test_case(60, 0, 1 ; "zero_cap_still_leaves_a_column")]
    fn prose_measure_caps_wide_terminals(viewport: u16, max: u16, expected: u16) {
        assert_eq!(prose_measure(viewport, max), expected);
    }
}
