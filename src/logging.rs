use std::sync::{Arc, Mutex, OnceLock, RwLock};

pub trait LogProvider: Send + Sync + 'static {
    fn log(&self, msg: &str);

    fn progress(&self, _position: u64, _length: u64) {}
}

struct StdoutLogProvider;

impl LogProvider for StdoutLogProvider {
    fn log(&self, msg: &str) {
        println!("{msg}");
    }
}

static LOG_PROVIDER: OnceLock<RwLock<Arc<dyn LogProvider>>> = OnceLock::new();

fn provider() -> &'static RwLock<Arc<dyn LogProvider>> {
    LOG_PROVIDER.get_or_init(|| RwLock::new(Arc::new(StdoutLogProvider)))
}

pub fn set_log_provider(log_provider: Arc<dyn LogProvider>) {
    if let Ok(mut provider) = provider().write() {
        *provider = log_provider;
    }
}

pub fn reset_log_provider_to_stdout() {
    set_log_provider(Arc::new(StdoutLogProvider));
}

pub fn emit_log(msg: &str) {
    if let Ok(provider) = provider().read() {
        provider.log(msg);
    }
}

pub fn emit_progress(position: u64, length: u64) {
    if let Ok(provider) = provider().read() {
        provider.progress(position, length);
    }
}

// log macros to check if log and log channel is enabled before performing potentially expensive string formatting
macro_rules! log {
    ($log:expr, $($arg:tt)*) => {
        $log.log(&format!($($arg)*));
    };
}
macro_rules! verbose {
    ($log:expr, $($arg:tt)*) => {
        if $log.verbose_enabled() {
            $log.log(&format!($($arg)*));
        }
    };
}
macro_rules! debug {
    ($log:expr, $($arg:tt)*) => {
        if $log.debug_enabled() {
            $log.log(&format!($($arg)*));
        }
    };
}

pub(crate) use debug;
pub(crate) use log;
pub(crate) use verbose;

pub(crate) struct Log {
    verbose: bool,
    debug: bool,
    progress: Arc<Mutex<Option<indicatif::ProgressBar>>>,
}
impl Log {
    pub(crate) fn new(verbose: bool, debug: bool) -> Self {
        Self {
            verbose,
            debug,
            progress: Default::default(),
        }
    }
    pub(crate) fn set_progress(&self, progress: Option<&indicatif::ProgressBar>) {
        *self.progress.lock().unwrap() = progress.cloned();
    }
    pub(crate) fn allow_stdout(&self) -> bool {
        true
    }
    pub(crate) fn log(&self, msg: &str) {
        if let Some(progress) = self.progress.lock().unwrap().as_ref() {
            progress.println(msg);
        } else {
            emit_log(msg);
        }
    }
    pub(crate) fn verbose_enabled(&self) -> bool {
        self.verbose
    }
    pub(crate) fn debug_enabled(&self) -> bool {
        self.debug
    }
}
