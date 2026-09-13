use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::render::write_result;
use crate::wire::{self, Frame};

/// Interactive client: reads SQL lines from `input`, renders server results.
/// `interactive` controls whether the prompt is written (see `run_repl`).
pub async fn run_client(
    stream: &mut TcpStream,
    mut input: impl AsyncBufRead + Unpin,
    output: &mut (impl AsyncWrite + Unpin),
    interactive: bool,
) -> io::Result<()> {
    let mut line = String::new();
    loop {
        if interactive {
            output.write_all(b"chibidb> ").await?;
            output.flush().await?;
        }
        line.clear();
        if input.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let sql = line.trim();
        if sql.is_empty() {
            continue;
        }
        if sql == "exit" || sql == "quit" {
            send(stream, "exit").await?;
            return Ok(());
        }
        send(stream, sql).await?;
        loop {
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await?;
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut body = vec![0u8; len];
            stream.read_exact(&mut body).await?;
            match wire::decode_frame(&body)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
            {
                Frame::Message(m) => output.write_all(format!("{m}\n").as_bytes()).await?,
                Frame::Error(e) => output.write_all(format!("error: {e}\n").as_bytes()).await?,
                Frame::Rows(rs) => write_result(output, &rs).await?,
                Frame::Done => break,
            }
        }
    }
}

async fn send(stream: &mut TcpStream, sql: &str) -> io::Result<()> {
    let bytes = sql.as_bytes();
    stream.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    stream.write_all(bytes).await?;
    stream.flush().await
}
