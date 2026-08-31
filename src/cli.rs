use lato_agent::default_fake_stream;
use lato_ai::{
    CredentialStore, HttpModelStream, ModelStream, api_key_login_allowed, get_auth, lookup_model,
    oauth_allowed, store_oauth,
};
use lato_workspace::SessionTrust;
use std::{io::IsTerminal, path::PathBuf, sync::Arc};

pub async fn run(args: Vec<String>) -> i32 {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("usage: lato -p [--ask] TEXT | lato login PROVIDER (--api-key KEY|--oauth)");
        return 0;
    }
    if args[0] == "acp" {
        return crate::stdio::run().await;
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
    let model_arg = args
        .iter()
        .position(|a| a == "--model")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| std::env::var("LATO_MODEL").ok());
    let mut text_parts = Vec::new();
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg == "--ask" {
            continue;
        }
        if arg == "--model" {
            skip = true;
            continue;
        }
        text_parts.push(arg.clone());
    }
    let text = text_parts.join(" ");
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let trust = if ask {
        SessionTrust::for_interactive(&cwd, true)
    } else {
        SessionTrust::for_headless_prompt(&cwd)
    };
    let stream: Arc<dyn ModelStream> = if let Some(selection) = model_arg {
        let Some((provider, model_id)) = selection.split_once('/') else {
            eprintln!("error: --model must be provider/model");
            return 2;
        };
        let Some(model) = lookup_model(provider, model_id) else {
            eprintln!("error: unknown model {selection}");
            return 1;
        };
        let store = match CredentialStore::open(&lato_home()) {
            Ok(store) => store,
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        };
        let Some(auth) = get_auth(&store, provider, &|name| std::env::var(name).ok(), None).await
        else {
            eprintln!("error: no credential configured for {provider}; run lato login {provider}");
            return 1;
        };
        Arc::new(HttpModelStream::new(model, auth))
    } else {
        default_fake_stream()
    };
    match crate::client::run_prompt_over_acp_with_stream(cwd, trust, text, stream).await {
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
        if std::env::var_os("LATO_MOCK_OAUTH").is_some() {
            let mut store = CredentialStore::open(&home).unwrap();
            store_oauth(
                &mut store,
                provider,
                "mock-access",
                "mock-refresh",
                4_102_444_800,
            )
            .unwrap();
            println!("oauth logged in {provider}");
            return 0;
        }
        eprintln!("error: oauth requires interactive/device flow; set LATO_MOCK_OAUTH=1 in tests");
        return 1;
    }
    if let Some(i) = args.iter().position(|a| a == "--api-key") {
        if !api_key_login_allowed(provider) {
            eprintln!("error: api-key login not supported for {provider}");
            return 1;
        }
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
