use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::backtrace::Backtrace;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

pub mod codecs;
pub mod platform;
pub mod telemetry;
pub use telemetry::*;

// --- Rotating File Logger (`logging.conf`) ---
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

impl LogLevel {
    pub fn from_str_loose(s: &str) -> Self {
        match s.trim().to_uppercase().as_str() {
            "TRACE" => LogLevel::Trace,
            "DEBUG" => LogLevel::Debug,
            "INFO" => LogLevel::Info,
            "WARN" | "WARNING" => LogLevel::Warn,
            "ERROR" | "ERR" => LogLevel::Error,
            _ => LogLevel::Info,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoggingConfig {
    pub level: LogLevel,
    pub log_file: PathBuf,
    pub max_files: usize,
    pub max_file_size_bytes: u64,
    pub format_template: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            log_file: PathBuf::from("rusty_h265_converter.log"),
            max_files: 5,
            max_file_size_bytes: 20 * 1024 * 1024, // 20 MB
            format_template:
                "YYYY-mm-dd HH:MM:SS.millisec [LOG_LEVEL] <log message> [<stacktrace if error>]"
                    .to_string(),
        }
    }
}

pub fn parse_size_to_bytes(raw: &str) -> Option<u64> {
    let s = raw.trim().to_uppercase();
    if let Some(num) = s.strip_suffix("GB").or_else(|| s.strip_suffix('G')) {
        let val: f64 = num.trim().parse().ok()?;
        Some((val * 1024.0 * 1024.0 * 1024.0).round() as u64)
    } else if let Some(num) = s.strip_suffix("MB").or_else(|| s.strip_suffix('M')) {
        let val: f64 = num.trim().parse().ok()?;
        Some((val * 1024.0 * 1024.0).round() as u64)
    } else if let Some(num) = s.strip_suffix("KB").or_else(|| s.strip_suffix('K')) {
        let val: f64 = num.trim().parse().ok()?;
        Some((val * 1024.0).round() as u64)
    } else if let Some(num) = s.strip_suffix('B') {
        num.trim().parse::<u64>().ok()
    } else {
        s.parse::<u64>().ok()
    }
}

impl LoggingConfig {
    pub fn load(path: &Path) -> Self {
        let Ok(content) = fs::read_to_string(path) else {
            return Self::default();
        };
        Self::from_conf_str(&content)
    }

    pub fn from_conf_str(content: &str) -> Self {
        let mut cfg = Self::default();
        for raw_line in content.lines() {
            let line = match raw_line.find('#') {
                Some(idx) => &raw_line[..idx],
                None => raw_line,
            }
            .trim();
            if line.is_empty() {
                continue;
            }
            let sep = if line.contains('=') { '=' } else { ':' };
            if let Some((k, v)) = line.split_once(sep) {
                let key = k
                    .trim()
                    .to_lowercase()
                    .replace(['-', ' '], "_");
                let val = unquote(v.trim()).trim_end_matches('.').to_string();
                match key.as_str() {
                    "log_level" | "default_log_level" | "level" => {
                        cfg.level = LogLevel::from_str_loose(&val);
                    }
                    "log_file" | "file" | "filepath" | "path" => {
                        if !val.is_empty() {
                            cfg.log_file = PathBuf::from(val);
                        }
                    }
                    "number_of_files" | "max_files" | "num_files" | "files" => {
                        if let Ok(n) = val.parse::<usize>() {
                            cfg.max_files = n.max(1);
                        }
                    }
                    "max_file_size" | "max_size_of_file" | "max_size" => {
                        if let Some(bytes) = parse_size_to_bytes(&val) {
                            cfg.max_file_size_bytes = bytes.max(1024);
                        }
                    }
                    "format" | "log_format" => {
                        if !val.is_empty() {
                            cfg.format_template = val;
                        }
                    }
                    _ => {}
                }
            }
        }
        cfg
    }
}

pub fn format_timestamp_now() -> (String, String) {
    if let Some(parts) = platform::local_time_parts() {
        let date_str = format!("{:04}-{:02}-{:02}", parts.year, parts.month, parts.day);
        let time_str = format!(
            "{:02}:{:02}:{:02}.{:03}",
            parts.hour, parts.minute, parts.second, parts.millis
        );
        return (date_str, time_str);
    }
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis();
    ("1970-01-01".to_string(), format!("00:00:00.{:03}", millis))
}

pub fn format_time_12h_millis_now() -> String {
    if let Some(parts) = platform::local_time_parts() {
        let is_pm = parts.hour >= 12;
        let hour_12 = match parts.hour % 12 {
            0 => 12,
            h => h,
        };
        let am_pm = if is_pm { "pm" } else { "am" };
        return format!(
            "{}:{:02}:{:02}.{:03} {}",
            hour_12, parts.minute, parts.second, parts.millis, am_pm
        );
    }
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis();
    format!("12:00:00.{:03} am", millis)
}

pub struct RotatingLogger {
    config: LoggingConfig,
    lock: Mutex<()>,
}

static GLOBAL_LOGGER: OnceLock<RotatingLogger> = OnceLock::new();

pub fn init_global_logger(config: LoggingConfig) {
    let _ = GLOBAL_LOGGER.set(RotatingLogger {
        config,
        lock: Mutex::new(()),
    });
}

impl RotatingLogger {
    fn rotate_if_needed(&self) {
        let cur_len = fs::metadata(&self.config.log_file)
            .map(|m| m.len())
            .unwrap_or(0);
        if cur_len < self.config.max_file_size_bytes {
            return;
        }

        if self.config.max_files <= 1 {
            let _ = File::create(&self.config.log_file);
            return;
        }

        // Shift .log.(N-2) -> .log.(N-1), ..., .log -> .log.1
        let base = self.config.log_file.display().to_string();
        let oldest = PathBuf::from(format!("{}.{}", base, self.config.max_files - 1));
        if oldest.exists() {
            let _ = fs::remove_file(&oldest);
        }

        for idx in (1..(self.config.max_files - 1)).rev() {
            let src = PathBuf::from(format!("{}.{}", base, idx));
            let dst = PathBuf::from(format!("{}.{}", base, idx + 1));
            if src.exists() {
                let _ = fs::rename(&src, &dst);
            }
        }

        let first_backup = PathBuf::from(format!("{}.1", base));
        if self.config.log_file.exists() {
            let _ = fs::rename(&self.config.log_file, &first_backup);
        }
    }

    pub fn format_line(&self, level: LogLevel, message: &str, stacktrace: Option<&str>) -> String {
        let (date_str, time_str) = format_timestamp_now();
        let st_str = match stacktrace {
            Some(st) if !st.trim().is_empty() => format!("\nStacktrace:\n{}", st.trim()),
            _ => String::new(),
        };

        let mut rendered = self.config.format_template.clone();
        rendered = rendered.replace("YYYY-mm-dd", &date_str);
        rendered = rendered.replace("HH:MM:SS.millisec", &time_str);
        rendered = rendered.replace("[LOG_LEVEL]", &format!("[{}]", level.as_str()));
        rendered = rendered.replace("<log message>", message);
        rendered = rendered.replace("[<stacktrace if error>]", &st_str);
        rendered.trim_end().to_string()
    }

    pub fn log(&self, level: LogLevel, message: &str, stacktrace: Option<&str>) {
        if level < self.config.level {
            return;
        }
        let line = self.format_line(level, message, stacktrace);
        if let Ok(_guard) = self.lock.lock() {
            if let Some(parent) = self.config.log_file.parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = fs::create_dir_all(parent);
                }
            }
            self.rotate_if_needed();
            if let Ok(mut f) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.config.log_file)
            {
                let _ = writeln!(f, "{}", line);
            }
        }
    }
}

pub fn log_event(level: LogLevel, msg: impl AsRef<str>) {
    global_metrics().inc_log_emitted(level.as_str());
    if let Some(logger) = GLOBAL_LOGGER.get() {
        logger.log(level, msg.as_ref(), None);
    }
}

pub fn log_error_with_bt(msg: impl AsRef<str>, bt: &str) {
    global_metrics().inc_log_emitted(LogLevel::Error.as_str());
    if let Some(logger) = GLOBAL_LOGGER.get() {
        logger.log(LogLevel::Error, msg.as_ref(), Some(bt));
    }
}

#[allow(clippy::too_many_arguments)]
pub fn log_structured_event(
    level: LogLevel,
    nodename: &str,
    host_os: &str,
    worker_group: &str,
    worker_thread: &str,
    mount_point: &str,
    task_id: Option<u64>,
    event: &str,
    fields: &[(&str, String)],
) {
    let line = format_structured_log_line(
        nodename,
        host_os,
        worker_group,
        worker_thread,
        mount_point,
        task_id,
        event,
        fields,
    );
    log_event(level, line);
}

// --- Verbose Logging Macro (writes to rotating log file AND prints to console if -v) ---
macro_rules! vlog {
    ($verbose:expr, $($arg:tt)*) => {{
        let msg = format!($($arg)*);
        log_event(LogLevel::Info, msg.trim());
        if $verbose {
            println!("{}", msg);
        }
    }};
}

// --- Failed Directories Logger (`failed_dirs.txt` / `broken_directories.journal`) ---
static FAILED_DIRS_REGISTRY: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
/// Directories that experienced a fatal directory-level error (e.g., `mkdir` failure or
/// insufficient mount free space) and therefore cannot accept any files. Kept strictly
/// separate from `FAILED_DIRS_REGISTRY` so a single corrupt input file never aborts the
/// remaining files in a directory.
static UNWRITABLE_DIRS_REGISTRY: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
static FAILED_DIRS_FILE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static GLOBAL_APP_CONFIG: OnceLock<AppConfig> = OnceLock::new();

pub fn init_global_app_config(config: AppConfig) {
    let _ = GLOBAL_APP_CONFIG.set(config);
}

pub fn count_eligible_video_files_recursive(dir: &Path) -> usize {
    if !dir.exists() || !dir.is_dir() {
        return 0;
    }
    let default_cfg;
    let cfg = match GLOBAL_APP_CONFIG.get() {
        Some(c) => c,
        None => {
            default_cfg = AppConfig::default();
            &default_cfg
        }
    };
    let walker = WalkDir::new(dir).into_iter().filter_entry(|e| {
        if e.depth() > 0 && e.file_type().is_dir() {
            !is_subdir_h265_named(e.path(), &cfg.OUTPUT_SUFFIX)
        } else {
            true
        }
    });
    walker
        .filter_map(|e| e.ok())
        .filter(|e| {
            let p = e.path();
            is_valid_video_file(p, cfg)
                && !relative_path_has_h265_dir_component(dir, p, &cfg.OUTPUT_SUFFIX)
        })
        .count()
}

fn strip_diagnostic_noise(s: &str) -> String {
    let mut kept_lines = Vec::new();
    for line in s.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Debug Path:")
            || trimmed.starts_with("Raw Bytes:")
            || trimmed.starts_with("Parent Info:")
            || trimmed.starts_with("Stack Trace:")
            || trimmed.starts_with("Stacktrace:")
        {
            break;
        }
        if !trimmed.is_empty() {
            kept_lines.push(trimmed);
        }
    }
    kept_lines.join(" ")
}

fn summarize_error_reason(context_msg: &str, err: &anyhow::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    let clean_ctx = strip_diagnostic_noise(context_msg);
    if !clean_ctx.is_empty() {
        parts.push(clean_ctx);
    }

    for cause in err.chain() {
        let cleaned = strip_diagnostic_noise(&cause.to_string());
        if !cleaned.is_empty() && !parts.iter().any(|p| p.contains(&cleaned)) {
            parts.push(cleaned);
        }
    }

    if parts.is_empty() {
        "Unknown error".to_string()
    } else {
        parts.join(" | Caused by: ")
    }
}

fn suggest_manual_corrective_action(
    source_dir: &Path,
    target_dir: Option<&Path>,
    context_msg: &str,
    reason: &str,
) -> String {
    let combined = format!("{} {}", context_msg, reason).to_lowercase();
    let src_str = source_dir.display().to_string();
    let tgt_str = target_dir
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<target_dir>".to_string());

    if combined.contains("failed to create target directory")
        || combined.contains("failed creating target directory")
    {
        if combined.contains("invalid argument") || combined.contains("os error 22") {
            format!(
                "Try creating the target directory manually (`mkdir -p \"{}\"`). If mkdir fails with 'Invalid argument' (os error 22) due to exFAT/filesystem directory-entry corruption, rename the source directory to \"{}_1\", create \"{}_1\", and remove the corresponding line(s) from broken_files.journal (or repair the drive with `chkdsk /f` on Windows).",
                tgt_str, src_str, tgt_str
            )
        } else if combined.contains("permission denied")
            || combined.contains("operation not permitted")
        {
            format!(
                "Check macOS Privacy & Security disk permissions and volume read/write mount status, then manually run `mkdir -p \"{}\"` and remove the corresponding line(s) from broken_files.journal.",
                tgt_str
            )
        } else if combined.contains("already exists but is not a directory") {
            format!(
                "Inspect \"{}\" and remove or rename the conflicting non-directory file so the target directory can be created.",
                tgt_str
            )
        } else {
            format!(
                "Manually test creating the target directory with `mkdir -p \"{}\"`, resolve any filesystem or mount errors on the drive, and remove the corresponding line(s) from broken_files.journal.",
                tgt_str
            )
        }
    } else if combined.contains("does not exist") {
        format!(
            "Verify that the drive/volume is mounted and that the source directory \"{}\" exists and is spelled correctly.",
            src_str
        )
    } else if combined.contains("exceeds available free space")
        || combined.contains("no space left on device")
    {
        format!(
            "Free up disk space on the target volume so available space exceeds the size of \"{}\", then re-run conversion.",
            src_str
        )
    } else if combined.contains("failed to delete completed subdir")
        || combined.contains("error deleting completed subdir")
    {
        format!(
            "Verify that all converted media files are present and valid in \"{}\", then manually delete the source directory (`rm -rf \"{}\"`). If deletion fails on exFAT, close open file handles or run `chkdsk /f` on Windows.",
            tgt_str, src_str
        )
    } else if combined.contains("media file count mismatch")
        || combined.contains("media count mismatch")
    {
        format!(
            "Compare media files between \"{}\" and \"{}\", manually convert or move any missing files into \"{}\", and then delete \"{}\".",
            src_str, tgt_str, tgt_str, src_str
        )
    } else if combined.contains("copy-back failed") || combined.contains("error moving") {
        format!(
            "Check volume write access, available disk space, and network/SSH connectivity (if using remote workers). Remove any partial output file in \"{}\" and re-run.",
            tgt_str
        )
    } else if combined.contains("corrupt")
        || combined.contains("broken_files.journal")
        || combined.contains("failed conversion")
        || combined.contains("error converting")
    {
        format!(
            "Inspect broken_files.journal for the failing media file(s) inside \"{}\". Repair, re-download, or delete the corrupt source file(s), remove their journal entries if repaired, and re-run (or manually remove \"{}\" once all valid files are in \"{}\").",
            src_str, src_str, tgt_str
        )
    } else {
        format!(
            "Inspect \"{}\" and \"{}\", resolve the underlying error reported above, and re-run the converter.",
            src_str, tgt_str
        )
    }
}

pub fn record_failed_dir(
    failed_dirs_file: &Path,
    source_dir: &Path,
    target_dir: Option<&Path>,
    context_msg: &str,
    err: &anyhow::Error,
) {
    let registry = FAILED_DIRS_REGISTRY.get_or_init(|| Mutex::new(HashSet::new()));
    let key_path = target_dir.unwrap_or(source_dir).to_path_buf();

    let is_first_report_for_dir = if let Ok(mut set) = registry.lock() {
        set.insert(key_path)
    } else {
        true
    };

    if is_first_report_for_dir {
        print_error_with_stack_trace(context_msg, err);

        let file_lock = FAILED_DIRS_FILE_LOCK.get_or_init(|| Mutex::new(()));
        if let Ok(_guard) = file_lock.lock() {
            let (date_str, time_str) = format_timestamp_now();
            let timestamp = format!("{} {}", date_str, time_str);
            let src_display = source_dir.display().to_string();
            let tgt_display = match target_dir {
                Some(tdir) if tdir != source_dir => tdir.display().to_string(),
                Some(tdir) => format!("{} (in-place)", tdir.display()),
                None => "N/A".to_string(),
            };
            let clean_reason = summarize_error_reason(context_msg, err);
            let suggested_action =
                suggest_manual_corrective_action(source_dir, target_dir, context_msg, &clean_reason);
            let impacted_files_count = count_eligible_video_files_recursive(source_dir);

            if let Ok(mut f) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(failed_dirs_file)
            {
                let _ = writeln!(
                    f,
                    "================================================================================\n\
                     Timestamp                  : {}\n\
                     Source Directory           : {}\n\
                     Target Directory           : {}\n\
                     Number of Files Impacted   : {}\n\
                     Reason for Failure         : {}\n\
                     Suggested Corrective Action: {}\n\
                     ================================================================================",
                    timestamp,
                    src_display,
                    tgt_display,
                    impacted_files_count,
                    clean_reason,
                    suggested_action
                );
            }
        }
    }
}

pub fn mark_dir_unwritable(dir: &Path) {
    let reg = UNWRITABLE_DIRS_REGISTRY.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut set) = reg.lock() {
        set.insert(dir.to_path_buf());
    }
}

/// Returns `true` ONLY when `dir` was explicitly marked unwritable by a fatal directory-level
/// failure (such as `mkdir` failure or insufficient mount free space). Per-file corruption
/// errors must never mark a directory unwritable.
pub fn is_dir_marked_failed(dir: &Path) -> bool {
    if let Some(reg) = UNWRITABLE_DIRS_REGISTRY.get() {
        if let Ok(set) = reg.lock() {
            return set.contains(dir);
        }
    }
    false
}

/// Option A per-file failure reporting:
/// - For files inside a tracked subdirectory (`item.source_subdir.is_some()`), logs the error
///   stack trace immediately while deferring the `broken_directories.journal` (`FAILED_DIRS_PATH`)
///   summary entry until Step 2.6 subdir completion (or graceful shutdown flush), so verified
///   converted source files are deleted first and `Number of Files Impacted` accurately reflects
///   only the remaining broken file(s).
/// - For untracked root/in-place files (`item.source_subdir.is_none()`), writes to
///   `FAILED_DIRS_PATH` immediately.
fn record_file_failure_if_untracked_subdir(
    failed_dirs_file: &Path,
    item: &QueuedMediaItem,
    source_dir: &Path,
    target_dir: Option<&Path>,
    context_msg: &str,
    err: &anyhow::Error,
) {
    if item.source_subdir.is_some() {
        print_error_with_stack_trace(context_msg, err);
    } else {
        record_failed_dir(failed_dirs_file, source_dir, target_dir, context_msg, err);
    }
}

// --- Stack Trace & Error Output (Always Printed & Logged) ---
fn print_error_with_stack_trace(context_msg: &str, err: &anyhow::Error) {
    let bt = Backtrace::force_capture();
    let bt_str = bt.to_string();
    log_error_with_bt(&format!("{} | Caused by: {:?}", context_msg, err), &bt_str);

    let mut stderr = std::io::stderr().lock();
    let _ = write!(stderr, "\r\x1b[2K\x1b[?25h\n");
    let _ = writeln!(
        stderr,
        "================================================================"
    );
    let _ = writeln!(stderr, "ERROR: {}", context_msg);
    let _ = writeln!(
        stderr,
        "----------------------------------------------------------------"
    );
    let _ = writeln!(stderr, "Error Chain:\n{:?}", err);
    let _ = writeln!(
        stderr,
        "----------------------------------------------------------------"
    );
    let _ = writeln!(stderr, "Captured Stack Trace:\n{}", bt_str);
    let _ = writeln!(
        stderr,
        "================================================================\n"
    );
    let _ = stderr.flush();
}

fn ensure_dir_created(target_dir: &Path) -> Result<()> {
    if target_dir.is_dir() {
        return Ok(());
    }
    if target_dir.exists() && !target_dir.is_dir() {
        let bt = Backtrace::force_capture();
        anyhow::bail!(
            "Target path already exists but is not a directory: {:?}\nStack Trace:\n{}",
            target_dir,
            bt
        );
    }

    // 1. Standard recursive mkdir -p via std::fs::create_dir_all
    let first_attempt = fs::create_dir_all(target_dir);
    if first_attempt.is_ok() || target_dir.is_dir() {
        return Ok(());
    }

    // 2. Step-by-step ancestor walk (mkdir -p style) with brief backoff for exFAT/external drives
    let mut ancestors: Vec<&Path> = target_dir.ancestors().collect();
    ancestors.reverse();
    for anc in ancestors {
        if anc.as_os_str().is_empty() || anc.is_dir() {
            continue;
        }
        if fs::create_dir(anc).is_err() && !anc.is_dir() {
            thread::sleep(Duration::from_millis(50));
            let _ = fs::create_dir_all(anc);
        }
    }
    if target_dir.is_dir() {
        return Ok(());
    }

    // 3. Fallback to OS `/bin/mkdir -p` command
    if let Ok(status) = Command::new("/bin/mkdir")
        .arg("-p")
        .arg(target_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        if status.success() || target_dir.is_dir() {
            return Ok(());
        }
    }

    let first_err = first_attempt.unwrap_err();
    let raw_bytes = target_dir.as_os_str().as_encoded_bytes();
    let parent_diag = match target_dir.parent() {
        Some(p) => format!(
            "parent={:?}, exists={}, is_dir={}, metadata={:?}",
            p,
            p.exists(),
            p.is_dir(),
            fs::metadata(p)
        ),
        None => "parent=None".to_string(),
    };
    let bt = Backtrace::force_capture();
    Err(first_err).with_context(|| {
        format!(
            "Failed to create target directory {}\n  Debug Path: {:?}\n  Raw Bytes: {:?}\n  Parent Info: {}\n  Stack Trace:\n{}",
            target_dir.display(),
            target_dir,
            raw_bytes,
            parent_diag,
            bt
        )
    })
}

// --- Configuration Structs ---
#[allow(non_snake_case)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerNodeConfig {
    #[serde(default)]
    pub NODENAME: String,
    pub IP_ADDRESS: String,
    pub USERNAME: String,
    pub SSH_PORT: u16,
    pub MAX_SIZE_IN_GB: f64,
    pub FFMPEG_PATH: String,
    pub OS_TYPE: String,
    #[serde(default)]
    pub GPU_PROFILE: String,
}

impl WorkerNodeConfig {
    pub fn effective_nodename(&self) -> String {
        if !self.NODENAME.trim().is_empty() {
            self.NODENAME.trim().to_string()
        } else if !self.USERNAME.is_empty() && !self.IP_ADDRESS.is_empty() {
            format!("{}@{}", self.USERNAME, self.IP_ADDRESS)
        } else if !self.IP_ADDRESS.is_empty() {
            self.IP_ADDRESS.clone()
        } else {
            "remote-node".to_string()
        }
    }
}

impl Default for WorkerNodeConfig {
    fn default() -> Self {
        Self {
            NODENAME: String::new(),
            IP_ADDRESS: String::new(),
            USERNAME: String::new(),
            SSH_PORT: 22,
            MAX_SIZE_IN_GB: 20.0,
            FFMPEG_PATH: "/usr/bin/ffmpeg".to_string(),
            OS_TYPE: "Linux".to_string(),
            GPU_PROFILE: String::new(),
        }
    }
}

#[allow(non_snake_case)]
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub RESOURCE_CEILING: f64,
    pub RAMDISK_EVICTION_THRESHOLD: f64,
    pub ORCHESTRATOR_CHECK_INTERVAL_SEC: f64,
    pub MONITOR_INTERVAL_MSEC: u64,
    pub FFMPEG_NICE_VALUE: i32,
    pub INTERMEDIATE_NICE_VALUE: i32,
    #[serde(default)]
    pub LOCAL_NODENAME: String,
    pub VIDEO_EXTENSIONS: Vec<String>,

    // FFmpeg & Output Criteria
    #[serde(default)]
    pub TARGET_VIDEO_CODEC: String,
    #[serde(default)]
    pub TARGET_AUDIO_CODEC: String,
    #[serde(default)]
    pub TARGET_CONTAINER: String,
    pub OUTPUT_SUFFIX: String,
    pub OUTPUT_EXTENSION: String,
    pub FFMPEG_DECODE_ARGS: Vec<String>,
    pub FFMPEG_MACOS_HWACCEL_ARGS: Vec<String>,
    #[serde(default)]
    pub FFMPEG_MACOS_ENCODER_ARGS: Vec<String>,
    #[serde(default)]
    pub FFMPEG_FALLBACK_VIDEO_FILTER_ARGS: Vec<String>,
    pub FFMPEG_NVIDIA_HWACCEL_ARGS: Vec<String>,
    #[serde(default)]
    pub FFMPEG_NVIDIA_ENCODER_ARGS: Vec<String>,
    #[serde(default)]
    pub FFMPEG_GTX_3050_TI_ENCODER_ARGS: Vec<String>,
    #[serde(default)]
    pub FFMPEG_CPU_ENCODER_ARGS: Vec<String>,
    #[serde(default)]
    pub FFMPEG_MACOS_AUDIO_ARGS: Vec<String>,
    #[serde(default)]
    pub FFMPEG_AUDIO_ARGS: Vec<String>,

    // Speed & Space Heuristics
    pub MIN_BITRATE_KBPS: u64,
    pub KEEP_SMALLER_ORIGINAL: bool,
    pub COPY_COMPATIBLE_AUDIO: bool,
    pub SORT_BY_SIZE_DESC: bool,

    // Post-Encode Duration & Audio/Video Sync Verification Thresholds
    #[serde(default)]
    pub DURATION_TOLERANCE_SECS: f64,
    #[serde(default)]
    pub AV_STREAM_MISMATCH_TOLERANCE_SECS: f64,
    #[serde(default)]
    pub LEGACY_CONTAINER_PREROLL_TOLERANCE_SECS: f64,
    #[serde(default)]
    pub LEGACY_CONTAINER_VFR_TOLERANCE_PCT: f64,
    #[serde(default)]
    pub LEGACY_CONTAINER_MAX_TOLERANCE_SECS: f64,

    // RAMDisk & Staging Configuration
    pub ENABLE_RAMDISK: bool,
    pub RAMDISK_NAME: String,
    pub RAMDISK_SIZE_GB: f64,
    pub RAMDISK_CLEANUP_ON_EXIT: bool,
    pub STAGER_WAIT_MSEC: u64,
    pub USE_LOCAL_STAGING_FOR_HDD: bool,
    pub LOCAL_STAGING_DIR: String,

    // Error Logging & Distributed System Configuration
    pub FAILED_DIRS_PATH: String,
    pub HIGH_PERFORMANCE_DISTRIBUTED_SYSTEM: bool,
    pub TASK_JOURNAL_PATH: String,
    pub WORKER_NODES: Vec<WorkerNodeConfig>,

    // Observability (Prometheus & Grafana Loki) Configuration
    #[serde(default)]
    pub PROMETHEUS_ENABLED: bool,
    #[serde(default)]
    pub AUTO_SPAWN_METRICS_AGENT: bool,
    #[serde(default)]
    pub DEPLOY_REMOTE_METRICS_AGENT: bool,
    #[serde(default)]
    pub METRICS_AGENT_SCRIPT_PATH: String,
    #[serde(default)]
    pub TELEMETRY_SNAPSHOT_PATH: String,
    #[serde(default)]
    pub PROMETHEUS_LISTEN_ADDR: String,
    #[serde(default)]
    pub PROMETHEUS_ENDPOINT: String,
    #[serde(default)]
    pub PROMETHEUS_PUSH_INTERVAL_SEC: u64,
    #[serde(default)]
    pub LOKI_ENABLED: bool,
    #[serde(default)]
    pub LOKI_ENDPOINT: String,
    #[serde(default)]
    pub LOKI_BATCH_WAIT_MSEC: u64,
    #[serde(default)]
    pub LOKI_BATCH_SIZE_BYTES: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            RESOURCE_CEILING: 90.0,
            RAMDISK_EVICTION_THRESHOLD: 95.0,
            ORCHESTRATOR_CHECK_INTERVAL_SEC: 1.0,
            MONITOR_INTERVAL_MSEC: 500,
            FFMPEG_NICE_VALUE: -10,
            INTERMEDIATE_NICE_VALUE: -13,
            LOCAL_NODENAME: gethostname_syscall(),
            VIDEO_EXTENSIONS: vec![
                ".wmv".to_string(),
                ".mov".to_string(),
                ".mp4".to_string(),
                ".mkv".to_string(),
                ".avi".to_string(),
                ".flv".to_string(),
                ".mpg".to_string(),
                ".m4v".to_string(),
            ],
            TARGET_VIDEO_CODEC: "hevc".to_string(),
            TARGET_AUDIO_CODEC: "aac".to_string(),
            TARGET_CONTAINER: "mp4".to_string(),
            OUTPUT_SUFFIX: "_h265".to_string(),
            OUTPUT_EXTENSION: ".mp4".to_string(),
            FFMPEG_DECODE_ARGS: vec![
                "-fflags".to_string(),
                "+genpts+igndts".to_string(),
                "-threads".to_string(),
                "0".to_string(),
            ],
            FFMPEG_MACOS_HWACCEL_ARGS: vec!["-hwaccel".to_string(), "videotoolbox".to_string()],
            FFMPEG_MACOS_ENCODER_ARGS: Vec::new(),
            FFMPEG_FALLBACK_VIDEO_FILTER_ARGS: vec![
                "-vf".to_string(),
                "bwdif=mode=0:parity=-1:deint=1,format=yuv420p".to_string(),
            ],
            FFMPEG_NVIDIA_HWACCEL_ARGS: vec![
                "-hwaccel".to_string(),
                "cuda".to_string(),
                "-hwaccel_output_format".to_string(),
                "cuda".to_string(),
            ],
            FFMPEG_NVIDIA_ENCODER_ARGS: Vec::new(),
            FFMPEG_GTX_3050_TI_ENCODER_ARGS: Vec::new(),
            FFMPEG_CPU_ENCODER_ARGS: Vec::new(),
            FFMPEG_MACOS_AUDIO_ARGS: Vec::new(),
            FFMPEG_AUDIO_ARGS: Vec::new(),
            MIN_BITRATE_KBPS: 1800,
            KEEP_SMALLER_ORIGINAL: true,
            COPY_COMPATIBLE_AUDIO: true,
            SORT_BY_SIZE_DESC: true,
            DURATION_TOLERANCE_SECS: 1.5,
            AV_STREAM_MISMATCH_TOLERANCE_SECS: 1.5,
            LEGACY_CONTAINER_PREROLL_TOLERANCE_SECS: 4.0,
            LEGACY_CONTAINER_VFR_TOLERANCE_PCT: 3.5,
            LEGACY_CONTAINER_MAX_TOLERANCE_SECS: 20.0,
            ENABLE_RAMDISK: true,
            RAMDISK_NAME: "RustyH265RAMDisk".to_string(),
            RAMDISK_SIZE_GB: 8.0,
            RAMDISK_CLEANUP_ON_EXIT: true,
            STAGER_WAIT_MSEC: 150,
            USE_LOCAL_STAGING_FOR_HDD: true,
            LOCAL_STAGING_DIR: "/Volumes/RustyH265RAMDisk/staging".to_string(),
            FAILED_DIRS_PATH: "failed_dirs.txt".to_string(),
            HIGH_PERFORMANCE_DISTRIBUTED_SYSTEM: false,
            TASK_JOURNAL_PATH: "distributed_tasks_journal.json".to_string(),
            WORKER_NODES: Vec::new(),
            PROMETHEUS_ENABLED: true,
            AUTO_SPAWN_METRICS_AGENT: true,
            DEPLOY_REMOTE_METRICS_AGENT: true,
            METRICS_AGENT_SCRIPT_PATH: "scripts/rusty_h265_metrics_agent.py".to_string(),
            TELEMETRY_SNAPSHOT_PATH: if cfg!(target_os = "windows") {
                std::env::temp_dir()
                    .join("rusty_h265_telemetry.json")
                    .to_string_lossy()
                    .to_string()
            } else {
                "/tmp/rusty_h265_telemetry.json".to_string()
            },
            PROMETHEUS_LISTEN_ADDR: "0.0.0.0:9184".to_string(),
            PROMETHEUS_ENDPOINT: String::new(),
            PROMETHEUS_PUSH_INTERVAL_SEC: 5,
            LOKI_ENABLED: false,
            LOKI_ENDPOINT: String::new(),
            LOKI_BATCH_WAIT_MSEC: 1000,
            LOKI_BATCH_SIZE_BYTES: 65536,
        }
    }
}

fn apply_worker_node_field(node: &mut WorkerNodeConfig, key: &str, val: &str) {
    let cleaned = unquote(val);
    match key.to_uppercase().replace('-', "_").as_str() {
        "NODENAME" | "NODE_NAME" | "NAME" => node.NODENAME = cleaned,
        "IP_ADDRESS" | "IP" | "HOST" => node.IP_ADDRESS = cleaned,
        "USERNAME" | "USER_NAME" | "USER" => node.USERNAME = cleaned,
        "SSH_PORT" | "PORT" => {
            if let Ok(p) = cleaned.parse::<u16>() {
                node.SSH_PORT = p;
            }
        }
        "MAX_SIZE_IN_GB" | "MAX_SIZE_IN_GB_FOR_INPUT_FILES_ONLY" | "MAX_SIZE_GB" => {
            if let Ok(sz) = cleaned.parse::<f64>() {
                node.MAX_SIZE_IN_GB = sz;
            }
        }
        "FFMPEG_PATH" | "PATH_TO_FFMPEG" => node.FFMPEG_PATH = cleaned,
        "OS_TYPE" | "OS" => node.OS_TYPE = cleaned,
        "GPU_PROFILE" | "GPU" | "GPU_MODEL" => node.GPU_PROFILE = cleaned,
        _ => {}
    }
}

impl AppConfig {
    pub fn effective_local_nodename(&self) -> String {
        if !self.LOCAL_NODENAME.trim().is_empty() {
            self.LOCAL_NODENAME.trim().to_string()
        } else {
            gethostname_syscall()
        }
    }

    pub fn load(config_path: &Path) -> Result<Self> {
        let content = fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
        Self::from_yaml_str(&content)
    }

    pub fn from_yaml_str(content: &str) -> Result<Self> {
        let mut config = AppConfig::default();
        let mut target_container_explicit = false;
        let mut output_extension_explicit = false;
        let mut current_list_key: Option<String> = None;
        let mut list_items: Vec<String> = Vec::new();
        let mut in_worker_nodes = false;
        let mut in_local_section = false;
        let mut current_node: Option<WorkerNodeConfig> = None;

        let assign_list = |cfg: &mut AppConfig, key: &str, items: Vec<String>| {
            match key.to_uppercase().as_str() {
                "VIDEO_EXTENSIONS" => cfg.VIDEO_EXTENSIONS = items,
                "FFMPEG_DECODE_ARGS" => cfg.FFMPEG_DECODE_ARGS = items,
                "FFMPEG_MACOS_HWACCEL_ARGS" => cfg.FFMPEG_MACOS_HWACCEL_ARGS = items,
                "FFMPEG_MACOS_ENCODER_ARGS" => cfg.FFMPEG_MACOS_ENCODER_ARGS = items,
                "FFMPEG_FALLBACK_VIDEO_FILTER_ARGS" => cfg.FFMPEG_FALLBACK_VIDEO_FILTER_ARGS = items,
                "FFMPEG_NVIDIA_HWACCEL_ARGS" => cfg.FFMPEG_NVIDIA_HWACCEL_ARGS = items,
                "FFMPEG_NVIDIA_ENCODER_ARGS" => cfg.FFMPEG_NVIDIA_ENCODER_ARGS = items,
                "FFMPEG_GTX_3050_TI_ENCODER_ARGS" | "FFMPEG_RTX_3050_TI_ENCODER_ARGS" => {
                    cfg.FFMPEG_GTX_3050_TI_ENCODER_ARGS = items
                }
                "FFMPEG_CPU_ENCODER_ARGS" => cfg.FFMPEG_CPU_ENCODER_ARGS = items,
                "FFMPEG_MACOS_AUDIO_ARGS" => cfg.FFMPEG_MACOS_AUDIO_ARGS = items,
                "FFMPEG_AUDIO_ARGS" => cfg.FFMPEG_AUDIO_ARGS = items,
                _ => {}
            }
        };

        let flush_list =
            |cfg: &mut AppConfig, key: &mut Option<String>, items: &mut Vec<String>| {
                if let Some(k) = key.take() {
                    assign_list(cfg, &k, std::mem::take(items));
                }
            };

        for raw_line in content.lines() {
            let trimmed_with_indent = match raw_line.find('#') {
                Some(idx) => &raw_line[..idx],
                None => raw_line,
            };
            let indent = trimmed_with_indent
                .chars()
                .take_while(|c| c.is_whitespace())
                .count();
            let line = trimmed_with_indent.trim();

            if line.is_empty() || line == "---" {
                continue;
            }

            if indent == 0 {
                in_local_section = false;
            }

            if in_local_section {
                if let Some((k, v)) = line.split_once(':') {
                    let k_norm = unquote(k.trim()).to_uppercase().replace('-', "_");
                    let v_clean = unquote(v.trim());
                    if matches!(k_norm.as_str(), "NODENAME" | "NODE_NAME" | "NAME")
                        && !v_clean.is_empty()
                    {
                        config.LOCAL_NODENAME = v_clean;
                    }
                }
                continue;
            }

            if in_worker_nodes {
                if indent == 0 && !line.starts_with('-') {
                    if let Some(n) = current_node.take() {
                        if !n.IP_ADDRESS.is_empty() {
                            config.WORKER_NODES.push(n);
                        }
                    }
                    in_worker_nodes = false;
                } else {
                    if let Some(rest) = line.strip_prefix('-') {
                        if let Some(n) = current_node.take() {
                            if !n.IP_ADDRESS.is_empty() {
                                config.WORKER_NODES.push(n);
                            }
                        }
                        let mut new_node = WorkerNodeConfig::default();
                        let rest_trimmed = rest.trim();
                        if let Some((k, v)) = rest_trimmed.split_once(':') {
                            apply_worker_node_field(&mut new_node, k.trim(), v.trim());
                        }
                        current_node = Some(new_node);
                    } else if let Some((k, v)) = line.split_once(':') {
                        if current_node.is_none() {
                            current_node = Some(WorkerNodeConfig::default());
                        }
                        if let Some(ref mut node) = current_node {
                            apply_worker_node_field(node, k.trim(), v.trim());
                        }
                    }
                    continue;
                }
            }

            if let Some(item) = line.strip_prefix("- ").or_else(|| {
                if line == "-" {
                    Some("")
                } else {
                    None
                }
            }) {
                let cleaned = unquote(item.trim());
                if !cleaned.is_empty() {
                    list_items.push(cleaned);
                }
                continue;
            }

            flush_list(&mut config, &mut current_list_key, &mut list_items);

            if let Some((raw_key, raw_val)) = line.split_once(':') {
                let key = unquote(raw_key.trim());
                let val = raw_val.trim();

                if key.eq_ignore_ascii_case("WORKER_NODES")
                    || key.eq_ignore_ascii_case("REMOTE_NODES")
                {
                    in_worker_nodes = true;
                    continue;
                }

                if key.eq_ignore_ascii_case("LOCAL") && val.is_empty() {
                    in_local_section = true;
                    continue;
                }

                if val.is_empty()
                    && !matches!(
                        key.to_uppercase().replace('-', "_").as_str(),
                        "PROMETHEUS_ENDPOINT"
                            | "PROMETHEUS_LISTEN_ADDR"
                            | "LOKI_ENDPOINT"
                            | "LOCAL_NODENAME"
                            | "NODENAME"
                    )
                {
                    current_list_key = Some(key);
                    continue;
                }

                if (val.starts_with('[') && val.ends_with(']'))
                    || (val.starts_with('(') && val.ends_with(')'))
                {
                    let inner = &val[1..val.len() - 1];
                    let parsed_list: Vec<String> = inner
                        .split(',')
                        .map(|s| unquote(s.trim()))
                        .filter(|s| !s.is_empty())
                        .collect();
                    assign_list(&mut config, &key, parsed_list);
                    continue;
                }

                let cleaned_val = unquote(val);
                match key.to_uppercase().replace('-', "_").as_str() {
                    "LOCAL_NODENAME" | "NODENAME" | "LOCAL_NODE_NAME" => {
                        if !cleaned_val.is_empty() {
                            config.LOCAL_NODENAME = cleaned_val;
                        }
                    }
                    "RESOURCE_CEILING" => {
                        config.RESOURCE_CEILING = cleaned_val.parse()?;
                    }
                    "RAMDISK_EVICTION_THRESHOLD" => {
                        config.RAMDISK_EVICTION_THRESHOLD = cleaned_val.parse()?;
                    }
                    "ORCHESTRATOR_CHECK_INTERVAL_SEC" => {
                        config.ORCHESTRATOR_CHECK_INTERVAL_SEC = cleaned_val.parse()?;
                    }
                    "MONITOR_INTERVAL_MSEC" => {
                        config.MONITOR_INTERVAL_MSEC = cleaned_val.parse()?;
                    }
                    "FFMPEG_NICE_VALUE" => {
                        config.FFMPEG_NICE_VALUE = cleaned_val.parse()?;
                    }
                    "INTERMEDIATE_NICE_VALUE" => {
                        config.INTERMEDIATE_NICE_VALUE = cleaned_val.parse()?;
                    }
                    "TARGET_VIDEO_CODEC" => {
                        config.TARGET_VIDEO_CODEC = cleaned_val;
                    }
                    "TARGET_AUDIO_CODEC" => {
                        config.TARGET_AUDIO_CODEC = cleaned_val;
                    }
                    "TARGET_CONTAINER" => {
                        config.TARGET_CONTAINER = cleaned_val;
                        target_container_explicit = true;
                    }
                    "OUTPUT_SUFFIX" => {
                        config.OUTPUT_SUFFIX = cleaned_val;
                    }
                    "OUTPUT_EXTENSION" => {
                        config.OUTPUT_EXTENSION = cleaned_val;
                        output_extension_explicit = true;
                    }
                    "MIN_BITRATE_KBPS" => {
                        config.MIN_BITRATE_KBPS = cleaned_val.parse()?;
                    }
                    "KEEP_SMALLER_ORIGINAL" => {
                        config.KEEP_SMALLER_ORIGINAL = parse_bool(&cleaned_val);
                    }
                    "COPY_COMPATIBLE_AUDIO" => {
                        config.COPY_COMPATIBLE_AUDIO = parse_bool(&cleaned_val);
                    }
                    "SORT_BY_SIZE_DESC" => {
                        config.SORT_BY_SIZE_DESC = parse_bool(&cleaned_val);
                    }
                    "DURATION_TOLERANCE_SECS" => {
                        if let Ok(v) = cleaned_val.parse::<f64>() {
                            config.DURATION_TOLERANCE_SECS = v.max(0.5);
                        }
                    }
                    "AV_STREAM_MISMATCH_TOLERANCE_SECS" => {
                        if let Ok(v) = cleaned_val.parse::<f64>() {
                            config.AV_STREAM_MISMATCH_TOLERANCE_SECS = v.max(0.5);
                        }
                    }
                    "LEGACY_CONTAINER_PREROLL_TOLERANCE_SECS" => {
                        if let Ok(v) = cleaned_val.parse::<f64>() {
                            config.LEGACY_CONTAINER_PREROLL_TOLERANCE_SECS = v.max(1.5);
                        }
                    }
                    "LEGACY_CONTAINER_VFR_TOLERANCE_PCT" => {
                        if let Ok(v) = cleaned_val.parse::<f64>() {
                            config.LEGACY_CONTAINER_VFR_TOLERANCE_PCT = v.max(0.0);
                        }
                    }
                    "LEGACY_CONTAINER_MAX_TOLERANCE_SECS" => {
                        if let Ok(v) = cleaned_val.parse::<f64>() {
                            config.LEGACY_CONTAINER_MAX_TOLERANCE_SECS = v.max(1.5);
                        }
                    }
                    "ENABLE_RAMDISK" => {
                        config.ENABLE_RAMDISK = parse_bool(&cleaned_val);
                    }
                    "RAMDISK_NAME" => {
                        config.RAMDISK_NAME = cleaned_val;
                    }
                    "RAMDISK_SIZE_GB" => {
                        config.RAMDISK_SIZE_GB = cleaned_val.parse()?;
                    }
                    "RAMDISK_CLEANUP_ON_EXIT" => {
                        config.RAMDISK_CLEANUP_ON_EXIT = parse_bool(&cleaned_val);
                    }
                    "STAGER_WAIT_MSEC"
                    | "STAGER_SLEEP_MSEC"
                    | "STAGER_WAIT_MS"
                    | "STAGER_SLEEP_MS" => {
                        config.STAGER_WAIT_MSEC = cleaned_val.parse()?;
                    }
                    "USE_LOCAL_STAGING_FOR_HDD" => {
                        config.USE_LOCAL_STAGING_FOR_HDD = parse_bool(&cleaned_val);
                    }
                    "LOCAL_STAGING_DIR" => {
                        config.LOCAL_STAGING_DIR = cleaned_val;
                    }
                    "FAILED_DIRS_PATH" => {
                        config.FAILED_DIRS_PATH = cleaned_val;
                    }
                    "HIGH_PERFORMANCE_DISTRIBUTED_SYSTEM" => {
                        config.HIGH_PERFORMANCE_DISTRIBUTED_SYSTEM = parse_bool(&cleaned_val);
                    }
                    "TASK_JOURNAL_PATH" => {
                        config.TASK_JOURNAL_PATH = cleaned_val;
                    }
                    "PROMETHEUS_ENABLED" => {
                        config.PROMETHEUS_ENABLED = parse_bool(&cleaned_val);
                    }
                    "AUTO_SPAWN_METRICS_AGENT" => {
                        config.AUTO_SPAWN_METRICS_AGENT = parse_bool(&cleaned_val);
                    }
                    "DEPLOY_REMOTE_METRICS_AGENT" => {
                        config.DEPLOY_REMOTE_METRICS_AGENT = parse_bool(&cleaned_val);
                    }
                    "METRICS_AGENT_SCRIPT_PATH" => {
                        if !cleaned_val.is_empty() {
                            config.METRICS_AGENT_SCRIPT_PATH = cleaned_val;
                        }
                    }
                    "TELEMETRY_SNAPSHOT_PATH" => {
                        if !cleaned_val.is_empty() {
                            config.TELEMETRY_SNAPSHOT_PATH = cleaned_val;
                        }
                    }
                    "PROMETHEUS_LISTEN_ADDR" => {
                        config.PROMETHEUS_LISTEN_ADDR = cleaned_val;
                    }
                    "PROMETHEUS_ENDPOINT" => {
                        config.PROMETHEUS_ENDPOINT = cleaned_val;
                    }
                    "PROMETHEUS_PUSH_INTERVAL_SEC" => {
                        if let Ok(sec) = cleaned_val.parse::<u64>() {
                            config.PROMETHEUS_PUSH_INTERVAL_SEC = sec.max(1);
                        }
                    }
                    "LOKI_ENABLED" => {
                        config.LOKI_ENABLED = parse_bool(&cleaned_val);
                    }
                    "LOKI_ENDPOINT" => {
                        config.LOKI_ENDPOINT = cleaned_val;
                    }
                    "LOKI_BATCH_WAIT_MSEC" => {
                        if let Ok(ms) = cleaned_val.parse::<u64>() {
                            config.LOKI_BATCH_WAIT_MSEC = ms.max(100);
                        }
                    }
                    "LOKI_BATCH_SIZE_BYTES" => {
                        if let Ok(sz) = cleaned_val.parse::<usize>() {
                            config.LOKI_BATCH_SIZE_BYTES = sz.max(1024);
                        }
                    }
                    _ => {}
                }
            }
        }

        if let Some(n) = current_node.take() {
            if !n.IP_ADDRESS.is_empty() {
                config.WORKER_NODES.push(n);
            }
        }
        flush_list(&mut config, &mut current_list_key, &mut list_items);
        if target_container_explicit && !output_extension_explicit {
            if let Ok(c) = codecs::TargetContainer::parse(&config.TARGET_CONTAINER) {
                config.OUTPUT_EXTENSION = c.extension().to_string();
            }
        } else if !target_container_explicit && output_extension_explicit {
            config.TARGET_CONTAINER = config
                .OUTPUT_EXTENSION
                .trim()
                .trim_start_matches('.')
                .to_lowercase();
        }
        Ok(config)
    }

    pub fn effective_staging_dir(&self) -> PathBuf {
        let configured = PathBuf::from(&self.LOCAL_STAGING_DIR);
        if cfg!(target_os = "windows") {
            if self.LOCAL_STAGING_DIR.starts_with("/Volumes/")
                || self.LOCAL_STAGING_DIR.starts_with("/dev/shm/")
            {
                return std::env::temp_dir()
                    .join(&self.RAMDISK_NAME)
                    .join("staging");
            }
            return configured;
        }
        if self.ENABLE_RAMDISK {
            if cfg!(target_os = "macos") {
                let ramdisk_mount = PathBuf::from("/Volumes").join(&self.RAMDISK_NAME);
                if configured.starts_with(&ramdisk_mount) {
                    return configured;
                }
                return ramdisk_mount.join("staging");
            } else if cfg!(target_os = "linux") {
                let shm_mount = PathBuf::from("/dev/shm").join(&self.RAMDISK_NAME);
                if configured.starts_with(&shm_mount) {
                    return configured;
                }
                return shm_mount.join("staging");
            }
        }
        configured
    }
}

fn parse_bool(s: &str) -> bool {
    matches!(
        s.trim().to_lowercase().as_str(),
        "true" | "yes" | "1" | "on"
    )
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

// --- RAM Disk Provisioning & Cleanup ---
#[derive(Debug, Clone)]
pub struct ProvisionedRamDisk {
    pub name: String,
    pub mount_path: PathBuf,
    pub staging_dir: PathBuf,
    pub device: Option<String>,
    pub size_gb: f64,
    pub created_by_us: bool,
    pub cleanup_on_exit: bool,
}

impl ProvisionedRamDisk {
    pub fn cleanup(&self, verbose: bool) {
        if self.staging_dir.exists() {
            let _ = fs::remove_dir_all(&self.staging_dir);
        }

        if !self.cleanup_on_exit || !self.created_by_us {
            return;
        }

        if cfg!(target_os = "macos") {
            let target = self
                .device
                .as_deref()
                .unwrap_or_else(|| self.mount_path.to_str().unwrap_or(""));
            if !target.is_empty() {
                vlog!(
                    verbose,
                    "[Orchestrator] Detaching provisioned RAMDisk '{}' ({})...",
                    self.name,
                    target
                );
                let _ = Command::new("hdiutil")
                    .args(["detach", "-force", target])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        } else if cfg!(target_os = "linux") && self.mount_path.exists() {
            let _ = fs::remove_dir_all(&self.mount_path);
        }
    }
}

pub fn provision_ramdisk(
    config: &AppConfig,
    verbose: bool,
) -> Result<Option<ProvisionedRamDisk>> {
    if !config.ENABLE_RAMDISK {
        return Ok(None);
    }

    let ramdisk_name = sanitize_dir_or_file_component(&config.RAMDISK_NAME);
    if ramdisk_name.is_empty() {
        anyhow::bail!("RAMDISK_NAME in config.yaml cannot be empty when ENABLE_RAMDISK is true");
    }
    let size_gb = config.RAMDISK_SIZE_GB.max(0.25);
    let staging_dir = config.effective_staging_dir();

    if cfg!(target_os = "macos") {
        let mount_path = PathBuf::from("/Volumes").join(&ramdisk_name);

        if mount_path.exists() && mount_path.is_dir() {
            let info = get_mount_info(&mount_path);
            if Path::new(&info.mount_point) == mount_path {
                vlog!(
                    verbose,
                    "[Orchestrator] Reusing existing mounted RAMDisk at '{}' (Device: {}).",
                    mount_path.display(),
                    info.device
                );
                ensure_dir_created(&staging_dir)?;
                return Ok(Some(ProvisionedRamDisk {
                    name: ramdisk_name,
                    mount_path,
                    staging_dir,
                    device: Some(info.device),
                    size_gb,
                    created_by_us: false,
                    cleanup_on_exit: config.RAMDISK_CLEANUP_ON_EXIT,
                }));
            }
        }

        let total_bytes = (size_gb * 1024.0 * 1024.0 * 1024.0).round() as u64;
        let sectors = total_bytes / 512;
        let ram_uri = format!("ram://{}", sectors);

        vlog!(
            verbose,
            "[Orchestrator] Provisioning {:.2} GB RAMDisk '{}' ({} sectors)...",
            size_gb,
            ramdisk_name,
            sectors
        );

        let attach_out = Command::new("hdiutil")
            .args(["attach", "-nomount", &ram_uri])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context("Failed to execute 'hdiutil attach -nomount'")?;

        if !attach_out.status.success() {
            let stderr = String::from_utf8_lossy(&attach_out.stderr);
            anyhow::bail!(
                "hdiutil attach failed for RAMDisk '{}': {}",
                ramdisk_name,
                stderr.trim()
            );
        }

        let raw_device = String::from_utf8_lossy(&attach_out.stdout)
            .trim()
            .to_string();
        if raw_device.is_empty() {
            anyhow::bail!("hdiutil attach returned an empty device node for RAMDisk");
        }

        let erase_out = Command::new("diskutil")
            .args(["erasevolume", "HFS+", &ramdisk_name, &raw_device])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context("Failed to execute 'diskutil erasevolume'")?;

        if !erase_out.status.success() {
            let _ = Command::new("hdiutil")
                .args(["detach", "-force", &raw_device])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let stderr = String::from_utf8_lossy(&erase_out.stderr);
            anyhow::bail!(
                "diskutil erasevolume failed for RAMDisk '{}' on {}: {}",
                ramdisk_name,
                raw_device,
                stderr.trim()
            );
        }

        ensure_dir_created(&staging_dir)?;
        Ok(Some(ProvisionedRamDisk {
            name: ramdisk_name,
            mount_path,
            staging_dir,
            device: Some(raw_device),
            size_gb,
            created_by_us: true,
            cleanup_on_exit: config.RAMDISK_CLEANUP_ON_EXIT,
        }))
    } else if cfg!(target_os = "linux") {
        let mount_path = PathBuf::from("/dev/shm").join(&ramdisk_name);
        let existed = mount_path.exists();
        ensure_dir_created(&staging_dir)?;
        Ok(Some(ProvisionedRamDisk {
            name: ramdisk_name,
            mount_path,
            staging_dir,
            device: Some("tmpfs".to_string()),
            size_gb,
            created_by_us: !existed,
            cleanup_on_exit: config.RAMDISK_CLEANUP_ON_EXIT,
        }))
    } else {
        vlog!(
            verbose,
            "[Orchestrator] Auto-RAMDisk provisioning is not built in on Windows; using staging dir '{}'.",
            staging_dir.display()
        );
        Ok(None)
    }
}

// --- CLI Definition ---
#[derive(Parser, Debug)]
#[command(
    name = "rusty_h265_converter",
    about = "Mount-point aware, distributed multi-node video converter with ncurses-style TUI."
)]
struct Cli {
    /// List of directories to convert.
    #[arg(required_unless_present = "dry_run")]
    input_dirs: Vec<PathBuf>,

    /// Path to the YAML configuration file.
    #[arg(long = "config", default_value = "config.yaml")]
    config: PathBuf,

    /// Path to the logging configuration file.
    #[arg(long = "log-config", default_value = "logging.conf")]
    log_config: PathBuf,

    /// Enable verbose step-by-step console logging instead of the ncurses-style TUI.
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Validate config.yaml and discover/probe compatible local & remote encoders without processing media.
    #[arg(long = "dry-run")]
    dry_run: bool,
}

// --- Distributed Task & Queue Data Structures ---
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum StageStatus {
    Pending,
    Started,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedMediaItem {
    pub item_id: u64,
    pub mount_point: String,
    pub root_dir: PathBuf,
    pub source_subdir: Option<PathBuf>,
    pub target_subdir: Option<PathBuf>,
    pub source_path: PathBuf,
    pub dest_dir: PathBuf,
    pub file_size_bytes: u64,
    pub target_label: String,
    pub target_size_bytes: u64,
    pub is_root_in_place: bool,
    #[serde(default)]
    pub cached_probe: Option<VideoProbeInfo>,
    #[serde(default)]
    pub force_unstaged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistributedTaskObject {
    pub task_id: u64,
    pub node_id: String,
    pub original_queue: String,
    pub original_path: PathBuf,
    pub file_size_bytes: u64,
    pub remote_input_path: String,
    pub remote_output_path: String,
    pub final_h265_dest_path: PathBuf,
    pub copy_to_remote_state: StageStatus,
    pub conversion_state: StageStatus,
    pub copy_back_state: StageStatus,
    pub error_occurred: Option<String>,
    pub updated_at_epoch_secs: u64,
    pub queued_item: QueuedMediaItem,
}

#[derive(Debug)]
pub struct SubdirTracker {
    pub mount_point: String,
    pub source_subdir: PathBuf,
    pub target_subdir: PathBuf,
    pub source_files: Vec<PathBuf>,
    pub total_files: usize,
    pub completed_files: usize,
    pub moved_files: usize,
    pub failed_files: usize,
    pub succeeded_files: Vec<(PathBuf, PathBuf)>,
    pub failed_source_files: HashSet<PathBuf>,
    pub deleted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum QueueEntryCategory {
    ActiveLocal = 0,
    ActiveRemote = 1,
    QueuedReady = 2,
    QueuedPending = 3,
}

#[derive(Debug, Clone)]
pub struct InFlightItemState {
    pub item: QueuedMediaItem,
    pub category: QueueEntryCategory,
    pub status_label: String,
}

pub struct MountQueueState {
    pub mount_point: String,
    pub info: MountInfo,
    pub root_dirs: Vec<PathBuf>,
    /// Raw unverified items discovered during directory scan (fed into the per-mount Slot 0 Verifier).
    pub raw_queue: Mutex<VecDeque<QueuedMediaItem>>,
    /// Verified items guaranteed to require H.265 encoding (pulled by local Worker-Group and remote nodes).
    pub queue: Mutex<VecDeque<QueuedMediaItem>>,
    /// Items from this mount currently in flight in local or remote worker pipelines (keyed by `item_id`).
    pub in_flight_items: Mutex<BTreeMap<u64, InFlightItemState>>,
    /// Set to true once the per-mount Slot 0 Verifier has drained and verified all items from `raw_queue`.
    pub verifier_done: AtomicBool,
    /// Set to true once this mount's Local-Worker-Group (stager + converter + flusher) has completed all work.
    pub local_worker_finished: AtomicBool,
    /// Number of items currently in flight inside this mount's Local-Worker-Group pipeline (stager + converter + flusher).
    pub in_flight_local_tasks: AtomicUsize,
    pub io_lock: Mutex<()>,
    pub active_remote_managers: AtomicUsize,
    pub total_enqueued: AtomicUsize,
}

impl MountQueueState {
    pub fn has_pending_work(&self) -> bool {
        !self.queue.lock().unwrap().is_empty()
            || !self.verifier_done.load(Ordering::Relaxed)
            || !self.raw_queue.lock().unwrap().is_empty()
    }

    pub fn total_remaining_items(&self) -> (usize, usize) {
        let raw_len = self.raw_queue.lock().unwrap().len();
        let ready_len = self.queue.lock().unwrap().len();
        (raw_len + ready_len, ready_len)
    }

    pub fn queue_depth(&self) -> usize {
        let (queued_len, _) = self.total_remaining_items();
        queued_len + self.in_flight_local_tasks.load(Ordering::Relaxed)
    }

    pub fn set_in_flight(
        &self,
        item: &QueuedMediaItem,
        category: QueueEntryCategory,
        status_label: impl Into<String>,
    ) {
        if let Ok(mut guard) = self.in_flight_items.lock() {
            guard.insert(
                item.item_id,
                InFlightItemState {
                    item: item.clone(),
                    category,
                    status_label: status_label.into(),
                },
            );
        }
    }

    pub fn clear_in_flight(&self, item_id: u64) {
        if let Ok(mut guard) = self.in_flight_items.lock() {
            guard.remove(&item_id);
        }
    }
}

// --- Interactive TUI Queue Snapshot & `less`-Style Pager / Sort Engine ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueViewEntry {
    pub original_index: usize,
    pub item_id: u64,
    pub display_path: String,
    pub file_name_lower: String,
    pub full_path_lower: String,
    pub file_size_bytes: u64,
    pub category: QueueEntryCategory,
    pub status_label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    Name,   // Column 1
    Size,   // Column 2
    Status, // Column 3
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Asc,  // 'a'
    Desc, // 'd'
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueSortSpec {
    pub column: SortColumn,
    pub direction: SortDirection,
}

impl QueueSortSpec {
    pub fn label(&self) -> &'static str {
        match (self.column, self.direction) {
            (SortColumn::Name, SortDirection::Asc) => "Col 1 (Name) ASC",
            (SortColumn::Name, SortDirection::Desc) => "Col 1 (Name) DESC",
            (SortColumn::Size, SortDirection::Asc) => "Col 2 (Size) ASC",
            (SortColumn::Size, SortDirection::Desc) => "Col 2 (Size) DESC",
            (SortColumn::Status, SortDirection::Asc) => "Col 3 (Status) ASC",
            (SortColumn::Status, SortDirection::Desc) => "Col 3 (Status) DESC",
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueueSnapshotView {
    pub mount_point: String,
    pub drive_type: DriveType,
    pub root_summary: String,
    pub captured_at_12h: String,
    pub active_count: usize,
    pub queued_count: usize,
    pub total_bytes: u64,
    pub original_entries: Vec<QueueViewEntry>,
    pub displayed_entries: Vec<QueueViewEntry>,
    pub active_sort: Option<QueueSortSpec>,
    pub scroll_offset: usize,
    pub command_input: Option<String>,
    pub status_message: Option<String>,
}

impl QueueSnapshotView {
    pub fn header_summary(&self) -> String {
        format!(
            "Queue of files from root: {} at {}. Total: {} files ({} active, {} queued | {})",
            self.root_summary,
            self.captured_at_12h,
            self.original_entries.len(),
            self.active_count,
            self.queued_count,
            format_bytes(self.total_bytes)
        )
    }

    pub fn sort_label(&self) -> &'static str {
        match self.active_sort {
            Some(spec) => spec.label(),
            None => "Original Queue Order (:r)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiViewMode {
    Dashboard,
    QueueSelector,
    QueuePager,
}

#[derive(Debug, Clone)]
pub struct TuiInteractiveState {
    pub mode: TuiViewMode,
    pub selected_queue_idx: usize,
    pub active_snapshot: Option<QueueSnapshotView>,
    pub page_height: usize,
    pub command_input_active: bool,
    pub command_buffer: String,
    pub status_message: Option<String>,
}

impl Default for TuiInteractiveState {
    fn default() -> Self {
        Self {
            mode: TuiViewMode::Dashboard,
            selected_queue_idx: 0,
            active_snapshot: None,
            page_height: 25,
            command_input_active: false,
            command_buffer: String::new(),
            status_message: None,
        }
    }
}

/// Takes a detached point-in-time deep copy of `mq` (`in_flight_items` + `queue` + `raw_queue`)
/// and flattens it into a standalone `QueueSnapshotView` without holding locks during sorting or scrolling.
pub fn snapshot_mount_queue_to_flat_list(mq: &MountQueueState) -> QueueSnapshotView {
    let captured_at_12h = format_time_12h_millis_now();

    // 1. Clone in-memory structs and drop each mutex lock immediately
    let mut in_flight_cloned: Vec<InFlightItemState> = mq
        .in_flight_items
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect();
    let ready_cloned: Vec<QueuedMediaItem> =
        mq.queue.lock().unwrap().iter().cloned().collect();
    let raw_cloned: Vec<QueuedMediaItem> =
        mq.raw_queue.lock().unwrap().iter().cloned().collect();

    // Order in-flight items by (category, item_id) so ActiveLocal comes first, then ActiveRemote
    in_flight_cloned.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then_with(|| a.item.item_id.cmp(&b.item.item_id))
    });

    let root_summary = if !mq.root_dirs.is_empty() {
        mq.root_dirs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    } else if let Some(first_inf) = in_flight_cloned.first() {
        first_inf.item.root_dir.display().to_string()
    } else if let Some(first_rdy) = ready_cloned.first() {
        first_rdy.root_dir.display().to_string()
    } else if let Some(first_raw) = raw_cloned.first() {
        first_raw.root_dir.display().to_string()
    } else {
        mq.mount_point.clone()
    };

    let single_root = mq.root_dirs.len() <= 1;
    let mut seen_ids: HashSet<u64> = HashSet::new();
    let mut entries: Vec<QueueViewEntry> =
        Vec::with_capacity(in_flight_cloned.len() + ready_cloned.len() + raw_cloned.len());
    let mut active_count = 0usize;
    let mut queued_count = 0usize;
    let mut total_bytes = 0u64;

    let make_entry = |idx: usize,
                      item: &QueuedMediaItem,
                      category: QueueEntryCategory,
                      status_label: String|
     -> QueueViewEntry {
        let display_path = if single_root {
            item.source_path
                .strip_prefix(&item.root_dir)
                .ok()
                .filter(|rel| !rel.as_os_str().is_empty())
                .map(|rel| rel.display().to_string())
                .unwrap_or_else(|| item.source_path.display().to_string())
        } else {
            item.source_path.display().to_string()
        };
        let file_name_lower = item
            .source_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&display_path)
            .to_lowercase();
        let full_path_lower = display_path.to_lowercase();
        QueueViewEntry {
            original_index: idx,
            item_id: item.item_id,
            display_path,
            file_name_lower,
            full_path_lower,
            file_size_bytes: item.file_size_bytes,
            category,
            status_label,
        }
    };

    for inf in &in_flight_cloned {
        if seen_ids.insert(inf.item.item_id) {
            let idx = entries.len() + 1;
            total_bytes = total_bytes.saturating_add(inf.item.file_size_bytes);
            active_count += 1;
            entries.push(make_entry(
                idx,
                &inf.item,
                inf.category,
                inf.status_label.clone(),
            ));
        }
    }

    for item in &ready_cloned {
        if seen_ids.insert(item.item_id) {
            let idx = entries.len() + 1;
            total_bytes = total_bytes.saturating_add(item.file_size_bytes);
            queued_count += 1;
            entries.push(make_entry(
                idx,
                item,
                QueueEntryCategory::QueuedReady,
                "QUEUED [Ready]".to_string(),
            ));
        }
    }

    for item in &raw_cloned {
        if seen_ids.insert(item.item_id) {
            let idx = entries.len() + 1;
            total_bytes = total_bytes.saturating_add(item.file_size_bytes);
            queued_count += 1;
            entries.push(make_entry(
                idx,
                item,
                QueueEntryCategory::QueuedPending,
                "QUEUED [Pending Probe]".to_string(),
            ));
        }
    }

    QueueSnapshotView {
        mount_point: mq.mount_point.clone(),
        drive_type: mq.info.drive_type,
        root_summary,
        captured_at_12h,
        active_count,
        queued_count,
        total_bytes,
        displayed_entries: entries.clone(),
        original_entries: entries,
        active_sort: None,
        scroll_offset: 0,
        command_input: None,
        status_message: None,
    }
}

/// Parses and applies interactive pager commands:
/// - `:s1a` / `:s1d` -> Sort by Column 1 (File Name) Ascending / Descending
/// - `:s2a` / `:s2d` -> Sort by Column 2 (File Size) Ascending / Descending
/// - `:s3a` / `:s3d` -> Sort by Column 3 (Status / Worker) Ascending / Descending
/// - `:r`            -> Reset sort order back to original snapshot order
pub fn parse_and_apply_queue_command(
    snapshot: &mut QueueSnapshotView,
    raw_cmd: &str,
) -> std::result::Result<String, String> {
    let normalized = raw_cmd.trim().trim_start_matches(':').trim().to_lowercase();
    match normalized.as_str() {
        "r" | "reset" => {
            snapshot.displayed_entries = snapshot.original_entries.clone();
            snapshot.active_sort = None;
            snapshot.scroll_offset = 0;
            let msg = "Reset sort to original queue order".to_string();
            snapshot.status_message = Some(msg.clone());
            Ok(msg)
        }
        "s1a" => {
            snapshot.displayed_entries.sort_by(|a, b| {
                a.file_name_lower
                    .cmp(&b.file_name_lower)
                    .then_with(|| a.full_path_lower.cmp(&b.full_path_lower))
                    .then_with(|| a.original_index.cmp(&b.original_index))
            });
            let spec = QueueSortSpec {
                column: SortColumn::Name,
                direction: SortDirection::Asc,
            };
            snapshot.active_sort = Some(spec);
            snapshot.scroll_offset = 0;
            let msg = format!("Sorted by {}", spec.label());
            snapshot.status_message = Some(msg.clone());
            Ok(msg)
        }
        "s1d" => {
            snapshot.displayed_entries.sort_by(|a, b| {
                b.file_name_lower
                    .cmp(&a.file_name_lower)
                    .then_with(|| b.full_path_lower.cmp(&a.full_path_lower))
                    .then_with(|| a.original_index.cmp(&b.original_index))
            });
            let spec = QueueSortSpec {
                column: SortColumn::Name,
                direction: SortDirection::Desc,
            };
            snapshot.active_sort = Some(spec);
            snapshot.scroll_offset = 0;
            let msg = format!("Sorted by {}", spec.label());
            snapshot.status_message = Some(msg.clone());
            Ok(msg)
        }
        "s2a" => {
            snapshot.displayed_entries.sort_by(|a, b| {
                a.file_size_bytes
                    .cmp(&b.file_size_bytes)
                    .then_with(|| a.original_index.cmp(&b.original_index))
            });
            let spec = QueueSortSpec {
                column: SortColumn::Size,
                direction: SortDirection::Asc,
            };
            snapshot.active_sort = Some(spec);
            snapshot.scroll_offset = 0;
            let msg = format!("Sorted by {}", spec.label());
            snapshot.status_message = Some(msg.clone());
            Ok(msg)
        }
        "s2d" => {
            snapshot.displayed_entries.sort_by(|a, b| {
                b.file_size_bytes
                    .cmp(&a.file_size_bytes)
                    .then_with(|| a.original_index.cmp(&b.original_index))
            });
            let spec = QueueSortSpec {
                column: SortColumn::Size,
                direction: SortDirection::Desc,
            };
            snapshot.active_sort = Some(spec);
            snapshot.scroll_offset = 0;
            let msg = format!("Sorted by {}", spec.label());
            snapshot.status_message = Some(msg.clone());
            Ok(msg)
        }
        "s3a" => {
            snapshot.displayed_entries.sort_by(|a, b| {
                a.category
                    .cmp(&b.category)
                    .then_with(|| a.status_label.cmp(&b.status_label))
                    .then_with(|| a.original_index.cmp(&b.original_index))
            });
            let spec = QueueSortSpec {
                column: SortColumn::Status,
                direction: SortDirection::Asc,
            };
            snapshot.active_sort = Some(spec);
            snapshot.scroll_offset = 0;
            let msg = format!("Sorted by {}", spec.label());
            snapshot.status_message = Some(msg.clone());
            Ok(msg)
        }
        "s3d" => {
            snapshot.displayed_entries.sort_by(|a, b| {
                b.category
                    .cmp(&a.category)
                    .then_with(|| b.status_label.cmp(&a.status_label))
                    .then_with(|| a.original_index.cmp(&b.original_index))
            });
            let spec = QueueSortSpec {
                column: SortColumn::Status,
                direction: SortDirection::Desc,
            };
            snapshot.active_sort = Some(spec);
            snapshot.scroll_offset = 0;
            let msg = format!("Sorted by {}", spec.label());
            snapshot.status_message = Some(msg.clone());
            Ok(msg)
        }
        _ => {
            let err = format!(
                "Unknown command ':{normalized}'. Valid: :s1a/:s1d (Name), :s2a/:s2d (Size), :s3a/:s3d (Status), :r (Reset)"
            );
            snapshot.status_message = Some(err.clone());
            Err(err)
        }
    }
}

pub struct TaskJournal {
    pub path: PathBuf,
    pub tasks: Mutex<BTreeMap<u64, DistributedTaskObject>>,
}

impl TaskJournal {
    pub fn new(path: PathBuf) -> Self {
        let mut initial = BTreeMap::new();
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(parsed) =
                    serde_json::from_str::<BTreeMap<u64, DistributedTaskObject>>(&content)
                {
                    initial = parsed;
                }
            }
        }
        Self {
            path,
            tasks: Mutex::new(initial),
        }
    }

    pub fn upsert(&self, mut task: DistributedTaskObject) {
        task.updated_at_epoch_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Ok(mut guard) = self.tasks.lock() {
            guard.insert(task.task_id, task);
            if let Ok(json) = serde_json::to_string_pretty(&*guard) {
                let _ = fs::write(&self.path, json);
            }
        }
    }
}

// --- Tier 1: Unified Append-Only Completion Journal (Local + Remote Workers) ---
pub struct CompletedFilesJournal {
    pub path: PathBuf,
    pub entries: Mutex<HashMap<PathBuf, (PathBuf, u64)>>,
}

static COMPLETED_FILES_JOURNAL: OnceLock<CompletedFilesJournal> = OnceLock::new();

pub fn init_completed_files_journal(journal_path: PathBuf, distributed_journal_path: &Path) {
    let mut map: HashMap<PathBuf, (PathBuf, u64)> = HashMap::new();

    // 1. Seed from existing distributed_tasks_journal.json if present
    if distributed_journal_path.exists() {
        if let Ok(content) = fs::read_to_string(distributed_journal_path) {
            if let Ok(parsed) =
                serde_json::from_str::<BTreeMap<u64, DistributedTaskObject>>(&content)
            {
                for task in parsed.values() {
                    if task.copy_back_state == StageStatus::Completed {
                        if let Ok(meta) = fs::metadata(&task.final_h265_dest_path) {
                            if meta.len() > 0 {
                                map.insert(
                                    task.original_path.clone(),
                                    (task.final_h265_dest_path.clone(), meta.len()),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Load append-only completed_files.journal (TSV: <source_path>\t<dest_path>\t<dest_size_bytes>)
    if journal_path.exists() {
        if let Ok(file) = File::open(&journal_path) {
            let reader = BufReader::new(file);
            for line in reader.lines().map_while(|r| r.ok()) {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() >= 3 {
                    if let Ok(sz) = parts[2].trim().parse::<u64>() {
                        if sz > 0 {
                            map.insert(
                                PathBuf::from(parts[0]),
                                (PathBuf::from(parts[1]), sz),
                            );
                        }
                    }
                }
            }
        }
    }

    log_event(
        LogLevel::Info,
        format!(
            "[Resume] Initialized unified completion journal '{}' with {} known completed file(s).",
            journal_path.display(),
            map.len()
        ),
    );

    let _ = COMPLETED_FILES_JOURNAL.set(CompletedFilesJournal {
        path: journal_path,
        entries: Mutex::new(map),
    });
}

pub fn record_completed_file_in_journal(source_path: &Path, dest_path: &Path) {
    let Ok(meta) = fs::metadata(dest_path) else {
        return;
    };
    let dest_size = meta.len();
    if dest_size == 0 {
        return;
    }
    if let Some(journal) = COMPLETED_FILES_JOURNAL.get() {
        if let Ok(mut guard) = journal.entries.lock() {
            guard.insert(
                source_path.to_path_buf(),
                (dest_path.to_path_buf(), dest_size),
            );
            if let Ok(mut f) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&journal.path)
            {
                let _ = writeln!(
                    f,
                    "{}\t{}\t{}",
                    source_path.display(),
                    dest_path.display(),
                    dest_size
                );
            }
        }
    }
}

pub fn is_file_verified_in_journal(source_path: &Path, dest_path: &Path, current_dest_size: u64) -> bool {
    if current_dest_size == 0 {
        return false;
    }
    if let Some(journal) = COMPLETED_FILES_JOURNAL.get() {
        if let Ok(guard) = journal.entries.lock() {
            if let Some((recorded_dest, recorded_size)) = guard.get(source_path) {
                if recorded_dest == dest_path && *recorded_size == current_dest_size {
                    return true;
                }
            }
        }
    }
    false
}

// --- Persistent Broken Files Journal (`broken_files.journal`) ---
pub struct BrokenFilesJournal {
    pub path: PathBuf,
    pub entries: Mutex<HashSet<PathBuf>>,
}

static BROKEN_FILES_JOURNAL: OnceLock<BrokenFilesJournal> = OnceLock::new();

pub fn is_remote_out_of_space_error(err_str: &str) -> bool {
    let lower = err_str.to_lowercase();
    lower.contains("no space left on device")
        || lower.contains("not enough space on the disk")
        || lower.contains("disk quota exceeded")
        || lower.contains("enospc")
        || lower.contains("error code: -28")
}

pub fn init_broken_files_journal(journal_path: PathBuf) {
    let mut set: HashSet<PathBuf> = HashSet::new();
    if journal_path.exists() {
        if let Ok(file) = File::open(&journal_path) {
            let reader = BufReader::new(file);
            for line in reader.lines().map_while(|r| r.ok()) {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if is_remote_out_of_space_error(trimmed) {
                    continue;
                }
                if let Some(first_col) = trimmed.split('\t').next() {
                    if !first_col.is_empty() {
                        set.insert(PathBuf::from(first_col));
                    }
                }
            }
        }
    }
    log_event(
        LogLevel::Info,
        format!(
            "[BrokenFiles] Initialized broken files journal '{}' with {} known broken file(s).",
            journal_path.display(),
            set.len()
        ),
    );
    let _ = BROKEN_FILES_JOURNAL.set(BrokenFilesJournal {
        path: journal_path,
        entries: Mutex::new(set),
    });
}

pub fn is_file_marked_broken(source_path: &Path) -> bool {
    if let Some(journal) = BROKEN_FILES_JOURNAL.get() {
        if let Ok(guard) = journal.entries.lock() {
            return guard.contains(source_path);
        }
    }
    false
}

pub fn record_broken_file_in_journal(source_path: &Path, file_size_bytes: u64, reason: &str) {
    if is_remote_out_of_space_error(reason) {
        return;
    }
    let clean_reason: String = reason
        .chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string();
    let epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    if let Some(journal) = BROKEN_FILES_JOURNAL.get() {
        if let Ok(mut guard) = journal.entries.lock() {
            if guard.insert(source_path.to_path_buf()) {
                if let Ok(mut f) = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&journal.path)
                {
                    let _ = writeln!(
                        f,
                        "{}\t{}\t{}\t{}",
                        source_path.display(),
                        file_size_bytes,
                        clean_reason,
                        epoch_secs
                    );
                }
            }
        }
    }
}

// --- Live UI State Structures ---
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActiveFileProgress {
    pub slot_id: usize,
    pub filename: String,
    pub codec: String,
    pub bitrate_kbps: u64,
    pub percent: f64,
    pub out_time_secs: f64,
    pub total_duration_secs: f64,
    pub speed: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkerUiSnapshot {
    pub worker_key: String,
    pub is_remote: bool,
    pub header_title: String,
    pub free_space_bytes: u64,
    pub bytes_saved_total: u64,
    pub current_root_dir: String,
    pub queued_items_remaining: usize,
    pub ready_to_encode_count: usize,
    pub current_target_label: String,
    pub current_target_size_bytes: u64,
    pub phase: String,
    pub persistent_error: Option<String>,
    pub batch_total_files: usize,
    pub batch_completed_files: usize,
    pub batch_converted_files: usize,
    pub batch_verified_files: usize,
    pub batch_moved_files: usize,
    pub batch_skipped_low_br_files: usize,
    pub batch_error_files: usize,
    pub active_files: Vec<ActiveFileProgress>,
    pub finished: bool,
}

#[allow(clippy::too_many_arguments)]
fn update_ui_slot(
    shared_ui: &Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    worker_key: &str,
    slot_id: usize,
    filename: &str,
    codec: &str,
    bitrate_kbps: u64,
    out_time_secs: f64,
    total_duration_secs: f64,
    percent: f64,
    speed: &str,
    status: &str,
) {
    if let Ok(mut map) = shared_ui.lock() {
        if let Some(w) = map.get_mut(worker_key) {
            if let Some(slot) = w.active_files.iter_mut().find(|s| s.slot_id == slot_id) {
                slot.filename = filename.to_string();
                slot.codec = codec.to_string();
                slot.bitrate_kbps = bitrate_kbps;
                slot.out_time_secs = out_time_secs;
                slot.total_duration_secs = total_duration_secs;
                slot.percent = percent;
                slot.speed = speed.to_string();
                slot.status = status.to_string();
            } else {
                w.active_files.push(ActiveFileProgress {
                    slot_id,
                    filename: filename.to_string(),
                    codec: codec.to_string(),
                    bitrate_kbps,
                    percent,
                    out_time_secs,
                    total_duration_secs,
                    speed: speed.to_string(),
                    status: status.to_string(),
                });
            }
        }
    }
}

fn remove_ui_slot(
    shared_ui: &Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    worker_key: &str,
    slot_id: usize,
) {
    if let Ok(mut map) = shared_ui.lock() {
        if let Some(w) = map.get_mut(worker_key) {
            w.active_files.retain(|s| s.slot_id != slot_id);
        }
    }
}

// --- Drive & Mount-Point Detection ---
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveType {
    Hdd,
    Ssd,
    Nvme,
}

impl DriveType {
    pub fn as_str(&self) -> &'static str {
        match self {
            DriveType::Hdd => "HDD",
            DriveType::Ssd => "SSD",
            DriveType::Nvme => "NVMe",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MountInfo {
    pub mount_point: String,
    pub device: String,
    pub drive_type: DriveType,
}

pub fn get_mount_info(dir: &Path) -> MountInfo {
    let target = if dir.exists() {
        fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
    } else {
        dir.parent()
            .and_then(|p| fs::canonicalize(p).ok())
            .unwrap_or_else(|| dir.to_path_buf())
    };

    #[cfg(windows)]
    {
        let (mount_point, device, is_removable_hdd) = platform::get_windows_mount_info(&target);
        return MountInfo {
            mount_point,
            device,
            drive_type: if is_removable_hdd {
                DriveType::Hdd
            } else {
                DriveType::Ssd
            },
        };
    }

    #[cfg(not(windows))]
    {
        let output = Command::new("df")
            .arg("-P")
            .arg(&target)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();

        if let Ok(out) = output {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(line) = text.lines().nth(1) {
                let cols: Vec<&str> = line.split_whitespace().collect();
                if cols.len() >= 6 {
                    let device = cols[0].to_string();
                    let mount_point = cols[5..].join(" ");
                    let drive_type = detect_drive_type(&device, &mount_point);
                    return MountInfo {
                        mount_point,
                        device,
                        drive_type,
                    };
                }
            }
        }

        MountInfo {
            mount_point: "/".to_string(),
            device: "unknown".to_string(),
            drive_type: DriveType::Ssd,
        }
    }
}

#[cfg(not(windows))]
fn detect_drive_type(device: &str, mount_point: &str) -> DriveType {
    if cfg!(target_os = "macos") {
        let output = Command::new("diskutil")
            .args(["info", mount_point])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        if let Ok(out) = output {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut is_solid_state: Option<bool> = None;
            let mut protocol = String::new();

            for line in text.lines() {
                let trimmed = line.trim();
                if let Some((k, v)) = trimmed.split_once(':') {
                    let key = k.trim().to_lowercase();
                    let val = v.trim().to_lowercase();
                    if key == "solid state" {
                        if val.contains("yes") {
                            is_solid_state = Some(true);
                        } else if val.contains("no") {
                            is_solid_state = Some(false);
                        }
                    } else if key == "protocol" {
                        protocol = val;
                    }
                }
            }

            if is_solid_state == Some(false) {
                return DriveType::Hdd;
            }
            if protocol.contains("nvme")
                || protocol.contains("apple fabric")
                || protocol.contains("pci")
            {
                return DriveType::Nvme;
            }
            if is_solid_state == Some(true) {
                return DriveType::Ssd;
            }
        }
        if mount_point.starts_with("/Volumes/") {
            return DriveType::Hdd;
        }
    } else if cfg!(target_os = "linux") {
        let dev_name = Path::new(device)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if dev_name.starts_with("nvme") {
            return DriveType::Nvme;
        }
        let base_block: String = dev_name
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        if !base_block.is_empty() {
            let rot_path = format!("/sys/block/{}/queue/rotational", base_block);
            if let Ok(rot) = fs::read_to_string(rot_path) {
                if rot.trim() == "1" {
                    return DriveType::Hdd;
                } else if rot.trim() == "0" {
                    return DriveType::Ssd;
                }
            }
        }
    }

    DriveType::Ssd
}

pub fn get_available_free_space_bytes(path: &Path) -> u64 {
    if let Some((avail_bytes, _total_bytes)) = get_mount_space_stats_syscall(path) {
        return avail_bytes;
    }

    let output = Command::new("df")
        .arg("-k")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = text.lines().nth(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() >= 4 {
                if let Ok(avail_kb) = cols[3].parse::<u64>() {
                    return avail_kb.saturating_mul(1024);
                }
            }
        }
    }
    0
}

pub fn compute_directory_size_bytes(dir: &Path) -> u64 {
    WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() > 0 && e.file_type().is_dir() {
                !is_subdir_h265_named(e.path(), "_h265")
            } else {
                true
            }
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

pub fn relative_path_has_h265_dir_component(root_dir: &Path, path: &Path, suffix: &str) -> bool {
    let rel = path.strip_prefix(root_dir).unwrap_or(path);
    let mut cur = if path.is_dir() {
        Some(rel)
    } else {
        rel.parent()
    };
    while let Some(dir) = cur {
        if dir.as_os_str().is_empty() {
            break;
        }
        if is_subdir_h265_named(dir, suffix) {
            return true;
        }
        cur = dir.parent();
    }
    false
}

pub fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= GB {
        format!("{:.2} GB", bytes as f64 / GB)
    } else {
        format!("{:.2} MB", bytes as f64 / MB)
    }
}

pub fn format_duration_hms(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return "00:00".to_string();
    }
    let total = secs.round() as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{:02}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

// --- Video & FFmpeg Helpers ---
#[derive(Clone, Debug)]
struct EncoderProfile {
    name: String,
    target: codecs::ValidatedTargetConfig,
    decode_args: Vec<String>,
    hwaccel_args: Vec<String>,
    encoder_args: Vec<String>,
    fallback_video_filter_args: Vec<String>,
    audio_args: Vec<String>,
    muxer_args: Vec<String>,
}

static GLOBAL_TARGET_PROFILE: OnceLock<codecs::ResolvedTargetProfile> = OnceLock::new();
static LOCAL_AAC_AT_SUPPORTED: OnceLock<bool> = OnceLock::new();

fn local_ffmpeg_supports_aac_at() -> bool {
    *LOCAL_AAC_AT_SUPPORTED.get_or_init(|| {
        if std::env::consts::OS != "macos" {
            return false;
        }
        Command::new("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("aac_at"))
            .unwrap_or(false)
    })
}

fn detect_encoder_profile(config: &AppConfig) -> EncoderProfile {
    if let Some(resolved) = GLOBAL_TARGET_PROFILE.get() {
        return EncoderProfile {
            name: resolved.display_name.clone(),
            target: resolved.target,
            decode_args: resolved.decode_args.clone(),
            hwaccel_args: resolved.hwaccel_args.clone(),
            encoder_args: resolved.encoder_args.clone(),
            fallback_video_filter_args: resolved.fallback_video_filter_args.clone(),
            audio_args: resolved.audio_args.clone(),
            muxer_args: resolved.muxer_args.clone(),
        };
    }

    let target = codecs::validate_config_targets(config).unwrap_or(codecs::ValidatedTargetConfig {
        video: codecs::TargetVideoCodec::Hevc,
        audio: codecs::TargetAudioCodec::Aac,
        container: codecs::TargetContainer::Mp4,
    });
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    let default_audio = |is_mac: bool| -> Vec<String> {
        if is_mac && !config.FFMPEG_MACOS_AUDIO_ARGS.is_empty() && local_ffmpeg_supports_aac_at() {
            config.FFMPEG_MACOS_AUDIO_ARGS.clone()
        } else if !config.FFMPEG_AUDIO_ARGS.is_empty() {
            config.FFMPEG_AUDIO_ARGS.clone()
        } else {
            let enc = match target.audio {
                codecs::TargetAudioCodec::Aac if is_mac && local_ffmpeg_supports_aac_at() => "aac_at",
                codecs::TargetAudioCodec::Aac => "aac",
                codecs::TargetAudioCodec::Opus => "libopus",
                codecs::TargetAudioCodec::Vorbis => "libvorbis",
                codecs::TargetAudioCodec::Mp3 => "libmp3lame",
                codecs::TargetAudioCodec::Flac => "flac",
                codecs::TargetAudioCodec::Ac3 => "ac3",
                codecs::TargetAudioCodec::Copy => "copy",
                codecs::TargetAudioCodec::None => "none",
            };
            codecs::default_audio_encoder_args(enc, target.audio, false)
        }
    };

    let (enc_name, raw_hwaccel, enc_args) = if os == "macos" && arch == "aarch64" {
        let fallback_enc = match target.video {
            codecs::TargetVideoCodec::Hevc => "hevc_videotoolbox",
            codecs::TargetVideoCodec::H264 => "h264_videotoolbox",
            codecs::TargetVideoCodec::ProRes => "prores_videotoolbox",
            codecs::TargetVideoCodec::Av1 => "libsvtav1",
            codecs::TargetVideoCodec::Vp9 => "libvpx-vp9",
            codecs::TargetVideoCodec::Vp8 => "libvpx",
            codecs::TargetVideoCodec::Webp => "libwebp_anim",
            codecs::TargetVideoCodec::Gif => "gif",
        };
        let args = if !config.FFMPEG_MACOS_ENCODER_ARGS.is_empty() {
            config.FFMPEG_MACOS_ENCODER_ARGS.clone()
        } else {
            codecs::default_video_encoder_args(fallback_enc, target.video, target.container)
        };
        (
            fallback_enc.to_string(),
            config.FFMPEG_MACOS_HWACCEL_ARGS.clone(),
            args,
        )
    } else if (os == "linux" || os == "windows")
        && Command::new("nvidia-smi")
            .arg("-L")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    {
        let fallback_enc = match target.video {
            codecs::TargetVideoCodec::Hevc => "hevc_nvenc",
            codecs::TargetVideoCodec::H264 => "h264_nvenc",
            codecs::TargetVideoCodec::Av1 => "av1_nvenc",
            codecs::TargetVideoCodec::Vp9 => "libvpx-vp9",
            codecs::TargetVideoCodec::Vp8 => "libvpx",
            codecs::TargetVideoCodec::ProRes => "prores_ks",
            codecs::TargetVideoCodec::Webp => "libwebp_anim",
            codecs::TargetVideoCodec::Gif => "gif",
        };
        let args = if !config.FFMPEG_NVIDIA_ENCODER_ARGS.is_empty() {
            config.FFMPEG_NVIDIA_ENCODER_ARGS.clone()
        } else {
            codecs::default_video_encoder_args(fallback_enc, target.video, target.container)
        };
        (
            fallback_enc.to_string(),
            config.FFMPEG_NVIDIA_HWACCEL_ARGS.clone(),
            args,
        )
    } else {
        let fallback_enc = match target.video {
            codecs::TargetVideoCodec::Hevc => "libx265",
            codecs::TargetVideoCodec::H264 => "libx264",
            codecs::TargetVideoCodec::Av1 => "libsvtav1",
            codecs::TargetVideoCodec::Vp9 => "libvpx-vp9",
            codecs::TargetVideoCodec::Vp8 => "libvpx",
            codecs::TargetVideoCodec::ProRes => "prores_ks",
            codecs::TargetVideoCodec::Webp => "libwebp_anim",
            codecs::TargetVideoCodec::Gif => "gif",
        };
        let args = if !config.FFMPEG_CPU_ENCODER_ARGS.is_empty() {
            config.FFMPEG_CPU_ENCODER_ARGS.clone()
        } else {
            codecs::default_video_encoder_args(fallback_enc, target.video, target.container)
        };
        (fallback_enc.to_string(), Vec::new(), args)
    };

    EncoderProfile {
        name: format!("{} ({})", enc_name, target.container.as_str()),
        target,
        decode_args: config.FFMPEG_DECODE_ARGS.clone(),
        hwaccel_args: codecs::sanitize_hwaccel_args_for_encoder(&raw_hwaccel, &enc_name),
        encoder_args: enc_args,
        fallback_video_filter_args: codecs::sanitize_fallback_filter_args(
            &config.FFMPEG_FALLBACK_VIDEO_FILTER_ARGS,
            target.video,
        ),
        audio_args: default_audio(os == "macos"),
        muxer_args: target.container.muxer_args(),
    }
}

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_SIGNAL_NUM: AtomicUsize = AtomicUsize::new(0);

pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Relaxed)
}

pub fn request_graceful_shutdown(reason: &str) {
    if !SHUTDOWN_REQUESTED.swap(true, Ordering::SeqCst) {
        log_event(
            LogLevel::Warn,
            format!(
                "[Shutdown] Graceful exit requested ({}). Stopping in-flight scp-up & ffmpeg encodes; draining scp-down & RAMDisk->HDD flushes...",
                reason
            ),
        );
    }
}

pub fn install_signal_handlers() {
    platform::install_os_signal_handlers(&SHUTDOWN_REQUESTED, &SHUTDOWN_SIGNAL_NUM);
}

/// Spawns a lightweight watchdog that immediately terminates `child_pid` (and its process group/tree)
/// as soon as `is_shutdown_requested()` becomes true, unless `finished_flag` is already set.
fn spawn_shutdown_killer(child_pid: u32, finished_flag: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !finished_flag.load(Ordering::Relaxed) {
            if is_shutdown_requested() {
                if child_pid > 0 {
                    platform::terminate_process_tree(child_pid, false);
                    thread::sleep(Duration::from_millis(120));
                    if !finished_flag.load(Ordering::Relaxed) {
                        platform::terminate_process_tree(child_pid, true);
                    }
                }
                break;
            }
            thread::sleep(Duration::from_millis(60));
        }
    })
}

pub struct TerminalInputGuard {
    _raw_guard: platform::RawTerminalGuard,
    stop_flag: Arc<AtomicBool>,
    listener_handle: Option<thread::JoinHandle<()>>,
}

/// Processes a chunk of raw terminal input bytes (including ANSI escape sequences)
/// and updates `TuiInteractiveState`. Returns `true` if graceful shutdown was triggered.
pub fn process_tui_input_chunk(
    chunk: &[u8],
    tui_state: &Arc<Mutex<TuiInteractiveState>>,
    mount_queues: &[Arc<MountQueueState>],
    page_rows: usize,
) -> bool {
    let page_step = page_rows.max(5);
    let half_step = (page_step / 2).max(1);
    let mut i = 0;

    while i < chunk.len() {
        let byte = chunk[i];

        // Ctrl+C always triggers graceful shutdown in any view mode
        if byte == 0x03 {
            request_graceful_shutdown("Ctrl+C");
            return true;
        }

        // Check for ANSI escape sequence vs standalone Esc key
        let mut is_standalone_esc = false;
        let mut arrow_up = false;
        let mut arrow_down = false;
        let mut page_up = false;
        let mut page_down = false;
        let mut home_key = false;
        let mut end_key = false;

        if byte == 0x1b {
            if i + 2 < chunk.len() && (chunk[i + 1] == b'[' || chunk[i + 1] == b'O') {
                let code = chunk[i + 2];
                match code {
                    b'A' => {
                        arrow_up = true;
                        i += 3;
                    }
                    b'B' => {
                        arrow_down = true;
                        i += 3;
                    }
                    b'H' => {
                        home_key = true;
                        i += 3;
                    }
                    b'F' => {
                        end_key = true;
                        i += 3;
                    }
                    b'1' | b'4' | b'5' | b'6' if i + 3 < chunk.len() && chunk[i + 3] == b'~' => {
                        match code {
                            b'1' => home_key = true,
                            b'4' => end_key = true,
                            b'5' => page_up = true,
                            b'6' => page_down = true,
                            _ => {}
                        }
                        i += 4;
                    }
                    _ => {
                        i += 3;
                        continue;
                    }
                }
            } else {
                is_standalone_esc = true;
                i += 1;
            }
        } else {
            i += 1;
        }

        let mut snapshot_queue_idx: Option<usize> = None;
        let mut should_quit = false;

        if let Ok(mut state) = tui_state.lock() {
            match state.mode {
                TuiViewMode::Dashboard => {
                    if byte == b'q' || byte == b'Q' {
                        should_quit = true;
                    } else if byte == b'v' || byte == b'V' {
                        state.mode = TuiViewMode::QueueSelector;
                        if !mount_queues.is_empty() && state.selected_queue_idx >= mount_queues.len()
                        {
                            state.selected_queue_idx = 0;
                        }
                        state.command_input_active = false;
                        state.command_buffer.clear();
                        state.status_message = None;
                    }
                }
                TuiViewMode::QueueSelector => {
                    if is_standalone_esc
                        || byte == b'v'
                        || byte == b'V'
                        || byte == b'q'
                        || byte == b'Q'
                    {
                        state.mode = TuiViewMode::Dashboard;
                        state.status_message = None;
                    } else if arrow_up || byte == b'k' || byte == b'K' {
                        if !mount_queues.is_empty() {
                            if state.selected_queue_idx == 0 {
                                state.selected_queue_idx = mount_queues.len() - 1;
                            } else {
                                state.selected_queue_idx -= 1;
                            }
                        }
                    } else if arrow_down || byte == b'j' || byte == b'J' {
                        if !mount_queues.is_empty() {
                            state.selected_queue_idx =
                                (state.selected_queue_idx + 1) % mount_queues.len();
                        }
                    } else if (b'1'..=b'9').contains(&byte) {
                        let idx = (byte - b'1') as usize;
                        if idx < mount_queues.len() {
                            state.selected_queue_idx = idx;
                            snapshot_queue_idx = Some(idx);
                        }
                    } else if byte == b'\r' || byte == b'\n' {
                        if !mount_queues.is_empty() {
                            let idx = state
                                .selected_queue_idx
                                .min(mount_queues.len().saturating_sub(1));
                            state.selected_queue_idx = idx;
                            snapshot_queue_idx = Some(idx);
                        }
                    }
                }
                TuiViewMode::QueuePager => {
                    if state.command_input_active {
                        if is_standalone_esc {
                            state.command_input_active = false;
                            state.command_buffer.clear();
                            state.status_message = Some("Command cancelled.".to_string());
                        } else if byte == 0x7f || byte == 0x08 {
                            if state.command_buffer.pop().is_none() {
                                state.command_input_active = false;
                            }
                        } else if byte == b'\r' || byte == b'\n' {
                            let cmd_str = state.command_buffer.trim().to_string();
                            state.command_input_active = false;
                            state.command_buffer.clear();
                            if cmd_str.eq_ignore_ascii_case("q")
                                || cmd_str.eq_ignore_ascii_case(":q")
                            {
                                state.mode = TuiViewMode::QueueSelector;
                                state.active_snapshot = None;
                                state.status_message = None;
                            } else if let Some(ref mut snap) = state.active_snapshot {
                                let msg = match parse_and_apply_queue_command(snap, &cmd_str) {
                                    Ok(m) | Err(m) => m,
                                };
                                state.status_message = Some(msg);
                            }
                        } else if !arrow_up
                            && !arrow_down
                            && !page_up
                            && !page_down
                            && !home_key
                            && !end_key
                            && (0x20..=0x7e).contains(&byte)
                        {
                            state.command_buffer.push(byte as char);
                        }
                    } else {
                        let total_rows = state
                            .active_snapshot
                            .as_ref()
                            .map(|s| s.displayed_entries.len())
                            .unwrap_or(0);
                        let max_scroll = total_rows.saturating_sub(page_step);

                        if is_standalone_esc || byte == b'q' || byte == b'Q' {
                            state.mode = TuiViewMode::QueueSelector;
                            state.active_snapshot = None;
                            state.command_input_active = false;
                            state.command_buffer.clear();
                            state.status_message = None;
                        } else if byte == b':' {
                            state.command_input_active = true;
                            state.command_buffer.clear();
                            state.status_message = None;
                        } else if arrow_up || byte == b'k' || byte == b'K' {
                            if let Some(ref mut snap) = state.active_snapshot {
                                snap.scroll_offset = snap.scroll_offset.saturating_sub(1);
                            }
                        } else if arrow_down || byte == b'j' || byte == b'J' {
                            if let Some(ref mut snap) = state.active_snapshot {
                                snap.scroll_offset = (snap.scroll_offset + 1).min(max_scroll);
                            }
                        } else if page_up || byte == b'b' || byte == b'B' {
                            if let Some(ref mut snap) = state.active_snapshot {
                                snap.scroll_offset = snap.scroll_offset.saturating_sub(page_step);
                            }
                        } else if page_down || byte == b' ' || byte == b'f' || byte == b'F' {
                            if let Some(ref mut snap) = state.active_snapshot {
                                snap.scroll_offset =
                                    (snap.scroll_offset + page_step).min(max_scroll);
                            }
                        } else if byte == b'u' || byte == b'U' {
                            if let Some(ref mut snap) = state.active_snapshot {
                                snap.scroll_offset = snap.scroll_offset.saturating_sub(half_step);
                            }
                        } else if byte == b'd' || byte == b'D' {
                            if let Some(ref mut snap) = state.active_snapshot {
                                snap.scroll_offset =
                                    (snap.scroll_offset + half_step).min(max_scroll);
                            }
                        } else if home_key || byte == b'g' {
                            if let Some(ref mut snap) = state.active_snapshot {
                                snap.scroll_offset = 0;
                            }
                        } else if end_key || byte == b'G' {
                            if let Some(ref mut snap) = state.active_snapshot {
                                snap.scroll_offset = max_scroll;
                            }
                        }
                    }
                }
            }
        }

        if should_quit {
            request_graceful_shutdown("key 'q'");
            return true;
        }

        if let Some(idx) = snapshot_queue_idx {
            if let Some(mq) = mount_queues.get(idx) {
                let snap = snapshot_mount_queue_to_flat_list(mq);
                if let Ok(mut state) = tui_state.lock() {
                    state.active_snapshot = Some(snap);
                    state.mode = TuiViewMode::QueuePager;
                    state.command_input_active = false;
                    state.command_buffer.clear();
                    state.status_message = Some(
                        "Snapshot captured. Type :s1a/:s1d (name), :s2a/:s2d (size), :s3a/:s3d (status), :r (reset), Esc (back)".to_string(),
                    );
                }
            }
        }
    }

    false
}

impl TerminalInputGuard {
    pub fn start(
        tui_state: Arc<Mutex<TuiInteractiveState>>,
        mount_queues: Vec<Arc<MountQueueState>>,
    ) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let (raw_guard, reader_opt) = platform::RawTerminalGuard::enable();

        let listener_handle = reader_opt.map(|mut reader| {
            let stop_clone = Arc::clone(&stop_flag);
            thread::spawn(move || {
                let mut buf = [0u8; 32];
                while !stop_clone.load(Ordering::Relaxed) && !is_shutdown_requested() {
                    let n = reader.read_chunk(&mut buf);
                    if n > 0 {
                        let (_cols, rows) = get_terminal_dimensions();
                        let page_rows = rows.saturating_sub(10).max(5);
                        if process_tui_input_chunk(
                            &buf[..n],
                            &tui_state,
                            &mount_queues,
                            page_rows,
                        ) {
                            break;
                        }
                    }
                    if SHUTDOWN_SIGNAL_NUM.load(Ordering::Relaxed) != 0 {
                        request_graceful_shutdown("OS termination signal");
                        break;
                    }
                }
            })
        });

        Self {
            _raw_guard: raw_guard,
            stop_flag,
            listener_handle,
        }
    }
}

impl Drop for TerminalInputGuard {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.listener_handle.take() {
            let _ = h.join();
        }
    }
}

pub fn get_terminal_dimensions() -> (usize, usize) {
    if let Some(dims) = platform::get_os_terminal_dimensions() {
        return dims;
    }
    let cols = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .filter(|&c| c >= 40)
        .unwrap_or(96);
    let rows = std::env::var("LINES")
        .ok()
        .and_then(|r| r.parse::<usize>().ok())
        .filter(|&r| r >= 10)
        .unwrap_or(50);
    (cols, rows)
}

pub fn get_terminal_width() -> usize {
    get_terminal_dimensions().0
}

static PRIORITY_WARNED: AtomicBool = AtomicBool::new(false);

fn try_set_process_priority(pid: u32, nice_value: i32, verbose: bool) {
    if nice_value == 0 {
        return;
    }
    if let Err(err) = platform::set_child_process_priority(pid, nice_value) {
        if !PRIORITY_WARNED.swap(true, Ordering::Relaxed) {
            vlog!(
                verbose,
                "[Worker] NOTE: Could not set ffmpeg nice value to {} ({}). Continuing with default OS priority.",
                nice_value,
                err
            );
        }
    }
}

pub fn is_valid_video_file(path: &Path, config: &AppConfig) -> bool {
    if !path.is_file() {
        return false;
    }
    if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
        if fname.starts_with('.') {
            return false;
        }
        let lower = fname.to_lowercase();
        return config
            .VIDEO_EXTENSIONS
            .iter()
            .any(|ext| lower.ends_with(&ext.to_lowercase()));
    }
    false
}

pub fn is_h265_named(path: &Path, suffix: &str) -> bool {
    let suffix_lower = suffix.to_lowercase();
    let match_legacy_h265 = suffix_lower == "_h265";
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|stem| {
            let lower = stem.to_lowercase();
            lower.ends_with(&suffix_lower) || (match_legacy_h265 && lower.ends_with("h265"))
        })
        .unwrap_or(false)
}

pub fn is_subdir_h265_named(dir: &Path, suffix: &str) -> bool {
    let suffix_lower = suffix.to_lowercase();
    let match_legacy_h265 = suffix_lower == "_h265";
    dir.file_name()
        .and_then(|s| s.to_str())
        .map(|name| {
            let trimmed = name.trim_end_matches(|c: char| c.is_whitespace() || c == '.');
            let lower = trimmed.to_lowercase();
            lower.ends_with(&suffix_lower) || (match_legacy_h265 && lower.ends_with("h265"))
        })
        .unwrap_or(false)
}

pub fn sanitize_dir_or_file_component(name: &str) -> String {
    name.trim_matches(|c: char| c == '\r' || c == '\n' || c == '\0')
        .trim_end_matches(' ')
        .to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VideoProbeInfo {
    pub video_codec: String,
    pub audio_codec: String,
    pub duration_secs: f64,
    #[serde(default)]
    pub video_stream_duration_secs: Option<f64>,
    #[serde(default)]
    pub audio_stream_duration_secs: Option<f64>,
    #[serde(default)]
    pub format_duration_secs: Option<f64>,
    pub bitrate_kbps: u64,
    pub file_size_bytes: u64,
    #[serde(default)]
    pub probe_error: Option<String>,
}

pub fn is_mp4_compatible_audio(audio_codec: &str) -> bool {
    matches!(
        audio_codec,
        "aac" | "mp3" | "ac3" | "eac3" | "alac" | "mp2"
    )
}

pub fn is_macos_hw_decode_supported(video_codec: &str) -> bool {
    matches!(video_codec, "h264" | "hevc" | "prores")
}

/// Verifies both internal output A/V stream duration alignment (`|out_video - out_audio|`)
/// and source-vs-output duration match using the container-aware thresholds in `config.yaml`.
pub fn verify_output_durations(
    source_path: &Path,
    src_probe: &VideoProbeInfo,
    out_probe: &VideoProbeInfo,
    config: &AppConfig,
) -> std::result::Result<(), String> {
    let is_audio_less_target = codecs::validate_config_targets(config)
        .map(|t| t.container.is_audio_less())
        .unwrap_or(false);

    if is_audio_less_target {
        if out_probe.video_codec == "unknown" || out_probe.file_size_bytes == 0 {
            return Err(format!(
                "Invalid animated image output stream (codec={}, bytes={})",
                out_probe.video_codec, out_probe.file_size_bytes
            ));
        }
        // ffprobe often reports duration=N/A (0.0s) for animated WebP/GIF; valid stream + non-zero size passes.
        return Ok(());
    }

    if out_probe.video_codec == "unknown" || out_probe.duration_secs <= 0.0 {
        return Err(format!(
            "Invalid output stream (codec={}, duration={:.2}s)",
            out_probe.video_codec, out_probe.duration_secs
        ));
    }

    // 1. Internal Output Audio vs. Video Stream Duration Check (within AV_STREAM_MISMATCH_TOLERANCE_SECS)
    //    Only enforced when the source itself did not already have an intentional A/V length difference.
    if let (Some(out_v), Some(out_a)) = (
        out_probe.video_stream_duration_secs.filter(|&d| d > 0.0),
        out_probe.audio_stream_duration_secs.filter(|&d| d > 0.0),
    ) {
        let av_tol = config.AV_STREAM_MISMATCH_TOLERANCE_SECS.max(0.5);
        let src_av_matched = match (
            src_probe.video_stream_duration_secs.filter(|&d| d > 0.0),
            src_probe.audio_stream_duration_secs.filter(|&d| d > 0.0),
        ) {
            (Some(sv), Some(sa)) => (sv - sa).abs() <= av_tol,
            _ => true,
        };
        if src_av_matched && (out_v - out_a).abs() > av_tol {
            return Err(format!(
                "Output A/V stream duration desync: output_video={:.2}s vs output_audio={:.2}s (tolerance={:.2}s)",
                out_v, out_a, av_tol
            ));
        }
    }

    if src_probe.duration_secs <= 0.0 {
        return Ok(());
    }

    // 2. Container-aware source-vs-output tolerance from config.yaml
    let ext = source_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let is_legacy_container = matches!(
        ext.as_str(),
        "wmv" | "asf" | "avi" | "flv" | "mpg" | "mpeg"
    );
    let allowed_delta = if is_legacy_container {
        let preroll_min = config.LEGACY_CONTAINER_PREROLL_TOLERANCE_SECS.max(1.5);
        let max_cap = config.LEGACY_CONTAINER_MAX_TOLERANCE_SECS.max(preroll_min);
        let pct_allowance =
            src_probe.duration_secs * (config.LEGACY_CONTAINER_VFR_TOLERANCE_PCT.max(0.0) / 100.0);
        pct_allowance.clamp(preroll_min, max_cap)
    } else {
        config.DURATION_TOLERANCE_SECS.max(0.5)
    };

    // 3. Compare output durations against source video/format/audio durations
    let mut src_candidates = vec![src_probe.duration_secs];
    if let Some(v) = src_probe.video_stream_duration_secs.filter(|&d| d > 0.0) {
        src_candidates.push(v);
    }
    if let Some(f) = src_probe.format_duration_secs.filter(|&d| d > 0.0) {
        src_candidates.push(f);
    }
    if let Some(a) = src_probe.audio_stream_duration_secs.filter(|&d| d > 0.0) {
        src_candidates.push(a);
    }

    let out_candidates = [
        Some(out_probe.duration_secs),
        out_probe.video_stream_duration_secs.filter(|&d| d > 0.0),
        out_probe.format_duration_secs.filter(|&d| d > 0.0),
    ];

    for sd in &src_candidates {
        for od in out_candidates.iter().flatten() {
            if (*sd - *od).abs() <= allowed_delta {
                return Ok(());
            }
        }
    }

    Err(format!(
        "source={:.2}s (v={:?}, a={:?}, fmt={:?}) vs output={:.2}s (v={:?}, a={:?}, fmt={:?}) [tolerance={:.2}s]",
        src_probe.duration_secs,
        src_probe.video_stream_duration_secs,
        src_probe.audio_stream_duration_secs,
        src_probe.format_duration_secs,
        out_probe.duration_secs,
        out_probe.video_stream_duration_secs,
        out_probe.audio_stream_duration_secs,
        out_probe.format_duration_secs,
        allowed_delta
    ))
}

pub fn get_video_probe_info(filepath: &Path) -> VideoProbeInfo {
    let file_size_bytes = fs::metadata(filepath).map(|m| m.len()).unwrap_or(0);

    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(filepath)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(out) => {
            let stderr_msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if out.status.success() {
                if let Ok(json_val) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                    let mut video_codec = String::from("unknown");
                    let mut audio_codec = String::from("unknown");
                    let mut stream_dur: Option<f64> = None;
                    let mut audio_dur: Option<f64> = None;
                    let mut stream_bps: Option<u64> = None;

                    if let Some(streams) = json_val.get("streams").and_then(|s| s.as_array()) {
                        for s in streams {
                            let codec_type = s
                                .get("codec_type")
                                .and_then(|t| t.as_str())
                                .unwrap_or_default();
                            let is_attached_pic = s
                                .get("disposition")
                                .and_then(|d| d.get("attached_pic"))
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0)
                                == 1;
                            if codec_type == "video" && !is_attached_pic && video_codec == "unknown"
                            {
                                video_codec = s
                                    .get("codec_name")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("unknown")
                                    .to_lowercase();
                                stream_dur = s
                                    .get("duration")
                                    .and_then(|d| d.as_str())
                                    .and_then(|v| v.parse::<f64>().ok())
                                    .filter(|&d| d > 0.0);
                                stream_bps = s
                                    .get("bit_rate")
                                    .and_then(|b| b.as_str())
                                    .and_then(|v| v.parse::<u64>().ok());
                            } else if codec_type == "audio" && audio_codec == "unknown" {
                                audio_codec = s
                                    .get("codec_name")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("unknown")
                                    .to_lowercase();
                                audio_dur = s
                                    .get("duration")
                                    .and_then(|d| d.as_str())
                                    .and_then(|v| v.parse::<f64>().ok())
                                    .filter(|&d| d > 0.0);
                            }
                        }
                    }

                    let format_obj = json_val.get("format");
                    let format_dur = format_obj
                        .and_then(|f| f.get("duration"))
                        .and_then(|d| d.as_str())
                        .and_then(|s| s.parse::<f64>().ok())
                        .filter(|&d| d > 0.0);
                    let format_bps = format_obj
                        .and_then(|f| f.get("bit_rate"))
                        .and_then(|b| b.as_str())
                        .and_then(|s| s.parse::<u64>().ok());

                    let duration_secs = stream_dur.or(format_dur).or(audio_dur).unwrap_or(0.0);
                    let bitrate_kbps = format_bps
                        .or(stream_bps)
                        .map(|bps| bps / 1000)
                        .unwrap_or_else(|| {
                            if duration_secs > 0.5 && file_size_bytes > 0 {
                                ((file_size_bytes as f64 * 8.0) / (duration_secs * 1000.0)) as u64
                            } else {
                                0
                            }
                        });

                    let probe_error = if video_codec == "unknown" || duration_secs <= 0.0 {
                        if !stderr_msg.is_empty() {
                            Some(stderr_msg)
                        } else {
                            Some(format!(
                                "Missing valid video stream or duration (codec={}, duration={:.2}s)",
                                video_codec, duration_secs
                            ))
                        }
                    } else {
                        None
                    };

                    return VideoProbeInfo {
                        video_codec,
                        audio_codec,
                        duration_secs,
                        video_stream_duration_secs: stream_dur,
                        audio_stream_duration_secs: audio_dur,
                        format_duration_secs: format_dur,
                        bitrate_kbps,
                        file_size_bytes,
                        probe_error,
                    };
                }
            }
            let err_detail = if !stderr_msg.is_empty() {
                stderr_msg
            } else {
                format!("ffprobe exited with status {}", out.status)
            };
            VideoProbeInfo {
                video_codec: "unknown".to_string(),
                audio_codec: "unknown".to_string(),
                duration_secs: 0.0,
                video_stream_duration_secs: None,
                audio_stream_duration_secs: None,
                format_duration_secs: None,
                bitrate_kbps: 0,
                file_size_bytes,
                probe_error: Some(err_detail),
            }
        }
        Err(e) => VideoProbeInfo {
            video_codec: "unknown".to_string(),
            audio_codec: "unknown".to_string(),
            duration_secs: 0.0,
            video_stream_duration_secs: None,
            audio_stream_duration_secs: None,
            format_duration_secs: None,
            bitrate_kbps: 0,
            file_size_bytes,
            probe_error: Some(format!("Failed to spawn ffprobe: {}", e)),
        },
    }
}

/// Stage B 1-frame decode smoke test (`ffmpeg -nostdin -v error -i <file> -map 0:v:0 -frames:v 1 -f null -`).
/// Verifies that FFmpeg can open the container and decode the first video frame without error.
pub fn verify_first_frame_decodable(filepath: &Path) -> std::result::Result<(), String> {
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(filepath)
        .args(["-map", "0:v:0", "-frames:v", "1", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run ffmpeg decode check: {}", e))?;

    if !out.status.success() {
        let stderr_msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let detail = if !stderr_msg.is_empty() {
            stderr_msg
        } else {
            format!("ffmpeg 1-frame decode exited with {}", out.status)
        };
        return Err(detail);
    }
    Ok(())
}

pub fn expected_target_candidates_for_source(
    source_file: &Path,
    source_dir: &Path,
    target_dir: &Path,
    config: &AppConfig,
) -> (PathBuf, PathBuf) {
    let rel = source_file.strip_prefix(source_dir).unwrap_or(source_file);
    let rel_parent = rel.parent().unwrap_or_else(|| Path::new(""));
    let mut dest_dir = target_dir.to_path_buf();
    for comp in rel_parent.components() {
        if let std::path::Component::Normal(os_str) = comp {
            if let Some(s) = os_str.to_str() {
                let cleaned = sanitize_dir_or_file_component(s);
                if !cleaned.is_empty() {
                    dest_dir.push(cleaned);
                }
            } else {
                dest_dir.push(os_str);
            }
        }
    }
    let cand_mp4 = dest_dir.join(build_h265_filename(source_file, false, config));
    let cand_orig = dest_dir.join(build_h265_filename(source_file, true, config));
    (cand_mp4, cand_orig)
}

pub fn build_h265_filename(
    source_path: &Path,
    preserve_original_ext: bool,
    config: &AppConfig,
) -> String {
    let raw_stem = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("video");
    let stem = sanitize_dir_or_file_component(raw_stem);
    let suffix = &config.OUTPUT_SUFFIX;

    let match_legacy_h265 = suffix.eq_ignore_ascii_case("_h265");
    let suffixed_stem = if stem.to_lowercase().ends_with(&suffix.to_lowercase())
        || (match_legacy_h265 && stem.to_lowercase().ends_with("h265"))
    {
        stem
    } else {
        format!("{}{}", stem, suffix)
    };

    if preserve_original_ext {
        let orig_ext = source_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", sanitize_dir_or_file_component(e)))
            .unwrap_or_else(|| config.OUTPUT_EXTENSION.clone());
        format!("{}{}", suffixed_stem, orig_ext)
    } else {
        let ext = if config.OUTPUT_EXTENSION.starts_with('.') {
            config.OUTPUT_EXTENSION.clone()
        } else {
            format!(".{}", config.OUTPUT_EXTENSION)
        };
        format!("{}{}", suffixed_stem, ext)
    }
}

fn move_file_safe(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        ensure_dir_created(parent)?;
    }
    if src == dst {
        return Ok(());
    }
    match fs::rename(src, dst) {
        Ok(_) => Ok(()),
        Err(_) => {
            fs::copy(src, dst).with_context(|| {
                format!(
                    "Failed to copy {:?} -> {:?}\nStack Trace:\n{}",
                    src,
                    dst,
                    Backtrace::force_capture()
                )
            })?;
            fs::remove_file(src).with_context(|| {
                format!(
                    "Failed to remove source {:?} after copy\nStack Trace:\n{}",
                    src,
                    Backtrace::force_capture()
                )
            })?;
            Ok(())
        }
    }
}

fn parse_hms_to_secs(hms: &str) -> Option<f64> {
    let parts: Vec<&str> = hms.trim().split(':').collect();
    if parts.len() == 3 {
        let h: f64 = parts[0].parse().ok()?;
        let m: f64 = parts[1].parse().ok()?;
        let s: f64 = parts[2].parse().ok()?;
        Some(h * 3600.0 + m * 60.0 + s)
    } else {
        None
    }
}

// --- Coordinated Subdir Completion & Deletion (Step 2.6) ---
/// Deletes individually verified converted source files inside `source_dir` when a subdir
/// completes (or is interrupted) with partial failures, leaving only failed/corrupt (and any
/// filename-colliding) source files behind for manual inspection. Also prunes empty nested
/// subdirectories strictly inside `source_dir` without removing `source_dir` itself.
fn cleanup_converted_source_files_on_partial_failure(
    source_dir: &Path,
    target_dir: &Path,
    tracked_source_files: &[PathBuf],
    succeeded_files: &[(PathBuf, PathBuf)],
    failed_source_files: &HashSet<PathBuf>,
    config: &AppConfig,
) -> usize {
    // Detect filename collisions across all source files in this subdir (e.g. `clip.mp4` and
    // `clip.wmv` both mapping to `clip_h265.mp4`). Never delete either colliding source file.
    let mut target_claims: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for sfile in tracked_source_files {
        let (cand_mp4, cand_orig) =
            expected_target_candidates_for_source(sfile, source_dir, target_dir, config);
        target_claims.entry(cand_mp4.clone()).or_default().push(sfile.clone());
        if cand_orig != cand_mp4 {
            target_claims.entry(cand_orig).or_default().push(sfile.clone());
        }
    }
    let mut colliding_sources: HashSet<PathBuf> = HashSet::new();
    for sources in target_claims.values() {
        if sources.len() > 1 {
            for s in sources {
                colliding_sources.insert(s.clone());
            }
        }
    }

    let mut deleted_count = 0usize;
    let mut seen_deleted: HashSet<PathBuf> = HashSet::new();
    for (src_file, verified_target) in succeeded_files {
        if src_file == verified_target
            || failed_source_files.contains(src_file)
            || colliding_sources.contains(src_file)
            || !seen_deleted.insert(src_file.clone())
        {
            continue;
        }
        let target_valid = verified_target.exists()
            && fs::metadata(verified_target)
                .map(|m| m.len() > 0)
                .unwrap_or(false);
        if target_valid && src_file.exists() {
            if fs::remove_file(src_file).is_ok() {
                deleted_count += 1;
            }
        }
    }

    // Prune any now-empty nested subdirectories inside `source_dir` (deepest-first),
    // keeping `source_dir` itself intact so the corrupt file(s) remain inspectable.
    if deleted_count > 0 && source_dir.exists() {
        let mut nested_dirs: Vec<PathBuf> = WalkDir::new(source_dir)
            .min_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_dir())
            .map(|e| e.into_path())
            .collect();
        nested_dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        for dir in nested_dirs {
            let _ = fs::remove_dir(&dir);
        }
    }

    deleted_count
}

/// On graceful shutdown (e.g. Ctrl+C mid-directory), flushes any tracked subdirectories that
/// encountered file failures before reaching `completed_files == total_files`: cleans up their
/// already-verified converted source files and records the directory in `FAILED_DIRS_PATH`
/// (`broken_directories.journal`).
fn flush_incomplete_failed_subdirs(
    subdir_trackers: &Arc<Mutex<HashMap<PathBuf, SubdirTracker>>>,
    config: &AppConfig,
) {
    let pending: Vec<(
        PathBuf,
        PathBuf,
        Vec<PathBuf>,
        Vec<(PathBuf, PathBuf)>,
        HashSet<PathBuf>,
        usize,
    )> = if let Ok(mut trackers) = subdir_trackers.lock() {
        let mut out = Vec::new();
        for t in trackers.values_mut() {
            if !t.deleted && t.failed_files > 0 {
                t.deleted = true;
                out.push((
                    t.source_subdir.clone(),
                    t.target_subdir.clone(),
                    t.source_files.clone(),
                    t.succeeded_files.clone(),
                    t.failed_source_files.clone(),
                    t.failed_files,
                ));
            }
        }
        out
    } else {
        Vec::new()
    };

    let failed_dirs_log = PathBuf::from(&config.FAILED_DIRS_PATH);
    for (
        source_dir,
        target_dir,
        tracked_source_files,
        succeeded_files,
        failed_source_files,
        failed_count,
    ) in pending
    {
        let deleted_converted = cleanup_converted_source_files_on_partial_failure(
            &source_dir,
            &target_dir,
            &tracked_source_files,
            &succeeded_files,
            &failed_source_files,
            config,
        );
        let err = anyhow::anyhow!(
            "{} file(s) failed conversion inside subdir '{}' before shutdown ({} converted source file(s) verified in target and deleted from source)",
            failed_count,
            source_dir.display(),
            deleted_converted
        );
        record_failed_dir(
            &failed_dirs_log,
            &source_dir,
            Some(&target_dir),
            "Subdir interrupted with conversion errors; deleted verified converted source files and kept remaining/failed file(s)",
            &err,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn complete_queued_item(
    item: &QueuedMediaItem,
    was_moved: bool,
    was_low_br: bool,
    had_error: bool,
    bytes_saved: u64,
    subdir_trackers: &Arc<Mutex<HashMap<PathBuf, SubdirTracker>>>,
    shared_ui: &Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    worker_key: &str,
    config: &AppConfig,
    verbose: bool,
) {
    if let Ok(mut map) = shared_ui.lock() {
        if let Some(w) = map.get_mut(worker_key) {
            if had_error {
                w.batch_error_files += 1;
            } else if was_moved {
                w.batch_moved_files += 1;
                if was_low_br {
                    w.batch_skipped_low_br_files += 1;
                }
            } else {
                w.batch_converted_files += 1;
            }
            w.batch_completed_files = w.batch_converted_files
                + w.batch_verified_files
                + w.batch_moved_files
                + w.batch_error_files;
            w.bytes_saved_total = w.bytes_saved_total.saturating_add(bytes_saved);
        }
    }

    if let Some(ref src_subdir) = item.source_subdir {
        let verified_dest = if !had_error {
            let cand_enc = item.dest_dir.join(build_h265_filename(&item.source_path, false, config));
            let cand_mov = item.dest_dir.join(build_h265_filename(&item.source_path, true, config));
            if cand_enc.exists() && fs::metadata(&cand_enc).map(|m| m.len() > 0).unwrap_or(false) {
                record_completed_file_in_journal(&item.source_path, &cand_enc);
                Some(cand_enc)
            } else if cand_mov.exists() && fs::metadata(&cand_mov).map(|m| m.len() > 0).unwrap_or(false) {
                record_completed_file_in_journal(&item.source_path, &cand_mov);
                Some(cand_mov)
            } else {
                None
            }
        } else {
            None
        };

        let mut should_verify_and_delete: Option<(
            PathBuf,
            PathBuf,
            Vec<PathBuf>,
            usize,
            usize,
            Vec<(PathBuf, PathBuf)>,
            HashSet<PathBuf>,
        )> = None;
        if let Ok(mut trackers) = subdir_trackers.lock() {
            if let Some(t) = trackers.get_mut(src_subdir) {
                t.completed_files += 1;
                if was_moved && !had_error {
                    t.moved_files += 1;
                }
                if had_error {
                    t.failed_files += 1;
                    t.failed_source_files.insert(item.source_path.clone());
                } else if let Some(ref vdest) = verified_dest {
                    t.succeeded_files.push((item.source_path.clone(), vdest.clone()));
                }
                if !t.deleted && t.completed_files >= t.total_files {
                    t.deleted = true;
                    should_verify_and_delete = Some((
                        t.source_subdir.clone(),
                        t.target_subdir.clone(),
                        t.source_files.clone(),
                        t.moved_files,
                        t.failed_files,
                        t.succeeded_files.clone(),
                        t.failed_source_files.clone(),
                    ));
                }
            }
        }

        if let Some((
            source_dir,
            target_dir,
            tracked_source_files,
            moved_count,
            failed_count,
            succeeded_files,
            failed_source_files,
        )) = should_verify_and_delete
        {
            let failed_dirs_log = PathBuf::from(&config.FAILED_DIRS_PATH);
            if failed_count > 0 {
                let deleted_converted = cleanup_converted_source_files_on_partial_failure(
                    &source_dir,
                    &target_dir,
                    &tracked_source_files,
                    &succeeded_files,
                    &failed_source_files,
                    config,
                );
                global_metrics().record_reconciliation(
                    &item.mount_point,
                    "partial_cleanup_retained_broken",
                    0,
                );
                let local_nodename = config.effective_local_nodename();
                let local_host_os = local_host_os_label();
                log_structured_event(
                    LogLevel::Warn,
                    &local_nodename,
                    local_host_os,
                    worker_key,
                    "flusher",
                    &item.mount_point,
                    Some(item.item_id),
                    "subdir_reconciled",
                    &[
                        ("source_dir", source_dir.display().to_string()),
                        ("target_dir", target_dir.display().to_string()),
                        ("failed_files", failed_count.to_string()),
                        ("deleted_converted_files", deleted_converted.to_string()),
                        ("outcome", "partial_cleanup_retained_broken".to_string()),
                    ],
                );
                let err = anyhow::anyhow!(
                    "{} file(s) failed conversion inside subdir '{}' ({} converted source file(s) verified in target and deleted from source)",
                    failed_count,
                    source_dir.display(),
                    deleted_converted
                );
                record_failed_dir(
                    &failed_dirs_log,
                    &source_dir,
                    Some(&target_dir),
                    "Subdir had conversion errors; deleted verified converted source files and kept failed file(s) for inspection",
                    &err,
                );
                return;
            }

            let remaining_source_files = collect_video_files_recursive(&source_dir, config);
            let remaining_in_source = remaining_source_files.len();
            let total_source_media = remaining_in_source + moved_count;
            let mut target_files = collect_video_files_recursive(&target_dir, config);
            let mut total_target_media = target_files.len();

            vlog!(
                verbose,
                "[Step 2.6] Subdir '{}' completed: source={} ({} remaining + {} moved), target={} in '{}'",
                source_dir.display(),
                total_source_media,
                remaining_in_source,
                moved_count,
                total_target_media,
                target_dir.display()
            );

            let mut deleted_bad_count = 0usize;
            if total_target_media > total_source_media {
                log_event(
                    LogLevel::Warn,
                    format!(
                        "[{}] [Step 2.6] Target media count ({}) > source media count ({}) for '{}'; ffprobing all target files in '{}' and removing any bad files...",
                        worker_key,
                        total_target_media,
                        total_source_media,
                        source_dir.display(),
                        target_dir.display()
                    ),
                );

                // 1. ffprobe all target files and delete any bad/corrupt files
                for tfile in &target_files {
                    let tprobe = get_video_probe_info(tfile);
                    if tprobe.video_codec == "unknown"
                        || tprobe.duration_secs <= 0.0
                        || tprobe.probe_error.is_some()
                    {
                        let reason = tprobe
                            .probe_error
                            .unwrap_or_else(|| "unknown codec or 0s duration".to_string());
                        let _ = fs::remove_file(tfile);
                        deleted_bad_count += 1;
                        log_event(
                            LogLevel::Warn,
                            format!(
                                "[{}] [Step 2.6] Deleted bad target file '{}' (ffprobe: {})",
                                worker_key,
                                tfile.display(),
                                reason
                            ),
                        );
                    }
                }

                // 2. Recheck source and target counts
                target_files = collect_video_files_recursive(&target_dir, config);
                total_target_media = target_files.len();
                log_event(
                    LogLevel::Info,
                    format!(
                        "[{}] [Step 2.6] Rechecked counts for '{}' after removing {} bad target file(s): source={} vs target={}",
                        worker_key,
                        target_dir.display(),
                        deleted_bad_count,
                        total_source_media,
                        total_target_media
                    ),
                );
            }

            let local_nodename = config.effective_local_nodename();
            let local_host_os = local_host_os_label();

            if total_source_media == total_target_media {
                if let Err(e) = fs::remove_dir_all(&source_dir).with_context(|| {
                    format!(
                        "Failed to delete completed subdir {:?}\nStack Trace:\n{}",
                        source_dir,
                        Backtrace::force_capture()
                    )
                }) {
                    record_failed_dir(
                        &failed_dirs_log,
                        &source_dir,
                        Some(&target_dir),
                        &format!("ERROR deleting completed subdir '{}'", source_dir.display()),
                        &e,
                    );
                } else {
                    global_metrics().record_reconciliation(
                        &item.mount_point,
                        "deleted_source",
                        deleted_bad_count as u64,
                    );
                    log_structured_event(
                        LogLevel::Info,
                        &local_nodename,
                        local_host_os,
                        worker_key,
                        "flusher",
                        &item.mount_point,
                        Some(item.item_id),
                        "subdir_reconciled",
                        &[
                            ("source_dir", source_dir.display().to_string()),
                            ("target_dir", target_dir.display().to_string()),
                            ("source_files", total_source_media.to_string()),
                            ("target_files", total_target_media.to_string()),
                            ("bad_target_deleted", deleted_bad_count.to_string()),
                            ("outcome", "deleted_source".to_string()),
                        ],
                    );
                }
            } else {
                // 3. Reconcile source media files against target_dir (both when target > source and when source > target)
                let mut all_source_files = tracked_source_files;
                for rem in remaining_source_files {
                    if !all_source_files.contains(&rem) {
                        all_source_files.push(rem);
                    }
                }
                let target_set: HashSet<PathBuf> = target_files.iter().cloned().collect();
                let mut accounted_target_files: HashSet<PathBuf> = HashSet::new();
                let mut claimed_target_files: HashMap<PathBuf, PathBuf> = HashMap::new();
                let mut missing_source_files: Vec<PathBuf> = Vec::new();
                let mut colliding_source_files: Vec<(PathBuf, PathBuf, PathBuf)> = Vec::new();

                for sfile in &all_source_files {
                    let (cand_mp4, cand_orig) =
                        expected_target_candidates_for_source(sfile, &source_dir, &target_dir, config);
                    let has_mp4 = target_set.contains(&cand_mp4);
                    let has_orig = target_set.contains(&cand_orig);

                    if !has_mp4 && !has_orig {
                        missing_source_files.push(sfile.clone());
                    } else {
                        if has_mp4 {
                            accounted_target_files.insert(cand_mp4.clone());
                        }
                        if has_orig {
                            accounted_target_files.insert(cand_orig.clone());
                        }

                        if cand_orig != cand_mp4
                            && has_orig
                            && !claimed_target_files.contains_key(&cand_orig)
                        {
                            claimed_target_files.insert(cand_orig, sfile.clone());
                        } else if has_mp4 && !claimed_target_files.contains_key(&cand_mp4) {
                            claimed_target_files.insert(cand_mp4, sfile.clone());
                        } else if has_orig && !claimed_target_files.contains_key(&cand_orig) {
                            claimed_target_files.insert(cand_orig, sfile.clone());
                        } else {
                            let mut resolved_by_shift = false;
                            if has_mp4 {
                                if let Some(prev_src) = claimed_target_files.get(&cand_mp4).cloned() {
                                    let (_, prev_orig) = expected_target_candidates_for_source(
                                        &prev_src,
                                        &source_dir,
                                        &target_dir,
                                        config,
                                    );
                                    if prev_orig != cand_mp4
                                        && target_set.contains(&prev_orig)
                                        && !claimed_target_files.contains_key(&prev_orig)
                                    {
                                        claimed_target_files.insert(prev_orig, prev_src);
                                        claimed_target_files.insert(cand_mp4.clone(), sfile.clone());
                                        resolved_by_shift = true;
                                    }
                                }
                            }
                            if !resolved_by_shift {
                                let shared_tgt = if has_mp4 { cand_mp4 } else { cand_orig };
                                let first_src = claimed_target_files
                                    .get(&shared_tgt)
                                    .cloned()
                                    .unwrap_or_else(|| sfile.clone());
                                colliding_source_files.push((sfile.clone(), first_src, shared_tgt));
                            }
                        }
                    }
                }

                let extra_target_files: Vec<String> = target_files
                    .iter()
                    .filter(|p| !accounted_target_files.contains(*p))
                    .map(|p| p.display().to_string())
                    .collect();

                if total_target_media > total_source_media
                    && missing_source_files.is_empty()
                    && colliding_source_files.is_empty()
                {
                    log_event(
                        LogLevel::Warn,
                        format!(
                            "[{}] [Step 2.6] All {} source media file(s) from '{}' are accounted for in '{}', with {} extra target file(s): [{}]",
                            worker_key,
                            all_source_files.len(),
                            source_dir.display(),
                            target_dir.display(),
                            extra_target_files.len(),
                            extra_target_files.join(", ")
                        ),
                    );

                    // 4. Delete the source subdir since all source files are accounted for, and log as WARN
                    if let Err(e) = fs::remove_dir_all(&source_dir).with_context(|| {
                        format!(
                            "Failed to delete completed subdir {:?}\nStack Trace:\n{}",
                            source_dir,
                            Backtrace::force_capture()
                        )
                    }) {
                        record_failed_dir(
                            &failed_dirs_log,
                            &source_dir,
                            Some(&target_dir),
                            &format!("ERROR deleting completed subdir '{}'", source_dir.display()),
                            &e,
                        );
                    } else {
                        global_metrics().record_reconciliation(
                            &item.mount_point,
                            "deleted_with_extra_target",
                            deleted_bad_count as u64,
                        );
                        log_structured_event(
                            LogLevel::Warn,
                            &local_nodename,
                            local_host_os,
                            worker_key,
                            "flusher",
                            &item.mount_point,
                            Some(item.item_id),
                            "subdir_reconciled",
                            &[
                                ("source_dir", source_dir.display().to_string()),
                                ("target_dir", target_dir.display().to_string()),
                                ("source_files", all_source_files.len().to_string()),
                                ("target_files", total_target_media.to_string()),
                                ("bad_target_deleted", deleted_bad_count.to_string()),
                                (
                                    "extra_target_files",
                                    extra_target_files.len().to_string(),
                                ),
                                ("outcome", "deleted_with_extra_target".to_string()),
                            ],
                        );
                    }
                } else {
                    global_metrics().record_reconciliation(
                        &item.mount_point,
                        "mismatch_retained",
                        deleted_bad_count as u64,
                    );
                    let mut missing_list: Vec<String> = missing_source_files
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect();
                    for (sfile, first_src, shared_tgt) in &colliding_source_files {
                        missing_list.push(format!(
                            "{} (collides with '{}' on '{}')",
                            sfile.display(),
                            first_src.display(),
                            shared_tgt.display()
                        ));
                    }

                    log_structured_event(
                        LogLevel::Error,
                        &local_nodename,
                        local_host_os,
                        worker_key,
                        "flusher",
                        &item.mount_point,
                        Some(item.item_id),
                        "subdir_reconciled",
                        &[
                            ("source_dir", source_dir.display().to_string()),
                            ("target_dir", target_dir.display().to_string()),
                            ("source_files", total_source_media.to_string()),
                            ("target_files", total_target_media.to_string()),
                            ("bad_target_deleted", deleted_bad_count.to_string()),
                            ("missing_count", missing_list.len().to_string()),
                            ("outcome", "mismatch_retained".to_string()),
                        ],
                    );

                    if !missing_list.is_empty() {
                        let err = anyhow::anyhow!(
                            "Media file count mismatch (source={} vs target={}) and {} source file(s) missing in target: [{}]",
                            total_source_media,
                            total_target_media,
                            missing_list.len(),
                            missing_list.join(", ")
                        );
                        record_failed_dir(
                            &failed_dirs_log,
                            &source_dir,
                            Some(&target_dir),
                            "Subdir media count mismatch with missing source files; keeping source subdir intact",
                            &err,
                        );
                    } else if !extra_target_files.is_empty() {
                        let err = anyhow::anyhow!(
                            "Media file count mismatch (source={} vs target={}) and {} extra target file(s) missing in source: [{}]",
                            total_source_media,
                            total_target_media,
                            extra_target_files.len(),
                            extra_target_files.join(", ")
                        );
                        record_failed_dir(
                            &failed_dirs_log,
                            &source_dir,
                            Some(&target_dir),
                            "Subdir media count mismatch; keeping source subdir intact",
                            &err,
                        );
                    } else {
                        let err = anyhow::anyhow!(
                            "Media file count mismatch (source={} vs target={})",
                            total_source_media,
                            total_target_media
                        );
                        record_failed_dir(
                            &failed_dirs_log,
                            &source_dir,
                            Some(&target_dir),
                            "Subdir media count mismatch; keeping source subdir intact",
                            &err,
                        );
                    }
                }
            }
        }
    } else if item.is_root_in_place && !had_error && !was_moved && item.source_path.exists() {
        let cand_enc = item.dest_dir.join(build_h265_filename(&item.source_path, false, config));
        if cand_enc.exists() {
            record_completed_file_in_journal(&item.source_path, &cand_enc);
        }
        let _ = fs::remove_file(&item.source_path);
        log_event(
            LogLevel::Info,
            format!(
                "[Step 2.9] Deleted original root-dir source file '{}'",
                item.source_path.display()
            ),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn complete_verified_queued_item(
    item: &QueuedMediaItem,
    was_moved: bool,
    bytes_saved: u64,
    subdir_trackers: &Arc<Mutex<HashMap<PathBuf, SubdirTracker>>>,
    shared_ui: &Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    worker_key: &str,
    config: &AppConfig,
    verbose: bool,
) {
    complete_queued_item(
        item,
        was_moved,
        false,
        false,
        bytes_saved,
        subdir_trackers,
        shared_ui,
        worker_key,
        config,
        verbose,
    );
    if let Ok(mut map) = shared_ui.lock() {
        if let Some(w) = map.get_mut(worker_key) {
            if !was_moved && w.batch_converted_files > 0 {
                w.batch_converted_files -= 1;
            }
            w.batch_verified_files += 1;
            w.batch_completed_files = w.batch_converted_files
                + w.batch_verified_files
                + w.batch_moved_files
                + w.batch_error_files;
        }
    }
}

/// Two-Tier Resume Check:
/// - Tier 1: Fast O(1) lookup in the unified append-only `completed_files.journal` + size match on disk.
/// - Tier 2: If an `_h265` output file already exists on disk (even if the journal was deleted),
///   shows a live `ffprobe-chk` / `Verifying` row in the worker's TUI box and uses `ffprobe` to verify
///   a valid video stream and matching runtime (`|src_dur - dst_dur| <= 1.5s`).
pub fn try_resume_existing_h265_output(
    item: &QueuedMediaItem,
    config: &AppConfig,
    shared_ui: &Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    worker_key: &str,
    slot_id: usize,
) -> Option<(PathBuf, u64, bool)> {
    let cand_mp4 = item
        .dest_dir
        .join(build_h265_filename(&item.source_path, false, config));
    let cand_orig = item
        .dest_dir
        .join(build_h265_filename(&item.source_path, true, config));

    let existing_target = if cand_mp4.is_file() {
        cand_mp4
    } else if cand_orig.is_file() {
        cand_orig
    } else {
        return None;
    };

    let target_size = fs::metadata(&existing_target).map(|m| m.len()).unwrap_or(0);
    if target_size == 0 {
        let _ = fs::remove_file(&existing_target);
        return None;
    }

    let src_size = if item.file_size_bytes > 0 {
        item.file_size_bytes
    } else {
        fs::metadata(&item.source_path).map(|m| m.len()).unwrap_or(0)
    };
    let saved = src_size.saturating_sub(target_size);
    let was_moved = !item.source_path.exists();

    // Tier 1: Fast append-only journal verification
    if is_file_verified_in_journal(&item.source_path, &existing_target, target_size) {
        log_event(
            LogLevel::Info,
            format!(
                "[Resume Tier-1] Journal verified existing H.265 file '{}' for source '{}'; skipping re-encode.",
                existing_target.display(),
                item.source_path.display()
            ),
        );
        return Some((existing_target, saved, was_moved));
    }

    // Tier 2: Show live `ffprobe-chk` / `Verifying` row in the worker's TUI box during ffprobe check
    let target_fname = existing_target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("output_h265.mp4");
    let display_with_size = format_filename_with_size(target_fname, target_size);
    update_ui_slot(
        shared_ui,
        worker_key,
        slot_id,
        &display_with_size,
        "ffprobe-chk",
        0,
        0.0,
        0.0,
        100.0,
        "Verifying",
        "Verifying",
    );

    let src_probe = get_video_probe_info(&item.source_path);
    let dst_probe = get_video_probe_info(&existing_target);
    remove_ui_slot(shared_ui, worker_key, slot_id);

    let runtime_matches =
        verify_output_durations(&item.source_path, &src_probe, &dst_probe, config).is_ok();

    if runtime_matches {
        log_event(
            LogLevel::Info,
            format!(
                "[Resume Tier-2] ffprobe verified existing H.265 file '{}' matches source '{}' (runtime {:.2}s vs {:.2}s); recording in journal and skipping re-encode.",
                existing_target.display(),
                item.source_path.display(),
                dst_probe.duration_secs,
                src_probe.duration_secs
            ),
        );
        record_completed_file_in_journal(&item.source_path, &existing_target);
        return Some((existing_target, saved, was_moved));
    }

    log_event(
        LogLevel::Warn,
        format!(
            "[Resume Tier-2] Existing target file '{}' is incomplete or runtime-mismatched (target={:.2}s [{}] vs source={:.2}s); deleting partial file and re-encoding.",
            existing_target.display(),
            dst_probe.duration_secs,
            dst_probe.video_codec,
            src_probe.duration_secs
        ),
    );
    let _ = fs::remove_file(&existing_target);
    None
}

// --- Local FFmpeg Conversion Engine ---
#[allow(clippy::too_many_arguments)]
fn run_local_ffmpeg_with_progress(
    input_path: &Path,
    output_path: &Path,
    profile: &EncoderProfile,
    nice_value: i32,
    use_hwaccel: bool,
    copy_audio: bool,
    use_fallback_filter: bool,
    probe: &VideoProbeInfo,
    slot_id: usize,
    display_filename: &str,
    shared_ui: &Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    worker_key: &str,
    verbose: bool,
) -> Result<()> {
    if is_shutdown_requested() {
        anyhow::bail!("Aborted local ffmpeg launch due to graceful shutdown");
    }

    if let Some(parent) = output_path.parent() {
        ensure_dir_created(parent)?;
    }

    let mut cmd = Command::new("ffmpeg");
    if !profile.decode_args.is_empty() {
        cmd.args(&profile.decode_args);
    }
    if use_hwaccel && !profile.hwaccel_args.is_empty() {
        cmd.args(&profile.hwaccel_args);
    }
    cmd.arg("-i").arg(input_path);
    if use_fallback_filter && !profile.fallback_video_filter_args.is_empty() {
        cmd.args(&profile.fallback_video_filter_args);
    }
    cmd.args(&profile.encoder_args);
    if copy_audio {
        cmd.args(["-c:a", "copy"]);
    } else {
        cmd.args(&profile.audio_args);
    }
    if !profile.muxer_args.is_empty() {
        cmd.args(&profile.muxer_args);
    }
    cmd.args([
        "-y",
        "-loglevel",
        "error",
        "-progress",
        "pipe:1",
        "-nostats",
    ]);
    cmd.arg(output_path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    platform::configure_child_process_group(&mut cmd);

    log_event(
        LogLevel::Info,
        format!("[{}] Executing local FFmpeg: {:?}", worker_key, cmd),
    );

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "Failed to spawn ffmpeg for {:?}\nStack Trace:\n{}",
            input_path,
            Backtrace::force_capture()
        )
    })?;
    let child_pid = child.id();
    try_set_process_priority(child_pid, nice_value, verbose);

    let finished_flag = Arc::new(AtomicBool::new(false));
    let killer_handle = spawn_shutdown_killer(child_pid, Arc::clone(&finished_flag));

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stderr_handle = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut err_pipe) = stderr {
            use std::io::Read;
            let _ = err_pipe.read_to_string(&mut buf);
        }
        buf
    });

    let local_nodename = GLOBAL_APP_CONFIG
        .get()
        .map(|c| c.effective_local_nodename())
        .unwrap_or_else(gethostname_syscall);
    let local_host_os = local_host_os_label();

    if let Some(out_pipe) = stdout {
        let reader = BufReader::new(out_pipe);
        let mut current_time_secs = 0.0f64;
        let mut current_speed = String::from("1.0x");
        let mut last_emit = Instant::now();

        for line in reader.lines().map_while(|r| r.ok()) {
            if is_shutdown_requested() {
                let _ = child.kill();
                break;
            }
            if let Some((k, v)) = line.split_once('=') {
                let key = k.trim();
                let val = v.trim();
                match key {
                    "out_time_us" | "out_time_ms" => {
                        if let Ok(us) = val.parse::<f64>() {
                            if us > 0.0 {
                                current_time_secs = us / 1_000_000.0;
                            }
                        }
                    }
                    "out_time" => {
                        if let Some(secs) = parse_hms_to_secs(val) {
                            if secs > 0.0 {
                                current_time_secs = secs;
                            }
                        }
                    }
                    "speed" => {
                        if !val.is_empty() && val != "N/A" {
                            current_speed = val.to_string();
                            if let Ok(mult) = val.trim_end_matches('x').trim().parse::<f64>() {
                                global_metrics().update_media_active_speed(
                                    &local_nodename,
                                    local_host_os,
                                    worker_key,
                                    &profile.name,
                                    mult,
                                );
                            }
                        }
                    }
                    "progress" => {
                        let pct = if val == "end" {
                            100.0
                        } else if probe.duration_secs > 0.0 {
                            ((current_time_secs / probe.duration_secs) * 100.0).clamp(0.0, 99.9)
                        } else {
                            0.0
                        };

                        if val == "end" || last_emit.elapsed() >= Duration::from_millis(80) {
                            update_ui_slot(
                                shared_ui,
                                worker_key,
                                slot_id,
                                display_filename,
                                &probe.video_codec,
                                probe.bitrate_kbps,
                                current_time_secs,
                                probe.duration_secs,
                                pct,
                                &current_speed,
                                "Encoding",
                            );
                            last_emit = Instant::now();
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let status = child.wait()?;
    finished_flag.store(true, Ordering::Relaxed);
    let _ = killer_handle.join();
    let stderr_msg = stderr_handle.join().unwrap_or_default();

    if !status.success() {
        global_metrics().update_media_active_speed(
            &local_nodename,
            local_host_os,
            worker_key,
            &profile.name,
            0.0,
        );
        let _ = fs::remove_file(output_path);
        if is_shutdown_requested() {
            anyhow::bail!("Aborted local ffmpeg encode due to graceful shutdown");
        }
        anyhow::bail!(
            "ffmpeg failed on {:?} (status {}): {}\nStack Trace:\n{}",
            input_path,
            status,
            stderr_msg.trim(),
            Backtrace::force_capture()
        );
    }

    Ok(())
}

// --- Local Worker-Group RAMDisk Partitioning & 3-Thread Pipeline (Stager / Converter / Flusher) ---

/// Computes dynamic RAMDisk partition sizing for `current_mount`:
/// - `partition_size_bytes = total_ramdisk_bytes / active_worker_groups`
/// - `max_input_stage_size = partition_size_bytes / 2` (`0.5 * partition_size`)
/// - `future_max_input_stage_size = (total_ramdisk_bytes / (active_worker_groups - smaller_queue_groups)) / 2`
///   where `smaller_queue_groups` is the count of other active Worker-Groups whose remaining queue depth
///   is strictly less than `current_mount`'s remaining queue depth (`other_depth < my_depth`).
pub fn compute_worker_group_partition_limits(
    current_mount: &MountQueueState,
    all_mount_queues: &[Arc<MountQueueState>],
    config: &AppConfig,
) -> (u64, u64, u64) {
    let total_ramdisk_bytes =
        (config.RAMDISK_SIZE_GB.max(0.25) * 1024.0 * 1024.0 * 1024.0).round() as u64;
    let my_depth = current_mount.queue_depth();
    let mut active_groups = 0usize;
    let mut smaller_other_groups = 0usize;

    for mq in all_mount_queues {
        if !mq.local_worker_finished.load(Ordering::Relaxed) {
            active_groups += 1;
            if mq.mount_point != current_mount.mount_point && mq.queue_depth() < my_depth {
                smaller_other_groups += 1;
            }
        }
    }

    let active_groups = active_groups.max(1);
    let partition_size_bytes = total_ramdisk_bytes / (active_groups as u64);
    let max_input_stage_size = partition_size_bytes / 2;

    let future_active_groups = active_groups.saturating_sub(smaller_other_groups).max(1);
    let future_partition_size_bytes = total_ramdisk_bytes / (future_active_groups as u64);
    let future_max_input_stage_size = future_partition_size_bytes / 2;

    (
        partition_size_bytes,
        max_input_stage_size,
        future_max_input_stage_size,
    )
}

#[derive(Debug)]
pub enum StagerQueueOutcome {
    /// Selected an item that fits into `MAX_INPUT_STAGE_SIZE` and `available_input_space` right now.
    StageNow(QueuedMediaItem),
    /// Selected an item marked `force_unstaged` to be processed directly without RAMDisk staging.
    ProcessUnstaged(QueuedMediaItem),
    /// Queue is currently empty.
    QueueEmpty,
    /// Queue is non-empty, but no item could be staged on this pass (waiting for Flusher to free
    /// RAMDisk space, or waiting for smaller-queue Worker-Groups to finish and resize partitions).
    WaitForSpaceOrResize,
}

/// Inspects `queue` for the next file to stage (or process unstaged):
/// - Case A (`file_size <= max_input_stage_size` but `file_size > available_input_space`):
///   Rotates the item to the back of the queue and continues inspecting subsequent items in the queue
///   that may fit into `available_input_space` while waiting for `Flusher`.
/// - Case B1 (`file_size > max_input_stage_size` but `file_size <= future_max_input_stage_size` because
///   another Worker-Group has a strictly smaller queue depth):
///   Rotates the item to the back of the queue without marking `force_unstaged` so it can be staged
///   once the smaller-queue Worker-Group completes and partitions are resized.
/// - Case B2 (`file_size > future_max_input_stage_size` — permanently too large):
///   Marks `force_unstaged = true` and rotates the item to the back of the queue to be processed
///   without staging after stageable files complete.
pub fn select_next_item_for_stager(
    q: &mut VecDeque<QueuedMediaItem>,
    max_input_stage_size: u64,
    future_max_input_stage_size: u64,
    current_staged_input_bytes: u64,
) -> StagerQueueOutcome {
    let available_input_space = max_input_stage_size.saturating_sub(current_staged_input_bytes);
    let q_len = q.len();
    if q_len == 0 {
        return StagerQueueOutcome::QueueEmpty;
    }

    for _ in 0..q_len {
        let mut candidate = match q.pop_front() {
            Some(c) => c,
            None => break,
        };

        if candidate.force_unstaged {
            // Keep force_unstaged items at the back while scanning so any stageable files run first
            q.push_back(candidate);
            continue;
        }

        let file_size = candidate.file_size_bytes;

        if file_size <= max_input_stage_size {
            if file_size <= available_input_space {
                global_metrics().record_stager_decision(&candidate.mount_point, "stage_now");
                return StagerQueueOutcome::StageNow(candidate);
            } else {
                // Case A: Temporarily too large for remaining allocated input space; rotate to back
                global_metrics()
                    .record_stager_decision(&candidate.mount_point, "case_a_wait_space");
                q.push_back(candidate);
            }
        } else if file_size <= future_max_input_stage_size {
            // Case B1: Will fit after smaller-queue Worker-Group(s) finish and partitions resize; rotate to back
            global_metrics().record_stager_decision(&candidate.mount_point, "case_b1_wait_resize");
            q.push_back(candidate);
        } else {
            // Case B2: Permanently too large even after partition expansion; mark force_unstaged and move to back
            global_metrics()
                .record_stager_decision(&candidate.mount_point, "case_b2_force_unstaged");
            log_event(
                LogLevel::Info,
                format!(
                    "[Stager {}] File '{}' ({}) exceeds max stageable input size (current={}, future={}); moving to back of queue for unstaged processing.",
                    candidate.mount_point,
                    candidate.source_path.display(),
                    format_bytes(file_size),
                    format_bytes(max_input_stage_size),
                    format_bytes(future_max_input_stage_size)
                ),
            );
            candidate.force_unstaged = true;
            q.push_back(candidate);
        }
    }

    // If no stageable file could be selected on this pass, and the front item is marked `force_unstaged`,
    // select it for unstaged processing now.
    if q.front().map(|i| i.force_unstaged).unwrap_or(false) {
        if let Some(item) = q.pop_front() {
            global_metrics().record_stager_decision(&item.mount_point, "process_unstaged");
            return StagerQueueOutcome::ProcessUnstaged(item);
        }
    }

    StagerQueueOutcome::WaitForSpaceOrResize
}

pub struct LocalPipelineTask {
    pub item: QueuedMediaItem,
    pub probe: VideoProbeInfo,
    pub staged_input: Option<PathBuf>,
    pub staged_input_size: u64,
    pub staged_output: Option<PathBuf>,
    pub staged_output_size: u64,
    pub final_encoded_dest: PathBuf,
    pub kept_original: bool,
    pub saved_bytes: u64,
    pub conversion_error: Option<anyhow::Error>,
}

pub struct LocalWorkerGroupState {
    pub to_convert: Mutex<VecDeque<LocalPipelineTask>>,
    pub to_flush: Mutex<VecDeque<LocalPipelineTask>>,
    pub staged_input_bytes: AtomicU64,
    pub staged_output_bytes: AtomicU64,
    pub stager_done: AtomicBool,
    pub converter_done: AtomicBool,
}

// --- Queue Building (Ordered: {max_subdir} -> {max_subdir-1} -> ... -> root-dir) ---
fn list_eligible_subdirs(root_dir: &Path, suffix: &str) -> Vec<(PathBuf, u64)> {
    let mut subdirs = Vec::new();
    if let Ok(entries) = fs::read_dir(root_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                if name.starts_with('.') {
                    continue;
                }
                if !is_subdir_h265_named(&path, suffix) {
                    let size = compute_directory_size_bytes(&path);
                    subdirs.push((path, size));
                }
            }
        }
    }
    subdirs
}

#[allow(clippy::too_many_arguments)]
fn copy_file_with_live_tui_progress(
    src: &Path,
    dst: &Path,
    expected_bytes: u64,
    shared_ui: &Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    worker_key: &str,
    slot_id: usize,
    display_filename: &str,
    codec_label: &str,
    bitrate_kbps: u64,
    direction_label: &str,
    status_label: &str,
    abort_on_shutdown: bool,
) -> Result<u64> {
    use std::io::Read;
    let total_bytes = if expected_bytes > 0 {
        expected_bytes
    } else {
        fs::metadata(src).map(|m| m.len()).unwrap_or(0)
    };
    let mount_label = worker_key.strip_prefix("local:").unwrap_or(worker_key);
    let is_write_to_hdd = direction_label.contains("->HDD");
    let mut reader = File::open(src)?;
    let mut writer = File::create(dst)?;
    let mut buf = vec![0u8; 4 * 1024 * 1024]; // 4 MB buffer for fast sequential HDD/RAMDisk I/O
    let mut copied: u64 = 0;
    let start_instant = Instant::now();
    let mut last_emit = Instant::now();

    loop {
        if abort_on_shutdown && is_shutdown_requested() {
            drop(writer);
            let _ = fs::remove_file(dst);
            global_metrics().clear_disk_live_throughput(mount_label, is_write_to_hdd);
            anyhow::bail!("Aborted copy {:?} -> {:?} due to graceful shutdown", src, dst);
        }
        let chunk_start = Instant::now();
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        let chunk_dur = chunk_start.elapsed().as_secs_f64();
        copied += n as u64;
        let elapsed = start_instant.elapsed().as_secs_f64().max(0.005);
        let mb_per_s = (copied as f64 / (1024.0 * 1024.0)) / elapsed;
        global_metrics().record_disk_chunk_io(
            mount_label,
            "Disk",
            is_write_to_hdd,
            n as u64,
            chunk_dur,
            mb_per_s,
        );

        if last_emit.elapsed() >= Duration::from_millis(80) || copied == total_bytes {
            let pct = if total_bytes > 0 {
                ((copied as f64 / total_bytes as f64) * 100.0).clamp(0.0, 100.0)
            } else {
                100.0
            };
            let total_secs = if pct > 1.0 {
                elapsed / (pct / 100.0)
            } else {
                0.0
            };
            let speed_display = format!("{} {:.0}M/s", direction_label, mb_per_s);
            update_ui_slot(
                shared_ui,
                worker_key,
                slot_id,
                display_filename,
                codec_label,
                bitrate_kbps,
                elapsed,
                total_secs,
                pct,
                &speed_display,
                status_label,
            );
            last_emit = Instant::now();
        }
    }
    writer.flush()?;
    global_metrics().clear_disk_live_throughput(mount_label, is_write_to_hdd);
    Ok(copied)
}

fn collect_video_files_recursive(dir: &Path, config: &AppConfig) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walker = WalkDir::new(dir).into_iter().filter_entry(|e| {
        if e.depth() > 0 && e.file_type().is_dir() {
            !is_subdir_h265_named(e.path(), &config.OUTPUT_SUFFIX)
        } else {
            true
        }
    });
    for entry in walker.filter_map(|e| e.ok()) {
        let p = entry.path();
        if is_valid_video_file(p, config)
            && !relative_path_has_h265_dir_component(dir, p, &config.OUTPUT_SUFFIX)
        {
            files.push(p.to_path_buf());
        }
    }
    if config.SORT_BY_SIZE_DESC {
        files.sort_by_key(|p| std::cmp::Reverse(fs::metadata(p).map(|m| m.len()).unwrap_or(0)));
    } else {
        files.sort();
    }
    files
}

fn collect_top_level_video_files(root_dir: &Path, config: &AppConfig) -> Vec<PathBuf> {
    if is_subdir_h265_named(root_dir, &config.OUTPUT_SUFFIX) {
        return Vec::new();
    }
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(root_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let p = entry.path();
            if is_valid_video_file(&p, config) && !is_h265_named(&p, &config.OUTPUT_SUFFIX) {
                files.push(p);
            }
        }
    }
    if config.SORT_BY_SIZE_DESC {
        files.sort_by_key(|p| std::cmp::Reverse(fs::metadata(p).map(|m| m.len()).unwrap_or(0)));
    } else {
        files.sort();
    }
    files
}

fn populate_mount_queue_for_dirs(
    mount_point: &str,
    dirs: &[PathBuf],
    config: &AppConfig,
    next_item_id: &AtomicU64,
    subdir_trackers: &Arc<Mutex<HashMap<PathBuf, SubdirTracker>>>,
    verbose: bool,
) -> VecDeque<QueuedMediaItem> {
    let mut queue = VecDeque::new();
    let failed_dirs_log = PathBuf::from(&config.FAILED_DIRS_PATH);

    for root_dir in dirs {
        if !root_dir.exists() || !root_dir.is_dir() {
            let err = anyhow::anyhow!("Root directory does not exist: {}", root_dir.display());
            record_failed_dir(
                &failed_dirs_log,
                root_dir,
                None,
                &format!("Skipping invalid root directory '{}'", root_dir.display()),
                &err,
            );
            continue;
        }
        if is_subdir_h265_named(root_dir, &config.OUTPUT_SUFFIX) {
            vlog!(
                verbose,
                "[Queue] Ignoring root directory '{}' because it ends with '{}'",
                root_dir.display(),
                config.OUTPUT_SUFFIX
            );
            continue;
        }
        let free_space = get_available_free_space_bytes(root_dir);

        let mut subdirs: Vec<(PathBuf, u64)> = Vec::new();
        for (subdir_path, sz) in list_eligible_subdirs(root_dir, &config.OUTPUT_SUFFIX) {
            if sz <= free_space {
                subdirs.push((subdir_path, sz));
            } else {
                let raw_name = subdir_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("subdir");
                let clean_name = sanitize_dir_or_file_component(raw_name);
                let target_subdir_name = format!("{}{}", clean_name, config.OUTPUT_SUFFIX);
                let target_subdir = subdir_path
                    .parent()
                    .unwrap_or(root_dir)
                    .join(&target_subdir_name);
                let err = anyhow::anyhow!(
                    "Subdirectory size ({}) exceeds available free space ({}) on mount '{}'",
                    format_bytes(sz),
                    format_bytes(free_space),
                    mount_point
                );
                mark_dir_unwritable(&target_subdir);
                record_failed_dir(
                    &failed_dirs_log,
                    &subdir_path,
                    Some(&target_subdir),
                    &format!(
                        "Subdir '{}' exceeds available free space; logging to failed_dirs.txt and skipping",
                        subdir_path.display()
                    ),
                    &err,
                );
            }
        }
        subdirs.sort_by_key(|(_, sz)| std::cmp::Reverse(*sz));

        for (subdir_path, subdir_size) in subdirs {
            let raw_name = subdir_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("subdir");
            let clean_name = sanitize_dir_or_file_component(raw_name);
            let target_subdir_name = format!("{}{}", clean_name, config.OUTPUT_SUFFIX);
            let target_subdir = subdir_path
                .parent()
                .unwrap_or(root_dir)
                .join(&target_subdir_name);

            let video_files = collect_video_files_recursive(&subdir_path, config);

            if video_files.is_empty() {
                continue;
            }

            if let Ok(mut trackers) = subdir_trackers.lock() {
                trackers.insert(
                    subdir_path.clone(),
                    SubdirTracker {
                        mount_point: mount_point.to_string(),
                        source_subdir: subdir_path.clone(),
                        target_subdir: target_subdir.clone(),
                        source_files: video_files.clone(),
                        total_files: video_files.len(),
                        completed_files: 0,
                        moved_files: 0,
                        failed_files: 0,
                        succeeded_files: Vec::new(),
                        failed_source_files: HashSet::new(),
                        deleted: false,
                    },
                );
            }

            let target_label = format!("{} -> {}", clean_name, target_subdir_name);
            for vf in video_files {
                let rel = vf.strip_prefix(&subdir_path).unwrap_or(&vf);
                let rel_parent = rel.parent().unwrap_or_else(|| Path::new(""));
                let mut dest_dir = target_subdir.clone();
                for comp in rel_parent.components() {
                    if let std::path::Component::Normal(os_str) = comp {
                        if let Some(s) = os_str.to_str() {
                            let cleaned = sanitize_dir_or_file_component(s);
                            if !cleaned.is_empty() {
                                dest_dir.push(cleaned);
                            }
                        } else {
                            dest_dir.push(os_str);
                        }
                    }
                }
                let sz = fs::metadata(&vf).map(|m| m.len()).unwrap_or(0);
                queue.push_back(QueuedMediaItem {
                    item_id: next_item_id.fetch_add(1, Ordering::Relaxed),
                    mount_point: mount_point.to_string(),
                    root_dir: root_dir.clone(),
                    source_subdir: Some(subdir_path.clone()),
                    target_subdir: Some(target_subdir.clone()),
                    source_path: vf,
                    dest_dir,
                    file_size_bytes: sz,
                    target_label: target_label.clone(),
                    target_size_bytes: subdir_size,
                    is_root_in_place: false,
                    cached_probe: None,
                    force_unstaged: false,
                });
            }
        }

        let top_files = collect_top_level_video_files(root_dir, config);
        for vf in top_files {
            let sz = fs::metadata(&vf).map(|m| m.len()).unwrap_or(0);
            queue.push_back(QueuedMediaItem {
                item_id: next_item_id.fetch_add(1, Ordering::Relaxed),
                mount_point: mount_point.to_string(),
                root_dir: root_dir.clone(),
                source_subdir: None,
                target_subdir: None,
                source_path: vf,
                dest_dir: root_dir.clone(),
                file_size_bytes: sz,
                target_label: "Top-level root-dir files (in-place)".to_string(),
                target_size_bytes: 0,
                is_root_in_place: true,
                cached_probe: None,
                force_unstaged: false,
            });
        }
    }

    vlog!(
        verbose,
        "[Orchestrator] Populated queue for mount '{}' with {} ordered media file(s).",
        mount_point,
        queue.len()
    );
    queue
}

// --- Per-Mount Integrated Verifier (Slot 0 inside each Local Mount Box, Bounded Lookahead Buffer) ---
fn run_mount_verifier_loop(
    ms: Arc<MountQueueState>,
    trackers: Arc<Mutex<HashMap<PathBuf, SubdirTracker>>>,
    ui: Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    cfg: AppConfig,
    wkey: String,
    verbose: bool,
) {
    let fdirs_log = PathBuf::from(&cfg.FAILED_DIRS_PATH);
    let local_nodename = cfg.effective_local_nodename();
    let local_host_os = local_host_os_label().to_string();
    let mut wait_tracker = ThreadWaitTracker::new(
        &local_nodename,
        &local_host_os,
        &wkey,
        "verifier",
        &ms.mount_point,
    );
    let lookahead_limit: usize = match ms.info.drive_type {
        DriveType::Hdd => 30,
        DriveType::Ssd => 50,
        DriveType::Nvme => 60,
    };

    loop {
        if is_shutdown_requested() {
            remove_ui_slot(&ui, &wkey, 0);
            ms.verifier_done.store(true, Ordering::Relaxed);
            break;
        }

        // Bounded lookahead: pause verifying if `queue` (verified ready-to-encode buffer) is full
        let ready_len = ms.queue.lock().unwrap().len();
        if ready_len >= lookahead_limit {
            remove_ui_slot(&ui, &wkey, 0);
            let (total_rem, r_len) = ms.total_remaining_items();
            if let Ok(mut map) = ui.lock() {
                if let Some(w) = map.get_mut(&wkey) {
                    w.queued_items_remaining = total_rem;
                    w.ready_to_encode_count = r_len;
                }
            }
            wait_tracker.sleep_with_reason(
                WaitReason::LookaheadBufferFull,
                Duration::from_millis(120),
            );
            continue;
        }

        let next_raw = ms.raw_queue.lock().unwrap().pop_front();
        let mut item = match next_raw {
            Some(it) => it,
            None => {
                remove_ui_slot(&ui, &wkey, 0);
                ms.verifier_done.store(true, Ordering::Relaxed);
                let (total_rem, r_len) = ms.total_remaining_items();
                if let Ok(mut map) = ui.lock() {
                    if let Some(w) = map.get_mut(&wkey) {
                        w.queued_items_remaining = total_rem;
                        w.ready_to_encode_count = r_len;
                    }
                }
                break;
            }
        };

        let mut wait_breakdown = wait_tracker.begin_active_task("verify_file");
        let verify_start_instant = Instant::now();
        let src_mb = (item.file_size_bytes as f64) / (1024.0 * 1024.0);
        log_structured_event(
            LogLevel::Info,
            &local_nodename,
            &local_host_os,
            &wkey,
            "verifier",
            &ms.mount_point,
            Some(item.item_id),
            "verify_file_start",
            &[
                ("source_file", item.source_path.display().to_string()),
                ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                (
                    "thread_wait_in_secs",
                    format!("{:.3}", wait_breakdown.thread_wait_in_secs),
                ),
                (
                    "lookahead_wait_in_secs",
                    format!("{:.3}", wait_breakdown.lookahead_wait_in_secs),
                ),
                ("wait_cycles", wait_breakdown.wait_cycles.to_string()),
            ],
        );

        struct VerifierInFlightGuard<'a> {
            ms: &'a MountQueueState,
            item_id: u64,
        }
        impl<'a> Drop for VerifierInFlightGuard<'a> {
            fn drop(&mut self) {
                self.ms.clear_in_flight(self.item_id);
            }
        }
        ms.set_in_flight(
            &item,
            QueueEntryCategory::ActiveLocal,
            "LOCAL [Verifier: Probing]",
        );
        let _verifier_guard = VerifierInFlightGuard {
            ms: &ms,
            item_id: item.item_id,
        };

        let (total_rem, r_len) = ms.total_remaining_items();
        if let Ok(mut map) = ui.lock() {
            if let Some(w) = map.get_mut(&wkey) {
                w.queued_items_remaining = total_rem;
                w.ready_to_encode_count = r_len;
                if w.current_root_dir.is_empty() {
                    w.current_root_dir = item.root_dir.display().to_string();
                    w.current_target_label = item.target_label.clone();
                    w.current_target_size_bytes = item.target_size_bytes;
                    w.free_space_bytes = get_available_free_space_bytes(&item.root_dir);
                }
                if w.phase == "Ready" {
                    w.phase = "Verifying & converting media queue".to_string();
                }
            }
        }

        if let Some(ref tdir) = item.target_subdir {
            if is_dir_marked_failed(tdir) {
                wait_tracker.complete_active_task(
                    "verify_file",
                    "failed_dir_skipped",
                    verify_start_instant.elapsed().as_secs_f64(),
                );
                complete_queued_item(
                    &item,
                    false,
                    false,
                    true,
                    0,
                    &trackers,
                    &ui,
                    &wkey,
                    &cfg,
                    verbose,
                );
                continue;
            }
            if let Err(dir_err) = ensure_dir_created(tdir) {
                global_metrics().record_disk_error(&ms.mount_point, "mkdir");
                wait_tracker.record_error("mkdir_failed");
                wait_tracker.complete_active_task(
                    "verify_file",
                    "mkdir_failed",
                    verify_start_instant.elapsed().as_secs_f64(),
                );
                let src_d = item.source_subdir.as_deref().unwrap_or(&item.root_dir);
                mark_dir_unwritable(tdir);
                record_failed_dir(
                    &fdirs_log,
                    src_d,
                    Some(tdir),
                    &format!(
                        "[Verifier {}] Failed creating target directory '{}'; logging to '{}' and skipping subdir",
                        ms.mount_point,
                        tdir.display(),
                        fdirs_log.display()
                    ),
                    &dir_err,
                );
                complete_queued_item(
                    &item,
                    false,
                    false,
                    true,
                    0,
                    &trackers,
                    &ui,
                    &wkey,
                    &cfg,
                    verbose,
                );
                continue;
            }
        }

        // 0. Fast Broken-Files Journal Check: skip files known to be corrupt from current or prior sessions
        if is_file_marked_broken(&item.source_path) {
            let v_dur = verify_start_instant.elapsed().as_secs_f64();
            wait_tracker.complete_active_task("verify_file", "corrupt_broken_journal", v_dur);
            log_structured_event(
                LogLevel::Warn,
                &local_nodename,
                &local_host_os,
                &wkey,
                "verifier",
                &ms.mount_point,
                Some(item.item_id),
                "verify_file_complete",
                &[
                    ("source_file", item.source_path.display().to_string()),
                    ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                    ("verify_duration_in_secs", format!("{:.3}", v_dur)),
                    ("verify_outcome", "corrupt_broken_journal".to_string()),
                ],
            );
            complete_queued_item(
                &item,
                false,
                false,
                true,
                0,
                &trackers,
                &ui,
                &wkey,
                &cfg,
                verbose,
            );
            continue;
        }

        // 1. Two-Tier Resume Check (Fast Journal + ffprobe Fallback in Slot 0)
        if let Some((_verified_dst, saved_bytes, was_moved)) =
            try_resume_existing_h265_output(&item, &cfg, &ui, &wkey, 0)
        {
            let v_dur = verify_start_instant.elapsed().as_secs_f64();
            global_metrics().record_resume_check(&ms.mount_point, "tier1_or_tier2_hit");
            wait_tracker.complete_active_task("verify_file", "skipped_existing_h265", v_dur);
            log_structured_event(
                LogLevel::Info,
                &local_nodename,
                &local_host_os,
                &wkey,
                "verifier",
                &ms.mount_point,
                Some(item.item_id),
                "verify_file_complete",
                &[
                    ("source_file", item.source_path.display().to_string()),
                    ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                    ("verify_duration_in_secs", format!("{:.3}", v_dur)),
                    ("verify_outcome", "skipped_existing_h265".to_string()),
                    (
                        "storage_saved_in_mb",
                        format!("{:.2}", saved_bytes as f64 / (1024.0 * 1024.0)),
                    ),
                ],
            );
            complete_verified_queued_item(
                &item,
                was_moved,
                saved_bytes,
                &trackers,
                &ui,
                &wkey,
                &cfg,
                verbose,
            );
            continue;
        }
        global_metrics().record_resume_check(&ms.mount_point, "miss");

        if is_shutdown_requested() {
            remove_ui_slot(&ui, &wkey, 0);
            ms.verifier_done.store(true, Ordering::Relaxed);
            break;
        }

        // 2. Stage A: Probe source video & validate container/stream headers via ffprobe
        let src_fname = item
            .source_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("video");
        let display_src = format_filename_with_size(src_fname, item.file_size_bytes);
        update_ui_slot(
            &ui,
            &wkey,
            0,
            &display_src,
            "ffprobe-src",
            0,
            0.0,
            0.0,
            100.0,
            "Probing",
            "Probing",
        );
        let probe = get_video_probe_info(&item.source_path);
        remove_ui_slot(&ui, &wkey, 0);

        if is_shutdown_requested() {
            ms.verifier_done.store(true, Ordering::Relaxed);
            break;
        }

        if probe.video_codec == "unknown" || probe.duration_secs <= 0.0 || probe.probe_error.is_some()
        {
            let reason = probe.probe_error.clone().unwrap_or_else(|| {
                format!(
                    "Invalid video stream (codec={}, duration={:.2}s)",
                    probe.video_codec, probe.duration_secs
                )
            });
            let v_dur = verify_start_instant.elapsed().as_secs_f64();
            wait_tracker.record_error("ffprobe_corrupt");
            wait_tracker.complete_active_task("verify_file", "corrupt_ffprobe", v_dur);
            log_structured_event(
                LogLevel::Error,
                &local_nodename,
                &local_host_os,
                &wkey,
                "verifier",
                &ms.mount_point,
                Some(item.item_id),
                "verify_file_complete",
                &[
                    ("source_file", item.source_path.display().to_string()),
                    ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                    ("verify_duration_in_secs", format!("{:.3}", v_dur)),
                    ("verify_outcome", "corrupt_ffprobe".to_string()),
                    ("error_type", "ffprobe_corrupt".to_string()),
                    ("reason", reason.clone()),
                ],
            );
            record_broken_file_in_journal(&item.source_path, item.file_size_bytes, &reason);
            let err = anyhow::anyhow!("Corrupt/unreadable media file (ffprobe): {}", reason);
            let src_d = item
                .source_path
                .parent()
                .or(item.source_subdir.as_deref())
                .unwrap_or(&item.root_dir);
            record_file_failure_if_untracked_subdir(
                &fdirs_log,
                &item,
                src_d,
                Some(item.dest_dir.as_path()),
                &format!(
                    "[Verifier {}] Marking broken file '{}' in broken_files.journal",
                    ms.mount_point,
                    item.source_path.display()
                ),
                &err,
            );
            complete_queued_item(
                &item,
                false,
                false,
                true,
                0,
                &trackers,
                &ui,
                &wkey,
                &cfg,
                verbose,
            );
            continue;
        }

        // 2b. Stage B: Fast 1-frame FFmpeg decode smoke test (`ffmpeg -frames:v 1 -f null -`)
        update_ui_slot(
            &ui,
            &wkey,
            0,
            &display_src,
            &probe.video_codec,
            probe.bitrate_kbps,
            0.0,
            probe.duration_secs,
            100.0,
            "1f-Decode",
            "Validating",
        );
        let decode_check = verify_first_frame_decodable(&item.source_path);
        remove_ui_slot(&ui, &wkey, 0);

        if is_shutdown_requested() {
            ms.verifier_done.store(true, Ordering::Relaxed);
            break;
        }

        if let Err(decode_err) = decode_check {
            let v_dur = verify_start_instant.elapsed().as_secs_f64();
            wait_tracker.record_error("decode_1f_failed");
            wait_tracker.complete_active_task("verify_file", "corrupt_1f_decode", v_dur);
            log_structured_event(
                LogLevel::Error,
                &local_nodename,
                &local_host_os,
                &wkey,
                "verifier",
                &ms.mount_point,
                Some(item.item_id),
                "verify_file_complete",
                &[
                    ("source_file", item.source_path.display().to_string()),
                    ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                    ("verify_duration_in_secs", format!("{:.3}", v_dur)),
                    ("verify_outcome", "corrupt_1f_decode".to_string()),
                    ("error_type", "decode_1f_failed".to_string()),
                    ("reason", decode_err.clone()),
                ],
            );
            record_broken_file_in_journal(&item.source_path, item.file_size_bytes, &decode_err);
            let err = anyhow::anyhow!("Corrupt media bitstream (1-frame decode failed): {}", decode_err);
            let src_d = item
                .source_path
                .parent()
                .or(item.source_subdir.as_deref())
                .unwrap_or(&item.root_dir);
            record_file_failure_if_untracked_subdir(
                &fdirs_log,
                &item,
                src_d,
                Some(item.dest_dir.as_path()),
                &format!(
                    "[Verifier {}] Marking broken file '{}' in broken_files.journal",
                    ms.mount_point,
                    item.source_path.display()
                ),
                &err,
            );
            complete_queued_item(
                &item,
                false,
                false,
                true,
                0,
                &trackers,
                &ui,
                &wkey,
                &cfg,
                verbose,
            );
            continue;
        }

        let validated_target = codecs::validate_config_targets(&cfg).ok();
        let is_already_target = match validated_target {
            Some(t) => codecs::is_source_already_target(&probe, &item.source_path, t.video, t.container),
            None => probe.video_codec == "hevc" || probe.video_codec == "h265",
        };
        let is_low_bitrate = !is_already_target
            && !validated_target
                .map(|t| t.video.is_animated_image())
                .unwrap_or(false)
            && cfg.MIN_BITRATE_KBPS > 0
            && probe.bitrate_kbps > 0
            && probe.bitrate_kbps < cfg.MIN_BITRATE_KBPS;

        if is_already_target || is_low_bitrate {
            let out_filename = build_h265_filename(&item.source_path, true, &cfg);
            let dest_file = item.dest_dir.join(out_filename);
            update_ui_slot(
                &ui,
                &wkey,
                0,
                &display_src,
                &probe.video_codec,
                probe.bitrate_kbps,
                0.0,
                probe.duration_secs,
                100.0,
                "Moving",
                "Moving",
            );
            global_metrics().inc_io_lock_waiting(&ms.mount_point, ms.info.drive_type.as_str());
            let lock_wait_start = Instant::now();
            let move_res = {
                let _io_guard = ms.io_lock.lock().unwrap();
                let lock_wait_secs = lock_wait_start.elapsed().as_secs_f64();
                wait_breakdown.add_io_lock_wait(lock_wait_secs);
                global_metrics().record_io_lock_acquired(
                    &ms.mount_point,
                    ms.info.drive_type.as_str(),
                    "verifier",
                    lock_wait_secs,
                );
                let hold_start = Instant::now();
                let r = move_file_safe(&item.source_path, &dest_file);
                global_metrics().record_io_lock_released(
                    &ms.mount_point,
                    "verifier",
                    hold_start.elapsed().as_secs_f64(),
                );
                r
            };
            remove_ui_slot(&ui, &wkey, 0);

            match move_res {
                Ok(_) => {
                    let v_dur = verify_start_instant.elapsed().as_secs_f64();
                    let outcome_str = if is_already_target {
                        "moved_already_target_codec"
                    } else {
                        "moved_low_bitrate"
                    };
                    wait_tracker.complete_active_task("verify_file", outcome_str, v_dur);
                    log_structured_event(
                        LogLevel::Info,
                        &local_nodename,
                        &local_host_os,
                        &wkey,
                        "verifier",
                        &ms.mount_point,
                        Some(item.item_id),
                        "verify_file_complete",
                        &[
                            ("source_file", item.source_path.display().to_string()),
                            ("target_file", dest_file.display().to_string()),
                            ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                            ("source_codec", probe.video_codec.clone()),
                            ("source_bitrate_kbps", probe.bitrate_kbps.to_string()),
                            (
                                "thread_wait_in_secs",
                                format!("{:.3}", wait_breakdown.thread_wait_in_secs),
                            ),
                            (
                                "io_lock_wait_in_secs",
                                format!("{:.3}", wait_breakdown.io_lock_wait_in_secs),
                            ),
                            ("verify_duration_in_secs", format!("{:.3}", v_dur)),
                            ("verify_outcome", outcome_str.to_string()),
                        ],
                    );
                    complete_queued_item(
                        &item,
                        true,
                        is_low_bitrate,
                        false,
                        0,
                        &trackers,
                        &ui,
                        &wkey,
                        &cfg,
                        verbose,
                    );
                }
                Err(e) => {
                    let v_dur = verify_start_instant.elapsed().as_secs_f64();
                    global_metrics().record_disk_error(&ms.mount_point, "move");
                    wait_tracker.record_error("move_failed");
                    wait_tracker.complete_active_task("verify_file", "move_failed", v_dur);
                    let src_d = item
                        .source_path
                        .parent()
                        .or(item.source_subdir.as_deref())
                        .unwrap_or(&item.root_dir);
                    record_file_failure_if_untracked_subdir(
                        &fdirs_log,
                        &item,
                        src_d,
                        Some(item.dest_dir.as_path()),
                        &format!(
                            "[Verifier {}] ERROR moving '{}'",
                            ms.mount_point,
                            item.source_path.display()
                        ),
                        &e,
                    );
                    complete_queued_item(
                        &item,
                        false,
                        false,
                        true,
                        0,
                        &trackers,
                        &ui,
                        &wkey,
                        &cfg,
                        verbose,
                    );
                }
            }
            continue;
        }

        // 3. File needs H.265 conversion: attach pre-probed metadata & enqueue into verified `queue`
        let v_dur = verify_start_instant.elapsed().as_secs_f64();
        wait_tracker.complete_active_task("verify_file", "ready_to_convert", v_dur);
        log_structured_event(
            LogLevel::Info,
            &local_nodename,
            &local_host_os,
            &wkey,
            "verifier",
            &ms.mount_point,
            Some(item.item_id),
            "verify_file_complete",
            &[
                ("source_file", item.source_path.display().to_string()),
                ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                ("source_codec", probe.video_codec.clone()),
                ("source_bitrate_kbps", probe.bitrate_kbps.to_string()),
                (
                    "video_duration_in_secs",
                    format!("{:.2}", probe.duration_secs),
                ),
                (
                    "thread_wait_in_secs",
                    format!("{:.3}", wait_breakdown.thread_wait_in_secs),
                ),
                ("verify_duration_in_secs", format!("{:.3}", v_dur)),
                ("verify_outcome", "ready_to_convert".to_string()),
            ],
        );
        item.cached_probe = Some(probe);
        ms.queue.lock().unwrap().push_back(item);
        let (total_rem, r_len) = ms.total_remaining_items();
        if let Ok(mut map) = ui.lock() {
            if let Some(w) = map.get_mut(&wkey) {
                w.queued_items_remaining = total_rem;
                w.ready_to_encode_count = r_len;
            }
        }
    }
    wait_tracker.mark_finished();
}

// --- Local Mount Worker-Group Execution (Stager -> Converter -> Flusher) ---
#[allow(clippy::too_many_arguments)]
pub fn execute_flusher_task(
    task: LocalPipelineTask,
    ms: &Arc<MountQueueState>,
    wg: &Arc<LocalWorkerGroupState>,
    trackers: &Arc<Mutex<HashMap<PathBuf, SubdirTracker>>>,
    ui: &Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    cfg: &AppConfig,
    wkey: &str,
    fdirs_log: &Path,
    slot_id: usize,
    verbose: bool,
) {
    let local_nodename = cfg.effective_local_nodename();
    let local_host_os = local_host_os_label().to_string();
    let file_name = task
        .item
        .source_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("video");
    let display_fname = format_filename_with_size(file_name, task.item.file_size_bytes);
    let src_mb = (task.item.file_size_bytes as f64) / (1024.0 * 1024.0);
    let tgt_mb = if task.kept_original {
        src_mb
    } else {
        (task.staged_output_size as f64) / (1024.0 * 1024.0)
    };
    let saved_mb = (task.saved_bytes as f64) / (1024.0 * 1024.0);

    // Case 1: Converter failed or did not produce a valid output file.
    // Flusher evicts staged input (and any partial staged output) from RAMDisk, reclaims partition space,
    // and records the failure in `broken_files.journal` + `failed_dirs.txt`.
    if let Some(conv_err) = task.conversion_error {
        let evict_start = Instant::now();
        ms.set_in_flight(
            &task.item,
            QueueEntryCategory::ActiveLocal,
            "LOCAL [Flusher: Evicting]",
        );
        if let Some(ref s_in) = task.staged_input {
            update_ui_slot(
                ui,
                wkey,
                slot_id,
                &format!("[Flusher] {}", display_fname),
                "evict-in",
                0,
                0.0,
                0.0,
                98.0,
                "RAMDisk",
                "Evicting Stage In",
            );
            let _ = fs::remove_file(s_in);
            wg.staged_input_bytes
                .fetch_sub(task.staged_input_size, Ordering::Relaxed);
        }
        if let Some(ref s_out) = task.staged_output {
            let _ = fs::remove_file(s_out);
            if task.staged_output_size > 0 {
                wg.staged_output_bytes
                    .fetch_sub(task.staged_output_size, Ordering::Relaxed);
            }
        } else {
            let _ = fs::remove_file(&task.final_encoded_dest);
        }

        let evict_dur = evict_start.elapsed().as_secs_f64();
        remove_ui_slot(ui, wkey, slot_id);
        ms.clear_in_flight(task.item.item_id);
        ms.in_flight_local_tasks.fetch_sub(1, Ordering::Relaxed);

        log_structured_event(
            LogLevel::Warn,
            &local_nodename,
            &local_host_os,
            wkey,
            "flusher",
            &ms.mount_point,
            Some(task.item.item_id),
            "flush_end",
            &[
                ("flush_mode", "evict_failed".to_string()),
                ("source_file", task.item.source_path.display().to_string()),
                ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                ("cleanup_duration_in_secs", format!("{:.3}", evict_dur)),
                ("reason", conv_err.to_string()),
            ],
        );

        if is_shutdown_requested() {
            return;
        }

        record_broken_file_in_journal(
            &task.item.source_path,
            task.item.file_size_bytes,
            &conv_err.to_string(),
        );
        let src_d = task
            .item
            .source_path
            .parent()
            .or(task.item.source_subdir.as_deref())
            .unwrap_or(&task.item.root_dir);
        record_file_failure_if_untracked_subdir(
            fdirs_log,
            &task.item,
            src_d,
            Some(task.item.dest_dir.as_path()),
            &format!(
                "[Worker-Group {} | Flusher] ERROR converting '{}' (evicted from RAMDisk & recorded in broken_files.journal)",
                ms.mount_point,
                task.item.source_path.display()
            ),
            &conv_err,
        );
        complete_queued_item(
            &task.item,
            false,
            false,
            true,
            0,
            trackers,
            ui,
            wkey,
            cfg,
            verbose,
        );
        return;
    }

    // Case 2: Converter succeeded.
    // Step 1: Move/copy converted file from RAMDisk to target on HDD (holding per-mount io_lock)
    ms.set_in_flight(
        &task.item,
        QueueEntryCategory::ActiveLocal,
        "LOCAL [Flusher: RAM->HDD]",
    );
    log_structured_event(
        LogLevel::Info,
        &local_nodename,
        &local_host_os,
        wkey,
        "flusher",
        &ms.mount_point,
        Some(task.item.item_id),
        "flush_start",
        &[
            ("flush_mode", "ramdisk_to_hdd".to_string()),
            ("source_file", task.item.source_path.display().to_string()),
            (
                "target_file",
                task.final_encoded_dest.display().to_string(),
            ),
            ("target_file_size_in_mb", format!("{:.2}", tgt_mb)),
        ],
    );

    let mut io_lock_wait_secs = 0.0f64;
    let mut active_flush_secs = 0.0f64;
    let flush_res: Result<()> = (|| {
        if let Some(parent) = task.final_encoded_dest.parent() {
            fs::create_dir_all(parent)?;
        }
        if task.kept_original {
            let orig_dest = task
                .item
                .dest_dir
                .join(build_h265_filename(&task.item.source_path, true, cfg));
            global_metrics().inc_io_lock_waiting(&ms.mount_point, ms.info.drive_type.as_str());
            let lock_wait_start = Instant::now();
            let _guard = ms.io_lock.lock().unwrap();
            io_lock_wait_secs = lock_wait_start.elapsed().as_secs_f64();
            global_metrics().record_io_lock_acquired(
                &ms.mount_point,
                ms.info.drive_type.as_str(),
                "flusher",
                io_lock_wait_secs,
            );
            let hold_start = Instant::now();
            update_ui_slot(
                ui,
                wkey,
                slot_id,
                &format!("[Flusher] {}", display_fname),
                "flush-orig",
                0,
                0.0,
                0.0,
                95.0,
                "HDD",
                "Moving Orig",
            );
            let r = move_file_safe(&task.item.source_path, &orig_dest);
            active_flush_secs = hold_start.elapsed().as_secs_f64();
            global_metrics().record_io_lock_released(&ms.mount_point, "flusher", active_flush_secs);
            r?;
        } else if let Some(ref s_out) = task.staged_output {
            global_metrics().inc_io_lock_waiting(&ms.mount_point, ms.info.drive_type.as_str());
            let lock_wait_start = Instant::now();
            let _guard = ms.io_lock.lock().unwrap();
            io_lock_wait_secs = lock_wait_start.elapsed().as_secs_f64();
            global_metrics().record_io_lock_acquired(
                &ms.mount_point,
                ms.info.drive_type.as_str(),
                "flusher",
                io_lock_wait_secs,
            );
            let hold_start = Instant::now();
            let r = copy_file_with_live_tui_progress(
                s_out,
                &task.final_encoded_dest,
                task.staged_output_size,
                ui,
                wkey,
                slot_id,
                &format!("[Flusher] {}", display_fname),
                "flush-out",
                task.probe.bitrate_kbps,
                "RAM->HDD",
                "Flushing->HDD",
                false,
            )
            .with_context(|| {
                format!(
                    "Failed flushing staged output {} -> {}",
                    s_out.display(),
                    task.final_encoded_dest.display()
                )
            });
            active_flush_secs = hold_start.elapsed().as_secs_f64();
            global_metrics().record_io_lock_released(&ms.mount_point, "flusher", active_flush_secs);
            r?;
            if task.item.is_root_in_place {
                let _ = fs::remove_file(&task.item.source_path);
            }
        } else if task.item.is_root_in_place {
            let _ = fs::remove_file(&task.item.source_path);
        }
        Ok(())
    })();

    // Step 2: Sequentially delete staged input file and staged output file from RAMDisk
    let cleanup_start = Instant::now();
    if let Some(ref s_in) = task.staged_input {
        update_ui_slot(
            ui,
            wkey,
            slot_id,
            &format!("[Flusher] {}", display_fname),
            "cleanup-in",
            0,
            0.0,
            0.0,
            98.0,
            "RAMDisk",
            "Del Stage In",
        );
        let _ = fs::remove_file(s_in);
        wg.staged_input_bytes
            .fetch_sub(task.staged_input_size, Ordering::Relaxed);
    }
    if let Some(ref s_out) = task.staged_output {
        update_ui_slot(
            ui,
            wkey,
            slot_id,
            &format!("[Flusher] {}", display_fname),
            "cleanup-out",
            0,
            0.0,
            0.0,
            99.0,
            "RAMDisk",
            "Del Stage Out",
        );
        let _ = fs::remove_file(s_out);
        wg.staged_output_bytes
            .fetch_sub(task.staged_output_size, Ordering::Relaxed);
    }
    let cleanup_secs = cleanup_start.elapsed().as_secs_f64();

    remove_ui_slot(ui, wkey, slot_id);
    ms.clear_in_flight(task.item.item_id);
    ms.in_flight_local_tasks.fetch_sub(1, Ordering::Relaxed);

    let gb_div = 1024.0 * 1024.0 * 1024.0;
    let cur_in_gb = (wg.staged_input_bytes.load(Ordering::Relaxed) as f64) / gb_div;
    let max_q_gb = cfg.RAMDISK_SIZE_GB * 0.5;
    let rem_q_gb = (max_q_gb - cur_in_gb).max(0.0);
    let tp_mbps = if active_flush_secs > 0.001 {
        tgt_mb / active_flush_secs
    } else {
        0.0
    };

    match flush_res {
        Ok(()) => {
            log_structured_event(
                LogLevel::Info,
                &local_nodename,
                &local_host_os,
                wkey,
                "flusher",
                &ms.mount_point,
                Some(task.item.item_id),
                "flush_end",
                &[
                    ("flush_mode", "ramdisk_to_hdd".to_string()),
                    ("source_file", task.item.source_path.display().to_string()),
                    (
                        "target_file",
                        task.final_encoded_dest.display().to_string(),
                    ),
                    ("target_file_size_in_mb", format!("{:.2}", tgt_mb)),
                    ("io_lock_wait_in_secs", format!("{:.3}", io_lock_wait_secs)),
                    ("flush_duration_in_secs", format!("{:.3}", active_flush_secs)),
                    ("cleanup_duration_in_secs", format!("{:.3}", cleanup_secs)),
                    ("throughput_mbps", format!("{:.2}", tp_mbps)),
                    ("storage_saved_in_mb", format!("{:.2}", saved_mb)),
                    ("quota_used_gb", format!("{:.2}", cur_in_gb)),
                    ("quota_remaining_gb", format!("{:.2}", rem_q_gb)),
                    ("quota_max_gb", format!("{:.2}", max_q_gb)),
                ],
            );
            complete_queued_item(
                &task.item,
                task.kept_original,
                false,
                false,
                task.saved_bytes,
                trackers,
                ui,
                wkey,
                cfg,
                verbose,
            );
        }
        Err(e) => {
            global_metrics().record_disk_error(&ms.mount_point, "write");
            log_structured_event(
                LogLevel::Error,
                &local_nodename,
                &local_host_os,
                wkey,
                "flusher",
                &ms.mount_point,
                Some(task.item.item_id),
                "flush_end",
                &[
                    ("flush_mode", "ramdisk_to_hdd_failed".to_string()),
                    ("source_file", task.item.source_path.display().to_string()),
                    (
                        "target_file",
                        task.final_encoded_dest.display().to_string(),
                    ),
                    ("error_type", "disk_flush_failed".to_string()),
                    ("reason", e.to_string()),
                ],
            );
            let _ = fs::remove_file(&task.final_encoded_dest);
            if is_shutdown_requested() {
                return;
            }
            record_broken_file_in_journal(
                &task.item.source_path,
                task.item.file_size_bytes,
                &e.to_string(),
            );
            let src_d = task
                .item
                .source_path
                .parent()
                .or(task.item.source_subdir.as_deref())
                .unwrap_or(&task.item.root_dir);
            record_file_failure_if_untracked_subdir(
                fdirs_log,
                &task.item,
                src_d,
                Some(task.item.dest_dir.as_path()),
                &format!(
                    "[Worker-Group {} | Flusher] ERROR flushing '{}' to target (recorded in broken_files.journal)",
                    ms.mount_point,
                    task.item.source_path.display()
                ),
                &e,
            );
            complete_queued_item(
                &task.item,
                false,
                false,
                true,
                0,
                trackers,
                ui,
                wkey,
                cfg,
                verbose,
            );
        }
    }
}

fn run_local_mount_worker(
    mount_state: Arc<MountQueueState>,
    all_mount_queues: Vec<Arc<MountQueueState>>,
    subdir_trackers: Arc<Mutex<HashMap<PathBuf, SubdirTracker>>>,
    shared_ui: Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    config: AppConfig,
    verbose: bool,
) {
    let worker_key = format!("local:{}", mount_state.mount_point);
    let profile = detect_encoder_profile(&config);
    global_metrics().register_mount(
        &mount_state.mount_point,
        mount_state.info.drive_type.as_str(),
    );
    let use_staging = config.ENABLE_RAMDISK
        || (mount_state.info.drive_type == DriveType::Hdd && config.USE_LOCAL_STAGING_FOR_HDD);
    let staging_dir = if use_staging {
        let dir = config.effective_staging_dir();
        if ensure_dir_created(&dir).is_ok() {
            Some(dir)
        } else {
            None
        }
    } else {
        None
    };
    let failed_dirs_log = PathBuf::from(&config.FAILED_DIRS_PATH);

    // Spawn Slot 0 Per-Mount Integrated Verifier Thread
    let verifier_handle = {
        let ms = Arc::clone(&mount_state);
        let trackers = Arc::clone(&subdir_trackers);
        let ui = Arc::clone(&shared_ui);
        let cfg = config.clone();
        let wkey = worker_key.clone();
        thread::spawn(move || {
            run_mount_verifier_loop(ms, trackers, ui, cfg, wkey, verbose);
        })
    };

    let wg_state = Arc::new(LocalWorkerGroupState {
        to_convert: Mutex::new(VecDeque::new()),
        to_flush: Mutex::new(VecDeque::new()),
        staged_input_bytes: AtomicU64::new(0),
        staged_output_bytes: AtomicU64::new(0),
        stager_done: AtomicBool::new(false),
        converter_done: AtomicBool::new(false),
    });

    // Thread 1 (Slot 1): worker-Group.stager
    let stager_handle = {
        let ms = Arc::clone(&mount_state);
        let all_mqs = all_mount_queues.clone();
        let wg = Arc::clone(&wg_state);
        let trackers = Arc::clone(&subdir_trackers);
        let ui = Arc::clone(&shared_ui);
        let cfg = config.clone();
        let wkey = worker_key.clone();
        let sdir = staging_dir.clone();
        let fdirs_log = failed_dirs_log.clone();
        let slot_id = 1usize;

        thread::Builder::new()
            .name(format!("wg-stager-{}", ms.mount_point))
            .spawn(move || {
                let local_nodename = cfg.effective_local_nodename();
                let local_host_os = local_host_os_label().to_string();
                let mut wait_tracker = ThreadWaitTracker::new(
                    &local_nodename,
                    &local_host_os,
                    &wkey,
                    "stager",
                    &ms.mount_point,
                );
                let gb_div = 1024.0 * 1024.0 * 1024.0;
                let mb_div = 1024.0 * 1024.0;

                loop {
                    if is_shutdown_requested() {
                        remove_ui_slot(&ui, &wkey, slot_id);
                        break;
                    }

                    // If staging is disabled or unavailable, pop directly from queue as unstaged
                    if sdir.is_none() {
                        let next_item = {
                            let mut q = ms.queue.lock().unwrap();
                            if let Some(it) = q.pop_front() {
                                ms.in_flight_local_tasks.fetch_add(1, Ordering::Relaxed);
                                Some(it)
                            } else {
                                None
                            }
                        };
                        match next_item {
                            Some(item) => {
                                let wait_b = wait_tracker.begin_active_task("stage_file");
                                let src_mb = (item.file_size_bytes as f64) / mb_div;
                                log_structured_event(
                                    LogLevel::Info,
                                    &local_nodename,
                                    &local_host_os,
                                    &wkey,
                                    "stager",
                                    &ms.mount_point,
                                    Some(item.item_id),
                                    "stage_file_complete",
                                    &[
                                        ("stage_mode", "unstaged_direct".to_string()),
                                        ("source_file", item.source_path.display().to_string()),
                                        ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                                        (
                                            "thread_wait_in_secs",
                                            format!("{:.3}", wait_b.thread_wait_in_secs),
                                        ),
                                        (
                                            "upstream_queue_wait_in_secs",
                                            format!("{:.3}", wait_b.upstream_queue_wait_in_secs),
                                        ),
                                        ("wait_cycles", wait_b.wait_cycles.to_string()),
                                        ("stage_duration_in_secs", "0.000".to_string()),
                                    ],
                                );
                                wait_tracker.complete_active_task(
                                    "stage_file",
                                    "unstaged_direct",
                                    0.0,
                                );
                                let (total_rem, r_len) = ms.total_remaining_items();
                                if let Ok(mut map) = ui.lock() {
                                    if let Some(w) = map.get_mut(&wkey) {
                                        w.queued_items_remaining = total_rem;
                                        w.ready_to_encode_count = r_len;
                                        w.current_root_dir = item.root_dir.display().to_string();
                                        w.current_target_label = item.target_label.clone();
                                        w.current_target_size_bytes = item.target_size_bytes;
                                        w.free_space_bytes =
                                            get_available_free_space_bytes(&item.root_dir);
                                        w.phase = "Worker-Group pipeline active".to_string();
                                    }
                                }
                                let probe = item
                                    .cached_probe
                                    .clone()
                                    .unwrap_or_else(|| get_video_probe_info(&item.source_path));
                                let final_encoded_dest = item
                                    .dest_dir
                                    .join(build_h265_filename(&item.source_path, false, &cfg));
                                ms.set_in_flight(
                                    &item,
                                    QueueEntryCategory::ActiveLocal,
                                    "LOCAL [Unstaged: Waiting Conv]",
                                );
                                wg.to_convert.lock().unwrap().push_back(LocalPipelineTask {
                                    item,
                                    probe,
                                    staged_input: None,
                                    staged_input_size: 0,
                                    staged_output: None,
                                    staged_output_size: 0,
                                    final_encoded_dest,
                                    kept_original: false,
                                    saved_bytes: 0,
                                    conversion_error: None,
                                });
                                continue;
                            }
                            None => {
                                if is_shutdown_requested()
                                    || (ms.verifier_done.load(Ordering::Relaxed)
                                        && ms.raw_queue.lock().unwrap().is_empty())
                                {
                                    break;
                                }
                                wait_tracker.sleep_with_reason(
                                    WaitReason::UpstreamQueueEmpty,
                                    Duration::from_millis(cfg.STAGER_WAIT_MSEC.max(10)),
                                );
                                continue;
                            }
                        }
                    }

                    let staging_root = sdir.as_ref().unwrap();
                    let (_part_size, max_input_stage_size, future_max_input_stage_size) =
                        compute_worker_group_partition_limits(&ms, &all_mqs, &cfg);
                    let cur_staged_in = wg.staged_input_bytes.load(Ordering::Relaxed);
                    let cur_staged_out = wg.staged_output_bytes.load(Ordering::Relaxed);

                    let outcome = {
                        let mut q = ms.queue.lock().unwrap();
                        let res = select_next_item_for_stager(
                            &mut q,
                            max_input_stage_size,
                            future_max_input_stage_size,
                            cur_staged_in,
                        );
                        if matches!(
                            res,
                            StagerQueueOutcome::StageNow(_) | StagerQueueOutcome::ProcessUnstaged(_)
                        ) {
                            ms.in_flight_local_tasks.fetch_add(1, Ordering::Relaxed);
                        }
                        res
                    };

                    match outcome {
                        StagerQueueOutcome::QueueEmpty => {
                            remove_ui_slot(&ui, &wkey, slot_id);
                            if is_shutdown_requested()
                                || (ms.verifier_done.load(Ordering::Relaxed)
                                    && ms.raw_queue.lock().unwrap().is_empty())
                            {
                                break;
                            }
                            wait_tracker.sleep_with_reason(
                                WaitReason::UpstreamQueueEmpty,
                                Duration::from_millis(cfg.STAGER_WAIT_MSEC.max(10)),
                            );
                        }
                        StagerQueueOutcome::WaitForSpaceOrResize => {
                            let wait_label = format!(
                                "[Stager] Waiting RAMDisk (in: {:.2}/{:.2} GB, out: {:.2} GB)",
                                cur_staged_in as f64 / gb_div,
                                max_input_stage_size as f64 / gb_div,
                                cur_staged_out as f64 / gb_div,
                            );
                            update_ui_slot(
                                &ui,
                                &wkey,
                                slot_id,
                                &wait_label,
                                "ramdisk-wait",
                                0,
                                0.0,
                                0.0,
                                0.0,
                                "Waiting",
                                "Waiting",
                            );
                            wait_tracker.sleep_with_reason(
                                WaitReason::RamdiskQuotaFull,
                                Duration::from_millis(cfg.STAGER_WAIT_MSEC.max(10)),
                            );
                        }
                        StagerQueueOutcome::ProcessUnstaged(item) => {
                            let wait_b = wait_tracker.begin_active_task("stage_file");
                            let src_mb = (item.file_size_bytes as f64) / mb_div;
                            let (total_rem, r_len) = ms.total_remaining_items();
                            if let Ok(mut map) = ui.lock() {
                                if let Some(w) = map.get_mut(&wkey) {
                                    w.queued_items_remaining = total_rem;
                                    w.ready_to_encode_count = r_len;
                                    w.current_root_dir = item.root_dir.display().to_string();
                                    w.current_target_label = item.target_label.clone();
                                    w.current_target_size_bytes = item.target_size_bytes;
                                    w.free_space_bytes =
                                        get_available_free_space_bytes(&item.root_dir);
                                    w.phase = "Worker-Group pipeline active".to_string();
                                }
                            }
                            log_structured_event(
                                LogLevel::Info,
                                &local_nodename,
                                &local_host_os,
                                &wkey,
                                "stager",
                                &ms.mount_point,
                                Some(item.item_id),
                                "stage_file_complete",
                                &[
                                    ("stage_mode", "unstaged_oversize".to_string()),
                                    ("source_file", item.source_path.display().to_string()),
                                    ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                                    (
                                        "thread_wait_in_secs",
                                        format!("{:.3}", wait_b.thread_wait_in_secs),
                                    ),
                                    (
                                        "quota_wait_in_secs",
                                        format!("{:.3}", wait_b.quota_wait_in_secs),
                                    ),
                                    (
                                        "upstream_queue_wait_in_secs",
                                        format!("{:.3}", wait_b.upstream_queue_wait_in_secs),
                                    ),
                                    ("wait_cycles", wait_b.wait_cycles.to_string()),
                                    ("stage_duration_in_secs", "0.000".to_string()),
                                ],
                            );
                            wait_tracker.complete_active_task(
                                "stage_file",
                                "unstaged_oversize",
                                0.0,
                            );
                            let probe = item
                                .cached_probe
                                .clone()
                                .unwrap_or_else(|| get_video_probe_info(&item.source_path));
                            let final_encoded_dest = item
                                .dest_dir
                                .join(build_h265_filename(&item.source_path, false, &cfg));
                            ms.set_in_flight(
                                &item,
                                QueueEntryCategory::ActiveLocal,
                                "LOCAL [Unstaged: Waiting Conv]",
                            );
                            wg.to_convert.lock().unwrap().push_back(LocalPipelineTask {
                                item,
                                probe,
                                staged_input: None,
                                staged_input_size: 0,
                                staged_output: None,
                                staged_output_size: 0,
                                final_encoded_dest,
                                kept_original: false,
                                saved_bytes: 0,
                                conversion_error: None,
                            });
                        }
                        StagerQueueOutcome::StageNow(item) => {
                            // Give Flusher priority on per-mount io_lock so RAMDisk space frees up first
                            while !wg.to_flush.lock().unwrap().is_empty()
                                && !is_shutdown_requested()
                            {
                                wait_tracker.sleep_with_reason(
                                    WaitReason::FlusherPriorityYield,
                                    Duration::from_millis(25),
                                );
                            }

                            let mut wait_b = wait_tracker.begin_active_task("stage_file");
                            let src_mb = (item.file_size_bytes as f64) / mb_div;
                            let quota_used_before_gb = (cur_staged_in as f64) / gb_div;
                            let quota_max_gb = (max_input_stage_size as f64) / gb_div;
                            let quota_rem_before_gb = (quota_max_gb - quota_used_before_gb).max(0.0);

                            ms.set_in_flight(
                                &item,
                                QueueEntryCategory::ActiveLocal,
                                "LOCAL [Stager: HDD->RAM]",
                            );
                            let (total_rem, r_len) = ms.total_remaining_items();
                            if let Ok(mut map) = ui.lock() {
                                if let Some(w) = map.get_mut(&wkey) {
                                    w.queued_items_remaining = total_rem;
                                    w.ready_to_encode_count = r_len;
                                    w.current_root_dir = item.root_dir.display().to_string();
                                    w.current_target_label = item.target_label.clone();
                                    w.current_target_size_bytes = item.target_size_bytes;
                                    w.free_space_bytes =
                                        get_available_free_space_bytes(&item.root_dir);
                                    w.phase = "Worker-Group pipeline active".to_string();
                                }
                            }

                            let file_name = item
                                .source_path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("video");
                            let display_fname =
                                format_filename_with_size(file_name, item.file_size_bytes);
                            let ext = item
                                .source_path
                                .extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("mp4");
                            let safe_key: String = wkey
                                .chars()
                                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                                .collect();
                            let pid = std::process::id();
                            let s_in = staging_root.join(format!(
                                "stage_{}_{}_item{}_in.{}",
                                pid, safe_key, item.item_id, ext
                            ));
                            let s_out = staging_root.join(format!(
                                "stage_{}_{}_item{}_out.mp4",
                                pid, safe_key, item.item_id
                            ));
                            let final_encoded_dest = item
                                .dest_dir
                                .join(build_h265_filename(&item.source_path, false, &cfg));

                            log_structured_event(
                                LogLevel::Info,
                                &local_nodename,
                                &local_host_os,
                                &wkey,
                                "stager",
                                &ms.mount_point,
                                Some(item.item_id),
                                "stage_file_start",
                                &[
                                    ("stage_mode", "hdd_to_ramdisk".to_string()),
                                    ("source_file", item.source_path.display().to_string()),
                                    ("staged_file", s_in.display().to_string()),
                                    ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                                    (
                                        "thread_wait_in_secs",
                                        format!("{:.3}", wait_b.thread_wait_in_secs),
                                    ),
                                    (
                                        "quota_wait_in_secs",
                                        format!("{:.3}", wait_b.quota_wait_in_secs),
                                    ),
                                    (
                                        "upstream_queue_wait_in_secs",
                                        format!("{:.3}", wait_b.upstream_queue_wait_in_secs),
                                    ),
                                    (
                                        "flusher_priority_wait_in_secs",
                                        format!("{:.3}", wait_b.flusher_priority_wait_in_secs),
                                    ),
                                    ("wait_cycles", wait_b.wait_cycles.to_string()),
                                    ("quota_used_gb", format!("{:.2}", quota_used_before_gb)),
                                    ("quota_remaining_gb", format!("{:.2}", quota_rem_before_gb)),
                                    ("quota_max_gb", format!("{:.2}", quota_max_gb)),
                                ],
                            );

                            let mut active_stage_secs = 0.0f64;
                            let stage_res: Result<()> = (|| {
                                global_metrics()
                                    .inc_io_lock_waiting(&ms.mount_point, ms.info.drive_type.as_str());
                                let lock_wait_start = Instant::now();
                                let _guard = ms.io_lock.lock().unwrap();
                                let lock_wait_secs = lock_wait_start.elapsed().as_secs_f64();
                                wait_b.add_io_lock_wait(lock_wait_secs);
                                global_metrics().record_io_lock_acquired(
                                    &ms.mount_point,
                                    ms.info.drive_type.as_str(),
                                    "stager",
                                    lock_wait_secs,
                                );
                                let hold_start = Instant::now();
                                let copy_r = copy_file_with_live_tui_progress(
                                    &item.source_path,
                                    &s_in,
                                    item.file_size_bytes,
                                    &ui,
                                    &wkey,
                                    slot_id,
                                    &format!("[Stager] {}", display_fname),
                                    "stage-in",
                                    0,
                                    "HDD->RAM",
                                    "Staging->RAM",
                                    true,
                                )
                                .with_context(|| {
                                    format!(
                                        "Failed staging copy {} -> {}",
                                        item.source_path.display(),
                                        s_in.display()
                                    )
                                });
                                active_stage_secs = hold_start.elapsed().as_secs_f64();
                                global_metrics().record_io_lock_released(
                                    &ms.mount_point,
                                    "stager",
                                    active_stage_secs,
                                );
                                copy_r?;
                                Ok(())
                            })();

                            remove_ui_slot(&ui, &wkey, slot_id);

                            match stage_res {
                                Ok(()) => {
                                    let staged_sz = fs::metadata(&s_in)
                                        .map(|m| m.len())
                                        .unwrap_or(item.file_size_bytes);
                                    let new_staged_in = wg
                                        .staged_input_bytes
                                        .fetch_add(staged_sz, Ordering::Relaxed)
                                        + staged_sz;
                                    let quota_used_after_gb = (new_staged_in as f64) / gb_div;
                                    let quota_rem_after_gb =
                                        (quota_max_gb - quota_used_after_gb).max(0.0);
                                    let tp_mbps = if active_stage_secs > 0.001 {
                                        src_mb / active_stage_secs
                                    } else {
                                        0.0
                                    };
                                    wait_tracker.complete_active_task(
                                        "stage_file",
                                        "success",
                                        active_stage_secs,
                                    );
                                    log_structured_event(
                                        LogLevel::Info,
                                        &local_nodename,
                                        &local_host_os,
                                        &wkey,
                                        "stager",
                                        &ms.mount_point,
                                        Some(item.item_id),
                                        "stage_file_complete",
                                        &[
                                            ("stage_mode", "hdd_to_ramdisk".to_string()),
                                            ("source_file", item.source_path.display().to_string()),
                                            ("staged_file", s_in.display().to_string()),
                                            ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                                            (
                                                "thread_wait_in_secs",
                                                format!("{:.3}", wait_b.thread_wait_in_secs),
                                            ),
                                            (
                                                "quota_wait_in_secs",
                                                format!("{:.3}", wait_b.quota_wait_in_secs),
                                            ),
                                            (
                                                "upstream_queue_wait_in_secs",
                                                format!("{:.3}", wait_b.upstream_queue_wait_in_secs),
                                            ),
                                            (
                                                "flusher_priority_wait_in_secs",
                                                format!(
                                                    "{:.3}",
                                                    wait_b.flusher_priority_wait_in_secs
                                                ),
                                            ),
                                            (
                                                "io_lock_wait_in_secs",
                                                format!("{:.3}", wait_b.io_lock_wait_in_secs),
                                            ),
                                            ("wait_cycles", wait_b.wait_cycles.to_string()),
                                            (
                                                "stage_duration_in_secs",
                                                format!("{:.3}", active_stage_secs),
                                            ),
                                            ("throughput_mbps", format!("{:.2}", tp_mbps)),
                                            ("quota_used_gb", format!("{:.2}", quota_used_after_gb)),
                                            (
                                                "quota_remaining_gb",
                                                format!("{:.2}", quota_rem_after_gb),
                                            ),
                                            ("quota_max_gb", format!("{:.2}", quota_max_gb)),
                                        ],
                                    );
                                    let probe = item
                                        .cached_probe
                                        .clone()
                                        .unwrap_or_else(|| get_video_probe_info(&s_in));
                                    ms.set_in_flight(
                                        &item,
                                        QueueEntryCategory::ActiveLocal,
                                        "LOCAL [Staged: Waiting Conv]",
                                    );
                                    wg.to_convert.lock().unwrap().push_back(LocalPipelineTask {
                                        item,
                                        probe,
                                        staged_input: Some(s_in),
                                        staged_input_size: staged_sz,
                                        staged_output: Some(s_out),
                                        staged_output_size: 0,
                                        final_encoded_dest,
                                        kept_original: false,
                                        saved_bytes: 0,
                                        conversion_error: None,
                                    });
                                }
                                Err(e) => {
                                    let _ = fs::remove_file(&s_in);
                                    ms.clear_in_flight(item.item_id);
                                    ms.in_flight_local_tasks.fetch_sub(1, Ordering::Relaxed);
                                    if is_shutdown_requested() {
                                        break;
                                    }
                                    global_metrics().record_disk_error(&ms.mount_point, "read");
                                    wait_tracker.record_error("stage_read_failed");
                                    wait_tracker.complete_active_task(
                                        "stage_file",
                                        "failed",
                                        active_stage_secs,
                                    );
                                    log_structured_event(
                                        LogLevel::Error,
                                        &local_nodename,
                                        &local_host_os,
                                        &wkey,
                                        "stager",
                                        &ms.mount_point,
                                        Some(item.item_id),
                                        "stage_file_failed",
                                        &[
                                            ("source_file", item.source_path.display().to_string()),
                                            ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                                            ("error_type", "stage_read_failed".to_string()),
                                            ("reason", e.to_string()),
                                        ],
                                    );
                                    record_broken_file_in_journal(
                                        &item.source_path,
                                        item.file_size_bytes,
                                        &e.to_string(),
                                    );
                                    let src_d = item
                                        .source_path
                                        .parent()
                                        .or(item.source_subdir.as_deref())
                                        .unwrap_or(&item.root_dir);
                                    record_file_failure_if_untracked_subdir(
                                        &fdirs_log,
                                        &item,
                                        src_d,
                                        Some(item.dest_dir.as_path()),
                                        &format!(
                                            "[Worker-Group {} | Stager] ERROR staging '{}' (recorded in broken_files.journal)",
                                            ms.mount_point,
                                            item.source_path.display()
                                        ),
                                        &e,
                                    );
                                    complete_queued_item(
                                        &item,
                                        false,
                                        false,
                                        true,
                                        0,
                                        &trackers,
                                        &ui,
                                        &wkey,
                                        &cfg,
                                        verbose,
                                    );
                                }
                            }
                        }
                    }
                }
                wg.stager_done.store(true, Ordering::Relaxed);
                wait_tracker.mark_finished();
            })
            .expect("Failed to spawn worker-Group.stager thread")
    };

    // Thread 2 (Slot 2): worker-Group.converter
    let converter_handle = {
        let ms = Arc::clone(&mount_state);
        let wg = Arc::clone(&wg_state);
        let ui = Arc::clone(&shared_ui);
        let cfg = config.clone();
        let prof = profile.clone();
        let wkey = worker_key.clone();
        let slot_id = 2usize;

        thread::Builder::new()
            .name(format!("wg-converter-{}", ms.mount_point))
            .spawn(move || {
                let local_nodename = cfg.effective_local_nodename();
                let local_host_os = local_host_os_label().to_string();
                let mut wait_tracker = ThreadWaitTracker::new(
                    &local_nodename,
                    &local_host_os,
                    &wkey,
                    "converter",
                    &ms.mount_point,
                );
                let mb_div = 1024.0 * 1024.0;

                loop {
                    let next_task = {
                        let mut q = wg.to_convert.lock().unwrap();
                        q.pop_front()
                    };

                    let mut task = match next_task {
                        Some(t) => t,
                        None => {
                            if is_shutdown_requested() || wg.stager_done.load(Ordering::Relaxed) {
                                break;
                            }
                            wait_tracker.sleep_with_reason(
                                WaitReason::UpstreamQueueEmpty,
                                Duration::from_millis(80),
                            );
                            continue;
                        }
                    };

                    if is_shutdown_requested() {
                        remove_ui_slot(&ui, &wkey, slot_id);
                        task.conversion_error = Some(anyhow::anyhow!(
                            "Aborted in-flight conversion due to graceful shutdown"
                        ));
                        ms.set_in_flight(
                            &task.item,
                            QueueEntryCategory::ActiveLocal,
                            "LOCAL [Failed: Waiting Evict]",
                        );
                        wg.to_flush.lock().unwrap().push_back(task);
                        continue;
                    }

                    let wait_b = wait_tracker.begin_active_task("conversion");
                    ms.set_in_flight(
                        &task.item,
                        QueueEntryCategory::ActiveLocal,
                        "LOCAL [Converter: Encoding]",
                    );

                    if let Some(parent) = task.final_encoded_dest.parent() {
                        let _ = fs::create_dir_all(parent);
                    }

                    let src_size = task.item.file_size_bytes;
                    let src_mb = (src_size as f64) / mb_div;
                    let active_in = task
                        .staged_input
                        .as_deref()
                        .unwrap_or(&task.item.source_path);
                    let active_out = task
                        .staged_output
                        .as_deref()
                        .unwrap_or(&task.final_encoded_dest);
                    let file_name = task
                        .item
                        .source_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("video");
                    let display_fname = format_filename_with_size(file_name, src_size);
                    let copy_audio = (cfg.COPY_COMPATIBLE_AUDIO
                        || prof.target.audio == codecs::TargetAudioCodec::Copy)
                        && codecs::is_audio_copy_compatible(
                            &task.probe.audio_codec,
                            prof.target.container,
                            prof.target.audio,
                        );
                    let use_hwaccel = !prof.hwaccel_args.is_empty()
                        && (std::env::consts::OS != "macos"
                            || is_macos_hw_decode_supported(&task.probe.video_codec));

                    log_structured_event(
                        LogLevel::Info,
                        &local_nodename,
                        &local_host_os,
                        &wkey,
                        "converter",
                        &ms.mount_point,
                        Some(task.item.item_id),
                        "conversion_start",
                        &[
                            ("source_file", task.item.source_path.display().to_string()),
                            ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                            ("source_codec", task.probe.video_codec.clone()),
                            (
                                "source_bitrate_kbps",
                                task.probe.bitrate_kbps.to_string(),
                            ),
                            ("audio_codec", task.probe.audio_codec.clone()),
                            (
                                "video_duration_in_secs",
                                format!("{:.2}", task.probe.duration_secs),
                            ),
                            ("encoder", prof.name.clone()),
                            ("hwaccel_enabled", use_hwaccel.to_string()),
                            (
                                "audio_mode",
                                if copy_audio { "copy" } else { "transcode_aac" }.to_string(),
                            ),
                            (
                                "thread_wait_in_secs",
                                format!("{:.3}", wait_b.thread_wait_in_secs),
                            ),
                            (
                                "upstream_queue_wait_in_secs",
                                format!("{:.3}", wait_b.upstream_queue_wait_in_secs),
                            ),
                            ("wait_cycles", wait_b.wait_cycles.to_string()),
                        ],
                    );

                    let encode_start = Instant::now();
                    let mut encode_only_secs = 0.0f64;
                    let mut verify_only_secs = 0.0f64;
                    let mut hwaccel_fallback_used = false;

                    let conv_res: Result<()> = (|| {
                        let verify_encoded_output = || -> Result<f64> {
                            let verify_start = Instant::now();
                            let produced_bytes =
                                fs::metadata(active_out).map(|m| m.len()).unwrap_or(0);
                            if !active_out.exists() || produced_bytes == 0 {
                                let _ = fs::remove_file(active_out);
                                anyhow::bail!(
                                    "Converter did not produce a non-empty output file at {:?} for source {:?}",
                                    active_out,
                                    task.item.source_path
                                );
                            }

                            // Requirement I: Verify output container, video+audio streams, runtime, and 1-frame decode
                            let out_probe = get_video_probe_info(active_out);
                            if let Err(dur_err) = verify_output_durations(
                                &task.item.source_path,
                                &task.probe,
                                &out_probe,
                                &cfg,
                            ) {
                                let _ = fs::remove_file(active_out);
                                anyhow::bail!(
                                    "Output duration mismatch for {:?}: {}",
                                    active_out,
                                    dur_err
                                );
                            }
                            if let Err(dec_err) = verify_first_frame_decodable(active_out) {
                                let _ = fs::remove_file(active_out);
                                anyhow::bail!(
                                    "Output 1-frame decode verification failed for {:?}: {}",
                                    active_out,
                                    dec_err
                                );
                            }
                            Ok(verify_start.elapsed().as_secs_f64())
                        };

                        // Attempt 1 (Fast Path): Hardware-accelerated decoding + MP4-compatible audio copy
                        let first_attempt = run_local_ffmpeg_with_progress(
                            active_in,
                            active_out,
                            &prof,
                            cfg.FFMPEG_NICE_VALUE,
                            use_hwaccel,
                            copy_audio,
                            false,
                            &task.probe,
                            slot_id,
                            &display_fname,
                            &ui,
                            &wkey,
                            verbose,
                        )
                        .and_then(|_| verify_encoded_output());

                        match first_attempt {
                            Ok(v_secs) => {
                                encode_only_secs =
                                    (encode_start.elapsed().as_secs_f64() - v_secs).max(0.0);
                                verify_only_secs = v_secs;
                                Ok(())
                            }
                            Err(err) => {
                                if is_shutdown_requested() {
                                    return Err(err);
                                }
                                let can_fallback = use_hwaccel
                                    || copy_audio
                                    || !prof.fallback_video_filter_args.is_empty();
                                if can_fallback {
                                    hwaccel_fallback_used = true;
                                    log_structured_event(
                                        LogLevel::Warn,
                                        &local_nodename,
                                        &local_host_os,
                                        &wkey,
                                        "converter",
                                        &ms.mount_point,
                                        Some(task.item.item_id),
                                        "hwaccel_software_fallback",
                                        &[
                                            (
                                                "source_file",
                                                task.item.source_path.display().to_string(),
                                            ),
                                            ("source_codec", task.probe.video_codec.clone()),
                                            ("reason", err.to_string()),
                                        ],
                                    );
                                    let _ = fs::remove_file(active_out);
                                    // Attempt 2 (Resilient Fallback): CPU software decode + conditional bwdif
                                    // deinterlace filter + clock-synced audio transcode (aac_at on macOS)
                                    run_local_ffmpeg_with_progress(
                                        active_in,
                                        active_out,
                                        &prof,
                                        cfg.FFMPEG_NICE_VALUE,
                                        false,
                                        false,
                                        true,
                                        &task.probe,
                                        slot_id,
                                        &display_fname,
                                        &ui,
                                        &wkey,
                                        verbose,
                                    )?;
                                    let v_secs = verify_encoded_output()?;
                                    encode_only_secs =
                                        (encode_start.elapsed().as_secs_f64() - v_secs).max(0.0);
                                    verify_only_secs = v_secs;
                                    Ok(())
                                } else {
                                    Err(err)
                                }
                            }
                        }
                    })();

                    remove_ui_slot(&ui, &wkey, slot_id);
                    let total_conv_secs = encode_start.elapsed().as_secs_f64();

                    match conv_res {
                        Ok(()) => {
                            let out_size =
                                fs::metadata(active_out).map(|m| m.len()).unwrap_or(0);
                            if cfg.KEEP_SMALLER_ORIGINAL && out_size >= src_size && src_size > 0 {
                                if task.staged_output.is_none() {
                                    let _ = fs::remove_file(&task.final_encoded_dest);
                                }
                                task.kept_original = true;
                                task.saved_bytes = 0;
                                task.staged_output_size = out_size;
                                if task.staged_output.is_some() {
                                    wg.staged_output_bytes
                                        .fetch_add(out_size, Ordering::Relaxed);
                                }
                            } else {
                                task.kept_original = false;
                                task.saved_bytes = src_size.saturating_sub(out_size);
                                task.staged_output_size = out_size;
                                if task.staged_output.is_some() {
                                    wg.staged_output_bytes
                                        .fetch_add(out_size, Ordering::Relaxed);
                                }
                            }

                            let tgt_mb = (out_size as f64) / mb_div;
                            let saved_mb = (task.saved_bytes as f64) / mb_div;
                            let comp_ratio = if src_size > 0 {
                                (out_size as f64) / (src_size as f64)
                            } else {
                                1.0
                            };
                            let speed_x = if encode_only_secs > 0.01 && task.probe.duration_secs > 0.0
                            {
                                task.probe.duration_secs / encode_only_secs
                            } else {
                                1.0
                            };

                            wait_tracker.complete_active_task(
                                "conversion",
                                if task.kept_original {
                                    "kept_smaller_original"
                                } else {
                                    "success"
                                },
                                total_conv_secs,
                            );
                            global_metrics().record_media_conversion_result(
                                &local_nodename,
                                &local_host_os,
                                &wkey,
                                &prof.name,
                                &task.probe.video_codec,
                                src_size,
                                out_size,
                                task.saved_bytes,
                                task.probe.duration_secs,
                                hwaccel_fallback_used,
                                task.kept_original,
                            );
                            log_structured_event(
                                LogLevel::Info,
                                &local_nodename,
                                &local_host_os,
                                &wkey,
                                "converter",
                                &ms.mount_point,
                                Some(task.item.item_id),
                                "conversion_end",
                                &[
                                    ("source_file", task.item.source_path.display().to_string()),
                                    ("source_codec", task.probe.video_codec.clone()),
                                    (
                                        "source_bitrate_kbps",
                                        task.probe.bitrate_kbps.to_string(),
                                    ),
                                    ("audio_codec", task.probe.audio_codec.clone()),
                                    (
                                        "video_duration_in_secs",
                                        format!("{:.2}", task.probe.duration_secs),
                                    ),
                                    ("encoder", prof.name.clone()),
                                    (
                                        "thread_wait_in_secs",
                                        format!("{:.3}", wait_b.thread_wait_in_secs),
                                    ),
                                    (
                                        "upstream_queue_wait_in_secs",
                                        format!("{:.3}", wait_b.upstream_queue_wait_in_secs),
                                    ),
                                    ("wait_cycles", wait_b.wait_cycles.to_string()),
                                    (
                                        "conversion_duration_in_secs",
                                        format!("{:.3}", encode_only_secs),
                                    ),
                                    (
                                        "output_verify_duration_in_secs",
                                        format!("{:.3}", verify_only_secs),
                                    ),
                                    ("speed_x", format!("{:.2}", speed_x)),
                                    ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                                    ("target_file_size_in_mb", format!("{:.2}", tgt_mb)),
                                    ("storage_saved_in_mb", format!("{:.2}", saved_mb)),
                                    ("compression_ratio", format!("{:.3}", comp_ratio)),
                                    (
                                        "hwaccel_fallback_used",
                                        hwaccel_fallback_used.to_string(),
                                    ),
                                    ("kept_original", task.kept_original.to_string()),
                                ],
                            );

                            ms.set_in_flight(
                                &task.item,
                                QueueEntryCategory::ActiveLocal,
                                "LOCAL [Encoded: Waiting Flush]",
                            );
                            wg.to_flush.lock().unwrap().push_back(task);
                        }
                        Err(e) => {
                            global_metrics().update_media_active_speed(
                                &local_nodename,
                                &local_host_os,
                                &wkey,
                                &prof.name,
                                0.0,
                            );
                            wait_tracker.record_error("ffmpeg_encode_failed");
                            wait_tracker.complete_active_task(
                                "conversion",
                                "failed",
                                total_conv_secs,
                            );
                            log_structured_event(
                                LogLevel::Error,
                                &local_nodename,
                                &local_host_os,
                                &wkey,
                                "converter",
                                &ms.mount_point,
                                Some(task.item.item_id),
                                "conversion_failed",
                                &[
                                    ("source_file", task.item.source_path.display().to_string()),
                                    ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                                    ("source_codec", task.probe.video_codec.clone()),
                                    (
                                        "conversion_duration_in_secs",
                                        format!("{:.3}", total_conv_secs),
                                    ),
                                    ("error_type", "ffmpeg_encode_failed".to_string()),
                                    ("reason", e.to_string()),
                                ],
                            );
                            // Forward failed conversion task to Flusher so Flusher evicts staged input/output
                            // from RAMDisk, reclaims partition space, and records broken_files.journal
                            let partial_out_size = task
                                .staged_output
                                .as_ref()
                                .and_then(|p| fs::metadata(p).ok())
                                .map(|m| m.len())
                                .unwrap_or(0);
                            if partial_out_size > 0 {
                                task.staged_output_size = partial_out_size;
                                wg.staged_output_bytes
                                    .fetch_add(partial_out_size, Ordering::Relaxed);
                            }
                            task.conversion_error = Some(e);
                            ms.set_in_flight(
                                &task.item,
                                QueueEntryCategory::ActiveLocal,
                                "LOCAL [Failed: Waiting Evict]",
                            );
                            wg.to_flush.lock().unwrap().push_back(task);
                        }
                    }
                }
                wg.converter_done.store(true, Ordering::Relaxed);
                wait_tracker.mark_finished();
            })
            .expect("Failed to spawn worker-Group.converter thread")
    };

    // Thread 3 (Slot 3): worker-Group.flusher
    let flusher_handle = {
        let ms = Arc::clone(&mount_state);
        let wg = Arc::clone(&wg_state);
        let trackers = Arc::clone(&subdir_trackers);
        let ui = Arc::clone(&shared_ui);
        let cfg = config.clone();
        let wkey = worker_key.clone();
        let fdirs_log = failed_dirs_log.clone();
        let slot_id = 3usize;

        thread::Builder::new()
            .name(format!("wg-flusher-{}", ms.mount_point))
            .spawn(move || {
                let local_nodename = cfg.effective_local_nodename();
                let local_host_os = local_host_os_label().to_string();
                let mut wait_tracker = ThreadWaitTracker::new(
                    &local_nodename,
                    &local_host_os,
                    &wkey,
                    "flusher",
                    &ms.mount_point,
                );
                loop {
                    let next_task = {
                        let mut q = wg.to_flush.lock().unwrap();
                        q.pop_front()
                    };

                    let task = match next_task {
                        Some(t) => t,
                        None => {
                            if wg.converter_done.load(Ordering::Relaxed) {
                                remove_ui_slot(&ui, &wkey, slot_id);
                                break;
                            }
                            wait_tracker.sleep_with_reason(
                                WaitReason::UpstreamQueueEmpty,
                                Duration::from_millis(80),
                            );
                            continue;
                        }
                    };

                    let _wait_b = wait_tracker.begin_active_task("flush");
                    let flush_start = Instant::now();
                    let had_err = task.conversion_error.is_some();
                    execute_flusher_task(
                        task,
                        &ms,
                        &wg,
                        &trackers,
                        &ui,
                        &cfg,
                        &wkey,
                        &fdirs_log,
                        slot_id,
                        verbose,
                    );
                    wait_tracker.complete_active_task(
                        "flush",
                        if had_err { "evicted_failed" } else { "success" },
                        flush_start.elapsed().as_secs_f64(),
                    );
                }
                wait_tracker.mark_finished();
            })
            .expect("Failed to spawn worker-Group.flusher thread")
    };

    let _ = stager_handle.join();
    let _ = converter_handle.join();
    let _ = flusher_handle.join();
    let _ = verifier_handle.join();

    // Mark this Worker-Group finished so remaining active Worker-Groups dynamically expand their RAMDisk partition
    mount_state
        .local_worker_finished
        .store(true, Ordering::Relaxed);

    let remaining_groups = all_mount_queues
        .iter()
        .filter(|mq| !mq.local_worker_finished.load(Ordering::Relaxed))
        .count();
    if remaining_groups > 0 && config.ENABLE_RAMDISK {
        global_metrics().inc_ramdisk_partition_resizes();
        let new_part_gb = config.RAMDISK_SIZE_GB / (remaining_groups as f64);
        log_event(
            LogLevel::Info,
            format!(
                "[Orchestrator] Worker-Group '{}' completed; resized RAMDISK partition for {} remaining Worker-Group(s) to {:.2} GB each (MAX_INPUT_STAGE_SIZE = {:.2} GB)",
                mount_state.mount_point,
                remaining_groups,
                new_part_gb,
                0.5 * new_part_gb
            ),
        );
    }

    if let Ok(mut map) = shared_ui.lock() {
        if let Some(w) = map.get_mut(&worker_key) {
            if is_shutdown_requested() {
                w.phase = "Stopped (Graceful Exit)".to_string();
            } else {
                w.phase = "Completed queue".to_string();
                w.queued_items_remaining = 0;
                w.ready_to_encode_count = 0;
            }
            w.active_files.clear();
            w.finished = true;
        }
    }
}

// --- Remote Machine Manager (Steps 1 - 7.9) ---
fn ssh_control_path(node: &WorkerNodeConfig) -> String {
    format!(
        "/tmp/rusty_h265_ssh_{}_{}_{}",
        node.USERNAME, node.IP_ADDRESS, node.SSH_PORT
    )
}

fn ssh_base_args(node: &WorkerNodeConfig) -> Vec<String> {
    let mut args = vec![
        "-n".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=4".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
    ];
    if !cfg!(target_os = "windows") {
        args.push("-o".to_string());
        args.push("ControlMaster=auto".to_string());
        args.push("-o".to_string());
        args.push(format!("ControlPath={}", ssh_control_path(node)));
        args.push("-o".to_string());
        args.push("ControlPersist=120".to_string());
    }
    args.push("-p".to_string());
    args.push(node.SSH_PORT.to_string());
    args.push(format!("{}@{}", node.USERNAME, node.IP_ADDRESS));
    args
}

fn wrap_remote_shell_cmd(node: &WorkerNodeConfig, inner_cmd: &str) -> String {
    if node.OS_TYPE.to_lowercase().contains("win") {
        inner_cmd.to_string()
    } else {
        // Append standard system/homebrew paths without overriding LD_LIBRARY_PATH
        // (injecting /opt/emby-server/lib into LD_LIBRARY_PATH overrides system glibc libm.so.6!).
        format!(
            "export PATH=\"$PATH:/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin:/opt/homebrew/opt/ffmpeg-full/bin\"; {}",
            inner_cmd
        )
    }
}

/// Runs a remote command over SSH (with retry on transient SSH exit code 255) and returns `(status_ok, combined_output)`.
fn run_ssh_command_raw(node: &WorkerNodeConfig, remote_cmd: &str) -> Result<(bool, String, String)> {
    let wrapped = wrap_remote_shell_cmd(node, remote_cmd);
    let remote_nodename = node.effective_nodename();
    log_event(
        LogLevel::Debug,
        format!(
            "[Remote {}@{}] SSH exec: {}",
            node.USERNAME, node.IP_ADDRESS, wrapped
        ),
    );

    let mut last_status_str = String::new();
    let mut last_stdout = String::new();
    let mut last_stderr = String::new();

    for attempt in 1..=3 {
        let mut cmd = Command::new("ssh");
        cmd.args(ssh_base_args(node));
        cmd.arg(&wrapped);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let rtt_start = Instant::now();
        let out = cmd.output().context("Failed to spawn ssh process")?;
        let rtt_secs = rtt_start.elapsed().as_secs_f64();
        global_metrics().record_ssh_rtt(&remote_nodename, &node.OS_TYPE, "ssh_exec", rtt_secs);

        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();

        if out.status.success() {
            return Ok((true, stdout, stderr));
        }

        last_status_str = out.status.to_string();
        last_stdout = stdout;
        last_stderr = stderr;

        // If ssh exited with 255 and produced no command output, retry after a brief pause
        if out.status.code() == Some(255)
            && last_stdout.trim().is_empty()
            && attempt < 3
        {
            global_metrics().record_ssh_retry(&remote_nodename, &node.OS_TYPE, "ssh_exec");
            let backoff_ms = 350 * attempt as u64;
            log_structured_event(
                LogLevel::Warn,
                &remote_nodename,
                &node.OS_TYPE,
                &format!("remote:{}@{}", node.USERNAME, node.IP_ADDRESS),
                "ssh_client",
                "-",
                None,
                "ssh_transient_retry",
                &[
                    ("attempt", attempt.to_string()),
                    ("exit_code", "255".to_string()),
                    ("backoff_ms", backoff_ms.to_string()),
                    ("stderr", last_stderr.trim().to_string()),
                ],
            );
            thread::sleep(Duration::from_millis(backoff_ms));
            continue;
        }
        break;
    }

    log_event(
        LogLevel::Warn,
        format!(
            "[Remote {}@{}] SSH command non-zero: `{}` -> {} (stdout: '{}', stderr: '{}')",
            node.USERNAME,
            node.IP_ADDRESS,
            wrapped,
            last_status_str,
            last_stdout.lines().next().unwrap_or("").trim(),
            last_stderr.trim()
        ),
    );
    Ok((false, last_stdout, last_stderr))
}

fn run_ssh_command(node: &WorkerNodeConfig, remote_cmd: &str) -> Result<String> {
    let (ok, stdout, stderr) = run_ssh_command_raw(node, remote_cmd)?;
    if !ok {
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            "ssh exited with non-zero status".to_string()
        };
        anyhow::bail!("{}", detail);
    }
    Ok(stdout)
}

pub fn format_filename_with_size(base_name: &str, size_bytes: u64) -> String {
    if size_bytes == 0 {
        return base_name.to_string();
    }
    let mb = (size_bytes as f64 / (1024.0 * 1024.0)).round() as u64;
    if mb >= 1 {
        format!("{} ({}MB)", base_name, mb)
    } else {
        let kb = (size_bytes / 1024).max(1);
        format!("{} ({}KB)", base_name, kb)
    }
}

fn query_remote_file_size_bytes(node: &WorkerNodeConfig, remote_path: &str) -> Option<u64> {
    let cmd = if node.OS_TYPE.to_lowercase().contains("win") {
        let win_p = remote_path.replace('/', "\\");
        format!(
            "powershell -NoProfile -Command \"if (Test-Path -LiteralPath '{}') {{ (Get-Item -LiteralPath '{}').Length }}\"",
            win_p, win_p
        )
    } else {
        format!(
            "stat -c%s \"{}\" 2>/dev/null || stat -f%z \"{}\" 2>/dev/null || wc -c < \"{}\" 2>/dev/null",
            remote_path, remote_path, remote_path
        )
    };
    if let Ok((true, stdout, _)) = run_ssh_command_raw(node, &cmd) {
        for line in stdout.lines() {
            if let Ok(sz) = line.trim().parse::<u64>() {
                if sz > 0 {
                    return Some(sz);
                }
            }
        }
    }
    None
}

/// Builds the OS-specific SSH cleanup command to evict both the `scp-up` input file
/// and any output file from the remote node's temporary directory.
pub fn build_remote_cleanup_command(
    node: &WorkerNodeConfig,
    remote_input_path: &str,
    remote_output_path: &str,
) -> String {
    if node.OS_TYPE.to_lowercase().contains("win") {
        format!(
            "powershell -NoProfile -Command \"Remove-Item -LiteralPath '{}','{}' -Force -ErrorAction SilentlyContinue\"",
            remote_input_path.replace('/', "\\"),
            remote_output_path.replace('/', "\\")
        )
    } else {
        format!(
            "rm -f \"{}\" \"{}\"",
            remote_input_path, remote_output_path
        )
    }
}

/// Evicts the `scp-up` input file (and any partial output stub) on the remote host when
/// `encode_handle` reports a failed conversion (`conversion_state == StageStatus::Failed`),
/// reclaims `remote_used_input_bytes`, and records the broken source file in `broken_files.journal`
/// and `failed_dirs.txt` without copying anything back.
#[allow(clippy::too_many_arguments)]
pub fn execute_remote_evict_failed_task<F>(
    mut task: DistributedTaskObject,
    inflight_ref: &Arc<Mutex<BTreeMap<u64, (Arc<MountQueueState>, DistributedTaskObject)>>>,
    in_pipe: &Arc<AtomicUsize>,
    used_ref: &Arc<AtomicU64>,
    max_size_bytes: u64,
    journal_ref: &Arc<TaskJournal>,
    trackers_ref: &Arc<Mutex<HashMap<PathBuf, SubdirTracker>>>,
    ui_ref: &Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    wkey: &str,
    node_cfg: &WorkerNodeConfig,
    cfg: &AppConfig,
    verbose: bool,
    remote_cleanup_fn: F,
) where
    F: FnOnce(&WorkerNodeConfig, &str),
{
    // If a concurrent graceful reset already removed and re-queued this task, skip
    let Some((mq, _)) = inflight_ref.lock().unwrap().remove(&task.task_id) else {
        return;
    };
    mq.set_in_flight(
        &task.queued_item,
        QueueEntryCategory::ActiveRemote,
        &format!("REMOTE {} [Evicting]", node_cfg.IP_ADDRESS),
    );

    let err_msg = task
        .error_occurred
        .clone()
        .unwrap_or_else(|| "Remote FFmpeg conversion failed".to_string());
    task.conversion_state = StageStatus::Failed;
    task.error_occurred = Some(err_msg.clone());
    journal_ref.upsert(task.clone());

    let fname = task
        .original_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("video");
    let display_fname = format_filename_with_size(fname, task.file_size_bytes);
    let remote_nodename = node_cfg.effective_nodename();
    let src_mb = (task.file_size_bytes as f64) / (1024.0 * 1024.0);
    let gb_div = 1024.0 * 1024.0 * 1024.0;
    log_structured_event(
        LogLevel::Warn,
        &remote_nodename,
        &node_cfg.OS_TYPE,
        wkey,
        "scp_copyback",
        &mq.mount_point,
        Some(task.task_id),
        "flush_start",
        &[
            ("flush_mode", "evict_failed".to_string()),
            ("source_file", task.original_path.display().to_string()),
            ("remote_file", task.remote_input_path.clone()),
            ("source_file_size_in_mb", format!("{:.2}", src_mb)),
            ("reason", err_msg.clone()),
        ],
    );
    update_ui_slot(
        ui_ref,
        wkey,
        2,
        &display_fname,
        "evict-remote",
        0,
        0.0,
        0.0,
        98.0,
        "Remote",
        "Evicting Input",
    );

    let evict_start = Instant::now();
    let rm_cmd = build_remote_cleanup_command(
        node_cfg,
        &task.remote_input_path,
        &task.remote_output_path,
    );
    remote_cleanup_fn(node_cfg, &rm_cmd);
    let evict_dur = evict_start.elapsed().as_secs_f64();

    remove_ui_slot(ui_ref, wkey, 2);
    mq.clear_in_flight(task.queued_item.item_id);

    used_ref.fetch_sub(
        task.file_size_bytes.min(used_ref.load(Ordering::Relaxed)),
        Ordering::Relaxed,
    );
    let cur_used = used_ref.load(Ordering::Relaxed);
    let quota_used_gb = (cur_used as f64) / gb_div;
    let quota_max_gb = (max_size_bytes as f64) / gb_div;
    let quota_rem_gb = (quota_max_gb - quota_used_gb).max(0.0);
    global_metrics().update_remote_quota(
        &remote_nodename,
        &node_cfg.OS_TYPE,
        cur_used,
        max_size_bytes,
    );
    log_structured_event(
        LogLevel::Warn,
        &remote_nodename,
        &node_cfg.OS_TYPE,
        wkey,
        "scp_copyback",
        &mq.mount_point,
        Some(task.task_id),
        "flush_end",
        &[
            ("flush_mode", "evict_failed".to_string()),
            ("source_file", task.original_path.display().to_string()),
            ("remote_file", task.remote_input_path.clone()),
            ("source_file_size_in_mb", format!("{:.2}", src_mb)),
            ("cleanup_duration_in_secs", format!("{:.3}", evict_dur)),
            ("quota_used_gb", format!("{:.2}", quota_used_gb)),
            ("quota_remaining_gb", format!("{:.2}", quota_rem_gb)),
            ("quota_max_gb", format!("{:.2}", quota_max_gb)),
            ("reason", err_msg.clone()),
        ],
    );
    if let Ok(mut map) = ui_ref.lock() {
        if let Some(w) = map.get_mut(wkey) {
            w.free_space_bytes = max_size_bytes.saturating_sub(cur_used);
        }
    }

    if !is_shutdown_requested() {
        record_broken_file_in_journal(&task.original_path, task.file_size_bytes, &err_msg);
        let fdirs_log = PathBuf::from(&cfg.FAILED_DIRS_PATH);
        let src_d = task
            .original_path
            .parent()
            .or(task.queued_item.source_subdir.as_deref())
            .unwrap_or(&task.queued_item.root_dir);
        let err_obj = anyhow::anyhow!("{}", err_msg);
        record_file_failure_if_untracked_subdir(
            &fdirs_log,
            &task.queued_item,
            src_d,
            Some(task.queued_item.dest_dir.as_path()),
            &format!(
                "[{}] Task {} failed for '{}' (evicted remote input '{}' & recorded in broken_files.journal; not re-queueing)",
                wkey,
                task.task_id,
                task.original_path.display(),
                task.remote_input_path
            ),
            &err_obj,
        );
        complete_queued_item(
            &task.queued_item,
            false,
            false,
            true,
            0,
            trackers_ref,
            ui_ref,
            wkey,
            cfg,
            verbose,
        );
    }

    in_pipe.fetch_sub(1, Ordering::Relaxed);
}

/// Parses an OpenSSH `scp` TTY progress frame such as:
/// `task_16_in.mp4  42%  128MB  84.5MB/s   00:04 ETA`
/// Returns `Some((percent, optional_speed_str, optional_kbps, optional_eta_or_total_secs, is_eta))`.
pub fn parse_scp_progress_line(line: &str) -> Option<(f64, Option<String>, Option<u64>, Option<f64>, bool)> {
    let mut pct_opt: Option<f64> = None;
    let mut speed_opt: Option<String> = None;
    let mut kbps_opt: Option<u64> = None;
    let mut time_opt: Option<f64> = None;
    let is_eta = line.contains("ETA");

    for tok in line.split_whitespace() {
        let clean = tok.trim_matches(|c: char| c == '\r' || c == '\n' || c == '\x08');
        if clean.is_empty() {
            continue;
        }
        if let Some(num_str) = clean.strip_suffix('%') {
            if let Ok(p) = num_str.parse::<f64>() {
                if (0.0..=100.0).contains(&p) {
                    pct_opt = Some(p);
                }
            }
        } else if clean.to_lowercase().ends_with("/s") && clean.len() > 2 {
            speed_opt = Some(clean.to_string());
            let rate_part = &clean[..clean.len() - 2];
            if let Some(bytes_per_sec) = parse_size_to_bytes(rate_part) {
                kbps_opt = Some((bytes_per_sec.saturating_mul(8)) / 1000);
            }
        } else if clean.contains(':') && clean.chars().all(|c| c.is_ascii_digit() || c == ':') {
            let parts: Vec<&str> = clean.split(':').collect();
            if parts.len() == 2 {
                if let (Ok(m), Ok(s)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                    time_opt = Some(m * 60.0 + s);
                }
            } else if parts.len() == 3 {
                if let (Ok(h), Ok(m), Ok(s)) = (
                    parts[0].parse::<f64>(),
                    parts[1].parse::<f64>(),
                    parts[2].parse::<f64>(),
                ) {
                    time_opt = Some(h * 3600.0 + m * 60.0 + s);
                }
            }
        }
    }

    pct_opt.map(|pct| (pct, speed_opt, kbps_opt, time_opt, is_eta))
}

#[allow(clippy::too_many_arguments)]
fn run_scp_with_live_tui_progress(
    node: &WorkerNodeConfig,
    src_arg: &str,
    dst_arg: &str,
    local_watch_file: Option<&Path>,
    expected_bytes: u64,
    shared_ui: &Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    worker_key: &str,
    slot_id: usize,
    display_fname: &str,
    codec_label: &str,
    default_direction: &str,
    status_label: &str,
    abort_on_shutdown: bool,
) -> Result<()> {
    if abort_on_shutdown && is_shutdown_requested() {
        anyhow::bail!("Aborted scp before start due to graceful shutdown");
    }

    let ctrl_opt = format!("ControlPath={}", ssh_control_path(node));
    let port_str = node.SSH_PORT.to_string();
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("script");
        c.args(["-q", "/dev/null", "scp"]);
        c
    } else {
        Command::new("scp")
    };

    cmd.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ]);
    if !cfg!(target_os = "windows") {
        cmd.args(["-o", "ControlMaster=auto", "-o", &ctrl_opt]);
    }
    cmd.args(["-P", &port_str, src_arg, dst_arg]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    platform::configure_child_process_group(&mut cmd);

    let mut child = cmd.spawn().context("Failed to spawn scp process")?;
    let child_pid = child.id();
    let finished_flag = Arc::new(AtomicBool::new(false));
    let killer_handle = if abort_on_shutdown {
        Some(spawn_shutdown_killer(child_pid, Arc::clone(&finished_flag)))
    } else {
        None
    };

    let stderr_pipe = child.stderr.take();
    let stderr_handle = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut ep) = stderr_pipe {
            use std::io::Read;
            let _ = ep.read_to_string(&mut buf);
        }
        buf
    });

    let start_instant = Instant::now();
    let mut captured_output = String::new();
    let fallback_kbps = ((expected_bytes / (1024 * 1024)).max(1)) * 8;
    let formatted_fname = format_filename_with_size(display_fname, expected_bytes);
    let remote_nodename = node.effective_nodename();
    let is_upload = codec_label == "scp-up";

    if let Some(mut out_pipe) = child.stdout.take() {
        use std::io::Read;
        let mut byte_buf = [0u8; 512];
        let mut line_buf = Vec::with_capacity(256);
        let mut last_ui_emit = Instant::now();

        loop {
            if abort_on_shutdown && is_shutdown_requested() {
                let _ = child.kill();
                break;
            }
            match out_pipe.read(&mut byte_buf) {
                Ok(0) => break,
                Ok(n) => {
                    for &b in &byte_buf[..n] {
                        if b == b'\r' || b == b'\n' {
                            if !line_buf.is_empty() {
                                let frame = String::from_utf8_lossy(&line_buf).to_string();
                                line_buf.clear();
                                if let Some((pct, speed_opt, kbps_opt, time_opt, is_eta)) =
                                    parse_scp_progress_line(&frame)
                                {
                                    let elapsed = start_instant.elapsed().as_secs_f64();
                                    let total_secs = match (time_opt, is_eta) {
                                        (Some(t), true) => elapsed + t,
                                        (Some(t), false) if pct >= 99.9 => t.max(elapsed),
                                        _ if pct > 1.0 => elapsed / (pct / 100.0),
                                        _ => 0.0,
                                    };
                                    let speed_str = speed_opt
                                        .unwrap_or_else(|| default_direction.to_string());
                                    let br_kbps = kbps_opt.unwrap_or(fallback_kbps);
                                    if let Some(kbps) = kbps_opt {
                                        let mb_per_s = (kbps as f64) / 8192.0;
                                        global_metrics().update_scp_live_throughput(
                                            &remote_nodename,
                                            &node.OS_TYPE,
                                            is_upload,
                                            mb_per_s,
                                        );
                                    }
                                    if pct >= 100.0
                                        || last_ui_emit.elapsed() >= Duration::from_millis(80)
                                    {
                                        update_ui_slot(
                                            shared_ui,
                                            worker_key,
                                            slot_id,
                                            &formatted_fname,
                                            codec_label,
                                            br_kbps,
                                            elapsed,
                                            total_secs,
                                            pct,
                                            &speed_str,
                                            status_label,
                                        );
                                        last_ui_emit = Instant::now();
                                    }
                                } else {
                                    let trimmed = frame.trim();
                                    if !trimmed.is_empty() && !trimmed.starts_with("^D") {
                                        captured_output.push_str(trimmed);
                                        captured_output.push('\n');
                                    }
                                }
                            }
                        } else {
                            line_buf.push(b);
                        }
                    }

                    // Fallback progress check via local file growth during copy-back
                    if let Some(watch_path) = local_watch_file {
                        if expected_bytes > 0
                            && last_ui_emit.elapsed() >= Duration::from_millis(120)
                        {
                            if let Ok(meta) = fs::metadata(watch_path) {
                                let cur_bytes = meta.len();
                                let pct = ((cur_bytes as f64 / expected_bytes as f64) * 100.0)
                                    .clamp(0.0, 99.9);
                                let elapsed = start_instant.elapsed().as_secs_f64().max(0.05);
                                let mb_per_s = (cur_bytes as f64 / (1024.0 * 1024.0)) / elapsed;
                                global_metrics().update_scp_live_throughput(
                                    &remote_nodename,
                                    &node.OS_TYPE,
                                    is_upload,
                                    mb_per_s,
                                );
                                let total_secs = if pct > 1.0 {
                                    elapsed / (pct / 100.0)
                                } else {
                                    0.0
                                };
                                let br_kbps = ((cur_bytes as f64 * 8.0) / (elapsed * 1000.0)) as u64;
                                update_ui_slot(
                                    shared_ui,
                                    worker_key,
                                    slot_id,
                                    &formatted_fname,
                                    codec_label,
                                    br_kbps.max(fallback_kbps),
                                    elapsed,
                                    total_secs,
                                    pct,
                                    &format!("{:.1}MB/s", mb_per_s),
                                    status_label,
                                );
                                last_ui_emit = Instant::now();
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }

    let status = child.wait().context("Failed waiting on scp process")?;
    let total_scp_dur = start_instant.elapsed().as_secs_f64();
    finished_flag.store(true, Ordering::Relaxed);
    if let Some(kh) = killer_handle {
        let _ = kh.join();
    }
    let stderr_text = stderr_handle.join().unwrap_or_default();
    if !status.success() {
        global_metrics().record_network_error(
            &remote_nodename,
            &node.OS_TYPE,
            if is_upload {
                "scp_upload_failed"
            } else {
                "scp_download_failed"
            },
        );
        global_metrics().record_scp_completed(
            &remote_nodename,
            &node.OS_TYPE,
            is_upload,
            0,
            total_scp_dur,
        );
        if abort_on_shutdown && is_shutdown_requested() {
            anyhow::bail!("Aborted scp upload due to graceful shutdown");
        }
        let detail = if !stderr_text.trim().is_empty() {
            stderr_text.trim().to_string()
        } else if !captured_output.trim().is_empty() {
            captured_output.trim().to_string()
        } else {
            format!("scp exited with status {}", status)
        };
        anyhow::bail!("{}", detail);
    }
    global_metrics().record_scp_completed(
        &remote_nodename,
        &node.OS_TYPE,
        is_upload,
        expected_bytes,
        total_scp_dur,
    );
    Ok(())
}

fn scp_upload_to_remote(
    node: &WorkerNodeConfig,
    local_src: &Path,
    remote_dst: &str,
    expected_bytes: u64,
    shared_ui: &Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    worker_key: &str,
    display_fname: &str,
) -> Result<()> {
    log_event(
        LogLevel::Info,
        format!(
            "[Remote {}@{}] SCP upload: '{}' -> '{}'",
            node.USERNAME,
            node.IP_ADDRESS,
            local_src.display(),
            remote_dst
        ),
    );
    let src_str = local_src.to_string_lossy().to_string();
    let dst_str = format!("{}@{}:{}", node.USERNAME, node.IP_ADDRESS, remote_dst);
    run_scp_with_live_tui_progress(
        node,
        &src_str,
        &dst_str,
        None,
        expected_bytes,
        shared_ui,
        worker_key,
        0,
        display_fname,
        "scp-up",
        "LAN->Remote",
        "Uploading",
        true, // abort_on_shutdown: immediately stop in-flight scp-up on quit
    )
    .context("scp upload failed")
}

fn scp_download_from_remote(
    node: &WorkerNodeConfig,
    remote_src: &str,
    local_dst: &Path,
    expected_bytes: u64,
    shared_ui: &Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    worker_key: &str,
    display_fname: &str,
) -> Result<()> {
    if let Some(parent) = local_dst.parent() {
        ensure_dir_created(parent)?;
    }
    log_event(
        LogLevel::Info,
        format!(
            "[Remote {}@{}] SCP copy-back: '{}' -> '{}'",
            node.USERNAME,
            node.IP_ADDRESS,
            remote_src,
            local_dst.display()
        ),
    );
    let src_str = format!("{}@{}:{}", node.USERNAME, node.IP_ADDRESS, remote_src);
    let dst_str = local_dst.to_string_lossy().to_string();
    run_scp_with_live_tui_progress(
        node,
        &src_str,
        &dst_str,
        Some(local_dst),
        expected_bytes,
        shared_ui,
        worker_key,
        2,
        display_fname,
        "scp-down",
        "Remote->LAN",
        "Copy-Back",
        false, // abort_on_shutdown = false: ALWAYS allow scp-down to complete on quit!
    )
    .context("scp copy-back failed")
}

/// Picks a verified media item from `mount_queues`, prioritizing mounts with the fewest active
/// remote managers, while enforcing `max_single_file_bytes` (50% of the remote node's `MAX_SIZE_IN_GB`).
/// Any file larger than `max_single_file_bytes` is left in `mq.queue` for the local worker to convert.
fn pick_mount_queue_prioritizing_unoccupied(
    mount_queues: &[Arc<MountQueueState>],
    rng_counter: &AtomicU64,
    max_single_file_bytes: u64,
) -> Option<(Arc<MountQueueState>, QueuedMediaItem)> {
    let mut candidates: Vec<(usize, usize)> = Vec::new();
    for (idx, mq) in mount_queues.iter().enumerate() {
        let has_fitting_item = mq
            .queue
            .lock()
            .unwrap()
            .iter()
            .any(|it| it.file_size_bytes <= max_single_file_bytes);
        if has_fitting_item {
            let active = mq.active_remote_managers.load(Ordering::Relaxed);
            candidates.push((active, idx));
        }
    }

    if candidates.is_empty() {
        return None;
    }

    let min_active = candidates.iter().map(|(a, _)| *a).min().unwrap_or(0);
    let best_indices: Vec<usize> = candidates
        .into_iter()
        .filter(|(a, _)| *a == min_active)
        .map(|(_, idx)| idx)
        .collect();

    let seed = rng_counter
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1);
    let chosen_pos = (seed as usize) % best_indices.len();
    let chosen_mq = &mount_queues[best_indices[chosen_pos]];

    let mut q = chosen_mq.queue.lock().unwrap();
    let pos = q
        .iter()
        .position(|it| it.file_size_bytes <= max_single_file_bytes)?;
    let item = q.remove(pos)?;
    drop(q);

    Some((Arc::clone(chosen_mq), item))
}

fn stop_remote_manager_with_persistent_error(
    shared_ui: &Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    worker_key: &str,
    phase: &str,
    err_reason: &str,
) {
    let bt = Backtrace::force_capture().to_string();
    log_error_with_bt(
        &format!("[{}] {}: {}", worker_key, phase, err_reason),
        &bt,
    );
    if let Ok(mut map) = shared_ui.lock() {
        if let Some(w) = map.get_mut(worker_key) {
            w.phase = phase.to_string();
            w.persistent_error = Some(err_reason.to_string());
            w.active_files.clear();
            w.finished = true;
        }
    }
}

/// Candidate H.265 encoders ordered by speed preference for a given OS.
pub fn candidate_remote_hevc_encoders(
    encoders_output: &str,
    os_type: &str,
) -> Vec<(String, Vec<String>)> {
    let mut candidates = Vec::new();
    let os_lower = os_type.to_lowercase();

    if os_lower.contains("mac") && encoders_output.contains("hevc_videotoolbox") {
        candidates.push((
            "hevc_videotoolbox".to_string(),
            vec![
                "-c:v".to_string(),
                "hevc_videotoolbox".to_string(),
                "-q:v".to_string(),
                "50".to_string(),
                "-prio_speed".to_string(),
                "1".to_string(),
                "-tag:v".to_string(),
                "hvc1".to_string(),
            ],
        ));
    }
    if encoders_output.contains("hevc_nvenc") {
        candidates.push((
            "hevc_nvenc".to_string(),
            vec![
                "-c:v".to_string(),
                "hevc_nvenc".to_string(),
                "-preset".to_string(),
                "p4".to_string(),
                "-cq".to_string(),
                "28".to_string(),
            ],
        ));
    }
    if encoders_output.contains("hevc_qsv") {
        candidates.push((
            "hevc_qsv".to_string(),
            vec![
                "-c:v".to_string(),
                "hevc_qsv".to_string(),
                "-global_quality".to_string(),
                "28".to_string(),
            ],
        ));
    }
    if encoders_output.contains("hevc_amf") {
        candidates.push((
            "hevc_amf".to_string(),
            vec!["-c:v".to_string(), "hevc_amf".to_string()],
        ));
    }
    if encoders_output.contains("libx265") {
        candidates.push((
            "libx265".to_string(),
            vec![
                "-c:v".to_string(),
                "libx265".to_string(),
                "-crf".to_string(),
                "28".to_string(),
                "-preset".to_string(),
                "veryfast".to_string(),
            ],
        ));
    }
    candidates
}

fn check_remote_ffmpeg_works(node: &WorkerNodeConfig, candidate_bin: &str) -> bool {
    if candidate_bin.trim().is_empty() {
        return false;
    }
    if let Ok((ok, stdout, stderr)) =
        run_ssh_command_raw(node, &format!("\"{}\" -version", candidate_bin))
    {
        let combined = format!("{}\n{}", stdout, stderr).to_lowercase();
        // Emby's custom FFmpeg prints "ffmpeg version ... EXIT" and may exit with 255 on -version;
        // accept it if it outputs "ffmpeg version" or "libavcodec".
        ok || combined.contains("ffmpeg version") || combined.contains("libavcodec")
    } else {
        false
    }
}

fn extract_found_nvenc_api_version(stderr: &str) -> Option<String> {
    let idx = stderr.find("Found:")?;
    let after = stderr[idx + "Found:".len()..].trim();
    let ver: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if !ver.is_empty() && ver.contains('.') {
        Some(ver)
    } else {
        None
    }
}

fn attempt_remote_nvenc_header_downgrade_rebuild(
    node: &WorkerNodeConfig,
    ffmpeg_bin: &str,
    api_version: &str,
    shared_ui: &Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    worker_key: &str,
) -> bool {
    let Some(ffmpeg_dir) = Path::new(ffmpeg_bin).parent().and_then(|p| p.to_str()) else {
        return false;
    };
    if !ffmpeg_dir.contains("ffmpeg") {
        return false;
    }

    if let Ok(mut map) = shared_ui.lock() {
        if let Some(w) = map.get_mut(worker_key) {
            w.phase = format!(
                "Step 4: Rebuilding remote FFmpeg for NVENC API {} (user-space, Emby-safe)...",
                api_version
            );
        }
    }

    log_event(
        LogLevel::Info,
        format!(
            "[Remote {}@{}] Detected NVENC API mismatch (Driver supports {}). Rebuilding '{}' against nv-codec-headers sdk/{} in $HOME/nvcodec_sdk without touching system drivers or Emby...",
            node.USERNAME, node.IP_ADDRESS, api_version, ffmpeg_bin, api_version
        ),
    );

    let rebuild_script = format!(
        "set -e; \
         mkdir -p \"$HOME/nvcodec_sdk\"; \
         cd /tmp; \
         if [ ! -d nv-codec-headers ]; then git clone https://git.videolan.org/git/ffmpeg/nv-codec-headers.git; fi; \
         cd nv-codec-headers; \
         git fetch --all --tags >/dev/null 2>&1 || true; \
         git checkout \"sdk/{api}\" >/dev/null 2>&1; \
         make PREFIX=\"$HOME/nvcodec_sdk\" install >/dev/null 2>&1; \
         cd \"{ffdir}\"; \
         export PKG_CONFIG_PATH=\"$HOME/nvcodec_sdk/lib/pkgconfig:$PKG_CONFIG_PATH\"; \
         ./configure --enable-nonfree --enable-gpl --enable-libx265 --enable-nvenc --enable-nvdec \
           --extra-cflags=\"-I$HOME/nvcodec_sdk/include -I/usr/local/include\" \
           --extra-ldflags=\"-L$HOME/nvcodec_sdk/lib -L/usr/local/lib\" >/dev/null 2>&1; \
         make -j$(nproc) >/dev/null 2>&1",
        api = api_version,
        ffdir = ffmpeg_dir
    );

    match run_ssh_command_raw(node, &rebuild_script) {
        Ok((true, _, _)) => {
            log_event(
                LogLevel::Info,
                format!(
                    "[Remote {}@{}] Successfully rebuilt '{}' against NVENC SDK {}.",
                    node.USERNAME, node.IP_ADDRESS, ffmpeg_bin, api_version
                ),
            );
            true
        }
        Ok((false, stdout, stderr)) => {
            log_event(
                LogLevel::Warn,
                format!(
                    "[Remote {}@{}] Automatic NVENC SDK {} rebuild did not complete (stdout: '{}', stderr: '{}').",
                    node.USERNAME,
                    node.IP_ADDRESS,
                    api_version,
                    stdout.trim(),
                    stderr.trim()
                ),
            );
            false
        }
        Err(e) => {
            log_event(
                LogLevel::Warn,
                format!(
                    "[Remote {}@{}] Automatic NVENC SDK {} rebuild error: {}",
                    node.USERNAME, node.IP_ADDRESS, api_version, e
                ),
            );
            false
        }
    }
}

fn format_remote_ffmpeg_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.contains(' ')
                || a.contains('(')
                || a.contains(')')
                || a.contains(',')
                || a.contains('\'')
                || a.contains('\\')
            {
                format!("\"{}\"", a.replace('"', "\\\""))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn verify_working_remote_encoder(
    node: &WorkerNodeConfig,
    config: &AppConfig,
    target: &codecs::ValidatedTargetConfig,
    ffmpeg_bin: &str,
    encoders_output: &str,
    shared_ui: Option<&Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>>,
    worker_key: &str,
    spinner: Option<&codecs::DiscoverySpinner>,
) -> Option<(String, Vec<String>, String, Vec<String>)> {
    let video_candidates = codecs::rank_discovered_video_candidates(
        encoders_output,
        &node.OS_TYPE,
        &node.GPU_PROFILE,
        target,
        config,
    );
    let audio_candidates =
        codecs::rank_discovered_audio_candidates(encoders_output, &node.OS_TYPE, target, config);

    if video_candidates.is_empty() || audio_candidates.is_empty() {
        return None;
    }

    let is_windows = node.OS_TYPE.to_lowercase().contains("win");
    let muxer_args = target.container.muxer_args();
    let muxer_str = format_remote_ffmpeg_args(&muxer_args);
    let remote_probe_file = if is_windows {
        format!(
            "C:/Temp/rusty_h265_preflight_{}{}",
            std::process::id(),
            target.container.extension()
        )
    } else {
        format!(
            "/tmp/rusty_h265_preflight_{}{}",
            std::process::id(),
            target.container.extension()
        )
    };
    let audio_input_str = if target.audio != codecs::TargetAudioCodec::None {
        format!(
            "-f lavfi -i anullsrc=r=48000:cl=stereo:d={}",
            codecs::PREFLIGHT_CLIP_DURATION_SECS
        )
    } else {
        String::new()
    };

    if is_windows {
        let _ = run_ssh_command_raw(
            node,
            "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path 'C:/Temp' | Out-Null\"",
        );
    }

    // Note: NVIDIA NVENC (including GTX/RTX 3050 Ti) requires minimum frame dimensions >= 144x144 (HEVC)
    // and >= 240x128 (AV1). Using 5-second 256x144 (16:9) satisfies all hardware encoders.
    for (enc_name, enc_args) in &video_candidates {
        for (aud_name, aud_args) in &audio_candidates {
            if let Some(sp) = spinner {
                sp.set_status(format!(
                    "Testing remote {}@{}: {} + {} (5s {}x{} encode + decode check)...",
                    node.USERNAME,
                    node.IP_ADDRESS,
                    enc_name,
                    aud_name,
                    codecs::PREFLIGHT_CLIP_WIDTH,
                    codecs::PREFLIGHT_CLIP_HEIGHT
                ));
            }

            let enc_str = format_remote_ffmpeg_args(enc_args);
            let probe_aud_str = if target.audio == codecs::TargetAudioCodec::Copy {
                match target.container {
                    codecs::TargetContainer::Webm => "-c:a libopus -b:a 64k".to_string(),
                    _ => "-c:a aac -b:a 64k".to_string(),
                }
            } else {
                format_remote_ffmpeg_args(aud_args)
            };

            let encode_cmd = format!(
                "\"{}\" -nostdin -hide_banner -loglevel error -f lavfi -i color=c=black:s={}x{}:d={}:r={} {} {} {} {} -y \"{}\"",
                ffmpeg_bin,
                codecs::PREFLIGHT_CLIP_WIDTH,
                codecs::PREFLIGHT_CLIP_HEIGHT,
                codecs::PREFLIGHT_CLIP_DURATION_SECS,
                codecs::PREFLIGHT_CLIP_FPS,
                audio_input_str,
                enc_str,
                probe_aud_str,
                muxer_str,
                remote_probe_file
            );
            let decode_cmd = format!(
                "\"{}\" -nostdin -xerror -err_detect explode -v error -i \"{}\" -f null -",
                ffmpeg_bin, remote_probe_file
            );
            let cleanup_cmd = if is_windows {
                format!(
                    "powershell -NoProfile -Command \"Remove-Item -Path '{}' -Force -ErrorAction SilentlyContinue\"",
                    remote_probe_file
                )
            } else {
                format!("rm -f \"{}\"", remote_probe_file)
            };

            let run_encode_and_decode = || -> std::result::Result<(), String> {
                match run_ssh_command_raw(node, &encode_cmd) {
                    Ok((true, _, _)) => {}
                    Ok((false, _, stderr)) => {
                        let _ = run_ssh_command_raw(node, &cleanup_cmd);
                        return Err(stderr.trim().to_string());
                    }
                    Err(e) => {
                        let _ = run_ssh_command_raw(node, &cleanup_cmd);
                        return Err(e.to_string());
                    }
                }
                let dec_res = run_ssh_command_raw(node, &decode_cmd);
                let _ = run_ssh_command_raw(node, &cleanup_cmd);
                match dec_res {
                    Ok((true, _, _)) => Ok(()),
                    Ok((false, _, stderr)) => Err(format!("decode check failed: {}", stderr.trim())),
                    Err(e) => Err(format!("decode check ssh error: {}", e)),
                }
            };

            match run_encode_and_decode() {
                Ok(()) => {
                    log_event(
                        LogLevel::Info,
                        format!(
                            "[Remote {}@{}] Verified working encoder '{} + {}' for [{}] via 5s {}x{} encode + decode check.",
                            node.USERNAME,
                            node.IP_ADDRESS,
                            enc_name,
                            aud_name,
                            target.label(),
                            codecs::PREFLIGHT_CLIP_WIDTH,
                            codecs::PREFLIGHT_CLIP_HEIGHT
                        ),
                    );
                    return Some((
                        enc_name.clone(),
                        enc_args.clone(),
                        aud_name.clone(),
                        aud_args.clone(),
                    ));
                }
                Err(stderr) => {
                    if enc_name.ends_with("_nvenc") {
                        if let Some(ui_arc) = shared_ui {
                            if let Some(found_api) = extract_found_nvenc_api_version(&stderr) {
                                if attempt_remote_nvenc_header_downgrade_rebuild(
                                    node,
                                    ffmpeg_bin,
                                    &found_api,
                                    ui_arc,
                                    worker_key,
                                ) && run_encode_and_decode().is_ok()
                                {
                                    log_event(
                                        LogLevel::Info,
                                        format!(
                                            "[Remote {}@{}] Verified working encoder '{}' after NVENC SDK {} rebuild!",
                                            node.USERNAME, node.IP_ADDRESS, enc_name, found_api
                                        ),
                                    );
                                    return Some((
                                        enc_name.clone(),
                                        enc_args.clone(),
                                        aud_name.clone(),
                                        aud_args.clone(),
                                    ));
                                }
                            }
                        }
                    }
                    log_event(
                        LogLevel::Warn,
                        format!(
                            "[Remote {}@{}] Candidate '{} + {}' failed 5s 256x144 pre-flight (err: '{}'); trying next candidate...",
                            node.USERNAME,
                            node.IP_ADDRESS,
                            enc_name,
                            aud_name,
                            stderr
                        ),
                    );
                }
            }
        }
    }

    None
}

fn probe_remote_node_for_dry_run(
    node: &WorkerNodeConfig,
    config: &AppConfig,
    target: &codecs::ValidatedTargetConfig,
    spinner: &codecs::DiscoverySpinner,
) -> Result<(String, String, String)> {
    let label = format!("{}@{}", node.USERNAME, node.IP_ADDRESS);
    spinner.set_status(format!("Checking SSH connectivity to {}...", label));
    run_ssh_command(node, "echo SSH_OK")
        .with_context(|| format!("SSH check failed for {}:{}", label, node.SSH_PORT))?;

    spinner.set_status(format!("Resolving FFmpeg binary on {}...", label));
    let mut resolved_ffmpeg = String::new();
    if !node.FFMPEG_PATH.is_empty() && check_remote_ffmpeg_works(node, &node.FFMPEG_PATH) {
        resolved_ffmpeg = node.FFMPEG_PATH.clone();
    }
    if resolved_ffmpeg.is_empty() {
        if let Ok((_ok, which_out, _)) =
            run_ssh_command_raw(node, "command -v ffmpeg || which ffmpeg")
        {
            let detected = which_out.lines().next().unwrap_or("").trim().to_string();
            if !detected.is_empty() && check_remote_ffmpeg_works(node, &detected) {
                resolved_ffmpeg = detected;
            }
        }
    }
    if resolved_ffmpeg.is_empty() && check_remote_ffmpeg_works(node, "ffmpeg") {
        resolved_ffmpeg = "ffmpeg".to_string();
    }
    if resolved_ffmpeg.is_empty() {
        anyhow::bail!(
            "FFmpeg not found in remote PATH or FFMPEG_PATH ('{}') on {}",
            node.FFMPEG_PATH,
            label
        );
    }

    spinner.set_status(format!("Querying ffmpeg -encoders on {}...", label));
    let encoders_out = match run_ssh_command_raw(
        node,
        &format!("\"{}\" -hide_banner -encoders", resolved_ffmpeg),
    )? {
        (_ok, stdout, stderr) if !stdout.trim().is_empty() || !stderr.trim().is_empty() => {
            format!("{}\n{}", stdout, stderr)
        }
        _ => anyhow::bail!("Empty output from '{} -encoders' on {}", resolved_ffmpeg, label),
    };

    let worker_key = format!("remote:{}", label);
    match verify_working_remote_encoder(
        node,
        config,
        target,
        &resolved_ffmpeg,
        &encoders_out,
        None,
        &worker_key,
        Some(spinner),
    ) {
        Some((v_enc, _v_args, a_enc, _a_args)) => Ok((resolved_ffmpeg, v_enc, a_enc)),
        None => anyhow::bail!(
            "No working remote encoder passed 5s 256x144 pre-flight for [{}] on {}",
            target.label(),
            label
        ),
    }
}

fn deploy_and_spawn_remote_metrics_agent(
    node: &WorkerNodeConfig,
    config: &AppConfig,
    remote_root_dir: &str,
    worker_key: &str,
    resolved_ffmpeg: &str,
    selected_encoder: &str,
) -> (Option<thread::JoinHandle<()>>, Arc<AtomicBool>) {
    let stop_flag = Arc::new(AtomicBool::new(false));
    if !config.DEPLOY_REMOTE_METRICS_AGENT {
        return (None, stop_flag);
    }
    let local_script = resolve_config_path(Path::new(&config.METRICS_AGENT_SCRIPT_PATH));
    if !local_script.exists() {
        log_event(
            LogLevel::Warn,
            format!(
                "[{}] Remote metrics agent script not found at '{}'; skipping remote agent deployment.",
                worker_key,
                local_script.display()
            ),
        );
        return (None, stop_flag);
    }

    let remote_script_path = format!("{}/rusty_h265_metrics_agent.py", remote_root_dir);
    let ctrl_opt = format!("ControlPath={}", ssh_control_path(node));
    let port_str = node.SSH_PORT.to_string();
    let dst_arg = format!("{}@{}:{}", node.USERNAME, node.IP_ADDRESS, remote_script_path);
    let mut scp_cmd = Command::new("scp");
    scp_cmd.args([
        "-q",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ]);
    if !cfg!(target_os = "windows") {
        scp_cmd.args(["-o", "ControlMaster=auto", "-o", &ctrl_opt]);
    }
    let scp_status = scp_cmd
        .args([
            "-P",
            &port_str,
            &local_script.to_string_lossy(),
            &dst_arg,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if !scp_status.map(|s| s.success()).unwrap_or(false) {
        log_event(
            LogLevel::Warn,
            format!(
                "[{}] Could not scp '{}' to '{}'; remote syscall telemetry will be skipped.",
                worker_key,
                local_script.display(),
                dst_arg
            ),
        );
        return (None, stop_flag);
    }

    let node_cfg = node.clone();
    let remote_nodename = node.effective_nodename();
    let wkey = worker_key.to_string();
    let r_root = remote_root_dir.to_string();
    let r_script = remote_script_path;
    let ff_bin = resolved_ffmpeg.to_string();
    let enc_name = selected_encoder.to_string();
    let stop_clone = Arc::clone(&stop_flag);

    let handle = thread::spawn(move || {
        let is_win = node_cfg.OS_TYPE.to_lowercase().contains("win");
        let raw_agent_cmd = if is_win {
            format!(
                "python \"{}\" --role remote --nodename \"{}\" --host-os \"{}\" --watch-dir \"{}\" --interval 5",
                r_script.replace('/', "\\"),
                remote_nodename,
                node_cfg.OS_TYPE,
                r_root.replace('/', "\\")
            )
        } else {
            format!(
                "(command -v python3 >/dev/null 2>&1 && python3 \"{s}\" --role remote --nodename \"{n}\" --host-os \"{o}\" --watch-dir \"{w}\" --interval 5) || python \"{s}\" --role remote --nodename \"{n}\" --host-os \"{o}\" --watch-dir \"{w}\" --interval 5",
                s = r_script,
                n = remote_nodename,
                o = node_cfg.OS_TYPE,
                w = r_root
            )
        };
        let wrapped = wrap_remote_shell_cmd(&node_cfg, &raw_agent_cmd);
        let mut cmd = Command::new("ssh");
        cmd.args(ssh_base_args(&node_cfg));
        cmd.arg(&wrapped);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        platform::configure_child_process_group(&mut cmd);

        let Ok(mut child) = cmd.spawn() else {
            return;
        };
        let child_pid = child.id();
        let finished_flag = Arc::new(AtomicBool::new(false));
        let killer_flag = Arc::clone(&finished_flag);
        let stop_for_killer = Arc::clone(&stop_clone);
        let killer_thread = thread::spawn(move || {
            while !killer_flag.load(Ordering::Relaxed) {
                if is_shutdown_requested() || stop_for_killer.load(Ordering::Relaxed) {
                    platform::terminate_process_tree(child_pid, false);
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        });

        if let Some(out_pipe) = child.stdout.take() {
            let reader = BufReader::new(out_pipe);
            let mut logged_inventory = false;
            for line in reader.lines().map_while(|r| r.ok()) {
                if is_shutdown_requested() || stop_clone.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    break;
                }
                let trimmed = line.trim();
                if !trimmed.starts_with('{') {
                    continue;
                }
                if let Ok(mut sample) = serde_json::from_str::<HostHardwareSample>(trimmed) {
                    sample.nodename = remote_nodename.clone();
                    sample.host_os = node_cfg.OS_TYPE.clone();
                    if !logged_inventory {
                        logged_inventory = true;
                        log_structured_event(
                            LogLevel::Info,
                            &remote_nodename,
                            &node_cfg.OS_TYPE,
                            &wkey,
                            "metrics_agent",
                            "-",
                            None,
                            "node_inventory",
                            &[
                                ("memory_total_gb", format!("{:.2}", sample.memory_total_gb)),
                                (
                                    "watch_dir_free_gb",
                                    format!("{:.2}", sample.watch_dir_free_gb),
                                ),
                                ("gpu_name", sample.gpu_name.clone()),
                                (
                                    "gpu_memory_total_in_mb",
                                    format!("{:.0}", sample.gpu_memory_total_in_mb),
                                ),
                                ("ffmpeg_path", ff_bin.clone()),
                                ("selected_encoder", enc_name.clone()),
                            ],
                        );
                    }
                    global_metrics().update_remote_hardware_sample(sample);
                }
            }
        }
        let _ = child.wait();
        finished_flag.store(true, Ordering::Relaxed);
        let _ = killer_thread.join();
    });

    (Some(handle), stop_flag)
}

#[allow(clippy::too_many_arguments)]
fn run_remote_machine_manager(
    node: WorkerNodeConfig,
    mount_queues: Vec<Arc<MountQueueState>>,
    subdir_trackers: Arc<Mutex<HashMap<PathBuf, SubdirTracker>>>,
    journal: Arc<TaskJournal>,
    next_task_id: Arc<AtomicU64>,
    shared_ui: Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>>,
    config: AppConfig,
    verbose: bool,
) {
    let worker_key = format!("remote:{}@{}", node.USERNAME, node.IP_ADDRESS);
    let remote_nodename = node.effective_nodename();
    log_event(
        LogLevel::Info,
        format!(
            "[{}] Starting Remote Machine Manager (nodename: '{}', OS: {}, SSH port: {}, Configured FFmpeg: '{}')",
            worker_key, remote_nodename, node.OS_TYPE, node.SSH_PORT, node.FFMPEG_PATH
        ),
    );

    // Step 1 & 2: Check if remote machine is SSH-able
    if let Ok(mut map) = shared_ui.lock() {
        if let Some(w) = map.get_mut(&worker_key) {
            w.phase = "Step 1: Checking passwordless SSH connectivity...".to_string();
        }
    }

    if let Err(ssh_err) = run_ssh_command(&node, "echo SSH_OK") {
        stop_remote_manager_with_persistent_error(
            &shared_ui,
            &worker_key,
            "STOPPED (SSH Unreachable)",
            &format!(
                "SSH check failed for {}@{}:{}: {}",
                node.USERNAME, node.IP_ADDRESS, node.SSH_PORT, ssh_err
            ),
        );
        return;
    }

    // Step 3: Check if ffmpeg is available in $PATH, else check FFMPEG_PATH from config.yaml
    if let Ok(mut map) = shared_ui.lock() {
        if let Some(w) = map.get_mut(&worker_key) {
            w.phase = "Step 3: Verifying remote FFmpeg binary...".to_string();
        }
    }

    let mut resolved_ffmpeg = String::new();
    if !node.FFMPEG_PATH.is_empty() && check_remote_ffmpeg_works(&node, &node.FFMPEG_PATH) {
        resolved_ffmpeg = node.FFMPEG_PATH.clone();
    }
    if resolved_ffmpeg.is_empty() {
        if let Ok((_ok, which_out, _)) =
            run_ssh_command_raw(&node, "command -v ffmpeg || which ffmpeg")
        {
            let detected = which_out.lines().next().unwrap_or("").trim().to_string();
            if !detected.is_empty() && check_remote_ffmpeg_works(&node, &detected) {
                resolved_ffmpeg = detected;
            }
        }
    }
    if resolved_ffmpeg.is_empty() && check_remote_ffmpeg_works(&node, "ffmpeg") {
        resolved_ffmpeg = "ffmpeg".to_string();
    }

    if resolved_ffmpeg.is_empty() {
        stop_remote_manager_with_persistent_error(
            &shared_ui,
            &worker_key,
            "STOPPED (FFmpeg Not Found)",
            &format!(
                "FFmpeg not found in remote $PATH or configured FFMPEG_PATH ('{}') on {}",
                node.FFMPEG_PATH, node.IP_ADDRESS
            ),
        );
        return;
    }

    log_event(
        LogLevel::Info,
        format!(
            "[{}] Resolved remote FFmpeg binary to '{}'",
            worker_key, resolved_ffmpeg
        ),
    );

    // Step 4 & 5: Check if FFmpeg on remote machine has a working H.265 / HEVC codec
    if let Ok(mut map) = shared_ui.lock() {
        if let Some(w) = map.get_mut(&worker_key) {
            w.phase = "Step 4: Verifying remote H.265 / HEVC encoder...".to_string();
        }
    }

    let encoders_out = match run_ssh_command_raw(
        &node,
        &format!("\"{}\" -hide_banner -encoders", resolved_ffmpeg),
    ) {
        Ok((_ok, stdout, stderr)) if !stdout.trim().is_empty() || !stderr.trim().is_empty() => {
            format!("{}\n{}", stdout, stderr)
        }
        Ok(_) => {
            stop_remote_manager_with_persistent_error(
                &shared_ui,
                &worker_key,
                "STOPPED (FFmpeg Query Failed)",
                &format!(
                    "Empty output querying encoders from '{}' on {}",
                    resolved_ffmpeg, node.IP_ADDRESS
                ),
            );
            return;
        }
        Err(e) => {
            stop_remote_manager_with_persistent_error(
                &shared_ui,
                &worker_key,
                "STOPPED (FFmpeg Query Failed)",
                &format!("Failed querying encoders on {}: {}", node.IP_ADDRESS, e),
            );
            return;
        }
    };

    let remote_target = codecs::validate_config_targets(&config).unwrap_or(
        codecs::ValidatedTargetConfig {
            video: codecs::TargetVideoCodec::Hevc,
            audio: codecs::TargetAudioCodec::Aac,
            container: codecs::TargetContainer::Mp4,
        },
    );

    let (remote_encoder_name, remote_encoder_args, _remote_audio_name, remote_audio_args) =
        match verify_working_remote_encoder(
            &node,
            &config,
            &remote_target,
            &resolved_ffmpeg,
            &encoders_out,
            Some(&shared_ui),
            &worker_key,
            None,
        ) {
            Some(enc) => enc,
            None => {
                stop_remote_manager_with_persistent_error(
                    &shared_ui,
                    &worker_key,
                    &format!("STOPPED (No Working {} Encoder)", remote_target.video.as_str().to_uppercase()),
                    &format!(
                        "No working encoder for [{}] found in '{}' on {}",
                        remote_target.label(),
                        resolved_ffmpeg,
                        node.IP_ADDRESS
                    ),
                );
                return;
            }
        };

    // Step 6: Create temporary root directory on the remote machine (cleaning up any leftover temp dirs first)
    let is_windows = node.OS_TYPE.to_lowercase().contains("win");
    let remote_root_dir = if is_windows {
        let _ = run_ssh_command_raw(
            &node,
            "powershell -NoProfile -Command \"Remove-Item -Path 'C:\\Temp\\rusty_h265_remote_*' -Recurse -Force -ErrorAction SilentlyContinue\"",
        );
        format!("C:/Temp/rusty_h265_remote_{}", std::process::id())
    } else {
        let _ = run_ssh_command_raw(
            &node,
            "rm -rf /tmp/rusty_h265_remote_* \"$HOME\"/.rusty_h265_remote_* 2>/dev/null || true",
        );
        format!("/tmp/rusty_h265_remote_{}", std::process::id())
    };

    let mkdir_cmd = if is_windows {
        format!(
            "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '{}' | Out-Null\"",
            remote_root_dir
        )
    } else {
        format!("mkdir -p \"{}\"", remote_root_dir)
    };

    if let Err(e) = run_ssh_command(&node, &mkdir_cmd) {
        stop_remote_manager_with_persistent_error(
            &shared_ui,
            &worker_key,
            "STOPPED (Temp Dir Error)",
            &format!(
                "Failed creating remote root dir '{}' on {}: {}",
                remote_root_dir, node.IP_ADDRESS, e
            ),
        );
        return;
    }

    // Step 6b: Deploy & spawn remote rusty_h265_metrics_agent.py companion via SCP/SSH
    let (remote_agent_handle, remote_agent_stop) = deploy_and_spawn_remote_metrics_agent(
        &node,
        &config,
        &remote_root_dir,
        &worker_key,
        &resolved_ffmpeg,
        &remote_encoder_name,
    );

    let max_size_bytes = (node.MAX_SIZE_IN_GB.max(1.0) * 1024.0 * 1024.0 * 1024.0).round() as u64;
    // 50% size-cap: Max allowable single file size for a remote manager cannot exceed 50% of MAX_SIZE_IN_GB
    // so there is always sufficient headroom in the remote temp folder for both the input and encoded output.
    let max_single_file_bytes = max_size_bytes / 2;
    let remote_used_input_bytes = Arc::new(AtomicU64::new(0));
    global_metrics().update_remote_quota(&remote_nodename, &node.OS_TYPE, 0, max_size_bytes);

    if let Ok(mut map) = shared_ui.lock() {
        if let Some(w) = map.get_mut(&worker_key) {
            w.header_title = format!(
                " Remote Node: {}@{}:{} [{} | {} | Max: {:.1} GB (File Cap: {:.1} GB)] ",
                node.USERNAME,
                node.IP_ADDRESS,
                node.SSH_PORT,
                node.OS_TYPE,
                remote_encoder_name,
                node.MAX_SIZE_IN_GB,
                node.MAX_SIZE_IN_GB * 0.5
            );
            w.current_root_dir = remote_root_dir.clone();
            w.free_space_bytes = max_size_bytes;
            w.phase = "Step 7: Active (Upload / Encode / Copy-Back)".to_string();
        }
    }

    let upload_queue: Arc<Mutex<VecDeque<(Arc<MountQueueState>, DistributedTaskObject)>>> =
        Arc::new(Mutex::new(VecDeque::new()));
    let encode_queue: Arc<Mutex<VecDeque<(Arc<MountQueueState>, DistributedTaskObject)>>> =
        Arc::new(Mutex::new(VecDeque::new()));
    let copyback_queue: Arc<Mutex<VecDeque<(Arc<MountQueueState>, DistributedTaskObject)>>> =
        Arc::new(Mutex::new(VecDeque::new()));

    // Tracks all currently in-flight tasks on this remote node so a graceful reset (e.g. on remote
    // "No space left on device") can purge the remote temp directory and return every in-flight
    // video file back to the front of its mount queue in original order.
    let inflight_tasks: Arc<Mutex<BTreeMap<u64, (Arc<MountQueueState>, DistributedTaskObject)>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    let reset_lock = Arc::new(Mutex::new(()));
    let active_encode_pid = Arc::new(AtomicUsize::new(0));

    let active_in_pipeline = Arc::new(AtomicUsize::new(0));
    let dispatcher_done = Arc::new(AtomicBool::new(false));
    let upload_done = Arc::new(AtomicBool::new(false));
    let encode_done = Arc::new(AtomicBool::new(false));

    let requeue_on_failure = {
        let journal_ref = Arc::clone(&journal);
        let used_ref = Arc::clone(&remote_used_input_bytes);
        let ui_ref = Arc::clone(&shared_ui);
        let wkey = worker_key.clone();
        move |mq: &Arc<MountQueueState>,
              mut task: DistributedTaskObject,
              stage_err: String,
              node_cfg: &WorkerNodeConfig| {
            let bt = Backtrace::force_capture().to_string();
            log_error_with_bt(
                &format!(
                    "[{}] Task {} failed for '{}': {} (Re-queueing to front of '{}')",
                    wkey,
                    task.task_id,
                    task.original_path.display(),
                    stage_err,
                    mq.mount_point
                ),
                &bt,
            );

            task.error_occurred = Some(stage_err);
            journal_ref.upsert(task.clone());

            let rm_cmd = build_remote_cleanup_command(
                node_cfg,
                &task.remote_input_path,
                &task.remote_output_path,
            );
            let _ = run_ssh_command(node_cfg, &rm_cmd);

            used_ref.fetch_sub(
                task.file_size_bytes.min(used_ref.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
            let cur_used = used_ref.load(Ordering::Relaxed);
            global_metrics().update_remote_quota(
                &node_cfg.effective_nodename(),
                &node_cfg.OS_TYPE,
                cur_used,
                max_size_bytes,
            );
            if let Ok(mut map) = ui_ref.lock() {
                if let Some(w) = map.get_mut(&wkey) {
                    w.free_space_bytes = max_size_bytes.saturating_sub(cur_used);
                    w.batch_error_files += 1;
                }
            }

            mq.clear_in_flight(task.queued_item.item_id);
            mq.queue.lock().unwrap().push_front(task.queued_item);
            let (total_rem, r_len) = mq.total_remaining_items();
            let local_wkey = format!("local:{}", mq.mount_point);
            if let Ok(mut map) = ui_ref.lock() {
                if let Some(lw) = map.get_mut(&local_wkey) {
                    lw.queued_items_remaining = total_rem;
                    lw.ready_to_encode_count = r_len;
                }
            }
        }
    };

    // Graceful Reset Closure: triggered when the remote host runs out of space in its temp folder
    // ("No space left on device" / ENOSPC). Purges the remote temp folder and moves all in-flight
    // video files for this remote manager back to the front of their respective mount queues.
    let graceful_reset_on_disk_full = {
        let reset_mtx = Arc::clone(&reset_lock);
        let inflight_ref = Arc::clone(&inflight_tasks);
        let up_q = Arc::clone(&upload_queue);
        let enc_q = Arc::clone(&encode_queue);
        let cb_q = Arc::clone(&copyback_queue);
        let enc_pid_ref = Arc::clone(&active_encode_pid);
        let used_ref = Arc::clone(&remote_used_input_bytes);
        let in_pipe = Arc::clone(&active_in_pipeline);
        let journal_ref = Arc::clone(&journal);
        let ui_ref = Arc::clone(&shared_ui);
        let wkey = worker_key.clone();
        let node_cfg = node.clone();
        let rem_root = remote_root_dir.clone();

        move |trigger_reason: &str| {
            let _guard = reset_mtx.lock().unwrap();

            // Kill any active local ssh encode process for this node so remote ffmpeg stops immediately
            let pid = enc_pid_ref.swap(0, Ordering::SeqCst) as u32;
            if pid > 0 {
                platform::terminate_process_tree(pid, false);
            }

            up_q.lock().unwrap().clear();
            enc_q.lock().unwrap().clear();
            cb_q.lock().unwrap().clear();

            let drained: Vec<(u64, (Arc<MountQueueState>, DistributedTaskObject))> = {
                let mut map = inflight_ref.lock().unwrap();
                let items: Vec<_> = map.iter().map(|(k, v)| (*k, v.clone())).collect();
                map.clear();
                items
            };

            if drained.is_empty() {
                return;
            }

            global_metrics().record_enospc_reset(&node_cfg.effective_nodename(), &node_cfg.OS_TYPE);
            log_structured_event(
                LogLevel::Warn,
                &node_cfg.effective_nodename(),
                &node_cfg.OS_TYPE,
                &wkey,
                "remote_manager",
                "-",
                None,
                "enospc_graceful_reset",
                &[
                    ("remote_root_dir", rem_root.clone()),
                    ("requeued_files", drained.len().to_string()),
                    ("reason", trigger_reason.to_string()),
                ],
            );

            // Purge remote temp directory and recreate it cleanly
            let purge_and_mkdir_cmd = if node_cfg.OS_TYPE.to_lowercase().contains("win") {
                format!(
                    "powershell -NoProfile -Command \"Stop-Process -Name ffmpeg -Force -ErrorAction SilentlyContinue; Remove-Item -Path 'C:\\Temp\\rusty_h265_remote_*' -Recurse -Force -ErrorAction SilentlyContinue; New-Item -ItemType Directory -Force -Path '{}' | Out-Null\"",
                    rem_root
                )
            } else {
                format!(
                    "killall -q ffmpeg 2>/dev/null; rm -rf /tmp/rusty_h265_remote_* \"$HOME\"/.rusty_h265_remote_* 2>/dev/null; mkdir -p \"{}\"",
                    rem_root
                )
            };
            let _ = run_ssh_command(&node_cfg, &purge_and_mkdir_cmd);

            // Return all in-flight tasks to the front of their mount queues in reverse task_id order
            // so the earliest task remains at the very front.
            let requeued_count = drained.len();
            for (_tid, (mq, mut task)) in drained.into_iter().rev() {
                task.error_occurred = Some(format!(
                    "Graceful remote reset after No space left on device: {}",
                    trigger_reason
                ));
                journal_ref.upsert(task.clone());
                mq.clear_in_flight(task.queued_item.item_id);
                mq.queue.lock().unwrap().push_front(task.queued_item);

                let (total_rem, r_len) = mq.total_remaining_items();
                let local_wkey = format!("local:{}", mq.mount_point);
                if let Ok(mut map) = ui_ref.lock() {
                    if let Some(lw) = map.get_mut(&local_wkey) {
                        lw.queued_items_remaining = total_rem;
                        lw.ready_to_encode_count = r_len;
                    }
                }
            }

            used_ref.store(0, Ordering::SeqCst);
            in_pipe.store(0, Ordering::SeqCst);
            global_metrics().update_remote_quota(
                &node_cfg.effective_nodename(),
                &node_cfg.OS_TYPE,
                0,
                max_size_bytes,
            );

            remove_ui_slot(&ui_ref, &wkey, 0);
            remove_ui_slot(&ui_ref, &wkey, 1);
            remove_ui_slot(&ui_ref, &wkey, 2);

            if let Ok(mut map) = ui_ref.lock() {
                if let Some(w) = map.get_mut(&wkey) {
                    w.free_space_bytes = max_size_bytes;
                    w.batch_total_files = w.batch_total_files.saturating_sub(requeued_count);
                    w.phase = format!(
                        "Graceful Reset: Purged tmp & re-queued {} file(s); resuming...",
                        requeued_count
                    );
                }
            }
        }
    };

    // --- Child Thread 1 (Step 7.4): Asynchronous SCP Upload Worker ---
    let upload_handle = {
        let up_q = Arc::clone(&upload_queue);
        let enc_q = Arc::clone(&encode_queue);
        let cb_q = Arc::clone(&copyback_queue);
        let inflight_ref = Arc::clone(&inflight_tasks);
        let in_pipe = Arc::clone(&active_in_pipeline);
        let disp_done = Arc::clone(&dispatcher_done);
        let up_done = Arc::clone(&upload_done);
        let used_ref = Arc::clone(&remote_used_input_bytes);
        let journal_ref = Arc::clone(&journal);
        let ui_ref = Arc::clone(&shared_ui);
        let wkey = worker_key.clone();
        let node_cfg = node.clone();
        let req_fail = requeue_on_failure.clone();
        let reset_fn = graceful_reset_on_disk_full.clone();

        thread::spawn(move || {
            let r_nodename = node_cfg.effective_nodename();
            let mut wait_tracker = ThreadWaitTracker::new(
                &r_nodename,
                &node_cfg.OS_TYPE,
                &wkey,
                "scp_upload",
                "-",
            );
            let mb_div = 1024.0 * 1024.0;
            let gb_div = 1024.0 * 1024.0 * 1024.0;
            let quota_max_gb = (max_size_bytes as f64) / gb_div;

            loop {
                if is_shutdown_requested() {
                    while let Some((mq, task)) = up_q.lock().unwrap().pop_front() {
                        mq.clear_in_flight(task.queued_item.item_id);
                        inflight_ref.lock().unwrap().remove(&task.task_id);
                        used_ref.fetch_sub(
                            task.file_size_bytes.min(used_ref.load(Ordering::Relaxed)),
                            Ordering::Relaxed,
                        );
                        in_pipe.fetch_sub(1, Ordering::Relaxed);
                    }
                    remove_ui_slot(&ui_ref, &wkey, 0);
                    break;
                }

                let next_task = up_q.lock().unwrap().pop_front();
                match next_task {
                    Some((mq, mut task)) => {
                        let wait_b = wait_tracker.begin_active_task("stage_file");
                        let src_mb = (task.file_size_bytes as f64) / mb_div;
                        let cur_used = used_ref.load(Ordering::Relaxed);
                        let quota_used_gb = (cur_used as f64) / gb_div;
                        let quota_rem_gb = (quota_max_gb - quota_used_gb).max(0.0);

                        mq.set_in_flight(
                            &task.queued_item,
                            QueueEntryCategory::ActiveRemote,
                            &format!("REMOTE {} [scp-up]", node_cfg.IP_ADDRESS),
                        );
                        let fname = task
                            .original_path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("file")
                            .to_string();
                        task.copy_to_remote_state = StageStatus::Started;
                        journal_ref.upsert(task.clone());

                        log_structured_event(
                            LogLevel::Info,
                            &r_nodename,
                            &node_cfg.OS_TYPE,
                            &wkey,
                            "scp_upload",
                            &mq.mount_point,
                            Some(task.task_id),
                            "stage_file_start",
                            &[
                                ("stage_mode", "scp_upload".to_string()),
                                ("source_file", task.original_path.display().to_string()),
                                ("remote_file", task.remote_input_path.clone()),
                                ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                                (
                                    "thread_wait_in_secs",
                                    format!("{:.3}", wait_b.thread_wait_in_secs),
                                ),
                                (
                                    "upstream_queue_wait_in_secs",
                                    format!("{:.3}", wait_b.upstream_queue_wait_in_secs),
                                ),
                                ("wait_cycles", wait_b.wait_cycles.to_string()),
                                ("quota_used_gb", format!("{:.2}", quota_used_gb)),
                                ("quota_remaining_gb", format!("{:.2}", quota_rem_gb)),
                                ("quota_max_gb", format!("{:.2}", quota_max_gb)),
                            ],
                        );

                        let up_display_name = format_filename_with_size(&fname, task.file_size_bytes);
                        update_ui_slot(
                            &ui_ref,
                            &wkey,
                            0,
                            &up_display_name,
                            "scp-up",
                            ((task.file_size_bytes / (1024 * 1024)).max(1)) * 8,
                            0.0,
                            0.0,
                            0.0,
                            "LAN->Remote",
                            "Uploading",
                        );

                        mq.active_remote_managers.fetch_add(1, Ordering::Relaxed);
                        let stage_start = Instant::now();
                        let upload_res = scp_upload_to_remote(
                            &node_cfg,
                            &task.original_path,
                            &task.remote_input_path,
                            task.file_size_bytes,
                            &ui_ref,
                            &wkey,
                            &fname,
                        );
                        let stage_secs = stage_start.elapsed().as_secs_f64();
                        mq.active_remote_managers.fetch_sub(1, Ordering::Relaxed);
                        remove_ui_slot(&ui_ref, &wkey, 0);

                        match upload_res {
                            Ok(_) => {
                                if is_shutdown_requested() {
                                    mq.clear_in_flight(task.queued_item.item_id);
                                    inflight_ref.lock().unwrap().remove(&task.task_id);
                                    let rm_cmd = if node_cfg.OS_TYPE.to_lowercase().contains("win") {
                                        format!(
                                            "powershell -NoProfile -Command \"Remove-Item -LiteralPath '{}' -Force -ErrorAction SilentlyContinue\"",
                                            task.remote_input_path.replace('/', "\\")
                                        )
                                    } else {
                                        format!("rm -f \"{}\"", task.remote_input_path)
                                    };
                                    let _ = run_ssh_command(&node_cfg, &rm_cmd);
                                    used_ref.fetch_sub(
                                        task.file_size_bytes.min(used_ref.load(Ordering::Relaxed)),
                                        Ordering::Relaxed,
                                    );
                                    in_pipe.fetch_sub(1, Ordering::Relaxed);
                                    break;
                                }
                                if !inflight_ref.lock().unwrap().contains_key(&task.task_id) {
                                    // Task was already re-queued by a concurrent graceful reset
                                    continue;
                                }
                                let tp_mbps = if stage_secs > 0.001 {
                                    src_mb / stage_secs
                                } else {
                                    0.0
                                };
                                wait_tracker.complete_active_task("stage_file", "success", stage_secs);
                                log_structured_event(
                                    LogLevel::Info,
                                    &r_nodename,
                                    &node_cfg.OS_TYPE,
                                    &wkey,
                                    "scp_upload",
                                    &mq.mount_point,
                                    Some(task.task_id),
                                    "stage_file_complete",
                                    &[
                                        ("stage_mode", "scp_upload".to_string()),
                                        ("source_file", task.original_path.display().to_string()),
                                        ("remote_file", task.remote_input_path.clone()),
                                        ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                                        (
                                            "thread_wait_in_secs",
                                            format!("{:.3}", wait_b.thread_wait_in_secs),
                                        ),
                                        (
                                            "upstream_queue_wait_in_secs",
                                            format!("{:.3}", wait_b.upstream_queue_wait_in_secs),
                                        ),
                                        ("stage_duration_in_secs", format!("{:.3}", stage_secs)),
                                        ("throughput_mbps", format!("{:.2}", tp_mbps)),
                                        ("quota_used_gb", format!("{:.2}", quota_used_gb)),
                                        ("quota_remaining_gb", format!("{:.2}", quota_rem_gb)),
                                        ("quota_max_gb", format!("{:.2}", quota_max_gb)),
                                    ],
                                );
                                task.copy_to_remote_state = StageStatus::Completed;
                                journal_ref.upsert(task.clone());
                                mq.set_in_flight(
                                    &task.queued_item,
                                    QueueEntryCategory::ActiveRemote,
                                    &format!(
                                        "REMOTE {} [Uploaded: Waiting Encode]",
                                        node_cfg.IP_ADDRESS
                                    ),
                                );
                                enc_q.lock().unwrap().push_back((mq, task));
                                global_metrics().update_remote_queues(
                                    &r_nodename,
                                    &node_cfg.OS_TYPE,
                                    up_q.lock().unwrap().len(),
                                    enc_q.lock().unwrap().len(),
                                    cb_q.lock().unwrap().len(),
                                    inflight_ref.lock().unwrap().len(),
                                );
                            }
                            Err(e) => {
                                wait_tracker.record_error("scp_upload_failed");
                                wait_tracker.complete_active_task("stage_file", "failed", stage_secs);
                                if is_shutdown_requested() {
                                    mq.clear_in_flight(task.queued_item.item_id);
                                    inflight_ref.lock().unwrap().remove(&task.task_id);
                                    log_event(
                                        LogLevel::Info,
                                        format!(
                                            "[{}] Aborted in-flight scp-up for '{}' due to graceful shutdown",
                                            wkey, fname
                                        ),
                                    );
                                    let rm_cmd = if node_cfg.OS_TYPE.to_lowercase().contains("win") {
                                        format!(
                                            "powershell -NoProfile -Command \"Remove-Item -LiteralPath '{}' -Force -ErrorAction SilentlyContinue\"",
                                            task.remote_input_path.replace('/', "\\")
                                        )
                                    } else {
                                        format!("rm -f \"{}\"", task.remote_input_path)
                                    };
                                    let _ = run_ssh_command(&node_cfg, &rm_cmd);
                                    used_ref.fetch_sub(
                                        task.file_size_bytes.min(used_ref.load(Ordering::Relaxed)),
                                        Ordering::Relaxed,
                                    );
                                    in_pipe.fetch_sub(1, Ordering::Relaxed);
                                    break;
                                }
                                let err_str = e.to_string();
                                log_structured_event(
                                    LogLevel::Error,
                                    &r_nodename,
                                    &node_cfg.OS_TYPE,
                                    &wkey,
                                    "scp_upload",
                                    &mq.mount_point,
                                    Some(task.task_id),
                                    "stage_file_complete",
                                    &[
                                        ("stage_mode", "scp_upload_failed".to_string()),
                                        ("source_file", task.original_path.display().to_string()),
                                        ("remote_file", task.remote_input_path.clone()),
                                        ("error_type", "scp_upload_failed".to_string()),
                                        ("reason", err_str.clone()),
                                    ],
                                );
                                if is_remote_out_of_space_error(&err_str) {
                                    reset_fn(&err_str);
                                    continue;
                                }
                                if inflight_ref.lock().unwrap().remove(&task.task_id).is_some() {
                                    task.copy_to_remote_state = StageStatus::Failed;
                                    req_fail(&mq, task, err_str, &node_cfg);
                                    in_pipe.fetch_sub(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    None => {
                        if disp_done.load(Ordering::Relaxed) {
                            break;
                        }
                        wait_tracker.sleep_with_reason(
                            WaitReason::UpstreamQueueEmpty,
                            Duration::from_millis(100),
                        );
                    }
                }
            }
            wait_tracker.mark_finished();
            up_done.store(true, Ordering::Relaxed);
        })
    };

    // --- Child Thread 2 (Step 7.6): Remote FFmpeg Conversion Watcher + Live TUI Progress ---
    let encode_handle = {
        let up_q = Arc::clone(&upload_queue);
        let enc_q = Arc::clone(&encode_queue);
        let cb_q = Arc::clone(&copyback_queue);
        let inflight_ref = Arc::clone(&inflight_tasks);
        let enc_pid_ref = Arc::clone(&active_encode_pid);
        let in_pipe = Arc::clone(&active_in_pipeline);
        let up_done = Arc::clone(&upload_done);
        let enc_done = Arc::clone(&encode_done);
        let used_ref = Arc::clone(&remote_used_input_bytes);
        let journal_ref = Arc::clone(&journal);
        let ui_ref = Arc::clone(&shared_ui);
        let wkey = worker_key.clone();
        let node_cfg = node.clone();
        let ffmpeg_bin = resolved_ffmpeg.clone();
        let enc_name_label = remote_encoder_name.clone();
        let enc_args = remote_encoder_args.clone();
        let remote_decode_args_str = if config.FFMPEG_DECODE_ARGS.is_empty() {
            "-fflags +genpts+igndts -threads 0".to_string()
        } else {
            config.FFMPEG_DECODE_ARGS.join(" ")
        };
        let remote_audio_args_str = format_remote_ffmpeg_args(&remote_audio_args);
        let remote_muxer_args_str = format_remote_ffmpeg_args(&remote_target.container.muxer_args());
        let req_fail = requeue_on_failure.clone();
        let reset_fn = graceful_reset_on_disk_full.clone();

        thread::spawn(move || {
            let r_nodename = node_cfg.effective_nodename();
            let mut wait_tracker = ThreadWaitTracker::new(
                &r_nodename,
                &node_cfg.OS_TYPE,
                &wkey,
                "remote_encode",
                "-",
            );
            let mb_div = 1024.0 * 1024.0;

            loop {
                if is_shutdown_requested() {
                    while let Some((mq, task)) = enc_q.lock().unwrap().pop_front() {
                        mq.clear_in_flight(task.queued_item.item_id);
                        inflight_ref.lock().unwrap().remove(&task.task_id);
                        let rm_cmd = build_remote_cleanup_command(
                            &node_cfg,
                            &task.remote_input_path,
                            &task.remote_output_path,
                        );
                        let _ = run_ssh_command(&node_cfg, &rm_cmd);
                        used_ref.fetch_sub(
                            task.file_size_bytes.min(used_ref.load(Ordering::Relaxed)),
                            Ordering::Relaxed,
                        );
                        in_pipe.fetch_sub(1, Ordering::Relaxed);
                    }
                    remove_ui_slot(&ui_ref, &wkey, 1);
                    break;
                }

                let next_task = enc_q.lock().unwrap().pop_front();
                match next_task {
                    Some((mq, mut task)) => {
                        let wait_b = wait_tracker.begin_active_task("convert_file");
                        let src_mb = (task.file_size_bytes as f64) / mb_div;

                        mq.set_in_flight(
                            &task.queued_item,
                            QueueEntryCategory::ActiveRemote,
                            &format!("REMOTE {} [Encoding]", node_cfg.IP_ADDRESS),
                        );
                        let fname = task
                            .original_path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("video")
                            .to_string();
                        let probe = task
                            .queued_item
                            .cached_probe
                            .clone()
                            .unwrap_or_else(|| get_video_probe_info(&task.original_path));

                        task.conversion_state = StageStatus::Started;
                        journal_ref.upsert(task.clone());

                        let raw_remote_cmd = format!(
                            "\"{}\" -nostdin {} -i \"{}\" {} {} {} -y -loglevel error -progress pipe:1 -nostats \"{}\"",
                            ffmpeg_bin,
                            remote_decode_args_str,
                            task.remote_input_path,
                            format_remote_ffmpeg_args(&enc_args),
                            remote_audio_args_str,
                            remote_muxer_args_str,
                            task.remote_output_path
                        );
                        let wrapped_remote_cmd = wrap_remote_shell_cmd(&node_cfg, &raw_remote_cmd);

                        log_structured_event(
                            LogLevel::Info,
                            &r_nodename,
                            &node_cfg.OS_TYPE,
                            &wkey,
                            "remote_encode",
                            &mq.mount_point,
                            Some(task.task_id),
                            "conversion_start",
                            &[
                                ("source_file", task.original_path.display().to_string()),
                                ("remote_input_file", task.remote_input_path.clone()),
                                ("remote_output_file", task.remote_output_path.clone()),
                                ("source_file_size_in_mb", format!("{:.2}", src_mb)),
                                ("source_codec", probe.video_codec.clone()),
                                ("source_bitrate_kbps", probe.bitrate_kbps.to_string()),
                                (
                                    "source_duration_in_secs",
                                    format!("{:.2}", probe.duration_secs),
                                ),
                                ("encoder", enc_name_label.clone()),
                                (
                                    "thread_wait_in_secs",
                                    format!("{:.3}", wait_b.thread_wait_in_secs),
                                ),
                                (
                                    "upstream_queue_wait_in_secs",
                                    format!("{:.3}", wait_b.upstream_queue_wait_in_secs),
                                ),
                                ("wait_cycles", wait_b.wait_cycles.to_string()),
                            ],
                        );

                        let mut ssh_cmd = Command::new("ssh");
                        ssh_cmd.args(ssh_base_args(&node_cfg));
                        ssh_cmd.arg(&wrapped_remote_cmd);
                        ssh_cmd
                            .stdin(Stdio::null())
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped());
                        platform::configure_child_process_group(&mut ssh_cmd);

                        let conv_start = Instant::now();
                        let spawn_res = ssh_cmd.spawn();
                        match spawn_res {
                            Ok(mut child) => {
                                let child_pid = child.id();
                                enc_pid_ref.store(child_pid as usize, Ordering::SeqCst);
                                let finished_flag = Arc::new(AtomicBool::new(false));
                                let killer_handle =
                                    spawn_shutdown_killer(child_pid, Arc::clone(&finished_flag));

                                let stderr_pipe = child.stderr.take();
                                let stderr_thread = thread::spawn(move || {
                                    let mut err_buf = String::new();
                                    if let Some(mut ep) = stderr_pipe {
                                        use std::io::Read;
                                        let _ = ep.read_to_string(&mut err_buf);
                                    }
                                    err_buf
                                });

                                let mut last_speed_mult = 1.0f64;
                                if let Some(out_pipe) = child.stdout.take() {
                                    let reader = BufReader::new(out_pipe);
                                    let mut current_time_secs = 0.0f64;
                                    let mut current_speed = String::from("1.0x");
                                    let mut last_emit = Instant::now();

                                    for line in reader.lines().map_while(|r| r.ok()) {
                                        if is_shutdown_requested() {
                                            let _ = child.kill();
                                            break;
                                        }
                                        if let Some((k, v)) = line.split_once('=') {
                                            let key = k.trim();
                                            let val = v.trim();
                                            match key {
                                                "out_time_us" | "out_time_ms" => {
                                                    if let Ok(us) = val.parse::<f64>() {
                                                        if us > 0.0 {
                                                            current_time_secs = us / 1_000_000.0;
                                                        }
                                                    }
                                                }
                                                "out_time" => {
                                                    if let Some(s) = parse_hms_to_secs(val) {
                                                        if s > 0.0 {
                                                            current_time_secs = s;
                                                        }
                                                    }
                                                }
                                                "speed" => {
                                                    if !val.is_empty() && val != "N/A" {
                                                        current_speed = val.to_string();
                                                        if let Ok(mult) = val
                                                            .trim_end_matches('x')
                                                            .trim()
                                                            .parse::<f64>()
                                                        {
                                                            last_speed_mult = mult;
                                                            global_metrics().update_media_active_speed(
                                                                &r_nodename,
                                                                &node_cfg.OS_TYPE,
                                                                &wkey,
                                                                &enc_name_label,
                                                                mult,
                                                            );
                                                        }
                                                    }
                                                }
                                                "progress" => {
                                                    let pct = if val == "end" {
                                                        100.0
                                                    } else if probe.duration_secs > 0.0 {
                                                        ((current_time_secs / probe.duration_secs)
                                                            * 100.0)
                                                            .clamp(0.0, 99.9)
                                                    } else {
                                                        0.0
                                                    };
                                                    if val == "end"
                                                        || last_emit.elapsed()
                                                            >= Duration::from_millis(90)
                                                    {
                                                        update_ui_slot(
                                                            &ui_ref,
                                                            &wkey,
                                                            1,
                                                            &fname,
                                                            &probe.video_codec,
                                                            probe.bitrate_kbps,
                                                            current_time_secs,
                                                            probe.duration_secs,
                                                            pct,
                                                            &current_speed,
                                                            "Remote Encode",
                                                        );
                                                        last_emit = Instant::now();
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }

                                let status = child.wait();
                                let conv_secs = conv_start.elapsed().as_secs_f64();
                                enc_pid_ref.store(0, Ordering::SeqCst);
                                finished_flag.store(true, Ordering::Relaxed);
                                let _ = killer_handle.join();
                                let stderr_text = stderr_thread.join().unwrap_or_default();
                                remove_ui_slot(&ui_ref, &wkey, 1);

                                if !stderr_text.trim().is_empty() {
                                    for sline in stderr_text.lines().take(5) {
                                        let st = sline.trim();
                                        if !st.is_empty() {
                                            log_structured_event(
                                                LogLevel::Warn,
                                                &r_nodename,
                                                &node_cfg.OS_TYPE,
                                                &wkey,
                                                "remote_encode",
                                                &mq.mount_point,
                                                Some(task.task_id),
                                                "ffmpeg_stderr_warn",
                                                &[
                                                    (
                                                        "source_file",
                                                        task.original_path.display().to_string(),
                                                    ),
                                                    ("encoder", enc_name_label.clone()),
                                                    ("stderr_line", st.to_string()),
                                                ],
                                            );
                                        }
                                    }
                                }

                                if is_shutdown_requested()
                                    && !status.as_ref().map(|s| s.success()).unwrap_or(false)
                                {
                                    mq.clear_in_flight(task.queued_item.item_id);
                                    inflight_ref.lock().unwrap().remove(&task.task_id);
                                    log_event(
                                        LogLevel::Info,
                                        format!(
                                            "[{}] Aborted in-flight remote FFmpeg for '{}' due to graceful shutdown",
                                            wkey, fname
                                        ),
                                    );
                                    let kill_and_rm = if node_cfg.OS_TYPE.to_lowercase().contains("win") {
                                        format!(
                                            "powershell -NoProfile -Command \"Stop-Process -Name ffmpeg -Force -ErrorAction SilentlyContinue; Remove-Item -LiteralPath '{}','{}' -Force -ErrorAction SilentlyContinue\"",
                                            task.remote_input_path.replace('/', "\\"),
                                            task.remote_output_path.replace('/', "\\")
                                        )
                                    } else {
                                        format!(
                                            "killall -q ffmpeg 2>/dev/null; rm -f \"{}\" \"{}\"",
                                            task.remote_input_path, task.remote_output_path
                                        )
                                    };
                                    let _ = run_ssh_command(&node_cfg, &kill_and_rm);
                                    used_ref.fetch_sub(
                                        task.file_size_bytes.min(used_ref.load(Ordering::Relaxed)),
                                        Ordering::Relaxed,
                                    );
                                    in_pipe.fetch_sub(1, Ordering::Relaxed);
                                    break;
                                }

                                match status {
                                    Ok(st) if st.success() => {
                                        if !inflight_ref.lock().unwrap().contains_key(&task.task_id) {
                                            continue;
                                        }
                                        // Verify that remote ffmpeg actually produced a non-empty output file
                                        match query_remote_file_size_bytes(&node_cfg, &task.remote_output_path) {
                                            Some(remote_out_bytes) => {
                                                let out_mb = (remote_out_bytes as f64) / mb_div;
                                                let saved_bytes = task
                                                    .file_size_bytes
                                                    .saturating_sub(remote_out_bytes);
                                                let saved_mb = (saved_bytes as f64) / mb_div;
                                                let ratio = if task.file_size_bytes > 0 {
                                                    (remote_out_bytes as f64)
                                                        / (task.file_size_bytes as f64)
                                                } else {
                                                    1.0
                                                };
                                                wait_tracker.complete_active_task(
                                                    "convert_file",
                                                    "encoded_h265",
                                                    conv_secs,
                                                );
                                                global_metrics().record_media_completed(
                                                    &r_nodename,
                                                    &node_cfg.OS_TYPE,
                                                    &wkey,
                                                    &enc_name_label,
                                                    task.file_size_bytes,
                                                    remote_out_bytes,
                                                    saved_bytes,
                                                    probe.duration_secs,
                                                );
                                                log_structured_event(
                                                    LogLevel::Info,
                                                    &r_nodename,
                                                    &node_cfg.OS_TYPE,
                                                    &wkey,
                                                    "remote_encode",
                                                    &mq.mount_point,
                                                    Some(task.task_id),
                                                    "conversion_end",
                                                    &[
                                                        (
                                                            "source_file",
                                                            task.original_path
                                                                .display()
                                                                .to_string(),
                                                        ),
                                                        (
                                                            "remote_output_file",
                                                            task.remote_output_path.clone(),
                                                        ),
                                                        (
                                                            "target_file",
                                                            task.final_h265_dest_path
                                                                .display()
                                                                .to_string(),
                                                        ),
                                                        (
                                                            "source_codec",
                                                            probe.video_codec.clone(),
                                                        ),
                                                        (
                                                            "source_bitrate_kbps",
                                                            probe.bitrate_kbps.to_string(),
                                                        ),
                                                        ("encoder", enc_name_label.clone()),
                                                        (
                                                            "thread_wait_in_secs",
                                                            format!(
                                                                "{:.3}",
                                                                wait_b.thread_wait_in_secs
                                                            ),
                                                        ),
                                                        (
                                                            "upstream_queue_wait_in_secs",
                                                            format!(
                                                                "{:.3}",
                                                                wait_b.upstream_queue_wait_in_secs
                                                            ),
                                                        ),
                                                        (
                                                            "conversion_duration_in_secs",
                                                            format!("{:.3}", conv_secs),
                                                        ),
                                                        (
                                                            "encode_speed_x",
                                                            format!("{:.2}", last_speed_mult),
                                                        ),
                                                        (
                                                            "source_file_size_in_mb",
                                                            format!("{:.2}", src_mb),
                                                        ),
                                                        (
                                                            "target_file_size_in_mb",
                                                            format!("{:.2}", out_mb),
                                                        ),
                                                        (
                                                            "storage_saved_in_mb",
                                                            format!("{:.2}", saved_mb),
                                                        ),
                                                        (
                                                            "compression_ratio",
                                                            format!("{:.3}", ratio),
                                                        ),
                                                        (
                                                            "outcome",
                                                            "encoded_h265".to_string(),
                                                        ),
                                                    ],
                                                );
                                                task.conversion_state = StageStatus::Completed;
                                                journal_ref.upsert(task.clone());
                                                mq.set_in_flight(
                                                    &task.queued_item,
                                                    QueueEntryCategory::ActiveRemote,
                                                    &format!(
                                                        "REMOTE {} [Encoded: Waiting Copy-Back]",
                                                        node_cfg.IP_ADDRESS
                                                    ),
                                                );
                                                cb_q.lock().unwrap().push_back((mq, task));
                                                global_metrics().update_remote_queues(
                                                    &r_nodename,
                                                    &node_cfg.OS_TYPE,
                                                    up_q.lock().unwrap().len(),
                                                    enc_q.lock().unwrap().len(),
                                                    cb_q.lock().unwrap().len(),
                                                    inflight_ref.lock().unwrap().len(),
                                                );
                                            }
                                            None => {
                                                let err_msg = format!(
                                                    "Remote ffmpeg exited 0 for `{}` but did not produce a non-empty output file at '{}'",
                                                    raw_remote_cmd, task.remote_output_path
                                                );
                                                wait_tracker.record_error("ffmpeg_empty_output");
                                                wait_tracker.complete_active_task(
                                                    "convert_file",
                                                    "failed",
                                                    conv_secs,
                                                );
                                                log_structured_event(
                                                    LogLevel::Error,
                                                    &r_nodename,
                                                    &node_cfg.OS_TYPE,
                                                    &wkey,
                                                    "remote_encode",
                                                    &mq.mount_point,
                                                    Some(task.task_id),
                                                    "conversion_end",
                                                    &[
                                                        (
                                                            "source_file",
                                                            task.original_path
                                                                .display()
                                                                .to_string(),
                                                        ),
                                                        ("encoder", enc_name_label.clone()),
                                                        (
                                                            "conversion_duration_in_secs",
                                                            format!("{:.3}", conv_secs),
                                                        ),
                                                        ("outcome", "failed".to_string()),
                                                        (
                                                            "error_type",
                                                            "ffmpeg_empty_output".to_string(),
                                                        ),
                                                        ("reason", err_msg.clone()),
                                                    ],
                                                );
                                                task.conversion_state = StageStatus::Failed;
                                                task.error_occurred = Some(err_msg);
                                                journal_ref.upsert(task.clone());
                                                mq.set_in_flight(
                                                    &task.queued_item,
                                                    QueueEntryCategory::ActiveRemote,
                                                    &format!(
                                                        "REMOTE {} [Failed: Waiting Evict]",
                                                        node_cfg.IP_ADDRESS
                                                    ),
                                                );
                                                // Forward to copyback_handle (Thread 3) so it evicts the remote input file
                                                // and records broken_files.journal without blocking Thread 2 (encode_handle).
                                                cb_q.lock().unwrap().push_back((mq, task));
                                            }
                                        }
                                    }
                                    Ok(st) => {
                                        let exit_code = st.code().unwrap_or(-1);
                                        let stderr_trimmed = stderr_text.trim();
                                        let err_msg = format!(
                                            "Remote ffmpeg command `{}` exited with {} | stderr: {}",
                                            raw_remote_cmd,
                                            st,
                                            stderr_trimmed
                                        );
                                        wait_tracker.record_error("ffmpeg_encode_failed");
                                        wait_tracker.complete_active_task(
                                            "convert_file",
                                            "failed",
                                            conv_secs,
                                        );
                                        log_structured_event(
                                            LogLevel::Error,
                                            &r_nodename,
                                            &node_cfg.OS_TYPE,
                                            &wkey,
                                            "remote_encode",
                                            &mq.mount_point,
                                            Some(task.task_id),
                                            "conversion_end",
                                            &[
                                                (
                                                    "source_file",
                                                    task.original_path.display().to_string(),
                                                ),
                                                ("encoder", enc_name_label.clone()),
                                                (
                                                    "conversion_duration_in_secs",
                                                    format!("{:.3}", conv_secs),
                                                ),
                                                ("outcome", "failed".to_string()),
                                                (
                                                    "error_type",
                                                    "ffmpeg_encode_failed".to_string(),
                                                ),
                                                ("exit_code", exit_code.to_string()),
                                                ("reason", stderr_trimmed.to_string()),
                                            ],
                                        );
                                        task.conversion_state = StageStatus::Failed;

                                        // 1. Check if remote temp directory ran out of space -> graceful reset!
                                        if is_remote_out_of_space_error(stderr_trimmed)
                                            || is_remote_out_of_space_error(&err_msg)
                                        {
                                            reset_fn(&err_msg);
                                            continue;
                                        }

                                        // 2. Exit code 255 from `ssh` indicates an SSH transport/connection failure -> re-queue.
                                        let is_ssh_transport_err = exit_code == 255
                                            && (stderr_trimmed.contains("Connection")
                                                || stderr_trimmed.contains("Operation timed out")
                                                || stderr_trimmed.contains("No route to host")
                                                || stderr_trimmed.contains("Broken pipe")
                                                || stderr_trimmed.contains("kex_exchange"));
                                        if is_ssh_transport_err {
                                            if inflight_ref.lock().unwrap().remove(&task.task_id).is_some() {
                                                req_fail(&mq, task, err_msg, &node_cfg);
                                                in_pipe.fetch_sub(1, Ordering::Relaxed);
                                            }
                                        } else {
                                            // Media/encode failure: hand off to copyback_handle (Thread 3) so it evicts
                                            // the remote scp-up input file, reclaims remote_used_input_bytes, and records
                                            // the file in broken_files.journal without blocking Thread 2.
                                            if !inflight_ref.lock().unwrap().contains_key(&task.task_id) {
                                                continue;
                                            }
                                            task.error_occurred = Some(err_msg);
                                            journal_ref.upsert(task.clone());
                                            mq.set_in_flight(
                                                &task.queued_item,
                                                QueueEntryCategory::ActiveRemote,
                                                &format!(
                                                    "REMOTE {} [Failed: Waiting Evict]",
                                                    node_cfg.IP_ADDRESS
                                                ),
                                            );
                                            cb_q.lock().unwrap().push_back((mq, task));
                                        }
                                    }
                                    Err(e) => {
                                        wait_tracker.record_error("ssh_wait_failed");
                                        wait_tracker.complete_active_task(
                                            "convert_file",
                                            "failed",
                                            conv_secs,
                                        );
                                        if inflight_ref.lock().unwrap().remove(&task.task_id).is_some() {
                                            task.conversion_state = StageStatus::Failed;
                                            req_fail(&mq, task, e.to_string(), &node_cfg);
                                            in_pipe.fetch_sub(1, Ordering::Relaxed);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                wait_tracker.record_error("ssh_spawn_failed");
                                wait_tracker.complete_active_task("convert_file", "failed", 0.0);
                                if inflight_ref.lock().unwrap().remove(&task.task_id).is_some() {
                                    task.conversion_state = StageStatus::Failed;
                                    req_fail(&mq, task, e.to_string(), &node_cfg);
                                    in_pipe.fetch_sub(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    None => {
                        if up_done.load(Ordering::Relaxed) && enc_q.lock().unwrap().is_empty() {
                            break;
                        }
                        wait_tracker.sleep_with_reason(
                            WaitReason::UpstreamQueueEmpty,
                            Duration::from_millis(100),
                        );
                    }
                }
            }
            wait_tracker.mark_finished();
            enc_done.store(true, Ordering::Relaxed);
        })
    };

    // --- Child Thread 3 (Step 7.7 & 7.8): Copy-Back & Task Completion Thread ---
    // Note: During graceful shutdown, copyback_handle ALWAYS finishes all queued/in-flight scp-down transfers!
    let copyback_handle = {
        let up_q = Arc::clone(&upload_queue);
        let enc_q = Arc::clone(&encode_queue);
        let cb_q = Arc::clone(&copyback_queue);
        let inflight_ref = Arc::clone(&inflight_tasks);
        let in_pipe = Arc::clone(&active_in_pipeline);
        let enc_done = Arc::clone(&encode_done);
        let journal_ref = Arc::clone(&journal);
        let used_ref = Arc::clone(&remote_used_input_bytes);
        let trackers_ref = Arc::clone(&subdir_trackers);
        let ui_ref = Arc::clone(&shared_ui);
        let wkey = worker_key.clone();
        let node_cfg = node.clone();
        let cfg = config.clone();
        let req_fail = requeue_on_failure.clone();

        thread::spawn(move || {
            let r_nodename = node_cfg.effective_nodename();
            let mut wait_tracker = ThreadWaitTracker::new(
                &r_nodename,
                &node_cfg.OS_TYPE,
                &wkey,
                "scp_copyback",
                "-",
            );
            let mb_div = 1024.0 * 1024.0;
            let gb_div = 1024.0 * 1024.0 * 1024.0;
            let quota_max_gb = (max_size_bytes as f64) / gb_div;

            loop {
                let next_task = cb_q.lock().unwrap().pop_front();
                match next_task {
                    Some((mq, mut task)) => {
                        // Branch A — Evict Only (No scp-down):
                        // If encode_handle reported a failed conversion (non-zero exit or 0-byte/missing output),
                        // skip scp_download_from_remote, evict the remote scp-up input file, reclaim remote_used_input_bytes,
                        // and record the broken source file in broken_files.journal.
                        if task.conversion_state == StageStatus::Failed
                            || task.error_occurred.is_some()
                        {
                            let _ = wait_tracker.begin_active_task("flush_evict_failed");
                            let ev_start = Instant::now();
                            execute_remote_evict_failed_task(
                                task,
                                &inflight_ref,
                                &in_pipe,
                                &used_ref,
                                max_size_bytes,
                                &journal_ref,
                                &trackers_ref,
                                &ui_ref,
                                &wkey,
                                &node_cfg,
                                &cfg,
                                verbose,
                                |n, cmd| {
                                    let _ = run_ssh_command(n, cmd);
                                },
                            );
                            wait_tracker.complete_active_task(
                                "flush_file",
                                "evict_failed",
                                ev_start.elapsed().as_secs_f64(),
                            );
                            continue;
                        }

                        // Branch B — Copy-Back & Cleanup (conversion_state == StageStatus::Completed):
                        let wait_b = wait_tracker.begin_active_task("flush_file");
                        mq.set_in_flight(
                            &task.queued_item,
                            QueueEntryCategory::ActiveRemote,
                            &format!("REMOTE {} [scp-down]", node_cfg.IP_ADDRESS),
                        );
                        let fname = task
                            .final_h265_dest_path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("output_h265.mp4")
                            .to_string();
                        task.copy_back_state = StageStatus::Started;
                        journal_ref.upsert(task.clone());

                        let remote_out_bytes =
                            query_remote_file_size_bytes(&node_cfg, &task.remote_output_path)
                                .unwrap_or(task.file_size_bytes);
                        let tgt_mb = (remote_out_bytes as f64) / mb_div;

                        log_structured_event(
                            LogLevel::Info,
                            &r_nodename,
                            &node_cfg.OS_TYPE,
                            &wkey,
                            "scp_copyback",
                            &mq.mount_point,
                            Some(task.task_id),
                            "flush_start",
                            &[
                                ("flush_mode", "scp_copyback".to_string()),
                                ("source_file", task.original_path.display().to_string()),
                                ("remote_file", task.remote_output_path.clone()),
                                (
                                    "target_file",
                                    task.final_h265_dest_path.display().to_string(),
                                ),
                                ("target_file_size_in_mb", format!("{:.2}", tgt_mb)),
                                (
                                    "thread_wait_in_secs",
                                    format!("{:.3}", wait_b.thread_wait_in_secs),
                                ),
                                (
                                    "upstream_queue_wait_in_secs",
                                    format!("{:.3}", wait_b.upstream_queue_wait_in_secs),
                                ),
                                ("wait_cycles", wait_b.wait_cycles.to_string()),
                            ],
                        );

                        let down_display_name =
                            format_filename_with_size(&fname, remote_out_bytes);
                        update_ui_slot(
                            &ui_ref,
                            &wkey,
                            2,
                            &down_display_name,
                            "scp-down",
                            ((remote_out_bytes / (1024 * 1024)).max(1)) * 8,
                            0.0,
                            0.0,
                            0.0,
                            "Remote->LAN",
                            "Copy-Back",
                        );

                        mq.active_remote_managers.fetch_add(1, Ordering::Relaxed);
                        let dl_start = Instant::now();
                        let download_res = scp_download_from_remote(
                            &node_cfg,
                            &task.remote_output_path,
                            &task.final_h265_dest_path,
                            remote_out_bytes,
                            &ui_ref,
                            &wkey,
                            &fname,
                        );
                        let flush_secs = dl_start.elapsed().as_secs_f64();
                        mq.active_remote_managers.fetch_sub(1, Ordering::Relaxed);
                        remove_ui_slot(&ui_ref, &wkey, 2);

                        match download_res {
                            Ok(_) => {
                                mq.clear_in_flight(task.queued_item.item_id);
                                inflight_ref.lock().unwrap().remove(&task.task_id);
                                let cleanup_start = Instant::now();
                                let rm_cmd = build_remote_cleanup_command(
                                    &node_cfg,
                                    &task.remote_input_path,
                                    &task.remote_output_path,
                                );
                                let _ = run_ssh_command(&node_cfg, &rm_cmd);
                                let cleanup_secs = cleanup_start.elapsed().as_secs_f64();

                                used_ref.fetch_sub(
                                    task.file_size_bytes.min(used_ref.load(Ordering::Relaxed)),
                                    Ordering::Relaxed,
                                );
                                let cur_used = used_ref.load(Ordering::Relaxed);
                                let quota_used_gb = (cur_used as f64) / gb_div;
                                let quota_rem_gb = (quota_max_gb - quota_used_gb).max(0.0);
                                global_metrics().update_remote_quota(
                                    &r_nodename,
                                    &node_cfg.OS_TYPE,
                                    cur_used,
                                    max_size_bytes,
                                );

                                let new_size = fs::metadata(&task.final_h265_dest_path)
                                    .map(|m| m.len())
                                    .unwrap_or(0);
                                let src_probe_for_verify = task
                                    .queued_item
                                    .cached_probe
                                    .clone()
                                    .unwrap_or_else(|| get_video_probe_info(&task.original_path));
                                let out_probe_for_verify =
                                    get_video_probe_info(&task.final_h265_dest_path);
                                if let Err(dur_err) = verify_output_durations(
                                    &task.original_path,
                                    &src_probe_for_verify,
                                    &out_probe_for_verify,
                                    &cfg,
                                ) {
                                    let _ = fs::remove_file(&task.final_h265_dest_path);
                                    let err_msg = format!(
                                        "Output duration mismatch for {:?} (remote {}): {}",
                                        task.final_h265_dest_path, r_nodename, dur_err
                                    );
                                    wait_tracker.record_error("remote_duration_mismatch");
                                    wait_tracker.complete_active_task(
                                        "flush_file",
                                        "duration_mismatch",
                                        flush_secs + cleanup_secs,
                                    );
                                    log_structured_event(
                                        LogLevel::Error,
                                        &r_nodename,
                                        &node_cfg.OS_TYPE,
                                        &wkey,
                                        "scp_copyback",
                                        &mq.mount_point,
                                        Some(task.task_id),
                                        "flush_end",
                                        &[
                                            ("flush_mode", "scp_copyback_duration_mismatch".to_string()),
                                            ("source_file", task.original_path.display().to_string()),
                                            ("target_file", task.final_h265_dest_path.display().to_string()),
                                            ("error_type", "remote_duration_mismatch".to_string()),
                                            ("reason", err_msg.clone()),
                                        ],
                                    );
                                    if !is_shutdown_requested() {
                                        record_broken_file_in_journal(
                                            &task.original_path,
                                            task.file_size_bytes,
                                            &err_msg,
                                        );
                                        let fdirs_log = PathBuf::from(&cfg.FAILED_DIRS_PATH);
                                        let src_d = task
                                            .original_path
                                            .parent()
                                            .or(task.queued_item.source_subdir.as_deref())
                                            .unwrap_or(&task.queued_item.root_dir);
                                        let err_obj = anyhow::anyhow!("{}", err_msg);
                                        record_file_failure_if_untracked_subdir(
                                            &fdirs_log,
                                            &task.queued_item,
                                            src_d,
                                            Some(task.queued_item.dest_dir.as_path()),
                                            &format!(
                                                "[Remote Manager {}] Remote output duration mismatch for '{}'",
                                                wkey,
                                                task.original_path.display()
                                            ),
                                            &err_obj,
                                        );
                                        complete_queued_item(
                                            &task.queued_item,
                                            false,
                                            false,
                                            true,
                                            0,
                                            &trackers_ref,
                                            &ui_ref,
                                            &wkey,
                                            &cfg,
                                            verbose,
                                        );
                                    }
                                    task.copy_back_state = StageStatus::Failed;
                                    task.error_occurred = Some(err_msg);
                                    journal_ref.upsert(task.clone());
                                    in_pipe.fetch_sub(1, Ordering::Relaxed);
                                    continue;
                                }
                                let saved = task.file_size_bytes.saturating_sub(new_size);
                                let final_tgt_mb = (new_size as f64) / mb_div;
                                let saved_mb = (saved as f64) / mb_div;
                                let tp_mbps = if flush_secs > 0.001 {
                                    final_tgt_mb / flush_secs
                                } else {
                                    0.0
                                };

                                wait_tracker.complete_active_task(
                                    "flush_file",
                                    "success",
                                    flush_secs + cleanup_secs,
                                );
                                log_structured_event(
                                    LogLevel::Info,
                                    &r_nodename,
                                    &node_cfg.OS_TYPE,
                                    &wkey,
                                    "scp_copyback",
                                    &mq.mount_point,
                                    Some(task.task_id),
                                    "flush_end",
                                    &[
                                        ("flush_mode", "scp_copyback".to_string()),
                                        (
                                            "source_file",
                                            task.original_path.display().to_string(),
                                        ),
                                        ("remote_file", task.remote_output_path.clone()),
                                        (
                                            "target_file",
                                            task.final_h265_dest_path.display().to_string(),
                                        ),
                                        (
                                            "target_file_size_in_mb",
                                            format!("{:.2}", final_tgt_mb),
                                        ),
                                        (
                                            "thread_wait_in_secs",
                                            format!("{:.3}", wait_b.thread_wait_in_secs),
                                        ),
                                        (
                                            "upstream_queue_wait_in_secs",
                                            format!("{:.3}", wait_b.upstream_queue_wait_in_secs),
                                        ),
                                        ("flush_duration_in_secs", format!("{:.3}", flush_secs)),
                                        (
                                            "cleanup_duration_in_secs",
                                            format!("{:.3}", cleanup_secs),
                                        ),
                                        ("throughput_mbps", format!("{:.2}", tp_mbps)),
                                        ("storage_saved_in_mb", format!("{:.2}", saved_mb)),
                                        ("quota_used_gb", format!("{:.2}", quota_used_gb)),
                                        ("quota_remaining_gb", format!("{:.2}", quota_rem_gb)),
                                        ("quota_max_gb", format!("{:.2}", quota_max_gb)),
                                    ],
                                );

                                task.copy_back_state = StageStatus::Completed;
                                journal_ref.upsert(task.clone());

                                if let Ok(mut map) = ui_ref.lock() {
                                    if let Some(w) = map.get_mut(&wkey) {
                                        w.free_space_bytes = max_size_bytes.saturating_sub(cur_used);
                                    }
                                }

                                complete_queued_item(
                                    &task.queued_item,
                                    false,
                                    false,
                                    false,
                                    saved,
                                    &trackers_ref,
                                    &ui_ref,
                                    &wkey,
                                    &cfg,
                                    verbose,
                                );
                                in_pipe.fetch_sub(1, Ordering::Relaxed);
                                global_metrics().update_remote_queues(
                                    &r_nodename,
                                    &node_cfg.OS_TYPE,
                                    up_q.lock().unwrap().len(),
                                    enc_q.lock().unwrap().len(),
                                    cb_q.lock().unwrap().len(),
                                    inflight_ref.lock().unwrap().len(),
                                );
                            }
                            Err(e) => {
                                wait_tracker.record_error("scp_copyback_failed");
                                wait_tracker.complete_active_task(
                                    "flush_file",
                                    "failed",
                                    flush_secs,
                                );
                                log_structured_event(
                                    LogLevel::Error,
                                    &r_nodename,
                                    &node_cfg.OS_TYPE,
                                    &wkey,
                                    "scp_copyback",
                                    &mq.mount_point,
                                    Some(task.task_id),
                                    "flush_end",
                                    &[
                                        ("flush_mode", "scp_copyback_failed".to_string()),
                                        (
                                            "source_file",
                                            task.original_path.display().to_string(),
                                        ),
                                        (
                                            "target_file",
                                            task.final_h265_dest_path.display().to_string(),
                                        ),
                                        ("error_type", "scp_copyback_failed".to_string()),
                                        ("reason", e.to_string()),
                                    ],
                                );
                                if inflight_ref.lock().unwrap().remove(&task.task_id).is_some() {
                                    let fdirs_log = PathBuf::from(&cfg.FAILED_DIRS_PATH);
                                    let src_d = task
                                        .original_path
                                        .parent()
                                        .or(task.queued_item.source_subdir.as_deref())
                                        .unwrap_or(&task.queued_item.root_dir);
                                    record_file_failure_if_untracked_subdir(
                                        &fdirs_log,
                                        &task.queued_item,
                                        src_d,
                                        Some(task.queued_item.dest_dir.as_path()),
                                        &format!(
                                            "[Remote Manager {}] Copy-back failed for '{}'",
                                            wkey,
                                            task.final_h265_dest_path.display()
                                        ),
                                        &e,
                                    );
                                    task.copy_back_state = StageStatus::Failed;
                                    req_fail(&mq, task, e.to_string(), &node_cfg);
                                    in_pipe.fetch_sub(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    None => {
                        if enc_done.load(Ordering::Relaxed) && cb_q.lock().unwrap().is_empty() {
                            break;
                        }
                        wait_tracker.sleep_with_reason(
                            WaitReason::UpstreamQueueEmpty,
                            Duration::from_millis(100),
                        );
                    }
                }
            }
            wait_tracker.mark_finished();
        })
    };

    // --- Step 7.1 - 7.5: Main Manager Dispatcher Loop ---
    let rng_counter = AtomicU64::new(1);
    let mut dispatcher_wait_tracker = ThreadWaitTracker::new(
        &remote_nodename,
        &node.OS_TYPE,
        &worker_key,
        "remote_dispatcher",
        "-",
    );
    loop {
        if is_shutdown_requested() {
            if let Ok(mut map) = shared_ui.lock() {
                if let Some(w) = map.get_mut(&worker_key) {
                    w.phase = "Graceful Exit: Draining Copy-Back...".to_string();
                }
            }
            break;
        }

        let any_pending_work = mount_queues.iter().any(|mq| mq.has_pending_work());

        if !any_pending_work {
            if active_in_pipeline.load(Ordering::Relaxed) == 0 {
                break;
            }
            dispatcher_wait_tracker.sleep_with_reason(
                WaitReason::UpstreamQueueEmpty,
                Duration::from_millis(200),
            );
            continue;
        }

        let Some((chosen_mq, item)) = pick_mount_queue_prioritizing_unoccupied(
            &mount_queues,
            &rng_counter,
            max_single_file_bytes,
        ) else {
            dispatcher_wait_tracker.sleep_with_reason(
                WaitReason::UpstreamQueueEmpty,
                Duration::from_millis(120),
            );
            continue;
        };

        let (total_rem, r_len) = chosen_mq.total_remaining_items();
        let local_wkey = format!("local:{}", chosen_mq.mount_point);
        if let Ok(mut map) = shared_ui.lock() {
            if let Some(lw) = map.get_mut(&local_wkey) {
                lw.queued_items_remaining = total_rem;
                lw.ready_to_encode_count = r_len;
            }
        }

        let current_used = remote_used_input_bytes.load(Ordering::Relaxed);
        let remaining_space = max_size_bytes.saturating_sub(current_used);

        if remaining_space < item.file_size_bytes {
            chosen_mq.queue.lock().unwrap().push_front(item);
            let (total_rem_after, r_len_after) = chosen_mq.total_remaining_items();
            if let Ok(mut map) = shared_ui.lock() {
                if let Some(lw) = map.get_mut(&local_wkey) {
                    lw.queued_items_remaining = total_rem_after;
                    lw.ready_to_encode_count = r_len_after;
                }
            }
            dispatcher_wait_tracker.sleep_with_reason(
                WaitReason::RemoteQuotaFull,
                Duration::from_millis(250),
            );
            continue;
        }

        let _disp_wait_b = dispatcher_wait_tracker.begin_active_task("dispatch_task");
        let dispatch_start = Instant::now();
        let new_used =
            remote_used_input_bytes.fetch_add(item.file_size_bytes, Ordering::Relaxed)
                + item.file_size_bytes;
        global_metrics().update_remote_quota(
            &remote_nodename,
            &node.OS_TYPE,
            new_used,
            max_size_bytes,
        );

        let out_filename = build_h265_filename(&item.source_path, false, &config);
        let final_h265_dest_path = item.dest_dir.join(&out_filename);

        let tid = if item.item_id > 0 {
            item.item_id
        } else {
            next_task_id.fetch_add(1, Ordering::Relaxed)
        };
        let orig_ext = item
            .source_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp4");
        let remote_input_path = format!("{}/task_{}_in.{}", remote_root_dir, tid, orig_ext);
        let remote_output_path = format!("{}/task_{}_out_h265.mp4", remote_root_dir, tid);

        let task_obj = DistributedTaskObject {
            task_id: tid,
            node_id: worker_key.clone(),
            original_queue: chosen_mq.mount_point.clone(),
            original_path: item.source_path.clone(),
            file_size_bytes: item.file_size_bytes,
            remote_input_path,
            remote_output_path,
            final_h265_dest_path,
            copy_to_remote_state: StageStatus::Pending,
            conversion_state: StageStatus::Pending,
            copy_back_state: StageStatus::Pending,
            error_occurred: None,
            updated_at_epoch_secs: 0,
            queued_item: item.clone(),
        };

        journal.upsert(task_obj.clone());

        if let Ok(mut map) = shared_ui.lock() {
            if let Some(w) = map.get_mut(&worker_key) {
                w.current_target_label = format!(
                    "Queue {} -> {}",
                    chosen_mq.mount_point, item.target_label
                );
                w.current_target_size_bytes = item.file_size_bytes;
                w.free_space_bytes = max_size_bytes
                    .saturating_sub(remote_used_input_bytes.load(Ordering::Relaxed));
                w.batch_total_files += 1;
                w.phase = "Step 7: Active (Upload / Encode / Copy-Back)".to_string();
            }
        }

        chosen_mq.set_in_flight(
            &item,
            QueueEntryCategory::ActiveRemote,
            &format!("REMOTE {} [Queued for scp-up]", node.IP_ADDRESS),
        );
        inflight_tasks
            .lock()
            .unwrap()
            .insert(tid, (Arc::clone(&chosen_mq), task_obj.clone()));
        active_in_pipeline.fetch_add(1, Ordering::Relaxed);
        upload_queue.lock().unwrap().push_back((chosen_mq, task_obj));
        global_metrics().update_remote_queues(
            &remote_nodename,
            &node.OS_TYPE,
            upload_queue.lock().unwrap().len(),
            encode_queue.lock().unwrap().len(),
            copyback_queue.lock().unwrap().len(),
            inflight_tasks.lock().unwrap().len(),
        );
        dispatcher_wait_tracker.complete_active_task(
            "dispatch_task",
            "dispatched",
            dispatch_start.elapsed().as_secs_f64(),
        );
    }

    dispatcher_wait_tracker.mark_finished();
    dispatcher_done.store(true, Ordering::Relaxed);
    let _ = upload_handle.join();
    let _ = encode_handle.join();
    let _ = copyback_handle.join();

    remote_agent_stop.store(true, Ordering::Relaxed);
    if let Some(ah) = remote_agent_handle {
        let _ = ah.join();
    }

    if is_windows {
        let _ = run_ssh_command(
            &node,
            &format!(
                "powershell -NoProfile -Command \"Remove-Item -LiteralPath '{}' -Recurse -Force -ErrorAction SilentlyContinue\"",
                remote_root_dir.replace('/', "\\")
            ),
        );
    } else {
        let _ = run_ssh_command(&node, &format!("rm -rf \"{}\"", remote_root_dir));
    }

    if let Ok(mut map) = shared_ui.lock() {
        if let Some(w) = map.get_mut(&worker_key) {
            w.phase = if is_shutdown_requested() {
                "Stopped (Graceful Exit)".to_string()
            } else {
                "Completed all remote tasks".to_string()
            };
            w.active_files.clear();
            w.finished = true;
        }
    }
}

// --- Ncurses-Style Terminal UI Renderer ---
fn render_progress_bar(percent: f64, width: usize) -> String {
    let clamped = percent.clamp(0.0, 100.0);
    let filled = ((clamped / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width.saturating_sub(filled);
    format!(
        "\x1b[32m{}\x1b[90m{}\x1b[0m",
        "█".repeat(filled),
        "░".repeat(empty)
    )
}

/// Returns the terminal column width of a Unicode character (0 for control/combining,
/// 2 for wide/CJK/emoji codepoints, 1 for standard ASCII and box-drawing characters).
fn char_display_width(c: char) -> usize {
    let u = c as u32;
    if u < 0x20 || (0x7f..0xa0).contains(&u) {
        return 0;
    }
    if (0x1100..=0x115f).contains(&u)
        || (0x231a..=0x23ff).contains(&u)
        || (0x2600..=0x27bf).contains(&u)
        || (0x2b00..=0x2bff).contains(&u)
        || (0x2e80..=0xa4cf).contains(&u)
        || (0xac00..=0xd7a3).contains(&u)
        || (0xf900..=0xfaff).contains(&u)
        || (0xfe10..=0xfe6f).contains(&u)
        || (0xff01..=0xff60).contains(&u)
        || (0xffe0..=0xffe6).contains(&u)
        || u >= 0x1f000
    {
        return 2;
    }
    1
}

fn str_display_width(s: &str) -> usize {
    s.chars().map(char_display_width).sum()
}

fn truncate_middle(s: &str, max_len: usize) -> String {
    let total_w = str_display_width(s);
    if total_w <= max_len {
        return s.to_string();
    }
    if max_len <= 5 {
        let mut out = String::new();
        let mut cur_w = 0;
        for c in s.chars() {
            let cw = char_display_width(c);
            if cur_w + cw > max_len {
                break;
            }
            out.push(c);
            cur_w += cw;
        }
        return out;
    }
    let head_target = (max_len - 3) / 2;
    let tail_target = max_len - 3 - head_target;

    let mut start = String::new();
    let mut start_w = 0;
    for c in s.chars() {
        let cw = char_display_width(c);
        if start_w + cw > head_target {
            break;
        }
        start.push(c);
        start_w += cw;
    }

    let mut end_chars: Vec<char> = Vec::new();
    let mut end_w = 0;
    for c in s.chars().rev() {
        let cw = char_display_width(c);
        if end_w + cw > tail_target {
            break;
        }
        end_chars.push(c);
        end_w += cw;
    }
    end_chars.reverse();
    let end: String = end_chars.into_iter().collect();
    format!("{}...{}", start, end)
}

fn pad_or_truncate(s: &str, width: usize) -> String {
    let truncated = if str_display_width(s) > width {
        truncate_middle(s, width)
    } else {
        s.to_string()
    };
    let cur_w = str_display_width(&truncated);
    if cur_w < width {
        format!("{}{}", truncated, " ".repeat(width - cur_w))
    } else {
        truncated
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_tui_frame(
    workers: &BTreeMap<String, WorkerUiSnapshot>,
    mount_queues: &[Arc<MountQueueState>],
    tui_state: &TuiInteractiveState,
    ramdisk_banner: &str,
    start_time: Instant,
    prev_lines_drawn: &mut usize,
    prev_term_width: &mut usize,
    prev_term_height: &mut usize,
    prev_mode: &mut TuiViewMode,
) {
    let mut out = String::new();
    let (raw_cols, raw_rows) = get_terminal_dimensions();
    // Leave a 1-column right margin so terminals with auto-wrap at the last column never wrap a line
    let box_width: usize = raw_cols.saturating_sub(1).max(60);
    let max_visible_rows: usize = raw_rows.saturating_sub(1).max(12);
    let inner_width = box_width.saturating_sub(4);

    if *prev_term_width == 0
        || *prev_term_width != box_width
        || *prev_term_height != raw_rows
        || *prev_mode != tui_state.mode
    {
        out.push_str("\x1b[2J\x1b[H");
    } else {
        out.push_str("\x1b[H");
    }
    *prev_term_width = box_width;
    *prev_term_height = raw_rows;
    *prev_mode = tui_state.mode;

    let mut lines: Vec<String> = Vec::new();
    let elapsed = format_duration_hms(start_time.elapsed().as_secs_f64());
    let total_saved_bytes: u64 = workers.values().map(|w| w.bytes_saved_total).sum();
    let shutting_down = is_shutdown_requested();

    // --- Mode 3: Full-Screen `less`-Style Sortable Queue Pager ---
    if tui_state.mode == TuiViewMode::QueuePager {
        if let Some(ref snap) = tui_state.active_snapshot {
            let top_border = format!("╔{}╗", "═".repeat(box_width - 2));
            let summary_text = format!(" {}", snap.header_summary());
            let sub_header_text = format!(
                " Mount: {} ({})  |  Sort: {}  |  Scroll: Up/Dn/j/k/PgUp/PgDn/Space/b/g/G  |  Cmds: :s1a :s1d :s2a :s2d :s3a :s3d :r  |  Esc/q: Back",
                snap.mount_point,
                snap.drive_type.as_str(),
                snap.sort_label()
            );

            lines.push(top_border);
            lines.push(format!(
                "║ \x1b[1;36m{}\x1b[0m ║",
                pad_or_truncate(&summary_text, inner_width)
            ));
            lines.push(format!(
                "║ \x1b[1;33m{}\x1b[0m ║",
                pad_or_truncate(&sub_header_text, inner_width)
            ));
            lines.push(format!("╠{}╣", "═".repeat(box_width - 2)));

            let idx_w = 6usize;
            let size_w = 12usize;
            let status_w = 34usize;
            let fixed_overhead = idx_w + 2 + 3 + size_w + 3 + status_w;
            let name_w = inner_width.saturating_sub(fixed_overhead).max(12);

            let (c1_ind, c2_ind, c3_ind) = match snap.active_sort {
                Some(QueueSortSpec {
                    column: SortColumn::Name,
                    direction: SortDirection::Asc,
                }) => (" (ASC)", "", ""),
                Some(QueueSortSpec {
                    column: SortColumn::Name,
                    direction: SortDirection::Desc,
                }) => (" (DESC)", "", ""),
                Some(QueueSortSpec {
                    column: SortColumn::Size,
                    direction: SortDirection::Asc,
                }) => ("", " (ASC)", ""),
                Some(QueueSortSpec {
                    column: SortColumn::Size,
                    direction: SortDirection::Desc,
                }) => ("", " (DESC)", ""),
                Some(QueueSortSpec {
                    column: SortColumn::Status,
                    direction: SortDirection::Asc,
                }) => ("", "", " (ASC)"),
                Some(QueueSortSpec {
                    column: SortColumn::Status,
                    direction: SortDirection::Desc,
                }) => ("", "", " (DESC)"),
                None => ("", "", ""),
            };

            let col1_hdr = pad_or_truncate(&format!("[1] File Name / Path{}", c1_ind), name_w);
            let col2_hdr = format!("{:>width$}", format!("[2] Size{}", c2_ind), width = size_w);
            let col3_hdr = pad_or_truncate(&format!("[3] Status / Worker{}", c3_ind), status_w);
            let table_hdr = format!(
                "{:>idx_w$}  {} │ {} │ {}",
                "#",
                col1_hdr,
                col2_hdr,
                col3_hdr,
                idx_w = idx_w
            );
            lines.push(format!(
                "║ \x1b[1;37m{}\x1b[0m ║",
                pad_or_truncate(&table_hdr, inner_width)
            ));
            lines.push(format!("╟{}╢", "─".repeat(box_width - 2)));

            // Reserve 8 lines for top/table headers (6 lines) and bottom status bar (2 lines)
            let body_rows = max_visible_rows.saturating_sub(8).max(3);
            let total_entries = snap.displayed_entries.len();
            let max_scroll = total_entries.saturating_sub(body_rows);
            let start_idx = snap.scroll_offset.min(max_scroll);
            let end_idx = (start_idx + body_rows).min(total_entries);

            if total_entries == 0 {
                lines.push(format!(
                    "║ \x1b[90m{}\x1b[0m ║",
                    pad_or_truncate(
                        " (No active or queued files in this mount queue snapshot)",
                        inner_width
                    )
                ));
                for _ in 1..body_rows {
                    lines.push(format!("║ {} ║", " ".repeat(inner_width)));
                }
            } else {
                for row_idx in start_idx..end_idx {
                    let entry = &snap.displayed_entries[row_idx];
                    let row_color = match entry.category {
                        QueueEntryCategory::ActiveLocal => "\x1b[1;32m",
                        QueueEntryCategory::ActiveRemote => "\x1b[1;35m",
                        QueueEntryCategory::QueuedReady => "\x1b[36m",
                        QueueEntryCategory::QueuedPending => "\x1b[90m",
                    };
                    let c1 = pad_or_truncate(&entry.display_path, name_w);
                    let c2 = format!(
                        "{:>width$}",
                        format_bytes(entry.file_size_bytes),
                        width = size_w
                    );
                    let c3 = pad_or_truncate(&entry.status_label, status_w);
                    let row_str = format!(
                        "{:>idx_w$}  {} │ {} │ {}",
                        row_idx + 1,
                        c1,
                        c2,
                        c3,
                        idx_w = idx_w
                    );
                    lines.push(format!(
                        "║ {}{}\x1b[0m ║",
                        row_color,
                        pad_or_truncate(&row_str, inner_width)
                    ));
                }
                let drawn_rows = end_idx.saturating_sub(start_idx);
                for _ in drawn_rows..body_rows {
                    lines.push(format!("║ \x1b[90m{}\x1b[0m ║", pad_or_truncate("~", inner_width)));
                }
            }

            lines.push(format!("╠{}╣", "═".repeat(box_width - 2)));

            let footer_line = if tui_state.command_input_active {
                let prompt = format!(
                    ":{}_   (Sort: s1a/s1d=Name, s2a/s2d=Size, s3a/s3d=Status, r=Reset, q=Back | Enter=Apply, Esc=Cancel)",
                    tui_state.command_buffer
                );
                format!(
                    "║ \x1b[1;33m{}\x1b[0m ║",
                    pad_or_truncate(&prompt, inner_width)
                )
            } else {
                let pct = if total_entries == 0 || end_idx >= total_entries {
                    100
                } else {
                    (end_idx * 100) / total_entries
                };
                let pos_str = if total_entries == 0 {
                    "Lines 0-0 of 0 (100%)".to_string()
                } else {
                    format!("Lines {}-{} of {} ({}%)", start_idx + 1, end_idx, total_entries, pct)
                };
                let msg = tui_state
                    .status_message
                    .as_deref()
                    .unwrap_or("Type ':' for sort commands (:s1a, :s2d, :s3a, :r) or Esc/q to exit queue view");
                let status_bar = format!(" {}  |  {}", pos_str, msg);
                format!(
                    "║ \x1b[1;32m{}\x1b[0m ║",
                    pad_or_truncate(&status_bar, inner_width)
                )
            };
            lines.push(footer_line);
            lines.push(format!("╚{}╝", "═".repeat(box_width - 2)));

            let visible_slice = if lines.len() > max_visible_rows {
                &lines[..max_visible_rows]
            } else {
                &lines[..]
            };
            for (idx, line) in visible_slice.iter().enumerate() {
                out.push_str("\r\x1b[2K");
                out.push_str(line);
                if idx + 1 < visible_slice.len() {
                    out.push('\n');
                }
            }
            out.push_str("\x1b[J");
            *prev_lines_drawn = visible_slice.len();
            let mut stdout = std::io::stdout().lock();
            let _ = write!(stdout, "\x1b[?25l\x1b[?7l{}", out);
            let _ = stdout.flush();
            return;
        }
    }

    // --- Mode 1 & Mode 2: Dashboard (+ Optional QueueSelector Dropdown Overlay) ---
    let top_border = format!("╔{}╗", "═".repeat(box_width - 2));
    let (header_color, header_text) = if shutting_down {
        (
            "\x1b[1;33m",
            format!(
                " RUSTY H.265 CONVERTER  |  [!] GRACEFUL EXIT: Draining scp-down & RAMDisk->HDD...  |  Saved: +{}  |  Elapsed: {}",
                format_bytes(total_saved_bytes),
                elapsed
            ),
        )
    } else {
        (
            "\x1b[1;36m",
            format!(
                " RUSTY H.265 CONVERTER  |  Saved: +{}  |  {}  |  Elapsed: {}  |  Queues: 'v'  |  Quit: 'q' / Ctrl+C",
                format_bytes(total_saved_bytes),
                ramdisk_banner,
                elapsed
            ),
        )
    };
    let header_line = format!(
        "║ {}{}\x1b[0m ║",
        header_color,
        pad_or_truncate(&header_text, inner_width)
    );
    let bottom_header = format!("╚{}╝", "═".repeat(box_width - 2));

    lines.push(top_border);
    lines.push(header_line);
    lines.push(bottom_header);

    if tui_state.mode == TuiViewMode::QueueSelector {
        let dd_title = " Select Mount Queue to Inspect (Up/Dn/j/k or 1-9, Enter: Open Snapshot, Esc/'v': Close) ";
        let dd_trimmed = truncate_middle(dd_title, box_width - 4);
        let dd_dashes = (box_width - 2).saturating_sub(str_display_width(&dd_trimmed));
        lines.push(format!(
            "┌\x1b[1;36m{}\x1b[0m{}┐",
            dd_trimmed,
            "─".repeat(dd_dashes)
        ));

        if mount_queues.is_empty() {
            lines.push(format!(
                "│ \x1b[90m{}\x1b[0m │",
                pad_or_truncate(" (No mount queues registered)", inner_width)
            ));
        } else {
            for (idx, mq) in mount_queues.iter().enumerate() {
                let is_selected = idx == tui_state.selected_queue_idx;
                let active_cnt = mq.in_flight_items.lock().map(|m| m.len()).unwrap_or(0);
                let (queued_cnt, ready_cnt) = mq.total_remaining_items();
                let total_cnt = active_cnt + queued_cnt;
                let roots_str = if mq.root_dirs.is_empty() {
                    mq.mount_point.clone()
                } else {
                    mq.root_dirs
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let prefix = if is_selected { ">" } else { " " };
                let row_text = format!(
                    " {} [{}] Root: {}  |  Mount: {} ({})  |  Total: {} files ({} active, {} queued [{} ready])",
                    prefix,
                    idx + 1,
                    roots_str,
                    mq.mount_point,
                    mq.info.drive_type.as_str(),
                    total_cnt,
                    active_cnt,
                    queued_cnt,
                    ready_cnt
                );
                if is_selected {
                    lines.push(format!(
                        "│ \x1b[1;30;46m{}\x1b[0m │",
                        pad_or_truncate(&row_text, inner_width)
                    ));
                } else {
                    lines.push(format!(
                        "│ \x1b[37m{}\x1b[0m │",
                        pad_or_truncate(&row_text, inner_width)
                    ));
                }
            }
        }
        lines.push(format!("└{}┘", "─".repeat(box_width - 2)));
    }

    let bar_width = (inner_width / 4).clamp(16, 48);

    for w in workers.values() {
        let title_with_saved = format!(
            "{} [Saved: +{}] ",
            w.header_title.trim_end(),
            format_bytes(w.bytes_saved_total)
        );
        let title_trimmed = truncate_middle(&title_with_saved, box_width - 4);
        let dash_count = (box_width - 2).saturating_sub(str_display_width(&title_trimmed));
        let title_color = if w.persistent_error.is_some() {
            "\x1b[1;31m"
        } else if w.is_remote {
            "\x1b[1;35m"
        } else {
            "\x1b[1;33m"
        };
        lines.push(format!(
            "┌{}{}\x1b[0m{}┐",
            title_color,
            title_trimmed,
            "─".repeat(dash_count)
        ));

        let line1 = if w.is_remote {
            format!(
                "Remote Dir: {}  |  Remaining Quota: {}",
                if w.current_root_dir.is_empty() {
                    "-"
                } else {
                    &w.current_root_dir
                },
                format_bytes(w.free_space_bytes)
            )
        } else {
            format!(
                "Root Dir  : {} (Queue left: {} [Ready: {}] | Free Space: {})",
                if w.current_root_dir.is_empty() {
                    "-"
                } else {
                    &w.current_root_dir
                },
                w.queued_items_remaining,
                w.ready_to_encode_count,
                format_bytes(w.free_space_bytes)
            )
        };
        lines.push(format!("│ {} │", pad_or_truncate(&line1, inner_width)));

        let line2 = format!(
            "Target    : {} (Size: {})",
            if w.current_target_label.is_empty() {
                "-"
            } else {
                &w.current_target_label
            },
            format_bytes(w.current_target_size_bytes)
        );
        lines.push(format!("│ {} │", pad_or_truncate(&line2, inner_width)));

        let batch_pct = if w.batch_total_files > 0 {
            (w.batch_completed_files as f64 / w.batch_total_files as f64) * 100.0
        } else if w.finished {
            100.0
        } else {
            0.0
        };
        let line3 = format!(
            "Status    : {}  |  Done: {}/{} ({:.0}%) [Conv:{} Ver:{} Moved/LowBR:{}/{} Err:{}]",
            w.phase,
            w.batch_completed_files,
            w.batch_total_files,
            batch_pct,
            w.batch_converted_files,
            w.batch_verified_files,
            w.batch_moved_files,
            w.batch_skipped_low_br_files,
            w.batch_error_files
        );
        lines.push(format!("│ {} │", pad_or_truncate(&line3, inner_width)));

        lines.push(format!("├{}┤", "─".repeat(box_width - 2)));

        if let Some(ref perr) = w.persistent_error {
            lines.push(format!(
                "│ \x1b[1;31m{}\x1b[0m │",
                pad_or_truncate(&format!("[ERROR] {}", perr), inner_width)
            ));
        } else if w.active_files.is_empty() {
            let idle_msg = if w.finished {
                "[DONE] Worker finished."
            } else {
                "... Waiting / probing / moving files..."
            };
            lines.push(format!(
                "│ \x1b[90m{}\x1b[0m │",
                pad_or_truncate(idle_msg, inner_width)
            ));
        } else {
            let mut active_sorted = w.active_files.clone();
            active_sorted.sort_by_key(|a| a.slot_id);
            for slot in active_sorted {
                let bar = render_progress_bar(slot.percent, bar_width);
                let time_info = format!(
                    "{}/{}",
                    format_duration_hms(slot.out_time_secs),
                    format_duration_hms(slot.total_duration_secs)
                );
                let meta = format!(
                    "{:5.1}% | {:>11} | {:>11} | {:>5}k {} | {}",
                    slot.percent,
                    time_info,
                    slot.speed,
                    slot.bitrate_kbps,
                    slot.codec,
                    slot.filename
                );
                let meta_padded =
                    pad_or_truncate(&meta, inner_width.saturating_sub(bar_width + 3));
                lines.push(format!("│ [{}] {} │", bar, meta_padded));
            }
        }

        lines.push(format!("└{}┘", "─".repeat(box_width - 2)));
    }

    let visible_slice = if lines.len() > max_visible_rows {
        &lines[..max_visible_rows]
    } else {
        &lines[..]
    };

    for (idx, line) in visible_slice.iter().enumerate() {
        out.push_str("\r\x1b[2K");
        out.push_str(line);
        if idx + 1 < visible_slice.len() {
            out.push('\n');
        }
    }
    // Clear any remaining lines below the current frame to the bottom of the screen
    out.push_str("\x1b[J");

    *prev_lines_drawn = visible_slice.len();

    let mut stdout = std::io::stdout().lock();
    let _ = write!(stdout, "\x1b[?25l\x1b[?7l{}", out);
    let _ = stdout.flush();
}

fn staging_mode_label(drive_type: DriveType, config: &AppConfig) -> String {
    let active = config.ENABLE_RAMDISK
        || (drive_type == DriveType::Hdd && config.USE_LOCAL_STAGING_FOR_HDD);
    if !active {
        "Direct".to_string()
    } else if config.ENABLE_RAMDISK {
        format!("RAMDisk ({:.1}G)", config.RAMDISK_SIZE_GB)
    } else {
        "NVMe-Staged".to_string()
    }
}

fn resolve_config_path(cli_config: &Path) -> PathBuf {
    if cli_config.exists() {
        return cli_config.to_path_buf();
    }
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let candidate = exe_dir.join(cli_config);
            if candidate.exists() {
                return candidate;
            }
            if let Some(project_root) = exe_dir.parent().and_then(|p| p.parent()) {
                let root_candidate = project_root.join(cli_config);
                if root_candidate.exists() {
                    return root_candidate;
                }
            }
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(cli_config)
}

fn run_app() -> Result<()> {
    let cli = Cli::parse();
    let log_config_path = resolve_config_path(&cli.log_config);
    let log_cfg = LoggingConfig::load(&log_config_path);
    init_global_logger(log_cfg.clone());

    log_event(
        LogLevel::Info,
        format!(
            "=== Starting rusty_h265_converter (log_config='{}', log_file='{}', level={}) ===",
            log_config_path.display(),
            log_cfg.log_file.display(),
            log_cfg.level.as_str()
        ),
    );

    let config_path = resolve_config_path(&cli.config);
    let config = AppConfig::load(&config_path)?;
    init_global_app_config(config.clone());
    global_metrics().set_local_identity(
        &config.effective_local_nodename(),
        local_host_os_label(),
    );

    // Layer 1: Instant static config & custom FFMPEG_*_ARGS override validation
    let validated_target = codecs::validate_config_targets(&config)?;

    // Layer 2: Discovery-first FFmpeg pre-flight probe (5s 256x144 synthetic encode + full-stream decode)
    // with user-friendly pure-ASCII waiting animation.
    let mut discovery_spinner =
        codecs::DiscoverySpinner::start(&validated_target.label(), cli.verbose);
    let resolved_local = match codecs::discover_and_verify_local_profile(
        &config,
        &validated_target,
        &discovery_spinner,
    ) {
        Ok(prof) => {
            let _ = GLOBAL_TARGET_PROFILE.set(prof.clone());
            prof
        }
        Err(err) => {
            discovery_spinner.finish_err(&format!("Local pre-flight failed: {}", err));
            return Err(err);
        }
    };

    if cli.dry_run {
        let mut remote_results: Vec<(String, Result<(String, String, String)>)> = Vec::new();
        if config.HIGH_PERFORMANCE_DISTRIBUTED_SYSTEM && !config.WORKER_NODES.is_empty() {
            for node in &config.WORKER_NODES {
                let node_label = format!(
                    "{} ({}@{}:{})",
                    node.effective_nodename(),
                    node.USERNAME,
                    node.IP_ADDRESS,
                    node.SSH_PORT
                );
                let res = probe_remote_node_for_dry_run(
                    node,
                    &config,
                    &validated_target,
                    &discovery_spinner,
                );
                remote_results.push((node_label, res));
            }
        }
        discovery_spinner.finish_ok(&format!(
            "Pre-flight discovery complete for [{}]",
            validated_target.label()
        ));

        println!("\n=== Dry-Run Configuration & Encoder Verification Summary ===");
        println!("Config File    : {}", config_path.display());
        println!(
            "Target Profile : video='{}', audio='{}', container='{}' (ext='{}', suffix='{}')",
            validated_target.video.as_str(),
            validated_target.audio.as_str(),
            validated_target.container.as_str(),
            config.OUTPUT_EXTENSION,
            config.OUTPUT_SUFFIX
        );
        println!("Local Node     : {} ({})", config.effective_local_nodename(), local_host_os_label());
        println!("  Video Encoder: {} ({})", resolved_local.video_encoder_name, resolved_local.encoder_args.join(" "));
        println!("  Audio Encoder: {} ({})", resolved_local.audio_encoder_name, resolved_local.audio_args.join(" "));
        if !resolved_local.muxer_args.is_empty() {
            println!("  Muxer Flags  : {}", resolved_local.muxer_args.join(" "));
        }
        if !resolved_local.hwaccel_args.is_empty() {
            println!("  HWAccel Args : {}", resolved_local.hwaccel_args.join(" "));
        }

        let mut any_remote_failed = false;
        if !remote_results.is_empty() {
            println!("\nRemote Worker Nodes:");
            for (node_label, res) in &remote_results {
                match res {
                    Ok((ffbin, v_enc, a_enc)) => {
                        println!(
                            "  [OK]   {} -> {} + {} (ffmpeg: {})",
                            node_label, v_enc, a_enc, ffbin
                        );
                    }
                    Err(e) => {
                        any_remote_failed = true;
                        println!("  [FAIL] {} -> {}", node_label, e);
                    }
                }
            }
        }
        println!("============================================================");
        if any_remote_failed {
            anyhow::bail!("Dry-run failed: one or more configured remote worker nodes failed pre-flight verification.");
        }
        return Ok(());
    }

    discovery_spinner.finish_ok(&format!(
        "Local encoder verified: {}",
        resolved_local.display_name
    ));
    let failed_dirs_log = PathBuf::from(&config.FAILED_DIRS_PATH);

    let mut mount_groups: BTreeMap<String, (MountInfo, Vec<PathBuf>)> = BTreeMap::new();

    for dir in &cli.input_dirs {
        if !dir.exists() {
            let err = anyhow::anyhow!("Input directory '{}' does not exist", dir.display());
            record_failed_dir(
                &failed_dirs_log,
                dir,
                None,
                &format!(
                    "[Orchestrator] Input directory '{}' does not exist; logging to '{}' and skipping",
                    dir.display(),
                    failed_dirs_log.display()
                ),
                &err,
            );
            continue;
        }
        let abs_dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
        let info = get_mount_info(&abs_dir);
        let entry = mount_groups
            .entry(info.mount_point.clone())
            .or_insert_with(|| (info, Vec::new()));
        if !entry.1.contains(&abs_dir) {
            entry.1.push(abs_dir);
        }
    }

    if mount_groups.is_empty() {
        vlog!(
            cli.verbose,
            "[Orchestrator] No valid input directories to process. Exiting."
        );
        return Ok(());
    }

    let provisioned_ramdisk = match provision_ramdisk(&config, cli.verbose) {
        Ok(rd) => rd,
        Err(err) => {
            print_error_with_stack_trace(
                "Failed to provision RAMDisk (continuing without RAMDisk)",
                &err,
            );
            None
        }
    };

    let ramdisk_banner = match &provisioned_ramdisk {
        Some(rd) => format!(
            "RAMDisk: {} ({:.1} GB @ {})",
            rd.name,
            rd.size_gb,
            rd.mount_path.display()
        ),
        None if config.USE_LOCAL_STAGING_FOR_HDD => {
            format!("Staging: {}", config.LOCAL_STAGING_DIR)
        }
        None => "Staging: Disabled".to_string(),
    };

    let completed_journal_path = PathBuf::from(&config.TASK_JOURNAL_PATH)
        .with_file_name("completed_files.journal");
    init_completed_files_journal(
        completed_journal_path,
        Path::new(&config.TASK_JOURNAL_PATH),
    );

    let broken_journal_path = PathBuf::from(&config.TASK_JOURNAL_PATH)
        .with_file_name("broken_files.journal");
    init_broken_files_journal(broken_journal_path);

    let subdir_trackers: Arc<Mutex<HashMap<PathBuf, SubdirTracker>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let next_item_id = AtomicU64::new(1);
    let shared_ui: Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>> =
        Arc::new(Mutex::new(BTreeMap::new()));

    let mut mount_queues: Vec<Arc<MountQueueState>> = Vec::new();

    for (mount_point, (info, dirs)) in &mount_groups {
        let queue = populate_mount_queue_for_dirs(
            mount_point,
            dirs,
            &config,
            &next_item_id,
            &subdir_trackers,
            cli.verbose,
        );
        let total_len = queue.len();
        let mq_state = Arc::new(MountQueueState {
            mount_point: mount_point.clone(),
            root_dirs: dirs.clone(),
            info: info.clone(),
            raw_queue: Mutex::new(queue),
            queue: Mutex::new(VecDeque::new()),
            in_flight_items: Mutex::new(BTreeMap::new()),
            verifier_done: AtomicBool::new(total_len == 0),
            io_lock: Mutex::new(()),
            active_remote_managers: AtomicUsize::new(0),
            total_enqueued: AtomicUsize::new(total_len),
            local_worker_finished: AtomicBool::new(total_len == 0),
            in_flight_local_tasks: AtomicUsize::new(0),
        });
        mount_queues.push(mq_state);

        let worker_key = format!("local:{}", mount_point);
        if let Ok(mut map) = shared_ui.lock() {
            map.insert(
                worker_key.clone(),
                WorkerUiSnapshot {
                    worker_key,
                    is_remote: false,
                    header_title: format!(
                        " Local Mount: {} [{} | {} | 1x Worker-Group (Stager/Conv/Flusher) | {} | {}]",
                        mount_point,
                        info.device,
                        info.drive_type.as_str(),
                        staging_mode_label(info.drive_type, &config),
                        detect_encoder_profile(&config).name
                    ),
                    free_space_bytes: dirs
                        .first()
                        .map(|d| get_available_free_space_bytes(d))
                        .unwrap_or(0),
                    queued_items_remaining: total_len,
                    ready_to_encode_count: 0,
                    batch_total_files: total_len,
                    phase: "Ready".to_string(),
                    ..Default::default()
                },
            );
        }
    }

    if config.HIGH_PERFORMANCE_DISTRIBUTED_SYSTEM {
        for node in &config.WORKER_NODES {
            let worker_key = format!("remote:{}@{}", node.USERNAME, node.IP_ADDRESS);
            if let Ok(mut map) = shared_ui.lock() {
                map.insert(
                    worker_key.clone(),
                    WorkerUiSnapshot {
                        worker_key,
                        is_remote: true,
                        header_title: format!(
                            " Remote Node: {}@{}:{} [{} | Max: {:.1} GB]",
                            node.USERNAME,
                            node.IP_ADDRESS,
                            node.SSH_PORT,
                            node.OS_TYPE,
                            node.MAX_SIZE_IN_GB
                        ),
                        free_space_bytes: (node.MAX_SIZE_IN_GB * 1024.0 * 1024.0 * 1024.0) as u64,
                        phase: "Initializing Remote Manager...".to_string(),
                        ..Default::default()
                    },
                );
            }
        }
    }

    let tui_state = Arc::new(Mutex::new(TuiInteractiveState::default()));
    install_signal_handlers();
    let _tty_input_guard =
        TerminalInputGuard::start(Arc::clone(&tui_state), mount_queues.clone());

    // Periodic Telemetry Snapshot Publisher Thread -> writes /tmp/rusty_h265_telemetry.json
    let telemetry_stop = Arc::new(AtomicBool::new(false));
    let telemetry_thread = {
        let stop_ref = Arc::clone(&telemetry_stop);
        let mqs_for_tel = mount_queues.clone();
        let snap_path = PathBuf::from(&config.TELEMETRY_SNAPSHOT_PATH);
        let interval_ms = config.MONITOR_INTERVAL_MSEC.clamp(500, 10_000);
        thread::spawn(move || {
            while !stop_ref.load(Ordering::Relaxed) {
                for mq in &mqs_for_tel {
                    if let Some(first_dir) = mq.root_dirs.first() {
                        if let Some((free_b, total_b)) = get_mount_space_stats_syscall(first_dir) {
                            global_metrics().update_mount_space(
                                &mq.mount_point,
                                mq.info.drive_type.as_str(),
                                free_b,
                                total_b,
                            );
                        }
                    }
                    let raw_len = mq.raw_queue.lock().map(|q| q.len()).unwrap_or(0);
                    let ready_len = mq.queue.lock().map(|q| q.len()).unwrap_or(0);
                    let active_local = mq.in_flight_local_tasks.load(Ordering::Relaxed);
                    let active_remote = mq.active_remote_managers.load(Ordering::Relaxed);
                    global_metrics().update_mount_queues(
                        &mq.mount_point,
                        mq.info.drive_type.as_str(),
                        raw_len,
                        ready_len,
                        active_local,
                        active_remote,
                    );
                }
                let _ = global_metrics().write_snapshot_atomic(&snap_path);
                let sleep_steps = (interval_ms / 200).max(1);
                for _ in 0..sleep_steps {
                    if stop_ref.load(Ordering::Relaxed) || is_shutdown_requested() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(200));
                }
            }
            let _ = global_metrics().write_snapshot_atomic(&snap_path);
        })
    };

    // Auto-spawn master metrics & log shipper companion process if enabled in config.yaml
    let mut master_agent_child = if (config.PROMETHEUS_ENABLED || config.LOKI_ENABLED)
        && config.AUTO_SPAWN_METRICS_AGENT
    {
        let script_path = resolve_config_path(Path::new(&config.METRICS_AGENT_SCRIPT_PATH));
        if script_path.exists() {
            let pid_str = std::process::id().to_string();
            let cfg_str = config_path.to_string_lossy().to_string();
            let log_cfg_str = log_config_path.to_string_lossy().to_string();
            let script_str = script_path.to_string_lossy().to_string();
            match Command::new(platform::python_command_name())
                .args([
                    &script_str,
                    "--role",
                    "master",
                    "--config",
                    &cfg_str,
                    "--log-config",
                    &log_cfg_str,
                    "--parent-pid",
                    &pid_str,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    log_event(
                        LogLevel::Info,
                        format!(
                            "[Observability] Spawned master metrics & Loki companion agent (PID: {}, script: '{}')",
                            child.id(),
                            script_path.display()
                        ),
                    );
                    Some(child)
                }
                Err(e) => {
                    log_event(
                        LogLevel::Warn,
                        format!(
                            "[Observability] Could not spawn master metrics agent '{}': {}",
                            script_path.display(),
                            e
                        ),
                    );
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    let mut worker_handles = Vec::new();

    for mq in &mount_queues {
        let mq_clone = Arc::clone(mq);
        let all_mqs_clone = mount_queues.clone();
        let trackers_clone = Arc::clone(&subdir_trackers);
        let ui_clone = Arc::clone(&shared_ui);
        let cfg_clone = config.clone();
        let verbose = cli.verbose;
        worker_handles.push(thread::spawn(move || {
            run_local_mount_worker(
                mq_clone,
                all_mqs_clone,
                trackers_clone,
                ui_clone,
                cfg_clone,
                verbose,
            );
        }));
    }

    if config.HIGH_PERFORMANCE_DISTRIBUTED_SYSTEM && !config.WORKER_NODES.is_empty() {
        let journal = Arc::new(TaskJournal::new(PathBuf::from(&config.TASK_JOURNAL_PATH)));
        let next_task_id = Arc::new(AtomicU64::new(1));

        for node in &config.WORKER_NODES {
            let node_clone = node.clone();
            let mqs_clone = mount_queues.clone();
            let trackers_clone = Arc::clone(&subdir_trackers);
            let journal_clone = Arc::clone(&journal);
            let tid_clone = Arc::clone(&next_task_id);
            let ui_clone = Arc::clone(&shared_ui);
            let cfg_clone = config.clone();
            let verbose = cli.verbose;

            worker_handles.push(thread::spawn(move || {
                run_remote_machine_manager(
                    node_clone,
                    mqs_clone,
                    trackers_clone,
                    journal_clone,
                    tid_clone,
                    ui_clone,
                    cfg_clone,
                    verbose,
                );
            }));
        }
    }

    if !cli.verbose {
        let tui_stop = Arc::new(AtomicBool::new(false));
        let tui_stop_clone = Arc::clone(&tui_stop);
        let workers_for_tui = Arc::clone(&shared_ui);
        let mqs_for_tui = mount_queues.clone();
        let tui_state_for_tui = Arc::clone(&tui_state);
        let banner_for_tui = ramdisk_banner.clone();
        let start_time = Instant::now();

        let tui_thread = thread::spawn(move || {
            let mut prev_lines = 0usize;
            let mut prev_width = 0usize;
            let mut prev_height = 0usize;
            let mut prev_mode = TuiViewMode::Dashboard;
            while !tui_stop_clone.load(Ordering::Relaxed) {
                if SHUTDOWN_SIGNAL_NUM.load(Ordering::Relaxed) != 0 {
                    request_graceful_shutdown("OS termination signal");
                }
                let tui_state_snap = tui_state_for_tui
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or_default();
                if let Ok(guard) = workers_for_tui.lock() {
                    draw_tui_frame(
                        &guard,
                        &mqs_for_tui,
                        &tui_state_snap,
                        &banner_for_tui,
                        start_time,
                        &mut prev_lines,
                        &mut prev_width,
                        &mut prev_height,
                        &mut prev_mode,
                    );
                }
                thread::sleep(Duration::from_millis(120));
            }
            let tui_state_snap = tui_state_for_tui
                .lock()
                .map(|s| s.clone())
                .unwrap_or_default();
            if let Ok(guard) = workers_for_tui.lock() {
                draw_tui_frame(
                    &guard,
                    &mqs_for_tui,
                    &tui_state_snap,
                    &banner_for_tui,
                    start_time,
                    &mut prev_lines,
                    &mut prev_width,
                    &mut prev_height,
                    &mut prev_mode,
                );
            }
            let mut stdout = std::io::stdout().lock();
            let _ = write!(stdout, "\x1b[?25h\x1b[?7h\n");
            let _ = stdout.flush();
        });

        for h in worker_handles {
            let _ = h.join();
        }
        tui_stop.store(true, Ordering::Relaxed);
        let _ = tui_thread.join();
    } else {
        for h in worker_handles {
            let _ = h.join();
        }
    }

    flush_incomplete_failed_subdirs(&subdir_trackers, &config);

    global_metrics().set_converter_active(false);
    telemetry_stop.store(true, Ordering::Relaxed);
    let _ = telemetry_thread.join();

    drop(_tty_input_guard);

    if let Some(rd) = provisioned_ramdisk {
        rd.cleanup(cli.verbose);
    }

    log_event(
        LogLevel::Info,
        "=== All workers finished. Exiting rusty_h265_converter ===",
    );

    if let Some(ref mut agent_child) = master_agent_child {
        // Give the master companion agent a brief moment to flush final metrics & Loki logs
        thread::sleep(Duration::from_millis(250));
        let _ = agent_child.kill();
        let _ = agent_child.wait();
    }
    Ok(())
}

fn main() {
    std::env::set_var("RUST_BACKTRACE", "full");
    std::env::set_var("RUST_LIB_BACKTRACE", "1");

    std::panic::set_hook(Box::new(|panic_info| {
        let bt = Backtrace::force_capture();
        let bt_str = bt.to_string();
        log_error_with_bt(&format!("PANIC: {}", panic_info), &bt_str);
        eprintln!("\r\x1b[2K\x1b[?25h\x1b[?7h");
        eprintln!("================================================================");
        eprintln!("PANIC: {}", panic_info);
        eprintln!("----------------------------------------------------------------");
        eprintln!("Stack Trace:\n{}", bt_str);
        eprintln!("================================================================\n");
    }));

    if let Err(err) = run_app() {
        print_error_with_stack_trace("Fatal error in rusty_h265_converter", &err);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, target_os = "windows"))]
mod windows_tests;

#[cfg(all(test, target_os = "linux"))]
mod linux_tests;
