use std::error::Error;
use std::fmt::{self, Display, Formatter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchError {
    line: usize,
    message: String,
}

impl PatchError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }

    pub fn line(&self) -> usize {
        self.line
    }
}

impl Display for PatchError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "patch line {}: {}", self.line, self.message)
    }
}

impl Error for PatchError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    Replace {
        start: usize,
        end: usize,
        lines: Vec<String>,
        patch_line: usize,
    },
    InsertBefore {
        line: usize,
        lines: Vec<String>,
        patch_line: usize,
    },
    InsertAfter {
        line: usize,
        lines: Vec<String>,
        patch_line: usize,
    },
    InsertHead {
        lines: Vec<String>,
        patch_line: usize,
    },
    InsertTail {
        lines: Vec<String>,
        patch_line: usize,
    },
    Cut {
        start: usize,
        end: usize,
        patch_line: usize,
    },
}

impl Edit {
    pub fn anchor(&self) -> Option<usize> {
        match self {
            Self::Replace { start, .. } | Self::Cut { start, .. } => Some(*start),
            Self::InsertBefore { line, .. } | Self::InsertAfter { line, .. } => Some(*line),
            Self::InsertHead { .. } => Some(1),
            Self::InsertTail { .. } => None,
        }
    }

    fn patch_line(&self) -> usize {
        match self {
            Self::Replace { patch_line, .. }
            | Self::InsertBefore { patch_line, .. }
            | Self::InsertAfter { patch_line, .. }
            | Self::InsertHead { patch_line, .. }
            | Self::InsertTail { patch_line, .. }
            | Self::Cut { patch_line, .. } => *patch_line,
        }
    }

    fn affected_range(&self) -> Option<(usize, usize)> {
        match self {
            Self::Replace { start, end, .. } | Self::Cut { start, end, .. } => Some((*start, *end)),
            _ => None,
        }
    }
}

pub fn parse_patch(patch: &str) -> Result<Vec<Edit>, PatchError> {
    let mut edits = Vec::new();
    let mut pending: Option<Edit> = None;

    for (index, row) in patch.lines().enumerate() {
        let patch_line = index + 1;
        if let Some(body) = row.strip_prefix('+') {
            let edit = pending.as_mut().ok_or_else(|| {
                PatchError::new(patch_line, "body row has no preceding `PUT` header")
            })?;
            match edit {
                Edit::Replace { lines, .. }
                | Edit::InsertBefore { lines, .. }
                | Edit::InsertAfter { lines, .. }
                | Edit::InsertHead { lines, .. }
                | Edit::InsertTail { lines, .. } => lines.push(body.to_owned()),
                Edit::Cut { .. } => {
                    return Err(PatchError::new(
                        patch_line,
                        "`CUT` does not accept `+TEXT` body rows",
                    ));
                }
            }
            continue;
        }

        if !row.starts_with("PUT ") && !row.starts_with("CUT ") {
            return Err(malformed_row(row, patch_line));
        }
        finish_pending(&mut pending, &mut edits)?;
        pending = Some(parse_header(row, patch_line)?);
    }

    finish_pending(&mut pending, &mut edits)?;
    if edits.is_empty() {
        return Err(PatchError::new(
            1,
            "patch must contain at least one operation",
        ));
    }
    validate_overlaps(&edits)?;
    Ok(edits)
}

fn finish_pending(pending: &mut Option<Edit>, edits: &mut Vec<Edit>) -> Result<(), PatchError> {
    let Some(edit) = pending.take() else {
        return Ok(());
    };
    let empty_put = match &edit {
        Edit::Replace { lines, .. }
        | Edit::InsertBefore { lines, .. }
        | Edit::InsertAfter { lines, .. }
        | Edit::InsertHead { lines, .. }
        | Edit::InsertTail { lines, .. } => lines.is_empty(),
        Edit::Cut { .. } => false,
    };
    if empty_put {
        return Err(PatchError::new(
            edit.patch_line(),
            "`PUT` needs at least one `+TEXT` body row; use `CUT` to delete",
        ));
    }
    edits.push(edit);
    Ok(())
}

fn parse_header(row: &str, patch_line: usize) -> Result<Edit, PatchError> {
    if let Some(target) = row
        .strip_prefix("PUT ")
        .and_then(|row| row.strip_suffix(':'))
    {
        if target == "<1" {
            return Ok(Edit::InsertHead {
                lines: Vec::new(),
                patch_line,
            });
        }
        if target == ">$" {
            return Ok(Edit::InsertTail {
                lines: Vec::new(),
                patch_line,
            });
        }
        if let Some(line) = target.strip_prefix('<') {
            return Ok(Edit::InsertBefore {
                line: parse_line_number(line, patch_line)?,
                lines: Vec::new(),
                patch_line,
            });
        }
        if let Some(line) = target.strip_prefix('>') {
            return Ok(Edit::InsertAfter {
                line: parse_line_number(line, patch_line)?,
                lines: Vec::new(),
                patch_line,
            });
        }
        let (start, end) = parse_range(target, patch_line)?;
        return Ok(Edit::Replace {
            start,
            end,
            lines: Vec::new(),
            patch_line,
        });
    }

    if let Some(target) = row.strip_prefix("CUT ") {
        if target.ends_with(':') {
            return Err(PatchError::new(
                patch_line,
                "`CUT` headers must not end with `:` because they have no body",
            ));
        }
        let (start, end) = parse_range(target, patch_line)?;
        return Ok(Edit::Cut {
            start,
            end,
            patch_line,
        });
    }

    Err(malformed_row(row, patch_line))
}

fn malformed_row(row: &str, patch_line: usize) -> PatchError {
    PatchError::new(
        patch_line,
        format!("malformed row `{row}`; expected `PUT` or `CUT` header, or `+TEXT` body"),
    )
}

fn parse_range(target: &str, patch_line: usize) -> Result<(usize, usize), PatchError> {
    let (start, end) = target.split_once(".=").ok_or_else(|| {
        PatchError::new(patch_line, "range must use the exact inclusive `N.=M` form")
    })?;
    if end.contains(".=") {
        return Err(PatchError::new(
            patch_line,
            "range must contain exactly one `.=` separator",
        ));
    }
    let start = parse_line_number(start, patch_line)?;
    let end = parse_line_number(end, patch_line)?;
    if start > end {
        return Err(PatchError::new(
            patch_line,
            "range start must not be greater than range end",
        ));
    }
    Ok((start, end))
}

fn parse_line_number(value: &str, patch_line: usize) -> Result<usize, PatchError> {
    let line = value.parse::<usize>().map_err(|_| {
        PatchError::new(patch_line, format!("`{value}` is not a valid line number"))
    })?;
    if line == 0 {
        return Err(PatchError::new(patch_line, "line numbers are 1-based"));
    }
    Ok(line)
}

fn validate_overlaps(edits: &[Edit]) -> Result<(), PatchError> {
    let mut ranges: Vec<(usize, usize, usize)> = edits
        .iter()
        .filter_map(|edit| {
            edit.affected_range()
                .map(|(start, end)| (start, end, edit.patch_line()))
        })
        .collect();
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        if pair[1].0 <= pair[0].1 {
            return Err(PatchError::new(
                pair[1].2,
                format!(
                    "range {}.={} overlaps range {}.={} from patch line {}",
                    pair[1].0, pair[1].1, pair[0].0, pair[0].1, pair[0].2
                ),
            ));
        }
    }
    Ok(())
}

pub fn apply_patch(content: &str, edits: &[Edit]) -> Result<String, PatchError> {
    validate_overlaps(edits)?;
    let trailing_newline = content.ends_with('\n');
    let body = content.strip_suffix('\n').unwrap_or(content);
    let mut lines: Vec<String> = if content.is_empty() {
        Vec::new()
    } else {
        body.split('\n').map(str::to_owned).collect()
    };
    let line_count = lines.len();
    let mut ordered = Vec::with_capacity(edits.len());

    for edit in edits {
        let (position, range_end) = application_position(edit, line_count)?;
        ordered.push((position, range_end, edit.patch_line(), edit));
    }
    ordered.sort_unstable_by_key(|(position, range_end, patch_line, _)| {
        (*position, *range_end, *patch_line)
    });

    for (_, _, _, edit) in ordered.into_iter().rev() {
        match edit {
            Edit::Replace {
                start,
                end,
                lines: replacement,
                ..
            } => lines
                .splice(start - 1..*end, replacement.iter().cloned())
                .for_each(drop),
            Edit::Cut { start, end, .. } => lines.drain(start - 1..*end).for_each(drop),
            Edit::InsertBefore {
                line,
                lines: inserted,
                ..
            } => lines
                .splice(line - 1..line - 1, inserted.iter().cloned())
                .for_each(drop),
            Edit::InsertAfter {
                line,
                lines: inserted,
                ..
            } => lines
                .splice(*line..*line, inserted.iter().cloned())
                .for_each(drop),
            Edit::InsertHead {
                lines: inserted, ..
            } => lines.splice(0..0, inserted.iter().cloned()).for_each(drop),
            Edit::InsertTail {
                lines: inserted, ..
            } => lines
                .splice(line_count..line_count, inserted.iter().cloned())
                .for_each(drop),
        }
    }

    let has_lines = !lines.is_empty();
    let mut result = lines.join("\n");
    if trailing_newline && has_lines {
        result.push('\n');
    }
    Ok(result)
}

fn application_position(edit: &Edit, line_count: usize) -> Result<(usize, usize), PatchError> {
    let out_of_bounds = |anchor: usize, rule: &str| {
        PatchError::new(
            edit.patch_line(),
            format!("{rule} line {anchor} is out of bounds for a {line_count}-line file"),
        )
    };
    match edit {
        Edit::Replace { start, end, .. } | Edit::Cut { start, end, .. } => {
            if *end > line_count {
                Err(out_of_bounds(*end, "range endpoint"))
            } else {
                Ok((start - 1, *end))
            }
        }
        Edit::InsertBefore { line, .. } => {
            if *line > line_count {
                Err(out_of_bounds(*line, "insert anchor"))
            } else {
                Ok((line - 1, line - 1))
            }
        }
        Edit::InsertAfter { line, .. } => {
            if *line > line_count {
                Err(out_of_bounds(*line, "insert anchor"))
            } else {
                Ok((*line, *line))
            }
        }
        Edit::InsertHead { .. } => Ok((0, 0)),
        Edit::InsertTail { .. } => Ok((line_count, line_count)),
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_patch, parse_patch};
    use test_case::test_case;

    fn apply(content: &str, patch: &str) -> String {
        apply_patch(content, &parse_patch(patch).unwrap()).unwrap()
    }

    #[test_case("one\ntwo\nthree", "PUT 2.=2:\n+TWO", "one\nTWO\nthree"; "replace_single_line")]
    #[test_case("one\ntwo\nthree", "PUT 1.=2:\n+first\n+second", "first\nsecond\nthree"; "replace_range")]
    #[test_case("one\ntwo", "PUT <2:\n+middle", "one\nmiddle\ntwo"; "insert_before")]
    #[test_case("one\ntwo", "PUT >1:\n+middle", "one\nmiddle\ntwo"; "insert_after")]
    #[test_case("one\ntwo", "PUT <1:\n+head", "head\none\ntwo"; "insert_head")]
    #[test_case("one\ntwo", "PUT >$:\n+tail", "one\ntwo\ntail"; "insert_tail")]
    #[test_case("one\ntwo\nthree", "CUT 2.=3", "one"; "cut_range")]
    #[test_case("", "PUT <1:\n+first", "first"; "head_insert_into_empty_file")]
    #[test_case("", "PUT >$:\n+last", "last"; "tail_insert_into_empty_file")]
    fn applies_each_operation(content: &str, patch: &str, expected: &str) {
        assert_eq!(apply(content, patch), expected);
    }

    #[test]
    fn multi_hunk_patch_uses_original_coordinates() {
        let patch = "PUT >1:\n+after one\nPUT 3.=3:\n+THREE\nCUT 5.=5\nPUT <5:\n+before five";
        assert_eq!(
            apply("one\ntwo\nthree\nfour\nfive", patch),
            "one\nafter one\ntwo\nTHREE\nfour\nbefore five"
        );
    }

    #[test_case(
        "one\ntwo",
        "PUT >$:\n+first\nPUT >$:\n+second\nPUT >$:\n+third",
        "one\ntwo\nfirst\nsecond\nthird";
        "repeated_tail_inserts"
    )]
    #[test_case(
        "one\ntwo",
        "PUT >$:\n+tail one\nPUT >2:\n+after one\nPUT >$:\n+tail two\nPUT >2:\n+after two",
        "one\ntwo\ntail one\nafter one\ntail two\nafter two";
        "tail_and_last_line_share_declaration_order"
    )]
    #[test_case(
        "",
        "PUT >$:\n+tail one\nPUT <1:\n+head one\nPUT >$:\n+tail two\nPUT <1:\n+head two",
        "tail one\nhead one\ntail two\nhead two";
        "empty_file_boundary_inserts_share_declaration_order"
    )]
    fn same_position_inserts_preserve_declaration_order(
        content: &str,
        patch: &str,
        expected: &str,
    ) {
        assert_eq!(apply(content, patch), expected);
    }

    #[test_case("one\ntwo\n", "PUT 2.=2:\n+TWO", "one\nTWO\n"; "preserves_present_newline")]
    #[test_case("one\ntwo", "PUT 2.=2:\n+TWO", "one\nTWO"; "preserves_absent_newline")]
    #[test_case("one\n", "PUT >$:\n+two", "one\ntwo\n"; "tail_insert_preserves_newline")]
    #[test_case("one\n", "CUT 1.=1", ""; "deleting_all_content_removes_newline")]
    #[test_case("\n", "PUT 1.=1:\n+blank", "blank\n"; "blank_line_is_addressable")]
    fn preserves_trailing_newline(content: &str, patch: &str, expected: &str) {
        assert_eq!(apply(content, patch), expected);
    }

    #[test_case("PUT 1.=2:\n+x\nCUT 2.=3", 3, "overlaps"; "overlapping_ranges")]
    #[test_case("PUT 1-2:\n+x", 1, "exact inclusive `N.=M`"; "widened_dash_range")]
    #[test_case("PUT 1.=2:\n context", 2, "malformed row"; "context_row")]
    #[test_case("PUT 1.=2:\n-old", 2, "malformed row"; "minus_row")]
    #[test_case("PUT 1.=1:", 1, "needs at least one `+TEXT`"; "empty_put_as_delete")]
    #[test_case("CUT 1.=1:\n+x", 1, "must not end with `:`"; "cut_with_colon")]
    #[test_case("CUT 1.=1\n+x", 2, "does not accept `+TEXT`"; "cut_with_body")]
    #[test_case("+orphan", 1, "no preceding `PUT`"; "orphan_body")]
    #[test_case("PUT 2.=1:\n+x", 1, "start must not be greater"; "reversed_range")]
    #[test_case("PUT 0.=1:\n+x", 1, "1-based"; "zero_line")]
    #[test_case("", 1, "at least one operation"; "empty_patch")]
    fn rejects_malformed_patch(patch: &str, line: usize, rule: &str) {
        let error = parse_patch(patch).unwrap_err();
        assert_eq!(error.line(), line);
        assert!(error.to_string().contains(rule), "{error}");
    }

    #[test_case("one\ntwo", "PUT 2.=3:\n+x", 1, "out of bounds"; "replace_endpoint")]
    #[test_case("one\ntwo", "CUT 3.=3", 1, "out of bounds"; "cut_endpoint")]
    #[test_case("one\ntwo", "PUT <3:\n+x", 1, "out of bounds"; "before_anchor")]
    #[test_case("one\ntwo", "PUT >3:\n+x", 1, "out of bounds"; "after_anchor")]
    fn rejects_out_of_bounds_anchor(content: &str, patch: &str, line: usize, rule: &str) {
        let error = apply_patch(content, &parse_patch(patch).unwrap()).unwrap_err();
        assert_eq!(error.line(), line);
        assert!(error.to_string().contains(rule), "{error}");
    }

    #[test]
    fn plus_alone_inserts_blank_line() {
        assert_eq!(apply("one\ntwo", "PUT >1:\n+"), "one\n\ntwo");
    }
}
