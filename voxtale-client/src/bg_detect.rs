//! 背景亮度检测: GDI 截屏窗口所在区域, 采样求加权亮度 (与 Python 版同款 0.299/0.587/0.114)。
//! >= 150 视为浅色背景。窗口未创建时调用 (启动检测) 或窗口隐藏后调用 (A 按钮重检测)。
//!
//! 读取路径说明 (实测于本机):
//! - GetDIBits / CreateDIBSection 在 windows-rs 0.58 下均失败 (GetDIBits 恒返 0,
//!   CreateDIBSection 报 ERROR_INVALID_HANDLE), 弃用;
//! - 可行链路: memDC + CreateCompatibleBitmap + BitBlt + GetPixel (内存 DC 上逐点采样),
//!   采样网格 ~300 点, 耗时微秒级, 足够亮度估计。

use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetPixel,
    ReleaseDC, SelectObject, SRCCOPY,
};

pub fn luminance(x: i32, y: i32, w: i32, h: i32) -> Option<f32> {
    if w <= 0 || h <= 0 {
        return None;
    }
    unsafe {
        let hdc_screen = GetDC(None);
        let hdc_mem = CreateCompatibleDC(hdc_screen);
        let mut result = None;

        let hbmp = CreateCompatibleBitmap(hdc_screen, w, h);
        if hbmp.is_invalid() {
            ReleaseDC(None, hdc_screen);
            return None;
        }
        let old = SelectObject(hdc_mem, hbmp);

        if BitBlt(hdc_mem, 0, 0, w, h, hdc_screen, x, y, SRCCOPY).is_ok() {
            // 采样网格: 短边约 24 点、长边按比例, 上限 ~1000 点
            let step_x = (w / 24).max(1);
            let step_y = (h / 24).max(1).max(step_x / 4);
            let (mut sum, mut n) = (0f64, 0u64);
            let mut yy = 0;
            while yy < h {
                let mut xx = 0;
                while xx < w {
                    let c = GetPixel(hdc_mem, xx, yy).0;
                    if c != 0xFFFFFFFF {
                        let (b, g, r) =
                            ((c & 0xFF) as f64, ((c >> 8) & 0xFF) as f64, ((c >> 16) & 0xFF) as f64);
                        sum += 0.299 * r + 0.587 * g + 0.114 * b;
                        n += 1;
                    }
                    xx += step_x;
                }
                yy += step_y;
            }
            if n > 0 {
                result = Some((sum / n as f64) as f32);
            }
        }

        SelectObject(hdc_mem, old);
        let _ = DeleteObject(hbmp);
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);
        result
    }
}
