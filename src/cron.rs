use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::OsString,
    fs::{self, File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, LocalResult, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use chrono_tz::Tz;
use croner::Cron;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use crate::{
    config::{Config, SessionRegistry, global_data_dir, normalized_root},
    coordination::SessionCoordinator,
    diagnostics::{DebugOutput, DiagnosticLog},
    provider::ModelSelection,
    session::{ScheduledRun, SessionStore},
};

pub(crate) const CRON_DOCUMENT_VERSION: u32 = 1;
pub(crate) const CRON_STATE_VERSION: u32 = 1;
pub(crate) const DEFAULT_CRON_FILE_NAME: &str = "cron.json";
pub(crate) const DEFAULT_TIME_ZONE: &str = "UTC";
pub(crate) const DUE_GRACE_SECONDS: i64 = 60;
pub(crate) const NEXT_OCCURRENCE_PREVIEW_COUNT: usize = 5;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CronDocument {
    pub version: u32,
    #[serde(default = "default_time_zone")]
    pub timezone: String,
    #[serde(default)]
    pub jobs: BTreeMap<String, CronJob>,
}

impl Default for CronDocument {
    fn default() -> Self {
        Self {
            version: CRON_DOCUMENT_VERSION,
            timezone: default_time_zone(),
            jobs: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CronJob {
    pub schedule: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub project: PathBuf,
    pub prompt: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default)]
    pub overlap: OverlapPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_id: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OverlapPolicy {
    #[default]
    Skip,
    Queue,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CronOccurrenceStatus {
    Queued,
    Running,
    Completed,
    Failed,
    TimedOut,
    SkippedOverlap,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CronOccurrence {
    pub sequence: u64,
    pub scheduled_at: DateTime<Utc>,
    pub status: CronOccurrenceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub manual: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OneTimeStatus {
    Pending,
    Completed,
    Failed,
    TimedOut,
    Expired,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct CronJobState {
    #[serde(default = "first_occurrence_sequence")]
    pub next_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_time_status: Option<OneTimeStatus>,
    #[serde(default)]
    pub occurrences: Vec<CronOccurrence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CronRuntimeState {
    pub version: u32,
    #[serde(default)]
    pub jobs: BTreeMap<String, CronJobState>,
}

impl Default for CronRuntimeState {
    fn default() -> Self {
        Self {
            version: CRON_STATE_VERSION,
            jobs: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CronDaemonStatus {
    Running,
    Stopped,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CronJobView {
    pub id: String,
    pub job: CronJob,
    pub description: String,
    pub next_occurrences: Vec<DateTime<Utc>>,
    pub state: CronJobState,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CronSnapshot {
    pub path: PathBuf,
    pub daemon: CronDaemonStatus,
    pub document: CronDocument,
    pub jobs: Vec<CronJobView>,
    pub installation: CronInstallationView,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CronInstallationStatus {
    NotInstalled,
    InstalledStopped,
    Running,
    RunningUnmanaged,
    Unhealthy,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CronInstallationView {
    pub status: CronInstallationStatus,
    pub method: Option<String>,
    pub artifact: Option<PathBuf>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CronInstallation {
    version: u32,
    method: String,
    name: String,
    artifact: Option<PathBuf>,
    executable: PathBuf,
    schedule_path: PathBuf,
    installed_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
enum ParsedSchedule {
    Cron(Box<Cron>),
    At(DateTime<Utc>),
    Reboot,
}

pub(crate) struct CronDaemonLock {
    #[allow(dead_code)]
    file: File,
    pub(crate) path: PathBuf,
}

#[derive(Clone)]
pub(crate) struct CronStore {
    schedule_path: PathBuf,
    runtime_dir: PathBuf,
    mutation_lock: Arc<Mutex<()>>,
}

#[derive(Debug)]
struct JobCompletion {
    job_id: String,
    sequence: u64,
    status: CronOccurrenceStatus,
    session_id: Option<Uuid>,
    last_message: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct ManualRunRequest {
    id: String,
    requested_at: DateTime<Utc>,
}

fn default_time_zone() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| DEFAULT_TIME_ZONE.into())
}

const fn enabled_by_default() -> bool {
    true
}

const fn first_occurrence_sequence() -> u64 {
    1
}

impl CronDocument {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.version != CRON_DOCUMENT_VERSION {
            anyhow::bail!(
                "unsupported cron document version {}; expected {}",
                self.version,
                CRON_DOCUMENT_VERSION
            );
        }
        parse_time_zone(&self.timezone)?;
        for (id, job) in &self.jobs {
            validate_job_id(id)?;
            job.validate(&self.timezone)
                .with_context(|| format!("invalid cron job {id:?}"))?;
        }
        Ok(())
    }
}

impl CronJob {
    pub(crate) fn validate(&self, default_timezone: &str) -> Result<()> {
        parse_schedule(&self.schedule)?;
        parse_time_zone(self.timezone.as_deref().unwrap_or(default_timezone))?;
        if self.prompt.trim().is_empty() {
            anyhow::bail!("prompt cannot be empty");
        }
        if !self.project.is_absolute() {
            anyhow::bail!("project must be an absolute path");
        }
        if self.provider.trim().is_empty() {
            anyhow::bail!("provider cannot be empty");
        }
        if self.model.trim().is_empty() || self.model == "auto" {
            anyhow::bail!("model must name a concrete provider model");
        }
        if let Some(timeout) = self.timeout_seconds
            && timeout == 0
        {
            anyhow::bail!("timeout_seconds must be greater than zero");
        }
        Ok(())
    }

    pub(crate) fn description(&self, default_timezone: &str) -> Result<String> {
        let timezone = self.timezone.as_deref().unwrap_or(default_timezone);
        Ok(match parse_schedule(&self.schedule)? {
            ParsedSchedule::Cron(cron) => format!("{} ({timezone})", cron.describe()),
            ParsedSchedule::At(at) => format!(
                "Once at {} ({timezone})",
                at.with_timezone(&parse_time_zone(timezone)?)
                    .to_rfc3339_opts(SecondsFormat::Secs, true)
            ),
            ParsedSchedule::Reboot => "Once when the cron daemon starts".into(),
        })
    }

    pub(crate) fn one_time_at(&self) -> Result<Option<DateTime<Utc>>> {
        Ok(match parse_schedule(&self.schedule)? {
            ParsedSchedule::At(at) => Some(at),
            ParsedSchedule::Cron(_) | ParsedSchedule::Reboot => None,
        })
    }
}

pub(crate) fn proposal_token(
    id: &str,
    job: &CronJob,
    existing: Option<&CronJob>,
) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(action_token("schedule", id, job)?.as_bytes());
    digest.update([0]);
    match existing {
        Some(existing) => digest.update(serde_json::to_vec(existing)?),
        None => digest.update(b"missing"),
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub(crate) fn action_token(action: &str, id: &str, job: &CronJob) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(action.as_bytes());
    digest.update([0]);
    digest.update(id.as_bytes());
    digest.update([0]);
    digest.update(serde_json::to_vec(job)?);
    Ok(format!("{:x}", digest.finalize()))
}

impl CronStore {
    #[cfg(test)]
    pub(crate) fn at(schedule_path: PathBuf, runtime_dir: PathBuf) -> Self {
        Self {
            schedule_path,
            runtime_dir,
            mutation_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn default() -> Result<Self> {
        Self::new(default_cron_path()?)
    }

    pub(crate) fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let schedule_path = resolved_schedule_path(&path.into())?;
        let runtime_dir = runtime_directory_for(&schedule_path)?;
        Ok(Self {
            schedule_path,
            runtime_dir,
            mutation_lock: Arc::new(Mutex::new(())),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.schedule_path
    }

    pub(crate) fn load_or_create(&self) -> Result<CronDocument> {
        if !self.schedule_path.exists() {
            if let Some(parent) = self.schedule_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }
            self.save_document_sync(&CronDocument::default())?;
        }
        self.load_document()
    }

    pub(crate) fn load_document(&self) -> Result<CronDocument> {
        let bytes = fs::read(&self.schedule_path)
            .with_context(|| format!("cannot read {}", self.schedule_path.display()))?;
        let document: CronDocument = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not valid cron JSON", self.schedule_path.display()))?;
        document.validate()?;
        Ok(document)
    }

    pub(crate) async fn save_document(&self, document: &CronDocument) -> Result<()> {
        let _guard = self.mutation_lock.lock().await;
        let _file_guard = self.lock_mutations()?;
        self.save_document_sync(document)
    }

    pub(crate) fn save_document_sync(&self, document: &CronDocument) -> Result<()> {
        document.validate()?;
        write_json_atomic(&self.schedule_path, document)
    }

    pub(crate) fn load_state(&self) -> Result<CronRuntimeState> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(CronRuntimeState::default());
        }
        let state: CronRuntimeState = serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?,
        )
        .with_context(|| format!("{} is not valid cron runtime state", path.display()))?;
        if state.version != CRON_STATE_VERSION {
            anyhow::bail!(
                "unsupported cron runtime state version {}; expected {}",
                state.version,
                CRON_STATE_VERSION
            );
        }
        Ok(state)
    }

    pub(crate) fn save_state(&self, state: &CronRuntimeState) -> Result<()> {
        fs::create_dir_all(&self.runtime_dir)
            .with_context(|| format!("cannot create {}", self.runtime_dir.display()))?;
        write_json_atomic(&self.state_path(), state)
    }

    pub(crate) fn try_daemon_lock(&self) -> Result<CronDaemonLock> {
        self.try_executor_lock("daemon.lock", "cron daemon")
    }

    fn try_direct_run_lock(&self) -> Result<CronDaemonLock> {
        self.try_executor_lock("direct-run.lock", "direct scheduled execution")
    }

    fn try_executor_lock(&self, file_name: &str, owner: &str) -> Result<CronDaemonLock> {
        fs::create_dir_all(&self.runtime_dir)
            .with_context(|| format!("cannot create {}", self.runtime_dir.display()))?;
        let path = self.runtime_dir.join(file_name);
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                anyhow::bail!(
                    "another CodeCrab {owner} already owns {}",
                    self.schedule_path.display()
                )
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("cannot lock {}", path.display()));
            }
        }
        use std::io::{Seek, Write};
        file.set_len(0)?;
        file.rewind()?;
        writeln!(
            file,
            "pid={}\nowner={}\nfile={}",
            std::process::id(),
            owner,
            self.schedule_path.display()
        )?;
        file.flush()?;
        Ok(CronDaemonLock { file, path })
    }

    pub(crate) fn daemon_status(&self) -> Result<CronDaemonStatus> {
        fs::create_dir_all(&self.runtime_dir)
            .with_context(|| format!("cannot create {}", self.runtime_dir.display()))?;
        if !self.lock_active("daemon.lock")? || self.direct_run_active()? {
            Ok(CronDaemonStatus::Stopped)
        } else {
            Ok(CronDaemonStatus::Running)
        }
    }

    fn direct_run_active(&self) -> Result<bool> {
        self.lock_active("direct-run.lock")
    }

    fn daemon_lock_active(&self) -> Result<bool> {
        self.lock_active("daemon.lock")
    }

    fn lock_active(&self, file_name: &str) -> Result<bool> {
        let path = self.runtime_dir.join(file_name);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(false),
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("cannot inspect {}", path.display()))
            }
        }
    }

    pub(crate) fn snapshot(&self, now: DateTime<Utc>) -> Result<CronSnapshot> {
        let document = self.load_or_create()?;
        let mut state = self.load_state()?;
        if let Ok(_direct_lock) = self.try_direct_run_lock()
            && let Ok(_daemon_lock) = self.try_daemon_lock()
            && recover_interrupted_occurrences(&mut state)
        {
            self.save_state(&state)?;
        }
        let jobs = document
            .jobs
            .iter()
            .map(|(id, job)| {
                Ok(CronJobView {
                    id: id.clone(),
                    job: job.clone(),
                    description: job.description(&document.timezone)?,
                    next_occurrences: next_occurrences(
                        job,
                        &document.timezone,
                        now,
                        NEXT_OCCURRENCE_PREVIEW_COUNT,
                    )?,
                    state: state.jobs.get(id).cloned().unwrap_or_default(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(CronSnapshot {
            path: self.schedule_path.clone(),
            daemon: self.daemon_status()?,
            document,
            jobs,
            installation: self.installation_view()?,
        })
    }

    pub(crate) async fn upsert(&self, id: &str, job: CronJob) -> Result<CronSnapshot> {
        validate_job_id(id)?;
        let _guard = self.mutation_lock.lock().await;
        let _file_guard = self.lock_mutations()?;
        let mut document = self.load_or_create()?;
        job.validate(&document.timezone)?;
        document.jobs.insert(id.to_owned(), job);
        self.save_document_sync(&document)?;
        self.snapshot(Utc::now())
    }

    pub(crate) async fn upsert_confirmed(
        &self,
        id: &str,
        job: CronJob,
        token: &str,
    ) -> Result<CronSnapshot> {
        validate_job_id(id)?;
        let _guard = self.mutation_lock.lock().await;
        let _file_guard = self.lock_mutations()?;
        let mut document = self.load_or_create()?;
        job.validate(&document.timezone)?;
        if token != proposal_token(id, &job, document.jobs.get(id))? {
            anyhow::bail!(
                "the schedule or existing job changed after preview; call cron_preview again and obtain explicit user confirmation"
            );
        }
        document.jobs.insert(id.to_owned(), job);
        self.save_document_sync(&document)?;
        self.snapshot(Utc::now())
    }

    pub(crate) async fn delete(&self, id: &str) -> Result<bool> {
        let _guard = self.mutation_lock.lock().await;
        let _file_guard = self.lock_mutations()?;
        let mut document = self.load_or_create()?;
        let removed = document.jobs.remove(id).is_some();
        if removed {
            self.save_document_sync(&document)?;
        }
        Ok(removed)
    }

    pub(crate) async fn delete_confirmed(&self, id: &str, token: &str) -> Result<bool> {
        let _guard = self.mutation_lock.lock().await;
        let _file_guard = self.lock_mutations()?;
        let mut document = self.load_or_create()?;
        let job = document
            .jobs
            .get(id)
            .with_context(|| format!("cron job {id:?} does not exist"))?;
        if token != action_token("delete", id, job)? {
            anyhow::bail!(
                "the job changed after it was listed; call cron_list again and obtain explicit user confirmation"
            );
        }
        let removed = document.jobs.remove(id).is_some();
        self.save_document_sync(&document)?;
        Ok(removed)
    }

    pub(crate) async fn set_enabled(&self, id: &str, enabled: bool) -> Result<CronSnapshot> {
        let _guard = self.mutation_lock.lock().await;
        let _file_guard = self.lock_mutations()?;
        let mut document = self.load_or_create()?;
        let job = document
            .jobs
            .get_mut(id)
            .with_context(|| format!("cron job {id:?} does not exist"))?;
        job.enabled = enabled;
        self.save_document_sync(&document)?;
        self.snapshot(Utc::now())
    }

    pub(crate) async fn set_enabled_confirmed(
        &self,
        id: &str,
        enabled: bool,
        token: &str,
    ) -> Result<CronSnapshot> {
        let _guard = self.mutation_lock.lock().await;
        let _file_guard = self.lock_mutations()?;
        let mut document = self.load_or_create()?;
        let job = document
            .jobs
            .get_mut(id)
            .with_context(|| format!("cron job {id:?} does not exist"))?;
        if token != action_token(&format!("enabled:{enabled}"), id, job)? {
            anyhow::bail!(
                "the job changed after it was listed; call cron_list again and obtain explicit user confirmation"
            );
        }
        job.enabled = enabled;
        self.save_document_sync(&document)?;
        self.snapshot(Utc::now())
    }

    pub(crate) async fn request_run(&self, id: &str) -> Result<CronSnapshot> {
        let document = self.load_or_create()?;
        if !document.jobs.contains_key(id) {
            anyhow::bail!("cron job {id:?} does not exist");
        }
        fs::create_dir_all(self.request_directory())?;
        let request = ManualRunRequest {
            id: id.to_owned(),
            requested_at: Utc::now(),
        };
        write_json_atomic(
            &self
                .request_directory()
                .join(format!("{}.json", Uuid::new_v4())),
            &request,
        )?;
        self.snapshot(Utc::now())
    }

    pub(crate) async fn run_now(
        &self,
        id: &str,
        coordinator: SessionCoordinator,
    ) -> Result<CronSnapshot> {
        let document = self.load_or_create()?;
        let job = document
            .jobs
            .get(id)
            .cloned()
            .with_context(|| format!("cron job {id:?} does not exist"))?;

        let direct_lock = match self.try_direct_run_lock() {
            Ok(lock) => lock,
            Err(error) => {
                return Err(error).context(
                    "another direct scheduled execution is still running; try again after it finishes",
                );
            }
        };
        let daemon_lock = match self.try_daemon_lock() {
            Ok(lock) => lock,
            Err(_error) if self.daemon_lock_active()? => {
                drop(direct_lock);
                return self.request_run(id).await;
            }
            Err(error) => return Err(error),
        };

        let _guard = self.mutation_lock.lock().await;
        let mut state = self.load_state()?;
        recover_interrupted_occurrences(&mut state);
        let job_state = state.jobs.entry(id.to_owned()).or_default();
        if job.one_time_at()?.is_some() {
            job_state.one_time_status = Some(OneTimeStatus::Pending);
        }
        let sequence = job_state.next_sequence.max(1);
        job_state.next_sequence = sequence.saturating_add(1);
        let scheduled_at = Utc::now();
        job_state.occurrences.push(CronOccurrence {
            sequence,
            scheduled_at,
            status: CronOccurrenceStatus::Running,
            started_at: Some(scheduled_at),
            completed_at: None,
            session_id: None,
            last_message: None,
            error: None,
            manual: true,
        });
        self.save_state(&state)?;
        drop(_guard);

        let store = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let _locks = (direct_lock, daemon_lock);
            let completion = execute_job(&id, sequence, scheduled_at, &job, coordinator).await;
            if let Err(error) = store.finish_direct_run(completion).await {
                eprintln!("Cannot persist cron run completion: {error:#}");
            }
        });
        self.snapshot(Utc::now())
    }

    async fn finish_direct_run(&self, completion: JobCompletion) -> Result<()> {
        let _guard = self.mutation_lock.lock().await;
        let mut state = self.load_state()?;
        let job_state = state.jobs.entry(completion.job_id.clone()).or_default();
        if let Some(occurrence) = job_state
            .occurrences
            .iter_mut()
            .find(|occurrence| occurrence.sequence == completion.sequence)
        {
            occurrence.status = completion.status;
            occurrence.completed_at = Some(Utc::now());
            occurrence.session_id = completion.session_id;
            occurrence.last_message = completion.last_message;
            occurrence.error = completion.error;
        }
        if let Ok(document) = self.load_document()
            && let Some(job) = document.jobs.get(&completion.job_id)
            && matches!(parse_schedule(&job.schedule), Ok(ParsedSchedule::At(_)))
        {
            job_state.one_time_status = Some(one_time_status_for(completion.status));
        }
        self.save_state(&state)
    }

    pub(crate) fn install(&self) -> Result<CronInstallationView> {
        let _file_guard = self.lock_mutations()?;
        self.load_or_create()?;
        if self.daemon_status()? == CronDaemonStatus::Running {
            anyhow::bail!(
                "stop the currently running cron daemon before installing managed autostart"
            );
        }
        let executable =
            std::env::current_exe().context("cannot locate the CodeCrab executable")?;
        if !executable.is_file() {
            anyhow::bail!(
                "CodeCrab executable does not exist: {}",
                executable.display()
            );
        }
        fs::create_dir_all(&self.runtime_dir)?;
        if self.installation_path().exists() {
            anyhow::bail!("cron autostart metadata already exists; uninstall it first");
        }
        let manual = manual_autostart_instructions(&executable, self.path());
        let installation = platform_install(self, &executable)
            .with_context(|| format!("automatic cron installation failed. {manual}"))?;
        if let Err(error) = write_json_atomic(&self.installation_path(), &installation) {
            let _ = platform_uninstall(self, Some(&installation));
            return Err(error).context(format!(
                "autostart was rolled back because metadata could not be saved. {manual}"
            ));
        }
        let view = self.installation_view()?;
        if matches!(
            view.status,
            CronInstallationStatus::Unhealthy | CronInstallationStatus::NotInstalled
        ) {
            drop(_file_guard);
            let _ = self.uninstall();
            anyhow::bail!(
                "cron autostart did not pass post-install verification and was rolled back. {manual}"
            );
        }
        Ok(view)
    }

    pub(crate) fn uninstall(&self) -> Result<CronInstallationView> {
        let _file_guard = self.lock_mutations()?;
        let installation = self.load_installation().transpose()?;
        let uninstall_result = platform_uninstall(self, installation.as_ref());
        if self.installation_path().exists() {
            fs::remove_file(self.installation_path())
                .with_context(|| format!("cannot remove {}", self.installation_path().display()))?;
        }
        uninstall_result?;
        self.installation_view()
    }

    pub(crate) fn installation_view(&self) -> Result<CronInstallationView> {
        let daemon = self.daemon_status()?;
        let Some(installation) = self.load_installation().transpose()? else {
            return Ok(CronInstallationView {
                status: if daemon == CronDaemonStatus::Running {
                    CronInstallationStatus::RunningUnmanaged
                } else {
                    CronInstallationStatus::NotInstalled
                },
                method: None,
                artifact: None,
                detail: None,
            });
        };
        let registered = platform_registration_exists(&installation)?;
        let status = if !registered {
            CronInstallationStatus::Unhealthy
        } else if daemon == CronDaemonStatus::Running {
            CronInstallationStatus::Running
        } else {
            CronInstallationStatus::InstalledStopped
        };
        Ok(CronInstallationView {
            status,
            method: Some(installation.method),
            artifact: installation.artifact,
            detail: (!registered)
                .then_some("the recorded operating-system registration is missing".into()),
        })
    }

    fn load_installation(&self) -> Option<Result<CronInstallation>> {
        let path = self.installation_path();
        path.exists().then(|| {
            serde_json::from_slice(&fs::read(&path)?)
                .with_context(|| format!("invalid cron installation metadata {}", path.display()))
        })
    }

    fn take_manual_requests(&self) -> Result<Vec<ManualRunRequest>> {
        let directory = self.request_directory();
        if !directory.exists() {
            return Ok(Vec::new());
        }
        let mut paths = fs::read_dir(&directory)?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
            .collect::<Vec<_>>();
        paths.sort();
        let mut requests = Vec::new();
        for path in paths {
            let request = serde_json::from_slice::<ManualRunRequest>(&fs::read(&path)?)
                .with_context(|| format!("invalid manual cron request {}", path.display()))?;
            fs::remove_file(&path)?;
            requests.push(request);
        }
        Ok(requests)
    }

    fn state_path(&self) -> PathBuf {
        self.runtime_dir.join("state.json")
    }

    fn request_directory(&self) -> PathBuf {
        self.runtime_dir.join("requests")
    }

    fn installation_path(&self) -> PathBuf {
        self.runtime_dir.join("installation.json")
    }

    fn lock_mutations(&self) -> Result<File> {
        fs::create_dir_all(&self.runtime_dir)
            .with_context(|| format!("cannot create {}", self.runtime_dir.display()))?;
        let path = self.runtime_dir.join("mutation.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        file.lock()
            .with_context(|| format!("cannot lock {}", path.display()))?;
        Ok(file)
    }

    fn installation_id(&self) -> String {
        self.runtime_dir
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("default")
            .to_owned()
    }
}

pub(crate) fn default_cron_path() -> Result<PathBuf> {
    Ok(global_data_dir()?.join(DEFAULT_CRON_FILE_NAME))
}

pub(crate) fn next_occurrences(
    job: &CronJob,
    default_timezone: &str,
    after: DateTime<Utc>,
    count: usize,
) -> Result<Vec<DateTime<Utc>>> {
    let timezone = parse_time_zone(job.timezone.as_deref().unwrap_or(default_timezone))?;
    match parse_schedule(&job.schedule)? {
        ParsedSchedule::At(at) => Ok((at > after).then_some(at).into_iter().collect()),
        ParsedSchedule::Reboot => Ok(Vec::new()),
        ParsedSchedule::Cron(cron) => {
            let mut cursor = after.with_timezone(&timezone);
            let mut result = Vec::with_capacity(count);
            let mut last_local = None;
            for _ in 0..count {
                let next = find_next_valid_cron_occurrence(&cron, &mut cursor, last_local)?;
                last_local = Some(next.naive_local());
                result.push(next.with_timezone(&Utc));
            }
            Ok(result)
        }
    }
}

pub(crate) async fn run_daemon(
    store: CronStore,
    config: Config,
    registry: SessionRegistry,
    debug_openai: DebugOutput,
) -> Result<()> {
    store.load_or_create()?;
    let lock = store.try_daemon_lock()?;
    println!(
        "CodeCrab cron is watching {} (lock: {}).",
        store.path().display(),
        lock.path.display()
    );
    let coordinator = SessionCoordinator::new(
        config,
        registry,
        debug_openai,
        DiagnosticLog::stderr(),
        std::env::current_dir()?,
        Config::instructions_path()?,
    );
    let mut state = store.load_state()?;
    recover_interrupted_occurrences(&mut state);
    store.save_state(&state)?;
    let mut last_checked = Utc::now();
    let mut active = HashSet::new();
    let mut queued = HashMap::<String, u64>::new();
    let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
    let mut last_document_error = None::<String>;
    let mut startup_pending = true;

    loop {
        let now = Utc::now();
        let document = match store.load_document() {
            Ok(document) => {
                if last_document_error.take().is_some() {
                    eprintln!("Cron configuration is valid again.");
                }
                Some(document)
            }
            Err(error) => {
                let error = format!("{error:#}");
                if last_document_error.as_deref() != Some(&error) {
                    eprintln!("Cron scheduling paused: {error}");
                    last_document_error = Some(error);
                }
                None
            }
        };

        if let Some(document) = document {
            if startup_pending {
                for (id, job) in &document.jobs {
                    if job.enabled
                        && matches!(parse_schedule(&job.schedule)?, ParsedSchedule::Reboot)
                    {
                        schedule_occurrence(
                            id,
                            job,
                            now,
                            false,
                            &mut state,
                            &mut active,
                            &mut queued,
                            &coordinator,
                            &completion_tx,
                        )?;
                    }
                }
                startup_pending = false;
            }
            for request in store.take_manual_requests()? {
                if let Some(job) = document.jobs.get(&request.id) {
                    schedule_occurrence(
                        &request.id,
                        job,
                        request.requested_at,
                        true,
                        &mut state,
                        &mut active,
                        &mut queued,
                        &coordinator,
                        &completion_tx,
                    )?;
                }
            }
            for (id, job) in &document.jobs {
                if !job.enabled {
                    continue;
                }
                for scheduled_at in due_occurrences(job, &document.timezone, last_checked, now)? {
                    schedule_occurrence(
                        id,
                        job,
                        scheduled_at,
                        false,
                        &mut state,
                        &mut active,
                        &mut queued,
                        &coordinator,
                        &completion_tx,
                    )?;
                }
                expire_one_time_if_needed(id, job, now, &mut state)?;
            }
            store.save_state(&state)?;
        }
        last_checked = now;

        tokio::select! {
            completion = completion_rx.recv() => {
                if let Some(completion) = completion {
                    finish_occurrence(
                        completion,
                        &mut state,
                        &mut active,
                        &mut queued,
                        &store,
                        &coordinator,
                        &completion_tx,
                    )?;
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = tokio::signal::ctrl_c() => {
                eprintln!("Stopping CodeCrab cron.");
                coordinator.manager().cancel_all();
                coordinator.manager().shutdown_all().await?;
                return Ok(());
            }
        }
    }
}

fn due_occurrences(
    job: &CronJob,
    default_timezone: &str,
    after: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Vec<DateTime<Utc>>> {
    let due = match parse_schedule(&job.schedule)? {
        ParsedSchedule::At(at) => (at > after
            && at <= now
            && now.signed_duration_since(at).num_seconds() <= DUE_GRACE_SECONDS)
            .then_some(at)
            .into_iter()
            .collect(),
        ParsedSchedule::Reboot => Vec::new(),
        ParsedSchedule::Cron(cron) => {
            let timezone = parse_time_zone(job.timezone.as_deref().unwrap_or(default_timezone))?;
            let grace_start = now - chrono::Duration::seconds(DUE_GRACE_SECONDS + 1);
            let mut cursor = after.max(grace_start).with_timezone(&timezone);
            let mut result = Vec::new();
            let mut last_local = None;
            for _ in 0..64 {
                let next = find_next_valid_cron_occurrence(&cron, &mut cursor, last_local)?;
                let next_utc = next.with_timezone(&Utc);
                if next_utc > now {
                    break;
                }
                last_local = Some(next.naive_local());
                if now.signed_duration_since(next_utc).num_seconds() <= DUE_GRACE_SECONDS {
                    result.push(next_utc);
                }
            }
            result
        }
    };
    Ok(due)
}

fn find_next_valid_cron_occurrence(
    cron: &Cron,
    cursor: &mut DateTime<Tz>,
    last_local: Option<NaiveDateTime>,
) -> Result<DateTime<Tz>> {
    for _ in 0..128 {
        let next = cron.find_next_occurrence(cursor, false)?;
        *cursor = next;
        let repeated_second = matches!(
            cursor.timezone().from_local_datetime(&cursor.naive_local()),
            LocalResult::Ambiguous(first, _) if cursor.with_timezone(&Utc) != first.with_timezone(&Utc)
        );
        if repeated_second
            || !cron.is_time_matching(cursor)?
            || Some(cursor.naive_local()) == last_local
        {
            continue;
        }
        return Ok(cursor.to_owned());
    }
    anyhow::bail!("cron expression did not yield a valid local-time occurrence")
}

fn expire_one_time_if_needed(
    id: &str,
    job: &CronJob,
    now: DateTime<Utc>,
    state: &mut CronRuntimeState,
) -> Result<()> {
    let ParsedSchedule::At(at) = parse_schedule(&job.schedule)? else {
        return Ok(());
    };
    let job_state = state.jobs.entry(id.to_owned()).or_default();
    if job_state.one_time_status.is_none() {
        job_state.one_time_status = Some(OneTimeStatus::Pending);
    }
    if job_state.one_time_status == Some(OneTimeStatus::Pending)
        && job_state.occurrences.is_empty()
        && now.signed_duration_since(at).num_seconds() > DUE_GRACE_SECONDS
    {
        job_state.one_time_status = Some(OneTimeStatus::Expired);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn schedule_occurrence(
    id: &str,
    job: &CronJob,
    scheduled_at: DateTime<Utc>,
    manual: bool,
    state: &mut CronRuntimeState,
    active: &mut HashSet<String>,
    queued: &mut HashMap<String, u64>,
    coordinator: &SessionCoordinator,
    completion_tx: &mpsc::UnboundedSender<JobCompletion>,
) -> Result<u64> {
    let job_state = state.jobs.entry(id.to_owned()).or_default();
    let sequence = job_state.next_sequence.max(1);
    job_state.next_sequence = sequence.saturating_add(1);
    let now = Utc::now();
    if active.contains(id) {
        match job.overlap {
            OverlapPolicy::Skip => {
                job_state.occurrences.push(CronOccurrence {
                    sequence,
                    scheduled_at,
                    status: CronOccurrenceStatus::SkippedOverlap,
                    started_at: None,
                    completed_at: Some(now),
                    session_id: None,
                    last_message: None,
                    error: Some("another occurrence was still running".into()),
                    manual,
                });
            }
            OverlapPolicy::Queue => {
                if let Some(previous) = queued.insert(id.to_owned(), sequence)
                    && let Some(occurrence) = job_state
                        .occurrences
                        .iter_mut()
                        .find(|occurrence| occurrence.sequence == previous)
                {
                    occurrence.status = CronOccurrenceStatus::SkippedOverlap;
                    occurrence.completed_at = Some(now);
                    occurrence.error = Some("replaced by a newer queued occurrence".into());
                }
                job_state.occurrences.push(CronOccurrence {
                    sequence,
                    scheduled_at,
                    status: CronOccurrenceStatus::Queued,
                    started_at: None,
                    completed_at: None,
                    session_id: None,
                    last_message: None,
                    error: None,
                    manual,
                });
            }
        }
    } else {
        active.insert(id.to_owned());
        job_state.occurrences.push(CronOccurrence {
            sequence,
            scheduled_at,
            status: CronOccurrenceStatus::Running,
            started_at: Some(now),
            completed_at: None,
            session_id: None,
            last_message: None,
            error: None,
            manual,
        });
        spawn_job(id, sequence, scheduled_at, job, coordinator, completion_tx);
    }
    Ok(sequence)
}

fn spawn_job(
    id: &str,
    sequence: u64,
    scheduled_at: DateTime<Utc>,
    job: &CronJob,
    coordinator: &SessionCoordinator,
    completion_tx: &mpsc::UnboundedSender<JobCompletion>,
) {
    let id = id.to_owned();
    let job = job.clone();
    let coordinator = coordinator.clone();
    let completion_tx = completion_tx.clone();
    tokio::spawn(async move {
        let completion = execute_job(&id, sequence, scheduled_at, &job, coordinator).await;
        let _ = completion_tx.send(completion);
    });
}

async fn execute_job(
    job_id: &str,
    sequence: u64,
    scheduled_at: DateTime<Utc>,
    job: &CronJob,
    coordinator: SessionCoordinator,
) -> JobCompletion {
    let mut created_session_id = None;
    let result = async {
        let root = normalized_root(&job.project);
        if !root.is_dir() {
            anyhow::bail!("project directory does not exist: {}", root.display());
        }
        let mut session = SessionStore::new(&root)?
            .create_for_provider(job.provider.clone(), job.model.clone())?;
        session.reasoning_effort.clone_from(&job.reasoning);
        session.service_tier.clone_from(&job.speed);
        session.parent_session_id = job.source_session_id;
        session.scheduled_run = Some(ScheduledRun {
            job_id: job_id.to_owned(),
            occurrence: sequence,
            scheduled_at,
        });
        let session_id = session.id;
        created_session_id = Some(session_id);
        SessionStore::new(&root)?.save(&session)?;
        let mut agent = coordinator.build_agent(&root, session)?;
        let catalog = agent.fetch_models().await?;
        let model = catalog
            .iter()
            .find(|model| model.slug == job.model)
            .with_context(|| {
                format!(
                    "scheduled model {:?} is unavailable from provider {:?}",
                    job.model, job.provider
                )
            })?;
        if let Some(reasoning) = &job.reasoning
            && !model
                .supported_reasoning_levels
                .iter()
                .any(|option| option.effort == *reasoning)
        {
            anyhow::bail!(
                "scheduled reasoning effort {reasoning:?} is unavailable for model {:?}",
                job.model
            );
        }
        if let Some(speed) = &job.speed
            && !model.service_tiers.iter().any(|option| option.id == *speed)
        {
            anyhow::bail!(
                "scheduled service tier {speed:?} is unavailable for model {:?}",
                job.model
            );
        }
        agent.set_model_selection(ModelSelection {
            model: job.model.clone(),
            reasoning_effort: job.reasoning.clone(),
            service_tier: job.speed.clone(),
        });
        SessionStore::new(&root)?.save(agent.session())?;
        let handle = coordinator.install(agent)?;
        let mut turn = handle.start_turn(job.prompt.clone(), None)?;
        let outcome = if let Some(seconds) = job.timeout_seconds {
            match tokio::time::timeout(Duration::from_secs(seconds), &mut turn).await {
                Ok(result) => JobTurnResult::from_join(result),
                Err(_) => {
                    handle.cancel();
                    let _ = turn.await;
                    JobTurnResult::TimedOut
                }
            }
        } else {
            JobTurnResult::from_join(turn.await)
        };
        if let Some(handle) = coordinator.manager().take_if_idle(session_id)? {
            let _ = handle.shutdown().await;
        }
        Ok::<_, anyhow::Error>((session_id, outcome))
    }
    .await;

    match result {
        Ok((session_id, JobTurnResult::Completed(message))) => JobCompletion {
            job_id: job_id.to_owned(),
            sequence,
            status: CronOccurrenceStatus::Completed,
            session_id: Some(session_id),
            last_message: Some(message),
            error: None,
        },
        Ok((session_id, JobTurnResult::Failed(error))) => JobCompletion {
            job_id: job_id.to_owned(),
            sequence,
            status: CronOccurrenceStatus::Failed,
            session_id: Some(session_id),
            last_message: None,
            error: Some(error),
        },
        Ok((session_id, JobTurnResult::TimedOut)) => JobCompletion {
            job_id: job_id.to_owned(),
            sequence,
            status: CronOccurrenceStatus::TimedOut,
            session_id: Some(session_id),
            last_message: None,
            error: Some("the configured execution timeout elapsed".into()),
        },
        Err(error) => JobCompletion {
            job_id: job_id.to_owned(),
            sequence,
            status: CronOccurrenceStatus::Failed,
            session_id: created_session_id,
            last_message: None,
            error: Some(format!("{error:#}")),
        },
    }
}

enum JobTurnResult {
    Completed(String),
    Failed(String),
    TimedOut,
}

impl JobTurnResult {
    fn from_join(
        result: std::result::Result<
            Result<crate::conversation::ConversationTurn>,
            tokio::task::JoinError,
        >,
    ) -> Self {
        match result {
            Ok(Ok(turn)) => match turn.result {
                Ok(message) => Self::Completed(message),
                Err(error) => Self::Failed(format!("{error:#}")),
            },
            Ok(Err(error)) => Self::Failed(format!("{error:#}")),
            Err(error) => Self::Failed(format!("conversation task failed: {error}")),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_occurrence(
    completion: JobCompletion,
    state: &mut CronRuntimeState,
    active: &mut HashSet<String>,
    queued: &mut HashMap<String, u64>,
    store: &CronStore,
    coordinator: &SessionCoordinator,
    completion_tx: &mpsc::UnboundedSender<JobCompletion>,
) -> Result<()> {
    let now = Utc::now();
    let job_state = state.jobs.entry(completion.job_id.clone()).or_default();
    if let Some(occurrence) = job_state
        .occurrences
        .iter_mut()
        .find(|occurrence| occurrence.sequence == completion.sequence)
    {
        occurrence.status = completion.status;
        occurrence.completed_at = Some(now);
        occurrence.session_id = completion.session_id;
        occurrence.last_message = completion.last_message;
        occurrence.error = completion.error;
    }
    active.remove(&completion.job_id);
    if let Ok(document) = store.load_document()
        && let Some(job) = document.jobs.get(&completion.job_id)
        && matches!(parse_schedule(&job.schedule), Ok(ParsedSchedule::At(_)))
    {
        job_state.one_time_status = Some(one_time_status_for(completion.status));
    }

    if let Some(sequence) = queued.remove(&completion.job_id)
        && let Some(occurrence) = job_state
            .occurrences
            .iter_mut()
            .find(|occurrence| occurrence.sequence == sequence)
    {
        let current_job = store.load_document().ok().and_then(|document| {
            document
                .jobs
                .get(&completion.job_id)
                .filter(|job| job.enabled)
                .cloned()
        });
        if let Some(job) = current_job {
            occurrence.status = CronOccurrenceStatus::Running;
            occurrence.started_at = Some(now);
            active.insert(completion.job_id.clone());
            spawn_job(
                &completion.job_id,
                sequence,
                occurrence.scheduled_at,
                &job,
                coordinator,
                completion_tx,
            );
        } else {
            occurrence.status = CronOccurrenceStatus::SkippedOverlap;
            occurrence.completed_at = Some(now);
            occurrence.error = Some(
                "job was deleted, paused, or unavailable before queued execution began".into(),
            );
        }
    }
    store.save_state(state)
}

fn one_time_status_for(status: CronOccurrenceStatus) -> OneTimeStatus {
    match status {
        CronOccurrenceStatus::Completed => OneTimeStatus::Completed,
        CronOccurrenceStatus::TimedOut => OneTimeStatus::TimedOut,
        CronOccurrenceStatus::Failed => OneTimeStatus::Failed,
        _ => OneTimeStatus::Pending,
    }
}

fn recover_interrupted_occurrences(state: &mut CronRuntimeState) -> bool {
    let now = Utc::now();
    let mut changed = false;
    for job in state.jobs.values_mut() {
        for occurrence in &mut job.occurrences {
            match occurrence.status {
                CronOccurrenceStatus::Running => {
                    changed = true;
                    occurrence.status = CronOccurrenceStatus::Failed;
                    occurrence.completed_at = Some(now);
                    occurrence.error = Some("the cron daemon stopped during this execution".into());
                }
                CronOccurrenceStatus::Queued => {
                    changed = true;
                    occurrence.status = CronOccurrenceStatus::SkippedOverlap;
                    occurrence.completed_at = Some(now);
                    occurrence.error =
                        Some("the cron daemon stopped before this queued execution began".into());
                }
                _ => {}
            }
        }
    }
    changed
}

fn parse_schedule(value: &str) -> Result<ParsedSchedule> {
    let value = value.trim();
    if value == "@reboot" {
        return Ok(ParsedSchedule::Reboot);
    }
    if let Some(at) = value.strip_prefix("@at ") {
        let at = DateTime::parse_from_rfc3339(at.trim())
            .context("@at must be followed by an RFC 3339 timestamp")?
            .with_timezone(&Utc);
        return Ok(ParsedSchedule::At(at));
    }
    let aliases = [
        "@hourly",
        "@daily",
        "@weekly",
        "@monthly",
        "@yearly",
        "@annually",
    ];
    if !aliases.contains(&value) && value.split_whitespace().count() != 5 {
        anyhow::bail!("cron expressions must contain five fields or a supported alias");
    }
    Ok(ParsedSchedule::Cron(Box::new(Cron::from_str(value)?)))
}

fn parse_time_zone(value: &str) -> Result<Tz> {
    value
        .parse::<Tz>()
        .with_context(|| format!("unknown IANA time zone {value:?}"))
}

fn validate_job_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!("job IDs may contain only letters, numbers, '-' and '_'");
    }
    Ok(())
}

fn resolved_schedule_path(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", path.display()));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let parent = if parent.exists() {
        parent
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", parent.display()))?
    } else {
        let absolute = if parent.is_absolute() {
            parent.to_owned()
        } else {
            std::env::current_dir()?.join(parent)
        };
        fs::create_dir_all(&absolute)
            .with_context(|| format!("cannot create {}", absolute.display()))?;
        absolute.canonicalize()?
    };
    let name = path
        .file_name()
        .context("cron file path must include a file name")?;
    Ok(parent.join(name))
}

fn runtime_directory_for(schedule_path: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    let identity = schedule_path.to_string_lossy().to_ascii_lowercase();
    #[cfg(not(windows))]
    let identity = schedule_path.to_string_lossy().into_owned();
    let hash = format!("{:x}", Sha256::digest(identity.as_bytes()));
    Ok(global_data_dir()?.join("cron").join(&hash[..24]))
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let mut temp_name = OsString::from(".");
    temp_name.push(
        path.file_name()
            .context("JSON path must include a file name")?,
    );
    temp_name.push(format!(".{}.tmp", Uuid::new_v4()));
    let temp = path.with_file_name(temp_name);
    fs::write(&temp, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("cannot write {}", temp.display()))?;
    if let Err(error) = replace_file(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error).with_context(|| format!("cannot replace {}", path.display()));
    }
    Ok(())
}

fn command_succeeded(command: &mut Command, action: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("cannot {action}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    anyhow::bail!(
        "cannot {action}: {}",
        if stderr.is_empty() { stdout } else { stderr }
    )
}

fn manual_autostart_instructions(executable: &Path, schedule: &Path) -> String {
    let manager = if cfg!(windows) {
        "Windows Task Scheduler"
    } else if cfg!(target_os = "linux") {
        "a systemd user service"
    } else if cfg!(target_os = "macos") {
        "a per-user LaunchAgent"
    } else {
        "your operating system's per-user service manager"
    };
    format!(
        "You can configure {manager} to start and supervise this command at login: \"{}\" cron \"{}\"",
        executable.display(),
        schedule.display()
    )
}

#[cfg(windows)]
fn platform_install(store: &CronStore, executable: &Path) -> Result<CronInstallation> {
    command_succeeded(
        Command::new("schtasks").args(["/Query", "/FO", "LIST"]),
        "query Windows Task Scheduler",
    )?;
    let name = format!("CodeCrab Cron {}", store.installation_id());
    let probe = Command::new("schtasks")
        .args(["/Query", "/TN", &name])
        .output()
        .context("cannot inspect the target Windows scheduled task")?;
    if probe.status.success() {
        anyhow::bail!("Windows scheduled task {name:?} already exists");
    }
    let task_command = format!(
        "\"{}\" cron \"{}\"",
        executable.display(),
        store.path().display()
    );
    let create = command_succeeded(
        Command::new("schtasks").args([
            "/Create",
            "/TN",
            &name,
            "/TR",
            &task_command,
            "/SC",
            "ONLOGON",
            "/RL",
            "LIMITED",
            "/F",
        ]),
        "create the Windows cron autostart task",
    );
    if let Err(error) = create {
        let _ = Command::new("schtasks")
            .args(["/Delete", "/TN", &name, "/F"])
            .output();
        return Err(error);
    }
    let configure_script = r#"$task = Get-ScheduledTask -TaskName $env:CODECRAB_CRON_TASK_NAME -ErrorAction Stop; $task.Settings.ExecutionTimeLimit = 'PT0S'; $task.Settings.MultipleInstances = 'IgnoreNew'; $task.Settings.RestartCount = 999; $task.Settings.RestartInterval = 'PT1M'; $task.Settings.DisallowStartIfOnBatteries = $false; $task.Settings.StopIfGoingOnBatteries = $false; Set-ScheduledTask -InputObject $task -ErrorAction Stop | Out-Null"#;
    if let Err(error) = command_succeeded(
        Command::new("powershell.exe")
            .env("CODECRAB_CRON_TASK_NAME", &name)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                configure_script,
            ]),
        "configure the Windows cron task for continuous execution",
    ) {
        let _ = Command::new("schtasks")
            .args(["/Delete", "/TN", &name, "/F"])
            .output();
        return Err(error).context("the partial Windows installation was rolled back");
    }
    if let Err(error) = command_succeeded(
        Command::new("schtasks").args(["/Run", "/TN", &name]),
        "start the Windows cron task",
    ) {
        let _ = Command::new("schtasks")
            .args(["/Delete", "/TN", &name, "/F"])
            .output();
        return Err(error).context("the partial Windows installation was rolled back");
    }
    Ok(CronInstallation {
        version: 1,
        method: "windows_task_scheduler".into(),
        name,
        artifact: None,
        executable: executable.to_owned(),
        schedule_path: store.path().to_owned(),
        installed_at: Utc::now(),
    })
}

#[cfg(windows)]
fn platform_registration_exists(installation: &CronInstallation) -> Result<bool> {
    Ok(Command::new("schtasks")
        .args(["/Query", "/TN", &installation.name])
        .output()
        .context("cannot query Windows Task Scheduler")?
        .status
        .success())
}

#[cfg(windows)]
fn platform_uninstall(store: &CronStore, installation: Option<&CronInstallation>) -> Result<()> {
    let name = installation
        .map(|installation| installation.name.clone())
        .unwrap_or_else(|| format!("CodeCrab Cron {}", store.installation_id()));
    let exists = Command::new("schtasks")
        .args(["/Query", "/TN", &name])
        .output()
        .context("cannot query Windows Task Scheduler during uninstall")?
        .status
        .success();
    if exists {
        command_succeeded(
            Command::new("schtasks").args(["/Delete", "/TN", &name, "/F"]),
            "delete the Windows cron task",
        )?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn platform_install(store: &CronStore, executable: &Path) -> Result<CronInstallation> {
    command_succeeded(
        Command::new("systemctl").args(["--user", "show-environment"]),
        "connect to the running systemd user manager",
    )?;
    let name = format!("codecrab-cron-{}.service", store.installation_id());
    let base = directories::BaseDirs::new().context("cannot locate the user config directory")?;
    let artifact = base.config_dir().join("systemd").join("user").join(&name);
    if artifact.exists() {
        anyhow::bail!("systemd user unit already exists: {}", artifact.display());
    }
    let contents = format!(
        "# Managed by CodeCrab for {}\n[Unit]\nDescription=CodeCrab scheduled agent tasks\n\n[Service]\nType=simple\nExecStart={} cron {}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        store.path().display(),
        systemd_quote(executable),
        systemd_quote(store.path()),
    );
    if let Some(parent) = artifact.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&artifact, contents)?;
    let setup = (|| {
        command_succeeded(
            Command::new("systemctl").args(["--user", "daemon-reload"]),
            "reload the systemd user manager",
        )?;
        command_succeeded(
            Command::new("systemctl").args(["--user", "enable", "--now", &name]),
            "enable the systemd user cron service",
        )
    })();
    if let Err(error) = setup {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", &name])
            .output();
        let _ = fs::remove_file(&artifact);
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        return Err(error).context("the partial systemd installation was rolled back");
    }
    Ok(CronInstallation {
        version: 1,
        method: "systemd_user".into(),
        name,
        artifact: Some(artifact),
        executable: executable.to_owned(),
        schedule_path: store.path().to_owned(),
        installed_at: Utc::now(),
    })
}

#[cfg(target_os = "linux")]
fn platform_registration_exists(installation: &CronInstallation) -> Result<bool> {
    Ok(Command::new("systemctl")
        .args(["--user", "is-enabled", &installation.name])
        .output()
        .context("cannot query the systemd user service")?
        .status
        .success()
        && installation
            .artifact
            .as_ref()
            .is_some_and(|path| path.is_file()))
}

#[cfg(target_os = "linux")]
fn platform_uninstall(store: &CronStore, installation: Option<&CronInstallation>) -> Result<()> {
    let name = installation
        .map(|installation| installation.name.clone())
        .unwrap_or_else(|| format!("codecrab-cron-{}.service", store.installation_id()));
    let artifact = installation
        .and_then(|installation| installation.artifact.clone())
        .or_else(|| {
            directories::BaseDirs::new()
                .map(|base| base.config_dir().join("systemd").join("user").join(&name))
        });
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", &name])
        .output();
    if let Some(artifact) = artifact
        && artifact.exists()
    {
        let contents = fs::read_to_string(&artifact).unwrap_or_default();
        if contents.contains("Managed by CodeCrab") {
            fs::remove_file(&artifact)?;
        } else {
            anyhow::bail!(
                "refusing to delete unrecognized unit {}",
                artifact.display()
            );
        }
    }
    command_succeeded(
        Command::new("systemctl").args(["--user", "daemon-reload"]),
        "reload the systemd user manager after uninstall",
    )
}

#[cfg(target_os = "linux")]
fn systemd_quote(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    )
}

#[cfg(target_os = "macos")]
fn platform_install(store: &CronStore, executable: &Path) -> Result<CronInstallation> {
    let uid = String::from_utf8(Command::new("id").arg("-u").output()?.stdout)?
        .trim()
        .to_owned();
    command_succeeded(
        Command::new("launchctl").args(["print", &format!("gui/{uid}")]),
        "connect to the per-user launchd domain",
    )?;
    let label = format!("com.codecrab.cron.{}", store.installation_id());
    let base = directories::BaseDirs::new().context("cannot locate the user home directory")?;
    let artifact = base
        .home_dir()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{label}.plist"));
    if artifact.exists() {
        anyhow::bail!("launchd agent already exists: {}", artifact.display());
    }
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array><string>{}</string><string>cron</string><string>{}</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/></dict></plist>\n",
        xml_escape(&label),
        xml_escape(&executable.display().to_string()),
        xml_escape(&store.path().display().to_string()),
    );
    if let Some(parent) = artifact.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&artifact, xml)?;
    if let Err(error) = command_succeeded(
        Command::new("launchctl").args([
            "bootstrap",
            &format!("gui/{uid}"),
            artifact.to_string_lossy().as_ref(),
        ]),
        "register the launchd cron agent",
    ) {
        let _ = fs::remove_file(&artifact);
        return Err(error).context("the partial launchd installation was rolled back");
    }
    Ok(CronInstallation {
        version: 1,
        method: "launch_agent".into(),
        name: label,
        artifact: Some(artifact),
        executable: executable.to_owned(),
        schedule_path: store.path().to_owned(),
        installed_at: Utc::now(),
    })
}

#[cfg(target_os = "macos")]
fn platform_registration_exists(installation: &CronInstallation) -> Result<bool> {
    let uid = String::from_utf8(Command::new("id").arg("-u").output()?.stdout)?
        .trim()
        .to_owned();
    Ok(Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{}", installation.name)])
        .output()?
        .status
        .success()
        && installation
            .artifact
            .as_ref()
            .is_some_and(|path| path.is_file()))
}

#[cfg(target_os = "macos")]
fn platform_uninstall(store: &CronStore, installation: Option<&CronInstallation>) -> Result<()> {
    let label = installation
        .map(|value| value.name.clone())
        .unwrap_or_else(|| format!("com.codecrab.cron.{}", store.installation_id()));
    let uid = String::from_utf8(Command::new("id").arg("-u").output()?.stdout)?
        .trim()
        .to_owned();
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{label}")])
        .output();
    let artifact = installation
        .and_then(|value| value.artifact.clone())
        .or_else(|| {
            directories::BaseDirs::new().map(|base| {
                base.home_dir()
                    .join("Library")
                    .join("LaunchAgents")
                    .join(format!("{label}.plist"))
            })
        });
    if let Some(artifact) = artifact
        && artifact.exists()
    {
        let contents = fs::read_to_string(&artifact).unwrap_or_default();
        if contents.contains(&label) {
            fs::remove_file(&artifact)?;
        } else {
            anyhow::bail!(
                "refusing to delete unrecognized launchd file {}",
                artifact.display()
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_install(_store: &CronStore, _executable: &Path) -> Result<CronInstallation> {
    anyhow::bail!("cron autostart is unsupported on this operating system")
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_registration_exists(_installation: &CronInstallation) -> Result<bool> {
    Ok(false)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_uninstall(_store: &CronStore, _installation: Option<&CronInstallation>) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn job(schedule: &str) -> CronJob {
        CronJob {
            schedule: schedule.into(),
            enabled: true,
            project: std::env::current_dir().unwrap(),
            prompt: "Run the report".into(),
            provider: "openai".into(),
            model: "gpt-test".into(),
            reasoning: Some("high".into()),
            speed: Some("fast".into()),
            timezone: None,
            overlap: OverlapPolicy::Skip,
            timeout_seconds: None,
            source_session_id: None,
        }
    }

    #[test]
    fn document_uses_jobs_keyed_by_id_and_rejects_unknown_fields() {
        let project = std::env::current_dir().unwrap();
        let document: CronDocument = serde_json::from_value(serde_json::json!({
            "version": 1,
            "timezone": "Europe/Madrid",
            "jobs": {
                "weekly-report": {
                    "schedule": "0 3 * * 2",
                    "project": project,
                    "prompt": "Run the report",
                    "provider": "openai",
                    "model": "gpt-test"
                }
            }
        }))
        .unwrap();
        document.validate().unwrap();
        assert!(document.jobs.contains_key("weekly-report"));

        let error = serde_json::from_value::<CronDocument>(serde_json::json!({
            "version": 1,
            "timezone": "UTC",
            "unknown": true,
            "jobs": {}
        }))
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn cron_is_five_field_and_at_is_absolute() {
        assert!(parse_schedule("0 3 * * 2").is_ok());
        assert!(parse_schedule("@weekly").is_ok());
        assert!(matches!(
            parse_schedule("@reboot").unwrap(),
            ParsedSchedule::Reboot
        ));
        assert!(parse_schedule("0 0 3 * * 2").is_err());
        assert!(parse_schedule("@at 2030-01-02T03:04:05+01:00").is_ok());
        assert!(parse_schedule("@at tomorrow").is_err());
        let mut relative = job("@daily");
        relative.project = PathBuf::from("relative-project");
        assert!(relative.validate("UTC").is_err());
    }

    #[test]
    fn next_occurrences_honor_the_named_timezone() {
        let after = DateTime::parse_from_rfc3339("2026-08-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = next_occurrences(&job("0 3 * * *"), "Europe/Madrid", after, 2).unwrap();
        assert_eq!(next[0].to_rfc3339(), "2026-08-02T01:00:00+00:00");
        assert_eq!(next[1].to_rfc3339(), "2026-08-03T01:00:00+00:00");
    }

    #[test]
    fn daylight_saving_gaps_are_skipped_and_repeated_times_run_once() {
        let before_spring_gap = DateTime::parse_from_rfc3339("2026-03-28T23:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let spring =
            next_occurrences(&job("30 2 * * *"), "Europe/Madrid", before_spring_gap, 1).unwrap();
        assert_eq!(spring[0].to_rfc3339(), "2026-03-30T00:30:00+00:00");

        let before_fall_repeat = DateTime::parse_from_rfc3339("2026-10-24T23:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let fall =
            next_occurrences(&job("30 2 * * *"), "Europe/Madrid", before_fall_repeat, 2).unwrap();
        assert_eq!(fall[0].to_rfc3339(), "2026-10-25T00:30:00+00:00");
        assert_eq!(fall[1].to_rfc3339(), "2026-10-26T01:30:00+00:00");
    }

    #[test]
    fn daemon_lock_is_released_with_its_file_handle() {
        let temp = TempDir::new().unwrap();
        let store = CronStore {
            schedule_path: temp.path().join("cron.json"),
            runtime_dir: temp.path().join("runtime"),
            mutation_lock: Arc::new(Mutex::new(())),
        };
        let first = store.try_daemon_lock().unwrap();
        assert_eq!(store.daemon_status().unwrap(), CronDaemonStatus::Running);
        assert!(store.try_daemon_lock().is_err());
        drop(first);
        assert_eq!(store.daemon_status().unwrap(), CronDaemonStatus::Stopped);
    }

    #[test]
    fn a_direct_execution_lock_is_not_reported_as_a_persistent_daemon() {
        let temp = TempDir::new().unwrap();
        let store = CronStore::at(temp.path().join("cron.json"), temp.path().join("runtime"));
        let _direct = store.try_direct_run_lock().unwrap();
        let _daemon_guard = store.try_daemon_lock().unwrap();

        assert_eq!(store.daemon_status().unwrap(), CronDaemonStatus::Stopped);
    }

    #[tokio::test]
    async fn daemon_lock_does_not_block_atomic_schedule_replacement() {
        let temp = TempDir::new().unwrap();
        let store = CronStore::at(temp.path().join("cron.json"), temp.path().join("runtime"));
        store.load_or_create().unwrap();
        let _lock = store.try_daemon_lock().unwrap();
        let mut document = CronDocument::default();
        document.jobs.insert("report".into(), job("@daily"));

        store.save_document(&document).await.unwrap();

        assert!(store.load_document().unwrap().jobs.contains_key("report"));
        assert_eq!(store.daemon_status().unwrap(), CronDaemonStatus::Running);
    }

    #[test]
    fn missed_occurrences_are_omitted_and_overdue_one_time_jobs_expire() {
        let now = DateTime::parse_from_rfc3339("2026-08-02T12:00:30Z")
            .unwrap()
            .with_timezone(&Utc);
        let after = DateTime::parse_from_rfc3339("2026-08-02T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let due = due_occurrences(&job("* * * * *"), "UTC", after, now).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].to_rfc3339(), "2026-08-02T12:00:00+00:00");

        let mut state = CronRuntimeState::default();
        let one_time = job("@at 2026-08-02T11:00:00Z");
        expire_one_time_if_needed("late", &one_time, now, &mut state).unwrap();
        assert_eq!(
            state.jobs["late"].one_time_status,
            Some(OneTimeStatus::Expired)
        );
        assert!(state.jobs["late"].occurrences.is_empty());
    }

    #[test]
    fn interrupted_running_and_queued_occurrences_are_terminal_after_restart() {
        let now = Utc::now();
        let mut state = CronRuntimeState::default();
        state.jobs.insert(
            "report".into(),
            CronJobState {
                next_sequence: 3,
                one_time_status: None,
                occurrences: vec![
                    CronOccurrence {
                        sequence: 1,
                        scheduled_at: now,
                        status: CronOccurrenceStatus::Running,
                        started_at: Some(now),
                        completed_at: None,
                        session_id: None,
                        last_message: None,
                        error: None,
                        manual: false,
                    },
                    CronOccurrence {
                        sequence: 2,
                        scheduled_at: now,
                        status: CronOccurrenceStatus::Queued,
                        started_at: None,
                        completed_at: None,
                        session_id: None,
                        last_message: None,
                        error: None,
                        manual: false,
                    },
                ],
            },
        );

        recover_interrupted_occurrences(&mut state);

        let occurrences = &state.jobs["report"].occurrences;
        assert_eq!(occurrences[0].status, CronOccurrenceStatus::Failed);
        assert_eq!(occurrences[1].status, CronOccurrenceStatus::SkippedOverlap);
        assert!(occurrences.iter().all(|run| run.completed_at.is_some()));
    }

    #[test]
    fn proposal_token_covers_every_job_setting() {
        let first = job("@daily");
        let mut changed = first.clone();
        changed.prompt = "A different task".into();
        assert_eq!(
            proposal_token("report", &first, None).unwrap(),
            proposal_token("report", &first, None).unwrap()
        );
        assert_ne!(
            proposal_token("report", &first, None).unwrap(),
            proposal_token("report", &changed, None).unwrap()
        );
    }

    #[tokio::test]
    async fn confirmation_tokens_cannot_mutate_a_job_that_changed_after_preview() {
        let temp = TempDir::new().unwrap();
        let store = CronStore::at(temp.path().join("cron.json"), temp.path().join("runtime"));
        let original = job("@daily");
        store.upsert("report", original.clone()).await.unwrap();
        let delete_token = action_token("delete", "report", &original).unwrap();
        let proposed = job("@weekly");
        let schedule_token = proposal_token("report", &proposed, Some(&original)).unwrap();
        let mut changed = original;
        changed.prompt = "Changed after preview".into();
        store.upsert("report", changed).await.unwrap();

        assert!(
            store
                .delete_confirmed("report", &delete_token)
                .await
                .is_err()
        );
        assert!(store.load_document().unwrap().jobs.contains_key("report"));
        assert!(
            store
                .upsert_confirmed("report", proposed, &schedule_token)
                .await
                .is_err()
        );
    }

    #[test]
    fn queue_overlap_keeps_only_the_newest_pending_occurrence() {
        let temp = TempDir::new().unwrap();
        let coordinator = SessionCoordinator::new(
            Config::test("gpt-test", "http://127.0.0.1:1/v1"),
            SessionRegistry::at(temp.path().join("global-config.toml")),
            DebugOutput::default(),
            DiagnosticLog::stderr(),
            temp.path().to_path_buf(),
            temp.path().join("AGENTS.md"),
        );
        let mut recurring = job("* * * * *");
        recurring.overlap = OverlapPolicy::Queue;
        let mut state = CronRuntimeState::default();
        let mut active = HashSet::from(["report".to_owned()]);
        let mut queued = HashMap::new();
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
        let first_at = Utc::now();
        let second_at = first_at + chrono::Duration::minutes(1);

        schedule_occurrence(
            "report",
            &recurring,
            first_at,
            false,
            &mut state,
            &mut active,
            &mut queued,
            &coordinator,
            &completion_tx,
        )
        .unwrap();
        schedule_occurrence(
            "report",
            &recurring,
            second_at,
            false,
            &mut state,
            &mut active,
            &mut queued,
            &coordinator,
            &completion_tx,
        )
        .unwrap();

        let occurrences = &state.jobs["report"].occurrences;
        assert_eq!(occurrences[0].status, CronOccurrenceStatus::SkippedOverlap);
        assert_eq!(occurrences[1].status, CronOccurrenceStatus::Queued);
        assert_eq!(queued["report"], occurrences[1].sequence);
        assert!(
            occurrences[0]
                .error
                .as_deref()
                .unwrap()
                .contains("newer queued occurrence")
        );
    }

    #[tokio::test]
    async fn queued_occurrence_is_skipped_when_job_is_paused_before_it_starts() {
        let temp = TempDir::new().unwrap();
        let store = CronStore::at(temp.path().join("cron.json"), temp.path().join("runtime"));
        let mut recurring = job("* * * * *");
        recurring.enabled = false;
        store.upsert("report", recurring).await.unwrap();
        let coordinator = SessionCoordinator::new(
            Config::test("gpt-test", "http://127.0.0.1:1/v1"),
            SessionRegistry::at(temp.path().join("global-config.toml")),
            DebugOutput::default(),
            DiagnosticLog::stderr(),
            temp.path().to_path_buf(),
            temp.path().join("AGENTS.md"),
        );
        let now = Utc::now();
        let mut state = CronRuntimeState::default();
        state.jobs.insert(
            "report".into(),
            CronJobState {
                next_sequence: 3,
                one_time_status: None,
                occurrences: vec![
                    CronOccurrence {
                        sequence: 1,
                        scheduled_at: now,
                        status: CronOccurrenceStatus::Running,
                        started_at: Some(now),
                        completed_at: None,
                        session_id: None,
                        last_message: None,
                        error: None,
                        manual: false,
                    },
                    CronOccurrence {
                        sequence: 2,
                        scheduled_at: now + chrono::Duration::minutes(1),
                        status: CronOccurrenceStatus::Queued,
                        started_at: None,
                        completed_at: None,
                        session_id: None,
                        last_message: None,
                        error: None,
                        manual: false,
                    },
                ],
            },
        );
        let mut active = HashSet::from(["report".to_owned()]);
        let mut queued = HashMap::from([("report".to_owned(), 2)]);
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();

        finish_occurrence(
            JobCompletion {
                job_id: "report".into(),
                sequence: 1,
                status: CronOccurrenceStatus::Completed,
                session_id: None,
                last_message: Some("done".into()),
                error: None,
            },
            &mut state,
            &mut active,
            &mut queued,
            &store,
            &coordinator,
            &completion_tx,
        )
        .unwrap();

        let occurrences = &state.jobs["report"].occurrences;
        assert_eq!(occurrences[0].status, CronOccurrenceStatus::Completed);
        assert_eq!(occurrences[1].status, CronOccurrenceStatus::SkippedOverlap);
        assert!(occurrences[1].completed_at.is_some());
        assert!(occurrences[1].error.as_deref().unwrap().contains("paused"));
        assert!(!active.contains("report"));
        assert!(!queued.contains_key("report"));
    }
}
