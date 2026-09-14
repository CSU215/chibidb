use chaoticdb::storage::lsm::PersistentLsm;

fn count_sst(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".sst"))
        .count()
}

#[test]
fn flushed_data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut lsm = PersistentLsm::open(dir.path(), 256).unwrap();
        lsm.put(b"a".to_vec(), b"1".to_vec());
        lsm.put(b"b".to_vec(), b"2".to_vec());
        lsm.flush().unwrap();
    }
    let lsm = PersistentLsm::open(dir.path(), 256).unwrap();
    assert_eq!(lsm.num_sstables(), 1);
    assert_eq!(lsm.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(lsm.get(b"b").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn deletes_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut lsm = PersistentLsm::open(dir.path(), 256).unwrap();
        lsm.put(b"k".to_vec(), b"v".to_vec());
        lsm.flush().unwrap();
        lsm.delete(b"k".to_vec());
        lsm.flush().unwrap();
    }
    let lsm = PersistentLsm::open(dir.path(), 256).unwrap();
    assert_eq!(lsm.num_sstables(), 2);
    assert_eq!(lsm.get(b"k").unwrap(), None);
}

#[test]
fn newest_value_wins_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut lsm = PersistentLsm::open(dir.path(), 256).unwrap();
        lsm.put(b"k".to_vec(), b"old".to_vec());
        lsm.flush().unwrap();
        lsm.put(b"k".to_vec(), b"new".to_vec());
        lsm.flush().unwrap();
    }
    let lsm = PersistentLsm::open(dir.path(), 256).unwrap();
    assert_eq!(lsm.get(b"k").unwrap(), Some(b"new".to_vec()));
}

#[test]
fn compaction_merges_files_and_removes_old_ones() {
    let dir = tempfile::tempdir().unwrap();
    {
        // a high trigger keeps auto-compaction out of the way of this test
        let mut lsm = PersistentLsm::open_with_trigger(dir.path(), 128, 100).unwrap();
        for round in 0..4u32 {
            for i in 0..20 {
                lsm.put(
                    format!("key{i:03}").into_bytes(),
                    format!("r{round}-{i}").into_bytes(),
                );
            }
            lsm.flush().unwrap();
        }
        assert_eq!(count_sst(dir.path()), 4);
        lsm.compact().unwrap();
        assert_eq!(lsm.num_sstables(), 1);
        assert_eq!(count_sst(dir.path()), 1);
    }
    let lsm = PersistentLsm::open(dir.path(), 128).unwrap();
    assert_eq!(lsm.num_sstables(), 1);
    for i in 0..20 {
        assert_eq!(
            lsm.get(format!("key{i:03}").as_bytes()).unwrap(),
            Some(format!("r3-{i}").into_bytes())
        );
    }
}

#[test]
fn leveled_compaction_preserves_data_and_tombstones() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut lsm = PersistentLsm::open_with_trigger(dir.path(), 128, 3).unwrap();
        for round in 0..12u32 {
            for i in 0..10u32 {
                if round >= 5 && i == 0 {
                    continue; // keep k000 deleted
                }
                lsm.put(format!("k{i:03}").into_bytes(), format!("r{round}-{i}").into_bytes());
            }
            if round == 5 {
                lsm.delete(b"k000".to_vec());
            }
            lsm.flush().unwrap();
        }
        // 12 flushes but the leveled layout keeps the table count logarithmic
        assert!(lsm.num_sstables() <= 6, "live tables {}", lsm.num_sstables());
        assert_eq!(lsm.get(b"k005").unwrap(), Some(b"r11-5".to_vec()));
        assert_eq!(lsm.get(b"k000").unwrap(), None);
    }

    let lsm = PersistentLsm::open_with_trigger(dir.path(), 128, 3).unwrap();
    assert_eq!(lsm.get(b"k005").unwrap(), Some(b"r11-5".to_vec()));
    assert_eq!(lsm.get(b"k000").unwrap(), None);
    assert_eq!(lsm.iter().unwrap().len(), 9);
}

#[test]
fn orphan_sstable_file_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut lsm = PersistentLsm::open(dir.path(), 256).unwrap();
        lsm.put(b"k".to_vec(), b"v".to_vec());
        lsm.flush().unwrap();
    }
    // a file the manifest never named (e.g. a crashed uncommitted flush)
    std::fs::write(dir.path().join("sst-000099.sst"), b"not a real table").unwrap();

    let lsm = PersistentLsm::open(dir.path(), 256).unwrap();
    assert_eq!(lsm.num_sstables(), 1);
    assert_eq!(lsm.get(b"k").unwrap(), Some(b"v".to_vec()));
}

#[test]
fn unflushed_memtable_is_not_recovered() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut lsm = PersistentLsm::open(dir.path(), 256).unwrap();
        lsm.put(b"k".to_vec(), b"v".to_vec());
        lsm.flush().unwrap();
        lsm.put(b"pending".to_vec(), b"x".to_vec());
        // no flush, no WAL yet: the pending write is expected to be lost
    }
    let lsm = PersistentLsm::open(dir.path(), 256).unwrap();
    assert_eq!(lsm.get(b"k").unwrap(), Some(b"v".to_vec()));
    assert_eq!(lsm.get(b"pending").unwrap(), None);
}
