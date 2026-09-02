use clap::{ArgAction, ArgGroup, CommandFactory, Parser, Subcommand, ValueEnum, error::ErrorKind};

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
    pub model: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoginMethod {
    ApiKey(String),
    Oauth,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorArgs {
    pub json: bool,
    pub strict: bool,
    pub live: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Invocation {
    InteractiveNew,
    Prompt(PromptArgs),
    Sessions {
        json: bool,
    },
    Resume {
        session_id: String,
    },
    Login {
        provider: String,
        method: LoginMethod,
    },
    Doctor(DoctorArgs),
    Acp,
}

#[derive(Debug, Parser)]
#[command(
    name = "lato",
    version,
    about = "Public Beta coding agent",
    after_help = "Run without arguments for the interactive coding CLI.\n\nHeadless: lato -p [--ask] [--sandbox off|workspace|read-only] [--model provider/model] TEXT\nDoctor: lato doctor [--json] [--strict] [--live]"
)]
struct Cli {
    /// Run one headless prompt
    #[arg(short = 'p', action = ArgAction::SetTrue)]
    prompt: bool,

    /// Ask before mutating tool calls
    #[arg(long, action = ArgAction::SetTrue)]
    ask: bool,

    /// Shell sandbox profile for a headless prompt
    #[arg(long, value_enum)]
    sandbox: Option<SandboxArg>,

    /// Model selection in provider/model form
    #[arg(long)]
    model: Option<String>,

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
    },
    /// Resume a persisted session in interactive mode
    Resume { session_id: String },
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
}

impl Cli {
    fn into_invocation(self) -> Result<Invocation, clap::Error> {
        if let Some(command) = self.command {
            if self.prompt
                || self.ask
                || self.sandbox.is_some()
                || self.model.is_some()
                || !self.text.is_empty()
            {
                return Err(semantic_error(
                    ErrorKind::ArgumentConflict,
                    "headless prompt options cannot be combined with a subcommand",
                ));
            }
            return Ok(match command {
                Command::Sessions { json } => Invocation::Sessions { json },
                Command::Resume { session_id } => Invocation::Resume { session_id },
                Command::Login {
                    provider,
                    api_key,
                    oauth,
                } => Invocation::Login {
                    provider,
                    method: match (api_key, oauth) {
                        (Some(key), false) => LoginMethod::ApiKey(key),
                        (None, true) => LoginMethod::Oauth,
                        _ => unreachable!("clap validates the login method group"),
                    },
                },
                Command::Doctor { json, strict, live } => {
                    Invocation::Doctor(DoctorArgs { json, strict, live })
                }
                Command::Acp => Invocation::Acp,
            });
        }

        if self.prompt {
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
                model: self.model,
            }));
        }

        if self.ask || self.sandbox.is_some() || self.model.is_some() || !self.text.is_empty() {
            return Err(semantic_error(
                ErrorKind::MissingRequiredArgument,
                "headless prompt options and text require -p",
            ));
        }
        Ok(Invocation::InteractiveNew)
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
    use super::{Invocation, LoginMethod, SandboxArg, parse};
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
                method: LoginMethod::Oauth,
                ..
            }
        ));
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
}
