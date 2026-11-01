//! Incoming desktop-session evidence. Process names only locate a provider;
//! resident services, network heartbeats and outgoing sessions never count.
use serde::Serialize;
use serde_json::Value;
use std::{
    mem::size_of,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE},
        Storage::FileSystem::{
            CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE, OPEN_EXISTING,
        },
        System::{Diagnostics::ToolHelp as processes, Pipes::*, Threading as threading},
        UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION},
    },
};

#[derive(Debug, Serialize)]
pub struct Status {
    pub active: bool,
    pub rdp: bool,
    /// None means unavailable/unconfirmed, rather than an active session.
    pub uu_is_controlled: Option<bool>,
    pub rustdesk_desktop_sessions: Option<u64>,
    pub todesk_is_controlled: Option<bool>,
}
pub fn detect() -> Status {
    let rdp = unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0;
    // Suspended after a real offline incident. The private pipe is not a
    // supported query API. UIA/MSAA did not expose the session banner either.
    let uu_is_controlled = None;
    let rustdesk_desktop_sessions = rustdesk_sessions();
    let todesk_is_controlled = todesk_state();
    Status {
        active: rdp
            || uu_is_controlled == Some(true)
            || rustdesk_desktop_sessions.is_some_and(|count| count > 0)
            || todesk_is_controlled == Some(true),
        rdp,
        uu_is_controlled,
        rustdesk_desktop_sessions,
        todesk_is_controlled,
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn rustdesk_sessions() -> Option<u64> {
    let pipe = Pipe::open(r"\\.\pipe\RustDesk\query", "rustdesk.exe")?;
    // Upstream src/ipc.rs counts only authenticated incoming desktop sessions.
    // ControlledSessionCount also includes file/terminal/unauthorized sessions.
    let body = br#"{"t":"VideoConnCount","c":null}"#;
    let mut frame = vec![(body.len() << 2) as u8];
    frame.extend_from_slice(body);
    pipe.write(&frame)?;
    let deadline = Instant::now() + Duration::from_millis(350);
    let mut header = [0u8; 4];
    pipe.read(&mut header[..1], deadline)?;
    let header_size = usize::from((header[0] & 3) + 1);
    if header_size > 1 {
        pipe.read(&mut header[1..header_size], deadline)?;
    }
    let size = (u32::from_le_bytes(header) >> 2) as usize;
    if size > 16 * 1024 {
        return None;
    }
    let mut response = vec![0u8; size];
    pipe.read(&mut response, deadline)?;
    let value: Value = serde_json::from_slice(&response).ok()?;
    parse_rustdesk_count(&value)
}
fn parse_rustdesk_count(value: &Value) -> Option<u64> {
    if value.get("t").and_then(Value::as_str) != Some("VideoConnCount") {
        return None;
    }
    value.get("c").and_then(Value::as_u64)
}

// ToDesk documents -status. Only explicit incoming-session fields/phrases
// count. This adapter still needs real connected/disconnected samples for
// each supported ToDesk version; a generic "online/connected" stays unknown.
fn todesk_state() -> Option<bool> {
    let path = process_path("todesk.exe")?;
    let output = command_output(&path, &["-status"])?;
    parse_todesk_state(&output)
}
fn parse_todesk_state(output: &str) -> Option<bool> {
    if let Ok(value) = serde_json::from_str::<Value>(output) {
        return value
            .get("isControlled")
            .or_else(|| value.get("is_controlled"))
            .and_then(Value::as_bool);
    }
    for line in output.lines().map(str::trim) {
        if matches!(line, "被控中" | "正在被控" | "Being controlled") {
            return Some(true);
        }
        if matches!(line, "未被控" | "Not being controlled") {
            return Some(false);
        }
    }
    None
}
fn command_output(path: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new(path)
        .args(args)
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(400);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let output = child.wait_with_output().ok()?;
                return String::from_utf8(output.stdout).ok();
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(8)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

struct Pipe(HANDLE);
impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
impl Pipe {
    fn open(path: &str, executable: &str) -> Option<Self> {
        let name = wide(path);
        unsafe {
            // No idle wait when the provider is absent; never start a vendor app.
            if !WaitNamedPipeW(PCWSTR(name.as_ptr()), 1).as_bool() {
                return None;
            }
            let pipe = Self(
                CreateFileW(
                    PCWSTR(name.as_ptr()),
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
                .ok()?,
            );
            let mut pid = 0;
            GetNamedPipeServerProcessId(pipe.0, &mut pid).ok()?;
            if process_name(pid).as_deref() != Some(executable) {
                return None;
            }
            Some(pipe)
        }
    }
    fn write(&self, bytes: &[u8]) -> Option<()> {
        let mut written = 0u32;
        unsafe {
            WriteFile(self.0, Some(bytes), Some(&mut written as *mut u32), None).ok()?;
        }
        (written as usize == bytes.len()).then_some(())
    }
    fn read(&self, bytes: &mut [u8], deadline: Instant) -> Option<()> {
        let mut offset = 0;
        while offset < bytes.len() && Instant::now() < deadline {
            let mut available = 0;
            unsafe {
                PeekNamedPipe(self.0, None, 0, None, Some(&mut available), None).ok()?;
            }
            if available == 0 {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            let size = (bytes.len() - offset).min(available as usize);
            let mut read = 0u32;
            unsafe {
                ReadFile(
                    self.0,
                    Some(&mut bytes[offset..offset + size]),
                    Some(&mut read as *mut u32),
                    None,
                )
                .ok()?;
            }
            if read == 0 {
                return None;
            }
            offset += read as usize;
        }
        (offset == bytes.len()).then_some(())
    }
}
fn process_entries() -> Vec<(u32, String)> {
    unsafe {
        let Ok(snapshot) = processes::CreateToolhelp32Snapshot(processes::TH32CS_SNAPPROCESS, 0)
        else {
            return vec![];
        };
        let mut entry = processes::PROCESSENTRY32W {
            dwSize: size_of::<processes::PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut result = Vec::new();
        if processes::Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let length = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(260);
                result.push((
                    entry.th32ProcessID,
                    String::from_utf16_lossy(&entry.szExeFile[..length]).to_ascii_lowercase(),
                ));
                if processes::Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        result
    }
}
fn process_name(pid: u32) -> Option<String> {
    process_entries()
        .into_iter()
        .find(|(id, _)| *id == pid)
        .map(|(_, name)| name)
}
fn process_path(name: &str) -> Option<PathBuf> {
    let pid = process_entries().into_iter().find(|(_, n)| n == name)?.0;
    unsafe {
        let handle =
            threading::OpenProcess(threading::PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                .ok()?;
        let mut buffer = [0u16; 32768];
        let mut length = buffer.len() as u32;
        let queried = threading::QueryFullProcessImageNameW(
            handle,
            threading::PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(handle);
        queried.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(
            &buffer[..length as usize],
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn rustdesk_requires_authenticated_desktop_count() {
        assert_eq!(
            parse_rustdesk_count(&json!({"t":"VideoConnCount","c":2})),
            Some(2)
        );
        assert_eq!(
            parse_rustdesk_count(&json!({"t":"VideoConnCount","c":0})),
            Some(0)
        );
        assert_eq!(
            parse_rustdesk_count(&json!({"t":"ControlledSessionCount","c":2})),
            None
        );
        assert_eq!(
            parse_rustdesk_count(&json!({"t":"VideoConnCount","c":null})),
            None
        );
    }
    #[test]
    fn service_online_is_not_an_incoming_session() {
        for output in [
            "service=running\nnetwork=connected",
            "online",
            "Remote control is enabled",
            "Not connected",
        ] {
            assert_eq!(parse_todesk_state(output), None);
        }
        assert_eq!(parse_todesk_state("被控中"), Some(true));
        assert_eq!(parse_todesk_state("未被控"), Some(false));
    }
}
