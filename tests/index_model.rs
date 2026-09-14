//! Randomized differential test for the B+ tree: a `BTreeMap` model is kept in
//! lockstep with the real index and search/scan results are compared. This
//! exercises splits, borrows, merges and duplicate runs that span leaves, with
//! variable-length keys.

use std::collections::{BTreeMap, BTreeSet};

use chibidb::index::{BTree, Bound};
use chibidb::storage::{BufferPool, DiskManager, Rid};

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        // SplitMix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

fn setup(dir: &tempfile::TempDir) -> (BufferPool, u32) {
    let disk = DiskManager::new();
    let file = disk.create_file(&dir.path().join("model.idxf")).unwrap();
    (BufferPool::new(disk, 64), file)
}

/// A pool of distinct keys, mostly short with the occasional longer one.
fn make_pool(rng: &mut Rng, n: usize) -> Vec<Vec<u8>> {
    let mut pool = BTreeSet::new();
    while pool.len() < n {
        let len = if rng.below(10) == 0 {
            20 + rng.below(180) as usize
        } else {
            1 + rng.below(6) as usize
        };
        let key: Vec<u8> = (0..len).map(|_| b'a' + rng.below(26) as u8).collect();
        pool.insert(key);
    }
    pool.into_iter().collect()
}

fn model_range(
    model: &BTreeMap<Vec<u8>, BTreeSet<Rid>>,
    start: &Bound<'_>,
    end: &Bound<'_>,
) -> Vec<(Vec<u8>, Rid)> {
    let mut want = Vec::new();
    for (k, set) in model {
        let after = match start {
            Bound::Unbounded => true,
            Bound::Included(s) => k.as_slice() >= *s,
            Bound::Excluded(s) => k.as_slice() > *s,
        };
        let before = match end {
            Bound::Unbounded => true,
            Bound::Included(e) => k.as_slice() <= *e,
            Bound::Excluded(e) => k.as_slice() < *e,
        };
        if after && before {
            for r in set {
                want.push((k.clone(), *r));
            }
        }
    }
    want
}

#[test]
fn randomized_model_matches() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir);
    let tree = BTree::init(&bp, f).unwrap();
    let mut rng = Rng(0xC0FFEE);
    let pool = make_pool(&mut rng, 24);
    let mut model: BTreeMap<Vec<u8>, BTreeSet<Rid>> = BTreeMap::new();

    for step in 0..10_000u32 {
        let key = pool[rng.below(pool.len() as u64) as usize].clone();
        let rid = Rid::new(rng.below(4) as u32, rng.below(8) as u16);
        match rng.below(100) {
            0..=44 => {
                // a rid is unique per row, so never insert the same pair twice
                if !model.get(&key).is_some_and(|s| s.contains(&rid)) {
                    tree.insert(&bp, &key, rid).unwrap();
                    model.entry(key).or_default().insert(rid);
                }
            }
            45..=64 => {
                let mut before = tree.search(&bp, &key).unwrap();
                before.sort();
                let want: Vec<Rid> =
                    model.get(&key).cloned().unwrap_or_default().into_iter().collect();
                assert_eq!(before, want, "pre-delete search mismatch at step {step}");
                let present = model.get(&key).is_some_and(|s| s.contains(&rid));
                if present {
                    tree.delete(&bp, &key, rid).unwrap();
                    let set = model.get_mut(&key).unwrap();
                    set.remove(&rid);
                    if set.is_empty() {
                        model.remove(&key);
                    }
                } else {
                    // absent: delete may be a no-op (empty tree) or an error,
                    // but it must never remove a different entry
                    let _ = tree.delete(&bp, &key, rid);
                    let mut after = tree.search(&bp, &key).unwrap();
                    after.sort();
                    assert_eq!(after, want, "delete of absent pair changed the tree at step {step}");
                }
            }
            65..=74 => {
                let mut got = tree.search(&bp, &key).unwrap();
                got.sort();
                let want: Vec<Rid> =
                    model.get(&key).cloned().unwrap_or_default().into_iter().collect();
                assert_eq!(got, want, "search mismatch at step {step}");
            }
            _ => {
                let a = pool[rng.below(pool.len() as u64) as usize].clone();
                let b = pool[rng.below(pool.len() as u64) as usize].clone();
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                let start = match rng.below(3) {
                    0 => Bound::Unbounded,
                    1 => Bound::Included(&lo),
                    _ => Bound::Excluded(&lo),
                };
                let end = match rng.below(3) {
                    0 => Bound::Unbounded,
                    1 => Bound::Included(&hi),
                    _ => Bound::Excluded(&hi),
                };
                let mut got = tree.scan_range(&bp, start, end).unwrap();
                got.sort();
                let mut want = model_range(&model, &start, &end);
                want.sort();
                assert_eq!(got, want, "range mismatch at step {step}");
            }
        }

        if step % 500 == 0 {
            let mut got = tree.scan_range(&bp, Bound::Unbounded, Bound::Unbounded).unwrap();
            got.sort();
            let mut want = model_range(&model, &Bound::Unbounded, &Bound::Unbounded);
            want.sort();
            assert_eq!(got, want, "full scan mismatch at step {step}");
            tree.check_invariants(&bp).unwrap();
        }
    }
}

#[test]
fn b_link_invariants_survive_random_splits_and_merges() {
    // Same model in lockstep, but the B-link invariants are checked after every
    // operation so a wrong high key or right link is caught immediately.
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir);
    let tree = BTree::init(&bp, f).unwrap();
    let mut rng = Rng(0xBEEF);
    let pool = make_pool(&mut rng, 20);
    let mut model: BTreeMap<Vec<u8>, BTreeSet<Rid>> = BTreeMap::new();

    for step in 0..4000u32 {
        let key = pool[rng.below(pool.len() as u64) as usize].clone();
        let rid = Rid::new(rng.below(3) as u32, rng.below(6) as u16);
        match rng.below(100) {
            0..=54 => {
                if !model.get(&key).is_some_and(|s| s.contains(&rid)) {
                    tree.insert(&bp, &key, rid).unwrap();
                    model.entry(key).or_default().insert(rid);
                }
            }
            _ => {
                if model.get(&key).is_some_and(|s| s.contains(&rid)) {
                    tree.delete(&bp, &key, rid).unwrap();
                    let set = model.get_mut(&key).unwrap();
                    set.remove(&rid);
                    if set.is_empty() {
                        model.remove(&key);
                    }
                }
            }
        }
        tree.check_invariants(&bp).unwrap_or_else(|e| {
            panic!("invariant broken at step {step}: {e}");
        });
    }
}

#[test]
fn concurrent_inserts_stay_consistent() {
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir);
    BTree::init(&bp, f).unwrap();
    let bp = Arc::new(bp);

    const THREADS: usize = 4;
    const PER: usize = 2000;
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let bp = Arc::clone(&bp);
            std::thread::spawn(move || {
                let tree = BTree::at(f);
                for i in 0..PER {
                    let key = format!("k{:08}", t * PER + i).into_bytes();
                    tree.insert(&bp, &key, Rid::new(t as u32, i as u16)).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let tree = BTree::at(f);
    tree.check_invariants(&bp).unwrap();
    for t in 0..THREADS {
        for i in 0..PER {
            let key = format!("k{:08}", t * PER + i).into_bytes();
            assert_eq!(
                tree.search(&bp, &key).unwrap(),
                vec![Rid::new(t as u32, i as u16)],
                "missing {key:?}"
            );
        }
    }
    let all = tree.scan_range(&bp, Bound::Unbounded, Bound::Unbounded).unwrap();
    assert_eq!(all.len(), THREADS * PER);
}

#[test]
fn oversized_key_is_rejected_without_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir);
    let tree = BTree::init(&bp, f).unwrap();

    // far larger than a page: rejected cleanly, no panic
    let huge = vec![b'z'; 9000];
    assert!(tree.insert(&bp, &huge, Rid::new(1, 0)).is_err());
    assert!(tree.search(&bp, &huge).unwrap().is_empty());

    // also rejected once the leaf already has entries, and the rest survives
    for i in 0..8u16 {
        tree.insert(&bp, format!("k{i}").as_bytes(), Rid::new(1, i)).unwrap();
    }
    assert!(tree.insert(&bp, &huge, Rid::new(1, 9)).is_err());
    for i in 0..8u16 {
        assert_eq!(tree.search(&bp, format!("k{i}").as_bytes()).unwrap(), [Rid::new(1, i)]);
    }
}
