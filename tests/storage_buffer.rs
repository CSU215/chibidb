use std::sync::Arc;

use chibidb::storage::{BufferPool, DiskManager, PAGE_SIZE};

fn pool(dir: &tempfile::TempDir, name: &str, cap: usize) -> (BufferPool, u32) {
    let mut disk = DiskManager::new();
    let file = disk.create_file(&dir.path().join(name)).unwrap();
    (BufferPool::new(disk, cap), file)
}

#[test]
fn new_page_is_zeroed() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = pool(&dir, "a.dbf", 4);
    let seen = bp.with_page(f, 0, |p| Ok(p[123] == 0)).unwrap();
    assert!(seen);
}

#[test]
fn writes_are_visible_through_pool() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = pool(&dir, "b.dbf", 4);
    bp.with_page(f, 1, |p| {
        p[0..4].copy_from_slice(&7u32.to_le_bytes());
        Ok(())
    })
    .unwrap();
    let v =
        bp.with_page(f, 1, |p| Ok(u32::from_le_bytes(p[0..4].try_into().unwrap()))).unwrap();
    assert_eq!(v, 7);
}

#[test]
fn data_survives_pool_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.dbf");

    {
        let mut disk = DiskManager::new();
        let f = disk.create_file(&path).unwrap();
        let bp = BufferPool::new(disk, 4);
        bp.with_page(f, 2, |p| {
            p[5] = 42;
            Ok(())
        })
        .unwrap();
    }

    let mut disk = DiskManager::new();
    let f = disk.open_file(&path).unwrap();
    let bp = BufferPool::new(disk, 4);
    let v = bp.with_page(f, 2, |p| Ok(p[5])).unwrap();
    assert_eq!(v, 42);
}

#[test]
fn evicts_least_recently_used() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = pool(&dir, "d.dbf", 2);

    bp.with_page(f, 1, |p| {
        p[0] = 10;
        Ok(())
    })
    .unwrap();
    bp.with_page(f, 2, |p| {
        p[0] = 20;
        Ok(())
    })
    .unwrap();
    // touch page 1: now page 2 is LRU
    let v = bp.with_page(f, 1, |p| Ok(p[0])).unwrap();
    assert_eq!(v, 10);
    // forces eviction of page 2
    bp.with_page(f, 3, |p| {
        p[0] = 30;
        Ok(())
    })
    .unwrap();
    // page 1 still has its data, page 2 must come back from disk
    let v = bp.with_page(f, 1, |p| Ok(p[0])).unwrap();
    assert_eq!(v, 10);
    let v = bp.with_page(f, 2, |p| Ok(p[0])).unwrap();
    assert_eq!(v, 20);
    let v = bp.with_page(f, 3, |p| Ok(p[0])).unwrap();
    assert_eq!(v, 30);
}

#[test]
fn alloc_pages_are_appended_and_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.dbf");

    {
        let mut disk = DiskManager::new();
        let f = disk.create_file(&path).unwrap();
        let bp = BufferPool::new(disk, 4);
        assert_eq!(bp.alloc_page(f).unwrap(), 0);
        assert_eq!(bp.alloc_page(f).unwrap(), 1);
        let no = bp.alloc_page(f).unwrap();
        assert_eq!(no, 2);
        bp.with_page(f, no, |p| {
            p[0] = 9;
            Ok(())
        })
        .unwrap();
    }

    let mut disk = DiskManager::new();
    let f = disk.open_file(&path).unwrap();
    let bp = BufferPool::new(disk, 4);
    assert_eq!(bp.page_count(f).unwrap(), 3);
    let v = bp.with_page(f, 2, |p| Ok(p[0])).unwrap();
    assert_eq!(v, 9);
}

#[test]
fn page_spans_full_page_size() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = pool(&dir, "f.dbf", 2);
    let last = bp.with_page(f, 0, |p| Ok(p[PAGE_SIZE - 1])).unwrap();
    assert_eq!(last, 0);
}

#[test]
fn shared_pool_reads_and_writes_concurrently() {
    let dir = tempfile::tempdir().unwrap();
    let mut disk = DiskManager::new();
    let f = disk.create_file(&dir.path().join("g.dbf")).unwrap();
    let bp = Arc::new(BufferPool::new(disk, 8));
    for no in 0..4u32 {
        bp.with_page(f, no, |p| {
            p[0] = no as u8;
            Ok(())
        })
        .unwrap();
    }

    let mut handles = Vec::new();
    for _ in 0..8 {
        let bp = Arc::clone(&bp);
        handles.push(std::thread::spawn(move || {
            for _ in 0..200 {
                for no in 0..12u32 {
                    let page = no % 4;
                    let seen = bp.read_page(f, page, |p| Ok(p[0])).unwrap();
                    assert_eq!(seen, page as u8, "torn read on page {page}");
                }
            }
        }));
    }
    for w in 0..4u32 {
        let bp = Arc::clone(&bp);
        handles.push(std::thread::spawn(move || {
            for _ in 0..100 {
                bp.with_page(f, w, |p| {
                    p[16] = w as u8;
                    Ok(())
                })
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    for no in 0..4u32 {
        let v = bp.read_page(f, no, |p| Ok(p[16])).unwrap();
        assert_eq!(v, no as u8);
    }
}
