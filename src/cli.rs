use lato_agent::{ApprovalRequest, ToolApproval, default_fake_stream};
use lato_ai::{
    AuthInteraction, AuthNotice, CATALOG, CredentialStore, CustomHttpModelStream, CustomModel,
    HttpModelStream, ModelApi, ModelStream, ProviderModelsStore, adapt_model_stream,
    api_key_login_allowed, custom_model_auth, get_auth_refreshing, load_models_json, login_oauth,
    lookup_model, oauth_allowed, phase0_supported, provider_spec, refresh_openai_compatible_models,
    refresh_remote_provider_catalog, store_oauth,
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
    "/help", "/clear", "/model", "/login", "/approve", "/status", "/exit", "/quit",
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
    async fn approve(&self, request: &ApprovalRequest) -> bool {
        let tool = request.request.tool_name.to_string();
        let capabilities = format!("{:?}", request.request.capabilities);
        let side_effect = format!("{:?}", request.request.side_effect);
        let summary = request.summary.clone();
        tokio::task::spawn_blocking(move || {
            println!(
                "\nTool request: {tool}\n  capabilities: {capabilities}\n  side effect: {side_effect}\n  {summary}"
            );
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
    let session_trust = trust.clone();
    let approval = trust.clone();
    let inline_approval: Option<Arc<dyn ToolApproval>> = (approval.mode == ApprovalMode::Ask)
        .then(|| Arc::new(ConsoleToolApproval) as Arc<dyn ToolApproval>);
    let mut client = match crate::client::InteractiveAcpClient::new_with_approval(
        cwd.clone(),
        trust,
        stream,
        inline_approval.clone(),
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
                let _ = editor.save_history(&history_path);
                println!("^C\nGoodbye.");
                return 0;
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
            command if is_exit_command(command) => {
                println!("Goodbye.");
                return 0;
            }
            "/help" => {
                println!(
                    "/help       show commands\n/clear      start a fresh conversation\n/model      choose and switch model\n/login      replace credential for the current provider\n/approve    pre-approve one mutating tool call\n/status     show model and workspace\n/exit       quit\n\nUse Up/Down for history and Tab to complete slash commands."
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
                    Ok(new_selection) => match configured_stream(&new_selection).await {
                        Ok(stream) => match crate::client::InteractiveAcpClient::new_with_approval(
                            cwd.clone(),
                            session_trust.clone(),
                            stream,
                            inline_approval.clone(),
                        )
                        .await
                        {
                            Ok(new_client) => {
                                client = new_client;
                                selection = new_selection;
                                println!("Switched to {selection}; started a fresh conversation.");
                            }
                            Err(error) => eprintln!("error: {error}"),
                        },
                        Err(error) => eprintln!("error: {error}"),
                    },
                    Err(error) => eprintln!("error: {error}"),
                }
                continue;
            }
            "/login" => {
                let Some((provider, _)) = selection.split_once('/') else {
                    eprintln!("error: invalid current model");
                    continue;
                };
                match configure_provider_auth(&home, provider, true).await {
                    Ok(()) => match configured_stream(&selection).await {
                        Ok(stream) => match crate::client::InteractiveAcpClient::new_with_approval(
                            cwd.clone(),
                            session_trust.clone(),
                            stream,
                            inline_approval.clone(),
                        )
                        .await
                        {
                            Ok(new_client) => {
                                client = new_client;
                                println!(
                                    "Credential replaced for {provider}; started a fresh conversation."
                                );
                            }
                            Err(error) => eprintln!("error: {error}"),
                        },
                        Err(error) => eprintln!("error: {error}"),
                    },
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
        let streamed = std::cell::Cell::new(false);
        let response = client.send_streaming(input, |event| {
            if let Some(delta) = event
                .pointer("/params/delta")
                .and_then(|value| value.as_str())
            {
                streamed.set(true);
                print!("{delta}");
                let _ = std::io::stdout().flush();
            } else if event["method"] == "session/tool_call"
                && let Some(name) = event
                    .pointer("/params/name")
                    .and_then(|value| value.as_str())
            {
                println!("\n[tool] {name}");
            }
        });
        tokio::pin!(response);
        let result = tokio::select! {
            result = &mut response => result,
            signal = tokio::signal::ctrl_c() => {
                let _ = editor.save_history(&history_path);
                match signal {
                    Ok(()) => println!("\n^C\nGoodbye."),
                    Err(error) => eprintln!("\nerror: Ctrl-C handler failed: {error}"),
                }
                return 0;
            }
        };
        match result {
            Ok(_text) if streamed.get() => println!("\n"),
            Ok(text) => println!("{text}\n"),
            Err(error) => eprintln!("\nerror: {error}\n"),
        }
    }
}

async fn configure_interactively(home: &std::path::Path) -> Result<String, String> {
    let custom_models = load_models_json(&home.join("models.json")).unwrap_or_default();
    let mut providers = CATALOG
        .iter()
        .filter(|model| phase0_supported(model.api))
        .filter(|model| api_key_login_allowed(model.provider) || oauth_allowed(model.provider))
        .map(|model| model.provider.to_string())
        .chain(custom_models.iter().map(|model| model.provider.clone()))
        .collect::<Vec<_>>();
    providers.sort();
    providers.dedup();
    println!("Choose a provider:");
    for (index, provider) in providers.iter().enumerate() {
        println!("  {}) {provider}", index + 1);
    }
    let provider = choose_item("Provider: ", &providers, "provider")?;
    let is_catalog_provider = CATALOG.iter().any(|model| model.provider == provider);
    if is_catalog_provider {
        configure_provider_auth(home, &provider, false).await?;
    }

    let fallback = CATALOG
        .iter()
        .filter(|model| model.provider == provider && phase0_supported(model.api))
        .map(|model| CustomModel {
            provider: provider.clone(),
            id: model.id.to_string(),
            api: model.api,
            base_url: model.base_url.unwrap_or_default().to_string(),
            env: lato_ai::env_names(&provider)
                .first()
                .copied()
                .unwrap_or("LATO_API_KEY")
                .to_string(),
        })
        .chain(
            custom_models
                .into_iter()
                .filter(|model| model.provider == provider),
        )
        .collect::<Vec<_>>();
    let models = if !should_discover_provider_models(&provider) {
        fallback
    } else if is_catalog_provider {
        match discover_provider_models(home, &provider).await {
            Ok(models) if !models.is_empty() => {
                println!("Fetched {} catalog models for {provider}.", models.len());
                let mut merged = fallback.clone();
                for model in models {
                    if let Some(index) = merged.iter().position(|entry| entry.id == model.id) {
                        merged[index] = model;
                    } else {
                        merged.push(model);
                    }
                }
                merged
            }
            Ok(_) => {
                eprintln!("Provider returned an empty model list; using built-in fallback.");
                fallback
            }
            Err(error)
                if !provider_spec(&provider).is_some_and(|spec| spec.remote_catalog)
                    && (error.contains("401") || error.contains("403")) =>
            {
                return Err(format!(
                    "{provider} rejected the credential while listing models: {error}"
                ));
            }
            Err(error) => {
                eprintln!(
                    "Could not fetch models from {provider}: {error}\nUsing built-in fallback models."
                );
                fallback
            }
        }
    } else {
        fallback
    };
    let model_ids = models.into_iter().map(|model| model.id).collect::<Vec<_>>();
    if model_ids.is_empty() {
        return Err(format!("no models available for {provider}"));
    }
    println!("Choose a model:");
    for (index, model) in model_ids.iter().enumerate() {
        println!("  {}) {model}", index + 1);
    }
    let model = choose_item("Model: ", &model_ids, "model")?;
    let selection = format!("{provider}/{model}");
    save_settings(
        home,
        &CliSettings {
            default_model: selection.clone(),
        },
    )?;
    Ok(selection)
}

fn choose_item(prompt: &str, values: &[String], kind: &str) -> Result<String, String> {
    let answer = read_line(prompt).map_err(|e| e.to_string())?;
    resolve_item(&answer, values, kind)
}

fn resolve_item(answer: &str, values: &[String], kind: &str) -> Result<String, String> {
    if let Some(value) = values.iter().find(|value| value.as_str() == answer) {
        return Ok(value.clone());
    }
    let index: usize = answer
        .parse()
        .map_err(|_| format!("invalid {kind} selection: enter its number or exact name"))?;
    values
        .get(index.saturating_sub(1))
        .cloned()
        .ok_or_else(|| format!("invalid {kind} selection"))
}

async fn configure_provider_auth(
    home: &std::path::Path,
    provider: &str,
    force_replace: bool,
) -> Result<(), String> {
    let store = CredentialStore::open(home).map_err(|error| error.to_string())?;
    let configured = store.get(provider).is_some() || provider_env_configured(provider);
    let supports_oauth = oauth_allowed(provider);
    let supports_api_key = api_key_login_allowed(provider);

    if configured && !force_replace {
        let prompt = match (supports_api_key, supports_oauth) {
            (true, true) => {
                "Credential already configured: 1) Use existing  2) Replace API key  3) OAuth [1]: "
            }
            (true, false) => {
                "Credential already configured: 1) Use existing  2) Replace API key [1]: "
            }
            (false, true) => {
                "Credential already configured: 1) Use existing  2) Re-login with OAuth [1]: "
            }
            (false, false) => return Ok(()),
        };
        match read_line(prompt).map_err(|error| error.to_string())?.trim() {
            "" | "1" => return Ok(()),
            "2" if supports_api_key => return save_interactive_api_key(home, provider),
            "2" if supports_oauth => return save_interactive_oauth(home, provider).await,
            "3" if supports_oauth => return save_interactive_oauth(home, provider).await,
            _ => return Err("invalid authentication selection".into()),
        }
    }

    match (supports_api_key, supports_oauth) {
        (true, true) => {
            let method = read_line("Authentication: 1) OAuth  2) API key [1]: ")
                .map_err(|error| error.to_string())?;
            if method.trim().is_empty() || method.trim() == "1" {
                save_interactive_oauth(home, provider).await
            } else if method.trim() == "2" {
                save_interactive_api_key(home, provider)
            } else {
                Err("invalid authentication selection".into())
            }
        }
        (true, false) => save_interactive_api_key(home, provider),
        (false, true) => save_interactive_oauth(home, provider).await,
        (false, false) => Err(format!(
            "no interactive authentication method for {provider}"
        )),
    }
}

async fn save_interactive_oauth(home: &std::path::Path, provider: &str) -> Result<(), String> {
    let tokens = login_oauth(provider, &ConsoleAuthInteraction, &reqwest::Client::new()).await?;
    let mut store = CredentialStore::open(home).map_err(|error| error.to_string())?;
    store_oauth(
        &mut store,
        provider,
        &tokens.access,
        &tokens.refresh,
        tokens.expires,
    )
    .map_err(|error| error.to_string())
}

fn should_discover_provider_models(provider: &str) -> bool {
    provider == "sensenova" || provider_spec(provider).is_none_or(|spec| spec.remote_catalog)
}

async fn discover_provider_models(
    home: &std::path::Path,
    provider: &str,
) -> Result<Vec<CustomModel>, String> {
    if let Some(spec) = provider_spec(provider).filter(|spec| spec.remote_catalog) {
        let catalog_base =
            std::env::var("LATO_CATALOG_BASE_URL").unwrap_or_else(|_| "https://pi.dev".into());
        return refresh_remote_provider_catalog(
            spec,
            &ProviderModelsStore::open(home),
            &catalog_base,
            false,
        )
        .await;
    }

    // Providers outside the translated reference registry retain explicit vendor discovery.
    let seed = CATALOG
        .iter()
        .find(|model| {
            model.provider == provider
                && matches!(
                    model.api,
                    ModelApi::OpenaiCompletions | ModelApi::OpenaiResponses
                )
        })
        .ok_or("provider has no dynamic model source")?;
    let base_url = seed.base_url.ok_or("provider has no model-list base URL")?;
    let mut credentials = CredentialStore::open(home).map_err(|error| error.to_string())?;
    let auth = get_auth_refreshing(
        &mut credentials,
        provider,
        &|name| std::env::var(name).ok(),
        None,
        &reqwest::Client::new(),
    )
    .await?
    .ok_or_else(|| format!("no credential configured for {provider}"))?;
    let env_name = lato_ai::env_names(provider)
        .first()
        .copied()
        .unwrap_or("LATO_API_KEY");
    refresh_openai_compatible_models(
        provider,
        seed.api,
        base_url,
        env_name,
        auth.api_key.as_deref(),
    )
    .await
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
        let raw: Arc<dyn ModelStream> = Arc::new(HttpModelStream::new(model, auth));
        adapt_model_stream(provider, model_id, raw).map_err(|error| error.to_string())
    } else {
        let home = lato_home();
        if let Some(custom) = load_models_json(&home.join("models.json"))
            .unwrap_or_default()
            .into_iter()
            .find(|model| model.provider == provider && model.id == model_id)
        {
            let auth = custom_model_auth(&custom, &|name| std::env::var(name).ok())
                .ok_or_else(|| format!("environment variable {} is not configured", custom.env))?;
            let raw: Arc<dyn ModelStream> = Arc::new(CustomHttpModelStream::new(custom, auth));
            return adapt_model_stream(provider, model_id, raw).map_err(|error| error.to_string());
        }
        let cached = ProviderModelsStore::open(&home)
            .read(provider)?
            .into_iter()
            .flat_map(|entry| entry.models)
            .chain(load_models_json(&home.join("model-cache.json")).unwrap_or_default())
            .find(|model| model.provider == provider && model.id == model_id)
            .ok_or_else(|| {
                format!("unknown model {selection}; run /model to refresh provider models")
            })?;
        let mut store = CredentialStore::open(&home).map_err(|e| e.to_string())?;
        let auth = get_auth_refreshing(
            &mut store,
            provider,
            &|name| std::env::var(name).ok(),
            None,
            &reqwest::Client::new(),
        )
        .await?
        .ok_or_else(|| format!("no credential configured for {provider}"))?;
        let raw: Arc<dyn ModelStream> = Arc::new(CustomHttpModelStream::new(cached, auth));
        adapt_model_stream(provider, model_id, raw).map_err(|error| error.to_string())
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

fn is_exit_command(command: &str) -> bool {
    matches!(
        command.to_ascii_lowercase().as_str(),
        "exit" | "quit" | "/exit" | "/quit"
    )
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

#[cfg(test)]
mod tests {
    use super::{is_exit_command, resolve_item, should_discover_provider_models};

    #[test]
    fn interactive_exit_commands_accept_plain_slash_and_case_variants() {
        for command in ["exit", "quit", "/exit", "/quit", "EXIT"] {
            assert!(is_exit_command(command));
        }
        assert!(!is_exit_command("please exit after the task"));
    }

    #[test]
    fn sensenova_uses_its_vendor_model_discovery_endpoint() {
        assert!(should_discover_provider_models("sensenova"));
        assert!(should_discover_provider_models("minimax-cn"));
    }

    #[test]
    fn model_choice_accepts_number_or_exact_model_id() {
        let models = vec!["sensenova-6.8-flash-lite".to_string()];
        assert_eq!(resolve_item("1", &models, "model").unwrap(), models[0]);
        assert_eq!(
            resolve_item("sensenova-6.8-flash-lite", &models, "model").unwrap(),
            models[0]
        );
    }
}
