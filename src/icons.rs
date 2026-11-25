//! 從本機安裝的各家 App 取出圖示，當作工作列上的代表圖案。
//!
//! 不把商標圖檔包進程式裡：執行時才從使用者已安裝的 App 讀，找不到就改顯示文字。

use crate::http::wide;
use crate::providers::Provider;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::ptr::null_mut;
use std::rc::Rc;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Storage::Packaging::Appx::{GetPackagePathByFullName, GetPackagesByPackageFamily};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// 未預乘的 RGBA（0–1），列優先。
pub struct Icon {
    pub size: i32,
    pub px: Vec<[f32; 4]>,
}

/// Microsoft Store 版 App 的安裝資料夾（版本號會變，所以每次用 API 查）。
fn package_dir(family: &str) -> Option<PathBuf> {
    unsafe {
        let fam = wide(family);
        let mut count = 0u32;
        let mut buf_len = 0u32;
        GetPackagesByPackageFamily(fam.as_ptr(), &mut count, null_mut(), &mut buf_len, null_mut());
        if count == 0 || buf_len == 0 {
            return None;
        }
        let mut names = vec![null_mut::<u16>(); count as usize];
        let mut buf = vec![0u16; buf_len as usize];
        if GetPackagesByPackageFamily(fam.as_ptr(), &mut count, names.as_mut_ptr(), &mut buf_len, buf.as_mut_ptr()) != 0 {
            return None;
        }
        let full = names.first().copied().filter(|p| !p.is_null())?;
        let mut len = 0u32;
        GetPackagePathByFullName(full, &mut len, null_mut());
        if len == 0 {
            return None;
        }
        let mut path = vec![0u16; len as usize];
        if GetPackagePathByFullName(full, &mut len, path.as_mut_ptr()) != 0 {
            return None;
        }
        let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
        Some(PathBuf::from(String::from_utf16_lossy(&path[..end])))
    }
}

fn env_path(var: &str, rest: &[&str]) -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os(var)?);
    for r in rest {
        p.push(r);
    }
    Some(p)
}

/// 依序嘗試的圖示來源。
fn candidates(p: Provider) -> Vec<PathBuf> {
    let v = match p {
        Provider::Claude => vec![
            package_dir("Claude_pzs8sxrjxfjjc").map(|d| d.join("app").join("Claude.exe")),
            env_path("LOCALAPPDATA", &["AnthropicClaude", "claude.exe"]),
            env_path("USERPROFILE", &[".local", "bin", "claude.exe"]),
        ],
        Provider::Codex => vec![
            package_dir("OpenAI.Codex_2p2nqsd0c76g0").map(|d| d.join("app").join("ChatGPT.exe")),
            package_dir("OpenAI.ChatGPT-Desktop_2p2nqsd0c76g0").map(|d| d.join("app").join("ChatGPT.exe")),
        ],
        Provider::Copilot => vec![env_path("LOCALAPPDATA", &["Programs", "GitHub Copilot", "github.exe"])],
        Provider::Antigravity => vec![
            env_path("LOCALAPPDATA", &["Programs", "Antigravity", "Antigravity.exe"]),
            env_path("LOCALAPPDATA", &["Programs", "Antigravity IDE", "Antigravity IDE.exe"]),
        ],
    };
    v.into_iter().flatten().filter(|p| p.is_file()).collect()
}

/// 把 HICON 畫成 RGBA。分別畫在黑底與白底上，由兩者差異還原透明度，
/// 舊式沒有 alpha 的圖示也能正確去背。
unsafe fn rasterize(icon: HICON, size: i32) -> Option<Vec<[f32; 4]>> {
    let draw = |bg: u8| -> Option<Vec<[u8; 4]>> {
        let dc = CreateCompatibleDC(null_mut());
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            },
            ..std::mem::zeroed()
        };
        let mut bits = null_mut();
        let bmp = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if bmp.is_null() {
            DeleteDC(dc);
            return None;
        }
        let old = SelectObject(dc, bmp);
        let n = (size * size) as usize;
        std::slice::from_raw_parts_mut(bits as *mut u8, n * 4).fill(bg);
        DrawIconEx(dc, 0, 0, icon, size, size, 0, null_mut(), DI_NORMAL);
        GdiFlush();
        let out = std::slice::from_raw_parts(bits as *const [u8; 4], n).to_vec();
        SelectObject(dc, old);
        DeleteObject(bmp);
        DeleteDC(dc);
        Some(out)
    };
    let black = draw(0)?;
    let white = draw(255)?;
    Some(
        black
            .iter()
            .zip(&white)
            .map(|(b, w)| {
                let ch = |i: usize| (b[i] as f32 / 255.0, w[i] as f32 / 255.0);
                let (b0, w0) = ch(0);
                let (b1, w1) = ch(1);
                let (b2, w2) = ch(2);
                let a = (1.0 - ((w0 - b0) + (w1 - b1) + (w2 - b2)) / 3.0).clamp(0.0, 1.0);
                if a <= 0.0 {
                    [0.0; 4]
                } else {
                    // 黑底上的顏色 = 原色 × a。
                    [(b2 / a).min(1.0), (b1 / a).min(1.0), (b0 / a).min(1.0), a]
                }
            })
            .collect(),
    )
}

/// 偏暗的灰色圖示（例如 OpenAI 標誌）在深色面板上看不清楚，改成亮色。
/// 只處理灰色系，彩色圖示（Claude 的橘色）維持原樣。
fn brighten_if_dark(px: &mut [[f32; 4]]) {
    let (mut lum, mut sat, mut n) = (0.0f32, 0.0f32, 0.0f32);
    for p in px.iter().filter(|p| p[3] > 0.5) {
        lum += 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
        sat += p[0].max(p[1]).max(p[2]) - p[0].min(p[1]).min(p[2]);
        n += 1.0;
    }
    if n > 0.0 && lum / n < 0.5 && sat / n < 0.12 {
        for p in px.iter_mut() {
            p[0] = 0.91;
            p[1] = 0.98;
            p[2] = 1.0;
        }
    }
}

fn load(p: Provider, size: i32) -> Option<Icon> {
    for path in candidates(p) {
        unsafe {
            let w = wide(&path.display().to_string());
            let mut icon: HICON = null_mut();
            let got = PrivateExtractIconsW(w.as_ptr(), 0, size, size, &mut icon, null_mut(), 1, 0);
            if got == 0 || got == u32::MAX || icon.is_null() {
                continue;
            }
            let px = rasterize(icon, size);
            DestroyIcon(icon);
            if let Some(mut px) = px {
                if px.iter().any(|p| p[3] > 0.1) {
                    brighten_if_dark(&mut px);
                    return Some(Icon { size, px });
                }
            }
        }
    }
    None
}

thread_local! {
    static CACHE: RefCell<HashMap<(u8, i32), Option<Rc<Icon>>>> = RefCell::new(HashMap::new());
}

/// 取得某家在指定大小的圖示；結果會快取（找不到也記住，不會一直重找）。
pub fn get(p: Provider, size: i32) -> Option<Rc<Icon>> {
    let key = (p.index() as u8, size);
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let v = load(p, size).map(Rc::new);
    CACHE.with(|c| c.borrow_mut().insert(key, v.clone()));
    v
}

