use lato_ai::{CredentialStore, oauth_allowed};
use lato_workspace::SessionTrust;
use std::{io::IsTerminal, path::PathBuf};

pub async fn run(args: Vec<String>) -> i32 {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("usage: lato -p [--ask] TEXT | lato login PROVIDER (--api-key KEY|--oauth)");
        return 0;
    }
    if args[0] == "login" {
        return login(&args[1..]).await;
    }
    if args[0] == "-p" {
        return prompt(&args[1..]).await;
    }
    eprintln!("error: unknown command");
    2
}

async fn prompt(args: &[String]) -> i32 {
    let ask = args.iter().any(|a| a == "--ask");
    if ask && !std::io::stdin().is_terminal() {
        eprintln!("error: --ask requires a tty");
        return 2;
    }
    let text = args
        .iter()
        .filter(|a| a.as_str() != "--ask")
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let trust = if ask {
        SessionTrust::for_interactive(&cwd, true)
    } else {
        SessionTrust::for_headless_prompt(&cwd)
    };
    match crate::client::run_prompt_over_acp(cwd, trust, text, None).await {
        Ok(s) => {
            println!("{s}");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

async fn login(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("error: missing provider");
        return 2;
    }
    let provider = &args[0];
    let home = lato_home();
    if args.iter().any(|a| a == "--oauth") {
        if !oauth_allowed(provider) {
            eprintln!("error: oauth not supported for {provider}");
            return 1;
        }
        eprintln!("error: oauth is phase 2");
        return 1;
    }
    if let Some(i) = args.iter().position(|a| a == "--api-key") {
        let Some(key) = args.get(i + 1) else {
            eprintln!("error: missing api key");
            return 2;
        };
        let mut store = CredentialStore::open(&home).unwrap();
        store
            .modify(|m| {
                m.insert(
                    provider.clone(),
                    serde_json::json!({"type":"api_key","key":key}),
                );
            })
            .unwrap();
        println!("logged in {provider}");
        return 0;
    }
    eprintln!("error: expected --api-key or --oauth");
    2
}

fn lato_home() -> PathBuf {
    std::env::var_os("LATO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".lato")
        })
}
