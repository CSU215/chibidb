use chibidb::storage::lob::LOB_CHUNK;
use chibidb::storage::LobStore;

fn data(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

#[test]
fn write_and_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = LobStore::open(dir.path()).unwrap();

    let empty = store.write(b"").unwrap();
    assert_eq!(store.read(empty).unwrap(), b"");
    assert!(store.is_empty(empty).unwrap());

    let small = store.write(b"hello lob").unwrap();
    assert_eq!(store.read(small).unwrap(), b"hello lob");
    assert_eq!(store.len(small).unwrap(), 9);
    assert!(!store.is_empty(small).unwrap());
}

#[test]
fn large_object_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let store = LobStore::open(dir.path()).unwrap();
    let payload = data(1_000_000);
    let id = store.write(&payload).unwrap();
    assert_eq!(store.read(id).unwrap(), payload);
    assert_eq!(store.len(id).unwrap(), 1_000_000);
}

#[test]
fn reader_streams_in_small_buffers() {
    let dir = tempfile::tempdir().unwrap();
    let store = LobStore::open(dir.path()).unwrap();
    let payload = data(10_000);
    let id = store.write(&payload).unwrap();

    let mut reader = store.reader(id).unwrap();
    assert_eq!(reader.remaining(), 10_000);
    let mut out = Vec::new();
    let mut buf = [0u8; 7];
    loop {
        let n = reader.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    assert_eq!(reader.remaining(), 0);
    assert_eq!(out, payload);
}

#[test]
fn next_chunk_walks_a_multi_chunk_object() {
    let dir = tempfile::tempdir().unwrap();
    let store = LobStore::open(dir.path()).unwrap();
    let payload = data(LOB_CHUNK * 2 + 123);
    let id = store.write(&payload).unwrap();

    let mut reader = store.reader(id).unwrap();
    let mut chunks = 0;
    let mut out = Vec::new();
    while let Some(chunk) = reader.next_chunk().unwrap() {
        chunks += 1;
        out.extend_from_slice(&chunk);
    }
    assert_eq!(chunks, 3);
    assert_eq!(out, payload);
}

#[test]
fn delete_removes_and_missing_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = LobStore::open(dir.path()).unwrap();
    let id = store.write(b"gone").unwrap();
    store.delete(id).unwrap();
    assert!(store.read(id).is_err());
    // deleting again is fine
    store.delete(id).unwrap();
}

#[test]
fn ids_are_not_reused_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let first;
    {
        let store = LobStore::open(dir.path()).unwrap();
        first = store.write(b"one").unwrap();
        store.write(b"two").unwrap();
    }
    let store = LobStore::open(dir.path()).unwrap();
    let next = store.write(b"three").unwrap();
    assert!(next > first);
    assert_eq!(store.read(next).unwrap(), b"three");
}
