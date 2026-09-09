use chibidb::{Database, run_repl};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let opened = match args.get(1) {
        Some(path) => Database::open(std::path::Path::new(path)),
        None => Database::open_in_memory(),
    };
    let mut db = match opened {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: cannot open database: {e}");
            std::process::exit(1);
        }
    };
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    run_repl(&mut db, stdin, &mut stdout).await?;
    if let Err(e) = db.flush() {
        eprintln!("error: flush failed: {e}");
    }
    Ok(())
}
