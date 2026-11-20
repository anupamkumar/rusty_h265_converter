use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[cfg(unix)]
use std::ffi::CString;
use std::fmt::Write as FmtWrite;
use std::fs;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "windows")]
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ============================================================================
// 1. Direct OS System Calls (Filesystem statfs/statvfs, Thread CPU Time, Hostname)
// ============================================================================

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Copy, Clone)]
struct Fsid {
    val: [i32; 2],
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct DarwinStatfs64 {
    f_bsize: u32,
    f_iosize: i32,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_fsid: Fsid,
    f_owner: u32,
    f_type: u32,
    f_flags: u32,
    f_fssubtype: u32,
    f_fstypename: [i8; 16],
    f_mntonname: [i8; 1024],
    f_mntfromname: [i8; 1024],
    f_flags_ext: u32,
    f_reserved: [u32; 7],
}

#[cfg(target_os = "macos")]
extern "C" {
    #[cfg_attr(target_arch = "x86_64", link_name = "statfs$INODE64")]
    fn statfs(path: *const std::ffi::c_char, buf: *mut DarwinStatfs64) -> i32;
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct LinuxStatvfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
    __f_spare: [i32; 6],
}

#[cfg(target_os = "linux")]
extern "C" {
    fn statvfs(path: *const std::ffi::c_char, buf: *mut LinuxStatvfs) -> i32;
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
extern "system" {
    fn GetDiskFreeSpaceExW(
        lp_directory_name: *const u16,
        lp_free_bytes_available_to_caller: *mut u64,
        lp_total_number_of_bytes: *mut u64,
        lp_total_number_of_free_bytes: *mut u64,
    ) -> i32;
}

/// Queries `(available_free_bytes, total_capacity_bytes)` for `path` (or its nearest existing ancestor)
/// using a direct in-process `statfs` / `statvfs` / `GetDiskFreeSpaceExW` system call without spawning `df`.
pub fn get_mount_space_stats_syscall(path: &Path) -> Option<(u64, u64)> {
    let mut probe = path.to_path_buf();
    while !probe.exists() {
        if let Some(parent) = probe.parent() {
            if parent.as_os_str().is_empty() {
                probe = Path::new(".").to_path_buf();
                break;
            }
            probe = parent.to_path_buf();
        } else {
            probe = Path::new(".").to_path_buf();
            break;
        }
    }

    #[cfg(unix)]
    let c_path = CString::new(probe.as_os_str().as_bytes()).ok()?;

    #[cfg(target_os = "macos")]
    unsafe {
        let mut buf: DarwinStatfs64 = std::mem::zeroed();
        if statfs(c_path.as_ptr(), &mut buf as *mut DarwinStatfs64) == 0 {
            let bsize = (buf.f_bsize as u64).max(1);
            let avail_bytes = buf.f_bavail.saturating_mul(bsize);
            let total_bytes = buf.f_blocks.saturating_mul(bsize);
            if total_bytes > 0 {
                return Some((avail_bytes, total_bytes));
            }
        }
    }

    #[cfg(target_os = "linux")]
    unsafe {
        let mut buf: LinuxStatvfs = std::mem::zeroed();
        if statvfs(c_path.as_ptr(), &mut buf as *mut LinuxStatvfs) == 0 {
            let frsize = if buf.f_frsize > 0 {
                buf.f_frsize
            } else {
                buf.f_bsize.max(1)
            };
            let avail_bytes = buf.f_bavail.saturating_mul(frsize);
            let total_bytes = buf.f_blocks.saturating_mul(frsize);
            if total_bytes > 0 {
                return Some((avail_bytes, total_bytes));
            }
        }
    }

    #[cfg(target_os = "windows")]
    unsafe {
        let mut wide: Vec<u16> = probe.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut avail_bytes: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut free_bytes: u64 = 0;
        if GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut avail_bytes as *mut u64,
            &mut total_bytes as *mut u64,
            &mut free_bytes as *mut u64,
        ) != 0
            && total_bytes > 0
        {
            return Some((avail_bytes, total_bytes));
        }
    }

    None
}

// --- Direct Per-Thread CPU Time Syscall (Mach thread_info on macOS / clock_gettime on Linux) ---

#[cfg(target_os = "macos")]
#[repr(C)]
struct MachTimeValue {
    seconds: i32,
    microseconds: i32,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MachThreadBasicInfo {
    user_time: MachTimeValue,
    system_time: MachTimeValue,
    cpu_usage: i32,
    policy: i32,
    run_state: i32,
    flags: i32,
    suspend_count: i32,
    sleep_time: i32,
}

#[cfg(target_os = "macos")]
const THREAD_BASIC_INFO_FLAVOR: u32 = 3;
#[cfg(target_os = "macos")]
const THREAD_BASIC_INFO_COUNT: u32 = 10;

#[cfg(target_os = "macos")]
extern "C" {
    static mach_task_self_: u32;
    fn mach_thread_self() -> u32;
    fn thread_info(
        target_act: u32,
        flavor: u32,
        thread_info_out: *mut MachThreadBasicInfo,
        thread_info_out_cnt: *mut u32,
    ) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct LinuxTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[cfg(target_os = "linux")]
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;

#[cfg(target_os = "linux")]
extern "C" {
    fn clock_gettime(clk_id: i32, tp: *mut LinuxTimespec) -> i32;
}

#[cfg(target_os = "windows")]
#[repr(C)]
#[derive(Clone, Copy)]
struct WinFileTime {
    dw_low_date_time: u32,
    dw_high_date_time: u32,
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentThread() -> isize;
    fn GetThreadTimes(
        h_thread: isize,
        lp_creation_time: *mut WinFileTime,
        lp_exit_time: *mut WinFileTime,
        lp_kernel_time: *mut WinFileTime,
        lp_user_time: *mut WinFileTime,
    ) -> i32;
}

/// Returns `(user_cpu_seconds, system_cpu_seconds)` consumed by the calling OS thread
/// using a direct Mach `thread_info(THREAD_BASIC_INFO)`, POSIX `clock_gettime`, or Win32 `GetThreadTimes` syscall.
pub fn get_current_thread_cpu_time_secs() -> (f64, f64) {
    #[cfg(target_os = "macos")]
    unsafe {
        let th = mach_thread_self();
        let mut info: MachThreadBasicInfo = std::mem::zeroed();
        let mut count: u32 = THREAD_BASIC_INFO_COUNT;
        let kr = thread_info(
            th,
            THREAD_BASIC_INFO_FLAVOR,
            &mut info as *mut MachThreadBasicInfo,
            &mut count as *mut u32,
        );
        let _ = mach_port_deallocate(mach_task_self_, th);
        if kr == 0 {
            let user = (info.user_time.seconds as f64)
                + (info.user_time.microseconds as f64) / 1_000_000.0;
            let sys = (info.system_time.seconds as f64)
                + (info.system_time.microseconds as f64) / 1_000_000.0;
            return (user.max(0.0), sys.max(0.0));
        }
    }

    #[cfg(target_os = "linux")]
    unsafe {
        let mut ts = LinuxTimespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts as *mut LinuxTimespec) == 0 {
            let user = (ts.tv_sec as f64) + (ts.tv_nsec as f64) / 1_000_000_000.0;
            return (user.max(0.0), 0.0);
        }
    }

    #[cfg(target_os = "windows")]
    unsafe {
        let mut creation = WinFileTime {
            dw_low_date_time: 0,
            dw_high_date_time: 0,
        };
        let mut exit = creation;
        let mut kernel = creation;
        let mut user = creation;
        let th = GetCurrentThread();
        if GetThreadTimes(
            th,
            &mut creation as *mut WinFileTime,
            &mut exit as *mut WinFileTime,
            &mut kernel as *mut WinFileTime,
            &mut user as *mut WinFileTime,
        ) != 0
        {
            let user_100ns =
                ((user.dw_high_date_time as u64) << 32) | (user.dw_low_date_time as u64);
            let kernel_100ns =
                ((kernel.dw_high_date_time as u64) << 32) | (kernel.dw_low_date_time as u64);
            return (
                (user_100ns as f64) / 10_000_000.0,
                (kernel_100ns as f64) / 10_000_000.0,
            );
        }
    }

    (0.0, 0.0)
}

#[cfg(unix)]
extern "C" {
    fn gethostname(name: *mut std::ffi::c_char, len: usize) -> i32;
}

/// Returns the local system hostname via POSIX `gethostname` syscall or Windows `COMPUTERNAME`.
pub fn gethostname_syscall() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        let ok = unsafe {
            gethostname(buf.as_mut_ptr() as *mut std::ffi::c_char, buf.len() - 1) == 0
        };
        if ok {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let raw = String::from_utf8_lossy(&buf[..end]).trim().to_string();
            if !raw.is_empty() {
                return raw;
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(name) = std::env::var("COMPUTERNAME") {
            let trimmed = name.trim().to_string();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
    }
    "local-host".to_string()
}

pub fn local_host_os_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Mac"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Linux"
    }
}

// ============================================================================
// 2. Per-Thread Wait-Time Taxonomy & ThreadWaitTracker
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum WaitReason {
    UpstreamQueueEmpty,
    RamdiskQuotaFull,
    RemoteQuotaFull,
    LookaheadBufferFull,
    FlusherPriorityYield,
    IoLock,
}

impl WaitReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            WaitReason::UpstreamQueueEmpty => "upstream_queue_empty",
            WaitReason::RamdiskQuotaFull => "ramdisk_quota_full",
            WaitReason::RemoteQuotaFull => "remote_quota_full",
            WaitReason::LookaheadBufferFull => "lookahead_buffer_full",
            WaitReason::FlusherPriorityYield => "flusher_priority_yield",
            WaitReason::IoLock => "io_lock",
        }
    }

    pub const ALL: [WaitReason; 6] = [
        WaitReason::UpstreamQueueEmpty,
        WaitReason::RamdiskQuotaFull,
        WaitReason::RemoteQuotaFull,
        WaitReason::LookaheadBufferFull,
        WaitReason::FlusherPriorityYield,
        WaitReason::IoLock,
    ];
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskWaitBreakdown {
    pub thread_wait_in_secs: f64,
    pub upstream_queue_wait_in_secs: f64,
    pub quota_wait_in_secs: f64,
    pub lookahead_wait_in_secs: f64,
    pub flusher_priority_wait_in_secs: f64,
    pub io_lock_wait_in_secs: f64,
    pub wait_cycles: u64,
}

impl TaskWaitBreakdown {
    pub fn add_io_lock_wait(&mut self, lock_wait_secs: f64) {
        if lock_wait_secs > 0.0 {
            self.io_lock_wait_in_secs += lock_wait_secs;
            self.thread_wait_in_secs += lock_wait_secs;
        }
    }
}

/// Owned by each worker thread (`verifier`, `stager`, `converter`, `flusher`,
/// `remote_dispatcher`, `scp_upload`, `remote_encode`, `scp_copyback`) to track
/// per-task and cumulative wait times across all `WaitReason` categories.
pub struct ThreadWaitTracker {
    pub nodename: String,
    pub host_os: String,
    pub worker_group: String,
    pub worker_thread: String,
    pub mount_point: String,
    current_task_waits: BTreeMap<WaitReason, f64>,
    current_task_cycles: u64,
    last_cpu_sample: (f64, f64),
}

impl ThreadWaitTracker {
    pub fn new(
        nodename: impl Into<String>,
        host_os: impl Into<String>,
        worker_group: impl Into<String>,
        worker_thread: impl Into<String>,
        mount_point: impl Into<String>,
    ) -> Self {
        let tracker = Self {
            nodename: nodename.into(),
            host_os: host_os.into(),
            worker_group: worker_group.into(),
            worker_thread: worker_thread.into(),
            mount_point: mount_point.into(),
            current_task_waits: BTreeMap::new(),
            current_task_cycles: 0,
            last_cpu_sample: get_current_thread_cpu_time_secs(),
        };
        global_metrics().register_thread(
            &tracker.nodename,
            &tracker.host_os,
            &tracker.worker_group,
            &tracker.worker_thread,
        );
        tracker
    }

    /// Sleeps for `duration` while recording the exact elapsed wait time under `reason`.
    pub fn sleep_with_reason(&mut self, reason: WaitReason, duration: Duration) {
        let state_code = match reason {
            WaitReason::UpstreamQueueEmpty => 0, // Idle / waiting on upstream queue
            _ => 2,                              // Blocked on quota, lookahead, or lock
        };
        global_metrics().set_thread_state(
            &self.nodename,
            &self.host_os,
            &self.worker_group,
            &self.worker_thread,
            state_code,
        );

        let start = Instant::now();
        std::thread::sleep(duration);
        let elapsed = start.elapsed().as_secs_f64();
        self.record_wait_duration(reason, elapsed, true);
    }

    /// Records an already-measured wait duration (such as `io_lock` mutex wait) under `reason`.
    pub fn record_wait_duration(&mut self, reason: WaitReason, secs: f64, count_cycle: bool) {
        let entry = self.current_task_waits.entry(reason).or_insert(0.0);
        *entry += secs.max(0.0);
        if count_cycle {
            self.current_task_cycles += 1;
        }
        let current_for_reason = *entry;
        global_metrics().record_thread_wait_tick(
            &self.nodename,
            &self.host_os,
            &self.worker_group,
            &self.worker_thread,
            reason,
            secs.max(0.0),
            current_for_reason,
            count_cycle,
        );
    }

    /// Called when the thread dequeues a task and transitions into active execution.
    /// Flushes accumulated pre-task wait times into the per-task wait histogram, resets
    /// `current_wait_in_secs` gauges to `0.0`, sets thread state to `1 (Active)`, and returns
    /// the `TaskWaitBreakdown` for structured logging.
    pub fn begin_active_task(&mut self, stage: &str) -> TaskWaitBreakdown {
        let mut breakdown = TaskWaitBreakdown {
            wait_cycles: self.current_task_cycles,
            ..Default::default()
        };

        for (&reason, &secs) in &self.current_task_waits {
            breakdown.thread_wait_in_secs += secs;
            match reason {
                WaitReason::UpstreamQueueEmpty => breakdown.upstream_queue_wait_in_secs += secs,
                WaitReason::RamdiskQuotaFull | WaitReason::RemoteQuotaFull => {
                    breakdown.quota_wait_in_secs += secs
                }
                WaitReason::LookaheadBufferFull => breakdown.lookahead_wait_in_secs += secs,
                WaitReason::FlusherPriorityYield => {
                    breakdown.flusher_priority_wait_in_secs += secs
                }
                WaitReason::IoLock => breakdown.io_lock_wait_in_secs += secs,
            }
        }

        global_metrics().finalize_thread_wait_for_task(
            &self.nodename,
            &self.host_os,
            &self.worker_group,
            &self.worker_thread,
            stage,
            &self.current_task_waits,
        );

        self.current_task_waits.clear();
        self.current_task_cycles = 0;
        self.refresh_thread_cpu_time();
        breakdown
    }

    /// Called when the thread completes a stage of duration `active_secs`.
    pub fn complete_active_task(&mut self, stage: &str, outcome: &str, active_secs: f64) {
        self.refresh_thread_cpu_time();
        global_metrics().record_stage_completion(
            &self.nodename,
            &self.host_os,
            &self.worker_group,
            &self.worker_thread,
            stage,
            outcome,
            active_secs,
        );
    }

    pub fn record_error(&mut self, error_type: &str) {
        global_metrics().record_thread_error(
            &self.nodename,
            &self.host_os,
            &self.worker_group,
            &self.worker_thread,
            error_type,
        );
    }

    pub fn mark_finished(&mut self) {
        self.refresh_thread_cpu_time();
        global_metrics().set_thread_state(
            &self.nodename,
            &self.host_os,
            &self.worker_group,
            &self.worker_thread,
            3,
        );
    }

    fn refresh_thread_cpu_time(&mut self) {
        let (u, s) = get_current_thread_cpu_time_secs();
        let du = (u - self.last_cpu_sample.0).max(0.0);
        let ds = (s - self.last_cpu_sample.1).max(0.0);
        self.last_cpu_sample = (u, s);
        if du > 0.0 || ds > 0.0 {
            global_metrics().add_thread_cpu_time(
                &self.nodename,
                &self.host_os,
                &self.worker_group,
                &self.worker_thread,
                du,
                ds,
            );
        }
    }
}

// ============================================================================
// 3. Serializable Histogram & Telemetry Snapshot Structures
// ============================================================================

pub const STAGE_DURATION_BUCKETS: &[f64] = &[
    0.05, 0.25, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 180.0, 600.0, 1800.0,
];

pub const WAIT_DURATION_BUCKETS: &[f64] = &[
    0.01, 0.05, 0.15, 0.5, 1.0, 2.5, 5.0, 15.0, 30.0, 60.0, 300.0,
];

pub const DISK_CHUNK_LATENCY_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
];

pub const COMPRESSION_RATIO_BUCKETS: &[f64] = &[
    0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.2,
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistogramSnapshot {
    pub bounds: Vec<f64>,
    /// Cumulative bucket counts matching `bounds.len()`, plus `+Inf` in `count`.
    pub bucket_counts: Vec<u64>,
    pub count: u64,
    pub sum: f64,
}

impl HistogramSnapshot {
    pub fn new(bounds: &[f64]) -> Self {
        Self {
            bounds: bounds.to_vec(),
            bucket_counts: vec![0; bounds.len()],
            count: 0,
            sum: 0.0,
        }
    }

    pub fn observe(&mut self, value: f64) {
        let v = value.max(0.0);
        for (idx, &bound) in self.bounds.iter().enumerate() {
            if v <= bound {
                self.bucket_counts[idx] += 1;
            }
        }
        self.count += 1;
        self.sum += v;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadTelemetryRecord {
    pub nodename: String,
    pub host_os: String,
    pub worker_group: String,
    pub worker_thread: String,
    /// `0 = Idle/Waiting`, `1 = Active/Working`, `2 = BlockedOnQuotaOrLock`, `3 = Finished`
    pub state: u8,
    pub busy_in_secs_total: f64,
    pub wait_in_secs_by_reason: BTreeMap<String, f64>,
    pub current_wait_in_secs_by_reason: BTreeMap<String, f64>,
    pub wait_cycles_by_reason: BTreeMap<String, u64>,
    pub task_wait_hist_by_reason: BTreeMap<String, HistogramSnapshot>,
    pub cpu_user_in_secs_total: f64,
    pub cpu_system_in_secs_total: f64,
    pub tasks_started_by_stage: BTreeMap<String, u64>,
    pub tasks_completed_by_stage_outcome: BTreeMap<String, BTreeMap<String, u64>>,
    pub errors_by_type: BTreeMap<String, u64>,
    pub stage_duration_hist_by_stage: BTreeMap<String, HistogramSnapshot>,
}

impl ThreadTelemetryRecord {
    pub fn new(nodename: &str, host_os: &str, worker_group: &str, worker_thread: &str) -> Self {
        let mut wait_by_reason = BTreeMap::new();
        let mut cur_wait = BTreeMap::new();
        let mut cycles = BTreeMap::new();
        let mut hists = BTreeMap::new();
        for r in WaitReason::ALL {
            let key = r.as_str().to_string();
            wait_by_reason.insert(key.clone(), 0.0);
            cur_wait.insert(key.clone(), 0.0);
            cycles.insert(key.clone(), 0);
            hists.insert(key, HistogramSnapshot::new(WAIT_DURATION_BUCKETS));
        }
        Self {
            nodename: nodename.to_string(),
            host_os: host_os.to_string(),
            worker_group: worker_group.to_string(),
            worker_thread: worker_thread.to_string(),
            state: 0,
            busy_in_secs_total: 0.0,
            wait_in_secs_by_reason: wait_by_reason,
            current_wait_in_secs_by_reason: cur_wait,
            wait_cycles_by_reason: cycles,
            task_wait_hist_by_reason: hists,
            cpu_user_in_secs_total: 0.0,
            cpu_system_in_secs_total: 0.0,
            tasks_started_by_stage: BTreeMap::new(),
            tasks_completed_by_stage_outcome: BTreeMap::new(),
            errors_by_type: BTreeMap::new(),
            stage_duration_hist_by_stage: BTreeMap::new(),
        }
    }

    pub fn utilization_ratio(&self) -> f64 {
        let total_wait: f64 = self.wait_in_secs_by_reason.values().sum();
        let denom = self.busy_in_secs_total + total_wait;
        if denom > 0.0001 {
            (self.busy_in_secs_total / denom).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountTelemetryRecord {
    pub nodename: String,
    pub host_os: String,
    pub mount_point: String,
    pub drive_type: String,
    pub read_chunk_hist: HistogramSnapshot,
    pub write_chunk_hist: HistogramSnapshot,
    pub io_lock_wait_hist_by_thread: BTreeMap<String, HistogramSnapshot>,
    pub io_lock_hold_in_secs_by_thread: BTreeMap<String, f64>,
    pub io_lock_waiting_threads: u64,
    pub read_size_in_mb_total: f64,
    pub write_size_in_mb_total: f64,
    pub read_throughput_mbps: f64,
    pub write_throughput_mbps: f64,
    pub free_gb: f64,
    pub total_gb: f64,
    pub ramdisk_staged_input_used_gb: f64,
    pub ramdisk_staged_output_used_gb: f64,
    pub ramdisk_partition_max_gb: f64,
    pub ramdisk_future_partition_max_gb: f64,
    pub disk_errors_by_op: BTreeMap<String, u64>,
    pub queue_files_by_state: BTreeMap<String, u64>,
    pub queue_size_in_mb_by_state: BTreeMap<String, f64>,
    pub stager_decisions_by_type: BTreeMap<String, u64>,
    pub resume_checks_by_outcome: BTreeMap<String, u64>,
    pub subdirs_reconciled_by_outcome: BTreeMap<String, u64>,
}

impl MountTelemetryRecord {
    pub fn new(nodename: &str, host_os: &str, mount_point: &str, drive_type: &str) -> Self {
        Self {
            nodename: nodename.to_string(),
            host_os: host_os.to_string(),
            mount_point: mount_point.to_string(),
            drive_type: drive_type.to_string(),
            read_chunk_hist: HistogramSnapshot::new(DISK_CHUNK_LATENCY_BUCKETS),
            write_chunk_hist: HistogramSnapshot::new(DISK_CHUNK_LATENCY_BUCKETS),
            io_lock_wait_hist_by_thread: BTreeMap::new(),
            io_lock_hold_in_secs_by_thread: BTreeMap::new(),
            io_lock_waiting_threads: 0,
            read_size_in_mb_total: 0.0,
            write_size_in_mb_total: 0.0,
            read_throughput_mbps: 0.0,
            write_throughput_mbps: 0.0,
            free_gb: 0.0,
            total_gb: 0.0,
            ramdisk_staged_input_used_gb: 0.0,
            ramdisk_staged_output_used_gb: 0.0,
            ramdisk_partition_max_gb: 0.0,
            ramdisk_future_partition_max_gb: 0.0,
            disk_errors_by_op: BTreeMap::new(),
            queue_files_by_state: BTreeMap::new(),
            queue_size_in_mb_by_state: BTreeMap::new(),
            stager_decisions_by_type: BTreeMap::new(),
            resume_checks_by_outcome: BTreeMap::new(),
            subdirs_reconciled_by_outcome: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteNodeTelemetryRecord {
    pub nodename: String,
    pub host_os: String,
    pub ssh_rtt_hist_by_cmd: BTreeMap<String, HistogramSnapshot>,
    pub scp_upload_duration_hist: HistogramSnapshot,
    pub scp_download_duration_hist: HistogramSnapshot,
    pub scp_upload_size_in_mb_total: f64,
    pub scp_download_size_in_mb_total: f64,
    pub scp_upload_throughput_mbps: f64,
    pub scp_download_throughput_mbps: f64,
    pub quota_used_gb: f64,
    pub quota_remaining_gb: f64,
    pub quota_max_gb: f64,
    pub quota_single_file_cap_gb: f64,
    pub queue_depth_by_stage: BTreeMap<String, u64>,
    pub network_errors_by_type: BTreeMap<String, u64>,
}

impl RemoteNodeTelemetryRecord {
    pub fn new(nodename: &str, host_os: &str) -> Self {
        Self {
            nodename: nodename.to_string(),
            host_os: host_os.to_string(),
            ssh_rtt_hist_by_cmd: BTreeMap::new(),
            scp_upload_duration_hist: HistogramSnapshot::new(STAGE_DURATION_BUCKETS),
            scp_download_duration_hist: HistogramSnapshot::new(STAGE_DURATION_BUCKETS),
            scp_upload_size_in_mb_total: 0.0,
            scp_download_size_in_mb_total: 0.0,
            scp_upload_throughput_mbps: 0.0,
            scp_download_throughput_mbps: 0.0,
            quota_used_gb: 0.0,
            quota_remaining_gb: 0.0,
            quota_max_gb: 0.0,
            quota_single_file_cap_gb: 0.0,
            queue_depth_by_stage: BTreeMap::new(),
            network_errors_by_type: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerGroupMediaTelemetry {
    pub nodename: String,
    pub host_os: String,
    pub worker_group: String,
    pub encoder: String,
    pub source_size_in_mb_by_codec: BTreeMap<String, f64>,
    pub target_size_in_mb_by_codec: BTreeMap<String, f64>,
    pub storage_saved_in_mb_total: f64,
    pub compression_ratio_hist: HistogramSnapshot,
    pub video_duration_encoded_in_secs_total: f64,
    pub realtime_speed_x: f64,
    pub hwaccel_fallback_by_codec: BTreeMap<String, u64>,
    pub kept_smaller_original_by_codec: BTreeMap<String, u64>,
}

impl WorkerGroupMediaTelemetry {
    pub fn new(nodename: &str, host_os: &str, worker_group: &str, encoder: &str) -> Self {
        Self {
            nodename: nodename.to_string(),
            host_os: host_os.to_string(),
            worker_group: worker_group.to_string(),
            encoder: encoder.to_string(),
            source_size_in_mb_by_codec: BTreeMap::new(),
            target_size_in_mb_by_codec: BTreeMap::new(),
            storage_saved_in_mb_total: 0.0,
            compression_ratio_hist: HistogramSnapshot::new(COMPRESSION_RATIO_BUCKETS),
            video_duration_encoded_in_secs_total: 0.0,
            realtime_speed_x: 0.0,
            hwaccel_fallback_by_codec: BTreeMap::new(),
            kept_smaller_original_by_codec: BTreeMap::new(),
        }
    }
}

/// Hardware sample streamed over SSH from `rusty_h265_metrics_agent.py --role remote`
/// (or collected locally by `--role master`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostHardwareSample {
    pub nodename: String,
    pub host_os: String,
    pub updated_at_epoch_secs: u64,
    pub cpu_user_ratio: f64,
    pub cpu_system_ratio: f64,
    pub cpu_idle_ratio: f64,
    pub cpu_active_ratio: f64,
    pub load_avg_1m: f64,
    pub load_avg_5m: f64,
    pub load_avg_15m: f64,
    pub memory_used_gb: f64,
    pub memory_free_gb: f64,
    pub memory_total_gb: f64,
    pub memory_wired_gb: f64,
    pub memory_compressed_gb: f64,
    pub watch_dir_free_gb: f64,
    pub watch_dir_total_gb: f64,
    pub gpu_name: String,
    pub gpu_core_utilization_ratio: f64,
    pub gpu_encoder_utilization_ratio: f64,
    pub gpu_decoder_utilization_ratio: f64,
    pub gpu_renderer_utilization_ratio: f64,
    pub gpu_tiler_utilization_ratio: f64,
    pub gpu_memory_used_in_mb: f64,
    pub gpu_memory_total_in_mb: f64,
    pub gpu_temperature_celsius: f64,
    pub gpu_power_watts: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetrySnapshotPayload {
    pub updated_at_epoch_ms: u64,
    pub converter_pid: u32,
    pub converter_active: bool,
    pub local_nodename: String,
    pub local_host_os: String,
    pub uptime_in_secs: f64,
    pub ramdisk_partition_resizes_total: u64,
    pub logs_emitted_by_level: BTreeMap<String, u64>,
    pub threads: BTreeMap<String, ThreadTelemetryRecord>,
    pub mounts: BTreeMap<String, MountTelemetryRecord>,
    pub remote_nodes: BTreeMap<String, RemoteNodeTelemetryRecord>,
    pub media_groups: BTreeMap<String, WorkerGroupMediaTelemetry>,
    pub remote_hardware: BTreeMap<String, HostHardwareSample>,
}

impl Default for TelemetrySnapshotPayload {
    fn default() -> Self {
        Self {
            updated_at_epoch_ms: 0,
            converter_pid: std::process::id(),
            converter_active: true,
            local_nodename: gethostname_syscall(),
            local_host_os: local_host_os_label().to_string(),
            uptime_in_secs: 0.0,
            ramdisk_partition_resizes_total: 0,
            logs_emitted_by_level: BTreeMap::new(),
            threads: BTreeMap::new(),
            mounts: BTreeMap::new(),
            remote_nodes: BTreeMap::new(),
            media_groups: BTreeMap::new(),
            remote_hardware: BTreeMap::new(),
        }
    }
}

// ============================================================================
// 4. Global MetricsRegistry & Prometheus Exposition Renderer
// ============================================================================

pub struct MetricsRegistry {
    start_instant: Instant,
    inner: Mutex<TelemetrySnapshotPayload>,
}

static GLOBAL_METRICS: OnceLock<Arc<MetricsRegistry>> = OnceLock::new();

pub fn global_metrics() -> &'static Arc<MetricsRegistry> {
    GLOBAL_METRICS.get_or_init(|| {
        Arc::new(MetricsRegistry {
            start_instant: Instant::now(),
            inner: Mutex::new(TelemetrySnapshotPayload::default()),
        })
    })
}

impl MetricsRegistry {
    pub fn set_local_identity(&self, nodename: &str, host_os: &str) {
        if let Ok(mut g) = self.inner.lock() {
            if !nodename.trim().is_empty() {
                g.local_nodename = nodename.trim().to_string();
            }
            if !host_os.trim().is_empty() {
                g.local_host_os = host_os.trim().to_string();
            }
        }
    }

    pub fn set_converter_active(&self, active: bool) {
        if let Ok(mut g) = self.inner.lock() {
            g.converter_active = active;
        }
    }

    fn thread_key(worker_group: &str, worker_thread: &str) -> String {
        format!("{}::{}", worker_group, worker_thread)
    }

    pub fn register_thread(
        &self,
        nodename: &str,
        host_os: &str,
        worker_group: &str,
        worker_thread: &str,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let key = Self::thread_key(worker_group, worker_thread);
            g.threads.entry(key).or_insert_with(|| {
                ThreadTelemetryRecord::new(nodename, host_os, worker_group, worker_thread)
            });
        }
    }

    pub fn set_thread_state(
        &self,
        nodename: &str,
        host_os: &str,
        worker_group: &str,
        worker_thread: &str,
        state: u8,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let key = Self::thread_key(worker_group, worker_thread);
            let rec = g.threads.entry(key).or_insert_with(|| {
                ThreadTelemetryRecord::new(nodename, host_os, worker_group, worker_thread)
            });
            rec.state = state;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_thread_wait_tick(
        &self,
        nodename: &str,
        host_os: &str,
        worker_group: &str,
        worker_thread: &str,
        reason: WaitReason,
        delta_secs: f64,
        current_wait_for_reason_secs: f64,
        count_cycle: bool,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let key = Self::thread_key(worker_group, worker_thread);
            let rec = g.threads.entry(key).or_insert_with(|| {
                ThreadTelemetryRecord::new(nodename, host_os, worker_group, worker_thread)
            });
            let r_str = reason.as_str().to_string();
            *rec.wait_in_secs_by_reason.entry(r_str.clone()).or_insert(0.0) += delta_secs;
            rec.current_wait_in_secs_by_reason
                .insert(r_str.clone(), current_wait_for_reason_secs);
            if count_cycle {
                *rec.wait_cycles_by_reason.entry(r_str).or_insert(0) += 1;
            }
        }
    }

    pub fn finalize_thread_wait_for_task(
        &self,
        nodename: &str,
        host_os: &str,
        worker_group: &str,
        worker_thread: &str,
        stage: &str,
        waits: &BTreeMap<WaitReason, f64>,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let key = Self::thread_key(worker_group, worker_thread);
            let rec = g.threads.entry(key).or_insert_with(|| {
                ThreadTelemetryRecord::new(nodename, host_os, worker_group, worker_thread)
            });
            rec.state = 1; // Active
            *rec.tasks_started_by_stage
                .entry(stage.to_string())
                .or_insert(0) += 1;

            for r in WaitReason::ALL {
                let r_str = r.as_str().to_string();
                rec.current_wait_in_secs_by_reason.insert(r_str.clone(), 0.0);
                let waited = waits.get(&r).copied().unwrap_or(0.0);
                if waited > 0.0 || matches!(r, WaitReason::UpstreamQueueEmpty) {
                    rec.task_wait_hist_by_reason
                        .entry(r_str)
                        .or_insert_with(|| HistogramSnapshot::new(WAIT_DURATION_BUCKETS))
                        .observe(waited);
                }
            }
        }
    }

    pub fn record_stage_completion(
        &self,
        nodename: &str,
        host_os: &str,
        worker_group: &str,
        worker_thread: &str,
        stage: &str,
        outcome: &str,
        duration_secs: f64,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let key = Self::thread_key(worker_group, worker_thread);
            let rec = g.threads.entry(key).or_insert_with(|| {
                ThreadTelemetryRecord::new(nodename, host_os, worker_group, worker_thread)
            });
            rec.busy_in_secs_total += duration_secs.max(0.0);
            rec.stage_duration_hist_by_stage
                .entry(stage.to_string())
                .or_insert_with(|| HistogramSnapshot::new(STAGE_DURATION_BUCKETS))
                .observe(duration_secs.max(0.0));
            let stage_map = rec
                .tasks_completed_by_stage_outcome
                .entry(stage.to_string())
                .or_default();
            *stage_map.entry(outcome.to_string()).or_insert(0) += 1;
            rec.state = 0;
        }
    }

    pub fn record_thread_error(
        &self,
        nodename: &str,
        host_os: &str,
        worker_group: &str,
        worker_thread: &str,
        error_type: &str,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let key = Self::thread_key(worker_group, worker_thread);
            let rec = g.threads.entry(key).or_insert_with(|| {
                ThreadTelemetryRecord::new(nodename, host_os, worker_group, worker_thread)
            });
            *rec.errors_by_type
                .entry(error_type.to_string())
                .or_insert(0) += 1;
        }
    }

    pub fn add_thread_cpu_time(
        &self,
        nodename: &str,
        host_os: &str,
        worker_group: &str,
        worker_thread: &str,
        user_delta_secs: f64,
        sys_delta_secs: f64,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let key = Self::thread_key(worker_group, worker_thread);
            let rec = g.threads.entry(key).or_insert_with(|| {
                ThreadTelemetryRecord::new(nodename, host_os, worker_group, worker_thread)
            });
            rec.cpu_user_in_secs_total += user_delta_secs.max(0.0);
            rec.cpu_system_in_secs_total += sys_delta_secs.max(0.0);
        }
    }

    // --- Disk & Mount Telemetry ---

    pub fn register_mount(&self, mount_point: &str, drive_type: &str) {
        if let Ok(mut g) = self.inner.lock() {
            let nodename = g.local_nodename.clone();
            let host_os = g.local_host_os.clone();
            g.mounts
                .entry(mount_point.to_string())
                .or_insert_with(|| {
                    MountTelemetryRecord::new(&nodename, &host_os, mount_point, drive_type)
                });
        }
    }

    pub fn record_disk_chunk_io(
        &self,
        mount_point: &str,
        drive_type: &str,
        is_write: bool,
        bytes: u64,
        chunk_duration_secs: f64,
        live_mbps: f64,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let nodename = g.local_nodename.clone();
            let host_os = g.local_host_os.clone();
            let rec = g
                .mounts
                .entry(mount_point.to_string())
                .or_insert_with(|| {
                    MountTelemetryRecord::new(&nodename, &host_os, mount_point, drive_type)
                });
            let mb = (bytes as f64) / (1024.0 * 1024.0);
            if is_write {
                rec.write_chunk_hist.observe(chunk_duration_secs);
                rec.write_size_in_mb_total += mb;
                rec.write_throughput_mbps = live_mbps;
            } else {
                rec.read_chunk_hist.observe(chunk_duration_secs);
                rec.read_size_in_mb_total += mb;
                rec.read_throughput_mbps = live_mbps;
            }
        }
    }

    pub fn clear_disk_live_throughput(&self, mount_point: &str, is_write: bool) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(rec) = g.mounts.get_mut(mount_point) {
                if is_write {
                    rec.write_throughput_mbps = 0.0;
                } else {
                    rec.read_throughput_mbps = 0.0;
                }
            }
        }
    }

    pub fn inc_io_lock_waiting(&self, mount_point: &str, drive_type: &str) {
        if let Ok(mut g) = self.inner.lock() {
            let nodename = g.local_nodename.clone();
            let host_os = g.local_host_os.clone();
            let rec = g
                .mounts
                .entry(mount_point.to_string())
                .or_insert_with(|| {
                    MountTelemetryRecord::new(&nodename, &host_os, mount_point, drive_type)
                });
            rec.io_lock_waiting_threads = rec.io_lock_waiting_threads.saturating_add(1);
        }
    }

    pub fn record_io_lock_acquired(
        &self,
        mount_point: &str,
        drive_type: &str,
        worker_thread: &str,
        wait_secs: f64,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let nodename = g.local_nodename.clone();
            let host_os = g.local_host_os.clone();
            let rec = g
                .mounts
                .entry(mount_point.to_string())
                .or_insert_with(|| {
                    MountTelemetryRecord::new(&nodename, &host_os, mount_point, drive_type)
                });
            rec.io_lock_waiting_threads = rec.io_lock_waiting_threads.saturating_sub(1);
            rec.io_lock_wait_hist_by_thread
                .entry(worker_thread.to_string())
                .or_insert_with(|| HistogramSnapshot::new(WAIT_DURATION_BUCKETS))
                .observe(wait_secs.max(0.0));
        }
    }

    pub fn record_io_lock_released(
        &self,
        mount_point: &str,
        worker_thread: &str,
        hold_secs: f64,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(rec) = g.mounts.get_mut(mount_point) {
                *rec.io_lock_hold_in_secs_by_thread
                    .entry(worker_thread.to_string())
                    .or_insert(0.0) += hold_secs.max(0.0);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_mount_space_and_queues(
        &self,
        mount_point: &str,
        drive_type: &str,
        free_bytes: u64,
        total_bytes: u64,
        staged_in_bytes: u64,
        staged_out_bytes: u64,
        partition_max_bytes: u64,
        future_partition_max_bytes: u64,
        raw_count: usize,
        raw_bytes: u64,
        ready_count: usize,
        ready_bytes: u64,
        in_flight_local: usize,
        in_flight_remote: usize,
        in_flight_bytes: u64,
        to_convert_count: usize,
        to_flush_count: usize,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let nodename = g.local_nodename.clone();
            let host_os = g.local_host_os.clone();
            let rec = g
                .mounts
                .entry(mount_point.to_string())
                .or_insert_with(|| {
                    MountTelemetryRecord::new(&nodename, &host_os, mount_point, drive_type)
                });
            let gb = 1024.0 * 1024.0 * 1024.0;
            let mb = 1024.0 * 1024.0;
            rec.free_gb = (free_bytes as f64) / gb;
            rec.total_gb = (total_bytes as f64) / gb;
            rec.ramdisk_staged_input_used_gb = (staged_in_bytes as f64) / gb;
            rec.ramdisk_staged_output_used_gb = (staged_out_bytes as f64) / gb;
            rec.ramdisk_partition_max_gb = (partition_max_bytes as f64) / gb;
            rec.ramdisk_future_partition_max_gb = (future_partition_max_bytes as f64) / gb;

            rec.queue_files_by_state
                .insert("raw".to_string(), raw_count as u64);
            rec.queue_files_by_state
                .insert("ready".to_string(), ready_count as u64);
            rec.queue_files_by_state
                .insert("in_flight_local".to_string(), in_flight_local as u64);
            rec.queue_files_by_state
                .insert("in_flight_remote".to_string(), in_flight_remote as u64);
            rec.queue_files_by_state
                .insert("to_convert".to_string(), to_convert_count as u64);
            rec.queue_files_by_state
                .insert("to_flush".to_string(), to_flush_count as u64);

            rec.queue_size_in_mb_by_state
                .insert("raw".to_string(), (raw_bytes as f64) / mb);
            rec.queue_size_in_mb_by_state
                .insert("ready".to_string(), (ready_bytes as f64) / mb);
            rec.queue_size_in_mb_by_state
                .insert("in_flight".to_string(), (in_flight_bytes as f64) / mb);
        }
    }

    pub fn record_disk_error(&self, mount_point: &str, op: &str) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(rec) = g.mounts.get_mut(mount_point) {
                *rec.disk_errors_by_op.entry(op.to_string()).or_insert(0) += 1;
            }
        }
    }

    pub fn record_stager_decision(&self, mount_point: &str, decision: &str) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(rec) = g.mounts.get_mut(mount_point) {
                *rec.stager_decisions_by_type
                    .entry(decision.to_string())
                    .or_insert(0) += 1;
            }
        }
    }

    pub fn record_resume_check(&self, mount_point: &str, outcome: &str) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(rec) = g.mounts.get_mut(mount_point) {
                *rec.resume_checks_by_outcome
                    .entry(outcome.to_string())
                    .or_insert(0) += 1;
            }
        }
    }

    pub fn record_subdir_reconciled(&self, mount_point: &str, outcome: &str) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(rec) = g.mounts.get_mut(mount_point) {
                *rec.subdirs_reconciled_by_outcome
                    .entry(outcome.to_string())
                    .or_insert(0) += 1;
            }
        }
    }

    pub fn inc_ramdisk_partition_resizes(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.ramdisk_partition_resizes_total += 1;
        }
    }

    // --- Remote Network / SSH / SCP Telemetry ---

    pub fn record_ssh_rtt(&self, nodename: &str, host_os: &str, cmd_type: &str, rtt_secs: f64) {
        if let Ok(mut g) = self.inner.lock() {
            let rec = g
                .remote_nodes
                .entry(nodename.to_string())
                .or_insert_with(|| RemoteNodeTelemetryRecord::new(nodename, host_os));
            rec.ssh_rtt_hist_by_cmd
                .entry(cmd_type.to_string())
                .or_insert_with(|| HistogramSnapshot::new(WAIT_DURATION_BUCKETS))
                .observe(rtt_secs.max(0.0));
        }
    }

    pub fn update_scp_live_throughput(
        &self,
        nodename: &str,
        host_os: &str,
        is_upload: bool,
        mbps: f64,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let rec = g
                .remote_nodes
                .entry(nodename.to_string())
                .or_insert_with(|| RemoteNodeTelemetryRecord::new(nodename, host_os));
            if is_upload {
                rec.scp_upload_throughput_mbps = mbps.max(0.0);
            } else {
                rec.scp_download_throughput_mbps = mbps.max(0.0);
            }
        }
    }

    pub fn record_scp_completed(
        &self,
        nodename: &str,
        host_os: &str,
        is_upload: bool,
        bytes: u64,
        duration_secs: f64,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let rec = g
                .remote_nodes
                .entry(nodename.to_string())
                .or_insert_with(|| RemoteNodeTelemetryRecord::new(nodename, host_os));
            let mb = (bytes as f64) / (1024.0 * 1024.0);
            if is_upload {
                rec.scp_upload_duration_hist.observe(duration_secs.max(0.0));
                rec.scp_upload_size_in_mb_total += mb;
                rec.scp_upload_throughput_mbps = 0.0;
            } else {
                rec.scp_download_duration_hist
                    .observe(duration_secs.max(0.0));
                rec.scp_download_size_in_mb_total += mb;
                rec.scp_download_throughput_mbps = 0.0;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_remote_quota_and_queues(
        &self,
        nodename: &str,
        host_os: &str,
        used_bytes: u64,
        max_bytes: u64,
        single_file_cap_bytes: u64,
        upload_q_len: usize,
        encode_q_len: usize,
        copyback_q_len: usize,
        inflight_len: usize,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let rec = g
                .remote_nodes
                .entry(nodename.to_string())
                .or_insert_with(|| RemoteNodeTelemetryRecord::new(nodename, host_os));
            let gb = 1024.0 * 1024.0 * 1024.0;
            rec.quota_used_gb = (used_bytes as f64) / gb;
            rec.quota_max_gb = (max_bytes as f64) / gb;
            rec.quota_remaining_gb = (max_bytes.saturating_sub(used_bytes) as f64) / gb;
            rec.quota_single_file_cap_gb = (single_file_cap_bytes as f64) / gb;
            rec.queue_depth_by_stage
                .insert("upload".to_string(), upload_q_len as u64);
            rec.queue_depth_by_stage
                .insert("encode".to_string(), encode_q_len as u64);
            rec.queue_depth_by_stage
                .insert("copyback".to_string(), copyback_q_len as u64);
            rec.queue_depth_by_stage
                .insert("inflight".to_string(), inflight_len as u64);
        }
    }

    pub fn record_network_error(&self, nodename: &str, host_os: &str, error_type: &str) {
        if let Ok(mut g) = self.inner.lock() {
            let rec = g
                .remote_nodes
                .entry(nodename.to_string())
                .or_insert_with(|| RemoteNodeTelemetryRecord::new(nodename, host_os));
            *rec.network_errors_by_type
                .entry(error_type.to_string())
                .or_insert(0) += 1;
        }
    }

    // --- Media Transcoding Domain Telemetry ---

    pub fn update_encoder_realtime_speed(
        &self,
        nodename: &str,
        host_os: &str,
        worker_group: &str,
        encoder: &str,
        speed_x: f64,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let rec = g
                .media_groups
                .entry(worker_group.to_string())
                .or_insert_with(|| {
                    WorkerGroupMediaTelemetry::new(nodename, host_os, worker_group, encoder)
                });
            if !encoder.is_empty() {
                rec.encoder = encoder.to_string();
            }
            rec.realtime_speed_x = speed_x.max(0.0);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_media_conversion_result(
        &self,
        nodename: &str,
        host_os: &str,
        worker_group: &str,
        encoder: &str,
        source_codec: &str,
        source_bytes: u64,
        target_bytes: u64,
        saved_bytes: u64,
        video_duration_secs: f64,
        hwaccel_fallback_used: bool,
        kept_original: bool,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let rec = g
                .media_groups
                .entry(worker_group.to_string())
                .or_insert_with(|| {
                    WorkerGroupMediaTelemetry::new(nodename, host_os, worker_group, encoder)
                });
            if !encoder.is_empty() {
                rec.encoder = encoder.to_string();
            }
            let mb = 1024.0 * 1024.0;
            let src_mb = (source_bytes as f64) / mb;
            let tgt_mb = (target_bytes as f64) / mb;
            let sav_mb = (saved_bytes as f64) / mb;
            let codec_key = if source_codec.is_empty() {
                "unknown"
            } else {
                source_codec
            };

            *rec.source_size_in_mb_by_codec
                .entry(codec_key.to_string())
                .or_insert(0.0) += src_mb;
            *rec.target_size_in_mb_by_codec
                .entry(codec_key.to_string())
                .or_insert(0.0) += tgt_mb;
            rec.storage_saved_in_mb_total += sav_mb;
            if source_bytes > 0 && target_bytes > 0 {
                let ratio = (target_bytes as f64) / (source_bytes as f64);
                rec.compression_ratio_hist.observe(ratio);
            }
            rec.video_duration_encoded_in_secs_total += video_duration_secs.max(0.0);
            rec.realtime_speed_x = 0.0;

            if hwaccel_fallback_used {
                *rec.hwaccel_fallback_by_codec
                    .entry(codec_key.to_string())
                    .or_insert(0) += 1;
            }
            if kept_original {
                *rec.kept_smaller_original_by_codec
                    .entry(codec_key.to_string())
                    .or_insert(0) += 1;
            }
        }
    }

    pub fn update_remote_hardware_sample(&self, sample: HostHardwareSample) {
        if let Ok(mut g) = self.inner.lock() {
            g.remote_hardware.insert(sample.nodename.clone(), sample);
        }
    }

    pub fn update_mount_space(
        &self,
        mount_point: &str,
        drive_type: &str,
        free_bytes: u64,
        total_bytes: u64,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let nodename = g.local_nodename.clone();
            let host_os = g.local_host_os.clone();
            let rec = g
                .mounts
                .entry(mount_point.to_string())
                .or_insert_with(|| {
                    MountTelemetryRecord::new(&nodename, &host_os, mount_point, drive_type)
                });
            let gb = 1024.0 * 1024.0 * 1024.0;
            rec.free_gb = (free_bytes as f64) / gb;
            rec.total_gb = (total_bytes as f64) / gb;
        }
    }

    pub fn update_mount_queues(
        &self,
        mount_point: &str,
        drive_type: &str,
        raw_count: usize,
        ready_count: usize,
        in_flight_local: usize,
        in_flight_remote: usize,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let nodename = g.local_nodename.clone();
            let host_os = g.local_host_os.clone();
            let rec = g
                .mounts
                .entry(mount_point.to_string())
                .or_insert_with(|| {
                    MountTelemetryRecord::new(&nodename, &host_os, mount_point, drive_type)
                });
            rec.queue_files_by_state
                .insert("raw".to_string(), raw_count as u64);
            rec.queue_files_by_state
                .insert("ready".to_string(), ready_count as u64);
            rec.queue_files_by_state
                .insert("in_flight_local".to_string(), in_flight_local as u64);
            rec.queue_files_by_state
                .insert("in_flight_remote".to_string(), in_flight_remote as u64);
        }
    }

    pub fn record_reconciliation(
        &self,
        mount_point: &str,
        outcome: &str,
        bad_targets_deleted: u64,
    ) {
        self.record_subdir_reconciled(mount_point, outcome);
        if bad_targets_deleted > 0 {
            if let Ok(mut g) = self.inner.lock() {
                if let Some(rec) = g.mounts.get_mut(mount_point) {
                    *rec.subdirs_reconciled_by_outcome
                        .entry("bad_target_deleted".to_string())
                        .or_insert(0) += bad_targets_deleted;
                }
            }
        }
    }

    pub fn record_ssh_retry(&self, nodename: &str, host_os: &str, _cmd_type: &str) {
        self.record_network_error(nodename, host_os, "ssh_255_retry");
    }

    pub fn record_enospc_reset(&self, nodename: &str, host_os: &str) {
        self.record_network_error(nodename, host_os, "enospc_reset");
    }

    pub fn update_remote_quota(
        &self,
        nodename: &str,
        host_os: &str,
        used_bytes: u64,
        max_bytes: u64,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let rec = g
                .remote_nodes
                .entry(nodename.to_string())
                .or_insert_with(|| RemoteNodeTelemetryRecord::new(nodename, host_os));
            let gb = 1024.0 * 1024.0 * 1024.0;
            rec.quota_used_gb = (used_bytes as f64) / gb;
            rec.quota_max_gb = (max_bytes as f64) / gb;
            rec.quota_remaining_gb = (max_bytes.saturating_sub(used_bytes) as f64) / gb;
            rec.quota_single_file_cap_gb = ((max_bytes / 2) as f64) / gb;
        }
    }

    pub fn update_remote_queues(
        &self,
        nodename: &str,
        host_os: &str,
        upload_q_len: usize,
        encode_q_len: usize,
        copyback_q_len: usize,
        inflight_len: usize,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            let rec = g
                .remote_nodes
                .entry(nodename.to_string())
                .or_insert_with(|| RemoteNodeTelemetryRecord::new(nodename, host_os));
            rec.queue_depth_by_stage
                .insert("upload".to_string(), upload_q_len as u64);
            rec.queue_depth_by_stage
                .insert("encode".to_string(), encode_q_len as u64);
            rec.queue_depth_by_stage
                .insert("copyback".to_string(), copyback_q_len as u64);
            rec.queue_depth_by_stage
                .insert("inflight".to_string(), inflight_len as u64);
        }
    }

    pub fn update_media_active_speed(
        &self,
        nodename: &str,
        host_os: &str,
        worker_group: &str,
        encoder: &str,
        speed_x: f64,
    ) {
        self.update_encoder_realtime_speed(nodename, host_os, worker_group, encoder, speed_x);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_media_completed(
        &self,
        nodename: &str,
        host_os: &str,
        worker_group: &str,
        encoder: &str,
        source_bytes: u64,
        target_bytes: u64,
        saved_bytes: u64,
        video_duration_secs: f64,
    ) {
        self.record_media_conversion_result(
            nodename,
            host_os,
            worker_group,
            encoder,
            "video",
            source_bytes,
            target_bytes,
            saved_bytes,
            video_duration_secs,
            false,
            false,
        );
    }

    pub fn inc_log_emitted(&self, level: &str) {
        if let Ok(mut g) = self.inner.lock() {
            *g.logs_emitted_by_level
                .entry(level.to_string())
                .or_insert(0) += 1;
        }
    }

    pub fn snapshot(&self) -> TelemetrySnapshotPayload {
        let mut snap = self.inner.lock().unwrap().clone();
        snap.updated_at_epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        snap.uptime_in_secs = self.start_instant.elapsed().as_secs_f64();
        snap
    }

    /// Atomically writes the current `TelemetrySnapshotPayload` to `target_path` via `<path>.tmp` + `rename`.
    pub fn write_snapshot_atomic(&self, target_path: &Path) -> std::io::Result<()> {
        let snap = self.snapshot();
        if let Some(parent) = target_path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }
        let tmp_path = target_path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "{}".to_string());
        fs::write(&tmp_path, json)?;
        fs::rename(&tmp_path, target_path)?;
        Ok(())
    }
}

// ============================================================================
// 5. Prometheus v0.0.4 Text Exposition Renderer
// ============================================================================

fn escape_prom_label(val: &str) -> String {
    val.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn write_histogram_prom(
    out: &mut String,
    metric_name: &str,
    base_labels: &str,
    hist: &HistogramSnapshot,
) {
    for (idx, &bound) in hist.bounds.iter().enumerate() {
        let cnt = hist.bucket_counts.get(idx).copied().unwrap_or(0);
        let _ = writeln!(
            out,
            "{}_bucket{{{},le=\"{}\"}} {}",
            metric_name, base_labels, bound, cnt
        );
    }
    let _ = writeln!(
        out,
        "{}_bucket{{{},le=\"+Inf\"}} {}",
        metric_name, base_labels, hist.count
    );
    let _ = writeln!(
        out,
        "{}_sum{{{}}} {:.6}",
        metric_name, base_labels, hist.sum
    );
    let _ = writeln!(
        out,
        "{}_count{{{}}} {}",
        metric_name, base_labels, hist.count
    );
}

/// Renders a `TelemetrySnapshotPayload` (and optional hardware samples) into standard
/// Prometheus v0.0.4 text format.
pub fn render_prometheus_from_snapshot(
    snap: &TelemetrySnapshotPayload,
    extra_hw_samples: &[HostHardwareSample],
) -> String {
    let mut out = String::with_capacity(32 * 1024);

    let _ = writeln!(
        out,
        "# HELP h265_converter_up Whether rusty_h265_converter is actively running (1=active, 0=finished/idle)."
    );
    let _ = writeln!(out, "# TYPE h265_converter_up gauge");
    let _ = writeln!(
        out,
        "h265_converter_up{{nodename=\"{}\",host_os=\"{}\"}} {}",
        escape_prom_label(&snap.local_nodename),
        escape_prom_label(&snap.local_host_os),
        if snap.converter_active { 1 } else { 0 }
    );

    let _ = writeln!(
        out,
        "# HELP h265_converter_uptime_in_secs Wall-clock uptime of rusty_h265_converter in seconds."
    );
    let _ = writeln!(out, "# TYPE h265_converter_uptime_in_secs gauge");
    let _ = writeln!(
        out,
        "h265_converter_uptime_in_secs{{nodename=\"{}\",host_os=\"{}\"}} {:.3}",
        escape_prom_label(&snap.local_nodename),
        escape_prom_label(&snap.local_host_os),
        snap.uptime_in_secs
    );

    // --- Section A: Thread-Level USE, RED & Wait-Time Metrics ---
    let _ = writeln!(
        out,
        "# HELP h265_thread_state Worker thread state (0=Idle/Waiting, 1=Active/Working, 2=BlockedOnQuotaOrLock, 3=Finished)."
    );
    let _ = writeln!(out, "# TYPE h265_thread_state gauge");
    let _ = writeln!(
        out,
        "# HELP h265_thread_busy_in_secs_total Cumulative seconds spent actively executing tasks."
    );
    let _ = writeln!(out, "# TYPE h265_thread_busy_in_secs_total counter");
    let _ = writeln!(
        out,
        "# HELP h265_thread_utilization_ratio Thread active utilization ratio (busy / (busy + wait))."
    );
    let _ = writeln!(out, "# TYPE h265_thread_utilization_ratio gauge");
    let _ = writeln!(
        out,
        "# HELP h265_thread_cpu_in_secs_total OS thread-level CPU time in seconds (user / system)."
    );
    let _ = writeln!(out, "# TYPE h265_thread_cpu_in_secs_total counter");
    let _ = writeln!(
        out,
        "# HELP h265_thread_wait_in_secs_total Cumulative seconds spent waiting per thread and wait_reason."
    );
    let _ = writeln!(out, "# TYPE h265_thread_wait_in_secs_total counter");
    let _ = writeln!(
        out,
        "# HELP h265_thread_current_wait_in_secs Live seconds the thread has been waiting right now per wait_reason."
    );
    let _ = writeln!(out, "# TYPE h265_thread_current_wait_in_secs gauge");
    let _ = writeln!(
        out,
        "# HELP h265_thread_wait_cycles_total Total sleep/backoff wait cycles per thread and wait_reason."
    );
    let _ = writeln!(out, "# TYPE h265_thread_wait_cycles_total counter");
    let _ = writeln!(
        out,
        "# HELP h265_thread_task_wait_in_secs Per-task wait duration histogram before/during each task by wait_reason."
    );
    let _ = writeln!(out, "# TYPE h265_thread_task_wait_in_secs histogram");
    let _ = writeln!(
        out,
        "# HELP h265_thread_tasks_started_total Total tasks started by thread and stage."
    );
    let _ = writeln!(out, "# TYPE h265_thread_tasks_started_total counter");
    let _ = writeln!(
        out,
        "# HELP h265_thread_tasks_completed_total Total tasks completed by thread, stage, and outcome."
    );
    let _ = writeln!(out, "# TYPE h265_thread_tasks_completed_total counter");
    let _ = writeln!(
        out,
        "# HELP h265_thread_errors_total Total errors encountered by thread and error_type."
    );
    let _ = writeln!(out, "# TYPE h265_thread_errors_total counter");
    let _ = writeln!(
        out,
        "# HELP h265_stage_duration_in_secs Stage execution duration histogram in seconds."
    );
    let _ = writeln!(out, "# TYPE h265_stage_duration_in_secs histogram");

    for t in snap.threads.values() {
        let base_lbl = format!(
            "nodename=\"{}\",host_os=\"{}\",worker_group=\"{}\",worker_thread=\"{}\"",
            escape_prom_label(&t.nodename),
            escape_prom_label(&t.host_os),
            escape_prom_label(&t.worker_group),
            escape_prom_label(&t.worker_thread)
        );
        let _ = writeln!(out, "h265_thread_state{{{}}} {}", base_lbl, t.state);
        let _ = writeln!(
            out,
            "h265_thread_busy_in_secs_total{{{}}} {:.6}",
            base_lbl, t.busy_in_secs_total
        );
        let _ = writeln!(
            out,
            "h265_thread_utilization_ratio{{{}}} {:.6}",
            base_lbl,
            t.utilization_ratio()
        );
        let _ = writeln!(
            out,
            "h265_thread_cpu_in_secs_total{{{},mode=\"user\"}} {:.6}",
            base_lbl, t.cpu_user_in_secs_total
        );
        let _ = writeln!(
            out,
            "h265_thread_cpu_in_secs_total{{{},mode=\"system\"}} {:.6}",
            base_lbl, t.cpu_system_in_secs_total
        );

        for (reason, &secs) in &t.wait_in_secs_by_reason {
            let _ = writeln!(
                out,
                "h265_thread_wait_in_secs_total{{{},wait_reason=\"{}\"}} {:.6}",
                base_lbl,
                escape_prom_label(reason),
                secs
            );
        }
        for (reason, &cur_secs) in &t.current_wait_in_secs_by_reason {
            let _ = writeln!(
                out,
                "h265_thread_current_wait_in_secs{{{},wait_reason=\"{}\"}} {:.6}",
                base_lbl,
                escape_prom_label(reason),
                cur_secs
            );
        }
        for (reason, &cycles) in &t.wait_cycles_by_reason {
            let _ = writeln!(
                out,
                "h265_thread_wait_cycles_total{{{},wait_reason=\"{}\"}} {}",
                base_lbl,
                escape_prom_label(reason),
                cycles
            );
        }
        for (reason, hist) in &t.task_wait_hist_by_reason {
            let lbl = format!("{},wait_reason=\"{}\"", base_lbl, escape_prom_label(reason));
            write_histogram_prom(&mut out, "h265_thread_task_wait_in_secs", &lbl, hist);
        }
        for (stage, &cnt) in &t.tasks_started_by_stage {
            let _ = writeln!(
                out,
                "h265_thread_tasks_started_total{{{},stage=\"{}\"}} {}",
                base_lbl,
                escape_prom_label(stage),
                cnt
            );
        }
        for (stage, outcomes) in &t.tasks_completed_by_stage_outcome {
            for (outcome, &cnt) in outcomes {
                let _ = writeln!(
                    out,
                    "h265_thread_tasks_completed_total{{{},stage=\"{}\",outcome=\"{}\"}} {}",
                    base_lbl,
                    escape_prom_label(stage),
                    escape_prom_label(outcome),
                    cnt
                );
            }
        }
        for (err_type, &cnt) in &t.errors_by_type {
            let _ = writeln!(
                out,
                "h265_thread_errors_total{{{},error_type=\"{}\"}} {}",
                base_lbl,
                escape_prom_label(err_type),
                cnt
            );
        }
        for (stage, hist) in &t.stage_duration_hist_by_stage {
            let lbl = format!("{},stage=\"{}\"", base_lbl, escape_prom_label(stage));
            write_histogram_prom(&mut out, "h265_stage_duration_in_secs", &lbl, hist);
        }
    }

    // --- Section B: Disk & RAMDisk I/O Metrics ---
    let _ = writeln!(
        out,
        "# HELP h265_disk_io_chunk_duration_in_secs 4MB disk chunk read/write syscall latency histogram."
    );
    let _ = writeln!(out, "# TYPE h265_disk_io_chunk_duration_in_secs histogram");
    let _ = writeln!(
        out,
        "# HELP h265_disk_io_lock_wait_in_secs Per-mount io_lock contention wait latency histogram."
    );
    let _ = writeln!(out, "# TYPE h265_disk_io_lock_wait_in_secs histogram");
    let _ = writeln!(
        out,
        "# HELP h265_disk_io_lock_hold_in_secs_total Cumulative seconds holding per-mount io_lock."
    );
    let _ = writeln!(out, "# TYPE h265_disk_io_lock_hold_in_secs_total counter");
    let _ = writeln!(
        out,
        "# HELP h265_disk_io_lock_waiting_threads Number of threads currently waiting on per-mount io_lock."
    );
    let _ = writeln!(out, "# TYPE h265_disk_io_lock_waiting_threads gauge");
    let _ = writeln!(
        out,
        "# HELP h265_disk_transfer_size_in_mb_total Cumulative megabytes read or written per mount."
    );
    let _ = writeln!(out, "# TYPE h265_disk_transfer_size_in_mb_total counter");
    let _ = writeln!(
        out,
        "# HELP h265_disk_throughput_mbps Live disk read/write throughput in MB/s."
    );
    let _ = writeln!(out, "# TYPE h265_disk_throughput_mbps gauge");
    let _ = writeln!(
        out,
        "# HELP h265_mount_free_gb Available free disk space on mount in GB (via statfs syscall)."
    );
    let _ = writeln!(out, "# TYPE h265_mount_free_gb gauge");
    let _ = writeln!(
        out,
        "# HELP h265_mount_total_gb Total disk capacity on mount in GB (via statfs syscall)."
    );
    let _ = writeln!(out, "# TYPE h265_mount_total_gb gauge");
    let _ = writeln!(
        out,
        "# HELP h265_ramdisk_quota_gb RAMDisk partition usage and limits per mount in GB."
    );
    let _ = writeln!(out, "# TYPE h265_ramdisk_quota_gb gauge");

    for m in snap.mounts.values() {
        let m_lbl = format!(
            "nodename=\"{}\",host_os=\"{}\",mount_point=\"{}\",drive_type=\"{}\"",
            escape_prom_label(&m.nodename),
            escape_prom_label(&m.host_os),
            escape_prom_label(&m.mount_point),
            escape_prom_label(&m.drive_type)
        );
        write_histogram_prom(
            &mut out,
            "h265_disk_io_chunk_duration_in_secs",
            &format!("{},op=\"read\"", m_lbl),
            &m.read_chunk_hist,
        );
        write_histogram_prom(
            &mut out,
            "h265_disk_io_chunk_duration_in_secs",
            &format!("{},op=\"write\"", m_lbl),
            &m.write_chunk_hist,
        );
        for (th, hist) in &m.io_lock_wait_hist_by_thread {
            write_histogram_prom(
                &mut out,
                "h265_disk_io_lock_wait_in_secs",
                &format!(
                    "nodename=\"{}\",host_os=\"{}\",mount_point=\"{}\",worker_thread=\"{}\"",
                    escape_prom_label(&m.nodename),
                    escape_prom_label(&m.host_os),
                    escape_prom_label(&m.mount_point),
                    escape_prom_label(th)
                ),
                hist,
            );
        }
        for (th, &hold) in &m.io_lock_hold_in_secs_by_thread {
            let _ = writeln!(
                out,
                "h265_disk_io_lock_hold_in_secs_total{{nodename=\"{}\",host_os=\"{}\",mount_point=\"{}\",worker_thread=\"{}\"}} {:.6}",
                escape_prom_label(&m.nodename),
                escape_prom_label(&m.host_os),
                escape_prom_label(&m.mount_point),
                escape_prom_label(th),
                hold
            );
        }
        let _ = writeln!(
            out,
            "h265_disk_io_lock_waiting_threads{{nodename=\"{}\",host_os=\"{}\",mount_point=\"{}\"}} {}",
            escape_prom_label(&m.nodename),
            escape_prom_label(&m.host_os),
            escape_prom_label(&m.mount_point),
            m.io_lock_waiting_threads
        );
        let _ = writeln!(
            out,
            "h265_disk_transfer_size_in_mb_total{{{},direction=\"read\"}} {:.3}",
            m_lbl, m.read_size_in_mb_total
        );
        let _ = writeln!(
            out,
            "h265_disk_transfer_size_in_mb_total{{{},direction=\"write\"}} {:.3}",
            m_lbl, m.write_size_in_mb_total
        );
        let _ = writeln!(
            out,
            "h265_disk_throughput_mbps{{{},direction=\"read\"}} {:.3}",
            m_lbl, m.read_throughput_mbps
        );
        let _ = writeln!(
            out,
            "h265_disk_throughput_mbps{{{},direction=\"write\"}} {:.3}",
            m_lbl, m.write_throughput_mbps
        );
        let _ = writeln!(out, "h265_mount_free_gb{{{}}} {:.3}", m_lbl, m.free_gb);
        let _ = writeln!(out, "h265_mount_total_gb{{{}}} {:.3}", m_lbl, m.total_gb);
        let util = if m.total_gb > 0.0 {
            ((m.total_gb - m.free_gb) / m.total_gb).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "h265_mount_utilization_ratio{{{}}} {:.6}",
            m_lbl, util
        );

        let rd_lbl = format!(
            "nodename=\"{}\",host_os=\"{}\",mount_point=\"{}\"",
            escape_prom_label(&m.nodename),
            escape_prom_label(&m.host_os),
            escape_prom_label(&m.mount_point)
        );
        let _ = writeln!(
            out,
            "h265_ramdisk_quota_gb{{{},kind=\"staged_input_used\"}} {:.4}",
            rd_lbl, m.ramdisk_staged_input_used_gb
        );
        let _ = writeln!(
            out,
            "h265_ramdisk_quota_gb{{{},kind=\"staged_output_used\"}} {:.4}",
            rd_lbl, m.ramdisk_staged_output_used_gb
        );
        let _ = writeln!(
            out,
            "h265_ramdisk_quota_gb{{{},kind=\"partition_max\"}} {:.4}",
            rd_lbl, m.ramdisk_partition_max_gb
        );
        let _ = writeln!(
            out,
            "h265_ramdisk_quota_gb{{{},kind=\"future_partition_max\"}} {:.4}",
            rd_lbl, m.ramdisk_future_partition_max_gb
        );

        for (q_state, &cnt) in &m.queue_files_by_state {
            let _ = writeln!(
                out,
                "h265_queue_files_remaining{{mount_point=\"{}\",queue=\"{}\"}} {}",
                escape_prom_label(&m.mount_point),
                escape_prom_label(q_state),
                cnt
            );
        }
        for (q_state, &mb_rem) in &m.queue_size_in_mb_by_state {
            let _ = writeln!(
                out,
                "h265_queue_size_in_mb_remaining{{mount_point=\"{}\",queue=\"{}\"}} {:.3}",
                escape_prom_label(&m.mount_point),
                escape_prom_label(q_state),
                mb_rem
            );
        }
        for (dec, &cnt) in &m.stager_decisions_by_type {
            let _ = writeln!(
                out,
                "h265_stager_decisions_total{{mount_point=\"{}\",decision=\"{}\"}} {}",
                escape_prom_label(&m.mount_point),
                escape_prom_label(dec),
                cnt
            );
        }
        for (outc, &cnt) in &m.resume_checks_by_outcome {
            let _ = writeln!(
                out,
                "h265_resume_checks_total{{mount_point=\"{}\",outcome=\"{}\"}} {}",
                escape_prom_label(&m.mount_point),
                escape_prom_label(outc),
                cnt
            );
        }
        for (outc, &cnt) in &m.subdirs_reconciled_by_outcome {
            let _ = writeln!(
                out,
                "h265_subdirs_reconciled_total{{mount_point=\"{}\",outcome=\"{}\"}} {}",
                escape_prom_label(&m.mount_point),
                escape_prom_label(outc),
                cnt
            );
        }
    }

    // --- Section C: Remote Network, SSH & SCP Metrics ---
    for r in snap.remote_nodes.values() {
        let r_lbl = format!(
            "nodename=\"{}\",host_os=\"{}\"",
            escape_prom_label(&r.nodename),
            escape_prom_label(&r.host_os)
        );
        for (cmd_type, hist) in &r.ssh_rtt_hist_by_cmd {
            write_histogram_prom(
                &mut out,
                "h265_network_ssh_rtt_in_secs",
                &format!("{},cmd_type=\"{}\"", r_lbl, escape_prom_label(cmd_type)),
                hist,
            );
        }
        write_histogram_prom(
            &mut out,
            "h265_network_scp_duration_in_secs",
            &format!("{},direction=\"upload\"", r_lbl),
            &r.scp_upload_duration_hist,
        );
        write_histogram_prom(
            &mut out,
            "h265_network_scp_duration_in_secs",
            &format!("{},direction=\"download\"", r_lbl),
            &r.scp_download_duration_hist,
        );
        let _ = writeln!(
            out,
            "h265_network_scp_transfer_size_in_mb_total{{{},direction=\"upload\"}} {:.3}",
            r_lbl, r.scp_upload_size_in_mb_total
        );
        let _ = writeln!(
            out,
            "h265_network_scp_transfer_size_in_mb_total{{{},direction=\"download\"}} {:.3}",
            r_lbl, r.scp_download_size_in_mb_total
        );
        let _ = writeln!(
            out,
            "h265_network_scp_throughput_mbps{{{},direction=\"upload\"}} {:.3}",
            r_lbl, r.scp_upload_throughput_mbps
        );
        let _ = writeln!(
            out,
            "h265_network_scp_throughput_mbps{{{},direction=\"download\"}} {:.3}",
            r_lbl, r.scp_download_throughput_mbps
        );
        let _ = writeln!(
            out,
            "h265_remote_quota_gb{{{},kind=\"used\"}} {:.4}",
            r_lbl, r.quota_used_gb
        );
        let _ = writeln!(
            out,
            "h265_remote_quota_gb{{{},kind=\"remaining\"}} {:.4}",
            r_lbl, r.quota_remaining_gb
        );
        let _ = writeln!(
            out,
            "h265_remote_quota_gb{{{},kind=\"max\"}} {:.4}",
            r_lbl, r.quota_max_gb
        );
        let _ = writeln!(
            out,
            "h265_remote_quota_gb{{{},kind=\"single_file_cap\"}} {:.4}",
            r_lbl, r.quota_single_file_cap_gb
        );
        let sat = if r.quota_max_gb > 0.0 {
            (r.quota_used_gb / r.quota_max_gb).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "h265_remote_quota_saturation_ratio{{{}}} {:.6}",
            r_lbl, sat
        );
        for (q, &depth) in &r.queue_depth_by_stage {
            let _ = writeln!(
                out,
                "h265_remote_queue_depth{{{},queue=\"{}\"}} {}",
                r_lbl,
                escape_prom_label(q),
                depth
            );
        }
        for (err_t, &cnt) in &r.network_errors_by_type {
            let _ = writeln!(
                out,
                "h265_network_errors_total{{{},error_type=\"{}\"}} {}",
                r_lbl,
                escape_prom_label(err_t),
                cnt
            );
        }
    }

    // --- Section D: Media Transcoding Domain Metrics ---
    for mg in snap.media_groups.values() {
        let mg_lbl = format!(
            "nodename=\"{}\",host_os=\"{}\",worker_group=\"{}\"",
            escape_prom_label(&mg.nodename),
            escape_prom_label(&mg.host_os),
            escape_prom_label(&mg.worker_group)
        );
        for (codec, &mb) in &mg.source_size_in_mb_by_codec {
            let _ = writeln!(
                out,
                "h265_media_source_file_size_in_mb_total{{{},source_codec=\"{}\"}} {:.3}",
                mg_lbl,
                escape_prom_label(codec),
                mb
            );
        }
        for (codec, &mb) in &mg.target_size_in_mb_by_codec {
            let _ = writeln!(
                out,
                "h265_media_target_file_size_in_mb_total{{{},source_codec=\"{}\"}} {:.3}",
                mg_lbl,
                escape_prom_label(codec),
                mb
            );
        }
        let _ = writeln!(
            out,
            "h265_media_storage_saved_in_mb_total{{{}}} {:.3}",
            mg_lbl, mg.storage_saved_in_mb_total
        );
        write_histogram_prom(
            &mut out,
            "h265_media_compression_ratio",
            &mg_lbl,
            &mg.compression_ratio_hist,
        );
        let _ = writeln!(
            out,
            "h265_media_video_duration_encoded_in_secs_total{{{},encoder=\"{}\"}} {:.3}",
            mg_lbl,
            escape_prom_label(&mg.encoder),
            mg.video_duration_encoded_in_secs_total
        );
        let _ = writeln!(
            out,
            "h265_encoder_realtime_speed_x{{{},encoder=\"{}\"}} {:.3}",
            mg_lbl,
            escape_prom_label(&mg.encoder),
            mg.realtime_speed_x
        );
        for (codec, &cnt) in &mg.hwaccel_fallback_by_codec {
            let _ = writeln!(
                out,
                "h265_hwaccel_software_fallback_total{{{},source_codec=\"{}\"}} {}",
                mg_lbl,
                escape_prom_label(codec),
                cnt
            );
        }
        for (codec, &cnt) in &mg.kept_smaller_original_by_codec {
            let _ = writeln!(
                out,
                "h265_kept_smaller_original_total{{{},source_codec=\"{}\"}} {}",
                mg_lbl,
                escape_prom_label(codec),
                cnt
            );
        }
    }

    // --- Section E: Host & GPU Hardware Samples (Remote + Local) ---
    let mut all_hw: Vec<&HostHardwareSample> = snap.remote_hardware.values().collect();
    for s in extra_hw_samples {
        all_hw.push(s);
    }
    for hw in all_hw {
        let h_lbl = format!(
            "nodename=\"{}\",host_os=\"{}\"",
            escape_prom_label(&hw.nodename),
            escape_prom_label(&hw.host_os)
        );
        let _ = writeln!(
            out,
            "h265_host_cpu_utilization_ratio{{{},mode=\"user\"}} {:.6}",
            h_lbl, hw.cpu_user_ratio
        );
        let _ = writeln!(
            out,
            "h265_host_cpu_utilization_ratio{{{},mode=\"system\"}} {:.6}",
            h_lbl, hw.cpu_system_ratio
        );
        let _ = writeln!(
            out,
            "h265_host_cpu_utilization_ratio{{{},mode=\"idle\"}} {:.6}",
            h_lbl, hw.cpu_idle_ratio
        );
        let _ = writeln!(
            out,
            "h265_host_cpu_utilization_ratio{{{},mode=\"active\"}} {:.6}",
            h_lbl, hw.cpu_active_ratio
        );
        let _ = writeln!(
            out,
            "h265_host_load_average{{{},window=\"1m\"}} {:.4}",
            h_lbl, hw.load_avg_1m
        );
        let _ = writeln!(
            out,
            "h265_host_load_average{{{},window=\"5m\"}} {:.4}",
            h_lbl, hw.load_avg_5m
        );
        let _ = writeln!(
            out,
            "h265_host_load_average{{{},window=\"15m\"}} {:.4}",
            h_lbl, hw.load_avg_15m
        );
        let _ = writeln!(
            out,
            "h265_host_memory_gb{{{},state=\"used\"}} {:.4}",
            h_lbl, hw.memory_used_gb
        );
        let _ = writeln!(
            out,
            "h265_host_memory_gb{{{},state=\"free\"}} {:.4}",
            h_lbl, hw.memory_free_gb
        );
        let _ = writeln!(
            out,
            "h265_host_memory_gb{{{},state=\"total\"}} {:.4}",
            h_lbl, hw.memory_total_gb
        );
        if !hw.gpu_name.is_empty() {
            let g_lbl = format!(
                "{},gpu_name=\"{}\"",
                h_lbl,
                escape_prom_label(&hw.gpu_name)
            );
            let _ = writeln!(
                out,
                "h265_gpu_utilization_ratio{{{},engine=\"core\"}} {:.6}",
                g_lbl, hw.gpu_core_utilization_ratio
            );
            let _ = writeln!(
                out,
                "h265_gpu_utilization_ratio{{{},engine=\"encoder\"}} {:.6}",
                g_lbl, hw.gpu_encoder_utilization_ratio
            );
            let _ = writeln!(
                out,
                "h265_gpu_utilization_ratio{{{},engine=\"decoder\"}} {:.6}",
                g_lbl, hw.gpu_decoder_utilization_ratio
            );
            let _ = writeln!(
                out,
                "h265_gpu_memory_in_mb{{{},state=\"used\"}} {:.2}",
                g_lbl, hw.gpu_memory_used_in_mb
            );
            let _ = writeln!(
                out,
                "h265_gpu_memory_in_mb{{{},state=\"total\"}} {:.2}",
                g_lbl, hw.gpu_memory_total_in_mb
            );
            let _ = writeln!(
                out,
                "h265_gpu_temperature_celsius{{{}}} {:.2}",
                g_lbl, hw.gpu_temperature_celsius
            );
            let _ = writeln!(
                out,
                "h265_gpu_power_watts{{{}}} {:.2}",
                g_lbl, hw.gpu_power_watts
            );
        }
    }

    let _ = writeln!(
        out,
        "h265_ramdisk_partition_resizes_total {}",
        snap.ramdisk_partition_resizes_total
    );
    for (lvl, &cnt) in &snap.logs_emitted_by_level {
        let _ = writeln!(
            out,
            "h265_logs_emitted_total{{level=\"{}\"}} {}",
            escape_prom_label(lvl),
            cnt
        );
    }

    out
}

// ============================================================================
// 6. Structured `logfmt` Event Formatter & Parser (1-to-1 Aligned with Prometheus)
// ============================================================================

pub fn format_logfmt_value(raw: &str) -> String {
    if raw.is_empty() {
        return "\"\"".to_string();
    }
    let needs_quotes = raw
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '=' || c == '\\');
    if !needs_quotes {
        raw.to_string()
    } else {
        let escaped = raw
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r");
        format!("\"{}\"", escaped)
    }
}

/// Formats a structured `logfmt` message carrying the mandatory identity labels
/// (`nodename`, `host_os`, `worker_group`, `worker_thread`, `mount_point`, `task_id`, `event`)
/// followed by any stage-specific key-value pairs.
#[allow(clippy::too_many_arguments)]
pub fn format_structured_log_line(
    nodename: &str,
    host_os: &str,
    worker_group: &str,
    worker_thread: &str,
    mount_point: &str,
    task_id: Option<u64>,
    event: &str,
    fields: &[(&str, String)],
) -> String {
    let mut line = String::with_capacity(256);
    let _ = write!(
        line,
        "nodename=\"{}\" host_os=\"{}\" worker_group=\"{}\" worker_thread=\"{}\" mount_point=\"{}\"",
        escape_prom_label(nodename),
        escape_prom_label(host_os),
        escape_prom_label(worker_group),
        escape_prom_label(worker_thread),
        escape_prom_label(mount_point),
    );
    if let Some(tid) = task_id {
        let _ = write!(line, " task_id={}", tid);
    }
    let _ = write!(line, " event=\"{}\"", escape_prom_label(event));

    for (k, v) in fields {
        let _ = write!(line, " {}={}", k, format_logfmt_value(v));
    }
    line
}

/// Parses a `logfmt` log line into a key-value map (used by unit tests and Loki stream label verification).
pub fn parse_logfmt_fields(line: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        while i < len && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= len {
            break;
        }
        let key_start = i;
        while i < len && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
            i += 1;
        }
        if i >= len || bytes[i] != b'=' {
            while i < len && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            continue;
        }
        let key = String::from_utf8_lossy(&bytes[key_start..i]).to_string();
        i += 1; // skip '='
        if i >= len {
            map.insert(key, String::new());
            break;
        }
        let val = if bytes[i] == b'"' {
            i += 1;
            let mut s = String::new();
            while i < len {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 1;
                    match bytes[i] {
                        b'n' => s.push('\n'),
                        b'r' => s.push('\r'),
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        other => s.push(other as char),
                    }
                    i += 1;
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    s.push(bytes[i] as char);
                    i += 1;
                }
            }
            s
        } else {
            let val_start = i;
            while i < len && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            String::from_utf8_lossy(&bytes[val_start..i]).to_string()
        };
        if !key.is_empty() {
            map.insert(key, val);
        }
    }
    map
}
