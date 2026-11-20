# Diagram 2: Parallel Sequence Diagram (Remote 3-Thread Pipeline, Failure Handoff & Work Stealing)

This sequence diagram illustrates how a Remote Machine Manager (`run_remote_machine_manager` in [src/main.rs](../src/main.rs)) coordinates with the per-mount `Slot 0` Stager (`run_local_stager_thread`) and overlaps its three child threads (`upload_handle`, `encode_handle`, and `copyback_handle`). When a remote FFmpeg encode fails due to a non-SSH media error, `encode_handle` hands off cleanup (`encode_failed: true`) to `copyback_handle` via `copyback_queue` so `Slot 1` immediately starts encoding the next queued file while `Slot 2` asynchronously evicts the remote input/output files and records the failure in `broken_files.journal`.

* **Parent Document**: [SOFTWARE_ARCHITECTURE_AND_DESIGN.md](SOFTWARE_ARCHITECTURE_AND_DESIGN.md)
* **Mermaid Syntax Reference**: [Mermaid Sequence Diagrams](https://mermaid.ai/open-source/syntax/sequenceDiagram.html)

```mermaid
sequenceDiagram
    autonumber
    participant S0 as Slot 0 Stager<br/>(Local Mount)
    participant MQ as MountQueueState<br/>(ms.queue + in_flight_items)
    participant Disp as Remote Dispatcher<br/>(Main Loop)
    participant T1 as Thread 1: upload_handle<br/>(Slot 0: scp-up)
    participant T2 as Thread 2: encode_handle<br/>(Slot 1: ssh ffmpeg)
    participant T3 as Thread 3: copyback_handle<br/>(Slot 2: scp-down / evict)
    participant J as Journals<br/>(Task + Completed + Broken)

    S0->>S0: Rotating Inspection (Case A Resume / B1 Fast-Move / B2 Probe)
    S0->>S0: Stage A: ffprobe -v error (codec & duration)
    S0->>S0: Stage B: ffmpeg -frames:v 1 -f null - (1-frame decode test)
    S0->>MQ: push_back(QueuedMediaItem + cached_probe) to ms.queue

    Disp->>MQ: pick_mount_queue_prioritizing_unoccupied()
    MQ-->>Disp: Pop verified item (enforce MAX_SIZE_IN_GB quota)
    Disp->>MQ: set_in_flight(Remote, "REMOTE <host> [Queued for scp-up]")
    Disp->>J: Upsert Task (Pending) in distributed_tasks_journal.json
    Disp->>T1: Push (mq, task) to upload_queue

    par Thread 1: Asynchronous Upload (Slot 0)
        T1->>MQ: set_in_flight(Remote, "REMOTE <host> [Thread 1: scp-up]")
        T1->>J: copy_to_remote_state = Started
        T1->>T1: Spawn PTY `scp` -> stream live MB/s & % to TUI Slot 0
        T1->>J: copy_to_remote_state = Completed
        T1->>MQ: set_in_flight(Remote, "REMOTE <host> [Queued for Encode]")
        T1->>T2: Push (mq, task) to encode_queue
    and Thread 2: Remote FFmpeg Conversion (Slot 1)
        T2->>MQ: set_in_flight(Remote, "REMOTE <host> [Thread 2: Encoding]")
        T2->>J: conversion_state = Started
        T2->>T2: Spawn `ssh ffmpeg -nostdin ... -progress pipe:1`
        alt FFmpeg Succeeded (exit 0)
            T2->>J: conversion_state = Completed
            T2->>MQ: set_in_flight(Remote, "REMOTE <host> [Queued for scp-down]")
            T2->>T3: Push RemoteCopybackTask(encode_failed = false) to copyback_queue
        else SSH Transport Drop (exit 255)
            T2->>MQ: clear_in_flight() & re-queue item to front of ms.queue
        else FFmpeg Media/Decode Error (e.g. exit 183)
            T2->>MQ: set_in_flight(Remote, "REMOTE <host> [Queued for Eviction]")
            T2->>T3: Push RemoteCopybackTask(encode_failed = true, reason) to copyback_queue
        end
    and Thread 3: Asynchronous Copy-Back or Failed-Encode Eviction (Slot 2)
        alt encode_failed == false (Successful Encode)
            T3->>MQ: set_in_flight(Remote, "REMOTE <host> [Thread 3: scp-down]")
            T3->>J: copy_back_state = Started
            T3->>T3: Spawn PTY `scp` download -> stream live MB/s & % to TUI Slot 2
            T3->>T3: SSH remove remote_input & remote_output + release quota
            T3->>J: copy_back_state = Completed & append to completed_files.journal
            T3->>MQ: clear_in_flight() & update SubdirTracker (Step 2.6 reconciliation)
        else encode_failed == true (Failed Encode Handoff)
            T3->>MQ: set_in_flight(Remote, "REMOTE <host> [Thread 3: Evicting Failed]")
            T3->>T3: SSH remove remote_input & partial remote_output + release quota
            T3->>J: conversion_state = Failed & append to broken_files.journal
            T3->>MQ: clear_in_flight() & mark failed in SubdirTracker (never re-queued)
        end
    end
```
