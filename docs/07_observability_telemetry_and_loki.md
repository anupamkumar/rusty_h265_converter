# Diagram 7: Observability Architecture — Prometheus Metrics, Per-Thread Wait Tracking & Grafana Loki Log Shipping

This document details the end-to-end observability architecture of [**Rusty H.265 Converter**](../src/main.rs), covering:
1. **In-Process Lock-Free Telemetry & Per-Thread Wait Tracking (`ThreadWaitTracker` & `MetricsRegistry`)**
2. **Structured `logfmt` Logging Aligned 1-to-1 with Prometheus Metric Labels**
3. **Distributed Python Observability Agent (`scripts/rusty_h265_metrics_agent.py`)** running locally (`--role master`) and deployed automatically via `scp` to remote SSH worker nodes (`--role remote`)
4. **Sequence & State-Machine Diagrams** for logging and metrics collection

* **Parent Document**: [SOFTWARE_ARCHITECTURE_AND_DESIGN.md](SOFTWARE_ARCHITECTURE_AND_DESIGN.md)

---

## 1. Distributed Observability Topology (`--role master` & `--role remote`)

```mermaid
flowchart TB
    subgraph LocalHost["Local Orchestrator Host (nodename: config.LOCAL.NODENAME)"]
        direction TB
        Orch["1. rusty_h265_converter (Rust Core Engine)<br/>• Assigns global monotonic task_id (1..N)<br/>• Runs Local Worker-Groups (verifier, stager, converter, flusher)<br/>• Runs Remote Managers (dispatcher, scp_upload, remote_encode, scp_copyback)<br/>• Tracks per-thread wait times &amp; USE/RED metrics + writes structured logfmt logs"]
        Snap[("Atomic Telemetry Snapshot<br/>/tmp/rusty_h265_telemetry.json<br/>(Flushed every 500ms)")]
        LocalLog[("Rotating Structured Log<br/>rusty_h265_converter.log")]
        LocalAgent["2. Local rusty_h265_metrics_agent.py (--role master)<br/>• Reads /tmp/rusty_h265_telemetry.json<br/>• Samples Local CPU, RAM, Disk, GPU via OS Syscalls (ctypes/Mach/procfs/IOKit)<br/>• Tails rusty_h265_converter.log (rotation-aware)<br/>• Aggregates remote agent telemetry streams"]

        Orch -->|"Atomic tmp+rename"| Snap --> LocalAgent
        Orch -->|"Structured logfmt"| LocalLog --> LocalAgent
        Orch -.->|"Auto-spawns on startup"| LocalAgent
    end

    subgraph RemoteHost["Remote Worker Host (e.g. win-rtx3050ti / mac-mini-m2)"]
        direction TB
        RemDir["Remote Temp Dir (/tmp/rusty_h265_remote_PID or C:/Temp/...)"]
        RemFFmpeg["Remote FFmpeg Process (hevc_nvenc / hevc_videotoolbox)"]
        RemAgent["3. Deployed Remote rusty_h265_metrics_agent.py (--role remote)<br/>• Copied via SCP at Step 6 startup into Remote Temp Dir<br/>• Samples Remote CPU, RAM, Temp Disk Free GB via ctypes Syscalls<br/>• Samples Remote GPU (NVML ctypes / nvidia-smi / IOKit)<br/>• Streams JSON metrics &amp; hardware logs over SSH pipe"]
        RemDir --> RemFFmpeg
        RemDir --> RemAgent
    end

    Orch ==>|"Step 6: SCP deploy metrics_agent.py<br/>+ Spawn persistent SSH telemetry stream"| RemAgent
    RemAgent ==>|"JSON Lines (5s interval):<br/>Remote CPU, RAM, Disk, GPU/NVENC/NVDEC"| LocalAgent

    subgraph ObservabilityStack["4. Prometheus &amp; Grafana Loki Stack"]
        Prom["Prometheus<br/>• Scrape: GET http://&lt;local&gt;:9184/metrics<br/>• Or Push: POST/PUT PROMETHEUS_ENDPOINT"]
        Loki["Grafana Loki<br/>• POST LOKI_ENDPOINT (/loki/api/v1/push)"]
        Grafana["Grafana Dashboards<br/>• Unified nodename, host_os, worker_group, worker_thread labels"]
    end

    LocalAgent --> Prom --> Grafana
    LocalAgent --> Loki --> Grafana
```

---

## 2. Sequence Diagram: End-to-End Logging & Metrics Collection

```mermaid
sequenceDiagram
    autonumber
    participant Orch as Orchestrator &<br/>Worker Threads (Rust)
    participant Reg as In-Process Lock-Free<br/>MetricsRegistry & Log
    participant RemAg as Remote Python Agent<br/>(--role remote via SSH)
    participant MasAg as Local Master Agent<br/>(--role master)
    participant Prom as Prometheus<br/>(:9184/metrics or Push)
    participant Loki as Grafana Loki<br/>(/loki/api/v1/push)

    Note over Orch,MasAg: Phase 1: Startup & Remote Agent Deployment
    Orch->>Reg: Init MetricsRegistry + RotatingLogger (log_level=INFO)
    Orch->>MasAg: Auto-spawn `python3 rusty_h265_metrics_agent.py --role master --parent-pid PID`
    Orch->>RemAg: Step 6: SCP `rusty_h265_metrics_agent.py` to remote_root_dir & spawn `--role remote`
    RemAg->>RemAg: Execute OS/GPU syscalls (ctypes: statvfs/GetDiskFreeSpaceExW, NVML/IOKit)
    RemAg-->>Orch: Emit JSON `event="node_inventory"` (CPU, RAM, GPU model/driver, Temp free GB)
    Orch->>Reg: Log `event="node_inventory"` + update remote hardware gauges

    Note over Orch,Loki: Phase 2: Worker Thread Wait-Time & Stage Execution Telemetry
    loop Every Worker Thread (verifier, stager, converter, flusher, dispatcher, scp_upload, remote_encode, scp_copyback)
        Orch->>Reg: ThreadWaitTracker::enter_wait(wait_reason) -> set state=Waiting, update current_wait_in_secs
        Orch->>Reg: Accumulate wait_in_secs_total{wait_reason} & wait_cycles_total{wait_reason}
        Orch->>Reg: Task acquired -> observe task_wait_in_secs_bucket{wait_reason} & reset current_wait_in_secs=0
        Orch->>Reg: Emit structured logfmt `event="<stage>_start"` (task_id, source_file, thread_wait_in_secs, ...)
        Orch->>Orch: Execute stage (4MB disk chunks / FFmpeg pipe:1 / PTY scp / SSH)
        Orch->>Reg: Update chunk latency histograms, speed_x, bytes, and Mach/POSIX thread CPU time
        Orch->>Reg: Observe stage_duration_in_secs_bucket & increment tasks_completed_total
        Orch->>Reg: Emit structured logfmt `event="<stage>_complete"` / `"<stage>_end"` (durations, throughput_mbps, saved_mb)
    end

    Note over Orch,Loki: Phase 3: Periodic Snapshot Flush, Hardware Sampling & Prometheus/Loki Shipping
    par Every 500ms: Rust Telemetry Snapshot Flush
        Reg->>MasAg: Atomic write `/tmp/rusty_h265_telemetry.json.tmp` -> rename `/tmp/rusty_h265_telemetry.json`
    and Every 5s: Remote Host Syscall Sampling
        RemAg->>RemAg: Sample remote CPU %, RAM GB, Temp Disk Free GB, GPU/NVENC/NVDEC %, VRAM, Temp, Watts
        RemAg-->>Orch: Stream JSON hardware sample over persistent SSH stdout pipe
        Orch->>Reg: Update remote host gauges in MetricsRegistry (+ log WARN if GPU temp > 85C or Disk < 1GB)
    and Continuous: Local Master Agent Export & Log Tailing
        MasAg->>MasAg: Sample local CPU, LoadAvg, VM Memory, Process RSS/CPU, Disk statvfs, IOKit/NVML GPU
        MasAg->>MasAg: Merge `/tmp/rusty_h265_telemetry.json` + local syscalls -> Render Prometheus v0.0.4 text
        MasAg->>Prom: Serve `GET /metrics` & optional periodic `POST/PUT` to `PROMETHEUS_ENDPOINT`
        MasAg->>MasAg: Tail `rusty_h265_converter.log` (detecting rotation via inode/size) & parse logfmt labels
        MasAg->>Loki: Batch & push JSON streams (`nodename`, `host_os`, `worker_group`, `worker_thread`, `level`)
    end

    Note over Orch,Loki: Phase 4: Graceful Shutdown & Final Flush
    Orch->>Reg: Final snapshot write (`converter_finished = true`) & final log flush
    MasAg->>MasAg: Detect parent PID exit -> final telemetry read & remaining log tail drain
    MasAg->>Prom: Final metrics push to `PROMETHEUS_ENDPOINT` (if configured)
    MasAg->>Loki: Final log batch flush to `LOKI_ENDPOINT`
```

---

## 3. State-Machine Diagram 1: Worker Thread Wait-Time & Lifecycle Telemetry (`ThreadWaitTracker`)

```mermaid
stateDiagram-v2
    [*] --> PollingForWork: Thread spawned (register nodename, host_os, worker_group, worker_thread)

    state "Waiting States (state = 0 Idle or 2 Blocked)" as Waiting {
        PollingForWork --> WaitUpstreamQueue: Queue empty (raw_queue / queue / to_convert / to_flush / upload_q / encode_q / copyback_q)
        PollingForWork --> WaitLookaheadFull: Verifier ms.queue.len() >= lookahead_limit (30/50/60)
        PollingForWork --> WaitRamDiskQuota: Stager WaitForSpaceOrResize (staged_input_bytes + file > max_input_stage_size)
        PollingForWork --> WaitRemoteQuota: Remote Dispatcher (remaining_space < item.file_size_bytes)

        WaitUpstreamQueue --> PollingForWork: Sleep tick elapsed -> inc wait_in_secs_total{wait_reason="upstream_queue_empty"}
        WaitLookaheadFull --> PollingForWork: Sleep 120ms -> inc wait_in_secs_total{wait_reason="lookahead_buffer_full"}
        WaitRamDiskQuota --> PollingForWork: Sleep STAGER_WAIT_MSEC -> inc wait_in_secs_total{wait_reason="ramdisk_quota_full"}
        WaitRemoteQuota --> PollingForWork: Sleep 250ms -> inc wait_in_secs_total{wait_reason="remote_quota_full"}
    }

    PollingForWork --> TaskPopped: Dequeued task_id (or selected StageNow / ProcessUnstaged)

    state "Pre-Execution I/O Synchronization" as PreIO {
        TaskPopped --> WaitFlusherPriority: Stager checks !wg.to_flush.is_empty()
        WaitFlusherPriority --> WaitFlusherPriority: Sleep 25ms -> inc wait_in_secs_total{wait_reason="flusher_priority_yield"}
        WaitFlusherPriority --> AcquireIoLock: wg.to_flush is empty
        TaskPopped --> AcquireIoLock: Thread needs disk I/O (verifier move / stager read / flusher write)
        AcquireIoLock --> IoLockAcquired: Lock contended -> record wait_in_secs_total{wait_reason="io_lock"} & disk_io_lock_wait_in_secs
    }

    TaskPopped --> StageExecution: No local disk lock required (converter / scp_upload / remote_encode / scp_copyback)
    IoLockAcquired --> StageExecution: Lock acquired (inc disk_io_lock_hold_in_secs_total while held)

    state "Active Stage Execution (state = 1 Active)" as StageExecution {
        [*] --> EmitStartLog: Reset current_wait_in_secs = 0\nObserve h265_thread_task_wait_in_secs_bucket\nLog event="<stage>_start" (task_id, thread_wait_in_secs breakdown)
        EmitStartLog --> RunningStage: Inc h265_thread_tasks_started_total
        RunningStage --> RunningStage: Stream live telemetry (4MB chunk latencies, SCP MB/s, FFmpeg speed_x)\nAccumulate busy_in_secs_total & thread_cpu_in_secs_total
        RunningStage --> StageSucceeded: Exit 0 & Output Verified
        RunningStage --> StageFailedOrRetried: Error / Non-zero Exit
    }

    StageSucceeded --> EmitCompleteLog: Observe h265_stage_duration_in_secs_bucket\nInc tasks_completed_total{outcome="success"}\nLog event="<stage>_complete" / "<stage>_end"
    StageFailedOrRetried --> EmitErrorLog: Inc h265_thread_errors_total{error_type}\nLog WARN/ERROR event="<stage>_failed" or "ssh_transient_retry"

    EmitCompleteLog --> ResetWaitTracker: Clear per-task wait accumulators
    EmitErrorLog --> ResetWaitTracker: Clear per-task wait accumulators
    ResetWaitTracker --> PollingForWork: Next iteration

    PollingForWork --> ThreadFinished: Shutdown requested or upstream stage done & queue empty
    ThreadFinished --> [*]: Set h265_thread_state = 3 (Finished)
```

---

## 4. State-Machine Diagram 2: `rusty_h265_metrics_agent.py` (Master & Remote Roles, Log Rotation & Shipping)

```mermaid
stateDiagram-v2
    [*] --> ParseCliArgs: Launch rusty_h265_metrics_agent.py

    ParseCliArgs --> RemoteRoleInit: --role remote
    ParseCliArgs --> MasterRoleInit: --role master

    state "Remote Host Mode (--role remote)" as RemoteMode {
        RemoteRoleInit --> EmitNodeInventory: Load ctypes (kernel32 / libSystem / procfs + NVML / IOKit)\nEmit JSON event="node_inventory" to stdout
        EmitNodeInventory --> SampleRemoteSyscalls: Poll cycle (--interval 5s or --once)
        SampleRemoteSyscalls --> PrintRemoteJson: Query CPU %, RAM GB, Temp Disk Free GB, GPU/NVENC/NVDEC %, VRAM, Temp, Power\nWrite single-line JSON to stdout & flush
        PrintRemoteJson --> SampleRemoteSyscalls: Sleep interval (if not --once)
    }

    state "Local Master Mode (--role master)" as MasterMode {
        MasterRoleInit --> StartHttpAndThreads: Parse config.yaml\nBind GET /metrics on PROMETHEUS_LISTEN_ADDR (unless --once)

        state "Prometheus Metrics Loop" as PromLoop {
            StartHttpAndThreads --> ReadRustSnapshot: Read /tmp/rusty_h265_telemetry.json
            ReadRustSnapshot --> SampleLocalSyscalls: Query local CPU, LoadAvg, VM Memory, Process Tree RSS/CPU, Disk statvfs, IOKit/NVML GPU
            SampleLocalSyscalls --> RenderPrometheusText: Merge Rust thread metrics + remote node samples + local syscalls -> Prometheus v0.0.4 text
            RenderPrometheusText --> PushToPrometheus: Update /metrics cache & POST/PUT to PROMETHEUS_ENDPOINT (if configured)
            PushToPrometheus --> ReadRustSnapshot: Sleep PROMETHEUS_PUSH_INTERVAL_SEC
        }

        state "Rotation-Aware Loki Log Tailer" as LokiLoop {
            StartHttpAndThreads --> TailActiveLogFile: Open rusty_h265_converter.log (record initial inode & offset)
            TailActiveLogFile --> ParseLogfmtLine: Read new lines -> extract nodename, host_os, worker_group, worker_thread, mount_point, level
            ParseLogfmtLine --> CheckRotationOrBatch: Append [ns_timestamp, line] to Loki stream buffer
            CheckRotationOrBatch --> DetectLogRotation: stat(log_file).st_ino != current_ino OR st_size < current_offset
            DetectLogRotation --> TailActiveLogFile: Drain remaining bytes from old fd, reopen new rusty_h265_converter.log at offset 0
            CheckRotationOrBatch --> PushLokiBatch: Batch size >= LOKI_BATCH_SIZE_BYTES or LOKI_BATCH_WAIT_MSEC elapsed
            PushLokiBatch --> TailActiveLogFile: POST /loki/api/v1/push (with retry on transient HTTP error)
        }
    }

    PrintRemoteJson --> [*]: Exit 0 (when --once or SSH pipe closes)
    PushToPrometheus --> FinalDrainAndExit: --once OR (--parent-pid exited and not --daemon)
    PushLokiBatch --> FinalDrainAndExit: --parent-pid exited and not --daemon
    FinalDrainAndExit --> [*]: Final snapshot read + final Prometheus push + final Loki log flush
```

---

## 5. Per-Thread Wait-Time Matrix & Shared Label Taxonomy

### 5.1 Shared Labels (`Prometheus == Loki`)
| Label Key | Example Values | Purpose |
| :--- | :--- | :--- |
| `app` | `"rusty_h265_converter"` | Application identifier |
| `nodename` | `"mac-master"`, `"win-rtx3050ti"`, `"mac-mini-m2"` | Configured in `config.yaml` via `LOCAL.NODENAME` and `WORKER_NODES[].NODENAME` |
| `host_os` | `"Mac"`, `"Windows"`, `"Linux"` | Operating system of the host executing the task or hardware sample |
| `worker_group` | `"local:/Volumes/f0m"`, `"remote:win-rtx3050ti"`, `"orchestrator"` | Isolates each physical mount worker-group or remote SSH manager |
| `worker_thread` | `"verifier"`, `"stager"`, `"converter"`, `"flusher"`, `"remote_dispatcher"`, `"scp_upload"`, `"remote_encode"`, `"scp_copyback"`, `"hw_sampler"` | Pinpoints the exact thread within the worker-group |
| `mount_point` | `"/Volumes/f0m"`, `"/Volumes/f1m"` | Physical drive mount point |
| `level` *(Loki)* | `"INFO"`, `"WARN"`, `"ERROR"`, `"DEBUG"` | Log severity |

### 5.2 Per-Thread Wait-Time Reasons (`wait_reason`)
| Worker-Group | `worker_thread` | Tracked `wait_reason` Values |
| :--- | :--- | :--- |
| `local:<mount>` | `verifier` | `lookahead_buffer_full`, `io_lock` |
| `local:<mount>` | `stager` | `upstream_queue_empty`, `ramdisk_quota_full`, `flusher_priority_yield`, `io_lock` |
| `local:<mount>` | `converter` | `upstream_queue_empty` |
| `local:<mount>` | `flusher` | `upstream_queue_empty`, `io_lock` |
| `remote:<nodename>` | `remote_dispatcher` | `upstream_queue_empty`, `remote_quota_full` |
| `remote:<nodename>` | `scp_upload` | `upstream_queue_empty` |
| `remote:<nodename>` | `remote_encode` | `upstream_queue_empty` |
| `remote:<nodename>` | `scp_copyback` | `upstream_queue_empty` |
