use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;

use chaoticdb::config::{EvictionPolicy, ObservabilityConfig};
use chaoticdb::storage::{BufferPool, CacheReporter, DiskManager, PoolStats, PAGE_SIZE};

const ALL_POLICIES: [EvictionPolicy; 3] =
    [EvictionPolicy::Lru, EvictionPolicy::Clock, EvictionPolicy::Fifo];

fn pool_with(
    dir: &tempfile::TempDir,
    name: &str,
    cap: usize,
    policy: EvictionPolicy,
) -> (BufferPool, u32) {
    let disk = DiskManager::new();
    let file = disk.create_file(&dir.path().join(name)).unwrap();
    (BufferPool::new_with_eviction(disk, cap, policy), file)
}

/// 默认策略（`lru`）的池，供不关心策略的用例使用。
fn pool(dir: &tempfile::TempDir, name: &str, cap: usize) -> (BufferPool, u32) {
    pool_with(dir, name, cap, EvictionPolicy::Lru)
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
        let disk = DiskManager::new();
        let f = disk.create_file(&path).unwrap();
        let bp = BufferPool::new(disk, 4);
        bp.with_page(f, 2, |p| {
            p[5] = 42;
            Ok(())
        })
        .unwrap();
    }

    let disk = DiskManager::new();
    let f = disk.open_file(&path).unwrap();
    let bp = BufferPool::new(disk, 4);
    let v = bp.with_page(f, 2, |p| Ok(p[5])).unwrap();
    assert_eq!(v, 42);
}

/// LRU 语义：容量 2 下写页 1、写页 2、读页 1（命中重排），再写页 3 时淘汰的是页 2。
///
/// 注意"淘汰了哪一帧"**不能**由读回值观察：脏页换出前会写回，读回来的值一样。
/// 唯一可观察的是后续读是否缺页，所以断言落在 `PoolStats` 增量的方向上。
/// 该断言对策略敏感：换成 `fifo`/`clock`，这里被淘汰的会变成页 1，用例即失败。
#[test]
fn evicts_least_recently_used() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = pool_with(&dir, "d.dbf", 2, EvictionPolicy::Lru);

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
    // 命中页 1：页 2 从此成为 LRU。
    assert_eq!(bp.read_page(f, 1, |p| Ok(p[0])).unwrap(), 10);
    bp.with_page(f, 3, |p| {
        p[0] = 30;
        Ok(())
    })
    .unwrap();

    // 页 1 是新近使用的，应当还在池里 → 命中。
    let before = bp.stats();
    assert_eq!(bp.read_page(f, 1, |p| Ok(p[0])).unwrap(), 10);
    let after1 = bp.stats();
    assert_eq!(after1.hits, before.hits + 1, "LRU 不该淘汰页 1");
    assert_eq!(after1.misses, before.misses);

    // 页 2 已被换出 → 必然缺页；值仍然对，因为换出时写回了。
    assert_eq!(bp.read_page(f, 2, |p| Ok(p[0])).unwrap(), 20);
    let after2 = bp.stats();
    assert_eq!(after2.misses, after1.misses + 1, "LRU 应当已经淘汰了页 2");
}

/// FIFO 的镜像用例：同一访问序列下命中**不**重排，所以先入池的页 1 先出局。
#[test]
fn fifo_evicts_the_oldest_insertion() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = pool_with(&dir, "fifo.dbf", 2, EvictionPolicy::Fifo);

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
    assert_eq!(bp.read_page(f, 1, |p| Ok(p[0])).unwrap(), 10, "命中不改变入池顺序");
    bp.with_page(f, 3, |p| {
        p[0] = 30;
        Ok(())
    })
    .unwrap();

    // 页 1 最先进池 → 先出局；页 2 还在。
    let before = bp.stats();
    assert_eq!(bp.read_page(f, 2, |p| Ok(p[0])).unwrap(), 20);
    let after2 = bp.stats();
    assert_eq!(after2.hits, before.hits + 1, "FIFO 不该淘汰页 2");
    assert_eq!(after2.misses, before.misses);

    assert_eq!(bp.read_page(f, 1, |p| Ok(p[0])).unwrap(), 10);
    let after1 = bp.stats();
    assert_eq!(after1.misses, after2.misses + 1, "FIFO 应当已经淘汰了页 1");
}

#[test]
fn alloc_pages_are_appended_and_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.dbf");

    {
        let disk = DiskManager::new();
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

    let disk = DiskManager::new();
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
    let disk = DiskManager::new();
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

/// 用小池 + 多线程 + 频繁淘汰敲同一个文件，返回 `(每页的成功自增次数, 重开后盘上的值)`。
/// 两个向量是否相等只取决于实现有没有丢写，与线程交错无关。
fn hammer(path: &std::path::Path, policy: EvictionPolicy) -> (Vec<u32>, Vec<u32>) {
    const PAGES: u32 = 8;
    const THREADS: u32 = 4;
    const OPS: usize = 200;

    let counts: Arc<Vec<AtomicU32>> =
        Arc::new((0..PAGES).map(|_| AtomicU32::new(0)).collect());

    {
        let disk = DiskManager::new();
        let f = disk.create_file(path).unwrap();
        // 容量 4、4 个线程：每个线程最多持 1 个 pin，故总有可淘汰的帧，
        // 不会真的耗尽；保留重试分支以防实现细节变化。
        let bp = Arc::new(BufferPool::new_with_eviction(disk, 4, policy));
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

    let disk = DiskManager::new();
    let f = disk.open_file(path).unwrap();
    let bp = BufferPool::new(disk, PAGES as usize);
    let on_disk = (0..PAGES)
        .map(|page| {
            bp.read_page(f, page, |p| Ok(u32::from_le_bytes(p[0..4].try_into().unwrap())))
                .unwrap()
        })
        .collect();
    let counted = counts.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    (counted, on_disk)
}

/// 小池 + 多线程 + 频繁淘汰：每次成功自增都必须落到盘上，一次都不能丢。
#[test]
fn concurrent_writers_do_not_lose_updates_under_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let (counted, on_disk) = hammer(&dir.path().join("concurrent.dbf"), EvictionPolicy::Lru);
    for page in 0..counted.len() {
        assert_eq!(on_disk[page], counted[page], "page {page} 丢了写");
    }
}

/// 淘汰策略只决定"换出哪一帧"，不影响任何可观测结果 —— 同一套并发写用例
/// 在三种策略下都必须一字不差地通过。
#[test]
fn every_eviction_policy_keeps_writes_correct() {
    let dir = tempfile::tempdir().unwrap();
    for policy in ALL_POLICIES {
        let path = dir.path().join(format!("hammer-{policy:?}.dbf"));
        let (counted, on_disk) = hammer(&path, policy);
        assert_eq!(on_disk, counted, "{policy:?} 下丢了写");
        assert!(counted.iter().all(|&c| c > 0), "{policy:?} 下并发负载没跑起来");
    }
}

/// 注入的 reporter 必须看到每一次淘汰，且区分干净/脏换出；`report_stats`
/// 在同一开关下交付快照。容量 1 让每一次缺页都恰好淘汰一个帧。
#[derive(Default)]
struct RecordingReporter {
    evictions: std::sync::Mutex<Vec<((u32, u32), bool)>>,
    stats: std::sync::Mutex<Vec<PoolStats>>,
}

impl CacheReporter for RecordingReporter {
    fn evict(&self, key: (u32, u32), dirty: bool, _stats: &PoolStats) {
        self.evictions.lock().unwrap().push((key, dirty));
    }

    fn stats(&self, stats: &PoolStats) {
        self.stats.lock().unwrap().push(*stats);
    }
}

#[test]
fn reporter_sees_eviction_events_with_dirtiness() {
    let dir = tempfile::tempdir().unwrap();
    let disk = DiskManager::new();
    let f = disk.create_file(&dir.path().join("report.dbf")).unwrap();
    let obs = ObservabilityConfig { cache_stats: true, eviction_log: true };
    let reporter = Arc::new(RecordingReporter::default());
    let bp =
        BufferPool::new_with_reporter(disk, 1, EvictionPolicy::Lru, obs, reporter.clone());

    // 干净换出：读入 page 0，再读 page 1 时 page 0（干净）被淘汰。
    bp.read_page(f, 0, |_| Ok(())).unwrap();
    bp.read_page(f, 1, |_| Ok(())).unwrap();
    // 脏换出：写 page 1 置脏，再读 page 2 时它被写回后淘汰。
    bp.with_page(f, 1, |p| {
        p[0] = 1;
        Ok(())
    })
    .unwrap();
    bp.read_page(f, 2, |_| Ok(())).unwrap();

    let stats = bp.stats();
    assert_eq!(stats.evictions, 2);
    assert_eq!(stats.dirty_evictions, 1);
    assert_eq!(stats.clean_evictions(), 1);
    assert_eq!(stats.resident, 1);
    assert_eq!(stats.capacity, 1);

    let events = reporter.evictions.lock().unwrap().clone();
    assert_eq!(events, vec![((f, 0), false), ((f, 1), true)]);

    // cache_stats 开启：report_stats 把快照交给 reporter。
    bp.report_stats();
    assert_eq!(reporter.stats.lock().unwrap().len(), 1);
}

/// 两个开关都关闭时，reporter 一次都不该被调用（默认行为与旧版一致）。
#[test]
fn reporter_is_silent_when_switches_are_off() {
    let dir = tempfile::tempdir().unwrap();
    let disk = DiskManager::new();
    let f = disk.create_file(&dir.path().join("quiet.dbf")).unwrap();
    let reporter = Arc::new(RecordingReporter::default());
    let bp = BufferPool::new_with_reporter(
        disk,
        1,
        EvictionPolicy::Lru,
        ObservabilityConfig::default(),
        reporter.clone(),
    );

    bp.read_page(f, 0, |_| Ok(())).unwrap();
    bp.read_page(f, 1, |_| Ok(())).unwrap();
    bp.report_stats();

    assert!(reporter.evictions.lock().unwrap().is_empty());
    assert!(reporter.stats.lock().unwrap().is_empty());
}

#[test]
fn pool_stats_derives_hit_rate_and_clean_evictions() {
    let s = PoolStats {
        hits: 3,
        misses: 1,
        evictions: 5,
        dirty_evictions: 2,
        resident: 4,
        capacity: 8,
    };
    assert!((s.hit_rate() - 0.75).abs() < 1e-9);
    assert_eq!(s.clean_evictions(), 3);
    assert_eq!(PoolStats::default().hit_rate(), 0.0);
}
