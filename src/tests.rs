//! Unit test suite for `rusty_h265_converter`.
//!
//! Organized by architectural subsystem:
//! 1. Logging & Size Parsing (`logging.conf`, `RotatingLogger`, `LogLevel`)
//! 2. YAML Configuration & Staging Paths (`config.yaml`, `AppConfig`, `WorkerNodeConfig`)
//! 3. Media Path, Filename & Codec Heuristics (`build_h265_filename`, audio/video compatibility)
//! 4. Remote SSH, SCP Progress & Encoder Discovery (`parse_scp_progress_line`, `candidate_remote_hevc_encoders`)
//! 5. Queue Ordering, Work Stealing & Step 2.6 Subdir Reconciliation

use super::*;

/// Resolves the project root for tests both locally (`cargo test`) and when a cross-compiled
/// standalone test binary is executed on a remote target via `scripts/verify.sh`.
pub(crate) fn test_project_root() -> PathBuf {
    if let Ok(env_root) = std::env::var("RUSTY_H265_TEST_ROOT") {
        let p = PathBuf::from(env_root);
        if p.exists() {
            return p;
        }
    }
    let compile_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if compile_dir.exists() {
        return compile_dir;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

// ============================================================================
// Test Fixture Builders
// ============================================================================

fn sample_remote_node(os_type: &str) -> WorkerNodeConfig {
    WorkerNodeConfig {
        NODENAME: "remote-node-test".to_string(),
        IP_ADDRESS: "192.168.1.50".to_string(),
        USERNAME: "media".to_string(),
        SSH_PORT: 2222,
        MAX_SIZE_IN_GB: 20.0,
        FFMPEG_PATH: "/usr/bin/ffmpeg".to_string(),
        OS_TYPE: os_type.to_string(),
        GPU_PROFILE: String::new(),
    }
}

fn sample_mount_queue(
    mount_point: &str,
    active_remote_managers: usize,
    item_id: u64,
) -> Arc<MountQueueState> {
    let mut ready_queue = VecDeque::new();
    ready_queue.push_back(QueuedMediaItem {
        item_id,
        mount_point: mount_point.to_string(),
        root_dir: PathBuf::from(mount_point),
        source_subdir: None,
        target_subdir: None,
        source_path: PathBuf::from(format!("{}/video_{}.mp4", mount_point, item_id)),
        dest_dir: PathBuf::from(mount_point),
        file_size_bytes: 1_000,
        target_label: "test_target".to_string(),
        target_size_bytes: 1_000,
        is_root_in_place: true,
        cached_probe: None,
        force_unstaged: false,
    });

    Arc::new(MountQueueState {
        mount_point: mount_point.to_string(),
        root_dirs: vec![PathBuf::from(mount_point)],
        info: MountInfo {
            mount_point: mount_point.to_string(),
            device: "/dev/disk1s1".to_string(),
            drive_type: DriveType::Hdd,
        },
        raw_queue: Mutex::new(VecDeque::new()),
        queue: Mutex::new(ready_queue),
        in_flight_items: Mutex::new(BTreeMap::new()),
        verifier_done: AtomicBool::new(true),
        io_lock: Mutex::new(()),
        active_remote_managers: AtomicUsize::new(active_remote_managers),
        total_enqueued: AtomicUsize::new(1),
        local_worker_finished: AtomicBool::new(false),
        in_flight_local_tasks: AtomicUsize::new(0),
    })
}

// ============================================================================
// 1. Logging & Size Parsing
// ============================================================================

#[test]
fn logging_conf_loads_defaults_and_rotates_files() {
    let conf_path = test_project_root().join("logging.conf");
    let loaded_cfg = LoggingConfig::load(&conf_path);
    assert!(matches!(loaded_cfg.level, LogLevel::Info | LogLevel::Warn));
    assert_eq!(loaded_cfg.max_files, 5);
    assert_eq!(loaded_cfg.max_file_size_bytes, 20 * 1024 * 1024);

    let tmp_log = std::env::temp_dir().join(format!("test_rotating_h265_{}.log", std::process::id()));
    let backup_1 = PathBuf::from(format!("{}.1", tmp_log.display()));
    let backup_2 = PathBuf::from(format!("{}.2", tmp_log.display()));
    let _ = fs::remove_file(&tmp_log);
    let _ = fs::remove_file(&backup_1);
    let _ = fs::remove_file(&backup_2);

    let logger = RotatingLogger {
        config: LoggingConfig {
            level: LogLevel::Info,
            log_file: tmp_log.clone(),
            max_files: 3,
            max_file_size_bytes: 120,
            format_template:
                "YYYY-mm-dd HH:MM:SS.millisec [LOG_LEVEL] <log message> [<stacktrace if error>]"
                    .to_string(),
        },
        lock: Mutex::new(()),
    };

    logger.log(LogLevel::Info, "First log entry exceeding small threshold", None);
    logger.log(
        LogLevel::Error,
        "Second error entry with stacktrace",
        Some("frame 0: src/main.rs:42"),
    );
    logger.log(LogLevel::Info, "Third log entry triggering file rotation", None);

    assert!(tmp_log.exists(), "Active log file should exist");
    assert!(backup_1.exists(), "Rotated backup .1 file should exist");

    let _ = fs::remove_file(&tmp_log);
    let _ = fs::remove_file(&backup_1);
    let _ = fs::remove_file(&backup_2);
}

#[test]
fn log_level_and_byte_size_parsers_handle_all_units() {
    assert_eq!(LogLevel::from_str_loose("trace"), LogLevel::Trace);
    assert_eq!(LogLevel::from_str_loose("DEBUG"), LogLevel::Debug);
    assert_eq!(LogLevel::from_str_loose(" info "), LogLevel::Info);
    assert_eq!(LogLevel::from_str_loose("WARNING"), LogLevel::Warn);
    assert_eq!(LogLevel::from_str_loose("err"), LogLevel::Error);
    assert_eq!(LogLevel::from_str_loose("unrecognized"), LogLevel::Info);

    assert!(LogLevel::Trace < LogLevel::Debug);
    assert!(LogLevel::Debug < LogLevel::Info);
    assert!(LogLevel::Info < LogLevel::Warn);
    assert!(LogLevel::Warn < LogLevel::Error);

    assert_eq!(parse_size_to_bytes("1024"), Some(1_024));
    assert_eq!(parse_size_to_bytes("512B"), Some(512));
    assert_eq!(parse_size_to_bytes("2KB"), Some(2_048));
    assert_eq!(parse_size_to_bytes("4K"), Some(4_096));
    assert_eq!(parse_size_to_bytes("10MB"), Some(10 * 1024 * 1024));
    assert_eq!(
        parse_size_to_bytes("1.5M"),
        Some((1.5 * 1024.0 * 1024.0) as u64)
    );
    assert_eq!(parse_size_to_bytes("2GB"), Some(2 * 1024 * 1024 * 1024));
    assert_eq!(parse_size_to_bytes("invalid"), None);
}

// ============================================================================
// 2. YAML Configuration & Staging Paths
// ============================================================================

#[test]
fn project_config_yaml_loads_with_worker_nodes() {
    let yaml_path = test_project_root().join("config.yaml");
    let cfg = AppConfig::load(&yaml_path).expect("Should load workspace config.yaml");

    assert_eq!(cfg.RESOURCE_CEILING, 90.0);
    assert_eq!(cfg.MIN_BITRATE_KBPS, 1800);
    assert!(cfg.ENABLE_RAMDISK);
    assert_eq!(cfg.RAMDISK_NAME, "RustyH265RAMDisk");
    assert_eq!(cfg.STAGER_WAIT_MSEC, 150);
    assert_eq!(cfg.FAILED_DIRS_PATH, "failed_dirs.txt");
    assert_eq!(cfg.TASK_JOURNAL_PATH, "distributed_tasks_journal.json");
    assert_eq!(cfg.WORKER_NODES.len(), 2);
}

#[test]
fn custom_yaml_overrides_scalars_lists_and_remote_nodes() {
    let custom_yaml = r#"
RESOURCE_CEILING: 85.5
MIN_BITRATE_KBPS: 2200
KEEP_SMALLER_ORIGINAL: false
COPY_COMPATIBLE_AUDIO: true
SORT_BY_SIZE_DESC: false
ENABLE_RAMDISK: false
STAGER_WAIT_MSEC: 250
LOCAL_STAGING_DIR: "/tmp/custom_staging"
VIDEO_EXTENSIONS: [.mp4, .mkv, ".mov"]
FFMPEG_DECODE_ARGS:
  - "-threads"
  - "4"
WORKER_NODES:
  - IP_ADDRESS: "10.0.0.10"
    USERNAME: "alice"
    SSH_PORT: 2222
    MAX_SIZE_IN_GB: 15.5
    FFMPEG_PATH: "/usr/local/bin/ffmpeg"
    OS_TYPE: "macOS"
    GPU_PROFILE: "AppleSilicon"
"#;

    let cfg = AppConfig::from_yaml_str(custom_yaml).expect("Should parse custom YAML");
    assert_eq!(cfg.RESOURCE_CEILING, 85.5);
    assert_eq!(cfg.MIN_BITRATE_KBPS, 2200);
    assert!(!cfg.KEEP_SMALLER_ORIGINAL);
    assert!(cfg.COPY_COMPATIBLE_AUDIO);
    assert!(!cfg.SORT_BY_SIZE_DESC);
    assert!(!cfg.ENABLE_RAMDISK);
    assert_eq!(cfg.STAGER_WAIT_MSEC, 250);
    assert_eq!(
        cfg.effective_staging_dir(),
        PathBuf::from("/tmp/custom_staging")
    );
    assert_eq!(cfg.VIDEO_EXTENSIONS, vec![".mp4", ".mkv", ".mov"]);
    assert_eq!(cfg.FFMPEG_DECODE_ARGS, vec!["-threads", "4"]);

    assert_eq!(cfg.WORKER_NODES.len(), 1);
    let node = &cfg.WORKER_NODES[0];
    assert_eq!(node.IP_ADDRESS, "10.0.0.10");
    assert_eq!(node.USERNAME, "alice");
    assert_eq!(node.SSH_PORT, 2222);
    assert_eq!(node.MAX_SIZE_IN_GB, 15.5);
    assert_eq!(node.OS_TYPE, "macOS");
    assert_eq!(node.GPU_PROFILE, "AppleSilicon");
}

#[test]
fn boolean_and_quote_helpers_normalize_strings() {
    for truthy in ["true", "TRUE", "yes", "1", "on"] {
        assert!(parse_bool(truthy), "Expected '{}' to parse as true", truthy);
    }
    for falsy in ["false", "FALSE", "no", "0", "off", ""] {
        assert!(!parse_bool(falsy), "Expected '{}' to parse as false", falsy);
    }

    assert_eq!(unquote("\"hello\""), "hello");
    assert_eq!(unquote("'world'"), "world");
    assert_eq!(unquote("  plain  "), "plain");
    assert_eq!(unquote("\""), "\"");
    assert_eq!(unquote(""), "");
}

// ============================================================================
// 3. Media Path, Filename & Codec Heuristics
// ============================================================================

#[test]
fn path_and_filename_builders_preserve_h265_conventions() {
    let cfg = AppConfig::default();

    assert_eq!(
        sanitize_dir_or_file_component("movie_folder \r\n\0"),
        "movie_folder"
    );

    assert!(is_h265_named(Path::new("/tmp/clip_h265.mp4"), "_h265"));
    assert!(is_h265_named(Path::new("/tmp/clip_H265.mkv"), "_h265"));
    assert!(!is_h265_named(Path::new("/tmp/clip_orig.mp4"), "_h265"));

    assert!(is_subdir_h265_named(Path::new("/Volumes/d/show_h265"), "_h265"));
    assert!(is_subdir_h265_named(Path::new("/Volumes/d/show_h265. "), "_h265"));
    assert!(!is_subdir_h265_named(Path::new("/Volumes/d/show"), "_h265"));

    let root = Path::new("/Volumes/media");
    assert!(relative_path_has_h265_dir_component(
        root,
        Path::new("/Volumes/media/sub_h265/video.mp4"),
        "_h265"
    ));
    assert!(!relative_path_has_h265_dir_component(
        root,
        Path::new("/Volumes/media/sub_normal/video.mp4"),
        "_h265"
    ));

    assert_eq!(
        build_h265_filename(Path::new("/a/b/sample.wmv"), false, &cfg),
        "sample_h265.mp4"
    );
    assert_eq!(
        build_h265_filename(Path::new("/a/b/sample.wmv"), true, &cfg),
        "sample_h265.wmv"
    );
    assert_eq!(
        build_h265_filename(Path::new("/a/b/already_h265.mkv"), false, &cfg),
        "already_h265.mp4"
    );

    let (cand_mp4, cand_orig) = expected_target_candidates_for_source(
        Path::new("/src/dir/season1/ep01.mkv"),
        Path::new("/src/dir"),
        Path::new("/dst/dir_h265"),
        &cfg,
    );
    assert_eq!(cand_mp4, PathBuf::from("/dst/dir_h265/season1/ep01_h265.mp4"));
    assert_eq!(cand_orig, PathBuf::from("/dst/dir_h265/season1/ep01_h265.mkv"));
}

#[test]
fn codec_compatibility_and_display_formatters_work_correctly() {
    assert!(is_mp4_compatible_audio("aac"));
    assert!(is_mp4_compatible_audio("ac3"));
    assert!(is_mp4_compatible_audio("mp3"));
    assert!(!is_mp4_compatible_audio("pcm_s16le"));
    assert!(!is_mp4_compatible_audio("vorbis"));

    assert!(is_macos_hw_decode_supported("h264"));
    assert!(is_macos_hw_decode_supported("hevc"));
    assert!(is_macos_hw_decode_supported("prores"));
    assert!(!is_macos_hw_decode_supported("wmv3"));

    assert_eq!(parse_hms_to_secs("01:02:03.5"), Some(3723.5));
    assert_eq!(parse_hms_to_secs("00:00:10.00"), Some(10.0));
    assert_eq!(parse_hms_to_secs("invalid"), None);

    assert_eq!(format_bytes(500 * 1024 * 1024), "500.00 MB");
    assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.00 GB");

    assert_eq!(format_duration_hms(0.0), "00:00");
    assert_eq!(format_duration_hms(-5.0), "00:00");
    assert_eq!(format_duration_hms(65.0), "01:05");
    assert_eq!(format_duration_hms(3661.0), "01:01:01");

    assert_eq!(format_filename_with_size("clip.mp4", 0), "clip.mp4");
    assert_eq!(format_filename_with_size("clip.mp4", 2048), "clip.mp4 (2KB)");
    assert_eq!(
        format_filename_with_size("clip.mp4", 5 * 1024 * 1024),
        "clip.mp4 (5MB)"
    );
}

// ============================================================================
// 4. Remote SSH, SCP Progress & Encoder Discovery
// ============================================================================

#[test]
fn candidate_remote_hevc_encoders_prioritizes_hwaccel_by_os() {
    let linux_encoders = candidate_remote_hevc_encoders(
        " V..... hevc_nvenc \n V..... libx265 ",
        "Linux",
    );
    assert_eq!(linux_encoders.len(), 2);
    assert_eq!(linux_encoders[0].0, "hevc_nvenc");
    assert_eq!(linux_encoders[1].0, "libx265");

    let mac_encoders = candidate_remote_hevc_encoders(
        " V..... hevc_videotoolbox \n V..... hevc_qsv \n V..... hevc_amf \n V..... libx265 ",
        "macOS",
    );
    assert_eq!(mac_encoders.len(), 4);
    assert_eq!(mac_encoders[0].0, "hevc_videotoolbox");
    assert_eq!(mac_encoders[1].0, "hevc_qsv");
    assert_eq!(mac_encoders[2].0, "hevc_amf");
    assert_eq!(mac_encoders[3].0, "libx265");
}

#[test]
fn scp_progress_and_nvenc_api_mismatch_parsers_extract_metrics() {
    let in_flight_line = "task_16_in.mp4  42%  128MB  84.5MB/s   00:04 ETA";
    let (pct, speed, kbps, eta_secs, is_eta) =
        parse_scp_progress_line(in_flight_line).expect("Should parse in-flight scp frame");
    assert_eq!(pct, 42.0);
    assert_eq!(speed.as_deref(), Some("84.5MB/s"));
    assert!(kbps.unwrap() > 0);
    assert_eq!(eta_secs, Some(4.0));
    assert!(is_eta);

    let finished_line = "task_16_in.mp4 100%  256MB  64.0MB/s   00:01:05";
    let (pct_done, _, _, total_secs, is_eta_done) =
        parse_scp_progress_line(finished_line).expect("Should parse finished scp frame");
    assert_eq!(pct_done, 100.0);
    assert_eq!(total_secs, Some(65.0));
    assert!(!is_eta_done);

    assert_eq!(parse_scp_progress_line("non-progress output"), None);

    let nvenc_stderr =
        "[hevc_nvenc @ 0x55] Driver does not support the required nvenc API version. Required: 12.2 Found: 12.1\nError";
    assert_eq!(
        extract_found_nvenc_api_version(nvenc_stderr),
        Some("12.1".to_string())
    );
    assert_eq!(extract_found_nvenc_api_version("No version info"), None);
}

#[test]
fn ssh_command_builders_and_tui_formatters_behave_as_expected() {
    let linux_node = sample_remote_node("Linux");
    assert_eq!(
        ssh_control_path(&linux_node),
        "/tmp/rusty_h265_ssh_media_192.168.1.50_2222"
    );

    let args = ssh_base_args(&linux_node);
    assert!(args.contains(&"2222".to_string()));
    assert!(args.contains(&"media@192.168.1.50".to_string()));

    let linux_wrapped = wrap_remote_shell_cmd(&linux_node, "ffmpeg -version");
    assert!(linux_wrapped.contains("export PATH="));
    assert!(linux_wrapped.ends_with("ffmpeg -version"));

    let windows_node = sample_remote_node("Windows");
    assert_eq!(
        wrap_remote_shell_cmd(&windows_node, "ffmpeg -version"),
        "ffmpeg -version"
    );

    assert_eq!(truncate_middle("short", 10), "short");
    assert_eq!(
        truncate_middle("abcdefghijklmnopqrstuvwxyz", 10),
        "abc...wxyz"
    );
    assert_eq!(pad_or_truncate("abc", 6), "abc   ");

    let bar = render_progress_bar(50.0, 10);
    assert!(bar.contains("█████"));
    assert!(bar.contains("░░░░░"));

    let mut cfg = AppConfig::default();
    assert_eq!(staging_mode_label(DriveType::Hdd, &cfg), "RAMDisk (8.0G)");
    cfg.ENABLE_RAMDISK = false;
    assert_eq!(staging_mode_label(DriveType::Hdd, &cfg), "NVMe-Staged");
    assert_eq!(staging_mode_label(DriveType::Ssd, &cfg), "Direct");

    assert_eq!(DriveType::Hdd.as_str(), "HDD");
    assert_eq!(DriveType::Ssd.as_str(), "SSD");
    assert_eq!(DriveType::Nvme.as_str(), "NVMe");
}

// ============================================================================
// 5. Queue Ordering, Work Stealing & Step 2.6 Subdir Reconciliation
// ============================================================================

#[test]
fn record_failed_dir_writes_to_error_log() {
    let tmp_log = std::env::temp_dir().join(format!("test_failed_dirs_{}.txt", std::process::id()));
    let _ = fs::remove_file(&tmp_log);

    let err = anyhow::anyhow!("Invalid argument (os error 22)");
    record_failed_dir(
        &tmp_log,
        Path::new("/Volumes/f0m/f/strictly_glamor"),
        Some(Path::new("/Volumes/f0m/f/strictly_glamor_h265")),
        "Test directory failure",
        &err,
    );

    let contents = fs::read_to_string(&tmp_log).expect("Should write failed_dirs.txt");
    assert!(contents.contains("/Volumes/f0m/f/strictly_glamor"));
    assert!(contents.contains("/Volumes/f0m/f/strictly_glamor_h265"));
    let _ = fs::remove_file(&tmp_log);
}

#[test]
fn queue_orders_largest_subdir_first_and_deletes_completed_source_subdir() {
    let base = std::env::temp_dir().join(format!("rusty_h265_unittest_{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    ensure_dir_created(&base).expect("Should create base test dir");

    let sub_small = base.join("small_sub");
    let sub_large = base.join("large_sub");
    let sub_already_h265 = base.join("already_h265");
    ensure_dir_created(&sub_small).unwrap();
    ensure_dir_created(&sub_large).unwrap();
    ensure_dir_created(&sub_already_h265).unwrap();

    fs::write(sub_small.join("a.mp4"), vec![b'x'; 1024]).unwrap();
    fs::write(sub_large.join("b.mkv"), vec![b'y'; 4096]).unwrap();
    fs::write(sub_already_h265.join("ignored.mp4"), vec![b'z'; 8192]).unwrap();
    fs::write(base.join("root_video.mov"), vec![b'r'; 512]).unwrap();
    fs::write(base.join(".hidden.mp4"), vec![b'h'; 512]).unwrap();

    let mut cfg = AppConfig::default();
    cfg.FAILED_DIRS_PATH = base.join("failed_dirs.txt").display().to_string();

    assert_eq!(compute_directory_size_bytes(&base), 1024 + 4096 + 512 + 512);

    let trackers: Arc<Mutex<HashMap<PathBuf, SubdirTracker>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let next_id = AtomicU64::new(1);
    let queue = populate_mount_queue_for_dirs(
        "/tmp",
        std::slice::from_ref(&base),
        &cfg,
        &next_id,
        &trackers,
        false,
    );

    // Expect 3 items ordered: large_sub/b.mkv -> small_sub/a.mp4 -> root_video.mov
    assert_eq!(queue.len(), 3);
    assert!(queue[0].source_path.ends_with("large_sub/b.mkv"));
    assert!(queue[1].source_path.ends_with("small_sub/a.mp4"));
    assert!(queue[2].source_path.ends_with("root_video.mov"));
    assert!(queue[2].is_root_in_place);

    // Simulate Step 2.6 subdir completion when target_count == source_count
    let small_item = queue[1].clone();
    let target_small = small_item.target_subdir.as_ref().unwrap();
    ensure_dir_created(target_small).unwrap();
    fs::write(target_small.join("a_h265.mp4"), vec![b'c'; 512]).unwrap();

    let shared_ui: Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    shared_ui
        .lock()
        .unwrap()
        .insert("local:/tmp".to_string(), WorkerUiSnapshot::default());

    complete_queued_item(
        &small_item,
        false,
        false,
        false,
        512,
        &trackers,
        &shared_ui,
        "local:/tmp",
        &cfg,
        false,
    );

    assert!(
        !sub_small.exists(),
        "Completed source subdir should be removed once target count matches"
    );
    assert!(target_small.join("a_h265.mp4").exists());

    let ui_snap = shared_ui.lock().unwrap().get("local:/tmp").cloned().unwrap();
    assert_eq!(ui_snap.batch_converted_files, 1);
    assert_eq!(ui_snap.batch_completed_files, 1);
    assert_eq!(ui_snap.bytes_saved_total, 512);

    let _ = fs::remove_dir_all(&base);
}


#[test]
fn work_stealing_prioritizes_unoccupied_mounts_and_persists_task_journal() {
    let mq_busy = sample_mount_queue("/mnt/busy", 2, 1);
    let mq_free = sample_mount_queue("/mnt/free", 0, 2);
    let mount_queues = vec![Arc::clone(&mq_busy), Arc::clone(&mq_free)];
    let rng = AtomicU64::new(1);
    let max_single_file_bytes = 5_000;

    // 0. Files exceeding 50% of MAX_SIZE_IN_GB (max_single_file_bytes) are skipped by remote managers
    assert!(
        pick_mount_queue_prioritizing_unoccupied(&mount_queues, &rng, 500).is_none(),
        "Files exceeding 50% of MAX_SIZE_IN_GB should remain in queue for local workers"
    );

    // 1. Unoccupied mount (/mnt/free) is selected first
    let (picked_mq, picked_item) =
        pick_mount_queue_prioritizing_unoccupied(&mount_queues, &rng, max_single_file_bytes)
            .expect("Should pick item");
    assert_eq!(picked_mq.mount_point, "/mnt/free");
    assert_eq!(picked_item.item_id, 2);

    // 2. Subsequent steal falls back to /mnt/busy once /mnt/free is empty
    let (picked_mq2, picked_item2) =
        pick_mount_queue_prioritizing_unoccupied(&mount_queues, &rng, max_single_file_bytes)
            .expect("Should pick second item");
    assert_eq!(picked_mq2.mount_point, "/mnt/busy");
    assert_eq!(picked_item2.item_id, 1);
    assert!(!picked_mq2.has_pending_work());

    assert!(is_remote_out_of_space_error(
        "Task finished with error code: -28 (No space left on device)"
    ));
    assert!(!is_remote_out_of_space_error("moov atom not found"));

    // 3. TaskJournal persists and reloads task state from JSON
    let journal_path = std::env::temp_dir().join(format!(
        "test_task_journal_{}.json",
        std::process::id()
    ));
    let _ = fs::remove_file(&journal_path);

    let journal = TaskJournal::new(journal_path.clone());
    journal.upsert(DistributedTaskObject {
        task_id: 99,
        node_id: "remote:user@host".to_string(),
        original_queue: "/mnt/free".to_string(),
        original_path: picked_item.source_path.clone(),
        file_size_bytes: 1_000,
        remote_input_path: "/tmp/in.mp4".to_string(),
        remote_output_path: "/tmp/out.mp4".to_string(),
        final_h265_dest_path: PathBuf::from("/mnt/free/video_2_h265.mp4"),
        copy_to_remote_state: StageStatus::Completed,
        conversion_state: StageStatus::Started,
        copy_back_state: StageStatus::Pending,
        error_occurred: None,
        updated_at_epoch_secs: 0,
        queued_item: picked_item,
        fallback_retry_attempted: false,
    });

    let reloaded = TaskJournal::new(journal_path.clone());
    let guard = reloaded.tasks.lock().unwrap();
    assert_eq!(guard.len(), 1);
    assert_eq!(guard.get(&99).unwrap().conversion_state, StageStatus::Started);
    drop(guard);

    let _ = fs::remove_file(&journal_path);
}

#[test]
fn subdir_mismatch_logs_missing_and_colliding_files_when_source_exceeds_target() {
    let base = std::env::temp_dir().join(format!(
        "rusty_h265_mismatch_src_gt_tgt_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&base);
    ensure_dir_created(&base).expect("Should create base test dir");

    let sub_src = base.join("mismatch_sub");
    ensure_dir_created(&sub_src).unwrap();

    // 3 source files:
    // - clip1.mp4 -> present in target (clip1_h265.mp4)
    // - clip1.wmv -> collides with clip1.mp4 on clip1_h265.mp4
    // - missing_clip.mp4 -> missing in target
    fs::write(sub_src.join("clip1.mp4"), vec![b'a'; 1024]).unwrap();
    fs::write(sub_src.join("clip1.wmv"), vec![b'b'; 1024]).unwrap();
    fs::write(sub_src.join("missing_clip.mp4"), vec![b'c'; 1024]).unwrap();

    let failed_log = base.join("failed_dirs.txt");
    let mut cfg = AppConfig::default();
    cfg.FAILED_DIRS_PATH = failed_log.display().to_string();

    let trackers: Arc<Mutex<HashMap<PathBuf, SubdirTracker>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let next_id = AtomicU64::new(1);
    let queue = populate_mount_queue_for_dirs(
        "/tmp",
        std::slice::from_ref(&base),
        &cfg,
        &next_id,
        &trackers,
        false,
    );
    assert_eq!(queue.len(), 3);

    // Only create 1 target file (`clip1_h265.mp4`), so source=3 > target=1
    let target_sub = queue[0].target_subdir.as_ref().unwrap().clone();
    ensure_dir_created(&target_sub).unwrap();
    fs::write(target_sub.join("clip1_h265.mp4"), vec![b'x'; 512]).unwrap();

    let shared_ui: Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    shared_ui
        .lock()
        .unwrap()
        .insert("local:/tmp".to_string(), WorkerUiSnapshot::default());

    for item in &queue {
        complete_queued_item(
            item,
            false,
            false,
            false,
            100,
            &trackers,
            &shared_ui,
            "local:/tmp",
            &cfg,
            false,
        );
    }

    assert!(
        sub_src.exists(),
        "Source subdir must remain intact when source > target"
    );
    let log_contents = fs::read_to_string(&failed_log).expect("Should write failed_dirs.txt");
    assert!(
        log_contents.contains("Media file count mismatch (source=3 vs target=1) and 2 source file(s) missing in target:"),
        "Log should report count mismatch and missing count: {}",
        log_contents
    );
    assert!(
        log_contents.contains("missing_clip.mp4"),
        "Log should list missing_clip.mp4: {}",
        log_contents
    );
    assert!(
        log_contents.contains("collides with"),
        "Log should report colliding source file: {}",
        log_contents
    );

    let _ = fs::remove_dir_all(&base);
}


#[test]
fn worker_group_partition_sizing_and_dynamic_resizing() {
    let mut cfg = AppConfig::default();
    cfg.ENABLE_RAMDISK = true;
    cfg.RAMDISK_SIZE_GB = 4.0;
    let gb = 1024 * 1024 * 1024u64;

    // 2 active Worker-Groups: WG1 has 3 items in queue, WG2 has 1 item in queue.
    let mq1 = sample_mount_queue("/Volumes/Drive1", 0, 1);
    {
        let mut q = mq1.queue.lock().unwrap();
        let template = q[0].clone();
        q.push_back(QueuedMediaItem { item_id: 2, ..template.clone() });
        q.push_back(QueuedMediaItem { item_id: 3, ..template });
    }
    let mq2 = sample_mount_queue("/Volumes/Drive2", 0, 4);
    let all_mqs = vec![Arc::clone(&mq1), Arc::clone(&mq2)];

    // WG1 has depth 3 > WG2 depth 1:
    // - Current partition = 4 GB / 2 = 2 GB (max_input_stage_size = 1 GB)
    // - Future partition after WG2 finishes = 4 GB / 1 = 4 GB (future_max_input_stage_size = 2 GB)
    let (part1, max_in1, future_max_in1) =
        compute_worker_group_partition_limits(&mq1, &all_mqs, &cfg);
    assert_eq!(part1, 2 * gb);
    assert_eq!(max_in1, 1 * gb);
    assert_eq!(future_max_in1, 2 * gb);

    // WG2 has depth 1 < WG1 depth 3 (no active group has a strictly smaller queue):
    // - Current partition = 2 GB (max_input_stage_size = 1 GB)
    // - Future partition = 2 GB (future_max_input_stage_size = 1 GB)
    let (part2, max_in2, future_max_in2) =
        compute_worker_group_partition_limits(&mq2, &all_mqs, &cfg);
    assert_eq!(part2, 2 * gb);
    assert_eq!(max_in2, 1 * gb);
    assert_eq!(future_max_in2, 1 * gb);

    // Once WG2 marks local_worker_finished = true, WG1 dynamically gets the full 4 GB partition immediately.
    mq2.local_worker_finished.store(true, Ordering::Relaxed);
    let (part1_after, max_in1_after, future_max_in1_after) =
        compute_worker_group_partition_limits(&mq1, &all_mqs, &cfg);
    assert_eq!(part1_after, 4 * gb);
    assert_eq!(max_in1_after, 2 * gb);
    assert_eq!(future_max_in1_after, 2 * gb);
}

#[test]
fn stager_queue_inspection_rotates_and_handles_case_a_b1_b2() {
    let gb = 1024 * 1024 * 1024u64;
    let max_input_stage_size = 1 * gb; // Current partition = 2 GB -> MAX_INPUT_STAGE_SIZE = 1 GB
    let future_max_input_stage_size = 2 * gb; // Future partition = 4 GB -> future MAX_INPUT_STAGE_SIZE = 2 GB

    let make_item = |id: u64, size: u64| QueuedMediaItem {
        item_id: id,
        mount_point: "/Volumes/Drive1".to_string(),
        root_dir: PathBuf::from("/Volumes/Drive1"),
        source_subdir: None,
        target_subdir: None,
        source_path: PathBuf::from(format!("/Volumes/Drive1/video_{}.mp4", id)),
        dest_dir: PathBuf::from("/Volumes/Drive1"),
        file_size_bytes: size,
        target_label: "test".to_string(),
        target_size_bytes: size,
        is_root_in_place: true,
        cached_probe: None,
        force_unstaged: false,
    };

    // Case A: Currently 700 MB is already staged (300 MB available).
    // Front item is 600 MB (<= 1 GB max_input_stage_size, but > 300 MB available),
    // second item is 250 MB (<= 300 MB available).
    // Stager should rotate the 600 MB item to the back and immediately stage the 250 MB item!
    let mb = 1024 * 1024u64;
    let mut q = VecDeque::from(vec![make_item(1, 600 * mb), make_item(2, 250 * mb)]);
    let outcome = select_next_item_for_stager(
        &mut q,
        max_input_stage_size,
        future_max_input_stage_size,
        700 * mb,
    );
    match outcome {
        StagerQueueOutcome::StageNow(it) => assert_eq!(it.item_id, 2),
        other => panic!("Expected StageNow(2), got {:?}", other),
    }
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].item_id, 1);
    assert!(!q[0].force_unstaged);

    // Case B1 vs Case B2:
    // Queue has:
    // - item 10: 1500 MB (> 1 GB current max, but <= 2 GB future max -> Case B1: wait for resize, do NOT mark force_unstaged)
    // - item 11: 2500 MB (> 2 GB future max -> Case B2: permanently too large, mark force_unstaged = true and rotate to back)
    let mut q2 = VecDeque::from(vec![make_item(10, 1500 * mb), make_item(11, 2500 * mb)]);
    let outcome_b = select_next_item_for_stager(
        &mut q2,
        max_input_stage_size,
        future_max_input_stage_size,
        0,
    );
    assert!(matches!(outcome_b, StagerQueueOutcome::WaitForSpaceOrResize));
    assert_eq!(q2.len(), 2);
    // Item 10 should NOT be marked force_unstaged (waiting for partition resize)
    let item10 = q2.iter().find(|i| i.item_id == 10).unwrap();
    assert!(!item10.force_unstaged);
    // Item 11 SHOULD be marked force_unstaged (permanently too large even after resize)
    let item11 = q2.iter().find(|i| i.item_id == 11).unwrap();
    assert!(item11.force_unstaged);

    // Now simulate the smaller Worker-Group finishing so max_input_stage_size expands to 2 GB:
    // Item 10 (1500 MB) now stages immediately, and then Item 11 (2500 MB, force_unstaged) is processed unstaged!
    let outcome_after_resize =
        select_next_item_for_stager(&mut q2, 2 * gb, 2 * gb, 0);
    match outcome_after_resize {
        StagerQueueOutcome::StageNow(it) => assert_eq!(it.item_id, 10),
        other => panic!("Expected StageNow(10) after partition resize, got {:?}", other),
    }

    let outcome_unstaged =
        select_next_item_for_stager(&mut q2, 2 * gb, 2 * gb, 1500 * mb);
    match outcome_unstaged {
        StagerQueueOutcome::ProcessUnstaged(it) => {
            assert_eq!(it.item_id, 11);
            assert!(it.force_unstaged);
        }
        other => panic!("Expected ProcessUnstaged(11), got {:?}", other),
    }
    assert!(q2.is_empty());
}

#[test]
fn converter_failure_handoff_to_flusher_evicts_staged_input_and_records_broken_journal() {
    let base = std::env::temp_dir().join(format!(
        "rusty_h265_flusher_evict_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&base);
    ensure_dir_created(&base).unwrap();

    let broken_journal_path = base.join("broken_files.journal");
    init_broken_files_journal(broken_journal_path);

    let failed_dirs_log = base.join("failed_dirs.txt");
    let mut cfg = AppConfig::default();
    cfg.FAILED_DIRS_PATH = failed_dirs_log.display().to_string();

    let source_path = base.join("corrupt_input.mp4");
    fs::write(&source_path, vec![b'v'; 4096]).unwrap();

    let staged_in = base.join("stage_item99_in.mp4");
    fs::write(&staged_in, vec![b's'; 4096]).unwrap();
    let staged_out = base.join("stage_item99_out.mp4");
    // Simulate converter failing without producing an output file (`staged_out` does not exist)

    let ms = sample_mount_queue("/tmp", 0, 99);
    ms.in_flight_local_tasks.store(1, Ordering::Relaxed);

    let wg = Arc::new(LocalWorkerGroupState {
        to_convert: Mutex::new(VecDeque::new()),
        to_flush: Mutex::new(VecDeque::new()),
        staged_input_bytes: AtomicU64::new(4096),
        staged_output_bytes: AtomicU64::new(0),
        stager_done: AtomicBool::new(true),
        converter_done: AtomicBool::new(true),
    });

    let item = QueuedMediaItem {
        item_id: 99,
        mount_point: "/tmp".to_string(),
        root_dir: base.clone(),
        source_subdir: None,
        target_subdir: None,
        source_path: source_path.clone(),
        dest_dir: base.clone(),
        file_size_bytes: 4096,
        target_label: "test_target".to_string(),
        target_size_bytes: 4096,
        is_root_in_place: true,
        cached_probe: None,
        force_unstaged: false,
    };

    let task = LocalPipelineTask {
        item,
        probe: VideoProbeInfo::default(),
        staged_input: Some(staged_in.clone()),
        staged_input_size: 4096,
        staged_output: Some(staged_out.clone()),
        staged_output_size: 0,
        final_encoded_dest: base.join("corrupt_input_h265.mp4"),
        kept_original: false,
        saved_bytes: 0,
        conversion_error: Some(anyhow::anyhow!(
            "Converter did not produce a non-empty output file at {:?}",
            staged_out
        )),
    };

    let trackers: Arc<Mutex<HashMap<PathBuf, SubdirTracker>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let ui: Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>> = Arc::new(Mutex::new(BTreeMap::new()));
    ui.lock()
        .unwrap()
        .insert("local:/tmp".to_string(), WorkerUiSnapshot::default());

    execute_flusher_task(
        task,
        &ms,
        &wg,
        &trackers,
        &ui,
        &cfg,
        "local:/tmp",
        &failed_dirs_log,
        3,
        false,
    );

    // 1. Staged input file must be evicted from RAMDisk and partition space reclaimed
    assert!(
        !staged_in.exists(),
        "Flusher must evict staged input file when converter fails"
    );
    assert_eq!(wg.staged_input_bytes.load(Ordering::Relaxed), 0);
    assert_eq!(ms.in_flight_local_tasks.load(Ordering::Relaxed), 0);

    // 2. Source file must be recorded in broken_files.journal and failed_dirs.txt
    assert!(
        is_file_marked_broken(&source_path),
        "Flusher must record failed source file in broken_files.journal"
    );
    let failed_log_text = fs::read_to_string(&failed_dirs_log).expect("Should write failed_dirs.txt");
    assert!(
        failed_log_text.contains("evicted from RAMDisk & recorded in broken_files.journal"),
        "failed_dirs.txt should log eviction and broken journal entry: {}",
        failed_log_text
    );

    // 3. UI error counter must be incremented
    let snap = ui.lock().unwrap().get("local:/tmp").cloned().unwrap();
    assert_eq!(snap.batch_error_files, 1);

    let _ = fs::remove_dir_all(&base);
}


#[test]
fn remote_encode_failure_handoff_evicts_remote_input_and_records_broken_journal() {
    let base = std::env::temp_dir().join(format!(
        "rusty_h265_remote_evict_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&base);
    ensure_dir_created(&base).unwrap();

    let broken_journal_path = base.join("broken_files.journal");
    init_broken_files_journal(broken_journal_path);

    let failed_dirs_log = base.join("failed_dirs.txt");
    let mut cfg = AppConfig::default();
    cfg.FAILED_DIRS_PATH = failed_dirs_log.display().to_string();

    // Verify OS-specific remote cleanup command construction for Unix and Windows
    let linux_node = WorkerNodeConfig {
        IP_ADDRESS: "10.0.0.50".to_string(),
        USERNAME: "media".to_string(),
        OS_TYPE: "Linux".to_string(),
        ..WorkerNodeConfig::default()
    };
    let win_node = WorkerNodeConfig {
        IP_ADDRESS: "10.0.0.51".to_string(),
        USERNAME: "media".to_string(),
        OS_TYPE: "Windows".to_string(),
        ..WorkerNodeConfig::default()
    };

    let linux_rm = build_remote_cleanup_command(
        &linux_node,
        "/tmp/remote/task_42_in.mp4",
        "/tmp/remote/task_42_out_h265.mp4",
    );
    assert_eq!(
        linux_rm,
        "rm -f \"/tmp/remote/task_42_in.mp4\" \"/tmp/remote/task_42_out_h265.mp4\""
    );

    let win_rm = build_remote_cleanup_command(
        &win_node,
        "C:/Temp/task_42_in.mp4",
        "C:/Temp/task_42_out_h265.mp4",
    );
    assert!(
        win_rm.contains("Remove-Item -LiteralPath 'C:\\Temp\\task_42_in.mp4','C:\\Temp\\task_42_out_h265.mp4'"),
        "Unexpected Windows cleanup command: {}",
        win_rm
    );

    // Create simulated source file and simulated remote scp-up input file
    let source_path = base.join("remote_broken_video.mkv");
    let file_size = 8192u64;
    fs::write(&source_path, vec![b'x'; file_size as usize]).unwrap();

    let remote_in_file = base.join("task_42_in.mkv");
    fs::write(&remote_in_file, vec![b'r'; file_size as usize]).unwrap();
    let remote_out_file = base.join("task_42_out_h265.mp4");
    // Simulate remote ffmpeg failing without producing a valid output file
    let final_local_dest = base.join("remote_broken_video_h265.mp4");

    let mq = sample_mount_queue("/tmp", 0, 42);
    // Simulate dispatcher having popped task 42 from mq.queue when dispatching to remote node
    mq.queue.lock().unwrap().clear();
    let item = QueuedMediaItem {
        item_id: 42,
        mount_point: "/tmp".to_string(),
        root_dir: base.clone(),
        source_subdir: None,
        target_subdir: None,
        source_path: source_path.clone(),
        dest_dir: base.clone(),
        file_size_bytes: file_size,
        target_label: "remote_target".to_string(),
        target_size_bytes: file_size,
        is_root_in_place: true,
        cached_probe: None,
        force_unstaged: false,
    };

    let task = DistributedTaskObject {
        task_id: 42,
        node_id: "remote:media@10.0.0.50".to_string(),
        original_queue: "/tmp".to_string(),
        original_path: source_path.clone(),
        file_size_bytes: file_size,
        remote_input_path: remote_in_file.display().to_string(),
        remote_output_path: remote_out_file.display().to_string(),
        final_h265_dest_path: final_local_dest.clone(),
        copy_to_remote_state: StageStatus::Completed,
        conversion_state: StageStatus::Failed,
        copy_back_state: StageStatus::Pending,
        error_occurred: Some(
            "Remote ffmpeg exited with 1 | stderr: Invalid data found when processing input"
                .to_string(),
        ),
        updated_at_epoch_secs: 0,
        queued_item: item,
        fallback_retry_attempted: false,
    };

    let inflight: Arc<Mutex<BTreeMap<u64, (Arc<MountQueueState>, DistributedTaskObject)>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    inflight
        .lock()
        .unwrap()
        .insert(42, (Arc::clone(&mq), task.clone()));

    let active_in_pipeline = Arc::new(AtomicUsize::new(1));
    let remote_used_input_bytes = Arc::new(AtomicU64::new(file_size));
    let max_size_bytes = 8 * 1024 * 1024 * 1024u64;
    let task_journal_path = base.join("task_state.journal");
    let journal = Arc::new(TaskJournal::new(task_journal_path.clone()));

    let trackers: Arc<Mutex<HashMap<PathBuf, SubdirTracker>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let wkey = "remote:10.0.0.50";
    let ui: Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>> = Arc::new(Mutex::new(BTreeMap::new()));
    ui.lock()
        .unwrap()
        .insert(wkey.to_string(), WorkerUiSnapshot::default());

    let captured_cmd = Arc::new(Mutex::new(String::new()));
    let captured_cmd_ref = Arc::clone(&captured_cmd);

    execute_remote_evict_failed_task(
        task,
        &inflight,
        &active_in_pipeline,
        &remote_used_input_bytes,
        max_size_bytes,
        &journal,
        &trackers,
        &ui,
        wkey,
        &linux_node,
        &cfg,
        false,
        |_node, cmd| {
            *captured_cmd_ref.lock().unwrap() = cmd.to_string();
            // Simulate executing the remote `rm -f` cleanup on the remote host
            let _ = fs::remove_file(&remote_in_file);
            let _ = fs::remove_file(&remote_out_file);
        },
    );

    // 1. Remote scp-up input file must be evicted and cleanup command must target both input & output
    let executed_rm = captured_cmd.lock().unwrap().clone();
    assert!(
        executed_rm.contains(&remote_in_file.display().to_string())
            && executed_rm.contains(&remote_out_file.display().to_string()),
        "Expected remote cleanup command to remove both input and output paths, got: {}",
        executed_rm
    );
    assert!(
        !remote_in_file.exists(),
        "Remote scp-up input file must be deleted on conversion failure"
    );
    assert!(
        !final_local_dest.exists(),
        "No local target output file should be written when remote conversion fails"
    );

    // 2. Remote partition accounting, inflight map, and pipeline counter must be reclaimed
    assert_eq!(remote_used_input_bytes.load(Ordering::Relaxed), 0);
    assert!(inflight.lock().unwrap().is_empty());
    assert_eq!(active_in_pipeline.load(Ordering::Relaxed), 0);
    assert!(
        mq.queue.lock().unwrap().is_empty(),
        "Broken media file must not be re-queued into the mount queue"
    );

    // 3. Source file must be recorded in broken_files.journal and failed_dirs.txt
    assert!(
        is_file_marked_broken(&source_path),
        "Remote eviction must record broken source file in broken_files.journal"
    );
    let failed_log_text =
        fs::read_to_string(&failed_dirs_log).expect("Should write failed_dirs.txt");
    assert!(
        failed_log_text.contains("evicted remote input")
            && failed_log_text.contains("broken_files.journal"),
        "failed_dirs.txt should log remote input eviction and broken_files.journal entry: {}",
        failed_log_text
    );

    // 4. UI snapshot and TaskJournal must record the error and free space reclamation
    let snap = ui.lock().unwrap().get(wkey).cloned().unwrap();
    assert_eq!(snap.batch_error_files, 1);
    assert_eq!(snap.free_space_bytes, max_size_bytes);
    let persisted = journal.tasks.lock().unwrap().get(&42).cloned().unwrap();
    assert_eq!(persisted.conversion_state, StageStatus::Failed);

    let _ = fs::remove_dir_all(&base);
}


#[test]
fn queue_snapshot_deep_copies_inflight_and_queued_items_and_sorts_columns() {
    let root = PathBuf::from("/Volumes/f0m/f");
    let make_item = |id: u64, rel_name: &str, sz: u64| QueuedMediaItem {
        item_id: id,
        mount_point: "/Volumes/f0m".to_string(),
        root_dir: root.clone(),
        source_subdir: None,
        target_subdir: None,
        source_path: root.join(rel_name),
        dest_dir: root.clone(),
        file_size_bytes: sz,
        target_label: rel_name.to_string(),
        target_size_bytes: sz,
        is_root_in_place: true,
        cached_probe: None,
        force_unstaged: false,
    };

    let item_local_active = make_item(1, "charlie_movie.mkv", 500_000_000);
    let item_remote_active = make_item(2, "alpha_episode.mp4", 900_000_000);
    let item_queued_ready = make_item(3, "delta_clip.mp4", 100_000_000);
    let item_queued_pending = make_item(4, "bravo_film.mkv", 300_000_000);

    let mq = Arc::new(MountQueueState {
        mount_point: "/Volumes/f0m".to_string(),
        root_dirs: vec![root.clone()],
        info: MountInfo {
            mount_point: "/Volumes/f0m".to_string(),
            device: "/dev/disk4s1".to_string(),
            drive_type: DriveType::Hdd,
        },
        raw_queue: Mutex::new(VecDeque::from(vec![item_queued_pending.clone()])),
        queue: Mutex::new(VecDeque::from(vec![item_queued_ready.clone()])),
        in_flight_items: Mutex::new(BTreeMap::new()),
        verifier_done: AtomicBool::new(false),
        io_lock: Mutex::new(()),
        active_remote_managers: AtomicUsize::new(1),
        total_enqueued: AtomicUsize::new(4),
        local_worker_finished: AtomicBool::new(false),
        in_flight_local_tasks: AtomicUsize::new(1),
    });

    mq.set_in_flight(
        &item_local_active,
        QueueEntryCategory::ActiveLocal,
        "LOCAL [Converter: Encoding]",
    );
    mq.set_in_flight(
        &item_remote_active,
        QueueEntryCategory::ActiveRemote,
        "REMOTE 10.0.0.50 [Encoding]",
    );

    // 1. Take deep-copy snapshot and verify human-friendly summary header
    let mut snap = snapshot_mount_queue_to_flat_list(&mq);
    let summary = snap.header_summary();
    assert!(
        summary.starts_with("Queue of files from root: /Volumes/f0m/f at ")
            && summary.contains(". Total: 4 files"),
        "Unexpected summary format: {}",
        summary
    );
    assert_eq!(snap.active_count, 2);
    assert_eq!(snap.queued_count, 2);
    assert_eq!(snap.displayed_entries.len(), 4);

    // Verify original snapshot order: ActiveLocal (1), ActiveRemote (2), QueuedReady (3), QueuedPending (4)
    let orig_ids: Vec<u64> = snap.displayed_entries.iter().map(|e| e.item_id).collect();
    assert_eq!(orig_ids, vec![1, 2, 3, 4]);

    // 2. Verify deep-copy isolation: mutating live queues and in_flight_items does NOT alter snapshot
    mq.clear_in_flight(1);
    mq.clear_in_flight(2);
    mq.queue.lock().unwrap().clear();
    mq.raw_queue.lock().unwrap().clear();
    assert_eq!(snap.displayed_entries.len(), 4);
    assert_eq!(snap.original_entries.len(), 4);

    // 3. Test sorting by Column 1 (Name) Ascending (:s1a) and Descending (:s1d)
    let msg_s1a = parse_and_apply_queue_command(&mut snap, ":s1a").unwrap();
    assert!(msg_s1a.contains("Col 1 (Name) ASC"));
    let names_asc: Vec<&str> = snap
        .displayed_entries
        .iter()
        .map(|e| e.display_path.as_str())
        .collect();
    assert_eq!(
        names_asc,
        vec![
            "alpha_episode.mp4",
            "bravo_film.mkv",
            "charlie_movie.mkv",
            "delta_clip.mp4"
        ]
    );

    let msg_s1d = parse_and_apply_queue_command(&mut snap, ":s1d").unwrap();
    assert!(msg_s1d.contains("Col 1 (Name) DESC"));
    let names_desc: Vec<&str> = snap
        .displayed_entries
        .iter()
        .map(|e| e.display_path.as_str())
        .collect();
    assert_eq!(
        names_desc,
        vec![
            "delta_clip.mp4",
            "charlie_movie.mkv",
            "bravo_film.mkv",
            "alpha_episode.mp4"
        ]
    );

    // 4. Test sorting by Column 2 (Size) Ascending (:s2a) and Descending (:s2d)
    parse_and_apply_queue_command(&mut snap, ":s2a").unwrap();
    let sizes_asc: Vec<u64> = snap
        .displayed_entries
        .iter()
        .map(|e| e.file_size_bytes)
        .collect();
    assert_eq!(
        sizes_asc,
        vec![100_000_000, 300_000_000, 500_000_000, 900_000_000]
    );

    parse_and_apply_queue_command(&mut snap, ":s2d").unwrap();
    let sizes_desc: Vec<u64> = snap
        .displayed_entries
        .iter()
        .map(|e| e.file_size_bytes)
        .collect();
    assert_eq!(
        sizes_desc,
        vec![900_000_000, 500_000_000, 300_000_000, 100_000_000]
    );

    // 5. Test sorting by Column 3 (Status / Worker) Ascending (:s3a) and Descending (:s3d)
    parse_and_apply_queue_command(&mut snap, ":s3a").unwrap();
    assert_eq!(
        snap.displayed_entries[0].category,
        QueueEntryCategory::ActiveLocal
    );
    assert_eq!(
        snap.displayed_entries[3].category,
        QueueEntryCategory::QueuedPending
    );

    parse_and_apply_queue_command(&mut snap, ":s3d").unwrap();
    assert_eq!(
        snap.displayed_entries[0].category,
        QueueEntryCategory::QueuedPending
    );
    assert_eq!(
        snap.displayed_entries[3].category,
        QueueEntryCategory::ActiveLocal
    );

    // 6. Test resetting sort (:r) back to original snapshot order
    let msg_r = parse_and_apply_queue_command(&mut snap, ":r").unwrap();
    assert!(msg_r.contains("Reset sort"));
    assert!(snap.active_sort.is_none());
    let reset_ids: Vec<u64> = snap.displayed_entries.iter().map(|e| e.item_id).collect();
    assert_eq!(reset_ids, vec![1, 2, 3, 4]);

    // 7. Test interactive key state machine (v -> Enter -> :s1a -> :r -> Esc -> Esc)
    mq.set_in_flight(
        &item_local_active,
        QueueEntryCategory::ActiveLocal,
        "LOCAL [Converter: Encoding]",
    );
    mq.queue.lock().unwrap().push_back(item_queued_ready);
    let tui_state = Arc::new(Mutex::new(TuiInteractiveState::default()));
    let mqs = vec![Arc::clone(&mq)];

    // Press 'v' to open QueueSelector
    process_tui_input_chunk(b"v", &tui_state, &mqs, 20);
    assert_eq!(tui_state.lock().unwrap().mode, TuiViewMode::QueueSelector);

    // Press Enter to snapshot and enter QueuePager
    process_tui_input_chunk(b"\r", &tui_state, &mqs, 20);
    assert_eq!(tui_state.lock().unwrap().mode, TuiViewMode::QueuePager);
    assert_eq!(
        tui_state
            .lock()
            .unwrap()
            .active_snapshot
            .as_ref()
            .unwrap()
            .displayed_entries
            .len(),
        2
    );

    // Type ':s1a' + Enter to sort by name ascending
    process_tui_input_chunk(b":s1a\r", &tui_state, &mqs, 20);
    assert_eq!(
        tui_state
            .lock()
            .unwrap()
            .active_snapshot
            .as_ref()
            .unwrap()
            .active_sort,
        Some(QueueSortSpec {
            column: SortColumn::Name,
            direction: SortDirection::Asc
        })
    );

    // Type ':r' + Enter to reset sort
    process_tui_input_chunk(b":r\r", &tui_state, &mqs, 20);
    assert!(tui_state
        .lock()
        .unwrap()
        .active_snapshot
        .as_ref()
        .unwrap()
        .active_sort
        .is_none());

    // Press Esc to return to QueueSelector, then Esc again to return to Dashboard
    process_tui_input_chunk(b"\x1b", &tui_state, &mqs, 20);
    assert_eq!(tui_state.lock().unwrap().mode, TuiViewMode::QueueSelector);
    process_tui_input_chunk(b"\x1b", &tui_state, &mqs, 20);
    assert_eq!(tui_state.lock().unwrap().mode, TuiViewMode::Dashboard);
}

#[test]
fn test_observability_config_nodenames_and_syscalls() {
    let cfg_path = test_project_root().join("config.yaml");
    let cfg = AppConfig::load(&cfg_path).expect("config.yaml must parse cleanly");
    assert_eq!(cfg.LOCAL_NODENAME, "mac-master");
    assert_eq!(cfg.effective_local_nodename(), "mac-master");
    assert_eq!(cfg.PROMETHEUS_LISTEN_ADDR, "0.0.0.0:9184");
    assert!(!cfg.AUTO_SPAWN_METRICS_AGENT);
    assert!(cfg.LOKI_ENABLED);
    assert_eq!(cfg.LOKI_ENDPOINT, "http://127.0.0.1:3100/loki/api/v1/push");
    assert_eq!(cfg.LOKI_BATCH_WAIT_MSEC, 1000);
    assert_eq!(
        cfg.METRICS_AGENT_SCRIPT_PATH,
        "scripts/rusty_h265_metrics_agent.py"
    );
    assert!(cfg.WORKER_NODES.len() >= 2);
    assert_eq!(cfg.WORKER_NODES[0].effective_nodename(), "win-rtx3050ti");
    assert_eq!(cfg.WORKER_NODES[1].effective_nodename(), "mac-mini-m2");

    // Verify fallback when NODENAME is empty
    let fallback_node = WorkerNodeConfig {
        NODENAME: String::new(),
        IP_ADDRESS: "10.0.0.99".to_string(),
        ..Default::default()
    };
    assert_eq!(fallback_node.effective_nodename(), "10.0.0.99");

    // Direct OS syscall checks (cross-platform: temp_dir works on macOS, Linux, and Windows)
    let tmp_dir = std::env::temp_dir();
    let (free_b, total_b) =
        get_mount_space_stats_syscall(&tmp_dir).expect("mount space syscall must succeed");
    assert!(total_b > 0);
    assert!(free_b <= total_b);

    let (cpu_user, cpu_sys) = get_current_thread_cpu_time_secs();
    assert!(cpu_user >= 0.0 && cpu_sys >= 0.0);
    let host = gethostname_syscall();
    assert!(!host.is_empty());

    // Platform shim checks
    let parts = platform::local_time_parts().expect("local_time_parts must succeed");
    assert!(parts.year >= 2024 && (1..=12).contains(&parts.month) && (1..=31).contains(&parts.day));
    assert_eq!(
        platform::parse_windows_mount_root(r"\\?\D:\Videos\clip.mp4"),
        (r"D:\".to_string(), "D:".to_string())
    );
    assert_eq!(
        platform::parse_windows_mount_root(r"\\?\UNC\nas01\media\shows"),
        (r"\\nas01\media".to_string(), r"\\nas01\media".to_string())
    );
}

#[test]
fn test_thread_wait_tracker_and_structured_logfmt_and_prometheus_rendering() {
    let mut tracker = ThreadWaitTracker::new(
        "mac-master",
        "MacOS",
        "local:/Volumes/Media1",
        "stager",
        "/Volumes/Media1",
    );

    tracker.sleep_with_reason(WaitReason::RamdiskQuotaFull, Duration::from_millis(12));
    tracker.sleep_with_reason(WaitReason::UpstreamQueueEmpty, Duration::from_millis(8));
    tracker.sleep_with_reason(WaitReason::FlusherPriorityYield, Duration::from_millis(5));

    let mut breakdown = tracker.begin_active_task("stage_file");
    assert_eq!(breakdown.wait_cycles, 3);
    assert!(breakdown.quota_wait_in_secs >= 0.010);
    assert!(breakdown.upstream_queue_wait_in_secs >= 0.005);
    assert!(breakdown.flusher_priority_wait_in_secs >= 0.003);
    assert!(breakdown.thread_wait_in_secs >= 0.018);

    breakdown.add_io_lock_wait(0.015);
    assert!(breakdown.io_lock_wait_in_secs >= 0.014);
    assert!(breakdown.thread_wait_in_secs >= 0.033);

    tracker.complete_active_task("stage_file", "success", 0.250);
    tracker.mark_finished();

    // Structured logfmt line generation & round-trip parsing
    let log_line = format_structured_log_line(
        "mac-master",
        "MacOS",
        "local:/Volumes/Media1",
        "stager",
        "/Volumes/Media1",
        Some(42),
        "stage_file_complete",
        &[
            ("stage_mode", "hdd_to_ramdisk".to_string()),
            ("source_file", "/Volumes/Media1/Movie 1.mkv".to_string()),
            ("source_file_size_in_mb", "1420.50".to_string()),
            (
                "thread_wait_in_secs",
                format!("{:.3}", breakdown.thread_wait_in_secs),
            ),
            ("quota_used_gb", "2.10".to_string()),
            ("quota_remaining_gb", "2.90".to_string()),
        ],
    );
    let parsed = parse_logfmt_fields(&log_line);
    assert_eq!(parsed.get("nodename").map(|s| s.as_str()), Some("mac-master"));
    assert_eq!(parsed.get("host_os").map(|s| s.as_str()), Some("MacOS"));
    assert_eq!(
        parsed.get("worker_group").map(|s| s.as_str()),
        Some("local:/Volumes/Media1")
    );
    assert_eq!(parsed.get("worker_thread").map(|s| s.as_str()), Some("stager"));
    assert_eq!(parsed.get("task_id").map(|s| s.as_str()), Some("42"));
    assert_eq!(
        parsed.get("event").map(|s| s.as_str()),
        Some("stage_file_complete")
    );
    assert_eq!(
        parsed.get("source_file").map(|s| s.as_str()),
        Some("/Volumes/Media1/Movie 1.mkv")
    );
    assert_eq!(
        parsed.get("source_file_size_in_mb").map(|s| s.as_str()),
        Some("1420.50")
    );

    // Snapshot & Prometheus OpenMetrics rendering
    global_metrics().set_local_identity("mac-master", "MacOS");
    global_metrics().update_mount_space("/Volumes/Media1", "HDD", 500_000_000, 1_000_000_000);
    global_metrics().update_remote_quota("win-rtx3050ti", "Windows", 2_000_000_000, 10_000_000_000);
    let snap = global_metrics().snapshot();
    let prom_text = render_prometheus_from_snapshot(&snap, &[]);
    assert!(prom_text.contains("h265_thread_wait_in_secs_total"));
    assert!(prom_text.contains("wait_reason=\"ramdisk_quota_full\""));
    assert!(prom_text.contains("h265_thread_task_wait_in_secs_bucket"));
    assert!(prom_text.contains("h265_mount_free_gb"));
    assert!(prom_text.contains("nodename=\"win-rtx3050ti\""));
}

#[test]
fn test_python_metrics_agent_remote_and_master_once_modes() {
    let manifest_dir = test_project_root();
    let script_path = manifest_dir.join("scripts").join("rusty_h265_metrics_agent.py");
    assert!(script_path.exists());

    let py = platform::python_command_name();
    let watch_dir = std::env::temp_dir();

    // 1. Test --role remote --once (direct OS syscall hardware telemetry JSON line)
    let out_remote = Command::new(py)
        .args([
            script_path.to_str().unwrap(),
            "--role",
            "remote",
            "--nodename",
            "test-remote-node",
            "--host-os",
            "MacOS",
            "--watch-dir",
            watch_dir.to_str().unwrap(),
            "--once",
        ])
        .output()
        .expect("Failed to execute rusty_h265_metrics_agent.py --role remote --once");
    assert!(
        out_remote.status.success(),
        "remote agent failed: {}",
        String::from_utf8_lossy(&out_remote.stderr)
    );
    let remote_stdout = String::from_utf8_lossy(&out_remote.stdout);
    let lines: Vec<&str> = remote_stdout.lines().filter(|l| l.starts_with('{')).collect();
    assert!(
        !lines.is_empty(),
        "Expected HostHardwareSample JSON line, got: {}",
        remote_stdout
    );
    let hw_sample: HostHardwareSample = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(hw_sample.nodename, "test-remote-node");
    assert!(hw_sample.memory_total_gb > 0.0);

    // 2. Test --role master --once reading an atomic snapshot file
    let tmp_snap = std::env::temp_dir().join(format!(
        "rusty_h265_test_snap_{}.json",
        std::process::id()
    ));
    global_metrics().set_local_identity("mac-master", "MacOS");
    global_metrics().update_remote_hardware_sample(hw_sample);
    global_metrics()
        .write_snapshot_atomic(&tmp_snap)
        .expect("Failed writing snapshot JSON");

    let out_master = Command::new(py)
        .args([
            script_path.to_str().unwrap(),
            "--role",
            "master",
            "--config",
            manifest_dir.join("config.yaml").to_str().unwrap(),
            "--telemetry-path",
            tmp_snap.to_str().unwrap(),
            "--once",
        ])
        .output()
        .expect("Failed to execute rusty_h265_metrics_agent.py --role master --once");
    let _ = fs::remove_file(&tmp_snap);
    assert!(
        out_master.status.success(),
        "master agent failed: {}",
        String::from_utf8_lossy(&out_master.stderr)
    );
    let master_prom = String::from_utf8_lossy(&out_master.stdout);
    assert!(master_prom.contains("# TYPE h265_host_cpu_utilization_ratio gauge"));
    assert!(master_prom.contains("h265_host_cpu_utilization_ratio"));
    assert!(master_prom.contains("nodename=\"test-remote-node\""));
    assert!(master_prom.contains("# TYPE h265_loki_shipped_entries_total counter"));
}

#[test]
fn test_ffmpeg_resilience_config_and_duration_av_sync_verification() {
    let cfg_path = test_project_root().join("config.yaml");
    let cfg = AppConfig::load(&cfg_path).expect("config.yaml must parse cleanly");

    // 1. Verify declarative target defaults + hardware-first decode + fallback filter + built-in encoder/audio defaults
    assert_eq!(cfg.TARGET_VIDEO_CODEC, "hevc");
    assert_eq!(cfg.TARGET_AUDIO_CODEC, "aac");
    assert_eq!(cfg.TARGET_CONTAINER, "mp4");
    assert!(cfg.FFMPEG_MACOS_ENCODER_ARGS.is_empty(), "Default config.yaml keeps FFMPEG_*_ENCODER_ARGS commented out");
    assert!(cfg.FFMPEG_MACOS_AUDIO_ARGS.is_empty(), "Default config.yaml keeps FFMPEG_*_AUDIO_ARGS commented out");
    assert!(cfg.FFMPEG_DECODE_ARGS.contains(&"+genpts+igndts".to_string()));
    assert!(cfg.FFMPEG_MACOS_HWACCEL_ARGS.contains(&"videotoolbox".to_string()));
    let vt_defaults = codecs::default_video_encoder_args(
        "hevc_videotoolbox",
        codecs::TargetVideoCodec::Hevc,
        codecs::TargetContainer::Mp4,
    );
    assert!(vt_defaults.contains(&"make_zero".to_string()));
    assert!(vt_defaults.contains(&"vfr".to_string()));
    assert!(cfg
        .FFMPEG_FALLBACK_VIDEO_FILTER_ARGS
        .iter()
        .any(|a| a.contains("bwdif=mode=0:parity=-1:deint=1")));
    let aac_at_defaults =
        codecs::default_audio_encoder_args("aac_at", codecs::TargetAudioCodec::Aac, false);
    assert!(aac_at_defaults.contains(&"aac_at".to_string()));
    assert!(aac_at_defaults
        .iter()
        .any(|a| a.contains("aresample=async=1:first_pts=0")));
    assert!(cfg.COPY_COMPATIBLE_AUDIO);

    // 2. Verify transparent config.yaml tolerance fields
    assert!((cfg.DURATION_TOLERANCE_SECS - 1.5).abs() < 0.001);
    assert!((cfg.AV_STREAM_MISMATCH_TOLERANCE_SECS - 1.5).abs() < 0.001);
    assert!((cfg.LEGACY_CONTAINER_PREROLL_TOLERANCE_SECS - 4.0).abs() < 0.001);
    assert!((cfg.LEGACY_CONTAINER_VFR_TOLERANCE_PCT - 3.5).abs() < 0.001);
    assert!((cfg.LEGACY_CONTAINER_MAX_TOLERANCE_SECS - 20.0).abs() < 0.001);

    let make_probe = |v: f64, a: Option<f64>, fmt: f64| VideoProbeInfo {
        video_codec: "hevc".to_string(),
        audio_codec: "aac".to_string(),
        duration_secs: v,
        video_stream_duration_secs: Some(v),
        audio_stream_duration_secs: a,
        format_duration_secs: Some(fmt),
        bitrate_kbps: 4000,
        file_size_bytes: 100_000_000,
        probe_error: None,
    };

    // Pattern A (+429.49s negative-PTS wraparound on .mp4) -> MUST FAIL
    let src_a = make_probe(254.35, Some(254.35), 254.35);
    let out_a = make_probe(683.85, Some(254.35), 683.85);
    let err_a = verify_output_durations(Path::new("POLISH MY BOOTS.mp4"), &src_a, &out_a, &cfg)
        .expect_err("Pattern A must be rejected");
    assert!(err_a.contains("A/V stream duration desync"));

    // Pattern B (2.0x field-coded interlaced duration doubling on .mp4) -> MUST FAIL
    let src_b = make_probe(1512.08, Some(1512.08), 1512.08);
    let out_b = make_probe(3024.15, Some(1512.08), 3024.15);
    let err_b = verify_output_durations(Path::new("10 pairs.mp4"), &src_b, &out_b, &cfg)
        .expect_err("Pattern B must be rejected");
    assert!(err_b.contains("A/V stream duration desync"));

    // Pattern C1/C2: .wmv ASF 2.00s preroll (1759.60s vs 1761.60s) -> MUST PASS
    let src_c_preroll = make_probe(1759.60, Some(1759.60), 1759.60);
    let out_c_preroll = make_probe(1761.60, Some(1761.60), 1761.60);
    assert!(verify_output_durations(
        Path::new("sf-blackhhshoestapping.wmv"),
        &src_c_preroll,
        &out_c_preroll,
        &cfg
    )
    .is_ok());

    // Pattern C: .wmv 2.8% VFR drift (544.41s vs 529.04s, within 3.5% <= 19.05s) -> MUST PASS
    let src_c_vfr = make_probe(544.41, Some(544.41), 544.41);
    let out_c_vfr = make_probe(529.04, Some(529.04), 529.04);
    assert!(verify_output_durations(
        Path::new("glam sexy heels.wmv"),
        &src_c_vfr,
        &out_c_vfr,
        &cfg
    )
    .is_ok());

    // Pattern D (Truncated outputs on .wmv and .mp4) -> MUST FAIL
    let src_d1 = make_probe(416.40, Some(416.40), 416.40);
    let out_d1 = make_probe(0.04, Some(0.04), 0.04);
    assert!(verify_output_durations(
        Path::new("crush money box.wmv"),
        &src_d1,
        &out_d1,
        &cfg
    )
    .is_err());

    let src_d2 = make_probe(472.84, Some(472.84), 472.84);
    let out_d2 = make_probe(127.57, Some(127.57), 127.57);
    assert!(verify_output_durations(
        Path::new("swallowsuffering2.wmv"),
        &src_d2,
        &out_d2,
        &cfg
    )
    .is_err());

    // Standard .mp4 exact match within 1.5s -> MUST PASS; off by 2.5s -> MUST FAIL
    let src_ok = make_probe(600.0, Some(600.0), 600.0);
    let out_ok = make_probe(601.1, Some(601.0), 601.1);
    assert!(verify_output_durations(Path::new("normal.mp4"), &src_ok, &out_ok, &cfg).is_ok());

    let out_bad_mp4 = make_probe(597.0, Some(597.0), 597.0);
    assert!(verify_output_durations(Path::new("normal.mp4"), &src_ok, &out_bad_mp4, &cfg).is_err());
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "rusty_h265_test_{}_{}_{}",
        label,
        std::process::id(),
        nanos
    ))
}

#[test]
fn unwritable_dir_registry_is_decoupled_from_failed_dirs_journal() {
    let base = unique_temp_dir("unwritable_decoupled");
    ensure_dir_created(&base).unwrap();
    let failed_log = base.join("broken_directories.journal");

    let logged_only_dir = base.join("logged_only_h265");
    let unwritable_dir = base.join("unwritable_h265");

    let err = anyhow::anyhow!("Single corrupt media file");
    record_failed_dir(
        &failed_log,
        &base.join("logged_only"),
        Some(&logged_only_dir),
        "Single file failure logged",
        &err,
    );
    assert!(
        !is_dir_marked_failed(&logged_only_dir),
        "Logging a directory to broken_directories.journal must NOT mark it unwritable"
    );

    mark_dir_unwritable(&unwritable_dir);
    assert!(
        is_dir_marked_failed(&unwritable_dir),
        "mark_dir_unwritable must mark fatal unwritable directories for skipping"
    );

    let _ = fs::remove_dir_all(&base);
}


#[test]
fn corrupt_file_in_subdir_does_not_abort_dir_and_cleans_up_converted_sources() {
    let base = unique_temp_dir("partial_subdir_cleanup");
    let sub_src = base.join("club_test");
    let sub_src_nested = sub_src.join("nested");
    let sub_tgt = base.join("club_test_h265");
    let sub_tgt_nested = sub_tgt.join("nested");
    ensure_dir_created(&sub_src_nested).unwrap();
    ensure_dir_created(&sub_tgt_nested).unwrap();

    let corrupt_src = sub_src.join("01_corrupt.mp4");
    let good1_src = sub_src.join("02_good.mp4");
    let good2_src = sub_src_nested.join("03_good_nested.mkv");
    fs::write(&corrupt_src, vec![b'x'; 2048]).unwrap();
    fs::write(&good1_src, vec![b'a'; 2048]).unwrap();
    fs::write(&good2_src, vec![b'b'; 2048]).unwrap();

    // Pre-create verified converted targets for the 2 healthy files (none for the corrupt file)
    let good1_tgt = sub_tgt.join("02_good_h265.mp4");
    let good2_tgt = sub_tgt_nested.join("03_good_nested_h265.mp4");
    fs::write(&good1_tgt, vec![b'1'; 1024]).unwrap();
    fs::write(&good2_tgt, vec![b'2'; 1024]).unwrap();

    let broken_dirs_log = base.join("broken_directories.journal");
    let mut cfg = AppConfig::default();
    cfg.FAILED_DIRS_PATH = broken_dirs_log.display().to_string();

    let trackers: Arc<Mutex<HashMap<PathBuf, SubdirTracker>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let next_id = AtomicU64::new(1);
    let queue = populate_mount_queue_for_dirs(
        "/tmp",
        std::slice::from_ref(&base),
        &cfg,
        &next_id,
        &trackers,
        false,
    );
    assert_eq!(queue.len(), 3);

    let ui: Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>> = Arc::new(Mutex::new(BTreeMap::new()));
    ui.lock()
        .unwrap()
        .insert("local:/tmp".to_string(), WorkerUiSnapshot::default());

    // 1. Fail the corrupt file FIRST (using Option A per-file helper + complete_queued_item)
    let corrupt_item = queue
        .iter()
        .find(|i| i.source_path == corrupt_src)
        .expect("corrupt item in queue");
    let err = anyhow::anyhow!("Corrupt media bitstream");
    record_file_failure_if_untracked_subdir(
        &broken_dirs_log,
        corrupt_item,
        &sub_src,
        Some(&sub_tgt),
        "Marking broken file in broken_files.journal",
        &err,
    );
    complete_queued_item(
        corrupt_item,
        false,
        false,
        true,
        0,
        &trackers,
        &ui,
        "local:/tmp",
        &cfg,
        false,
    );

    // Crucial: the directory must NOT be marked failed/unwritable, and Option A defers the journal entry
    assert!(
        !is_dir_marked_failed(&sub_tgt),
        "A single corrupt file must NOT mark target subdir unwritable"
    );
    assert!(
        !broken_dirs_log.exists(),
        "Option A must defer broken_directories.journal entry until Step 2.6 completion"
    );

    // 2. Process the remaining 2 healthy files normally
    for item in queue.iter().filter(|i| i.source_path != corrupt_src) {
        complete_queued_item(
            item,
            false,
            false,
            false,
            1024,
            &trackers,
            &ui,
            "local:/tmp",
            &cfg,
            false,
        );
    }

    // 3. Verify Step 2.6 partial cleanup:
    //    - Good source files deleted & empty nested dir pruned
    //    - Corrupt source file and parent source dir remain intact for manual inspection
    assert!(!good1_src.exists(), "Verified converted source file good1 must be deleted");
    assert!(!good2_src.exists(), "Verified converted nested source file good2 must be deleted");
    assert!(!sub_src_nested.exists(), "Empty nested subdir should be pruned");
    assert!(corrupt_src.exists(), "Corrupt source file must remain for inspection");
    assert!(sub_src.exists(), "Source subdir holding corrupt file must remain");
    assert!(good1_tgt.exists() && good2_tgt.exists(), "Converted target files must remain intact");

    // 4. Verify broken_directories.journal has 1 accurate summary with Number of Files Impacted = 1
    let log_text = fs::read_to_string(&broken_dirs_log)
        .expect("Step 2.6 must write summary entry to broken_directories.journal");
    assert!(
        log_text.contains("Number of Files Impacted   : 1"),
        "Number of Files Impacted should reflect only the 1 remaining broken file: {}",
        log_text
    );
    assert!(
        log_text.contains("1 file(s) failed conversion")
            && log_text.contains("2 converted source file(s) verified in target and deleted from source"),
        "Summary should report failed and cleaned counts: {}",
        log_text
    );

    let _ = fs::remove_dir_all(&base);
}


#[test]
fn partial_cleanup_preserves_colliding_sources_and_flushes_incomplete_on_shutdown() {
    let base = unique_temp_dir("partial_collision_shutdown");
    let sub_src = base.join("interrupted_dir");
    let sub_tgt = base.join("interrupted_dir_h265");
    ensure_dir_created(&sub_src).unwrap();
    ensure_dir_created(&sub_tgt).unwrap();

    // clip1.mp4 + clip1.wmv collide onto clip1_h265.mp4; good_unique.mp4 has a unique target;
    // corrupt.mp4 fails; unstarted.mp4 simulates a file not yet processed before Ctrl+C shutdown.
    let clip_mp4 = sub_src.join("clip1.mp4");
    let clip_wmv = sub_src.join("clip1.wmv");
    let good_unique = sub_src.join("good_unique.mp4");
    let corrupt = sub_src.join("corrupt.mp4");
    let unstarted = sub_src.join("unstarted.mp4");
    for f in [&clip_mp4, &clip_wmv, &good_unique, &corrupt, &unstarted] {
        fs::write(f, vec![b'v'; 1024]).unwrap();
    }
    fs::write(sub_tgt.join("clip1_h265.mp4"), vec![b't'; 512]).unwrap();
    fs::write(sub_tgt.join("good_unique_h265.mp4"), vec![b't'; 512]).unwrap();

    let broken_dirs_log = base.join("broken_directories.journal");
    let mut cfg = AppConfig::default();
    cfg.FAILED_DIRS_PATH = broken_dirs_log.display().to_string();

    let trackers: Arc<Mutex<HashMap<PathBuf, SubdirTracker>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let next_id = AtomicU64::new(1);
    let queue = populate_mount_queue_for_dirs(
        "/tmp",
        std::slice::from_ref(&base),
        &cfg,
        &next_id,
        &trackers,
        false,
    );
    let ui: Arc<Mutex<BTreeMap<String, WorkerUiSnapshot>>> = Arc::new(Mutex::new(BTreeMap::new()));
    ui.lock()
        .unwrap()
        .insert("local:/tmp".to_string(), WorkerUiSnapshot::default());

    // Process 4 of the 5 files (leaving unstarted.mp4 incomplete to simulate Ctrl+C shutdown)
    for item in &queue {
        if item.source_path == unstarted {
            continue;
        }
        let is_err = item.source_path == corrupt;
        complete_queued_item(
            item,
            false,
            false,
            is_err,
            512,
            &trackers,
            &ui,
            "local:/tmp",
            &cfg,
            false,
        );
    }

    assert!(
        !broken_dirs_log.exists(),
        "Incomplete subdir shouldn't have flushed before shutdown"
    );

    // Trigger graceful shutdown flush
    flush_incomplete_failed_subdirs(&trackers, &cfg);

    assert!(!good_unique.exists(), "Non-colliding verified file should be deleted on shutdown flush");
    assert!(clip_mp4.exists() && clip_wmv.exists(), "Colliding source files must NOT be deleted");
    assert!(corrupt.exists() && unstarted.exists(), "Corrupt and unstarted files must remain");

    let log_text = fs::read_to_string(&broken_dirs_log)
        .expect("Graceful shutdown flush must write incomplete failed subdir to broken_directories.journal");
    assert!(
        log_text.contains("1 file(s) failed conversion inside subdir")
            && log_text.contains("before shutdown"),
        "Shutdown flush entry should document interruption: {}",
        log_text
    );

    let _ = fs::remove_dir_all(&base);
}

// ============================================================================
// 6. Configurable Target Codecs, Containers & Pre-Flight Validation
// ============================================================================

#[test]
fn test_layer1_static_codec_container_compatibility_and_extension_check() {
    // Valid default (hevc + aac + mp4)
    let default_cfg = AppConfig::default();
    let v = codecs::validate_config_targets(&default_cfg).expect("Default HEVC/AAC/MP4 must pass");
    assert_eq!(v.video, codecs::TargetVideoCodec::Hevc);
    assert_eq!(v.audio, codecs::TargetAudioCodec::Aac);
    assert_eq!(v.container, codecs::TargetContainer::Mp4);

    // Valid declarative YAML for animated webp (omitting OUTPUT_EXTENSION auto-syncs .webp)
    let webp_yaml = r#"
TARGET_VIDEO_CODEC: "webp"
TARGET_AUDIO_CODEC: "none"
TARGET_CONTAINER: "webp"
"#;
    let webp_cfg = AppConfig::from_yaml_str(webp_yaml).expect("webp yaml parses");
    assert_eq!(webp_cfg.OUTPUT_EXTENSION, ".webp");
    let webp_target = codecs::validate_config_targets(&webp_cfg).expect("webp + none + webp passes");
    assert_eq!(webp_target.container.muxer_args(), vec!["-loop", "0"]);

    // Audio-less container (webp) with TARGET_AUDIO_CODEC != none MUST fail in Layer 1 with MDN link
    let bad_webp_yaml = r#"
TARGET_VIDEO_CODEC: "webp"
TARGET_AUDIO_CODEC: "aac"
TARGET_CONTAINER: "webp"
"#;
    let bad_webp_cfg = AppConfig::from_yaml_str(bad_webp_yaml).unwrap();
    let err = codecs::validate_config_targets(&bad_webp_cfg)
        .expect_err("webp + aac must fail fast in Layer 1")
        .to_string();
    assert!(err.contains("README.md"), "Expected README.md reference: {}", err);
    assert!(
        err.contains(codecs::MDN_VIDEO_CODECS_URL),
        "Expected MDN link: {}",
        err
    );

    // Invalid codec/container combo (hevc inside webm) MUST fail in Layer 1
    let bad_webm_yaml = r#"
TARGET_VIDEO_CODEC: "hevc"
TARGET_AUDIO_CODEC: "opus"
TARGET_CONTAINER: "webm"
"#;
    let bad_webm_cfg = AppConfig::from_yaml_str(bad_webm_yaml).unwrap();
    assert!(codecs::validate_config_targets(&bad_webm_cfg).is_err());

    // Mismatched OUTPUT_EXTENSION vs TARGET_CONTAINER MUST fail in Layer 1
    let ext_mismatch_yaml = r#"
TARGET_VIDEO_CODEC: "hevc"
TARGET_AUDIO_CODEC: "aac"
TARGET_CONTAINER: "mp4"
OUTPUT_EXTENSION: ".mkv"
"#;
    let ext_mismatch_cfg = AppConfig::from_yaml_str(ext_mismatch_yaml).unwrap();
    let ext_err = codecs::validate_config_targets(&ext_mismatch_cfg)
        .expect_err("OUTPUT_EXTENSION mismatch must fail in Layer 1")
        .to_string();
    assert!(
        ext_err.contains("OUTPUT_EXTENSION") && ext_err.contains("TARGET_CONTAINER"),
        "Unexpected error: {}",
        ext_err
    );
}

#[test]
fn test_layer1_custom_ffmpeg_args_conflict_enforcement() {
    // Matching custom override (hevc_videotoolbox with TARGET_VIDEO_CODEC=hevc) passes
    let ok_yaml = r#"
TARGET_VIDEO_CODEC: "hevc"
TARGET_AUDIO_CODEC: "aac"
TARGET_CONTAINER: "mp4"
FFMPEG_MACOS_ENCODER_ARGS:
  - -c:v
  - hevc_videotoolbox
  - -q:v
  - "55"
FFMPEG_MACOS_AUDIO_ARGS:
  - -c:a
  - aac_at
  - -b:a
  - 192k
"#;
    let ok_cfg = AppConfig::from_yaml_str(ok_yaml).unwrap();
    codecs::validate_config_targets(&ok_cfg).expect("Matching custom overrides must pass");

    // Conflicting video override (hevc_videotoolbox when TARGET_VIDEO_CODEC=webp) MUST throw hard error
    let conflict_v_yaml = r#"
TARGET_VIDEO_CODEC: "webp"
TARGET_AUDIO_CODEC: "none"
TARGET_CONTAINER: "webp"
FFMPEG_MACOS_ENCODER_ARGS:
  - -c:v
  - hevc_videotoolbox
"#;
    let conflict_v_cfg = AppConfig::from_yaml_str(conflict_v_yaml).unwrap();
    let err_v = codecs::validate_config_targets(&conflict_v_cfg)
        .expect_err("Conflicting FFMPEG_MACOS_ENCODER_ARGS must fail fast")
        .to_string();
    assert!(
        err_v.contains("FFMPEG_MACOS_ENCODER_ARGS") && err_v.contains("hevc_videotoolbox"),
        "Error must name offending key and codec: {}",
        err_v
    );

    // Conflicting audio override (aac when TARGET_AUDIO_CODEC=opus) MUST throw hard error
    let conflict_a_yaml = r#"
TARGET_VIDEO_CODEC: "vp9"
TARGET_AUDIO_CODEC: "opus"
TARGET_CONTAINER: "webm"
FFMPEG_AUDIO_ARGS:
  - -c:a
  - aac
  - -b:a
  - 192k
"#;
    let conflict_a_cfg = AppConfig::from_yaml_str(conflict_a_yaml).unwrap();
    let err_a = codecs::validate_config_targets(&conflict_a_cfg)
        .expect_err("Conflicting FFMPEG_AUDIO_ARGS must fail fast")
        .to_string();
    assert!(
        err_a.contains("FFMPEG_AUDIO_ARGS") && err_a.contains("aac"),
        "Error must name offending audio key: {}",
        err_a
    );
}

#[test]
fn test_discovery_first_encoder_parser_ranking_and_cuda_sanitization() {
    let sample_encoders = r#"
Encoders:
 V..... = Video
 ------
 V....D libaom-av1           libaom AV1 (codec av1)
 V....D libsvtav1            SVT-AV1(Scalable Video Technology for AV1) encoder (codec av1)
 V....D av1_nvenc            NVIDIA NVENC av1 encoder (codec av1)
 V....D libx265              libx265 H.265 / HEVC (codec hevc)
 V....D hevc_videotoolbox    VideoToolbox H.265 Encoder (codec hevc)
 A..X.. opus                 Opus (codec opus)
 A....D libopus              libopus Opus (codec opus)
 A....D aac                  AAC (Advanced Audio Coding)
 A....D aac_at               aac (AudioToolbox) (codec aac)
"#;
    let parsed = codecs::parse_ffmpeg_encoders_output(sample_encoders);
    assert!(parsed.iter().any(|e| e.name == "opus" && e.experimental));
    assert!(parsed.iter().any(|e| e.name == "libopus" && !e.experimental));

    let cfg = AppConfig::default();

    // On macOS, hevc_videotoolbox ranks ahead of libx265; aac_at ranks ahead of aac
    let hevc_target = codecs::ValidatedTargetConfig {
        video: codecs::TargetVideoCodec::Hevc,
        audio: codecs::TargetAudioCodec::Aac,
        container: codecs::TargetContainer::Mp4,
    };
    let mac_v = codecs::rank_discovered_video_candidates(
        sample_encoders,
        "macos",
        "",
        &hevc_target,
        &cfg,
    );
    assert_eq!(mac_v[0].0, "hevc_videotoolbox");
    assert_eq!(mac_v[1].0, "libx265");

    let mac_a = codecs::rank_discovered_audio_candidates(sample_encoders, "macos", &hevc_target, &cfg);
    assert_eq!(mac_a[0].0, "aac_at");
    assert_eq!(mac_a[1].0, "aac");

    // On Linux, av1_nvenc ranks first, then libsvtav1, then libaom-av1; experimental `opus` is skipped in favor of `libopus`
    let av1_target = codecs::ValidatedTargetConfig {
        video: codecs::TargetVideoCodec::Av1,
        audio: codecs::TargetAudioCodec::Opus,
        container: codecs::TargetContainer::Mkv,
    };
    let linux_v = codecs::rank_discovered_video_candidates(
        sample_encoders,
        "linux",
        "",
        &av1_target,
        &cfg,
    );
    let linux_v_names: Vec<&str> = linux_v.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(linux_v_names, vec!["av1_nvenc", "libsvtav1", "libaom-av1"]);

    let linux_a = codecs::rank_discovered_audio_candidates(sample_encoders, "linux", &av1_target, &cfg);
    let linux_a_names: Vec<&str> = linux_a.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(linux_a_names, vec!["libopus"]);

    // -hwaccel_output_format cuda is kept for *_nvenc and stripped for non-NVENC encoders
    let nv_hw = vec![
        "-hwaccel".to_string(),
        "cuda".to_string(),
        "-hwaccel_output_format".to_string(),
        "cuda".to_string(),
    ];
    assert_eq!(
        codecs::sanitize_hwaccel_args_for_encoder(&nv_hw, "hevc_nvenc"),
        nv_hw
    );
    assert_eq!(
        codecs::sanitize_hwaccel_args_for_encoder(&nv_hw, "libvpx-vp9"),
        vec!["-hwaccel".to_string(), "cuda".to_string()]
    );
}

#[test]
fn test_custom_output_suffix_allows_reconverting_h265_libraries() {
    // Legacy _h265 suffix still matches bare "h265"
    assert!(is_h265_named(Path::new("/tmp/movie_h265.mp4"), "_h265"));
    assert!(is_subdir_h265_named(Path::new("/tmp/VacationH265"), "_h265"));

    // Custom _webp suffix must NOT skip existing _h265 folders/files!
    assert!(!is_h265_named(Path::new("/tmp/movie_h265.mp4"), "_webp"));
    assert!(!is_subdir_h265_named(Path::new("/tmp/Vacation_h265"), "_webp"));
    assert!(is_h265_named(Path::new("/tmp/movie_webp.webp"), "_webp"));
    assert!(is_subdir_h265_named(Path::new("/tmp/Vacation_webp"), "_webp"));

    let mut cfg = AppConfig::default();
    cfg.OUTPUT_SUFFIX = "_webp".to_string();
    cfg.OUTPUT_EXTENSION = ".webp".to_string();
    let built = build_h265_filename(Path::new("/tmp/clip_h265.mp4"), false, &cfg);
    assert_eq!(built, "clip_h265_webp.webp");
}

#[test]
fn test_layer2_live_preflight_5s_256x144_hevc_vp9_and_webp() {
    if codecs::query_local_ffmpeg_encoders().is_err() {
        eprintln!("Skipping live Layer 2 FFmpeg pre-flight test: ffmpeg not found in PATH");
        return;
    }

    let silent_spinner = codecs::DiscoverySpinner::silent();

    // 1. HEVC + AAC + MP4
    let hevc_cfg = AppConfig::default();
    let hevc_target = codecs::validate_config_targets(&hevc_cfg).unwrap();
    let hevc_prof =
        codecs::discover_and_verify_local_profile(&hevc_cfg, &hevc_target, &silent_spinner)
            .expect("Local HEVC/AAC/MP4 5s 256x144 pre-flight must succeed");
    assert!(hevc_target
        .video
        .matches_ffmpeg_encoder(&hevc_prof.video_encoder_name));
    assert_eq!(hevc_prof.muxer_args, vec!["-movflags", "+faststart"]);

    // 2. VP9 + Opus + WebM
    let vp9_cfg = AppConfig::from_yaml_str(
        "TARGET_VIDEO_CODEC: \"vp9\"\nTARGET_AUDIO_CODEC: \"opus\"\nTARGET_CONTAINER: \"webm\"\n",
    )
    .unwrap();
    let vp9_target = codecs::validate_config_targets(&vp9_cfg).unwrap();
    let vp9_prof =
        codecs::discover_and_verify_local_profile(&vp9_cfg, &vp9_target, &silent_spinner)
            .expect("Local VP9/Opus/WebM 5s 256x144 pre-flight must succeed");
    assert_eq!(vp9_prof.video_encoder_name, "libvpx-vp9");
    assert!(vp9_prof.muxer_args.is_empty());

    // 3. Animated WebP + none + WebP
    let webp_cfg = AppConfig::from_yaml_str(
        "TARGET_VIDEO_CODEC: \"webp\"\nTARGET_AUDIO_CODEC: \"none\"\nTARGET_CONTAINER: \"webp\"\n",
    )
    .unwrap();
    let webp_target = codecs::validate_config_targets(&webp_cfg).unwrap();
    let webp_prof =
        codecs::discover_and_verify_local_profile(&webp_cfg, &webp_target, &silent_spinner)
            .expect("Local WebP/none/WebP 5s 256x144 pre-flight must succeed");
    assert!(webp_target
        .video
        .matches_ffmpeg_encoder(&webp_prof.video_encoder_name));
    assert_eq!(webp_prof.audio_args, vec!["-an"]);
    assert_eq!(webp_prof.muxer_args, vec!["-loop", "0"]);
}

#[test]
fn test_headerless_wmv_zero_duration_fallback_and_corrupt_rejection() {
    if codecs::query_local_ffmpeg_encoders().is_err() {
        eprintln!("Skipping headerless WMV probe test: ffmpeg not found in PATH");
        return;
    }

    let tmp_dir = std::env::temp_dir().join(format!(
        "rusty_h265_headerless_wmv_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).unwrap();

    // 1. Create a genuine headerless WMV/ASF file via piped stdout (no ASF Play Duration in header)
    let wmv_path = tmp_dir.join("headerless_broadcast.wmv");
    let gen_out = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=256x144:rate=15:duration=5",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=44100:cl=stereo:d=5",
            "-c:v",
            "wmv1",
            "-c:a",
            "wmav2",
            "-f",
            "asf",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("ffmpeg must run to synthesize headerless WMV");
    assert!(
        gen_out.status.success() && !gen_out.stdout.is_empty(),
        "Synthesizing headerless WMV failed: {}",
        String::from_utf8_lossy(&gen_out.stderr)
    );
    fs::write(&wmv_path, &gen_out.stdout).unwrap();

    // Confirm raw ffprobe header truly lacks duration, then verify get_video_probe_info recovers it!
    let probe = get_video_probe_info(&wmv_path);
    assert_eq!(probe.video_codec, "wmv1");
    assert!(
        probe.probe_error.is_none(),
        "Headerless playable WMV must not be marked broken: {:?}",
        probe.probe_error
    );
    assert!(
        probe.duration_secs >= 4.5 && probe.duration_secs <= 5.5,
        "Demux-copy fallback must recover ~5.0s duration, got {:.3}s",
        probe.duration_secs
    );
    assert!(probe.bitrate_kbps > 0, "Bitrate must be derived from recovered duration");
    verify_first_frame_decodable(&wmv_path).expect("Headerless WMV must pass 1-frame decode check");

    // 2. Corrupt garbage WMV must STILL fail probing cleanly
    let bad_wmv = tmp_dir.join("corrupt.wmv");
    fs::write(&bad_wmv, b"NOT_A_VALID_ASF_WMV_FILE_1234567890").unwrap();
    let bad_probe = get_video_probe_info(&bad_wmv);
    assert_eq!(bad_probe.video_codec, "unknown");
    assert!(bad_probe.probe_error.is_some());

    // 3. Step 2 fallback: when src_probe.duration_secs == 0.0 (unindexed timestamps),
    // verify_output_durations accepts valid synced output, but still rejects 0s or A/V-desynced output.
    let cfg = AppConfig::default();
    let src_zero = VideoProbeInfo {
        video_codec: "wmv2".to_string(),
        audio_codec: "wmav2".to_string(),
        duration_secs: 0.0,
        video_stream_duration_secs: None,
        audio_stream_duration_secs: None,
        format_duration_secs: None,
        bitrate_kbps: 0,
        file_size_bytes: 25_000_000,
        probe_error: None,
    };
    let good_out = VideoProbeInfo {
        video_codec: "hevc".to_string(),
        audio_codec: "aac".to_string(),
        duration_secs: 120.5,
        video_stream_duration_secs: Some(120.5),
        audio_stream_duration_secs: Some(120.4),
        format_duration_secs: Some(120.5),
        bitrate_kbps: 1500,
        file_size_bytes: 12_000_000,
        probe_error: None,
    };
    assert!(verify_output_durations(&wmv_path, &src_zero, &good_out, &cfg).is_ok());

    let desync_out = VideoProbeInfo {
        audio_stream_duration_secs: Some(110.0),
        ..good_out.clone()
    };
    assert!(verify_output_durations(&wmv_path, &src_zero, &desync_out, &cfg).is_err());

    let zero_out = VideoProbeInfo {
        duration_secs: 0.0,
        video_stream_duration_secs: None,
        ..good_out
    };
    assert!(verify_output_durations(&wmv_path, &src_zero, &zero_out, &cfg).is_err());

    let _ = fs::remove_dir_all(&tmp_dir);
}

#[test]
fn test_asymmetric_wmv_lazy_av_demux_verification() {
    if codecs::query_local_ffmpeg_encoders().is_err() {
        eprintln!("Skipping asymmetric WMV A/V demux test: ffmpeg not found in PATH");
        return;
    }

    let tmp_dir = std::env::temp_dir().join(format!(
        "rusty_h265_asym_wmv_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).unwrap();

    // Synthesize a WMV where true video is 5s and true audio is 10s
    // (ASF headers duplicate 10s Play_Duration onto BOTH streams in ffprobe!)
    let asym_wmv = tmp_dir.join("asymmetric_tracks.wmv");
    let status = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=256x144:rate=15:duration=5",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=44100:cl=stereo:d=10",
            "-c:v",
            "wmv1",
            "-c:a",
            "wmav2",
            "-y",
        ])
        .arg(&asym_wmv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("ffmpeg must synthesize asymmetric WMV");
    assert!(status.success());

    let src_probe = get_video_probe_info(&asym_wmv);
    let cfg = AppConfig::default();

    // Faithful MP4 output preserves true 5s video & 10s audio -> MUST PASS via lazy source demux check!
    let faithful_out = VideoProbeInfo {
        video_codec: "hevc".to_string(),
        audio_codec: "aac".to_string(),
        duration_secs: 5.02,
        video_stream_duration_secs: Some(5.02),
        audio_stream_duration_secs: Some(10.01),
        format_duration_secs: Some(10.01),
        bitrate_kbps: 800,
        file_size_bytes: 500_000,
        probe_error: None,
    };
    verify_output_durations(&asym_wmv, &src_probe, &faithful_out, &cfg)
        .expect("Faithful transcode of asymmetric WMV must pass via lazy demux span verification");

    // Genuinely drifted/truncated output (video truncated to 0.5s vs 5.0s true source) -> MUST FAIL!
    let broken_out = VideoProbeInfo {
        duration_secs: 0.50,
        video_stream_duration_secs: Some(0.50),
        audio_stream_duration_secs: Some(10.01),
        format_duration_secs: Some(10.01),
        ..faithful_out
    };
    let err = verify_output_durations(&asym_wmv, &src_probe, &broken_out, &cfg)
        .expect_err("Truncated/desynced output must still be rejected");
    assert!(
        err.contains("true_source_demux"),
        "Rejection should report demuxed true source spans: {}",
        err
    );

    let _ = fs::remove_dir_all(&tmp_dir);
}

#[test]
fn test_wrapped_32bit_mp4_header_lazy_demux_recovery() {
    if codecs::query_local_ffmpeg_encoders().is_err() {
        eprintln!("Skipping wrapped MP4 header test: ffmpeg not found in PATH");
        return;
    }

    let tmp_dir = std::env::temp_dir().join(format!(
        "rusty_h265_wrapped_mp4_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).unwrap();

    // Synthesize a real 5.0s MP4 file
    let mp4_path = tmp_dir.join("wrapped_header_source.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=256x144:rate=15:duration=5",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=48000:cl=stereo:d=5",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-y",
        ])
        .arg(&mp4_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("ffmpeg must synthesize 5s MP4");
    assert!(status.success());

    let cfg = AppConfig::default();

    // Simulate a wrapped 32-bit 10MHz MP4 header where ffprobe under-reports source as 0.50s
    // while actual packet stream duration in mp4_path is 5.00s.
    let wrapped_src_probe = VideoProbeInfo {
        video_codec: "h264".to_string(),
        audio_codec: "aac".to_string(),
        duration_secs: 0.50,
        video_stream_duration_secs: Some(0.50),
        audio_stream_duration_secs: Some(0.49),
        format_duration_secs: Some(0.50),
        bitrate_kbps: 2000,
        file_size_bytes: 200_000,
        probe_error: None,
    };

    // Output has true synced duration 5.00s (both v & a match!) -> MUST PASS via lazy demux check!
    let true_out_probe = VideoProbeInfo {
        video_codec: "hevc".to_string(),
        audio_codec: "aac".to_string(),
        duration_secs: 5.00,
        video_stream_duration_secs: Some(5.00),
        audio_stream_duration_secs: Some(4.99),
        format_duration_secs: Some(5.00),
        bitrate_kbps: 800,
        file_size_bytes: 100_000,
        probe_error: None,
    };
    verify_output_durations(&mp4_path, &wrapped_src_probe, &true_out_probe, &cfg)
        .expect("Lazy demux check must recover true 5.0s source duration and accept synced output");

    // Genuinely truncated output (2.50s vs wrapped 0.50s header AND true 5.00s demux) -> MUST STILL FAIL!
    let truncated_out = VideoProbeInfo {
        duration_secs: 2.50,
        video_stream_duration_secs: Some(2.50),
        audio_stream_duration_secs: Some(2.50),
        format_duration_secs: Some(2.50),
        ..true_out_probe
    };
    let err = verify_output_durations(&mp4_path, &wrapped_src_probe, &truncated_out, &cfg)
        .expect_err("Genuinely truncated output must still fail even after lazy demux check");
    assert!(
        err.contains("true_source_demux_v="),
        "Error should include demuxed source duration: {}",
        err
    );

    let _ = fs::remove_dir_all(&tmp_dir);
}







