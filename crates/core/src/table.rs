//! Plain-text column layout for the CLI's tables (`pods list`, `gpus`).
//!
//! Widths are computed from the content instead of hard-coded `{:<22}` specs, so a long
//! machine name or GPU label never shoves the following columns out of line. Pure (returns
//! a `String`) so the rendered tables can be snapshot-tested.

/// How a column's cells are padded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    /// Numbers (VRAM, prices) read best right-aligned.
    Right,
}

/// Render `headers` + `rows` as aligned columns separated by two spaces, one line per row
/// (each ending in `\n`). Widths count `char`s, not bytes, so labels like `1×A4000` or
/// `€0.006` line up. Trailing padding is trimmed so lines don't end in spaces. A row
/// shorter than `headers` is padded with empty cells; `align` defaults to `Left` for
/// columns it doesn't cover.
pub fn render(headers: &[&str], align: &[Align], rows: &[Vec<String>]) -> String {
    let ncol = headers.len();
    let len = |s: &str| s.chars().count();
    let mut widths: Vec<usize> = headers.iter().map(|h| len(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().take(ncol).enumerate() {
            widths[i] = widths[i].max(len(cell));
        }
    }
    let line = |cells: Vec<&str>| -> String {
        let mut out = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            let pad = " ".repeat(widths[i].saturating_sub(len(cell)));
            match align.get(i).copied().unwrap_or(Align::Left) {
                Align::Left => {
                    out.push_str(cell);
                    out.push_str(&pad);
                }
                Align::Right => {
                    out.push_str(&pad);
                    out.push_str(cell);
                }
            }
        }
        let mut out = out.trim_end().to_string();
        out.push('\n');
        out
    };
    let mut out = line(headers.to_vec());
    for row in rows {
        let cells: Vec<&str> = (0..ncol).map(|i| row.get(i).map(String::as_str).unwrap_or("")).collect();
        out.push_str(&line(cells));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligns_by_char_width_and_trims_trailing_space() {
        let rows = vec![
            vec!["a".to_string(), "1×A4000".to_string(), "€0.006".to_string()],
            vec!["longer-name".to_string(), "-".to_string(), "$12.00".to_string()],
        ];
        let out = render(&["NAME", "GPU", "$/H"], &[Align::Left, Align::Left, Align::Right], &rows);
        assert_eq!(
            out,
            "NAME         GPU         $/H\n\
             a            1×A4000  €0.006\n\
             longer-name  -        $12.00\n"
        );
    }

    #[test]
    fn short_rows_are_padded_not_panicking() {
        let out = render(&["A", "B"], &[], &[vec!["x".to_string()]]);
        assert_eq!(out, "A  B\nx\n");
    }
}
