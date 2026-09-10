use chibidb::config::Config;
use chibidb::instance::Instance;
use chibidb::run_repl;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, duplex};

async fn run_with(input: &[u8]) -> String {
    let (mut cmd_tx, repl_input) = duplex(4096);
    let repl_input = BufReader::new(repl_input);
    let (mut repl_output, mut out_rx) = duplex(4096);
    let mut instance = Instance::open_in_memory(&Config::default()).unwrap();

    cmd_tx.write_all(input).await.unwrap();
    cmd_tx.shutdown().await.unwrap();

    run_repl(&mut instance, repl_input, &mut repl_output)
        .await
        .unwrap();
    drop(repl_output);

    let mut buf = Vec::new();
    out_rx.read_to_end(&mut buf).await.unwrap();
    String::from_utf8(buf).unwrap()
}

#[tokio::test]
async fn prompts_renders_results_and_exits() {
    let output = run_with(b"select 1+2;\nexit\n").await;

    assert!(output.starts_with("db> "), "got: {output:?}");
    assert!(output.contains("(+ 1 2)"), "got: {output:?}");
    assert!(output.contains("-------"), "got: {output:?}");
    assert!(output.contains("3"), "got: {output:?}");
    assert!(!output.contains("error"), "got: {output:?}");
    assert_eq!(output.matches("db> ").count(), 2, "got: {output:?}");
}

#[tokio::test]
async fn reports_errors_without_crashing() {
    let output = run_with(b"select 1/0;\nexit\n").await;

    assert!(output.contains("error: division by zero"), "got: {output:?}");
    assert_eq!(output.matches("db> ").count(), 2, "got: {output:?}");
}

#[tokio::test]
async fn blank_lines_are_ignored() {
    let output = run_with(b"\n   \nexit\n").await;

    assert!(!output.contains("error"), "got: {output:?}");
    assert_eq!(output.matches("db> ").count(), 3, "got: {output:?}");
}

#[tokio::test]
async fn transactions_span_lines() {
    let output = run_with(
        b"create table t (id int);\nbegin;\ninsert into t values (1);\nrollback;\nselect count(*) from t;\nexit\n",
    )
    .await;

    assert!(
        !output.contains("no active transaction"),
        "transactions must span lines, got: {output:?}"
    );
    assert!(!output.contains("error"), "got: {output:?}");
    // the rolled-back insert left nothing behind
    assert!(output.contains("\n0\n"), "got: {output:?}");
}
