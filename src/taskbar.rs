//! 找出各螢幕的工作列，以及小工具在工作列上可以放到多右邊。
//!
//! 主螢幕的工作列是 Shell_TrayWnd（右邊有系統匣 TrayNotifyWnd）；其他螢幕各有一個
//! Shell_SecondaryTrayWnd，Windows 11 上沒有系統匣、只有時鐘，所以要保留一段寬度給時鐘。
//! 工作列上也可能已經嵌了別的小工具（例如 BatteryCPUMonitor），要放在它們左邊。

use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

/// 副螢幕工作列右側保留給時鐘的寬度（96 DPI 下的像素，與 BatteryCPUMonitor 相同）。
pub const SECONDARY_CLOCK_RESERVE: i32 = 130;

/// 工作列上屬於 Windows 自己、不用避開的子視窗。
const SYSTEM_CLASSES: [&str; 9] = [
    "Windows.UI.Composition.DesktopWindowContentBridge",
    "Windows.UI.Core.CoreWindow",
    "Start",
    "TrayDummySearchControl",
    "ReBarWindow32",
    "WorkerW",
    "MSTaskSwWClass",
    "MSTaskListWClass",
    "AiQuotaBarWidget",
];

#[derive(Clone, Debug)]
pub struct Screen {
    /// 裝置名稱，例如 `\\.\DISPLAY2`，用來記住使用者的選擇。
    pub device: String,
    pub primary: bool,
    pub width: i32,
    pub height: i32,
    pub left: i32,
    pub taskbar: HWND,
}

fn class_of(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn monitor_info(m: HMONITOR) -> Option<(String, RECT, bool)> {
    unsafe {
        let mut mi: MONITORINFOEXW = std::mem::zeroed();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(m, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) == 0 {
            return None;
        }
        let end = mi.szDevice.iter().position(|&c| c == 0).unwrap_or(mi.szDevice.len());
        Some((
            String::from_utf16_lossy(&mi.szDevice[..end]),
            mi.monitorInfo.rcMonitor,
            mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        ))
    }
}

/// 有工作列的螢幕：主螢幕在前，其他依左到右。
pub fn screens() -> Vec<Screen> {
    let mut bars: Vec<HWND> = Vec::new();
    unsafe {
        let main = FindWindowW(w!("Shell_TrayWnd"), null());
        if !main.is_null() {
            bars.push(main);
        }
        let mut h: HWND = null_mut();
        loop {
            h = FindWindowExW(null_mut(), h, w!("Shell_SecondaryTrayWnd"), null());
            if h.is_null() {
                break;
            }
            bars.push(h);
        }
    }
    let mut out: Vec<Screen> = bars
        .into_iter()
        .filter_map(|tb| {
            let m = unsafe { MonitorFromWindow(tb, MONITOR_DEFAULTTONEAREST) };
            let (device, r, primary) = monitor_info(m)?;
            Some(Screen { device, primary, width: r.right - r.left, height: r.bottom - r.top, left: r.left, taskbar: tb })
        })
        .collect();
    out.sort_by_key(|s| (!s.primary, s.left));
    out.dedup_by(|a, b| a.device == b.device);
    out
}

/// 依設定的裝置名稱找工作列；沒設定、螢幕拔掉或那個螢幕沒有工作列時用主螢幕。
pub fn find(device: Option<&str>) -> Option<Screen> {
    let all = screens();
    device
        .and_then(|d| all.iter().find(|s| s.device == d).cloned())
        .or_else(|| all.iter().find(|s| s.primary).cloned())
        .or_else(|| all.first().cloned())
}

/// 小工具右緣最多能到哪裡（螢幕座標）。
///
/// 只看工作列右半邊的子視窗：系統匣、時鐘按鈕、其他程式嵌進來的小工具，取最左邊的那個；
/// 都沒有時（副螢幕）保留 `reserve` 給時鐘。
pub fn right_limit(tb_left: i32, tb_right: i32, obstacles: &[i32], reserve: i32) -> i32 {
    let mid = (tb_left + tb_right) / 2;
    obstacles
        .iter()
        .copied()
        .filter(|&l| l > mid && l < tb_right)
        .min()
        .unwrap_or(tb_right - reserve)
}

/// 工作列右半邊需要避開的子視窗左緣（螢幕座標）。
pub fn obstacles(taskbar: HWND) -> Vec<i32> {
    struct Ctx {
        parent: HWND,
        lefts: Vec<i32>,
    }
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let ctx = &mut *(lp as *mut Ctx);
        if GetParent(h) == ctx.parent && IsWindowVisible(h) != 0 {
            let cls = class_of(h);
            if !SYSTEM_CLASSES.contains(&cls.as_str()) {
                let mut r: RECT = std::mem::zeroed();
                if GetWindowRect(h, &mut r) != 0 && r.right > r.left {
                    ctx.lefts.push(r.left);
                }
            }
        }
        1
    }
    let mut ctx = Ctx { parent: taskbar, lefts: Vec::new() };
    unsafe { EnumChildWindows(taskbar, Some(cb), &mut ctx as *mut Ctx as LPARAM) };
    ctx.lefts
}

/// 選單上顯示的螢幕名稱。
pub fn label(index: usize, s: &Screen) -> String {
    let main = if s.primary { "，主螢幕" } else { "" };
    format!("螢幕 {}（{}×{}{main}）", index + 1, s.width, s.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_uses_tray() {
        // 系統匣在 1535，左半邊的東西不算。
        assert_eq!(right_limit(0, 1920, &[1535, 50, 300], 130), 1535);
    }

    #[test]
    fn secondary_avoids_other_widget_and_clock() {
        // 副螢幕：已有別的小工具在 4405，時鐘預留 130。
        assert_eq!(right_limit(2400, 5600, &[4405], 130), 4405);
        assert_eq!(right_limit(2400, 5600, &[], 130), 5470);
    }
}
