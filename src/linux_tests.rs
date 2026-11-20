//! Linux-specific unit test suite (`linux_tests::`).
//!
//! Compiled only when targeting Linux (`#[cfg(all(test, target_os = "linux"))]`).
//! Exercises Linux `statvfs`, `/sys/block` drive detection, `CLOCK_THREAD_CPUTIME_ID`,
//! `/dev/shm` tmpfs RAMDisk staging, POSIX process-group termination & nice priority,
//! OpenSSH ControlMaster args, ARM64/Raspberry Pi encoder fallback, and `/proc` metrics.

use super::*;

#[test]
fn linux_statvfs_and_mount_info_sysfs() {
    let tmp = std::env::temp_dir();
    let (avail_b, total_b) = get_mount_space_stats_syscall(&tmp)
        .expect("Linux statvfs must succeed for temp dir");
    assert!(total_b > 0, "Total filesystem bytes must be > 0");
    assert!(avail_b <= total_b, "Available bytes must be <= total bytes");
    assert!(get_available_free_space_bytes(&tmp) > 0);

    let info = get_mount_info(&tmp);
    assert!(
        info.mount_point.starts_with('/'),
        "Linux mount_point must start with '/': {}",
        info.mount_point
    );
    assert!(!info.device.is_empty());
}

#[test]
fn linux_clock_gettime_thread_cpu_and_hostname() {
    let (u0, _s0) = get_current_thread_cpu_time_secs();
    let mut acc: u64 = 0;
    for i in 0..2_000_000u64 {
        acc = acc.wrapping_add(i.wrapping_mul(17));
    }
    std::hint::black_box(acc);
    let (u1, s1) = get_current_thread_cpu_time_secs();

    assert!(u1 >= u0 && u1 > 0.0, "CLOCK_THREAD_CPUTIME_ID should advance: {} -> {}", u0, u1);
    assert!(s1 >= 0.0);

    let host = gethostname_syscall();
    assert!(!host.is_empty());
    assert_ne!(host, "local-host");
}

#[test]
fn linux_dev_shm_staging_and_ramdisk() {
    let mut cfg = AppConfig::default();
    cfg.ENABLE_RAMDISK = true;
    cfg.RAMDISK_NAME = format!("RustyH265TestShm_{}", std::process::id());
    cfg.RAMDISK_CLEANUP_ON_EXIT = true;
    cfg.LOCAL_STAGING_DIR = "/Volumes/RustyH265RAMDisk/staging".to_string();

    let eff = cfg.effective_staging_dir();
    let expected = PathBuf::from("/dev/shm")
        .join(&cfg.RAMDISK_NAME)
        .join("staging");
    assert_eq!(
        eff, expected,
        "Linux ENABLE_RAMDISK should resolve to /dev/shm/<RAMDISK_NAME>/staging"
    );

    if Path::new("/dev/shm").is_dir() {
        let rd = provision_ramdisk(&cfg, false)
            .expect("provision_ramdisk should succeed on Linux /dev/shm")
            .expect("provision_ramdisk should return Some on Linux");
        assert!(rd.staging_dir.exists(), "Staging dir under /dev/shm must be created");
        rd.cleanup(false);
        assert!(
            !rd.mount_path.exists(),
            "Cleanup should remove provisioned /dev/shm dir"
        );
    }
}

#[test]
fn linux_process_group_priority_and_signal_termination() {
    let mut cmd = Command::new("sleep");
    cmd.arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    platform::configure_child_process_group(&mut cmd);

    let mut child = cmd.spawn().expect("Failed to spawn Linux sleep child");
    let pid = child.id();

    let prio_res = platform::set_child_process_priority(pid, 5);
    assert!(
        prio_res.is_ok(),
        "Positive nice (+5) via setpriority should succeed unprivileged on Linux: {:?}",
        prio_res
    );

    let start = Instant::now();
    platform::terminate_process_tree(pid, false);
    let _ = child.wait();
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "SIGTERM process-group kill should terminate child promptly on Linux"
    );
}

#[test]
fn linux_ssh_args_include_control_master() {
    let node = WorkerNodeConfig {
        NODENAME: "mac-master".to_string(),
        IP_ADDRESS: "192.168.1.10".to_string(),
        USERNAME: "dev".to_string(),
        SSH_PORT: 22,
        MAX_SIZE_IN_GB: 30.0,
        FFMPEG_PATH: "/opt/homebrew/bin/ffmpeg".to_string(),
        OS_TYPE: "Mac".to_string(),
        GPU_PROFILE: String::new(),
    };
    let args = ssh_base_args(&node);
    assert!(args.iter().any(|a| a == "ControlMaster=auto"));
    assert!(args.iter().any(|a| a.starts_with("ControlPath=/tmp/rusty_h265_ssh_")));
    assert!(args.iter().any(|a| a == "ControlPersist=120"));
}

#[test]
fn linux_encoder_profile_arm64_fallback() {
    let cfg = AppConfig::default();
    let prof = detect_encoder_profile(&cfg);
    // Must never pick macOS VideoToolbox or Apple AudioToolbox on Linux
    assert!(
        !prof.name.contains("VideoToolbox"),
        "Linux must not select macOS VideoToolbox: {}",
        prof.name
    );
    assert!(
        !prof.audio_args.iter().any(|a| a == "aac_at"),
        "Linux must not select macOS aac_at audio encoder: {:?}",
        prof.audio_args
    );
    let has_nvidia = Command::new("nvidia-smi")
        .arg("-L")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if has_nvidia {
        assert_eq!(prof.name, "NVIDIA NVENC");
    } else {
        assert_eq!(prof.name, "CPU libx265");
    }
}

#[test]
fn linux_python_metrics_agent_procfs_syscalls() {
    let root = tests::test_project_root();
    let script_path = root.join("scripts").join("rusty_h265_metrics_agent.py");
    assert!(
        script_path.exists(),
        "Expected metrics agent script at '{}'",
        script_path.display()
    );

    let out = Command::new("python3")
        .args([
            script_path.to_str().unwrap(),
            "--role",
            "remote",
            "--nodename",
            "pi-arm64-test",
            "--watch-dir",
            "/tmp",
            "--once",
        ])
        .output()
        .expect("Failed to execute metrics agent on Linux");
    assert!(
        out.status.success(),
        "Linux metrics agent failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json_line = stdout
        .lines()
        .find(|l| l.starts_with('{'))
        .expect("Expected JSON hardware sample from metrics agent");
    let sample: HostHardwareSample = serde_json::from_str(json_line).unwrap();
    assert_eq!(sample.host_os, "Linux");
    assert!(sample.memory_total_gb > 0.0);
    assert!((0.0..=1.0).contains(&sample.cpu_active_ratio));
    assert!(sample.watch_dir_total_gb > 0.0);
}
