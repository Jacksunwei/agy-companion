use std::path::Path;

use crate::config::CoreError;

use super::FISH_DROPIN_FILENAME;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MarkerState {
    Absent,
    Valid { start: usize, end: usize },
}

pub(super) fn split_lines_preserving_eol(content: &str) -> (Vec<&str>, &'static str, bool) {
    let eol = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let had_trailing_newline = content.ends_with('\n');
    if content.is_empty() {
        return (Vec::new(), eol, false);
    }
    let body = content
        .strip_suffix("\r\n")
        .or_else(|| content.strip_suffix('\n'))
        .unwrap_or(content);
    let lines = body
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    (lines, eol, had_trailing_newline)
}

fn is_begin_marker(trimmed: &str) -> bool {
    trimmed.starts_with("# >>> agy-companion")
}

fn is_end_marker(trimmed: &str) -> bool {
    trimmed.starts_with("# <<< agy-companion")
}

pub(super) fn parse_markers(path: &Path, lines: &[&str]) -> Result<MarkerState, CoreError> {
    let mut begins = Vec::new();
    let mut ends = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if is_begin_marker(trimmed) {
            begins.push(idx + 1);
        } else if is_end_marker(trimmed) {
            ends.push(idx + 1);
        }
    }

    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => Ok(MarkerState::Absent),
        ([begin], [end]) if begin < end => Ok(MarkerState::Valid {
            start: begin - 1,
            end: end - 1,
        }),
        ([begin], []) => Err(CoreError::CorruptedMarkers {
            path: path.to_path_buf(),
            reason: format!("unclosed begin marker at line {begin} (missing end marker)"),
        }),
        ([], [end]) => Err(CoreError::CorruptedMarkers {
            path: path.to_path_buf(),
            reason: format!("orphan end marker at line {end} (missing begin marker)"),
        }),
        ([begin], [end]) => Err(CoreError::CorruptedMarkers {
            path: path.to_path_buf(),
            reason: format!("end marker at line {end} appears before begin marker at line {begin}"),
        }),
        _ => Err(CoreError::CorruptedMarkers {
            path: path.to_path_buf(),
            reason: format!(
                "duplicate or unbalanced markers (begin lines: {begins:?}, end lines: {ends:?})"
            ),
        }),
    }
}

fn join_lines_preserving_eol(lines: &[&str], eol: &str, trailing_newline: bool) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut out = lines.join(eol);
    if trailing_newline {
        out.push_str(eol);
    }
    out
}

pub(super) fn upsert_marker_block(
    path: &Path,
    content: &str,
    block: &str,
) -> Result<String, CoreError> {
    let (lines, eol, had_trailing_newline) = split_lines_preserving_eol(content);
    let block_lines: Vec<&str> = block
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let state = parse_markers(path, &lines)?;

    let mut result_lines: Vec<&str> = Vec::with_capacity(lines.len() + block_lines.len());
    match state {
        MarkerState::Absent => {
            result_lines.extend_from_slice(&lines);
            result_lines.extend_from_slice(&block_lines);
        }
        MarkerState::Valid { start, end } => {
            result_lines.extend_from_slice(&lines[..start]);
            result_lines.extend_from_slice(&block_lines);
            result_lines.extend_from_slice(&lines[end + 1..]);
        }
    }

    let trailing = if lines.is_empty() {
        true
    } else {
        had_trailing_newline
    };
    Ok(join_lines_preserving_eol(&result_lines, eol, trailing))
}

pub(super) fn strip_marker_block(path: &Path, content: &str) -> Result<(String, bool), CoreError> {
    let (lines, eol, had_trailing_newline) = split_lines_preserving_eol(content);
    let state = parse_markers(path, &lines)?;
    match state {
        MarkerState::Absent => Ok((content.to_string(), false)),
        MarkerState::Valid { start, end } => {
            let mut result_lines: Vec<&str> = Vec::with_capacity(lines.len());
            result_lines.extend_from_slice(&lines[..start]);
            result_lines.extend_from_slice(&lines[end + 1..]);
            Ok((
                join_lines_preserving_eol(&result_lines, eol, had_trailing_newline),
                true,
            ))
        }
    }
}

pub(super) fn is_deletable_when_empty(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some(".zlogin") | Some(FISH_DROPIN_FILENAME)
    )
}
