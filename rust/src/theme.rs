//! Panel colors and read-only Windows application-theme preference.
use std::path::Path;
use windows::{
    core::w,
    Win32::{Foundation::COLORREF, System::Registry::*},
};

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub bg: COLORREF,
    pub surface: COLORREF,
    pub ink: COLORREF,
    pub secondary: COLORREF,
    pub line: COLORREF,
    pub border: COLORREF,
    pub hover: COLORREF,
    pub pressed: COLORREF,
    pub caption_hover: COLORREF,
    pub disabled: COLORREF,
    pub accent: COLORREF,
    pub primary: COLORREF,
    pub primary_pressed: COLORREF,
    pub track: COLORREF,
    pub off: COLORREF,
}
pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            dark,
            bg: COLORREF(0x202020),
            surface: COLORREF(0x2B2B2B),
            ink: COLORREF(0xF1F1F1),
            secondary: COLORREF(0xB8B8B8),
            line: COLORREF(0x444444),
            border: COLORREF(0x777777),
            hover: COLORREF(0x363636),
            pressed: COLORREF(0x404040),
            caption_hover: COLORREF(0x383838),
            disabled: COLORREF(0x929292),
            accent: COLORREF(0x7373FF),
            primary: COLORREF(0x3535C5),
            primary_pressed: COLORREF(0x2B2BA8),
            track: COLORREF(0x696969),
            off: COLORREF(0xB8B8B8),
        }
    } else {
        Palette {
            dark,
            bg: COLORREF(0xF3F3F3),
            surface: COLORREF(0xFFFFFF),
            ink: COLORREF(0x202020),
            secondary: COLORREF(0x666666),
            line: COLORREF(0xE5E5E5),
            border: COLORREF(0xD3D3D3),
            hover: COLORREF(0xF7F7F7),
            pressed: COLORREF(0xF0F0F0),
            caption_hover: COLORREF(0xE8E8E8),
            disabled: COLORREF(0x777777),
            accent: COLORREF(0x4B4BEB),
            primary: COLORREF(0x3535CB),
            primary_pressed: COLORREF(0x2B2BA8),
            track: COLORREF(0xDCDCDC),
            off: COLORREF(0x666666),
        }
    }
}

pub fn system_dark(test_directory: Option<&Path>) -> bool {
    // Test injection is confined to --ui-test's isolated directory; never change
    // the user's registry to validate theme notifications.
    if let Some(dir) = test_directory {
        if let Ok(value) = std::fs::read_to_string(dir.join("system-theme")) {
            match value.trim() {
                "dark" => return true,
                "light" => return false,
                _ => {}
            }
        }
    }
    let mut light = 1u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    unsafe {
        let code = RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut light as *mut u32).cast()),
            Some(&mut size),
        );
        code.0 == 0 && light == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn contrast(a: COLORREF, b: COLORREF) -> f64 {
        let luminance = |color: COLORREF| {
            [0.2126, 0.7152, 0.0722]
                .iter()
                .enumerate()
                .map(|(i, weight)| {
                    let value = f64::from((color.0 >> (i * 8)) & 255) / 255.0;
                    weight
                        * if value <= 0.04045 {
                            value / 12.92
                        } else {
                            ((value + 0.055) / 1.055).powf(2.4)
                        }
                })
                .sum::<f64>()
        };
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }
    #[test]
    fn panel_text_and_primary_action_remain_readable_in_both_themes() {
        for dark in [false, true] {
            let colors = palette(dark);
            for surface in [colors.bg, colors.surface] {
                assert!(contrast(colors.ink, surface) >= 4.5);
                assert!(contrast(colors.secondary, surface) >= 4.5);
            }
            assert!(contrast(COLORREF(0xFFFFFF), colors.primary) >= 4.5);
            assert!(contrast(colors.accent, colors.surface) >= 3.0);
        }
    }
}
