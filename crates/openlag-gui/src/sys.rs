//! Small Windows helpers: elevation, process list, relaunch as Administrator.

use std::collections::BTreeSet;

pub fn is_admin() -> bool {
    // SAFETY: no arguments.
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

/// Executable names of running processes, lower-case, without obvious system ones.
pub fn process_names() -> Vec<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    const SKIP: &[&str] = &[
        "system",
        "registry",
        "smss.exe",
        "csrss.exe",
        "wininit.exe",
        "services.exe",
        "lsass.exe",
        "svchost.exe",
        "winlogon.exe",
        "dwm.exe",
        "fontdrvhost.exe",
        "conhost.exe",
        "runtimebroker.exe",
        "sihost.exe",
        "taskhostw.exe",
        "ctfmon.exe",
        "searchhost.exe",
        "startmenuexperiencehost.exe",
        "shellexperiencehost.exe",
        "textinputhost.exe",
        "dllhost.exe",
        "wmiprvse.exe",
        "audiodg.exe",
        "spoolsv.exe",
        "memory compression",
        "secure system",
        "[system process]",
        "openlag.exe",
        "pingless.exe",
        "pingless-xray.exe",
        "openlag-gui.exe",
        "xray.exe",
        "msedgewebview2.exe",
    ];
    let mut names = BTreeSet::new();
    // SAFETY: standard ToolHelp iteration; the snapshot handle is closed below.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok {
            let len = e
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase();
            if !SKIP.contains(&name.as_str()) {
                names.insert(name);
            }
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    names.into_iter().collect()
}

/// Starts this executable again with a UAC prompt. Returns false if the user declined.
pub fn relaunch_as_admin() -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    // The new process waits for this one to exit before claiming the single instance.
    let params = wide(&format!("--relaunch {}", std::process::id()));
    // SAFETY: NUL-terminated wide strings that outlive the call.
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    r as isize > 32
}

/// Allows inbound connections to this executable in Windows Firewall. TCP of captured
/// programs is redirected to our local listener and arrives as inbound traffic.
/// Recreated on every start so the rule follows the exe if it was moved.
pub fn ensure_firewall_rule() -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    const NAME: &str = "name=Pingless local proxy";
    let exe = std::env::current_exe()?;
    let netsh = |args: &[&str]| {
        std::process::Command::new("netsh")
            .args(["advfirewall", "firewall"])
            .args(args)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
    };
    let _ = netsh(&["delete", "rule", NAME]);
    // Left over by versions called OpenLag.
    let _ = netsh(&["delete", "rule", "name=OpenLag local proxy"]);
    let program = format!("program={}", exe.display());
    let out = netsh(&[
        "add",
        "rule",
        NAME,
        "dir=in",
        "action=allow",
        "protocol=TCP",
        &program,
        "enable=yes",
        "profile=any",
    ])?;
    anyhow::ensure!(
        out.status.success(),
        "netsh failed: {}",
        String::from_utf8_lossy(&out.stdout).trim()
    );
    Ok(())
}

/// Starts Pingless at logon through Task Scheduler with highest privileges, so there is
/// no UAC prompt at every login (a Run registry key cannot elevate).
pub fn set_autostart(on: bool) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    let schtasks = |args: &[&str]| {
        std::process::Command::new("schtasks")
            .args(args)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
    };
    // The task of versions called OpenLag.
    let _ = schtasks(&["/Delete", "/TN", "OpenLag", "/F"]);
    if !on {
        let _ = schtasks(&["/Delete", "/TN", "Pingless", "/F"]);
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let run = format!("\"{}\" --minimized", exe.display());
    let out = schtasks(&[
        "/Create", "/TN", "Pingless", "/TR", &run, "/SC", "ONLOGON", "/RL", "HIGHEST", "/F",
    ])?;
    anyhow::ensure!(
        out.status.success(),
        "schtasks: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

pub fn open_url(url: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (verb, file) = (w("open"), w(url));
    // SAFETY: NUL-terminated strings that outlive the call.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Waits up to `ms` for a process to exit (used after a relaunch as Administrator).
pub fn wait_for_pid(pid: u32, ms: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    // SAFETY: plain handle use; the handle is closed below.
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            WaitForSingleObject(h, ms);
            CloseHandle(h);
        }
    }
}

/// Registers `pingless://` links for the current user, so "Add to Pingless" buttons on
/// subscription pages open this app. Re-done on every start to follow the exe.
pub fn register_url_scheme() -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    let exe = std::env::current_exe()?;
    let reg = |key: &str, name: Option<&str>, value: &str| {
        let mut c = std::process::Command::new("reg");
        c.args(["add", key, "/f", "/t", "REG_SZ", "/d", value]);
        match name {
            Some(n) => c.args(["/v", n]),
            None => c.arg("/ve"),
        };
        c.creation_flags(0x0800_0000).output()
    };
    let _ = std::process::Command::new("reg")
        .args(["delete", r"HKCU\Software\Classes\openlag", "/f"])
        .creation_flags(0x0800_0000)
        .output();
    const KEY: &str = r"HKCU\Software\Classes\pingless";
    reg(KEY, None, "URL:Pingless")?;
    reg(KEY, Some("URL Protocol"), "")?;
    reg(
        &format!(r"{KEY}\DefaultIcon"),
        None,
        &format!("{},0", exe.display()),
    )?;
    let out = reg(
        &format!(r"{KEY}\shell\open\command"),
        None,
        &format!("\"{}\" \"%1\"", exe.display()),
    )?;
    anyhow::ensure!(out.status.success(), "reg add failed");
    Ok(())
}

/// Running processes as (pid, lower-case exe name).
pub fn processes() -> Vec<(u32, String)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    let mut out = Vec::new();
    // SAFETY: the snapshot handle is checked and closed; the entry has its size set.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok {
            let len = e
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(e.szExeFile.len());
            out.push((
                e.th32ProcessID,
                String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase(),
            ));
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    out
}

/// Sets a process's priority class; returns the previous one (0 on failure).
pub fn set_priority(pid: u32, class: u32) -> u32 {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetPriorityClass, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
        SetPriorityClass,
    };
    // SAFETY: plain handle use; the handle is closed below.
    unsafe {
        let h = OpenProcess(
            PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if h.is_null() {
            return 0;
        }
        let old = GetPriorityClass(h);
        if old != 0 && old != class && SetPriorityClass(h, class) == 0 {
            CloseHandle(h);
            return 0;
        }
        CloseHandle(h);
        old
    }
}

pub const NORMAL_PRIORITY: u32 = 0x20;
pub const HIGH_PRIORITY: u32 = 0x80;

/// Full path of a running process's executable.
pub fn process_path(pid: u32) -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    // SAFETY: plain handle use; the buffer outlives the call; the handle is closed.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        (ok != 0).then(|| std::ffi::OsString::from_wide(&buf[..len as usize]).into())
    }
}

/// Up to 1.0 the app was called OpenLag and could run twice. A copy of it still running
/// next to Pingless captures traffic with its own, possibly different, mode and
/// programs: close it and drop its logon task. Only an installed OpenLag is touched
/// (its folder holds openlag-xray.exe), never some other program of that name.
pub fn stop_legacy_openlag() -> usize {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    let _ = std::process::Command::new("schtasks")
        .args(["/Delete", "/TN", "OpenLag", "/F"])
        .creation_flags(0x0800_0000)
        .output();
    let mut stopped = 0;
    for pid in legacy_openlag_pids() {
        // SAFETY: plain handle use; the handle is closed below.
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                if TerminateProcess(h, 0) != 0 {
                    stopped += 1;
                }
                CloseHandle(h);
            }
        }
    }
    stopped
}

/// Running OpenLag.exe copies from an OpenLag install folder.
fn legacy_openlag_pids() -> Vec<u32> {
    let me = std::process::id();
    processes()
        .into_iter()
        .filter(|(pid, name)| *pid != me && name == "openlag.exe")
        .filter(|(pid, _)| {
            process_path(*pid)
                .and_then(|p| p.parent().map(|d| d.join("openlag-xray.exe").exists()))
                .unwrap_or(false)
        })
        .map(|(pid, _)| pid)
        .collect()
}

#[cfg(test)]
mod legacy_tests {
    #[test]
    fn finds_an_installed_openlag_only() {
        let dir = std::env::temp_dir().join(format!("legacy-openlag-{}", std::process::id()));
        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        // A fake install: OpenLag.exe (really ping.exe) next to openlag-xray.exe...
        let ping = std::path::Path::new(r"C:\Windows\System32\PING.EXE");
        std::fs::copy(ping, dir.join("OpenLag.exe")).unwrap();
        std::fs::write(dir.join("openlag-xray.exe"), b"").unwrap();
        // ...and an unrelated program that is merely called openlag.exe.
        std::fs::copy(ping, plain.join("OpenLag.exe")).unwrap();
        let spawn = |p: std::path::PathBuf| {
            std::process::Command::new(p)
                .args(["-n", "30", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut ours = spawn(dir.join("OpenLag.exe"));
        let mut other = spawn(plain.join("OpenLag.exe"));
        std::thread::sleep(std::time::Duration::from_millis(300));
        let found = super::legacy_openlag_pids();
        let _ = ours.kill();
        let _ = other.kill();
        let _ = ours.wait();
        let _ = other.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(found.contains(&ours.id()), "{found:?}");
        assert!(!found.contains(&other.id()));
    }
}
