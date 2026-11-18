use clap::{Parser, Subcommand};
use serde_json::json;
use skflow::engine::{self, Command};
use std::{
    io::{self, Write},
    path::PathBuf,
};

#[derive(Parser)]
#[command(
    version,
    about = "Local, model-free workflow and cognitive task CLI for Agent Skills"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        default_value = ".skflow/state.db",
        help = "SQLite run file; use a different path for an independent run"
    )]
    db: PathBuf,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Initialize a run and advance to the first cognitive task.
    Start {
        workflow: PathBuf,
        #[arg(long)]
        input: PathBuf,
    },
    /// Resume unfinished work, or reproduce pending self-contained tasks.
    Next,
    /// Validate and commit one task's answer, then advance if possible.
    Submit {
        task_id: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// Read the latest committed progress and pending tasks.
    Status,
    /// Read a JSON Pointer from state (an empty pointer reads the whole tree).
    Inspect { json_path: String },
    /// Invalidate this step and all following outputs/tasks, then re-run.
    Rewind {
        step_name: String,
        #[arg(long)]
        accept_workflow: bool,
    },
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            std::process::exit(0);
        }
        Err(error) => {
            println!(
                "{}",
                json!({"status":"error","errors":[{"code":"cli_usage","message":error.to_string()}]})
            );
            std::process::exit(1);
        }
    };
    let command = match cli.command {
        Action::Start { workflow, input } => Command::Start { workflow, input },
        Action::Next => Command::Next,
        Action::Submit { task_id, file } => Command::Submit { task_id, file },
        Action::Status => Command::Status,
        Action::Inspect { json_path } => Command::Inspect { path: json_path },
        Action::Rewind {
            step_name,
            accept_workflow,
        } => Command::Rewind {
            step: step_name,
            accept_workflow,
        },
    };
    let response = engine::execute(&cli.db, command).unwrap_or_else(|error| {
        json!({
            "status":"error", "errors":[{"code":"operation_error","message":format!("{error:#}")}]
        })
    });
    let exit = engine::exit_code(&response);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if serde_json::to_writer_pretty(&mut out, &response).is_err() || writeln!(out).is_err() {
        std::process::exit(1);
    }
    std::process::exit(exit);
}
