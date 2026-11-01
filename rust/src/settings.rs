#![allow(static_mut_refs)]
//! 设置面板 — WinUI 3 / Windows 11 Settings 风格, 全自绘单窗口实现
//!
//! 与旧版差异(根因级重写, 非调参):
//!  1) 零子控件: 旧版 STATIC/BUTTON 子窗口是"灰底块/系统字形/模糊"的根源
//!  2) GDI+ 抗锯齿几何: 圆角卡片/开关/单选/滑块全部走 Gdip*, 消除锯齿
//!  3) 双缓冲: 内存 DC 一次合成后 BitBlt, 无闪烁
//!  4) Per-Monitor V2 DPI 感知: enable_dpi() 在 main() 最先调用,
//!     高分屏不再被系统位图拉伸发糊(旧版"像素很低"的直接根因)
//!  5) 真 SettingsCard 行布局: 标题+描述在左, 控件右对齐, 整行 hover
//!  6) 无保存/取消按钮: 改动即时生效并自动保存

use std::ffi::OsStr;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    BOOL, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC, GetStockObject,
    GetTextExtentPoint32W, GetDeviceCaps, InvalidateRect, LineTo, MoveToEx, ReleaseDC,
    ScreenToClient, SelectObject, SetBkColor, SetBkMode, SetTextColor, HBRUSH, HDC, HFONT, PAINTSTRUCT,
    PS_SOLID, SRCCOPY, TRANSPARENT, DT_CENTER, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
    HOLLOW_BRUSH, LOGPIXELSX, RoundRect,
};
use windows::Win32::Graphics::GdiPlus::{
    FillModeAlternate, SmoothingModeAntiAlias, Unit, GdipAddPathArc, GdipClosePathFigure,
    GdipCreateFromHDC, GdipCreatePen1, GdipCreatePath, GdipCreateSolidFill, GdipDeleteGraphics,
    GdipDeletePath, GdipDeletePen, GdipDeleteBrush, GdipDrawEllipse, GdipDrawPath, GdipFillEllipse,
    GdipFillPath, GdipSetSmoothingMode, GdiplusStartup, GdiplusStartupInput, GpBrush, GpGraphics,
    GpPath, GpSolidFill,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const IDC_CAP: u16 = 2001;
pub const IDC_CUR_AUTO: u16 = 2011;
pub const IDC_CUR_HIDE: u16 = 2012;
pub const IDC_CUR_KEEP: u16 = 2013;
pub const IDC_AUTOSTART: u16 = 2021;
pub const IDC_DIM_EXT: u16 = 2022;
pub const IDC_DIM_INT: u16 = 2023;
pub const IDC_CLOSE_HIDE: u16 = 2041;
pub const IDC_CLOSE_EXIT: u16 = 2042;
pub const IDC_HDR: u16 = 2051;
pub const IDC_ORDER_BLACK: u16 = 2071;
pub const IDC_ORDER_DIM: u16 = 2072;
pub const IDC_ORDER_FADE: u16 = 2073;
pub const IDC_REST_READY: u16 = 2074;
pub const IDC_REST_DELAY: u16 = 2075;
pub const IDC_MEASURE: u16 = 2078;
pub const IDC_SILENT: u16 = 2081;
pub const IDC_DELAY_EDIT: u16 = 2091;
pub const IDC_HOTKEY: u16 = 2092;
// 定时策略
pub const IDC_SCHED: u16 = 2101;
pub const IDC_TIME_ON: u16 = 2102;
pub const IDC_TIME_OFF: u16 = 2103;

// ---- WinUI 3 / Win11 设置 (浅色) 色值, COLORREF = 0x00BBGGRR ----
const WIN_BG: COLORREF = COLORREF(0x00F3F3F3); // Mica 近似
const CARD: COLORREF = COLORREF(0x00FBFBFB); // 卡片填充(接近白)
const CARD_STROKE: COLORREF = COLORREF(0x00E5E5E5); // CardStrokeDefault
const SEP: COLORREF = COLORREF(0x00EBEBEB); // 行分隔
const LABEL: COLORREF = COLORREF(0x001B1B1B); // TextPrimary
const SEC: COLORREF = COLORREF(0x005D5D5D); // TextSecondary
const ROW_HOVER: COLORREF = COLORREF(0x00F1F1F1); // SubtleFillSecondary 近似
const TRACK_OFF: COLORREF = COLORREF(0x00D9D9D9); // Slider 未填充
const RADIO_OFF: COLORREF = COLORREF(0x00767676); // 单选未选环
const TOGGLE_OFF_STROKE: COLORREF = COLORREF(0x008A8A8A); // 开关关闭描边
const TOGGLE_OFF_KNOB: COLORREF = COLORREF(0x005D5D5D); // 开关关闭拇指
const BTN_FILL: COLORREF = COLORREF(0x00FCFCFC);
const BTN_STROKE: COLORREF = COLORREF(0x00D6D6D6);
const BTN_FILL_HOVER: COLORREF = COLORREF(0x00F4F4F4);
const BTN_STROKE_HOVER: COLORREF = COLORREF(0x00C8C8C8);
const THUMB_STROKE: COLORREF = COLORREF(0x00C9C9C9);
const WHITE: COLORREF = COLORREF(0x00FFFFFF);

static mut ACCENT: COLORREF = COLORREF(0x00C06700); // #0067C0, show() 时改为系统 accent
static mut S: f64 = 1.0; // DPI 缩放
static mut UI_SCALE: f64 = 1.0; // 窗口自适应等比缩放(build_layout 计算, 字体/几何统一跟随)

// ---- 布局(客户端坐标, 已含缩放) ----
static mut WND: HWND = HWND(core::ptr::null_mut());
static mut CARDS: Vec<RECT> = Vec::new();
static mut HEADERS: Vec<(String, RECT)> = Vec::new(); // (组标题, 矩形)
static mut TITLE_R: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
static mut SUB_R: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
static mut NOTE_R: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
static mut SLIDER_R: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
static mut EDIT_R: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
static mut H_EDIT: HWND = HWND(core::ptr::null_mut());
static mut EDITING: bool = false; // EDIT 回写保护: SetWindowText 会触发 EN_CHANGE
static mut DRAG: bool = false;
static mut HOT_ROW: i32 = -1;
static mut HOT_BTN: bool = false;
static mut TRACKING: bool = false;
// 自实现窗口拖动状态(绕开 DefWindowProcW 的 NC 模态拖动循环 —— 本环境该循环异常, 疑似"卡死"来源)
static mut CAPT_DRAG: bool = false;
static mut CAPT_MOVED: bool = false;
static mut CAPT_START: POINT = POINT { x: 0, y: 0 };
static mut CAPT_WIN: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
// 边框缩放(自实现拖边, 绕开系统模态循环 —— 与窗口拖动同款安全阀机制)
static mut RESIZE: bool = false;
static mut R_EDGE: u32 = 0;
static mut R_START: POINT = POINT { x: 0, y: 0 };
static mut R_WIN: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
// 客户区尺寸轮询兜底: 本环境实测 WM_SIZE 在 SetWindowPos/MoveWindow 后不投递,
// 布局重算靠 16ms 定时器比对客户区尺寸触发
static mut LAST_CW: i32 = 0;
static mut LAST_CH: i32 = 0;
// 当前布局是否处于"压缩留白"模式(push_group 等共享)
static mut LAY_COMPRESS: bool = false;
// 边框按住状态(轮询按下沿检测用, 与消息路径冗余)
static mut BORDER_DOWN_PREV: bool = false;
// 自绘标题栏: [关于][—][✕] 按钮矩形 + 检测按钮矩形(滑块行左侧)
static mut ABOUT_R: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
static mut MIN_R: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
static mut CLOSE_R: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
static mut MBTN_R: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
// 标题栏按钮悬停: 0无 1关于 2最小化 3关闭(轮询更新, 变化才重绘)
static mut HOT_TB: u8 = 0;
// 全局热键录入框: 矩形 + 捕获态(点击后进入, 按下组合键完成)
static mut HK_R: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
static mut HK_CAPTURE: bool = false;
static mut HK_HOT: bool = false; // 鼠标悬停高亮
// 内部重建窗口标志: WM_DESTROY 据此区分"用户关闭(退出进程)"与"卡片重建(继续运行)"
static mut REBUILD: bool = false;
// 定时策略: 两个时间输入框(熄屏/亮屏)的矩形与 EDIT 句柄
static mut TIME_ON_R: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
static mut TIME_OFF_R: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
static mut H_EDIT_ON: HWND = HWND(core::ptr::null_mut());
static mut H_EDIT_OFF: HWND = HWND(core::ptr::null_mut());
static mut EDITING_T: bool = false; // 时间输入框正在被用户编辑(防回写打架)
/// 是否处于"录入热键"捕获态。main 的 WM_HOTKEY 据此忽略触发(双保险,
/// 即使 UnregisterHotKey 没生效也不会误熄屏)。
pub static CAPTURING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 切换热键捕获态。**必须走这里**: HK_CAPTURE 与 CAPTURING 两个状态要同步,
/// 只改其中一个会出现"捕获结束后新热键也不响应"(CAPTURING 残留为 true,
/// WM_HOTKEY 一直被误判成捕获中而忽略) 或反之。
unsafe fn set_capture(on: bool) {
    HK_CAPTURE = on;
    CAPTURING.store(on, std::sync::atomic::Ordering::Relaxed);
    crate::log_line(&format!("settings: hotkey capture={}", on));
}

fn bar_h() -> i32 {
    unsafe { sc(40) }
}
// 链路探针: show 完成后记录窗口收到的每条消息(限 200 条), 供实机诊断输入链路
static PROBE_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static PROBE_N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
// 点击节流(消息路径与轮询路径去重)
static LAST_CLICK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
// 轮询路径: 上一采样时刻左键状态
static LB_PREV: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// 行类型: 0=值 1=开关 2=单选组 3=按钮 4=滑块
struct Row {
    id: u16,
    kind: u8,
    title: &'static str,
    desc: &'static str,
    rect: RECT,              // 整行(卡内)
    rads: Vec<(u16, RECT)>,  // 单选项(圆+文字热区, 绝对坐标)
    btn: RECT,               // 按钮
}
static mut ROWS: Vec<Row> = Vec::new();

static mut F_TITLE: HFONT = HFONT(core::ptr::null_mut());
static mut F_SUB: HFONT = HFONT(core::ptr::null_mut());
static mut F_HEAD: HFONT = HFONT(core::ptr::null_mut());
static mut F_BODY: HFONT = HFONT(core::ptr::null_mut());
static mut F_SEC: HFONT = HFONT(core::ptr::null_mut());
static mut F_MONO: HFONT = HFONT(core::ptr::null_mut());
static mut F_ICON: HFONT = HFONT(core::ptr::null_mut());
// 输入框专用字体: 比正文大一号, 配合 sc(32) 框高视觉居中
static mut F_EDIT: HFONT = HFONT(core::ptr::null_mut());

static mut MEASURE: String = String::new();

// A/B 开关: false = 生产版(全窗口 HTCAPTION NC 通道 + 抬起死区还原点击)。
// PLAIN 复刻实验证明: 朴素 wndproc 在本环境建窗 10 连败, NC 分支是必需品。
const PLAIN_MODE: bool = false;

static mut GP_TOKEN: usize = 0;

/// Per-Monitor V2 DPI 感知 — 必须在 main() 创建任何窗口/HDC 之前调用。
/// 旧版未声明感知 => 系统按 96dpi 渲染再位图拉伸, 高分屏整体发糊(像素低的根因)。
pub unsafe fn enable_dpi() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    // Windows 10 1703+ 必有 V2; 失败则退回 BOOL 版 API(WindowsAndMessaging)
    if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err() {
        let _ = SetProcessDPIAware();
    }
}

/// 按所需尺寸缓存应用图标(布局缩放变化时按新尺寸重载)
unsafe fn app_icon_cached(px: i32) -> HICON {
    use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
    static SIZE: AtomicI32 = AtomicI32::new(0);
    static HANDLE: AtomicUsize = AtomicUsize::new(0);
    let want = px.max(1);
    if SIZE.load(Ordering::Relaxed) == want {
        let h = HANDLE.load(Ordering::Relaxed);
        if h != 0 {
            return HICON(h as *mut core::ffi::c_void);
        }
    }
    let hi = crate::app_icon_sized(want);
    SIZE.store(want, Ordering::Relaxed);
    HANDLE.store(hi.0 as usize, Ordering::Relaxed);
    hi
}

/// 检测完成回调(main.rs 调用): 显示结果并重绘
pub unsafe fn set_measure_text(t: &str) {
    MEASURE = t.to_string();
    if !WND.0.is_null() {
        let _ = InvalidateRect(WND, None, false);
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(once(0)).collect()
}

fn sc(v: i32) -> i32 {
    unsafe { (v as f64 * S * UI_SCALE) as i32 }
}

fn font(px: i32, weight: i32, mono: bool) -> HFONT {
    unsafe {
        // "Segoe UI Variable Text" 是 Win11 专有字体, Win10 上 CreateFontW 会静默
        // 回落成旧系统字体(用户实测"字体很古老"的根因)。Segoe UI 在 Win7+ 全系可用。
        let n = if mono {
            wide("Cascadia Mono")
        } else {
            wide("Segoe UI")
        };
        CreateFontW(-sc(px), 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, PCWSTR(n.as_ptr()))
    }
}

fn icon_font() -> HFONT {
    unsafe {
        let n = wide("Segoe Fluent Icons");
        CreateFontW(-sc(16), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, PCWSTR(n.as_ptr()))
    }
}

fn del_font(f: &mut HFONT) {
    unsafe {
        if !f.0.is_null() {
            let _ = DeleteObject(*f);
            *f = HFONT(core::ptr::null_mut());
        }
    }
}

// ================= GDI+ 封装 =================

unsafe fn gp_init() -> bool {
    if GP_TOKEN != 0 {
        return true;
    }
    let mut input = GdiplusStartupInput::default();
    input.GdiplusVersion = 1;
    let mut token: usize = 0;
    if GdiplusStartup(&mut token, &input, std::ptr::null_mut()).0 != 0 {
        return false;
    }
    GP_TOKEN = token;
    true
}

fn argb(c: COLORREF, a: u8) -> u32 {
    ((a as u32) << 24) | (((c.0 & 0xFF) as u32) << 16) | (c.0 & 0xFF00) | ((c.0 >> 16) & 0xFF)
}

/// 圆角矩形路径(4 段弧)
unsafe fn gp_rrect_path(r: (f32, f32, f32, f32), rad: f32) -> *mut GpPath {
    let mut p: *mut GpPath = std::ptr::null_mut();
    GdipCreatePath(FillModeAlternate, &mut p);
    let (x, y, w, h) = r;
    let d = rad * 2.0;
    GdipAddPathArc(p, x, y, d, d, 180.0, 90.0);
    GdipAddPathArc(p, x + w - d, y, d, d, 270.0, 90.0);
    GdipAddPathArc(p, x + w - d, y + h - d, d, d, 0.0, 90.0);
    GdipAddPathArc(p, x, y + h - d, d, d, 90.0, 90.0);
    GdipClosePathFigure(p);
    p
}

unsafe fn gp_fill(g: *mut GpGraphics, r: (f32, f32, f32, f32), rad: f32, c: COLORREF) {
    let path = gp_rrect_path(r, rad);
    let mut br: *mut GpSolidFill = std::ptr::null_mut();
    GdipCreateSolidFill(argb(c, 0xFF), &mut br);
    GdipFillPath(g, br as *mut GpBrush, path);
    GdipDeleteBrush(br as *mut GpBrush);
    GdipDeletePath(path);
}

unsafe fn gp_stroke(g: *mut GpGraphics, r: (f32, f32, f32, f32), rad: f32, c: COLORREF, w: f32) {
    let path = gp_rrect_path(r, rad);
    let mut pen = std::ptr::null_mut();
    GdipCreatePen1(argb(c, 0xFF), w, Unit(2), &mut pen); // 2 = UnitPixel
    GdipDrawPath(g, pen, path);
    GdipDeletePen(pen);
    GdipDeletePath(path);
}

unsafe fn gp_ellipse_fill(g: *mut GpGraphics, x: f32, y: f32, w: f32, h: f32, c: COLORREF) {
    let mut br: *mut GpSolidFill = std::ptr::null_mut();
    GdipCreateSolidFill(argb(c, 0xFF), &mut br);
    GdipFillEllipse(g, br as *mut GpBrush, x, y, w, h);
    GdipDeleteBrush(br as *mut GpBrush);
}

unsafe fn gp_ellipse_stroke(g: *mut GpGraphics, x: f32, y: f32, w: f32, h: f32, c: COLORREF, pw: f32) {
    let mut pen = std::ptr::null_mut();
    GdipCreatePen1(argb(c, 0xFF), pw, Unit(2), &mut pen);
    GdipDrawEllipse(g, pen, x, y, w, h);
    GdipDeletePen(pen);
}

// ================= GDI 文本/杂项 =================

unsafe fn fill(hdc: HDC, r: &RECT, c: COLORREF) {
    let b = CreateSolidBrush(c);
    FillRect(hdc, r, b);
    let _ = DeleteObject(b);
}

unsafe fn dt(hdc: HDC, r: &RECT, s: &str, c: COLORREF, f: HFONT, align: i32) {
    // align: 0 左 1 右 2 居中
    if s.is_empty() {
        return;
    }
    let old = SelectObject(hdc, f);
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, c);
    let mut b = wide(s);
    // 必须去掉 null 终止符: DrawTextW 带显式长度时会把 null 当字符渲染,
    // Cascadia Mono 的 null 字形是可见虚线框("Ctrl + Alt + S" 后多出不明符号的根因)
    let al = if align == 1 { DT_RIGHT } else if align == 2 { DT_CENTER } else { DT_LEFT };
    let fl = DT_SINGLELINE | DT_VCENTER | al;
    let mut rc = *r;
    let n = b.len() - 1; // 去掉 null 终止符(见上)
    let _ = DrawTextW(hdc, &mut b[..n], &mut rc, fl);
    let _ = SelectObject(hdc, old);
}

unsafe fn text_w(s: &str, f: HFONT) -> i32 {
    let hdc = GetDC(None);
    let old = SelectObject(hdc, f);
    let b = wide(s);
    let mut sz = windows::Win32::Foundation::SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &b[..b.len() - 1], &mut sz);
    let _ = SelectObject(hdc, old);
    ReleaseDC(None, hdc);
    sz.cx
}

// ================= 配置语义映射 =================

fn radio_of(id: u16, c: &crate::Cfg) -> Option<bool> {
    Some(match id {
        IDC_CUR_AUTO => c.cursor_mode == 0,
        IDC_CUR_HIDE => c.cursor_mode == 1,
        IDC_CUR_KEEP => c.cursor_mode == 2,
        IDC_ORDER_BLACK => c.dim_order == 0,
        IDC_ORDER_DIM => c.dim_order == 1,
        IDC_ORDER_FADE => c.dim_order == 2,
        IDC_REST_READY => c.restore_mode == 0,
        IDC_REST_DELAY => c.restore_mode == 1,
        IDC_CLOSE_HIDE => c.close_action == 0,
        IDC_CLOSE_EXIT => c.close_action == 1,
        _ => return None,
    })
}

fn toggle_of(id: u16, c: &crate::Cfg) -> Option<bool> {
    Some(match id {
        IDC_DIM_EXT => c.dim_external,
        IDC_DIM_INT => c.dim_internal,
        IDC_HDR => c.hdr_off,
        IDC_AUTOSTART => c.autostart,
        IDC_SILENT => c.silent_start,
        IDC_SCHED => c.sched_enabled,
        _ => return None,
    })
}

fn radio_label(id: u16) -> &'static str {
    match id {
        IDC_CUR_AUTO => "远控保留，其余隐藏",
        IDC_CUR_HIDE => "始终隐藏",
        IDC_CUR_KEEP => "始终保留",
        IDC_ORDER_BLACK => "先黑后熄",
        IDC_ORDER_DIM => "先熄后黑",
        IDC_ORDER_FADE => "淡入并行",
        IDC_REST_READY => "亮度就绪即撤窗",
        IDC_REST_DELAY => "固定延迟",
        IDC_CLOSE_HIDE => "隐藏到托盘",
        IDC_CLOSE_EXIT => "退出程序",
        _ => "",
    }
}

unsafe fn apply_toggle(id: u16) {
    let mut c = crate::cfg_clone();
    match id {
        IDC_DIM_EXT => c.dim_external = !c.dim_external,
        IDC_DIM_INT => c.dim_internal = !c.dim_internal,
        IDC_HDR => c.hdr_off = !c.hdr_off,
        IDC_AUTOSTART => c.autostart = !c.autostart,
        IDC_SILENT => c.silent_start = !c.silent_start,
        IDC_SCHED => c.sched_enabled = !c.sched_enabled,
        _ => return,
    }
    crate::cfg_apply(c);
}

unsafe fn apply_radio(id: u16) {
    let mut c = crate::cfg_clone();
    match id {
        IDC_CUR_AUTO => c.cursor_mode = 0,
        IDC_CUR_HIDE => c.cursor_mode = 1,
        IDC_CUR_KEEP => c.cursor_mode = 2,
        IDC_ORDER_BLACK => c.dim_order = 0,
        IDC_ORDER_DIM => c.dim_order = 1,
        IDC_ORDER_FADE => c.dim_order = 2,
        IDC_REST_READY => c.restore_mode = 0,
        IDC_REST_DELAY => c.restore_mode = 1,
        IDC_CLOSE_HIDE => c.close_action = 0,
        IDC_CLOSE_EXIT => c.close_action = 1,
        _ => return,
    }
    crate::cfg_apply(c);
}

// ================= 控件绘制 (GDI+ 抗锯齿) =================

/// ToggleSwitch: 轨道 40x20 r10, 拇指 12
unsafe fn draw_toggle(g: *mut GpGraphics, cx: i32, cy: i32, on: bool) {
    let w = sc(40) as f32;
    let h = sc(20) as f32;
    let x = cx as f32 - w / 2.0;
    let y = cy as f32 - h / 2.0;
    let rad = h / 2.0;
    if on {
        gp_fill(g, (x, y, w, h), rad, ACCENT);
    } else {
        gp_fill(g, (x, y, w, h), rad, CARD);
        gp_stroke(g, (x, y, w, h), rad, TOGGLE_OFF_STROKE, 1.0);
    }
    let kd = sc(12) as f32;
    let kx = if on { x + w - kd - sc(4) as f32 } else { x + sc(4) as f32 };
    let ky = y + (h - kd) / 2.0;
    gp_ellipse_fill(g, kx, ky, kd, kd, if on { WHITE } else { TOGGLE_OFF_KNOB });
}

/// RadioButton: 外环 20, 选中 accent 环 + 实心点
unsafe fn draw_radio(g: *mut GpGraphics, cx: i32, cy: i32, on: bool) {
    let d = sc(20) as f32;
    let x = cx as f32 - d / 2.0;
    let y = cy as f32 - d / 2.0;
    if on {
        gp_ellipse_stroke(g, x, y, d, d, ACCENT, 1.0);
        let id = sc(10) as f32;
        gp_ellipse_fill(g, x + (d - id) / 2.0, y + (d - id) / 2.0, id, id, ACCENT);
    } else {
        gp_ellipse_stroke(g, x, y, d, d, RADIO_OFF, 1.0);
    }
}

/// 标准按钮: 填充 + 1px 描边 r4
unsafe fn draw_button(g: *mut GpGraphics, r: &RECT, hover: bool) {
    let (x, y, w, h) = (r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32);
    let rad = sc(4) as f32;
    gp_fill(g, (x, y, w, h), rad, if hover { BTN_FILL_HOVER } else { BTN_FILL });
    gp_stroke(g, (x, y, w, h), rad, if hover { BTN_STROKE_HOVER } else { BTN_STROKE }, 1.0);
}

/// Slider: 轨道 4px, 填充 accent, 拇指 20x20 r4 白底描边(Win11 规格)
unsafe fn draw_slider(g: *mut GpGraphics, r: &RECT, v: u32) {
    let cy = (r.top + r.bottom) as f32 / 2.0;
    let x0 = r.left as f32 + sc(10) as f32;
    let x1 = r.right as f32 - sc(10) as f32;
    let f = (v as f64 / 500.0).clamp(0.0, 1.0) as f32;
    let xk = x0 + (x1 - x0) * f;
    let th = sc(4) as f32;
    gp_fill(g, (x0, cy - th / 2.0, x1 - x0, th), th / 2.0, TRACK_OFF);
    if xk > x0 + th {
        gp_fill(g, (x0, cy - th / 2.0, xk - x0, th), th / 2.0, ACCENT);
    }
    let tw = sc(20) as f32;
    gp_fill(g, (xk - tw / 2.0, cy - tw / 2.0, tw, tw), sc(4) as f32, WHITE);
    gp_stroke(g, (xk - tw / 2.0, cy - tw / 2.0, tw, tw), sc(4) as f32, THUMB_STROKE, 1.0);
}

// ================= 主流程 =================

pub unsafe fn show(hinst: HINSTANCE, _p: HWND, _cfg: &mut crate::Cfg) -> bool {
    // 已有面板实例: 直接复用(重新布局+显示)。绝不能再建窗口 —— 两个窗口共享
    // 全局布局表(ROWS/CARDS/LAST_CW...), 两个 16ms 定时器交替重排互相踩,
    // 实测表现为"托盘打开后窄卡片+鬼影文字"(用户截图实锤)。
    if !WND.0.is_null() {
        // 最小化中的窗口 SW_SHOW 无效(仍在任务栏收着), 必须 SW_RESTORE 才能还原。
        // 但本环境 SW_RESTORE 对设置窗口恒定失败(记事本同方式正常, 只有本进程不行),
        // 一旦窗口进入 iconic 就永久卡死 —— 用户现象"最小化后打不开"。
        // 急救: restore 无效时直接销毁窗口重建(REBUILD 标志让 WM_DESTROY 不退出进程)。
        if IsIconic(WND).as_bool() {
            let _ = ShowWindow(WND, SW_RESTORE);
            if IsIconic(WND).as_bool() {
                crate::log_line("settings: iconic stuck -> rebuild window");
                REBUILD = true;
                let _ = DestroyWindow(WND);
                REBUILD = false;
                // 落到下方正常建窗流程
            } else {
                relayout(WND);
                crate::log_line("settings: reused existing window (restored)");
                return true;
            }
        } else {
            let _ = ShowWindow(WND, SW_SHOW);
            relayout(WND);
            crate::log_line("settings: reused existing window");
            return true;
        }
    }
    crate::log_line("settings: show begin");
    if !gp_init() {
        crate::log_line("settings: gdiplus init FAILED");
        return false;
    }

    let cls = wide("SOGSettings");
    let title = wide("一键熄屏 · 设置");

    let mut wc = WNDCLASSW::default();
    wc.lpfnWndProc = Some(dlg_proc);
    wc.hInstance = hinst;
    wc.hbrBackground = HBRUSH(CreateSolidBrush(COLORREF(0x00F0F0F0)).0); // 与 Python 版一致的实心刷
    wc.lpszClassName = PCWSTR(cls.as_ptr());
    crate::class_icon_cursor(&mut wc); // 任务栏/标题栏图标; hCursor 留 NULL(光标自管)
    let _ = RegisterClassW(&wc);

    // 注意: read_accent() 涉及 DwmGetColorizationColor/注册表调用,
    // 实测必须放在建窗【之后】—— 建窗前调用 DWM 会让后续 CreateWindowExW
    // 稳定返回 null(错误码 183/0), 旧版正是建窗后才读 accent 所以从未触发。
    ACCENT = COLORREF(0x00C06700); // 先给默认值, 建窗后修正

    // ---------- 窗口: 先以小尺寸隐藏创建(最稳路径), 再实测 DPI -> 布局 -> 定尺寸 -> 显示 ----------
    // 说明: 本环境直接按最终大尺寸 CreateWindowExW 偶发返回 null(错误码 183/0 漂移),
    // 小尺寸创建(与托盘窗口同路径)从未失败; 且大尺寸还依赖估算 DPI, 多屏/VM 下会算错导致截断。
    // 窗口 style: PLAIN 模式完整复刻 Python 版(WS_POPUP+WS_CAPTION 组合)
    let style = if PLAIN_MODE {
        WINDOW_STYLE(WS_POPUP.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0 | WS_VISIBLE.0)
    } else {
        // 真无边框窗口样式:
        // 【不能带 WS_THICKFRAME】—— 系统会给带 THICKFRAME 的窗口强制补上
        // WS_CAPTION(log 实测: 源码 style=0x120F0000 无 caption, 运行时变
        // 0x16CF0000 带 caption), 于是任何激活态变化(点关于弹模态/拖动/切焦点)
        // 系统都会重画原生标题栏 —— 用户多次实拍的"无边框其实有边框"。
        // 边缘缩放本来就由 NCHITTEST(10..17) + begin_resize 自实现, 不需要 THICKFRAME。
        // WS_EX_APPWINDOW(见下方 CreateWindowExW)保证任务栏有按钮。
        WINDOW_STYLE(
            WS_POPUP.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0 | WS_CLIPCHILDREN.0 | WS_VISIBLE.0,
        )
    };
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    crate::log_line(&format!(
        "settings: style=0x{:08X} (caption={})",
        style.0,
        (style.0 & WS_CAPTION.0) != 0
    ));

    let mk = || {
        unsafe {
            // WS_EX_APPWINDOW: WS_POPUP 顶层窗口默认不进任务栏, 加它才有关闭/切换入口
            CreateWindowExW(
                WINDOW_EX_STYLE(WS_EX_APPWINDOW.0 as u32),
                PCWSTR(cls.as_ptr()), PCWSTR(title.as_ptr()),
                style,
                (sw - 400) / 2, (sh - 300) / 2, 400, 300,
                HWND(core::ptr::null_mut()), HMENU(core::ptr::null_mut()), hinst, None,
            )
        }
    };
    // 本环境建窗偶发被取消(null + 183/0 漂移, 换类名/样式/尺寸/DPI 均无法根除,
    // 且同代码多次运行成功率不定), 因此做多次重试
    let mut h = None;
    for attempt in 0..10 {
        match mk() {
            Ok(x) => {
                h = Some(x);
                break;
            }
            Err(e) => {
                crate::log_line(&format!("settings: create attempt {} failed: {:?}", attempt, e));
                windows::Win32::System::Threading::Sleep(300);
            }
        }
    }
    let h = match h {
        Some(h) => h,
        None => {
            crate::log_line("settings: create failed after 10 attempts");
            return false;
        }
    };
    WND = h;

    // 实测窗口 DPI 并布局(建窗后再读系统 accent, 见上方注释)
    ACCENT = read_accent();
    S = windows::Win32::UI::HiDpi::GetDpiForWindow(h) as f64 / 96.0;
    // 初始定位一律按设计尺寸(scale=1.0)布局: 小窗建窗期间的收缩比例绝不外泄到
    // win_size —— 否则窗口会按缩小后的内容尺寸开出一个小窗(实测 204x297)。
    // 最终尺寸下的自适应重排由 SetWindowPos 触发的 WM_SIZE -> build_layout 完成。
    let client_h = layout_at(h, 1.0, false);
    let (ww, mut wh) = win_size(client_h, style);
    // 高度 clamp 到屏幕内(留 24px): 125% DPI 下整窗可能超出 1080p, 超出会导致
    // 底部控件不可见且窗口无法整体显示
    let max_wh = sh - 24;
    if wh > max_wh {
        wh = max_wh.max(200);
        crate::log_line(&format!(
            "settings: window clamped to {} (screen h={})",
            wh, sh
        ));
    }
    crate::log_line(&format!(
        "settings: dpi_scale={:.2} client_h={} win={}x{} screen={}x{}",
        S, client_h, ww, wh, sw, sh
    ));
    // 默认 Z 序(用户要求不置顶): 点击已走双通道修复, 无需置顶对抗遮挡
    let _ = SetWindowPos(
        h, None, (sw - ww) / 2, ((sh - wh) / 2).max(0), ww, wh,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );

    // 撤窗延迟输入框(真 EDIT 子控件, 与滑块双向同步; 布局坐标已在 build_layout 算好)
    let er = EDIT_R;
    let init = wide(&format!("{}", crate::cfg_clone().delay_ms));
    if let Ok(he) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(wide("EDIT").as_ptr()),
        PCWSTR(init.as_ptr()),
        // ES_CENTER(0x0001) 水平居中 + ES_NUMBER; 数值型输入用户要求文字居中
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | 0x0001 /*ES_CENTER*/ | 0x2000 /*ES_NUMBER*/),
        er.left, er.top, er.right - er.left, er.bottom - er.top,
        h, HMENU(IDC_DELAY_EDIT as *mut core::ffi::c_void), hinst, None,
    ) {
        H_EDIT = he;
        let _ = SendMessageW(he, 0x0030 /*WM_SETFONT*/, WPARAM(F_EDIT.0 as usize), LPARAM(1));
        // 行距: 让文字在 sc(32) 框内视觉居中(EDIT 单行默认贴顶 +1px)
        let _ = SendMessageW(he, 0x00D3 /*EM_SETMARGINS*/, WPARAM(0), LPARAM(0));
    }

    // 定时策略的两个时间输入框(HH:MM, 不能用 ES_NUMBER —— 要输冒号)
    {
        let c = crate::cfg_clone();
        let items: [(u16, RECT, String); 2] = [
            (
                IDC_TIME_ON,
                TIME_ON_R,
                format!("{:02}:{:02}", c.sched_on_h, c.sched_on_m),
            ),
            (
                IDC_TIME_OFF,
                TIME_OFF_R,
                format!("{:02}:{:02}", c.sched_off_h, c.sched_off_m),
            ),
        ];
        for (id, r, txt) in items {
            let init = wide(&txt);
            if let Ok(he) = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(wide("EDIT").as_ptr()),
                PCWSTR(init.as_ptr()),
                WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | 0x0001 /*ES_CENTER*/),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                h,
                HMENU(id as *mut core::ffi::c_void),
                hinst,
                None,
            ) {
                let _ = SendMessageW(
                    he,
                    0x0030, /*WM_SETFONT*/
                    WPARAM(F_EDIT.0 as usize),
                    LPARAM(1),
                );
                if id == IDC_TIME_ON {
                    H_EDIT_ON = he;
                } else {
                    H_EDIT_OFF = he;
                }
            }
        }
    }

    // 位置护栏: 拖动/多屏切换可能把窗口甩到可见区域外(实测被拖到 x=3112,
    // 屏幕宽 2560), 之后用户点哪儿都落空, 表现为"所有控件都不正常"。
    {
        let mut wr = RECT::default();
        let _ = GetWindowRect(h, &mut wr);
        let sw2 = GetSystemMetrics(SM_CXSCREEN);
        let sh2 = GetSystemMetrics(SM_CYSCREEN);
        let ww = wr.right - wr.left;
        let whh = wr.bottom - wr.top;
        let nx = wr.left.min(sw2 - ww).max(0);
        let ny = wr.top.min(sh2 - whh).max(0);
        if nx != wr.left || ny != wr.top {
            crate::log_line(&format!(
                "settings: window off-screen ({},{}), clamped to ({},{})",
                wr.left, wr.top, nx, ny
            ));
            let _ = SetWindowPos(
                h, None, nx, ny, 0, 0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
    let _ = ShowWindow(h, SW_SHOW);
    // 强制系统按 WM_NCCALCSIZE 重算非客户区: 窗口若曾进入最小化/被系统接管的
    // NC 状态, 会残留原生标题栏(用户实拍"无边框其实有边框"), 一次 FRAMECHANGED
    // 让它回到"客户区=窗口"的自绘无边框状态。
    let _ = SetWindowPos(
        h, None, 0, 0, 0, 0,
        SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
    // 不调 SetForegroundWindow: 非前台进程抢焦点必失败, 系统随即进入
    // 任务栏闪烁循环(FlashWindow) → GETICON/PAINT 风暴 → 整页高频重绘闪烁。
    let _ = InvalidateRect(h, None, false);
    // 轮询点击检测定时器(16ms): 常开的兜底通道
    let tset = SetTimer(h, 1, 16, None);
    crate::log_line(&format!("settings: settimer ok={}", tset != 0));    // 打开链路探针(仅此后 200 条消息)
    PROBE_ON.store(true, std::sync::atomic::Ordering::Relaxed);
    true
}

/// 布局: 依赖 S(DPI 缩放), 填充 CARDS/HEADERS/ROWS/SLIDER_R 等静态布局表, 返回客户区高度
/// 布局: 双轮等比缩放 —— 先按客户区宽求 UI_SCALE, 重排后内容超高再按高度收缩。
/// 整体缩放(含字体)保证任意窗口尺寸下布局永不破碎。
unsafe fn build_layout(h: HWND) -> i32 {
    let mut crc = RECT::default();
    let _ = GetClientRect(h, &mut crc);
    let cw = (crc.right - crc.left).max(100);
    let chh = crc.bottom - crc.top;

    // 自适应策略(空间不够先去空白, 最后才整体缩小):
    //   1) 全尺寸字体 + 常规留白
    //   2) 全尺寸字体 + 压缩留白(边距/间隙/标题区/组标题收窄)
    //   3) 压缩留白 + 等比缩小(字体同步缩小, 兜底)
    let mut ch = layout_at(h, 1.0, false);
    if ch > chh {
        ch = layout_at(h, 1.0, true);
    }
    if ch > chh {
        let mut s = if ch > 0 { chh as f64 / ch as f64 } else { 1.0 };
        ch = layout_at(h, s, true);
        if ch > chh && ch > 0 {
            s *= chh as f64 / ch as f64;
            ch = layout_at(h, s, true);
        }
    }
    crate::log_line(&format!(
        "settings: layout cw={} chh={} ch={} compress={} card1_top={}",
        cw, chh, ch, LAY_COMPRESS, CARDS.first().map(|c| c.top).unwrap_or(-1)
    ));
    ch
}

unsafe fn layout_at(h: HWND, scale: f64, compress: bool) -> i32 {
    UI_SCALE = scale;
    LAY_COMPRESS = compress;
    create_fonts();
    ROWS.clear();
    CARDS.clear();
    HEADERS.clear();

    let mut crc = RECT::default();
    let _ = GetClientRect(h, &mut crc);
    // 卡片恒填满客户区宽(压缩时只收窄边距, 不缩小字体)
    let dw = (crc.right - crc.left).max(100);
    let margin = if compress { sc(12) } else { sc(24) };
    let card_w = dw - margin * 2;
    let right_in = margin + card_w - sc(16);
    let row_h = sc(48);
    let gap = if compress { sc(6) } else { sc(12) };

    let bh = bar_h();
    // 自绘标题栏按钮(右上角): [关于][—][✕]
    CLOSE_R = RECT { left: dw - sc(46), top: 0, right: dw - sc(6), bottom: bh };
    MIN_R = RECT { left: CLOSE_R.left - sc(40), top: 0, right: CLOSE_R.left, bottom: bh };
    ABOUT_R = RECT { left: MIN_R.left - sc(48), top: 0, right: MIN_R.left, bottom: bh };

    let mut y = bh + if compress { sc(8) } else { sc(20) };
    TITLE_R = RECT { left: margin, top: y, right: dw - margin, bottom: y + sc(34) };
    y += if compress { sc(30) } else { sc(34) };
    SUB_R = RECT { left: margin, top: y + sc(2), right: dw - margin, bottom: y + sc(20) };
    y += if compress { sc(26) } else { sc(34) };

    // 行构造闭包集
    let mut add_row = |rows: &mut Vec<Row>, card: RECT, idx: usize, row: RowDef| unsafe {
        let ry = card.top + (idx as i32) * row_h;
        let rect = RECT { left: card.left, top: ry, right: card.right, bottom: ry + row_h };
        let cy = ry + row_h / 2;
        let mut rads = Vec::new();
        let mut btn = RECT::default();
        match row.kind {
            2 => {
                // 单选组: 右对齐排布
                let gapr = sc(20);
                let mut total = 0;
                for (i, id) in row.rads.iter().enumerate() {
                    if i > 0 { total += gapr; }
                    total += sc(26) + text_w(radio_label(*id), F_BODY);
                }
                let mut x = right_in - total;
                for (i, id) in row.rads.iter().enumerate() {
                    let w = sc(26) + text_w(radio_label(*id), F_BODY);
                    rads.push((*id, RECT { left: x, top: ry, right: x + w, bottom: ry + row_h }));
                    x += w;
                    if i + 1 < row.rads.len() {
                        x += gapr;
                    }
                }
            }
            3 => {
                let bw = sc(120);
                let bh = sc(30);
                btn = RECT { left: right_in - bw, top: cy - bh / 2, right: right_in, bottom: cy + bh / 2 };
            }
            // 5 = 热键录入框: 右侧一个可点击的"输入框"(点击进入按键捕获)
            // 注意: kind 4 已被"调光耗时滑块行"占用, 热键行必须用 5
            5 => {
                let bw = sc(150);
                let bh = sc(32);
                HK_R = RECT { left: right_in - bw, top: cy - bh / 2, right: right_in, bottom: cy + bh / 2 };
            }
            // 6 = 时间输入框(HH:MM), 由真实 EDIT 子控件承载, 这里只算矩形
            6 => {
                let bw = sc(110);
                let bh = sc(32);
                let r = RECT { left: right_in - bw, top: cy - bh / 2, right: right_in, bottom: cy + bh / 2 };
                if row.id == IDC_TIME_ON {
                    TIME_ON_R = r;
                } else {
                    TIME_OFF_R = r;
                }
            }
            _ => {}
        }
        if row.kind > 0 {
            crate::log_line(&format!(
                "settings: row id={} kind={} y={}..{}",
                row.id, row.kind, rect.top, rect.bottom
            ));
        }
        rows.push(Row { id: row.id, kind: row.kind, title: row.title, desc: row.desc, rect, rads, btn });
        };

    // 卡片 1: 快捷键
    push_group(&mut y, &mut HEADERS, margin, card_w, "快捷键", "\u{E765}");
    let c1 = RECT { left: margin, top: y, right: margin + card_w, bottom: y + row_h };
    CARDS.push(c1);
    add_row(&mut ROWS, c1, 0, RowDef {
        id: IDC_HOTKEY, kind: 5, title: "全局热键", desc: "点击后按下组合键即可更改", rads: &[],
    });
    y += row_h + gap;

    // 卡片 2: 屏幕(4 行 —— 检测按钮已并入滑块行)
    push_group(&mut y, &mut HEADERS, margin, card_w, "屏幕", "\u{E7F4}");
    let c2 = RECT { left: margin, top: y, right: margin + card_w, bottom: y + row_h * 4 };
    CARDS.push(c2);
    add_row(&mut ROWS, c2, 0, RowDef {
        id: 0, kind: 2, title: "亮屏后光标", desc: "鼠标指针的处理方式",
        rads: &[IDC_CUR_AUTO, IDC_CUR_HIDE, IDC_CUR_KEEP],
    });
    add_row(&mut ROWS, c2, 1, RowDef {
        id: IDC_DIM_EXT, kind: 1, title: "压暗外接屏", desc: "通过 DDC/CI 通道", rads: &[],
    });
    add_row(&mut ROWS, c2, 2, RowDef {
        id: IDC_DIM_INT, kind: 1, title: "压暗内置屏", desc: "通过 WMI 通道", rads: &[],
    });
    add_row(&mut ROWS, c2, 3, RowDef {
        id: IDC_HDR, kind: 1, title: "关闭 HDR", desc: "恢复亮度后自动打开", rads: &[],
    });
    y += row_h * 4 + gap;

    // 卡片 3: 时序
    push_group(&mut y, &mut HEADERS, margin, card_w, "时序", "\u{E823}");
    let c3 = RECT { left: margin, top: y, right: margin + card_w, bottom: y + row_h * 3 };
    CARDS.push(c3);
    add_row(&mut ROWS, c3, 0, RowDef {
        id: 0, kind: 2, title: "熄屏顺序", desc: "熄屏时的执行顺序",
        rads: &[IDC_ORDER_BLACK, IDC_ORDER_DIM, IDC_ORDER_FADE],
    });
    add_row(&mut ROWS, c3, 1, RowDef {
        id: 0, kind: 2, title: "亮屏方式", desc: "黑窗撤除的时机",
        rads: &[IDC_REST_READY, IDC_REST_DELAY],
    });
    add_row(&mut ROWS, c3, 2, RowDef {
        id: 0, kind: 4, title: "调光耗时", desc: "亮屏后黑窗保留时长(ms)", rads: &[],
    });
    // 滑块行几何: 左标题 + [检测按钮 + 滑块 + 右输入框](窄窗时依次让位, 防倒挂/裁切)
    {
        let ry = c3.top + row_h * 2;
        let tz = sc(280).min(card_w / 2);                    // 标题区宽度上限
        let sl = c3.left + tz;                               // 检测按钮左缘
        let bw2 = sc(56);                                    // 检测按钮宽
        MBTN_R = RECT {
            left: sl,
            top: ry + (row_h - sc(30)) / 2,
            right: sl + bw2,
            bottom: ry + (row_h + sc(30)) / 2,
        };
        let sl2 = sl + bw2 + sc(12);                         // 滑块左缘(按钮右侧)
        let er_right = right_in - sc(12);
        let er_left = (er_right - sc(84)).max(sl2 + sc(140)); // 输入框不越过滑块
        let sr = (er_left - sc(14)).max(sl2 + sc(60));        // 滑块保留最小宽度
        SLIDER_R = RECT { left: sl2, top: ry, right: sr, bottom: ry + row_h };
        EDIT_R = RECT {
            left: er_left,
            // 高度 sc(32) 配合 F_EDIT(sc(16)) 视觉垂直居中; 旧的 sc(26) 框高
            // 小于字体行高, 文字贴顶显得"不居中、大小不协调"(用户实拍)
            top: ry + (row_h - sc(32)) / 2,
            right: er_right,
            bottom: ry + (row_h + sc(32)) / 2,
        };
    }
    y += row_h * 3 + gap;

    // 卡片 3.5: 定时策略(每天到点自动熄屏 / 自动亮屏)
    push_group(&mut y, &mut HEADERS, margin, card_w, "定时", "\u{E916}");
    let cs = RECT { left: margin, top: y, right: margin + card_w, bottom: y + row_h * 3 };
    CARDS.push(cs);
    add_row(&mut ROWS, cs, 0, RowDef {
        id: IDC_SCHED, kind: 1, title: "定时熄屏", desc: "每天到点自动熄屏 / 亮屏", rads: &[],
    });
    add_row(&mut ROWS, cs, 1, RowDef {
        id: IDC_TIME_ON, kind: 6, title: "熄屏时间", desc: "24 小时制，格式 HH:MM", rads: &[],
    });
    add_row(&mut ROWS, cs, 2, RowDef {
        id: IDC_TIME_OFF, kind: 6, title: "亮屏时间", desc: "24 小时制，格式 HH:MM", rads: &[],
    });
    y += row_h * 3 + gap;
    push_group(&mut y, &mut HEADERS, margin, card_w, "启动与关闭", "\u{E7E8}");
    let c4 = RECT { left: margin, top: y, right: margin + card_w, bottom: y + row_h * 3 };
    CARDS.push(c4);
    add_row(&mut ROWS, c4, 0, RowDef {
        id: IDC_AUTOSTART, kind: 1, title: "开机自启", desc: "登录 Windows 后自动运行", rads: &[],
    });
    add_row(&mut ROWS, c4, 1, RowDef {
        id: IDC_SILENT, kind: 1, title: "静默启动", desc: "启动时不弹出设置面板", rads: &[],
    });
    add_row(&mut ROWS, c4, 2, RowDef {
        id: 0, kind: 2, title: "关闭窗口时", desc: "点击标题栏 X 的行为",
        rads: &[IDC_CLOSE_HIDE, IDC_CLOSE_EXIT],
    });
    y += row_h * 3;

    NOTE_R = RECT {
        left: margin,
        top: y + if LAY_COMPRESS { sc(4) } else { sc(8) },
        right: dw - margin,
        bottom: y + if LAY_COMPRESS { sc(20) } else { sc(26) },
    };
    // 底部备注行已按需求移除(不再绘制), 但仍回收其空间
    y + if LAY_COMPRESS { sc(10) } else { sc(18) }
}

/// 重排 + EDIT 子控件跟随 + 重绘(WM_SIZE 与轮询兜底共用)
/// NCHITTEST 调试: 仅在返回码变化时落日志(避免悬停刷屏)
fn ht_dbg(code: i32, rel_x: i32, rel_y: i32) {
    use std::sync::atomic::{AtomicI32, Ordering};
    static LAST: AtomicI32 = AtomicI32::new(-999);
    if LAST.swap(code, Ordering::Relaxed) != code {
        crate::log_line(&format!("settings: nchittest -> {} rel=({},{})", code, rel_x, rel_y));
    }
}

/// 开始边框缩放(消息路径与轮询兜底共用); src 标记触发来源便于诊断
unsafe fn begin_resize(h: HWND, edge: u32, src: &str) {
    let mut sp = POINT::default();
    let _ = GetCursorPos(&mut sp);
    let mut wr = RECT::default();
    let _ = GetWindowRect(h, &mut wr);
    R_START = sp;
    R_WIN = wr;
    R_EDGE = edge;
    RESIZE = true;
    let _ = SetCapture(h);
    crate::log_line(&format!("settings: resize begin edge={} ({})", edge, src));
}

unsafe fn relayout(h: HWND) {
    build_layout(h);
    if !H_EDIT.0.is_null() {
        let _ = SetWindowPos(
            H_EDIT, None,
            EDIT_R.left, EDIT_R.top,
            EDIT_R.right - EDIT_R.left, EDIT_R.bottom - EDIT_R.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    // 定时时间输入框跟随新布局
    for (hw, r) in [
        (H_EDIT_ON, TIME_ON_R),
        (H_EDIT_OFF, TIME_OFF_R),
    ] {
        if !hw.0.is_null() {
            let _ = SetWindowPos(
                hw, None,
                r.left, r.top,
                r.right - r.left, r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
    let _ = InvalidateRect(h, None, false);
}

unsafe fn win_size(client_h: i32, style: WINDOW_STYLE) -> (i32, i32) {
    let dw = sc(620);
    if PLAIN_MODE {
        let mut wr = RECT { left: 0, top: 0, right: dw, bottom: client_h };
        let _ = AdjustWindowRectEx(&mut wr, style, false, WINDOW_EX_STYLE(0));
        (wr.right - wr.left, wr.bottom - wr.top)
    } else {
        // 无边框(NCCALCSIZE 全客户区): 窗口尺寸 == 客户区尺寸, 无外框
        (dw, client_h)
    }
}

unsafe fn create_fonts() {
    del_font(&mut F_TITLE);
    del_font(&mut F_SUB);
    del_font(&mut F_HEAD);
    del_font(&mut F_BODY);
    del_font(&mut F_SEC);
    del_font(&mut F_MONO);
    del_font(&mut F_ICON);
    F_TITLE = font(28, 600, false);
    F_SUB = font(12, 400, false);
    F_HEAD = font(14, 600, false);
    F_BODY = font(14, 400, false);
    F_SEC = font(12, 400, false);
    F_MONO = font(13, 400, true);
    F_ICON = icon_font();
    F_EDIT = font(16, 400, false);
}

struct RowDef {
    id: u16,
    kind: u8,
    title: &'static str,
    desc: &'static str,
    rads: &'static [u16],
}

unsafe fn push_group(y: &mut i32, headers: &mut Vec<(String, RECT)>, x: i32, card_w: i32, t: &str, icon: &str) {
    let r = RECT { left: x, top: *y, right: x + card_w, bottom: *y + sc(22) };
    headers.push((format!("{}\u{1}{}", icon, t), r)); // \u{1} 分隔图标字形与组名
    *y += if LAY_COMPRESS { sc(22) } else { sc(30) };
}

/// 强调色: 注册表 SystemAccentColor 优先, DWM 回退; 保证可读性
unsafe fn read_accent() -> COLORREF {
    if let Some(c) = accent_from_registry() {
        return ensure_contrast(c);
    }
    use windows::Win32::Graphics::Dwm::DwmGetColorizationColor;
    let mut color: u32 = 0;
    let mut opaque: BOOL = BOOL(0);
    if DwmGetColorizationColor(&mut color, &mut opaque).is_ok() {
        let r = (color >> 16) & 0xFF;
        let g = (color >> 8) & 0xFF;
        let b = color & 0xFF;
        return ensure_contrast(COLORREF((b << 16) | (g << 8) | r));
    }
    COLORREF(0x00C06700)
}

fn accent_from_registry() -> Option<COLORREF> {
    use windows::core::w;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
    };
    unsafe {
        let mut key = windows::Win32::System::Registry::HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, w!("Software\\Microsoft\\Windows\\DWM"), 0, KEY_READ, &mut key)
            .is_err()
        {
            return None;
        }
        let mut ty = REG_DWORD;
        let mut data: u32 = 0;
        let mut cb: u32 = 4;
        let r = RegQueryValueExW(
            key, w!("AccentColor"), None, Some(&mut ty),
            Some(&mut data as *mut u32 as *mut u8), Some(&mut cb),
        );
        let _ = RegCloseKey(key);
        if r.is_err() {
            return None;
        }
        let r8 = (data & 0xFF) as u32;
        let g8 = ((data >> 8) & 0xFF) as u32;
        let b8 = ((data >> 16) & 0xFF) as u32;
        Some(COLORREF((b8 << 16) | (g8 << 8) | r8))
    }
}

fn ensure_contrast(c: COLORREF) -> COLORREF {
    let r = (c.0 & 0xFF) as i32;
    let g = ((c.0 >> 8) & 0xFF) as i32;
    let b = ((c.0 >> 16) & 0xFF) as i32;
    let lum = (r * 299 + g * 587 + b * 114) / 1000;
    let target = 118;
    if lum < target {
        let k = target as f64 / lum.max(1) as f64;
        let f = |v: i32| ((v as f64 * k).min(255.0)) as u32;
        COLORREF(f(b) << 16 | f(g) << 8 | f(r))
    } else if lum > 200 {
        let k = 200.0 / lum as f64;
        let f = |v: i32| ((v as f64 * k).min(255.0)) as u32;
        COLORREF(f(b) << 16 | f(g) << 8 | f(r))
    } else {
        c
    }
}

// ================= 绘制 =================

unsafe fn paint(h: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let wdc = BeginPaint(h, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(h, &mut rc);
    let cw = rc.right;
    let ch = rc.bottom;

    // 双缓冲
    let mem = CreateCompatibleDC(wdc);
    let bmp = CreateCompatibleBitmap(wdc, cw, ch);
    let ob = SelectObject(mem, bmp);

    fill(mem, &rc, WIN_BG);

    let mut g: *mut GpGraphics = std::ptr::null_mut();
    GdipCreateFromHDC(mem, &mut g);
    GdipSetSmoothingMode(g, SmoothingModeAntiAlias);

    let cfg = crate::cfg_clone();

    // 卡片
    for card in CARDS.iter() {
        let (x, y, w, hh) = (
            card.left as f32, card.top as f32,
            (card.right - card.left) as f32, (card.bottom - card.top) as f32,
        );
        gp_fill(g, (x, y, w, hh), sc(8) as f32, CARD);
        gp_stroke(g, (x, y, w, hh), sc(8) as f32, CARD_STROKE, 1.0);
    }

    // 行内容
    for (idx, row) in ROWS.iter().enumerate() {
        let row_hot = HOT_ROW == idx as i32;

        // 行分隔线(第一行除外): 与文字左缘对齐
        if idx > 0 && CARDS.iter().any(|c| c.top <= row.rect.top && c.bottom >= row.rect.bottom) {
            let pen = CreatePen(PS_SOLID, 1, SEP);
            let old = SelectObject(mem, pen);
            let yy = row.rect.top;
            let _ = MoveToEx(mem, row.rect.left + sc(16), yy, None);
            let _ = LineTo(mem, row.rect.right - sc(16), yy);
            let _ = SelectObject(mem, old);
            let _ = DeleteObject(pen);
        }

        // 行 hover
        if row_hot && row.kind != 0 {
            let (x, y, w, hh) = (
                row.rect.left as f32 + 2.0, row.rect.top as f32 + 1.0,
                (row.rect.right - row.rect.left - 4) as f32, (row.rect.bottom - row.rect.top - 2) as f32,
            );
            gp_fill(g, (x, y, w, hh), sc(4) as f32, ROW_HOVER);
        }

        // 文本
        let tx = row.rect.left + sc(16);
        let measure_row = row.kind == 4 && !MEASURE.is_empty();
        let has_desc = !row.desc.is_empty() || measure_row;
        let (ty, dy) = if has_desc {
            (row.rect.top + sc(7), row.rect.top + sc(27))
        } else {
            (row.rect.top, row.rect.top)
        };
        let th = if has_desc { sc(20) } else { row_h_px() };
        let tr = RECT { left: tx, top: ty, right: row.rect.right, bottom: ty + th };
        dt(mem, &tr, row.title, LABEL, F_BODY, 0);
        if has_desc {
            let dtxt = if measure_row { MEASURE.as_str() } else { row.desc };
            let dr = RECT { left: tx, top: dy, right: row.rect.right, bottom: dy + sc(18) };
            dt(mem, &dr, dtxt, SEC, F_SEC, 0);
        }

        // 控件
        match row.kind {
            0 => {}
            // 5 = 热键录入框: 右侧圆角"输入框", 点击进入按键捕获; 捕获/悬停强调色描边
            5 => {
                let hk = HK_R;
                let w = (hk.right - hk.left) as f32;
                let h = (hk.bottom - hk.top) as f32;
                let rad = sc(6) as f32;
                let capturing = HK_CAPTURE;
                let txt = if capturing {
                    "请按下组合键…".to_string()
                } else {
                    hotkey_text(&cfg)
                };
                // 填充: 捕获态用淡强调色底, 平时白底
                let fill = if capturing {
                    COLORREF(0x00F2F2FF) // 淡蓝紫(强调色 7% 亮度)
                } else {
                    WHITE
                };
                gp_fill(g, (hk.left as f32, hk.top as f32, w, h), rad, fill);
                let (st, sw) = if capturing {
                    (ACCENT, 2.0)
                } else if HK_HOT {
                    (ACCENT, 1.5)
                } else {
                    (CARD_STROKE, 1.0)
                };
                gp_stroke(g, (hk.left as f32, hk.top as f32, w, h), rad, st, sw);
                let tc = if capturing { SEC } else { LABEL };
                dt(mem, &hk, &txt, tc, F_BODY, 2); // 2 = 水平居中
            }
            // 6 = 时间输入框: 底框与描边自绘, 文本由 EDIT 子控件渲染
            6 => {
                let r = if row.id == IDC_TIME_ON { TIME_ON_R } else { TIME_OFF_R };
                let w = (r.right - r.left) as f32;
                let h = (r.bottom - r.top) as f32;
                gp_fill(g, (r.left as f32, r.top as f32, w, h), sc(6) as f32, WHITE);
                gp_stroke(
                    g,
                    (r.left as f32, r.top as f32, w, h),
                    sc(6) as f32,
                    CARD_STROKE,
                    1.0,
                );
            }
            1 => {
                let on = toggle_of(row.id, &cfg).unwrap_or(false);
                let cx = row.rect.right - sc(16) - sc(20);
                draw_toggle(g, cx, row.rect.top + row_h_px() / 2, on);
            }
            2 => {
                for (id, rr) in row.rads.iter() {
                    let on = radio_of(*id, &cfg).unwrap_or(false);
                    let ccx = rr.left + sc(10);
                    draw_radio(g, ccx, rr.top + row_h_px() / 2, on);
                    let lr = RECT {
                        left: rr.left + sc(26),
                        top: rr.top,
                        right: rr.right,
                        bottom: rr.bottom,
                    };
                    dt(mem, &lr, radio_label(*id), LABEL, F_BODY, 0);
                }
            }
            3 => {
                draw_button(g, &row.btn, HOT_BTN && row_hot);
                dt(mem, &row.btn, "检测", LABEL, F_BODY, 2);
            }
            4 => {
                // 检测按钮(左) + 滑块; 右侧数值由 EDIT 输入框显示
                draw_button(g, &MBTN_R, row_hot && HOT_BTN);
                dt(mem, &MBTN_R, "检测", LABEL, F_BODY, 2);
                draw_slider(g, &SLIDER_R, cfg.delay_ms);
            }
            _ => {}
        }
    }

    GdipDeleteGraphics(g);

    // GDI 层: 自绘标题栏 / 页面标题 / 副标题 / 组标题(图标+文字) / 底部说明
    {
        // 标题栏: 应用图标 + 应用名 + [关于][—][✕](悬停高亮 + 按钮间分隔线)
        let bh = bar_h();
        let icon = app_icon_cached(sc(18));
        if !icon.0.is_null() {
            let _ = DrawIconEx(
                mem, sc(14), (bh - sc(18)) / 2, icon, sc(18), sc(18), 0, None, DI_FLAGS(3),
            );
        }
        let ntr = RECT { left: sc(40), top: 0, right: ABOUT_R.left, bottom: bh };
        dt(mem, &ntr, "一键熄屏", LABEL, F_HEAD, 0);
        // 悬停高亮(WinUI 标题栏按钮风格: 整格浅灰)
        let hot = HOT_TB;
        if hot > 0 {
            let hr = match hot {
                1 => ABOUT_R, 2 => MIN_R, _ => CLOSE_R,
            };
            let hbr = CreateSolidBrush(SEP);
            let hr2 = RECT { left: hr.left + 1, top: 1, right: hr.right - 1, bottom: hr.bottom - 1 };
            FillRect(mem, &hr2, hbr);
            let _ = DeleteObject(hbr);
        }
        // 按钮间分隔线
        let pen = CreatePen(PS_SOLID, 1, SEP);
        let old = SelectObject(mem, pen);
        for xx in [MIN_R.left, CLOSE_R.left] {
            let _ = MoveToEx(mem, xx, bh / 4, None);
            let _ = LineTo(mem, xx, bh * 3 / 4);
        }
        let _ = SelectObject(mem, old);
        let _ = DeleteObject(pen);
        dt(mem, &ABOUT_R, "关于", SEC, F_BODY, 2);
        dt(mem, &MIN_R, "\u{E921}", LABEL, F_ICON, 2); // — 最小化(MDL2 字形)
        dt(mem, &CLOSE_R, "\u{E8BB}", LABEL, F_ICON, 2); // ✕ 需 MDL2 字形, F_BODY 无此字形
        let pen = CreatePen(PS_SOLID, 1, SEP);
        let old = SelectObject(mem, pen);
        let _ = MoveToEx(mem, 0, bh, None);
        let _ = LineTo(mem, rc.right, bh);
        let _ = SelectObject(mem, old);
        let _ = DeleteObject(pen);
        // 输入框扁平描边(圆角 1px, 替代系统白底黑框)
        let pen2 = CreatePen(PS_SOLID, 1, CARD_STROKE);
        let old2 = SelectObject(mem, pen2);
        let hb = GetStockObject(HOLLOW_BRUSH);
        let oldb = SelectObject(mem, hb);
        let _ = RoundRect(
            mem, EDIT_R.left, EDIT_R.top, EDIT_R.right, EDIT_R.bottom, sc(8), sc(8),
        );
        let _ = SelectObject(mem, old2);
        let _ = SelectObject(mem, oldb);
        let _ = DeleteObject(pen2);
    }
    dt(mem, &TITLE_R, "设置", LABEL, F_TITLE, 0);
    dt(mem, &SUB_R, "双引擎调光 · 更改立即生效并自动保存", SEC, F_SUB, 0);
    for (txt, r) in HEADERS.iter() {
        let (icon, name) = txt.split_once('\u{1}').unwrap_or(("", txt.as_str()));
        let ir = RECT { left: r.left, top: r.top, right: r.left + sc(22), bottom: r.bottom };
        dt(mem, &ir, icon, SEC, F_ICON, 0);
        let nr = RECT { left: r.left + sc(28), top: r.top, right: r.right, bottom: r.bottom };
        dt(mem, &nr, name, LABEL, F_HEAD, 0);
    }
    // 底部备注说明已按需求移除
    // 只拷贝本次脏区: 全客户区 BitBlt 会放大闪烁观感(小 hover 变化也整页重绘)
    let pr = ps.rcPaint;
    let bw = (pr.right - pr.left).max(0);
    let bh = (pr.bottom - pr.top).max(0);
    if bw > 0 && bh > 0 {
        let _ = BitBlt(wdc, pr.left, pr.top, bw, bh, mem, pr.left, pr.top, SRCCOPY);
    }
    let _ = SelectObject(mem, ob);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mem);
    let _ = EndPaint(h, &mut ps);
}

fn row_h_px() -> i32 {
    sc(48)
}

/// 解析 "HH:MM" 时间(容忍全角冒号/空格/纯四位数字如 2300)
fn parse_hm(s: &str) -> Option<(u8, u8)> {
    let t: String = s.trim().replace(['：', '.', ' '], ":");
    let t = t.trim();
    let (hs, ms) = if let Some((a, b)) = t.split_once(':') {
        (a.to_string(), b.to_string())
    } else if t.len() == 4 && t.chars().all(|c| c.is_ascii_digit()) {
        (t[..2].to_string(), t[2..].to_string())
    } else if t.len() <= 2 && t.chars().all(|c| c.is_ascii_digit()) {
        (t.to_string(), "0".to_string())
    } else {
        return None;
    };
    let h: u8 = hs.trim().parse().ok()?;
    let m: u8 = ms.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

/// 热键显示文本(Ctrl + Alt + S 形式), 由配置动态生成
unsafe fn hotkey_text(c: &crate::Cfg) -> String {
    let mut s = String::new();
    if c.hotkey_mod & 0x0002 != 0 { s.push_str("Ctrl + "); }
    if c.hotkey_mod & 0x0001 != 0 { s.push_str("Alt + "); }
    if c.hotkey_mod & 0x0004 != 0 { s.push_str("Shift + "); }
    if c.hotkey_mod & 0x0008 != 0 { s.push_str("Win + "); }
    s.push_str(&vk_name(c.hotkey_vk));
    s
}

fn vk_name(vk: u32) -> String {
    match vk {
        0x30..=0x39 => ((vk as u8) as char).to_string(),               // 0..9
        0x41..=0x5A => ((vk as u8) as char).to_string(),               // A..Z
        0x70..=0x87 => format!("F{}", vk - 0x6F),                      // F1..F24
        0x20 => "Space".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x25 => "Left".into(), 0x26 => "Up".into(),
        0x27 => "Right".into(), 0x28 => "Down".into(),
        0x24 => "Home".into(), 0x23 => "End".into(),
        0x21 => "PgUp".into(), 0x22 => "PgDn".into(),
        0x2D => "Insert".into(), 0x2E => "Delete".into(),
        0x1B => "Esc".into(),
        _ => format!("VK 0x{:02X}", vk),
    }
}

/// 捕获态按键扫描表: 字母/数字/F1-F8
const CAPTURE_VKS: [u32; 44] = [
    0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x4B, 0x4C, 0x4D,
    0x4E, 0x4F, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A,
    0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39,
    0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, // F1..F8
];

// ================= 交互 =================

// 命中: (-3, IDC_MEASURE)=检测按钮 / (-2, 0)=滑块 / (行序, 选项id) / (-1, 0)=无
fn hit(pt: POINT) -> (i32, u16) {
    unsafe {
        // 检测按钮(滑块行左侧)
        if pt.x >= MBTN_R.left && pt.x <= MBTN_R.right && pt.y >= MBTN_R.top && pt.y <= MBTN_R.bottom {
            return (-3, IDC_MEASURE);
        }
        // 热键录入框
        if pt.x >= HK_R.left && pt.x <= HK_R.right && pt.y >= HK_R.top && pt.y <= HK_R.bottom {
            return (-3, IDC_HOTKEY);
        }
        // 滑块
        if pt.x >= SLIDER_R.left && pt.x <= SLIDER_R.right && pt.y >= SLIDER_R.top && pt.y <= SLIDER_R.bottom {
            return (-2, 0);
        }
        for (idx, row) in ROWS.iter().enumerate() {
            let in_row = pt.x >= row.rect.left && pt.x <= row.rect.right
                && pt.y >= row.rect.top && pt.y <= row.rect.bottom;
            if !in_row {
                continue;
            }
            for (id, rr) in row.rads.iter() {
                if pt.x >= rr.left && pt.x <= rr.right {
                    return (idx as i32, *id);
                }
            }
            if row.kind == 3
                && pt.x >= row.btn.left && pt.x <= row.btn.right
                && pt.y >= row.btn.top && pt.y <= row.btn.bottom
            {
                return (idx as i32, row.id);
            }
            if row.kind == 1 {
                return (idx as i32, row.id);
            }
        }
        (-1, 0)
    }
}

unsafe fn update_hot(h: HWND, pt: POINT) {
    let (rowi, id) = hit(pt);
    let mut nh = -1i32;
    let mut nb = false;
    let mut hk = false;
    if rowi == -3 {
        // 检测按钮悬停(无行高亮); 热键框单独维护 HK_HOT 描边高亮
        if id == IDC_HOTKEY {
            hk = true;
        } else {
            nb = true;
        }
    } else if rowi >= 0 && nh == -1 {
        nh = rowi;
        let row = &ROWS[rowi as usize];
        nb = row.kind == 3
            && pt.x >= row.btn.left && pt.x <= row.btn.right
            && pt.y >= row.btn.top && pt.y <= row.btn.bottom;
    }
    if hk != HK_HOT {
        HK_HOT = hk;
        let _ = InvalidateRect(h, None, false);
    }
    {
        // 注意: 不能加 "|| id != 0" —— 悬停在开关/单选/按钮行时 id 恒非 0,
        // 会导致每次 WM_MOUSEMOVE 都整窗重绘(hover 高亮反复刷新=实机"阴影闪烁"根因)
        if nh != HOT_ROW || nb != HOT_BTN {
            HOT_ROW = nh;
            HOT_BTN = nb;
            let _ = InvalidateRect(h, None, false);
        }
    }
    if !TRACKING {
        TRACKING = true;
        let mut tme = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: h,
            dwHoverTime: 0,
        };
        let _ = TrackMouseEvent(&mut tme);
    }
}

/// 设置面板窗口过程
///
/// 重要: WM_NCCREATE / WM_NCCALCSIZE 必须在函数最前面显式交 DefWindowProc 并
/// 直接返回其结果。本环境下若让它们走 match 的 default 分支(语义完全相同的调用),
/// DefWindowProcW 会异常: NCCREATE 返回 0 -> 内核取消创建(返回 null, 183/0 漂移);
/// NCCALCSIZE 不收缩矩形 -> 非客户区丢失(无标题栏, 无法拖动/关闭)。
/// 显式分支则行为正常。原因未明(疑与本 VM 的 win32k 相关), 但 100% 可复现。
unsafe extern "system" fn dlg_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // 链路探针: 记录窗口收到的消息(限 400 条; 过滤 PAINT/ERASEBKGND/GETICON 风暴,
    // 否则额度被任务栏闪烁循环烧光, 看不到鼠标消息)
    let noise = m == WM_PAINT || m == WM_ERASEBKGND || m == WM_GETICON || m == WM_NCACTIVATE;
    if !noise && PROBE_ON.load(std::sync::atomic::Ordering::Relaxed) {
        let n = PROBE_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 400 {
            crate::log_line(&format!("probe 0x{:04X} w=0x{:016X} l=0x{:016X}", m, w.0, l.0));
        }
    }
    // ===== A/B 开关: PLAIN 模式完整复刻 Python 版窗口结构(朴素 wndproc) =====
    // Python 版在同一台机器上点击正常 —— 证明"环境 win32k 异常"不成立,
    // 是 Rust 版窗口结构差异(style/HOLLOW_BRUSH/前置分支)触发的消息丢失。
    if !PLAIN_MODE {
        if m == WM_NCCREATE || m == WM_NCCALCSIZE || m == WM_NCPAINT || m == WM_NCACTIVATE {
            // 无边框自绘标题栏: NCCALCSIZE 把客户区扩到整个窗口(去掉系统标题栏),
            // 标题栏由 paint 自绘(图标/应用名/关于/✕), 拖动/边框/按钮全自管。
            // 显式分支处理(本环境 NCCALCSIZE 走 match default 会让 DefWindowProcW 异常)。
            if m == WM_NCCALCSIZE && w.0 != 0 {
                let r = l.0 as *mut RECT;
                if !r.is_null() {
                    // 客户区 = 窗口矩形(不动 lParam 指向的矩形即保持原值)
                    return LRESULT(0);
                }
                return DefWindowProcW(h, m, w, l);
            }
            if m == WM_NCPAINT {
                return LRESULT(0); // 不让系统画标题栏
            }
            // 激活态变化(弹模态框/切焦点/拖动)时不让 DefWindowProc 重绘非客户区:
            // 它是"点关于、一拖动就冒出系统标题栏"的触发点。返回 1 = 已处理的激活变化。
            if m == WM_NCACTIVATE {
                return LRESULT(1);
            }
            return DefWindowProcW(h, m, w, l);
        }
        // NC 通道交互(本环境客户区鼠标消息不可靠, 全部交互改走 NC 消息)
        if m == WM_NCLBUTTONDOWN {
            match w.0 as u32 {
                2 => {
                    // HTCAPTION: 先判定是否标题栏按钮(关于/最小化/关闭)——
                    // NCHITTEST 对按钮区也返回 HTCAPTION(客户区消息不可靠), 点击在此分流
                    let mut sp = POINT::default();
                    let _ = GetCursorPos(&mut sp);
                    let mut wr = RECT::default();
                    let _ = GetWindowRect(h, &mut wr);
                    let rel_x = sp.x - wr.left;
                    let rel_y = sp.y - wr.top;
                    if rel_y >= 0 && rel_y < bar_h() && rel_x >= ABOUT_R.left {
                        crate::log_line(&format!(
                            "settings: nclb tb button rel=({},{})",
                            rel_x, rel_y
                        ));
                        handle_click(h, sp.x, sp.y);
                        return LRESULT(0);
                    }
                    // 非按钮: 自实现拖动
                    CAPT_START = sp;
                    CAPT_WIN = wr;
                    CAPT_DRAG = true;
                    CAPT_MOVED = false;
                    let _ = SetCapture(h);
                    let rel_y = sp.y - wr.top;
                    crate::log_line(&format!("settings: drag begin rel_y={}", rel_y));
                    return LRESULT(0);
                }
                // 10..17 = HTLEFT/HTRIGHT/HTTOP*/HTBOTTOM*: 自实现拖边缩放
                // (不进系统模态循环 —— 本环境该循环异常; 跟随由 WM_TIMER 兜底驱动)
                10..=17 => {
                    begin_resize(h, w.0 as u32, "nc");
                    return LRESULT(0);
                }
                5 => {
                    // HTMENU: 内容区点击 —— 按 lParam 屏幕坐标还原
                    let sx = (l.0 & 0xFFFF) as i16 as i32;
                    let sy = ((l.0 >> 16) & 0xFFFF) as i16 as i32;
                    handle_click(h, sx, sy);
                    return LRESULT(0);
                }
                _ => {}
            }
        }
        if m == WM_NCMOUSEMOVE {
            // NC 移动: 滑块拖动中跟随更新
            if DRAG {
                let mut pt = POINT {
                    x: (l.0 & 0xFFFF) as i16 as i32,
                    y: ((l.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                let _ = ScreenToClient(h, &mut pt);
                let x0 = SLIDER_R.left + sc(10);
                let x1 = SLIDER_R.right - sc(10);
                let f = ((pt.x - x0) as f64 / (x1 - x0) as f64).clamp(0.0, 1.0);
                let mut c = crate::cfg_clone();
                c.delay_ms = (500.0 * f) as u32;
                crate::cfg_apply(c);
                let _ = InvalidateRect(h, None, false);
                return LRESULT(0);
            }
        }
        if m == WM_NCLBUTTONUP {
            if DRAG {
                DRAG = false;
                let _ = InvalidateRect(h, None, false);
            }
            if RESIZE {
                RESIZE = false;
                let _ = ReleaseCapture();
                crate::log_line("settings: resize end");
            }
            return LRESULT(0);
        }
        if m == WM_NCHITTEST {
            static NC_LOG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let sx = (l.0 & 0xFFFF) as i16 as i32;
            let sy = ((l.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut wr = RECT::default();
            let _ = GetWindowRect(h, &mut wr);
            let rel_y = sy - wr.top;
            let rel_x = sx - wr.left;
            let ww = wr.right - wr.left;
            let wh = wr.bottom - wr.top;
            let bh = bar_h();
            // 按钮区从 ABOUT_R.left 到右缘(总宽 sc(134)=CLOSE46+MIN40+ABOUT48)。
            // 旧写法 ABOUT_R.left.max(ww - sc(96)) 是旧布局残留, ww-sc(96)=655 > 608,
            // 把"关于"按钮左半 47px 挤出按钮区(实测 poll press tb_btn=false 实锤)。
            let btn_zone = (rel_y >= 0 && rel_y < bh) && (rel_x >= ABOUT_R.left);
            // 边框热区: 返回标准 HT* 码 → 系统自动显示缩放光标;
            // 按下后由 WM_NCLBUTTONDOWN 自实现拖边(不进系统模态循环)。
            let bw = (sc(6)).max(4);
            let zn_l = rel_x < bw;
            let zn_r = rel_x >= ww - bw;
            let zn_t = rel_y < bw;
            let zn_b = rel_y >= wh - bw;
            if zn_t && zn_l { ht_dbg(13, rel_x, rel_y); return LRESULT(13); } // HTTOPLEFT
            if zn_t && zn_r { ht_dbg(14, rel_x, rel_y); return LRESULT(14); } // HTTOPRIGHT
            if zn_b && zn_l { ht_dbg(16, rel_x, rel_y); return LRESULT(16); } // HTBOTTOMLEFT
            if zn_b && zn_r { ht_dbg(17, rel_x, rel_y); return LRESULT(17); } // HTBOTTOMRIGHT
            if zn_b { ht_dbg(15, rel_x, rel_y); return LRESULT(15); }         // HTBOTTOM
            if zn_l { ht_dbg(10, rel_x, rel_y); return LRESULT(10); }         // HTLEFT
            if zn_r { ht_dbg(11, rel_x, rel_y); return LRESULT(11); }         // HTRIGHT
            if zn_t && rel_y < bh && !btn_zone { ht_dbg(12, rel_x, rel_y); return LRESULT(12); } // HTTOP(仅标题条内)
            // 自绘标题栏(含按钮区) → HTCAPTION: 按钮点击在 NCLBUTTONDOWN(2) 分支优先判定
            // (本环境客户区鼠标消息不可靠, 按钮若走 HTCLIENT→客户区消息会点击无效);
            // WM_TIMER 轮询同判定作兜底, handle_click 内 300ms 节流防双触发。
            if rel_y >= 0 && rel_y < bh {
                let n = NC_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 30 {
                    crate::log_line(&format!(
                        "settings: nchittest caption rel=({},{}) win=({},{})",
                        rel_x, rel_y, wr.left, wr.top
                    ));
                }
                return LRESULT(2); // HTCAPTION
            }
            // 内容区: 标准 HTCLIENT, 物理点击走客户区消息;
            // WM_TIMER 轮询(GetAsyncKeyState)作为消息丢失时的兜底通道。
            return LRESULT(1); // HTCLIENT
        }
    }

    /// 关闭窗口: 按"关闭窗口时"配置分流 —— 0=隐藏到托盘, 1=退出程序。
    /// 不走 SendMessageW(WM_CLOSE): 本环境实测 WM_CLOSE 能抵达 dlg_proc 入口与
    /// match 之前, 但 match 分支不执行(消息派发层异常), 点击✕无任何反应。
    unsafe fn do_close(h: HWND) {
        let act = crate::cfg_clone().close_action;
        crate::log_line(&format!("settings: close action={}", act));
        if act == 1 {
            let r = DestroyWindow(h); // WM_DESTROY → PostQuitMessage → 进程退出
            crate::log_line(&format!("settings: close destroy ok={}", r.is_ok()));
            // 不依赖 WM_DESTROY 的 PostQuitMessage: 本环境 DestroyWindow 内部的
            // WM_DESTROY 也可能不投递(实测 wm_destroy 日志缺失、窗口没了进程还在,
            // 单实例互斥把后续启动全挡死)。这里直接结束消息循环。
            PostQuitMessage(0);
        } else {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }

    /// 点击处理(客户区逻辑坐标)。NC 通道与普通通道共用。
    unsafe fn handle_click(h: HWND, sx: i32, sy: i32) {
        // 节流: 消息路径与轮询路径可能同时触发同一物理点击
        let now = windows::Win32::System::SystemInformation::GetTickCount();
        let last = LAST_CLICK.load(std::sync::atomic::Ordering::Relaxed);
        // 节流: 消息路径与轮询路径可能同时触发同一物理点击。
        // 300ms 太长会把"快速连续点两个开关"的第二次吞掉, 实测手感"点不动"。
        if now.wrapping_sub(last) < 120 {
            return;
        }
        LAST_CLICK.store(now, std::sync::atomic::Ordering::Relaxed);
        let mut pt = POINT { x: sx, y: sy };
        let _ = ScreenToClient(h, &mut pt);
        // 自绘标题栏按钮(关于 / 最小化 / 关闭)
        if pt.y >= 0 && pt.y < bar_h() {
            if pt.x >= CLOSE_R.left && pt.x < CLOSE_R.right {
                do_close(h);
                return;
            }
            if pt.x >= MIN_R.left && pt.x < MIN_R.right {
                crate::log_line("settings: titlebar minimize");
                // 本环境系统最小化(iconic)后无法还原 —— SW_RESTORE/SC_RESTORE/
                // ShowWindowAsync/AttachThreadInput 全部失败(记事本同方式正常, 只有
                // 本进程不行), 托盘点击都救不回来。因此最小化实现为"收起到托盘":
                // 隐藏窗口 + 气泡告知, 托盘左键/热键可原样唤起(窗口未进 iconic)。
                let _ = ShowWindow(h, SW_HIDE);
                crate::tray_balloon("已收起到托盘", "点击托盘图标或按热键可重新打开设置面板");
                return;
            }
            if pt.x >= ABOUT_R.left && pt.x < ABOUT_R.right {
                crate::log_line("settings: titlebar about");
                let txt = wide(
                    "双引擎调光原理:\n\n\
                     · 内置屏幕 — 经 WMI 通道直接调节面板背光\n\
                     · 外接显示器 — 经 DDC/CI 通道下发亮度指令\n\
                     · 关闭 HDR — 熄屏时关闭, 恢复亮度后自动打开\n\
                     · 熄屏 — 全屏黑窗覆盖 + 关闭背光(引擎按配置启用)\n\
                     · 亮屏 — 撤除黑窗并按调光耗时延迟恢复, 避免闪白\n\n\
                     更改立即生效并自动保存。",
                );
                let cap = wide("关于 一键熄屏");
                let _ = MessageBoxW(h, PCWSTR(txt.as_ptr()), PCWSTR(cap.as_ptr()), MB_OK | MB_ICONINFORMATION);
                return;
            }
        }
        let (rowi, id) = hit(pt);
        crate::log_line(&format!("settings: click pt=({},{}) hit={},{}", pt.x, pt.y, rowi, id));
        if rowi == -3 && id == IDC_MEASURE {
            MEASURE.clear();
            crate::run_measure();
        } else if rowi == -3 && id == IDC_HOTKEY {
            // 点击热键框: 进入/退出按键捕获态。
            // 进入时必须注销全局热键 —— 否则在捕获框里按到原来那组键,
            // WM_HOTKEY 会照常触发一次熄屏(用户实测踩到)。
            set_capture(!HK_CAPTURE);
            if HK_CAPTURE {
                crate::hotkey_unregister(h);
            } else {
                let c = crate::cfg_clone();
                crate::hotkey_apply(h, c.hotkey_mod, c.hotkey_vk);
            }
            let _ = InvalidateRect(h, None, false);
        } else if rowi == -2 {
            DRAG = true;
            let x0 = SLIDER_R.left + sc(10);
            let x1 = SLIDER_R.right - sc(10);
            let f = ((pt.x - x0) as f64 / (x1 - x0) as f64).clamp(0.0, 1.0);
            let mut c = crate::cfg_clone();
            c.delay_ms = (500.0 * f) as u32;
            crate::cfg_apply(c);
        } else if rowi >= 0 {
            let kind = ROWS[rowi as usize].kind;
            if kind == 1 && id != 0 {
                apply_toggle(id);
            } else if kind == 2 && id != 0 {
                apply_radio(id);
            } else if kind == 3 && id == IDC_MEASURE {
                MEASURE.clear();
                crate::run_measure();
            }
        }
        let _ = InvalidateRect(h, None, false);
    }
    // 拖动状态安全阀: 不依赖可能丢失的抬起消息 —— 每次鼠标事件都检查
    // 左键真实状态(GetAsyncKeyState), 已松开则立即结束拖动。
    // 之前"抬起消息丢失 → CAPT_DRAG 残留 → 窗口被绑在光标上"的根因就在此。
    unsafe fn capt_release_if_up() -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        if CAPT_DRAG {
            let down = (GetAsyncKeyState(0x01 /*VK_LBUTTON*/) as u16 & 0x8000) != 0;
            // 消抖: 注入/远程输入下 GetAsyncKeyState 的按键状态会瞬时抖动(实测
            // drag begin 后下一 tick 就读到松开, 拖动立刻被安全阀掐断)。
            // 连续 3 tick(48ms) 都读到松开才认定真的抬起。
            static UP_TICKS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if down {
                UP_TICKS.store(0, std::sync::atomic::Ordering::Relaxed);
                return false;
            }
            let n = UP_TICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n < 3 {
                return false;
            }
            UP_TICKS.store(0, std::sync::atomic::Ordering::Relaxed);
            CAPT_DRAG = false;
            CAPT_MOVED = false;
            let h0 = WND;
            if !h0.0.is_null() {
                let _ = ReleaseCapture();
                let _ = InvalidateRect(h0, None, false);
            }
            return true; // 已释放
        }
        false
    }
    // 轮询点击兜底(常开, 16ms): 客户区鼠标消息可能随机丢失,
    // 用 GetAsyncKeyState+GetCursorPos 直接检测按下沿, 与消息路径靠 300ms 节流去重。
    if m == WM_TIMER && w.0 == 1 {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        // 热键捕获: 点击热键框后进入, 按下"修饰键 + 普通键"完成, ESC 取消。
        // 用轮询而不是 WM_KEYDOWN —— 本环境客户区键盘消息同样不可靠。
        if HK_CAPTURE {
            let kd = |vk: u32| (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0;
            if kd(0x1B) {
                // ESC 取消: 恢复原热键注册
                set_capture(false);
                let c = crate::cfg_clone();
                crate::hotkey_apply(h, c.hotkey_mod, c.hotkey_vk);
                let _ = InvalidateRect(h, None, false);
            } else {
                let mut mods = 0u32;
                if kd(0x11) { mods |= 0x0002; } // Ctrl
                if kd(0x12) { mods |= 0x0001; } // Alt
                if kd(0x10) { mods |= 0x0004; } // Shift
                if kd(0x5B) || kd(0x5C) { mods |= 0x0008; } // Win
                for &vk in CAPTURE_VKS.iter() {
                    if kd(vk) {
                        if mods == 0 {
                            // 无修饰键: 继续等待(纯单键易与日常输入冲突)
                            break;
                        }
                        set_capture(false);
                        let mut c = crate::cfg_clone();
                        c.hotkey_mod = mods;
                        c.hotkey_vk = vk;
                        crate::cfg_apply(c);
                        let okh = crate::hotkey_apply(h, mods, vk);
                        crate::log_line(&format!(
                            "settings: hotkey set mod=0x{:X} vk=0x{:X} ok={}",
                            mods, vk, okh
                        ));
                        let _ = InvalidateRect(h, None, false);
                        break;
                    }
                }
            }
        }
        // 定时时间框回显: 托盘菜单改了时间后, 已打开的面板要跟着变
        if !EDITING_T && (!H_EDIT_ON.0.is_null() || !H_EDIT_OFF.0.is_null()) {
            let c = crate::cfg_clone();
            for (hw, want) in [
                (H_EDIT_ON, format!("{:02}:{:02}", c.sched_on_h, c.sched_on_m)),
                (H_EDIT_OFF, format!("{:02}:{:02}", c.sched_off_h, c.sched_off_m)),
            ] {
                if hw.0.is_null() {
                    continue;
                }
                let mut buf = [0u16; 16];
                let n = GetWindowTextW(hw, &mut buf) as usize;
                let cur = String::from_utf16_lossy(&buf[..n]);
                if cur != want {
                    EDITING_T = true;
                    let _ = SetWindowTextW(hw, PCWSTR(wide(&want).as_ptr()));
                    EDITING_T = false;
                }
            }
        }
        // 输入框与 cfg 双向同步(滑块/轮询改动 16ms 内回显)
        if !H_EDIT.0.is_null() && !EDITING {
            let s = format!("{}", crate::cfg_clone().delay_ms);
            let mut buf = [0u16; 16];
            let n = GetWindowTextW(H_EDIT, &mut buf) as usize;
            let cur = String::from_utf16_lossy(&buf[..n]);
            if cur != s {
                EDITING = true;
                let _ = SetWindowTextW(H_EDIT, PCWSTR(wide(&s).as_ptr()));
                EDITING = false;
            }
        }
        if capt_release_if_up() {
            crate::log_line("settings: drag end (timer safety valve)");
        }
        let raw_down = (GetAsyncKeyState(0x01 /*VK_LBUTTON*/) as u16 & 0x8000) != 0;
        // 按下/抬起消抖: 本环境(远程桌面/注入输入)GetAsyncKeyState 的按键状态会
        // 瞬时抖动 —— 实测 drag begin 后下一 tick 就读到松开, 拖动/边框缩放/滑块
        // 被立即判定为抬起而中断。按下立即生效, 抬起需连续 3 tick(48ms) 确认。
        static mut DOWN_STABLE: bool = false;
        static REL_TICKS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if raw_down {
            DOWN_STABLE = true;
            REL_TICKS.store(0, std::sync::atomic::Ordering::Relaxed);
        } else if DOWN_STABLE {
            let n = REL_TICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n >= 3 {
                DOWN_STABLE = false;
            }
        }
        let down = DOWN_STABLE;
        let prev = LB_PREV.swap(down, std::sync::atomic::Ordering::Relaxed);
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        let mut wr = RECT::default();
        let _ = GetWindowRect(h, &mut wr);
        let rx = p.x - wr.left;
        let ry = p.y - wr.top;
        let inside = rx >= 0 && ry >= 0 && rx < wr.right - wr.left && ry < wr.bottom - wr.top;
        // 边框带判定(与 NCHITTEST 同几何): 本环境侧边/底部的 NCHITTEST/SETCURSOR/
        // NCLBUTTONDOWN 真实输入不投递, 光标与按下检测由本轮询兜底
        let bw = (sc(6)).max(4);
        let ww = wr.right - wr.left;
        let wh = wr.bottom - wr.top;
        let zn_l = inside && rx < bw;
        let zn_r = inside && rx >= ww - bw;
        let zn_t = inside && ry < bw;
        let zn_b = inside && ry >= wh - bw;
        let edge_now: u32 = if zn_t && zn_l { 13 }
            else if zn_t && zn_r { 14 }
            else if zn_b && zn_l { 16 }
            else if zn_b && zn_r { 17 }
            else if zn_b { 15 }
            else if zn_l { 10 }
            else if zn_r { 11 }
            else if zn_t && ry < bar_h() && rx < ABOUT_R.left { 12 }
            else { 0 };
        let at_border = edge_now >= 10 && edge_now <= 17;
        // 自绘标题栏拖动(无边框后标题栏属于客户区, 本环境客户区消息不可靠 → 轮询兜底)
        let in_tb_btn = inside && ry >= 0 && ry < bar_h() && rx >= ABOUT_R.left;
        let zone_caption = inside && !at_border && !in_tb_btn && ry >= 0 && ry < bar_h();
        // 标题栏按钮悬停(轮询更新, 变化才重绘)
        {
            let tb: u8 = if inside && ry >= 0 && ry < bar_h() {
                if rx >= CLOSE_R.left && rx < CLOSE_R.right { 3 }
                else if rx >= MIN_R.left && rx < MIN_R.right { 2 }
                else if rx >= ABOUT_R.left && rx < ABOUT_R.right { 1 }
                else { 0 }
            } else { 0 };
            if tb != HOT_TB {
                HOT_TB = tb;
                let _ = InvalidateRect(h, None, false);
            }
        }
        // 点击按下沿: 标题栏按钮(关于/最小化/关闭)必须放行 —— 它们位于顶栏
        // (ry < bar_h), 旧门槛 ry > sc(44) 会把按钮点击全部挡掉(实测点击无效)。
        // zone_caption(非按钮栏区)已排除, 不与拖动冲突。
        if down && !prev && inside {
            crate::log_line(&format!(
                "settings: poll press rx={} ry={} ww={} tb_btn={} zc={} border={} edge={}",
                rx, ry, ww, in_tb_btn, zone_caption, at_border, edge_now
            ));
        }
        if down && !prev && inside && !at_border && !zone_caption && (ry > sc(44) || in_tb_btn) {
            handle_click(h, p.x, p.y);
        }
        // 光标自管(轮询): 边框带显示缩放光标, 客户区按悬停位置给手/箭头/工字
        if inside && !RESIZE {
            let idc = if at_border {
                match edge_now {
                    10 | 11 => IDC_SIZEWE,
                    12 | 15 => IDC_SIZENS,
                    13 | 17 => IDC_SIZENWSE,
                    _ => IDC_SIZENESW,
                }
            } else if ry >= sc(44) || rx >= ww - sc(150) {
                let mut pt = POINT { x: p.x, y: p.y };
                let _ = ScreenToClient(h, &mut pt);
                let over_edit = !H_EDIT.0.is_null()
                    && pt.x >= EDIT_R.left && pt.x < EDIT_R.right
                    && pt.y >= EDIT_R.top && pt.y < EDIT_R.bottom;
                if over_edit {
                    IDC_IBEAM
                } else {
                    let (rowi, id) = hit(pt);
                    if rowi >= 0 || rowi == -2 || (rowi == -3 && id == IDC_HOTKEY) { IDC_HAND } else { IDC_ARROW }
                }
            } else {
                IDC_ARROW
            };
            {
                static LAST_IDC: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                let key = idc.0 as usize;
                if LAST_IDC.swap(key, std::sync::atomic::Ordering::Relaxed) != key {
                    crate::log_line(&format!(
                        "settings: poll cursor edge={} rx={} ry={} ww={} idc=0x{:X}",
                        edge_now, rx, ry, ww, key
                    ));
                }
            }
            let cur = LoadCursorW(None, idc).unwrap_or_default();
            let _ = SetCursor(cur);
        }
        // 边框按下沿(轮询兜底): 真实机器上 NCLBUTTONDOWN 先到, 这里幂等
        if at_border && down && !BORDER_DOWN_PREV && !RESIZE {
            begin_resize(h, edge_now, "poll");
        }
        BORDER_DOWN_PREV = at_border && down;
        // 标题栏按下沿 → 开始拖动(与 NCLBUTTONDOWN(HTCAPTION) 路径幂等)
        if zone_caption && down && !prev && !CAPT_DRAG {
            CAPT_START = p;
            CAPT_WIN = wr;
            CAPT_DRAG = true;
            CAPT_MOVED = false;
            let _ = SetCapture(h);
            crate::log_line("settings: drag begin (poll)");
        }
        // 标题栏拖动跟随(轮询驱动, 不依赖可能丢失的鼠标移动消息)
        if CAPT_DRAG && down {
            let dx = p.x - CAPT_START.x;
            let dy = p.y - CAPT_START.y;
            if dx != 0 || dy != 0 {
                static DRAG_LOG: std::sync::atomic::AtomicU32 =
                    std::sync::atomic::AtomicU32::new(0);
                let n = DRAG_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 8 {
                    crate::log_line(&format!(
                        "settings: drag follow dx={} dy={} win=({},{})",
                        dx, dy, CAPT_WIN.left, CAPT_WIN.top
                    ));
                }
                CAPT_WIN = RECT {
                    left: CAPT_WIN.left + dx, top: CAPT_WIN.top + dy,
                    right: CAPT_WIN.right + dx, bottom: CAPT_WIN.bottom + dy,
                };
                CAPT_START = p;
                let _ = SetWindowPos(
                    h, None, CAPT_WIN.left, CAPT_WIN.top, 0, 0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
        // 滑块按住跟随
        if DRAG {
            if down && inside {
                let mut pt = POINT { x: p.x, y: p.y };
                let _ = ScreenToClient(h, &mut pt);
                let x0 = SLIDER_R.left + sc(10);
                let x1 = SLIDER_R.right - sc(10);
                let f = ((pt.x - x0) as f64 / (x1 - x0) as f64).clamp(0.0, 1.0);
                let mut c = crate::cfg_clone();
                c.delay_ms = (500.0 * f) as u32;
                crate::cfg_apply(c);
                let _ = InvalidateRect(h, None, false);
            } else if !down {
                DRAG = false;
                let _ = InvalidateRect(h, None, false);
            }
        }
        // 边框缩放跟随(定时器驱动, 不依赖可能丢失的鼠标消息; 松开即停)
        if RESIZE {
            // 结束条件: 按键已松开 **且** 光标连续 3 tick 没再移动。
            // 只靠按键状态会误判: 远程/注入输入下 GetAsyncKeyState 的按住状态
            // 维持很短, 实测 resize begin 后立刻 resize end, 缩放手势被掐断。
            static mut R_LAST: POINT = POINT { x: 0, y: 0 };
            static R_IDLE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if R_LAST.x != p.x || R_LAST.y != p.y {
                R_LAST = p;
                R_IDLE.store(0, std::sync::atomic::Ordering::Relaxed);
            }
            let idle = R_IDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if !down && idle >= 3 {
                RESIZE = false;
                let _ = ReleaseCapture();
                crate::log_line("settings: resize end (timer)");
            } else {
                let dx = p.x - R_START.x;
                let dy = p.y - R_START.y;
                let mut nr = R_WIN;
                // 与 WM_GETMINMAXINFO 同一最小尺寸(物理像素, 只乘 DPI 不乘 UI_SCALE)
                let min_w = (590.0 * S) as i32;
                let min_h = (470.0 * S) as i32;
                if matches!(R_EDGE, 13 | 10 | 16) {
                    nr.left = (R_WIN.left + dx).min(R_WIN.right - min_w);
                }
                if matches!(R_EDGE, 14 | 11 | 17) {
                    nr.right = (R_WIN.right + dx).max(R_WIN.left + min_w);
                }
                if matches!(R_EDGE, 13 | 12 | 14) {
                    nr.top = (R_WIN.top + dy).min(R_WIN.bottom - min_h);
                }
                if matches!(R_EDGE, 16 | 15 | 17) {
                    nr.bottom = (R_WIN.bottom + dy).max(R_WIN.top + min_h);
                }
                let _ = SetWindowPos(
                    h, None,
                    nr.left, nr.top,
                    nr.right - nr.left, nr.bottom - nr.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
        // 客户区尺寸轮询兜底: 本环境 WM_SIZE 不投递, 靠 16ms 比对发现尺寸变化。
        // 最小化时客户区是 199x34 的占位值, 绝不能据此重排 ——
        // 否则 relayout 的 SetWindowPos 会把窗口强缩成小窗, 之后 SW_RESTORE 也救不回
        // (实测: 最小化后三种恢复方式全部失败, 窗口缩成 159x27, 日志 layout cw=199 实锤)。
        if !IsIconic(h).as_bool() {
            let mut crc2 = RECT::default();
            let _ = GetClientRect(h, &mut crc2);
            let cw2 = crc2.right - crc2.left;
            let ch2 = crc2.bottom - crc2.top;
            if cw2 != LAST_CW || ch2 != LAST_CH {
                LAST_CW = cw2;
                LAST_CH = ch2;
                relayout(h);
            }
        }
        return LRESULT(0);
    }
    match m {
        WM_ERASEBKGND => {
            if PLAIN_MODE {
                DefWindowProcW(h, m, w, l) // Python 版同款: 系统用实刷擦背景
            } else {
                LRESULT(1) // 全量自绘, 防闪烁
            }
        }
        WM_PAINT => {
            paint(h);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let pt = POINT {
                x: (l.0 & 0xFFFF) as i16 as i32,
                y: ((l.0 >> 16) & 0xFFFF) as i16 as i32,
            };
            // 自实现拖动中: 先安全阀检查(左键已松开则立即结束, 防绑定光标),
            // 再用屏幕光标位置移动窗口; 4px 死区防误判
            if CAPT_DRAG {
                if capt_release_if_up() {
                    crate::log_line("settings: drag end (safety valve)");
                    return LRESULT(0);
                }
                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                let dx = p.x - CAPT_START.x;
                let dy = p.y - CAPT_START.y;
                if dx.abs() > 4 || dy.abs() > 4 {
                    if !CAPT_MOVED {
                        CAPT_MOVED = true;
                        crate::log_line("settings: drag move");
                    }
                    let _ = SetWindowPos(
                        h, None,
                        CAPT_WIN.left + dx,
                        CAPT_WIN.top + dy,
                        0, 0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                return LRESULT(0);
            }
            update_hot(h, pt);
            if DRAG {
                let x0 = SLIDER_R.left + sc(10);
                let x1 = SLIDER_R.right - sc(10);
                let f = ((pt.x - x0) as f64 / (x1 - x0) as f64).clamp(0.0, 1.0);
                let mut c = crate::cfg_clone();
                c.delay_ms = (500.0 * f) as u32;
                crate::cfg_apply(c);
                let _ = InvalidateRect(h, None, false);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            // 可调整大小: 内部自适应重排(本环境该消息不投递, 由 WM_TIMER 轮询兜底)。
            // 最小化(SIZE_MINIMIZED)时客户区是 199x34 占位值 —— 此时 relayout 的
            // SetWindowPos 会破坏最小化几何, 之后 SW_RESTORE 永远失败(实测根因):
            // 最小化→打不开。必须跳过, 恢复时 WM_SIZE(SIZE_RESTORED) 自然重排。
            if !PLAIN_MODE && !IsIconic(h).as_bool() {
                crate::log_line(&format!("settings: wm_size w={} l=0x{:X}", w.0, l.0 as usize));
                relayout(h);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            // 消抖: 系统偶发假 LEAVE(重绘/capture 期间) —— 光标实际仍在窗口内
            // 则忽略, 否则 hover 高亮会在"进入/离开"间高频震荡闪烁
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            let mut wr = RECT::default();
            let _ = GetWindowRect(h, &mut wr);
            let inside = p.x >= wr.left && p.x < wr.right && p.y >= wr.top && p.y < wr.bottom;
            TRACKING = false;
            if inside {
                return LRESULT(0); // 假 LEAVE, 保留当前 hover
            }
            HOT_ROW = -1;
            HOT_BTN = false;
            let _ = InvalidateRect(h, None, false);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let pt = POINT {
                x: (l.0 & 0xFFFF) as i16 as i32,
                y: ((l.0 >> 16) & 0xFFFF) as i16 as i32,
            };
            // 顶栏(自绘标题栏按钮区)点击 → 统一走 handle_click 的栏区分支
            if pt.y < bar_h() {
                let mut sp = POINT::default();
                let _ = GetCursorPos(&mut sp);
                handle_click(h, sp.x, sp.y);
                return LRESULT(0);
            }
            let _ = SetCapture(h);
            let (rowi, id) = hit(pt);
            // 诊断: 实机"无法点击"时回传日志定位(命中行/命中控件/坐标)
            crate::log_line(&format!("settings: click pt=({},{}) hit={},{id}", pt.x, pt.y, rowi));
            if rowi == -3 && id == IDC_HOTKEY {
                set_capture(!HK_CAPTURE);
                if HK_CAPTURE {
                    crate::hotkey_unregister(h);
                } else {
                    let c = crate::cfg_clone();
                    crate::hotkey_apply(h, c.hotkey_mod, c.hotkey_vk);
                }
                let _ = InvalidateRect(h, None, false);
            } else if rowi == -2 {
                DRAG = true;
                let x0 = SLIDER_R.left + sc(10);
                let x1 = SLIDER_R.right - sc(10);
                let f = ((pt.x - x0) as f64 / (x1 - x0) as f64).clamp(0.0, 1.0);
                let mut c = crate::cfg_clone();
                c.delay_ms = (500.0 * f) as u32;
                crate::cfg_apply(c);
            } else if rowi >= 0 {
                let kind = ROWS[rowi as usize].kind;
                if kind == 1 && id != 0 {
                    apply_toggle(id);
                } else if kind == 2 && id != 0 {
                    apply_radio(id);
                } else if kind == 3 && id == IDC_MEASURE {
                    MEASURE.clear();
                    crate::run_measure();
                }
            }
            let _ = InvalidateRect(h, None, false);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if CAPT_DRAG {
                CAPT_DRAG = false;
                CAPT_MOVED = false;
                let _ = ReleaseCapture();
                crate::log_line("settings: drag end");
                return LRESULT(0);
            }
            crate::log_line("settings: lbuttonup");
            DRAG = false;
            let _ = ReleaseCapture();
            LRESULT(0)
        }
        WM_SETCURSOR => {
            {
                use std::sync::atomic::{AtomicI32, Ordering};
                static LAST_SC: AtomicI32 = AtomicI32::new(-999);
                let code = (l.0 & 0xFFFF) as i32;
                if LAST_SC.swap(code, Ordering::Relaxed) != code {
                    crate::log_line(&format!("settings: setcursor code={}", code));
                }
            }
            if (l.0 & 0xFFFF) as u16 == 1 {
                // HTCLIENT
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(h, &mut pt);
                let (rowi, hid) = hit(pt);
                let hand = rowi >= 0 || rowi == -2 || (rowi == -3 && hid == IDC_HOTKEY);
                let cur = LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }).unwrap_or_default();
                let _ = SetCursor(cur);
                return LRESULT(1);
            }
            DefWindowProcW(h, m, w, l)
        }
        WM_GETMINMAXINFO => {
            // 系统级最小尺寸: 最宽约束行是"亮屏后光标"(标题+描述 ~170px + 三组单选 ~470px
            // + 内边距), 实测低于 ~720 物理像素时单选组与标题重叠(用户截图实锤)。
            // 590 逻辑宽 / 560 逻辑高(×DPI, 不乘 UI_SCALE —— 窗口外框不随内容缩放)。
            let mmi = l.0 as *mut MINMAXINFO;
            if !mmi.is_null() {
                // 高度放宽到 470 逻辑(压缩留白+等比兜底在该高度内不破碎),
                // 宽度维持 590(单选组与标题不重叠的下限)
                (*mmi).ptMinTrackSize = POINT { x: (590.0 * S) as i32, y: (470.0 * S) as i32 };
                return LRESULT(0);
            }
            DefWindowProcW(h, m, w, l)
        }
        WM_SYSCOMMAND => {
            const SC_MINIMIZE: usize = 0xF020;
            const SC_CLOSE: usize = 0xF060;
            // 拦截系统最小化(任务栏右键等入口): 本环境最小化后无法还原, 改为收起
            if (w.0 & 0xFFF0) == SC_MINIMIZE {
                let _ = ShowWindow(h, SW_HIDE);
                crate::tray_balloon("已收起到托盘", "点击托盘图标或按热键可重新打开设置面板");
                return LRESULT(0);
            }
            // 系统关闭(右键关闭/Alt+F4): 与自绘✕一致
            if (w.0 & 0xFFF0) == SC_CLOSE {
                do_close(h);
                return LRESULT(0);
            }
            DefWindowProcW(h, m, w, l)
        }
        WM_CTLCOLOREDIT => {
            // 输入框扁平化: 卡片底色 + 深色文字(去掉系统白底黑框的生硬感),
            // 外描边由 paint 的圆角矩形负责
            static BRUSH: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
            let hb = BRUSH.get_or_init(|| CreateSolidBrush(CARD).0 as usize);
            let hdc = HDC(w.0 as *mut core::ffi::c_void);
            let _ = SetBkColor(hdc, CARD);
            let _ = SetTextColor(hdc, LABEL);
            LRESULT(*hb as isize)
        }
        WM_COMMAND => {
            let id = (w.0 & 0xFFFF) as u16;
            let code = ((w.0 >> 16) & 0xFFFF) as u16;
            // 定时时间输入: EN_CHANGE 时解析并立即生效(解析失败静默忽略, 允许边输)
            if (id == IDC_TIME_ON || id == IDC_TIME_OFF) && code == 0x0300 && !EDITING_T {
                let hw = if id == IDC_TIME_ON { H_EDIT_ON } else { H_EDIT_OFF };
                let mut buf = [0u16; 16];
                let n = GetWindowTextW(hw, &mut buf) as usize;
                let txt = String::from_utf16_lossy(&buf[..n]);
                if let Some((th, tm)) = parse_hm(&txt) {
                    let mut c = crate::cfg_clone();
                    let changed = if id == IDC_TIME_ON {
                        let ch = c.sched_on_h != th || c.sched_on_m != tm;
                        c.sched_on_h = th;
                        c.sched_on_m = tm;
                        ch
                    } else {
                        let ch = c.sched_off_h != th || c.sched_off_m != tm;
                        c.sched_off_h = th;
                        c.sched_off_m = tm;
                        ch
                    };
                    if changed {
                        crate::cfg_apply(c);
                        crate::log_line(&format!("settings: sched time {} = {:02}:{:02}", id, th, tm));
                        let _ = InvalidateRect(h, None, false);
                    }
                }
            }
            if id == IDC_DELAY_EDIT && code == 0x0300 && !EDITING {
                // EN_CHANGE: 用户输入 → 解析 → 应用(0..500 夹取) → 重绘滑块
                EDITING = true;
                let mut buf = [0u16; 16];
                let n = GetWindowTextW(H_EDIT, &mut buf) as usize;
                let ms: u32 = String::from_utf16_lossy(&buf[..n])
                    .trim()
                    .parse()
                    .unwrap_or(crate::cfg_clone().delay_ms);
                let mut c = crate::cfg_clone();
                let ms = ms.clamp(0, 500);
                if c.delay_ms != ms {
                    c.delay_ms = ms;
                    crate::cfg_apply(c);
                    let _ = InvalidateRect(h, None, false);
                }
                EDITING = false;
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if w.0 as u32 == 27 {
                // ESC
                let _ = DestroyWindow(h);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // 兜底: 自绘✕与 SC_CLOSE 已直接走 do_close, 但外部通过
            // PostMessage(WM_CLOSE) 也应生效(本环境该消息派发不稳定, 尽人事)
            do_close(h);
            LRESULT(0)
        }
        WM_DESTROY => {
            crate::log_line("settings: wm_destroy");
            // 捕获态随窗口销毁一起清掉: 否则 CAPTURING 残留会让全局热键永久失效
            if HK_CAPTURE {
                set_capture(false);
                let c = crate::cfg_clone();
                if !REBUILD {
                    let _ = crate::hotkey_apply(h, c.hotkey_mod, c.hotkey_vk);
                }
            }
            ROWS.clear();
            CARDS.clear();
            HEADERS.clear();
            H_EDIT = HWND(core::ptr::null_mut());
            H_EDIT_ON = HWND(core::ptr::null_mut());
            H_EDIT_OFF = HWND(core::ptr::null_mut());
            WND = HWND(core::ptr::null_mut());
            // 窗口销毁 = 用户选择"退出程序"(close_action=1): 结束消息循环退出进程。
            // 缺这行时窗口消失但进程驻留, 单实例互斥会挡住后续启动
            // (实测: 关闭后再启动报 "already running, exit", 程序彻底打不开)。
            // REBUILD 为真时是内部重建(iconic 卡死急救), 不能退出进程。
            if !REBUILD {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(h, m, w, l),
    }
}
