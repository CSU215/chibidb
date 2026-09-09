use std::io;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::wire;
use crate::Database;

pub type SharedDb = Arc<Mutex<Database>>;

/// Accepts connections until the listener is closed.
///
/// Statement execution happens under the database lock; concurrent sessions
/// are serialized on writes and reads alike (single-writer model).
pub async fn serve(db: SharedDb, listener: TcpListener) -> io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let db = db.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(db, stream).await {
                eprintln!("connection {peer} error: {e}");
            }
        });
    }
}

/// Request protocol: [u32 len][sql utf8]; response: a sequence of wire frames.
async fn read_sql(rd: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<String>> {
    let mut len_buf = [0u8; 4];
    match rd.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    rd.read_exact(&mut body).await?;
    String::from_utf8(body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

async fn handle_conn(db: SharedDb, stream: TcpStream) -> io::Result<()> {
    let (mut rd, mut wr) = stream.into_split();
    while let Some(sql) = read_sql(&mut rd).await? {
        let sql = sql.trim();
        if sql.is_empty() {
            continue;
        }
        if sql == "exit" || sql == "quit" {
            break;
        }
        match db.lock().await.execute_sql(sql) {
            Ok(results) => {
                for rs in &results {
                    wr.write_all(&wire::encode_result_frame(rs)).await?;
                }
            }
            Err(e) => {
                wr.write_all(&wire::encode_error_frame(&e.to_string())).await?;
            }
        }
        wr.write_all(&wire::encode_done_frame()).await?;
    }
    Ok(())
}
