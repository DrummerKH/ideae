//! A cell of a result, as the column's type says it is: NULL is not an empty
//! string, and a BIGINT or a DECIMAL never passes through a float.

use std::borrow::Cow;
use std::cmp::Ordering;

use mysql::consts::{ColumnFlags, ColumnType};

/// The character set MySQL calls `binary`.
const BINARY: u16 = 63;

/// How many bytes of a binary value are written out.
const BYTES_SHOWN: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cell {
    Null,
    Int(i64),
    UInt(u64),
    /// As the server wrote it: exact.
    Decimal(Box<str>),
    /// As the server wrote it: what it shows is what is kept.
    Float(Box<str>),
    Text(Box<str>),
    Bytes(Box<[u8]>),
}

pub type Row = Box<[Cell]>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Int { unsigned: bool },
    Decimal,
    Float,
    Text,
    Binary,
}

impl Kind {
    pub fn of(column: ColumnType, flags: ColumnFlags, charset: u16) -> Kind {
        use ColumnType::*;
        match column {
            MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_YEAR => {
                Kind::Int { unsigned: flags.contains(ColumnFlags::UNSIGNED_FLAG) }
            }
            MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => Kind::Decimal,
            MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => Kind::Float,
            MYSQL_TYPE_BIT | MYSQL_TYPE_GEOMETRY => Kind::Binary,
            MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR | MYSQL_TYPE_STRING
                if charset == BINARY =>
            {
                Kind::Binary
            }
            _ => Kind::Text,
        }
    }

    pub fn right_aligned(self) -> bool {
        matches!(self, Kind::Int { .. } | Kind::Decimal | Kind::Float)
    }

    /// A value as the text protocol brings it.
    pub fn cell(self, value: mysql::Value) -> Cell {
        let bytes = match value {
            mysql::Value::NULL => return Cell::Null,
            mysql::Value::Bytes(bytes) => bytes,
            other => other.as_sql(true).into_bytes(),
        };
        let text = || std::str::from_utf8(&bytes).ok();
        match self {
            Kind::Int { unsigned: false } => text().and_then(|t| t.parse().ok()).map(Cell::Int),
            Kind::Int { unsigned: true } => text().and_then(|t| t.parse().ok()).map(Cell::UInt),
            Kind::Decimal => text().map(|t| Cell::Decimal(t.into())),
            Kind::Float => text().map(|t| Cell::Float(t.into())),
            Kind::Text | Kind::Binary => None,
        }
        .unwrap_or_else(|| match (self, String::from_utf8(bytes)) {
            (Kind::Binary, Ok(text)) => Cell::Bytes(text.into_bytes().into()),
            (_, Ok(text)) => Cell::Text(text.into()),
            (_, Err(e)) => Cell::Bytes(e.into_bytes().into()),
        })
    }
}

impl Cell {
    pub fn is_null(&self) -> bool {
        matches!(self, Cell::Null)
    }

    /// As a cell of the table shows it.
    pub fn shown(&self) -> Cow<'_, str> {
        match self {
            Cell::Null => "NULL".into(),
            Cell::Int(n) => n.to_string().into(),
            Cell::UInt(n) => n.to_string().into(),
            Cell::Decimal(t) | Cell::Float(t) | Cell::Text(t) => Cow::Borrowed(t),
            Cell::Bytes(b) => {
                let mut out = hex(&b[..b.len().min(BYTES_SHOWN)]);
                if b.len() > BYTES_SHOWN {
                    out.push('…');
                }
                out.into()
            }
        }
    }

    /// Whole, for a copy: NULL as an empty field of a TSV would be lost, so it stays `NULL`.
    pub fn copied(&self) -> Cow<'_, str> {
        match self {
            Cell::Bytes(b) => hex(b).into(),
            other => other.shown(),
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Cell::Null => 0,
            Cell::Int(_) | Cell::UInt(_) | Cell::Decimal(_) | Cell::Float(_) => 1,
            Cell::Text(_) => 2,
            Cell::Bytes(_) => 3,
        }
    }

    /// For sorting a column: NULL first, numbers by their exact value.
    pub fn compare(&self, other: &Cell) -> Ordering {
        match (self, other) {
            (Cell::Int(a), Cell::Int(b)) => a.cmp(b),
            (Cell::UInt(a), Cell::UInt(b)) => a.cmp(b),
            (Cell::Text(a), Cell::Text(b)) => a.cmp(b),
            (Cell::Bytes(a), Cell::Bytes(b)) => a.cmp(b),
            (Cell::Float(a), Cell::Float(b)) => float(a).total_cmp(&float(b)),
            (a, b) if a.rank() == 1 && b.rank() == 1 => match (a, b) {
                (Cell::Float(_), _) | (_, Cell::Float(_)) => float(&a.shown()).total_cmp(&float(&b.shown())),
                _ => compare_decimal(&a.shown(), &b.shown()),
            },
            (a, b) => a.rank().cmp(&b.rank()),
        }
    }
}

/// What the status line says of the cells selected.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub cells: usize,
    pub nulls: usize,
    pub numbers: usize,
    pub sum: Option<String>,
    pub average: Option<String>,
    pub least: Option<String>,
    pub most: Option<String>,
}

/// Digits past the point an exact sum goes to; past them it is a float's.
const MOST_SCALE: usize = 18;

fn scaled(text: &str, scale: usize) -> Option<i128> {
    let (negative, digits) = text.strip_prefix('-').map_or((false, text), |rest| (true, rest));
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if fraction.len() > scale || !whole.bytes().chain(fraction.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut value: i128 = 0;
    for b in whole.bytes().chain(fraction.bytes()) {
        value = value.checked_mul(10)?.checked_add(i128::from(b - b'0'))?;
    }
    value = value.checked_mul(10i128.checked_pow((scale - fraction.len()) as u32)?)?;
    Some(if negative { -value } else { value })
}

fn unscaled(value: i128, scale: usize) -> String {
    let digits = value.unsigned_abs().to_string();
    let sign = if value < 0 { "-" } else { "" };
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    let digits = format!("{digits:0>width$}", width = scale + 1);
    let (whole, fraction) = digits.split_at(digits.len() - scale);
    format!("{sign}{whole}.{fraction}")
}

/// Exact for integers and decimals; with a float among them, as a float is.
pub fn summarize<'a>(cells: impl Iterator<Item = &'a Cell>) -> Summary {
    let mut out = Summary::default();
    let (mut texts, mut any_float): (Vec<String>, bool) = (Vec::new(), false);
    let (mut least, mut most): (Option<&Cell>, Option<&Cell>) = (None, None);
    for cell in cells {
        out.cells += 1;
        match cell {
            Cell::Null => out.nulls += 1,
            Cell::Int(_) | Cell::UInt(_) | Cell::Decimal(_) | Cell::Float(_) => {
                any_float |= matches!(cell, Cell::Float(_));
                texts.push(cell.shown().into_owned());
                if least.is_none_or(|l| cell.compare(l).is_lt()) {
                    least = Some(cell);
                }
                if most.is_none_or(|m| cell.compare(m).is_gt()) {
                    most = Some(cell);
                }
            }
            _ => {}
        }
    }
    out.numbers = texts.len();
    if texts.is_empty() {
        return out;
    }
    out.least = least.map(|c| c.shown().into_owned());
    out.most = most.map(|c| c.shown().into_owned());
    let scale = texts.iter().map(|t| t.split_once('.').map_or(0, |(_, f)| f.len())).max().unwrap_or(0);
    let exact = (!any_float && scale <= MOST_SCALE)
        .then(|| texts.iter().try_fold(0i128, |sum, t| sum.checked_add(scaled(t, scale)?)))
        .flatten();
    match exact {
        Some(sum) => {
            out.sum = Some(unscaled(sum, scale));
            // Two digits past the values', rounded half away from zero.
            let average = sum.checked_mul(1000).map(|n| n / texts.len() as i128).map(|n| (n + n.signum() * 5) / 10);
            out.average = Some(average.map_or_else(|| (sum as f64 / texts.len() as f64 / 10f64.powi(scale as i32)).to_string(), |n| unscaled(n, scale + 2)));
        }
        None => {
            let sum: f64 = texts.iter().map(|t| float(t)).sum();
            out.sum = Some(format!("≈{sum}"));
            out.average = Some(format!("≈{}", sum / texts.len() as f64));
        }
    }
    out
}

fn float(text: &str) -> f64 {
    text.parse().unwrap_or(f64::NEG_INFINITY)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Two decimals of any length, by their digits.
pub fn compare_decimal(a: &str, b: &str) -> Ordering {
    let parts = |text: &'_ str| -> (bool, String, String) {
        let (negative, digits) = text.strip_prefix('-').map_or((false, text), |rest| (true, rest));
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        (negative, whole.trim_start_matches('0').to_string(), fraction.trim_end_matches('0').to_string())
    };
    let (a_negative, a_whole, a_fraction) = parts(a);
    let (b_negative, b_whole, b_fraction) = parts(b);
    let zero = |whole: &str, fraction: &str| whole.is_empty() && fraction.is_empty();
    let a_negative = a_negative && !zero(&a_whole, &a_fraction);
    let b_negative = b_negative && !zero(&b_whole, &b_fraction);
    if a_negative != b_negative {
        return if a_negative { Ordering::Less } else { Ordering::Greater };
    }
    let size = a_whole.len().cmp(&b_whole.len()).then_with(|| a_whole.cmp(&b_whole)).then_with(|| a_fraction.cmp(&b_fraction));
    if a_negative { size.reverse() } else { size }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sum_of_money_is_exact() {
        let cells = [Cell::Decimal("0.10".into()), Cell::Decimal("0.20".into()), Cell::Null, Cell::Int(1), Cell::Text("x".into())];
        let summary = summarize(cells.iter());
        assert_eq!((summary.cells, summary.nulls, summary.numbers), (5, 1, 3));
        assert_eq!(summary.sum.as_deref(), Some("1.30"), "not 1.3000000000000000444");
        assert_eq!(summary.average.as_deref(), Some("0.4333"));
        assert_eq!((summary.least.as_deref(), summary.most.as_deref()), (Some("0.10"), Some("1")));
        let big = [Cell::Int(9_007_199_254_740_993), Cell::Int(1)];
        assert_eq!(summarize(big.iter()).sum.as_deref(), Some("9007199254740994"));
        let negative = [Cell::Decimal("-1.50".into()), Cell::Decimal("0.25".into())];
        assert_eq!(summarize(negative.iter()).sum.as_deref(), Some("-1.25"));
    }

    #[test]
    fn a_float_among_them_is_said_to_be_approximate() {
        let cells = [Cell::Float("1.5".into()), Cell::Int(1)];
        assert_eq!(summarize(cells.iter()).sum.as_deref(), Some("≈2.5"));
    }

    #[test]
    fn null_is_not_an_empty_string() {
        assert_eq!(Kind::Text.cell(mysql::Value::NULL), Cell::Null);
        assert_eq!(Kind::Text.cell(mysql::Value::Bytes(Vec::new())), Cell::Text("".into()));
        assert!(Cell::Null.compare(&Cell::Text("".into())).is_lt());
    }

    #[test]
    fn a_big_integer_keeps_every_digit_and_sorts_by_them() {
        let (a, b) = (9_007_199_254_740_993_i64, 9_007_199_254_740_992_i64);
        let kind = Kind::Int { unsigned: false };
        let cell = |n: i64| kind.cell(mysql::Value::Bytes(n.to_string().into_bytes()));
        assert_eq!(cell(a).shown(), a.to_string());
        assert!(cell(a).compare(&cell(b)).is_gt());
        let top = Kind::Int { unsigned: true }.cell(mysql::Value::Bytes(u64::MAX.to_string().into_bytes()));
        assert_eq!(top, Cell::UInt(u64::MAX));
    }

    #[test]
    fn decimals_sort_by_value_at_any_length() {
        use Ordering::*;
        for (a, b, order) in [
            ("10.00", "9.99", Greater),
            ("-10.00", "-9.99", Less),
            ("1.10", "1.1", Equal),
            ("0.00", "-0.00", Equal),
            ("-0.01", "0.00", Less),
            ("12345678901234567890.000000000000000002", "12345678901234567890.000000000000000001", Greater),
        ] {
            assert_eq!(compare_decimal(a, b), order, "{a} against {b}");
        }
    }

    #[test]
    fn a_binary_value_is_shown_in_hex_and_copied_whole() {
        let bytes = vec![0xab; 40];
        let cell = Kind::Binary.cell(mysql::Value::Bytes(bytes));
        assert!(cell.shown().ends_with('…'));
        assert_eq!(cell.copied().len(), 2 + 80);
    }

    #[test]
    fn text_that_is_not_utf8_is_kept_as_bytes() {
        assert_eq!(Kind::Text.cell(mysql::Value::Bytes(vec![0xff, 0x00])), Cell::Bytes(vec![0xff, 0x00].into()));
    }
}
