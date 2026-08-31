use lato_agent::{ToolApproval, default_fake_stream};
use lato_ai::{
    AuthInteraction, AuthNotice, CATALOG, CredentialStore, CustomHttpModelStream, HttpModelStream,
    ModelStream, api_key_login_allowed, custom_model_auth, get_auth_refreshing, load_models_json,
    login_oauth, lookup_model, oauth_allowed, phase0_supported, store_oauth,
};
use lato_workspace::{ApprovalMode, SandboxProfile, SessionTrust};
use rustyline::{
    CompletionType, Config, Context, Editor, Helper,
    completion::{Completer, Pair},
    error::ReadlineError,
    highlight::Highlighter,
    hint::Hinter,
    history::DefaultHistory,
    validate::Validator,
};
use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    sync::Arc,
};

#[derive(serde::Serialize, serde::Deserialize)]
struct CliSettings {
    default_model: String,
}

const INTERACTIVE_COMMANDS: &[&str] = &[
    "/help", "/clear", "/model", "/approve", "/status", "/exit", "/quit",
];

struct LatoLineHelper;
impl Helper for LatoLineHelper {}
impl Validator for LatoLineHelper {}
impl Highlighter for LatoLineHelper {}
impl Hinter for LatoLineHelper {
    type Hint = String;
}
impl Completer for LatoLineHelper {
    type Candidate = Pair;
    fn complete(
        &self,
        line: &str,
        _position: usize,
        _context: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        if !line.starts_with('/') {
            return Ok((0, Vec::new()));
        }
        Ok((
            0,
            INTERACTIVE_COMMANDS
                .iter()
                .filter(|command| command.starts_with(line))
                .map(|command| Pair {
                    display: (*command).to_string(),
                    replacement: (*command).to_string(),
                })
                .collect(),
        ))
    }
}

struct ConsoleToolApproval;

#[async_trait::async_trait]
impl ToolApproval for ConsoleToolApproval {
    async fn approve(&self, name: &str, arguments: &serde_json::Value) -> bool {
        let name = name.to_string();
        let arguments = arguments.to_string();
        tokio::task::spawn_blocking(move || {
            println!("\nTool request: {name}\n  {arguments}");
            read_line("Allow this tool call? [y/N] ")
                .map(|answer| matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }
}

pub async fn run(args: Vec<String>) -> i32 {
    if args.is_empty() {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            eprintln!("error: interactive mode requires a tty; use lato -p TEXT for headless mode");
            return 2;
        }
        return interactive().await;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "usage: lato\n       lato -p [--ask] [--sandbox off|workspace|read-only] [--model provider/model] TEXT\n       lato acp\n       lato login PROVIDER (--api-key KEY|--oauth)\n\nRun without arguments for the interactive coding CLI."
        );
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
    let sandbox = match args
        .iter()
        .position(|a| a == "--sandbox")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
    {
        None | Some("off") => SandboxProfile::Off,
        Some("workspace") => SandboxProfile::Workspace,
        Some("read-only") => SandboxProfile::ReadOnly,
        Some(other) => {
            eprintln!("error: unknown sandbox profile {other}");
            return 2;
        }
    };
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
        if arg == "--model" || arg == "--sandbox" {
            skip = true;
            continue;
        }
        text_parts.push(arg.clone());
    }
    let text = text_parts.join(" ");
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut trust = if ask {
        SessionTrust::for_interactive(&cwd, true)
    } else {
        SessionTrust::for_headless_prompt(&cwd)
    };
    trust.sandbox = sandbox;
    let stream: Arc<dyn ModelStream> = if let Some(selection) = model_arg {
        match configured_stream(&selection).await {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("error: {error}");
                return 1;
            }
        }
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

async fn interactive() -> i32 {
    let home = lato_home();
    if let Err(error) = std::fs::create_dir_all(&home) {
        eprintln!("error: cannot create {}: {error}", home.display());
        return 1;
    }
    println!("Lato coding agent\nType /help for commands.\n");
    let mut selection = match load_settings(&home) {
        Ok(Some(settings)) => settings.default_model,
        Ok(None) => match configure_interactively(&home).await {
            Ok(selection) => selection,
            Err(error) => {
                eprintln!("error: {error}");
                return 1;
            }
        },
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let stream = match configured_stream(&selection).await {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("The saved model cannot start: {error}");
            match configure_interactively(&home).await {
                Ok(new_selection) => {
                    selection = new_selection;
                    match configured_stream(&selection).await {
                        Ok(stream) => stream,
                        Err(error) => {
                            eprintln!("error: {error}");
                            return 1;
                        }
                    }
                }
                Err(error) => {
                    eprintln!("error: {error}");
                    return 1;
                }
            }
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let trusted = match read_line("Trust this folder and allow edits/commands this session? [y/N] ")
    {
        Ok(answer) => matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"),
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let trust = if trusted {
        SessionTrust::for_interactive_auto(&cwd)
    } else {
        SessionTrust::for_interactive(&cwd, false)
    };
    let approval = trust.clone();
    let inline_approval: Option<Arc<dyn ToolApproval>> = (approval.mode == ApprovalMode::Ask)
        .then(|| Arc::new(ConsoleToolApproval) as Arc<dyn ToolApproval>);
    let mut client = match crate::client::InteractiveAcpClient::new_with_approval(
        cwd.clone(),
        trust,
        stream,
        inline_approval,
    )
    .await
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let history_path = home.join("history");
    let config = Config::builder()
        .completion_type(CompletionType::List)
        .build();
    let mut editor = match Editor::<LatoLineHelper, DefaultHistory>::with_config(config) {
        Ok(editor) => editor,
        Err(error) => {
            eprintln!("error: initialize line editor: {error}");
            return 1;
        }
    };
    editor.set_helper(Some(LatoLineHelper));
    let _ = editor.load_history(&history_path);
    println!("Model: {selection}\nWorkspace: {}\n", cwd.display());
    loop {
        let input = match editor.readline("lato> ") {
            Ok(input) => input.trim().to_string(),
            Err(ReadlineError::Interrupted) => {
                println!("^C");
                continue;
            }
            Err(ReadlineError::Eof) => {
                let _ = editor.save_history(&history_path);
                println!("Goodbye.");
                return 0;
            }
            Err(error) => {
                eprintln!("error: {error}");
                return 1;
            }
        };
        if input.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(input.as_str());
        let _ = editor.save_history(&history_path);
        match input.as_str() {
            "/exit" | "/quit" => {
                println!("Goodbye.");
                return 0;
            }
            "/help" => {
                println!(
                    "/help       show commands\n/clear      start a fresh conversation\n/model      configure the default model for the next run\n/approve    pre-approve one mutating tool call\n/status     show model and workspace\n/exit       quit\n\nUse Up/Down for history and Tab to complete slash commands."
                );
                continue;
            }
            "/clear" => {
                match client.clear().await {
                    Ok(()) => println!("Conversation cleared."),
                    Err(error) => eprintln!("error: {error}"),
                }
                continue;
            }
            "/model" => {
                match configure_interactively(&home).await {
                    Ok(new_selection) => println!(
                        "Saved {new_selection}. Restart Lato to switch the current conversation."
                    ),
                    Err(error) => eprintln!("error: {error}"),
                }
                continue;
            }
            "/approve" => {
                approval.allow_once();
                println!("Approved one mutating tool call.");
                continue;
            }
            "/status" => {
                println!("Model: {selection}\nWorkspace: {}", cwd.display());
                continue;
            }
            command if command.starts_with('/') => {
                eprintln!("Unknown command. Type /help.");
                continue;
            }
            _ => {}
        }
        print!("Lato: ");
        let _ = std::io::stdout().flush();
        let mut streamed = false;
        let result = client
            .send_streaming(input, |event| {
                if let Some(delta) = event
                    .pointer("/params/delta")
                    .and_then(|value| value.as_str())
                {
                    streamed = true;
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                } else if event["method"] == "session/tool_call"
                    && let Some(name) = event
                        .pointer("/params/name")
                        .and_then(|value| value.as_str())
                {
                    println!("\n[tool] {name}");
                }
            })
            .await;
        match result {
            Ok(_text) if streamed => println!("\n"),
            Ok(text) => println!("{text}\n"),
            Err(error) => eprintln!("\nerror: {error}\n"),
        }
    }
}

async fn configure_interactively(home: &std::path::Path) -> Result<String, String> {
    let mut options: Vec<String> = CATALOG
        .iter()
        .filter(|model| phase0_supported(model.api))
        .filter(|model| api_key_login_allowed(model.provider) || oauth_allowed(model.provider))
        .map(|model| format!("{}/{}", model.provider, model.id))
        .collect();
    if let Ok(custom) = load_models_json(&home.join("models.json")) {
        options.extend(
            custom
                .into_iter()
                .map(|model| format!("{}/{}", model.provider, model.id)),
        );
    }
    options.sort();
    options.dedup();
    println!("Choose a model:");
    for (index, model) in options.iter().enumerate() {
        println!("  {}) {model}", index + 1);
    }
    let answer = read_line("Selection: ").map_err(|e| e.to_string())?;
    let index: usize = answer
        .parse()
        .map_err(|_| "invalid model selection".to_string())?;
    let selection = options
        .get(index.saturating_sub(1))
        .cloned()
        .ok_or("invalid model selection")?;
    let (provider, _) = selection.split_once('/').ok_or("invalid model")?;
    if lookup_model(provider, selection.split_once('/').unwrap().1).is_some() {
        let store = CredentialStore::open(home).map_err(|e| e.to_string())?;
        if store.get(provider).is_none() && !provider_env_configured(provider) {
            if oauth_allowed(provider) {
                let method = read_line("Authentication: 1) OAuth  2) API key [1]: ")
                    .map_err(|e| e.to_string())?;
                if method.trim().is_empty() || method.trim() == "1" {
                    let tokens =
                        login_oauth(provider, &ConsoleAuthInteraction, &reqwest::Client::new())
                            .await?;
                    let mut store = CredentialStore::open(home).map_err(|e| e.to_string())?;
                    store_oauth(
                        &mut store,
                        provider,
                        &tokens.access,
                        &tokens.refresh,
                        tokens.expires,
                    )
                    .map_err(|e| e.to_string())?;
                } else if api_key_login_allowed(provider) {
                    save_interactive_api_key(home, provider)?;
                } else {
                    return Err(format!("API-key login is not supported for {provider}"));
                }
            } else {
                save_interactive_api_key(home, provider)?;
            }
        }
    }
    save_settings(
        home,
        &CliSettings {
            default_model: selection.clone(),
        },
    )?;
    Ok(selection)
}

fn save_interactive_api_key(home: &std::path::Path, provider: &str) -> Result<(), String> {
    let key = rpassword::prompt_password(format!("API key for {provider}: "))
        .map_err(|e| e.to_string())?;
    if key.trim().is_empty() {
        return Err("API key cannot be empty".into());
    }
    let mut store = CredentialStore::open(home).map_err(|e| e.to_string())?;
    store
        .modify(|entries| {
            entries.insert(
                provider.to_string(),
                serde_json::json!({"type":"api_key","key":key.trim()}),
            );
        })
        .map_err(|e| e.to_string())
}

async fn configured_stream(selection: &str) -> Result<Arc<dyn ModelStream>, String> {
    let (provider, model_id) = selection
        .split_once('/')
        .ok_or("model must be provider/model")?;
    if let Some(model) = lookup_model(provider, model_id) {
        let mut store = CredentialStore::open(&lato_home()).map_err(|e| e.to_string())?;
        let auth = get_auth_refreshing(
            &mut store,
            provider,
            &|name| std::env::var(name).ok(),
            None,
            &reqwest::Client::new(),
        )
        .await?
        .ok_or_else(|| format!("no credential configured for {provider}"))?;
        Ok(Arc::new(HttpModelStream::new(model, auth)))
    } else {
        let custom = load_models_json(&lato_home().join("models.json"))
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|model| model.provider == provider && model.id == model_id)
            .ok_or_else(|| format!("unknown model {selection}"))?;
        let auth = custom_model_auth(&custom, &|name| std::env::var(name).ok())
            .ok_or_else(|| format!("environment variable {} is not configured", custom.env))?;
        Ok(Arc::new(CustomHttpModelStream::new(custom, auth)))
    }
}

fn provider_env_configured(provider: &str) -> bool {
    lato_ai::env_names(provider)
        .iter()
        .any(|name| std::env::var_os(name).is_some())
}

fn load_settings(home: &std::path::Path) -> Result<Option<CliSettings>, String> {
    let path = home.join("config.json");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| format!("parse {}: {e}", path.display()))
}

fn save_settings(home: &std::path::Path, settings: &CliSettings) -> Result<(), String> {
    std::fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let path = home.join("config.json");
    let temporary = home.join("config.json.tmp");
    std::fs::write(
        &temporary,
        serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    std::fs::rename(temporary, path).map_err(|e| e.to_string())
}

fn read_line(prompt: &str) -> std::io::Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
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
                4_102_444_800_000,
            )
            .unwrap();
            println!("oauth logged in {provider}");
            return 0;
        }
        if !std::io::stdin().is_terminal() {
            eprintln!(
                "error: oauth CLI login requires a tty; use lato/auth/login from an ACP client"
            );
            return 2;
        }
        match login_oauth(provider, &ConsoleAuthInteraction, &reqwest::Client::new()).await {
            Ok(tokens) => {
                let mut store = CredentialStore::open(&home).unwrap();
                if let Err(e) = store_oauth(
                    &mut store,
                    provider,
                    &tokens.access,
                    &tokens.refresh,
                    tokens.expires,
                ) {
                    eprintln!("error: {e}");
                    return 1;
                }
                println!("oauth logged in {provider}");
                return 0;
            }
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        }
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

struct ConsoleAuthInteraction;

#[async_trait::async_trait]
impl AuthInteraction for ConsoleAuthInteraction {
    async fn notify(&self, notice: AuthNotice) {
        match notice {
            AuthNotice::AuthUrl(url) => eprintln!(
                "Open this URL in your browser:\n{url}\nThen paste the full redirect URL."
            ),
            AuthNotice::DeviceCode {
                code,
                verification_url,
            } => eprintln!("Open {verification_url} and enter code: {code}"),
            AuthNotice::Info(message) | AuthNotice::Progress(message) => eprintln!("{message}"),
        }
    }

    async fn redirect_url(&self) -> Result<String, String> {
        tokio::task::spawn_blocking(|| {
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .map_err(|e| e.to_string())?;
            Ok(input.trim().to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }
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
