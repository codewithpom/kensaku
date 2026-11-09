//! CSV and TSV extraction.
//!
//! Each data row becomes one line of text: the header repeated as
//! `column = "value"` pairs. A search for a column name or a cell value then
//! lands on the row that holds it. Lines are separated by newlines, so a
//! newline inside a cell is flattened to a space.
//!
//! The parser borrows field text from the input wherever it can, and writes
//! each row straight into the output, so a plain cell costs no allocation.

use std::borrow::Cow;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use super::text::decode_utf16_bom;

/// Only the start of the file is used to choose the delimiter.
const SNIFF_BYTES: usize = 8 * 1024;
/// How many records are compared when choosing the delimiter.
const SNIFF_RECORDS: usize = 20;

/// Extracts text from a CSV or TSV file like `pandas.head`: a column summary
/// and the first `head_rows` data rows, reading at most `max_bytes`.
pub fn extract_delimited(path: &Path, max_bytes: usize, head_rows: usize) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let truncated = file.metadata().ok()?.len() > max_bytes as u64;
    let mut buffer = Vec::new();
    (&mut file)
        .take(max_bytes as u64)
        .read_to_end(&mut buffer)
        .ok()?;
    render_rows(&buffer, truncated, hint_for_path(path), max_bytes, Some(head_rows))
}

/// Row text as separate lines, for the snippet path. Rows are never split.
pub(crate) fn row_blocks(buffer: &[u8], truncated: bool, hint: u8) -> Vec<String> {
    render_rows(buffer, truncated, hint, usize::MAX, None)
        .map(|text| text.lines().map(str::to_string).collect::<Vec<String>>())
        .unwrap_or_default()
}

/// Decodes `buffer` and renders one line per data row, up to `cap` bytes.
///
/// `truncated` means the buffer is only the start of a larger file: a last
/// record that ends without a line break may be cut off, so it is dropped.
/// `hint` is the delimiter to prefer when candidates look equally consistent.
/// `head_rows` of `Some(n)` keeps only the first `n` data rows and puts a
/// column summary (names and inferred types) first; `None` renders every row.
fn render_rows(
    buffer: &[u8],
    truncated: bool,
    hint: u8,
    cap: usize,
    head_rows: Option<usize>,
) -> Option<String> {
    let text = decode_text(buffer)?;
    if text.trim().is_empty() {
        return None;
    }
    let delimiter = sniff_delimiter(&text, hint);

    let mut names: Option<Vec<String>> = None;
    let mut kinds: Vec<CellKind> = Vec::new();
    let mut data_rows = 0usize;
    let mut sampled = false;
    let mut out = String::new();

    for_each_record(&text, delimiter, |fields, terminated| {
        if truncated && !terminated {
            return false;
        }
        if names.is_none() {
            names = Some(
                fields
                    .iter()
                    .enumerate()
                    .map(|(index, raw)| column_name(raw, index))
                    .collect(),
            );
            kinds = vec![CellKind::Empty; fields.len()];
            return true;
        }
        if head_rows.is_some_and(|limit| data_rows >= limit) {
            sampled = true;
            return false;
        }
        data_rows += 1;
        for (kind, value) in kinds.iter_mut().zip(fields) {
            *kind = kind.merge(value);
        }
        let names_ref = names.as_deref().unwrap_or(&[]);

        let mark = out.len();
        if !out.is_empty() {
            out.push('\n');
        }
        for (index, value) in fields.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            match names_ref.get(index) {
                Some(name) => out.push_str(name),
                None => out.push_str(&fallback_name(index)),
            }
            out.push_str(" = \"");
            push_one_line(&mut out, value);
            out.push('"');
        }

        if out.len() <= cap {
            return true;
        }
        if mark == 0 {
            // The first row alone is longer than the cap: keep the part that fits.
            let keep = clip_to_char_boundary(&out, cap).len();
            out.truncate(keep);
        } else {
            out.truncate(mark);
        }
        false
    });

    if out.is_empty() {
        // No data rows: index the column names so they stay searchable.
        out = names?.join(", ");
        if out.len() > cap {
            let keep = clip_to_char_boundary(&out, cap).len();
            out.truncate(keep);
        }
    } else if head_rows.is_some() {
        let mut summary = String::from(if sampled { "columns (first rows sampled): " } else { "columns: " });
        for (index, (name, kind)) in names.iter().flatten().zip(&kinds).enumerate() {
            if index > 0 {
                summary.push_str(", ");
            }
            summary.push_str(name);
            summary.push_str(" (");
            summary.push_str(kind.label());
            summary.push(')');
        }
        // A very wide header must not outgrow the row text it describes.
        summary.truncate(clip_to_char_boundary(&summary, 8 * 1024).len());
        out = format!("{summary}\n{out}");
    }
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// What a column holds, judged from the sampled rows.
#[derive(Clone, Copy, PartialEq)]
enum CellKind {
    Empty,
    Integer,
    Float,
    Text,
}

impl CellKind {
    fn merge(self, value: &str) -> Self {
        let value = value.trim();
        if value.is_empty() || self == Self::Text {
            return self;
        }
        let seen = if value.parse::<i64>().is_ok() {
            Self::Integer
        } else if value.parse::<f64>().is_ok() {
            Self::Float
        } else {
            Self::Text
        };
        match (self, seen) {
            (Self::Empty, kind) => kind,
            (Self::Integer, Self::Float) | (Self::Float, Self::Integer) => Self::Float,
            (_, Self::Text) => Self::Text,
            (kind, _) => kind,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Integer => "integer",
            Self::Float => "number",
            Self::Text => "text",
        }
    }
}

/// Decodes UTF-16 (with its BOM), or UTF-8 with an optional UTF-8 BOM.
/// Returns None for binary data, using the same NUL-byte check as plain text.
fn decode_text(buffer: &[u8]) -> Option<Cow<'_, str>> {
    if let Some(text) = decode_utf16_bom(buffer) {
        return Some(Cow::Owned(text));
    }
    if buffer.iter().take(1024).any(|&byte| byte == 0) {
        return None;
    }
    Some(match String::from_utf8_lossy(buffer) {
        Cow::Borrowed(valid) => Cow::Borrowed(valid.strip_prefix('\u{feff}').unwrap_or(valid)),
        Cow::Owned(lossy) => Cow::Owned(
            lossy
                .strip_prefix('\u{feff}')
                .unwrap_or(lossy.as_str())
                .to_string(),
        ),
    })
}

/// Picks the delimiter whose first records agree on the most fields.
/// Ties go to the earlier candidate, so the hint is tried first.
fn sniff_delimiter(text: &str, hint: u8) -> u8 {
    let sample = clip_to_char_boundary(text, SNIFF_BYTES);
    // (delimiter, records that agree on the field count, field count)
    let mut best = (hint, 0usize, 0usize);
    // Scan each unique candidate exactly once: the old list repeated the
    // hint (tab for `.tsv`, comma otherwise), so a quarter of the sniff
    // work re-parsed the first scan. Ties prefer the earlier candidate, so
    // the hint must lead: hint → comma → semi → tab for TSV, collapsed to
    // comma → semi → tab for everything else.
    let order: [u8; 3] = if hint == b'\t' {
        *b"\t,;"
    } else {
        *b",;\t"
    };
    for candidate in order {
        let mut width = 0usize;
        let mut agreeing = 0usize;
        let mut seen = 0usize;
        for_each_record(sample, candidate, |fields, _| {
            if seen == 0 {
                width = fields.len();
            }
            if fields.len() == width {
                agreeing += 1;
            }
            seen += 1;
            seen < SNIFF_RECORDS
        });
        if width >= 2 && (agreeing, width) > (best.1, best.2) {
            best = (candidate, agreeing, width);
        }
    }
    best.0
}

/// Calls `visit` with the fields of each record, in order. The bool passed
/// with the fields is false when the record ends at the end of the text
/// without a line break. Blank lines are skipped. Returning false stops.
fn for_each_record<'a>(
    text: &'a str,
    delimiter: u8,
    mut visit: impl FnMut(&[Cow<'a, str>], bool) -> bool,
) {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut pos = 0;
    let mut fields: Vec<Cow<'a, str>> = Vec::new();

    while pos < len {
        fields.clear();
        let terminated = loop {
            let (field, next) = read_field(text, pos, delimiter);
            fields.push(field);
            pos = next;
            if pos >= len {
                break false;
            }
            // Here `pos` is a delimiter, a line break, or the end of the text.
            match bytes[pos] {
                b'\n' => {
                    pos += 1;
                    break true;
                }
                b'\r' => {
                    pos += 1;
                    if pos < len && bytes[pos] == b'\n' {
                        pos += 1;
                    }
                    break true;
                }
                _ => pos += 1,
            }
        };
        // A blank line is one empty field: skip it.
        if fields.len() == 1 && fields[0].is_empty() {
            continue;
        }
        if !visit(&fields, terminated) {
            return;
        }
    }
}

/// Reads one field starting at `start`. Returns its value and the position of
/// the delimiter, line break or end of text that follows it.
fn read_field(text: &str, start: usize, delimiter: u8) -> (Cow<'_, str>, usize) {
    let bytes = text.as_bytes();
    if bytes.get(start) == Some(&b'"') {
        return read_quoted(text, start + 1, delimiter);
    }
    let mut end = start;
    while end < bytes.len() && !is_field_end(bytes[end], delimiter) {
        end += 1;
    }
    (Cow::Borrowed(&text[start..end]), end)
}

/// Reads a quoted field whose content starts at `content_start`, just after
/// the opening quote. `""` inside quotes is one literal quote. Text after the
/// closing quote, up to the next delimiter or line break, is kept.
fn read_quoted(text: &str, content_start: usize, delimiter: u8) -> (Cow<'_, str>, usize) {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut owned: Option<String> = None;
    let mut segment = content_start;
    let mut pos = content_start;

    let close = loop {
        let Some(offset) = bytes[pos..].iter().position(|&b| b == b'"') else {
            // Unclosed quote: the field runs to the end of the text.
            break len;
        };
        let quote = pos + offset;
        if bytes.get(quote + 1) == Some(&b'"') {
            owned
                .get_or_insert_with(String::new)
                .push_str(&text[segment..=quote]);
            segment = quote + 2;
            pos = segment;
        } else {
            break quote;
        }
    };

    let mut value = match owned {
        Some(mut buf) => {
            buf.push_str(&text[segment..close]);
            Cow::Owned(buf)
        }
        None => Cow::Borrowed(&text[content_start..close]),
    };

    let mut next = if close < len { close + 1 } else { len };
    let trailing_start = next;
    while next < len && !is_field_end(bytes[next], delimiter) {
        next += 1;
    }
    if next > trailing_start {
        value = Cow::Owned(format!("{value}{}", &text[trailing_start..next]));
    }
    (value, next)
}

fn is_field_end(byte: u8, delimiter: u8) -> bool {
    byte == delimiter || byte == b'\n' || byte == b'\r'
}

/// Appends a cell value on one line: trimmed, with line breaks turned into spaces.
fn push_one_line(out: &mut String, value: &str) {
    let value = value.trim();
    if value.contains(['\n', '\r']) {
        out.push_str(&value.replace(['\n', '\r'], " "));
    } else {
        out.push_str(value);
    }
}

fn column_name(raw: &str, index: usize) -> String {
    let name = raw.trim();
    if name.is_empty() {
        fallback_name(index)
    } else {
        name.to_string()
    }
}

fn fallback_name(index: usize) -> String {
    format!("column{}", index + 1)
}

/// Delimiter to prefer on ties: tab for `.tsv`, comma otherwise.
pub(crate) fn hint_for_path(path: &Path) -> u8 {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("tsv") => b'\t',
        _ => b',',
    }
}

/// Longest prefix of `text` that is at most `max_bytes` long and ends on a
/// character boundary.
pub(crate) fn clip_to_char_boundary(text: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A temp file that removes itself on drop.
    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn file_with(tag: &str, bytes: &[u8]) -> Fixture {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "kensaku-delimited-{tag}-{}-{nanos}.csv",
            std::process::id()
        ));
        std::fs::write(&path, bytes).unwrap();
        Fixture(path)
    }

    fn rows(text: &str) -> Vec<String> {
        render_rows(text.as_bytes(), false, b',', usize::MAX, None)
            .map(|out| out.lines().map(str::to_string).collect::<Vec<String>>())
            .unwrap_or_default()
    }

    #[test]
    fn head_keeps_the_first_rows_and_a_typed_column_summary() {
        let text = "id,price,name
1,2.5,Ada
2,3,Bob
3,4,Cy
4,5,Di
";
        let out = render_rows(text.as_bytes(), false, b',', usize::MAX, Some(2)).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "columns (first rows sampled): id (integer), price (number), name (text)"
        );
        assert_eq!(lines.len(), 3, "summary plus two rows: {out}");
        assert!(lines[2].contains("name = \"Bob\""));
        assert!(!out.contains("Cy"), "rows past the head are not indexed");
    }

    #[test]
    fn head_over_a_short_file_is_not_marked_sampled() {
        let out = render_rows(b"a,b
1,x
", false, b',', usize::MAX, Some(200)).unwrap();
        assert!(out.starts_with("columns: a (integer), b (text)
"));
    }

    #[test]
    fn quoted_newline_stays_inside_one_row() {
        let text = "name,note\nAda,\"line one\nline two\"\n";
        assert_eq!(
            rows(text),
            vec!["name = \"Ada\", note = \"line one line two\""]
        );
    }

    #[test]
    fn semicolon_export_is_sniffed_and_keeps_decimal_commas() {
        assert_eq!(
            rows("name;amount\nAda;1,5\n"),
            vec!["name = \"Ada\", amount = \"1,5\""]
        );
    }

    #[test]
    fn tsv_file_uses_tab() {
        assert_eq!(
            render_rows(b"a\tb\n1\t2\n", false, b'\t', usize::MAX, None).as_deref(),
            Some("a = \"1\", b = \"2\"")
        );
    }

    #[test]
    fn tab_hint_wins_a_tie_against_commas_in_values() {
        assert_eq!(
            render_rows("a\tb,c\n1\t2,3\n".as_bytes(), false, b'\t', usize::MAX, None).as_deref(),
            Some("a = \"1\", b,c = \"2,3\"")
        );
    }

    #[test]
    fn utf8_bom_is_removed_before_the_header() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"name,city\nAda,London\n");
        assert_eq!(
            render_rows(&bytes, false, b',', usize::MAX, None).as_deref(),
            Some("name = \"Ada\", city = \"London\"")
        );
    }

    #[test]
    fn utf16_le_file_extracts() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(
            "name,city\nAda,London\n"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        let fixture = file_with("utf16", &bytes);
        assert_eq!(
            extract_delimited(&fixture.0, 10_000, 200).as_deref(),
            Some("columns: name (text), city (text)\nname = \"Ada\", city = \"London\"")
        );
    }

    #[test]
    fn empty_and_whitespace_only_files_are_none() {
        let empty = file_with("empty", b"");
        assert_eq!(extract_delimited(&empty.0, 1000, 200), None);
        let blank = file_with("blank", b"  \n\n");
        assert_eq!(extract_delimited(&blank.0, 1000, 200), None);
    }

    #[test]
    fn row_wider_than_the_cap_is_cut_not_dropped() {
        let text = format!("name\n{}\n", "x".repeat(300));
        let out = render_rows(text.as_bytes(), false, b',', 40, None).unwrap();
        assert!(out.len() <= 40);
        assert!(out.starts_with("name = \"xxx"));
    }

    #[test]
    fn malformed_input_does_not_panic_or_hang() {
        for text in ["a,b\n1,\"oops\n2,3", "\"", "a,\"b\"c,d\n", ",,,\n\"\"\"\n"] {
            let _ = rows(text);
        }
        assert!(!rows("a,b\n1,\"oops\n2,3").is_empty());
    }

    #[test]
    fn ragged_rows_keep_every_value() {
        assert_eq!(
            rows("a,b,c\n1,2\n1,2,3,4\n"),
            vec![
                "a = \"1\", b = \"2\"",
                "a = \"1\", b = \"2\", c = \"3\", column4 = \"4\"",
            ]
        );
    }

    #[test]
    fn header_only_file_indexes_the_column_names() {
        assert_eq!(rows("name,city\n"), vec!["name, city"]);
    }

    #[test]
    fn truncated_input_drops_the_partial_last_row() {
        assert_eq!(
            render_rows(b"a,b\n1,2\n3,4", true, b',', usize::MAX, None).as_deref(),
            Some("a = \"1\", b = \"2\"")
        );
    }

    #[test]
    fn truncated_input_keeps_rows_that_end_at_a_line_break() {
        assert_eq!(
            render_rows(b"a,b\n1,2\n", true, b',', usize::MAX, None).as_deref(),
            Some("a = \"1\", b = \"2\"")
        );
    }

    #[test]
    fn windows_line_endings_are_handled() {
        assert_eq!(rows("a,b\r\n1,2\r\n"), vec!["a = \"1\", b = \"2\""]);
    }

    #[test]
    fn clip_respects_character_boundaries() {
        assert_eq!(clip_to_char_boundary("héllo", 2), "h");
        assert_eq!(clip_to_char_boundary("héllo", 3), "hé");
    }
}
