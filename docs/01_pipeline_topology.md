# Diagram 1: Swimlane & Pipeline Topology

This diagram illustrates the spatial architecture of [**Rusty H.265 Converter**](../src/main.rs), showing how the orchestrator groups input directories by physical mount point, feeds raw files through the per-mount 3-thread local pipeline (`Slot 0` Stager, `Slot 1` Converter, `Slot 2` Flusher) and 3-thread remote SSH managers (`upload_handle`, `encode_handle`, `copyback_handle`), and exposes a detached deep-copy snapshot to the interactive TUI Queue Viewer (`'v'`).

* **Parent Document**: [SOFTWARE_ARCHITECTURE_AND_DESIGN.md](SOFTWARE_ARCHITECTURE_AND_DESIGN.md)
* **Declarative D2 Version**: [01_pipeline_topology.d2](01_pipeline_topology.d2) (renderable with [D2](https://d2lang.com/tour/intro/))
* **Mermaid Syntax Reference**: [Mermaid Flowcharts](https://mermaid.js.org/syntax/flowchart.html)

```mermaid
flowchart TB
    subgraph CLI_Orch["1. Orchestrator & Startup Initialization (run_app)"]
        CLI["CLI Args & Config Loader<br/>(config.yaml + logging.conf)"]
        RD["ProvisionedRamDisk<br/>(/Volumes/RustyH265RAMDisk)"]
        MG["Mount Point Grouping<br/>(get_mount_info -> DriveType::Hdd/Ssd/Nvme)"]
        CLI --> RD --> MG
    end

    subgraph Journals["2. Persistent Journals & Logs"]
        CJ[("completed_files.journal<br/>(Tier-1 Resume Cache)")]
        BJ[("broken_files.journal<br/>(Permanent Corrupt Skip)")]
        TJ[("distributed_tasks_journal.json<br/>(Remote Crash Recovery)")]
        FD[("failed_dirs.txt<br/>(Directory Error Log)")]
    end

    subgraph MountBox["3. Per-Mount Queue State (MountQueueState per Physical Drive)"]
        RQ[["raw_queue: VecDeque&lt;QueuedMediaItem&gt;<br/>(Ordered largest-subdir-first)"]]
        VQ[["queue: VecDeque&lt;QueuedMediaItem&gt;<br/>(Bounded Ready Buffer: 30 HDD / 50 SSD / 60 NVMe)"]]
        IF[["in_flight_items: BTreeMap&lt;PathBuf, InFlightMediaItem&gt;<br/>(Tracks Active Local &amp; Remote Stage Status)"]]
        IOL{"io_lock: Mutex&lt;()&gt;<br/>(Serializes HDD Sequential I/O)"}
    end

    MG --> RQ

    subgraph LocalPool["4. Local Mount 3-Thread Pipeline (run_local_mount_pipeline)"]
        S0["Slot 0: Stager Thread (run_local_stager_thread)<br/>• Rotating Inspection (Case A Resume / B1 Fast-Move / B2 Probe + 1f-Decode)<br/>• Copies HDD -&gt; RAMDisk via io_lock<br/>• Configurable STAGER_WAIT_MSEC (150ms)"]
        SQ[["staged_queue<br/>(Max 4 Staged Files in RAMDisk)"]]
        S1["Slot 1: Converter Thread (run_local_converter_thread)<br/>• Local FFmpeg Encode in RAMDisk<br/>  (hevc_videotoolbox / hevc_nvenc / libx265)<br/>• Auto SW-Decode Fallback"]
        FQ[["flush_queue"]]
        S2["Slot 2: Flusher Thread (run_local_flusher_thread)<br/>• Flushes RAMDisk -&gt; HDD via io_lock<br/>• Evicts RAMDisk temp files"]
        S0 --> SQ --> S1 --> FQ --> S2
    end

    Journals <--> S0
    RQ -->|"Inspect &amp; Probe"| S0
    S0 -->|"Verified Ready"| VQ
    VQ -->|"Pop for RAMDisk Staging"| S0

    subgraph RemotePool["5. Remote Machine Manager (run_remote_machine_manager per SSH Node)"]
        Disp["Main Dispatcher Loop<br/>(Steals from Unoccupied Mounts first<br/>+ Enforces MAX_SIZE_IN_GB Quota)"]
        UQ[["upload_queue"]]
        T1["Child Thread 1: upload_handle<br/>(Slot 0: PTY-Streamed scp-up)"]
        EQ[["encode_queue"]]
        T2["Child Thread 2: encode_handle<br/>(Slot 1: SSH + Remote FFmpeg)"]
        CBQ[["copyback_queue<br/>(Success or encode_failed Handoff)"]]
        T3["Child Thread 3: copyback_handle<br/>(Slot 2: PTY scp-down OR<br/>Failed-Encode SSH Eviction)"]
        Disp --> UQ --> T1 --> EQ --> T2 --> CBQ --> T3
    end

    VQ -->|"Work Steal (Remote)"| Disp
    IOL -.->|"Acquires Lock"| S0
    IOL -.->|"Acquires Lock"| S2

    subgraph TuiInspector["6. Interactive TUI &amp; Queue Inspector (draw_tui_frame)"]
        TuiModes["Dashboard &lt;-&gt; QueueSelector ('v') &lt;-&gt; QueuePager<br/>• Detached Deep-Copy Snapshot (snapshot_mount_queue_to_flat_list)<br/>• Sort Cmds: :s1a/:s1d (name), :s2a/:s2d (size), :s3a/:s3d (status), :r (reset)"]
    end

    MountBox -.->|"Detached Clone"| TuiModes

    subgraph Observability["8. Distributed Observability (Prometheus + Grafana Loki)"]
        Reg["In-Process MetricsRegistry &amp; ThreadWaitTracker<br/>(/tmp/rusty_h265_telemetry.json + structured logfmt)"]
        RemPy["Remote rusty_h265_metrics_agent.py (--role remote)<br/>(SCP-deployed at Step 6; ctypes CPU/RAM/Disk/NVML/IOKit)"]
        MasPy["Local rusty_h265_metrics_agent.py (--role master)<br/>• Serves GET :9184/metrics &amp; pushes PROMETHEUS_ENDPOINT<br/>• Rotation-aware tailer -&gt; POST LOKI_ENDPOINT (/loki/api/v1/push)"]
        RemPy -->|"SSH JSON Pipe"| Reg --> MasPy
    end

    LocalPool -.->|"Wait &amp; Stage Telemetry"| Reg
    RemotePool -.->|"Wait &amp; Stage Telemetry"| Reg

    subgraph Completion["7. Coordinated Subdir Completion (complete_queued_item)"]
        ST["SubdirTracker<br/>(Tracks total, completed, moved, failed,<br/>and original source_files)"]
        Rec["Step 2.6 Verification &amp; Self-Healing<br/>• If target == source: delete source_subdir<br/>• If target &gt; source: ffprobe targets, purge bad,<br/>  verify all source files accounted for,<br/>  log extra files [WARN] &amp; delete source_subdir [WARN]"]
        ST --> Rec
    end

    S2 --> ST
    T3 --> ST
    Rec --> CJ
    S0 -.->|"Corrupt Media"| BJ
    T3 -.->|"encode_failed Eviction"| BJ
    Rec -.->|"Missing Source / Error"| FD
```
