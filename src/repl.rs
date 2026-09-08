use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::result::ResultSet;
use crate::value::Value;
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
            Ok(results) => {
                for rs in &results {
                    print_result(output, rs).await?;
                }
            }
            Err(e) => output.write_all(format!("error: {e}\n").as_bytes()).await?,
        }
    }
}

async fn print_result(
    output: &mut (impl AsyncWrite + Unpin),
    rs: &ResultSet,
) -> io::Result<()> {
    match rs {
        ResultSet::Message(m) => output.write_all(format!("{m}\n").as_bytes()).await,
        ResultSet::Rows { columns, rows } => {
            let mut grid: Vec<Vec<String>> = vec![columns.clone()];
            for row in rows {
                grid.push(row.iter().map(Value::to_string).collect());
            }
            let n = columns.len();
            let mut widths = vec![0; n];
            for line in &grid {
                for (i, cell) in line.iter().enumerate() {
                    widths[i] = widths[i].max(cell.chars().count());
                }
            }
            let mut text = String::new();
            text.push_str(&grid_line(&grid[0], &widths));
            let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
            text.push_str(&sep.join("-+-"));
            text.push('\n');
            for row in &grid[1..] {
                text.push_str(&grid_line(row, &widths));
            }
            output.write_all(text.as_bytes()).await
        }
    }
}

fn grid_line(cells: &[String], widths: &[usize]) -> String {
    let mut line = cells
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{c:<width$}", width = widths[i]))
        .collect::<Vec<_>>()
        .join(" | ");
    line.push('\n');
    line.trim_end().to_string() + "\n"
}
