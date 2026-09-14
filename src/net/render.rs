use std::io;

use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::result::ResultSet;
use crate::value::Value;

/// Renders a ResultSet as an aligned text table (used by both REPL and client).
pub async fn write_result<W>(out: &mut W, rs: &ResultSet) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    match rs {
        ResultSet::Message(m) => out.write_all(format!("{m}\n").as_bytes()).await,
        ResultSet::Affected(n) => {
            out.write_all(format!("{}\n", crate::wire::affected_message(*n)).as_bytes()).await
        }
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
            out.write_all(text.as_bytes()).await
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
