//! palicode - minimal, shortcut-driven terminal coding agent.

mod agent;
mod approval;
mod cli;
mod config;
mod credentials;
mod event;
mod memory;
mod provider;
mod state;
mod token;
mod tools;
mod trust;
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
