//! 光标隐藏/恢复(与黑窗对齐: 熄屏后立即隐藏, 亮屏前恢复)
use std::mem::zeroed;

use windows::Win32::Foundation::{HANDLE, HINSTANCE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateCursor, DestroyCursor, LoadCursorW, SetCursor, SetSystemCursor, SystemParametersInfoW,
    HCURSOR, IDC_ARROW, SPI_SETCURSORS, SPIF_SENDCHANGE, SYSTEM_CURSOR_ID,
};

// 9 个系统用途的光标(只换箭头会漏: 输入框变 I 型、窗口边缘变调整箭头)
const OCR_IDS: [SYSTEM_CURSOR_ID; 9] = [
    SYSTEM_CURSOR_ID(32512), SYSTEM_CURSOR_ID(32513), SYSTEM_CURSOR_ID(32514),
    SYSTEM_CURSOR_ID(32649), SYSTEM_CURSOR_ID(32646), SYSTEM_CURSOR_ID(32645),
    SYSTEM_CURSOR_ID(32644), SYSTEM_CURSOR_ID(32648), SYSTEM_CURSOR_ID(32650),
];

static mut HIDDEN: bool = false;

fn log_line(s: &str) {
    use std::io::Write;
    let mut p = std::env::temp_dir();
    p.push("sog_rust.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(f, "{}", s);
    }
}

pub unsafe fn hide() {
    if HIDDEN {
        return;
    }
    let t0 = std::time::Instant::now();
    let size = 32i32;
    let nb = (size * size) / 8;
    let and_mask: Vec<u8> = vec![0xFF; nb as usize];
    let xor_mask: Vec<u8> = vec![0u8; nb as usize];

    let mut n = 0;
    for ocr in OCR_IDS.iter() {
        let h = CreateCursor(
            HINSTANCE(std::ptr::null_mut()),
            0,
            0,
            size,
            size,
            and_mask.as_ptr() as *const core::ffi::c_void,
            xor_mask.as_ptr() as *const core::ffi::c_void,
        );
        let hc = match h {
            Ok(c) => c,
            Err(_) => continue,
        };
        if hc.0.is_null() {
            continue;
        }
        if SetSystemCursor(hc, *ocr).is_ok() {
            n += 1;
        } else {
            let _ = DestroyCursor(hc);
        }
    }
    HIDDEN = true;
    log_line(&format!(
        "cursor hidden {}/{} in {} ms",
        n,
        OCR_IDS.len(),
        t0.elapsed().as_millis()
    ));
}

pub unsafe fn restore() {
    if !HIDDEN {
        return;
    }
    let t0 = std::time::Instant::now();
    // SPI_SETCURSORS 的 pvparam 必须指向 0(表示"重置"), 传 None 时系统
    // 认为没有参数, 光标不会恢复 —— 这是之前"鼠标未恢复"的根因
    let mut zero: u32 = 0;
    let _ = SystemParametersInfoW(
        SPI_SETCURSORS,
        0,
        Some(&mut zero as *mut u32 as *mut core::ffi::c_void),
        SPIF_SENDCHANGE,
    );
    // 立即刷新当前显示的光标形状
    if let Ok(h) = LoadCursorW(HINSTANCE(std::ptr::null_mut()), IDC_ARROW) {
        let _ = SetCursor(h);
    }
    HIDDEN = false;
    log_line(&format!("cursor restored in {} ms", t0.elapsed().as_millis()));
}

pub unsafe fn cleanup() {
    restore();
}
