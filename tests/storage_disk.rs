use chibidb::storage::{DiskManager, PAGE_SIZE};

#[test]
fn writes_and_reads_pages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.dbf");

    let mut disk = DiskManager::new();
    let file = disk.create_file(&path).unwrap();

    let mut page = [0u8; PAGE_SIZE];
    page[0..4].copy_from_slice(&42u32.to_le_bytes());
    page[PAGE_SIZE - 1] = 7;
    disk.write_page(file, 0, &page).unwrap();

    let mut page3 = [0u8; PAGE_SIZE];
    page3[10] = 99;
    disk.write_page(file, 3, &page3).unwrap();
    drop(disk);

    let mut disk = DiskManager::new();
    let file = disk.open_file(&path).unwrap();

    let mut buf = [0u8; PAGE_SIZE];
    disk.read_page(file, 0, &mut buf).unwrap();
    assert_eq!(&buf[0..4], &42u32.to_le_bytes());
    assert_eq!(buf[PAGE_SIZE - 1], 7);

    disk.read_page(file, 3, &mut buf).unwrap();
    assert_eq!(buf[10], 99);

    assert_eq!(disk.page_count(file).unwrap(), 4);
}

#[test]
fn reads_unallocated_page_as_zeros() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.dbf");

    let mut disk = DiskManager::new();
    let file = disk.create_file(&path).unwrap();

    let mut buf = [1u8; PAGE_SIZE];
    disk.read_page(file, 5, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0));
    assert_eq!(disk.page_count(file).unwrap(), 0);
}

#[test]
fn create_existing_file_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.dbf");

    let mut disk = DiskManager::new();
    disk.create_file(&path).unwrap();
    assert!(disk.create_file(&path).is_err());
}
