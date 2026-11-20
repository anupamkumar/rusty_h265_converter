#[cfg(windows)]
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

/// Cross-platform local calendar/clock components returned by `local_time_parts()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTimeParts {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub millis: u32,
}

/// Returns the default Python launcher command name for the current host OS.
pub fn python_command_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "python"
    } else {
        "python3"
    }
}

/// Strips the Windows verbatim path prefix (`\\?\` or `\\?\UNC\`) for human-readable
/// drive/mount parsing, while leaving Unix paths untouched.
pub fn strip_windows_verbatim_prefix(path_str: &str) -> String {
    if let Some(rest) = path_str.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{}", rest)
    } else if let Some(rest) = path_str.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path_str.to_string()
    }
}

/// Extracts `(mount_point, device_label)` from a Windows path string (e.g. `C:\Videos\foo` -> `("C:\", "C:")`,
/// `\\server\share\dir` -> `("\\server\share", "\\server\share")`). Pure string helper usable in tests on all OSes.
pub fn parse_windows_mount_root(raw_path: &str) -> (String, String) {
    let cleaned = strip_windows_verbatim_prefix(raw_path);
    let bytes = cleaned.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let drive_letter = (bytes[0] as char).to_ascii_uppercase();
        return (format!("{}:\\", drive_letter), format!("{}:", drive_letter));
    }
    if let Some(unc_rest) = cleaned.strip_prefix(r"\\") {
        let parts: Vec<&str> = unc_rest
            .split(|c| c == '\\' || c == '/')
            .filter(|s| !s.is_empty())
            .collect();
        if parts.len() >= 2 {
            let root = format!(r"\\{}\{}", parts[0], parts[1]);
            return (root.clone(), root);
        }
    }
    (r"C:\".to_string(), "C:".to_string())
}

// ============================================================================
// Unix Implementation (macOS & Linux)
// ============================================================================
#[cfg(unix)]
mod os_impl {
    use super::*;
    use std::fs::File;
    use std::io::Read;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: isize,
        tm_zone: *const std::ffi::c_char,
    }

    extern "C" {
        fn localtime_r(time_p: *const i64, result: *mut Tm) -> *mut Tm;
        fn setpriority(which: i32, who: u32, prio: i32) -> i32;
        fn ioctl(fd: i32, request: usize, ...) -> i32;
        fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
        fn kill(pid: i32, sig: i32) -> i32;
        fn tcgetattr(fd: i32, termios_p: *mut Termios) -> i32;
        fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const Termios) -> i32;
    }

    const SIGHUP_NUM: i32 = 1;
    const SIGINT_NUM: i32 = 2;
    const SIGQUIT_NUM: i32 = 3;
    const SIGKILL_NUM: i32 = 9;
    const SIGTERM_NUM: i32 = 15;

    static SIGNAL_TARGET_FLAG: OnceLock<&'static AtomicBool> = OnceLock::new();
    static SIGNAL_TARGET_NUM: OnceLock<&'static AtomicUsize> = OnceLock::new();

    extern "C" fn handle_termination_signal(sig: i32) {
        if let Some(num) = SIGNAL_TARGET_NUM.get() {
            num.store(sig as usize, Ordering::SeqCst);
        }
        if let Some(flag) = SIGNAL_TARGET_FLAG.get() {
            flag.store(true, Ordering::SeqCst);
        }
    }

    pub fn local_time_parts() -> Option<LocalTimeParts> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs() as i64;
        let millis = now.subsec_millis();
        unsafe {
            let mut tm = Tm {
                tm_sec: 0,
                tm_min: 0,
                tm_hour: 0,
                tm_mday: 1,
                tm_mon: 0,
                tm_year: 70,
                tm_wday: 0,
                tm_yday: 0,
                tm_isdst: 0,
                tm_gmtoff: 0,
                tm_zone: std::ptr::null(),
            };
            if !localtime_r(&secs as *const i64, &mut tm as *mut Tm).is_null() {
                return Some(LocalTimeParts {
                    year: tm.tm_year + 1900,
                    month: (tm.tm_mon + 1).max(1) as u32,
                    day: tm.tm_mday.max(1) as u32,
                    hour: tm.tm_hour.max(0) as u32,
                    minute: tm.tm_min.max(0) as u32,
                    second: tm.tm_sec.max(0) as u32,
                    millis,
                });
            }
        }
        None
    }

    pub fn install_os_signal_handlers(
        shutdown_flag: &'static AtomicBool,
        signal_num: &'static AtomicUsize,
    ) {
        let _ = SIGNAL_TARGET_FLAG.set(shutdown_flag);
        let _ = SIGNAL_TARGET_NUM.set(signal_num);
        unsafe {
            signal(SIGHUP_NUM, handle_termination_signal);
            signal(SIGINT_NUM, handle_termination_signal);
            signal(SIGQUIT_NUM, handle_termination_signal);
            signal(SIGTERM_NUM, handle_termination_signal);
        }
    }

    pub fn configure_child_process_group(cmd: &mut Command) {
        cmd.process_group(0);
    }

    pub fn terminate_process_tree(pid: u32, force: bool) {
        let p = pid as i32;
        if p <= 0 {
            return;
        }
        let sig = if force { SIGKILL_NUM } else { SIGTERM_NUM };
        unsafe {
            let _ = kill(-p, sig);
            let _ = kill(p, sig);
        }
    }

    pub fn set_child_process_priority(pid: u32, nice_value: i32) -> Result<(), std::io::Error> {
        if nice_value == 0 {
            return Ok(());
        }
        let rc = unsafe { setpriority(0, pid, nice_value) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(target_os = "macos")]
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Termios {
        c_iflag: u64,
        c_oflag: u64,
        c_cflag: u64,
        c_lflag: u64,
        c_cc: [u8; 20],
        c_ispeed: u64,
        c_ospeed: u64,
    }

    #[cfg(target_os = "macos")]
    const ICANON_FLAG: u64 = 0x0000_0100;
    #[cfg(target_os = "macos")]
    const ECHO_FLAG: u64 = 0x0000_0008;
    #[cfg(target_os = "macos")]
    const VMIN_IDX: usize = 16;
    #[cfg(target_os = "macos")]
    const VTIME_IDX: usize = 17;

    #[cfg(not(target_os = "macos"))]
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Termios {
        c_iflag: u32,
        c_oflag: u32,
        c_cflag: u32,
        c_lflag: u32,
        c_line: u8,
        c_cc: [u8; 32],
        c_ispeed: u32,
        c_ospeed: u32,
    }

    #[cfg(not(target_os = "macos"))]
    const ICANON_FLAG: u32 = 0x0002;
    #[cfg(not(target_os = "macos"))]
    const ECHO_FLAG: u32 = 0x0008;
    #[cfg(not(target_os = "macos"))]
    const VMIN_IDX: usize = 6;
    #[cfg(not(target_os = "macos"))]
    const VTIME_IDX: usize = 5;

    const TCSANOW_OPT: i32 = 0;

    #[repr(C)]
    struct WinSize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: usize = 0x4008_7468;

    #[cfg(not(target_os = "macos"))]
    const TIOCGWINSZ: usize = 0x5413;

    pub fn get_os_terminal_dimensions() -> Option<(usize, usize)> {
        unsafe {
            for fd in [1i32, 2i32, 0i32] {
                let mut ws = WinSize {
                    ws_row: 0,
                    ws_col: 0,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                if ioctl(fd, TIOCGWINSZ, &mut ws as *mut WinSize) == 0 && ws.ws_col >= 40 {
                    let rows = if ws.ws_row >= 10 {
                        ws.ws_row as usize
                    } else {
                        50
                    };
                    return Some((ws.ws_col as usize, rows));
                }
            }
        }
        None
    }

    pub struct RawTerminalReader {
        file: File,
    }

    impl RawTerminalReader {
        pub fn read_chunk(&mut self, buf: &mut [u8]) -> usize {
            self.file.read(buf).unwrap_or(0)
        }
    }

    pub struct RawTerminalGuard {
        tty_file: Option<File>,
        tty_fd: i32,
        orig_termios: Option<Termios>,
    }

    impl RawTerminalGuard {
        pub fn enable() -> (Self, Option<RawTerminalReader>) {
            let tty_file = File::open("/dev/tty").ok();
            let tty_fd = tty_file.as_ref().map(|f| f.as_raw_fd()).unwrap_or(0);

            let mut orig_termios: Option<Termios> = None;
            unsafe {
                let mut tio: Termios = std::mem::zeroed();
                if tcgetattr(tty_fd, &mut tio as *mut Termios) == 0 {
                    orig_termios = Some(tio);
                    let mut raw = tio;
                    raw.c_lflag &= !(ICANON_FLAG | ECHO_FLAG);
                    raw.c_cc[VMIN_IDX] = 0;
                    raw.c_cc[VTIME_IDX] = 1; // 100ms non-blocking read timeout
                    let _ = tcsetattr(tty_fd, TCSANOW_OPT, &raw as *const Termios);
                }
            }

            let reader = if orig_termios.is_some() {
                tty_file
                    .as_ref()
                    .and_then(|f| f.try_clone().ok())
                    .map(|file| RawTerminalReader { file })
            } else {
                None
            };

            (
                Self {
                    tty_file,
                    tty_fd,
                    orig_termios,
                },
                reader,
            )
        }
    }

    impl Drop for RawTerminalGuard {
        fn drop(&mut self) {
            if let Some(ref orig) = self.orig_termios {
                unsafe {
                    let _ = tcsetattr(self.tty_fd, TCSANOW_OPT, orig as *const Termios);
                }
            }
            drop(self.tty_file.take());
        }
    }
}

// ============================================================================
// Windows Implementation (Win32 kernel32 FFI — zero external crates)
// ============================================================================
#[cfg(windows)]
mod os_impl {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    const STD_INPUT_HANDLE: u32 = (-10i32) as u32;
    const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;
    const INVALID_HANDLE_VALUE: isize = -1;

    const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
    const ENABLE_LINE_INPUT: u32 = 0x0002;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const PROCESS_TERMINATE: u32 = 0x0001;
    const PROCESS_SET_INFORMATION: u32 = 0x0200;
    const HIGH_PRIORITY_CLASS: u32 = 0x0000_0080;
    const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

    const WAIT_OBJECT_0: u32 = 0;
    const KEY_EVENT: u16 = 0x0001;

    const VK_PRIOR: u16 = 0x21; // Page Up
    const VK_NEXT: u16 = 0x22;  // Page Down
    const VK_END: u16 = 0x23;
    const VK_HOME: u16 = 0x24;
    const VK_LEFT: u16 = 0x25;
    const VK_UP: u16 = 0x26;
    const VK_RIGHT: u16 = 0x27;
    const VK_DOWN: u16 = 0x28;
    const VK_ESCAPE: u16 = 0x1B;

    const DRIVE_REMOVABLE: u32 = 2;

    #[repr(C)]
    struct SystemTime {
        w_year: u16,
        w_month: u16,
        w_day_of_week: u16,
        w_day: u16,
        w_hour: u16,
        w_minute: u16,
        w_second: u16,
        w_milliseconds: u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Coord {
        x: i16,
        y: i16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SmallRect {
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
    }

    #[repr(C)]
    struct ConsoleScreenBufferInfo {
        dw_size: Coord,
        dw_cursor_position: Coord,
        w_attributes: u16,
        sr_window: SmallRect,
        dw_maximum_window_size: Coord,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KeyEventRecord {
        b_key_down: i32,
        w_repeat_count: u16,
        w_virtual_key_code: u16,
        w_virtual_scan_code: u16,
        u_char: u16,
        dw_control_key_state: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InputRecord {
        event_type: u16,
        _padding: u16,
        key_event: KeyEventRecord,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetLocalTime(lp_system_time: *mut SystemTime);
        fn SetConsoleCtrlHandler(
            handler_routine: Option<extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
        fn OpenProcess(dw_desired_access: u32, b_inherit_handle: i32, dw_process_id: u32) -> isize;
        fn TerminateProcess(h_process: isize, u_exit_code: u32) -> i32;
        fn SetPriorityClass(h_process: isize, dw_priority_class: u32) -> i32;
        fn CloseHandle(h_object: isize) -> i32;
        fn GetStdHandle(n_std_handle: u32) -> isize;
        fn GetConsoleMode(h_console_handle: isize, lp_mode: *mut u32) -> i32;
        fn SetConsoleMode(h_console_handle: isize, dw_mode: u32) -> i32;
        fn GetConsoleScreenBufferInfo(
            h_console_output: isize,
            lp_console_screen_buffer_info: *mut ConsoleScreenBufferInfo,
        ) -> i32;
        fn WaitForSingleObject(h_handle: isize, dw_milliseconds: u32) -> u32;
        fn ReadConsoleInputW(
            h_console_input: isize,
            lp_buffer: *mut InputRecord,
            n_length: u32,
            lp_number_of_events_read: *mut u32,
        ) -> i32;
        fn GetDriveTypeW(lp_root_path_name: *const u16) -> u32;
    }

    static SIGNAL_TARGET_FLAG: OnceLock<&'static AtomicBool> = OnceLock::new();
    static SIGNAL_TARGET_NUM: OnceLock<&'static AtomicUsize> = OnceLock::new();

    extern "system" fn console_ctrl_handler(ctrl_type: u32) -> i32 {
        // CTRL_C_EVENT=0, CTRL_BREAK_EVENT=1, CTRL_CLOSE_EVENT=2, CTRL_LOGOFF_EVENT=5, CTRL_SHUTDOWN_EVENT=6
        if let Some(num) = SIGNAL_TARGET_NUM.get() {
            num.store((ctrl_type as usize) + 1, Ordering::SeqCst);
        }
        if let Some(flag) = SIGNAL_TARGET_FLAG.get() {
            flag.store(true, Ordering::SeqCst);
        }
        1 // Handled: allow graceful shutdown instead of immediate process kill
    }

    pub fn local_time_parts() -> Option<LocalTimeParts> {
        unsafe {
            let mut st: SystemTime = std::mem::zeroed();
            GetLocalTime(&mut st as *mut SystemTime);
            if st.w_year >= 1970 {
                return Some(LocalTimeParts {
                    year: st.w_year as i32,
                    month: st.w_month as u32,
                    day: st.w_day as u32,
                    hour: st.w_hour as u32,
                    minute: st.w_minute as u32,
                    second: st.w_second as u32,
                    millis: st.w_milliseconds as u32,
                });
            }
        }
        None
    }

    pub fn install_os_signal_handlers(
        shutdown_flag: &'static AtomicBool,
        signal_num: &'static AtomicUsize,
    ) {
        let _ = SIGNAL_TARGET_FLAG.set(shutdown_flag);
        let _ = SIGNAL_TARGET_NUM.set(signal_num);
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(console_ctrl_handler), 1);
        }
    }

    pub fn configure_child_process_group(cmd: &mut Command) {
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    pub fn terminate_process_tree(pid: u32, force: bool) {
        if pid == 0 {
            return;
        }
        if force {
            let _ = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if h != 0 && h != INVALID_HANDLE_VALUE {
                let _ = TerminateProcess(h, 1);
                let _ = CloseHandle(h);
            }
        }
    }

    pub fn set_child_process_priority(pid: u32, nice_value: i32) -> Result<(), std::io::Error> {
        if nice_value == 0 {
            return Ok(());
        }
        let prio_class = if nice_value <= -10 {
            HIGH_PRIORITY_CLASS
        } else if nice_value < 0 {
            ABOVE_NORMAL_PRIORITY_CLASS
        } else {
            BELOW_NORMAL_PRIORITY_CLASS
        };
        unsafe {
            let h = OpenProcess(PROCESS_SET_INFORMATION, 0, pid);
            if h == 0 || h == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error());
            }
            let ok = SetPriorityClass(h, prio_class) != 0;
            let err = if ok {
                None
            } else {
                Some(std::io::Error::last_os_error())
            };
            let _ = CloseHandle(h);
            match err {
                None => Ok(()),
                Some(e) => Err(e),
            }
        }
    }

    pub fn get_os_terminal_dimensions() -> Option<(usize, usize)> {
        unsafe {
            let h_out = GetStdHandle(STD_OUTPUT_HANDLE);
            if h_out == 0 || h_out == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut info: ConsoleScreenBufferInfo = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(h_out, &mut info as *mut ConsoleScreenBufferInfo) != 0 {
                let cols = (info.sr_window.right - info.sr_window.left + 1) as isize;
                let rows = (info.sr_window.bottom - info.sr_window.top + 1) as isize;
                if cols >= 40 {
                    let eff_rows = if rows >= 10 { rows as usize } else { 50 };
                    return Some((cols as usize, eff_rows));
                }
            }
        }
        None
    }

    pub fn get_windows_mount_info(target: &Path) -> (String, String, bool) {
        let raw = target.to_string_lossy();
        let (mount_point, device) = parse_windows_mount_root(&raw);
        let mut wide: Vec<u16> = std::ffi::OsStr::new(&mount_point)
            .encode_wide()
            .collect();
        wide.push(0);
        let dtype = unsafe { GetDriveTypeW(wide.as_ptr()) };
        let is_removable_hdd = dtype == DRIVE_REMOVABLE;
        (mount_point, device, is_removable_hdd)
    }

    pub struct RawTerminalReader {
        stdin_handle: isize,
    }

    impl RawTerminalReader {
        pub fn read_chunk(&mut self, buf: &mut [u8]) -> usize {
            if self.stdin_handle == 0 || self.stdin_handle == INVALID_HANDLE_VALUE || buf.is_empty() {
                return 0;
            }
            unsafe {
                if WaitForSingleObject(self.stdin_handle, 100) != WAIT_OBJECT_0 {
                    return 0;
                }
                let mut records = [InputRecord {
                    event_type: 0,
                    _padding: 0,
                    key_event: KeyEventRecord {
                        b_key_down: 0,
                        w_repeat_count: 0,
                        w_virtual_key_code: 0,
                        w_virtual_scan_code: 0,
                        u_char: 0,
                        dw_control_key_state: 0,
                    },
                }; 8];
                let mut read_count: u32 = 0;
                if ReadConsoleInputW(
                    self.stdin_handle,
                    records.as_mut_ptr(),
                    records.len() as u32,
                    &mut read_count as *mut u32,
                ) == 0
                {
                    return 0;
                }
                let mut written = 0usize;
                for rec in &records[..(read_count as usize).min(records.len())] {
                    if rec.event_type != KEY_EVENT || rec.key_event.b_key_down == 0 {
                        continue;
                    }
                    let seq: &[u8] = match rec.key_event.w_virtual_key_code {
                        VK_UP => b"\x1b[A",
                        VK_DOWN => b"\x1b[B",
                        VK_RIGHT => b"\x1b[C",
                        VK_LEFT => b"\x1b[D",
                        VK_PRIOR => b"\x1b[5~",
                        VK_NEXT => b"\x1b[6~",
                        VK_HOME => b"\x1b[H",
                        VK_END => b"\x1b[F",
                        VK_ESCAPE => b"\x1b",
                        _ => {
                            let ch = rec.key_event.u_char;
                            if ch > 0 && ch < 128 && written < buf.len() {
                                buf[written] = ch as u8;
                                written += 1;
                            }
                            continue;
                        }
                    };
                    if written + seq.len() <= buf.len() {
                        buf[written..written + seq.len()].copy_from_slice(seq);
                        written += seq.len();
                    }
                }
                written
            }
        }
    }

    pub struct RawTerminalGuard {
        stdin_handle: isize,
        stdout_handle: isize,
        orig_in_mode: Option<u32>,
        orig_out_mode: Option<u32>,
    }

    impl RawTerminalGuard {
        pub fn enable() -> (Self, Option<RawTerminalReader>) {
            unsafe {
                let h_in = GetStdHandle(STD_INPUT_HANDLE);
                let h_out = GetStdHandle(STD_OUTPUT_HANDLE);

                let mut orig_out_mode = None;
                if h_out != 0 && h_out != INVALID_HANDLE_VALUE {
                    let mut mode: u32 = 0;
                    if GetConsoleMode(h_out, &mut mode as *mut u32) != 0 {
                        orig_out_mode = Some(mode);
                        let _ = SetConsoleMode(h_out, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
                    }
                }

                let mut orig_in_mode = None;
                if h_in != 0 && h_in != INVALID_HANDLE_VALUE {
                    let mut mode: u32 = 0;
                    if GetConsoleMode(h_in, &mut mode as *mut u32) != 0 {
                        orig_in_mode = Some(mode);
                        let raw = (mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT))
                            | ENABLE_PROCESSED_INPUT;
                        let _ = SetConsoleMode(h_in, raw);
                    }
                }

                let reader = if orig_in_mode.is_some() {
                    Some(RawTerminalReader { stdin_handle: h_in })
                } else {
                    None
                };

                (
                    Self {
                        stdin_handle: h_in,
                        stdout_handle: h_out,
                        orig_in_mode,
                        orig_out_mode,
                    },
                    reader,
                )
            }
        }
    }

    impl Drop for RawTerminalGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(m) = self.orig_in_mode {
                    let _ = SetConsoleMode(self.stdin_handle, m);
                }
                if let Some(m) = self.orig_out_mode {
                    let _ = SetConsoleMode(self.stdout_handle, m);
                }
            }
        }
    }
}

pub use os_impl::{
    configure_child_process_group, get_os_terminal_dimensions, install_os_signal_handlers,
    local_time_parts, set_child_process_priority, terminate_process_tree, RawTerminalGuard,
    RawTerminalReader,
};

#[cfg(windows)]
pub use os_impl::get_windows_mount_info;
