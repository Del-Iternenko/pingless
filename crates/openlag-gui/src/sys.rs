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
    // SAFETY: NUL-terminated wide strings that outlive the call.
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
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
    const NAME: &str = "name=OpenLag local proxy";
    let exe = std::env::current_exe()?;
    let netsh = |args: &[&str]| {
        std::process::Command::new("netsh")
            .args(["advfirewall", "firewall"])
            .args(args)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
    };
    let _ = netsh(&["delete", "rule", NAME]);
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

/// Starts OpenLag at logon through Task Scheduler with highest privileges, so there is
/// no UAC prompt at every login (a Run registry key cannot elevate).
pub fn set_autostart(on: bool) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    let schtasks = |args: &[&str]| {
        std::process::Command::new("schtasks")
            .args(args)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
    };
    if !on {
        let _ = schtasks(&["/Delete", "/TN", "OpenLag", "/F"]);
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let run = format!("\"{}\" --minimized", exe.display());
    let out = schtasks(&[
        "/Create", "/TN", "OpenLag", "/TR", &run, "/SC", "ONLOGON", "/RL", "HIGHEST", "/F",
    ])?;
    anyhow::ensure!(
        out.status.success(),
        "schtasks: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}
