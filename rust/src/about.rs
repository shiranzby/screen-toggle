//! A quiet, themed About window. No alert/message-box sound is invoked.
use crate::{theme::Palette, win::*};
use std::mem::size_of;
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        UI::{
            Input::KeyboardAndMouse::{EnableWindow, SetFocus},
            Shell::{DefSubclassProc, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
};

pub const FIRST_LINE: &str = "一键熄屏 · Rust 0.3.0";
const DESCRIPTION: &str = "本应用使用 Rust 编写，通过 Windows 接口实现软件熄屏。\r\n\r\n熄屏时覆盖全屏黑窗，并通过 DDC/CI 调低外接屏、通过 WMI 调低内置屏背光。黑窗可穿透点击，并从屏幕捕获中排除，便于远程操作。\r\n\r\n亮屏时恢复熄屏前的亮度和光标；启用关闭 HDR 时，也会还原原有 HDR 状态。\r\n\r\n使用设置中的全局热键或托盘菜单切换，再次执行即可恢复屏幕。显示器需支持相应的亮度接口，调低背光并非切断电源。";
struct About {
    owner: HWND,
    colors: Palette,
    brush: HBRUSH,
    title: HFONT,
    body: HFONT,
    activate: bool,
}
impl Drop for About {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.brush);
            let _ = DeleteObject(self.title);
            let _ = DeleteObject(self.body);
        }
    }
}
unsafe fn state(h: HWND) -> Option<&'static About> {
    (GetWindowLongPtrW(h, GWLP_USERDATA) as *const About).as_ref()
}
pub unsafe fn show(owner: HWND, colors: Palette, activate: bool) -> Result<HWND, String> {
    let test_area = crate::panel::test_workarea(!activate)?;
    register("SOGRustAbout", Some(proc), HBRUSH::default())?;
    let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(owner).max(96);
    let s = |v: i32| (v as i64 * i64::from(dpi) / 96) as i32;
    let about = Box::new(About {
        owner,
        colors,
        brush: CreateSolidBrush(colors.bg),
        title: font(s(18), 600),
        body: font(s(15), 400),
        activate,
    });
    let mut area = RECT::default();
    if let Some(target) = test_area {
        area = target;
    } else if !IsWindowVisible(owner).as_bool() || GetWindowRect(owner, &mut area).is_err() {
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&mut area as *mut RECT).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    let width = s(520);
    let height = s(420);
    let raw = Box::into_raw(about);
    let h = CreateWindowExW(
        WS_EX_DLGMODALFRAME | WS_EX_COMPOSITED,
        w!("SOGRustAbout"),
        w!("关于一键熄屏"),
        WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN,
        area.left + ((area.right - area.left - width) / 2).max(0),
        area.top + ((area.bottom - area.top - height) / 2).max(0),
        width,
        height,
        owner,
        None,
        instance(),
        Some(raw.cast()),
    )
    .map_err(|e| e.to_string())?;
    let p = state(h).ok_or("关于窗口初始化失败")?;
    let dark = BOOL::from(colors.dark);
    let _ = DwmSetWindowAttribute(
        h,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        (&dark as *const BOOL).cast(),
        size_of::<BOOL>() as u32,
    );
    let mut client = RECT::default();
    let _ = GetClientRect(h, &mut client);
    let children = (|| -> Result<HWND, String> {
        for (id, text, y, height, font) in [
            (10, FIRST_LINE, s(20), s(32), p.title),
            (11, DESCRIPTION, s(66), client.bottom - s(126), p.body),
        ] {
            let control = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                PCWSTR(wide(text).as_ptr()),
                WS_CHILD | WS_VISIBLE,
                s(24),
                y,
                client.right - s(48),
                height,
                h,
                HMENU(id as usize as *mut _),
                instance(),
                None,
            )
            .map_err(|e| e.to_string())?;
            SendMessageW(control, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(0));
        }
        let button = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("确定"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            client.right - s(120),
            client.bottom - s(52),
            s(96),
            s(36),
            h,
            HMENU(IDOK.0 as usize as *mut _),
            instance(),
            None,
        )
        .map_err(|e| e.to_string())?;
        SendMessageW(button, WM_SETFONT, WPARAM(p.body.0 as usize), LPARAM(0));
        let _ = SetWindowSubclass(button, Some(button_proc), 1, h.0 as usize);
        Ok(button)
    })();
    let button = match children {
        Ok(button) => button,
        Err(error) => {
            let _ = DestroyWindow(h);
            return Err(error);
        }
    };
    let _ = EnableWindow(owner, false);
    let _ = ShowWindow(h, if activate { SW_SHOW } else { SW_SHOWNOACTIVATE });
    if activate {
        let _ = SetForegroundWindow(h);
        let _ = SetFocus(button);
    }
    Ok(h)
}
unsafe extern "system" fn proc(h: HWND, m: u32, wparam: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_NCCREATE {
        let cs = &*(l.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(h, GWLP_USERDATA, cs.lpCreateParams as isize);
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
            let mut r = RECT::default();
            let _ = GetClientRect(h, &mut r);
            FillRect(dc, &r, p.brush);
            let _ = EndPaint(h, &ps);
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let dc = HDC(wparam.0 as *mut _);
            SetTextColor(dc, p.colors.ink);
            SetBkColor(dc, p.colors.bg);
            LRESULT(p.brush.0 as isize)
        }
        WM_COMMAND if [IDOK.0, IDCANCEL.0].contains(&(wparam.0 as u16 as i32)) => {
            let _ = PostMessageW(h, WM_CLOSE, WPARAM(0), LPARAM(0));
            LRESULT(0)
        }
        WM_CLOSE => {
            let owner = p.owner;
            let activate = p.activate;
            let _ = EnableWindow(owner, true);
            let _ = DestroyWindow(h);
            if activate && IsWindowVisible(owner).as_bool() {
                let _ = SetForegroundWindow(owner);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let owner = p.owner;
            let _ = EnableWindow(owner, true);
            SetWindowLongPtrW(h, GWLP_USERDATA, 0);
            drop(Box::from_raw(p as *const About as *mut About));
            DefWindowProcW(h, m, wparam, l)
        }
        _ => DefWindowProcW(h, m, wparam, l),
    }
}
unsafe extern "system" fn button_proc(
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
            let _ = crate::antialias::shape(
                dc,
                r,
                6.0,
                Some(p.colors.surface),
                Some(p.colors.border),
                false,
            );
            label(
                dc,
                r,
                "确定",
                p.body,
                p.colors.ink,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
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
    ) {
        let _ = InvalidateRect(h, None, false);
    }
    result
}
