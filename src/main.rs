use chibidb::{Database, run_repl};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut db = Database::open_in_memory();
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    run_repl(&mut db, stdin, &mut stdout).await
}
