//! Отдача кадра системе с альфой на каждый пиксель.
//!
//! Это единственный способ иметь одновременно настоящую прозрачность и
//! сквозные клики. Причина в том, как Windows складывает окна: сквозной
//! хит-тест доступен только слоёному окну (`WS_EX_LAYERED`), а альфу слоя,
//! заданную `SetLayeredWindowAttributes`, система применяет к поверхности,
//! мимо которой рисует видеокарта, — окно выходит сплошным. Здесь картинку
//! отдаём мы сами, поэтому альфа наша и работает как надо.
//!
//! Проверено на стенде `dev/probe --mode perpixel`: окно с градиентом по альфе
//! просвечивает, и клик сквозь него доходит до чужого процесса.

use windows::Win32::Foundation::{COLORREF, HWND, POINT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
    HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

/// Буфер под кадр. Держится между кадрами: пересоздавать DIB на каждый кадр
/// дороже самой отрисовки.
pub struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
}

impl Surface {
    fn new(width: i32, height: i32) -> Option<Self> {
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);

            let mut info = BITMAPINFO::default();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = width;
            // Отрицательная высота — строки сверху вниз, как у egui.
            info.bmiHeader.biHeight = -height;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB.0;

            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
                .ok()
                .filter(|_| !bits.is_null())?;
            let old = SelectObject(dc, bitmap.into());
            Some(Self { dc, bitmap, old, bits: bits as *mut u8, width, height })
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

// Указатель внутрь DIB принадлежит нам и живёт ровно столько же, сколько сам
// буфер; ни на какой другой поток он не уходит.
unsafe impl Send for Surface {}

#[derive(Default)]
pub struct Layered {
    surface: Option<Surface>,
}

impl Layered {
    /// Показывает кадр. `pixels` — RGBA сверху вниз, альфа не домножена.
    ///
    /// Возвращает false, если система кадр не приняла: в этом случае окно
    /// останется с прошлой картинкой, и это лучше, чем мигание.
    pub fn present(&mut self, h: HWND, pixels: &[u8], width: usize, height: usize) -> bool {
        let (w, ht) = (width as i32, height as i32);
        if w <= 0 || ht <= 0 || pixels.len() < width * height * 4 {
            return false;
        }
        let stale = self.surface.as_ref().is_none_or(|s| s.width != w || s.height != ht);
        if stale {
            self.surface = Surface::new(w, ht);
        }
        let Some(surface) = &self.surface else { return false };

        // GDI ждёт BGRA с цветом, уже домноженным на альфу.
        unsafe {
            let dst = std::slice::from_raw_parts_mut(surface.bits, width * height * 4);
            for i in (0..width * height * 4).step_by(4) {
                let a = pixels[i + 3] as u32;
                dst[i] = (pixels[i + 2] as u32 * a / 255) as u8;
                dst[i + 1] = (pixels[i + 1] as u32 * a / 255) as u8;
                dst[i + 2] = (pixels[i] as u32 * a / 255) as u8;
                dst[i + 3] = a as u8;
            }

            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let size = SIZE { cx: w, cy: ht };
            let src = POINT { x: 0, y: 0 };
            UpdateLayeredWindow(
                h,
                None,
                None,
                Some(&size),
                Some(surface.dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
            .is_ok()
        }
    }
}
