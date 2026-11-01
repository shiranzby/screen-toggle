//! Optional antialiasing for small control shapes. One CPU target is reused on
//! the window thread; no GPU device or full-window backing surface is retained.
use std::cell::RefCell;
use windows::Win32::{
    Foundation::{COLORREF, RECT},
    Graphics::{
        Direct2D::{Common::*, *},
        Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
        Gdi::{GetPixel, HDC},
    },
};

struct Painter {
    _factory: ID2D1Factory,
    target: ID2D1DCRenderTarget,
    brush: ID2D1SolidColorBrush,
}
thread_local! {static PAINTER:RefCell<Option<Painter>>=const{RefCell::new(None)};}
pub fn shutdown() {
    // Release Direct2D on the live UI thread. Deferring COM/GDI cleanup to
    // process TLS teardown can fail after Windows has started thread cleanup.
    PAINTER.with(|cell| drop(cell.borrow_mut().take()));
}
fn color(c: COLORREF) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: (c.0 & 255) as f32 / 255.,
        g: ((c.0 >> 8) & 255) as f32 / 255.,
        b: ((c.0 >> 16) & 255) as f32 / 255.,
        a: 1.,
    }
}
impl Painter {
    unsafe fn new() -> windows::core::Result<Self> {
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let target = factory.CreateDCRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.,
            dpiY: 96.,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        })?;
        let brush = target.CreateSolidColorBrush(&color(COLORREF(0)), None)?;
        Ok(Self {
            _factory: factory,
            target,
            brush,
        })
    }
}
pub unsafe fn shape(
    dc: HDC,
    r: RECT,
    radius: f32,
    fill: COLORREF,
    border: COLORREF,
    ellipse: bool,
) -> bool {
    if r.right <= r.left || r.bottom <= r.top {
        return false;
    }
    // Snapshot the existing background so GDI's clip region and opaque controls
    // keep their correct surface color at antialiased edges.
    let background = GetPixel(dc, r.left, r.top);
    PAINTER.with(|cell| {
        let mut painter = cell.borrow_mut();
        if painter.is_none() {
            *painter = Painter::new().ok();
        }
        let Some(p) = painter.as_ref() else {
            return false;
        };
        if p.target.BindDC(dc, &r).is_err() {
            *painter = None;
            return false;
        }
        p.target.SetDpi(96., 96.);
        p.target.BeginDraw();
        p.target.Clear(Some(&color(background)));
        let bounds = D2D_RECT_F {
            left: 0.5,
            top: 0.5,
            right: (r.right - r.left) as f32 - 0.5,
            bottom: (r.bottom - r.top) as f32 - 0.5,
        };
        p.brush.SetColor(&color(fill));
        if ellipse {
            let e = D2D1_ELLIPSE {
                point: D2D_POINT_2F {
                    x: (bounds.right + 0.5) / 2.,
                    y: (bounds.bottom + 0.5) / 2.,
                },
                radiusX: bounds.right / 2.,
                radiusY: bounds.bottom / 2.,
            };
            p.target.FillEllipse(&e, &p.brush);
            p.brush.SetColor(&color(border));
            p.target.DrawEllipse(&e, &p.brush, 1., None);
        } else {
            let rounded = D2D1_ROUNDED_RECT {
                rect: bounds,
                radiusX: radius,
                radiusY: radius,
            };
            p.target.FillRoundedRectangle(&rounded, &p.brush);
            p.brush.SetColor(&color(border));
            p.target.DrawRoundedRectangle(&rounded, &p.brush, 1., None);
        }
        if p.target.EndDraw(None, None).is_err() {
            *painter = None;
            return false;
        }
        true
    })
}
