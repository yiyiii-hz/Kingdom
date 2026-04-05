use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "kingdom", about = "Kingdom AI worker orchestrator")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Up {
        #[arg(default_value = ".")]
        workspace: PathBuf,
    },
    Down {
        #[arg(default_value = ".")]
        workspace: PathBuf,
        #[arg(long)]
        force: bool,
    },
    Daemon {
        workspace: PathBuf,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Up { workspace } => {
            kingdom_v2::cli::up::run_up(workspace)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                });
        }
        Commands::Down { workspace, force } => {
            kingdom_v2::cli::down::run_down(workspace, force)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                });
        }
        Commands::Daemon { workspace } => {
            kingdom_v2::cli::daemon::run_daemon(workspace)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("Daemon error: {e}");
                    std::process::exit(1);
                });
        }
    }
}
