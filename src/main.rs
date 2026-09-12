mod args;
mod cli;
mod client;
mod permissions;
mod resume;
mod sessions;
mod stdio;
mod tui;
mod workflow;

#[tokio::main]
async fn main() {
    let code = cli::run(std::env::args().skip(1).collect()).await;
    std::process::exit(code);
}
