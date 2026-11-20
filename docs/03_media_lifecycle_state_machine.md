# Diagram 3: Media Item & Subdirectory Reconciliation State Machine

This state machine diagram traces the complete lifecycle of a media file and its parent subdirectory in [src/main.rs](../src/main.rs)—from initial discovery in `raw_queue`, through the `Slot 0` Stager's rotating queue inspection (`completed_files.journal`, `broken_files.journal`, `ffprobe -v error`, and 1-frame decode test), local 3-thread (`Stager` -> `Converter` -> `Flusher`) or remote 3-thread (`scp-up` -> `ssh ffmpeg` -> `scp-down` / eviction) conversion, and finally Step 2.6 subdirectory completion and `target_count > source_count` self-healing reconciliation.

* **Parent Document**: [SOFTWARE_ARCHITECTURE_AND_DESIGN.md](SOFTWARE_ARCHITECTURE_AND_DESIGN.md)
* **Mermaid Syntax Reference**: [Mermaid Flowcharts & State Diagrams](https://mermaid.js.org/syntax/flowchart.html)

```mermaid
stateDiagram-v2
    [*] --> RawQueued: Discovered in populate_mount_queue_for_dirs()

    RawQueued --> BrokenSkipped: Found in broken_files.journal
    RawQueued --> Tier1Journal: Not in broken_files.journal (Case A Check)

    Tier1Journal --> VerifiedSkip: Match found in completed_files.journal O(1)
    Tier1Journal --> Tier2Ffprobe: Not in journal, but _h265 file exists
    Tier1Journal --> StageA_Ffprobe: No existing _h265 output

    Tier2Ffprobe --> VerifiedSkip: Valid stream and duration diff within 1.5s
    Tier2Ffprobe --> DeletePartial: Corrupt or runtime mismatch
    DeletePartial --> StageA_Ffprobe: Delete partial output and re-probe source

    VerifiedSkip --> CheckSubdirDone: Increment Ver count

    StageA_Ffprobe --> RecordBroken: Unknown codec, zero duration, or stderr error
    StageA_Ffprobe --> StageB_1Frame: Valid container and stream headers
    StageB_1Frame --> RecordBroken: 1-frame ffmpeg decode test fails
    StageB_1Frame --> CodecHeuristic: 1st frame decodes cleanly

    RecordBroken --> CheckSubdirDone: Append to broken_files.journal and increment Err count
    BrokenSkipped --> CheckSubdirDone: Increment Err count (keep source subdir intact)

    CodecHeuristic --> FastMove: Case B1 - Already HEVC or bitrate below MIN_BITRATE_KBPS
    CodecHeuristic --> ReadyQueue: Case B2 - Needs H.265 conversion (push to ms.queue)

    FastMove --> CheckSubdirDone: Move via io_lock and increment Moved count

    ReadyQueue --> LocalStagerRead: Popped by Local Slot 0 Stager (copy HDD to RAMDisk via io_lock)
    ReadyQueue --> ScpUp: Picked by Remote Dispatcher (Thread 1 scp-up)

    LocalStagerRead --> StagedQueue: Push to staged_queue (Max 4 items in RAMDisk)
    StagedQueue --> LocalEncode: Popped by Local Slot 1 Converter (VideoToolbox / NVENC / libx265)
    LocalEncode --> FlushQueue: Push to flush_queue
    LocalEncode --> RecordBroken: Local FFmpeg conversion error
    FlushQueue --> LocalFlusherWrite: Popped by Local Slot 2 Flusher (copy RAMDisk to HDD via io_lock)
    FlushQueue --> KeepSmallerOrig: Encoded size larger than source (move original via io_lock)

    ScpUp --> RemoteEncode: Thread 2 (ssh ffmpeg)
    RemoteEncode --> ScpDown: FFmpeg exit 0 -> Thread 3 (encode_failed = false)
    RemoteEncode --> ReadyQueue: SSH transport drop (exit 255, re-queue to front)
    RemoteEncode --> RemoteEvictFailed: FFmpeg media error -> Thread 3 (encode_failed = true)
    RemoteEvictFailed --> RecordBroken: Thread 3 SSH deletes remote files, releases quota, logs broken

    KeepSmallerOrig --> CheckSubdirDone: Move original and record in completed_files.journal
    LocalFlusherWrite --> CheckSubdirDone: Record in completed_files.journal and increment Conv count
    ScpDown --> CheckSubdirDone: Record in completed_files.journal and increment Conv count

    CheckSubdirDone --> KeepSourceIntact: failed_files greater than 0
    CheckSubdirDone --> CompareCounts: completed_files equals total_files and failed_files equals 0

    CompareCounts --> DeleteSourceInfo: target_count equals source_count (Log INFO)
    CompareCounts --> FfprobeAllTargets: target_count greater than source_count
    CompareCounts --> KeepSourceIntact: target_count less than source_count (Log ERROR)

    FfprobeAllTargets --> RecheckCounts: Step 1 and 2 - ffprobe targets, purge bad, recount
    RecheckCounts --> DeleteSourceInfo: Recounted target_count equals source_count
    RecheckCounts --> VerifyAllSourcesAccounted: Recounted target_count still greater than source_count
    VerifyAllSourcesAccounted --> DeleteSourceWarn: Step 3 and 4 - All sources present, log extra (WARN) and delete source (WARN)
    VerifyAllSourcesAccounted --> KeepSourceIntact: Any source file missing (Log ERROR)

    DeleteSourceInfo --> [*]
    DeleteSourceWarn --> [*]
    KeepSourceIntact --> [*]
```
