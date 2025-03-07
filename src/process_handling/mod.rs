use crate::config::Color;
use crate::generate_tracemap;
use crate::statemachine::{create_state_machine, TestState};
use crate::traces::*;
use crate::{Config, EventLog, LineAnalysis, RunError, TestBinary, TraceEngine};
use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use tracing::{error, info, trace_span};

/// Handle to a test currently either PID or a `std::process::Child`
pub enum TestHandle {
    Id(ProcessHandle),
    Process(RunningProcessHandle),
}

pub struct RunningProcessHandle {
    /// Used to map coverage counters to line numbers
    pub(crate) path: PathBuf,
    /// Get the exit status to work out if tests have passed
    pub(crate) child: Child,
    /// maintain a list of existing profraws in the project root to avoid picking up old results
    pub(crate) existing_profraws: Vec<PathBuf>,
}

impl RunningProcessHandle {
    pub fn new(path: PathBuf, cmd: &mut Command, config: &Config) -> Result<Self, RunError> {
        let child = cmd.spawn()?;
        let existing_profraws = fs::read_dir(config.root())?
            .into_iter()
            .filter_map(|x| x.ok())
            .filter(|x| x.path().is_file() && x.path().extension() == Some(OsStr::new("profraw")))
            .map(|x| x.path())
            .collect();

        Ok(Self {
            path,
            child,
            existing_profraws,
        })
    }
}

impl fmt::Display for TestHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TestHandle::Id(id) => write!(f, "{}", id),
            TestHandle::Process(c) => write!(f, "{}", c.child.id()),
        }
    }
}

impl From<ProcessHandle> for TestHandle {
    fn from(handle: ProcessHandle) -> Self {
        Self::Id(handle)
    }
}

impl From<RunningProcessHandle> for TestHandle {
    fn from(handle: RunningProcessHandle) -> Self {
        Self::Process(handle)
    }
}

pub fn get_test_coverage(
    test: &TestBinary,
    analysis: &HashMap<PathBuf, LineAnalysis>,
    config: &Config,
    ignored: bool,
    logger: &Option<EventLog>,
) -> Result<Option<(TraceMap, i32)>, RunError> {
    let handle = launch_test(test, config, ignored, logger)?;
    if let Some(handle) = handle {
        match collect_coverage(test.path(), handle, analysis, config, logger) {
            Ok(t) => Ok(Some(t)),
            Err(e) => Err(RunError::TestCoverage(e.to_string())),
        }
    } else {
        Ok(None)
    }
}

fn launch_test(
    test: &TestBinary,
    config: &Config,
    ignored: bool,
    logger: &Option<EventLog>,
) -> Result<Option<TestHandle>, RunError> {
    if let Some(log) = logger.as_ref() {
        log.push_binary(test.clone());
    }
    match config.engine() {
        TraceEngine::Ptrace => {
            cfg_if::cfg_if! {
                if #[cfg(target_os="linux")] {
                    linux::get_test_coverage(test, config, ignored)
                } else {
                    error!("Ptrace is not supported on this platform");
                    Err(RunError::TestCoverage("Unsupported OS".to_string()))
                }
            }
        }
        TraceEngine::Llvm => {
            let res = execute_test(test, ignored, config)?;
            Ok(Some(res))
        }
        e => {
            error!(
                "Tarpaulin cannot execute tests with {:?} on this platform",
                e
            );
            Err(RunError::TestCoverage("Unsupported OS".to_string()))
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(target_os= "linux")] {
        pub mod linux;
        pub use linux::*;

        pub mod breakpoint;
        pub mod ptrace_control;

        pub type ProcessHandle = nix::unistd::Pid;
    } else {
        pub type ProcessHandle = u64;
    }
}

/// Collects the coverage data from the launched test
pub(crate) fn collect_coverage(
    test_path: &Path,
    test: TestHandle,
    analysis: &HashMap<PathBuf, LineAnalysis>,
    config: &Config,
    logger: &Option<EventLog>,
) -> Result<(TraceMap, i32), RunError> {
    let mut ret_code = 0;
    let mut traces = generate_tracemap(test_path, analysis, config)?;
    {
        let span = trace_span!("Collect coverage", pid=%test);
        let _enter = span.enter();
        let (mut state, mut data) =
            create_state_machine(test, &mut traces, analysis, config, logger);
        loop {
            state = state.step(&mut data, config)?;
            if state.is_finished() {
                if let TestState::End(i) = state {
                    ret_code = i;
                }
                break;
            }
        }
    }
    Ok((traces, ret_code))
}

/// Launches the test executable
fn execute_test(test: &TestBinary, ignored: bool, config: &Config) -> Result<TestHandle, RunError> {
    info!("running {}", test.path().display());
    let _ = match test.manifest_dir() {
        Some(md) => env::set_current_dir(&md),
        None => env::set_current_dir(&config.root()),
    };

    let mut envars: Vec<(String, String)> = Vec::new();

    for (key, value) in env::vars() {
        envars.push((key.to_string(), value.to_string()));
    }
    let mut argv = vec![];
    if ignored {
        argv.push("--ignored".to_string());
    }
    if config.verbose {
        envars.push(("RUST_BACKTRACE".to_string(), "1".to_string()));
    }
    argv.extend_from_slice(&config.varargs);
    if config.color != Color::Auto {
        argv.push("--color".to_string());
        argv.push(config.color.to_string().to_ascii_lowercase());
    }

    if let Some(s) = test.pkg_name() {
        envars.push(("CARGO_PKG_NAME".to_string(), s.to_string()));
    }
    if let Some(s) = test.pkg_version() {
        envars.push(("CARGO_PKG_VERSION".to_string(), s.to_string()));
    }
    if let Some(s) = test.pkg_authors() {
        envars.push(("CARGO_PKG_AUTHORS".to_string(), s.join(":")));
    }
    if let Some(s) = test.manifest_dir() {
        envars.push(("CARGO_MANIFEST_DIR".to_string(), s.display().to_string()));
    }
    match config.engine() {
        TraceEngine::Llvm => {
            // Used for llvm coverage to avoid report naming clashes TODO could have clashes
            // between runs
            envars.push((
                "LLVM_PROFILE_FILE".to_string(),
                format!("{}_%p.profraw", test.file_name()),
            ));
            let mut child = Command::new(test.path());
            child.envs(envars).args(&argv);

            let hnd = RunningProcessHandle::new(test.path().to_path_buf(), &mut child, config)?;
            Ok(hnd.into())
        }
        #[cfg(target_os = "linux")]
        TraceEngine::Ptrace => execute(test.path(), &argv, envars.as_slice()),
        e => Err(RunError::Engine(format!(
            "invalid execution engine {:?}",
            e
        ))),
    }
}
