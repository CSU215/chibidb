use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;

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

/// 写线程停在闭包里时，该帧被 pin；此时池满，缺页必须跳过它去淘汰别人。
/// 没有 pin 的实现会淘汰页 0（还干净，于是直接丢弃），闭包里的写入落进
/// 已脱离页表的孤儿帧 —— 之后读回的是盘上的旧值。
#[test]
fn a_pinned_frame_is_not_evicted() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = pool(&dir, "pin.dbf", 2);
    let bp = Arc::new(bp);

    // 页 1 常驻，干净（read_page 不置脏）。
    bp.read_page(f, 1, |_| Ok(())).unwrap();

    let (ready_tx, ready_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let writer = {
        let bp = Arc::clone(&bp);
        std::thread::spawn(move || {
            bp.with_page(f, 0, |p| {
                p[0] = 10;
                ready_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        })
    };

    ready_rx.recv().unwrap(); // 写线程已在闭包内：页 0 被 pin，页闩也被持有
    // 把页 1 提到 MRU，使页 0 成为 LRU 队首。
    bp.read_page(f, 1, |_| Ok(())).unwrap();
    // 容量 2 已满，这次缺页必须淘汰：页 0 被 pin，只能淘汰页 1。
    bp.with_page(f, 2, |p| {
        p[0] = 30;
        Ok(())
    })
    .unwrap();
    go_tx.send(()).unwrap();
    writer.join().unwrap();

    assert_eq!(bp.stats().evictions, 1, "应当恰好发生一次淘汰");
    let v = bp.read_page(f, 0, |p| Ok(p[0])).unwrap();
    assert_eq!(v, 10, "写入落进了被淘汰的孤儿帧");
}

/// 池里每一帧都被 pin 时，淘汰器无从下手 —— 这是引入 pin 之后才可达的分支，
/// 必须报错而不是悄悄丢写。
#[test]
fn a_fully_pinned_pool_reports_exhaustion() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = pool(&dir, "exhaust.dbf", 1);
    let bp = Arc::new(bp);

    let (ready_tx, ready_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let holder = {
        let bp = Arc::clone(&bp);
        std::thread::spawn(move || {
            bp.with_page(f, 0, |p| {
                p[0] = 1;
                ready_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        })
    };

    ready_rx.recv().unwrap();
    let err = bp
        .with_page(f, 1, |p| {
            p[0] = 2;
            Ok(())
        })
        .unwrap_err();
    assert!(err.to_string().contains("buffer pool exhausted"), "unexpected: {err}");
    go_tx.send(()).unwrap();
    holder.join().unwrap();

    // pin 释放后池恢复正常，缺页照样能换入。
    bp.with_page(f, 1, |p| {
        p[0] = 2;
        Ok(())
    })
    .unwrap();
    assert_eq!(bp.read_page(f, 1, |p| Ok(p[0])).unwrap(), 2);
}

/// 小池 + 多线程 + 频繁淘汰：每次成功自增都必须落到盘上，一次都不能丢。
/// 断言的是"最终状态等于成功次数"这一确定性性质，与线程交错无关。
#[test]
fn concurrent_writers_do_not_lose_updates_under_eviction() {
    const PAGES: u32 = 8;
    const THREADS: u32 = 4;
    const OPS: usize = 400;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("concurrent.dbf");
    let counts: Arc<Vec<AtomicU32>> =
        Arc::new((0..PAGES).map(|_| AtomicU32::new(0)).collect());

    {
        let mut disk = DiskManager::new();
        let f = disk.create_file(&path).unwrap();
        // 容量 4、4 个线程：每个线程最多持 1 个 pin，故总有可淘汰的帧，
        // 不会真的耗尽；保留重试分支以防实现细节变化。
        let bp = Arc::new(BufferPool::new(disk, 4));
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let bp = Arc::clone(&bp);
            let counts = Arc::clone(&counts);
            handles.push(std::thread::spawn(move || {
                for i in 0..OPS {
                    let page = ((t as usize * PAGES as usize + i) % PAGES as usize) as u32;
                    loop {
                        let r = bp.with_page(f, page, |p| {
                            let v = u32::from_le_bytes(p[0..4].try_into().unwrap());
                            p[0..4].copy_from_slice(&(v + 1).to_le_bytes());
                            Ok(())
                        });
                        match r {
                            Ok(()) => {
                                counts[page as usize].fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                            Err(_) => std::thread::yield_now(),
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    let mut disk = DiskManager::new();
    let f = disk.open_file(&path).unwrap();
    let bp = BufferPool::new(disk, PAGES as usize);
    for page in 0..PAGES {
        let v = bp
            .read_page(f, page, |p| Ok(u32::from_le_bytes(p[0..4].try_into().unwrap())))
            .unwrap();
        assert_eq!(v, counts[page as usize].load(Ordering::Relaxed), "page {page} 丢了写");
    }
}
