//! Themed display selection. Enumeration is supplied by the caller and is read-only.
use crate::{theme::Palette, win::*};
use sog::native::DisplayInfo;
use std::{
    cell::{Cell, RefCell},
    mem::size_of,
    rc::Rc,
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        UI::{
            Controls::SetWindowTheme,
            Input::KeyboardAndMouse::{EnableWindow, GetFocus, SetFocus},
            Shell::{DefSubclassProc, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
};

pub const ALL: i32 = 3001;
pub const PREVIOUS: i32 = 3002;
pub const NEXT: i32 = 3003;
pub const FIRST_DISPLAY: i32 = 3100;
pub const SAVE: i32 = 1;
pub const CANCEL: i32 = 2;
const SUMMARY: i32 = 3004;
const PAGE: i32 = 3005;
const MAX_ROWS: usize = 5;
// winuser.h static-control alignment; windows-rs gates this behind SystemServices.
const SS_CENTER: u32 = 1;

struct Row {
    id: String,
    label: String,
    checked: bool,
}
struct Picker {
    owner: HWND,
    previous_focus: HWND,
    callback: HWND,
    message: u32,
    activate: bool,
    modal: Cell<bool>,
    colors: Palette,
    brush: HBRUSH,
    body: HFONT,
    title: HFONT,
    dpi: u32,
    all: Cell<bool>,
    page: Cell<usize>,
    page_size: usize,
    rows: RefCell<Vec<Row>>,
    result: RefCell<Option<Option<Vec<String>>>>,
    handed_off: Rc<Cell<bool>>,
}
impl Drop for Picker {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.brush);
            let _ = DeleteObject(self.body);
            let _ = DeleteObject(self.title);
        }
    }
}
impl Picker {
    fn scale(&self, value: i32) -> i32 {
        (i64::from(value) * i64::from(self.dpi) / 96) as i32
    }
}
unsafe fn state(h: HWND) -> Option<&'static Picker> {
    (GetWindowLongPtrW(h, GWLP_USERDATA) as *const Picker).as_ref()
}

/// None means no saved result; Some(None) means all current and future displays.
pub unsafe fn selection(h: HWND) -> Option<Option<Vec<String>>> {
    state(h).and_then(|p| p.result.borrow().clone())
}
pub unsafe fn close(h: HWND) {
    if IsWindow(h).as_bool() {
        let _ = DestroyWindow(h);
    }
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn show(
    owner: HWND,
    callback: HWND,
    message: u32,
    displays: Vec<DisplayInfo>,
    selected: Option<Vec<String>>,
    colors: Palette,
    activate: bool,
) -> Result<HWND, String> {
    let test_area = crate::panel::test_workarea(!activate)?;
    register("SOGRustDisplayPicker", Some(proc), HBRUSH::default())?;
    let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(owner).max(96);
    let s = |v: i32| (i64::from(v) * i64::from(dpi) / 96) as i32;
    let mut monitor = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let mut area = if let Some(area) = test_area {
        area
    } else {
        if !GetMonitorInfoW(
            MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST),
            &mut monitor,
        )
        .as_bool()
        {
            return Err("无法取得显示器工作区".into());
        }
        monitor.rcWork
    };
    let work = area;
    if test_area.is_none() && IsWindowVisible(owner).as_bool() {
        let _ = GetWindowRect(owner, &mut area);
    }
    let width = s(600).min(work.right - work.left - s(24));
    let disconnected = selected.as_ref().map_or(0, |ids| {
        ids.iter()
            .filter(|id| !displays.iter().any(|d| d.id.eq_ignore_ascii_case(id)))
            .count()
    });
    let row_count = (displays.len() + disconnected).clamp(1, MAX_ROWS);
    let height = s(330 + 46 * row_count as i32).min(work.bottom - work.top - s(24));
    if width < s(390) || height < s(390) {
        return Err("屏幕工作区过小，无法显示选屏窗口".into());
    }
    let x = (area.left + (area.right - area.left - width) / 2).clamp(work.left, work.right - width);
    let y =
        (area.top + (area.bottom - area.top - height) / 2).clamp(work.top, work.bottom - height);
    let mut rows = Vec::<Row>::new();
    for display in displays {
        if rows
            .iter()
            .any(|row| row.id.eq_ignore_ascii_case(&display.id))
        {
            continue;
        }
        let checked = selected
            .as_ref()
            .is_none_or(|ids| ids.iter().any(|id| id.eq_ignore_ascii_case(&display.id)));
        rows.push(Row {
            id: display.id,
            label: format!(
                "屏幕 {} · {} · {}",
                display
                    .source
                    .rsplit("DISPLAY")
                    .next()
                    .unwrap_or(&display.source),
                if display.name.is_empty() {
                    "未命名显示器"
                } else {
                    &display.name
                },
                if display.internal {
                    "内置屏"
                } else {
                    "外接屏"
                }
            ),
            checked,
        });
    }
    if let Some(ids) = &selected {
        for id in ids {
            if !rows.iter().any(|row| row.id.eq_ignore_ascii_case(id)) {
                rows.push(Row {
                    id: id.clone(),
                    label: format!("未连接 · {id}"),
                    checked: true,
                });
            }
        }
    }
    // The non-client frame consumes up to 48 DIP. Always reserve the controls
    // above/below the paged list before choosing its number of rows.
    let page_size = (((height - s(320)) / s(46)).max(1) as usize).min(MAX_ROWS);
    let handed_off = Rc::new(Cell::new(false));
    let data = Box::new(Picker {
        owner,
        previous_focus: GetFocus(),
        callback,
        message,
        activate,
        modal: Cell::new(false),
        colors,
        brush: CreateSolidBrush(colors.bg),
        body: font(s(15), 400),
        title: font(s(20), 600),
        dpi,
        all: Cell::new(selected.is_none()),
        page: Cell::new(0),
        page_size,
        rows: RefCell::new(rows),
        result: RefCell::new(None),
        handed_off: handed_off.clone(),
    });
    let raw = Box::into_raw(data);
    let created = CreateWindowExW(
        WS_EX_DLGMODALFRAME | WS_EX_COMPOSITED,
        w!("SOGRustDisplayPicker"),
        w!("选择控制的屏幕"),
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN,
        x,
        y,
        width,
        height,
        owner,
        None,
        instance(),
        Some(raw.cast()),
    );
    let h = match created {
        Ok(h) => h,
        Err(error) => {
            if !handed_off.get() {
                drop(Box::from_raw(raw));
            }
            return Err(error.to_string());
        }
    };
    let p = state(h).ok_or("选屏窗口初始化失败")?;
    let dark = BOOL::from(colors.dark);
    let _ = DwmSetWindowAttribute(
        h,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        (&dark as *const BOOL).cast(),
        size_of::<BOOL>() as u32,
    );
    if let Err(error) = children(h) {
        close(h);
        return Err(error);
    }
    p.modal
        .set(windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(owner).as_bool());
    let _ = EnableWindow(owner, false);
    let _ = ShowWindow(h, if activate { SW_SHOW } else { SW_SHOWNOACTIVATE });
    if activate {
        let _ = SetForegroundWindow(h);
        if let Ok(all) = GetDlgItem(h, ALL) {
            let _ = SetFocus(all);
        }
    }
    Ok(h)
}

unsafe fn add(
    h: HWND,
    id: i32,
    caption: &str,
    class: PCWSTR,
    style: u32,
    r: RECT,
) -> Result<HWND, String> {
    let p = state(h).ok_or("选屏窗口状态缺失")?;
    let child = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR(wide(caption).as_ptr()),
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
        r.left,
        r.top,
        r.right - r.left,
        r.bottom - r.top,
        h,
        HMENU(id as usize as *mut _),
        instance(),
        None,
    )
    .map_err(|error| error.to_string())?;
    SendMessageW(child, WM_SETFONT, WPARAM(p.body.0 as usize), LPARAM(0));
    if [ALL, PREVIOUS, NEXT, SAVE, CANCEL].contains(&id) || id >= FIRST_DISPLAY {
        let _ = SetWindowTheme(
            child,
            if p.colors.dark {
                w!("DarkMode_Explorer")
            } else {
                w!("Explorer")
            },
            PCWSTR::null(),
        );
        if !SetWindowSubclass(child, Some(control_proc), 1, h.0 as usize).as_bool() {
            return Err("无法初始化选屏控件".into());
        }
    }
    Ok(child)
}
unsafe fn children(h: HWND) -> Result<(), String> {
    let p = state(h).ok_or("选屏窗口状态缺失")?;
    let s = |v| p.scale(v);
    let mut client = RECT::default();
    GetClientRect(h, &mut client).map_err(|error| error.to_string())?;
    let width = client.right;
    let title = add(
        h,
        3010,
        "选择熄屏范围",
        w!("STATIC"),
        0,
        rect(s(24), s(18), width - s(48), s(32)),
    )?;
    SendMessageW(title, WM_SETFONT, WPARAM(p.title.0 as usize), LPARAM(0));
    add(
        h,
        3011,
        "仅控制选中的屏幕；亮屏时恢复原亮度。",
        w!("STATIC"),
        0,
        rect(s(24), s(56), width - s(48), s(46)),
    )?;
    add(
        h,
        ALL,
        "全部屏幕（包含以后连接的屏幕）",
        w!("BUTTON"),
        WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
        rect(s(24), s(104), width - s(48), s(40)),
    )?;
    for row in 0..p.page_size {
        add(
            h,
            FIRST_DISPLAY + row as i32,
            "",
            w!("BUTTON"),
            WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
            rect(s(24), s(152) + s(46) * row as i32, width - s(48), s(42)),
        )?;
    }
    let nav_y = s(154) + s(46) * p.page_size as i32;
    add(
        h,
        PREVIOUS,
        "上一页",
        w!("BUTTON"),
        WS_TABSTOP.0 | BS_PUSHBUTTON as u32,
        rect(s(24), nav_y, s(88), s(36)),
    )?;
    add(
        h,
        PAGE,
        "",
        w!("STATIC"),
        SS_CENTER,
        rect(s(120), nav_y + s(7), width - s(240), s(24)),
    )?;
    add(
        h,
        NEXT,
        "下一页",
        w!("BUTTON"),
        WS_TABSTOP.0 | BS_PUSHBUTTON as u32,
        rect(width - s(112), nav_y, s(88), s(36)),
    )?;
    add(
        h,
        SUMMARY,
        "",
        w!("STATIC"),
        0,
        rect(s(24), client.bottom - s(94), width - s(48), s(32)),
    )?;
    add(
        h,
        CANCEL,
        "取消",
        w!("BUTTON"),
        WS_TABSTOP.0 | BS_PUSHBUTTON as u32,
        rect(width - s(232), client.bottom - s(52), s(96), s(36)),
    )?;
    add(
        h,
        SAVE,
        "保存",
        w!("BUTTON"),
        WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32,
        rect(width - s(120), client.bottom - s(52), s(96), s(36)),
    )?;
    refresh(h);
    Ok(())
}
unsafe fn refresh(h: HWND) {
    let Some(p) = state(h) else {
        return;
    };
    let all = p.all.get();
    let page = p.page.get();
    let (labels, count, selected) = {
        let rows = p.rows.borrow();
        let labels = rows
            .iter()
            .skip(page * p.page_size)
            .take(p.page_size)
            .map(|row| (row.label.clone(), row.checked))
            .collect::<Vec<_>>();
        (
            labels,
            rows.len(),
            rows.iter().filter(|row| row.checked).count(),
        )
    };
    if let Ok(control) = GetDlgItem(h, ALL) {
        SendMessageW(control, BM_SETCHECK, WPARAM(usize::from(all)), LPARAM(0));
        let _ = InvalidateRect(control, None, false);
    }
    for index in 0..p.page_size {
        if let Ok(control) = GetDlgItem(h, FIRST_DISPLAY + index as i32) {
            let visible = index < labels.len();
            if let Some((caption, checked)) = labels.get(index) {
                set_text(control, caption);
                SendMessageW(
                    control,
                    BM_SETCHECK,
                    WPARAM(usize::from(*checked)),
                    LPARAM(0),
                );
            }
            let _ = EnableWindow(control, !all);
            let _ = ShowWindow(control, if visible { SW_SHOWNA } else { SW_HIDE });
            let _ = InvalidateRect(control, None, false);
        }
    }
    let pages = count.div_ceil(p.page_size).max(1);
    for (id, enabled) in [(PREVIOUS, page > 0), (NEXT, page + 1 < pages)] {
        if let Ok(control) = GetDlgItem(h, id) {
            let _ = EnableWindow(control, enabled);
        }
    }
    if let Ok(control) = GetDlgItem(h, PAGE) {
        set_text(control, &format!("{} / {}", page + 1, pages));
    }
    if let Ok(control) = GetDlgItem(h, SUMMARY) {
        set_text(
            control,
            if count == 0 {
                "当前未发现可选择的屏幕，请取消后重试。"
            } else if all {
                "已选择全部屏幕"
            } else if selected == 0 {
                "请至少选择一块屏幕。"
            } else {
                "未连接的已选屏幕会保留，重连后继续生效。"
            },
        );
    }
    if let Ok(control) = GetDlgItem(h, SAVE) {
        let _ = EnableWindow(control, count > 0 && (all || selected > 0));
    }
}

unsafe extern "system" fn proc(h: HWND, m: u32, wparam: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_NCCREATE {
        let create = &*(l.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(h, GWLP_USERDATA, create.lpCreateParams as isize);
        if let Some(p) = state(h) {
            p.handed_off.set(true);
        }
        return LRESULT(1);
    }
    let Some(p) = state(h) else {
        return DefWindowProcW(h, m, wparam, l);
    };
    match m {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(h, &mut ps);
            let mut area = RECT::default();
            let _ = GetClientRect(h, &mut area);
            FillRect(dc, &area, p.brush);
            let _ = EndPaint(h, &ps);
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let dc = HDC(wparam.0 as *mut _);
            SetTextColor(dc, p.colors.secondary);
            SetBkColor(dc, p.colors.bg);
            LRESULT(p.brush.0 as isize)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as i32;
            if id == CANCEL {
                close(h);
            } else if id == ALL {
                if let Ok(control) = GetDlgItem(h, ALL) {
                    p.all
                        .set(SendMessageW(control, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 != 0);
                }
                refresh(h);
            } else if (FIRST_DISPLAY..FIRST_DISPLAY + p.page_size as i32).contains(&id) {
                let index = p.page.get() * p.page_size + (id - FIRST_DISPLAY) as usize;
                if let Ok(control) = GetDlgItem(h, id) {
                    let checked = SendMessageW(control, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 != 0;
                    if let Some(row) = p.rows.borrow_mut().get_mut(index) {
                        row.checked = checked;
                    }
                }
                refresh(h);
            } else if id == PREVIOUS || id == NEXT {
                let pages = p.rows.borrow().len().div_ceil(p.page_size).max(1);
                p.page.set(if id == PREVIOUS {
                    p.page.get().saturating_sub(1)
                } else {
                    (p.page.get() + 1).min(pages - 1)
                });
                refresh(h);
            } else if id == SAVE {
                let ids = p
                    .rows
                    .borrow()
                    .iter()
                    .filter(|row| row.checked)
                    .map(|row| row.id.clone())
                    .collect::<Vec<_>>();
                if !p.rows.borrow().is_empty() && (p.all.get() || !ids.is_empty()) {
                    *p.result.borrow_mut() = Some(if p.all.get() { None } else { Some(ids) });
                    if PostMessageW(p.callback, p.message, WPARAM(0), LPARAM(h.0 as isize)).is_err()
                    {
                        *p.result.borrow_mut() = None;
                        if let Ok(control) = GetDlgItem(h, SUMMARY) {
                            set_text(control, "无法保存选择，请重试。");
                        }
                    }
                } else {
                    refresh(h);
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            close(h);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let owner = p.owner;
            let callback = p.callback;
            let message = p.message;
            let activate = p.activate;
            let previous_focus = p.previous_focus;
            if p.modal.get() {
                let _ = EnableWindow(owner, true);
            }
            SetWindowLongPtrW(h, GWLP_USERDATA, 0);
            drop(Box::from_raw(p as *const Picker as *mut Picker));
            let _ = PostMessageW(callback, message, WPARAM(1), LPARAM(h.0 as isize));
            if activate && IsWindowVisible(owner).as_bool() {
                let _ = SetForegroundWindow(owner);
                if IsWindow(previous_focus).as_bool() {
                    let _ = SetFocus(previous_focus);
                }
            }
            DefWindowProcW(h, m, wparam, l)
        }
        _ => DefWindowProcW(h, m, wparam, l),
    }
}

unsafe extern "system" fn control_proc(
    h: HWND,
    m: u32,
    wparam: WPARAM,
    l: LPARAM,
    _id: usize,
    root: usize,
) -> LRESULT {
    if m == WM_PAINT {
        if let Some(p) = state(HWND(root as *mut _)) {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(h, &mut ps);
            let mut r = RECT::default();
            let _ = GetClientRect(h, &mut r);
            FillRect(dc, &r, p.brush);
            let id = GetDlgCtrlID(h);
            let enabled = windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(h).as_bool();
            let ink = if enabled {
                p.colors.ink
            } else {
                p.colors.disabled
            };
            if id == ALL || id >= FIRST_DISPLAY {
                let checked = SendMessageW(h, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 != 0;
                let edge = p.scale(20);
                let square = rect(p.scale(2), (r.bottom - edge) / 2, edge, edge);
                let _ = crate::antialias::shape(
                    dc,
                    square,
                    4.0,
                    Some(if checked {
                        p.colors.primary
                    } else {
                        p.colors.surface
                    }),
                    Some(p.colors.border),
                    false,
                );
                if checked {
                    label(
                        dc,
                        square,
                        "✓",
                        p.body,
                        COLORREF(0xffffff),
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                    );
                }
                r.left += p.scale(34);
                label(
                    dc,
                    r,
                    &text(h),
                    p.body,
                    ink,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                );
            } else {
                let pressed = SendMessageW(h, BM_GETSTATE, WPARAM(0), LPARAM(0)).0 & 4 != 0;
                let primary = id == SAVE;
                let fill = if !enabled {
                    p.colors.bg
                } else if primary {
                    if pressed {
                        p.colors.primary_pressed
                    } else {
                        p.colors.primary
                    }
                } else if pressed {
                    p.colors.pressed
                } else {
                    p.colors.surface
                };
                let _ =
                    crate::antialias::shape(dc, r, 6.0, Some(fill), Some(p.colors.border), false);
                label(
                    dc,
                    r,
                    &text(h),
                    p.body,
                    if primary && enabled {
                        COLORREF(0xffffff)
                    } else {
                        ink
                    },
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
            }
            let _ = EndPaint(h, &ps);
            return LRESULT(0);
        }
    }
    let result = DefSubclassProc(h, m, wparam, l);
    if matches!(
        m,
        WM_SETFOCUS
            | WM_KILLFOCUS
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_KEYDOWN
            | WM_KEYUP
            | BM_SETSTATE
            | BM_SETCHECK
            | WM_ENABLE
    ) {
        let _ = InvalidateRect(h, None, false);
    }
    result
}
