use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::instance::Instance;
use crate::render::write_result;
use crate::trx::Session;

pub async fn run_repl(
    instance: &mut Instance,
    mut input: impl AsyncBufRead + Unpin,
    output: &mut (impl AsyncWrite + Unpin),
) -> io::Result<()> {
    // one session for the whole REPL so transactions span lines
    let mut session = Session::new();
    let mut line = String::new();
    let result = loop {
        output.write_all(b"db> ").await?;
        output.flush().await?;
        line.clear();
        if input.read_line(&mut line).await? == 0 {
            break Ok(());
        }
        let sql = line.trim();
        if sql.is_empty() {
            continue;
        }
        if sql == "exit" || sql == "quit" {
            break Ok(());
        }
        match instance.execute_with(&mut session, sql) {
            Ok(results) => {
                for rs in &results {
                    write_result(output, rs).await?;
                }
            }
            Err(e) => output.write_all(format!("error: {e}\n").as_bytes()).await?,
        }
    };
    // roll back any open transaction when the REPL goes away
    if let Err(e) = instance.rollback_session(&mut session) {
        eprintln!("error: rollback failed: {e}");
    }
    result
}
