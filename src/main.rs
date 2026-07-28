#![warn(unreachable_pub)]

mod agent;
mod audio;
mod auth;
mod completion;
mod config;
mod events;
mod http_debug;
mod provider;
mod server;
mod session;
mod skills;
mod tools;
mod transcription;
mod ui;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::{
    agent::Agent,
    auth::OAuthStore,
    config::{Config, SessionRegistry},
    provider::OpenAiCompatible,
    session::{SessionStore, list_session_projects, resolve_global_session},
    skills::SkillRegistry,
    tools::ToolBox,
};

#[derive(Parser)]
#[command(
    name = "codecrab",
    version,
    about = "A small, auditable coding agent for your terminal"
)]
struct Cli {
    /// Project directory the agent may access.
    #[arg(short = 'C', long, global = true, default_value = ".")]
    cwd: PathBuf,

    /// Model name, overriding config and CODECRAB_MODEL.
    #[arg(short, long, global = true)]
    model: Option<String>,

    /// OpenAI-compatible API base URL.
    #[arg(long, global = true)]
    base_url: Option<String>,

    /// Print complete OpenAI HTTP requests and responses to stderr, including credentials.
    #[arg(long, global = true)]
    debug_openai: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Authenticate with a ChatGPT subscription or inspect auth state.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Run one prompt, print the answer, and exit.
    Run {
        /// Prompt. If omitted, it is read from stdin.
        prompt: Option<String>,
    },
    /// Resume an existing interactive session.
    Resume {
        /// Session id or an unambiguous prefix. Omit for the latest session.
        id: Option<String>,
    },
    /// List saved sessions across every registered project.
    Sessions,
    /// List available project and user skills.
    Skills,
    /// Print the effective configuration (secrets are omitted).
    Config,
    /// Serve the embedded web application and agent API.
    Serve {
        /// Interface to listen on.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// TCP port. Use 0 to select an available port.
        #[arg(long, default_value_t = 4096)]
        port: u16,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Sign in with ChatGPT in your browser (Plus/Pro subscription).
    Login,
    /// Show the active authentication method without exposing credentials.
    Status,
    /// Remove CodeCrab's saved ChatGPT credentials.
    Logout,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = cli
        .cwd
        .canonicalize()
        .with_context(|| format!("cannot open project {}", cli.cwd.display()))?;

    let mut config = Config::load()?;
    config.apply_cli(cli.model, cli.base_url);

    if let Some(Command::Auth { command }) = &cli.command {
        let mut auth = OAuthStore::new()?;
        auth.set_debug_openai(cli.debug_openai);
        match command {
            AuthCommand::Login => {
                let identity = auth.login().await?;
                println!(
                    "Signed in with ChatGPT{}{}.",
                    identity
                        .email
                        .as_deref()
                        .map(|email| format!(" as {email}"))
                        .unwrap_or_default(),
                    identity
                        .plan
                        .as_deref()
                        .map(|plan| format!(" ({plan})"))
                        .unwrap_or_default()
                );
            }
            AuthCommand::Status => match auth.status()? {
                Some(status) => println!(
                    "ChatGPT OAuth: signed in{}{}",
                    status
                        .email
                        .as_deref()
                        .map(|email| format!(" as {email}"))
                        .unwrap_or_default(),
                    status
                        .plan
                        .as_deref()
                        .map(|plan| format!(" ({plan})"))
                        .unwrap_or_default()
                ),
                None => println!(
                    "ChatGPT OAuth: not signed in\nRun `codecrab auth login` to use your subscription."
                ),
            },
            AuthCommand::Logout => {
                auth.logout()?;
                println!("CodeCrab's ChatGPT credentials were removed.");
            }
        }
        return Ok(());
    }

    let store = SessionStore::new(&root)?;
    let skills = SkillRegistry::discover(&root);
    let registry = SessionRegistry::global();

    match cli.command {
        Some(Command::Sessions) => {
            ui::print_sessions(&list_session_projects(&root, &registry)?);
            return Ok(());
        }
        Some(Command::Skills) => {
            skills.print();
            return Ok(());
        }
        Some(Command::Config) => {
            let contents = toml::to_string_pretty(&config.public_view())?;
            println!(
                "{}",
                format_config_output(Config::file_path().as_deref(), &contents)
            );
            return Ok(());
        }
        Some(Command::Serve { host, port }) => {
            server::serve(root, config, host, port, cli.debug_openai).await?;
            return Ok(());
        }
        Some(Command::Auth { .. }) => unreachable!("auth commands return before session setup"),
        Some(Command::Run { prompt }) => {
            let prompt = one_shot_prompt(prompt)?;
            if prompt.trim().is_empty() {
                anyhow::bail!("prompt is empty");
            }
            let provider = new_provider(&config, cli.debug_openai)?;
            let tools = ToolBox::new(root.clone());
            let mut agent =
                Agent::new(provider, tools, skills, store.create(config.model.clone())?)?;
            match agent.fetch_models().await {
                Ok(catalog) => {
                    agent.resolve_auto_model(&catalog);
                }
                Err(error) if agent.session().model == "auto" => {
                    return Err(error).context("cannot resolve the automatic model");
                }
                Err(_) => {}
            }
            let answer = agent.turn(prompt.trim()).await?;
            println!("{answer}");
            store.save(agent.session())?;
            registry.register(&root)?;
            return Ok(());
        }
        Some(Command::Resume { id }) => {
            let projects = list_session_projects(&root, &registry)?;
            let (session_root, session_id) = resolve_global_session(&projects, id.as_deref())?;
            let session_store = SessionStore::new(&session_root)?;
            let session = session_store.load(Some(&session_id.to_string()))?;
            let provider = new_provider(&config, cli.debug_openai)?;
            let tools = ToolBox::new(session_root.clone());
            let agent = Agent::new(
                provider,
                tools,
                SkillRegistry::discover(&session_root),
                session,
            )?;
            ui::interactive(agent, &registry, cli.debug_openai).await?;
        }
        None => {
            let provider = new_provider(&config, cli.debug_openai)?;
            let tools = ToolBox::new(root);
            let session = store.create(config.model.clone())?;
            let agent = Agent::new(provider, tools, skills, session)?;
            ui::interactive(agent, &registry, cli.debug_openai).await?;
        }
    }

    Ok(())
}

fn new_provider(config: &Config, debug_openai: bool) -> Result<OpenAiCompatible> {
    let mut provider = OpenAiCompatible::new(config)?;
    provider.set_debug_openai(debug_openai);
    Ok(provider)
}

fn one_shot_prompt(prompt: Option<String>) -> Result<String> {
    use std::io::{IsTerminal, Read};

    let stdin_is_terminal = std::io::stdin().is_terminal();
    if prompt.is_none() && stdin_is_terminal {
        anyhow::bail!("provide a prompt or pipe input to `codecrab run`");
    }

    let mut piped = String::new();
    if !stdin_is_terminal {
        std::io::stdin().read_to_string(&mut piped)?;
    }

    Ok(match (prompt, piped.trim()) {
        (Some(prompt), "") => prompt,
        (Some(prompt), input) => {
            format!("{prompt}\n\nThe following content was provided on stdin:\n\n{input}")
        }
        (None, input) => input.to_owned(),
    })
}

fn format_config_output(path: Option<&Path>, contents: &str) -> String {
    let path = path
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(platform configuration path unavailable)".into());
    format!(
        "Configuration file path:\n{path}\n\nEffective configuration content:\n{}",
        contents.trim_end()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_output_separates_the_file_path_from_effective_content() {
        let output = format_config_output(
            Some(Path::new("/config/codecrab/config.toml")),
            "model = \"auto\"\n",
        );

        assert!(output.starts_with("Configuration file path:\n/config/codecrab/config.toml\n\n"));
        assert!(output.contains("Effective configuration content:\nmodel = \"auto\""));
    }
}
