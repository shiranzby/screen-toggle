//! Antialiased control geometry; native controls continue to own input and focus.
use std::cell::{Cell, RefCell};
use windows::Win32::{
    Foundation::{COLORREF, RECT},
    Graphics::{Gdi::HDC, GdiPlus as gp},
};

struct Startup(usize);
impl Startup {
    unsafe fn new() -> Option<Self> {
        let mut token = 0;
        let input = gp::GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        if gp::GdiplusStartup(&mut token, &input, std::ptr::null_mut()) == gp::Ok && token != 0 {
            Some(Self(token))
        } else {
            None
        }
    }
}
impl Drop for Startup {
    fn drop(&mut self) {
        unsafe { gp::GdiplusShutdown(self.0) }
    }
}
thread_local! {
    static STARTUP: RefCell<Option<Startup>> = const { RefCell::new(None) };
}

/// Release the runtime on its owning UI thread before Windows thread teardown.
pub fn shutdown() {
    STARTUP.with(|slot| drop(slot.borrow_mut().take()));
}

struct Resource<'a, T> {
    ptr: *mut T,
    delete: unsafe fn(*mut T) -> gp::Status,
    cleanup_failed: &'a Cell<bool>,
}
impl<'a, T> Resource<'a, T> {
    fn new(delete: unsafe fn(*mut T) -> gp::Status, cleanup_failed: &'a Cell<bool>) -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            delete,
            cleanup_failed,
        }
    }
}
impl<T> Drop for Resource<'_, T> {
    fn drop(&mut self) {
        if !self.ptr.is_null() && unsafe { (self.delete)(self.ptr) } != gp::Ok {
            self.cleanup_failed.set(true);
        }
    }
}
fn checked(status: gp::Status) -> Option<()> {
    (status == gp::Ok).then_some(())
}
fn argb(color: COLORREF) -> u32 {
    0xff00_0000 | ((color.0 & 0xff) << 16) | (color.0 & 0xff00) | ((color.0 >> 16) & 0xff)
}
unsafe fn delete_solid(brush: *mut gp::GpSolidFill) -> gp::Status {
    gp::GdipDeleteBrush(brush.cast())
}

/// Draw within an existing HDC and its clip region. Radius is a true radius in
/// device pixels. A one-pixel outline is inset by half a pixel on every side.
/// Returns false on an invalid extent or any GDI+ failure, allowing GDI fallback.
pub unsafe fn shape(
    dc: HDC,
    area: RECT,
    radius: f32,
    fill: Option<COLORREF>,
    border: Option<COLORREF>,
    ellipse: bool,
) -> bool {
    let width = i64::from(area.right) - i64::from(area.left);
    let height = i64::from(area.bottom) - i64::from(area.top);
    if dc.0.is_null() || width <= 0 || height <= 0 || !radius.is_finite() {
        return false;
    }
    if fill.is_none() && border.is_none() {
        return true;
    }
    let inset = if border.is_some() { 0.5 } else { 0.0 };
    let (x, y) = (area.left as f32 + inset, area.top as f32 + inset);
    let (width, height) = (width as f32 - 2.0 * inset, height as f32 - 2.0 * inset);
    if width <= 0.0 || height <= 0.0 {
        return false;
    }
    STARTUP.with(|slot| {
        let mut startup = slot.borrow_mut();
        if startup.is_none() {
            *startup = Startup::new();
        }
        if startup.is_none() {
            return false;
        }
        let cleanup_failed = Cell::new(false);
        let drawn = (|| -> Option<()> {
            // Graphics created from an HDC retain the HDC clip and viewport;
            // never clear or replace the caller's excluded edit/client regions.
            let mut graphics = Resource::new(gp::GdipDeleteGraphics, &cleanup_failed);
            checked(gp::GdipCreateFromHDC(dc, &mut graphics.ptr))?;
            if graphics.ptr.is_null() {
                return None;
            }
            checked(gp::GdipSetPageUnit(graphics.ptr, gp::UnitPixel))?;
            checked(gp::GdipSetSmoothingMode(
                graphics.ptr,
                gp::SmoothingModeAntiAlias,
            ))?;
            checked(gp::GdipSetPixelOffsetMode(
                graphics.ptr,
                gp::PixelOffsetModeHalf,
            ))?;

            let mut brush = Resource::new(delete_solid, &cleanup_failed);
            if let Some(color) = fill {
                checked(gp::GdipCreateSolidFill(argb(color), &mut brush.ptr))?;
                if brush.ptr.is_null() {
                    return None;
                }
            }
            let mut pen = Resource::new(gp::GdipDeletePen, &cleanup_failed);
            if let Some(color) = border {
                checked(gp::GdipCreatePen1(
                    argb(color),
                    1.0,
                    gp::UnitPixel,
                    &mut pen.ptr,
                ))?;
                if pen.ptr.is_null() {
                    return None;
                }
            }
            if ellipse {
                if fill.is_some() {
                    checked(gp::GdipFillEllipse(
                        graphics.ptr,
                        brush.ptr.cast(),
                        x,
                        y,
                        width,
                        height,
                    ))?;
                }
                if border.is_some() {
                    checked(gp::GdipDrawEllipse(
                        graphics.ptr,
                        pen.ptr,
                        x,
                        y,
                        width,
                        height,
                    ))?;
                }
            } else {
                let mut path = Resource::new(gp::GdipDeletePath, &cleanup_failed);
                checked(gp::GdipCreatePath(gp::FillModeWinding, &mut path.ptr))?;
                if path.ptr.is_null() {
                    return None;
                }
                let radius = radius.max(0.0).min(width / 2.0).min(height / 2.0);
                if radius == 0.0 {
                    checked(gp::GdipAddPathRectangle(path.ptr, x, y, width, height))?;
                } else {
                    let diameter = radius * 2.0;
                    for (left, top, angle) in [
                        (x, y, 180.0),
                        (x + width - diameter, y, 270.0),
                        (x + width - diameter, y + height - diameter, 0.0),
                        (x, y + height - diameter, 90.0),
                    ] {
                        checked(gp::GdipAddPathArc(
                            path.ptr, left, top, diameter, diameter, angle, 90.0,
                        ))?;
                    }
                    checked(gp::GdipClosePathFigure(path.ptr))?;
                }
                if fill.is_some() {
                    checked(gp::GdipFillPath(graphics.ptr, brush.ptr.cast(), path.ptr))?;
                }
                if border.is_some() {
                    checked(gp::GdipDrawPath(graphics.ptr, pen.ptr, path.ptr))?;
                }
            }
            Some(())
        })();
        drawn.is_some() && !cleanup_failed.get()
    })
}
