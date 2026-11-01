//! UI state belongs to this thread. Never hold a RefCell borrow across a Win32 call.
use crate::{about as about_window, panel as p, theme, win::*};
use sog::{
    config::*,
    engine::{
        measurement_config, Backend, Change, ChangeResult, Device, Engine, FileJournal, Inventory,
        Kind, Outcome, PresentationAck, RestoreBatch, ScanRequest,
    },
    native::Native,
    schedule::Schedule,
    storage,
};
use std::{
    cell::RefCell,
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    mem::size_of,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{Power::*, Registry::*, SystemInformation::*, Threading::*},
        UI::{Controls::*, HiDpi::*, Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    },
};

pub const WM_PANEL: u32 = WM_APP + 2;
pub const WM_CANCEL_CAPTURE: u32 = WM_APP + 3;
const WM_REPORT: u32 = WM_APP + 4;
const WM_SHOW: u32 = WM_APP + 5;
const WM_CAPTURE_KEY: u32 = WM_APP + 6;
const WM_RELEASE_HOOK: u32 = WM_APP + 7;
const WM_TRAY: u32 = WM_APP + 1;
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
const TIMER_CLOCK: usize = 1;
const TIMER_FADE: usize = 2;
const TIMER_SELFTEST: usize = 4;
const TIMER_EDIT: usize = 5;
const CMD_TOGGLE: usize = 1001;
const CMD_SETTINGS: usize = 1002;
const CMD_ABOUT: usize = 1003;
const CMD_EXIT: usize = 1004;
const CMD_CURSOR: usize = 1100;
const CMD_ORDER: usize = 1200;

#[derive(Clone)]
struct Options {
    dir: PathBuf,
    isolated: bool,
    test: bool,
    selftest: bool,
    silent: bool,
    restrict_id: Option<String>,
    probe: Option<PathBuf>,
    key: String,
}
impl Options {
    fn parse() -> Result<Self, String> {
        let mut args = std::env::args().skip(1);
        let (mut dir, mut test, mut selftest, mut silent, mut probe, mut restrict_id) =
            (None, false, false, false, None, None);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--data-dir" => {
                    dir = Some(PathBuf::from(args.next().ok_or("--data-dir 缺少路径")?))
                }
                "--ui-test" => test = true,
                "--selftest" => {
                    selftest = true;
                    test = true;
                }
                "--silent" => silent = true,
                "--restrict-id" => {
                    let value = args.next().ok_or("--restrict-id 缺少设备身份")?;
                    if !matches!(value.split_once(':'), Some(("ddc" | "wmi" | "hdr", _))) {
                        return Err("--restrict-id 必须是 ddc:/wmi:/hdr: 设备身份".into());
                    }
                    restrict_id = Some(value);
                }
                "--probe" => {
                    probe = Some(PathBuf::from(args.next().ok_or("--probe 缺少输出路径")?))
                }
                _ => return Err(format!("不支持的参数：{arg}")),
            }
        }
        if selftest && dir.is_none() {
            dir = Some(std::env::temp_dir().join(format!("sog-selftest-{}", std::process::id())));
        }
        if test && dir.is_none() {
            return Err("--ui-test 必须指定 --data-dir，避免修改日用配置".into());
        }
        let isolated = dir.is_some();
        let dir = dir.unwrap_or_else(storage::default_dir);
        if !dir.is_absolute() {
            return Err("--data-dir 必须是绝对路径".into());
        }
        let mut hasher = DefaultHasher::new();
        dir.to_string_lossy()
            .replace('/', "\\")
            .to_lowercase()
            .hash(&mut hasher);
        Ok(Self {
            key: format!("{:016x}", hasher.finish()),
            dir,
            isolated,
            test,
            selftest,
            silent,
            restrict_id,
            probe,
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum Operation {
    Dark,
    Light,
    Measure,
    Quit,
}
struct Work {
    requested_at: Instant,
    op: Operation,
    cfg: Config,
    generation: u64,
}
enum Report {
    AutostartError(String),
    Presentation {
        generation: u64,
        dark: bool,
        hidden: bool,
        reply: mpsc::Sender<PresentationAck>,
    },
    Ready(Result<Outcome, String>, bool),
    Hardware {
        op: Operation,
        generation: u64,
        result: Result<Outcome, String>,
        pending: bool,
    },
    Saved {
        cfg: Box<Config>,
        result: Result<(), String>,
    },
}
struct App {
    owner: HWND,
    settings: HWND,
    about: HWND,
    overlay: HWND,
    cursor_hidden: bool,
    presentation_trace: Vec<serde_json::Value>,
    opts: Options,
    saved: Config,
    draft: Config,
    saving: bool,
    reserved: Option<i32>,
    hotkey_id: i32,
    capture: bool,
    hook: HHOOK,
    captured_down: u32,
    desired: bool,
    phase: &'static str,
    ready: bool,
    exiting: bool,
    hardware_done: bool,
    pending_edit: i32,
    fade: u8,
    reports: mpsc::Receiver<Report>,
    work: mpsc::Sender<Work>,
    saves: mpsc::Sender<Config>,
    generation: Arc<AtomicU64>,
    schedule: Schedule,
    message: String,
    taskbar: u32,
    tray_v4: bool,
}
thread_local! {static UI:RefCell<Option<App>>=const{RefCell::new(None)};}
// Callbacks below only copy or update Rust values in this closure.
fn state<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    UI.with(|cell| cell.borrow_mut().as_mut().map(f))
}
fn wake(sender: &mpsc::Sender<Report>, owner: usize, report: Report) {
    if sender.send(report).is_ok() {
        unsafe {
            let _ = PostMessageW(HWND(owner as *mut _), WM_REPORT, WPARAM(0), LPARAM(0));
        }
    }
}
fn load(opts: &Options) -> (Config, Option<String>) {
    let path = opts.dir.join("settings.json");
    match fs::read_to_string(&path) {
        Ok(s) => match Config::decode(&s) {
            Ok(c) => (c, None),
            Err(e) => (Config::default(), Some(e)),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !opts.isolated {
                if let Some(root) = std::env::var_os("APPDATA") {
                    let legacy = PathBuf::from(root).join("sog.json");
                    if let Ok(s) = fs::read_to_string(legacy) {
                        return match Config::migrate(&s) {
                            Ok(c) => (c, Some("已读取旧偏好；首次修改时保存新配置".into())),
                            Err(e) => (Config::default(), Some(e)),
                        };
                    }
                }
            }
            (Config::default(), None)
        }
        Err(e) => (
            Config::default(),
            Some(format!("无法读取配置，原文件保留：{e}")),
        ),
    }
}
fn registry(opts: &Options, enabled: bool) -> Result<(), String> {
    unsafe {
        let sub = if opts.isolated {
            format!("Software\\ScreenOffGuard\\Tests\\{}", opts.key)
        } else {
            "Software\\Microsoft\\Windows\\CurrentVersion\\Run".into()
        };
        let mut key = HKEY::default();
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(&sub).as_ptr()),
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()
        .map_err(|e| e.to_string())?;
        let result = if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let isolated = if opts.isolated {
                format!(
                    " --data-dir \"{}\"{}",
                    opts.dir.display(),
                    if opts.test { " --ui-test" } else { "" }
                )
            } else {
                String::new()
            };
            let value = wide(&format!("\"{}\"{isolated} --silent", exe.display()));
            RegSetValueExW(
                key,
                w!("ScreenOffGuardRust"),
                0,
                REG_SZ,
                Some(std::slice::from_raw_parts(
                    value.as_ptr().cast(),
                    value.len() * 2,
                )),
            )
            .ok()
            .map_err(|e| e.to_string())
        } else {
            let code = RegDeleteValueW(key, w!("ScreenOffGuardRust"));
            if code == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                code.ok().map_err(|e| e.to_string())
            }
        };
        let _ = RegCloseKey(key);
        result
    }
}
fn save_loop(
    rx: mpsc::Receiver<Config>,
    sender: mpsc::Sender<Report>,
    owner: usize,
    opts: Options,
    mut autostart: bool,
) {
    // Repair a missing/stale registration when a validated saved preference
    // already enables it. Keep this serialized with later user save requests.
    if autostart {
        if let Err(error) = registry(&opts, true) {
            wake(&sender, owner, Report::AutostartError(error));
        }
    }
    for cfg in rx {
        if opts.test {
            if let Some(ms) = fs::read_to_string(opts.dir.join("save-delay-ms"))
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
            {
                thread::sleep(Duration::from_millis(ms.min(2000)));
            }
        }
        let changed = autostart != cfg.autostart;
        let result = (|| {
            cfg.validate()?;
            if let Ok(original) = fs::read_to_string(opts.dir.join("settings.json")) {
                Config::decode(&original)
                    .map_err(|e| format!("{e}；请先修复配置文件，未覆盖原件"))?;
            }
            if changed {
                registry(&opts, cfg.autostart)?;
            }
            if let Err(e) = storage::atomic_json(&opts.dir.join("settings.json"), &cfg) {
                if changed {
                    if let Err(rollback) = registry(&opts, autostart) {
                        return Err(format!("{e}；自启回滚失败：{rollback}"));
                    }
                }
                return Err(e);
            }
            autostart = cfg.autostart;
            Ok(())
        })();
        wake(
            &sender,
            owner,
            Report::Saved {
                cfg: Box::new(cfg),
                result,
            },
        );
    }
}

// A persistent simulation exercises the real Engine/journal without touching the desktop.
struct Sim {
    path: PathBuf,
    devices: Vec<Device>,
    hidden: bool,
}
impl Sim {
    fn new(dir: &std::path::Path) -> Self {
        let path = dir.join("simulated-hardware.json");
        let old = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
        let devices = old
            .as_ref()
            .and_then(|v| serde_json::from_value(v["devices"].clone()).ok())
            .unwrap_or_else(|| {
                vec![
                    Device {
                        id: "sim-external".into(),
                        name: "模拟 DDC".into(),
                        kind: Kind::External,
                        min: 0,
                        value: 72,
                        max: 100,
                    },
                    Device {
                        id: "sim-internal".into(),
                        name: "模拟 WMI".into(),
                        kind: Kind::Internal,
                        min: 0,
                        value: 38,
                        max: 100,
                    },
                    Device {
                        id: "sim-hdr".into(),
                        name: "模拟 HDR".into(),
                        kind: Kind::Hdr,
                        min: 0,
                        value: 1,
                        max: 1,
                    },
                ]
            });
        Self {
            path,
            devices,
            hidden: old.is_some_and(|v| v["cursor_hidden"] == true),
        }
    }
    fn persist(&self) -> Result<(), String> {
        storage::atomic_json(
            &self.path,
            &serde_json::json!({"simulated":true,"devices":self.devices,"cursor_hidden":self.hidden}),
        )
    }
}
impl Backend for Sim {
    fn scan(&mut self) -> Result<Inventory, String> {
        Ok(Inventory {
            devices: self.devices.clone(),
            warnings: vec![],
        })
    }
    fn write(&mut self, id: &str, kind: Kind, value: u32) -> Result<(), String> {
        thread::sleep(Duration::from_millis(45));
        self.devices
            .iter_mut()
            .find(|d| d.id == id && d.kind == kind)
            .ok_or("模拟设备不存在")?
            .value = value;
        self.persist()
    }
    fn read(&mut self, id: &str, kind: Kind) -> Result<u32, String> {
        self.devices
            .iter()
            .find(|d| d.id == id && d.kind == kind)
            .map(|d| d.value)
            .ok_or("模拟设备不存在".into())
    }
    fn cursor(&mut self, hidden: bool) -> Result<(), String> {
        self.hidden = hidden;
        self.persist()
    }
    fn remote_active(&self) -> bool {
        false
    }
}
/// Hardware stays on its worker; all visible transitions belong to the UI.
struct Presented<B> {
    inner: B,
    sender: mpsc::Sender<Report>,
    owner: usize,
    generation: Arc<AtomicU64>,
    command_generation: u64,
    op: Operation,
    reveal_delay: u32,
    reveal_on_write: bool,
    test: bool,
}
impl<B: Backend> Backend for Presented<B> {
    fn scan(&mut self) -> Result<Inventory, String> {
        self.inner.scan()
    }
    fn scan_required(&mut self, request: ScanRequest) -> Result<Inventory, String> {
        self.inner.scan_required(request)
    }
    fn prepare_restore(
        &mut self,
        entries: &[Device],
        allow_reuse: bool,
    ) -> Result<Vec<String>, String> {
        self.inner.prepare_restore(entries, allow_reuse)
    }
    fn read(&mut self, id: &str, kind: Kind) -> Result<u32, String> {
        self.inner.read(id, kind)
    }
    fn write(&mut self, id: &str, kind: Kind, value: u32) -> Result<(), String> {
        self.inner.write(id, kind, value)
    }
    fn cursor(&mut self, hidden: bool) -> Result<(), String> {
        self.inner.cursor(hidden)
    }
    fn remote_active(&self) -> bool {
        self.inner.remote_active()
    }
    fn apply(
        &mut self,
        changes: &[Change],
        current: &(dyn Fn() -> bool + Sync),
    ) -> Vec<ChangeResult> {
        self.inner.apply(changes, current)
    }
    fn apply_restore(
        &mut self,
        changes: &[Change],
        current: &(dyn Fn() -> bool + Sync),
        hidden: bool,
    ) -> RestoreBatch {
        if !self.reveal_on_write || self.test {
            return RestoreBatch {
                changes: self.inner.apply(changes, current),
                presentation: None,
            };
        }
        let (reply, received) = mpsc::channel();
        let posted = AtomicBool::new(false);
        let sender = &self.sender;
        let owner = self.owner;
        let generation = self.command_generation;
        let all_written = || {
            if current() && !posted.swap(true, Ordering::AcqRel) {
                // Only post here: the last write lane immediately starts its
                // read while the UI reveals the desktop independently.
                wake(
                    sender,
                    owner,
                    Report::Presentation {
                        generation,
                        dark: false,
                        hidden,
                        reply: reply.clone(),
                    },
                );
            }
        };
        let changes = self
            .inner
            .apply_with_write_ack(changes, current, &all_written);
        let presentation = if posted.load(Ordering::Acquire) {
            Some(
                received
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap_or_else(|_| PresentationAck {
                        result: Err("界面未确认屏幕与光标切换，恢复记录已保留".into()),
                        at: Instant::now(),
                    }),
            )
        } else {
            None
        };
        RestoreBatch {
            changes,
            presentation,
        }
    }
    fn present(&mut self, dark: bool, hidden: bool) -> Result<(), String> {
        if self.op == Operation::Measure {
            return Ok(());
        }
        let current = || self.generation.load(Ordering::Acquire) == self.command_generation;
        if !dark && self.reveal_delay > 0 {
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(u64::from(self.reveal_delay)) && current()
            {
                thread::sleep(Duration::from_millis(5));
            }
        }
        if !current() {
            return Err("显示切换已被新指令替代".into());
        }
        let (reply, received) = mpsc::channel();
        wake(
            &self.sender,
            self.owner,
            Report::Presentation {
                generation: self.command_generation,
                dark,
                hidden,
                reply,
            },
        );
        received
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "界面未确认屏幕与光标切换，恢复记录已保留".to_string())?
            .result?;
        if self.test {
            self.inner.present(dark, hidden)?;
        }
        Ok(())
    }
}
fn worker<B: Backend>(
    backend: B,
    rx: mpsc::Receiver<Work>,
    sender: mpsc::Sender<Report>,
    owner: usize,
    opts: Options,
    generation: Arc<AtomicU64>,
) {
    let backend = Presented {
        inner: backend,
        sender: sender.clone(),
        owner,
        generation: generation.clone(),
        command_generation: 0,
        op: Operation::Light,
        reveal_delay: 0,
        reveal_on_write: false,
        test: opts.test,
    };
    let mut engine = Engine::new(backend, FileJournal(opts.dir.join("recovery.json")));
    let initial = match &mut engine {
        Ok(e) => e.light(|| true),
        Err(e) => Err(e.clone()),
    };
    let pending = engine.as_ref().is_ok_and(|e| e.has_pending_recovery());
    wake(&sender, owner, Report::Ready(initial, pending));
    while let Ok(command) = rx.recv() {
        if command.op != Operation::Quit && generation.load(Ordering::Acquire) != command.generation
        {
            continue;
        }
        let alive = || generation.load(Ordering::Acquire) == command.generation;
        if command.op == Operation::Dark && !opts.test {
            unsafe {
                let _ = SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
            }
        }
        let result = match &mut engine {
            Err(e) => Err(e.clone()),
            Ok(engine) => {
                engine.backend.command_generation = command.generation;
                engine.backend.op = command.op;
                engine.backend.reveal_on_write = command.op == Operation::Light
                    && command.cfg.restore_mode == RestoreMode::Ready;
                engine.backend.reveal_delay = if command.op == Operation::Light
                    && command.cfg.restore_mode == RestoreMode::Delay
                {
                    command.cfg.delay_ms
                } else {
                    0
                };
                match command.op {
                    Operation::Dark => engine.dark(&command.cfg, alive),
                    Operation::Light => engine.light(alive),
                    Operation::Quit => engine.light(|| true),
                    Operation::Measure => {
                        let cfg = measurement_config(&command.cfg);
                        let dark = engine.dark(&cfg, alive);
                        // Even a superseded or failed measurement must restore its baseline.
                        let light = engine.light_after_measure(|| true);
                        if opts.isolated {
                            let trace = serde_json::json!({
                                "generation": command.generation,
                                "dark": dark.as_ref().ok(), "dark_error": dark.as_ref().err(),
                                "light": light.as_ref().ok(), "light_error": light.as_ref().err()
                            });
                            let _ = storage::atomic_json(
                                &opts.dir.join("hardware-measurement.json"),
                                &trace,
                            );
                        }
                        match (dark, light) {
                            (Ok(mut a), Ok(b)) => {
                                a.elapsed_ms = a.elapsed_ms.max(b.elapsed_ms);
                                a.write_ms = a.write_ms.max(b.write_ms);
                                a.prepare_ms = a.prepare_ms.saturating_add(b.prepare_ms);
                                a.total_ms = a.total_ms.saturating_add(b.total_ms);
                                a.changes.extend(b.changes);
                                a.warnings.extend(b.warnings);
                                Ok(a)
                            }
                            (Err(a), Err(b)) => Err(format!("{a}；恢复失败：{b}")),
                            (Err(e), _) | (_, Err(e)) => Err(e),
                        }
                    }
                }
            }
        };
        let pending = engine.as_ref().is_ok_and(|e| e.has_pending_recovery());
        if !pending && !opts.test {
            unsafe {
                let _ = SetThreadExecutionState(ES_CONTINUOUS);
            }
        }
        if opts.isolated && matches!(command.op, Operation::Dark | Operation::Light) {
            // Observation runs after the operation; never delay presentation.
            let trace = serde_json::json!({
                "generation": command.generation,
                "operation": if command.op == Operation::Dark { "dark" } else { "light" },
                "outcome": result.as_ref().ok(), "error": result.as_ref().err(),
                "pending": pending,
                "requested_to_complete_ms": command.requested_at.elapsed().as_secs_f64() * 1000.0
            });
            let _ = storage::atomic_json(&opts.dir.join("hardware-operation.json"), &trace);
        }
        wake(
            &sender,
            owner,
            Report::Hardware {
                op: command.op,
                generation: command.generation,
                result,
                pending,
            },
        );
        // Keep the worker available if exit is postponed by a configuration save failure.
    }
}

unsafe fn register_hotkey(owner: HWND, id: i32, key: Hotkey) -> Result<(), String> {
    RegisterHotKey(
        owner,
        id,
        HOT_KEY_MODIFIERS(key.modifiers | MOD_NOREPEAT.0),
        key.key,
    )
    .map_err(|_| format!("{} 已被占用；请使用其它组合键", key.label()))
}
fn feedback(message: impl Into<String>) {
    let message = message.into();
    let hwnd = state(|s| {
        s.message = message.clone();
        s.settings
    })
    .unwrap_or_default();
    unsafe {
        p::status(hwnd, &message);
    }
    publish();
}
fn publish() {
    let snapshot=state(|s|if s.opts.test{Some((s.opts.dir.clone(),serde_json::json!({
        "pid":std::process::id(),"ready":s.ready,"desired_dark":s.desired,"phase":s.phase,"generation":s.generation.load(Ordering::Acquire),"message":s.message,
        "capture":s.capture,"saving":s.saving,"saved":s.saved,"draft":s.draft,"exiting":s.exiting
    })))}else{None}).flatten();
    if let Some((dir, mut value)) = snapshot {
        let panel = state(|s| s.settings).unwrap_or_default();
        value["effective_theme"] = unsafe { p::effective_dark(panel) }
            .map(|dark| if dark { "dark" } else { "light" })
            .into();
        let _ = storage::atomic_json(&dir.join("status.json"), &value);
    }
}
unsafe fn refresh(force: bool) {
    if let Some((h, cfg, dark, busy)) =
        state(|s| (s.settings, s.draft.clone(), s.desired, s.phase != "light"))
    {
        p::sync(h, &cfg, force);
        p::set_dark(h, dark);
        p::set_busy(h, busy);
    }
}
fn save_draft() {
    let work = state(|s| {
        if s.saving || s.draft == s.saved {
            return None;
        }
        s.saving = true;
        Some((s.saves.clone(), s.draft.clone()))
    })
    .flatten();
    if let Some((tx, cfg)) = work {
        if tx.send(cfg).is_err() {
            state(|s| s.saving = false);
            feedback("保存线程已停止，请重启程序");
        } else {
            feedback("正在保存…");
        }
    }
}
unsafe fn cancel_capture() {
    let changed = state(|s| {
        let was = s.capture;
        s.capture = false;
        was
    })
    .unwrap_or(false);
    stop_hook();
    if changed {
        let (h, key) = state(|s| (s.settings, s.draft.hotkey.label())).unwrap();
        p::set_capture(h, false, &key);
        feedback("已取消快捷键录入");
    }
}
unsafe fn stop_hook() {
    let hook = state(|s| {
        s.captured_down = 0;
        std::mem::take(&mut s.hook)
    })
    .unwrap_or_default();
    if !hook.0.is_null() {
        let _ = UnhookWindowsHookEx(hook);
    }
}
unsafe extern "system" fn capture_hook(code: i32, wparam: WPARAM, l: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, l);
    }
    let key = &*(l.0 as *const KBDLLHOOKSTRUCT);
    let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    let interested = state(|s| s.capture || s.captured_down == key.vkCode).unwrap_or(false);
    if interested && !matches!(key.vkCode,0x10..=0x12|0x5b|0x5c|0xa0..=0xa5) {
        let (owner, post, release) = state(|s| {
            let post = down && s.captured_down != key.vkCode && s.capture;
            s.captured_down = if down { key.vkCode } else { 0 };
            (s.owner, post, !down && !s.capture)
        })
        .unwrap();
        if post {
            let mut mods = 0;
            for (vk, bit) in [
                (VK_CONTROL, 2),
                (VK_MENU, 1),
                (VK_SHIFT, 4),
                (VK_LWIN, 8),
                (VK_RWIN, 8),
            ] {
                if GetAsyncKeyState(vk.0 as i32) < 0 {
                    mods |= bit;
                }
            }
            let _ = PostMessageW(
                owner,
                WM_CAPTURE_KEY,
                WPARAM(key.vkCode as usize),
                LPARAM(mods),
            );
        }
        if release {
            let _ = PostMessageW(owner, WM_RELEASE_HOOK, WPARAM(0), LPARAM(0));
        }
        return LRESULT(1);
    }
    CallNextHookEx(None, code, wparam, l)
}
unsafe fn capture_key(key: u32, modifiers: Option<u32>) -> bool {
    if !state(|s| s.capture).unwrap_or(false) {
        return false;
    }
    if key == VK_ESCAPE.0 as u32 {
        cancel_capture();
        return true;
    }
    if matches!(key,0x10..=0x12|0x5b|0x5c|0xa0..=0xa5) {
        return true;
    }
    let mut mods = 0;
    for (vk, bit) in [
        (VK_CONTROL, 2),
        (VK_MENU, 1),
        (VK_SHIFT, 4),
        (VK_LWIN, 8),
        (VK_RWIN, 8),
    ] {
        if GetKeyState(vk.0 as i32) < 0 {
            mods |= bit;
        }
    }
    let hotkey = Hotkey {
        modifiers: modifiers.unwrap_or(mods),
        key,
    };
    if let Err(e) = hotkey.validate() {
        feedback(e);
        return true;
    }
    let (owner, id, old) = state(|s| {
        (
            s.owner,
            if s.hotkey_id == 1 { 2 } else { 1 },
            s.saved.hotkey,
        )
    })
    .unwrap();
    if hotkey == old {
        cancel_capture();
        return true;
    }
    if let Err(e) = register_hotkey(owner, id, hotkey) {
        feedback(e);
        return true;
    }
    state(|s| {
        s.capture = false;
        s.reserved = Some(id);
        s.draft.hotkey = hotkey;
    });
    let h = state(|s| s.settings).unwrap_or_default();
    p::set_capture(h, false, &hotkey.label());
    refresh(true);
    save_draft();
    true
}
unsafe fn show_settings() {
    let (owner, h, cfg, test, message, system_dark) = state(|s| {
        (
            s.owner,
            s.settings,
            s.draft.clone(),
            s.opts.test,
            s.message.clone(),
            theme::system_dark(s.opts.test.then_some(s.opts.dir.as_path())),
        )
    })
    .unwrap();
    let background_test = test && std::env::var_os("SOG_TEST_WORKAREA").is_some();
    if h.0.is_null() {
        match p::create(owner, &cfg, test, system_dark) {
            Ok(h) => {
                state(|s| s.settings = h);
                p::status(h, &message);
                refresh(true);
                if !background_test {
                    let _ = SetForegroundWindow(h);
                }
            }
            Err(e) => {
                MessageBoxW(
                    owner,
                    PCWSTR(wide(&e).as_ptr()),
                    w!("设置页创建失败"),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
    } else {
        p::system_theme_changed(h, system_dark);
        let _ = ShowWindow(
            h,
            if background_test {
                SW_SHOWNOACTIVATE
            } else {
                SW_RESTORE
            },
        );
        if !background_test {
            let _ = SetForegroundWindow(h);
        }
    }
    publish();
}
unsafe fn about() {
    cancel_capture();
    let background_test =
        state(|s| s.opts.test && std::env::var_os("SOG_TEST_WORKAREA").is_some()).unwrap_or(false);
    let (existing, owner, dark) = state(|s| {
        (
            s.about,
            if s.settings.0.is_null() {
                s.owner
            } else {
                s.settings
            },
            s.draft.theme.is_dark(theme::system_dark(
                s.opts.test.then_some(s.opts.dir.as_path()),
            )),
        )
    })
    .unwrap();
    if IsWindow(existing).as_bool() {
        if !background_test {
            let _ = SetForegroundWindow(existing);
        }
        return;
    }
    match about_window::show(owner, theme::palette(dark), !background_test) {
        Ok(h) => {
            state(|s| s.about = h);
        }
        Err(error) => feedback(error),
    }
}
unsafe fn panel_command(wparam: WPARAM) {
    let id = (wparam.0 & 0xffff) as i32;
    let event = (wparam.0 >> 16) as u32;
    if state(|s| s.exiting).unwrap_or(true) {
        return;
    }
    match id {
        p::MAXIMIZE => {
            let h = state(|s| s.settings).unwrap();
            let _ = ShowWindow(
                h,
                if IsZoomed(h).as_bool() {
                    SW_RESTORE
                } else {
                    SW_MAXIMIZE
                },
            );
            return;
        }
        p::RETRY => {
            save_draft();
            return;
        }
        p::ABOUT => {
            about();
            return;
        }
        p::TOGGLE => {
            toggle();
            return;
        }
        p::CLOSE | p::MINIMIZE => {
            cancel_capture();
            let (h, exit) = state(|s| {
                (
                    s.settings,
                    id == p::CLOSE && s.draft.close_action == CloseAction::Exit,
                )
            })
            .unwrap();
            if exit {
                begin_exit();
            } else {
                let _ = ShowWindow(h, SW_HIDE);
            }
            return;
        }
        p::HOTKEY => {
            if state(|s| s.saving).unwrap_or(true) {
                feedback("请等待当前设置保存完成");
                return;
            }
            stop_hook();
            match SetWindowsHookExW(WH_KEYBOARD_LL, Some(capture_hook), instance(), 0) {
                Ok(hook) => {
                    let h = state(|s| {
                        s.capture = true;
                        s.hook = hook;
                        s.settings
                    })
                    .unwrap();
                    p::set_capture(h, true, "请按组合键… · Esc 取消");
                    feedback("正在录入快捷键，原快捷键暂不执行");
                }
                Err(e) => feedback(format!("无法开始快捷键录入：{e}")),
            }
            return;
        }
        p::MEASURE => {
            request(Operation::Measure, true);
            return;
        }
        _ => {}
    }
    if event != BN_CLICKED && event != EN_CHANGE && event != EN_KILLFOCUS {
        return;
    }
    if [p::DELAY, p::DARK_TIME, p::LIGHT_TIME].contains(&id) {
        let owner = state(|s| {
            s.pending_edit = id;
            s.owner
        })
        .unwrap();
        let _ = KillTimer(owner, TIMER_EDIT);
        if event == EN_CHANGE {
            SetTimer(owner, TIMER_EDIT, 350, None);
            return;
        }
    }
    let (h, mut cfg) = state(|s| (s.settings, s.draft.clone())).unwrap();
    let checked = |id| SendMessageW(p::control(h, id), BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == 1;
    let parsed: Result<(), String> = (|| {
        match id {
            p::EXT => cfg.dim_external = checked(id),
            p::INT => cfg.dim_internal = checked(id),
            p::HDR => cfg.hdr_off = checked(id),
            p::AUTOSTART => cfg.autostart = checked(id),
            p::SCHEDULE => cfg.schedule_enabled = checked(id),
            p::BLACK => cfg.dim_order = DimOrder::BlackFirst,
            p::DIM => cfg.dim_order = DimOrder::DimFirst,
            p::FADE => cfg.dim_order = DimOrder::Fade,
            p::READY => cfg.restore_mode = RestoreMode::Ready,
            p::DELAY_MODE => cfg.restore_mode = RestoreMode::Delay,
            p::CUR_AUTO => cfg.cursor_mode = CursorMode::Auto,
            p::CUR_HIDE => cfg.cursor_mode = CursorMode::Hide,
            p::CUR_KEEP => cfg.cursor_mode = CursorMode::Keep,
            p::CLOSE_HIDE => cfg.close_action = CloseAction::Hide,
            p::CLOSE_EXIT => cfg.close_action = CloseAction::Exit,
            p::THEME_LIGHT => cfg.theme = Theme::Light,
            p::THEME_DARK => cfg.theme = Theme::Dark,
            p::THEME_SYSTEM => cfg.theme = Theme::System,
            p::DELAY => {
                cfg.delay_ms = p::value(h, id)
                    .parse::<u32>()
                    .map_err(|_| "请输入 0–500 ms，当前输入尚未保存")?
            }
            p::DARK_TIME => cfg.dark_at = Time::parse(&p::value(h, id))?,
            p::LIGHT_TIME => cfg.light_at = Time::parse(&p::value(h, id))?,
            _ => return Ok(()),
        }
        cfg.validate()
    })();
    match parsed {
        Err(e) => feedback(format!("尚未保存：{e}")),
        Ok(()) => {
            state(|s| s.draft = cfg);
            refresh(false);
            save_draft();
        }
    }
}

unsafe extern "system" fn overlay_proc(h: HWND, m: u32, wparam: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_DISPLAYCHANGE => {
            let _ = SetWindowPos(
                h,
                HWND_TOPMOST,
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
                SWP_NOACTIVATE,
            );
            LRESULT(0)
        }
        _ => DefWindowProcW(h, m, wparam, l),
    }
}
unsafe fn overlay(show: bool, hidden: bool) -> Result<(), String> {
    let start = Instant::now();
    let (h, owner, test) = state(|s| (s.overlay, s.owner, s.opts.test)).unwrap();
    if !show {
        let _ = KillTimer(owner, TIMER_FADE);
        let restore_cursor = state(|s| s.cursor_hidden).unwrap_or(false) || hidden;
        if restore_cursor {
            if !test {
                sog::native::set_cursor_hidden(false)?;
            }
            state(|s| s.cursor_hidden = false);
        }
        let cursor_ms = start.elapsed().as_secs_f64() * 1000.0;
        // Reload the cursor while the overlay still covers the panels. Removing
        // the window first exposes a cursor-less desktop during SPI_SETCURSORS.
        if !h.0.is_null() {
            let _ = DestroyWindow(h);
            state(|s| s.overlay = HWND::default());
        }
        let window_ms = start.elapsed().as_secs_f64() * 1000.0;
        if !h.0.is_null() || restore_cursor {
            record_presentation(
                serde_json::json!({"dark":false,"hidden":false,"window_ms":window_ms,"cursor_ms":cursor_ms,"simulated":test}),
            );
        }
        return Ok(());
    }
    if !h.0.is_null() {
        return Ok(());
    }
    register(
        "SOGRustOverlay",
        Some(overlay_proc),
        HBRUSH(GetStockObject(BLACK_BRUSH).0),
    )?;
    let h = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
        w!("SOGRustOverlay"),
        w!("一键熄屏覆盖窗"),
        WS_POPUP,
        GetSystemMetrics(SM_XVIRTUALSCREEN),
        GetSystemMetrics(SM_YVIRTUALSCREEN),
        GetSystemMetrics(SM_CXVIRTUALSCREEN),
        GetSystemMetrics(SM_CYVIRTUALSCREEN),
        None,
        None,
        instance(),
        None,
    )
    .map_err(|e| e.to_string())?;
    let created_ms = start.elapsed().as_secs_f64() * 1000.0;
    if let Err(e) = SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE) {
        let _ = DestroyWindow(h);
        return Err(format!("黑窗无法从远程捕获中排除：{e}"));
    }
    let fade = state(|s| s.saved.dim_order == DimOrder::Fade).unwrap_or(false);
    let _ = SetLayeredWindowAttributes(h, COLORREF(0), if fade { 0 } else { 255 }, LWA_ALPHA);
    state(|s| {
        s.overlay = h;
        s.fade = 0;
    });
    // The cursor and visible overlay are changed in this one UI dispatch,
    // after the recovery journal commits and before any hardware wait.
    if hidden {
        if !test {
            if let Err(error) = sog::native::set_cursor_hidden(true) {
                let _ = DestroyWindow(h);
                state(|s| s.overlay = HWND::default());
                return Err(error);
            }
        }
        state(|s| s.cursor_hidden = true);
    }
    let cursor_ms = start.elapsed().as_secs_f64() * 1000.0;
    if !test {
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
    }
    if fade {
        SetTimer(owner, TIMER_FADE, 20, None);
    }
    record_presentation(
        serde_json::json!({"dark":true,"hidden":hidden,"created_ms":created_ms,"cursor_ms":cursor_ms,"window_ms":start.elapsed().as_secs_f64()*1000.0,"simulated":test}),
    );
    Ok(())
}
fn record_presentation(mut event: serde_json::Value) {
    let trace = state(|s| {
        if !s.opts.isolated {
            return None;
        }
        event["generation"] = s.generation.load(Ordering::Acquire).into();
        s.presentation_trace.push(event);
        Some((
            s.opts.dir.join("presentation-trace.json"),
            s.presentation_trace.clone(),
        ))
    })
    .flatten();
    if let Some((path, trace)) = trace {
        let _ = storage::atomic_json(&path, &trace);
    }
}
unsafe fn toggle() {
    let desired = state(|s| s.desired).unwrap_or(false);
    request(
        if desired {
            Operation::Light
        } else {
            Operation::Dark
        },
        true,
    );
}
unsafe fn request(op: Operation, manual: bool) {
    let now = GetTickCount64();
    let work = state(|s| {
        if !s.ready || s.exiting || (op == Operation::Measure && s.phase != "light") {
            return None;
        }
        if manual {
            s.schedule.manual(now);
        }
        let generation = s.generation.fetch_add(1, Ordering::AcqRel) + 1;
        s.hardware_done = false;
        s.desired = op == Operation::Dark;
        s.phase = match op {
            Operation::Dark => "darkening",
            Operation::Light => "restoring",
            Operation::Measure => "measuring",
            Operation::Quit => "exiting",
        };
        Some((
            s.work.clone(),
            Work {
                requested_at: Instant::now(),
                op,
                cfg: s.saved.clone(),
                generation,
            },
        ))
    })
    .flatten();
    let Some((tx, command)) = work else {
        return;
    };
    feedback(match op {
        Operation::Dark => "正在熄屏…",
        Operation::Light => "正在恢复屏幕…",
        Operation::Measure => "正在检测：压暗后自动恢复，请稍候…",
        Operation::Quit => "正在恢复并退出…",
    });
    if tx.send(command).is_err() {
        let _ = overlay(false, false);
        feedback("调光线程已停止，请重启程序");
    }
    refresh(false);
}
unsafe fn begin_exit() {
    cancel_capture();
    let pending = state(|s| s.pending_edit).unwrap_or(0);
    if pending != 0 {
        panel_command(WPARAM(pending as usize | ((EN_KILLFOCUS as usize) << 16)));
    }
    save_draft();
    let command = state(|s| {
        if s.exiting {
            return None;
        }
        s.exiting = true;
        s.hardware_done = false;
        s.phase = "exiting";
        s.desired = false;
        let generation = s.generation.fetch_add(1, Ordering::AcqRel) + 1;
        Some((
            s.work.clone(),
            Work {
                requested_at: Instant::now(),
                op: Operation::Quit,
                cfg: s.saved.clone(),
                generation,
            },
        ))
    })
    .flatten();
    if let Some((tx, command)) = command {
        feedback("正在恢复屏幕并退出…");
        if tx.send(command).is_err() {
            state(|s| {
                s.exiting = false;
            });
            feedback("恢复线程不可用，恢复记录已保留");
        }
    }
}
unsafe fn finish_exit() {
    if state(|s| s.exiting && s.hardware_done && !s.saving && s.draft == s.saved).unwrap_or(false) {
        PostQuitMessage(0);
    }
}

unsafe fn receive_reports() {
    while let Some(report) = state(|s| s.reports.try_recv().ok()).flatten() {
        match report {
            Report::Presentation {
                generation,
                dark,
                hidden,
                reply,
            } => {
                let result = if state(|s| s.generation.load(Ordering::Acquire) == generation)
                    .unwrap_or(false)
                {
                    overlay(dark, hidden)
                } else {
                    Err("显示切换已被新指令替代".into())
                };
                let _ = reply.send(PresentationAck {
                    result,
                    at: Instant::now(),
                });
            }
            Report::AutostartError(error) => {
                feedback(format!("开机自启未能同步：{error}；可重新切换自启重试"));
            }
            Report::Ready(result, pending) => {
                state(|s| {
                    s.ready = true;
                    s.phase = if pending { "recovery_failed" } else { "light" };
                });
                match result {
                    Err(e) => feedback(e),
                    Ok(out) => {
                        let existing = state(|s| s.message.clone()).unwrap();
                        if existing == "正在启动…" {
                            feedback(if pending {
                                "上次状态未完全恢复，记录已保留".into()
                            } else if out.warnings.is_empty() {
                                "就绪".into()
                            } else {
                                out.warnings.join("；")
                            });
                        }
                    }
                }
                refresh(false);
            }
            Report::Saved { cfg, result } => {
                state(|s| s.saving = false);
                match result {
                    Ok(()) => {
                        let h = state(|s| s.settings).unwrap();
                        p::set_retry(h, false);
                        let remove = state(|s| {
                            let old = s.hotkey_id;
                            if s.saved.hotkey != cfg.hotkey {
                                if let Some(id) = s.reserved.take() {
                                    s.hotkey_id = id;
                                }
                            }
                            s.saved = *cfg;
                            if old != s.hotkey_id {
                                Some((s.owner, old))
                            } else {
                                None
                            }
                        })
                        .flatten();
                        if let Some((h, id)) = remove {
                            let _ = UnregisterHotKey(h, id);
                        }
                        feedback("已保存");
                        refresh(false);
                        save_draft();
                        finish_exit();
                    }
                    Err(e) => {
                        let requested_exit = state(|s| s.exiting).unwrap_or(false);
                        let h = state(|s| s.settings).unwrap();
                        p::set_retry(h, true);
                        let remove = state(|s| {
                            s.exiting = false;
                            s.draft.hotkey = s.saved.hotkey;
                            s.reserved.take().map(|id| (s.owner, id))
                        })
                        .flatten();
                        if let Some((h, id)) = remove {
                            let _ = UnregisterHotKey(h, id);
                        }
                        let (h, label) = state(|s| (s.settings, s.saved.hotkey.label())).unwrap();
                        set_text(p::control(h, p::HOTKEY), &label);
                        refresh(false);
                        feedback(format!("保存失败：{e}；输入已保留，可点击重试"));
                        if requested_exit {
                            show_settings();
                            if MessageBoxW(h,w!("设置未能保存。是否放弃本次未保存的修改，恢复屏幕后退出？\n\n选择“否”可保留输入并重试保存。"),w!("退出一键熄屏"),MB_YESNO|MB_ICONWARNING|MB_DEFBUTTON2)==IDYES{
                                let (panel,cfg)=state(|s|{s.draft=s.saved.clone();s.pending_edit=0;(s.settings,s.saved.clone())}).unwrap();
                                p::sync(panel,&cfg,true);begin_exit();
                            }
                        }
                    }
                }
            }
            Report::Hardware {
                op,
                generation,
                result,
                pending,
            } => {
                if !state(|s| s.generation.load(Ordering::Acquire) == generation).unwrap_or(false) {
                    continue;
                }
                if op == Operation::Quit && !pending {
                    let _ = overlay(false, false);
                    state(|s| s.hardware_done = true);
                    feedback(match result {
                        Ok(_) => "屏幕已恢复，正在退出".into(),
                        Err(e) => format!("无法读取恢复记录：{e}；原件保留；本次未执行新调光"),
                    });
                    finish_exit();
                    continue;
                }
                if op == Operation::Quit {
                    state(|s| s.exiting = false);
                    show_settings();
                }
                match result {
                    Err(e) => {
                        let _ = overlay(false, false);
                        state(|s| {
                            s.desired = false;
                            s.phase = if pending { "recovery_failed" } else { "light" };
                        });
                        feedback(e);
                    }
                    Ok(out) => {
                        if out.superseded {
                            continue;
                        }
                        if op == Operation::Dark {
                            state(|s| s.phase = "dark");
                        } else {
                            state(|s| {
                                s.phase = if pending { "recovery_failed" } else { "light" };
                                s.desired = false;
                            });
                        }
                        let message = if op == Operation::Measure {
                            if out.channels > 0
                                && !pending
                                && out
                                    .changes
                                    .iter()
                                    .all(|change| matches!(change.result, Ok(true)))
                            {
                                // Preserve the full readiness contract. Write latency is
                                // reported separately and never substitutes for read-back.
                                let measured = out.elapsed_ms.min(500);
                                state(|s| s.draft.delay_ms = measured);
                                save_draft();
                                let simulated = state(|s| s.opts.test).unwrap_or(false);
                                format!(
                                    "检测完成：调光 {} ms（写入 {} ms），已恢复{}",
                                    out.elapsed_ms,
                                    out.write_ms,
                                    if simulated {
                                        "；模拟结果仅用于交互验证"
                                    } else {
                                        ""
                                    }
                                )
                            } else {
                                "检测未全部完成或没有可用调光通道；未修改延迟".into()
                            }
                        } else if op == Operation::Dark {
                            format!("已熄屏 · {} 个硬件通道", out.channels)
                        } else if pending {
                            "部分设备尚未恢复；恢复记录已保留，可再次亮屏或退出重试".into()
                        } else {
                            "屏幕已恢复".into()
                        };
                        feedback(if out.warnings.is_empty() {
                            message
                        } else {
                            format!("{message}；{}", out.warnings.join("；"))
                        });
                    }
                }
                let (owner, dark) = state(|s| (s.owner, s.desired)).unwrap();
                tray(NIM_MODIFY, owner, dark);
                refresh(false);
            }
        }
    }
    publish();
}
unsafe fn tray(operation: NOTIFY_ICON_MESSAGE, owner: HWND, dark: bool) -> bool {
    let mut hash = DefaultHasher::new();
    std::env::current_exe().ok().hash(&mut hash);
    let key = state(|s| s.opts.key.clone()).unwrap_or_default();
    key.hash(&mut hash);
    let guid = windows::core::GUID::from_u128(
        0x8c67d319_53d8_4b41_0000_000000000000 | hash.finish() as u128,
    );
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: owner,
        uID: 1,
        uFlags: NIF_ICON | NIF_TIP | NIF_MESSAGE | NIF_GUID,
        guidItem: guid,
        uCallbackMessage: WM_TRAY,
        hIcon: icon(32),
        ..Default::default()
    };
    let status = if dark { "已熄屏" } else { "就绪" };
    let label = wide(&if state(|s| s.opts.test).unwrap_or(false) {
        format!("一键熄屏 · 测试 PID {} · {status}", std::process::id())
    } else {
        format!("一键熄屏 · {status}")
    });
    let n = label.len().min(data.szTip.len() - 1);
    data.szTip[..n].copy_from_slice(&label[..n]);
    SetLastError(WIN32_ERROR(0));
    let success = Shell_NotifyIconW(operation, &data).as_bool();
    if success && operation == NIM_ADD {
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let version4 = Shell_NotifyIconW(NIM_SETVERSION, &data).as_bool();
        state(|s| s.tray_v4 = version4);
    }
    if let Some(dir) = state(|s| {
        if s.opts.test {
            Some(s.opts.dir.clone())
        } else {
            None
        }
    })
    .flatten()
    {
        let _ = storage::atomic_json(
            &dir.join("tray.json"),
            &serde_json::json!({"success":success,"operation":operation.0,"guid":format!("{guid:?}"),"owner":owner.0 as usize,"id":1}),
        );
    }
    if !success {
        if let Some(dir) = state(|s| s.opts.dir.clone()) {
            let _ = storage::atomic_json(
                &dir.join("tray-error.json"),
                &serde_json::json!({"operation":operation.0,"last_error":GetLastError().0,"owner":owner.0 as usize,"icon":data.hIcon.0 as usize}),
            );
        }
    }
    success
}
unsafe fn menu(owner: HWND) {
    cancel_capture();
    let (cfg, dark) = state(|s| (s.saved.clone(), s.desired)).unwrap();
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let append = |menu: HMENU, id, text: &str, flags| {
        let _ = AppendMenuW(menu, flags, id, PCWSTR(wide(text).as_ptr()));
    };
    append(
        menu,
        CMD_TOGGLE,
        if dark { "恢复亮屏" } else { "立即熄屏" },
        MF_STRING,
    );
    append(menu, CMD_SETTINGS, "设置面板", MF_STRING);
    let mut children = Vec::new();
    for (title, base, labels, selected) in [
        (
            "光标策略",
            CMD_CURSOR,
            vec!["远控保留，其余隐藏", "始终隐藏", "始终保留"],
            match cfg.cursor_mode {
                CursorMode::Auto => 0,
                CursorMode::Hide => 1,
                CursorMode::Keep => 2,
            },
        ),
        (
            "熄屏顺序",
            CMD_ORDER,
            vec!["先黑后熄", "先熄后黑", "淡入并行"],
            match cfg.dim_order {
                DimOrder::BlackFirst => 0,
                DimOrder::DimFirst => 1,
                DimOrder::Fade => 2,
            },
        ),
    ] {
        if let Ok(sub) = CreatePopupMenu() {
            for (i, label) in labels.into_iter().enumerate() {
                append(
                    sub,
                    base + i,
                    label,
                    MF_STRING
                        | if i == selected {
                            MF_CHECKED
                        } else {
                            MF_UNCHECKED
                        },
                );
            }
            let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(wide(title).as_ptr()));
            children.push(sub);
        }
    }
    append(menu, CMD_ABOUT, "关于", MF_STRING);
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    append(menu, CMD_EXIT, "恢复并退出", MF_STRING);
    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    let _ = SetForegroundWindow(owner);
    let command = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        point.x,
        point.y,
        0,
        owner,
        None,
    )
    .0 as usize;
    let _ = DestroyMenu(menu);
    let _ = PostMessageW(owner, WM_NULL, WPARAM(0), LPARAM(0));
    execute(command);
}
unsafe fn execute(command: usize) {
    match command {
        CMD_TOGGLE => toggle(),
        CMD_SETTINGS => show_settings(),
        CMD_ABOUT => about(),
        CMD_EXIT => begin_exit(),
        CMD_CURSOR..=1102 => {
            state(|s| {
                s.draft.cursor_mode = match command - CMD_CURSOR {
                    0 => CursorMode::Auto,
                    1 => CursorMode::Hide,
                    _ => CursorMode::Keep,
                }
            });
            refresh(false);
            save_draft();
        }
        CMD_ORDER..=1202 => {
            state(|s| {
                s.draft.dim_order = match command - CMD_ORDER {
                    0 => DimOrder::BlackFirst,
                    1 => DimOrder::DimFirst,
                    _ => DimOrder::Fade,
                }
            });
            refresh(false);
            save_draft();
        }
        _ => {}
    }
}
unsafe extern "system" fn controller(h: HWND, m: u32, wparam: WPARAM, l: LPARAM) -> LRESULT {
    if state(|s| s.taskbar == m).unwrap_or(false) {
        tray(NIM_ADD, h, false);
        return LRESULT(0);
    }
    match m {
        WM_SETTINGCHANGE | WM_THEMECHANGED | WM_SYSCOLORCHANGE => {
            let current = state(|s| {
                (
                    s.settings,
                    theme::system_dark(s.opts.test.then_some(s.opts.dir.as_path())),
                )
            });
            if let Some((panel, dark)) = current {
                p::system_theme_changed(panel, dark);
            }
            publish();
            LRESULT(0)
        }
        WM_SHOW => {
            show_settings();
            LRESULT(0)
        }
        WM_PANEL => {
            panel_command(wparam);
            LRESULT(0)
        }
        WM_CAPTURE_KEY => {
            capture_key(wparam.0 as u32, Some(l.0 as u32));
            LRESULT(0)
        }
        WM_RELEASE_HOOK => {
            if !state(|s| s.capture).unwrap_or(false) {
                stop_hook();
            }
            LRESULT(0)
        }
        WM_CANCEL_CAPTURE => {
            cancel_capture();
            LRESULT(0)
        }
        WM_REPORT => {
            receive_reports();
            LRESULT(0)
        }
        WM_COMMAND => {
            execute(wparam.0 & 0xffff);
            LRESULT(0)
        }
        WM_HOTKEY => {
            if state(|s| !s.capture && s.hotkey_id == wparam.0 as i32).unwrap_or(false) {
                toggle();
            }
            LRESULT(0)
        }
        WM_TRAY => {
            if let Some(dir) = state(|s| {
                if s.opts.test {
                    Some(s.opts.dir.clone())
                } else {
                    None
                }
            })
            .flatten()
            {
                let _ = storage::atomic_json(
                    &dir.join("last-tray-event.json"),
                    &serde_json::json!({"event":l.0 as u32&0xffff,"wparam":wparam.0,"lparam":l.0}),
                );
                if l.0 as u32 & 0xffff != WM_MOUSEMOVE {
                    use std::io::Write;
                    if let Ok(mut file) = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(dir.join("tray-events.log"))
                    {
                        let _ = writeln!(
                            file,
                            "{} {} {}",
                            GetTickCount64(),
                            l.0 as u32 & 0xffff,
                            wparam.0
                        );
                    }
                }
            }
            let version4 = state(|s| s.tray_v4).unwrap_or(false);
            match l.0 as u32 & 0xffff {
                WM_CONTEXTMENU => menu(h),
                WM_RBUTTONUP if !version4 => menu(h),
                NIN_SELECT | NIN_KEYSELECT => show_settings(),
                WM_LBUTTONUP | WM_LBUTTONDBLCLK if !version4 => show_settings(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            begin_exit();
            LRESULT(0)
        }
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                begin_exit();
                let start = GetTickCount64();
                while GetTickCount64() - start < 4000 && !state(|s| s.hardware_done).unwrap_or(true)
                {
                    receive_reports();
                    thread::sleep(Duration::from_millis(10));
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                TIMER_CLOCK => {
                    let now = GetLocalTime();
                    let tick = GetTickCount64();
                    let scheduled = state(|s| {
                        s.schedule.tick(
                            now.wYear as u32 * 10000 + now.wMonth as u32 * 100 + now.wDay as u32,
                            now.wHour * 60 + now.wMinute,
                            tick,
                            &s.saved,
                        )
                    })
                    .flatten();
                    if let Some(dark) = scheduled {
                        if state(|s| s.desired != dark).unwrap_or(false) {
                            request(
                                if dark {
                                    Operation::Dark
                                } else {
                                    Operation::Light
                                },
                                false,
                            );
                        }
                    }
                }
                TIMER_FADE => {
                    let (overlay, alpha) = state(|s| {
                        s.fade = s.fade.saturating_add(26);
                        (s.overlay, s.fade)
                    })
                    .unwrap();
                    if !overlay.0.is_null() {
                        let _ = SetLayeredWindowAttributes(overlay, COLORREF(0), alpha, LWA_ALPHA);
                    }
                    if alpha == 255 {
                        let _ = KillTimer(h, TIMER_FADE);
                    }
                }
                TIMER_EDIT => {
                    let _ = KillTimer(h, TIMER_EDIT);
                    let id = state(|s| s.pending_edit).unwrap_or(0);
                    if id != 0 {
                        panel_command(WPARAM(id as usize | ((EN_KILLFOCUS as usize) << 16)));
                    }
                }
                TIMER_SELFTEST
                    if state(|s| s.ready && !s.settings.0.is_null()).unwrap_or(false) =>
                {
                    let _ = KillTimer(h, TIMER_SELFTEST);
                    begin_exit();
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(h, m, wparam, l),
    }
}
struct Instance(HANDLE);
impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn run() -> Result<(), String> {
    unsafe {
        let opts = Options::parse()?;
        if let Some(path) = &opts.probe {
            let remote = sog::remote::detect();
            let result = Native::new(false).scan();
            return storage::atomic_json(
                path,
                &match result {
                    Ok(i) => serde_json::json!({
                        "read_only":true,
                        "remote":remote,
                        "inventory":i
                    }),
                    Err(e) => serde_json::json!({
                        "read_only":true,
                        "remote":remote,
                        "error":e
                    }),
                },
            );
        }
        let class = format!("SOGController{}", opts.key);
        SetLastError(WIN32_ERROR(0));
        let handle = CreateMutexW(
            None,
            false,
            PCWSTR(wide(&format!("Local\\ScreenOffGuard-{}", opts.key)).as_ptr()),
        )
        .map_err(|e| e.to_string())?;
        let existed = GetLastError() == ERROR_ALREADY_EXISTS;
        let _instance = Instance(handle);
        if existed {
            for _ in 0..40 {
                if let Ok(first) = FindWindowW(PCWSTR(wide(&class).as_ptr()), PCWSTR::null()) {
                    if !opts.silent {
                        let _ = PostMessageW(first, WM_SHOW, WPARAM(0), LPARAM(0));
                    }
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(50));
            }
            return Err("首个实例仍在初始化，请稍后重试".into());
        }
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if !InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES,
        })
        .as_bool()
        {
            return Err("无法初始化 Windows 控件".into());
        }
        register(&class, Some(controller), HBRUSH::default())?;
        let owner = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR(wide(&class).as_ptr()),
            w!("一键熄屏控制器"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            instance(),
            None,
        )
        .map_err(|e| e.to_string())?;
        let (cfg, warning) = load(&opts);
        let mut message = warning.unwrap_or_else(|| "正在启动…".into());
        if let Err(e) = register_hotkey(owner, 1, cfg.hotkey) {
            message = e;
        }
        let (work_tx, work_rx) = mpsc::channel();
        let (save_tx, save_rx) = mpsc::channel();
        let (reports_tx, reports_rx) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));
        let worker_gen = generation.clone();
        let worker_opts = opts.clone();
        let saver_opts = opts.clone();
        let worker_reports = reports_tx.clone();
        let address = owner.0 as usize;
        let auto = cfg.autostart;
        let taskbar = RegisterWindowMessageW(w!("TaskbarCreated"));
        UI.with(|cell| {
            *cell.borrow_mut() = Some(App {
                owner,
                settings: HWND::default(),
                about: HWND::default(),
                overlay: HWND::default(),
                cursor_hidden: false,
                presentation_trace: vec![],
                opts: opts.clone(),
                saved: cfg.clone(),
                draft: cfg.clone(),
                saving: false,
                reserved: None,
                hotkey_id: 1,
                capture: false,
                hook: HHOOK::default(),
                captured_down: 0,
                desired: false,
                phase: "starting",
                ready: false,
                exiting: false,
                hardware_done: false,
                pending_edit: 0,
                fade: 0,
                reports: reports_rx,
                work: work_tx,
                saves: save_tx,
                generation,
                schedule: Schedule::default(),
                message,
                taskbar,
                tray_v4: false,
            })
        });
        let hardware = thread::spawn(move || {
            if worker_opts.test {
                let backend = Sim::new(&worker_opts.dir);
                worker(
                    backend,
                    work_rx,
                    worker_reports,
                    address,
                    worker_opts,
                    worker_gen,
                );
            } else {
                let native = worker_opts
                    .restrict_id
                    .as_ref()
                    .map(|id| {
                        Native::restricted_to(vec![id.clone()])
                            .expect("validated device restriction")
                    })
                    .unwrap_or_else(|| Native::new(false));
                worker(
                    native,
                    work_rx,
                    worker_reports,
                    address,
                    worker_opts,
                    worker_gen,
                );
            }
        });
        let saver =
            thread::spawn(move || save_loop(save_rx, reports_tx, address, saver_opts, auto));
        if !tray(NIM_ADD, owner, false) {
            feedback("托盘图标创建失败，设置窗口保持可见");
            show_settings();
        }
        SetTimer(owner, TIMER_CLOCK, 1000, None);
        if opts.selftest {
            SetTimer(owner, TIMER_SELFTEST, 300, None);
        }
        if opts.selftest || !opts.silent {
            show_settings();
        }
        publish();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let about = state(|s| s.about).unwrap_or_default();
            if IsWindow(about).as_bool() && IsDialogMessageW(about, &msg).as_bool() {
                continue;
            }
            let settings = state(|s| s.settings).unwrap_or_default();
            let belongs = msg.hwnd == settings || IsChild(settings, msg.hwnd).as_bool();
            if belongs && matches!(msg.message, WM_KEYDOWN | WM_SYSKEYDOWN) {
                if capture_key(msg.wParam.0 as u32, None) {
                    continue;
                }
                if msg.wParam.0 == VK_ESCAPE.0 as usize {
                    cancel_capture();
                    continue;
                }
                if msg.wParam.0 == 0x41
                    && GetKeyState(VK_CONTROL.0 as i32) < 0
                    && p::is_edit(settings, msg.hwnd)
                {
                    SendMessageW(msg.hwnd, EM_SETSEL, WPARAM(0), LPARAM(-1));
                    continue;
                }
                if msg.wParam.0 == VK_RETURN.0 as usize && p::is_edit(settings, msg.hwnd) {
                    let id = GetDlgCtrlID(msg.hwnd);
                    panel_command(WPARAM(id as usize | ((EN_KILLFOCUS as usize) << 16)));
                    continue;
                }
            }
            if belongs && IsDialogMessageW(settings, &msg).as_bool() {
                p::ensure_focus(settings);
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        stop_hook();
        let _ = overlay(false, false);
        tray(NIM_DELETE, owner, false);
        for id in [1, 2] {
            let _ = UnregisterHotKey(owner, id);
        }
        let h = state(|s| s.settings).unwrap_or_default();
        if !h.0.is_null() {
            let _ = DestroyWindow(h);
        }
        #[cfg(feature = "direct2d")]
        crate::d2d::shutdown();
        crate::antialias::shutdown();
        let _ = DestroyWindow(owner);
        UI.with(|cell| *cell.borrow_mut() = None);
        let _ = hardware.join();
        let _ = saver.join();
        Ok(())
    }
}
