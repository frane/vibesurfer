//! Stable per-caller key. Identifies the process tree that invoked
//! `vs` so different shells / agents get different auto-sessions.
//!
//! Key is `<parent_pid>-<parent_start_time>`. Parent start time
//! disambiguates PID reuse: even if the OS recycles a PID after a
//! parent exits, the new process has a different start time.

#[cfg(unix)]
fn parent_pid() -> u32 {
    #[allow(clippy::cast_sign_loss)]
    let pid = unsafe { libc::getppid() } as u32;
    pid
}

#[cfg(windows)]
fn parent_pid() -> u32 {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::GetCurrentProcessId;
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return 0;
        };
        let me = GetCurrentProcessId();
        let mut entry = PROCESSENTRY32W {
            dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0),
            ..PROCESSENTRY32W::default()
        };
        let mut found = 0;
        if Process32FirstW(snap, &raw mut entry).is_ok() {
            loop {
                if entry.th32ProcessID == me {
                    found = entry.th32ParentProcessID;
                    break;
                }
                if Process32NextW(snap, &raw mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

#[cfg(target_os = "macos")]
fn parent_start_time(ppid: u32) -> Option<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    #[allow(clippy::cast_possible_wrap)]
    let ret = unsafe {
        libc::proc_pidinfo(
            ppid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::from_mut(&mut info).cast::<libc::c_void>(),
            i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).unwrap_or(0),
        )
    };
    if ret > 0 {
        Some(info.pbi_start_tvsec)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn parent_start_time(ppid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{ppid}/stat")).ok()?;
    // The "comm" field at index 1 may contain spaces and parens; skip
    // past the final ')' before splitting on whitespace. `starttime`
    // is field 22 of the original record, which is index 19 after the
    // closing paren (state is the first remaining field).
    let after_paren = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_paren.split_whitespace().collect();
    fields.get(19)?.parse().ok()
}

#[cfg(target_os = "windows")]
fn parent_start_time(ppid: u32) -> Option<u64> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, ppid).ok()?;
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let r = GetProcessTimes(
            h,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        );
        let _ = CloseHandle(h);
        r.ok()?;
        let high = u64::from(creation.dwHighDateTime);
        let low = u64::from(creation.dwLowDateTime);
        Some((high << 32) | low)
    }
}

/// Return the stable caller key, or `None` if either the parent PID
/// could not be read (extremely rare; would mean the kernel refused
/// to answer). The key is filename-safe: only digits and a single
/// hyphen.
#[must_use]
pub fn caller_key() -> Option<String> {
    let ppid = parent_pid();
    if ppid == 0 {
        return None;
    }
    let start = parent_start_time(ppid).unwrap_or(0);
    Some(format!("{ppid}-{start}"))
}
