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
    Attach {
        #[arg(default_value = "kingdom")]
        session_name: String,
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
    Swap {
        #[arg(default_value = ".")]
        workspace: PathBuf,
        worker_id: String,
        provider: Option<String>,
    },
    Log {
        #[arg(default_value = ".")]
        workspace: PathBuf,
        #[arg(long)]
        detail: Option<String>,
        #[arg(long)]
        actions: bool,
        #[arg(long)]
        limit: Option<usize>,
    },
    Doctor {
        #[arg(default_value = ".")]
        workspace: PathBuf,
    },
    Cost {
        #[arg(default_value = ".")]
        workspace: PathBuf,
    },
    Clean {
        #[arg(default_value = ".")]
        workspace: PathBuf,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        all: bool,
    },
    Restart {
        #[arg(default_value = ".")]
        workspace: PathBuf,
    },
    Replay {
        #[arg(default_value = ".")]
        workspace: PathBuf,
        job_id: String,
    },
    #[command(name = "job-diff")]
    JobDiff {
        #[arg(default_value = ".")]
        workspace: PathBuf,
        job_id: String,
    },
    Open {
        #[arg(default_value = ".")]
        workspace: PathBuf,
        target: String,
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
        Commands::Attach { session_name } => {
            kingdom_v2::cli::attach::run_attach(&session_name).unwrap_or_else(|e| {
                eprintln!("Attach error: {e}");
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
        Commands::Swap {
            workspace,
            worker_id,
            provider,
        } => {
            kingdom_v2::cli::swap::run_swap(workspace, worker_id, provider)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("Swap error: {e}");
                    std::process::exit(1);
                });
        }
        Commands::Log {
            workspace,
            detail,
            actions,
            limit,
        } => {
            kingdom_v2::cli::log::run_log(workspace, detail, actions, limit).unwrap_or_else(|e| {
                eprintln!("Log error: {e}");
                std::process::exit(1);
            });
        }
        Commands::Doctor { workspace } => {
            kingdom_v2::cli::doctor::run_doctor(workspace).unwrap_or_else(|e| {
                eprintln!("Doctor error: {e}");
                std::process::exit(1);
            });
        }
        Commands::Cost { workspace } => {
            kingdom_v2::cli::cost::run_cost(workspace).unwrap_or_else(|e| {
                eprintln!("Cost error: {e}");
                std::process::exit(1);
            });
        }
        Commands::Clean {
            workspace,
            dry_run,
            all,
        } => {
            kingdom_v2::cli::clean::run_clean(workspace, dry_run, all).unwrap_or_else(|e| {
                eprintln!("Clean error: {e}");
                std::process::exit(1);
            });
        }
        Commands::Restart { workspace } => {
            kingdom_v2::cli::restart::run_restart(workspace)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                });
        }
        Commands::Replay { workspace, job_id } => {
            kingdom_v2::cli::replay::run_replay(workspace, job_id)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                });
        }
        Commands::JobDiff { workspace, job_id } => {
            kingdom_v2::cli::job_diff::run_job_diff(workspace, job_id).unwrap_or_else(|e| {
                eprintln!("Error: {e}");
                std::process::exit(1);
            });
        }
        Commands::Open { workspace, target } => {
            kingdom_v2::cli::open::run_open(workspace, target).unwrap_or_else(|e| {
                eprintln!("Error: {e}");
                std::process::exit(1);
            });
        }
    }
}
