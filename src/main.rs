mod args;
mod cli;
mod client;
mod sessions;
mod stdio;

#[tokio::main]
async fn main() {
    let code = cli::run(std::env::args().skip(1).collect()).await;
    std::process::exit(code);
}
