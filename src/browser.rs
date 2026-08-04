use std::{
    ffi::OsString,
    io::ErrorKind,
    path::PathBuf,
    process::{Command, Stdio},
};

#[cfg(any(test, all(unix, not(target_os = "macos"))))]
use std::ffi::OsStr;
#[cfg(any(test, target_os = "macos"))]
use std::path::Path;

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

    fn scheme(self) -> &'static str {
        if self.uses_https() { "https" } else { "http" }
    }
}

pub(crate) fn open(mode: OpenBrowserMode, http_origin: &str, https_origin: &str) -> Result<String> {
    let url = browser_url(mode, http_origin, https_origin);
    if mode.uses_app_window() {
        open_default_browser_app(mode.scheme(), &url)?;
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct BrowserCommand {
    program: PathBuf,
    arguments: Vec<OsString>,
}

impl BrowserCommand {
    fn with_app_url(
        program: impl Into<PathBuf>,
        arguments: impl IntoIterator<Item = impl Into<OsString>>,
        url: &str,
    ) -> Self {
        let mut arguments = arguments
            .into_iter()
            .map(Into::into)
            .collect::<Vec<OsString>>();
        arguments.push(OsString::from(format!("--app={url}")));
        Self {
            program: program.into(),
            arguments,
        }
    }
}

fn open_default_browser_app(scheme: &str, url: &str) -> Result<()> {
    let commands = default_browser_commands(scheme, url)
        .with_context(|| app_mode_guidance(scheme, "cannot resolve the default browser"))?;
    let mut last_error = None;

    for launch in commands {
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
            Err(error) if error.kind() == ErrorKind::NotFound => {
                last_error = Some((launch.program, error));
            }
            Err(error) => last_error = Some((launch.program, error)),
        }
    }

    let detail = match last_error {
        Some((program, error)) => format!("cannot launch {}: {error}", program.display()),
        None => "no default-browser launch command was available".to_owned(),
    };
    bail!(app_mode_guidance(scheme, &detail))
}

fn app_mode_guidance(scheme: &str, detail: &str) -> String {
    let fallback = if scheme == "https" {
        "--open-browser"
    } else {
        "--open-browser http"
    };
    format!("{detail}; use {fallback} to open the default browser without requesting an app window")
}

#[cfg(windows)]
fn default_browser_commands(scheme: &str, url: &str) -> Result<Vec<BrowserCommand>> {
    Ok(vec![BrowserCommand::with_app_url(
        windows_default_browser_executable(scheme)?,
        std::iter::empty::<OsString>(),
        url,
    )])
}

#[cfg(windows)]
fn windows_default_browser_executable(scheme: &str) -> Result<PathBuf> {
    use std::ptr;
    use windows_sys::Win32::UI::Shell::{
        ASSOCF_IS_PROTOCOL, ASSOCSTR_EXECUTABLE, AssocQueryStringW,
    };

    let association = scheme
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut length = 0_u32;
    // SAFETY: `association` is NUL-terminated and remains alive for both calls. The first call
    // intentionally supplies no output buffer so Windows reports the required length.
    let result = unsafe {
        AssocQueryStringW(
            ASSOCF_IS_PROTOCOL,
            ASSOCSTR_EXECUTABLE,
            association.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            &mut length,
        )
    };
    if result < 0 || length == 0 {
        bail!(
            "Windows did not expose an executable for the default {scheme} association (HRESULT 0x{:08X})",
            result as u32
        );
    }

    let mut executable = vec![0_u16; length as usize];
    // SAFETY: `executable` has the size requested by the first call and both pointers are valid
    // for the duration of the function call.
    let result = unsafe {
        AssocQueryStringW(
            ASSOCF_IS_PROTOCOL,
            ASSOCSTR_EXECUTABLE,
            association.as_ptr(),
            ptr::null(),
            executable.as_mut_ptr(),
            &mut length,
        )
    };
    if result < 0 {
        bail!(
            "Windows could not resolve the executable for the default {scheme} association (HRESULT 0x{:08X})",
            result as u32
        );
    }

    let end = executable
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(executable.len());
    if end == 0 {
        bail!("Windows returned an empty executable for the default {scheme} association");
    }
    Ok(windows_executable_from_wide(&executable[..end]))
}

#[cfg(windows)]
fn windows_executable_from_wide(executable: &[u16]) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;

    PathBuf::from(OsString::from_wide(executable))
}

#[cfg(target_os = "macos")]
fn default_browser_commands(scheme: &str, url: &str) -> Result<Vec<BrowserCommand>> {
    let script = r#"ObjC.import('AppKit');
function run(argv) {
    const target = $.NSURL.URLWithString(argv[0]);
    const application = $.NSWorkspace.sharedWorkspace.URLForApplicationToOpenURL(target);
    if (!application) throw new Error('no default application');
    return ObjC.unwrap(application.path);
}"#;
    let lookup_url = format!("{scheme}://localhost/");
    let output = Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", script, "--", &lookup_url])
        .output()
        .context("cannot query macOS AppKit for the default browser")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!("macOS AppKit could not resolve the default {scheme} browser: {error}");
    }
    let bundle =
        String::from_utf8(output.stdout).context("macOS returned a non-UTF-8 application path")?;
    let bundle = bundle.trim();
    if bundle.is_empty() {
        bail!("macOS returned an empty application path for the default {scheme} browser");
    }
    Ok(vec![macos_browser_command(Path::new(bundle), url)])
}

#[cfg(any(test, target_os = "macos"))]
fn macos_browser_command(bundle: &Path, url: &str) -> BrowserCommand {
    BrowserCommand::with_app_url(
        "/usr/bin/open",
        [
            OsString::from("-n"),
            OsString::from("-a"),
            bundle.as_os_str().to_owned(),
            OsString::from("--args"),
        ],
        url,
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
fn default_browser_commands(scheme: &str, url: &str) -> Result<Vec<BrowserCommand>> {
    linux::default_browser_commands(scheme, url)
}

#[cfg(any(test, all(unix, not(target_os = "macos"))))]
mod linux {
    #[cfg(all(unix, not(target_os = "macos")))]
    use std::env;
    use std::fs;

    use anyhow::{Context, Result, bail};
    use walkdir::WalkDir;

    use super::*;

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(super) fn default_browser_commands(scheme: &str, url: &str) -> Result<Vec<BrowserCommand>> {
        let mut commands = Vec::new();

        if let Some(browser) = env::var_os("BROWSER") {
            commands.extend(browser_override_commands(&browser, url)?);
        }

        let data_directories = xdg_data_directories();
        for (program, arguments) in [
            ("xdg-settings", vec!["get", "default-web-browser"]),
            (
                "xdg-mime",
                vec!["query", "default", &format!("x-scheme-handler/{scheme}")],
            ),
        ] {
            if let Some(desktop_id) = query_desktop_id(program, &arguments)
                && let Some(command) = command_for_desktop_id(&desktop_id, &data_directories, url)?
                && !commands.contains(&command)
            {
                commands.push(command);
            }
        }

        commands.push(BrowserCommand::with_app_url(
            "x-www-browser",
            std::iter::empty::<OsString>(),
            url,
        ));
        Ok(commands)
    }

    fn browser_override_commands(value: &OsStr, url: &str) -> Result<Vec<BrowserCommand>> {
        let value = value.to_string_lossy();
        let mut commands = Vec::new();
        for candidate in value
            .split(':')
            .filter(|candidate| !candidate.trim().is_empty())
        {
            let tokens = shell_words::split(candidate)
                .with_context(|| format!("cannot parse BROWSER entry {candidate:?}"))?;
            if let Some(command) = command_from_tokens(tokens, true, url)? {
                commands.push(command);
            }
        }
        Ok(commands)
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn query_desktop_id(program: &str, arguments: &[&str]) -> Option<String> {
        let output = Command::new(program).args(arguments).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let desktop_id = String::from_utf8(output.stdout).ok()?;
        let desktop_id = desktop_id.trim();
        (!desktop_id.is_empty()).then(|| desktop_id.to_owned())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn xdg_data_directories() -> Vec<PathBuf> {
        let mut directories = Vec::new();
        if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
            directories.push(PathBuf::from(data_home));
        } else if let Some(home) = env::var_os("HOME") {
            directories.push(PathBuf::from(home).join(".local/share"));
        }

        let system_directories = env::var_os("XDG_DATA_DIRS")
            .unwrap_or_else(|| OsString::from("/usr/local/share:/usr/share"));
        directories.extend(
            env::split_paths(&system_directories)
                .filter(|directory| !directory.as_os_str().is_empty()),
        );
        directories
    }

    fn command_for_desktop_id(
        desktop_id: &str,
        data_directories: &[PathBuf],
        url: &str,
    ) -> Result<Option<BrowserCommand>> {
        let Some(path) = find_desktop_file(desktop_id, data_directories) else {
            return Ok(None);
        };
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("cannot read default-browser entry {}", path.display()))?;
        desktop_exec_command(&contents, url)
            .with_context(|| format!("cannot parse default-browser entry {}", path.display()))
    }

    fn find_desktop_file(desktop_id: &str, data_directories: &[PathBuf]) -> Option<PathBuf> {
        for data_directory in data_directories {
            let applications = data_directory.join("applications");
            let direct = applications.join(desktop_id);
            if direct.is_file() {
                return Some(direct);
            }
            for entry in WalkDir::new(&applications)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_file())
            {
                let Ok(relative) = entry.path().strip_prefix(&applications) else {
                    continue;
                };
                let candidate_id = relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("-");
                if candidate_id == desktop_id {
                    return Some(entry.into_path());
                }
            }
        }
        None
    }

    fn desktop_exec_command(contents: &str, url: &str) -> Result<Option<BrowserCommand>> {
        let mut in_desktop_entry = false;
        for line in contents.lines() {
            let line = line.trim();
            if line.starts_with('[') && line.ends_with(']') {
                in_desktop_entry = line == "[Desktop Entry]";
                continue;
            }
            if in_desktop_entry && let Some(exec) = line.strip_prefix("Exec=") {
                let tokens = shell_words::split(exec).context("invalid quoting in Exec")?;
                return command_from_tokens(tokens, false, url);
            }
        }
        Ok(None)
    }

    fn command_from_tokens(
        tokens: Vec<String>,
        browser_override: bool,
        url: &str,
    ) -> Result<Option<BrowserCommand>> {
        let mut expanded = Vec::new();
        for token in tokens {
            if let Some(token) = remove_field_codes(&token, browser_override)?
                && !token.is_empty()
            {
                expanded.push(OsString::from(token));
            }
        }
        if expanded.is_empty() {
            return Ok(None);
        }
        let program = PathBuf::from(expanded.remove(0));
        Ok(Some(BrowserCommand::with_app_url(program, expanded, url)))
    }

    fn remove_field_codes(token: &str, browser_override: bool) -> Result<Option<String>> {
        let mut output = String::new();
        let mut characters = token.chars();
        while let Some(character) = characters.next() {
            if character != '%' {
                output.push(character);
                continue;
            }
            let Some(code) = characters.next() else {
                bail!("unterminated field code in {token:?}");
            };
            match code {
                '%' => output.push('%'),
                'f' | 'F' | 'u' | 'U' | 'i' | 'c' | 'k' => {}
                's' if browser_override => {}
                _ => bail!("unsupported field code %{code} in {token:?}"),
            }
        }
        Ok((!output.is_empty()).then_some(output))
    }

    #[cfg(test)]
    mod tests {
        use tempfile::tempdir;

        use super::*;

        #[test]
        fn browser_override_preserves_launcher_arguments_and_replaces_placeholder() {
            let commands = browser_override_commands(
                OsStr::new(
                    "flatpak run org.chromium.Chromium %s:brave --profile-directory='Work Profile'",
                ),
                "https://localhost:4097/",
            )
            .unwrap();

            assert_eq!(
                commands,
                vec![
                    BrowserCommand::with_app_url(
                        "flatpak",
                        ["run", "org.chromium.Chromium"],
                        "https://localhost:4097/"
                    ),
                    BrowserCommand::with_app_url(
                        "brave",
                        ["--profile-directory=Work Profile"],
                        "https://localhost:4097/"
                    ),
                ]
            );
        }

        #[test]
        fn desktop_entry_preserves_flatpak_prefix_and_removes_url_field_codes() {
            let command = desktop_exec_command(
                "[Desktop Entry]\nName=Browser\nExec=flatpak run org.example.Browser --ozone=%c %U\n",
                "http://localhost:4096/",
            )
            .unwrap()
            .unwrap();

            assert_eq!(
                command,
                BrowserCommand::with_app_url(
                    "flatpak",
                    ["run", "org.example.Browser", "--ozone="],
                    "http://localhost:4096/"
                )
            );
        }

        #[test]
        fn xdg_lookup_honors_data_directory_precedence_and_nested_desktop_ids() {
            let first = tempdir().unwrap();
            let second = tempdir().unwrap();
            let nested = first.path().join("applications/vendor");
            fs::create_dir_all(&nested).unwrap();
            fs::create_dir_all(second.path().join("applications")).unwrap();
            fs::write(
                nested.join("browser.desktop"),
                "[Desktop Entry]\nExec=snap run preferred-browser %u\n",
            )
            .unwrap();
            fs::write(
                second.path().join("applications/vendor-browser.desktop"),
                "[Desktop Entry]\nExec=other-browser %u\n",
            )
            .unwrap();

            let command = command_for_desktop_id(
                "vendor-browser.desktop",
                &[first.path().to_owned(), second.path().to_owned()],
                "https://localhost/",
            )
            .unwrap()
            .unwrap();

            assert_eq!(
                command,
                BrowserCommand::with_app_url(
                    "snap",
                    ["run", "preferred-browser"],
                    "https://localhost/"
                )
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_modes_select_the_expected_origin_scheme_and_window_style() {
        let http = "http://127.0.0.1:4096";
        let https = "https://127.0.0.1:4097";
        for (mode, expected, scheme, app) in [
            (OpenBrowserMode::Https, format!("{https}/"), "https", false),
            (OpenBrowserMode::Http, format!("{http}/"), "http", false),
            (OpenBrowserMode::App, format!("{https}/"), "https", true),
            (OpenBrowserMode::AppHttp, format!("{http}/"), "http", true),
        ] {
            assert_eq!(browser_url(mode, http, https), expected);
            assert_eq!(mode.scheme(), scheme);
            assert_eq!(mode.uses_app_window(), app);
        }
    }

    #[test]
    fn app_url_is_one_process_argument() {
        let launch = BrowserCommand::with_app_url(
            "browser",
            ["--existing-argument"],
            "https://127.0.0.1:4097/",
        );
        assert_eq!(
            launch.arguments,
            vec![
                OsString::from("--existing-argument"),
                OsString::from("--app=https://127.0.0.1:4097/")
            ]
        );
    }

    #[test]
    fn macos_launch_targets_the_resolved_bundle_and_forwards_app_argument() {
        let launch = macos_browser_command(
            Path::new("/Applications/Selected Browser.app"),
            "https://localhost/",
        );
        assert_eq!(launch.program, PathBuf::from("/usr/bin/open"));
        assert_eq!(
            launch.arguments,
            vec![
                OsString::from("-n"),
                OsString::from("-a"),
                OsString::from("/Applications/Selected Browser.app"),
                OsString::from("--args"),
                OsString::from("--app=https://localhost/"),
            ]
        );
    }

    #[test]
    fn error_guidance_matches_the_selected_scheme() {
        assert!(app_mode_guidance("https", "failed").contains("use --open-browser to"));
        assert!(app_mode_guidance("http", "failed").contains("use --open-browser http to"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_association_fixture_becomes_the_selected_executable() {
        let fixture = "C:\\Program Files\\Selected Browser\\browser.exe"
            .encode_utf16()
            .collect::<Vec<_>>();
        assert_eq!(
            windows_executable_from_wide(&fixture),
            PathBuf::from(r"C:\Program Files\Selected Browser\browser.exe")
        );
    }
}
