//! S1: 托盘 + 热键 + 黑窗 + DDC(外接屏) + WMI(内置屏, 进程内 COM) + 亮度还原保护
#![windows_subsystem = "windows"]

mod settings;
mod cursor;

use std::ffi::OsStr;
use std::iter::once;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;

use windows::core::{BSTR, PCWSTR, VARIANT};
use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitor, GetMonitorBrightness, GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR, SetMonitorBrightness, PHYSICAL_MONITOR,
};
use windows::Win32::Foundation::{
    BOOL, COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, EnumDisplayMonitors, HBRUSH, HDC, HMONITOR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoInitializeSecurity, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_DEFAULT, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::System::Wmi::{
    IEnumWbemClassObject, IWbemClassObject, IWbemContext, IWbemLocator, IWbemServices,
    WbemLocator,
    WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_GENERIC_FLAG_TYPE, WBEM_INFINITE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_CONTROL};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIM_SETVERSION, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const TRAY_UID: u32 = 1;
const TRAY_MSG: u32 = 0x0400 + 1;
const ID_EXIT: u16 = 1002;
const ID_ABOUT: u16 = 1004;
const ID_SETTINGS: u16 = 1005;
const ID_TOGGLE: u16 = 1006;
// 托盘右键二级策略菜单(文案与设置面板一致, 改了就存盘)
const ID_M_ORDER_BLACK: u16 = 3001;
const ID_M_ORDER_DIM: u16 = 3002;
const ID_M_ORDER_FADE: u16 = 3003;
const ID_M_REST_READY: u16 = 3011;
const ID_M_REST_DELAY: u16 = 3012;
const ID_M_CUR_AUTO: u16 = 3021;
const ID_M_CUR_HIDE: u16 = 3022;
const ID_M_CUR_KEEP: u16 = 3023;
// 定时策略(托盘): 开关 + 熄屏/亮屏时间预设
const ID_M_SCHED_TOGGLE: u16 = 3031;
const ID_M_ON_BASE: u16 = 3100;  // 3100..3123 => 熄屏 时:00
const ID_M_OFF_BASE: u16 = 3200; // 3200..3223 => 亮屏 时:00
const WDA_EXCLUDEFROMCAPTURE: WINDOW_DISPLAY_AFFINITY = WINDOW_DISPLAY_AFFINITY(0x11);

static mut OVERLAY: Option<HWND> = None;
static mut TRAY: Option<HWND> = None;
static mut DARK: bool = false;
static mut BASE_DDC: Vec<(HANDLE, u32)> = Vec::new();
static mut BASE_NAMES: Vec<(String, u32)> = Vec::new();  // (描述, 基线) 用于持久化与按名匹配
static mut BASE_INTERNAL: u32 = 100;
static mut CFG: Cfg = Cfg::new();

#[derive(Clone, Copy)]
pub struct Cfg {
    pub dim_order: u8,      // 0=black_first 1=dim_first 2=fade
    pub restore_mode: u8,   // 0=ready(就绪即撤窗) 1=delay(固定延迟)
    pub delay_ms: u32,
    pub dim_internal: bool,
    pub dim_external: bool,
    pub cursor_mode: u8,    // 0=auto 1=hide 2=keep
    pub hdr_off: bool,
    pub silent_start: bool,
    pub close_action: u8,   // 0=hide 1=exit
    pub autostart: bool,
    pub hotkey_mod: u32,    // MOD_CONTROL|MOD_ALT 等(RegisterHotKey 修饰位)
    pub hotkey_vk: u32,     // 虚拟键码(默认 'S')
    // 定时策略: 每天到点自动熄屏 / 到点自动亮屏
    pub sched_enabled: bool,
    pub sched_on_h: u8,
    pub sched_on_m: u8,
    pub sched_off_h: u8,
    pub sched_off_m: u8,
}
impl Cfg {
    const fn new() -> Self {
        Self {
            dim_order: 0, restore_mode: 0, delay_ms: 140,
            dim_internal: true, dim_external: true,
            cursor_mode: 0, hdr_off: true, silent_start: false,
            close_action: 0, autostart: false,
            hotkey_mod: 0x0003, // MOD_ALT|MOD_CONTROL
            hotkey_vk: 0x53,    // 'S'
            sched_enabled: false,
            sched_on_h: 23, sched_on_m: 0,   // 默认 23:00 熄屏
            sched_off_h: 7, sched_off_m: 0,  // 默认 07:00 亮屏
        }
    }
}

// ---------------- 异常退出保护 ----------------
fn state_path() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push("sog_state.txt");
    p
}

unsafe fn mark_dark(on: bool) {
    if on {
        let _ = std::fs::write(state_path(), "1");
    } else {
        let _ = std::fs::remove_file(state_path());
    }
}

// 启动时: 若状态文件残留(上次被强杀), 把亮度恢复到安全值
unsafe fn recover_if_needed() {
    if !state_path().exists() {
        return;
    }
    log_line("recover: 上次异常退出, 恢复亮度");
    // 用 catch_unwind 包裹: 任何回调 panic 都不应导致进程 abort
    let _ = std::panic::catch_unwind(|| {
        if let Some(w) = Wmi::connect() {
            let _ = w.set_internal(100);
        }
        let _ = EnumDisplayMonitors(None, None, Some(recover_cb), LPARAM(0));
    });
    let _ = std::fs::remove_file(state_path());
    log_line("recover done");
}

unsafe extern "system" fn recover_cb(hmon: HMONITOR, _hdc: HDC, _rc: *mut RECT, _l: LPARAM) -> BOOL {
    let mut n = 0u32;
    if GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut n).is_ok() && n > 0 {
        let mut arr: Vec<PHYSICAL_MONITOR> = vec![PHYSICAL_MONITOR::default(); n as usize];
        if GetPhysicalMonitorsFromHMONITOR(hmon, arr.as_mut_slice()).is_ok() {
            for m in arr.iter() {
                let mon_h = m.hPhysicalMonitor;
                let (mut mn, mut cur, mut mx) = (0u32, 0u32, 0u32);
                if GetMonitorBrightness(mon_h, &mut mn, &mut cur, &mut mx) != 0 {
                    // 处于最低亮度(说明熄屏后未见恢复) -> 提到安全值 60
                    if cur <= mn + 2 {
                        let target = 60.max(mn + 2);
                        let _ = SetMonitorBrightness(mon_h, target);
                        log_line(&format!("recover ddc {} -> {}", cur, target));
                    }
                }
                let _ = DestroyPhysicalMonitor(mon_h);
            }
        }
    }
    BOOL(1)
}

// ---------------- 配置持久化 ----------------
fn cfg_path() -> std::path::PathBuf {
    let mut p = std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    p.push("sog.json");
    p
}

unsafe fn cfg_load() {
    if let Ok(t) = std::fs::read_to_string(cfg_path()) {
        for kv in t.replace(['{', '}', '"'], "").split(',') {
            let mut it = kv.split(':');
            let k = it.next().unwrap_or("").trim();
            let v = it.next().unwrap_or("").trim();
            if let Ok(n) = v.parse::<u32>() {
                match k {
                    "dim_order" => CFG.dim_order = (n as u8).min(2),
                    "restore_mode" => CFG.restore_mode = (n as u8).min(1),
                    "delay_ms" => CFG.delay_ms = n.min(500),
                    "dim_internal" => CFG.dim_internal = n != 0,
                    "dim_external" => CFG.dim_external = n != 0,
                    "cursor_mode" => CFG.cursor_mode = (n as u8).min(2),
                    "hdr_off" => CFG.hdr_off = n != 0,
                    "silent_start" => CFG.silent_start = n != 0,
                    "close_action" => CFG.close_action = (n as u8).min(1),
                    "autostart" => CFG.autostart = n != 0,
                    "hotkey_mod" => CFG.hotkey_mod = n & 0x000F,
                    "hotkey_vk" => CFG.hotkey_vk = n & 0x00FF,
                    "sched_enabled" => CFG.sched_enabled = n != 0,
                    "sched_on_h" => CFG.sched_on_h = (n as u8).min(23),
                    "sched_on_m" => CFG.sched_on_m = (n as u8).min(59),
                    "sched_off_h" => CFG.sched_off_h = (n as u8).min(23),
                    "sched_off_m" => CFG.sched_off_m = (n as u8).min(59),
                    _ => {}
                }
            }
        }
        log_line(&format!(
            "cfg loaded order={} mode={} delay={} cursor={}",
            CFG.dim_order, CFG.restore_mode, CFG.delay_ms, CFG.cursor_mode
        ));
    }
}

unsafe fn cfg_save() {
    let t = format!(
        "{{\"dim_order\":{},\"restore_mode\":{},\"delay_ms\":{},\"dim_internal\":{},\"dim_external\":{},\"cursor_mode\":{},\"hdr_off\":{},\"silent_start\":{},\"close_action\":{},\"autostart\":{},\"hotkey_mod\":{},\"hotkey_vk\":{},\"sched_enabled\":{},\"sched_on_h\":{},\"sched_on_m\":{},\"sched_off_h\":{},\"sched_off_m\":{}}}",
        CFG.dim_order, CFG.restore_mode, CFG.delay_ms,
        CFG.dim_internal as u8, CFG.dim_external as u8, CFG.cursor_mode,
        CFG.hdr_off as u8, CFG.silent_start as u8, CFG.close_action, CFG.autostart as u8,
        CFG.hotkey_mod, CFG.hotkey_vk,
        CFG.sched_enabled as u8, CFG.sched_on_h, CFG.sched_on_m,
        CFG.sched_off_h, CFG.sched_off_m
    );
    let _ = std::fs::write(cfg_path(), t);
    log_line("cfg saved");
}

/// 注销全局热键。设置面板进入"录入热键"捕获态时调用 ——
/// 否则用户在捕获框里按到原来那组键, WM_HOTKEY 会照常触发一次熄屏。
pub unsafe fn hotkey_unregister(_hwnd: HWND) {
    use windows::Win32::UI::Input::KeyboardAndMouse::UnregisterHotKey;
    let Some(t) = TRAY else { return };
    let _ = UnregisterHotKey(t, 1);
    log_line("hotkey unregistered (capture mode)");
}

// ---------------- 托盘气泡 ----------------
/// 托盘气泡提示。设置面板"最小化=收起到托盘"后用它告知用户去哪找回窗口
/// (本环境系统最小化无法还原, 只能收起; 不提示的话用户会以为程序被关了)。
pub unsafe fn tray_balloon(title: &str, text: &str) {
    let Some(t) = TRAY else { return };
    use windows::Win32::UI::Shell::{NIF_INFO, NIIF_INFO};
    let mut nid = NOTIFYICONDATAW::default();
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = t;
    nid.uID = TRAY_UID;
    nid.uFlags = NIF_INFO;
    let t1 = wide(title);
    let n1 = t1.len().min(nid.szInfoTitle.len());
    nid.szInfoTitle[..n1].copy_from_slice(&t1[..n1]);
    let t2 = wide(text);
    let n2 = t2.len().min(nid.szInfo.len());
    nid.szInfo[..n2].copy_from_slice(&t2[..n2]);
    nid.dwInfoFlags = NIIF_INFO;
    let ok = Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool();
    log_line(&format!("tray balloon ok={}", ok));
}

// ---------------- 定时策略 ----------------
/// 最后一次"用户手动"操作(热键/托盘菜单)的时刻, 毫秒(GetTickCount)。
/// 定时策略据此让步: 用户手动接管后 GRACE 窗口内不再自动熄屏/亮屏 ——
/// 用户明确要求"手动优先级高于定时, 可以随时终止这个定时动作"。
static mut MANUAL_AT: u32 = 0;
/// 手动操作后的"定时豁免窗口"(毫秒): 10 分钟内到点的定时动作一律跳过
const MANUAL_GRACE_MS: u32 = 10 * 60 * 1000;

pub unsafe fn mark_manual() {
    MANUAL_AT = windows::Win32::System::SystemInformation::GetTickCount();
    log_line("manual action (schedule yields)");
}


/// 每天到点自动熄屏 / 到点自动亮屏。由主消息循环每 500ms 调用一次,
/// 同一分钟只处理一次(避免重复触发)。
pub unsafe fn check_schedule() {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    if !CFG.sched_enabled {
        return;
    }
    static mut LAST_MIN: u32 = u32::MAX;
    let st = GetLocalTime(); // returns SYSTEMTIME (windows-0.58)
    let cur = st.wHour as u32 * 60 + st.wMinute as u32;
    if cur == LAST_MIN {
        return;
    }
    LAST_MIN = cur;
    let on = CFG.sched_on_h as u32 * 60 + CFG.sched_on_m as u32;
    let off = CFG.sched_off_h as u32 * 60 + CFG.sched_off_m as u32;
    if cur != on && cur != off {
        return;
    }
    // 手动优先: 用户在豁免窗口内操作过 → 本次定时动作直接跳过
    // (手动熄屏不被定时亮屏打断; 手动亮屏后也不被定时熄屏立刻盖掉)
    let now_ms = windows::Win32::System::SystemInformation::GetTickCount();
    if MANUAL_AT != 0 && now_ms.wrapping_sub(MANUAL_AT) < MANUAL_GRACE_MS {
        log_line(&format!(
            "schedule: skipped at {:02}:{:02} (manual override {}s ago)",
            cur / 60,
            cur % 60,
            now_ms.wrapping_sub(MANUAL_AT) / 1000
        ));
        return;
    }
    if cur == on && !DARK {
        log_line("schedule: auto dark");
        do_toggle();
    } else if cur == off && DARK {
        log_line("schedule: auto light");
        do_toggle();
    }
}

// ---------------- 全局热键 ----------------/// 重新注册全局热键(用户在设置面板改了组合键后调用)。true=成功。
/// 先注销旧的再注册新的; 组合键被占用时 RegisterHotKey 返回失败, 由调用方提示。
pub unsafe fn hotkey_apply(_hwnd: HWND, mods: u32, vk: u32) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
    };
    // 必须用托盘窗口注册/注销 —— 热键最初就注册在它上面。传设置窗口句柄
    // 会导致 UnregisterHotKey 注销错窗口(原热键仍生效), 随后 Register 报
    // "热键已注册"(用户实测: 捕获时按原热键仍触发一次熄屏)。
    let Some(t) = TRAY else { return false };
    let _ = UnregisterHotKey(t, 1);
    match RegisterHotKey(t, 1, HOT_KEY_MODIFIERS(mods), vk) {
        Ok(()) => {
            log_line(&format!("hotkey apply ok mod=0x{:X} vk=0x{:X}", mods, vk));
            true
        }
        Err(e) => {
            log_line(&format!("hotkey apply failed mod=0x{:X} vk=0x{:X}: {:?}", mods, vk, e));
            false
        }
    }
}

// ---------------- 设置面板辅助 ----------------
pub fn ui_font() -> windows::Win32::Graphics::Gdi::HFONT {
    unsafe {
        windows::Win32::Graphics::Gdi::HFONT(
            windows::Win32::Graphics::Gdi::GetStockObject(
                windows::Win32::Graphics::Gdi::DEFAULT_GUI_FONT,
            )
            .0,
        )
    }
}

pub fn cfg_clone() -> Cfg {
    unsafe { CFG }
}

pub unsafe fn cfg_apply(c: Cfg) {
    CFG = c;
    cfg_save();
    log_line("cfg applied by settings");
}

pub fn is_autostart_on() -> bool {
    unsafe { CFG.autostart }
}

/// 实测各屏"从最低亮度回到基线"的耗时, 结果回填滑块并把最慢值写进数字框。
/// 会闪两次屏(压暗 -> 恢复), 由用户点按钮触发。
pub fn run_measure() {
    unsafe {
        settings::set_measure_text("检测中…（屏幕会闪两次）");
        std::thread::spawn(|| unsafe {
            std::thread::sleep(std::time::Duration::from_millis(300));

            // ---- 1) 记录当前基线并压到最低 ----
            let mut plan: Vec<(HANDLE, u32, u32)> = Vec::new(); // (句柄, 基线, 最低)
            MEASURE_PLAN = &mut plan as *mut Vec<(HANDLE, u32, u32)>;
            let _ = EnumDisplayMonitors(None, None, Some(measure_collect), LPARAM(0));

            for (h, _b, mn) in plan.iter() {
                let _ = SetMonitorBrightness(*h, *mn);
            }
            let internal_base = if let Some(w) = Wmi::connect() {
                let cur = w.get_internal().unwrap_or(100);
                let _ = w.set_internal(0);
                cur
            } else {
                0
            };
            std::thread::sleep(std::time::Duration::from_millis(900));

            // ---- 2) 计时恢复 ----
            let t0 = std::time::Instant::now();
            let mut worst = 0u128;

            for (h, base, _mn) in plan.iter() {
                let t = std::time::Instant::now();
                let _ = SetMonitorBrightness(*h, *base);
                // 轮询到位
                let mut waited = 0u64;
                loop {
                    let (mut a, mut c, mut z) = (0u32, 0u32, 0u32);
                    if GetMonitorBrightness(*h, &mut a, &mut c, &mut z) != 0 && c + 3 >= *base {
                        let ms = t.elapsed().as_millis();
                        worst = worst.max(ms);
                        log_line(&format!("measure ddc -> {} in {} ms", base, ms));
                        break;
                    }
                    waited += 10;
                    if waited > 500 {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                let _ = DestroyPhysicalMonitor(*h);
            }

            if internal_base > 0 {
                let t = std::time::Instant::now();
                if let Some(w) = Wmi::connect() {
                    let _ = w.set_internal(internal_base as u8);
                }
                let ms = t.elapsed().as_millis();
                worst = worst.max(ms);
                log_line(&format!("measure internal -> {} in {} ms", internal_base, ms));
            }

            let total = t0.elapsed().as_millis();
            let v = worst.min(500) as u32;
            log_line(&format!("measure done: worst={} ms total={} ms", v, total));

            // ---- 3) 回填 ----
            let mut cc = crate::cfg_clone();
            cc.delay_ms = v.min(500);
            crate::cfg_apply(cc);
            settings::set_measure_text(&format!("最慢通道 {} ms（已填入）", v));
        });
    }
}

static mut MEASURE_PLAN: *mut Vec<(HANDLE, u32, u32)> = core::ptr::null_mut();

unsafe extern "system" fn measure_collect(
    hmon: HMONITOR,
    _hdc: HDC,
    _rc: *mut RECT,
    _l: LPARAM,
) -> BOOL {
    let mut n = 0u32;
    if GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut n).is_ok() && n > 0 {
        let mut arr: Vec<PHYSICAL_MONITOR> = vec![PHYSICAL_MONITOR::default(); n as usize];
        if GetPhysicalMonitorsFromHMONITOR(hmon, arr.as_mut_slice()).is_ok() {
            for m in arr.iter() {
                let mon_h = m.hPhysicalMonitor;
                let (mut mn, mut cur, mut mx) = (0u32, 0u32, 0u32);
                if GetMonitorBrightness(mon_h, &mut mn, &mut cur, &mut mx) != 0 {
                    let base = if cur > mn + 2 { cur } else { 60 };
                    if !MEASURE_PLAN.is_null() {
                        (*MEASURE_PLAN).push((mon_h, base, mn));
                    }
                }
            }
        }
    }
    BOOL(1)
}

// 单实例互斥
static mut MUTEX: Option<windows::Win32::Foundation::HANDLE> = None;

unsafe fn single_instance() -> bool {
    use windows::Win32::Foundation::{SetLastError, WIN32_ERROR};
    use windows::Win32::System::Threading::CreateMutexW;

    // 必须先清零: 否则 GetLastError 读到的是上一次调用的残留错误码(如 183),
    // 会导致"明明没有实例却判定为已运行" —— 与 Python 版踩过的坑一致
    SetLastError(WIN32_ERROR(0));

    let name = wide("SOGRustSingleton");
    match CreateMutexW(None, BOOL(1), PCWSTR(name.as_ptr())) {
        Ok(h) => {
            let err = windows::Win32::Foundation::GetLastError();
            // 句柄必须长期持有(存到静态), 否则被 Drop 后互斥失效
            MUTEX = Some(h);
            err.0 != 183 // ERROR_ALREADY_EXISTS
        }
        Err(_) => false,
    }
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(once(0)).collect()
}

pub fn log_line(s: &str) {
    use std::io::Write;
    let mut p = std::env::temp_dir();
    p.push("sog_rust.log");
    // 主文件被外部工具(杀毒/索引/tail)锁住时静默丢日志会误导诊断,
    // 失败则落到备用文件
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{}", s);
    } else {
        p.set_file_name("sog_rust2.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = writeln!(f, "{}", s);
        }
    }
}

// ---------------- WMI 内置屏 ----------------
struct Wmi {
    svc: IWbemServices,
}

impl Wmi {
    unsafe fn connect() -> Option<Self> {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_err() && hr.0 != -2147417850 {
            log_line(&format!("wmi: CoInit err {}", hr.0));
            return None;
        }
        // 设置 COM 安全级别(否则 IWbemServices 的跨进程调用会被拒绝)
        let sec = CoInitializeSecurity(
            None,
            -1,
            None,
            None,
            RPC_C_AUTHN_LEVEL_DEFAULT,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
            None,
        );
        log_line(&format!("wmi: security ok={}", sec.is_ok()));
        let loc: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).ok()?;
        // ConnectServer(网络资源, 用户, 密码, 区域, 安全标志, 授权, 上下文)
        let svc = loc
            .ConnectServer(
                &BSTR::from("root\\wmi"),
                &BSTR::new(),
                &BSTR::new(),
                &BSTR::new(),
                0,
                &BSTR::new(),
                None,
            )
            .ok()?;
        log_line("wmi: connected");
        Some(Self { svc })
    }

    unsafe fn get_internal(&self) -> Option<u32> {
        log_line("wmi: get_internal begin");
        let en: IEnumWbemClassObject = match self.svc.ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from("SELECT * FROM WmiMonitorBrightness"),
            WBEM_GENERIC_FLAG_TYPE(0),
            None,
        ) {
            Ok(e) => e,
            Err(_) => {
                log_line("wmi: query Brightness err");
                return None;
            }
        };
        let mut objs: [Option<IWbemClassObject>; 1] = [None];
        let mut got = 0u32;
        if en.Next(WBEM_INFINITE, &mut objs, &mut got).is_err() || got == 0 {
            log_line("wmi: no Brightness instance");
            return None;
        }
        log_line("wmi: got brightness obj");
        let obj = objs[0].as_ref()?;
        let mut v = VARIANT::default();
        let pn = wide("CurrentBrightness");
        if obj.Get(PCWSTR(pn.as_ptr()), 0, &mut v, None, None).is_err() {
            return None;
        }
        // VARIANT 只暴露 as_raw(); VT_I4=3, VT_UI1=17
        let raw = v.as_raw();
        let vt = unsafe { raw.Anonymous.Anonymous.vt };
        if vt == 3 {
            Some(unsafe { raw.Anonymous.Anonymous.Anonymous.lVal } as u32)
        } else if vt == 17 {
            Some(unsafe { raw.Anonymous.Anonymous.Anonymous.bVal } as u32)
        } else {
            None
        }
    }

    unsafe fn set_internal(&self, val: u8) -> bool {
        // 1) 取 WmiMonitorBrightness 实例(它的 __PATH 带实例标识)
        let en = match self.svc.ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from("SELECT * FROM WmiMonitorBrightness"),
            WBEM_GENERIC_FLAG_TYPE(0),
            None,
        ) {
            Ok(e) => e,
            Err(_) => {
                log_line("wmi: query Brightness err");
                return false;
            }
        };
        let mut objs: [Option<IWbemClassObject>; 1] = [None];
        let mut got = 0u32;
        if en.Next(WBEM_INFINITE, &mut objs, &mut got).is_err() || got == 0 {
            log_line("wmi: no Brightness instance");
            return false;
        }
        let bobj = match objs[0].as_ref() {
            Some(o) => o.clone(),
            None => return false,
        };

        // 2) 取该实例的 __PATH(例如 ...WmiMonitorBrightness.InstanceName="...")
        let mut pathv = VARIANT::new();
        if bobj
            .Get(PCWSTR(wide("__PATH").as_ptr()), 0, &mut pathv, None, None)
            .is_err()
        {
            log_line("wmi: __PATH err");
            return false;
        }
        if unsafe { pathv.as_raw().Anonymous.Anonymous.vt } != 8 {
            log_line("wmi: __PATH not bstr");
            return false;
        }
        // 方法定义在 Methods 类上: 把路径里的类名替换为 Methods 类
        let full = unsafe { pathv.as_raw().Anonymous.Anonymous.Anonymous.bstrVal };
        let full_bstr = unsafe { BSTR::from_raw(full) };
        let path_str = full_bstr.to_string();
        let method_path = path_str.replace("WmiMonitorBrightness", "WmiMonitorBrightnessMethods");
        let path = BSTR::from(&method_path);
        log_line(&format!("wmi: method path={}", method_path));

        // 3) 取 Methods 类的定义(用 GetObject 拿类定义)以取方法签名
        let mut cls_out: Option<IWbemClassObject> = None;
        if self
            .svc
            .GetObject(
                &BSTR::from("WmiMonitorBrightnessMethods"),
                WBEM_GENERIC_FLAG_TYPE(0),
                None,
                Some(&mut cls_out),
                None,
            )
            .is_err()
        {
            log_line("wmi: GetObject Methods err");
            return false;
        }
        let cls = match cls_out {
            Some(c) => c,
            None => {
                log_line("wmi: no Methods class");
                return false;
            }
        };
        let mut in_sig: Option<IWbemClassObject> = None;
        let mut out_sig: Option<IWbemClassObject> = None;
        if cls
            .GetMethod(
                PCWSTR(wide("WmiSetBrightness").as_ptr()),
                0,
                &mut in_sig,
                &mut out_sig,
            )
            .is_err()
        {
            log_line("wmi: GetMethod err");
            return false;
        }
        let in_sig = match in_sig {
            Some(s) => s,
            None => {
                log_line("wmi: no in_sig");
                return false;
            }
        };
        let params = match in_sig.SpawnInstance(0) {
            Ok(i) => i,
            Err(_) => {
                log_line("wmi: SpawnInstance err");
                return false;
            }
        };

        let mut vt = VARIANT::new();
        unsafe {
            let r = vt.as_raw() as *const _ as *mut windows::core::imp::VARIANT;
            (*r).Anonymous.Anonymous.vt = 3; // VT_I4
            (*r).Anonymous.Anonymous.Anonymous.lVal = 1;
        }
        let _ = params.Put(PCWSTR(wide("Timeout").as_ptr()), 0, &vt, 0);
        let mut vb = VARIANT::new();
        unsafe {
            let r = vb.as_raw() as *const _ as *mut windows::core::imp::VARIANT;
            (*r).Anonymous.Anonymous.vt = 17; // VT_UI1
            (*r).Anonymous.Anonymous.Anonymous.bVal = val;
        }
        let _ = params.Put(PCWSTR(wide("Brightness").as_ptr()), 0, &vb, 0);

        let ok = self
            .svc
            .ExecMethod(
                &path,
                &BSTR::from("WmiSetBrightness"),
                WBEM_GENERIC_FLAG_TYPE(0),
                None,
                &params,
                None,
                None,
            )
            .is_ok();
        log_line(&format!("wmi: ExecMethod ok={} val={}", ok, val));
        ok
    }
}

impl Drop for Wmi {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

// ---------------- DDC 外接屏 ----------------
unsafe extern "system" fn enum_cb(hmon: HMONITOR, _hdc: HDC, _rc: *mut RECT, _l: LPARAM) -> BOOL {
    let mut n = 0u32;
    if GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut n).is_ok() && n > 0 {
        let mut arr: Vec<PHYSICAL_MONITOR> = vec![PHYSICAL_MONITOR::default(); n as usize];
        if GetPhysicalMonitorsFromHMONITOR(hmon, arr.as_mut_slice()).is_ok() {
            for m in arr.iter() {
                let (mut mn, mut cur, mut mx) = (0u32, 0u32, 0u32);
                let mon_h = m.hPhysicalMonitor; // packed struct: 先拷贝, 避免未对齐引用
                if GetMonitorBrightness(mon_h, &mut mn, &mut cur, &mut mx) != 0 {
                    // 描述作为稳定标识(句柄每次枚举会变)
                    let raw_desc = m.szPhysicalMonitorDescription; // packed: 先拷贝
                    let desc = raw_desc
                        .iter()
                        .take_while(|c| **c != 0)
                        .map(|c| String::from_utf16_lossy(&[*c]))
                        .collect::<String>();
                    // 同描述的显示器用序号区分(实测两块都叫 Generic PnP Monitor)
                    let idx = BASE_DDC.len();
                    let key = if idx == 0 { desc.clone() } else { format!("{}#{}", desc, idx + 1) };
                    let remembered = BASE_NAMES.iter().find(|(d, _)| *d == key).map(|(_, b)| *b);
                    let base = if cur > mn + 2 {
                        cur
                    } else {
                        remembered.filter(|v| *v > mn + 2).unwrap_or(60)
                    };
                    // 更新记忆
                    if let Some(e) = BASE_NAMES.iter_mut().find(|(d, _)| *d == key) {
                        e.1 = base;
                    } else {
                        BASE_NAMES.push((key, base));
                    }
                    BASE_DDC.push((mon_h, base));
                    let _ = SetMonitorBrightness(mon_h, mn);
                }
            }
        }
    }
    BOOL(1)
}

unsafe fn dim_external() {
    BASE_DDC.clear();
    let _ = EnumDisplayMonitors(None, None, Some(enum_cb), LPARAM(0));
    log_line(&format!("ddc dim: {} mon", BASE_DDC.len()));
}

unsafe fn restore_external() {
    // 重新枚举并按出现顺序匹配 BASE_NAMES(同描述用 #序号 区分)
    RESTORE_IDX[0] = 0;
    let _ = EnumDisplayMonitors(None, None, Some(restore_cb), LPARAM(0));
    log_line(&format!("ddc restore: {} mon", BASE_NAMES.len()));
    BASE_DDC.clear();
}

static mut RESTORE_IDX: [usize; 1] = [0];

unsafe extern "system" fn restore_cb(hmon: HMONITOR, _hdc: HDC, _rc: *mut RECT, _l: LPARAM) -> BOOL {
    let mut n = 0u32;
    if GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut n).is_ok() && n > 0 {
        let mut arr: Vec<PHYSICAL_MONITOR> = vec![PHYSICAL_MONITOR::default(); n as usize];
        if GetPhysicalMonitorsFromHMONITOR(hmon, arr.as_mut_slice()).is_ok() {
            for m in arr.iter() {
                let mon_h = m.hPhysicalMonitor;
                let (mut mn, mut cur, mut mx) = (0u32, 0u32, 0u32);
                let readable = GetMonitorBrightness(mon_h, &mut mn, &mut cur, &mut mx) != 0;
                if readable {
                    let i = RESTORE_IDX[0];
                    if let Some((key, base)) = BASE_NAMES.get(i) {
                        let _ = SetMonitorBrightness(mon_h, *base);
                        let (mut m2, mut c2, mut x2) = (0u32, 0u32, 0u32);
                        if GetMonitorBrightness(mon_h, &mut m2, &mut c2, &mut x2) != 0 && c2 + 3 < *base
                        {
                            let _ = SetMonitorBrightness(mon_h, *base);
                        }
                        log_line(&format!("restore {} -> {}", key, base));
                    }
                    RESTORE_IDX[0] += 1;
                }
                let _ = DestroyPhysicalMonitor(mon_h);
            }
        }
    }
    BOOL(1)
}


// ---------------- 黑窗 ----------------
unsafe fn screen_rect() -> (i32, i32, i32, i32) {
    (
        GetSystemMetrics(SM_XVIRTUALSCREEN),
        GetSystemMetrics(SM_YVIRTUALSCREEN),
        GetSystemMetrics(SM_CXVIRTUALSCREEN),
        GetSystemMetrics(SM_CYVIRTUALSCREEN),
    )
}

unsafe extern "system" fn overlay_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(h, m, w, l)
}

unsafe fn create_overlay() -> bool {
    let (x, y, w, h) = screen_rect();
    let cls = wide("SOGOverlay");
    let title = wide("screen_off");
    let hmod = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
    let hinst: HINSTANCE = HINSTANCE(hmod.0);

    let mut wc = WNDCLASSW::default();
    wc.lpfnWndProc = Some(overlay_proc);
    wc.hInstance = hinst;
    wc.hbrBackground = HBRUSH(CreateSolidBrush(COLORREF(0)).0);
    wc.lpszClassName = PCWSTR(cls.as_ptr());
    let _ = RegisterClassW(&wc);

    let hwnd = match CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
        PCWSTR(cls.as_ptr()),
        PCWSTR(title.as_ptr()),
        WS_POPUP | WS_VISIBLE,
        x,
        y,
        w,
        h,
        None,
        None,
        hinst,
        None,
    ) {
        Ok(h) => h,
        Err(_) => return false,
    };
    if hwnd.0.is_null() {
        return false;
    }
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
    let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (WS_EX_TRANSPARENT | WS_EX_LAYERED).0 as isize);
    let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    OVERLAY = Some(hwnd);
    log_line("overlay on");
    true
}

unsafe fn destroy_overlay() {
    if let Some(h) = OVERLAY.take() {
        let _ = DestroyWindow(h);
        log_line("overlay off");
    }
}

// ---------------- 切换 ----------------
unsafe fn do_toggle() {
    let wmi = Wmi::connect();
    if DARK {
        // 亮屏: 先恢复亮度
        if CFG.dim_internal {
            if let Some(w) = &wmi {
                if w.set_internal(BASE_INTERNAL as u8) {
                    log_line(&format!("internal -> {}", BASE_INTERNAL));
                }
            }
        }
        if CFG.dim_external {
            restore_external();
        }
        // 按策略决定: ready=等到位(已同步完成) / delay=额外延迟
        if CFG.restore_mode == 1 {
            let ms = CFG.delay_ms.min(500);
            if ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(ms as u64));
            }
            log_line(&format!("delay {}ms then withdraw", ms));
        }
        cursor::restore();
        destroy_overlay();
        DARK = false;
        mark_dark(false);
        log_line("state off");
    } else {
        // 熄屏: 先抓基线
        if CFG.dim_internal {
            if let Some(w) = &wmi {
                if let Some(cur) = w.get_internal() {
                    if cur > 5 {
                        BASE_INTERNAL = cur;
                    }
                }
            }
        }
        log_line(&format!("baseline internal={}", BASE_INTERNAL));

        // 熄屏顺序: 0=先黑后熄 1=先熄后黑 2=淡入
        if CFG.dim_order == 1 {
            // 先熄后黑: 先压暗, 再盖窗
            if CFG.dim_internal {
                if let Some(w) = &wmi {
                    let _ = w.set_internal(0);
                }
            }
            if CFG.dim_external {
                dim_external();
            }
            if !create_overlay() {
                return;
            }
        } else {
            // 先黑后熄 / 淡入: 先盖窗, 再压暗
            if !create_overlay() {
                return;
            }
            cursor::hide();
            if CFG.dim_internal {
                if let Some(w) = &wmi {
                    let _ = w.set_internal(0);
                }
            }
            if CFG.dim_external {
                dim_external();
            }
        }
        DARK = true;
        mark_dark(true);
        log_line(&format!("state on (order={})", CFG.dim_order));
    }
    if let Some(t) = TRAY {
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = t;
        nid.uID = TRAY_UID;
        nid.uFlags = NIF_MESSAGE | NIF_TIP | NIF_ICON;
        nid.uCallbackMessage = TRAY_MSG;
        nid.hIcon = tray_icon();
        let tip = wide(if DARK { "off" } else { "on" });
        let n = tip.len().min(nid.szTip.len());
        nid.szTip[..n].copy_from_slice(&tip[..n]);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

// ---------------- 托盘 ----------------
/// 按指定像素尺寸加载应用图标(标题栏小尺寸绘制时避免大图标缩放锯齿)
pub unsafe fn app_icon_sized(px: i32) -> HICON {
    use windows::Win32::UI::WindowsAndMessaging::{LoadImageW, IMAGE_ICON, LR_LOADFROMFILE};
    if let Ok(exe) = std::env::current_exe() {
        let ico = exe.with_file_name("icon.ico");
        if ico.exists() {
            let w: Vec<u16> = ico.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
            if let Ok(hi) = LoadImageW(None, PCWSTR(w.as_ptr()), IMAGE_ICON, px, px, LR_LOADFROMFILE) {
                if !hi.is_invalid() {
                    return HICON(hi.0);
                }
            }
        }
    }
    tray_icon()
}

/// 托盘图标(共享句柄无需销毁)。
/// 按 DPI 实际小图标尺寸(SM_CXSMICON, 高 DPI 下为 20/24 等物理像素)1:1 加载 ——
/// 旧写法 LoadImageW(0,0) 拿默认 32px 再被系统缩到 20px, 实测托盘图标发糊。
unsafe fn tray_icon() -> HICON {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, LoadIconW, LoadImageW, IDI_APPLICATION, IMAGE_ICON, LR_LOADFROMFILE,
        SM_CXSMICON,
    };
    let px = GetSystemMetrics(SM_CXSMICON);
    if let Ok(icon) = app_icon_file(px) {
        return icon;
    }
    LoadIconW(HINSTANCE::default(), IDI_APPLICATION).unwrap_or_default()
}

/// 从 exe 同目录 icon.ico 按指定像素加载图标(ico 内含 16..256 尺寸链, 1:1 无缩放)
unsafe fn app_icon_file(px: i32) -> Result<HICON, ()> {
    use windows::Win32::UI::WindowsAndMessaging::{LoadImageW, IMAGE_ICON, LR_LOADFROMFILE};
    let exe = std::env::current_exe().map_err(|_| ())?;
    let ico = exe.with_file_name("icon.ico");
    if !ico.exists() {
        return Err(());
    }
    let w: Vec<u16> = ico.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let hi = LoadImageW(None, PCWSTR(w.as_ptr()), IMAGE_ICON, px, px, LR_LOADFROMFILE)
        .map_err(|_| ())?;
    if hi.is_invalid() {
        return Err(());
    }
    Ok(HICON(hi.0))
}

unsafe fn add_tray(hwnd: HWND) {
    let mut nid = NOTIFYICONDATAW::default();
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    // NIF_ICON 必须带: 之前漏掉导致托盘完全没有图标, 用户找不到入口
    nid.uFlags = NIF_MESSAGE | NIF_TIP | NIF_ICON;
    nid.uCallbackMessage = TRAY_MSG;
    nid.hIcon = tray_icon();
    let tip = wide("一键熄屏");
    let n = tip.len().min(nid.szTip.len());
    nid.szTip[..n].copy_from_slice(&tip[..n]);
    let ok = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
    let err = windows::Win32::Foundation::GetLastError();
    log_line(&format!("tray add ok={} err={}", ok, err.0));
    // 注意: 绝不能用 NIM_SETVERSION(4) —— V4 协议下回调 lParam 变成坐标,
    // 事件类型移到 wParam, 与 wnd_proc 的老协议解析(TRAY_MSG lParam=鼠标消息)不兼容,
    // 会导致托盘点击完全失效(实测踩坑)。
}

/// 窗口类公共图标/光标(任务栏与标题栏图标、箭头光标)
pub(crate) unsafe fn class_icon_cursor(wc: &mut WNDCLASSW) {
    // 任务栏/Alt-Tab 用大图标: 按 SM_CXICON(DPI 缩放后的物理像素)1:1 加载, 避免缩放发糊
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXICON};
    wc.hIcon = app_icon_file(GetSystemMetrics(SM_CXICON)).unwrap_or_else(|_| tray_icon());
    // hCursor 必须为 NULL: 非 NULL 类光标会在每次鼠标移动时被系统恢复,
    // 压掉设置面板的边框缩放光标/手型光标(光标由 WM_SETCURSOR + 16ms 轮询自管)。
    // HTCLIENT 由面板自设手/箭头, NC 区由 DefWindowProc 按返回码设缩放光标。
}

pub unsafe fn on_settings_save(h: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{BM_GETCHECK, GetDlgItem};
    let get_check = |id: i32| -> bool {
        let c = GetDlgItem(h, id).unwrap_or_default();
        if c.0.is_null() { return false; }
        SendMessageW(c, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 != 0
    };
    let get_pos = |id: i32| -> u32 {
        let c = GetDlgItem(h, id).unwrap_or_default();
        if c.0.is_null() { return 140; }
        SendMessageW(c, 1024, WPARAM(0), LPARAM(0)).0 as u32
    };
    if get_check(settings::IDC_ORDER_DIM as i32) { CFG.dim_order = 1; }
    else if get_check(settings::IDC_ORDER_FADE as i32) { CFG.dim_order = 2; }
    else { CFG.dim_order = 0; }

    CFG.restore_mode = if get_check(settings::IDC_REST_DELAY as i32) { 1 } else { 0 };
    // 延迟由滑块直接写入 CFG(滑块已自绘, 无系统 trackbar)
    cfg_save();
}

unsafe fn show_menu(hwnd: HWND) {
    let m = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };
    // 顶部实用动作: 立即熄屏/恢复(DARK 状态动态文案)
    let action = if DARK { "恢复亮屏" } else { "立即熄屏" };
    let _ = AppendMenuW(m, MF_STRING, ID_TOGGLE as usize, PCWSTR(wide(action).as_ptr()));
    let _ = AppendMenuW(m, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(m, MF_STRING, ID_SETTINGS as usize, PCWSTR(wide("设置面板").as_ptr()));
    let _ = AppendMenuW(m, MF_SEPARATOR, 0, PCWSTR::null());

    // ---- 策略二级菜单(用户要求: 亮屏后光标/熄屏顺序/亮屏方式 收进二级页) ----
    let sub = |items: &[(&str, u16, bool)]| -> HMENU {
        let s = CreatePopupMenu().unwrap_or_default();
        for (txt, id, checked) in items {
            let mut f = MF_STRING;
            if *checked {
                f |= MF_CHECKED;
            }
            let _ = AppendMenuW(s, f, *id as usize, PCWSTR(wide(txt).as_ptr()));
        }
        s
    };

    let order = sub(&[
        ("先黑后熄", ID_M_ORDER_BLACK, CFG.dim_order == 0),
        ("先熄后黑", ID_M_ORDER_DIM, CFG.dim_order == 1),
        ("淡入并行", ID_M_ORDER_FADE, CFG.dim_order == 2),
    ]);
    let restore = sub(&[
        ("亮度就绪即撤窗", ID_M_REST_READY, CFG.restore_mode == 0),
        ("固定延迟", ID_M_REST_DELAY, CFG.restore_mode == 1),
    ]);
    let cursor = sub(&[
        ("远控保留，其余隐藏", ID_M_CUR_AUTO, CFG.cursor_mode == 0),
        ("始终隐藏", ID_M_CUR_HIDE, CFG.cursor_mode == 1),
        ("始终保留", ID_M_CUR_KEEP, CFG.cursor_mode == 2),
    ]);
    let _ = AppendMenuW(m, MF_POPUP, order.0 as usize, PCWSTR(wide("熄屏顺序").as_ptr()));
    let _ = AppendMenuW(m, MF_POPUP, restore.0 as usize, PCWSTR(wide("亮屏方式").as_ptr()));
    let _ = AppendMenuW(m, MF_POPUP, cursor.0 as usize, PCWSTR(wide("亮屏后光标").as_ptr()));

    // ---- 定时策略: 开关 + 熄屏/亮屏时间(整点预设, 精确到分请在设置面板改) ----
    let sched = CreatePopupMenu().unwrap_or_default();
    let mut f = MF_STRING;
    if CFG.sched_enabled {
        f |= MF_CHECKED;
    }
    let _ = AppendMenuW(sched, f, ID_M_SCHED_TOGGLE as usize, PCWSTR(wide("启用定时").as_ptr()));
    let _ = AppendMenuW(sched, MF_SEPARATOR, 0, PCWSTR::null());

    let mut on_menu = CreatePopupMenu().unwrap_or_default();
    for h in 0..24u16 {
        let mut f = MF_STRING;
        if CFG.sched_on_h as u16 == h && CFG.sched_on_m == 0 {
            f |= MF_CHECKED;
        }
        let txt = format!("{:02}:00", h);
        let _ = AppendMenuW(on_menu, f, (ID_M_ON_BASE + h) as usize, PCWSTR(wide(&txt).as_ptr()));
    }
    let _ = AppendMenuW(sched, MF_POPUP, on_menu.0 as usize, PCWSTR(wide("熄屏时间").as_ptr()));

    let mut off_menu = CreatePopupMenu().unwrap_or_default();
    for h in 0..24u16 {
        let mut f = MF_STRING;
        if CFG.sched_off_h as u16 == h && CFG.sched_off_m == 0 {
            f |= MF_CHECKED;
        }
        let txt = format!("{:02}:00", h);
        let _ = AppendMenuW(off_menu, f, (ID_M_OFF_BASE + h) as usize, PCWSTR(wide(&txt).as_ptr()));
    }
    let _ = AppendMenuW(sched, MF_POPUP, off_menu.0 as usize, PCWSTR(wide("亮屏时间").as_ptr()));

    let _ = AppendMenuW(m, MF_POPUP, sched.0 as usize, PCWSTR(wide("定时策略").as_ptr()));
    let _ = AppendMenuW(m, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(m, MF_STRING, ID_EXIT as usize, PCWSTR(wide("退出").as_ptr()));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    // TPM_RETURNCMD: TrackPopupMenu 直接返回被点击的 ID(不会发 WM_COMMAND),
    // 必须在这里处理, 否则点菜单没反应 —— 之前正是漏了这一步。
    // DestroyMenu(父) 会连带销毁 MF_POPUP 挂上的子菜单, 无需单独释放。
    let cmd = TrackPopupMenu(
        m,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = DestroyMenu(m);
    if cmd.0 == 0 {
        return;
    }
    match cmd.0 as u16 {
        ID_TOGGLE => {
            mark_manual();
            do_toggle();
        }
        ID_SETTINGS => {
            let hinst: HINSTANCE =
                HINSTANCE(GetModuleHandleW(PCWSTR::null()).unwrap_or_default().0);
            let _ = settings::show(hinst, hwnd, &mut CFG);
            log_line("settings opened");
        }
        ID_M_ORDER_BLACK | ID_M_ORDER_DIM | ID_M_ORDER_FADE => {
            CFG.dim_order = match cmd.0 as u16 {
                ID_M_ORDER_DIM => 1,
                ID_M_ORDER_FADE => 2,
                _ => 0,
            };
            cfg_save();
            log_line(&format!("tray menu dim_order={}", CFG.dim_order));
        }
        ID_M_REST_READY | ID_M_REST_DELAY => {
            CFG.restore_mode = if cmd.0 as u16 == ID_M_REST_DELAY { 1 } else { 0 };
            cfg_save();
            log_line(&format!("tray menu restore_mode={}", CFG.restore_mode));
        }
        ID_M_CUR_AUTO | ID_M_CUR_HIDE | ID_M_CUR_KEEP => {
            CFG.cursor_mode = match cmd.0 as u16 {
                ID_M_CUR_HIDE => 1,
                ID_M_CUR_KEEP => 2,
                _ => 0,
            };
            cfg_save();
            log_line(&format!("tray menu cursor_mode={}", CFG.cursor_mode));
        }
        ID_M_SCHED_TOGGLE => {
            CFG.sched_enabled = !CFG.sched_enabled;
            cfg_save();
            log_line(&format!("tray menu sched_enabled={}", CFG.sched_enabled));
        }
        id if id >= ID_M_ON_BASE && id < ID_M_ON_BASE + 24 => {
            CFG.sched_on_h = (id - ID_M_ON_BASE) as u8;
            CFG.sched_on_m = 0;
            cfg_save();
            log_line(&format!("tray menu sched_on={:02}:00", CFG.sched_on_h));
        }
        id if id >= ID_M_OFF_BASE && id < ID_M_OFF_BASE + 24 => {
            CFG.sched_off_h = (id - ID_M_OFF_BASE) as u8;
            CFG.sched_off_m = 0;
            cfg_save();
            log_line(&format!("tray menu sched_off={:02}:00", CFG.sched_off_h));
        }
        ID_ABOUT => {
            let _ = MessageBoxW(
                hwnd,
                PCWSTR(wide("一键熄屏 v0.1 (Rust)\n\n托盘左键: 打开设置面板\n托盘右键: 策略菜单\n热键: 见设置面板").as_ptr()),
                PCWSTR(wide("关于").as_ptr()),
                MB_OK,
            );
        }
        ID_EXIT => {
            cleanup();
            PostQuitMessage(0);
        }
        _ => {}
    }
}

unsafe fn cleanup() {
    if DARK {
        if let Some(w) = Wmi::connect() {
            let _ = w.set_internal(BASE_INTERNAL as u8);
        }
        restore_external();
        cursor::restore();
        destroy_overlay();
        DARK = false;
        mark_dark(false);
        log_line("cleanup restored");
    }
    if let Some(t) = TRAY {
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = t;
        nid.uID = TRAY_UID;
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        let _ = DestroyWindow(t);
    }
}

unsafe extern "system" fn wnd_proc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            if w.0 == 2 {
                check_schedule();
                return LRESULT(0);
            }
            DefWindowProcW(h, msg, w, l)
        }
        WM_HOTKEY => {
            // 设置面板正在录入热键时忽略 —— 双保险(注销失败也不会误触发熄屏)
            if settings::CAPTURING.load(std::sync::atomic::Ordering::Relaxed) {
                log_line("WM_HOTKEY ignored (capturing)");
                return LRESULT(0);
            }
            log_line("WM_HOTKEY recv");
            mark_manual();
            do_toggle();
            LRESULT(0)
        }
        TRAY_MSG => {
            match l.0 as u32 {
                WM_RBUTTONUP => show_menu(h),
                // 用户要求: 左键不再触发熄屏, 改为弹出设置面板(熄屏走热键/右键菜单)
                WM_LBUTTONUP => {
                    let hinst: HINSTANCE =
                        HINSTANCE(GetModuleHandleW(PCWSTR::null()).unwrap_or_default().0);
                    let _ = settings::show(hinst, h, &mut CFG);
                    log_line("tray left click -> settings");
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (w.0 & 0xFFFF) as u16;
            if id == ID_EXIT {
                cleanup();
                PostQuitMessage(0);
            } else if id == ID_SETTINGS {
                let hinst: HINSTANCE =
                    HINSTANCE(GetModuleHandleW(PCWSTR::null()).unwrap_or_default().0);
                let _ = settings::show(hinst, h, &mut CFG);
            } else if id == ID_ABOUT {
                let _ = MessageBoxW(
                    h,
                    PCWSTR(wide("sog v0.1 (Rust S1)\n\nCtrl+Alt+S").as_ptr()),
                    PCWSTR(wide("about").as_ptr()),
                    MB_OK,
                );
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            cleanup();
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, w, l),
    }
}

fn main() {
    unsafe {
        // 必须最先调用: Per-Monitor V2 DPI 感知(否则高分屏被系统位图拉伸发糊)
        settings::enable_dpi();
        if !single_instance() {
            log_line("already running, exit");
            return;
        }
        let hmod = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
    let hinst: HINSTANCE = HINSTANCE(hmod.0);
        let cls = wide("SOGTray");
        let title = wide("sog");

        let mut wc = WNDCLASSW::default();
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = hinst;
        wc.lpszClassName = PCWSTR(cls.as_ptr());
        class_icon_cursor(&mut wc);
        let _ = RegisterClassW(&wc);

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(cls.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            100,
            100,
            None,
            None,
            hinst,
            None,
        ) {
            Ok(h) => h,
            Err(_) => return,
        };
        if hwnd.0.is_null() {
            return;
        }
        // 窗口创建后保持隐藏(样式无 WS_VISIBLE, 也绝不 ShowWindow):
        // RegisterHotKey 的 WM_HOTKEY 只要求窗口存在, 与可见性无关(文档明确)。
        // 旧实现 ShowWindow+移到 -32000 —— RDP/多屏会话下该坐标会被系统
        // 拉回可见区, 导致出现一个 "sog" 空白窗口并干扰点击, 已废弃。
        TRAY = Some(hwnd);
        add_tray(hwnd);
        match RegisterHotKey(hwnd, 1, MOD_CONTROL | MOD_ALT, 0x53) {
            Ok(()) => log_line("hotkey ok"),
            Err(e) => log_line(&format!("hotkey failed: {:?}", e)),
        }

        cfg_load();
        // 按配置重新注册(默认 Ctrl+Alt+S 与上面一致; 用户改过则生效)
        hotkey_apply(hwnd, CFG.hotkey_mod, CFG.hotkey_vk);
        recover_if_needed();
        log_line("start");
        // 双击启动即弹出设置面板(用户要求); 静默启动则跳过
        if !CFG.silent_start {
            let _ = settings::show(hinst, hwnd, &mut CFG);
            log_line("settings opened on start");
        }

        // 定时检查: 托盘窗口 1s 定时器(而不是在主循环里 sleep 轮询 ——
        // sleep 会把 WM_TIMER 压低到同一节流频率, 设置面板的 16ms 输入/点击轮询
        // 被拖成 500ms 一次, 实测导致"开关点不动、热键录不进").
        let _ = SetTimer(hwnd, 2, 1000, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
