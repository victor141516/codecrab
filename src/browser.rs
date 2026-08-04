use std::{
    ffi::OsString,
    io::ErrorKind,
    path::PathBuf,
    process::{Command, Stdio},
};

#[cfg(any(windows, target_os = "macos"))]
use std::env;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum OpenBrowserMode {
    Https,
    Http,
    App,
    AppHttp,
}

impl OpenBrowserMode {
    fn uses_https(self) -> bool {
        matches!(self, Self::Https | Self::App)
    }

    fn uses_app_window(self) -> bool {
        matches!(self, Self::App | Self::AppHttp)
    }
}

pub(crate) fn open(mode: OpenBrowserMode, http_origin: &str, https_origin: &str) -> Result<String> {
    let url = browser_url(mode, http_origin, https_origin);
    if mode.uses_app_window() {
        open_chrome_app(&url)?;
    } else {
        webbrowser::open(&url)
            .with_context(|| format!("cannot open the default browser at {url}"))?;
    }
    Ok(url)
}

fn browser_url(mode: OpenBrowserMode, http_origin: &str, https_origin: &str) -> String {
    format!(
        "{}/",
        if mode.uses_https() {
            https_origin
        } else {
            http_origin
        }
        .trim_end_matches('/')
    )
}

#[derive(Debug, Eq, PartialEq)]
struct ChromeCommand {
    program: PathBuf,
    arguments: Vec<OsString>,
}

fn open_chrome_app(url: &str) -> Result<()> {
    let mut last_error = None;
    for launch in chrome_app_commands(url) {
        if launch.program.components().count() > 1 && !launch.program.is_file() {
            continue;
        }
        let mut command = Command::new(&launch.program);
        command
            .args(&launch.arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        match command.spawn() {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => last_error = Some((launch.program, error)),
        }
    }

    if let Some((program, error)) = last_error {
        return Err(error)
            .with_context(|| format!("cannot launch Google Chrome at {}", program.display()));
    }
    bail!(
        "cannot open CodeCrab in app mode because Google Chrome was not found; install Chrome or use --open-browser without app mode"
    )
}

fn chrome_app_commands(url: &str) -> Vec<ChromeCommand> {
    let argument = OsString::from(format!("--app={url}"));
    chrome_commands()
        .into_iter()
        .map(|mut launch| {
            launch.arguments.push(argument.clone());
            launch
        })
        .collect()
}

#[cfg(windows)]
fn chrome_commands() -> Vec<ChromeCommand> {
    let mut commands = Vec::new();
    for variable in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Some(directory) = env::var_os(variable) {
            commands.push(ChromeCommand {
                program: PathBuf::from(directory)
                    .join("Google")
                    .join("Chrome")
                    .join("Application")
                    .join("chrome.exe"),
                arguments: Vec::new(),
            });
        }
    }
    commands.push(ChromeCommand {
        program: PathBuf::from("chrome.exe"),
        arguments: Vec::new(),
    });
    commands
}

#[cfg(target_os = "macos")]
fn chrome_commands() -> Vec<ChromeCommand> {
    let mut commands = vec![ChromeCommand {
        program: PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        arguments: Vec::new(),
    }];
    if let Some(home) = env::var_os("HOME") {
        commands.push(ChromeCommand {
            program: PathBuf::from(home)
                .join("Applications")
                .join("Google Chrome.app")
                .join("Contents")
                .join("MacOS")
                .join("Google Chrome"),
            arguments: Vec::new(),
        });
    }
    commands
}

#[cfg(all(unix, not(target_os = "macos")))]
fn chrome_commands() -> Vec<ChromeCommand> {
    ["google-chrome", "google-chrome-stable"]
        .into_iter()
        .map(|program| ChromeCommand {
            program: PathBuf::from(program),
            arguments: Vec::new(),
        })
        .chain(std::iter::once(ChromeCommand {
            program: PathBuf::from("flatpak"),
            arguments: vec![OsString::from("run"), OsString::from("com.google.Chrome")],
        }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_modes_select_the_expected_origin_and_window_style() {
        let http = "http://127.0.0.1:4096";
        let https = "https://127.0.0.1:4097";
        for (mode, expected, app) in [
            (OpenBrowserMode::Https, format!("{https}/"), false),
            (OpenBrowserMode::Http, format!("{http}/"), false),
            (OpenBrowserMode::App, format!("{https}/"), true),
            (OpenBrowserMode::AppHttp, format!("{http}/"), true),
        ] {
            assert_eq!(browser_url(mode, http, https), expected);
            assert_eq!(mode.uses_app_window(), app);
        }
    }

    #[test]
    fn chrome_app_candidates_receive_the_url_as_one_flag() {
        let argument = OsString::from("--app=https://127.0.0.1:4097/");
        for launch in chrome_app_commands("https://127.0.0.1:4097/") {
            assert_eq!(launch.arguments.last(), Some(&argument));
        }
    }
}
