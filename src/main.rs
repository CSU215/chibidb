use std::path::Path;
use std::sync::Arc;

use chibidb::config::Config;
use chibidb::instance::Instance;
use chibidb::{client, run_repl, server};

fn usage() -> ! {
    eprintln!(
        "usage:\n  chibidb [dir]            interactive REPL (in-memory without dir)\n  chibidb serve <dir> [addr]  start TCP server (addr defaults to config.toml)\n  chibidb client [addr]    connect to a running server"
    );
    std::process::exit(2);
}

fn open(dir: &str, config: &Config) -> Instance {
    match Instance::open(Path::new(dir), config) {
        Ok(instance) => instance,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let config = match Config::load(Path::new("config.toml")) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let default_addr = config.server.addr.clone();
    let args: Vec<String> = std::env::args().collect();
    match args.len() {
        // REPL, in-memory
        1 => repl(Instance::open_in_memory(&config).expect("cannot open instance")).await,
        // REPL, file-backed
        2 if args[1] != "serve" && args[1] != "client" => repl(open(&args[1], &config)).await,
        // server
        3 | 4 if args[1] == "serve" => {
            let addr = args.get(3).cloned().unwrap_or_else(|| default_addr.clone());
            let instance = Arc::new(open(&args[2], &config));
            if let Some(http_addr) = config.server.http_addr.clone() {
                let http_listener = tokio::net::TcpListener::bind(&http_addr).await?;
                eprintln!("chibidb http listening on {http_addr}");
                let http_instance = instance.clone();
                tokio::spawn(async move {
                    if let Err(e) = chibidb::http::serve(http_instance, http_listener).await {
                        eprintln!("http server error: {e}");
                    }
                });
            }
            if let Some(mysql_addr) = config.server.mysql_addr.clone() {
                let mysql_listener = tokio::net::TcpListener::bind(&mysql_addr).await?;
                eprintln!("chibidb mysql listening on {mysql_addr}");
                let mysql_instance = instance.clone();
                tokio::spawn(async move {
                    if let Err(e) = chibidb::mysql::serve(mysql_instance, mysql_listener).await {
                        eprintln!("mysql server error: {e}");
                    }
                });
            }
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            eprintln!("chibidb server listening on {addr}");
            server::serve(instance, listener).await
        }
        // client
        2 if args[1] == "client" => connect(&default_addr).await,
        3 if args[1] == "client" => connect(&args[2]).await,
        _ => usage(),
    }
}

async fn repl(instance: Instance) -> std::io::Result<()> {
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    run_repl(&instance, stdin, &mut stdout).await?;
    if let Err(e) = instance.flush() {
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
