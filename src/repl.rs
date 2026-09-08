use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::Database;

pub async fn run_repl(
    db: &mut Database,
    mut input: impl AsyncBufRead + Unpin,
    output: &mut (impl AsyncWrite + Unpin),
) -> io::Result<()> {
    let mut line = String::new();
    loop {
        output.write_all(b"db> ").await?;
        output.flush().await?;
        line.clear();
        if input.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let sql = line.trim();
        if sql.is_empty() {
            continue;
        }
        if sql == "exit" || sql == "quit" {
            return Ok(());
        }
        match db.execute_sql(sql) {
            Ok(_) => output.write_all(b"ok\n").await?,
            Err(e) => output.write_all(format!("error: {e}\n").as_bytes()).await?,
        }
    }
}
