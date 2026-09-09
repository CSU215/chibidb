use std::path::Path;
use std::sync::Arc;

use chibidb::{client, run_repl, server, Database};

const DEFAULT_ADDR: &str = "127.0.0.1:5678";

fn usage() -> ! {
    eprintln!(
        "usage:\n  chibidb [dir]            interactive REPL (in-memory without dir)\n  chibidb serve <dir> [addr]  start TCP server (default {DEFAULT_ADDR})\n  chibidb client [addr]    connect to a running server"
    );
    std::process::exit(2);
}

fn open(dir: &str) -> Database {
    match Database::open(Path::new(dir)) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.len() {
        // REPL, in-memory
        1 => repl(Database::open_in_memory().expect("cannot open database")).await,
        // REPL, file-backed
        2 if args[1] != "serve" && args[1] != "client" => repl(open(&args[1])).await,
        // server
        3 | 4 if args[1] == "serve" => {
            let addr = args.get(3).cloned().unwrap_or_else(|| DEFAULT_ADDR.to_string());
            let db = Arc::new(tokio::sync::Mutex::new(open(&args[2])));
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            eprintln!("chibidb server listening on {addr}");
            server::serve(db, listener).await
        }
        // client
        2 if args[1] == "client" => connect(&DEFAULT_ADDR.to_string()).await,
        3 if args[1] == "client" => connect(&args[2]).await,
        _ => usage(),
    }
}

async fn repl(mut db: Database) -> std::io::Result<()> {
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    run_repl(&mut db, stdin, &mut stdout).await?;
    if let Err(e) = db.flush() {
        eprintln!("error: flush failed: {e}");
    }
    Ok(())
}

async fn connect(addr: &str) -> std::io::Result<()> {
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    eprintln!("connected to {addr}");
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    client::run_client(&mut stream, stdin, &mut stdout).await
}
