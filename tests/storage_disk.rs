use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use chibidb::storage::{BufferPool, DiskManager, PAGE_SIZE};

/// "挂住"和"失败"不是一回事：`cargo test` 不会给用例超时，锁序写错时进程会一直等下去。
/// 这个上界把死锁变成一条可读的断言失败。
const PATIENCE: Duration = Duration::from_secs(20);

#[test]
fn writes_and_reads_pages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.dbf");

    let disk = DiskManager::new();
    let file = disk.create_file(&path).unwrap();

    let mut page = [0u8; PAGE_SIZE];
    page[0..4].copy_from_slice(&42u32.to_le_bytes());
    page[PAGE_SIZE - 1] = 7;
    disk.write_page(file, 0, &page).unwrap();

    let mut page3 = [0u8; PAGE_SIZE];
    page3[10] = 99;
    disk.write_page(file, 3, &page3).unwrap();
    drop(disk);

    let disk = DiskManager::new();
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

    let disk = DiskManager::new();
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

    let disk = DiskManager::new();
    disk.create_file(&path).unwrap();
    assert!(disk.create_file(&path).is_err());
}

#[test]
fn unknown_file_id_errors() {
    let disk = DiskManager::new();
    let mut buf = [0u8; PAGE_SIZE];
    assert!(disk.read_page(7, 0, &mut buf).is_err());
    assert!(disk.page_count(7).is_err());
    assert!(disk.with_file(7, |_, _| Ok(())).is_err());
}

/// `close_file` 必须真正关掉句柄再返回路径：调用方紧接着要 `remove_file`，
/// 而 Windows 不允许删除仍有打开句柄的文件。
#[test]
fn closed_file_handle_is_released_before_the_path_is_returned() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gone.dbf");

    let disk = DiskManager::new();
    let file = disk.create_file(&path).unwrap();
    let returned = disk.close_file(file).unwrap();
    assert_eq!(returned, path);

    // 句柄已关：文件能删，且这个 id 不再可用。
    std::fs::remove_file(&path).unwrap();
    assert!(disk.with_file(file, |_, _| Ok(())).is_err());
}

#[test]
fn alloc_page_appends_zeroed_pages() {
    let dir = tempfile::tempdir().unwrap();
    let disk = DiskManager::new();
    let file = disk.create_file(&dir.path().join("alloc.dbf")).unwrap();

    assert_eq!(disk.alloc_page(file).unwrap(), 0);
    assert_eq!(disk.alloc_page(file).unwrap(), 1);
    assert_eq!(disk.alloc_page(file).unwrap(), 2);
    assert_eq!(disk.page_count(file).unwrap(), 3);

    let mut buf = [1u8; PAGE_SIZE];
    disk.read_page(file, 1, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0), "新分配的页应当是零页");
}

/// 同一个 `with_file` 调用内是**排他**的：两个线程各做 K 次
/// "读-改-写 +1"，每次自增都在一个 `with_file` 里完成，最终必须恰好是 2K。
/// 若每个文件的锁没有真的互斥，这里会静默少计数。
#[test]
fn with_file_makes_a_read_modify_write_atomic_on_one_file() {
    const THREADS: u32 = 4;
    const OPS: u32 = 250;

    let dir = tempfile::tempdir().unwrap();
    let disk = Arc::new(DiskManager::new());
    let file = disk.create_file(&dir.path().join("rmw.dbf")).unwrap();
    disk.write_page(file, 0, &[0u8; PAGE_SIZE]).unwrap();

    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let disk = Arc::clone(&disk);
        handles.push(std::thread::spawn(move || {
            for _ in 0..OPS {
                disk.with_file(file, |f, _| {
                    let mut page = [0u8; PAGE_SIZE];
                    f.seek(SeekFrom::Start(0)).unwrap();
                    f.read_exact(&mut page).unwrap();
                    let v = u32::from_le_bytes(page[0..4].try_into().unwrap());
                    page[0..4].copy_from_slice(&(v + 1).to_le_bytes());
                    f.seek(SeekFrom::Start(0)).unwrap();
                    f.write_all(&page).unwrap();
                    Ok(())
                })
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let mut buf = [0u8; PAGE_SIZE];
    disk.read_page(file, 0, &mut buf).unwrap();
    assert_eq!(
        u32::from_le_bytes(buf[0..4].try_into().unwrap()),
        THREADS * OPS,
        "每个文件的锁没有真正排他，丢了自增"
    );
}

/// 不同文件之间**不**互相阻塞。全局一把锁的实现会让这里挂住：
/// 线程 A 停在 f1 的闭包里（持锁），主线程对 f2 的访问必须仍然能完成。
#[test]
fn different_files_do_not_block_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let disk = Arc::new(DiskManager::new());
    let f1 = disk.create_file(&dir.path().join("one.dbf")).unwrap();
    let f2 = disk.create_file(&dir.path().join("two.dbf")).unwrap();

    let (inside_tx, inside_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let holder = {
        let disk = Arc::clone(&disk);
        std::thread::spawn(move || {
            disk.with_file(f1, |_, _| {
                inside_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        })
    };

    inside_rx.recv().unwrap(); // A 已在 f1 的闭包里，f1 的文件锁被持有

    let (done_tx, done_rx) = mpsc::channel();
    let other = {
        let disk = Arc::clone(&disk);
        std::thread::spawn(move || {
            disk.write_page(f2, 0, &[1u8; PAGE_SIZE]).unwrap();
            done_tx.send(()).unwrap();
        })
    };
    assert!(
        done_rx.recv_timeout(PATIENCE).is_ok(),
        "对另一个文件的 I/O 被 f1 的锁挡住了 —— 文件锁没有按文件划分"
    );

    release_tx.send(()).unwrap();
    holder.join().unwrap();
    other.join().unwrap();
}

/// 死锁哨兵。两条路径的取锁顺序在直觉上是相反的：
/// 淘汰是 `state → 文件锁 → 页闩`，而 `flush_all` 是 `页闩 → 文件锁`。
/// 它们能共存靠的是"页闩的持有期 ⊆ pin 的持有期，而淘汰只挑 `pins == 0`"，
/// 所以被持闩的帧永远不会成为淘汰对象。这条推理若被破坏，本用例会挂住。
#[test]
fn flush_all_under_concurrent_writers_terminates() {
    const PAGES: u32 = 8;
    const THREADS: u32 = 4;
    const OPS: usize = 150;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.dbf");
    {
        let seed = DiskManager::new();
        let f = seed.create_file(&path).unwrap();
        for no in 0..PAGES {
            seed.write_page(f, no, &[0u8; PAGE_SIZE]).unwrap();
        }
    }

    // 容量 4 < 页数 8：写者必然触发淘汰，与 flush_all 争用同一批文件锁。
    let pool_disk = DiskManager::new();
    let file = pool_disk.open_file(&path).unwrap();
    let pool = Arc::new(BufferPool::new(pool_disk, THREADS as usize));

    let (done_tx, done_rx) = mpsc::channel();
    let writers: Vec<_> = (0..THREADS)
        .map(|t| {
            let pool = Arc::clone(&pool);
            std::thread::spawn(move || {
                for i in 0..OPS {
                    let page = ((t as usize * 3 + i) % PAGES as usize) as u32;
                    // 小池 + flush 持 pin，缺页可能被拒（buffer pool exhausted）——
                    // 这是有意的语义，重试即可。
                    while pool
                        .with_page(file, page, |p| {
                            p[0] = p[0].wrapping_add(1);
                            Ok(())
                        })
                        .is_err()
                    {
                        std::thread::yield_now();
                    }
                }
            })
        })
        .collect();

    let flusher = {
        let pool = Arc::clone(&pool);
        std::thread::spawn(move || {
            for _ in 0..200 {
                pool.flush_all().unwrap();
            }
            done_tx.send(()).unwrap();
        })
    };

    assert!(
        done_rx.recv_timeout(PATIENCE).is_ok(),
        "flush_all 与并发写者互相等待 —— 锁序被破坏"
    );
    for w in writers {
        w.join().unwrap();
    }
    flusher.join().unwrap();

    // 每一次自增都必须落到盘上：清空缓存后重开，逐页求和。
    drop(pool);
    let check = DiskManager::new();
    let f = check.open_file(&path).unwrap();
    let mut total = 0u32;
    for no in 0..PAGES {
        let mut buf = [0u8; PAGE_SIZE];
        check.read_page(f, no, &mut buf).unwrap();
        total += buf[0] as u32;
    }
    assert_eq!(total, THREADS * OPS as u32, "淘汰 + flush 并发下丢了写");
}

/// 每线程一个文件、各自做一整轮写入再读回：不同文件的页号空间互不干扰。
#[test]
fn per_file_page_spaces_are_independent() {
    const FILES: u32 = 8;
    const PAGES: u32 = 6;

    let dir = tempfile::tempdir().unwrap();
    let disk = Arc::new(DiskManager::new());
    let ids: Vec<u32> = (0..FILES)
        .map(|i| disk.create_file(&dir.path().join(format!("t{i}.dbf"))).unwrap())
        .collect();

    let written = Arc::new(AtomicU32::new(0));
    let handles: Vec<_> = ids
        .iter()
        .map(|&id| {
            let disk = Arc::clone(&disk);
            let written = Arc::clone(&written);
            std::thread::spawn(move || {
                for no in 0..PAGES {
                    let mut page = [0u8; PAGE_SIZE];
                    page[0] = id as u8;
                    page[1] = no as u8;
                    disk.write_page(id, no, &page).unwrap();
                    written.fetch_add(1, Ordering::Relaxed);
                }
                for no in 0..PAGES {
                    let mut buf = [0u8; PAGE_SIZE];
                    disk.read_page(id, no, &mut buf).unwrap();
                    assert_eq!(buf[0], id as u8, "file {id} page {no} 串了数据");
                    assert_eq!(buf[1], no as u8, "file {id} page {no} 串了数据");
                }
                assert_eq!(disk.page_count(id).unwrap(), PAGES);
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(written.load(Ordering::Relaxed), FILES * PAGES);
}
