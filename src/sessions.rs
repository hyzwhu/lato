use crate::{
    args::SessionCommand,
    client::{
        SessionSummary, delete_session_over_acp, list_session_summaries_over_acp,
        rename_session_over_acp,
    },
};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

const SESSION_LIST_SCHEMA_VERSION: u32 = 2;

#[derive(serde::Serialize)]
struct SessionListOutput {
    schema_version: u32,
    sessions: Vec<SessionSummary>,
}

pub async fn run(command: SessionCommand) -> i32 {
    match command {
        SessionCommand::List { json } => list(json).await,
        SessionCommand::Rename { session_id, title } => rename(&session_id, &title).await,
        SessionCommand::Delete { session_id, yes } => delete(&session_id, yes).await,
    }
}

async fn list(json: bool) -> i32 {
    match list_session_summaries_over_acp(current_dir(), crate::cli::lato_home()).await {
        Ok(sessions) => {
            if json {
                let output = SessionListOutput {
                    schema_version: SESSION_LIST_SCHEMA_VERSION,
                    sessions,
                };
                match serde_json::to_string_pretty(&output) {
                    Ok(body) => println!("{body}"),
                    Err(error) => return print_error(error),
                }
            } else if sessions.is_empty() {
                println!("No saved sessions.");
            } else {
                for session in sessions {
                    println!("{}\t{}", session.title, session.session_id);
                }
            }
            0
        }
        Err(error) => print_error(error),
    }
}

async fn rename(session_id: &str, title: &str) -> i32 {
    match rename_session_over_acp(current_dir(), crate::cli::lato_home(), session_id, title).await {
        Ok(summary) => {
            println!("Renamed {} to {}", summary.session_id, summary.title);
            0
        }
        Err(error) => print_error(error),
    }
}

async fn delete(session_id: &str, yes: bool) -> i32 {
    if !yes {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            eprintln!(
                "error: deleting a session requires confirmation; rerun with --yes in non-interactive use"
            );
            return 2;
        }
        print!("Permanently delete session {session_id}? [y/N] ");
        if let Err(error) = io::stdout().flush() {
            return print_error(error);
        }
        let mut answer = String::new();
        if let Err(error) = io::stdin().read_line(&mut answer) {
            return print_error(error);
        }
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Cancelled.");
            return 0;
        }
    }
    match delete_session_over_acp(current_dir(), crate::cli::lato_home(), session_id).await {
        Ok(()) => {
            println!("Deleted session {session_id} permanently.");
            0
        }
        Err(error) => print_error(error),
    }
}

fn current_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn print_error(error: impl std::fmt::Display) -> i32 {
    eprintln!("error: {error}");
    1
}
