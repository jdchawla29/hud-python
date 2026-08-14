mod agent;
mod claude_cli;
mod cli;
mod dashboard;
mod gateway;
mod git;
mod harness;
mod interrupt;
mod model;
mod orchestrate;
mod orchestrator;
mod paths;
mod runner;
mod session;
mod state;
mod ui;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    if let Some(home) = std::env::var_os("HOME") {
        let _ = dotenvy::from_path(std::path::PathBuf::from(home).join(".hud/.env"));
    }
    cli::run().await
}
