//! palicode - minimal, shortcut-driven terminal coding agent.

mod agent;
mod cli;
mod config;
mod event;
mod provider;
mod state;
mod tools;
mod ui;
mod util;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match cli::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
