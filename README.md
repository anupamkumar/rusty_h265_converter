# Rusty H.265 Converter

**Shrink your video library by 40–70% — automatically, safely, and fast.**

**Rusty H.265 Converter** (`rusty_h265_converter`) is an automated video-shrinking assistant that converts bulky older video files (`.mp4`, `.mkv`, `.mov`, `.avi`, `.wmv`, `.flv`, `.mpg`, and more) into modern, space-saving **H.265 (HEVC)** `.mp4` files out of the box — across one computer or a whole team of computers on your home network.

**Built for everyday users *and* audio/video super-users:** Want to convert an entire library to **AV1**, **VP9/WebM**, **H.264**, **ProRes**, or even **animated WebP** instead of H.265? Just change 3 simple target fields in `config.yaml` — no FFmpeg wizardry required. And if you *are* an audio/video enthusiast who knows your exact encoder flags, bitrates, and quality curves, you can uncomment any `FFMPEG_*` override block in `config.yaml` for full low-level control, backed by automatic pre-flight validation (`--dry-run`) so mismatched settings are caught before a single file is touched.

---

## Why Use Rusty H.265 Converter?

If you have years of home videos, camera clips, screen recordings, or a large personal media library spread across external hard drives, you have probably noticed how fast storage fills up. Older video formats (like H.264, WMV, or AVI) take up **2× to 3× more disk space** than modern H.265 at virtually identical visual quality.

Converting thousands of videos by hand is tedious and risky. Rusty H.265 Converter takes care of the hard parts for you:

- **Reclaim massive disk space automatically:** Converts entire folder trees into compact H.265 `.mp4` files (or any target codec/container you choose) while keeping your folder structure intact.
- **Flexible formats + super-user FFmpeg control:** Switch targets (`hevc`, `av1`, `vp9`, `h264`, `prores`, `webp`, `gif`) in 3 lines of `config.yaml` with automatic encoder discovery and 5-second encode/decode self-testing — or uncomment raw `FFMPEG_*` override flags for full power-user tuning.
- **Safety first — never lose a good video:** Every converted video is checked for audio/video length and sync before any original file is removed. If a converted file somehow ends up larger than the original, it keeps the smaller original instead.
- **Built for real-world messy libraries:** Already-compressed H.265 or low-bitrate videos are moved over instantly without wasting time re-encoding. Corrupt or unreadable videos are logged and skipped cleanly so the rest of your folder finishes normally.
- **Protects external hard drives:** Uses a fast temporary staging area (like a RAM disk) and reads/writes one stream per physical drive at a time so mechanical hard drives stay cool, fast, and quiet.
- **Pause and resume anytime:** Press `Ctrl+C` to stop gracefully; progress is saved in journals so your next run picks up right where you left off.
- **Use your computer's video hardware — and borrow other computers too:** Automatically taps Apple Silicon (`VideoToolbox`) and NVIDIA (`NVENC`) hardware encoders for super-fast conversion, and can optionally offload work to other Mac, Linux, or Windows PCs on your local network.
- **Live dashboard & optional monitoring:** Includes a clean live terminal progress dashboard (plus optional **Prometheus**, **Loki**, and **Grafana** graphs and log search).

---

## Quickstart Guide

### 1. Prerequisite: Install FFmpeg

Rusty H.265 Converter uses **FFmpeg** (and `ffprobe`) under the hood to inspect and convert videos. Install it once on the computer where you will run the app:

- **macOS:** `brew install ffmpeg`
- **Windows:** `winget install Gyan.FFmpeg` (then restart your terminal so `ffmpeg` and `ffprobe` are in your `PATH`)
- **Linux / Raspberry Pi (Debian/Ubuntu):** `sudo apt update && sudo apt install -y ffmpeg`

### 2. Get the Executable for Your Platform

Grab the binary for your operating system and CPU architecture from the [`dist/`](dist/) folder (verify integrity anytime with [`dist/SHA256SUMS.txt`](dist/SHA256SUMS.txt)):

| Platform | CPU Architecture | Binary |
| :--- | :--- | :--- |
| **macOS** | Apple Silicon (M1/M2/M3/M4 — `arm64`) | [`rusty_h265_converter-macos-arm64`](https://github.com/anupamkumar/rusty_h265_converter/releases/download/v1.0/rusty_h265_converter-macos-arm64) |
| **macOS** | Intel (`x86_64`) | Build with `./scripts/build_binaries.sh macos x86_64` → `dist/rusty_h265_converter-macos-x86_64` |
| **Windows** | 64-bit Intel/AMD (`x86_64`) | [`rusty_h265_converter-windows-x86_64.exe`](https://github.com/anupamkumar/rusty_h265_converter/releases/download/v1.0/rusty_h265_converter-windows-x86_64.exe) |
| **Windows** | ARM64 (`arm64`) | Build with `./scripts/build_binaries.sh macos x86_64` → `dist/rusty_h265_converter-windows-arm64.exe` |
| **Linux / Raspberry Pi** | ARM64 (`arm64` / `aarch64`) | [`rusty_h265_converter-linux-arm64`](https://github.com/anupamkumar/rusty_h265_converter/releases/download/v1.0/rusty_h265_converter-linux-arm64) |
| **Linux** | 64-bit Intel/AMD (`x86_64`) | [`rusty_h265_converter-linux-x86_64`](https://github.com/anupamkumar/rusty_h265_converter/releases/download/v1.0/rusty_h265_converter-linux-x86_64) |

Place the executable in a folder alongside [`config.yaml`](config.yaml) and [`logging.conf`](logging.conf).

### 3. Verify Your Setup with `--dry-run` (Recommended)

Before pointing the app at your video library (or anytime you edit [`config.yaml`](config.yaml) to change codecs, containers, or worker machines), run `--dry-run`. It checks your config for compatibility, discovers the fastest working hardware/software encoder on your computer (and any configured remote `WORKER_NODES`), and runs a quick 5-second synthetic test encode/decode — **without touching any of your files or creating a RAM disk**:

#### macOS
```bash
chmod +x ./rusty_h265_converter-macos-arm64
./rusty_h265_converter-macos-arm64 --dry-run
```

#### Windows (PowerShell or Command Prompt)
```powershell
.\rusty_h265_converter-windows-x86_64.exe --dry-run
```

#### Linux / Raspberry Pi
```bash
chmod +x ./rusty_h265_converter-linux-arm64
./rusty_h265_converter-linux-arm64 --dry-run
```

*(Depending on your hardware and configured remote workers, encoder discovery may take up to a couple of minutes — you will see a live progress animation while it runs.)*

### 4. Run the App

Point the app at one or more folders containing your videos. For each subfolder (for example `Vacation`), it creates a matching `Vacation_h265` folder alongside it and converts your videos into it.

#### macOS
```bash
./rusty_h265_converter-macos-arm64 "/Volumes/MyDrive/Videos"
```
*(You can pass multiple drives/folders at once — each physical drive is processed in parallel!)*
```bash
./rusty_h265_converter-macos-arm64 "/Volumes/Drive1/Movies" "/Volumes/Drive2/Clips"
```

#### Windows (PowerShell or Command Prompt)
```powershell
.\rusty_h265_converter-windows-x86_64.exe "D:\Videos"
```

#### Linux / Raspberry Pi
```bash
./rusty_h265_converter-linux-arm64 "/mnt/media/Videos"
```

#### Useful Command-Line Options
| Flag | What It Does |
| :--- | :--- |
| `--dry-run` | Validate `config.yaml` compatibility and discover/probe working encoders on this machine (and all configured `WORKER_NODES`) using a 5-second synthetic test clip — without mounting a RAM disk or converting any files. |
| `--config <path>` | Use a custom configuration file (default: `config.yaml`). |
| `--log-config <path>` | Use a custom logging settings file (default: `logging.conf`). |
| `-v`, `--verbose` | Print plain step-by-step log lines instead of the interactive dashboard. |
| `--help` | Show all command-line options. |

> **Tip while running:** Press **`v`** inside the interactive terminal UI to open the live queue viewer and browse upcoming files (use arrow keys / PageUp / PageDown to scroll, `q` to return). Press **`Ctrl+C`** anytime for a clean, safe shutdown that finishes in-progress disk writes before exiting.

---

### Understanding `config.yaml`

[`config.yaml`](config.yaml) controls how Rusty H.265 Converter encodes, stages, verifies, and distributes your videos. The defaults work well out of the box, and the settings you are most likely to customize are:

| Setting | Default | Plain-English Meaning |
| :--- | :--- | :--- |
| `LOCAL.NODENAME` | `"mac-master"` | Friendly name for this computer in logs and dashboards. |
| `VIDEO_EXTENSIONS` | `.mp4`, `.mkv`, `.mov`, `.wmv`, `.avi`, … | File types the app treats as videos. |
| `TARGET_VIDEO_CODEC` | `"hevc"` | Target video codec (`hevc`, `h264`, `av1`, `vp9`, `vp8`, `prores`, `webp`, `gif`). |
| `TARGET_AUDIO_CODEC` | `"aac"` | Target audio codec (`aac`, `opus`, `vorbis`, `mp3`, `flac`, `ac3`, `copy`, or `none`). |
| `TARGET_CONTAINER` | `"mp4"` | Output container format (`mp4`, `mkv`, `webm`, `mov`, `webp`, `gif`). Must match `OUTPUT_EXTENSION` if both are set. |
| `OUTPUT_SUFFIX` / `OUTPUT_EXTENSION` | `"_h265"` / `".mp4"` | Suffix added to converted folders/files and output file extension. |
| `MIN_BITRATE_KBPS` | `1800` | Videos already below this bitrate are moved as-is instead of re-encoded (saves time and avoids quality loss). |
| `KEEP_SMALLER_ORIGINAL` | `true` | If a converted file ends up larger than the original, keep the smaller original instead. |
| `COPY_COMPATIBLE_AUDIO` | `true` | Copies already-container-compatible audio tracks directly for speed; automatically re-encodes audio on retry if sync issues appear. |
| `SORT_BY_SIZE_DESC` | `true` | Processes the largest folders/files first so you reclaim the most space earliest. |
| `ENABLE_RAMDISK` / `RAMDISK_SIZE_GB` | `true` / `8.0` | Creates a fast in-memory staging disk on macOS (`/Volumes/...`) or Linux (`/dev/shm/...`) to reduce wear and thrashing on hard drives. On Windows, falls back cleanly to `LOCAL_STAGING_DIR` (or `%TEMP%`). |
| `LOCAL_STAGING_DIR` | `"/Volumes/RustyH265RAMDisk/staging"` | Temporary workspace for active encodes (on Windows, set e.g. `"C:\\Temp\\staging"` or a mounted RAM-drive letter like `"R:\\staging"`). |
| `DURATION_TOLERANCE_SECS` / `AV_STREAM_MISMATCH_TOLERANCE_SECS` | `1.5` / `1.5` | Safety checks ensuring the converted video length and audio/video sync match the original before accepting it. |
| `FAILED_DIRS_PATH` | `"failed_dirs.txt"` | File where folders containing corrupt/unreadable videos are summarized for your review (e.g., `"broken_directories.journal"`). |
| `HIGH_PERFORMANCE_DISTRIBUTED_SYSTEM` | `true` | Turns remote SSH worker machines on (`true`) or off (`false` for single-machine mode). |
| `PROMETHEUS_ENABLED` / `LOKI_ENABLED` | `true` / `true` | Enables live metrics (Prometheus) and centralized log shipping (Loki). |

*(Log verbosity, log file location, and log rotation size/count are configured separately in [`logging.conf`](logging.conf).)*

#### Supported Codec & Container Pairings

You can switch your output format simply by editing `TARGET_VIDEO_CODEC`, `TARGET_AUDIO_CODEC`, and `TARGET_CONTAINER` in [`config.yaml`](config.yaml) (plus `OUTPUT_EXTENSION` / `OUTPUT_SUFFIX` if desired) — no raw FFmpeg flags needed! Before starting any conversion (or when running `--dry-run`), the app automatically validates compatibility and runs a 5-second `256×144` synthetic encode + full-stream decode check on each host.

| Container (`TARGET_CONTAINER`) | Allowed Video Codecs (`TARGET_VIDEO_CODEC`) | Allowed Audio Codecs (`TARGET_AUDIO_CODEC`) | Stream-Copy Compatible Audio (`-c:a copy`) |
| :--- | :--- | :--- | :--- |
| **`mp4`** (`.mp4`) | `hevc`, `h264`, `av1`, `vp9` | `aac`, `mp3`, `ac3`, `opus`, `flac`, `copy`, `none` | `aac`, `mp3`, `ac3`, `eac3`, `alac`, `mp2`, `opus`, `flac` |
| **`mkv`** (`.mkv`) | `hevc`, `h264`, `av1`, `vp9`, `vp8`, `prores` | `aac`, `opus`, `vorbis`, `mp3`, `flac`, `ac3`, `copy`, `none` | *(all standard codecs)* |
| **`webm`** (`.webm`) | `vp9`, `vp8`, `av1` | `opus`, `vorbis`, `copy`, `none` | `opus`, `vorbis` |
| **`mov`** (`.mov`) | `hevc`, `h264`, `prores`, `av1` | `aac`, `mp3`, `ac3`, `flac`, `copy`, `none` | `aac`, `alac`, `mp3`, `ac3`, `pcm_s16le`, `pcm_s24le` |
| **`webp`** (`.webp`) | `webp` | `none` **only** | *(none — audio is stripped via `-an`)* |
| **`gif`** (`.gif`) | `gif` | `none` **only** | *(none — audio is stripped via `-an`)* |

> **Need an in-depth codec & container reference?** See MDN Web Docs' [**Video codecs guide**](https://developer.mozilla.org/en-US/docs/Web/Media/Guides/Formats/Video_codecs) for detailed browser, device, and container compatibility guidance.
>
> **Power-user tip:** To customize raw FFmpeg flags, uncomment any `FFMPEG_*_ENCODER_ARGS` or `FFMPEG_*_AUDIO_ARGS` block in [`config.yaml`](config.yaml) and run `./rusty_h265_converter --dry-run` to verify your overrides before running a batch. If a custom block's `-c:v` or `-c:a` conflicts with `TARGET_*_CODEC`, the app stops immediately with a clear error so you never accidentally encode with mismatched flags.

---

### Wiring Up the Distributed Platform (Optional Extra Worker Computers)

Have a gaming PC with an NVIDIA GPU or a spare Mac/Linux machine sitting on the same Wi-Fi/Ethernet network? Rusty H.265 Converter can automatically send videos to those machines over SSH, convert them in parallel, and copy the smaller H.265 files right back.

1. **Install FFmpeg on each worker machine** (Mac, Linux, or Windows) and enable its SSH server (macOS: *System Settings → General → Sharing → Remote Login*; Linux: `sudo apt install openssh-server`; Windows: *Optional Features → OpenSSH Server*).
2. **Set up passwordless SSH login** from your main computer to each worker (so the app can connect without prompting for a password):
   ```bash
   ssh-keygen -t ed25519           # skip if you already have an SSH key
   ssh-copy-id user@192.168.1.50   # repeat for each worker
   ssh user@192.168.1.50 "echo OK" # verify it connects with no password prompt
   ```
3. **Add each worker to `WORKER_NODES` in [`config.yaml`](config.yaml)** and set `HIGH_PERFORMANCE_DISTRIBUTED_SYSTEM: true`:
   ```yaml
   HIGH_PERFORMANCE_DISTRIBUTED_SYSTEM: true

   WORKER_NODES:
     - NODENAME: "win-rtx3050ti"
       IP_ADDRESS: "192.168.1.120"
       USERNAME: "conv"
       SSH_PORT: 22
       MAX_SIZE_IN_GB: 8
       FFMPEG_PATH: 'C:\ffmpeg\bin\ffmpeg.exe'
       OS_TYPE: "Windows"
       GPU_PROFILE: "GTX_3050_TI"

     - NODENAME: "mac-mini-m2"
       IP_ADDRESS: "192.168.1.107"
       USERNAME: "dev"
       SSH_PORT: 22
       MAX_SIZE_IN_GB: 30.0
       FFMPEG_PATH: "/opt/homebrew/opt/ffmpeg-full/bin/ffmpeg"
       OS_TYPE: "Mac"
   ```
   - `MAX_SIZE_IN_GB` caps how much temporary disk space the converter is allowed to use on that worker.
   - `OS_TYPE` should be `"Mac"`, `"Linux"`, or `"Windows"`.
   - When you run the converter, it automatically discovers the fastest available hardware H.265 encoder on each worker and manages uploads, encodes, copy-backs, and cleanup for you.

---

### Wiring Up Prometheus & Grafana Loki (Optional Metrics & Log Dashboards)

If you want real-time charts of encoding speed, space saved, CPU/GPU usage across all your machines, and searchable logs, the app includes a zero-dependency Python companion agent ([`scripts/rusty_h265_metrics_agent.py`](scripts/rusty_h265_metrics_agent.py)) that works out of the box with **Prometheus**, **Grafana Loki**, and **Grafana**.

1. **Install Prometheus, Loki, and Grafana** (for example on macOS via Homebrew):
   ```bash
   brew install prometheus loki grafana
   brew services start prometheus loki grafana
   ```
2. **Tell Prometheus to scrape the converter's metrics** by adding this job to `prometheus.yml`:
   ```yaml
   scrape_configs:
     - job_name: "rusty_h265_converter"
       scrape_interval: 5s
       static_configs:
         - targets: ["127.0.0.1:9184"]
   ```
3. **Enable Prometheus & Loki in [`config.yaml`](config.yaml):**
   ```yaml
   PROMETHEUS_ENABLED: true
   PROMETHEUS_LISTEN_ADDR: "0.0.0.0:9184"
   AUTO_SPAWN_METRICS_AGENT: true    # set to false if running the agent as an always-on daemon
   DEPLOY_REMOTE_METRICS_AGENT: true # streams remote workers' CPU/RAM/GPU stats over SSH

   LOKI_ENABLED: true
   LOKI_ENDPOINT: "http://127.0.0.1:3100/loki/api/v1/push"
   LOKI_BATCH_WAIT_MSEC: 1000
   LOKI_BATCH_SIZE_BYTES: 65536
   ```
4. **Choose how to run the companion agent:**
   - **Auto-spawn (easiest):** Leave `AUTO_SPAWN_METRICS_AGENT: true` and the converter starts and stops the agent automatically whenever you run a conversion.
   - **Always-on background daemon:** Set `AUTO_SPAWN_METRICS_AGENT: false` and run the agent persistently (e.g., via macOS `launchd` or Linux `systemd`):
     ```bash
     python3 scripts/rusty_h265_metrics_agent.py --role master --daemon \
       --config config.yaml --log-config logging.conf
     ```
5. **Open Grafana** (`http://localhost:3000`), add **Prometheus** (`http://127.0.0.1:9090`) and **Loki** (`http://127.0.0.1:3100`) as data sources, and explore metrics like `h265_encoder_realtime_speed_x`, `h265_media_storage_saved_in_mb_total`, `h265_host_cpu_utilization_ratio`, and logs under `{app="rusty_h265_converter"}`.

---

## How It Works

Under the hood, Rusty H.265 Converter organizes your folders by physical drive and runs a smooth assembly line for each drive so nothing sits idle and no hard drive gets overloaded:

1. **Inspect & Stage:** A verifier checks each video — instantly skipping files already converted in a previous run, moving already-target-codec or low-bitrate videos directly, and catching corrupt files with `ffprobe` + a 1-frame decode test before spending time encoding them. If an old `.wmv`/`.asf` file is missing duration metadata in its header, a fast packet-copy 2nd pass recovers its true duration automatically. Healthy videos are copied sequentially into a fast staging area (RAM disk / temp SSD).
2. **Hardware Convert (Local + Remote) with Attempt 2 Fallback:** The local hardware encoder (Apple Silicon VideoToolbox, NVIDIA NVENC, or discovered software fallback) converts staged videos while remote SSH workers simultaneously pull queued videos, encode them on their own GPUs/CPUs, and send the results back. If Attempt 1 hits a tricky file (like interlaced video or A/V drift), **both local and remote workers** automatically retry once (**Attempt 2**) with deinterlacing (`bwdif`) and clock-synced audio resampling (remote workers reuse the already-uploaded input file with zero extra upload).
3. **Verify (with 2nd-Pass Demux Checks) & Clean Up:** Every finished output is checked against the original's duration and audio/video sync. If a container header looks suspicious (such as `.wmv`/`.asf` files with asymmetric video/audio track lengths or buggy 32-bit MP4 headers whose duration wrapped every 7 minutes), a fast 2nd-pass packet-span check verifies against the source's true packet timeline before rejecting. Once a whole folder finishes cleanly, the original folder is removed. If a folder had 1 or more corrupt files, every verified converted source file is still cleaned up while the corrupt file(s) remain in the source folder and are recorded in `broken_files.journal` and your `FAILED_DIRS_PATH` journal for easy manual inspection.

### Deep-Dive Architecture Documentation

Want the full technical blueprint, sequence diagrams, and state machines? Explore [`docs/`](docs/):

- [**Software Architecture & Detailed Design**](docs/SOFTWARE_ARCHITECTURE_AND_DESIGN.md) — Complete system design, threading model, and journal formats.
- [**Diagram 1: Pipeline Topology**](docs/01_pipeline_topology.md) — End-to-end swimlane view of mount isolation, RAMDisk staging, and workers.
- [**Diagram 2: Remote Pipeline Sequence**](docs/02_remote_pipeline_sequence.md) — How SSH/SCP uploads, remote encodes, copy-backs, and error eviction work.
- [**Diagram 3: Media Lifecycle State Machine**](docs/03_media_lifecycle_state_machine.md) — Every state a video file passes through from discovery to cleanup.
- [**Diagram 4: Concurrency Timeline Gantt**](docs/04_concurrency_timeline_gantt.md) — How local and remote threads overlap without I/O contention.
- [**Diagram 5: Graceful Shutdown Flow**](docs/05_graceful_shutdown_flow.md) — How `Ctrl+C` safely drains in-flight writes and preserves resume state.
- [**Diagram 6: Annotated TUI Guide**](docs/06_tui_annotated_guide.md) — Visual walkthrough of the interactive terminal dashboard and queue viewer.
- [**Diagram 7: Observability, Telemetry & Loki**](docs/07_observability_telemetry_and_loki.md) — Prometheus metric taxonomy, per-thread wait tracking, and Loki log shipping.

---

## Building from Source

### Prerequisites

- **Rust toolchain** (`rustc` and `cargo` install via [rustup.rs](https://rustup.rs/))
- **Python 3** (used by the optional observability companion agent and test/build scripts)
- **FFmpeg & ffprobe** installed and available in your `PATH` (runtime prerequisite)

> **NOTE on Dependencies:**
> Rusty H.265 Converter uses only lightweight, pure-Rust crate dependencies (defined in [`Cargo.toml`](Cargo.toml)):
> `anyhow`, `clap`, `rayon`, `serde`, `serde_json`, and `walkdir`.
> There are **no external C-library build dependencies** (no OpenSSL, libav, or ncurses dev headers required) — all OS-level calls (`statfs`/`statvfs`/`GetDiskFreeSpaceExW`, thread CPU times, raw terminal control, signals) are implemented directly in [`src/platform.rs`](src/platform.rs) and [`src/telemetry.rs`](src/telemetry.rs).
>
> For **cross-compiling** Linux and Windows binaries from macOS (see [Build & Verification Scripts](#build--verification-scripts) below), you will also need **Zig** (`brew install zig`) and **`cargo-zigbuild`** (`cargo install --locked cargo-zigbuild`).

### Compile the Release Binary

```bash
cd rusty_h265_converter
cargo build --release
```

The compiled executable will be created at:
- **macOS / Linux:** `target/release/rusty_h265_converter`
- **Windows:** `target\release\rusty_h265_converter.exe`

### Running Unit Tests

Run the full unit test suite for your current platform:

```bash
cargo test
```

Tests are organized into three suites:
- **`tests::`** ([`src/tests.rs`](src/tests.rs)) — Cross-platform core unit tests (configuration parsing, filename/codec heuristics, duration & A/V sync checks, RAMDisk partition math, partial-failure cleanup, work-stealing journals, telemetry rendering, and Python metrics agent smoke tests).
- **`windows_tests::`** ([`src/windows_tests.rs`](src/windows_tests.rs)) — Compiled and run automatically on Windows (Win32 `GetLocalTime`, `GetDiskFreeSpaceExW`, drive-root mount parsing, RAMDisk fallback, OpenSSH `ControlMaster` omission, `GetThreadTimes`, process-tree termination, and backslash path cleanup).
- **`linux_tests::`** ([`src/linux_tests.rs`](src/linux_tests.rs)) — Compiled and run automatically on Linux / Raspberry Pi (`statvfs`, `/sys/block` drive detection, `CLOCK_THREAD_CPUTIME_ID`, `/dev/shm` staging, POSIX process-group signals & priority, ARM64 CPU fallback, and `/proc` metrics).

To run a specific suite or test by name:
```bash
cargo test tests::
cargo test windows_tests::   # on Windows
cargo test linux_tests::     # on Linux
```

---

## Build & Verification Scripts

The [`scripts/`](scripts/) folder provides two helper scripts to cross-compile executables for every supported OS/CPU from a single macOS machine and run automated remote verification without installing Rust on target machines.

### 1. Cross-Compiling Binaries (`scripts/build_binaries.sh`)

One-time cross-compilation setup on macOS:
```bash
brew install zig
cargo install --locked cargo-zigbuild
rustup target add \
  x86_64-apple-darwin \
  x86_64-unknown-linux-gnu \
  aarch64-unknown-linux-gnu \
  x86_64-pc-windows-gnu \
  aarch64-pc-windows-gnullvm
```

Build a single platform/architecture or all 6 at once:
```bash
# Build one target
./scripts/build_binaries.sh macos arm64
./scripts/build_binaries.sh linux arm64
./scripts/build_binaries.sh windows x86_64
./scripts/build_binaries.sh windows arm64

# Also emit a standalone unit-test runner binary into dist/
./scripts/build_binaries.sh windows x86_64 --with-tests
./scripts/build_binaries.sh linux arm64 --with-tests

# Build every supported platform and architecture
./scripts/build_binaries.sh all
./scripts/build_binaries.sh all --with-tests
```

All outputs are placed in `dist/` alongside an updated `dist/SHA256SUMS.txt`:

| Command Arguments | Rust Target | Output Binary |
| :--- | :--- | :--- |
| `macos arm64` | `aarch64-apple-darwin` | `dist/rusty_h265_converter-macos-arm64` |
| `macos x86_64` | `x86_64-apple-darwin` | `dist/rusty_h265_converter-macos-x86_64` |
| `linux x86_64` | `x86_64-unknown-linux-gnu.2.31` | `dist/rusty_h265_converter-linux-x86_64` |
| `linux arm64` | `aarch64-unknown-linux-gnu.2.31` | `dist/rusty_h265_converter-linux-arm64` |
| `windows x86_64` | `x86_64-pc-windows-gnu` | `dist/rusty_h265_converter-windows-x86_64.exe` |
| `windows arm64` | `aarch64-pc-windows-gnullvm` | `dist/rusty_h265_converter-windows-arm64.exe` |

### 2. Local & Remote Platform Verification (`scripts/verify.sh`)

`scripts/verify.sh` builds both the release binary and the standalone unit-test binary, deploys them (along with `config.yaml`, `logging.conf`, and `scripts/rusty_h265_metrics_agent.py`) to the target host over SCP/SSH, runs a `--help` smoke check, executes the base unit tests (`tests::`), and executes the platform-specific test suite (`windows_tests::` or `linux_tests::`). **No Rust installation is required on the remote Windows or Linux machine.**

```bash
# Verify locally on this Apple Silicon Mac
./scripts/verify.sh macos arm64

# Verify remotely on a Raspberry Pi (Linux ARM64)
./scripts/verify.sh linux arm64 pi@192.168.1.150

# Verify remotely on a Windows x64 PC (custom SSH port optional as 4th arg)
./scripts/verify.sh windows x86_64 user@192.168.1.120 22
```

You can also store default remote hosts in environment variables (`VERIFY_LINUX_ARM64_HOST`, `VERIFY_LINUX_X86_64_HOST`, `VERIFY_WINDOWS_X86_64_HOST`, `VERIFY_WINDOWS_ARM64_HOST`) and omit the `<user@host>` argument.

---

## Credits

Rusty H.265 Converter stands on the shoulders of an incredible open-source ecosystem:

- [**FFmpeg**](https://ffmpeg.org/) — The powerhouse multimedia framework handling decoding, hardware-accelerated H.265 encoding (`hevc_videotoolbox`, `hevc_nvenc`, `libx265`), audio resampling, and media probing (`ffprobe`).
- [**Rust**](https://www.rust-lang.org/) & the Rust crate community — [`clap`](https://crates.io/crates/clap), [`rayon`](https://crates.io/crates/rayon), [`serde`](https://crates.io/crates/serde) / [`serde_json`](https://crates.io/crates/serde_json), [`anyhow`](https://crates.io/crates/anyhow), and [`walkdir`](https://crates.io/crates/walkdir).
- [**Zig**](https://ziglang.org/) & [**cargo-zigbuild**](https://github.com/rust-cross/cargo-zigbuild) — Making seamless cross-compilation across macOS, Linux, and Windows possible from a single machine.
- [**Prometheus**](https://prometheus.io/), [**Grafana**](https://grafana.com/), and [**Grafana Loki**](https://grafana.com/oss/loki/) — Open-source metrics, visualization, and log aggregation powering the observability stack.
- **Hardware Acceleration Teams** — Apple VideoToolbox & AudioToolbox (`hevc_videotoolbox` / `aac_at`) and NVIDIA NVENC/NVML (`hevc_nvenc`).
