//! CSV read record by record, however long the file: quotes, doubled quotes,
//! line ends inside quotes, CRLF, and a separator told from its first line.

use std::io::{self, BufRead};

/// The separator its first line has most of, outside quotes.
pub fn separator_of(first_line: &str) -> u8 {
    let mut counts = [0usize; 4];
    let mut quoted = false;
    for b in first_line.bytes() {
        match b {
            b'"' => quoted = !quoted,
            _ if quoted => {}
            b',' => counts[0] += 1,
            b';' => counts[1] += 1,
            b'\t' => counts[2] += 1,
            b'|' => counts[3] += 1,
            _ => {}
        }
    }
    let seps = [b',', b';', b'\t', b'|'];
    let best = (0..4).max_by_key(|&i| (counts[i], 4 - i)).unwrap_or(0);
    if counts[best] == 0 { b',' } else { seps[best] }
}

/// A field as read, and whether it was quoted: `""` is an empty string, and
/// nothing between two separators is what a NULL is written as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub text: String,
    pub quoted: bool,
}

pub struct Records<R> {
    reader: R,
    separator: u8,
}

impl<R: BufRead> Records<R> {
    pub fn new(reader: R, separator: u8) -> Self {
        Records { reader, separator }
    }

    /// The next record; None at the end of the file.
    pub fn next_record(&mut self) -> io::Result<Option<Vec<Field>>> {
        let mut line = Vec::new();
        if self.reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(None);
        }
        // A line end inside quotes: the record goes on.
        while line.iter().filter(|b| **b == b'"').count() % 2 == 1 {
            if self.reader.read_until(b'\n', &mut line)? == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "a quote is not closed"));
            }
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        let (mut fields, mut field, mut quoted, mut inside, mut i) = (Vec::new(), Vec::new(), false, false, 0);
        while i < line.len() {
            let b = line[i];
            if inside {
                if b == b'"' && line.get(i + 1) == Some(&b'"') {
                    field.push(b'"');
                    i += 1;
                } else if b == b'"' {
                    inside = false;
                } else {
                    field.push(b);
                }
            } else if b == b'"' && field.is_empty() {
                inside = true;
                quoted = true;
            } else if b == self.separator {
                fields.push(Field { text: into_text(std::mem::take(&mut field))?, quoted });
                quoted = false;
            } else {
                field.push(b);
            }
            i += 1;
        }
        fields.push(Field { text: into_text(field)?, quoted });
        Ok(Some(fields))
    }
}

fn into_text(bytes: Vec<u8>) -> io::Result<String> {
    String::from_utf8(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "the file is not UTF-8"))
}

/// Its column names, how many records follow, and its separator.
pub fn look_at(path: &std::path::Path) -> io::Result<(Vec<String>, usize, u8)> {
    let mut first = String::new();
    io::BufReader::new(std::fs::File::open(path)?).read_line(&mut first)?;
    let first = first.strip_prefix('\u{feff}').unwrap_or(&first).to_string();
    let separator = separator_of(&first);
    let mut records = opened(path, separator)?;
    let header = records.next_record()?.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the file is empty"))?;
    let mut count = 0;
    while let Some(record) = records.next_record()? {
        if record.len() != header.len() && !(record.len() == 1 && record[0].text.is_empty()) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("record {} has {} fields, the header {}", count + 1, record.len(), header.len())));
        }
        if !(record.len() == 1 && record[0].text.is_empty()) {
            count += 1;
        }
    }
    Ok((header.into_iter().map(|f| f.text.trim().to_string()).collect(), count, separator))
}

/// The file's records from its first, past a byte-order mark.
pub fn opened(path: &std::path::Path, separator: u8) -> io::Result<Records<io::BufReader<std::fs::File>>> {
    let mut reader = io::BufReader::new(std::fs::File::open(path)?);
    if reader.fill_buf()?.starts_with(b"\xef\xbb\xbf") {
        reader.consume(3);
    }
    Ok(Records::new(reader, separator))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(text: &str, separator: u8) -> Vec<Vec<(String, bool)>> {
        let mut records = Records::new(text.as_bytes(), separator);
        let mut out = Vec::new();
        while let Some(record) = records.next_record().unwrap() {
            out.push(record.into_iter().map(|f| (f.text, f.quoted)).collect());
        }
        out
    }

    #[test]
    fn quotes_line_ends_and_empties_are_read_as_written() {
        let found = all("a,b\r\n\"x, y\",\"say \"\"hi\"\"\"\n\"two\nlines\",\n\"\",z\n", b',');
        assert_eq!(found[1][0].0, "x, y");
        assert_eq!(found[1][1].0, "say \"hi\"");
        assert_eq!(found[2][0].0, "two\nlines");
        assert_eq!(found[2][1], (String::new(), false), "nothing: what a NULL is written as");
        assert_eq!(found[3][0], (String::new(), true), "two quotes: an empty string");
    }

    #[test]
    fn the_separator_is_told_from_the_first_line() {
        assert_eq!(separator_of("a;b;c"), b';');
        assert_eq!(separator_of("a\tb"), b'\t');
        assert_eq!(separator_of("\"a;b\",c"), b',');
    }

    #[test]
    fn a_quote_never_closed_is_an_error() {
        assert!(Records::new("\"open\n".as_bytes(), b',').next_record().is_err());
    }
}
