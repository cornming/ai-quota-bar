//! 把工作列上的內容畫成預乘 alpha 的 BGRA 像素，交給 UpdateLayeredWindow。
//!
//! GDI 畫字不帶 alpha，所以先在黑底畫白字當遮罩，用亮度當覆蓋率再上色。

use crate::http::wide;
use crate::model::{level, Level};
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rgb(pub f32, pub f32, pub f32);

const fn hex(v: u32) -> Rgb {
    Rgb(((v >> 16) & 255) as f32 / 255.0, ((v >> 8) & 255) as f32 / 255.0, (v & 255) as f32 / 255.0)
}

pub struct Palette {
    pub name: Rgb,
    pub text: Rgb,
    pub track: (Rgb, f32),
    pub bar: Rgb,
    pub warn: Rgb,
    pub crit: Rgb,
    pub hover: (Rgb, f32),
    pub stale: f32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            name: hex(0xC8C8C8),
            text: hex(0xFFFFFF),
            track: (hex(0xFFFFFF), 0.18),
            bar: hex(0x60CDFF),
            warn: hex(0xF5A35C),
            crit: hex(0xFF7B72),
            hover: (hex(0xFFFFFF), 0.08),
            stale: 0.45,
        }
    } else {
        Palette {
            name: hex(0x5A5A5A),
            text: hex(0x1B1B1B),
            track: (hex(0x000000), 0.14),
            bar: hex(0x005FB8),
            warn: hex(0xB4500F),
            crit: hex(0xC42B1C),
            hover: (hex(0xFFFFFF), 0.55),
            stale: 0.45,
        }
    }
}

/// 一列：「5h 16%」加一條細進度條。
#[derive(Clone, PartialEq, Debug)]
pub struct Row {
    pub label: String,
    pub used: Option<f64>,
}

/// 一家：名稱在左，最多兩列數值在右。
#[derive(Clone, PartialEq, Debug)]
pub struct Block {
    pub name: String,
    pub rows: Vec<Row>,
    /// 資料過期（登入過期或暫時錯誤）時淡化顯示。
    pub stale: bool,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Model {
    pub blocks: Vec<Block>,
    pub dark: bool,
    pub hover: bool,
}

pub struct Frame {
    pub w: i32,
    pub h: i32,
    pub px: Vec<[u8; 4]>,
}

struct Canvas {
    w: i32,
    h: i32,
    /// 未預乘的 RGBA（0–1），最後再轉換。
    px: Vec<[f32; 4]>,
}

impl Canvas {
    fn new(w: i32, h: i32) -> Self {
        Canvas { w, h, px: vec![[0.0; 4]; (w * h) as usize] }
    }
    fn blend(&mut self, x: i32, y: i32, c: Rgb, a: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h || a <= 0.0 {
            return;
        }
        let p = &mut self.px[(y * self.w + x) as usize];
        let out_a = a + p[3] * (1.0 - a);
        if out_a <= 0.0 {
            return;
        }
        let mix = |src: f32, dst: f32| (src * a + dst * p[3] * (1.0 - a)) / out_a;
        *p = [mix(c.0, p[0]), mix(c.1, p[1]), mix(c.2, p[2]), out_a];
    }
    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: Rgb, a: f32) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.blend(xx, yy, c, a);
            }
        }
    }
    fn into_frame(self) -> Frame {
        let px = self
            .px
            .iter()
            .map(|p| {
                // 完全透明的像素滑鼠會穿透，保留最低 alpha 讓整塊都能點。
                let a = p[3].max(1.0 / 255.0);
                let q = |v: f32| (v * a * 255.0).round().clamp(0.0, 255.0) as u8;
                [q(p[2]), q(p[1]), q(p[0]), (a * 255.0).round() as u8]
            })
            .collect();
        Frame { w: self.w, h: self.h, px }
    }
}

/// 用 GDI 量字與畫字，回傳覆蓋率遮罩。
struct TextMask {
    dc: HDC,
    font: HFONT,
}

impl TextMask {
    unsafe fn new(px: i32, bold: bool) -> Self {
        let dc = CreateCompatibleDC(null_mut());
        let face = wide("Microsoft JhengHei UI");
        let font = CreateFontW(
            -px, 0, 0, 0, if bold { 600 } else { 400 }, 0, 0, 0,
            DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32,
            ANTIALIASED_QUALITY as u32, (DEFAULT_PITCH | FF_DONTCARE) as u32, face.as_ptr(),
        );
        SelectObject(dc, font);
        TextMask { dc, font }
    }
    unsafe fn measure(&self, s: &str) -> (i32, i32) {
        let w = wide(s);
        let mut sz = SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(self.dc, w.as_ptr(), (w.len() - 1) as i32, &mut sz);
        (sz.cx, sz.cy)
    }
    /// 在畫布 (x, y) 畫字，y 是字框上緣。
    unsafe fn draw(&self, cv: &mut Canvas, s: &str, x: i32, y: i32, c: Rgb, alpha: f32) {
        let (tw, th) = self.measure(s);
        if tw <= 0 || th <= 0 {
            return;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: tw,
                biHeight: -th,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            },
            ..std::mem::zeroed()
        };
        let mut bits = null_mut();
        let bmp = CreateDIBSection(self.dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if bmp.is_null() {
            return;
        }
        let old = SelectObject(self.dc, bmp);
        SetBkMode(self.dc, TRANSPARENT as i32);
        SetTextColor(self.dc, 0x00FF_FFFF);
        let w = wide(s);
        let mut rc = RECT { left: 0, top: 0, right: tw, bottom: th };
        DrawTextW(self.dc, w.as_ptr(), (w.len() - 1) as i32, &mut rc, DT_SINGLELINE | DT_NOPREFIX | DT_LEFT | DT_TOP);
        GdiFlush();
        let data = std::slice::from_raw_parts(bits as *const [u8; 4], (tw * th) as usize);
        for yy in 0..th {
            for xx in 0..tw {
                let p = data[(yy * tw + xx) as usize];
                let cov = (p[0] as f32 + p[1] as f32 + p[2] as f32) / (3.0 * 255.0);
                cv.blend(x + xx, y + yy, c, cov * alpha);
            }
        }
        SelectObject(self.dc, old);
        DeleteObject(bmp);
    }
}

impl Drop for TextMask {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.font);
            DeleteDC(self.dc);
        }
    }
}

fn level_color(p: &Palette, used: f64) -> Rgb {
    match level(used) {
        Level::Normal => p.bar,
        Level::Warn => p.warn,
        Level::Crit => p.crit,
    }
}

fn value_text(used: Option<f64>) -> String {
    match used {
        Some(v) => format!("{}%", v.round() as i64),
        None => "–".into(),
    }
}

/// 畫出整條。`h` 是工作列高度（實際像素），`scale` 是 DPI 倍率。
pub fn render(m: &Model, h: i32, scale: f32) -> Frame {
    let s = |v: f32| (v * scale).round() as i32;
    let pal = palette(m.dark);
    unsafe {
        let name_font = TextMask::new(s(12.0), true);
        let row_font = TextMask::new(s(10.5), false);
        let pad = s(6.0);
        let gap_name = s(5.0);
        let gap_blocks = s(12.0);
        let bar_h = s(2.0).max(1);
        let label_gap = s(3.0);

        // 先排版：每家的名稱寬度、標籤寬度、數值寬度。
        let pct_w = row_font.measure("100%").0;
        struct Lay {
            name_w: i32,
            label_w: i32,
        }
        let lays: Vec<Lay> = m
            .blocks
            .iter()
            .map(|b| Lay {
                name_w: name_font.measure(&b.name).0,
                label_w: b.rows.iter().map(|r| row_font.measure(&r.label).0).max().unwrap_or(0),
            })
            .collect();
        let block_w = |l: &Lay| l.name_w + gap_name + l.label_w + label_gap + pct_w;
        let total: i32 = if lays.is_empty() {
            s(40.0)
        } else {
            pad * 2 + lays.iter().map(block_w).sum::<i32>() + gap_blocks * (lays.len() as i32 - 1)
        };
        let mut cv = Canvas::new(total.max(1), h.max(1));
        if m.hover {
            cv.fill(0, s(3.0), cv.w, h - s(6.0), pal.hover.0, pal.hover.1);
        }

        let row_h = row_font.measure("0").1;
        let name_h = name_font.measure("C").1;
        let mut x = pad;
        for (b, l) in m.blocks.iter().zip(&lays) {
            let alpha = if b.stale { pal.stale } else { 1.0 };
            name_font.draw(&mut cv, &b.name, x, (h - name_h) / 2, pal.name, alpha);
            let cx = x + l.name_w + gap_name;
            let n = b.rows.len().max(1) as i32;
            // 每列：字 + 進度條；正黑體字框下方留白多，進度條往上疊進字框底部。整體垂直置中。
            let line_h = row_h - s(2.0) + bar_h;
            let row_gap = s(1.0);
            let block_h = line_h * n + row_gap * (n - 1);
            let mut y = (h - block_h) / 2;
            for r in &b.rows {
                let color = r.used.map_or(pal.text, |u| match level(u) {
                    Level::Normal => pal.text,
                    _ => level_color(&pal, u),
                });
                row_font.draw(&mut cv, &r.label, cx, y, pal.name, alpha);
                let vt = value_text(r.used);
                let vx = cx + l.label_w + label_gap + pct_w - row_font.measure(&vt).0;
                row_font.draw(&mut cv, &vt, vx, y, color, alpha);
                let by = y + row_h - s(2.0);
                let bw = l.label_w + label_gap + pct_w;
                cv.fill(cx, by, bw, bar_h, pal.track.0, pal.track.1 * alpha);
                if let Some(u) = r.used {
                    let fw = ((bw as f64) * (u.clamp(0.0, 100.0) / 100.0)).round() as i32;
                    cv.fill(cx, by, fw, bar_h, level_color(&pal, u), alpha);
                }
                y += line_h + row_gap;
            }
            x += block_w(l) + gap_blocks;
        }
        cv.into_frame()
    }
}
