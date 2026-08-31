//! Stable per-caller key. Identifies the process tree that invoked
//! `vs` so different shells / agents get different auto-sessions.
//!
//! Resolution order:
//!
//!   1. `VS_CALLER` — an explicit, durable name (`named-<name>`).
//!   2. POSIX session id (`sid-<sid>`) on Unix. This is the OS's own
//!      notion of "the shell I belong to" and, unlike the parent pid,
//!      survives command substitution, nested shells and pipelines.
//!   3. `<parent_pid>-<parent_start_time>` as a fallback (Windows, or
//!      a process with no session id). Parent start time disambiguates
//!      PID reuse: even if the OS recycles a PID after a parent exits,
//!      the new process has a different start time.
//!
//! Step 2 exists because step 3 alone was too fine-grained to be an
//! identity: a command-substitution subshell is a different process
//! from its shell, so `P=$(vs open …)` bound its page to a session
//! that the next `vs view $P` could not see.

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

/// Return the stable caller key. `VS_CALLER=<name>` takes priority:
/// a durable identity that survives process restarts, so a relaunched
/// agent or host app rebinds to the session it had before instead of
/// silently getting a fresh one (pid-based keys die with the process
/// — the daemon kept the session, but nothing ever reconnected to
/// it). Set it once in an MCP server config or an agent's env. The
/// name is sanitized to filename-safe chars and prefixed so it can
/// never collide with a pid key.
///
/// Fallback is `<parent_pid>-<parent_start_time>`; `None` only if the
/// kernel refused to answer (extremely rare).
#[must_use]
pub fn caller_key() -> Option<String> {
    if let Ok(name) = std::env::var("VS_CALLER") {
        let clean: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(64)
            .collect();
        if !clean.is_empty() {
            return Some(format!("named-{clean}"));
        }
    }
    // Prefer the POSIX session id. The parent pid is too fine-grained
    // to be a caller identity: `P=$(vs open …)` runs `vs` inside a
    // command-substitution subshell, so every capture had a different
    // parent and therefore its own auto-session. The documented flow
    //
    //     P=$(vs open URL)
    //     vs view $P            -> ! WRONG_SESSION
    //
    // could not work, and each call leaked a session row plus a file
    // under `callers/` (301 of them on the author's machine).
    //
    // The session id is what "the shell I am running under" actually
    // means to the OS: identical across command substitution, nested
    // shells and pipelines, distinct between terminals and between
    // separately-launched agents. Verified across all four shapes.
    #[cfg(unix)]
    {
        let sid = unsafe { libc::getsid(0) };
        if sid > 0 {
            return Some(format!("sid-{sid}"));
        }
    }
    // Fallback: no session id (Windows, or a process with none).
    let ppid = parent_pid();
    if ppid == 0 {
        return None;
    }
    let start = parent_start_time(ppid).unwrap_or(0);
    Some(format!("{ppid}-{start}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn vs_caller_env_overrides_pid_key() {
        // Serial-safe: no other test reads VS_CALLER.
        std::env::set_var("VS_CALLER", "claude-desktop!!");
        let k = super::caller_key().expect("key");
        assert_eq!(k, "named-claude-desktop", "sanitized, prefixed: {k}");
        std::env::set_var("VS_CALLER", "///");
        let k = super::caller_key().expect("key");
        assert!(
            k.chars().next().is_some_and(char::is_numeric),
            "empty after sanitize falls back to pid key: {k}"
        );
        std::env::remove_var("VS_CALLER");
    }
}
