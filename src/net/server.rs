use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream as StdTcpStream};
use std::sync::{mpsc, Arc};

use tokio::net::TcpListener;

use crate::config::{Config, ThreadModel};
use crate::instance::Instance;
use crate::net::protocol::{Protocol, TextProtocol};
use crate::txn::trx::Session;

pub type SharedInstance = Arc<Instance>;

/// Dispatches accepted connections to blocking execution threads.
pub trait ThreadHandler: Send + Sync {
    fn dispatch(&self, instance: SharedInstance, stream: StdTcpStream, peer: SocketAddr);
}

/// Accepts connections until the listener is closed, handing each to the
/// configured thread model. Statement execution is synchronous and runs on the
/// handler's threads; only accept and socket transfer use the async runtime.
pub async fn serve(instance: SharedInstance, listener: TcpListener) -> io::Result<()> {
    let handler = build_handler(instance.config());
    loop {
        let (stream, peer) = listener.accept().await?;
        let stream = stream.into_std()?;
        stream.set_nonblocking(false)?;
        handler.dispatch(instance.clone(), stream, peer);
    }
}

fn build_handler(config: &Config) -> Box<dyn ThreadHandler> {
    match config.server.thread_model {
        ThreadModel::PerConnection => Box::new(PerConnectionHandler),
        ThreadModel::ThreadPool => Box::new(ThreadPoolHandler::new(config.server.worker_threads)),
    }
}

/// One dedicated thread per connection.
struct PerConnectionHandler;

impl ThreadHandler for PerConnectionHandler {
    fn dispatch(&self, instance: SharedInstance, stream: StdTcpStream, peer: SocketAddr) {
        std::thread::spawn(move || run_connection(instance, stream, peer));
    }
}

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A fixed pool of worker threads shared by all connections.
struct ThreadPoolHandler {
    sender: mpsc::Sender<Job>,
    _workers: Vec<std::thread::JoinHandle<()>>,
}

impl ThreadPoolHandler {
    fn new(size: usize) -> Self {
        let (sender, receiver) = mpsc::channel::<Job>();
        let receiver = Arc::new(parking_lot::Mutex::new(receiver));
        let mut workers = Vec::new();
        for _ in 0..size.max(1) {
            let receiver = Arc::clone(&receiver);
            workers.push(std::thread::spawn(move || loop {
                let job = {
                    let guard = receiver.lock();
                    guard.recv()
                };
                match job {
                    Ok(job) => {
                        // A panicking connection handler (e.g. from malformed
                        // input) must not shrink the pool permanently.
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    }
                    Err(_) => break,
                }
            }));
        }
        Self { sender, _workers: workers }
    }
}

impl ThreadHandler for ThreadPoolHandler {
    fn dispatch(&self, instance: SharedInstance, stream: StdTcpStream, peer: SocketAddr) {
        let job: Job = Box::new(move || run_connection(instance, stream, peer));
        let _ = self.sender.send(job);
    }
}

fn run_connection(instance: SharedInstance, mut stream: StdTcpStream, peer: SocketAddr) {
    // one session per connection so transactions span statements
    let mut session = Session::new();
    let result = serve_connection(&instance, &mut stream, &mut session);
    // roll back any open transaction when the session goes away
    if let Err(e) = instance.rollback_session(&mut session) {
        eprintln!("connection cleanup error: {e}");
    }
    if let Err(e) = result {
        eprintln!("connection {peer} error: {e}");
    }
}

/// Request protocol: `[u32 len][sql utf8]`; response: a sequence of wire frames.
fn serve_connection(
    instance: &Instance,
    stream: &mut StdTcpStream,
    session: &mut Session,
) -> io::Result<()> {
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
                match instance.execute_with(session, sql) {
                    Ok(results) => protocol.encode_success(&results, &mut out),
                    Err(e) => protocol.encode_failure(&e.to_string(), &mut out),
                }
                stream.write_all(&out)?;
                stream.flush()?;
            }
            Ok(None) => {
                let n = stream.read(&mut chunk)?;
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
