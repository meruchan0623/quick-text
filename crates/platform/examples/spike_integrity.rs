//! S-29 spike, item 2: UIPI pre-detection through the token integrity level.
//!
//! READ-ONLY. This program never sends input, never installs a hook, never
//! changes focus and never creates a window. It only asks the OS three
//! questions about a process:
//!
//! 1. `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`
//! 2. `OpenProcessToken(TOKEN_QUERY)`
//! 3. `GetTokenInformation(TokenIntegrityLevel)` -> last sub-authority of the
//!    label SID is the integrity RID
//!
//! and compares the answer with the level of this process.
//!
//! Usage:
//!   cargo run -p quicker-platform --example spike_integrity            # foreground window's process
//!   cargo run -p quicker-platform --example spike_integrity -- <PID>   # explicit PID
//!   cargo run -p quicker-platform --example spike_integrity -- <PID> --bench 2000
//!   cargo run -p quicker-platform --example spike_integrity -- --scan  # counts only
//!   cargo run -p quicker-platform --example spike_integrity -- --system # own elevation / UIAccess + foreground lock settings
//!
//! Output intentionally contains only the process name and the level, never a
//! window title (see docs/spikes/s-29.md).

#[cfg(not(windows))]
fn main() {
    eprintln!("spike_integrity requires Windows.");
}

#[cfg(windows)]
fn main() {
    imp::run();
}

#[cfg(windows)]
mod imp {
    use std::collections::BTreeMap;
    use std::ptr::null_mut;
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenElevation,
        TokenIntegrityLevel, TokenUIAccess, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
        QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, SystemParametersInfoW,
        SPI_GETFOREGROUNDFLASHCOUNT, SPI_GETFOREGROUNDLOCKTIMEOUT,
    };

    const ERROR_ACCESS_DENIED: u32 = 5;
    const ERROR_INVALID_PARAMETER: u32 = 87;

    /// Where the query chain stopped.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum Stage {
        OpenProcess,
        OpenProcessToken,
        GetTokenInformation,
        ParseSid,
    }

    impl Stage {
        fn name(self) -> &'static str {
            match self {
                Stage::OpenProcess => "OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)",
                Stage::OpenProcessToken => "OpenProcessToken(TOKEN_QUERY)",
                Stage::GetTokenInformation => "GetTokenInformation(TokenIntegrityLevel)",
                Stage::ParseSid => "parse label SID",
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Probe {
        /// Integrity RID read successfully. `elevated` / `uiaccess` are only
        /// filled in when the caller asked for detail (they cost two more
        /// GetTokenInformation calls and are not part of the B-17 chain).
        Level {
            rid: u32,
            elevated: Option<bool>,
            uiaccess: Option<bool>,
        },
        /// The chain failed at `stage` with Win32 error `code`.
        Unreadable { stage: Stage, code: u32 },
    }

    /// Well-known integrity RIDs (Mandatory Integrity Control / Well-known SIDs).
    fn level_name(rid: u32) -> &'static str {
        match rid {
            0x0000 => "Untrusted",
            0x1000 => "Low",
            0x2000 => "Medium",
            0x2100 => "Medium+",
            0x3000 => "High",
            0x4000 => "System",
            0x5000 => "Protected",
            _ => "Other",
        }
    }

    /// Reads the integrity RID of the token behind `token`.
    ///
    /// # Safety
    /// `token` must be a valid token handle opened with `TOKEN_QUERY`.
    unsafe fn read_rid(token: HANDLE, detail: bool) -> Probe {
        let mut needed = 0u32;
        // First call only sizes the buffer; ERROR_INSUFFICIENT_BUFFER is expected.
        GetTokenInformation(token, TokenIntegrityLevel, null_mut(), 0, &mut needed);
        if needed == 0 {
            return Probe::Unreadable {
                stage: Stage::GetTokenInformation,
                code: GetLastError(),
            };
        }
        // u64 elements keep the buffer 8-byte aligned for TOKEN_MANDATORY_LABEL.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let ok = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        );
        if ok == 0 {
            return Probe::Unreadable {
                stage: Stage::GetTokenInformation,
                code: GetLastError(),
            };
        }
        let label = &*(buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>());
        let sid = label.Label.Sid;
        if sid.is_null() {
            return Probe::Unreadable {
                stage: Stage::ParseSid,
                code: 0,
            };
        }
        let count_ptr = GetSidSubAuthorityCount(sid);
        if count_ptr.is_null() || *count_ptr == 0 {
            return Probe::Unreadable {
                stage: Stage::ParseSid,
                code: GetLastError(),
            };
        }
        let rid_ptr = GetSidSubAuthority(sid, u32::from(*count_ptr) - 1);
        if rid_ptr.is_null() {
            return Probe::Unreadable {
                stage: Stage::ParseSid,
                code: GetLastError(),
            };
        }
        let (elevated, uiaccess) = if detail {
            (
                token_dword(token, TokenElevation).map(|v| v != 0),
                token_dword(token, TokenUIAccess).map(|v| v != 0),
            )
        } else {
            (None, None)
        };
        Probe::Level {
            rid: *rid_ptr,
            elevated,
            uiaccess,
        }
    }

    /// A DWORD-sized token property (TokenElevation / TokenUIAccess).
    ///
    /// # Safety
    /// `token` must be a valid token handle opened with `TOKEN_QUERY`.
    unsafe fn token_dword(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Option<u32> {
        let mut value = 0u32;
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            class,
            (&mut value as *mut u32).cast(),
            4,
            &mut returned,
        );
        (ok != 0).then_some(value)
    }

    /// The whole query chain for one PID. Every handle is closed before return.
    fn probe_pid(pid: u32, detail: bool) -> Probe {
        // SAFETY: plain query calls; handles are closed on every path.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return Probe::Unreadable {
                    stage: Stage::OpenProcess,
                    code: GetLastError(),
                };
            }
            let mut token: HANDLE = null_mut();
            let result = if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
                Probe::Unreadable {
                    stage: Stage::OpenProcessToken,
                    code: GetLastError(),
                }
            } else {
                let probe = read_rid(token, detail);
                CloseHandle(token);
                probe
            };
            CloseHandle(process);
            result
        }
    }

    /// Image file name only (no directory), same API the engine uses for
    /// foreground identification. `None` when the process cannot be opened.
    fn process_name(pid: u32) -> Option<String> {
        // SAFETY: query only; handle closed before return.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return None;
            }
            let mut buf = vec![0u16; 32768];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len);
            CloseHandle(process);
            if ok == 0 {
                return None;
            }
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            Some(path.rsplit(['\\', '/']).next().unwrap_or(&path).to_owned())
        }
    }

    fn foreground_pid() -> Option<u32> {
        // SAFETY: both calls are read-only window queries.
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_null() {
                return None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            (pid != 0).then_some(pid)
        }
    }

    fn describe(probe: Probe) -> String {
        match probe {
            Probe::Level {
                rid,
                elevated,
                uiaccess,
            } => {
                let extra = match (elevated, uiaccess) {
                    (Some(e), Some(u)) => format!("  elevated={e} uiaccess={u}"),
                    _ => String::new(),
                };
                format!("{} (0x{rid:04X}){extra}", level_name(rid))
            }
            Probe::Unreadable { stage, code } => {
                format!("UNREADABLE at {} (Win32 error {code})", stage.name())
            }
        }
    }

    fn explain_unreadable(stage: Stage, code: u32) -> &'static str {
        match (stage, code) {
            (Stage::OpenProcess, ERROR_ACCESS_DENIED) => concat!(
                "access denied even for PROCESS_QUERY_LIMITED_INFORMATION: the process DACL ",
                "refuses this caller (SYSTEM / service / other user / elevated, or a protected ",
                "process / security product). On the spike machine this is the dominant failure ",
                "mode (about a third of all processes). Plan B-17: treat as 'elevated or ",
                "unknown' and degrade; do not assume it is always an administrator window"
            ),
            (Stage::OpenProcess, ERROR_INVALID_PARAMETER) => {
                "no such process (already exited); re-sample the foreground window"
            }
            (Stage::OpenProcessToken, ERROR_ACCESS_DENIED) => concat!(
                "the token's DACL refuses TOKEN_QUERY to this caller (not seen on the spike ",
                "machine, where every process that opened also gave up its token). Treat as ",
                "'elevated or unknown' exactly like the OpenProcess case"
            ),
            _ => "unexpected failure; log it and degrade conservatively (treat as elevated)",
        }
    }

    fn verdict(self_probe: Probe, target: Probe) -> String {
        match (self_probe, target) {
            (Probe::Level { rid: me, .. }, Probe::Level { rid: other, .. }) => {
                if other > me {
                    "HIGHER: target integrity is above self -> UIPI would block injected input \
                     (SendInput returns normally but the target never sees it)"
                        .to_owned()
                } else if other == me {
                    "SAME: same integrity level -> UIPI does not block injection".to_owned()
                } else {
                    "LOWER: target integrity is below self -> UIPI does not block injection"
                        .to_owned()
                }
            }
            (_, Probe::Unreadable { stage, code }) => {
                format!("UNREADABLE: {}", explain_unreadable(stage, code))
            }
            (Probe::Unreadable { .. }, _) => {
                "UNKNOWN: could not read this process's own level".to_owned()
            }
        }
    }

    fn bench(pid: u32, iterations: usize) {
        let mut samples: Vec<u128> = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let started = Instant::now();
            std::hint::black_box(probe_pid(pid, false));
            samples.push(started.elapsed().as_nanos());
        }
        samples.sort_unstable();
        let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize] as f64 / 1000.0;
        let mean = samples.iter().sum::<u128>() as f64 / samples.len() as f64 / 1000.0;
        println!(
            "bench: {iterations} runs of the full chain  mean={mean:.1}us  p50={:.1}us  p99={:.1}us  max={:.1}us",
            at(0.5),
            at(0.99),
            at(1.0)
        );
    }

    /// Counts only; names are printed for unreadable processes so the
    /// "ACCESS_DENIED means elevated" heuristic can be eyeballed.
    fn scan() {
        // SAFETY: snapshot enumeration is read-only; handle closed before return.
        let entries = unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                println!("scan: CreateToolhelp32Snapshot failed ({})", GetLastError());
                return;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut entries: Vec<(u32, String)> = Vec::new();
            let mut more = Process32FirstW(snapshot, &mut entry) != 0;
            while more {
                let end = entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(0);
                entries.push((
                    entry.th32ProcessID,
                    String::from_utf16_lossy(&entry.szExeFile[..end]),
                ));
                more = Process32NextW(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
            entries
        };

        let mut by_level: BTreeMap<u32, usize> = BTreeMap::new();
        let mut high_names: Vec<String> = Vec::new();
        let mut high_detail: BTreeMap<String, (Option<bool>, Option<bool>)> = BTreeMap::new();
        let mut unreadable: BTreeMap<(Stage, u32), Vec<String>> = BTreeMap::new();
        for (pid, name) in &entries {
            if *pid == 0 {
                continue; // System Idle Process
            }
            match probe_pid(*pid, true) {
                Probe::Level {
                    rid,
                    elevated,
                    uiaccess,
                } => {
                    *by_level.entry(rid).or_default() += 1;
                    if rid >= 0x3000 {
                        high_names.push(name.clone());
                        high_detail.insert(name.clone(), (elevated, uiaccess));
                    }
                }
                Probe::Unreadable { stage, code } => {
                    unreadable
                        .entry((stage, code))
                        .or_default()
                        .push(name.clone());
                }
            }
        }
        println!("scan: {} processes enumerated", entries.len());
        for (rid, count) in &by_level {
            println!(
                "  readable  {:<10} (0x{rid:04X}): {count} processes",
                level_name(*rid)
            );
        }
        high_names.sort();
        high_names.dedup();
        println!(
            "  readable processes at High or above: {}",
            high_names.join(", ")
        );
        for (name, (elevated, uiaccess)) in &high_detail {
            println!("    {name}: elevated={elevated:?} uiaccess={uiaccess:?}");
        }
        for ((stage, code), names) in &mut unreadable {
            let total = names.len();
            names.sort();
            names.dedup();
            println!(
                "  unreadable at {} error {code}: {total} processes, {} distinct image names",
                stage.name(),
                names.len()
            );
            println!(
                "    {}",
                names
                    .iter()
                    .take(25)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    /// A DWORD-sized token property (TokenElevation / TokenUIAccess) of this process.
    fn own_token_dword(class: TOKEN_INFORMATION_CLASS) -> Option<u32> {
        // SAFETY: query only; the token handle is closed before return and the
        // output buffer is a u32, which matches both classes used here.
        unsafe {
            let mut token: HANDLE = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let value = token_dword(token, class);
            CloseHandle(token);
            value
        }
    }

    /// Own elevation / UIAccess and the two foreground-lock settings the focus
    /// spike depends on (SystemParametersInfo GET calls only).
    fn system_report() {
        let flag =
            |v: Option<u32>| v.map_or_else(|| "unreadable".to_owned(), |n| (n != 0).to_string());
        println!(
            "self:    elevated={}  uiaccess={}",
            flag(own_token_dword(TokenElevation)),
            flag(own_token_dword(TokenUIAccess))
        );
        let spi = |action| {
            let mut value = 0u32;
            // SAFETY: GET action; pvParam points at a DWORD, uiParam is 0 as documented.
            let ok =
                unsafe { SystemParametersInfoW(action, 0, (&mut value as *mut u32).cast(), 0) };
            (ok != 0).then_some(value)
        };
        match spi(SPI_GETFOREGROUNDLOCKTIMEOUT) {
            Some(ms) => println!("system:  SPI_GETFOREGROUNDLOCKTIMEOUT = {ms} ms"),
            None => println!("system:  SPI_GETFOREGROUNDLOCKTIMEOUT failed"),
        }
        match spi(SPI_GETFOREGROUNDFLASHCOUNT) {
            Some(n) => println!("system:  SPI_GETFOREGROUNDFLASHCOUNT  = {n}"),
            None => println!("system:  SPI_GETFOREGROUNDFLASHCOUNT failed"),
        }
    }

    pub fn run() {
        let mut pid_arg: Option<u32> = None;
        let mut bench_runs: Option<usize> = None;
        let mut do_scan = false;
        let mut do_system = false;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--bench" => {
                    bench_runs = args
                        .next()
                        .and_then(|v| v.parse::<usize>().ok())
                        .map(|n| n.clamp(1, 100_000));
                    if bench_runs.is_none() {
                        eprintln!("--bench needs a positive integer");
                        std::process::exit(2);
                    }
                }
                "--scan" => do_scan = true,
                "--system" => do_system = true,
                "-h" | "--help" => {
                    println!("usage: spike_integrity [PID] [--bench N] [--scan] [--system]");
                    return;
                }
                other => match other.parse::<u32>() {
                    Ok(pid) => pid_arg = Some(pid),
                    Err(_) => {
                        eprintln!("unrecognised argument: {other}");
                        std::process::exit(2);
                    }
                },
            }
        }

        let self_pid = current_pid();
        let self_probe = probe_pid(self_pid, true);
        println!(
            "self:    {}  level={}",
            process_name(self_pid).unwrap_or_else(|| "<unknown>".into()),
            describe(self_probe)
        );

        if do_system {
            system_report();
            return;
        }
        if do_scan {
            scan();
            return;
        }

        let (target_pid, source) = match pid_arg {
            Some(pid) => (pid, "command line"),
            None => {
                match foreground_pid() {
                    Some(pid) => (pid, "foreground window"),
                    None => {
                        println!("target:  no foreground window (secure desktop / lock screen / no focus)");
                        println!("verdict: UNREADABLE: nothing to inject into; plan treats this as 'no target'");
                        return;
                    }
                }
            }
        };

        let target_probe = probe_pid(target_pid, true);
        println!(
            "target:  {}  level={}  (source: {source})",
            process_name(target_pid).unwrap_or_else(|| "<unreadable name>".into()),
            describe(target_probe)
        );
        println!("verdict: {}", verdict(self_probe, target_probe));

        if let Some(runs) = bench_runs {
            bench(target_pid, runs);
        }
    }

    fn current_pid() -> u32 {
        // SAFETY: GetCurrentProcessId has no preconditions.
        unsafe { GetCurrentProcessId() }
    }
}
