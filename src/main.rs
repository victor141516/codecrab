#![warn(unreachable_pub)]

mod agent;
mod auth;
mod config;
mod events;
mod provider;
mod session;
mod skills;
mod tools;
mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::{
    agent::Agent,
    auth::OAuthStore,
    config::Config,
    provider::OpenAiCompatible,
    session::SessionStore,
    skills::SkillRegistry,
    tools::{ApprovalMode, ToolBox},
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

    /// Approve all file mutations and shell commands.
    #[arg(short = 'y', long, global = true)]
    yes: bool,

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
    /// List saved sessions for this project.
    Sessions,
    /// List available project and user skills.
    Skills,
    /// Print the effective configuration (secrets are omitted).
    Config,
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
        let auth = OAuthStore::new()?;
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

    match cli.command {
        Some(Command::Sessions) => {
            ui::print_sessions(&store.list()?);
            return Ok(());
        }
        Some(Command::Skills) => {
            skills.print();
            return Ok(());
        }
        Some(Command::Config) => {
            println!("{}", toml::to_string_pretty(&config.public_view())?);
            return Ok(());
        }
        Some(Command::Auth { .. }) => unreachable!("auth commands return before session setup"),
        Some(Command::Run { prompt }) => {
            let prompt = one_shot_prompt(prompt)?;
            if prompt.trim().is_empty() {
                anyhow::bail!("prompt is empty");
            }
            let provider = OpenAiCompatible::new(&config)?;
            let tools = ToolBox::new(
                root.clone(),
                if cli.yes {
                    ApprovalMode::Always
                } else {
                    ApprovalMode::Never
                },
            );
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
            return Ok(());
        }
        Some(Command::Resume { id }) => {
            let session = store.load(id.as_deref())?;
            let provider = OpenAiCompatible::new(&config)?;
            let tools = ToolBox::new(
                root,
                if cli.yes {
                    ApprovalMode::Always
                } else {
                    ApprovalMode::Ask
                },
            );
            let agent = Agent::new(provider, tools, skills, session)?;
            ui::interactive(agent, &store).await?;
        }
        None => {
            let provider = OpenAiCompatible::new(&config)?;
            let tools = ToolBox::new(
                root,
                if cli.yes {
                    ApprovalMode::Always
                } else {
                    ApprovalMode::Ask
                },
            );
            let session = store.create(config.model.clone())?;
            let agent = Agent::new(provider, tools, skills, session)?;
            ui::interactive(agent, &store).await?;
        }
    }

    Ok(())
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
