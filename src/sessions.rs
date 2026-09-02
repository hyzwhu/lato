use std::path::PathBuf;

const SESSION_LIST_SCHEMA_VERSION: u32 = 1;

#[derive(serde::Serialize)]
struct SessionListOutput {
    schema_version: u32,
    sessions: Vec<String>,
}

pub async fn list(json: bool) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match crate::client::list_sessions_over_acp(cwd).await {
        Ok(mut sessions) => {
            sessions.sort_by(|a, b| b.cmp(a));
            if json {
                let output = SessionListOutput {
                    schema_version: SESSION_LIST_SCHEMA_VERSION,
                    sessions,
                };
                match serde_json::to_string_pretty(&output) {
                    Ok(body) => println!("{body}"),
                    Err(error) => {
                        eprintln!("error: {error}");
                        return 1;
                    }
                }
            } else if sessions.is_empty() {
                println!("No saved sessions.");
            } else {
                for session in sessions {
                    println!("{session}");
                }
            }
            0
        }
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}
