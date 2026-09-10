use chibidb::ast::DataType;
use chibidb::catalog::meta::{
    decode_catalog, encode_catalog, CatalogSnapshot, IndexMeta, TableMeta, ViewMeta,
};

fn snapshot() -> CatalogSnapshot {
    CatalogSnapshot {
        next_table_file: 7,
        next_index_file: 3,
        next_trx_id: 9,
        committed_trxs: vec![1, 2, 3],
        tables: vec![
            TableMeta {
                name: "student".into(),
                columns: vec![
                    ("id".into(), DataType::Int),
                    ("name".into(), DataType::Char(10)),
                    ("score".into(), DataType::Float),
                ],
                file_no: 0,
            },
            TableMeta {
                name: "课程".into(),
                columns: vec![("cid".into(), DataType::Int), ("title".into(), DataType::Char(32))],
                file_no: 1,
            },
        ],
        indexes: vec![IndexMeta {
            name: "idx_id".into(),
            table: "student".into(),
            column: "id".into(),
            file_no: 0,
        }],
        views: vec![ViewMeta {
            name: "passed".into(),
            sql: "select id from student where score >= 75.0".into(),
        }],
    }
}

#[test]
fn roundtrips_snapshot() {
    let snap = snapshot();
    let bytes = encode_catalog(&snap);
    let back = decode_catalog(&bytes).unwrap();
    assert_eq!(back.next_table_file, 7);
    assert_eq!(back.next_index_file, 3);
    assert_eq!(back.tables.len(), 2);
    assert_eq!(back.tables[0].name, "student");
    assert_eq!(back.tables[0].columns[1], ("name".into(), DataType::Char(10)));
    assert_eq!(back.tables[1].name, "课程");
    assert_eq!(back.tables[1].file_no, 1);
    assert_eq!(back.indexes.len(), 1);
    assert_eq!(back.indexes[0].name, "idx_id");
    assert_eq!(back.indexes[0].column, "id");
    assert_eq!(back.views.len(), 1);
    assert_eq!(back.views[0].name, "passed");
    assert_eq!(back.views[0].sql, "select id from student where score >= 75.0");
}

#[test]
fn empty_catalog_roundtrips() {
    let snap = CatalogSnapshot {
        next_table_file: 0,
        next_index_file: 0,
        next_trx_id: 0,
        committed_trxs: vec![],
        tables: vec![],
        indexes: vec![],
        views: vec![],
    };
    let bytes = encode_catalog(&snap);
    let back = decode_catalog(&bytes).unwrap();
    assert_eq!(back.next_table_file, 0);
    assert_eq!(back.tables.len(), 0);
    assert_eq!(back.indexes.len(), 0);
    assert_eq!(back.views.len(), 0);
}

#[test]
fn rejects_garbage() {
    assert!(decode_catalog(&[]).is_err());
    assert!(decode_catalog(b"XXXXXXXX").is_err(), "bad magic");
    assert!(decode_catalog(b"CHIDCATX".as_ref()).is_err(), "truncated header");
    let mut bytes = encode_catalog(&snapshot());
    bytes.truncate(bytes.len() - 3);
    assert!(decode_catalog(&bytes).is_err(), "truncated body");
}
