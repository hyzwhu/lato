use crate::args::{DoctorArgs, Invocation, LoginMethod, PromptArgs, SandboxArg};
use crate::tui::i18n::Language;
use lato::doctor::{self, DoctorDependencies, DoctorOptions, LiveProbe};
use lato_agent::default_fake_stream;
use lato_ai::{
    AuthInteraction, AuthNotice, CATALOG, CredentialStore, CustomHttpModelStream, CustomModel,
    HttpModelStream, ModelApi, ModelStream, OpenAICodexLoginMode, ProviderModelsEntry,
    ProviderModelsStore, RemoteCatalogRefreshPolicy, adapt_model_stream, api_key_login_allowed,
    custom_model_auth, get_auth_refreshing, load_models_json, login_oauth, login_oauth_with_mode,
    lookup_model, oauth_allowed, phase0_supported, provider_spec, refresh_openai_compatible_models,
    refresh_remote_provider_catalog_with_policy, store_oauth,
};
use lato_workspace::{ApprovalMode, SandboxProfile, SessionTrust};
use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(serde::Serialize, serde::Deserialize)]
struct CliSettings {
    default_model: String,
    #[serde(default)]
    language: Option<Language>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LocalFact {
    Model,
    CurrentDirectory,
    AncestorDirectory(usize),
}

fn local_fact_response(input: &str, cwd: &std::path::Path, model: Option<&str>) -> Option<String> {
    let facts = requested_local_facts(input);
    if facts.is_empty() {
        return None;
    }

    let mut lines = Vec::new();
    for fact in facts {
        match fact {
            LocalFact::Model => {
                lines.push(format!("当前模型: {}", model.unwrap_or("built-in/fake")))
            }
            LocalFact::CurrentDirectory => lines.push(format!("当前工作目录: {}", cwd.display())),
            LocalFact::AncestorDirectory(levels) => {
                let mut ancestor = cwd;
                for _ in 0..levels {
                    ancestor = ancestor.parent().unwrap_or(ancestor);
                }
                let label = match levels {
                    1 => "上一层目录".to_string(),
                    2 => "上上层目录".to_string(),
                    _ => format!("上{levels}层目录"),
                };
                lines.push(format!("{label}: {}", ancestor.display()));
            }
        }
    }
    Some(lines.join("\n"))
}

fn requested_local_facts(input: &str) -> Vec<LocalFact> {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower == "pwd" {
        return vec![LocalFact::CurrentDirectory];
    }

    let asks_current_model = [
        "当前模型",
        "现在的模型",
        "你是什么模型",
        "你当前是什么模型",
        "你现在是什么模型",
        "current model",
        "which model are you",
        "what model are you",
    ]
    .iter()
    .any(|term| lower.contains(term));
    let asks_current_directory = [
        "当前目录",
        "当前文件夹",
        "当前工作目录",
        "当前工作区路径",
        "current directory",
        "current folder",
        "working directory",
    ]
    .iter()
    .any(|term| lower.contains(term));
    let asks_grandparent = [
        "上上层目录",
        "上两层目录",
        "祖父目录",
        "grandparent directory",
        "two levels up",
    ]
    .iter()
    .any(|term| lower.contains(term));
    let asks_parent = asks_grandparent
        || [
            "上一层目录",
            "上一级目录",
            "父目录",
            "parent directory",
            "one level up",
        ]
        .iter()
        .any(|term| lower.contains(term));

    let mut facts = Vec::new();
    if asks_current_model {
        facts.push(LocalFact::Model);
    }
    if asks_current_directory {
        facts.push(LocalFact::CurrentDirectory);
    }
    if asks_parent {
        facts.push(LocalFact::AncestorDirectory(if asks_grandparent {
            2
        } else {
            1
        }));
    }
    facts
}

pub async fn run(args: Vec<String>) -> i32 {
    match crate::args::parse(args) {
        Ok(Invocation::InteractiveNew { language }) => {
            interactive(InteractiveStartup::New, language).await
        }
        Ok(Invocation::Prompt(args)) => prompt(args).await,
        Ok(Invocation::Sessions { json }) => crate::sessions::list(json).await,
        Ok(Invocation::Resume {
            session_id,
            language,
        }) => interactive(InteractiveStartup::Resume(session_id), language).await,
        Ok(Invocation::Login { provider, method }) => login(provider, method).await,
        Ok(Invocation::Doctor(args)) => doctor_cmd(args).await,
        Ok(Invocation::Acp) => crate::stdio::run().await,
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            code
        }
    }
}

struct CatalogLiveProbe;

#[async_trait::async_trait]
impl LiveProbe for CatalogLiveProbe {
    async fn probe(&self) -> Result<String, String> {
        let count = CATALOG.len();
        if count == 0 {
            return Err("built-in catalog is empty".into());
        }
        let url = CATALOG
            .iter()
            .find_map(|model| model.base_url)
            .ok_or_else(|| "built-in catalog has no endpoint".to_string())?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(4))
            .build()
            .map_err(|error| error.to_string())?;
        let status = client
            .get(url)
            .send()
            .await
            .map_err(|error| error.to_string())?
            .status();
        Ok(format!(
            "built-in catalog has {count} models; {url} returned {status}"
        ))
    }
}

async fn doctor_cmd(args: DoctorArgs) -> i32 {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let deps = DoctorDependencies {
        home: lato_home(),
        workspace,
        live_probe: Arc::new(CatalogLiveProbe),
    };
    let report = doctor::run(DoctorOptions { live: args.live }, &deps).await;
    if args.json {
        match serde_json::to_string_pretty(&report) {
            Ok(body) => println!("{body}"),
            Err(error) => {
                eprintln!("error: {error}");
                return 1;
            }
        }
    } else {
        println!("{}", doctor::render_human(&report));
    }
    doctor::exit_code(&report, args.strict)
}

async fn prompt(args: PromptArgs) -> i32 {
    if args.ask && !std::io::stdin().is_terminal() {
        eprintln!("error: --ask requires a tty");
        return 2;
    }
    let sandbox = match args.sandbox {
        SandboxArg::Off => SandboxProfile::Off,
        SandboxArg::Workspace => SandboxProfile::Workspace,
        SandboxArg::ReadOnly => SandboxProfile::ReadOnly,
    };
    let model_arg = args.model.or_else(|| std::env::var("LATO_MODEL").ok());
    let text = args.text;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(response) = local_fact_response(&text, &cwd, model_arg.as_deref()) {
        println!("{response}");
        return 0;
    }
    let mut trust = if args.ask {
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

enum InteractiveStartup {
    New,
    Resume(String),
}

async fn interactive(startup: InteractiveStartup, language_override: Option<Language>) -> i32 {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        let message = match startup {
            InteractiveStartup::New => {
                "interactive mode requires a tty; use lato -p TEXT for headless mode"
            }
            InteractiveStartup::Resume(_) => "resume requires a tty",
        };
        eprintln!("error: {message}");
        return 2;
    }
    let home = lato_home();
    if let Err(error) = std::fs::create_dir_all(&home) {
        eprintln!("error: cannot create {}: {error}", home.display());
        return 1;
    }
    let settings = match load_settings(&home) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let tui_test_mode = std::env::var_os("LATO_TUI_TEST").is_some();
    let language = Language::resolve(
        language_override,
        settings.as_ref().and_then(|settings| settings.language),
        Language::system().as_deref(),
    );
    let mut selection = if tui_test_mode {
        "built-in/fake".to_string()
    } else {
        match settings {
            Some(settings) => settings.default_model,
            None => match configure_interactively(&home).await {
                Ok(selection) => selection,
                Err(error) => {
                    eprintln!("error: {error}");
                    return 1;
                }
            },
        }
    };
    if !tui_test_mode && let Err(error) = persist_language(&home, language) {
        eprintln!("error: {error}");
        return 1;
    }
    let stream = if tui_test_mode {
        default_fake_stream()
    } else {
        match configured_stream(&selection).await {
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
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let trust_prompt = match language {
        Language::ZhCn => "是否信任此文件夹并允许本次会话编辑文件/执行命令？[y/N] ",
        Language::En => "Trust this folder and allow edits/commands this session? [y/N] ",
    };
    let trusted = if tui_test_mode {
        true
    } else {
        match read_line(trust_prompt) {
            Ok(answer) => matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"),
            Err(error) => {
                eprintln!("error: {error}");
                return 1;
            }
        }
    };
    let trust = if trusted {
        SessionTrust::for_interactive_auto(&cwd)
    } else {
        SessionTrust::for_interactive(&cwd, false)
    };
    let session_trust = trust.clone();
    let (tui_approval, approvals) = crate::tui::backend::TuiToolApproval::channel();
    let inline_approval = (trust.mode == ApprovalMode::Ask).then_some(tui_approval);
    let client_result = match &startup {
        InteractiveStartup::New => {
            crate::client::InteractiveAcpClient::new_session_with_approval(
                cwd.clone(),
                trust.clone(),
                stream,
                inline_approval.clone(),
            )
            .await
        }
        InteractiveStartup::Resume(session_id) => {
            crate::client::InteractiveAcpClient::resume_session_with_approval(
                cwd.clone(),
                trust.clone(),
                stream,
                inline_approval.clone(),
                session_id.clone(),
            )
            .await
        }
    };
    let client = match client_result {
        Ok(client) => client,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let mut sessions = crate::client::list_sessions_over_acp(cwd.clone())
        .await
        .unwrap_or_default();
    if !sessions.iter().any(|id| id == client.session_id()) {
        sessions.push(client.session_id().to_string());
    }
    sessions.sort_by(|left, right| right.cmp(left));
    match crate::tui::run(crate::tui::InteractiveBootstrap {
        client,
        approvals,
        trust: session_trust,
        language,
        workspace: cwd,
        model: selection.clone(),
        home: home.clone(),
        sessions,
        resumed: matches!(startup, InteractiveStartup::Resume(_)),
    })
    .await
    {
        Ok(crate::tui::TuiExit::Quit) => 0,
        Ok(crate::tui::TuiExit::SwitchModel) => match configure_interactively(&home).await {
            Ok(_) => Box::pin(interactive(InteractiveStartup::New, Some(language))).await,
            Err(error) => {
                eprintln!("error: {error}");
                1
            }
        },
        Ok(crate::tui::TuiExit::Login) => {
            let Some((provider, _)) = selection.split_once('/') else {
                eprintln!("error: model must be provider/model");
                return 1;
            };
            match configure_provider_auth(&home, provider, true).await {
                Ok(()) => Box::pin(interactive(InteractiveStartup::New, Some(language))).await,
                Err(error) => {
                    eprintln!("error: {error}");
                    1
                }
            }
        }
        Ok(crate::tui::TuiExit::Resume(session_id)) => {
            Box::pin(interactive(
                InteractiveStartup::Resume(session_id),
                Some(language),
            ))
            .await
        }
        Err(error) => {
            eprintln!("error: {error}");
            1
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
        let discovered = discover_provider_models(home, &provider).await;
        if requires_authoritative_remote_models(&provider) {
            resolve_authoritative_models(&provider, discovered)?
        } else {
            match discovered {
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
            language: None,
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
        tokens.account_id.as_deref(),
    )
    .map_err(|error| error.to_string())
}

fn should_discover_provider_models(provider: &str) -> bool {
    provider == "sensenova" || provider_spec(provider).is_none_or(|spec| spec.remote_catalog)
}

fn requires_authoritative_remote_models(provider: &str) -> bool {
    provider == "minimax-cn"
}

fn resolve_authoritative_models(
    provider: &str,
    discovered: Result<Vec<CustomModel>, String>,
) -> Result<Vec<CustomModel>, String> {
    match discovered {
        Ok(models) if !models.is_empty() => {
            println!("Fetched {} catalog models for {provider}.", models.len());
            Ok(models)
        }
        Ok(_) => Err(format!(
            "authoritative model catalog for {provider} returned no models; model selection stopped"
        )),
        Err(error) => Err(format!(
            "could not refresh authoritative model catalog for {provider}: {error}; model selection stopped"
        )),
    }
}

async fn discover_provider_models(
    home: &std::path::Path,
    provider: &str,
) -> Result<Vec<CustomModel>, String> {
    if let Some(spec) = provider_spec(provider).filter(|spec| spec.remote_catalog) {
        let catalog_base =
            std::env::var("LATO_CATALOG_BASE_URL").unwrap_or_else(|_| "https://pi.dev".into());
        let policy = if requires_authoritative_remote_models(provider) {
            RemoteCatalogRefreshPolicy::Authoritative { attempts: 3 }
        } else {
            RemoteCatalogRefreshPolicy::Cached
        };
        return refresh_remote_provider_catalog_with_policy(
            spec,
            &ProviderModelsStore::open(home),
            &catalog_base,
            false,
            policy,
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
    let models = refresh_openai_compatible_models(
        provider,
        seed.api,
        base_url,
        env_name,
        auth.api_key.as_deref(),
    )
    .await?;
    ProviderModelsStore::open(home).write(
        provider,
        ProviderModelsEntry {
            models: models.clone(),
            checked_at: now_ms(),
            last_modified: 0,
            etag: None,
        },
    )?;
    Ok(models)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
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

pub(crate) fn persist_language(home: &std::path::Path, language: Language) -> Result<(), String> {
    let mut settings = load_settings(home)?
        .ok_or_else(|| "cannot persist language before model configuration".to_string())?;
    settings.language = Some(language);
    save_settings(home, &settings)
}

#[cfg(test)]
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

async fn login(provider: String, method: LoginMethod) -> i32 {
    let home = lato_home();
    if let LoginMethod::Oauth { device_auth } = method {
        if !oauth_allowed(&provider) {
            eprintln!("error: oauth not supported for {provider}");
            return 1;
        }
        if std::env::var_os("LATO_MOCK_OAUTH").is_some() {
            let mut store = CredentialStore::open(&home).unwrap();
            store_oauth(
                &mut store,
                &provider,
                "mock-access",
                "mock-refresh",
                4_102_444_800_000,
                (provider == "openai-codex").then_some("mock-account"),
            )
            .unwrap();
            println!("oauth logged in {provider}");
            return 0;
        }
        if device_auth && provider != "openai-codex" {
            eprintln!("error: --device-auth is only supported for openai-codex");
            return 1;
        }
        if !device_auth && !std::io::stdin().is_terminal() {
            eprintln!(
                "error: oauth CLI login requires a tty; use lato/auth/login from an ACP client"
            );
            return 2;
        }
        let mode = if device_auth {
            OpenAICodexLoginMode::DeviceCode
        } else {
            OpenAICodexLoginMode::Browser
        };
        match login_oauth_with_mode(
            &provider,
            mode,
            &ConsoleAuthInteraction,
            &reqwest::Client::new(),
        )
        .await
        {
            Ok(tokens) => {
                let mut store = CredentialStore::open(&home).unwrap();
                if let Err(e) = store_oauth(
                    &mut store,
                    &provider,
                    &tokens.access,
                    &tokens.refresh,
                    tokens.expires,
                    tokens.account_id.as_deref(),
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
    if let LoginMethod::ApiKey(key) = method {
        if !api_key_login_allowed(&provider) {
            eprintln!("error: api-key login not supported for {provider}");
            return 1;
        }
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
    unreachable!("clap requires exactly one login method")
}

struct ConsoleAuthInteraction;

#[async_trait::async_trait]
impl AuthInteraction for ConsoleAuthInteraction {
    async fn notify(&self, notice: AuthNotice) {
        match notice {
            AuthNotice::AuthUrl(url) => eprintln!(
                "Open this URL in your browser:\n{url}\nWaiting for the local callback on port 1455."
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

    fn prefers_local_callback(&self) -> bool {
        true
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
    use super::{
        LocalFact, is_exit_command, requested_local_facts, requires_authoritative_remote_models,
        resolve_authoritative_models, resolve_item, should_discover_provider_models,
    };
    use lato_ai::{CustomModel, ModelApi};

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
    fn minimax_cn_requires_authoritative_remote_models() {
        assert!(requires_authoritative_remote_models("minimax-cn"));
        assert!(!requires_authoritative_remote_models("minimax"));
        assert!(!requires_authoritative_remote_models("sensenova"));
    }

    #[test]
    fn minimax_cn_uses_only_successful_remote_models() {
        let remote = CustomModel {
            provider: "minimax-cn".to_string(),
            id: "MiniMax-M3".to_string(),
            api: ModelApi::AnthropicMessages,
            base_url: "https://api.minimaxi.com/anthropic".to_string(),
            env: "MINIMAX_CN_API_KEY".to_string(),
        };
        let models = resolve_authoritative_models("minimax-cn", Ok(vec![remote])).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "MiniMax-M3");
        assert!(
            resolve_authoritative_models("minimax-cn", Ok(Vec::new()))
                .unwrap_err()
                .contains("model selection stopped")
        );
        assert!(
            resolve_authoritative_models("minimax-cn", Err("invalid JSON".to_string()))
                .unwrap_err()
                .contains("invalid JSON")
        );
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

    #[test]
    fn local_fact_matcher_only_intercepts_explicit_current_facts() {
        assert_eq!(
            requested_local_facts("pwd"),
            vec![LocalFact::CurrentDirectory]
        );
        assert_eq!(
            requested_local_facts("你是什么模型"),
            vec![LocalFact::Model]
        );
        assert!(requested_local_facts("which model architecture should I use?").is_empty());
        assert!(requested_local_facts("show me how path handling works").is_empty());
        assert!(requested_local_facts("解释这个 workspace 文件").is_empty());
    }
}
