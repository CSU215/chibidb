use chaoticdb::index::node::{
    internal_child_for, internal_init, internal_insert_entry, internal_num,
    leaf_entries, leaf_init, leaf_insert_at, leaf_lower_bound, leaf_remove_at,
};
use chaoticdb::storage::{Rid, PAGE_SIZE};

fn leaf_page() -> Box<[u8; PAGE_SIZE]> {
    let mut page = Box::new([0u8; PAGE_SIZE]);
    leaf_init(&mut page, 0, 0);
    page
}

fn no_rid() -> Rid {
    Rid::new(0, 0)
}

#[test]
fn leaf_roundtrip_entries() {
    let mut page = leaf_page();
    leaf_insert_at(&mut page, 0, b"k1", Rid::new(5, 0)).unwrap();
    leaf_insert_at(&mut page, 1, b"k3", Rid::new(5, 1)).unwrap();
    // insert in the middle shifts the rest
    leaf_insert_at(&mut page, 1, b"k2", Rid::new(5, 2)).unwrap();

    let entries: Vec<(Vec<u8>, Rid)> = leaf_entries(&page[..]).collect();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0], (b"k1".to_vec(), Rid::new(5, 0)));
    assert_eq!(entries[1], (b"k2".to_vec(), Rid::new(5, 2)));
    assert_eq!(entries[2], (b"k3".to_vec(), Rid::new(5, 1)));
}

#[test]
fn leaf_lower_bound_finds_first_ge() {
    let mut page = leaf_page();
    leaf_insert_at(&mut page, 0, b"1", Rid::new(1, 0)).unwrap();
    leaf_insert_at(&mut page, 1, b"3", Rid::new(1, 1)).unwrap();
    leaf_insert_at(&mut page, 2, b"5", Rid::new(1, 2)).unwrap();

    assert_eq!(leaf_lower_bound(&page[..], b"0", no_rid()), 0);
    assert_eq!(leaf_lower_bound(&page[..], b"1", no_rid()), 0);
    assert_eq!(leaf_lower_bound(&page[..], b"2", no_rid()), 1);
    assert_eq!(leaf_lower_bound(&page[..], b"3", no_rid()), 1);
    assert_eq!(leaf_lower_bound(&page[..], b"5", no_rid()), 2);
    assert_eq!(leaf_lower_bound(&page[..], b"6", no_rid()), 3);
}

#[test]
fn leaf_remove_shifts_and_frees_space() {
    let mut page = leaf_page();
    for i in 0..10 {
        leaf_insert_at(&mut page, i as usize, b"key", Rid::new(1, i)).unwrap();
    }
    leaf_remove_at(&mut page, 4).unwrap();
    let entries: Vec<(Vec<u8>, Rid)> = leaf_entries(&page[..]).collect();
    assert_eq!(entries.len(), 9);
    // removing frees space for one more insert
    leaf_insert_at(&mut page, 9, b"key", Rid::new(2, 0)).unwrap();
    assert_eq!(leaf_entries(&page[..]).count(), 10);
}

#[test]
fn leaf_rejects_when_full() {
    let mut page = leaf_page();
    let mut i = 0u32;
    while leaf_insert_at(&mut page, i as usize, b"x", Rid::new(1, i as u16)).is_ok() {
        i += 1;
    }
    assert!(i > 100, "page should hold many entries, got {i}");
    assert!(leaf_insert_at(&mut page, i as usize, b"x", Rid::new(9, 9)).is_err());
    leaf_remove_at(&mut page, 0).unwrap();
    assert!(leaf_insert_at(&mut page, i as usize - 1, b"x", Rid::new(9, 9)).is_ok());
}

#[test]
fn internal_routes_to_children() {
    let mut page = Box::new([0u8; PAGE_SIZE]);
    internal_init(&mut page, 100);
    // separators: child 100 covers k < 10; child 101 covers 10 <= k < 20; child 102 covers k >= 20
    internal_insert_entry(&mut page, 0, b"10", no_rid(), 101).unwrap();
    internal_insert_entry(&mut page, 1, b"20", no_rid(), 102).unwrap();

    assert_eq!(internal_num(&page[..]), 2);
    assert_eq!(internal_child_for(&page[..], b"05", no_rid()), 100);
    assert_eq!(internal_child_for(&page[..], b"10", no_rid()), 101);
    assert_eq!(internal_child_for(&page[..], b"15", no_rid()), 101);
    assert_eq!(internal_child_for(&page[..], b"20", no_rid()), 102);
    assert_eq!(internal_child_for(&page[..], b"99", no_rid()), 102);
    assert_eq!(internal_child_for(&page[..], b"", no_rid()), 100, "empty key goes leftmost");
}

#[test]
fn internal_insert_shifts_children() {
    let mut page = Box::new([0u8; PAGE_SIZE]);
    internal_init(&mut page, 1);
    internal_insert_entry(&mut page, 0, b"30", no_rid(), 2).unwrap();
    // insert a separator between first_child(1) and child 2
    internal_insert_entry(&mut page, 0, b"10", no_rid(), 3).unwrap();
    assert_eq!(internal_child_for(&page[..], b"05", no_rid()), 1);
    assert_eq!(internal_child_for(&page[..], b"10", no_rid()), 3);
    assert_eq!(internal_child_for(&page[..], b"30", no_rid()), 2);
}
