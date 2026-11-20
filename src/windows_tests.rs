//! Windows-specific unit test suite (`windows_tests::`).
//!
//! Compiled only when targeting Windows (`#[cfg(all(test, target_os = "windows"))]`).
//! Exercises Win32 syscalls, drive-root mount detection, RAMDisk/staging fallback,
//! Windows OpenSSH arg compatibility, process-tree termination, backslash paths,
//! and the Windows syscall path of `rusty_h265_metrics_agent.py`.

use super::*;

fn win_unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "rusty_h265_wintest_{}_{}_{}",
        label,
        std::process::id(),
        nanos
    ))
}

#[test]
fn win_local_time_and_timestamps() {
    let parts = platform::local_time_parts().expect("Win32 GetLocalTime must succeed");
    assert!(
        parts.year >= 2024,
        "Expected current year >= 2024, got {}",
        parts.year
    );
    assert!((1..=12).contains(&parts.month));
    assert!((1..=31).contains(&parts.day));
    assert!(parts.hour < 24 && parts.minute < 60 && parts.second < 60);

    let (date_str, time_str) = format_timestamp_now();
    assert_ne!(
        date_str, "1970-01-01",
        "format_timestamp_now must not fall back to epoch on Windows"
    );
    assert_eq!(date_str.len(), 10);
    assert!(time_str.contains(':'));

    let t12 = format_time_12h_millis_now();
    assert!(t12.ends_with(" am") || t12.ends_with(" pm"));
}

#[test]
fn win_mount_info_and_disk_space_syscall() {
    let tmp = std::env::temp_dir();
    let info = get_mount_info(&tmp);
    assert!(
        !info.mount_point.starts_with(r"\\?\"),
        "Mount point must strip Windows verbatim prefix: {}",
        info.mount_point
    );
    assert!(
        info.mount_point.ends_with('\\') || info.mount_point.starts_with(r"\\"),
        "Expected Windows drive root or UNC share, got: {}",
        info.mount_point
    );

    let (avail_b, total_b) = get_mount_space_stats_syscall(&tmp)
        .expect("Win32 GetDiskFreeSpaceExW must succeed for temp dir");
    assert!(total_b > 0, "Total volume bytes must be > 0");
    assert!(avail_b <= total_b, "Available bytes must be <= total bytes");
    assert!(get_available_free_space_bytes(&tmp) > 0);
}

#[test]
fn win_staging_dir_and_ramdisk_fallback() {
    let mut cfg = AppConfig::default();
    cfg.ENABLE_RAMDISK = true;
    cfg.RAMDISK_NAME = "RustyH265RAMDisk".to_string();
    cfg.LOCAL_STAGING_DIR = "/Volumes/RustyH265RAMDisk/staging".to_string();

    let eff = cfg.effective_staging_dir();
    let eff_str = eff.to_string_lossy();
    assert!(
        !eff_str.starts_with("/Volumes/"),
        "macOS /Volumes path must fall back to Windows temp dir, got: {}",
        eff_str
    );
    assert!(eff_str.contains("RustyH265RAMDisk"));

    // Explicit Windows drive path (e.g. ImDisk R:\staging) must be preserved as-is
    cfg.LOCAL_STAGING_DIR = r"R:\staging".to_string();
    assert_eq!(cfg.effective_staging_dir(), PathBuf::from(r"R:\staging"));

    // provision_ramdisk must cleanly return Ok(None) on Windows (never invoking hdiutil or /dev/shm)
    cfg.LOCAL_STAGING_DIR = "/Volumes/RustyH265RAMDisk/staging".to_string();
    let rd = provision_ramdisk(&cfg, false).expect("provision_ramdisk must not error on Windows");
    assert!(
        rd.is_none(),
        "Windows should not auto-create a macOS/Linux RAMDisk"
    );
}

#[test]
fn win_ssh_args_omit_control_master() {
    let node = WorkerNodeConfig {
        NODENAME: "remote-linux".to_string(),
        IP_ADDRESS: "192.168.1.50".to_string(),
        USERNAME: "dev".to_string(),
        SSH_PORT: 2222,
        MAX_SIZE_IN_GB: 20.0,
        FFMPEG_PATH: "/usr/bin/ffmpeg".to_string(),
        OS_TYPE: "Linux".to_string(),
        GPU_PROFILE: String::new(),
    };
    let args = ssh_base_args(&node);
    assert!(
        !args.iter().any(|a| a.contains("ControlMaster")),
        "Windows OpenSSH must omit ControlMaster: {:?}",
        args
    );
    assert!(
        !args.iter().any(|a| a.contains("ControlPath")),
        "Windows OpenSSH must omit ControlPath: {:?}",
        args
    );
    assert!(
        !args.iter().any(|a| a.contains("ControlPersist")),
        "Windows OpenSSH must omit ControlPersist: {:?}",
        args
    );
    assert!(args.iter().any(|a| a == "BatchMode=yes"));
    assert!(args.iter().any(|a| a == "dev@192.168.1.50"));
}

#[test]
fn win_thread_cpu_time_and_hostname() {
    let mut acc: u64 = 0;
    for i in 0..200_000u64 {
        acc = acc.wrapping_add(i.wrapping_mul(31));
    }
    std::hint::black_box(acc);

    let (user_s, sys_s) = get_current_thread_cpu_time_secs();
    assert!(user_s.is_finite() && user_s >= 0.0);
    assert!(sys_s.is_finite() && sys_s >= 0.0);

    let host = gethostname_syscall();
    assert!(!host.is_empty());
    if let Ok(env_host) = std::env::var("COMPUTERNAME") {
        if !env_host.trim().is_empty() {
            assert_eq!(host, env_host.trim());
        }
    }
}

#[test]
fn win_process_priority_and_tree_termination() {
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", "ping", "-n", "15", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    platform::configure_child_process_group(&mut cmd);

    let mut child = cmd.spawn().expect("Failed to spawn Windows cmd child process");
    let pid = child.id();

    let prio_res = platform::set_child_process_priority(pid, 5);
    assert!(
        prio_res.is_ok(),
        "SetPriorityClass(BELOW_NORMAL) should succeed for own child: {:?}",
        prio_res
    );

    let start = Instant::now();
    platform::terminate_process_tree(pid, true);
    let _ = child.wait();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "Child process tree should terminate promptly on Windows"
    );
}

#[test]
fn win_terminal_dimensions_and_backslash_subdir_cleanup() {
    let (cols, rows) = get_terminal_dimensions();
    assert!(cols >= 40 && rows >= 10);

    let base = win_unique_temp_dir("backslash_cleanup");
    let sub_src = base.join("shows").join("clip_dir");
    let sub_tgt = base.join("shows").join("clip_dir_h265");
    ensure_dir_created(&sub_src).unwrap();
    ensure_dir_created(&sub_tgt).unwrap();

    let good_src = sub_src.join("ok_video.mp4");
    let bad_src = sub_src.join("broken_video.mkv");
    fs::write(&good_src, vec![b'a'; 1024]).unwrap();
    fs::write(&bad_src, vec![b'b'; 1024]).unwrap();
    fs::write(sub_tgt.join("ok_video_h265.mp4"), vec![b'c'; 512]).unwrap();

    let broken_log = base.join("broken_directories.journal");
    let mut cfg = AppConfig::default();
    cfg.FAILED_DIRS_PATH = broken_log.display().to_string();

    let trackers: Arc<Mutex<HashMap<PathBuf, SubdirTracker>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let next_id = AtomicU64::new(1);
    let queue = populate_mount_queue_for_dirs(
        &get_mount_info(&base).mount_point,
        &[base.join("shows")],
        &cfg,
        &next_id,
        &trackers,
        false,
    );
    assert_eq!(queue.len(), 2);

    let ui: Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let wkey = format!("local:{}", get_mount_info(&base).mount_point);
    ui.lock()
        .unwrap()
        .insert(wkey.clone(), WorkerUiSnapshot::default());

    for item in &queue {
        let is_bad = item.source_path == bad_src;
        complete_queued_item(
            item,
            false,
            false,
            is_bad,
            512,
            &trackers,
            &ui,
            &wkey,
            &cfg,
            false,
        );
    }

    assert!(!good_src.exists(), "Converted source file must be deleted on Windows");
    assert!(bad_src.exists(), "Corrupt source file must remain on Windows");
    assert!(broken_log.exists(), "broken_directories.journal must be written");

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn win_python_metrics_agent_syscalls() {
    let py = platform::python_command_name();
    let has_py = Command::new(py)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !has_py {
        eprintln!("[win_tests] Skipping Python metrics agent test: '{}' not in PATH", py);
        return;
    }

    let root = tests::test_project_root();
    let script_path = root.join("scripts").join("rusty_h265_metrics_agent.py");
    assert!(
        script_path.exists(),
        "Expected metrics agent script at '{}'",
        script_path.display()
    );

    let out = Command::new(py)
        .args([
            script_path.to_str().unwrap(),
            "--role",
            "remote",
            "--nodename",
            "win-test-node",
            "--watch-dir",
            std::env::temp_dir().to_str().unwrap(),
            "--once",
        ])
        .output()
        .expect("Failed to execute metrics agent on Windows");
    assert!(
        out.status.success(),
        "Windows metrics agent failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json_line = stdout
        .lines()
        .find(|l| l.starts_with('{'))
        .expect("Expected JSON hardware sample from metrics agent");
    let sample: HostHardwareSample = serde_json::from_str(json_line).unwrap();
    assert_eq!(sample.host_os, "Windows");
    assert!(sample.memory_total_gb > 0.0);
    assert!(sample.watch_dir_total_gb > 0.0);
}
