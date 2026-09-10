use std::io;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::protocol::{Protocol, TextProtocol};
use crate::trx::Session;
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

/// Request protocol: `[u32 len][sql utf8]`; response: a sequence of wire frames.
async fn handle_conn(db: SharedDb, stream: TcpStream) -> io::Result<()> {
    // one session per connection so transactions span statements
    let mut session = Session::new();
    let result = serve_session(&db, stream, &mut session).await;
    // roll back any open transaction when the session goes away
    if let Err(e) = db.lock().await.rollback_session(&mut session) {
        eprintln!("connection cleanup error: {e}");
    }
    result
}

async fn serve_session(
    db: &SharedDb,
    stream: TcpStream,
    session: &mut Session,
) -> io::Result<()> {
    let (mut rd, mut wr) = stream.into_split();
    let mut protocol = TextProtocol;
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match protocol.decode_request(&pending) {
            Ok(Some((sql, consumed))) => {
                pending.drain(..consumed);
                let sql = sql.trim();
                if sql.is_empty() {
                    continue;
                }
                if sql == "exit" || sql == "quit" {
                    break;
                }
                let mut out = Vec::new();
                match db.lock().await.execute_sql_with(session, sql) {
                    Ok(results) => protocol.encode_success(&results, &mut out),
                    Err(e) => protocol.encode_failure(&e.to_string(), &mut out),
                }
                wr.write_all(&out).await?;
            }
            Ok(None) => {
                let n = rd.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                pending.extend_from_slice(&chunk[..n]);
            }
            Err(e) => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
            }
        }
    }
    Ok(())
}
