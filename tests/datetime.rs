use chibidb::datetime::{format_date, parse_date};

#[test]
fn epoch_is_zero() {
    assert_eq!(parse_date("1970-01-01").unwrap(), 0);
    assert_eq!(format_date(0), "1970-01-01");
}

#[test]
fn roundtrips_dates() {
    for s in ["2024-01-01", "2000-02-29", "2023-12-31", "1999-06-15", "2100-03-01"] {
        let d = parse_date(s).unwrap();
        assert_eq!(format_date(d), s, "roundtrip {s}");
    }
}

#[test]
fn orders_chronologically() {
    assert!(parse_date("2024-01-01").unwrap() < parse_date("2024-03-01").unwrap());
    assert!(parse_date("1999-12-31").unwrap() < parse_date("2000-01-01").unwrap());
}

#[test]
fn leap_rules() {
    assert!(parse_date("2024-02-29").is_ok(), "2024 is a leap year");
    assert!(parse_date("2000-02-29").is_ok(), "2000 divisible by 400");
    assert!(parse_date("1900-02-29").is_err(), "1900 not a leap year");
    assert!(parse_date("2023-02-29").is_err());
}

#[test]
fn rejects_malformed_dates() {
    assert!(parse_date("2024-13-01").is_err(), "month 13");
    assert!(parse_date("2024-00-10").is_err(), "month 0");
    assert!(parse_date("2024-01-00").is_err(), "day 0");
    assert!(parse_date("2024-01-32").is_err(), "day 32");
    assert!(parse_date("2024-4-1").is_err(), "unpadded");
    assert!(parse_date("2024/01/01").is_err(), "slashes");
    assert!(parse_date("24-01-01").is_err(), "short year");
    assert!(parse_date("abcd-ef-gh").is_err(), "not digits");
    assert!(parse_date("").is_err());
}
