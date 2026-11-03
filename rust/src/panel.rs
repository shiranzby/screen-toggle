//! Window callbacks post intents; painting and child creation never save settings.
use crate::{app, theme, win::*};
use sog::config::*;
use std::{
    cell::{Cell, RefCell},
    mem::size_of,
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        UI::{
            Controls::*,
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{
                EnableWindow, GetFocus, IsWindowEnabled, SetFocus, TrackMouseEvent, TME_LEAVE,
                TRACKMOUSEEVENT, VK_END, VK_HOME, VK_NEXT, VK_PRIOR,
            },
            Shell::{DefSubclassProc, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
};

pub const HOTKEY: i32 = 2001;
pub const EXT: i32 = 2002;
pub const INT: i32 = 2003;
pub const HDR: i32 = 2004;
pub const BLACK: i32 = 2011;
pub const DIM: i32 = 2012;
pub const FADE: i32 = 2013;
pub const READY: i32 = 2021;
pub const DELAY_MODE: i32 = 2022;
pub const DELAY: i32 = 2031;
pub const SCHEDULE: i32 = 2041;
pub const DARK_TIME: i32 = 2042;
pub const LIGHT_TIME: i32 = 2043;
pub const AUTOSTART: i32 = 2051;
pub const CLOSE_HIDE: i32 = 2061;
pub const CLOSE_EXIT: i32 = 2062;
pub const MEASURE: i32 = 2071;
pub const CUR_AUTO: i32 = 2081;
pub const CUR_HIDE: i32 = 2082;
pub const CUR_KEEP: i32 = 2083;
pub const SLIDER: i32 = 2091;
pub const ABOUT: i32 = 2092;
pub const MINIMIZE: i32 = 2093;
pub const CLOSE: i32 = 2094;
pub const TOGGLE: i32 = 2095;
pub const STATUS: i32 = 2096;
pub const RETRY: i32 = 2097;
pub const MAXIMIZE: i32 = 2098;
pub const THEME_LIGHT: i32 = 2111;
pub const THEME_DARK: i32 = 2112;
pub const THEME_SYSTEM: i32 = 2113;
pub const DESKTOP_ENABLED: i32 = 2131;
pub const DESKTOP_HOTKEY: i32 = 2132;
pub const DESKTOP_DOUBLE: i32 = 2133;
pub const HOTKEY_DOUBLE: i32 = 2136;
pub const SCREENS: i32 = 2141;

const BODY_H: i32 = 116;
const FOOTER_H: i32 = 40;
// commctrl.h aliases TBM_GETPOS to WM_USER; windows 0.58 omits the alias.
const TBM_GETPOS: u32 = WM_USER;

#[derive(Clone)]
struct Row {
    title: &'static str,
    desc: &'static str,
    group: usize,
    kind: RowKind,
}
#[derive(Clone)]
enum RowKind {
    Toggle(i32),
    TogglePair([(i32, &'static str, &'static str); 2]),
    Choice(Vec<(i32, &'static str, i32)>),
    Hotkey(i32),
    Action(i32),
    Delay,
    Time(i32),
}
#[derive(Clone)]
struct Placement {
    row: Row,
    y: i32,
    height: i32,
}
type Position = (HWND, i32, i32, i32, i32);
unsafe fn window_rect_in(hwnd: HWND, parent: HWND) -> RECT {
    let mut area = RECT::default();
    let _ = GetWindowRect(hwnd, &mut area);
    let mut origin = POINT {
        x: area.left,
        y: area.top,
    };
    let _ = ScreenToClient(parent, &mut origin);
    rect(
        origin.x,
        origin.y,
        area.right - area.left,
        area.bottom - area.top,
    )
}
unsafe fn validate_surface(p: &Panel, hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let _ = BeginPaint(hwnd, &mut ps);
    let _ = EndPaint(hwnd, &ps);
    if ps.rcPaint.right > ps.rcPaint.left && ps.rcPaint.bottom > ps.rcPaint.top {
        p.invalidate_window(hwnd);
    }
}
/// Native HWNDs own input geometry. The root owns their visual pixels, so do
/// not let USER copy or paint intermediate child positions during a batch.
unsafe fn move_batch(positions: &[Position]) -> bool {
    let changed: Vec<_> = positions
        .iter()
        .filter_map(|&(h, x, y, width, height)| {
            let parent = GetParent(h).ok()?;
            let mut old = RECT::default();
            let _ = GetWindowRect(h, &mut old);
            let mut origin = POINT {
                x: old.left,
                y: old.top,
            };
            let _ = ScreenToClient(parent, &mut origin);
            let old = rect(
                origin.x,
                origin.y,
                old.right - old.left,
                old.bottom - old.top,
            );
            let new = rect(x, y, width, height);
            (old != new).then_some((h, parent, old, new))
        })
        .collect();
    if changed.is_empty() {
        return false;
    }
    let flags = SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOREDRAW | SWP_NOCOPYBITS;
    let mut batch = BeginDeferWindowPos(changed.len() as i32).ok();
    for &(h, _, _, r) in &changed {
        if let Some(current) = batch {
            batch = DeferWindowPos(
                current,
                h,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                flags,
            )
            .ok();
        }
    }
    if !batch.is_some_and(|batch| EndDeferWindowPos(batch).is_ok()) {
        for &(h, _, _, r) in &changed {
            let _ = SetWindowPos(
                h,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                flags,
            );
        }
    }
    true
}

/// Compose each update off-screen and publish it once. Keep native input and
/// the target DC's child/sibling clipping intact.
struct Buffer {
    handle: isize,
    dc: HDC,
}
impl Buffer {
    unsafe fn begin(target: HDC, area: &RECT) -> Self {
        let mut dc = HDC::default();
        let handle = BeginBufferedPaint(target, area, BPBF_COMPATIBLEBITMAP, None, &mut dc);
        Self {
            handle,
            dc: if handle == 0 { target } else { dc },
        }
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe {
                let _ = EndBufferedPaint(self.handle, true);
            }
        }
    }
}
struct Paint {
    hwnd: HWND,
    ps: PAINTSTRUCT,
    buffer: Option<Buffer>,
    dc: HDC,
    caret: Option<HWND>,
}
impl Paint {
    unsafe fn begin(p: &Panel) -> Self {
        let hwnd = p.root.get();
        let mut ps = PAINTSTRUCT::default();
        let target = BeginPaint(hwnd, &mut ps);
        // Native EDIT clients retain their own text/selection/IME/caret paint.
        // Every other pixel, including edit frames, belongs to this buffer.
        let focus = GetFocus();
        let caret = (p.sizing.get()
            && [DELAY, DARK_TIME, LIGHT_TIME].contains(&GetDlgCtrlID(focus))
            && GetParent(focus).ok() == Some(p.body.get())
            && HideCaret(focus).is_ok())
        .then_some(focus);
        for id in [DELAY, DARK_TIME, LIGHT_TIME] {
            let edit = p.control(id);
            if !p.sizing.get() && IsWindowVisible(edit).as_bool() {
                let mut client = RECT::default();
                let _ = GetClientRect(edit, &mut client);
                let mut origin = POINT::default();
                let _ = ClientToScreen(edit, &mut origin);
                let _ = ScreenToClient(hwnd, &mut origin);
                let client = rect(origin.x, origin.y, client.right, client.bottom);
                let body = window_rect_in(p.body.get(), hwnd);
                let mut visible = RECT::default();
                if IntersectRect(&mut visible, &client, &body).as_bool() {
                    ExcludeClipRect(
                        target,
                        visible.left,
                        visible.top,
                        visible.right,
                        visible.bottom,
                    );
                }
            }
        }
        let buffer = Buffer::begin(target, &ps.rcPaint);
        let dc = buffer.dc;
        Self {
            hwnd,
            ps,
            buffer: Some(buffer),
            dc,
            caret,
        }
    }
}
impl Drop for Paint {
    fn drop(&mut self) {
        unsafe {
            drop(self.buffer.take());
            let _ = EndPaint(self.hwnd, &self.ps);
            if let Some(caret) = self.caret {
                let _ = ShowCaret(caret);
            }
        }
    }
}
struct Panel {
    owner: HWND,
    root: Cell<HWND>,
    body: Cell<HWND>,
    status: Cell<HWND>,
    syncing: Cell<bool>,
    ready: Cell<bool>,
    sizing: Cell<bool>,
    dpi: Cell<u32>,
    scroll: Cell<i32>,
    total: Cell<i32>,
    layout_bounds: Cell<(i32, i32, i32, u32)>,
    fonts: RefCell<Vec<HFONT>>,
    controls: RefCell<Vec<(i32, HWND)>>,
    placements: RefCell<Vec<Placement>>,
    groups: RefCell<Vec<(&'static str, i32, i32, i32)>>,
    surface: Cell<HBRUSH>,
    background: Cell<HBRUSH>,
    colors: Cell<theme::Palette>,
    theme: Cell<Theme>,
    system_dark: Cell<bool>,
    capturing: Cell<i32>,
    test_mode: bool,
    buffered: bool,
    last_values: RefCell<Vec<(i32, String)>>,
    footer: Cell<i32>,
}
impl Drop for Panel {
    fn drop(&mut self) {
        unsafe {
            for f in self.fonts.get_mut().drain(..) {
                let _ = DeleteObject(f);
            }
            let _ = DeleteObject(self.surface.get());
            let _ = DeleteObject(self.background.get());
            if self.buffered {
                let _ = BufferedPaintUnInit();
            }
        }
    }
}
impl Panel {
    unsafe fn invalidate_window(&self, h: HWND) {
        if !self.ready.get() {
            return;
        }
        let mut area = window_rect_in(h, self.root.get());
        if GetParent(h).ok() == Some(self.body.get()) {
            let body = window_rect_in(self.body.get(), self.root.get());
            let mut clipped = RECT::default();
            if !IntersectRect(&mut clipped, &area, &body).as_bool() {
                return;
            }
            area = clipped;
        }
        let _ = RedrawWindow(
            self.root.get(),
            Some(&area),
            None,
            RDW_INVALIDATE | RDW_NOCHILDREN,
        );
    }
    unsafe fn apply_theme(&self, mode: Theme) {
        self.theme.set(mode);
        let colors = theme::palette(mode.is_dark(self.system_dark.get()));
        if self.colors.get().dark == colors.dark {
            return;
        }
        self.colors.set(colors);
        let background = self.background.replace(CreateSolidBrush(colors.bg));
        let surface = self.surface.replace(CreateSolidBrush(colors.surface));
        self.window_theme();
        // State/brushes are current before any Win32 call can synchronously paint.
        let _ = RedrawWindow(
            self.root.get(),
            None,
            None,
            RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN,
        );
        let _ = DeleteObject(background);
        let _ = DeleteObject(surface);
    }
    unsafe fn window_theme(&self) {
        let dark = BOOL::from(self.colors.get().dark);
        let _ = DwmSetWindowAttribute(
            self.root.get(),
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&dark as *const BOOL).cast(),
            size_of::<BOOL>() as u32,
        );
        let name = if self.colors.get().dark {
            w!("DarkMode_Explorer")
        } else {
            w!("Explorer")
        };
        let _ = SetWindowTheme(self.body.get(), name, PCWSTR::null());
        let controls = self.controls.borrow().clone();
        for (_, h) in controls {
            let _ = SetWindowTheme(h, name, PCWSTR::null());
        }
    }
    fn scale(&self, v: i32) -> i32 {
        ((v as i64 * self.dpi.get() as i64 + 48) / 96) as i32
    }
    fn control(&self, id: i32) -> HWND {
        self.controls
            .borrow()
            .iter()
            .find(|(key, _)| *key == id)
            .map(|(_, h)| *h)
            .unwrap_or_default()
    }
    unsafe fn fonts(&self) {
        let old = std::mem::take(&mut *self.fonts.borrow_mut());
        *self.fonts.borrow_mut() = vec![
            font(self.scale(15), 400),
            font(self.scale(13), 400),
            font(self.scale(28), 600),
            font(self.scale(15), 600),
            font(self.scale(13), 400),
        ];
        let controls = self.controls.borrow().clone();
        for (id, h) in controls {
            let current = self.fonts.borrow()[if id == STATUS || id == ABOUT { 1 } else { 0 }];
            SendMessageW(h, WM_SETFONT, WPARAM(current.0 as usize), LPARAM(1));
        }
        for f in old {
            let _ = DeleteObject(f);
        }
    }
    unsafe fn make(
        &self,
        parent: HWND,
        id: i32,
        class: PCWSTR,
        text: &str,
        style: u32,
    ) -> Result<HWND, String> {
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(wide(text).as_ptr()),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | style),
            0,
            0,
            10,
            10,
            parent,
            HMENU(id as usize as *mut _),
            instance(),
            None,
        )
        .map_err(|e| format!("创建控件 {id} 失败：{e}"))?;
        SendMessageW(
            h,
            WM_SETFONT,
            WPARAM(self.fonts.borrow()[0].0 as usize),
            LPARAM(0),
        );
        let _ = SetWindowTheme(h, w!("Explorer"), PCWSTR::null());
        self.controls.borrow_mut().push((id, h));
        let _ = SetWindowSubclass(h, Some(widget_proc), 2, self.root.get().0 as usize);
        Ok(h)
    }
    fn rows() -> Vec<Row> {
        vec![
            Row {
                title: "全局触发热键",
                desc: "点击后按键盘或鼠标按键；Esc 取消",
                group: 0,
                kind: RowKind::Hotkey(HOTKEY),
            },
            Row {
                title: "双击触发",
                desc: "开启后连续按两次；关闭时按一次",
                group: 0,
                kind: RowKind::Toggle(HOTKEY_DOUBLE),
            },
            Row {
                title: "启用桌面触发",
                desc: "不响应图标与窗口；酷呆接管时暂停",
                group: 1,
                kind: RowKind::Toggle(DESKTOP_ENABLED),
            },
            Row {
                title: "桌面触发热键",
                desc: "仅桌面空白处；支持键盘与鼠标",
                group: 1,
                kind: RowKind::Hotkey(DESKTOP_HOTKEY),
            },
            Row {
                title: "双击触发",
                desc: "独立设置；侧键可避开左键双击冲突",
                group: 1,
                kind: RowKind::Toggle(DESKTOP_DOUBLE),
            },
            Row {
                title: "面板主题",
                desc: "深色减少亮光；可跟随系统",
                group: 2,
                kind: RowKind::Choice(vec![
                    (THEME_LIGHT, "浅色", 88),
                    (THEME_DARK, "深色", 88),
                    (THEME_SYSTEM, "跟随系统", 116),
                ]),
            },
            Row {
                title: "熄屏时光标",
                desc: "亮屏时始终恢复指针",
                group: 3,
                kind: RowKind::Choice(vec![
                    (CUR_AUTO, "远控保留，其余隐藏", 176),
                    (CUR_HIDE, "始终隐藏", 104),
                    (CUR_KEEP, "始终保留", 104),
                ]),
            },
            Row {
                title: "切换屏幕",
                desc: "仅切换所选屏幕，选择自动保存",
                group: 3,
                kind: RowKind::Action(SCREENS),
            },
            Row {
                title: "",
                desc: "",
                group: 3,
                kind: RowKind::TogglePair([
                    (EXT, "压暗外接屏", "通过 DDC/CI 调节背光"),
                    (INT, "压暗内置屏", "通过 WMI 调节背光"),
                ]),
            },
            Row {
                title: "关闭 HDR",
                desc: "恢复时还原熄屏前的 HDR 状态",
                group: 3,
                kind: RowKind::Toggle(HDR),
            },
            Row {
                title: "熄屏顺序",
                desc: "黑窗与背光的执行顺序",
                group: 4,
                kind: RowKind::Choice(vec![
                    (BLACK, "先黑后熄", 126),
                    (DIM, "先熄后黑", 126),
                    (FADE, "淡入并行", 126),
                ]),
            },
            Row {
                title: "亮屏方式",
                desc: "写入成功先亮屏，后台回读确认",
                group: 4,
                kind: RowKind::Choice(vec![
                    (READY, "写入完成即撤窗", 190),
                    (DELAY_MODE, "固定延迟", 126),
                ]),
            },
            Row {
                title: "调光耗时",
                desc: "固定延迟时，恢复后额外保留黑窗；ms",
                group: 4,
                kind: RowKind::Delay,
            },
            Row {
                title: "定时熄屏",
                desc: "每天到点自动熄屏 / 亮屏",
                group: 5,
                kind: RowKind::Toggle(SCHEDULE),
            },
            Row {
                title: "熄屏时间",
                desc: "24 小时制，例如 23:00",
                group: 5,
                kind: RowKind::Time(DARK_TIME),
            },
            Row {
                title: "亮屏时间",
                desc: "手动操作后 10 分钟内定时让步",
                group: 5,
                kind: RowKind::Time(LIGHT_TIME),
            },
            Row {
                title: "开机静默启动",
                desc: "登录后驻留托盘；双击打开设置",
                group: 6,
                kind: RowKind::Toggle(AUTOSTART),
            },
            Row {
                title: "关闭窗口时",
                desc: "隐藏到托盘或恢复屏幕后退出",
                group: 6,
                kind: RowKind::Choice(vec![
                    (CLOSE_HIDE, "隐藏到托盘", 158),
                    (CLOSE_EXIT, "退出程序", 126),
                ]),
            },
        ]
    }
    unsafe fn populate(&self) -> Result<(), String> {
        self.fonts();
        for (id, text) in [
            (ABOUT, "关于"),
            (MINIMIZE, "最小化"),
            (MAXIMIZE, "最大化"),
            (CLOSE, "关闭"),
            (TOGGLE, "立即熄屏"),
            (RETRY, "重试保存"),
        ] {
            self.make(
                self.root.get(),
                id,
                w!("BUTTON"),
                text,
                BS_PUSHBUTTON as u32,
            )?;
        }
        let status = self.make(self.root.get(), STATUS, w!("STATIC"), "正在启动…", 0)?;
        self.status.set(status);
        let _ = ShowWindow(self.control(RETRY), SW_HIDE);
        for row in Self::rows() {
            match row.kind {
                RowKind::Hotkey(id) | RowKind::Action(id) => {
                    self.make(self.body.get(), id, w!("BUTTON"), "", BS_PUSHBUTTON as u32)?;
                }
                RowKind::TogglePair(items) => {
                    for (id, title, _) in items {
                        self.make(
                            self.body.get(),
                            id,
                            w!("BUTTON"),
                            title,
                            BS_AUTOCHECKBOX as u32,
                        )?;
                    }
                }
                RowKind::Toggle(id) => {
                    let h = self.make(
                        self.body.get(),
                        id,
                        w!("BUTTON"),
                        row.title,
                        BS_AUTOCHECKBOX as u32,
                    )?;
                    let _ = h;
                }
                RowKind::Choice(items) => {
                    for (n, (id, label, _)) in items.into_iter().enumerate() {
                        self.make(
                            self.body.get(),
                            id,
                            w!("BUTTON"),
                            label,
                            BS_AUTORADIOBUTTON as u32 | if n == 0 { WS_GROUP.0 } else { 0 },
                        )?;
                    }
                }
                RowKind::Time(id) => {
                    self.make(
                        self.body.get(),
                        id,
                        w!("EDIT"),
                        "",
                        ES_CENTER as u32 | ES_AUTOHSCROLL as u32,
                    )?;
                }
                RowKind::Delay => {
                    self.make(
                        self.body.get(),
                        MEASURE,
                        w!("BUTTON"),
                        "检测",
                        BS_PUSHBUTTON as u32,
                    )?;
                    self.make(
                        self.body.get(),
                        SLIDER,
                        TRACKBAR_CLASSW,
                        "",
                        TBS_HORZ | TBS_NOTICKS | TBS_FIXEDLENGTH,
                    )?;
                    SendMessageW(
                        self.control(SLIDER),
                        TBM_SETRANGE,
                        WPARAM(1),
                        LPARAM(500 << 16),
                    );
                    self.make(
                        self.body.get(),
                        DELAY,
                        w!("EDIT"),
                        "",
                        ES_CENTER as u32 | ES_NUMBER as u32,
                    )?;
                }
            }
        }
        self.ready.set(true);
        Ok(())
    }
    unsafe fn layout(&self) {
        if !self.ready.get() {
            return;
        }
        let mut cr = RECT::default();
        let _ = GetClientRect(self.root.get(), &mut cr);
        let s = |v| self.scale(v);
        let root_width = cr.right;
        let footer = self.footer.get();
        let previous =
            self.layout_bounds
                .replace((root_width, cr.bottom, s(footer), self.dpi.get()));
        let previous_scroll = self.scroll.get();
        let body_height = (cr.bottom - s(BODY_H + footer)).max(1);
        let mut roots = vec![(self.body.get(), 0, s(BODY_H), root_width, body_height)];
        for (id, x, width) in [
            (ABOUT, root_width - s(194), 56),
            (MINIMIZE, root_width - s(138), 46),
            (MAXIMIZE, root_width - s(92), 46),
            (CLOSE, root_width - s(46), 46),
        ] {
            roots.push((self.control(id), x, 0, s(width), s(40)));
        }
        roots.push((
            self.control(TOGGLE),
            root_width - s(160),
            s(62),
            s(132),
            s(36),
        ));
        roots.push((
            self.status.get(),
            s(28),
            cr.bottom - s(footer) + s(8),
            root_width - s(176),
            s(footer - 12),
        ));
        roots.push((
            self.control(RETRY),
            root_width - s(132),
            cr.bottom - s(footer) + s((footer - 32) / 2),
            s(104),
            s(32),
        ));
        let width = root_width * 96 / self.dpi.get() as i32;
        // Measuring must not acquire/release a window DC: with composition that
        // would publish the old frame before any child has reached its new spot.
        let dc = CreateCompatibleDC(None);
        let old_font = SelectObject(dc, self.fonts.borrow()[0]);
        let key_widths = [HOTKEY, DESKTOP_HOTKEY].map(|id| {
            let key_text: Vec<u16> = text(self.control(id)).encode_utf16().collect();
            let mut key_size = SIZE::default();
            let _ = GetTextExtentPoint32W(dc, &key_text, &mut key_size);
            (id, key_size.cx.saturating_add(s(32)).max(s(204)))
        });
        SelectObject(dc, old_font);
        let _ = DeleteDC(dc);
        let key_width = |id| key_widths.iter().find(|(key, _)| *key == id).unwrap().1;
        let mut y = 4;
        let mut placements = Vec::new();
        let mut groups = Vec::new();
        for (index, title) in [
            "全局触发",
            "桌面触发",
            "外观",
            "屏幕",
            "时序",
            "定时",
            "启动与关闭",
        ]
        .into_iter()
        .enumerate()
        {
            let header = y;
            y += 28;
            let top = y;
            for row in Self::rows().into_iter().filter(|row| row.group == index) {
                let stacked = match &row.kind {
                    RowKind::Choice(items) => items.iter().map(|i| i.2).sum::<i32>() > width - 316,
                    RowKind::Delay => width < 700,
                    RowKind::Hotkey(id) => key_width(*id) > s(width - 340),
                    _ => false,
                };
                let height = if stacked {
                    if let RowKind::Choice(items) = &row.kind {
                        let available = (width - 88).max(1);
                        let (mut lines, mut used) = (1, 0);
                        for (_, _, width) in items {
                            if used > 0 && used + width > available {
                                lines += 1;
                                used = 0;
                            }
                            used += width;
                        }
                        52 + lines * 40
                    } else {
                        92
                    }
                } else {
                    48
                };
                placements.push(Placement { row, y, height });
                y += height;
            }
            groups.push((title, header, top, y));
            y += 12;
        }
        self.total.set(s(y));
        let maximum = (self.total.get() - body_height).max(0);
        self.scroll.set(self.scroll.get().clamp(0, maximum));
        let scroll = self.scroll.get();
        let body_rect = rect(0, 0, root_width, body_height);
        let children = RefCell::new(Vec::new());
        let mut slider_center = None;
        for placement in &placements {
            let row_y = s(placement.y) - scroll;
            let h = s(placement.height);
            let right = body_rect.right - s(44);
            let cy = row_y + h / 2;
            let mv = |id, x, y, w, h| {
                children.borrow_mut().push((self.control(id), x, y, w, h));
            };
            match &placement.row.kind {
                RowKind::Toggle(id) => mv(*id, right - s(64), cy - s(22), s(64), s(44)),
                RowKind::TogglePair(items) => {
                    let start = s(44);
                    let half = (right - start) / 2;
                    for (index, (id, _, _)) in items.iter().enumerate() {
                        let edge = if index == 0 {
                            start + half - s(16)
                        } else {
                            right
                        };
                        mv(*id, edge - s(64), cy - s(22), s(64), s(44));
                    }
                }
                RowKind::Action(id) => mv(*id, right - s(176), cy - s(18), s(176), s(36)),
                RowKind::Hotkey(id) => {
                    let cy = if placement.height > 48 {
                        row_y + s(68)
                    } else {
                        cy
                    };
                    let key_width = key_width(*id).min(body_rect.right - s(88));
                    mv(*id, right - key_width, cy - s(18), key_width, s(36));
                }
                RowKind::Time(id) => mv(*id, right - s(126), cy - s(17), s(126), s(34)),
                RowKind::Choice(items) => {
                    let group_width = items.iter().map(|i| s(i.2)).sum::<i32>();
                    let left = s(44);
                    let wrap = group_width > right - left;
                    let mut x = if wrap { left } else { right - group_width };
                    let mut center = if placement.height > 48 {
                        row_y + s(68)
                    } else {
                        cy
                    };
                    for (id, _, w) in items {
                        if wrap && x > left && x + s(*w) > right {
                            x = left;
                            center += s(40);
                        }
                        mv(*id, x, center - s(20), s(*w), s(40));
                        x += s(*w);
                    }
                }
                RowKind::Delay => {
                    let cy = if placement.height > 48 {
                        row_y + s(68)
                    } else {
                        cy
                    };
                    let x = right - s(332);
                    mv(MEASURE, x, cy - s(18), s(64), s(36));
                    let mut channel = RECT::default();
                    SendMessageW(
                        self.control(SLIDER),
                        TBM_GETCHANNELRECT,
                        WPARAM(0),
                        LPARAM((&mut channel as *mut RECT) as isize),
                    );
                    mv(
                        SLIDER,
                        x + s(78),
                        cy - (channel.top + channel.bottom) / 2,
                        s(164),
                        s(36),
                    );
                    slider_center = Some((x + s(78), cy));
                    mv(DELAY, right - s(76), cy - s(17), s(76), s(34));
                }
            }
        }
        let reflow = *self.groups.borrow() != groups;
        *self.placements.borrow_mut() = placements;
        *self.groups.borrow_mut() = groups;
        // EndDeferWindowPos may synchronously request a parent/child paint.
        // Publish all drawing geometry before allowing that callback.
        let mut moved = move_batch(&roots);
        moved |= move_batch(&children.into_inner());
        if let Some((x, cy)) = slider_center {
            let slider = self.control(SLIDER);
            if SendMessageW(slider, TBM_GETTHUMBLENGTH, WPARAM(0), LPARAM(0)).0 != s(24) as isize {
                SendMessageW(
                    slider,
                    TBM_SETTHUMBLENGTH,
                    WPARAM(s(24) as usize),
                    LPARAM(0),
                );
            }
            // Query after the batch has updated the native channel geometry.
            let mut channel = RECT::default();
            SendMessageW(
                slider,
                TBM_GETCHANNELRECT,
                WPARAM(0),
                LPARAM((&mut channel as *mut RECT) as isize),
            );
            let y = cy - (channel.top + channel.bottom) / 2;
            moved |= move_batch(&[(slider, x, y, s(164), s(36))]);
        }
        if reflow
            || previous.0 != root_width
            || previous.3 != self.dpi.get()
            || previous_scroll != scroll
        {
            self.invalidate_window(self.body.get());
        }
        if previous.3 != self.dpi.get() {
            let _ = InvalidateRect(self.root.get(), None, false);
        } else if previous.1 != cr.bottom || previous.2 != s(footer) {
            // The footer rule belongs to the parent, outside its child controls.
            let top = (previous.1 - previous.2).min(cr.bottom - s(footer)) - s(2);
            let dirty = rect(0, top, root_width, cr.bottom - top);
            let _ = RedrawWindow(
                self.root.get(),
                Some(&dirty),
                None,
                RDW_INVALIDATE | RDW_NOCHILDREN,
            );
        }
        if moved {
            // Visual geometry was committed before any native call. One root
            // update presents all moved controls in that same geometry.
            let _ = RedrawWindow(self.root.get(), None, None, RDW_INVALIDATE | RDW_NOCHILDREN);
            for id in [DELAY, DARK_TIME, LIGHT_TIME] {
                let _ = InvalidateRect(self.control(id), None, false);
            }
        }
    }
    unsafe fn scroll_by(&self, position: i32) {
        let body = self.body.get();
        let mut client = RECT::default();
        let _ = GetClientRect(body, &mut client);
        let next = position.clamp(0, (self.total.get() - client.bottom).max(0));
        let delta = self.scroll.get() - next;
        if delta == 0 {
            return;
        }
        self.scroll.set(next);
        let controls = self.controls.borrow().clone();
        let mut positions = Vec::new();
        for (_, h) in controls {
            if GetParent(h).ok() != Some(body) {
                continue;
            }
            let mut area = RECT::default();
            let _ = GetWindowRect(h, &mut area);
            let mut origin = POINT {
                x: area.left,
                y: area.top,
            };
            let _ = ScreenToClient(body, &mut origin);
            positions.push((h, origin.x, origin.y + delta));
        }
        let flags = SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOSIZE | SWP_NOREDRAW | SWP_NOCOPYBITS;
        let mut batch = BeginDeferWindowPos(positions.len() as i32).ok();
        for &(h, x, y) in &positions {
            if let Some(current) = batch {
                batch = DeferWindowPos(current, h, None, x, y, 0, 0, flags).ok();
            }
        }
        if !batch.is_some_and(|batch| EndDeferWindowPos(batch).is_ok()) {
            for (h, x, y) in positions {
                let _ = SetWindowPos(h, None, x, y, 0, 0, flags);
            }
        }
        self.invalidate_window(body);
        for id in [DELAY, DARK_TIME, LIGHT_TIME] {
            let _ = InvalidateRect(self.control(id), None, false);
        }
    }
    unsafe fn draw_body(&self, dc: HDC, cr: RECT) {
        FillRect(dc, &cr, self.background.get());
        let fonts = self.fonts.borrow().clone();
        if fonts.len() < 4 {
            return;
        }
        let s = |v| self.scale(v);
        let scroll = self.scroll.get();
        let left = s(28);
        let right = cr.right - s(28);
        let groups = self.groups.borrow().clone();
        for (index, (title, header, top, bottom)) in groups.into_iter().enumerate() {
            group_icon(
                dc,
                index,
                left,
                s(header) + s(3) - scroll,
                s(18),
                self.colors.get().secondary,
            );
            label(
                dc,
                rect(
                    left + s(28),
                    s(header) - scroll,
                    right - left - s(28),
                    s(24),
                ),
                title,
                fonts[3],
                self.colors.get().ink,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
            rounded(
                dc,
                rect(left, s(top) - scroll, right - left, s(bottom - top)),
                s(10),
                self.colors.get().surface,
                self.colors.get().line,
            );
        }
        let placements = self.placements.borrow().clone();
        for p in &placements {
            let top = s(p.y) - scroll;
            let label_top = top + s(4);
            if top + s(p.height) < 0 || top > cr.bottom {
                continue;
            }
            if let RowKind::TogglePair(items) = &p.row.kind {
                let start = left + s(16);
                let end = right - s(16);
                let half = (end - start) / 2;
                for (index, (_, title, desc)) in items.iter().enumerate() {
                    let x = start + half * index as i32;
                    let width = half - s(if index == 0 { 96 } else { 80 });
                    label(
                        dc,
                        rect(x, label_top, width, s(20)),
                        title,
                        fonts[0],
                        self.colors.get().ink,
                        DT_LEFT | DT_SINGLELINE,
                    );
                    label(
                        dc,
                        rect(x, label_top + s(21), width, s(19)),
                        desc,
                        fonts[1],
                        self.colors.get().secondary,
                        DT_LEFT | DT_SINGLELINE,
                    );
                }
            } else {
                label(
                    dc,
                    rect(left + s(16), label_top, s(210), s(20)),
                    p.row.title,
                    fonts[0],
                    self.colors.get().ink,
                    DT_LEFT | DT_SINGLELINE,
                );
                label(
                    dc,
                    rect(left + s(16), label_top + s(21), s(228), s(19)),
                    p.row.desc,
                    fonts[1],
                    self.colors.get().secondary,
                    DT_LEFT | DT_SINGLELINE,
                );
            }
            if p.row.title != "全局热键"
                && p.row.title != "熄屏时光标"
                && p.row.title != "熄屏顺序"
                && p.row.title != "定时熄屏"
                && p.row.title != "开机静默启动"
            {
                let pen = CreatePen(PS_SOLID, 1, self.colors.get().line);
                let old = SelectObject(dc, pen);
                let _ = MoveToEx(dc, left + s(16), top, None);
                let _ = LineTo(dc, right - s(16), top);
                SelectObject(dc, old);
                let _ = DeleteObject(pen);
            }
        }
    }
    unsafe fn paint_root(&self, hwnd: HWND) {
        let paint = Paint::begin(self);
        let dc = paint.dc;
        let mut cr = RECT::default();
        let _ = GetClientRect(hwnd, &mut cr);
        FillRect(dc, &cr, self.background.get());
        let fonts = self.fonts.borrow().clone();
        let s = |v| self.scale(v);
        if fonts.len() >= 4 {
            let _ = DrawIconEx(
                dc,
                s(16),
                s(12),
                icon(s(16)),
                s(16),
                s(16),
                0,
                None,
                DI_NORMAL,
            );
            label(
                dc,
                rect(s(42), 0, s(300), s(40)),
                "一键熄屏",
                fonts[4],
                self.colors.get().ink,
                DT_SINGLELINE | DT_VCENTER,
            );
            label(
                dc,
                rect(s(28), s(54), s(300), s(38)),
                "设置",
                fonts[2],
                self.colors.get().ink,
                DT_SINGLELINE,
            );
            label(
                dc,
                rect(s(28), s(94), s(480), s(18)),
                if self.test_mode {
                    "交互测试 · 模拟调光，不改变真实屏幕"
                } else {
                    "双引擎调光 · 更改立即生效并自动保存"
                },
                fonts[1],
                self.colors.get().secondary,
                DT_SINGLELINE,
            );
        }
        let pen = CreatePen(PS_SOLID, 1, self.colors.get().line);
        let old = SelectObject(dc, pen);
        let _ = MoveToEx(dc, s(28), cr.bottom - s(self.footer.get()), None);
        let _ = LineTo(dc, cr.right - s(28), cr.bottom - s(self.footer.get()));
        SelectObject(dc, old);
        let _ = DeleteObject(pen);
        if !self.ready.get() {
            return;
        }
        let body = window_rect_in(self.body.get(), hwnd);
        let saved = SaveDC(dc);
        IntersectClipRect(dc, body.left, body.top, body.right, body.bottom);
        let _ = OffsetViewportOrgEx(dc, body.left, body.top, None);
        self.draw_body(
            dc,
            rect(0, 0, body.right - body.left, body.bottom - body.top),
        );
        let _ = RestoreDC(dc, saved);
        // Do not hold the controls RefCell across native queries or rendering:
        // a native EDIT/trackbar can synchronously notify its parent.
        let controls = self.controls.borrow().clone();
        for (id, h) in controls {
            if !IsWindowVisible(h).as_bool() {
                continue;
            }
            let area = window_rect_in(h, hwnd);
            let saved = SaveDC(dc);
            if GetParent(h).ok() == Some(self.body.get()) {
                IntersectClipRect(dc, body.left, body.top, body.right, body.bottom);
            }
            IntersectClipRect(dc, area.left, area.top, area.right, area.bottom);
            let _ = OffsetViewportOrgEx(dc, area.left, area.top, None);
            let local = rect(0, 0, area.right - area.left, area.bottom - area.top);
            match id {
                DELAY | DARK_TIME | LIGHT_TIME => {
                    FillRect(dc, &local, self.surface.get());
                    rounded(
                        dc,
                        local,
                        self.scale(8),
                        self.colors.get().surface,
                        self.colors.get().border,
                    );
                    if self.sizing.get() {
                        // Let EDIT render its own text/selection into the same
                        // resize buffer as its frame. Normal input/IME/caret
                        // painting remains native outside the resize commit.
                        let saved = SaveDC(dc);
                        let mut client = RECT::default();
                        let _ = GetClientRect(h, &mut client);
                        let mut origin = POINT::default();
                        let _ = ClientToScreen(h, &mut origin);
                        let _ = ScreenToClient(hwnd, &mut origin);
                        let x = origin.x - area.left;
                        let y = origin.y - area.top;
                        IntersectClipRect(dc, x, y, x + client.right, y + client.bottom);
                        let _ = OffsetViewportOrgEx(dc, x, y, None);
                        SendMessageW(
                            h,
                            WM_PRINTCLIENT,
                            WPARAM(dc.0 as usize),
                            LPARAM((PRF_CLIENT | PRF_ERASEBKGND) as isize),
                        );
                        let _ = RestoreDC(dc, saved);
                    }
                }
                SLIDER => draw_slider(self, h, dc),
                STATUS => {
                    label(
                        dc,
                        local,
                        &text(h),
                        fonts[1],
                        self.colors.get().secondary,
                        DT_LEFT | DT_WORDBREAK,
                    );
                }
                _ => draw_button(self, h, dc),
            }
            let _ = RestoreDC(dc, saved);
        }
    }
}

unsafe fn data(hwnd: HWND) -> Option<&'static Panel> {
    (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Panel).as_ref()
}
pub(crate) fn test_workarea(test_mode: bool) -> Result<Option<RECT>, String> {
    if !test_mode {
        return Ok(None);
    }
    let Ok(value) = std::env::var("SOG_TEST_WORKAREA") else {
        return Ok(None);
    };
    let numbers: Vec<i32> = value
        .split(',')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(|_| "测试屏幕坐标无效")?;
    let [left, top, right, bottom] = numbers.as_slice() else {
        return Err("测试屏幕需要四个坐标".into());
    };
    let area = RECT {
        left: *left,
        top: *top,
        right: *right,
        bottom: *bottom,
    };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        if area.right <= area.left
            || area.bottom <= area.top
            || !GetMonitorInfoW(MonitorFromRect(&area, MONITOR_DEFAULTTONULL), &mut info).as_bool()
            || info.rcWork != area
        {
            return Err("指定的测试屏幕工作区已变化".into());
        }
    }
    Ok(Some(area))
}
pub unsafe fn create(
    owner: HWND,
    config: &Config,
    test_mode: bool,
    system_dark: bool,
) -> Result<HWND, String> {
    let target = test_workarea(test_mode)?;
    let initial = target.map(|area| {
        rect(
            area.left + 24,
            area.top + 24,
            800.min(area.right - area.left - 48),
            1000.min(area.bottom - area.top - 48),
        )
    });
    let colors = theme::palette(config.theme.is_dark(system_dark));
    let bg = CreateSolidBrush(colors.bg);
    let surface = CreateSolidBrush(colors.surface);
    register("SOGRustSettings", Some(root_proc), HBRUSH::default())?;
    register("SOGRustSettingsBody", Some(body_proc), HBRUSH::default())?;
    let p = Box::new(Panel {
        owner,
        root: Cell::new(HWND::default()),
        body: Cell::new(HWND::default()),
        status: Cell::new(HWND::default()),
        syncing: Cell::new(false),
        ready: Cell::new(false),
        sizing: Cell::new(false),
        dpi: Cell::new(96),
        scroll: Cell::new(0),
        total: Cell::new(0),
        layout_bounds: Cell::new((0, 0, 0, 0)),
        fonts: RefCell::new(vec![]),
        controls: RefCell::new(vec![]),
        placements: RefCell::new(vec![]),
        groups: RefCell::new(vec![]),
        last_values: RefCell::new(vec![]),
        footer: Cell::new(FOOTER_H),
        surface: Cell::new(surface),
        background: Cell::new(bg),
        colors: Cell::new(colors),
        theme: Cell::new(config.theme),
        system_dark: Cell::new(system_dark),
        capturing: Cell::new(0),
        test_mode,
        buffered: BufferedPaintInit().is_ok(),
    });
    let raw = Box::into_raw(p);
    let hwnd = CreateWindowExW(
        WS_EX_APPWINDOW,
        w!("SOGRustSettings"),
        PCWSTR(
            wide(if test_mode {
                "一键熄屏 · 设置（交互测试）"
            } else {
                "一键熄屏 · 设置"
            })
            .as_ptr(),
        ),
        WS_POPUP | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX,
        initial.map_or(CW_USEDEFAULT, |r| r.left),
        initial.map_or(CW_USEDEFAULT, |r| r.top),
        initial.map_or(800, |r| r.right - r.left),
        initial.map_or(1000, |r| r.bottom - r.top),
        None,
        None,
        instance(),
        Some(raw.cast()),
    )
    .map_err(|e| e.to_string())?;
    let p = data(hwnd).ok_or("窗口状态未初始化")?;
    let corner = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        (&corner as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
        size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
    );
    p.dpi.set(GetDpiForWindow(hwnd).max(96));
    let body = CreateWindowExW(
        WS_EX_CONTROLPARENT,
        w!("SOGRustSettingsBody"),
        PCWSTR::null(),
        WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN,
        0,
        0,
        10,
        10,
        hwnd,
        None,
        instance(),
        Some(raw.cast()),
    )
    .map_err(|e| e.to_string())?;
    p.body.set(body);
    p.populate()?;
    p.window_theme();
    sync(hwnd, config, true);
    let mut work = RECT::default();
    SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        Some((&mut work as *mut RECT).cast()),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .map_err(|e| e.to_string())?;
    if let Some(target) = target {
        work = target;
    }
    let width = p.scale(724).min(work.right - work.left - 32);
    let height = p.scale(1040).min(work.bottom - work.top - 48);
    let _ = SetWindowPos(
        hwnd,
        None,
        work.left + (work.right - work.left - width) / 2,
        work.top + (work.bottom - work.top - height) / 2,
        width,
        height,
        SWP_NOZORDER | SWP_FRAMECHANGED,
    );
    p.layout();
    let _ = ShowWindow(
        hwnd,
        if target.is_some() {
            SW_SHOWNOACTIVATE
        } else {
            SW_SHOW
        },
    );
    let _ = RedrawWindow(
        hwnd,
        None,
        None,
        RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN | RDW_UPDATENOW,
    );
    Ok(hwnd)
}
pub unsafe fn control(hwnd: HWND, id: i32) -> HWND {
    data(hwnd).map(|p| p.control(id)).unwrap_or_default()
}
pub unsafe fn value(hwnd: HWND, id: i32) -> String {
    text(control(hwnd, id))
}
pub unsafe fn status(hwnd: HWND, message: &str) {
    if let Some(p) = data(hwnd) {
        set_text(p.status.get(), message);
        let expanded = message.chars().count() > 56
            || ["保存失败", "尚未保存", "无法读取", "配置版本不受支持"]
                .iter()
                .any(|word| message.contains(word));
        let height = if expanded { 72 } else { FOOTER_H };
        if p.footer.replace(height) != height {
            p.layout();
        }
    }
}
pub unsafe fn set_capture(hwnd: HWND, target_id: i32, capturing: bool, label: &str) {
    if !is_hotkey_id(target_id) {
        return;
    }
    if let Some(p) = data(hwnd) {
        p.capturing.set(if capturing { target_id } else { 0 });
        set_text(p.control(target_id), label);
        if capturing {
            let _ = SetFocus(p.control(target_id));
        }
        p.layout();
    }
}
pub unsafe fn system_theme_changed(hwnd: HWND, dark: bool) {
    if let Some(p) = data(hwnd) {
        p.system_dark.set(dark);
        p.apply_theme(p.theme.get());
    }
}
pub unsafe fn effective_dark(hwnd: HWND) -> Option<bool> {
    data(hwnd).map(|p| p.colors.get().dark)
}
pub unsafe fn sync(hwnd: HWND, cfg: &Config, force: bool) {
    let Some(p) = data(hwnd) else {
        return;
    };
    p.syncing.set(true);
    p.apply_theme(cfg.theme);
    for (id, on) in [
        (EXT, cfg.dim_external),
        (INT, cfg.dim_internal),
        (HDR, cfg.hdr_off),
        (AUTOSTART, cfg.autostart),
        (SCHEDULE, cfg.schedule_enabled),
        (HOTKEY_DOUBLE, cfg.hotkey_double_click),
        (DESKTOP_ENABLED, cfg.desktop_trigger_enabled),
        (DESKTOP_DOUBLE, cfg.desktop_double_click),
        (BLACK, cfg.dim_order == DimOrder::BlackFirst),
        (DIM, cfg.dim_order == DimOrder::DimFirst),
        (FADE, cfg.dim_order == DimOrder::Fade),
        (READY, cfg.restore_mode == RestoreMode::Ready),
        (DELAY_MODE, cfg.restore_mode == RestoreMode::Delay),
        (CUR_AUTO, cfg.cursor_mode == CursorMode::Auto),
        (CUR_HIDE, cfg.cursor_mode == CursorMode::Hide),
        (CUR_KEEP, cfg.cursor_mode == CursorMode::Keep),
        (CLOSE_HIDE, cfg.close_action == CloseAction::Hide),
        (CLOSE_EXIT, cfg.close_action == CloseAction::Exit),
        (THEME_LIGHT, cfg.theme == Theme::Light),
        (THEME_DARK, cfg.theme == Theme::Dark),
        (THEME_SYSTEM, cfg.theme == Theme::System),
    ] {
        SendMessageW(
            p.control(id),
            BM_SETCHECK,
            WPARAM(usize::from(on)),
            LPARAM(0),
        );
        let _ = InvalidateRect(p.control(id), None, false);
    }
    let values = vec![
        (HOTKEY, cfg.hotkey.label()),
        (DESKTOP_HOTKEY, cfg.desktop_hotkey.label()),
        (
            SCREENS,
            cfg.selected_displays.as_ref().map_or_else(
                || "所有屏幕".into(),
                |ids| format!("已选 {} 块屏幕", ids.len()),
            ),
        ),
        (DELAY, cfg.delay_ms.to_string()),
        (DARK_TIME, cfg.dark_at.label()),
        (LIGHT_TIME, cfg.light_at.label()),
    ];
    for (id, value) in &values {
        let previous = p
            .last_values
            .borrow()
            .iter()
            .find(|(key, _)| key == id)
            .map(|(_, text)| text.clone());
        if p.capturing.get() != *id
            && (force || is_hotkey_id(*id) || previous.as_ref() == Some(&text(p.control(*id))))
        {
            set_text(p.control(*id), value);
        }
    }
    *p.last_values.borrow_mut() = values;
    let slider = p.control(SLIDER);
    if SendMessageW(slider, TBM_GETPOS, WPARAM(0), LPARAM(0)).0 != cfg.delay_ms as isize {
        // The native control recomputes its thumb/hit rectangle on a redraw
        // request. Our WM_PAINT still owns presentation inside the body buffer.
        SendMessageW(slider, TBM_SETPOS, WPARAM(1), LPARAM(cfg.delay_ms as isize));
        let _ = InvalidateRect(slider, None, false);
    }
    p.syncing.set(false);
}
pub unsafe fn is_edit(hwnd: HWND, target: HWND) -> bool {
    [DELAY, DARK_TIME, LIGHT_TIME]
        .into_iter()
        .any(|id| control(hwnd, id) == target)
}
pub fn is_hotkey_id(id: i32) -> bool {
    matches!(id, HOTKEY | DESKTOP_HOTKEY)
}
pub unsafe fn set_dark(hwnd: HWND, dark: bool) {
    set_text(
        control(hwnd, TOGGLE),
        if dark { "恢复亮屏" } else { "立即熄屏" },
    );
}
pub unsafe fn set_busy(hwnd: HWND, busy: bool) {
    let _ = EnableWindow(control(hwnd, MEASURE), !busy);
}
pub unsafe fn set_retry(hwnd: HWND, visible: bool) {
    let _ = ShowWindow(
        control(hwnd, RETRY),
        if visible { SW_SHOW } else { SW_HIDE },
    );
}
pub unsafe fn ensure_focus(hwnd: HWND) {
    let Some(p) = data(hwnd) else {
        return;
    };
    let focus = GetFocus();
    if [HOTKEY, DESKTOP_HOTKEY]
        .into_iter()
        .any(|id| focus == p.control(id))
    {
        p.scroll_by(0);
        return;
    }
    if GetParent(focus).ok() != Some(p.body.get()) {
        return;
    }
    let mut r = RECT::default();
    let _ = GetWindowRect(focus, &mut r);
    let mut point = POINT {
        x: r.left,
        y: r.top,
    };
    let _ = ScreenToClient(p.body.get(), &mut point);
    let mut body = RECT::default();
    let _ = GetClientRect(p.body.get(), &mut body);
    if point.y < 0 {
        p.scroll_by(p.scroll.get() + point.y - p.scale(8));
    } else if point.y + r.bottom - r.top > body.bottom {
        p.scroll_by(p.scroll.get() + point.y + r.bottom - r.top - body.bottom + p.scale(8));
    }
}

unsafe extern "system" fn root_proc(h: HWND, m: u32, wparam: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_NCCREATE {
        let cs = &*(l.0 as *const CREATESTRUCTW);
        let p = cs.lpCreateParams as *mut Panel;
        SetWindowLongPtrW(h, GWLP_USERDATA, p as isize);
        (*p).root.set(h);
        return LRESULT(1);
    }
    if m == WM_NCCALCSIZE {
        // The scene is size-dependent. Preserving the old top-left-aligned
        // client bitmap can expose clipped right-anchored controls before the
        // new scene is committed (especially when dragging the left edge).
        return LRESULT(if wparam.0 != 0 {
            WVR_REDRAW as isize
        } else {
            0
        });
    }
    let Some(p) = data(h) else {
        return DefWindowProcW(h, m, wparam, l);
    };
    match m {
        WM_NOTIFY => custom_draw(p, l).unwrap_or_else(|| DefWindowProcW(h, m, wparam, l)),
        WM_LBUTTONDOWN => {
            let _ = SetFocus(h);
            LRESULT(0)
        }
        WM_KEYDOWN if [VK_PRIOR.0, VK_NEXT.0, VK_HOME.0, VK_END.0].contains(&(wparam.0 as u16)) => {
            SendMessageW(p.body.get(), WM_KEYDOWN, wparam, l)
        }
        WM_MOUSEWHEEL => SendMessageW(p.body.get(), m, wparam, l),
        WM_NCPAINT => LRESULT(0),
        WM_NCACTIVATE => LRESULT(1),
        WM_ERASEBKGND => LRESULT(1),
        // Deterministic layout scaling without changing the user's Windows display settings.
        value if value == WM_APP + 30 && p.test_mode => {
            if [96, 120, 144, 192].contains(&(wparam.0 as u32)) {
                p.dpi.set(wparam.0 as u32);
                p.fonts();
                p.layout();
            }
            LRESULT(0)
        }
        WM_PAINT => {
            p.paint_root(h);
            LRESULT(0)
        }
        WM_WINDOWPOSCHANGING => {
            let result = DefWindowProcW(h, m, wparam, l);
            let position = &mut *(l.0 as *mut WINDOWPOS);
            if !position.flags.contains(SWP_NOSIZE) {
                // No old client bitmap is valid for this size-dependent scene.
                // Inhibit SetWindowPos's copy as well as invalidating via WVR.
                position.flags |= SWP_NOCOPYBITS;
            }
            result
        }
        WM_SIZE => {
            if wparam.0 != SIZE_MINIMIZED as usize {
                let sizing = p.sizing.replace(true);
                if p.ready.get() {
                    set_text(
                        p.control(MAXIMIZE),
                        if IsZoomed(h).as_bool() {
                            "还原"
                        } else {
                            "最大化"
                        },
                    );
                }
                p.layout();
                // A resize can publish the old client bitmap as soon as this
                // callback returns. Commit the composed scene and native edit
                // clients now, rather than waiting for an idle WM_PAINT.
                let _ = RedrawWindow(
                    h,
                    None,
                    None,
                    RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
                );
                p.sizing.set(sizing);
            }
            LRESULT(0)
        }
        WM_ENTERSIZEMOVE => {
            p.sizing.set(true);
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            p.sizing.set(false);
            let _ = RedrawWindow(
                h,
                None,
                None,
                RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
            );
            LRESULT(0)
        }
        WM_DPICHANGED => {
            p.dpi.set((wparam.0 & 0xffff) as u32);
            p.fonts();
            let r = &*(l.0 as *const RECT);
            let _ = SetWindowPos(
                h,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            p.layout();
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let info = &mut *(l.0 as *mut MINMAXINFO);
            info.ptMinTrackSize = POINT {
                x: p.scale(640),
                y: p.scale(480),
            };
            let mut monitor = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut monitor)
                .as_bool()
            {
                info.ptMaxPosition = POINT {
                    x: monitor.rcWork.left - monitor.rcMonitor.left,
                    y: monitor.rcWork.top - monitor.rcMonitor.top,
                };
                info.ptMaxSize = POINT {
                    x: monitor.rcWork.right - monitor.rcWork.left,
                    y: monitor.rcWork.bottom - monitor.rcWork.top,
                };
            }
            LRESULT(0)
        }
        WM_NCHITTEST => {
            let mut r = RECT::default();
            let _ = GetWindowRect(h, &mut r);
            let x = l.0 as u16 as i16 as i32 - r.left;
            let y = (l.0 >> 16) as u16 as i16 as i32 - r.top;
            let b = p.scale(7);
            let left = x < b;
            let right = x >= r.right - r.left - b;
            let top = y < b;
            let bottom = y >= r.bottom - r.top - b;
            let code = if !IsZoomed(h).as_bool() && top && left {
                HTTOPLEFT
            } else if !IsZoomed(h).as_bool() && top && right {
                HTTOPRIGHT
            } else if !IsZoomed(h).as_bool() && bottom && left {
                HTBOTTOMLEFT
            } else if !IsZoomed(h).as_bool() && bottom && right {
                HTBOTTOMRIGHT
            } else if !IsZoomed(h).as_bool() && left {
                HTLEFT
            } else if !IsZoomed(h).as_bool() && right {
                HTRIGHT
            } else if !IsZoomed(h).as_bool() && top {
                HTTOP
            } else if !IsZoomed(h).as_bool() && bottom {
                HTBOTTOM
            } else if y < p.scale(44) && x < r.right - r.left - p.scale(204) {
                HTCAPTION
            } else {
                HTCLIENT
            };
            LRESULT(code as isize)
        }
        WM_COMMAND => {
            if p.ready.get() && !p.syncing.get() {
                let _ = PostMessageW(p.owner, app::WM_PANEL, wparam, l);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = PostMessageW(p.owner, app::WM_PANEL, WPARAM(CLOSE as usize), LPARAM(0));
            LRESULT(0)
        }
        WM_SYSCOMMAND if wparam.0 & 0xfff0 == SC_MINIMIZE as usize => {
            let _ = PostMessageW(p.owner, app::WM_PANEL, WPARAM(MINIMIZE as usize), LPARAM(0));
            LRESULT(0)
        }
        WM_ACTIVATE => {
            let margins = MARGINS {
                cxLeftWidth: 1,
                cxRightWidth: 1,
                cyTopHeight: 1,
                cyBottomHeight: 1,
            };
            let _ = DwmExtendFrameIntoClientArea(h, &margins);
            if wparam.0 & 0xffff == WA_INACTIVE as usize {
                let _ = PostMessageW(p.owner, app::WM_CANCEL_CAPTURE, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let dc = HDC(wparam.0 as *mut _);
            SetTextColor(dc, p.colors.get().secondary);
            SetBkColor(dc, p.colors.get().bg);
            LRESULT(p.background.get().0 as isize)
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(h, GWLP_USERDATA, 0);
            drop(Box::from_raw(p as *const Panel as *mut Panel));
            DefWindowProcW(h, m, wparam, l)
        }
        _ => DefWindowProcW(h, m, wparam, l),
    }
}
unsafe extern "system" fn body_proc(h: HWND, m: u32, wparam: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_NCCREATE {
        let cs = &*(l.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(h, GWLP_USERDATA, cs.lpCreateParams as isize);
        return LRESULT(1);
    }
    let Some(p) = data(h) else {
        return DefWindowProcW(h, m, wparam, l);
    };
    match m {
        WM_NOTIFY => custom_draw(p, l).unwrap_or_else(|| DefWindowProcW(h, m, wparam, l)),
        WM_KEYDOWN if [VK_PRIOR.0, VK_NEXT.0, VK_HOME.0, VK_END.0].contains(&(wparam.0 as u16)) => {
            let command = if wparam.0 == usize::from(VK_PRIOR.0) {
                2
            } else if wparam.0 == usize::from(VK_NEXT.0) {
                3
            } else if wparam.0 == usize::from(VK_HOME.0) {
                6
            } else {
                7
            };
            SendMessageW(h, WM_VSCROLL, WPARAM(command), LPARAM(0))
        }
        WM_LBUTTONDOWN => {
            let _ = SetFocus(h);
            LRESULT(0)
        }
        WM_NCHITTEST => {
            let code = SendMessageW(p.root.get(), m, wparam, l).0;
            if (HTLEFT as isize..=HTBOTTOMRIGHT as isize).contains(&code) {
                LRESULT(HTTRANSPARENT as isize)
            } else {
                DefWindowProcW(h, m, wparam, l)
            }
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            validate_surface(p, h);
            LRESULT(0)
        }
        WM_COMMAND => {
            if p.ready.get() && !p.syncing.get() {
                let _ = PostMessageW(p.owner, app::WM_PANEL, wparam, l);
            }
            LRESULT(0)
        }
        WM_HSCROLL => {
            if p.ready.get() && !p.syncing.get() {
                let _ = InvalidateRect(p.control(SLIDER), None, true);
                let value = SendMessageW(p.control(SLIDER), WM_USER, WPARAM(0), LPARAM(0)).0;
                p.syncing.set(true);
                set_text(p.control(DELAY), &value.to_string());
                p.syncing.set(false);
                let _ = PostMessageW(
                    p.owner,
                    app::WM_PANEL,
                    WPARAM(DELAY as usize | ((EN_KILLFOCUS as usize) << 16)),
                    LPARAM(0),
                );
            }
            LRESULT(0)
        }
        WM_VSCROLL => {
            let mut client = RECT::default();
            let _ = GetClientRect(h, &mut client);
            let position = p.scroll.get();
            let pos = match wparam.0 as u16 as u32 {
                0 => position - p.scale(40),
                1 => position + p.scale(40),
                2 => position - client.bottom,
                3 => position + client.bottom,
                4 | 5 => (wparam.0 >> 16) as i32,
                6 => 0,
                7 => p.total.get(),
                _ => position,
            };
            p.scroll_by(pos);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = (wparam.0 >> 16) as u16 as i16 as i32;
            p.scroll_by(p.scroll.get() - delta * p.scale(96) / 120);
            LRESULT(0)
        }
        WM_CTLCOLORBTN | WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
            let dc = HDC(wparam.0 as *mut _);
            SetTextColor(dc, p.colors.get().ink);
            SetBkColor(dc, p.colors.get().surface);
            LRESULT(p.surface.get().0 as isize)
        }
        _ => DefWindowProcW(h, m, wparam, l),
    }
}
unsafe fn draw_toggle(p: &Panel, h: HWND, dc: HDC) {
    let mut r = RECT::default();
    let _ = GetClientRect(h, &mut r);
    FillRect(dc, &r, p.surface.get());
    let on = SendMessageW(h, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == 1;
    let s = |v| p.scale(v);
    let x = (r.right - s(40)) / 2;
    let y = (r.bottom - s(20)) / 2;
    rounded(
        dc,
        rect(x, y, s(40), s(20)),
        s(20),
        if on {
            p.colors.get().accent
        } else {
            p.colors.get().surface
        },
        if on {
            p.colors.get().accent
        } else {
            p.colors.get().off
        },
    );
    let dot = if on {
        if p.colors.get().dark {
            p.colors.get().bg
        } else {
            COLORREF(0xffffff)
        }
    } else {
        p.colors.get().off
    };
    let dx = x + if on { s(24) } else { s(4) };
    ellipse(dc, rect(dx, y + s(4), s(12), s(12)), Some(dot), None);
}

unsafe fn rounded(dc: HDC, area: RECT, radius: i32, fill: COLORREF, border: COLORREF) {
    #[cfg(feature = "direct2d")]
    if crate::d2d::shape(dc, area, radius as f32 / 2., fill, border, false) {
        return;
    }
    if crate::antialias::shape(
        dc,
        area,
        radius as f32 / 2.,
        Some(fill),
        Some(border),
        false,
    ) {
        return;
    }
    let brush = CreateSolidBrush(fill);
    let pen = CreatePen(PS_SOLID, 1, border);
    let oldb = SelectObject(dc, brush);
    let oldp = SelectObject(dc, pen);
    let _ = RoundRect(
        dc,
        area.left,
        area.top,
        area.right,
        area.bottom,
        radius,
        radius,
    );
    SelectObject(dc, oldb);
    SelectObject(dc, oldp);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);
}
unsafe fn ellipse(dc: HDC, area: RECT, fill: Option<COLORREF>, border: Option<COLORREF>) {
    if crate::antialias::shape(dc, area, 0., fill, border, true) {
        return;
    }
    let brush = fill.map(|color| CreateSolidBrush(color));
    let pen = border.map(|color| CreatePen(PS_SOLID, 1, color));
    let oldb = SelectObject(
        dc,
        brush
            .map(|b| HGDIOBJ(b.0))
            .unwrap_or(GetStockObject(NULL_BRUSH)),
    );
    let oldp = SelectObject(
        dc,
        pen.map(|p| HGDIOBJ(p.0))
            .unwrap_or(GetStockObject(NULL_PEN)),
    );
    let _ = Ellipse(dc, area.left, area.top, area.right, area.bottom);
    SelectObject(dc, oldb);
    SelectObject(dc, oldp);
    if let Some(brush) = brush {
        let _ = DeleteObject(brush);
    }
    if let Some(pen) = pen {
        let _ = DeleteObject(pen);
    }
}
unsafe fn outline_round(dc: HDC, area: RECT, diameter: i32, color: COLORREF) {
    if !crate::antialias::shape(dc, area, diameter as f32 / 2., None, Some(color), false) {
        let _ = RoundRect(
            dc,
            area.left,
            area.top,
            area.right,
            area.bottom,
            diameter,
            diameter,
        );
    }
}
unsafe fn stroke(dc: HDC, from: (i32, i32), to: (i32, i32)) {
    let _ = MoveToEx(dc, from.0, from.1, None);
    let _ = LineTo(dc, to.0, to.1);
}
unsafe fn group_icon(dc: HDC, index: usize, x: i32, y: i32, size: i32, secondary: COLORREF) {
    let index = match index {
        0 | 1 => 0,
        2 | 3 => 1,
        4 => 2,
        5 => 3,
        _ => 4,
    };
    let s = |v| v * size / 18;
    let pen = CreatePen(PS_SOLID, 1, secondary);
    let oldp = SelectObject(dc, pen);
    let oldb = SelectObject(dc, GetStockObject(NULL_BRUSH));
    match index {
        0 => {
            outline_round(dc, rect(x, y + s(3), size, s(13)), s(3), secondary);
            for row in [6, 9] {
                for col in [3, 7, 11, 15] {
                    let _ = Rectangle(
                        dc,
                        x + s(col),
                        y + s(row),
                        x + s(col + 1) + 1,
                        y + s(row + 1) + 1,
                    );
                }
            }
            stroke(dc, (x + s(5), y + s(13)), (x + s(13), y + s(13)));
        }
        1 => {
            outline_round(dc, rect(x, y + s(1), size, s(12)), s(2), secondary);
            stroke(dc, (x + s(9), y + s(13)), (x + s(9), y + s(17)));
            stroke(dc, (x + s(5), y + s(17)), (x + s(13), y + s(17)));
        }
        2 | 3 => {
            let oy = if index == 3 { s(3) } else { 0 };
            ellipse(dc, rect(x, y + oy, size, size - oy), None, Some(secondary));
            stroke(dc, (x + s(9), y + s(5)), (x + s(9), y + s(10)));
            stroke(dc, (x + s(9), y + s(10)), (x + s(13), y + s(12)));
            if index == 3 {
                stroke(dc, (x + s(6), y), (x + s(12), y));
            }
        }
        _ => {
            let _ = Arc(
                dc,
                x,
                y + s(1),
                x + size,
                y + size,
                x + s(5),
                y + s(2),
                x + s(13),
                y + s(2),
            );
            stroke(dc, (x + s(9), y), (x + s(9), y + s(9)));
        }
    }
    SelectObject(dc, oldb);
    SelectObject(dc, oldp);
    let _ = DeleteObject(pen);
}
unsafe fn draw_slider(p: &Panel, h: HWND, dc: HDC) {
    let mut client = RECT::default();
    let _ = GetClientRect(h, &mut client);
    FillRect(dc, &client, p.surface.get());
    // These pointer messages stay within the window's own thread/process.
    let (mut channel, mut thumb) = (RECT::default(), RECT::default());
    SendMessageW(
        h,
        TBM_GETCHANNELRECT,
        WPARAM(0),
        LPARAM((&mut channel as *mut RECT) as isize),
    );
    SendMessageW(
        h,
        TBM_GETTHUMBRECT,
        WPARAM(0),
        LPARAM((&mut thumb as *mut RECT) as isize),
    );
    let s = |v| p.scale(v);
    let cy = (channel.top + channel.bottom) / 2;
    let cx = (thumb.left + thumb.right) / 2;
    rounded(
        dc,
        rect(channel.left, cy - s(2), channel.right - channel.left, s(4)),
        s(4),
        p.colors.get().track,
        p.colors.get().track,
    );
    if cx > channel.left {
        rounded(
            dc,
            rect(channel.left, cy - s(2), cx - channel.left, s(4)),
            s(4),
            p.colors.get().accent,
            p.colors.get().accent,
        );
    }
    rounded(
        dc,
        rect(cx - s(9), cy - s(12), s(18), s(24)),
        s(6),
        p.colors.get().surface,
        p.colors.get().border,
    );
}
unsafe fn draw_button(p: &Panel, h: HWND, dc: HDC) {
    let control_id = GetDlgCtrlID(h);
    if matches!(
        control_id,
        EXT | INT | HDR | SCHEDULE | AUTOSTART | HOTKEY_DOUBLE | DESKTOP_ENABLED | DESKTOP_DOUBLE
    ) {
        draw_toggle(p, h, dc);
        return;
    }
    let s = |v| p.scale(v);
    let mut r = RECT::default();
    let _ = GetClientRect(h, &mut r);
    let caption = matches!(control_id, ABOUT | MINIMIZE | MAXIMIZE | CLOSE);
    FillRect(
        dc,
        &r,
        if caption {
            p.background.get()
        } else {
            p.surface.get()
        },
    );
    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    let _ = ScreenToClient(h, &mut point);
    let hot = point.x >= 0 && point.y >= 0 && point.x < r.right && point.y < r.bottom;
    let down = SendMessageW(h, BM_GETSTATE, WPARAM(0), LPARAM(0)).0 & 4 != 0;
    let enabled = IsWindowEnabled(h).as_bool();
    let color = if !enabled {
        p.colors.get().disabled
    } else {
        p.colors.get().ink
    };
    let fonts = p.fonts.borrow().clone();
    if matches!(
        control_id,
        CUR_AUTO
            | CUR_HIDE
            | CUR_KEEP
            | BLACK
            | DIM
            | FADE
            | READY
            | DELAY_MODE
            | CLOSE_HIDE
            | CLOSE_EXIT
            | THEME_LIGHT
            | THEME_DARK
            | THEME_SYSTEM
    ) {
        let on = SendMessageW(h, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == 1;
        let y = (r.bottom - s(18)) / 2;
        ellipse(
            dc,
            rect(0, y, s(18), s(18)),
            Some(if hot {
                p.colors.get().hover
            } else {
                p.colors.get().surface
            }),
            Some(if on {
                p.colors.get().accent
            } else {
                p.colors.get().off
            }),
        );
        if on {
            ellipse(
                dc,
                rect(s(5), y + s(5), s(8), s(8)),
                Some(p.colors.get().accent),
                None,
            );
        }
        label(
            dc,
            rect(s(26), 0, r.right - s(26), r.bottom),
            &text(h),
            fonts[0],
            color,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    } else {
        let primary = control_id == TOGGLE;
        let fill = if caption {
            if hot {
                if control_id == CLOSE {
                    COLORREF(0x2323C4)
                } else {
                    p.colors.get().caption_hover
                }
            } else {
                p.colors.get().bg
            }
        } else if primary {
            if down {
                p.colors.get().primary_pressed
            } else {
                p.colors.get().primary
            }
        } else if down {
            p.colors.get().pressed
        } else if hot {
            p.colors.get().hover
        } else {
            p.colors.get().surface
        };
        let capture = is_hotkey_id(control_id) && p.capturing.get() == control_id;
        // A continuous outer accent ring indicates recording, not ordinary
        // keyboard focus. Keep normal buttons/edits free of focus adornments.
        if capture {
            rounded(dc, r, s(8), p.colors.get().accent, p.colors.get().accent);
        }
        let mut face = r;
        if capture {
            let _ = InflateRect(&mut face, -s(2), -s(2));
        }
        rounded(
            dc,
            face,
            if caption {
                0
            } else if capture {
                s(6)
            } else {
                s(8)
            },
            fill,
            if caption || primary || capture {
                fill
            } else {
                p.colors.get().border
            },
        );
        let ink = if (caption && control_id == CLOSE && hot) || primary {
            COLORREF(0xFFFFFF)
        } else {
            color
        };
        if matches!(control_id, MINIMIZE | MAXIMIZE | CLOSE) {
            let x = r.right / 2;
            let y = r.bottom / 2;
            let pen = CreatePen(PS_SOLID, 1, ink);
            let oldp = SelectObject(dc, pen);
            let oldb = SelectObject(dc, GetStockObject(NULL_BRUSH));
            match control_id {
                MINIMIZE => stroke(dc, (x - s(5), y), (x + s(5), y)),
                MAXIMIZE => {
                    if IsZoomed(p.root.get()).as_bool() {
                        let _ = Rectangle(dc, x - s(3), y - s(5), x + s(5), y + s(3));
                    }
                    let _ = Rectangle(dc, x - s(5), y - s(3), x + s(3), y + s(5));
                }
                _ => {
                    stroke(dc, (x - s(5), y - s(5)), (x + s(5), y + s(5)));
                    stroke(dc, (x + s(5), y - s(5)), (x - s(5), y + s(5)));
                }
            }
            SelectObject(dc, oldb);
            SelectObject(dc, oldp);
            let _ = DeleteObject(pen);
        } else {
            label(
                dc,
                r,
                &text(h),
                fonts[if caption { 4 } else { 0 }],
                ink,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }
    }
}
unsafe fn custom_draw(p: &Panel, l: LPARAM) -> Option<LRESULT> {
    let notification = &*(l.0 as *const NMHDR);
    if notification.code != NM_CUSTOMDRAW {
        return None;
    }
    let id = GetDlgCtrlID(notification.hwndFrom);
    if p.control(id) != notification.hwndFrom {
        return None;
    }
    let draw = &*(l.0 as *const NMCUSTOMDRAW);
    if draw.dwDrawStage == CDDS_PREPAINT || draw.dwDrawStage == CDDS_PREERASE {
        // Native controls still own interaction/state, but never publish their
        // themed bitmap independently of the retained root scene.
        p.invalidate_window(notification.hwndFrom);
        Some(LRESULT(CDRF_SKIPDEFAULT as isize))
    } else {
        Some(LRESULT(CDRF_DODEFAULT as isize))
    }
}
unsafe extern "system" fn widget_proc(
    h: HWND,
    m: u32,
    wparam: WPARAM,
    l: LPARAM,
    _id: usize,
    root: usize,
) -> LRESULT {
    let Some(p) = data(HWND(root as *mut _)) else {
        return DefSubclassProc(h, m, wparam, l);
    };
    let control_id = GetDlgCtrlID(h);
    let edit = matches!(control_id, DELAY | DARK_TIME | LIGHT_TIME);
    if (!edit || p.sizing.get()) && m == WM_PAINT {
        validate_surface(p, h);
        return LRESULT(0);
    }
    if (!edit || p.sizing.get()) && m == WM_ERASEBKGND {
        return LRESULT(1);
    }
    if !edit && m == WM_SETTEXT {
        // BUTTON/STATIC WM_SETTEXT can erase through GetDC before custom draw.
        // The base window stores the caption; presentation belongs to the root.
        let result = DefWindowProcW(h, m, wparam, l);
        p.invalidate_window(h);
        return result;
    }
    if is_hotkey_id(control_id) && m == WM_KILLFOCUS && p.capturing.get() == control_id {
        let _ = PostMessageW(
            p.owner,
            app::WM_CANCEL_CAPTURE,
            WPARAM(control_id as usize),
            LPARAM(0),
        );
    }
    if m == WM_MOUSEWHEEL {
        // Settings pages scroll under the pointer; a focused native trackbar
        // must not consume wheel input and silently change the saved delay.
        return SendMessageW(p.body.get(), m, wparam, l);
    }
    if m == WM_NCHITTEST {
        let code = SendMessageW(p.root.get(), m, wparam, l).0;
        if (HTLEFT as isize..=HTBOTTOMRIGHT as isize).contains(&code) {
            return LRESULT(HTTRANSPARENT as isize);
        }
    }
    if control_id == SLIDER {
        if m == WM_ERASEBKGND {
            return LRESULT(1);
        }
        let result = DefSubclassProc(h, m, wparam, l);
        if matches!(
            m,
            TBM_SETPOS
                | WM_SETFOCUS
                | WM_KILLFOCUS
                | WM_MOUSEMOVE
                | WM_LBUTTONDOWN
                | WM_LBUTTONUP
                | WM_KEYDOWN
                | WM_KEYUP
                | WM_ENABLE
        ) {
            p.invalidate_window(h);
        }
        return result;
    }
    let s = |v| p.scale(v);
    if edit && m == WM_NCCALCSIZE {
        let area = if wparam.0 != 0 {
            &mut (*(l.0 as *mut NCCALCSIZE_PARAMS)).rgrc[0]
        } else {
            &mut *(l.0 as *mut RECT)
        };
        area.left += s(8);
        area.right -= s(8);
        area.top += s(6);
        area.bottom -= s(6);
        return LRESULT(0);
    }
    if m == WM_NCPAINT {
        p.invalidate_window(h);
        return LRESULT(0);
    }
    if m == WM_MOUSEMOVE {
        let mut tracking = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: h,
            ..Default::default()
        };
        let _ = TrackMouseEvent(&mut tracking);
    }
    let result = DefSubclassProc(h, m, wparam, l);
    if matches!(
        m,
        WM_SETFOCUS
            | WM_KILLFOCUS
            | WM_SETTEXT
            | BM_SETSTATE
            | BM_SETSTYLE
            | WM_MOUSEMOVE
            | WM_MOUSELEAVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_KEYDOWN
            | WM_KEYUP
            | BM_SETCHECK
            | WM_ENABLE
    ) {
        p.invalidate_window(h);
    }
    result
}
