use chibidb::storage::page::PAGE_SIZE;
use chibidb::storage::slotted::{page_delete, page_get, page_insert, page_iter};

fn new_page() -> Box<[u8; PAGE_SIZE]> {
    Box::new([0u8; PAGE_SIZE])
}

#[test]
fn inserts_and_reads_back() {
    let mut page = new_page();
    let s0 = page_insert(&mut page, b"aaa").unwrap();
    let s1 = page_insert(&mut page, b"bbbbb").unwrap();
    assert_eq!(s0, 0);
    assert_eq!(s1, 1);
    assert_eq!(page_get(&page, 0).unwrap(), Some(&b"aaa"[..]));
    assert_eq!(page_get(&page, 1).unwrap(), Some(&b"bbbbb"[..]));

    let slots: Vec<(u16, &[u8])> = page_iter(&page).collect();
    assert_eq!(slots.len(), 2);
    assert_eq!(slots[0].1, b"aaa");
    assert_eq!(slots[1].1, b"bbbbb");
}

#[test]
fn get_on_invalid_slot_returns_none() {
    let mut page = new_page();
    page_insert(&mut page, b"x").unwrap();
    assert_eq!(page_get(&page, 5).unwrap(), None);
}

#[test]
fn delete_removes_record_and_compacts() {
    let mut page = new_page();
    page_insert(&mut page, b"one").unwrap();
    page_insert(&mut page, b"two").unwrap();
    page_insert(&mut page, b"three").unwrap();

    page_delete(&mut page, 1).unwrap();
    assert_eq!(page_get(&page, 1).unwrap(), None);

    let slots: Vec<(u16, &[u8])> = page_iter(&page).collect();
    assert_eq!(slots.len(), 2);
    assert_eq!(slots[0].1, b"one");
    assert_eq!(slots[1].1, b"three");
}

#[test]
fn freed_space_is_reusable_after_compact() {
    let mut page = new_page();
    let mut count = 0;
    for i in 0.. {
        let rec = vec![b'a' + (i % 26) as u8; 100];
        match page_insert(&mut page, &rec) {
            Ok(_) => count += 1,
            Err(_) => break,
        }
    }
    assert!(count > 10, "page should hold many 100-byte records, got {count}");
    let used_before = page_iter(&page).count();
    assert_eq!(used_before, count);

    // delete one record, its space must be reusable (compaction)
    page_delete(&mut page, 0).unwrap();
    assert_eq!(page_iter(&page).count(), used_before - 1);
    page_insert(&mut page, &[b'z'; 100]).unwrap();
    assert_eq!(page_iter(&page).count(), used_before);
}

#[test]
fn oversized_record_errors() {
    let mut page = new_page();
    assert!(page_insert(&mut page, &vec![b'x'; PAGE_SIZE]).is_err());
}

#[test]
fn delete_invalid_slot_errors() {
    let mut page = new_page();
    page_insert(&mut page, b"a").unwrap();
    assert!(page_delete(&mut page, 3).is_err());
    assert!(page_delete(&mut page, 0).is_ok());
    assert!(page_delete(&mut page, 0).is_err(), "double delete must fail");
}

#[test]
fn empty_page_iterates_nothing() {
    let page = new_page();
    assert_eq!(page_iter(&page).count(), 0);
}
