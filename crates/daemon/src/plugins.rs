//! `trafficview-plugin-v1` 宿主：发现、调度、超时、退避与故障隔离。
//!
//! 插件清单位于 `$XDG_DATA_HOME/trafficview/plugins/<id>/plugin.toml`。
//! 每个插件在独立 OS 线程中运行，通过行分隔 JSON 与宿主通信；任一插件
//! 崩溃、超时或输出垃圾都不会阻塞内置采集与其他插件。

use std::{
    collections::BTreeMap,
    env, fmt, fs, io,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex, MutexGuard,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use tracing::{debug, info, warn};
use trafficview_plugin_proto::{
    decode_response_for, encode_request, valid_id, PluginRequest, ProtoError, MAX_FRAME_BYTES,
};

/// 插件周期下限，避免子进程被过度调度。
pub const PLUGIN_INTERVAL_MIN_MS: u32 = 500;
/// 单次请求的等待上限；超时即 kill 子进程。
pub const PLUGIN_TIMEOUT: Duration = Duration::from_secs(2);
/// 连续失败达到该次数后禁用，直到显式 ReloadConfig。
pub const MAX_CONSECUTIVE_FAILURES: u32 = 3;

fn default_interval_ms() -> u32 {
    1000
}

/// `plugin.toml` 的模型；`command` 必须是绝对路径且直接 exec。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub id: String,
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl PluginManifest {
    /// 拒绝非法 id、id 与目录不一致、相对 command 与过短周期。
    pub fn validate(&self, dir_name: &str) -> Result<(), String> {
        if !valid_id(&self.id) {
            return Err(format!(
                "plugin id '{}' must match [A-Za-z0-9_-]{{1,64}}",
                self.id
            ));
        }
        if self.id != dir_name {
            return Err(format!(
                "plugin id '{}' does not match directory '{dir_name}'",
                self.id
            ));
        }
        if !self.command.is_absolute() {
            return Err(format!(
                "command '{}' must be an absolute path",
                self.command.display()
            ));
        }
        if self.interval_ms < PLUGIN_INTERVAL_MIN_MS {
            return Err(format!(
                "interval_ms {} is below the {} ms minimum",
                self.interval_ms, PLUGIN_INTERVAL_MIN_MS
            ));
        }
        Ok(())
    }
}

/// 一次发现的结果；单个清单损坏不影响其他插件。
#[derive(Debug, Default)]
pub struct PluginDiscovery {
    pub manifests: Vec<PluginManifest>,
    pub errors: Vec<String>,
}

/// 扫描插件根目录；目录不存在时返回空结果。
pub fn discover(root: &Path) -> PluginDiscovery {
    let mut discovery = PluginDiscovery::default();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return discovery,
        Err(error) => {
            discovery
                .errors
                .push(format!("cannot read {}: {error}", root.display()));
            return discovery;
        }
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let name = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let text = match fs::read_to_string(dir.join("plugin.toml")) {
            Ok(text) => text,
            Err(error) => {
                discovery
                    .errors
                    .push(format!("{name}: cannot read plugin.toml: {error}"));
                continue;
            }
        };
        let manifest: PluginManifest = match toml::from_str(&text) {
            Ok(manifest) => manifest,
            Err(error) => {
                discovery
                    .errors
                    .push(format!("{name}: invalid plugin.toml: {error}"));
                continue;
            }
        };
        match manifest.validate(&name) {
            Ok(()) => discovery.manifests.push(manifest),
            Err(error) => discovery.errors.push(format!("{name}: {error}")),
        }
    }
    discovery
}

/// `$XDG_DATA_HOME/trafficview/plugins`，回退到 `$HOME/.local/share/...`。
pub fn default_root() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir).join("trafficview").join("plugins"));
    }
    let home = env::var_os("HOME").filter(|value| !value.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("trafficview")
            .join("plugins"),
    )
}

/// 一次成功调用的输出；键已带 `<plugin-id>.` 前缀。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginOutput {
    pub values: BTreeMap<String, String>,
}

/// 插件调用失败原因；宿主据此更新 CollectorStatus。
#[derive(Debug)]
pub enum PluginRunError {
    Spawn(io::Error),
    Write(io::Error),
    Io(io::Error),
    Timeout,
    Exit(Option<i32>),
    Protocol(ProtoError),
}

impl fmt::Display for PluginRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "cannot spawn plugin: {error}"),
            Self::Write(error) => write!(f, "cannot write plugin request: {error}"),
            Self::Io(error) => write!(f, "cannot read plugin response: {error}"),
            Self::Timeout => write!(f, "plugin timed out"),
            Self::Exit(code) => match code {
                Some(code) => write!(f, "plugin exited with status {code}"),
                None => write!(f, "plugin was terminated by a signal"),
            },
            Self::Protocol(error) => write!(f, "plugin protocol error: {error}"),
        }
    }
}

impl std::error::Error for PluginRunError {}

/// 发送一次请求并读取一帧响应；`timeout` 到期会 kill 子进程。
pub fn run_plugin_once(
    manifest: &PluginManifest,
    now_unix: f64,
    timeout: Duration,
) -> Result<PluginOutput, PluginRunError> {
    let request = PluginRequest {
        t: now_unix,
        interval_ms: manifest.interval_ms,
    };
    let line = encode_request(&request);
    let mut child = Command::new(&manifest.command)
        .args(&manifest.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(PluginRunError::Spawn)?;

    // stderr 必须在独立线程排空，否则插件写满管道后会死锁。
    let stderr_thread = child.stderr.take().map(|stderr| {
        let plugin = manifest.id.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if !line.is_empty() {
                    warn!(plugin = %plugin, "plugin stderr: {line}");
                }
            }
        })
    });

    let (tx, rx) = mpsc::sync_channel(1);
    let stdout_thread = child.stdout.take().map(|stdout| {
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut frame = Vec::new();
            // 多读一个字节即可判定超长帧，避免为垃圾输入无限分配。
            let result = reader
                .by_ref()
                .take(MAX_FRAME_BYTES as u64 + 1)
                .read_until(b'\n', &mut frame);
            let _ = tx.send(result.map(|_| frame));
        })
    });

    if let Some(mut stdin) = child.stdin.take() {
        let written = stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush());
        // 关闭 stdin 表示本次请求结束；插件通常在读到一行后回写响应。
        drop(stdin);
        if let Err(error) = written {
            // 插件可能在读取请求前就退出（EPIPE）；优先报告真实退出码。
            let status = reap_after_write_error(&mut child);
            join_thread(stdout_thread);
            join_thread(stderr_thread);
            return match status {
                Some(status) if !status.success() => Err(PluginRunError::Exit(status.code())),
                _ => Err(PluginRunError::Write(error)),
            };
        }
    }

    let received = rx.recv_timeout(timeout);
    // 协议为一次请求一帧：优先保留真实退出码；仅在插件仍存活时 kill，
    // 避免把已经正常退出的子进程误报为被信号终止。
    let mut status = child.try_wait().ok().flatten();
    if status.is_none() {
        let _ = child.kill();
        status = child.wait().ok();
    }
    join_thread(stdout_thread);
    join_thread(stderr_thread);

    match received {
        Ok(Ok(frame)) => {
            if frame.is_empty() {
                return match status {
                    Some(status) if !status.success() => Err(PluginRunError::Exit(status.code())),
                    _ => Err(PluginRunError::Protocol(ProtoError::EmptyFrame)),
                };
            }
            let values =
                decode_response_for(&frame, &manifest.id).map_err(PluginRunError::Protocol)?;
            Ok(PluginOutput { values })
        }
        Ok(Err(error)) => Err(PluginRunError::Io(error)),
        Err(RecvTimeoutError::Timeout) => Err(PluginRunError::Timeout),
        Err(RecvTimeoutError::Disconnected) => Err(PluginRunError::Io(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "plugin stdout reader stopped",
        ))),
    }
}

fn join_thread(handle: Option<JoinHandle<()>>) {
    if let Some(handle) = handle {
        let _ = handle.join();
    }
}

/// 写请求失败后短暂等待子进程自行退出，尽量拿到真实退出码；仍存活则 kill。
fn reap_after_write_error(child: &mut std::process::Child) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = child.kill();
                return child.wait().ok();
            }
            Err(_) => return None,
        }
    }
}

/// 单插件失败状态机：成功清零，失败累计，达到上限后禁用。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailureState {
    consecutive: u32,
    backoff_index: usize,
    disabled: bool,
}

impl FailureState {
    pub fn record_success(&mut self) {
        self.consecutive = 0;
        self.backoff_index = 0;
        self.disabled = false;
    }

    pub fn record_failure(&mut self, max_failures: u32) {
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive >= max_failures.max(1) {
            self.disabled = true;
        }
    }

    /// 按 `1/2/4/8/16/32/60` 秒序列返回下一次重试间隔。
    pub fn backoff(&mut self, schedule: &[Duration]) -> Duration {
        if schedule.is_empty() {
            return Duration::from_secs(60);
        }
        let delay = schedule[self.backoff_index.min(schedule.len() - 1)];
        self.backoff_index = self.backoff_index.saturating_add(1);
        delay
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    pub fn consecutive(&self) -> u32 {
        self.consecutive
    }
}

/// 宿主可调参数；测试注入更短的超时与退避。
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub timeout: Duration,
    pub backoff: Vec<Duration>,
    pub max_failures: u32,
    pub poll_interval: Duration,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            timeout: PLUGIN_TIMEOUT,
            backoff: [1, 2, 4, 8, 16, 32, 60]
                .into_iter()
                .map(Duration::from_secs)
                .collect(),
            max_failures: MAX_CONSECUTIVE_FAILURES,
            poll_interval: Duration::from_millis(100),
        }
    }
}

/// 单插件运行状态；映射为 `CollectorStatus`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginStatus {
    pub available: bool,
    pub reason: String,
}

#[derive(Default)]
struct Data {
    values: BTreeMap<String, BTreeMap<String, String>>,
    statuses: BTreeMap<String, PluginStatus>,
}

struct Shared {
    root: PathBuf,
    runtime: RuntimeConfig,
    enabled: AtomicBool,
    shutdown: AtomicBool,
    generation: AtomicU64,
    data: Mutex<Data>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Worker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

/// 后台监督器：按周期运行插件，将最新值发布给 daemon 调度器。
pub struct PluginSupervisor {
    shared: Arc<Shared>,
    control: Mutex<Option<JoinHandle<()>>>,
}

impl PluginSupervisor {
    /// 启动后台控制线程；`enabled=false` 时只等待 ReloadConfig。
    pub fn start(root: impl Into<PathBuf>, runtime: RuntimeConfig, enabled: bool) -> Self {
        let shared = Arc::new(Shared {
            root: root.into(),
            runtime,
            enabled: AtomicBool::new(enabled),
            shutdown: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            data: Mutex::new(Data::default()),
        });
        let control_shared = shared.clone();
        let control = thread::spawn(move || control_loop(control_shared));
        Self {
            shared,
            control: Mutex::new(Some(control)),
        }
    }

    /// 显式重载：重新发现清单、清零失败计数并允许重试。
    pub fn reload(&self, enabled: bool) {
        self.shared.enabled.store(enabled, Ordering::Relaxed);
        self.shared.generation.fetch_add(1, Ordering::Release);
    }

    /// 所有插件当前值的合并视图；不会阻塞调用方。
    pub fn published_values(&self) -> BTreeMap<String, String> {
        lock(&self.shared.data)
            .values
            .values()
            .flat_map(|values| {
                values
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
            })
            .collect()
    }

    /// 各插件可用性；不可用插件保留条目并给出原因。
    pub fn statuses(&self) -> BTreeMap<String, PluginStatus> {
        lock(&self.shared.data).statuses.clone()
    }

    /// 停止控制线程与全部插件；可重复调用。
    pub fn stop(&self) {
        self.shared.shutdown.store(true, Ordering::Release);
        if let Some(handle) = lock(&self.control).take() {
            let _ = handle.join();
        }
    }
}

impl Drop for PluginSupervisor {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.control.lock().ok().and_then(|mut slot| slot.take()) {
            let _ = handle.join();
        }
    }
}

fn control_loop(shared: Arc<Shared>) {
    let mut workers: Vec<Worker> = Vec::new();
    let mut applied = u64::MAX;
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        let generation = shared.generation.load(Ordering::Acquire);
        if generation != applied {
            stop_workers(&mut workers);
            {
                let mut data = lock(&shared.data);
                data.values.clear();
                data.statuses.clear();
            }
            applied = generation;
            if shared.enabled.load(Ordering::Relaxed) {
                let discovery = discover(&shared.root);
                for error in &discovery.errors {
                    warn!(%error, "ignoring plugin manifest");
                }
                for manifest in discovery.manifests {
                    info!(
                        plugin = %manifest.id,
                        interval_ms = manifest.interval_ms,
                        "starting metrics plugin"
                    );
                    workers.push(spawn_worker(shared.clone(), manifest));
                }
            } else {
                debug!("metrics plugins are disabled");
            }
        }
        thread::sleep(shared.runtime.poll_interval);
    }
    stop_workers(&mut workers);
}

fn spawn_worker(shared: Arc<Shared>, manifest: PluginManifest) -> Worker {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let handle = thread::spawn(move || worker_loop(shared, manifest, worker_stop));
    Worker {
        stop,
        handle: Some(handle),
    }
}

fn stop_workers(workers: &mut Vec<Worker>) {
    for worker in workers.iter() {
        worker.stop.store(true, Ordering::Release);
    }
    for worker in workers.iter_mut() {
        if let Some(handle) = worker.handle.take() {
            let _ = handle.join();
        }
    }
    workers.clear();
}

fn worker_loop(shared: Arc<Shared>, manifest: PluginManifest, stop: Arc<AtomicBool>) {
    let id = manifest.id.clone();
    set_status(&shared, &id, false, "starting");
    let mut failures = FailureState::default();
    while !stop.load(Ordering::Acquire) && !shared.shutdown.load(Ordering::Acquire) {
        match run_plugin_once(&manifest, unix_seconds(), shared.runtime.timeout) {
            Ok(output) => {
                failures.record_success();
                {
                    let mut data = lock(&shared.data);
                    data.values.insert(id.clone(), output.values);
                    data.statuses.insert(
                        id.clone(),
                        PluginStatus {
                            available: true,
                            reason: String::new(),
                        },
                    );
                }
                if sleep_interruptible(
                    Duration::from_millis(manifest.interval_ms as u64),
                    &stop,
                    &shared.shutdown,
                ) {
                    break;
                }
            }
            Err(error) => {
                failures.record_failure(shared.runtime.max_failures);
                let disabled = failures.is_disabled();
                let reason = if disabled {
                    format!(
                        "disabled after {} consecutive failures: {error}",
                        failures.consecutive()
                    )
                } else {
                    error.to_string()
                };
                warn!(plugin = %id, %error, disabled, "metrics plugin failed");
                {
                    let mut data = lock(&shared.data);
                    data.values.remove(&id);
                    data.statuses.insert(
                        id.clone(),
                        PluginStatus {
                            available: false,
                            reason,
                        },
                    );
                }
                if disabled {
                    break;
                }
                let delay = failures.backoff(&shared.runtime.backoff);
                if sleep_interruptible(delay, &stop, &shared.shutdown) {
                    break;
                }
            }
        }
    }
}

fn set_status(shared: &Shared, id: &str, available: bool, reason: impl Into<String>) {
    lock(&shared.data).statuses.insert(
        id.to_owned(),
        PluginStatus {
            available,
            reason: reason.into(),
        },
    );
}

/// 以 50ms 切片休眠，便于停止或重载及时生效；返回是否被打断。
fn sleep_interruptible(mut remaining: Duration, stop: &AtomicBool, shutdown: &AtomicBool) -> bool {
    const SLICE: Duration = Duration::from_millis(50);
    while !remaining.is_zero() {
        if stop.load(Ordering::Acquire) || shutdown.load(Ordering::Acquire) {
            return true;
        }
        let step = remaining.min(SLICE);
        thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
    stop.load(Ordering::Acquire) || shutdown.load(Ordering::Acquire)
}

fn unix_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}
