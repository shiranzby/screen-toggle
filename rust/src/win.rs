use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::*, Graphics::Gdi::*, System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::*,
    },
};

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
pub fn instance() -> HINSTANCE {
    unsafe { HINSTANCE(GetModuleHandleW(PCWSTR::null()).expect("process module").0) }
}
pub fn text(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 2048];
        let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..n])
    }
}
pub fn set_text(hwnd: HWND, value: &str) {
    // WM_SETTEXT resets native EDIT selection/caret even for identical text.
    if text(hwnd) == value {
        return;
    }
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(wide(value).as_ptr()));
    }
}
pub fn icon(px: i32) -> HICON {
    unsafe {
        LoadImageW(
            instance(),
            // MAKEINTRESOURCEW(1): an integer resource ID, never dereferenced.
            PCWSTR(std::ptr::without_provenance(1)),
            IMAGE_ICON,
            px,
            px,
            LR_SHARED,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_else(|_| LoadIconW(None, IDI_APPLICATION).unwrap_or_default())
    }
}
pub unsafe fn register(name: &str, proc: WNDPROC, background: HBRUSH) -> Result<(), String> {
    let class_name = wide(name);
    let wc = WNDCLASSW {
        lpfnWndProc: proc,
        hInstance: instance(),
        hIcon: icon(32),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: background,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
        return Err(windows::core::Error::from_win32().to_string());
    }
    Ok(())
}
pub fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}
pub unsafe fn label(
    dc: HDC,
    area: RECT,
    text: &str,
    font: HFONT,
    color: COLORREF,
    flags: DRAW_TEXT_FORMAT,
) {
    let old = SelectObject(dc, font);
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, color);
    let mut s: Vec<u16> = text.encode_utf16().collect();
    let mut area = area;
    DrawTextW(dc, &mut s, &mut area, flags | DT_NOPREFIX);
    SelectObject(dc, old);
}
pub unsafe fn font(px: i32, weight: i32) -> HFONT {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    if SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        metrics.cbSize,
        Some((&mut metrics as *mut NONCLIENTMETRICSW).cast()),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_ok()
    {
        let mut face = metrics.lfMessageFont;
        face.lfHeight = -px;
        face.lfWidth = 0;
        face.lfWeight = weight;
        face.lfQuality = CLEARTYPE_QUALITY;
        return CreateFontIndirectW(&face);
    }
    CreateFontW(
        -px,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        1,
        0,
        0,
        5,
        0,
        w!("Microsoft YaHei UI"),
    )
}
