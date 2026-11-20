//! Configurable target video/audio codec & container registry, Layer 1 static validator,
//! Layer 2 discovery-first FFmpeg pre-flight probe (5s 256x144 synthetic encode + full-stream decode),
//! and zero-dependency pure-ASCII waiting animation.

use anyhow::{anyhow, bail, Result};
use std::collections::HashSet;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::{get_video_probe_info, log_event, AppConfig, LogLevel, VideoProbeInfo};

pub const MDN_VIDEO_CODECS_URL: &str =
    "https://developer.mozilla.org/en-US/docs/Web/Media/Guides/Formats/Video_codecs";

pub const PREFLIGHT_CLIP_WIDTH: u32 = 256;
pub const PREFLIGHT_CLIP_HEIGHT: u32 = 144;
pub const PREFLIGHT_CLIP_DURATION_SECS: u32 = 5;
pub const PREFLIGHT_CLIP_FPS: u32 = 10;

static PREFLIGHT_SEQ: AtomicUsize = AtomicUsize::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetVideoCodec {
    Hevc,
    H264,
    Av1,
    Vp9,
    Vp8,
    ProRes,
    Webp,
    Gif,
}

impl TargetVideoCodec {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_lowercase().as_str() {
            "hevc" | "h265" | "h.265" | "x265" => Ok(Self::Hevc),
            "h264" | "avc" | "h.264" | "x264" => Ok(Self::H264),
            "av1" | "av01" => Ok(Self::Av1),
            "vp9" => Ok(Self::Vp9),
            "vp8" => Ok(Self::Vp8),
            "prores" => Ok(Self::ProRes),
            "webp" => Ok(Self::Webp),
            "gif" => Ok(Self::Gif),
            other => bail!(
                "Unsupported TARGET_VIDEO_CODEC '{}'. Supported values: hevc, h264, av1, vp9, vp8, prores, webp, gif. \
                 Refer to the compatibility table in README.md or MDN Video Codecs guide ({}).",
                other,
                MDN_VIDEO_CODECS_URL
            ),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Hevc => "hevc",
            Self::H264 => "h264",
            Self::Av1 => "av1",
            Self::Vp9 => "vp9",
            Self::Vp8 => "vp8",
            Self::ProRes => "prores",
            Self::Webp => "webp",
            Self::Gif => "gif",
        }
    }

    pub fn is_animated_image(&self) -> bool {
        matches!(self, Self::Webp | Self::Gif)
    }

    pub fn matches_ffprobe_codec(&self, probe_codec: &str) -> bool {
        let c = probe_codec.trim().to_lowercase();
        match self {
            Self::Hevc => matches!(c.as_str(), "hevc" | "h265"),
            Self::H264 => matches!(c.as_str(), "h264" | "avc" | "avc1"),
            Self::Av1 => matches!(c.as_str(), "av1" | "av01"),
            Self::Vp9 => c == "vp9",
            Self::Vp8 => c == "vp8",
            Self::ProRes => c.starts_with("prores"),
            Self::Webp => c == "webp",
            Self::Gif => c == "gif",
        }
    }

    pub fn matches_ffmpeg_encoder(&self, encoder_name: &str) -> bool {
        let e = encoder_name.trim().to_lowercase();
        match self {
            Self::Hevc => matches!(
                e.as_str(),
                "hevc_videotoolbox"
                    | "hevc_nvenc"
                    | "hevc_qsv"
                    | "hevc_amf"
                    | "hevc_vaapi"
                    | "hevc_mf"
                    | "hevc_v4l2m2m"
                    | "hevc_omx"
                    | "hevc_mediacodec"
                    | "hevc_d3d12va"
                    | "hevc_vulkan"
                    | "libx265"
                    | "libkvazaar"
            ),
            Self::H264 => matches!(
                e.as_str(),
                "h264_videotoolbox"
                    | "h264_nvenc"
                    | "h264_qsv"
                    | "h264_amf"
                    | "h264_vaapi"
                    | "h264_mf"
                    | "h264_v4l2m2m"
                    | "h264_omx"
                    | "h264_mediacodec"
                    | "h264_vulkan"
                    | "libx264"
                    | "libx264rgb"
                    | "libopenh264"
            ),
            Self::Av1 => matches!(
                e.as_str(),
                "av1_nvenc"
                    | "av1_qsv"
                    | "av1_amf"
                    | "av1_vaapi"
                    | "av1_videotoolbox"
                    | "av1_mf"
                    | "av1_vulkan"
                    | "libsvtav1"
                    | "libaom-av1"
                    | "librav1e"
            ),
            Self::Vp9 => matches!(e.as_str(), "libvpx-vp9" | "vp9_vaapi" | "vp9_qsv"),
            Self::Vp8 => matches!(e.as_str(), "libvpx" | "vp8_vaapi" | "vp8_v4l2m2m"),
            Self::ProRes => matches!(
                e.as_str(),
                "prores_videotoolbox" | "prores_ks" | "prores_aw" | "prores"
            ),
            Self::Webp => matches!(e.as_str(), "libwebp_anim" | "libwebp"),
            Self::Gif => e == "gif",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetAudioCodec {
    Aac,
    Opus,
    Vorbis,
    Mp3,
    Flac,
    Ac3,
    Copy,
    None,
}

impl TargetAudioCodec {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_lowercase().as_str() {
            "aac" => Ok(Self::Aac),
            "opus" => Ok(Self::Opus),
            "vorbis" => Ok(Self::Vorbis),
            "mp3" => Ok(Self::Mp3),
            "flac" => Ok(Self::Flac),
            "ac3" => Ok(Self::Ac3),
            "copy" => Ok(Self::Copy),
            "none" | "strip" | "off" | "disabled" => Ok(Self::None),
            other => bail!(
                "Unsupported TARGET_AUDIO_CODEC '{}'. Supported values: aac, opus, vorbis, mp3, flac, ac3, copy, none. \
                 Refer to the compatibility table in README.md or MDN Video Codecs guide ({}).",
                other,
                MDN_VIDEO_CODECS_URL
            ),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Opus => "opus",
            Self::Vorbis => "vorbis",
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
            Self::Ac3 => "ac3",
            Self::Copy => "copy",
            Self::None => "none",
        }
    }

    pub fn matches_ffmpeg_encoder(&self, encoder_name: &str) -> bool {
        let e = encoder_name.trim().to_lowercase();
        match self {
            Self::Aac => matches!(e.as_str(), "aac" | "aac_at" | "libfdk_aac" | "aac_mf"),
            Self::Opus => matches!(e.as_str(), "libopus" | "opus"),
            Self::Vorbis => matches!(e.as_str(), "libvorbis" | "vorbis"),
            Self::Mp3 => matches!(e.as_str(), "libmp3lame" | "mp3" | "mp3_mf"),
            Self::Flac => e == "flac",
            Self::Ac3 => matches!(e.as_str(), "ac3" | "ac3_fixed" | "ac3_mf"),
            Self::Copy => e == "copy",
            Self::None => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetContainer {
    Mp4,
    Mkv,
    Webm,
    Mov,
    Webp,
    Gif,
}

impl TargetContainer {
    pub fn parse(raw: &str) -> Result<Self> {
        let norm = raw.trim().trim_start_matches('.').to_lowercase();
        match norm.as_str() {
            "mp4" | "m4v" => Ok(Self::Mp4),
            "mkv" | "matroska" => Ok(Self::Mkv),
            "webm" => Ok(Self::Webm),
            "mov" => Ok(Self::Mov),
            "webp" => Ok(Self::Webp),
            "gif" => Ok(Self::Gif),
            other => bail!(
                "Unsupported TARGET_CONTAINER '{}'. Supported values: mp4, mkv, webm, mov, webp, gif. \
                 Refer to the compatibility table in README.md or MDN Video Codecs guide ({}).",
                other,
                MDN_VIDEO_CODECS_URL
            ),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "mkv",
            Self::Webm => "webm",
            Self::Mov => "mov",
            Self::Webp => "webp",
            Self::Gif => "gif",
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            Self::Mp4 => ".mp4",
            Self::Mkv => ".mkv",
            Self::Webm => ".webm",
            Self::Mov => ".mov",
            Self::Webp => ".webp",
            Self::Gif => ".gif",
        }
    }

    pub fn is_audio_less(&self) -> bool {
        matches!(self, Self::Webp | Self::Gif)
    }

    pub fn muxer_args(&self) -> Vec<String> {
        match self {
            Self::Mp4 | Self::Mov => vec!["-movflags".to_string(), "+faststart".to_string()],
            Self::Webp | Self::Gif => vec!["-loop".to_string(), "0".to_string()],
            Self::Mkv | Self::Webm => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedTargetConfig {
    pub video: TargetVideoCodec,
    pub audio: TargetAudioCodec,
    pub container: TargetContainer,
}

impl ValidatedTargetConfig {
    pub fn label(&self) -> String {
        format!(
            "{} + {} -> {}",
            self.video.as_str(),
            self.audio.as_str(),
            self.container.as_str()
        )
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedTargetProfile {
    pub target: ValidatedTargetConfig,
    pub video_encoder_name: String,
    pub audio_encoder_name: String,
    pub display_name: String,
    pub decode_args: Vec<String>,
    pub hwaccel_args: Vec<String>,
    pub encoder_args: Vec<String>,
    pub fallback_video_filter_args: Vec<String>,
    pub audio_args: Vec<String>,
    pub muxer_args: Vec<String>,
}

/// Extracts the value immediately following any of the given FFmpeg flags (e.g., `-c:v`, `-vcodec`).
pub fn extract_codec_flag(args: &[String], flags: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].trim();
        if flags.iter().any(|f| arg.eq_ignore_ascii_case(f)) {
            if let Some(val) = args.get(i + 1) {
                return Some(val.trim().to_string());
            }
        }
        i += 1;
    }
    None
}

/// Checks whether `(video, audio, container)` is a valid pairing in the static compatibility matrix.
pub fn is_target_combination_compatible(
    video: TargetVideoCodec,
    audio: TargetAudioCodec,
    container: TargetContainer,
) -> bool {
    let video_ok = match container {
        TargetContainer::Mp4 => matches!(
            video,
            TargetVideoCodec::Hevc
                | TargetVideoCodec::H264
                | TargetVideoCodec::Av1
                | TargetVideoCodec::Vp9
        ),
        TargetContainer::Mkv => matches!(
            video,
            TargetVideoCodec::Hevc
                | TargetVideoCodec::H264
                | TargetVideoCodec::Av1
                | TargetVideoCodec::Vp9
                | TargetVideoCodec::Vp8
                | TargetVideoCodec::ProRes
        ),
        TargetContainer::Webm => matches!(
            video,
            TargetVideoCodec::Vp9 | TargetVideoCodec::Vp8 | TargetVideoCodec::Av1
        ),
        TargetContainer::Mov => matches!(
            video,
            TargetVideoCodec::Hevc
                | TargetVideoCodec::H264
                | TargetVideoCodec::ProRes
                | TargetVideoCodec::Av1
        ),
        TargetContainer::Webp => video == TargetVideoCodec::Webp,
        TargetContainer::Gif => video == TargetVideoCodec::Gif,
    };

    let audio_ok = match container {
        TargetContainer::Mp4 => matches!(
            audio,
            TargetAudioCodec::Aac
                | TargetAudioCodec::Mp3
                | TargetAudioCodec::Ac3
                | TargetAudioCodec::Opus
                | TargetAudioCodec::Flac
                | TargetAudioCodec::Copy
                | TargetAudioCodec::None
        ),
        TargetContainer::Mkv => matches!(
            audio,
            TargetAudioCodec::Aac
                | TargetAudioCodec::Opus
                | TargetAudioCodec::Vorbis
                | TargetAudioCodec::Mp3
                | TargetAudioCodec::Flac
                | TargetAudioCodec::Ac3
                | TargetAudioCodec::Copy
                | TargetAudioCodec::None
        ),
        TargetContainer::Webm => matches!(
            audio,
            TargetAudioCodec::Opus
                | TargetAudioCodec::Vorbis
                | TargetAudioCodec::Copy
                | TargetAudioCodec::None
        ),
        TargetContainer::Mov => matches!(
            audio,
            TargetAudioCodec::Aac
                | TargetAudioCodec::Mp3
                | TargetAudioCodec::Ac3
                | TargetAudioCodec::Flac
                | TargetAudioCodec::Copy
                | TargetAudioCodec::None
        ),
        TargetContainer::Webp | TargetContainer::Gif => audio == TargetAudioCodec::None,
    };

    video_ok && audio_ok
}

/// Checks whether a source audio stream can be stream-copied (`-c:a copy`) into the target container.
pub fn is_audio_copy_compatible(
    source_audio_codec: &str,
    container: TargetContainer,
    target_audio: TargetAudioCodec,
) -> bool {
    if container.is_audio_less() || target_audio == TargetAudioCodec::None {
        return false;
    }
    let c = source_audio_codec.trim().to_lowercase();
    if c.is_empty() || c == "none" || c == "unknown" {
        return false;
    }
    match container {
        TargetContainer::Mp4 => matches!(
            c.as_str(),
            "aac" | "mp3" | "ac3" | "eac3" | "alac" | "mp2" | "opus" | "flac"
        ),
        TargetContainer::Mkv => true,
        TargetContainer::Webm => matches!(c.as_str(), "opus" | "vorbis"),
        TargetContainer::Mov => matches!(
            c.as_str(),
            "aac" | "alac" | "mp3" | "ac3" | "pcm_s16le" | "pcm_s24le"
        ),
        TargetContainer::Webp | TargetContainer::Gif => false,
    }
}

/// Returns true if the source file already matches the target video codec (and container for image targets),
/// allowing the Verifier to fast-move it without re-encoding.
pub fn is_source_already_target(
    probe: &VideoProbeInfo,
    source_path: &Path,
    target_video: TargetVideoCodec,
    target_container: TargetContainer,
) -> bool {
    if !target_video.matches_ffprobe_codec(&probe.video_codec) {
        return false;
    }
    if target_container.is_audio_less() {
        let ext = source_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e.to_lowercase()))
            .unwrap_or_default();
        return ext == target_container.extension();
    }
    true
}

/// Strips `-hwaccel_output_format <fmt>` whenever the selected output video encoder is not `*_nvenc`,
/// preventing CUDA VRAM surface format errors on CPU encoders (e.g., `libvpx-vp9`, `libsvtav1`, `libwebp_anim`).
pub fn sanitize_hwaccel_args_for_encoder(
    hwaccel_args: &[String],
    video_encoder_name: &str,
) -> Vec<String> {
    if video_encoder_name.to_lowercase().ends_with("_nvenc") {
        return hwaccel_args.to_vec();
    }
    let mut sanitized = Vec::with_capacity(hwaccel_args.len());
    let mut i = 0;
    while i < hwaccel_args.len() {
        if hwaccel_args[i].eq_ignore_ascii_case("-hwaccel_output_format") {
            i += 2; // Skip flag and its value (e.g., "cuda")
            continue;
        }
        sanitized.push(hwaccel_args[i].clone());
        i += 1;
    }
    sanitized
}

/// Adapts fallback deinterlace/pixel-format filter args for containers like `gif` that do not use `yuv420p`.
pub fn sanitize_fallback_filter_args(
    filter_args: &[String],
    video: TargetVideoCodec,
) -> Vec<String> {
    if video != TargetVideoCodec::Gif {
        return filter_args.to_vec();
    }
    let mut out = Vec::with_capacity(filter_args.len());
    for arg in filter_args {
        let cleaned = arg
            .replace(",format=yuv420p", "")
            .replace("format=yuv420p,", "");
        out.push(cleaned);
    }
    out
}

/// Layer 1: Instant static configuration & custom `FFMPEG_*_ARGS` override validation.
pub fn validate_config_targets(config: &AppConfig) -> Result<ValidatedTargetConfig> {
    let raw_video = if config.TARGET_VIDEO_CODEC.trim().is_empty() {
        "hevc"
    } else {
        config.TARGET_VIDEO_CODEC.trim()
    };
    let raw_audio = if config.TARGET_AUDIO_CODEC.trim().is_empty() {
        "aac"
    } else {
        config.TARGET_AUDIO_CODEC.trim()
    };
    let raw_container = if !config.TARGET_CONTAINER.trim().is_empty() {
        config.TARGET_CONTAINER.trim()
    } else if !config.OUTPUT_EXTENSION.trim().is_empty() {
        config.OUTPUT_EXTENSION.trim()
    } else {
        "mp4"
    };

    let video = TargetVideoCodec::parse(raw_video)?;
    let audio = TargetAudioCodec::parse(raw_audio)?;
    let container = TargetContainer::parse(raw_container)?;

    // 1. Check OUTPUT_EXTENSION vs TARGET_CONTAINER in Layer 1
    let raw_ext = config.OUTPUT_EXTENSION.trim();
    if !raw_ext.is_empty() && !config.TARGET_CONTAINER.trim().is_empty() {
        let norm_ext = if raw_ext.starts_with('.') {
            raw_ext.to_lowercase()
        } else {
            format!(".{}", raw_ext.to_lowercase())
        };
        if norm_ext != container.extension() {
            bail!(
                "Configuration error: OUTPUT_EXTENSION ('{}') does not match TARGET_CONTAINER ('{}', expected '{}'). \
                 Please align OUTPUT_EXTENSION with TARGET_CONTAINER in config.yaml.",
                raw_ext,
                container.as_str(),
                container.extension()
            );
        }
    }

    // 2. Check (video, audio, container) compatibility matrix
    if !is_target_combination_compatible(video, audio, container) {
        bail!(
            "Incompatible codec/container configuration: TARGET_VIDEO_CODEC='{}', TARGET_AUDIO_CODEC='{}', TARGET_CONTAINER='{}'. \
             Please consult the Supported Codec & Container Pairings table in README.md or MDN's Video Codecs guide ({}).",
            video.as_str(),
            audio.as_str(),
            container.as_str(),
            MDN_VIDEO_CODECS_URL
        );
    }

    // 3. Check explicit FFMPEG_*_ENCODER_ARGS / FFMPEG_*_AUDIO_ARGS FIRST
    let video_override_blocks: [(&str, &[String]); 4] = [
        ("FFMPEG_MACOS_ENCODER_ARGS", &config.FFMPEG_MACOS_ENCODER_ARGS),
        (
            "FFMPEG_NVIDIA_ENCODER_ARGS",
            &config.FFMPEG_NVIDIA_ENCODER_ARGS,
        ),
        (
            "FFMPEG_GTX_3050_TI_ENCODER_ARGS",
            &config.FFMPEG_GTX_3050_TI_ENCODER_ARGS,
        ),
        ("FFMPEG_CPU_ENCODER_ARGS", &config.FFMPEG_CPU_ENCODER_ARGS),
    ];

    for (key_name, args) in video_override_blocks {
        if args.is_empty() {
            continue;
        }
        if let Some(enc) = extract_codec_flag(args, &["-c:v", "-vcodec", "-codec:v"]) {
            if !video.matches_ffmpeg_encoder(&enc) {
                bail!(
                    "Configuration conflict in {}: custom FFmpeg arguments specify video encoder '-c:v {}', \
                     which conflicts with TARGET_VIDEO_CODEC='{}'. Either update {} to use a valid '{}' encoder \
                     if you know what you are doing, or comment out / remove {} from config.yaml so built-in defaults can be used.",
                    key_name,
                    enc,
                    video.as_str(),
                    key_name,
                    video.as_str(),
                    key_name
                );
            }
        }
    }

    let audio_override_blocks: [(&str, &[String]); 2] = [
        ("FFMPEG_MACOS_AUDIO_ARGS", &config.FFMPEG_MACOS_AUDIO_ARGS),
        ("FFMPEG_AUDIO_ARGS", &config.FFMPEG_AUDIO_ARGS),
    ];

    for (key_name, args) in audio_override_blocks {
        if args.is_empty() {
            continue;
        }
        let has_an = args.iter().any(|a| a.trim().eq_ignore_ascii_case("-an"));
        let audio_enc = extract_codec_flag(args, &["-c:a", "-acodec", "-codec:a"]);
        if audio == TargetAudioCodec::None {
            if !has_an || audio_enc.is_some() {
                bail!(
                    "Configuration conflict in {}: TARGET_AUDIO_CODEC='none' (no audio), but {} specifies custom audio encoding args. \
                     Either update {} to '-an' or comment out / remove {} from config.yaml so built-in defaults can be used.",
                    key_name,
                    key_name,
                    key_name,
                    key_name
                );
            }
        } else if let Some(enc) = audio_enc {
            if audio != TargetAudioCodec::Copy && !audio.matches_ffmpeg_encoder(&enc) {
                bail!(
                    "Configuration conflict in {}: custom FFmpeg arguments specify audio encoder '-c:a {}', \
                     which conflicts with TARGET_AUDIO_CODEC='{}'. Either update {} to use a valid '{}' encoder \
                     if you know what you are doing, or comment out / remove {} from config.yaml so built-in defaults can be used.",
                    key_name,
                    enc,
                    audio.as_str(),
                    key_name,
                    audio.as_str(),
                    key_name
                );
            }
        }
    }

    Ok(ValidatedTargetConfig {
        video,
        audio,
        container,
    })
}

/// Represents a single encoder entry parsed from `ffmpeg -hide_banner -encoders`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredEncoder {
    /// `'V'` for video, `'A'` for audio, `'S'` for subtitle.
    pub kind: char,
    pub name: String,
    pub experimental: bool,
}

/// Parses `ffmpeg -hide_banner -encoders` stdout into structured encoder descriptors.
pub fn parse_ffmpeg_encoders_output(raw_output: &str) -> Vec<DiscoveredEncoder> {
    let mut result = Vec::new();
    let mut past_header = false;
    for line in raw_output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("------") {
            past_header = true;
            continue;
        }
        // Also support test fixtures that omit the `------` header separator
        if !past_header && !(trimmed.starts_with("V.") || trimmed.starts_with("A.") || trimmed.starts_with("VF") || trimmed.starts_with("AF") || trimmed.starts_with("VS") || trimmed.starts_with("AS")) {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let flags = match parts.next() {
            Some(f) if f.len() == 6 => f,
            _ => continue,
        };
        let name = match parts.next() {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };
        let bytes = flags.as_bytes();
        let kind = bytes[0] as char;
        if kind != 'V' && kind != 'A' {
            continue;
        }
        // FFmpeg encoder flag positions:
        // [0] V/A/S, [1] F/., [2] S/., [3] X/. (experimental), [4] B/., [5] D/.
        let experimental = bytes[3] == b'X';
        result.push(DiscoveredEncoder {
            kind,
            name: name.to_string(),
            experimental,
        });
    }
    result
}

/// Built-in default FFmpeg video encoder arguments for a discovered encoder.
pub fn default_video_encoder_args(
    encoder_name: &str,
    _video: TargetVideoCodec,
    container: TargetContainer,
) -> Vec<String> {
    let e = encoder_name.to_lowercase();
    let vfr_prefix = [
        "-fps_mode",
        "vfr",
        "-avoid_negative_ts",
        "make_zero",
        "-max_interleave_delta",
        "0",
    ];
    let needs_hvc1_tag = matches!(container, TargetContainer::Mp4 | TargetContainer::Mov);

    let mut args: Vec<String> = match e.as_str() {
        // --- HEVC ---
        "hevc_videotoolbox" => {
            let mut a: Vec<&str> = vfr_prefix.to_vec();
            a.extend_from_slice(&["-c:v", "hevc_videotoolbox", "-q:v", "50", "-prio_speed", "1"]);
            if needs_hvc1_tag {
                a.extend_from_slice(&["-tag:v", "hvc1"]);
            }
            a.into_iter().map(String::from).collect()
        }
        "hevc_nvenc" => {
            let mut a: Vec<&str> = vfr_prefix.to_vec();
            a.extend_from_slice(&["-c:v", "hevc_nvenc", "-preset", "p4", "-cq", "28"]);
            if needs_hvc1_tag {
                a.extend_from_slice(&["-tag:v", "hvc1"]);
            }
            a.into_iter().map(String::from).collect()
        }
        "hevc_qsv" | "hevc_amf" | "hevc_vaapi" | "hevc_mf" => {
            let mut a: Vec<String> = vfr_prefix.iter().map(|s| s.to_string()).collect();
            a.extend(["-c:v".to_string(), encoder_name.to_string()]);
            if needs_hvc1_tag {
                a.extend(["-tag:v".to_string(), "hvc1".to_string()]);
            }
            a
        }
        "libx265" => {
            let mut a: Vec<&str> = vfr_prefix.to_vec();
            a.extend_from_slice(&["-c:v", "libx265", "-crf", "28", "-preset", "ultrafast"]);
            if needs_hvc1_tag {
                a.extend_from_slice(&["-tag:v", "hvc1"]);
            }
            a.into_iter().map(String::from).collect()
        }
        // --- H.264 ---
        "h264_videotoolbox" => {
            let mut a: Vec<&str> = vfr_prefix.to_vec();
            a.extend_from_slice(&["-c:v", "h264_videotoolbox", "-q:v", "65", "-prio_speed", "1"]);
            a.into_iter().map(String::from).collect()
        }
        "h264_nvenc" => {
            let mut a: Vec<&str> = vfr_prefix.to_vec();
            a.extend_from_slice(&["-c:v", "h264_nvenc", "-preset", "p4", "-cq", "23"]);
            a.into_iter().map(String::from).collect()
        }
        "libx264" => {
            let mut a: Vec<&str> = vfr_prefix.to_vec();
            a.extend_from_slice(&["-c:v", "libx264", "-crf", "23", "-preset", "fast"]);
            a.into_iter().map(String::from).collect()
        }
        // --- AV1 ---
        "av1_nvenc" => {
            let mut a: Vec<&str> = vfr_prefix.to_vec();
            a.extend_from_slice(&["-c:v", "av1_nvenc", "-preset", "p4", "-cq", "32"]);
            a.into_iter().map(String::from).collect()
        }
        "libsvtav1" => {
            let mut a: Vec<&str> = vfr_prefix.to_vec();
            a.extend_from_slice(&["-c:v", "libsvtav1", "-crf", "32", "-preset", "6"]);
            a.into_iter().map(String::from).collect()
        }
        "libaom-av1" => {
            let mut a: Vec<&str> = vfr_prefix.to_vec();
            a.extend_from_slice(&[
                "-c:v",
                "libaom-av1",
                "-crf",
                "34",
                "-b:v",
                "0",
                "-cpu-used",
                "6",
                "-row-mt",
                "1",
            ]);
            a.into_iter().map(String::from).collect()
        }
        // --- VP9 / VP8 ---
        "libvpx-vp9" => {
            let mut a: Vec<&str> = vfr_prefix.to_vec();
            a.extend_from_slice(&[
                "-c:v",
                "libvpx-vp9",
                "-b:v",
                "0",
                "-crf",
                "32",
                "-deadline",
                "good",
                "-cpu-used",
                "2",
                "-row-mt",
                "1",
            ]);
            a.into_iter().map(String::from).collect()
        }
        "libvpx" => {
            let mut a: Vec<&str> = vfr_prefix.to_vec();
            a.extend_from_slice(&["-c:v", "libvpx", "-b:v", "1500k", "-crf", "10"]);
            a.into_iter().map(String::from).collect()
        }
        // --- ProRes ---
        "prores_videotoolbox" | "prores_ks" | "prores_aw" | "prores" => {
            let mut a: Vec<String> = vfr_prefix.iter().map(|s| s.to_string()).collect();
            a.extend([
                "-c:v".to_string(),
                encoder_name.to_string(),
                "-profile:v".to_string(),
                "2".to_string(),
            ]);
            a
        }
        // --- Animated WebP ---
        "libwebp_anim" | "libwebp" => vec![
            "-c:v".to_string(),
            encoder_name.to_string(),
            "-lossless".to_string(),
            "0".to_string(),
            "-compression_level".to_string(),
            "4".to_string(),
            "-q:v".to_string(),
            "75".to_string(),
            "-vf".to_string(),
            "fps=15,scale=min(1280\\,iw):-2".to_string(),
        ],
        // --- Animated GIF ---
        "gif" => vec![
            "-c:v".to_string(),
            "gif".to_string(),
            "-vf".to_string(),
            "fps=12,scale=min(720\\,iw):-2".to_string(),
        ],
        // --- Generic fallback for any other discovered encoder (e.g., *_qsv, *_amf, *_vaapi) ---
        _ => {
            let mut a: Vec<String> = vfr_prefix.iter().map(|s| s.to_string()).collect();
            a.extend(["-c:v".to_string(), encoder_name.to_string()]);
            a
        }
    };

    if args.is_empty() {
        args = vec!["-c:v".to_string(), encoder_name.to_string()];
    }
    args
}

/// Built-in default FFmpeg audio encoder arguments for a discovered audio encoder.
pub fn default_audio_encoder_args(
    encoder_name: &str,
    audio: TargetAudioCodec,
    experimental: bool,
) -> Vec<String> {
    match audio {
        TargetAudioCodec::None => return vec!["-an".to_string()],
        TargetAudioCodec::Copy => return vec!["-c:a".to_string(), "copy".to_string()],
        _ => {}
    }

    let mut args = vec![
        "-af".to_string(),
        "aresample=async=1:first_pts=0".to_string(),
        "-c:a".to_string(),
        encoder_name.to_string(),
    ];

    match audio {
        TargetAudioCodec::Aac | TargetAudioCodec::Mp3 => {
            args.extend(["-b:a".to_string(), "192k".to_string()]);
        }
        TargetAudioCodec::Opus => {
            args.extend(["-b:a".to_string(), "128k".to_string()]);
        }
        TargetAudioCodec::Vorbis => {
            args.extend(["-q:a".to_string(), "4".to_string()]);
        }
        TargetAudioCodec::Ac3 => {
            args.extend(["-b:a".to_string(), "384k".to_string()]);
        }
        TargetAudioCodec::Flac | TargetAudioCodec::Copy | TargetAudioCodec::None => {}
    }

    if experimental {
        args.extend(["-strict".to_string(), "-2".to_string()]);
    }
    args
}

fn video_encoder_rank(encoder_name: &str, os_lower: &str, experimental: bool) -> u32 {
    let e = encoder_name.to_lowercase();
    let exp_penalty = if experimental { 1000 } else { 0 };
    let is_mac = os_lower.contains("mac") || os_lower.contains("darwin");
    let is_win = os_lower.contains("win");

    let base = if is_mac && e.ends_with("_videotoolbox") {
        10
    } else if e.ends_with("_nvenc") {
        if is_mac { 90 } else { 15 }
    } else if e.ends_with("_qsv") {
        25
    } else if e.ends_with("_amf") {
        if is_win { 28 } else { 35 }
    } else if e.ends_with("_vaapi") || e.ends_with("_d3d12va") || e.ends_with("_mf") {
        38
    } else if e.ends_with("_videotoolbox") {
        95
    } else {
        match e.as_str() {
            "libx265" | "libx264" | "libsvtav1" | "libvpx-vp9" | "libvpx" | "prores_ks"
            | "libwebp_anim" | "gif" => 50,
            "prores_aw" | "prores" | "libopenh264" => 65,
            "libaom-av1" | "librav1e" | "libwebp" => 80,
            _ => 75,
        }
    };
    base + exp_penalty
}

fn audio_encoder_rank(encoder_name: &str, os_lower: &str, experimental: bool) -> u32 {
    let e = encoder_name.to_lowercase();
    let exp_penalty = if experimental { 1000 } else { 0 };
    let is_mac = os_lower.contains("mac") || os_lower.contains("darwin");

    let base = match e.as_str() {
        "aac_at" => if is_mac { 10 } else { 90 },
        "libfdk_aac" => 20,
        "aac" => 30,
        "libopus" => 15,
        "libvorbis" => 15,
        "libmp3lame" => 15,
        "flac" => 15,
        "ac3" => 15,
        "ac3_fixed" => 25,
        "opus" | "vorbis" | "mp3" => 80,
        _ => 60,
    };
    base + exp_penalty
}

/// Discovery-first candidate builder for video encoders on a host (local or remote).
/// Checks explicit user `FFMPEG_*_ENCODER_ARGS` overrides first, then ranks actually-discovered
/// encoders from `ffmpeg -hide_banner -encoders`.
pub fn rank_discovered_video_candidates(
    encoders_output: &str,
    os_type: &str,
    gpu_profile: &str,
    target: &ValidatedTargetConfig,
    config: &AppConfig,
) -> Vec<(String, Vec<String>)> {
    let os_lower = os_type.to_lowercase();
    let is_mac = os_lower.contains("mac") || os_lower.contains("darwin");
    let is_win = os_lower.contains("win");
    let use_3050_ti = gpu_profile.to_lowercase().contains("3050")
        || (is_win && !config.FFMPEG_GTX_3050_TI_ENCODER_ARGS.is_empty());

    let mut candidates: Vec<(String, Vec<String>)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // 1. Prioritize matching custom override blocks configured in config.yaml
    let mut custom_blocks: Vec<&[String]> = Vec::new();
    if is_mac && !config.FFMPEG_MACOS_ENCODER_ARGS.is_empty() {
        custom_blocks.push(&config.FFMPEG_MACOS_ENCODER_ARGS);
    }
    if !is_mac {
        if use_3050_ti && !config.FFMPEG_GTX_3050_TI_ENCODER_ARGS.is_empty() {
            custom_blocks.push(&config.FFMPEG_GTX_3050_TI_ENCODER_ARGS);
        }
        if !config.FFMPEG_NVIDIA_ENCODER_ARGS.is_empty() {
            custom_blocks.push(&config.FFMPEG_NVIDIA_ENCODER_ARGS);
        }
    }
    if !config.FFMPEG_CPU_ENCODER_ARGS.is_empty() {
        custom_blocks.push(&config.FFMPEG_CPU_ENCODER_ARGS);
    }

    for block in custom_blocks {
        if let Some(enc_name) = extract_codec_flag(block, &["-c:v", "-vcodec", "-codec:v"]) {
            if target.video.matches_ffmpeg_encoder(&enc_name) && seen.insert(enc_name.clone()) {
                candidates.push((enc_name, block.to_vec()));
            }
        }
    }

    // 2. Discover actually available encoders from `ffmpeg -hide_banner -encoders`
    let discovered = parse_ffmpeg_encoders_output(encoders_output);
    let mut matching: Vec<DiscoveredEncoder> = discovered
        .into_iter()
        .filter(|d| d.kind == 'V' && target.video.matches_ffmpeg_encoder(&d.name))
        .collect();

    // Skip experimental (X) encoders whenever at least one production encoder exists
    let has_non_experimental = matching.iter().any(|d| !d.experimental);
    if has_non_experimental {
        matching.retain(|d| !d.experimental);
    }

    matching.sort_by_key(|d| {
        (
            video_encoder_rank(&d.name, &os_lower, d.experimental),
            d.name.clone(),
        )
    });

    for d in matching {
        if seen.insert(d.name.clone()) {
            let args = default_video_encoder_args(&d.name, target.video, target.container);
            candidates.push((d.name, args));
        }
    }

    candidates
}

/// Discovery-first candidate builder for audio encoders on a host (local or remote).
pub fn rank_discovered_audio_candidates(
    encoders_output: &str,
    os_type: &str,
    target: &ValidatedTargetConfig,
    config: &AppConfig,
) -> Vec<(String, Vec<String>)> {
    if target.audio == TargetAudioCodec::None {
        return vec![("none".to_string(), vec!["-an".to_string()])];
    }
    if target.audio == TargetAudioCodec::Copy {
        return vec![(
            "copy".to_string(),
            vec!["-c:a".to_string(), "copy".to_string()],
        )];
    }

    let os_lower = os_type.to_lowercase();
    let is_mac = os_lower.contains("mac") || os_lower.contains("darwin");
    let mut candidates: Vec<(String, Vec<String>)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // 1. Prioritize explicit custom audio args if provided in config.yaml
    if is_mac && !config.FFMPEG_MACOS_AUDIO_ARGS.is_empty() {
        if let Some(enc) =
            extract_codec_flag(&config.FFMPEG_MACOS_AUDIO_ARGS, &["-c:a", "-acodec", "-codec:a"])
        {
            if target.audio.matches_ffmpeg_encoder(&enc) && seen.insert(enc.clone()) {
                candidates.push((enc, config.FFMPEG_MACOS_AUDIO_ARGS.clone()));
            }
        }
    }
    if !config.FFMPEG_AUDIO_ARGS.is_empty() {
        if let Some(enc) =
            extract_codec_flag(&config.FFMPEG_AUDIO_ARGS, &["-c:a", "-acodec", "-codec:a"])
        {
            if target.audio.matches_ffmpeg_encoder(&enc) && seen.insert(enc.clone()) {
                candidates.push((enc, config.FFMPEG_AUDIO_ARGS.clone()));
            }
        }
    }

    // 2. Discover actually available audio encoders from `ffmpeg -hide_banner -encoders`
    let discovered = parse_ffmpeg_encoders_output(encoders_output);
    let mut matching: Vec<DiscoveredEncoder> = discovered
        .into_iter()
        .filter(|d| d.kind == 'A' && target.audio.matches_ffmpeg_encoder(&d.name))
        .collect();

    let has_non_experimental = matching.iter().any(|d| !d.experimental);
    if has_non_experimental {
        matching.retain(|d| !d.experimental);
    }

    matching.sort_by_key(|d| {
        (
            audio_encoder_rank(&d.name, &os_lower, d.experimental),
            d.name.clone(),
        )
    });

    for d in matching {
        if seen.insert(d.name.clone()) {
            let args = default_audio_encoder_args(&d.name, target.audio, d.experimental);
            candidates.push((d.name, args));
        }
    }

    candidates
}

/// Pure-ASCII waiting animation for pre-flight encoder discovery & 5-second probes.
pub struct DiscoverySpinner {
    stop_flag: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    handle: Option<thread::JoinHandle<()>>,
    is_tty: bool,
    verbose: bool,
    silent: bool,
}

impl DiscoverySpinner {
    pub fn start(target_label: &str, verbose: bool) -> Self {
        let is_tty = io::stderr().is_terminal() && !verbose;
        let stop_flag = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new("Querying local FFmpeg encoders...".to_string()));

        eprintln!(
            "Discovering and verifying compatible encoders for [{}]...",
            target_label
        );
        eprintln!("This may take up to a couple of minutes depending on your hardware.");

        let handle = if is_tty {
            let stop_ref = Arc::clone(&stop_flag);
            let status_ref = Arc::clone(&status);
            Some(thread::spawn(move || {
                const SPIN: [char; 4] = ['|', '/', '-', '\\'];
                const BAR_WIDTH: usize = 10;
                let start = Instant::now();
                let mut tick: usize = 0;

                while !stop_ref.load(Ordering::Relaxed) {
                    let spin_ch = SPIN[tick % SPIN.len()];
                    let cycle = (BAR_WIDTH - 3) * 2;
                    let phase = tick % cycle.max(1);
                    let pos = if phase < BAR_WIDTH - 3 {
                        phase
                    } else {
                        cycle - phase
                    };
                    let mut bar = String::with_capacity(BAR_WIDTH);
                    for i in 0..BAR_WIDTH {
                        if i >= pos && i < pos + 3 {
                            bar.push('=');
                        } else {
                            bar.push(' ');
                        }
                    }
                    let elapsed = start.elapsed().as_secs();
                    let cur_status = status_ref
                        .lock()
                        .map(|s| s.clone())
                        .unwrap_or_default();
                    let _ = write!(
                        io::stderr(),
                        "\r\x1b[2K[{}] [{}]  {} ({:02}s)",
                        spin_ch,
                        bar,
                        cur_status,
                        elapsed
                    );
                    let _ = io::stderr().flush();
                    tick = tick.wrapping_add(1);
                    thread::sleep(Duration::from_millis(120));
                }
            }))
        } else {
            None
        };

        Self {
            stop_flag,
            status,
            handle,
            is_tty,
            verbose,
            silent: false,
        }
    }

    pub fn silent() -> Self {
        Self {
            stop_flag: Arc::new(AtomicBool::new(true)),
            status: Arc::new(Mutex::new(String::new())),
            handle: None,
            is_tty: false,
            verbose: false,
            silent: true,
        }
    }

    pub fn set_status(&self, msg: impl Into<String>) {
        let s = msg.into();
        if let Ok(mut guard) = self.status.lock() {
            *guard = s.clone();
        }
        if !self.silent && (!self.is_tty || self.verbose) {
            eprintln!("  -> {}", s);
        }
    }

    fn stop_thread(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if self.is_tty && !self.silent {
            let _ = write!(io::stderr(), "\r\x1b[2K");
            let _ = io::stderr().flush();
        }
    }

    pub fn finish_ok(&mut self, summary: &str) {
        self.stop_thread();
        if !self.silent {
            eprintln!("[OK] {}", summary);
        }
    }

    pub fn finish_err(&mut self, summary: &str) {
        self.stop_thread();
        if !self.silent {
            eprintln!("[ERROR] {}", summary);
        }
    }
}

impl Drop for DiscoverySpinner {
    fn drop(&mut self) {
        self.stop_thread();
    }
}

/// Runs a strict full-stream decode verification (`ffmpeg -nostdin -xerror -err_detect explode -v error -i <file> -f null -`)
/// ensuring every video and audio frame decodes cleanly without fatal decode or container errors.
pub fn verify_full_stream_decodable(filepath: &Path) -> std::result::Result<(), String> {
    let out = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-xerror",
            "-err_detect",
            "explode",
            "-v",
            "error",
            "-i",
        ])
        .arg(filepath)
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to spawn ffmpeg decode verification: {}", e))?;

    if !out.status.success() {
        let stderr_msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let detail = if !stderr_msg.is_empty() {
            stderr_msg
        } else {
            format!("ffmpeg full-stream decode exited with {}", out.status)
        };
        return Err(detail);
    }
    Ok(())
}

/// Runs a 5-second 256x144 synthetic encode into a real container file, then validates
/// non-zero file size, `ffprobe` codec inspection, and full-stream decode via `ffmpeg -v error -i ... -f null -`.
fn run_local_synthetic_preflight(
    target: &ValidatedTargetConfig,
    video_args: &[String],
    audio_args: &[String],
    muxer_args: &[String],
) -> std::result::Result<(), String> {
    let seq = PREFLIGHT_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp_probe = std::env::temp_dir().join(format!(
        "rusty_h265_preflight_{}_{}{}",
        std::process::id(),
        seq,
        target.container.extension()
    ));
    let _ = fs::remove_file(&temp_probe);

    let video_src = format!(
        "color=c=black:s={}x{}:d={}:r={}",
        PREFLIGHT_CLIP_WIDTH,
        PREFLIGHT_CLIP_HEIGHT,
        PREFLIGHT_CLIP_DURATION_SECS,
        PREFLIGHT_CLIP_FPS
    );

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", &video_src]);

    if target.audio != TargetAudioCodec::None {
        let audio_src = format!(
            "anullsrc=r=48000:cl=stereo:d={}",
            PREFLIGHT_CLIP_DURATION_SECS
        );
        cmd.args(["-f", "lavfi", "-i", &audio_src]);
    }

    cmd.args(video_args);
    if target.audio == TargetAudioCodec::Copy {
        // Synthetic lavfi PCM audio cannot always be stream-copied into mp4/webm;
        // test the container's default native audio codec during pre-flight when target_audio=copy.
        let copy_probe_audio = match target.container {
            TargetContainer::Webm => vec!["-c:a", "libopus", "-b:a", "64k"],
            _ => vec!["-c:a", "aac", "-b:a", "64k"],
        };
        cmd.args(copy_probe_audio);
    } else {
        cmd.args(audio_args);
    }
    cmd.args(muxer_args);
    cmd.arg("-y").arg(&temp_probe);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let output = cmd
        .output()
        .map_err(|e| format!("Failed to spawn local ffmpeg pre-flight: {}", e))?;

    if !output.status.success() {
        let _ = fs::remove_file(&temp_probe);
        let stderr_msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr_msg.is_empty() {
            format!("ffmpeg pre-flight encode exited with {}", output.status)
        } else {
            stderr_msg
        });
    }

    let size = fs::metadata(&temp_probe).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        let _ = fs::remove_file(&temp_probe);
        return Err("Pre-flight encode produced an empty (0-byte) file".to_string());
    }

    let probe = get_video_probe_info(&temp_probe);
    if probe.video_codec == "unknown" || !target.video.matches_ffprobe_codec(&probe.video_codec) {
        let _ = fs::remove_file(&temp_probe);
        return Err(format!(
            "ffprobe reported unexpected video codec '{}' (expected '{}')",
            probe.video_codec,
            target.video.as_str()
        ));
    }

    if let Err(dec_err) = verify_full_stream_decodable(&temp_probe) {
        let _ = fs::remove_file(&temp_probe);
        return Err(format!("Full-stream decode verification failed: {}", dec_err));
    }

    let _ = fs::remove_file(&temp_probe);
    Ok(())
}

/// Queries local `ffmpeg -hide_banner -encoders` and returns its output.
pub fn query_local_ffmpeg_encoders() -> Result<String> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            anyhow!(
                "Failed to execute 'ffmpeg -hide_banner -encoders': {}. Is FFmpeg installed and in PATH?",
                e
            )
        })?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stdout.trim().is_empty() && stderr.trim().is_empty() {
        bail!("'ffmpeg -hide_banner -encoders' returned empty output.");
    }
    Ok(format!("{}\n{}", stdout, stderr))
}

/// Layer 2: Discovery-first local FFmpeg pre-flight validation.
/// Returns a verified `ResolvedTargetProfile` ready for encoding.
pub fn discover_and_verify_local_profile(
    config: &AppConfig,
    target: &ValidatedTargetConfig,
    spinner: &DiscoverySpinner,
) -> Result<ResolvedTargetProfile> {
    spinner.set_status("Querying local ffmpeg -encoders...");
    let encoders_out = query_local_ffmpeg_encoders()?;
    let local_os = std::env::consts::OS;

    let video_candidates =
        rank_discovered_video_candidates(&encoders_out, local_os, "", target, config);
    if video_candidates.is_empty() {
        bail!(
            "Local FFmpeg does not have any available encoder for TARGET_VIDEO_CODEC='{}'. \
             Run `ffmpeg -encoders` to inspect supported codecs on this machine.",
            target.video.as_str()
        );
    }

    let audio_candidates =
        rank_discovered_audio_candidates(&encoders_out, local_os, target, config);
    if audio_candidates.is_empty() {
        bail!(
            "Local FFmpeg does not have any available encoder for TARGET_AUDIO_CODEC='{}'. \
             Run `ffmpeg -encoders` to inspect supported audio codecs on this machine.",
            target.audio.as_str()
        );
    }

    let muxer_args = target.container.muxer_args();
    let mut failure_notes: Vec<String> = Vec::new();

    // Test each discovered video candidate paired with the best working audio candidate
    for (v_name, v_args) in &video_candidates {
        for (a_name, a_args) in &audio_candidates {
            spinner.set_status(format!(
                "Testing local: {} + {} (5s {}x{} encode + decode check)...",
                v_name, a_name, PREFLIGHT_CLIP_WIDTH, PREFLIGHT_CLIP_HEIGHT
            ));
            match run_local_synthetic_preflight(target, v_args, a_args, &muxer_args) {
                Ok(()) => {
                    let raw_hwaccel = if local_os == "macos" {
                        config.FFMPEG_MACOS_HWACCEL_ARGS.clone()
                    } else if (local_os == "linux" || local_os == "windows")
                        && Command::new("nvidia-smi")
                            .arg("-L")
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status()
                            .map(|s| s.success())
                            .unwrap_or(false)
                    {
                        config.FFMPEG_NVIDIA_HWACCEL_ARGS.clone()
                    } else {
                        Vec::new()
                    };
                    let hwaccel_args = sanitize_hwaccel_args_for_encoder(&raw_hwaccel, v_name);
                    let fallback_video_filter_args = sanitize_fallback_filter_args(
                        &config.FFMPEG_FALLBACK_VIDEO_FILTER_ARGS,
                        target.video,
                    );
                    let display_name = format!(
                        "{} + {} ({})",
                        v_name,
                        a_name,
                        target.container.as_str()
                    );
                    log_event(
                        LogLevel::Info,
                        format!(
                            "[Pre-Flight] Verified local target profile: {} (video_args='{}', audio_args='{}', muxer_args='{}')",
                            display_name,
                            v_args.join(" "),
                            a_args.join(" "),
                            muxer_args.join(" ")
                        ),
                    );
                    return Ok(ResolvedTargetProfile {
                        target: *target,
                        video_encoder_name: v_name.clone(),
                        audio_encoder_name: a_name.clone(),
                        display_name,
                        decode_args: config.FFMPEG_DECODE_ARGS.clone(),
                        hwaccel_args,
                        encoder_args: v_args.clone(),
                        fallback_video_filter_args,
                        audio_args: a_args.clone(),
                        muxer_args,
                    });
                }
                Err(err) => {
                    log_event(
                        LogLevel::Warn,
                        format!(
                            "[Pre-Flight] Local candidate ({} + {}) failed 5s 256x144 probe: {}",
                            v_name, a_name, err
                        ),
                    );
                    failure_notes.push(format!("({} + {}): {}", v_name, a_name, err));
                }
            }
        }
    }

    bail!(
        "No working local FFmpeg encoder combination passed pre-flight verification for [{}].\nAttempts:\n  - {}",
        target.label(),
        failure_notes.join("\n  - ")
    );
}
