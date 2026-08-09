use std::{
    collections::{HashMap, VecDeque},
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(not(windows))]
use std::{
    fs::{File, OpenOptions},
    net::{Ipv4Addr, TcpListener},
    process::Stdio,
};
use tokio::process::Child;
#[cfg(not(windows))]
use tokio::process::Command;
#[cfg(not(windows))]
use tokio::{net::TcpStream, sync::Mutex as AsyncMutex, time::sleep};
use uuid::Uuid;

use crate::config::global_data_dir;

pub(crate) const TESTED_CODE_SERVER_VERSION: &str = "4.131.0";
#[cfg(not(windows))]
pub(crate) const INSTALL_MESSAGE: &str = "To see the changes and explore the files, install code-server: https://github.com/coder/code-server#getting-started";
#[cfg(any(not(windows), test))]
const EXTENSION_PACKAGE: &str = include_str!("../code-server-extension/package.json");
#[cfg(any(not(windows), test))]
const EXTENSION_MAIN: &str = include_str!("../code-server-extension/extension.js");
#[cfg(any(not(windows), test))]
const EXTENSION_ID: &str = "codecrab.codecrab-integration";
#[derive(Clone)]
pub(crate) struct CodeServerManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    #[cfg_attr(windows, allow(dead_code))]
    executable: Option<PathBuf>,
    #[cfg_attr(windows, allow(dead_code))]
    profile: PathBuf,
    control_origin: RwLock<String>,
    #[cfg(not(windows))]
    start_gate: AsyncMutex<()>,
    instances: Mutex<HashMap<Uuid, ManagedInstance>>,
    projects: Mutex<HashMap<PathBuf, Uuid>>,
}

struct ManagedInstance {
    token: String,
    project: PathBuf,
    address: SocketAddr,
    child: Child,
    log_path: PathBuf,
    extension_ready: bool,
    started_at: Instant,
    commands: VecDeque<ExtensionCommand>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum EditorStatus {
    Closed,
    Unavailable {
        message: String,
    },
    Starting {
        instance_id: Uuid,
        path: String,
        log_path: PathBuf,
        tested_version: &'static str,
    },
    Ready {
        instance_id: Uuid,
        path: String,
        log_path: PathBuf,
        tested_version: &'static str,
    },
    Failed {
        message: String,
        log_path: Option<PathBuf>,
        tested_version: &'static str,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ExtensionCommand {
    pub id: Uuid,
    #[serde(flatten)]
    pub action: ExtensionAction,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum ExtensionAction {
    OpenDiff {
        title: String,
        files: Vec<ExtensionDiffFile>,
        focus: usize,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ExtensionDiffFile {
    pub path: PathBuf,
    pub before: String,
    pub after: String,
    pub focus_line: usize,
    pub changed_lines: usize,
}

impl CodeServerManager {
    pub(crate) fn new(executable: Option<PathBuf>) -> Result<Self> {
        let profile = global_data_dir()?.join("code-server");
        Ok(Self {
            inner: Arc::new(ManagerInner {
                executable,
                profile,
                control_origin: RwLock::new(String::new()),
                #[cfg(not(windows))]
                start_gate: AsyncMutex::new(()),
                instances: Mutex::new(HashMap::new()),
                projects: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub(crate) fn set_control_origin(&self, origin: String) {
        *self
            .inner
            .control_origin
            .write()
            .expect("code-server control origin lock poisoned") = origin;
    }

    pub(crate) async fn start(&self, project: &Path) -> EditorStatus {
        #[cfg(windows)]
        {
            let _ = project;
            EditorStatus::Unavailable {
                message: "The managed code-server integration is unavailable on native Windows."
                    .into(),
            }
        }
        #[cfg(not(windows))]
        {
            match self.start_supported(project).await {
                Ok(status) => status,
                Err(error) => EditorStatus::Failed {
                    message: format!("{error:#}"),
                    log_path: None,
                    tested_version: TESTED_CODE_SERVER_VERSION,
                },
            }
        }
    }

    #[cfg(not(windows))]
    async fn start_supported(&self, project: &Path) -> Result<EditorStatus> {
        let project = fs::canonicalize(project)
            .with_context(|| format!("cannot open project {}", project.display()))?;
        let _start_guard = self.inner.start_gate.lock().await;
        if let Some(id) = self
            .inner
            .projects
            .lock()
            .expect("code-server projects lock poisoned")
            .get(&project)
            .copied()
        {
            return Ok(self.status(id));
        }
        let Some(executable) = find_executable(self.inner.executable.as_deref()) else {
            return Ok(EditorStatus::Unavailable {
                message: INSTALL_MESSAGE.into(),
            });
        };
        install_extension(&self.inner.profile)
            .context("cannot install or update the CodeCrab code-server extension")?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("cannot reserve a code-server port")?;
        let address = listener.local_addr()?;
        drop(listener);
        let id = Uuid::new_v4();
        let editor_path = editor_path(id, &project);
        let token = Uuid::new_v4().to_string();
        let logs = self.inner.profile.join("logs");
        fs::create_dir_all(&logs)?;
        let log_path = logs.join(format!("{id}.log"));
        let log = log_file(&log_path)?;
        let error_log = log.try_clone()?;
        let user_data = self.inner.profile.join("user-data");
        let extensions = self.inner.profile.join("extensions");
        let runtime = self.inner.profile.join("runtime");
        fs::create_dir_all(&user_data)?;
        fs::create_dir_all(&runtime)?;
        let control_origin = self
            .inner
            .control_origin
            .read()
            .expect("code-server control origin lock poisoned")
            .clone();
        if control_origin.is_empty() {
            anyhow::bail!("the CodeCrab control origin is unavailable");
        }
        let mut command = Command::new(&executable);
        command
            .arg("--bind-addr")
            .arg(address.to_string())
            .args(["--auth", "none", "--cert", "false"])
            .arg("--cookie-suffix")
            .arg(id.to_string())
            .args([
                "--disable-telemetry",
                "--disable-update-check",
                "--disable-getting-started-override",
                "--disable-workspace-trust",
            ])
            .arg("--user-data-dir")
            .arg(&user_data)
            .arg("--extensions-dir")
            .arg(&extensions)
            .arg("--session-socket")
            .arg(runtime.join(format!("{id}.sock")))
            .arg(&project)
            .env("CODECRAB_CONTROL_ORIGIN", control_origin)
            .env("CODECRAB_INSTANCE_ID", id.to_string())
            .env("CODECRAB_EXTENSION_TOKEN", &token)
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(error_log))
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot start code-server from {}", executable.display()))?;
        let ready_deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if TcpStream::connect(address).await.is_ok() {
                break;
            }
            if let Some(status) = child.try_wait()? {
                return Ok(EditorStatus::Failed {
                    message: format!("code-server stopped during startup with {status}"),
                    log_path: Some(log_path),
                    tested_version: TESTED_CODE_SERVER_VERSION,
                });
            }
            if Instant::now() >= ready_deadline {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Ok(EditorStatus::Failed {
                    message: "code-server did not become ready within 15 seconds".into(),
                    log_path: Some(log_path),
                    tested_version: TESTED_CODE_SERVER_VERSION,
                });
            }
            sleep(Duration::from_millis(50)).await;
        }
        self.inner
            .instances
            .lock()
            .expect("code-server instances lock poisoned")
            .insert(
                id,
                ManagedInstance {
                    token,
                    project: project.clone(),
                    address,
                    child,
                    log_path: log_path.clone(),
                    extension_ready: false,
                    started_at: Instant::now(),
                    commands: VecDeque::new(),
                },
            );
        self.inner
            .projects
            .lock()
            .expect("code-server projects lock poisoned")
            .insert(project, id);
        Ok(EditorStatus::Starting {
            instance_id: id,
            path: editor_path,
            log_path,
            tested_version: TESTED_CODE_SERVER_VERSION,
        })
    }

    pub(crate) fn status_for_project(&self, project: &Path) -> EditorStatus {
        let Ok(project) = fs::canonicalize(project) else {
            return EditorStatus::Closed;
        };
        let id = self
            .inner
            .projects
            .lock()
            .expect("code-server projects lock poisoned")
            .get(&project)
            .copied();
        id.map_or(EditorStatus::Closed, |id| self.status(id))
    }

    pub(crate) fn status(&self, id: Uuid) -> EditorStatus {
        let mut instances = self
            .inner
            .instances
            .lock()
            .expect("code-server instances lock poisoned");
        let Some(instance) = instances.get_mut(&id) else {
            return EditorStatus::Closed;
        };
        match instance.child.try_wait() {
            Ok(Some(status)) => EditorStatus::Failed {
                message: format!("code-server stopped with {status}. Restart it to continue."),
                log_path: Some(instance.log_path.clone()),
                tested_version: TESTED_CODE_SERVER_VERSION,
            },
            Err(error) => EditorStatus::Failed {
                message: format!("cannot inspect code-server: {error}"),
                log_path: Some(instance.log_path.clone()),
                tested_version: TESTED_CODE_SERVER_VERSION,
            },
            Ok(None)
                if !instance.extension_ready
                    && instance.started_at.elapsed() > Duration::from_secs(45) =>
            {
                let _ = instance.child.start_kill();
                EditorStatus::Failed {
                    message: format!(
                        "CodeCrab could not load the extension required for this integration. Make sure you are using a compatible version of code-server. Tested with code-server {TESTED_CODE_SERVER_VERSION}."
                    ),
                    log_path: Some(instance.log_path.clone()),
                    tested_version: TESTED_CODE_SERVER_VERSION,
                }
            }
            Ok(None) if instance.extension_ready => EditorStatus::Ready {
                instance_id: id,
                path: editor_path(id, &instance.project),
                log_path: instance.log_path.clone(),
                tested_version: TESTED_CODE_SERVER_VERSION,
            },
            Ok(None) => EditorStatus::Starting {
                instance_id: id,
                path: editor_path(id, &instance.project),
                log_path: instance.log_path.clone(),
                tested_version: TESTED_CODE_SERVER_VERSION,
            },
        }
    }

    pub(crate) fn target(&self, id: Uuid) -> Option<SocketAddr> {
        self.inner
            .instances
            .lock()
            .expect("code-server instances lock poisoned")
            .get(&id)
            .map(|instance| instance.address)
    }

    pub(crate) fn authenticate(&self, id: Uuid, token: &str) -> bool {
        self.inner
            .instances
            .lock()
            .expect("code-server instances lock poisoned")
            .get(&id)
            .is_some_and(|instance| instance.token == token)
    }

    pub(crate) fn handshake(&self, id: Uuid) -> bool {
        let mut instances = self
            .inner
            .instances
            .lock()
            .expect("code-server instances lock poisoned");
        let Some(instance) = instances.get_mut(&id) else {
            return false;
        };
        instance.extension_ready = true;
        true
    }

    pub(crate) fn enqueue(&self, id: Uuid, action: ExtensionAction) -> Result<Uuid> {
        let command_id = Uuid::new_v4();
        let mut instances = self
            .inner
            .instances
            .lock()
            .expect("code-server instances lock poisoned");
        let instance = instances
            .get_mut(&id)
            .context("code-server instance not found")?;
        instance.commands.push_back(ExtensionCommand {
            id: command_id,
            action,
        });
        Ok(command_id)
    }

    pub(crate) fn take_commands(&self, id: Uuid) -> Option<Vec<ExtensionCommand>> {
        let mut instances = self
            .inner
            .instances
            .lock()
            .expect("code-server instances lock poisoned");
        let instance = instances.get_mut(&id)?;
        Some(instance.commands.drain(..).collect())
    }

    pub(crate) async fn restart(&self, project: &Path) -> EditorStatus {
        self.stop_project(project).await;
        self.start(project).await
    }

    async fn stop_project(&self, project: &Path) {
        let Ok(project) = fs::canonicalize(project) else {
            return;
        };
        let id = self
            .inner
            .projects
            .lock()
            .expect("code-server projects lock poisoned")
            .remove(&project);
        if let Some(id) = id {
            self.stop_instance(id).await;
        }
    }

    async fn stop_instance(&self, id: Uuid) {
        let instance = self
            .inner
            .instances
            .lock()
            .expect("code-server instances lock poisoned")
            .remove(&id);
        if let Some(mut instance) = instance {
            #[cfg(unix)]
            if let Some(pid) = instance.child.id() {
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
            let _ = instance.child.kill().await;
            let _ = instance.child.wait().await;
        }
    }

    pub(crate) async fn shutdown(&self) {
        let ids = self
            .inner
            .instances
            .lock()
            .expect("code-server instances lock poisoned")
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for id in ids {
            self.stop_instance(id).await;
        }
        self.inner
            .projects
            .lock()
            .expect("code-server projects lock poisoned")
            .clear();
    }
}

#[cfg(any(not(windows), test))]
fn install_extension(profile: &Path) -> Result<()> {
    let extensions = profile.join("extensions");
    let version = extension_version()?;
    let current_name = format!("{EXTENSION_ID}-{version}");
    fs::create_dir_all(&extensions)?;
    let extension = extensions.join(&current_name);
    fs::create_dir_all(&extension)?;
    publish(&extension.join("package.json"), EXTENSION_PACKAGE)?;
    publish(&extension.join("extension.js"), EXTENSION_MAIN)?;
    for entry in fs::read_dir(&extensions)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&format!("{EXTENSION_ID}-")) && name != current_name {
            fs::remove_dir_all(entry.path())?;
        }
    }
    register_extension(
        &extensions.join("extensions.json"),
        &extension,
        &current_name,
        &version,
    )?;
    remove_obsolete_markers(&extensions.join(".obsolete"))
}

#[cfg(any(not(windows), test))]
fn extension_version() -> Result<String> {
    let manifest: serde_json::Value = serde_json::from_str(EXTENSION_PACKAGE)
        .context("cannot parse the embedded CodeCrab extension manifest")?;
    manifest["version"]
        .as_str()
        .map(ToOwned::to_owned)
        .context("the embedded CodeCrab extension has no version")
}

#[cfg(any(not(windows), test))]
fn register_extension(
    path: &Path,
    extension: &Path,
    relative_name: &str,
    version: &str,
) -> Result<()> {
    let mut registry: Vec<serde_json::Value> = if path.exists() {
        serde_json::from_str(&fs::read_to_string(path)?)
            .with_context(|| format!("cannot parse {}", path.display()))?
    } else {
        Vec::new()
    };
    registry.retain(|entry| entry["identifier"]["id"].as_str() != Some(EXTENSION_ID));
    registry.push(serde_json::json!({
        "identifier": { "id": EXTENSION_ID },
        "version": version,
        "location": {
            "$mid": 1,
            "path": extension.to_string_lossy(),
            "scheme": "file"
        },
        "relativeLocation": relative_name
    }));
    publish(path, &serde_json::to_string(&registry)?)
}

#[cfg(any(not(windows), test))]
fn remove_obsolete_markers(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut obsolete: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&fs::read_to_string(path)?)
            .with_context(|| format!("cannot parse {}", path.display()))?;
    obsolete.retain(|name, _| !name.starts_with(&format!("{EXTENSION_ID}-")));
    publish(path, &serde_json::to_string(&obsolete)?)
}

fn editor_path(id: Uuid, project: &Path) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("folder", &project.to_string_lossy())
        .finish();
    format!("/code-server/{id}/?{query}")
}

#[cfg(any(not(windows), test))]
fn publish(path: &Path, content: &str) -> Result<()> {
    if fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(());
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, content)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(not(windows))]
fn log_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))
}

#[cfg(not(windows))]
fn find_executable(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured.filter(|path| path.is_file()) {
        return Some(path.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join("code-server"))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    fn test_manager(profile: PathBuf) -> CodeServerManager {
        CodeServerManager {
            inner: Arc::new(ManagerInner {
                executable: None,
                profile,
                control_origin: RwLock::new("http://127.0.0.1:1".into()),
                start_gate: AsyncMutex::new(()),
                instances: Mutex::new(HashMap::new()),
                projects: Mutex::new(HashMap::new()),
            }),
        }
    }

    #[test]
    fn extension_is_published_without_removing_other_extensions() {
        let temp = tempfile::tempdir().unwrap();
        let other = temp.path().join("extensions/other.extension/keep.txt");
        let old = temp
            .path()
            .join("extensions/codecrab.codecrab-integration-0.0.1/old.js");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&other, "keep").unwrap();
        fs::write(&old, "old").unwrap();
        install_extension(temp.path()).unwrap();
        assert_eq!(fs::read_to_string(other).unwrap(), "keep");
        assert!(!old.exists());
        assert!(
            temp.path()
                .join("extensions/codecrab.codecrab-integration-1.0.1/extension.js")
                .is_file()
        );
    }

    #[test]
    fn extension_install_repairs_stale_persistent_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let extensions = temp.path().join("extensions");
        fs::create_dir_all(&extensions).unwrap();
        fs::write(
            extensions.join("extensions.json"),
            serde_json::json!([
                {
                    "identifier": { "id": EXTENSION_ID },
                    "version": "1.0.0",
                    "relativeLocation": "codecrab.codecrab-integration-1.10.1"
                },
                {
                    "identifier": { "id": "other.extension" },
                    "version": "2.0.0"
                }
            ])
            .to_string(),
        )
        .unwrap();
        fs::write(
            extensions.join(".obsolete"),
            serde_json::json!({
                "codecrab.codecrab-integration-1.0.1": true,
                "other.extension-2.0.0": true
            })
            .to_string(),
        )
        .unwrap();

        install_extension(temp.path()).unwrap();

        let registry: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(extensions.join("extensions.json")).unwrap())
                .unwrap();
        assert_eq!(registry.as_array().unwrap().len(), 2);
        assert_eq!(registry[0]["identifier"]["id"], "other.extension");
        assert_eq!(registry[1]["identifier"]["id"], EXTENSION_ID);
        assert_eq!(registry[1]["version"], "1.0.1");
        assert_eq!(
            registry[1]["relativeLocation"],
            "codecrab.codecrab-integration-1.0.1"
        );
        let obsolete: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(extensions.join(".obsolete")).unwrap())
                .unwrap();
        assert_eq!(
            obsolete,
            serde_json::json!({ "other.extension-2.0.0": true })
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn editor_path_selects_the_managed_project_explicitly() {
        let id = Uuid::nil();
        assert_eq!(
            editor_path(id, Path::new("/tmp/Code Crab")),
            "/code-server/00000000-0000-0000-0000-000000000000/?folder=%2Ftmp%2FCode+Crab"
        );
    }

    #[test]
    fn extension_manifest_has_a_specific_compatible_vscode_engine() {
        let manifest: serde_json::Value = serde_json::from_str(EXTENSION_PACKAGE).unwrap();
        assert_eq!(manifest["version"], "1.0.1");
        assert_eq!(manifest["engines"]["vscode"], "^1.85.0");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn native_windows_reports_the_managed_integration_as_unavailable() {
        let manager = CodeServerManager::new(None).unwrap();
        let status = manager.start(Path::new("C:\\")).await;
        assert!(matches!(
            status,
            EditorStatus::Unavailable { message }
                if message == "The managed code-server integration is unavailable on native Windows."
        ));
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn handshake_commands_and_shutdown_use_one_reaped_instance() {
        let temp = tempfile::tempdir().unwrap();
        let manager = test_manager(temp.path().to_path_buf());
        let id = Uuid::new_v4();
        let token = "test-token".to_owned();
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 60"]);
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        manager.inner.instances.lock().unwrap().insert(
            id,
            ManagedInstance {
                token: token.clone(),
                project: temp.path().to_path_buf(),
                address: "127.0.0.1:9".parse().unwrap(),
                child,
                log_path: temp.path().join("instance.log"),
                extension_ready: false,
                started_at: Instant::now(),
                commands: VecDeque::new(),
            },
        );

        assert!(manager.authenticate(id, &token));
        assert!(matches!(manager.status(id), EditorStatus::Starting { .. }));
        assert!(manager.handshake(id));
        assert!(matches!(manager.status(id), EditorStatus::Ready { .. }));
        manager
            .enqueue(
                id,
                ExtensionAction::OpenDiff {
                    title: "Changes".into(),
                    files: Vec::new(),
                    focus: 0,
                },
            )
            .unwrap();
        assert_eq!(manager.take_commands(id).unwrap().len(), 1);

        manager.shutdown().await;

        assert!(manager.target(id).is_none());
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
