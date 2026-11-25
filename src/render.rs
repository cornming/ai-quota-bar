//! 把工作列上的內容畫成預乘 alpha 的 BGRA 像素，交給 UpdateLayeredWindow。
//!
//! 風格：深色 HUD 面板（不管工作列深淺色都一樣），霓虹青色邊框與四角括號、
//! 淡掃描線、LED 分段進度條（亮起的格子有光暈）、等寬數字。
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

// ---------- 配色 ----------

const PANEL: Rgb = hex(0x070B12);
const PANEL_TOP: Rgb = hex(0x0E1722);
const NEON: Rgb = hex(0x00E5FF);
const WARN: Rgb = hex(0xFFB020);
const CRIT: Rgb = hex(0xFF2E63);
const NAME: Rgb = hex(0xBDF6FF);
const LABEL: Rgb = hex(0xA6D4E4);
const VALUE: Rgb = hex(0xFFFFFF);
const SEG_OFF: Rgb = hex(0x1C3A48);
const IDLE: Rgb = hex(0x76858D);
/// 資料過期時內容的透明度。
const STALE: f32 = 0.55;
/// LED 進度條的格數。
const SEGMENTS: i32 = 8;

/// 面板配色。深色是霓虹 HUD；淺色是白底、顏色加深以保持對比。
pub struct Theme {
    panel: Rgb,
    panel_top: Rgb,
    panel_alpha: f32,
    accent: Rgb,
    warn: Rgb,
    crit: Rgb,
    name: Rgb,
    label: Rgb,
    value: Rgb,
    seg_off: Rgb,
    idle: Rgb,
    scan_alpha: f32,
    border_alpha: (f32, f32),
    glow_alpha: f32,
}

pub fn theme(light: bool) -> Theme {
    if light {
        Theme {
            panel: hex(0xE8F0F5),
            panel_top: hex(0xFAFCFE),
            panel_alpha: 1.0,
            accent: hex(0x0A8FB0),
            warn: hex(0xC46A00),
            crit: hex(0xD21A48),
            name: hex(0x0B3A4A),
            label: hex(0x2A4552),
            value: hex(0x000000),
            seg_off: hex(0xC2D2DB),
            idle: hex(0x7A8A93),
            scan_alpha: 0.015,
            border_alpha: (0.40, 0.75),
            glow_alpha: 0.12,
        }
    } else {
        Theme {
            panel: PANEL,
            panel_top: PANEL_TOP,
            panel_alpha: 1.0,
            accent: NEON,
            warn: WARN,
            crit: CRIT,
            name: NAME,
            label: LABEL,
            value: VALUE,
            seg_off: SEG_OFF,
            idle: IDLE,
            scan_alpha: 0.02,
            border_alpha: (0.22, 0.55),
            glow_alpha: 0.22,
        }
    }
}

fn level_color(th: &Theme, used: f64) -> Rgb {
    match level(used) {
        Level::Normal => th.accent,
        Level::Warn => th.warn,
        Level::Crit => th.crit,
    }
}

// ---------- 資料 ----------

/// 一列：「5H ▮▮▮▯▯▯▯▯▯▯ 28%」。
#[derive(Clone, PartialEq, Debug)]
pub struct Row {
    pub label: String,
    pub used: Option<f64>,
}

/// 一家：狀態燈與名稱在左，最多兩列數值在右。
#[derive(Clone, PartialEq, Debug)]
pub struct Block {
    /// 有對應的 App 圖示就畫圖示，沒有才畫名稱文字。
    pub provider: Option<crate::providers::Provider>,
    pub name: String,
    pub rows: Vec<Row>,
    /// 資料過期（登入過期或暫時錯誤）時淡化顯示。
    pub stale: bool,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Model {
    pub blocks: Vec<Block>,
    /// 工作列是深色。面板本身固定深色，淺色工作列時加一圈外框陰影讓面板浮起來。
    pub dark: bool,
    /// 面板用淺色主題。
    pub light: bool,
    pub hover: bool,
    /// 顯示剩餘百分比（進度條也改成剩下多少），顏色仍依已用程度。
    pub show_remaining: bool,
}

#[derive(Clone)]
pub struct Frame {
    pub w: i32,
    pub h: i32,
    pub px: Vec<[u8; 4]>,
}

// ---------- 畫布 ----------

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
        let a = a.min(1.0);
        let p = &mut self.px[(y * self.w + x) as usize];
        let out_a = a + p[3] * (1.0 - a);
        if out_a <= 0.0 {
            return;
        }
        let mix = |src: f32, dst: f32| (src * a + dst * p[3] * (1.0 - a)) / out_a;
        *p = [mix(c.0, p[0]), mix(c.1, p[1]), mix(c.2, p[2]), out_a];
    }

    /// 只加亮不蓋掉（光暈用）。
    fn add(&mut self, x: i32, y: i32, c: Rgb, a: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h || a <= 0.0 {
            return;
        }
        let p = &mut self.px[(y * self.w + x) as usize];
        let base_a = p[3];
        let out_a = (base_a + a * (1.0 - base_a)).min(1.0);
        let k = a / out_a.max(1e-6);
        p[0] = (p[0] * base_a / out_a.max(1e-6) + c.0 * k).min(1.0);
        p[1] = (p[1] * base_a / out_a.max(1e-6) + c.1 * k).min(1.0);
        p[2] = (p[2] * base_a / out_a.max(1e-6) + c.2 * k).min(1.0);
        p[3] = out_a;
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: Rgb, a: f32) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.blend(xx, yy, c, a);
            }
        }
    }

    /// 圓角矩形；`grad` 不為 None 時由上往下從 grad 漸變到 c。
    fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgb, a: f32, grad: Option<Rgb>) {
        for py in (y.floor() as i32)..((y + h).ceil() as i32) {
            for px in (x.floor() as i32)..((x + w).ceil() as i32) {
                let cov = rrect_coverage(px as f32 + 0.5, py as f32 + 0.5, x, y, w, h, r);
                if cov > 0.0 {
                    let col = match grad {
                        Some(top) => {
                            let t = ((py as f32 + 0.5 - y) / h).clamp(0.0, 1.0);
                            Rgb(top.0 + (c.0 - top.0) * t, top.1 + (c.1 - top.1) * t, top.2 + (c.2 - top.2) * t)
                        }
                        None => c,
                    };
                    self.blend(px, py, col, a * cov);
                }
            }
        }
    }

    /// 圓角矩形外框（寬 `t`）。
    fn rrect_stroke(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, t: f32, c: Rgb, a: f32) {
        for py in (y.floor() as i32 - 1)..((y + h).ceil() as i32 + 1) {
            for px in (x.floor() as i32 - 1)..((x + w).ceil() as i32 + 1) {
                let d = rrect_sdf(px as f32 + 0.5, py as f32 + 0.5, x, y, w, h, r);
                let cov = (t / 2.0 + 0.5 - (d + t / 2.0).abs()).clamp(0.0, 1.0);
                self.blend(px, py, c, a * cov);
            }
        }
    }

    /// 矩形周圍的柔和光暈。
    fn glow(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, c: Rgb, a: f32) {
        for py in ((y - radius).floor() as i32)..((y + h + radius).ceil() as i32) {
            for px in ((x - radius).floor() as i32)..((x + w + radius).ceil() as i32) {
                let d = rrect_sdf(px as f32 + 0.5, py as f32 + 0.5, x, y, w, h, 0.0).max(0.0);
                if d < radius {
                    let f = 1.0 - d / radius;
                    self.add(px, py, c, a * f * f);
                }
            }
        }
    }

    fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgb, a: f32) {
        for py in ((cy - r - 1.0).floor() as i32)..((cy + r + 1.0).ceil() as i32) {
            for px in ((cx - r - 1.0).floor() as i32)..((cx + r + 1.0).ceil() as i32) {
                let d = ((px as f32 + 0.5 - cx).powi(2) + (py as f32 + 0.5 - cy).powi(2)).sqrt();
                self.blend(px, py, c, a * (r + 0.5 - d).clamp(0.0, 1.0));
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

/// 點到圓角矩形邊界的帶號距離（內部為負）。
fn rrect_sdf(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let (hw, hh) = (w / 2.0, h / 2.0);
    let (cx, cy) = (x + hw, y + hh);
    let qx = (px - cx).abs() - (hw - r);
    let qy = (py - cy).abs() - (hh - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

fn rrect_coverage(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    (0.5 - rrect_sdf(px, py, x, y, w, h, r)).clamp(0.0, 1.0)
}

// ---------- 文字 ----------

/// 用 GDI 量字與畫字，回傳覆蓋率遮罩。
struct TextMask {
    dc: HDC,
    font: HFONT,
}

impl TextMask {
    unsafe fn new(px: i32, weight: i32, face: &str) -> Self {
        let dc = CreateCompatibleDC(null_mut());
        let face = wide(face);
        let font = CreateFontW(
            -px, 0, 0, 0, weight, 0, 0, 0,
            DEFAULT_CHARSET as u32, OUT_TT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32,
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

    /// 字的上緣到基線的距離，用來讓不同字型的字對齊同一條基線。
    unsafe fn ascent(&self) -> i32 {
        let mut tm: TEXTMETRICW = std::mem::zeroed();
        GetTextMetricsW(self.dc, &mut tm);
        tm.tmAscent
    }

    /// 在畫布上畫字，`baseline` 是基線的 y。
    unsafe fn draw(&self, cv: &mut Canvas, s: &str, x: i32, baseline: i32, c: Rgb, alpha: f32) {
        let (tw, th) = self.measure(s);
        if tw <= 0 || th <= 0 {
            return;
        }
        let y = baseline - self.ascent();
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
                // 小字的灰階反鋸齒邊緣偏淡，加一點 gamma 讓筆畫實一點。
                cv.blend(x + xx, y + yy, c, cov.powf(0.7) * alpha);
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

fn value_text(used: Option<f64>) -> String {
    match used {
        Some(v) => format!("{:>3}%", v.round().clamp(0.0, 999.0) as i64),
        None => "  –%".into(),
    }
}

/// 這家目前最嚴重的程度，決定狀態燈顏色。
fn block_color(th: &Theme, b: &Block) -> Rgb {
    if b.stale {
        return th.idle;
    }
    b.rows
        .iter()
        .filter_map(|r| r.used)
        .fold(None::<f64>, |m, u| Some(m.map_or(u, |m| m.max(u))))
        .map_or(th.idle, |u| level_color(th, u))
}

// ---------- 版面 ----------

/// 畫出整條。`h` 是工作列高度（實際像素），`scale` 是 DPI 倍率。
pub fn render(m: &Model, h: i32, scale: f32) -> Frame {
    let s = |v: f32| (v * scale).round() as i32;
    let f = |v: f32| v * scale;
    let th = theme(m.light);
    unsafe {
        let name_font = TextMask::new(s(9.5), 600, "Bahnschrift");
        let label_font = TextMask::new(s(10.0), 600, "Cascadia Mono");
        let value_font = TextMask::new(s(11.0), 700, "Cascadia Mono");

        let pad_x = s(8.0);
        let dot_gap = s(4.0);
        let name_gap = s(6.0);
        let sep_gap = s(5.0);
        let seg_w = s(2.0).max(2);
        let seg_gap = s(1.0).max(1);
        let seg_h = s(6.0).max(3);
        let bar_w = SEGMENTS * seg_w + (SEGMENTS - 1) * seg_gap;
        let col_gap = s(3.0);
        let dot_r = f(2.5);

        let value_w = value_font.measure("100%").0;
        let icon_size = s(18.0).min(h - s(12.0)).max(12);
        let dot_w = (dot_r * 2.0).ceil() as i32;
        struct Lay {
            icon: Option<std::rc::Rc<crate::icons::Icon>>,
            /// 圖示或「狀態燈＋名稱」的寬度。
            head_w: i32,
            label_w: i32,
        }
        let lays: Vec<Lay> = m
            .blocks
            .iter()
            .map(|b| {
                let icon = b.provider.and_then(|p| crate::icons::get(p, icon_size, m.light));
                Lay {
                    head_w: if icon.is_some() { icon_size } else { dot_w + dot_gap + name_font.measure(&b.name).0 },
                    icon,
                    label_w: b.rows.iter().map(|r| label_font.measure(&r.label).0).max().unwrap_or(0),
                }
            })
            .collect();
        let block_w = |l: &Lay| l.head_w + name_gap + l.label_w + col_gap + bar_w + col_gap + value_w;
        let content: i32 =
            lays.iter().map(block_w).sum::<i32>() + (sep_gap * 2 + 1) * (lays.len() as i32 - 1).max(0);
        let total = pad_x * 2 + content;
        let mut cv = Canvas::new(total.max(1), h.max(1));

        // 面板：上下各留一點，讓工作列看得出這是一塊浮起來的 HUD。
        let my = f(2.0);
        let (px0, py0, pw, ph) = (f(1.0), my, total as f32 - f(2.0), h as f32 - my * 2.0);
        let radius = f(5.0);
        if !m.dark {
            // 淺色工作列：外圈淡陰影。
            cv.rrect_stroke(px0, py0, pw, ph, radius, f(2.0), hex(0x000000), 0.10);
        }
        cv.rrect(px0, py0, pw, ph, radius, th.panel, th.panel_alpha, Some(th.panel_top));
        // 掃描線。
        let step = s(3.0).max(2);
        let mut yy = py0 as i32 + 1;
        while (yy as f32) < py0 + ph - 1.0 {
            for xx in (px0 as i32 + 2)..((px0 + pw) as i32 - 2) {
                cv.blend(xx, yy, th.accent, th.scan_alpha);
            }
            yy += step;
        }
        // 邊框與四角括號。
        cv.rrect_stroke(px0 + 0.5, py0 + 0.5, pw - 1.0, ph - 1.0, radius, 1.0, th.accent, if m.hover { th.border_alpha.1 } else { th.border_alpha.0 });
        let arm = s(7.0);
        let t = s(1.5).max(1);
        let (l, tp, r, b) = (px0 as i32, py0 as i32, (px0 + pw) as i32, (py0 + ph) as i32);
        let corner_a = if m.hover { 1.0 } else { 0.85 };
        for (cx, cy, dx, dy) in [(l, tp, 1, 1), (r, tp, -1, 1), (l, b, 1, -1), (r, b, -1, -1)] {
            let hx = if dx > 0 { cx } else { cx - arm };
            let vy = if dy > 0 { cy } else { cy - arm };
            let hy = if dy > 0 { cy } else { cy - t };
            let vx = if dx > 0 { cx } else { cx - t };
            cv.fill(hx, hy, arm, t, th.accent, corner_a);
            cv.fill(vx, vy, t, arm, th.accent, corner_a);
        }

        // 內容。
        let mid = h / 2;
        let value_asc = value_font.ascent();
        let row_pitch = s(14.5);
        let mut x = pad_x;
        for (i, (b, lay)) in m.blocks.iter().zip(&lays).enumerate() {
            if i > 0 {
                // 區塊分隔線：上下淡出。
                let sx = x + sep_gap;
                let (top, bot) = (py0 as i32 + s(6.0), (py0 + ph) as i32 - s(6.0));
                for yy in top..bot {
                    let t = (yy - top) as f32 / (bot - top).max(1) as f32;
                    let fade = 1.0 - (t * 2.0 - 1.0).abs();
                    cv.blend(sx, yy, th.accent, 0.35 * fade);
                }
                x += sep_gap * 2 + 1;
            }
            let a = if b.stale { STALE } else { 1.0 };
            let dc = block_color(&th, b);
            if let Some(icon) = &lay.icon {
                // App 圖示；過期時轉灰階並淡化。右下角疊一顆狀態燈。
                let iy = mid - icon.size / 2;
                for yy in 0..icon.size {
                    for xx in 0..icon.size {
                        let p = icon.px[(yy * icon.size + xx) as usize];
                        let c = if b.stale {
                            let l = 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
                            Rgb(l, l, l)
                        } else {
                            Rgb(p[0], p[1], p[2])
                        };
                        cv.blend(x + xx, iy + yy, c, p[3] * a);
                    }
                }
                let lr = f(2.6);
                let (lx, ly) = ((x + icon.size) as f32 - lr * 0.4, (iy + icon.size) as f32 - lr * 0.4);
                cv.circle(lx, ly, lr + f(1.2), th.panel_top, 1.0);
                if !b.stale {
                    cv.glow(lx - lr, ly - lr, lr * 2.0, lr * 2.0, f(4.0), dc, 0.35);
                }
                cv.circle(lx, ly, lr, dc, 1.0);
            } else {
                let dcx = x as f32 + dot_r;
                let dcy = mid as f32;
                if !b.stale {
                    cv.glow(dcx - dot_r, dcy - dot_r, dot_r * 2.0, dot_r * 2.0, f(4.0), dc, 0.35);
                }
                cv.circle(dcx, dcy, dot_r, dc, 1.0);
                let name_base = mid + name_font.ascent() / 2 - s(1.0);
                name_font.draw(&mut cv, &b.name, x + dot_w + dot_gap, name_base, th.name, a);
            }

            let cx = x + lay.head_w + name_gap;
            let n = b.rows.len().max(1) as i32;
            let first_center = mid - row_pitch * (n - 1) / 2;
            for (ri, r) in b.rows.iter().enumerate() {
                let rc = first_center + row_pitch * ri as i32;
                let base = rc + value_asc / 2 - s(1.0);
                label_font.draw(&mut cv, &r.label, cx, base, th.label, a);
                // LED 分段條。
                let bx = cx + lay.label_w + col_gap;
                let by = rc - seg_h / 2;
                let shown = r.used.map(|u| if m.show_remaining { 100.0 - u.clamp(0.0, 100.0) } else { u.clamp(0.0, 100.0) });
                let lit = shown.map_or(0, |v| ((v / 100.0) * SEGMENTS as f64).ceil() as i32);
                let color = r.used.map_or(th.idle, |u| level_color(&th, u));
                for k in 0..SEGMENTS {
                    let sx = bx + k * (seg_w + seg_gap);
                    if k < lit {
                        if !b.stale {
                            cv.glow(sx as f32, by as f32, seg_w as f32, seg_h as f32, f(3.0), color, th.glow_alpha);
                        }
                        cv.fill(sx, by, seg_w, seg_h, color, a);
                    } else {
                        cv.fill(sx, by, seg_w, seg_h, th.seg_off, a);
                    }
                }
                let vt = value_text(shown);
                let vcol = match r.used {
                    Some(u) if level(u) != Level::Normal => level_color(&th, u),
                    _ => th.value,
                };
                let vx = bx + bar_w + col_gap + value_w - value_font.measure(&vt).0;
                value_font.draw(&mut cv, &vt, vx, base, vcol, a);
            }
            x += block_w(lay);
        }
        cv.into_frame()
    }
}

/// 更新時掃過的光澤：一道斜向的亮帶從左到右，`t` 是 0–1 的進度。
/// 只加亮已經畫出來的像素（依原本的透明度），所以光不會跑到面板外面。
/// 淺色面板上白光看不出來，改成一道淡淡的主題色光帶。
pub fn sheen(base: &Frame, t: f32, scale: f32, light: bool) -> Frame {
    // 光帶的顏色（BGR 順序，0–1）與強度。
    let (tint, strength) = if light { ([0.69, 0.56, 0.04], 0.55) } else { ([1.0, 1.0, 0.92], 1.0) };
    let mut out = base.clone();
    let band = 22.0 * scale; // 亮帶的半寬
    let skew = 0.8; // 每往下一個像素，亮帶往左偏多少
    let h = base.h as f32;
    let travel = base.w as f32 + band * 2.0 + skew * h;
    let center = -band + t * travel;
    for y in 0..base.h {
        let row_center = center - skew * (y as f32 - h / 2.0);
        let x0 = ((row_center - band).floor() as i32).max(0);
        let x1 = ((row_center + band).ceil() as i32).min(base.w);
        for x in x0..x1 {
            let d = (x as f32 + 0.5 - row_center) / band;
            if d.abs() >= 1.0 {
                continue;
            }
            // 柔和的外圈加上中間一道細的高光。
            let soft = (1.0 - d * d).powi(2) * 0.30;
            let core = (1.0 - (d * 6.0).abs()).max(0.0) * 0.35;
            let k = (soft + core) * strength;
            let p = &mut out.px[(y * base.w + x) as usize];
            let a = p[3] as f32;
            if a < 8.0 {
                continue;
            }
            // 預乘 alpha：顏色值不能超過 alpha。往白色（帶一點青）推。
            let mix = |c: u8, target: f32| -> u8 { (c as f32 + (a * target - c as f32) * k).clamp(0.0, a) as u8 };
            p[0] = mix(p[0], tint[0]);
            p[1] = mix(p[1], tint[1]);
            p[2] = mix(p[2], tint[2]);
        }
    }
    out
}

/// 系統匣圖示：深色圓角方塊裡三根由低到高的霓虹 LED 柱。回傳未預乘的 BGRA。
pub fn tray_icon(size: i32) -> Vec<[u8; 4]> {
    let f = size as f32 / 32.0;
    let mut cv = Canvas::new(size, size);
    cv.rrect(f, f, 30.0 * f, 30.0 * f, 7.0 * f, PANEL, 1.0, Some(PANEL_TOP));
    cv.rrect_stroke(1.5 * f, 1.5 * f, 29.0 * f, 29.0 * f, 6.5 * f, 1.5 * f, NEON, 0.9);
    for (i, (hgt, c)) in [(8.0, NEON), (13.0, NEON), (18.0, WARN)].iter().enumerate() {
        let x = (8.0 + i as f32 * 6.0) * f;
        let y = (25.0 - hgt) * f;
        cv.glow(x, y, 4.0 * f, hgt * f, 3.0 * f, *c, 0.35);
        cv.rrect(x, y, 4.0 * f, hgt * f, 1.0 * f, *c, 1.0, None);
    }
    cv.px
        .iter()
        .map(|p| {
            let q = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
            [q(p[2]), q(p[1]), q(p[0]), q(p[3])]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sheen_brightens_inside_panel_only() {
        // 左半透明（面板外），右半是不透明的深色面板。
        let (w, h) = (40, 10);
        let mut px = vec![[0u8, 0, 0, 0]; (w * h) as usize];
        for y in 0..h {
            for x in 20..w {
                px[(y * w + x) as usize] = [20, 15, 10, 240];
            }
        }
        let base = Frame { w, h, px };
        let out = sheen(&base, 0.5, 1.0, false);
        let mut brighter = 0;
        for (a, b) in base.px.iter().zip(&out.px) {
            if a[3] == 0 {
                assert_eq!(a, b, "面板外不能被畫上光");
            }
            assert!(b[0] <= b[3] && b[1] <= b[3] && b[2] <= b[3], "預乘 alpha 不能超過 alpha");
            assert_eq!(a[3], b[3], "透明度不變");
            if b[0] > a[0] {
                brighter += 1;
            }
        }
        assert!(brighter > 0, "亮帶經過的地方要變亮");
        // 動畫開始前與結束後，亮帶都在畫面外。
        assert_eq!(sheen(&base, 0.0, 1.0, false).px, base.px);
        assert_eq!(sheen(&base, 1.0, 1.0, false).px, base.px);
        assert_eq!(sheen(&base, 1.0, 1.0, true).px, base.px);
    }

    /// 預覽用：`PREVIEW_DIR=<資料夾> cargo test --release preview -- --ignored`，
    /// 把深淺色兩種主題疊在工作列底色上存成 BMP。
    #[test]
    #[ignore]
    fn preview() {
        use crate::providers::Provider;
        let dir = std::env::var("PREVIEW_DIR").expect("PREVIEW_DIR");
        let row = |l: &str, u: f64| Row { label: l.into(), used: Some(u) };
        let blocks = vec![
            Block { provider: Some(Provider::Claude), name: "CLAUDE".into(), rows: vec![row("5H", 36.0), row("7D", 78.0)], stale: false },
            Block { provider: Some(Provider::Codex), name: "CODEX".into(), rows: vec![row("30D", 0.0)], stale: false },
            Block { provider: Some(Provider::Copilot), name: "COPILOT".into(), rows: vec![row("PREM", 93.0)], stale: false },
            Block { provider: Some(Provider::Antigravity), name: "AGY".into(), rows: vec![row("GEM", 23.0), row("C+G", 51.0)], stale: true },
        ];
        for (light, dark_bar, bg, name) in [(false, true, [0x1C, 0x1C, 0x1C], "dark"), (true, false, [0xEE, 0xEE, 0xEE], "light")] {
            for scale in [1.0f32, 1.5] {
                let h = (40.0 * scale) as i32;
                let m = Model { blocks: blocks.clone(), dark: dark_bar, light, hover: false, show_remaining: false };
                let f = render(&m, h, scale);
                let (w, h) = (f.w + 20, f.h);
                let mut out = Vec::new();
                let row_len = (w * 3 + 3) & !3;
                let size = 54 + row_len * h;
                out.extend(b"BM");
                out.extend((size as u32).to_le_bytes());
                out.extend([0u8; 4]);
                out.extend(54u32.to_le_bytes());
                out.extend(40u32.to_le_bytes());
                out.extend(w.to_le_bytes());
                out.extend(h.to_le_bytes());
                out.extend(1u16.to_le_bytes());
                out.extend(24u16.to_le_bytes());
                out.extend([0u8; 24]);
                for y in (0..h).rev() {
                    let start = out.len();
                    for x in 0..w {
                        let fx = x - 10;
                        let px = if fx >= 0 && fx < f.w { f.px[(y * f.w + fx) as usize] } else { [0, 0, 0, 0] };
                        let a = px[3] as u32;
                        for c in 0..3 {
                            out.push((px[c] as u32 + bg[c] as u32 * (255 - a) / 255).min(255) as u8);
                        }
                    }
                    out.resize(start + row_len as usize, 0);
                }
                std::fs::write(format!("{dir}/{name}-{scale}.bmp"), out).unwrap();
            }
        }
    }
}
