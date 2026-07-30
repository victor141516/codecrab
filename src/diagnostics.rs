use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use chrono::Local;
use uuid::Uuid;

#[derive(Clone, Default)]
pub(crate) struct DebugOutput {
    destination: DebugDestination,
}

#[derive(Clone, Default)]
enum DebugDestination {
    #[default]
    Disabled,
    Stderr,
    File(Arc<Mutex<LazyDebugFile>>),
}

struct LazyDebugFile {
    path: PathBuf,
    file: Option<File>,
}

impl DebugOutput {
    pub(crate) fn stderr() -> Self {
        Self {
            destination: DebugDestination::Stderr,
        }
    }

    pub(crate) fn file(path: PathBuf) -> Self {
        Self {
            destination: DebugDestination::File(Arc::new(Mutex::new(LazyDebugFile {
                path,
                file: None,
            }))),
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        !matches!(self.destination, DebugDestination::Disabled)
    }

    pub(crate) fn write_all(&self, contents: &[u8]) -> Result<()> {
        match &self.destination {
            DebugDestination::Disabled => Ok(()),
            DebugDestination::Stderr => {
                let mut stderr = io::stderr().lock();
                stderr
                    .write_all(contents)
                    .context("cannot write OpenAI debug output to stderr")?;
                stderr
                    .flush()
                    .context("cannot flush OpenAI debug output to stderr")
            }
            DebugDestination::File(state) => {
                let mut state = state.lock().expect("debug output mutex poisoned");
                if state.file.is_none() {
                    state.file = Some(append_file(&state.path).with_context(|| {
                        format!(
                            "cannot open OpenAI debug output file {}",
                            state.path.display()
                        )
                    })?);
                }
                let path = state.path.clone();
                let file = state.file.as_mut().expect("debug file was initialized");
                file.write_all(contents).with_context(|| {
                    format!("cannot write OpenAI debug output to {}", path.display())
                })?;
                file.flush().with_context(|| {
                    format!("cannot flush OpenAI debug output to {}", path.display())
                })
            }
        }
    }
}

impl From<bool> for DebugOutput {
    fn from(enabled: bool) -> Self {
        if enabled {
            Self::stderr()
        } else {
            Self::default()
        }
    }
}

#[derive(Clone)]
pub(crate) struct DiagnosticLog {
    destination: DiagnosticDestination,
}

#[derive(Clone)]
enum DiagnosticDestination {
    Stderr,
    Tui(Arc<Mutex<TuiLogState>>),
}

struct TuiLogState {
    path: PathBuf,
    custom_path: bool,
    file: Option<File>,
    wrote: bool,
    failure: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DiagnosticReport {
    pub(crate) path: Option<PathBuf>,
    pub(crate) failure: Option<String>,
}

impl Default for DiagnosticLog {
    fn default() -> Self {
        Self::stderr()
    }
}

impl DiagnosticLog {
    pub(crate) fn stderr() -> Self {
        Self {
            destination: DiagnosticDestination::Stderr,
        }
    }

    pub(crate) fn tui(path: Option<PathBuf>) -> Self {
        Self::tui_in(path, std::env::temp_dir())
    }

    fn tui_in(path: Option<PathBuf>, temp_dir: PathBuf) -> Self {
        let custom_path = path.is_some();
        let path = path.unwrap_or_else(|| {
            temp_dir.join(format!(
                "codecrab-log-{}-{}.log",
                Local::now().format("%Y-%m-%d-%H-%M-%S"),
                Uuid::new_v4()
            ))
        });
        Self {
            destination: DiagnosticDestination::Tui(Arc::new(Mutex::new(TuiLogState {
                path,
                custom_path,
                file: None,
                wrote: false,
                failure: None,
            }))),
        }
    }

    pub(crate) fn error(&self, message: impl AsRef<str>) {
        match &self.destination {
            DiagnosticDestination::Stderr => eprintln!("{}", message.as_ref()),
            DiagnosticDestination::Tui(state) => {
                let mut state = state.lock().expect("diagnostic log mutex poisoned");
                if state.failure.is_some() {
                    return;
                }
                if state.file.is_none() {
                    let opened = if state.custom_path {
                        append_file(&state.path)
                    } else {
                        create_new_file(&state.path)
                    };
                    match opened {
                        Ok(file) => state.file = Some(file),
                        Err(error) => {
                            state.failure = Some(format!(
                                "cannot open error log {}: {error}",
                                state.path.display()
                            ));
                            return;
                        }
                    }
                }
                let path = state.path.clone();
                let file = state
                    .file
                    .as_mut()
                    .expect("diagnostic file was initialized");
                if let Err(error) =
                    writeln!(file, "{}", message.as_ref()).and_then(|()| file.flush())
                {
                    state.failure = Some(format!(
                        "cannot write error log {}: {error}",
                        path.display()
                    ));
                    return;
                }
                state.wrote = true;
            }
        }
    }

    pub(crate) fn warning(&self, message: impl AsRef<str>) {
        if matches!(self.destination, DiagnosticDestination::Stderr) {
            eprintln!("{}", message.as_ref());
        }
    }

    pub(crate) fn report(&self) -> DiagnosticReport {
        match &self.destination {
            DiagnosticDestination::Stderr => DiagnosticReport::default(),
            DiagnosticDestination::Tui(state) => {
                let state = state.lock().expect("diagnostic log mutex poisoned");
                DiagnosticReport {
                    path: state.wrote.then(|| state.path.clone()),
                    failure: state.failure.clone(),
                }
            }
        }
    }
}

fn append_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    restrict_permissions(&mut options);
    options.open(path)
}

fn create_new_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    restrict_permissions(&mut options);
    options.open(path)
}

#[cfg(unix)]
fn restrict_permissions(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn restrict_permissions(_options: &mut OpenOptions) {}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn tui_log_is_not_created_until_an_error_is_written() {
        let temp = tempdir().unwrap();
        let log = DiagnosticLog::tui_in(None, temp.path().to_path_buf());

        assert_eq!(log.report(), DiagnosticReport::default());
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn tui_log_uses_a_unique_timestamped_default_name() {
        let temp = tempdir().unwrap();
        let log = DiagnosticLog::tui_in(None, temp.path().to_path_buf());

        log.error("request failed");

        let report = log.report();
        let path = report.path.unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("codecrab-log-"));
        assert!(name.ends_with(".log"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "request failed\n");
    }

    #[test]
    fn custom_tui_log_appends_and_ignores_warnings() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("errors.log");
        std::fs::write(&path, "before\n").unwrap();
        let log = DiagnosticLog::tui(Some(path.clone()));

        log.warning("not an error");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before\n");

        log.error("request failed");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "before\nrequest failed\n"
        );
        assert_eq!(log.report().path.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn debug_file_is_lazy_and_appends() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("debug.log");
        let output = DebugOutput::file(path.clone());
        assert!(!path.exists());

        output.write_all(b"first\n").unwrap();
        output.write_all(b"second\n").unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn tui_log_retains_an_open_failure_for_reporting_after_exit() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("missing").join("errors.log");
        let log = DiagnosticLog::tui(Some(path.clone()));

        log.error("request failed");

        let report = log.report();
        assert_eq!(report.path, None);
        assert!(
            report
                .failure
                .unwrap()
                .contains(&path.display().to_string())
        );
    }

    #[test]
    fn debug_file_never_falls_back_when_it_cannot_be_opened() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("missing").join("debug.log");
        let output = DebugOutput::file(path.clone());

        let error = output.write_all(b"secret").unwrap_err();

        assert!(format!("{error:#}").contains(&path.display().to_string()));
        assert!(!path.exists());
    }
}
