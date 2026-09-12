use crate::{Error, Result};

/// Days since 1970-01-01 (Howard Hinnant's civil-date algorithms).
pub fn parse_date(s: &str) -> Result<i32> {
    let b = s.as_bytes();
    let malformed = || Error::Runtime(format!("invalid date '{s}', expected YYYY-MM-DD"));
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || !b[0..4].iter().all(u8::is_ascii_digit)
        || !b[5..7].iter().all(u8::is_ascii_digit)
        || !b[8..10].iter().all(u8::is_ascii_digit)
    {
        return Err(malformed());
    }
    let year: i64 = s[0..4].parse().map_err(|_| malformed())?;
    let month: u32 = s[5..7].parse().map_err(|_| malformed())?;
    let day: u32 = s[8..10].parse().map_err(|_| malformed())?;
    if !(1..=12).contains(&month) {
        return Err(malformed());
    }
    if day == 0 || day > days_in_month(year, month) {
        return Err(malformed());
    }
    Ok(days_from_civil(year, month, day) as i32)
}

pub fn format_date(days: i32) -> String {
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
