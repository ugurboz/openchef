//! Small rendering helpers.

/// Lays out rows in left-aligned columns separated by two spaces (like Go's
/// tabwriter). The last column is not padded.
pub(crate) fn table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let last = row.len().saturating_sub(1);
        for (i, cell) in row.iter().enumerate() {
            out.push_str(cell);
            if i < last {
                let pad = widths[i] - cell.chars().count() + 2;
                out.extend(std::iter::repeat_n(' ', pad));
            }
        }
        out.push('\n');
    }
    out
}

/// `-` for an empty value.
pub(crate) fn or_dash(s: Option<&str>) -> String {
    match s {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => "-".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn table_aligns_columns() {
        let rows = vec![
            vec!["a".to_owned(), "bb".to_owned(), "c".to_owned()],
            vec!["aaa".to_owned(), "b".to_owned(), "c".to_owned()],
        ];
        assert_eq!(super::table(&rows), "a    bb  c\naaa  b   c\n");
    }
}
