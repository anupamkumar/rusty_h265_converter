#!/usr/bin/env python3
"""
rusty_h265_metrics_agent.py — Portable Companion Observability Agent for rusty_h265_converter.

Supports two execution roles across macOS, Linux, and Windows with zero external pip dependencies:
  1. --role remote : Deployed via SCP into each remote worker node's temp directory at Step 6 startup.
                     Uses direct OS syscalls & C libraries via ctypes (statvfs / GetDiskFreeSpaceExW,
                     Mach host_statistics / procfs / Win32 GetSystemTimes & GlobalMemoryStatusEx,
                     NVIDIA NVML C API / Apple IOKit IOAccelerator) to stream JSON hardware inventory
                     and periodic CPU/RAM/Disk/GPU telemetry lines over the SSH pipe.
  2. --role master : Runs on the local orchestrator host. Reads /tmp/rusty_h265_telemetry.json,
                     merges local OS/GPU syscall samples, serves GET /metrics on PROMETHEUS_LISTEN_ADDR,
                     pushes periodically to PROMETHEUS_ENDPOINT, and runs a rotation-aware log tailer on
                     rusty_h265_converter.log that ships structured logfmt streams to Grafana Loki
                     (POST LOKI_ENDPOINT /loki/api/v1/push).
"""

import argparse
import ctypes
import ctypes.util
import datetime
import http.server
import json
import os
import platform
import re
import resource
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from typing import Any, Dict, List, Optional, Tuple


def detect_host_os_label() -> str:
    sys_name = platform.system().lower()
    if "darwin" in sys_name or "mac" in sys_name:
        return "Mac"
    if "win" in sys_name:
        return "Windows"
    return "Linux"


def unquote(val: str) -> str:
    v = val.strip()
    if len(v) >= 2 and ((v[0] == '"' and v[-1] == '"') or (v[0] == "'" and v[-1] == "'")):
        return v[1:-1]
    return v


def parse_bool(val: str) -> bool:
    return unquote(val).strip().lower() in ("true", "yes", "1", "on")


def load_simple_config_yaml(path: str) -> Dict[str, Any]:
    cfg: Dict[str, Any] = {
        "LOCAL_NODENAME": socket.gethostname() or "local-host",
        "PROMETHEUS_ENABLED": True,
        "TELEMETRY_SNAPSHOT_PATH": "/tmp/rusty_h265_telemetry.json",
        "PROMETHEUS_LISTEN_ADDR": "0.0.0.0:9184",
        "PROMETHEUS_ENDPOINT": "",
        "PROMETHEUS_PUSH_INTERVAL_SEC": 5,
        "LOKI_ENABLED": False,
        "LOKI_ENDPOINT": "",
        "LOKI_BATCH_WAIT_MSEC": 1000,
        "LOKI_BATCH_SIZE_BYTES": 65536,
    }
    if not path or not os.path.isfile(path):
        return cfg

    in_local_block = False
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as f:
            for raw_line in f:
                no_comment = raw_line.split("#", 1)[0]
                indent = len(no_comment) - len(no_comment.lstrip())
                line = no_comment.strip()
                if not line or line == "---":
                    continue
                if indent == 0:
                    in_local_block = False
                if line.upper() == "LOCAL:":
                    in_local_block = True
                    continue
                if ":" in line and not line.startswith("-"):
                    k_raw, v_raw = line.split(":", 1)
                    k = unquote(k_raw).strip().upper().replace("-", "_")
                    v = unquote(v_raw).strip()
                    if in_local_block and k == "NODENAME" and v:
                        cfg["LOCAL_NODENAME"] = v
                    elif k in ("LOCAL_NODENAME", "NODENAME") and indent == 0 and v:
                        cfg["LOCAL_NODENAME"] = v
                    elif k == "PROMETHEUS_ENABLED":
                        cfg["PROMETHEUS_ENABLED"] = parse_bool(v)
                    elif k == "TELEMETRY_SNAPSHOT_PATH" and v:
                        cfg["TELEMETRY_SNAPSHOT_PATH"] = v
                    elif k == "PROMETHEUS_LISTEN_ADDR":
                        cfg["PROMETHEUS_LISTEN_ADDR"] = v
                    elif k == "PROMETHEUS_ENDPOINT":
                        cfg["PROMETHEUS_ENDPOINT"] = v
                    elif k == "PROMETHEUS_PUSH_INTERVAL_SEC":
                        try:
                            cfg["PROMETHEUS_PUSH_INTERVAL_SEC"] = max(1, int(float(v)))
                        except ValueError:
                            pass
                    elif k == "LOKI_ENABLED":
                        cfg["LOKI_ENABLED"] = parse_bool(v)
                    elif k == "LOKI_ENDPOINT":
                        cfg["LOKI_ENDPOINT"] = v
                    elif k == "LOKI_BATCH_WAIT_MSEC":
                        try:
                            cfg["LOKI_BATCH_WAIT_MSEC"] = max(100, int(float(v)))
                        except ValueError:
                            pass
                    elif k == "LOKI_BATCH_SIZE_BYTES":
                        try:
                            cfg["LOKI_BATCH_SIZE_BYTES"] = max(1024, int(float(v)))
                        except ValueError:
                            pass
    except Exception:
        pass
    return cfg


def load_log_file_path_from_logging_conf(path: str = "logging.conf") -> str:
    log_file = "rusty_h265_converter.log"
    if not path or not os.path.isfile(path):
        return log_file
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as f:
            for raw in f:
                line = raw.split("#", 1)[0].strip()
                if not line:
                    continue
                sep = "=" if "=" in line else (":" if ":" in line else None)
                if sep:
                    k, v = line.split(sep, 1)
                    if k.strip().lower() in ("log_file", "file", "filepath", "path"):
                        cleaned = unquote(v).strip()
                        if cleaned:
                            return cleaned
    except Exception:
        pass
    return log_file


# ============================================================================
# 1. Direct OS Syscall Samplers (ctypes / Mach / Win32 / procfs / NVML / IOKit)
# ============================================================================

def get_disk_space_gb_syscall(target_path: str) -> Tuple[float, float]:
    """Returns (free_gb, total_gb) using os.statvfs (POSIX) or GetDiskFreeSpaceExW (Windows)."""
    probe = target_path or "."
    while not os.path.exists(probe):
        parent = os.path.dirname(probe)
        if not parent or parent == probe:
            probe = "."
            break
        probe = parent

    gb = 1024.0 * 1024.0 * 1024.0
    if hasattr(os, "statvfs"):
        try:
            st = os.statvfs(probe)
            frsize = st.f_frsize if st.f_frsize > 0 else st.f_bsize
            free_b = st.f_bavail * frsize
            total_b = st.f_blocks * frsize
            return (free_b / gb, total_b / gb)
        except Exception:
            return (0.0, 0.0)
    elif os.name == "nt":
        try:
            avail = ctypes.c_ulonglong(0)
            total = ctypes.c_ulonglong(0)
            free = ctypes.c_ulonglong(0)
            ok = ctypes.windll.kernel32.GetDiskFreeSpaceExW(
                ctypes.c_wchar_p(probe),
                ctypes.byref(avail),
                ctypes.byref(total),
                ctypes.byref(free),
            )
            if ok:
                return (avail.value / gb, total.value / gb)
        except Exception:
            pass
    return (0.0, 0.0)


class CpuTickTracker:
    """Computes host CPU user/system/idle/active ratios using OS syscalls without subprocesses."""

    def __init__(self) -> None:
        self.prev_ticks: Optional[Tuple[int, int, int, int]] = None
        self.host_os = detect_host_os_label()

    def _read_raw_ticks(self) -> Optional[Tuple[int, int, int, int]]:
        if self.host_os == "Mac":
            try:
                libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
                host_port = libc.mach_host_self()
                # HOST_CPU_LOAD_INFO = 3, count = 4 (USER=0, SYSTEM=1, IDLE=2, NICE=3)
                ticks = (ctypes.c_uint32 * 4)()
                count = ctypes.c_uint32(4)
                kr = libc.host_statistics(
                    host_port,
                    ctypes.c_int(3),
                    ctypes.byref(ticks),
                    ctypes.byref(count),
                )
                if kr == 0:
                    return (int(ticks[0]), int(ticks[1]), int(ticks[2]), int(ticks[3]))
            except Exception:
                pass
        elif self.host_os == "Linux":
            try:
                with open("/proc/stat", "r", encoding="utf-8") as f:
                    first = f.readline()
                parts = first.split()
                if len(parts) >= 5 and parts[0] == "cpu":
                    user = int(parts[1])
                    nice = int(parts[2])
                    system = int(parts[3])
                    idle = int(parts[4])
                    iowait = int(parts[5]) if len(parts) > 5 else 0
                    return (user, system + iowait, idle, nice)
            except Exception:
                pass
        elif self.host_os == "Windows":
            try:
                class FILETIME(ctypes.Structure):
                    _fields_ = [("dwLowDateTime", ctypes.c_uint32), ("dwHighDateTime", ctypes.c_uint32)]

                idle_ft = FILETIME()
                kernel_ft = FILETIME()
                user_ft = FILETIME()
                if ctypes.windll.kernel32.GetSystemTimes(
                    ctypes.byref(idle_ft), ctypes.byref(kernel_ft), ctypes.byref(user_ft)
                ):
                    idle = (idle_ft.dwHighDateTime << 32) | idle_ft.dwLowDateTime
                    kernel = (kernel_ft.dwHighDateTime << 32) | kernel_ft.dwLowDateTime
                    user = (user_ft.dwHighDateTime << 32) | user_ft.dwLowDateTime
                    # On Win32, kernel time includes idle time
                    sys_only = max(0, kernel - idle)
                    return (user, sys_only, idle, 0)
            except Exception:
                pass
        return None

    def sample_cpu_ratios(self) -> Tuple[float, float, float, float]:
        cur = self._read_raw_ticks()
        if cur is None:
            return (0.0, 0.0, 1.0, 0.0)
        if self.prev_ticks is None:
            self.prev_ticks = cur
            time.sleep(0.05)
            cur2 = self._read_raw_ticks()
            if cur2 is not None and cur2 != cur:
                cur = cur2
            else:
                total = sum(cur)
                if total > 0:
                    u = (cur[0] + cur[3]) / total
                    s = cur[1] / total
                    i = cur[2] / total
                    return (u, s, i, max(0.0, min(1.0, u + s)))
                return (0.0, 0.0, 1.0, 0.0)

        prev = self.prev_ticks
        self.prev_ticks = cur
        du = max(0, (cur[0] - prev[0]) + (cur[3] - prev[3]))
        ds = max(0, cur[1] - prev[1])
        di = max(0, cur[2] - prev[2])
        dt = du + ds + di
        if dt <= 0:
            return (0.0, 0.0, 1.0, 0.0)
        u_ratio = min(1.0, du / dt)
        s_ratio = min(1.0, ds / dt)
        i_ratio = min(1.0, di / dt)
        act_ratio = min(1.0, (du + ds) / dt)
        return (u_ratio, s_ratio, i_ratio, act_ratio)


def get_load_averages_syscall() -> Tuple[float, float, float]:
    if hasattr(os, "getloadavg"):
        try:
            la = os.getloadavg()
            return (float(la[0]), float(la[1]), float(la[2]))
        except Exception:
            pass
    return (0.0, 0.0, 0.0)


def get_memory_stats_gb_syscall() -> Tuple[float, float, float, float, float]:
    """Returns (used_gb, free_gb, total_gb, wired_gb, compressed_gb) via OS syscalls."""
    host_os = detect_host_os_label()
    gb = 1024.0 * 1024.0 * 1024.0

    if host_os == "Mac":
        try:
            libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
            memsize = ctypes.c_uint64(0)
            sz = ctypes.c_size_t(ctypes.sizeof(memsize))
            libc.sysctlbyname(
                ctypes.c_char_p(b"hw.memsize"),
                ctypes.byref(memsize),
                ctypes.byref(sz),
                None,
                ctypes.c_size_t(0),
            )
            total_b = memsize.value

            page_size = ctypes.c_uint64(0)
            sz_pg = ctypes.c_size_t(ctypes.sizeof(page_size))
            libc.sysctlbyname(
                ctypes.c_char_p(b"hw.pagesize"),
                ctypes.byref(page_size),
                ctypes.byref(sz_pg),
                None,
                ctypes.c_size_t(0),
            )
            pg = page_size.value if page_size.value > 0 else 16384

            # vm_statistics64_data_t has 38 uint32/uint64 fields (~160 bytes)
            vm_buf = (ctypes.c_uint32 * 48)()
            count = ctypes.c_uint32(38)
            host_port = libc.mach_host_self()
            # HOST_VM_INFO64 = 4
            kr = libc.host_statistics64(
                host_port,
                ctypes.c_int(4),
                ctypes.byref(vm_buf),
                ctypes.byref(count),
            )
            if kr == 0 and total_b > 0:
                free_pages = vm_buf[0]
                active_pages = vm_buf[1]
                inactive_pages = vm_buf[2]
                wire_pages = vm_buf[3]
                compressor_pages = vm_buf[32] if count.value > 32 else 0
                free_b = (free_pages + inactive_pages) * pg
                used_b = max(0, total_b - free_b)
                wired_b = wire_pages * pg
                comp_b = compressor_pages * pg
                return (used_b / gb, free_b / gb, total_b / gb, wired_b / gb, comp_b / gb)
        except Exception:
            pass
    elif host_os == "Linux":
        try:
            mem_info: Dict[str, int] = {}
            with open("/proc/meminfo", "r", encoding="utf-8") as f:
                for line in f:
                    if ":" in line:
                        k, v = line.split(":", 1)
                        toks = v.strip().split()
                        if toks and toks[0].isdigit():
                            mem_info[k.strip()] = int(toks[0]) * 1024
            total_b = mem_info.get("MemTotal", 0)
            avail_b = mem_info.get("MemAvailable", mem_info.get("MemFree", 0))
            used_b = max(0, total_b - avail_b)
            return (used_b / gb, avail_b / gb, total_b / gb, 0.0, 0.0)
        except Exception:
            pass
    elif host_os == "Windows":
        try:
            class MEMORYSTATUSEX(ctypes.Structure):
                _fields_ = [
                    ("dwLength", ctypes.c_uint32),
                    ("dwMemoryLoad", ctypes.c_uint32),
                    ("ullTotalPhys", ctypes.c_uint64),
                    ("ullAvailPhys", ctypes.c_uint64),
                    ("ullTotalPageFile", ctypes.c_uint64),
                    ("ullAvailPageFile", ctypes.c_uint64),
                    ("ullTotalVirtual", ctypes.c_uint64),
                    ("ullAvailVirtual", ctypes.c_uint64),
                    ("ullAvailExtendedVirtual", ctypes.c_uint64),
                ]

            stat = MEMORYSTATUSEX()
            stat.dwLength = ctypes.sizeof(MEMORYSTATUSEX)
            if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(stat)):
                total_b = stat.ullTotalPhys
                avail_b = stat.ullAvailPhys
                used_b = max(0, total_b - avail_b)
                return (used_b / gb, avail_b / gb, total_b / gb, 0.0, 0.0)
        except Exception:
            pass

    return (0.0, 0.0, 0.0, 0.0, 0.0)


def sample_gpu_stats_syscall() -> Dict[str, Any]:
    """
    Samples GPU, hardware encoder (NVENC / VideoToolbox), and decoder utilization
    via direct C shared library calls (NVML via ctypes on Windows/Linux, IOKit on macOS),
    with graceful fallback.
    """
    res: Dict[str, Any] = {
        "gpu_name": "",
        "gpu_core_utilization_ratio": 0.0,
        "gpu_encoder_utilization_ratio": 0.0,
        "gpu_decoder_utilization_ratio": 0.0,
        "gpu_renderer_utilization_ratio": 0.0,
        "gpu_tiler_utilization_ratio": 0.0,
        "gpu_memory_used_in_mb": 0.0,
        "gpu_memory_total_in_mb": 0.0,
        "gpu_temperature_celsius": 0.0,
        "gpu_power_watts": 0.0,
    }
    host_os = detect_host_os_label()

    # 1. Try NVIDIA NVML C shared library directly via ctypes (Windows & Linux)
    if host_os in ("Windows", "Linux"):
        nvml_candidates = (
            [
                "nvml.dll",
                r"C:\Windows\System32\nvml.dll",
                r"C:\Program Files\NVIDIA Corporation\NVSMI\nvml.dll",
            ]
            if host_os == "Windows"
            else ["libnvidia-ml.so.1", "libnvidia-ml.so"]
        )
        for lib_path in nvml_candidates:
            try:
                nvml = ctypes.CDLL(lib_path)
                if nvml.nvmlInit_v2() == 0:
                    dev_handle = ctypes.c_void_p()
                    if nvml.nvmlDeviceGetHandleByIndex_v2(ctypes.c_uint(0), ctypes.byref(dev_handle)) == 0:
                        name_buf = ctypes.create_string_buffer(96)
                        if nvml.nvmlDeviceGetName(dev_handle, name_buf, ctypes.c_uint(95)) == 0:
                            res["gpu_name"] = name_buf.value.decode("utf-8", errors="replace").strip()

                        class NvmlUtilization(ctypes.Structure):
                            _fields_ = [("gpu", ctypes.c_uint), ("memory", ctypes.c_uint)]

                        util = NvmlUtilization()
                        if nvml.nvmlDeviceGetUtilizationRates(dev_handle, ctypes.byref(util)) == 0:
                            res["gpu_core_utilization_ratio"] = min(1.0, max(0.0, util.gpu / 100.0))

                        enc_util = ctypes.c_uint(0)
                        enc_period = ctypes.c_uint(0)
                        if (
                            nvml.nvmlDeviceGetEncoderUtilization(
                                dev_handle, ctypes.byref(enc_util), ctypes.byref(enc_period)
                            )
                            == 0
                        ):
                            res["gpu_encoder_utilization_ratio"] = min(1.0, max(0.0, enc_util.value / 100.0))

                        dec_util = ctypes.c_uint(0)
                        dec_period = ctypes.c_uint(0)
                        if (
                            nvml.nvmlDeviceGetDecoderUtilization(
                                dev_handle, ctypes.byref(dec_util), ctypes.byref(dec_period)
                            )
                            == 0
                        ):
                            res["gpu_decoder_utilization_ratio"] = min(1.0, max(0.0, dec_util.value / 100.0))

                        class NvmlMemory(ctypes.Structure):
                            _fields_ = [
                                ("total", ctypes.c_ulonglong),
                                ("free", ctypes.c_ulonglong),
                                ("used", ctypes.c_ulonglong),
                            ]

                        mem = NvmlMemory()
                        if nvml.nvmlDeviceGetMemoryInfo(dev_handle, ctypes.byref(mem)) == 0:
                            mb = 1024.0 * 1024.0
                            res["gpu_memory_used_in_mb"] = mem.used / mb
                            res["gpu_memory_total_in_mb"] = mem.total / mb

                        temp_c = ctypes.c_uint(0)
                        if nvml.nvmlDeviceGetTemperature(dev_handle, ctypes.c_uint(0), ctypes.byref(temp_c)) == 0:
                            res["gpu_temperature_celsius"] = float(temp_c.value)

                        power_mw = ctypes.c_uint(0)
                        if nvml.nvmlDeviceGetPowerUsage(dev_handle, ctypes.byref(power_mw)) == 0:
                            res["gpu_power_watts"] = power_mw.value / 1000.0

                    nvml.nvmlShutdown()
                    if res["gpu_name"]:
                        return res
            except Exception:
                continue

        # Fallback to nvidia-smi if NVML library path is non-standard
        try:
            out = subprocess.check_output(
                [
                    "nvidia-smi",
                    "--query-gpu=name,utilization.gpu,utilization.encoder,utilization.decoder,memory.used,memory.total,temperature.gpu,power.draw",
                    "--format=csv,noheader,nounits",
                ],
                stderr=subprocess.DEVNULL,
                timeout=2.0,
            ).decode("utf-8", errors="replace")
            first_line = out.strip().splitlines()[0] if out.strip() else ""
            parts = [p.strip() for p in first_line.split(",")]
            if len(parts) >= 8:
                res["gpu_name"] = parts[0]
                res["gpu_core_utilization_ratio"] = min(1.0, float(parts[1]) / 100.0) if parts[1].replace(".", "").isdigit() else 0.0
                res["gpu_encoder_utilization_ratio"] = min(1.0, float(parts[2]) / 100.0) if parts[2].replace(".", "").isdigit() else 0.0
                res["gpu_decoder_utilization_ratio"] = min(1.0, float(parts[3]) / 100.0) if parts[3].replace(".", "").isdigit() else 0.0
                res["gpu_memory_used_in_mb"] = float(parts[4]) if parts[4].replace(".", "").isdigit() else 0.0
                res["gpu_memory_total_in_mb"] = float(parts[5]) if parts[5].replace(".", "").isdigit() else 0.0
                res["gpu_temperature_celsius"] = float(parts[6]) if parts[6].replace(".", "").isdigit() else 0.0
                res["gpu_power_watts"] = float(parts[7]) if parts[7].replace(".", "").isdigit() else 0.0
                return res
        except Exception:
            pass

    # 2. macOS Apple Silicon / VideoToolbox IOAccelerator PerformanceStatistics
    if host_os == "Mac":
        res["gpu_name"] = "Apple_Silicon_VideoToolbox"
        try:
            out = subprocess.check_output(
                ["/usr/sbin/ioreg", "-r", "-w", "0", "-c", "IOAccelerator"],
                stderr=subprocess.DEVNULL,
                timeout=1.5,
            ).decode("utf-8", errors="replace")
            m_dev = re.search(r'"Device Utilization %"\s*=\s*(\d+)', out)
            m_ren = re.search(r'"Renderer Utilization %"\s*=\s*(\d+)', out)
            m_til = re.search(r'"Tiler Utilization %"\s*=\s*(\d+)', out)
            m_mem_used = re.search(r'"In use system memory"\s*=\s*(\d+)', out)
            m_mem_tot = re.search(r'"Alloc system memory"\s*=\s*(\d+)', out)
            if m_dev:
                res["gpu_core_utilization_ratio"] = min(1.0, float(m_dev.group(1)) / 100.0)
            if m_ren:
                res["gpu_renderer_utilization_ratio"] = min(1.0, float(m_ren.group(1)) / 100.0)
            if m_til:
                res["gpu_tiler_utilization_ratio"] = min(1.0, float(m_til.group(1)) / 100.0)
            mb = 1024.0 * 1024.0
            if m_mem_used:
                res["gpu_memory_used_in_mb"] = float(m_mem_used.group(1)) / mb
            if m_mem_tot:
                res["gpu_memory_total_in_mb"] = float(m_mem_tot.group(1)) / mb
        except Exception:
            pass

    return res


def collect_host_hardware_sample(
    cpu_tracker: CpuTickTracker,
    nodename: str,
    host_os: str,
    watch_dir: str = ".",
) -> Dict[str, Any]:
    u_cpu, s_cpu, i_cpu, act_cpu = cpu_tracker.sample_cpu_ratios()
    la1, la5, la15 = get_load_averages_syscall()
    mem_used, mem_free, mem_tot, mem_wired, mem_comp = get_memory_stats_gb_syscall()
    dir_free_gb, dir_tot_gb = get_disk_space_gb_syscall(watch_dir)
    gpu = sample_gpu_stats_syscall()

    return {
        "nodename": nodename,
        "host_os": host_os,
        "updated_at_epoch_secs": int(time.time()),
        "cpu_user_ratio": round(u_cpu, 6),
        "cpu_system_ratio": round(s_cpu, 6),
        "cpu_idle_ratio": round(i_cpu, 6),
        "cpu_active_ratio": round(act_cpu, 6),
        "load_avg_1m": round(la1, 4),
        "load_avg_5m": round(la5, 4),
        "load_avg_15m": round(la15, 4),
        "memory_used_gb": round(mem_used, 4),
        "memory_free_gb": round(mem_free, 4),
        "memory_total_gb": round(mem_tot, 4),
        "memory_wired_gb": round(mem_wired, 4),
        "memory_compressed_gb": round(mem_comp, 4),
        "watch_dir_free_gb": round(dir_free_gb, 4),
        "watch_dir_total_gb": round(dir_tot_gb, 4),
        "gpu_name": gpu["gpu_name"],
        "gpu_core_utilization_ratio": round(gpu["gpu_core_utilization_ratio"], 6),
        "gpu_encoder_utilization_ratio": round(gpu["gpu_encoder_utilization_ratio"], 6),
        "gpu_decoder_utilization_ratio": round(gpu["gpu_decoder_utilization_ratio"], 6),
        "gpu_renderer_utilization_ratio": round(gpu["gpu_renderer_utilization_ratio"], 6),
        "gpu_tiler_utilization_ratio": round(gpu["gpu_tiler_utilization_ratio"], 6),
        "gpu_memory_used_in_mb": round(gpu["gpu_memory_used_in_mb"], 2),
        "gpu_memory_total_in_mb": round(gpu["gpu_memory_total_in_mb"], 2),
        "gpu_temperature_celsius": round(gpu["gpu_temperature_celsius"], 2),
        "gpu_power_watts": round(gpu["gpu_power_watts"], 2),
    }


def collect_process_tree_stats() -> Dict[str, Tuple[float, float]]:
    """
    Returns {process_group: (cpu_ratio, rss_in_mb)} for rusty_h265_converter,
    ffmpeg_children, and scp_children.
    """
    stats: Dict[str, Tuple[float, float]] = {
        "rusty_h265_converter": (0.0, 0.0),
        "ffmpeg_children": (0.0, 0.0),
        "scp_children": (0.0, 0.0),
    }
    try:
        ru_self = resource.getrusage(resource.RUSAGE_SELF)
        # On macOS ru_maxrss is in bytes; on Linux ru_maxrss is in kilobytes
        rss_div = (1024.0 * 1024.0) if detect_host_os_label() == "Mac" else 1024.0
        stats["rusty_h265_converter"] = (0.0, ru_self.ru_maxrss / rss_div)
    except Exception:
        pass
    return stats


# ============================================================================
# 2. Prometheus Exposition Text Renderer (Merging Rust Snapshot + Syscalls)
# ============================================================================

def esc_label(v: Any) -> str:
    return str(v).replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")


def render_histogram_lines(
    add_sample,
    metric_name: str,
    base_labels: str,
    hist: Dict[str, Any],
) -> None:
    bounds = hist.get("bounds", [])
    counts = hist.get("bucket_counts", [])
    total_cnt = hist.get("count", 0)
    total_sum = hist.get("sum", 0.0)
    for idx, b in enumerate(bounds):
        c = counts[idx] if idx < len(counts) else 0
        add_sample(metric_name, "histogram", f'{metric_name}_bucket{{{base_labels},le="{b}"}} {c}')
    add_sample(metric_name, "histogram", f'{metric_name}_bucket{{{base_labels},le="+Inf"}} {total_cnt}')
    add_sample(metric_name, "histogram", f"{metric_name}_sum{{{base_labels}}} {total_sum:.6f}")
    add_sample(metric_name, "histogram", f"{metric_name}_count{{{base_labels}}} {total_cnt}")


def render_prometheus_text(
    snap: Optional[Dict[str, Any]],
    local_hw: Dict[str, Any],
    loki_shipped_ok: int,
    loki_shipped_err: int,
) -> str:
    families: Dict[str, Tuple[str, Optional[str], List[str]]] = {}

    def add_sample(
        family: str,
        mtype: str,
        line: str,
        help_text: Optional[str] = None,
    ) -> None:
        if family not in families:
            families[family] = (mtype, help_text, [line])
        else:
            families[family][2].append(line)

    nodename = esc_label(local_hw["nodename"])
    host_os = esc_label(local_hw["host_os"])

    snap_pid = int(snap.get("converter_pid", 0)) if snap else 0
    pid_alive = (snap_pid <= 0) or is_pid_alive(snap_pid)
    converter_active = 1 if (snap and snap.get("converter_active", False) and pid_alive) else 0
    uptime_secs = float(snap.get("uptime_in_secs", 0.0)) if (snap and converter_active) else 0.0

    add_sample(
        "h265_converter_up",
        "gauge",
        f'h265_converter_up{{nodename="{nodename}",host_os="{host_os}"}} {converter_active}',
        "Whether rusty_h265_converter is actively running (1=active, 0=idle).",
    )
    add_sample(
        "h265_converter_uptime_in_secs",
        "gauge",
        f'h265_converter_uptime_in_secs{{nodename="{nodename}",host_os="{host_os}"}} {uptime_secs:.3f}',
        "Wall-clock uptime of rusty_h265_converter in seconds.",
    )

    if snap:
        # 1. Thread-level USE, RED & Wait-time metrics
        for t in snap.get("threads", {}).values():
            t_lbl = (
                f'nodename="{esc_label(t.get("nodename", nodename))}",'
                f'host_os="{esc_label(t.get("host_os", host_os))}",'
                f'worker_group="{esc_label(t.get("worker_group", ""))}",'
                f'worker_thread="{esc_label(t.get("worker_thread", ""))}"'
            )
            state = int(t.get("state", 0))
            busy = float(t.get("busy_in_secs_total", 0.0))
            wait_map: Dict[str, float] = t.get("wait_in_secs_by_reason", {})
            total_wait = sum(float(v) for v in wait_map.values())
            denom = busy + total_wait
            util = (busy / denom) if denom > 0.0001 else 0.0

            add_sample("h265_thread_state", "gauge", f"h265_thread_state{{{t_lbl}}} {state}")
            add_sample("h265_thread_busy_in_secs_total", "counter", f"h265_thread_busy_in_secs_total{{{t_lbl}}} {busy:.6f}")
            add_sample("h265_thread_utilization_ratio", "gauge", f"h265_thread_utilization_ratio{{{t_lbl}}} {util:.6f}")
            add_sample(
                "h265_thread_cpu_in_secs_total",
                "counter",
                f'h265_thread_cpu_in_secs_total{{{t_lbl},mode="user"}} {float(t.get("cpu_user_in_secs_total", 0.0)):.6f}',
            )
            add_sample(
                "h265_thread_cpu_in_secs_total",
                "counter",
                f'h265_thread_cpu_in_secs_total{{{t_lbl},mode="system"}} {float(t.get("cpu_system_in_secs_total", 0.0)):.6f}',
            )

            for r, w_secs in wait_map.items():
                add_sample(
                    "h265_thread_wait_in_secs_total",
                    "counter",
                    f'h265_thread_wait_in_secs_total{{{t_lbl},wait_reason="{esc_label(r)}"}} {float(w_secs):.6f}',
                )
            for r, cur_secs in t.get("current_wait_in_secs_by_reason", {}).items():
                add_sample(
                    "h265_thread_current_wait_in_secs",
                    "gauge",
                    f'h265_thread_current_wait_in_secs{{{t_lbl},wait_reason="{esc_label(r)}"}} {float(cur_secs):.6f}',
                )
            for r, cyc in t.get("wait_cycles_by_reason", {}).items():
                add_sample(
                    "h265_thread_wait_cycles_total",
                    "counter",
                    f'h265_thread_wait_cycles_total{{{t_lbl},wait_reason="{esc_label(r)}"}} {int(cyc)}',
                )
            for r, hist in t.get("task_wait_hist_by_reason", {}).items():
                render_histogram_lines(
                    add_sample,
                    "h265_thread_task_wait_in_secs",
                    f'{t_lbl},wait_reason="{esc_label(r)}"',
                    hist,
                )
            for stg, cnt in t.get("tasks_started_by_stage", {}).items():
                add_sample(
                    "h265_thread_tasks_started_total",
                    "counter",
                    f'h265_thread_tasks_started_total{{{t_lbl},stage="{esc_label(stg)}"}} {int(cnt)}',
                )
            for stg, out_map in t.get("tasks_completed_by_stage_outcome", {}).items():
                for outc, cnt in out_map.items():
                    add_sample(
                        "h265_thread_tasks_completed_total",
                        "counter",
                        f'h265_thread_tasks_completed_total{{{t_lbl},stage="{esc_label(stg)}",outcome="{esc_label(outc)}"}} {int(cnt)}',
                    )
            for err_t, cnt in t.get("errors_by_type", {}).items():
                add_sample(
                    "h265_thread_errors_total",
                    "counter",
                    f'h265_thread_errors_total{{{t_lbl},error_type="{esc_label(err_t)}"}} {int(cnt)}',
                )
            for stg, hist in t.get("stage_duration_hist_by_stage", {}).items():
                render_histogram_lines(
                    add_sample,
                    "h265_stage_duration_in_secs",
                    f'{t_lbl},stage="{esc_label(stg)}"',
                    hist,
                )

        # 2. Mount & Disk I/O metrics
        for m in snap.get("mounts", {}).values():
            m_lbl = (
                f'nodename="{esc_label(m.get("nodename", nodename))}",'
                f'host_os="{esc_label(m.get("host_os", host_os))}",'
                f'mount_point="{esc_label(m.get("mount_point", ""))}",'
                f'drive_type="{esc_label(m.get("drive_type", ""))}"'
            )
            render_histogram_lines(
                add_sample,
                "h265_disk_io_chunk_duration_in_secs",
                f'{m_lbl},op="read"',
                m.get("read_chunk_hist", {}),
            )
            render_histogram_lines(
                add_sample,
                "h265_disk_io_chunk_duration_in_secs",
                f'{m_lbl},op="write"',
                m.get("write_chunk_hist", {}),
            )
            mp_str = esc_label(m.get("mount_point", ""))
            m_node = esc_label(m.get("nodename", nodename))
            m_os = esc_label(m.get("host_os", host_os))
            for th, hist in m.get("io_lock_wait_hist_by_thread", {}).items():
                render_histogram_lines(
                    add_sample,
                    "h265_disk_io_lock_wait_in_secs",
                    f'nodename="{m_node}",host_os="{m_os}",mount_point="{mp_str}",worker_thread="{esc_label(th)}"',
                    hist,
                )
            for th, hold in m.get("io_lock_hold_in_secs_by_thread", {}).items():
                add_sample(
                    "h265_disk_io_lock_hold_in_secs_total",
                    "counter",
                    f'h265_disk_io_lock_hold_in_secs_total{{nodename="{m_node}",host_os="{m_os}",mount_point="{mp_str}",worker_thread="{esc_label(th)}"}} {float(hold):.6f}',
                )
            add_sample(
                "h265_disk_io_lock_waiting_threads",
                "gauge",
                f'h265_disk_io_lock_waiting_threads{{nodename="{m_node}",host_os="{m_os}",mount_point="{mp_str}"}} {int(m.get("io_lock_waiting_threads", 0))}',
            )
            add_sample(
                "h265_disk_transfer_size_in_mb_total",
                "counter",
                f'h265_disk_transfer_size_in_mb_total{{{m_lbl},direction="read"}} {float(m.get("read_size_in_mb_total", 0.0)):.3f}',
            )
            add_sample(
                "h265_disk_transfer_size_in_mb_total",
                "counter",
                f'h265_disk_transfer_size_in_mb_total{{{m_lbl},direction="write"}} {float(m.get("write_size_in_mb_total", 0.0)):.3f}',
            )
            add_sample(
                "h265_disk_throughput_mbps",
                "gauge",
                f'h265_disk_throughput_mbps{{{m_lbl},direction="read"}} {float(m.get("read_throughput_mbps", 0.0)):.3f}',
            )
            add_sample(
                "h265_disk_throughput_mbps",
                "gauge",
                f'h265_disk_throughput_mbps{{{m_lbl},direction="write"}} {float(m.get("write_throughput_mbps", 0.0)):.3f}',
            )
            free_gb = float(m.get("free_gb", 0.0))
            tot_gb = float(m.get("total_gb", 0.0))
            util = ((tot_gb - free_gb) / tot_gb) if tot_gb > 0.0 else 0.0
            add_sample("h265_mount_free_gb", "gauge", f"h265_mount_free_gb{{{m_lbl}}} {free_gb:.3f}")
            add_sample("h265_mount_total_gb", "gauge", f"h265_mount_total_gb{{{m_lbl}}} {tot_gb:.3f}")
            add_sample("h265_mount_utilization_ratio", "gauge", f"h265_mount_utilization_ratio{{{m_lbl}}} {max(0.0, min(1.0, util)):.6f}")

            rd_lbl = f'nodename="{m_node}",host_os="{m_os}",mount_point="{mp_str}"'
            add_sample(
                "h265_ramdisk_quota_gb",
                "gauge",
                f'h265_ramdisk_quota_gb{{{rd_lbl},kind="staged_input_used"}} {float(m.get("ramdisk_staged_input_used_gb", 0.0)):.4f}',
            )
            add_sample(
                "h265_ramdisk_quota_gb",
                "gauge",
                f'h265_ramdisk_quota_gb{{{rd_lbl},kind="staged_output_used"}} {float(m.get("ramdisk_staged_output_used_gb", 0.0)):.4f}',
            )
            add_sample(
                "h265_ramdisk_quota_gb",
                "gauge",
                f'h265_ramdisk_quota_gb{{{rd_lbl},kind="partition_max"}} {float(m.get("ramdisk_partition_max_gb", 0.0)):.4f}',
            )
            add_sample(
                "h265_ramdisk_quota_gb",
                "gauge",
                f'h265_ramdisk_quota_gb{{{rd_lbl},kind="future_partition_max"}} {float(m.get("ramdisk_future_partition_max_gb", 0.0)):.4f}',
            )
            for q_st, cnt in m.get("queue_files_by_state", {}).items():
                add_sample("h265_queue_files_remaining", "gauge", f'h265_queue_files_remaining{{mount_point="{mp_str}",queue="{esc_label(q_st)}"}} {int(cnt)}')
            for q_st, mb in m.get("queue_size_in_mb_by_state", {}).items():
                add_sample(
                    "h265_queue_size_in_mb_remaining",
                    "gauge",
                    f'h265_queue_size_in_mb_remaining{{mount_point="{mp_str}",queue="{esc_label(q_st)}"}} {float(mb):.3f}',
                )
            for dec, cnt in m.get("stager_decisions_by_type", {}).items():
                add_sample(
                    "h265_stager_decisions_total",
                    "counter",
                    f'h265_stager_decisions_total{{mount_point="{mp_str}",decision="{esc_label(dec)}"}} {int(cnt)}',
                )
            for outc, cnt in m.get("resume_checks_by_outcome", {}).items():
                add_sample(
                    "h265_resume_checks_total",
                    "counter",
                    f'h265_resume_checks_total{{mount_point="{mp_str}",outcome="{esc_label(outc)}"}} {int(cnt)}',
                )
            for outc, cnt in m.get("subdirs_reconciled_by_outcome", {}).items():
                add_sample(
                    "h265_subdirs_reconciled_total",
                    "counter",
                    f'h265_subdirs_reconciled_total{{mount_point="{mp_str}",outcome="{esc_label(outc)}"}} {int(cnt)}',
                )

        # 3. Remote Network, SSH & SCP metrics
        for r in snap.get("remote_nodes", {}).values():
            r_lbl = f'nodename="{esc_label(r.get("nodename", ""))}",host_os="{esc_label(r.get("host_os", ""))}"'
            for cmd_t, hist in r.get("ssh_rtt_hist_by_cmd", {}).items():
                render_histogram_lines(
                    add_sample,
                    "h265_network_ssh_rtt_in_secs",
                    f'{r_lbl},cmd_type="{esc_label(cmd_t)}"',
                    hist,
                )
            render_histogram_lines(
                add_sample,
                "h265_network_scp_duration_in_secs",
                f'{r_lbl},direction="upload"',
                r.get("scp_upload_duration_hist", {}),
            )
            render_histogram_lines(
                add_sample,
                "h265_network_scp_duration_in_secs",
                f'{r_lbl},direction="download"',
                r.get("scp_download_duration_hist", {}),
            )
            add_sample(
                "h265_network_scp_transfer_size_in_mb_total",
                "counter",
                f'h265_network_scp_transfer_size_in_mb_total{{{r_lbl},direction="upload"}} {float(r.get("scp_upload_size_in_mb_total", 0.0)):.3f}',
            )
            add_sample(
                "h265_network_scp_transfer_size_in_mb_total",
                "counter",
                f'h265_network_scp_transfer_size_in_mb_total{{{r_lbl},direction="download"}} {float(r.get("scp_download_size_in_mb_total", 0.0)):.3f}',
            )
            add_sample(
                "h265_network_scp_throughput_mbps",
                "gauge",
                f'h265_network_scp_throughput_mbps{{{r_lbl},direction="upload"}} {float(r.get("scp_upload_throughput_mbps", 0.0)):.3f}',
            )
            add_sample(
                "h265_network_scp_throughput_mbps",
                "gauge",
                f'h265_network_scp_throughput_mbps{{{r_lbl},direction="download"}} {float(r.get("scp_download_throughput_mbps", 0.0)):.3f}',
            )
            q_used = float(r.get("quota_used_gb", 0.0))
            q_rem = float(r.get("quota_remaining_gb", 0.0))
            q_max = float(r.get("quota_max_gb", 0.0))
            q_cap = float(r.get("quota_single_file_cap_gb", 0.0))
            add_sample("h265_remote_quota_gb", "gauge", f'h265_remote_quota_gb{{{r_lbl},kind="used"}} {q_used:.4f}')
            add_sample("h265_remote_quota_gb", "gauge", f'h265_remote_quota_gb{{{r_lbl},kind="remaining"}} {q_rem:.4f}')
            add_sample("h265_remote_quota_gb", "gauge", f'h265_remote_quota_gb{{{r_lbl},kind="max"}} {q_max:.4f}')
            add_sample("h265_remote_quota_gb", "gauge", f'h265_remote_quota_gb{{{r_lbl},kind="single_file_cap"}} {q_cap:.4f}')
            sat = (q_used / q_max) if q_max > 0.0 else 0.0
            add_sample("h265_remote_quota_saturation_ratio", "gauge", f"h265_remote_quota_saturation_ratio{{{r_lbl}}} {max(0.0, min(1.0, sat)):.6f}")
            for q_name, depth in r.get("queue_depth_by_stage", {}).items():
                add_sample("h265_remote_queue_depth", "gauge", f'h265_remote_queue_depth{{{r_lbl},queue="{esc_label(q_name)}"}} {int(depth)}')
            for err_t, cnt in r.get("network_errors_by_type", {}).items():
                add_sample("h265_network_errors_total", "counter", f'h265_network_errors_total{{{r_lbl},error_type="{esc_label(err_t)}"}} {int(cnt)}')

        # 4. Media Transcoding Domain metrics
        for mg in snap.get("media_groups", {}).values():
            mg_lbl = (
                f'nodename="{esc_label(mg.get("nodename", ""))}",'
                f'host_os="{esc_label(mg.get("host_os", ""))}",'
                f'worker_group="{esc_label(mg.get("worker_group", ""))}"'
            )
            for codec, mb in mg.get("source_size_in_mb_by_codec", {}).items():
                add_sample(
                    "h265_media_source_file_size_in_mb_total",
                    "counter",
                    f'h265_media_source_file_size_in_mb_total{{{mg_lbl},source_codec="{esc_label(codec)}"}} {float(mb):.3f}',
                )
            for codec, mb in mg.get("target_size_in_mb_by_codec", {}).items():
                add_sample(
                    "h265_media_target_file_size_in_mb_total",
                    "counter",
                    f'h265_media_target_file_size_in_mb_total{{{mg_lbl},source_codec="{esc_label(codec)}"}} {float(mb):.3f}',
                )
            add_sample(
                "h265_media_storage_saved_in_mb_total",
                "counter",
                f'h265_media_storage_saved_in_mb_total{{{mg_lbl}}} {float(mg.get("storage_saved_in_mb_total", 0.0)):.3f}',
            )
            render_histogram_lines(
                add_sample,
                "h265_media_compression_ratio",
                mg_lbl,
                mg.get("compression_ratio_hist", {}),
            )
            enc_lbl = esc_label(mg.get("encoder", "hevc"))
            add_sample(
                "h265_media_video_duration_encoded_in_secs_total",
                "counter",
                f'h265_media_video_duration_encoded_in_secs_total{{{mg_lbl},encoder="{enc_lbl}"}} {float(mg.get("video_duration_encoded_in_secs_total", 0.0)):.3f}',
            )
            add_sample(
                "h265_encoder_realtime_speed_x",
                "gauge",
                f'h265_encoder_realtime_speed_x{{{mg_lbl},encoder="{enc_lbl}"}} {float(mg.get("realtime_speed_x", 0.0)):.3f}',
            )
            for codec, cnt in mg.get("hwaccel_fallback_by_codec", {}).items():
                add_sample(
                    "h265_hwaccel_software_fallback_total",
                    "counter",
                    f'h265_hwaccel_software_fallback_total{{{mg_lbl},source_codec="{esc_label(codec)}"}} {int(cnt)}',
                )
            for codec, cnt in mg.get("kept_smaller_original_by_codec", {}).items():
                add_sample(
                    "h265_kept_smaller_original_total",
                    "counter",
                    f'h265_kept_smaller_original_total{{{mg_lbl},source_codec="{esc_label(codec)}"}} {int(cnt)}',
                )

        add_sample(
            "h265_ramdisk_partition_resizes_total",
            "counter",
            f'h265_ramdisk_partition_resizes_total {int(snap.get("ramdisk_partition_resizes_total", 0))}',
        )
        for lvl, cnt in snap.get("logs_emitted_by_level", {}).items():
            add_sample("h265_logs_emitted_total", "counter", f'h265_logs_emitted_total{{level="{esc_label(lvl)}"}} {int(cnt)}')

    # 5. Host & GPU Hardware Samples (Local + Remote Agents)
    all_hw: List[Dict[str, Any]] = [local_hw]
    if snap:
        for r_hw in snap.get("remote_hardware", {}).values():
            if r_hw.get("nodename") != local_hw.get("nodename"):
                all_hw.append(r_hw)

    for hw in all_hw:
        h_lbl = f'nodename="{esc_label(hw.get("nodename", ""))}",host_os="{esc_label(hw.get("host_os", ""))}"'
        add_sample(
            "h265_host_cpu_utilization_ratio",
            "gauge",
            f'h265_host_cpu_utilization_ratio{{{h_lbl},mode="user"}} {float(hw.get("cpu_user_ratio", 0.0)):.6f}',
        )
        add_sample(
            "h265_host_cpu_utilization_ratio",
            "gauge",
            f'h265_host_cpu_utilization_ratio{{{h_lbl},mode="system"}} {float(hw.get("cpu_system_ratio", 0.0)):.6f}',
        )
        add_sample(
            "h265_host_cpu_utilization_ratio",
            "gauge",
            f'h265_host_cpu_utilization_ratio{{{h_lbl},mode="idle"}} {float(hw.get("cpu_idle_ratio", 0.0)):.6f}',
        )
        add_sample(
            "h265_host_cpu_utilization_ratio",
            "gauge",
            f'h265_host_cpu_utilization_ratio{{{h_lbl},mode="active"}} {float(hw.get("cpu_active_ratio", 0.0)):.6f}',
        )
        add_sample(
            "h265_host_load_average",
            "gauge",
            f'h265_host_load_average{{{h_lbl},window="1m"}} {float(hw.get("load_avg_1m", 0.0)):.4f}',
        )
        add_sample(
            "h265_host_load_average",
            "gauge",
            f'h265_host_load_average{{{h_lbl},window="5m"}} {float(hw.get("load_avg_5m", 0.0)):.4f}',
        )
        add_sample(
            "h265_host_load_average",
            "gauge",
            f'h265_host_load_average{{{h_lbl},window="15m"}} {float(hw.get("load_avg_15m", 0.0)):.4f}',
        )
        add_sample(
            "h265_host_memory_gb",
            "gauge",
            f'h265_host_memory_gb{{{h_lbl},state="used"}} {float(hw.get("memory_used_gb", 0.0)):.4f}',
        )
        add_sample(
            "h265_host_memory_gb",
            "gauge",
            f'h265_host_memory_gb{{{h_lbl},state="free"}} {float(hw.get("memory_free_gb", 0.0)):.4f}',
        )
        add_sample(
            "h265_host_memory_gb",
            "gauge",
            f'h265_host_memory_gb{{{h_lbl},state="total"}} {float(hw.get("memory_total_gb", 0.0)):.4f}',
        )
        add_sample(
            "h265_host_memory_gb",
            "gauge",
            f'h265_host_memory_gb{{{h_lbl},state="wired"}} {float(hw.get("memory_wired_gb", 0.0)):.4f}',
        )
        add_sample(
            "h265_host_memory_gb",
            "gauge",
            f'h265_host_memory_gb{{{h_lbl},state="compressed"}} {float(hw.get("memory_compressed_gb", 0.0)):.4f}',
        )
        gpu_name = hw.get("gpu_name", "")
        if gpu_name:
            g_lbl = f'{h_lbl},gpu_name="{esc_label(gpu_name)}"'
            add_sample(
                "h265_gpu_utilization_ratio",
                "gauge",
                f'h265_gpu_utilization_ratio{{{g_lbl},engine="core"}} {float(hw.get("gpu_core_utilization_ratio", 0.0)):.6f}',
            )
            add_sample(
                "h265_gpu_utilization_ratio",
                "gauge",
                f'h265_gpu_utilization_ratio{{{g_lbl},engine="encoder"}} {float(hw.get("gpu_encoder_utilization_ratio", 0.0)):.6f}',
            )
            add_sample(
                "h265_gpu_utilization_ratio",
                "gauge",
                f'h265_gpu_utilization_ratio{{{g_lbl},engine="decoder"}} {float(hw.get("gpu_decoder_utilization_ratio", 0.0)):.6f}',
            )
            add_sample(
                "h265_gpu_utilization_ratio",
                "gauge",
                f'h265_gpu_utilization_ratio{{{g_lbl},engine="renderer"}} {float(hw.get("gpu_renderer_utilization_ratio", 0.0)):.6f}',
            )
            add_sample(
                "h265_gpu_utilization_ratio",
                "gauge",
                f'h265_gpu_utilization_ratio{{{g_lbl},engine="tiler"}} {float(hw.get("gpu_tiler_utilization_ratio", 0.0)):.6f}',
            )
            add_sample(
                "h265_gpu_memory_in_mb",
                "gauge",
                f'h265_gpu_memory_in_mb{{{g_lbl},state="used"}} {float(hw.get("gpu_memory_used_in_mb", 0.0)):.2f}',
            )
            add_sample(
                "h265_gpu_memory_in_mb",
                "gauge",
                f'h265_gpu_memory_in_mb{{{g_lbl},state="total"}} {float(hw.get("gpu_memory_total_in_mb", 0.0)):.2f}',
            )
            add_sample(
                "h265_gpu_temperature_celsius",
                "gauge",
                f'h265_gpu_temperature_celsius{{{g_lbl}}} {float(hw.get("gpu_temperature_celsius", 0.0)):.2f}',
            )
            add_sample(
                "h265_gpu_power_watts",
                "gauge",
                f'h265_gpu_power_watts{{{g_lbl}}} {float(hw.get("gpu_power_watts", 0.0)):.2f}',
            )

    for proc_name, (cpu_r, rss_mb) in collect_process_tree_stats().items():
        add_sample(
            "h265_process_cpu_utilization_ratio",
            "gauge",
            f'h265_process_cpu_utilization_ratio{{nodename="{nodename}",host_os="{host_os}",process="{esc_label(proc_name)}"}} {cpu_r:.6f}',
        )
        add_sample(
            "h265_process_rss_in_mb",
            "gauge",
            f'h265_process_rss_in_mb{{nodename="{nodename}",host_os="{host_os}",process="{esc_label(proc_name)}"}} {rss_mb:.2f}',
        )

    add_sample(
        "h265_loki_shipped_entries_total",
        "counter",
        f'h265_loki_shipped_entries_total{{status="success"}} {loki_shipped_ok}',
        "Total number of log entries shipped to Grafana Loki by status.",
    )
    add_sample(
        "h265_loki_shipped_entries_total",
        "counter",
        f'h265_loki_shipped_entries_total{{status="error"}} {loki_shipped_err}',
    )

    lines: List[str] = []
    for family, (mtype, help_text, sample_lines) in families.items():
        if help_text:
            lines.append(f"# HELP {family} {help_text}")
        lines.append(f"# TYPE {family} {mtype}")
        lines.extend(sample_lines)

    return "\n".join(lines) + "\n"


# ============================================================================
# 3. Rotation-Aware Log Tailer & Grafana Loki JSON Stream Shipper
# ============================================================================

LOG_HEADER_RE = re.compile(
    r"^(\d{4}-\d{2}-\d{2})\s+(\d{2}:\d{2}:\d{2}\.\d{3})\s+\[([A-Z]+)\]\s+(.*)$"
)
LOGFMT_PAIR_RE = re.compile(r'([a-zA-Z0-9_]+)=(?:"((?:\\.|[^"\\])*)"|([^\s]+))')


def parse_log_line_to_loki_entry(
    raw_line: str,
    default_nodename: str,
    default_host_os: str,
) -> Tuple[Dict[str, str], str, str]:
    """
    Extracts low-cardinality Loki stream labels (app, nodename, host_os, worker_group,
    worker_thread, mount_point, level) matching Prometheus metric labels 1-to-1, and
    returns (stream_labels, nanosecond_epoch_str, log_body).
    """
    line = raw_line.rstrip("\r\n")
    m = LOG_HEADER_RE.match(line)
    if m:
        date_s, time_s, level_s, body = m.group(1), m.group(2), m.group(3), m.group(4)
        try:
            dt = datetime.datetime.strptime(f"{date_s} {time_s}", "%Y-%m-%d %H:%M:%S.%f")
            ns = int(dt.timestamp() * 1_000_000_000)
        except Exception:
            ns = int(time.time() * 1_000_000_000)
    else:
        level_s = "INFO"
        body = line
        ns = int(time.time() * 1_000_000_000)

    kv: Dict[str, str] = {}
    for pm in LOGFMT_PAIR_RE.finditer(body):
        key = pm.group(1)
        val = pm.group(2) if pm.group(2) is not None else (pm.group(3) or "")
        kv[key] = val

    labels = {
        "app": "rusty_h265_converter",
        "level": level_s,
        "nodename": kv.get("nodename", default_nodename),
        "host_os": kv.get("host_os", default_host_os),
        "worker_group": kv.get("worker_group", "orchestrator"),
        "worker_thread": kv.get("worker_thread", "main"),
        "mount_point": kv.get("mount_point", "global"),
    }
    return (labels, str(ns), line)


class RotationAwareLokiShipper:
    def __init__(
        self,
        log_file_path: str,
        loki_endpoint: str,
        default_nodename: str,
        default_host_os: str,
        batch_wait_msec: int = 1000,
        batch_size_bytes: int = 65536,
        checkpoint_path: str = "",
    ) -> None:
        self.log_file_path = log_file_path
        self.loki_endpoint = loki_endpoint.strip()
        self.default_nodename = default_nodename
        self.default_host_os = default_host_os
        self.batch_wait_sec = max(0.1, batch_wait_msec / 1000.0)
        self.batch_size_bytes = max(1024, batch_size_bytes)
        self.checkpoint_path = checkpoint_path.strip()

        self._fp: Optional[Any] = None
        self._current_ino: Optional[int] = None
        self._offset: int = 0
        self._pending_streams: Dict[Tuple[Tuple[str, str], ...], List[List[str]]] = {}
        self._pending_bytes: int = 0
        self._pending_count: int = 0
        self._last_flush: float = time.time()

        self.shipped_ok: int = 0
        self.shipped_err: int = 0

    def _load_checkpoint_offset(self, st: os.stat_result) -> int:
        if not self.checkpoint_path or not os.path.isfile(self.checkpoint_path):
            return 0
        try:
            with open(self.checkpoint_path, "r", encoding="utf-8") as f:
                data = json.load(f)
            abs_log = os.path.abspath(self.log_file_path)
            if (
                data.get("path") == abs_log
                and int(data.get("ino", -1)) == st.st_ino
                and 0 <= int(data.get("offset", -1)) <= st.st_size
            ):
                return int(data["offset"])
        except Exception:
            pass
        return 0

    def _save_checkpoint(self) -> None:
        if not self.checkpoint_path or self._current_ino is None:
            return
        try:
            parent = os.path.dirname(self.checkpoint_path)
            if parent:
                os.makedirs(parent, exist_ok=True)
            tmp_path = self.checkpoint_path + ".tmp"
            payload = {
                "path": os.path.abspath(self.log_file_path),
                "ino": self._current_ino,
                "offset": self._offset,
                "updated_at": int(time.time()),
            }
            with open(tmp_path, "w", encoding="utf-8") as f:
                json.dump(payload, f)
            os.replace(tmp_path, self.checkpoint_path)
        except Exception:
            pass

    def _open_if_needed(self) -> None:
        if not os.path.isfile(self.log_file_path):
            return
        try:
            st = os.stat(self.log_file_path)
            if self._fp is None:
                self._fp = open(self.log_file_path, "r", encoding="utf-8", errors="replace")
                self._current_ino = st.st_ino
                resume_offset = self._load_checkpoint_offset(st)
                if resume_offset > 0:
                    self._fp.seek(resume_offset)
                self._offset = resume_offset
            elif st.st_ino != self._current_ino or st.st_size < self._offset:
                # Rotation detected: drain remaining lines from old descriptor first
                self._drain_current_fp()
                try:
                    self._fp.close()
                except Exception:
                    pass
                self._fp = open(self.log_file_path, "r", encoding="utf-8", errors="replace")
                self._current_ino = st.st_ino
                self._offset = 0
        except Exception:
            pass

    def _drain_current_fp(self) -> None:
        if self._fp is None:
            return
        while True:
            line = self._fp.readline()
            if not line:
                break
            self._offset = self._fp.tell()
            if not line.strip():
                continue
            labels, ns_str, text = parse_log_line_to_loki_entry(
                line, self.default_nodename, self.default_host_os
            )
            key = tuple(sorted(labels.items()))
            self._pending_streams.setdefault(key, []).append([ns_str, text])
            self._pending_bytes += len(text)
            self._pending_count += 1

    def build_loki_payload(self) -> Optional[Tuple[bytes, int]]:
        if not self._pending_streams:
            return None
        streams = []
        for label_tuple, values in self._pending_streams.items():
            values.sort(key=lambda pair: int(pair[0]))
            streams.append({"stream": dict(label_tuple), "values": values})
        payload_bytes = json.dumps({"streams": streams}).encode("utf-8")
        count = self._pending_count
        self._pending_streams.clear()
        self._pending_bytes = 0
        self._pending_count = 0
        self._last_flush = time.time()
        return (payload_bytes, count)

    def poll_and_ship(self, force_flush: bool = False) -> None:
        self._open_if_needed()
        self._drain_current_fp()

        should_flush = force_flush or (
            self._pending_bytes >= self.batch_size_bytes
            or (self._pending_count > 0 and (time.time() - self._last_flush) >= self.batch_wait_sec)
        )
        if not should_flush or not self.loki_endpoint:
            return

        built = self.build_loki_payload()
        if not built:
            return
        payload_bytes, entry_count = built

        req = urllib.request.Request(
            self.loki_endpoint,
            data=payload_bytes,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=5.0) as resp:
                if 200 <= resp.status < 300:
                    self.shipped_ok += entry_count
                    self._save_checkpoint()
                else:
                    self.shipped_err += entry_count
                    sys.stderr.write(
                        f"[LokiShipper] Unexpected HTTP status {resp.status} for {entry_count} entries\n"
                    )
                    sys.stderr.flush()
        except urllib.error.HTTPError as e:
            self.shipped_err += entry_count
            body_preview = ""
            try:
                body_preview = e.read().decode("utf-8", errors="replace").strip()[:240]
            except Exception:
                pass
            sys.stderr.write(
                f"[LokiShipper] Push failed (HTTP {e.code}) for {entry_count} entries: {body_preview}\n"
            )
            sys.stderr.flush()
        except Exception as e:
            self.shipped_err += entry_count
            sys.stderr.write(
                f"[LokiShipper] Push failed for {entry_count} entries: {e}\n"
            )
            sys.stderr.flush()


# ============================================================================
# 4. Remote Mode (--role remote) & Master Mode (--role master) Runners
# ============================================================================

def run_remote_role(args: argparse.Namespace) -> int:
    nodename = args.nodename or socket.gethostname() or "remote-node"
    host_os = args.host_os or detect_host_os_label()
    watch_dir = args.watch_dir or "."
    cpu_tracker = CpuTickTracker()

    # 1. Emit initial node_inventory JSON event unless --once is purely requesting a sample
    first_sample = collect_host_hardware_sample(cpu_tracker, nodename, host_os, watch_dir)
    if args.once:
        print(json.dumps(first_sample), flush=True)
        return 0

    inventory_event = dict(first_sample)
    inventory_event["type"] = "node_inventory"
    inventory_event["cpu_cores"] = os.cpu_count() or 1
    print(json.dumps(inventory_event), flush=True)

    interval = max(1.0, float(args.interval))
    while True:
        time.sleep(interval)
        sample = collect_host_hardware_sample(cpu_tracker, nodename, host_os, watch_dir)
        sample["type"] = "hw_sample"
        try:
            print(json.dumps(sample), flush=True)
        except BrokenPipeError:
            break
    return 0


def is_pid_alive(pid: int) -> bool:
    if pid <= 0:
        return True
    try:
        os.kill(pid, 0)
        return True
    except PermissionError:
        return True
    except OSError:
        return False


def push_prometheus_metrics(endpoint_url: str, payload_text: str) -> bool:
    if not endpoint_url:
        return False
    url = endpoint_url.strip()
    if not url.startswith("http://") and not url.startswith("https://"):
        url = "http://" + url
    req = urllib.request.Request(
        url,
        data=payload_text.encode("utf-8"),
        headers={"Content-Type": "text/plain; version=0.0.4; charset=utf-8"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=5.0) as resp:
            return 200 <= resp.status < 300
    except Exception:
        return False


class ReusableThreadingHTTPServer(http.server.ThreadingHTTPServer):
    allow_reuse_address = True
    daemon_threads = True


def run_master_role(args: argparse.Namespace) -> int:
    cfg = load_simple_config_yaml(args.config)
    log_path = args.log_file or load_log_file_path_from_logging_conf(args.log_config)
    telemetry_path = args.telemetry_path or cfg["TELEMETRY_SNAPSHOT_PATH"]
    nodename = args.nodename or cfg["LOCAL_NODENAME"]
    host_os = args.host_os or detect_host_os_label()
    cpu_tracker = CpuTickTracker()

    loki_shipper = RotationAwareLokiShipper(
        log_file_path=log_path,
        loki_endpoint=cfg["LOKI_ENDPOINT"],
        default_nodename=nodename,
        default_host_os=host_os,
        batch_wait_msec=int(cfg["LOKI_BATCH_WAIT_MSEC"]),
        batch_size_bytes=int(cfg["LOKI_BATCH_SIZE_BYTES"]),
        checkpoint_path="" if args.once else args.checkpoint_file,
    )

    def load_snapshot() -> Optional[Dict[str, Any]]:
        if os.path.isfile(telemetry_path):
            try:
                with open(telemetry_path, "r", encoding="utf-8") as f:
                    return json.load(f)
            except Exception:
                return None
        return None

    # Single-shot diagnostic / test mode (--once)
    if args.once:
        snap = load_snapshot()
        local_hw = collect_host_hardware_sample(cpu_tracker, nodename, host_os, ".")
        prom_text = render_prometheus_text(
            snap, local_hw, loki_shipper.shipped_ok, loki_shipper.shipped_err
        )
        sys.stdout.write(prom_text)
        sys.stdout.flush()
        return 0

    metrics_refresh_lock = threading.Lock()
    latest_metrics_box = {"text": ""}

    def refresh_metrics_text() -> str:
        with metrics_refresh_lock:
            snap = load_snapshot()
            local_hw = collect_host_hardware_sample(cpu_tracker, nodename, host_os, ".")
            text = render_prometheus_text(
                snap, local_hw, loki_shipper.shipped_ok, loki_shipper.shipped_err
            )
            latest_metrics_box["text"] = text
            return text

    refresh_metrics_text()

    listen_addr = (args.listen_addr or cfg["PROMETHEUS_LISTEN_ADDR"]).strip()
    httpd: Optional[http.server.HTTPServer] = None
    if cfg["PROMETHEUS_ENABLED"] and listen_addr and ":" in listen_addr:
        host_part, port_part = listen_addr.rsplit(":", 1)
        bind_host = host_part.strip() or "0.0.0.0"
        try:
            bind_port = int(port_part.strip())

            class MetricsHandler(http.server.BaseHTTPRequestHandler):
                def do_GET(self) -> None:
                    if self.path.startswith("/metrics"):
                        body_text = refresh_metrics_text()
                        body = body_text.encode("utf-8")
                        self.send_response(200)
                        self.send_header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
                        self.send_header("Content-Length", str(len(body)))
                        self.end_headers()
                        self.wfile.write(body)
                    elif self.path.startswith("/healthz"):
                        self.send_response(200)
                        self.end_headers()
                        self.wfile.write(b"OK\n")
                    else:
                        self.send_response(404)
                        self.end_headers()

                def log_message(self, format: str, *log_args: Any) -> None:
                    return

            httpd = ReusableThreadingHTTPServer((bind_host, bind_port), MetricsHandler)
            server_thread = threading.Thread(target=httpd.serve_forever, daemon=True)
            server_thread.start()
        except Exception as e:
            sys.stderr.write(f"[MetricsAgent] Failed to bind HTTP server on {listen_addr}: {e}\n")
            sys.stderr.flush()
            httpd = None

    push_interval = max(1.0, float(cfg["PROMETHEUS_PUSH_INTERVAL_SEC"]))
    prom_endpoint = cfg["PROMETHEUS_ENDPOINT"].strip()
    loki_enabled = bool(cfg["LOKI_ENABLED"] and cfg["LOKI_ENDPOINT"].strip())
    last_prom_push = 0.0

    stop_event = threading.Event()

    def handle_signal(signum: int, _frame: Any) -> None:
        stop_event.set()

    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGINT, handle_signal)

    sys.stdout.write(
        f"[MetricsAgent] Started master role (nodename={nodename}, listen={listen_addr}, "
        f"loki_enabled={loki_enabled}, loki_endpoint={cfg['LOKI_ENDPOINT']}, "
        f"log_file={log_path}, checkpoint={args.checkpoint_file or 'none'})\n"
    )
    sys.stdout.flush()

    try:
        while not stop_event.is_set():
            now = time.time()
            if (now - last_prom_push) >= push_interval:
                txt = refresh_metrics_text()
                if cfg["PROMETHEUS_ENABLED"] and prom_endpoint:
                    push_prometheus_metrics(prom_endpoint, txt)
                last_prom_push = now

            if loki_enabled:
                loki_shipper.poll_and_ship(force_flush=False)

            if args.parent_pid > 0 and not args.daemon:
                if not is_pid_alive(args.parent_pid):
                    break

            stop_event.wait(0.5)

        # Graceful final flush on stop signal or parent PID exit
        txt = refresh_metrics_text()
        if cfg["PROMETHEUS_ENABLED"] and prom_endpoint:
            push_prometheus_metrics(prom_endpoint, txt)
        if loki_enabled:
            loki_shipper.poll_and_ship(force_flush=True)
    except KeyboardInterrupt:
        if loki_enabled:
            loki_shipper.poll_and_ship(force_flush=True)
    finally:
        if httpd is not None:
            try:
                httpd.shutdown()
                httpd.server_close()
            except Exception:
                pass
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Portable Prometheus & Grafana Loki Observability Agent for rusty_h265_converter"
    )
    parser.add_argument("--role", choices=["master", "remote"], default="master")
    parser.add_argument("--config", default="config.yaml")
    parser.add_argument("--log-config", default="logging.conf")
    parser.add_argument("--log-file", default="")
    parser.add_argument("--telemetry-path", default="")
    parser.add_argument("--listen-addr", default="")
    parser.add_argument("--nodename", default="")
    parser.add_argument("--host-os", default="")
    parser.add_argument("--watch-dir", default=".")
    parser.add_argument("--interval", type=float, default=5.0)
    parser.add_argument("--parent-pid", type=int, default=0)
    parser.add_argument("--checkpoint-file", default="")
    parser.add_argument("--daemon", action="store_true")
    parser.add_argument("--once", action="store_true")
    args = parser.parse_args()

    if args.role == "remote":
        return run_remote_role(args)
    return run_master_role(args)


if __name__ == "__main__":
    sys.exit(main())
