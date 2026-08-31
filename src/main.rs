mod cli;
mod client;

#[tokio::main]
async fn main() {
    let code = cli::run(std::env::args().skip(1).collect()).await;
    std::process::exit(code);
}
