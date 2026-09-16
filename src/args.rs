use crate::tui::i18n::Language;
use clap::{ArgAction, ArgGroup, CommandFactory, Parser, Subcommand, ValueEnum, error::ErrorKind};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SandboxArg {
    Off,
    Workspace,
    ReadOnly,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptArgs {
    pub text: String,
    pub ask: bool,
    pub sandbox: SandboxArg,
    pub plan: bool,
    pub model: Option<String>,
    pub plugin_dirs: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoginMethod {
    ApiKey(String),
    Oauth { device_auth: bool },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorArgs {
    pub json: bool,
    pub strict: bool,
    pub live: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Invocation {
    InteractiveNew {
        language: Option<Language>,
        sandbox: Option<SandboxArg>,
        plugin_dirs: Vec<PathBuf>,
    },
    Prompt(PromptArgs),
    Sessions(SessionCommand),
    Resume {
        session_id: String,
        language: Option<Language>,
        sandbox: Option<SandboxArg>,
        plan: bool,
        plugin_dirs: Vec<PathBuf>,
    },
    Login {
        provider: String,
        method: LoginMethod,
    },
    Doctor(DoctorArgs),
    Acp {
        plugin_dirs: Vec<PathBuf>,
    },
    Workflow(WorkflowCommand),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionCommand {
    List { json: bool },
    Rename { session_id: String, title: String },
    Delete { session_id: String, yes: bool },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkflowCommand {
    List {
        json: bool,
        plugin_dirs: Vec<PathBuf>,
    },
    Run {
        id: String,
        input: Option<String>,
        plugin_dirs: Vec<PathBuf>,
        model: Option<String>,
        sandbox: Option<SandboxArg>,
        validate_only: bool,
        agent_budget: Option<u64>,
    },
}

#[derive(Debug, Parser)]
#[command(
    name = "lato",
    version,
    about = "Public Beta coding agent",
    after_help = "Run without arguments for the interactive coding CLI.\n\nInteractive: lato [--sandbox off|workspace|read-only]\nResume: lato resume ID|TITLE [--sandbox off|workspace|read-only]\nHeadless: lato -p [--ask] [--plan] [--sandbox off|workspace|read-only] [--model provider/model] TEXT
Plan mode: lato -p --plan TEXT (exit code 3 when a plan was drafted but not approved); lato resume ID|TITLE --plan\nWorkflows: lato workflow list|run ID [--input JSON] [--model provider/model] [--sandbox off|workspace|read-only] [--validate-only] [--agent-budget N] [--plugin-dir PATH]\nDoctor: lato doctor [--json] [--strict] [--live]"
)]
struct Cli {
    /// Interface language for interactive mode
    #[arg(long = "lang", global = true, value_enum)]
    language: Option<Language>,

    /// Run one headless prompt
    #[arg(short = 'p', action = ArgAction::SetTrue)]
    prompt: bool,

    /// Ask before mutating tool calls
    #[arg(long, action = ArgAction::SetTrue)]
    ask: bool,

    /// Sandbox for interactive/resumed sessions or -p; off permits writes outside the workspace
    #[arg(long, global = true, value_enum)]
    sandbox: Option<SandboxArg>,

    /// Run the session in Plan mode (read-only planning; -p and resume only)
    #[arg(long, global = true, action = ArgAction::SetTrue)]
    plan: bool,

    /// Model selection in provider/model form
    #[arg(long)]
    model: Option<String>,

    /// Load a trusted plugin root for this process; may be repeated
    #[arg(
        long = "plugin-dir",
        global = true,
        value_name = "PATH",
        action = ArgAction::Append
    )]
    plugin_dirs: Vec<PathBuf>,

    /// Prompt text; multiple words are joined with spaces
    #[arg(value_name = "TEXT", num_args = 0..)]
    text: Vec<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List locally persisted sessions
    Sessions {
        /// Emit versioned JSON
        #[arg(long, action = ArgAction::SetTrue)]
        json: bool,
        #[command(subcommand)]
        action: Option<SessionsAction>,
    },
    /// Resume a persisted session by exact ID or exact title
    Resume {
        #[arg(value_name = "REFERENCE")]
        session_id: String,
    },
    /// Store a provider credential
    #[command(group(
        ArgGroup::new("method")
            .required(true)
            .multiple(false)
            .args(["api_key", "oauth"])
    ))]
    Login {
        provider: String,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long, action = ArgAction::SetTrue)]
        oauth: bool,
        #[arg(long, action = ArgAction::SetTrue, requires = "oauth")]
        device_auth: bool,
    },
    /// Diagnose local configuration and runtime readiness
    Doctor {
        #[arg(long, action = ArgAction::SetTrue)]
        json: bool,
        #[arg(long, action = ArgAction::SetTrue)]
        strict: bool,
        #[arg(long, action = ArgAction::SetTrue)]
        live: bool,
    },
    /// Serve the Agent Client Protocol over stdio
    Acp,
    /// List or run plugin workflow descriptors
    Workflow {
        #[command(subcommand)]
        action: WorkflowAction,
    },
}

#[derive(Debug, Subcommand)]
enum WorkflowAction {
    /// List materialized workflows from trusted plugins
    List {
        #[arg(long, action = ArgAction::SetTrue)]
        json: bool,
    },
    /// Run a workflow id (`plugin/name`) through the Rhai host
    Run {
        id: String,
        /// JSON object passed as workflow input
        #[arg(long)]
        input: Option<String>,
        /// Model selection in provider/model form; same semantics as -p
        #[arg(long)]
        model: Option<String>,
        /// Validate the script with a canned host; do not start agents
        #[arg(long, action = ArgAction::SetTrue)]
        validate_only: bool,
        /// Override the descriptor agent-budget (1..=1024)
        #[arg(long, value_name = "N")]
        agent_budget: Option<u64>,
    },
}

#[derive(Debug, Subcommand)]
enum SessionsAction {
    /// Assign a manual title to a persisted session
    Rename {
        session_id: String,
        #[arg(value_name = "TITLE", num_args = 1..)]
        title: Vec<String>,
    },
    /// Permanently delete a persisted session
    Delete {
        session_id: String,
        /// Skip the interactive confirmation
        #[arg(long, action = ArgAction::SetTrue)]
        yes: bool,
    },
}

impl Cli {
    fn into_invocation(self) -> Result<Invocation, clap::Error> {
        if let Some(command) = self.command {
            if self.prompt || self.ask || self.model.is_some() || !self.text.is_empty() {
                return Err(semantic_error(
                    ErrorKind::ArgumentConflict,
                    "headless prompt options cannot be combined with a subcommand",
                ));
            }
            if self.language.is_some() && !matches!(&command, Command::Resume { .. }) {
                return Err(semantic_error(
                    ErrorKind::ArgumentConflict,
                    "--lang is only available in interactive mode",
                ));
            }
            if self.sandbox.is_some()
                && !matches!(
                    &command,
                    Command::Resume { .. }
                        | Command::Workflow {
                            action: WorkflowAction::Run { .. },
                        }
                )
            {
                return Err(semantic_error(
                    ErrorKind::ArgumentConflict,
                    "--sandbox is only available in interactive mode, resume, -p, or workflow run",
                ));
            }
            if self.plan && !matches!(&command, Command::Resume { .. }) {
                return Err(semantic_error(
                    ErrorKind::ArgumentConflict,
                    "--plan is only available with -p or resume",
                ));
            }
            if !self.plugin_dirs.is_empty()
                && !matches!(
                    &command,
                    Command::Resume { .. } | Command::Acp | Command::Workflow { .. }
                )
            {
                return Err(semantic_error(
                    ErrorKind::ArgumentConflict,
                    "--plugin-dir is only available in interactive mode, resume, -p, acp, or workflow",
                ));
            }
            return Ok(match command {
                Command::Sessions { json, action } => {
                    if json && action.is_some() {
                        return Err(semantic_error(
                            ErrorKind::ArgumentConflict,
                            "--json cannot be combined with a sessions action",
                        ));
                    }
                    Invocation::Sessions(match action {
                        None => SessionCommand::List { json },
                        Some(SessionsAction::Rename { session_id, title }) => {
                            SessionCommand::Rename {
                                session_id,
                                title: title.join(" "),
                            }
                        }
                        Some(SessionsAction::Delete { session_id, yes }) => {
                            SessionCommand::Delete { session_id, yes }
                        }
                    })
                }
                Command::Resume { session_id } => Invocation::Resume {
                    session_id,
                    language: self.language,
                    sandbox: self.sandbox,
                    plan: self.plan,
                    plugin_dirs: self.plugin_dirs,
                },
                Command::Login {
                    provider,
                    api_key,
                    oauth,
                    device_auth,
                } => Invocation::Login {
                    provider,
                    method: match (api_key, oauth) {
                        (Some(key), false) => LoginMethod::ApiKey(key),
                        (None, true) => LoginMethod::Oauth { device_auth },
                        _ => unreachable!("clap validates the login method group"),
                    },
                },
                Command::Doctor { json, strict, live } => {
                    Invocation::Doctor(DoctorArgs { json, strict, live })
                }
                Command::Acp => Invocation::Acp {
                    plugin_dirs: self.plugin_dirs,
                },
                Command::Workflow { action } => Invocation::Workflow(match action {
                    WorkflowAction::List { json } => WorkflowCommand::List {
                        json,
                        plugin_dirs: self.plugin_dirs,
                    },
                    WorkflowAction::Run {
                        id,
                        input,
                        model,
                        validate_only,
                        agent_budget,
                    } => WorkflowCommand::Run {
                        id,
                        input,
                        plugin_dirs: self.plugin_dirs,
                        model,
                        sandbox: self.sandbox,
                        validate_only,
                        agent_budget,
                    },
                }),
            });
        }

        if self.prompt {
            if self.language.is_some() {
                return Err(semantic_error(
                    ErrorKind::ArgumentConflict,
                    "--lang is only available in interactive mode",
                ));
            }
            if self.text.is_empty() {
                return Err(semantic_error(
                    ErrorKind::MissingRequiredArgument,
                    "-p requires prompt text",
                ));
            }
            return Ok(Invocation::Prompt(PromptArgs {
                text: self.text.join(" "),
                ask: self.ask,
                sandbox: self.sandbox.unwrap_or(SandboxArg::Off),
                plan: self.plan,
                model: self.model,
                plugin_dirs: self.plugin_dirs,
            }));
        }

        if self.plan {
            return Err(semantic_error(
                ErrorKind::ArgumentConflict,
                "--plan is only available with -p or resume",
            ));
        }

        if self.ask || self.model.is_some() || !self.text.is_empty() {
            return Err(semantic_error(
                ErrorKind::MissingRequiredArgument,
                "headless prompt options and text require -p",
            ));
        }
        Ok(Invocation::InteractiveNew {
            language: self.language,
            sandbox: self.sandbox,
            plugin_dirs: self.plugin_dirs,
        })
    }
}

fn semantic_error(kind: ErrorKind, message: &str) -> clap::Error {
    Cli::command().error(kind, message)
}

pub fn parse(args: Vec<String>) -> Result<Invocation, clap::Error> {
    Cli::try_parse_from(std::iter::once("lato".to_string()).chain(args))?.into_invocation()
}

#[cfg(test)]
mod tests {
    use super::{
        Invocation, LoginMethod, PathBuf, PromptArgs, SandboxArg, SessionCommand, WorkflowCommand,
        parse,
    };
    use crate::tui::i18n::Language;
    use clap::error::ErrorKind;

    #[test]
    fn parses_existing_headless_ordering() {
        let invocation = parse(vec![
            "-p".into(),
            "--model".into(),
            "openai/gpt-4.1".into(),
            "--sandbox".into(),
            "workspace".into(),
            "hello".into(),
            "world".into(),
        ])
        .unwrap();
        let Invocation::Prompt(prompt) = invocation else {
            panic!("expected prompt");
        };
        assert_eq!(prompt.text, "hello world");
        assert_eq!(prompt.sandbox, SandboxArg::Workspace);
        assert_eq!(prompt.model.as_deref(), Some("openai/gpt-4.1"));
    }

    #[test]
    fn parses_workflow_commands() {
        assert_eq!(
            parse(vec!["workflow".into(), "list".into(), "--json".into()]).unwrap(),
            Invocation::Workflow(WorkflowCommand::List {
                json: true,
                plugin_dirs: Vec::new(),
            })
        );
        let Invocation::Workflow(WorkflowCommand::Run { id, input, .. }) = parse(vec![
            "workflow".into(),
            "run".into(),
            "demo/review".into(),
            "--input".into(),
            "{\"n\":1}".into(),
        ])
        .unwrap() else {
            panic!("expected workflow run");
        };
        assert_eq!(id, "demo/review");
        assert_eq!(input.as_deref(), Some("{\"n\":1}"));
    }

    #[test]
    fn parses_workflow_run_host_flags() {
        assert_eq!(
            parse(vec![
                "workflow".into(),
                "run".into(),
                "demo/review".into(),
                "--model".into(),
                "openai/gpt-4.1".into(),
                "--sandbox".into(),
                "workspace".into(),
                "--validate-only".into(),
                "--agent-budget".into(),
                "32".into(),
            ])
            .unwrap(),
            Invocation::Workflow(WorkflowCommand::Run {
                id: "demo/review".into(),
                input: None,
                plugin_dirs: Vec::new(),
                model: Some("openai/gpt-4.1".into()),
                sandbox: Some(SandboxArg::Workspace),
                validate_only: true,
                agent_budget: Some(32),
            })
        );
    }

    #[test]
    fn parses_session_management_commands() {
        assert_eq!(
            parse(vec![
                "sessions".into(),
                "rename".into(),
                "s1".into(),
                "New".into(),
                "title".into(),
            ])
            .unwrap(),
            Invocation::Sessions(SessionCommand::Rename {
                session_id: "s1".into(),
                title: "New title".into(),
            })
        );
        assert_eq!(
            parse(vec![
                "sessions".into(),
                "delete".into(),
                "s1".into(),
                "--yes".into(),
            ])
            .unwrap(),
            Invocation::Sessions(SessionCommand::Delete {
                session_id: "s1".into(),
                yes: true,
            })
        );
    }

    #[test]
    fn rejects_prompt_options_without_prompt_mode() {
        let error = parse(vec!["--ask".into()]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn rejects_empty_prompt() {
        let error = parse(vec!["-p".into()]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn parses_sandbox_for_interactive_and_resume_in_either_order() {
        for (name, profile) in [
            ("off", SandboxArg::Off),
            ("workspace", SandboxArg::Workspace),
            ("read-only", SandboxArg::ReadOnly),
        ] {
            assert_eq!(
                parse(vec!["--sandbox".into(), name.into()]).unwrap(),
                Invocation::InteractiveNew {
                    language: None,
                    sandbox: Some(profile),
                    plugin_dirs: vec![],
                }
            );
            for args in [
                vec!["--sandbox", name, "resume", "session-1"],
                vec!["resume", "session-1", "--sandbox", name],
            ] {
                assert_eq!(
                    parse(args.into_iter().map(String::from).collect()).unwrap(),
                    Invocation::Resume {
                        session_id: "session-1".into(),
                        language: None,
                        sandbox: Some(profile),
                        plan: false,
                        plugin_dirs: vec![],
                    }
                );
            }
        }
        assert_eq!(
            parse(vec![]).unwrap(),
            Invocation::InteractiveNew {
                language: None,
                sandbox: None,
                plugin_dirs: vec![],
            }
        );
        assert!(matches!(
            parse(vec!["resume".into(), "session-1".into()]).unwrap(),
            Invocation::Resume { sandbox: None, .. }
        ));
        assert!(matches!(
            parse(vec!["-p".into(), "hello".into()]).unwrap(),
            Invocation::Prompt(super::PromptArgs {
                sandbox: SandboxArg::Off,
                ..
            })
        ));
    }

    #[test]
    fn rejects_sandbox_on_unrelated_subcommands_and_invalid_profiles() {
        for command in [
            vec!["sessions"],
            vec!["doctor"],
            vec!["acp"],
            vec!["login", "openai", "--api-key", "fixture"],
        ] {
            let mut args = command.into_iter().map(String::from).collect::<Vec<_>>();
            args.extend(["--sandbox".into(), "off".into()]);
            assert_eq!(parse(args).unwrap_err().kind(), ErrorKind::ArgumentConflict);
        }
        assert_eq!(
            parse(vec!["--sandbox".into(), "unknown".into()])
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidValue
        );
    }

    #[test]
    fn login_requires_exactly_one_method() {
        let missing = parse(vec!["login".into(), "openai".into()]).unwrap_err();
        assert_eq!(missing.kind(), ErrorKind::MissingRequiredArgument);
        let conflict = parse(vec![
            "login".into(),
            "openai".into(),
            "--api-key".into(),
            "secret".into(),
            "--oauth".into(),
        ])
        .unwrap_err();
        assert_eq!(conflict.kind(), ErrorKind::ArgumentConflict);
        let oauth = parse(vec![
            "login".into(),
            "openai-codex".into(),
            "--oauth".into(),
        ])
        .unwrap();
        assert!(matches!(
            oauth,
            Invocation::Login {
                method: LoginMethod::Oauth { device_auth: false },
                ..
            }
        ));

        let device = parse(vec![
            "login".into(),
            "openai-codex".into(),
            "--oauth".into(),
            "--device-auth".into(),
        ])
        .unwrap();
        assert!(matches!(
            device,
            Invocation::Login {
                method: LoginMethod::Oauth { device_auth: true },
                ..
            }
        ));

        let missing_oauth = parse(vec![
            "login".into(),
            "openai-codex".into(),
            "--device-auth".into(),
        ])
        .unwrap_err();
        assert_eq!(missing_oauth.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn resume_requires_an_id() {
        let error = parse(vec!["resume".into()]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn duplicate_doctor_flags_are_rejected() {
        let error = parse(vec!["doctor".into(), "--json".into(), "--json".into()]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parses_interactive_language_override() {
        assert!(matches!(
            parse(vec!["--lang".into(), "zh-CN".into()]),
            Ok(Invocation::InteractiveNew {
                language: Some(Language::ZhCn),
                ..
            })
        ));
        assert!(matches!(
            parse(vec![
                "resume".into(),
                "session-1".into(),
                "--lang".into(),
                "en".into()
            ]),
            Ok(Invocation::Resume {
                language: Some(Language::En),
                ..
            })
        ));
    }

    #[test]
    fn parses_repeatable_plugin_dirs_for_host_modes() {
        let dirs = vec![PathBuf::from("first"), PathBuf::from("second")];
        assert!(matches!(
            parse(vec![
                "acp".into(),
                "--plugin-dir".into(),
                "first".into(),
                "--plugin-dir".into(),
                "second".into(),
            ]),
            Ok(Invocation::Acp { plugin_dirs }) if plugin_dirs == dirs
        ));
        assert!(matches!(
            parse(vec![
                "--plugin-dir".into(),
                "first".into(),
                "-p".into(),
                "hello".into(),
            ]),
            Ok(Invocation::Prompt(PromptArgs { plugin_dirs, .. }))
                if plugin_dirs == vec![PathBuf::from("first")]
        ));
    }

    #[test]
    fn rejects_language_for_headless_prompt() {
        let error = parse(vec![
            "--lang".into(),
            "en".into(),
            "-p".into(),
            "hello".into(),
        ])
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn rejects_language_for_non_interactive_subcommands() {
        let error = parse(vec!["--lang".into(), "en".into(), "sessions".into()]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
    }
}
