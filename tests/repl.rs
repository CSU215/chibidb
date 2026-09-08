use chibidb::{Database, run_repl};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, duplex};

async fn run_with(input: &[u8]) -> String {
    let (mut cmd_tx, repl_input) = duplex(64);
    let repl_input = BufReader::new(repl_input);
    let (mut repl_output, mut out_rx) = duplex(64);
    let mut db = Database::open_in_memory();

    cmd_tx.write_all(input).await.unwrap();
    cmd_tx.shutdown().await.unwrap();

    run_repl(&mut db, repl_input, &mut repl_output)
        .await
        .unwrap();
    drop(repl_output);

    let mut buf = Vec::new();
    out_rx.read_to_end(&mut buf).await.unwrap();
    String::from_utf8(buf).unwrap()
}

#[tokio::test]
async fn prompts_reports_unsupported_and_exits() {
    let output = run_with(b"select 1;\nexit\n").await;

    assert!(output.starts_with("db> "), "got: {output:?}");
    assert!(output.contains("error: unsupported"), "got: {output:?}");
    assert_eq!(output.matches("db> ").count(), 2, "got: {output:?}");
}

#[tokio::test]
async fn blank_lines_are_ignored() {
    let output = run_with(b"\n   \nexit\n").await;

    assert!(!output.contains("error"), "got: {output:?}");
    assert_eq!(output.matches("db> ").count(), 3, "got: {output:?}");
}
