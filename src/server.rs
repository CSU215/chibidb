use std::io;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::instance::Instance;
use crate::protocol::{Protocol, TextProtocol};
use crate::trx::Session;

pub type SharedInstance = Arc<Mutex<Instance>>;

/// Accepts connections until the listener is closed.
///
/// Statement execution happens under the instance lock; concurrent sessions
/// are serialized on writes and reads alike (single-writer model).
pub async fn serve(instance: SharedInstance, listener: TcpListener) -> io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let instance = instance.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(instance, stream).await {
                eprintln!("connection {peer} error: {e}");
            }
        });
    }
}

/// Request protocol: `[u32 len][sql utf8]`; response: a sequence of wire frames.
async fn handle_conn(instance: SharedInstance, stream: TcpStream) -> io::Result<()> {
    // one session per connection so transactions span statements
    let mut session = Session::new();
    let result = serve_session(&instance, stream, &mut session).await;
    // roll back any open transaction when the session goes away
    if let Err(e) = instance.lock().await.rollback_session(&mut session) {
        eprintln!("connection cleanup error: {e}");
    }
    result
}

async fn serve_session(
    instance: &SharedInstance,
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
                match instance.lock().await.execute_with(session, sql) {
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
