use std::time::Duration;

pub(crate) const INITIAL_OBSERVATION: Duration = Duration::from_secs(5);
pub(crate) const SCREEN_STABILITY: Duration = Duration::from_millis(250);
pub(crate) const FOLLOW_UP_DEADLINE: Duration = Duration::from_secs(5);
pub(crate) const DEFAULT_COLUMNS: u16 = 120;
pub(crate) const DEFAULT_ROWS: u16 = 40;
pub(crate) const MAX_TERMINALS_PER_SESSION: usize = 8;
pub(crate) const SCROLLBACK_BYTES: usize = 1024 * 1024;
pub(crate) const MODEL_TRANSCRIPT_TAIL: usize = 30_000;

pub(super) const READER_BUFFER_BYTES: usize = 16 * 1024;
pub(super) const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(25);
pub(super) const SCROLLBACK_ROWS: usize = SCROLLBACK_BYTES / 80;
