# Diagram 5: Graceful Shutdown & Signal Handling Flow

This diagram details what happens inside [src/main.rs](../src/main.rs) when the user presses `'q'` (on the Dashboard) or `Ctrl+C`, or when the process receives `SIGINT`, `SIGTERM`, `SIGHUP`, or `SIGQUIT`. In-flight uploads (`scp-up`), staging reads (`HDD -> RAMDisk`), and `ffmpeg` encodes are aborted and cleaned up immediately without polluting `failed_dirs.txt` or `broken_files.journal`, while completed encodes currently flushing via `scp-down` or `RAMDisk -> HDD` (`Slot 2` Flusher) are allowed to finish and record into `completed_files.journal`.

* **Parent Document**: [SOFTWARE_ARCHITECTURE_AND_DESIGN.md](SOFTWARE_ARCHITECTURE_AND_DESIGN.md)
* **Mermaid Syntax Reference**: [Mermaid Flowcharts](https://mermaid.js.org/syntax/flowchart.html)

```mermaid
flowchart TD
    subgraph Triggers["1. Graceful Exit Triggers"]
        K["Keyboard Listener (TerminalInputGuard)<br/>Reads 'q'/'Q' (in Dashboard) or 0x03 (Ctrl+C) from /dev/tty"]
        S["POSIX Signal Handler (install_signal_handlers)<br/>Traps SIGINT (2), SIGTERM (15), SIGHUP (1), SIGQUIT (3)<br/>(All termination signals except SIGKILL -9)"]
        K --> Flag["SHUTDOWN_REQUESTED = true (AtomicBool)<br/>TUI Banner -> [!] GRACEFUL EXIT: Draining scp-down &amp; RAMDisk-&gt;HDD..."]
        S --> Flag
    end

    subgraph Aborted["2. Immediately Aborted Stages (No False Error Logs)"]
        A1["Local Slot 0: Stager Thread<br/>Stops inspecting raw_queue/queue;<br/>aborts in-flight HDD -&gt; RAMDisk copy;<br/>deletes partial staged_input &amp; drains staged_queue"]
        A3["Local Slot 1: Converter Thread<br/>spawn_shutdown_killer sends SIGTERM/SIGKILL<br/>to local ffmpeg process group; deletes partial output"]
        A4["Remote Dispatcher &amp; Thread 1 (scp-up)<br/>Drains upload_queue; kills in-flight scp-up;<br/>SSH deletes partial remote_input_path"]
        A5["Remote Thread 2 (ssh ffmpeg)<br/>Drains encode_queue; kills local ssh client;<br/>runs remote killall ffmpeg / Stop-Process;<br/>SSH deletes partial remote_input &amp; remote_output"]
    end

    subgraph Drained["3. Completed / Drained Stages (Preserves Completed Work)"]
        D1["Local Slot 2: Flusher Thread (RAMDisk -&gt; HDD)<br/>abort_on_shutdown = false<br/>Drains flush_queue &amp; completes full 4MB chunk flush to dest_dir;<br/>records in completed_files.journal"]
        D2["Remote Thread 3: Copy-Back (scp-down)<br/>abort_on_shutdown = false &amp; process_group(0)<br/>Continues until copyback_queue is empty;<br/>completes scp-down to final_h265_dest_path;<br/>records in completed_files.journal &amp; TaskJournal"]
    end

    subgraph Cleanup["4. Final Teardown"]
        C1["SSH Cleanup of Remote Temp Dir<br/>(/tmp/rusty_h265_remote_&lt;pid&gt;)"]
        C2["Restore Terminal State<br/>(tcsetattr ICANON/ECHO + show cursor \\x1b[?25h + enable wrap \\x1b[?7h)"]
        C3["Unmount &amp; Detach Provisioned RAMDisk<br/>(hdiutil detach -force /Volumes/RustyH265RAMDisk)"]
        C1 --> C2 --> C3
    end

    Flag --> A1 & A3 & A4 & A5
    Flag --> D1 & D2
    A1 & A3 & A4 & A5 --> Cleanup
    D1 & D2 --> Cleanup
```
