#[tokio::main]
async fn main() {
    if let Err(error) = Box::pin(mimir::cli::entrypoint()).await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
