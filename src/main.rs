//! AI Quota Bar：在 Windows 工作列時鐘旁顯示 AI 訂閱額度。
//!
//! 結構：
//! - 每家一條背景執行緒定時查額度，結果放進共用的 `SLOTS`，再通知 UI 執行緒。
//! - 一個隱藏的頂層視窗（ctl）收計時器、廣播（工作列重建、佈景主題變更）與選單命令。
//! - 一個嵌進工作列（Shell_TrayWnd）的分層子視窗（widget）負責顯示與滑鼠互動。
#![windows_subsystem = "windows"]

mod claude_auth;
mod http;
mod icons;
mod model;
mod providers;
mod taskbar;
mod render;

use http::wide;
use model::*;
use providers::{Provider, ALL};
use std::cell::RefCell;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Time::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const WM_APP_DATA: u32 = WM_APP + 1;
const TIMER_PLACE: usize = 1;
const CMD_REFRESH: usize = 1;
const CMD_TOGGLE_BASE: usize = 10;
const CMD_AUTOSTART: usize = 20;
const CMD_OPEN_CLAUDE: usize = 21;
const CMD_EXIT: usize = 30;
const CMD_COPILOT_BASE: usize = 100;
const CMD_MONITOR_BASE: usize = 200;
const CMD_TOGGLE_WIDGET: usize = 22;
const CMD_SHOW_REMAINING: usize = 23;
const WM_APP_TRAY: u32 = WM_APP + 2;
const TRAY_ID: u32 = 1;

const REG_APP: &str = "Software\\AiQuotaBar";
const REG_RUN: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_NAME: &str = "AiQuotaBar";

// ---------- 背景查詢 ----------

#[derive(Clone, Default)]
struct Slot {
    quota: Option<Quota>,
    error: Option<FetchError>,
    /// 最後一次成功的時間。
    updated: Option<i64>,
    /// 已經查過至少一次（成功或失敗）。
    done: bool,
    /// 被限流時，下次自動重試的時間。
    retry_at: Option<i64>,
}

static SLOTS: Mutex<[Option<Slot>; 4]> = Mutex::new([None, None, None, None]);
static HIDDEN: [AtomicBool; 4] = [const { AtomicBool::new(false) }; 4];
static CTL: AtomicIsize = AtomicIsize::new(0);

fn slot(p: Provider) -> Slot {
    SLOTS.lock().unwrap()[p.index()].clone().unwrap_or_default()
}

fn store(p: Provider, r: Result<Quota, FetchError>, retry_at: Option<i64>) {
    let mut slots = SLOTS.lock().unwrap();
    let s = slots[p.index()].get_or_insert_with(Slot::default);
    s.done = true;
    s.retry_at = retry_at;
    match r {
        Ok(q) => {
            s.quota = Some(q);
            s.error = None;
            s.updated = Some(now_secs());
        }
        Err(e) => {
            // 找不到登入資料就清掉舊數字；其他錯誤保留上次的數字、淡化顯示。
            if matches!(e, FetchError::NotSetUp(_)) {
                s.quota = None;
            }
            s.error = Some(e);
        }
    }
}

fn spawn_worker(p: Provider) -> Sender<()> {
    let (tx, rx) = channel::<()>();
    std::thread::spawn(move || {
        let base = p.interval().as_secs();
        // 連續被限流的次數；被限流時等待時間加倍，期間也不理會手動更新，免得越查越久。
        let mut streak = 0u32;
        loop {
            let mut wait = base;
            if !HIDDEN[p.index()].load(Ordering::Relaxed) {
                let r = p.fetch();
                let stop = matches!(r, Err(FetchError::Disabled(_)));
                streak = if matches!(r, Err(FetchError::RateLimited(_))) { streak + 1 } else { 0 };
                wait = backoff_secs(base, streak);
                store(p, r, (streak > 0).then(|| now_secs() + wait as i64));
                notify();
                if stop {
                    return;
                }
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait);
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                match rx.recv_timeout(left) {
                    Ok(()) => {
                        while rx.try_recv().is_ok() {}
                        if streak == 0 {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    });
    tx
}

fn notify() {
    let ctl = CTL.load(Ordering::Relaxed);
    if ctl != 0 {
        unsafe { PostMessageW(ctl as HWND, WM_APP_DATA, 0, 0) };
    }
}

// ---------- UI 狀態（只在 UI 執行緒用） ----------

struct Ui {
    ctl: HWND,
    taskbar: HWND,
    widget: HWND,
    tip: HWND,
    tip_text: Vec<u16>,
    taskbar_created: u32,
    dark: bool,
    hover: bool,
    rendered: Option<(render::Model, i32, u32)>,
    pos: (i32, i32, i32, i32),
    workers: Vec<Sender<()>>,
    /// 要顯示在哪個螢幕（裝置名稱）；None 為主螢幕。
    monitor: Option<String>,
    /// 使用者把小工具整個隱藏了（從系統匣圖示再叫出來）。
    widget_hidden: bool,
    /// 顯示剩餘百分比而不是已用。
    show_remaining: bool,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Ui) -> R) -> R {
    UI.with(|u| f(u.borrow_mut().as_mut().expect("UI 尚未初始化")))
}

// ---------- 登錄檔 ----------

fn reg_get_dword(root: HKEY, key: &str, name: &str) -> Option<u32> {
    let mut v: u32 = 0;
    let mut len = 4u32;
    let r = unsafe {
        RegGetValueW(root, wide(key).as_ptr(), wide(name).as_ptr(), RRF_RT_REG_DWORD, null_mut(), &mut v as *mut u32 as *mut _, &mut len)
    };
    (r == 0).then_some(v)
}

fn reg_set_dword(key: &str, name: &str, v: u32) {
    unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(name).as_ptr(), REG_DWORD, &v as *const u32 as *const _, 4);
    }
}

fn reg_get_string(key: &str, name: &str) -> Option<String> {
    let mut buf = vec![0u16; 512];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(name).as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr() as *mut _, &mut len)
    };
    if r != 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end])).filter(|s| !s.is_empty())
}

fn reg_set_string(key: &str, name: &str, v: &str) {
    let w = wide(v);
    unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(name).as_ptr(), REG_SZ, w.as_ptr() as *const _, (w.len() * 2) as u32);
    }
}

fn reg_has_value(key: &str, name: &str) -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(name).as_ptr(), RRF_RT_ANY, null_mut(), null_mut(), null_mut()) == 0 }
}

fn exe_path() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
}

fn set_autostart(on: bool) {
    unsafe {
        if on {
            let v = wide(&format!("\"{}\"", exe_path()));
            RegSetKeyValueW(HKEY_CURRENT_USER, wide(REG_RUN).as_ptr(), wide(RUN_NAME).as_ptr(), REG_SZ, v.as_ptr() as *const _, (v.len() * 2) as u32);
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(REG_RUN).as_ptr(), wide(RUN_NAME).as_ptr());
        }
    }
}

fn system_dark() -> bool {
    reg_get_dword(HKEY_CURRENT_USER, "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize", "SystemUsesLightTheme") == Some(0)
}

fn load_hidden() {
    let mask = reg_get_dword(HKEY_CURRENT_USER, REG_APP, "Hidden").unwrap_or(0);
    for p in ALL {
        HIDDEN[p.index()].store(mask & (1 << p.index()) != 0, Ordering::Relaxed);
    }
}

fn save_hidden() {
    let mask = ALL.iter().filter(|p| HIDDEN[p.index()].load(Ordering::Relaxed)).fold(0u32, |m, p| m | 1 << p.index());
    reg_set_dword(REG_APP, "Hidden", mask);
}

// ---------- 時間格式 ----------

/// Unix 秒轉成本地時間 "17:40" 或 "11/28 08:00"（不是今天時）。
fn local_time(ts: i64) -> String {
    unsafe fn to_local(ts: i64) -> SYSTEMTIME {
        let ft_val = (ts as u64 + 11_644_473_600) * 10_000_000;
        let ft = FILETIME { dwLowDateTime: ft_val as u32, dwHighDateTime: (ft_val >> 32) as u32 };
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut local: SYSTEMTIME = std::mem::zeroed();
        FileTimeToSystemTime(&ft, &mut utc);
        SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local);
        local
    }
    unsafe {
        let t = to_local(ts.max(0));
        let today = to_local(now_secs());
        if (t.wYear, t.wMonth, t.wDay) == (today.wYear, today.wMonth, today.wDay) {
            format!("{:02}:{:02}", t.wHour, t.wMinute)
        } else {
            format!("{}/{} {:02}:{:02}", t.wMonth, t.wDay, t.wHour, t.wMinute)
        }
    }
}

fn window_line(w: &Window, now: i64) -> String {
    let mut s = format!("  {}：已用 {}（剩 {}）", w.name, fmt_pct(w.used), fmt_pct((100.0 - w.used).max(0.0)));
    if let Some(d) = &w.detail {
        s += &format!("，{d}");
    }
    match w.resets_at {
        Some(r) if r <= now => s += &format!("，已在 {} 重置，等待新資料", local_time(r)),
        Some(r) => s += &format!("，{} 重置（{}後）", local_time(r), fmt_duration(r - now)),
        None => {}
    }
    s
}

fn tooltip_text() -> String {
    let now = now_secs();
    let mut lines = Vec::new();
    for p in ALL {
        if HIDDEN[p.index()].load(Ordering::Relaxed) {
            continue;
        }
        let s = slot(p);
        let plan = s.quota.as_ref().and_then(|q| q.plan.clone());
        lines.push(match plan {
            Some(pl) => format!("{}（{pl}）", p.name()),
            None => p.name().to_string(),
        });
        if !s.done {
            lines.push("  查詢中…".into());
        }
        if let Some(q) = &s.quota {
            for w in q.windows.iter().chain(&q.extra) {
                lines.push(window_line(w, now));
            }
        }
        if let Some(e) = &s.error {
            match s.retry_at {
                Some(t) => lines.push(format!("  ⚠ {}，{} 自動重試", e.message(), local_time(t))),
                None => lines.push(format!("  ⚠ {}", e.message())),
            }
            if let (Some(u), Some(_)) = (s.updated, &s.quota) {
                lines.push(format!("  （顯示的是 {} 的資料）", local_time(u)));
            }
        }
    }
    lines.push(String::new());
    lines.push("左鍵：立即更新　右鍵：選單".into());
    lines.join("\r\n")
}

// ---------- 工作列上的內容 ----------

fn build_model(dark: bool, hover: bool, show_remaining: bool) -> render::Model {
    let mut blocks = Vec::new();
    for p in ALL {
        if HIDDEN[p.index()].load(Ordering::Relaxed) {
            continue;
        }
        let s = slot(p);
        let stale = s.error.is_some();
        let now = now_secs();
        match (&s.quota, &s.error) {
            (Some(q), _) => blocks.push(render::Block {
                provider: Some(p),
                name: p.short().into(),
                rows: q
                    .windows
                    .iter()
                    .take(2)
                    .map(|w| {
                        // 查不到新資料、而且已過重置時間：舊數字一定不對，改顯示「–」。
                        let expired = stale && w.resets_at.is_some_and(|r| r <= now);
                        render::Row { label: w.short.clone(), used: (!expired).then_some(w.used) }
                    })
                    .collect(),
                stale,
            }),
            // 沒設定或已停用的那家不佔位置；其他錯誤顯示「!」。
            (None, Some(FetchError::NotSetUp(_) | FetchError::Disabled(_))) | (None, None) => {}
            (None, Some(_)) => blocks.push(render::Block {
                provider: Some(p),
                name: p.short().into(),
                rows: vec![render::Row { label: "ERR".into(), used: None }],
                stale: true,
            }),
        }
    }
    if blocks.is_empty() {
        blocks.push(render::Block {
            provider: None,
            name: "AI QUOTA".into(),
            rows: vec![render::Row { label: "...".into(), used: None }],
            stale: true,
        });
    }
    render::Model { blocks, dark, hover, show_remaining }
}

unsafe fn present(widget: HWND, f: &render::Frame) {
    let dc = CreateCompatibleDC(null_mut());
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: f.w,
            biHeight: -f.h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        },
        ..std::mem::zeroed()
    };
    let mut bits = null_mut();
    let bmp = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if !bmp.is_null() {
        std::ptr::copy_nonoverlapping(f.px.as_ptr(), bits as *mut [u8; 4], f.px.len());
        let old = SelectObject(dc, bmp);
        let size = SIZE { cx: f.w, cy: f.h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        UpdateLayeredWindow(widget, null_mut(), null(), &size, dc, &src, 0, &blend, ULW_ALPHA);
        SelectObject(dc, old);
        DeleteObject(bmp);
    }
    DeleteDC(dc);
}

unsafe fn create_tooltip(widget: HWND) -> HWND {
    let tip = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        TOOLTIPS_CLASSW,
        null(),
        WS_POPUP | TTS_ALWAYSTIP | TTS_NOPREFIX,
        CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT,
        widget,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    let mut ti: TTTOOLINFOW = std::mem::zeroed();
    ti.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
    ti.uFlags = TTF_IDISHWND | TTF_SUBCLASS;
    ti.hwnd = widget;
    ti.uId = widget as usize;
    ti.lpszText = -1isize as *mut u16; // LPSTR_TEXTCALLBACKW
    SendMessageW(tip, TTM_ADDTOOLW, 0, &ti as *const _ as LPARAM);
    SendMessageW(tip, TTM_SETMAXTIPWIDTH, 0, 800);
    SendMessageW(tip, TTM_SETDELAYTIME, TTDT_AUTOPOP as usize, 30_000);
    tip
}

/// 建立（或在工作列重建、換螢幕後重建）嵌入視窗，放到系統匣（或副螢幕時鐘）左邊，內容有變才重畫。
fn ensure_widget() {
    unsafe {
        if with(|u| u.widget_hidden) {
            let widget = with(|u| u.widget);
            if !widget.is_null() && IsWindow(widget) != 0 && IsWindowVisible(widget) != 0 {
                ShowWindow(widget, SW_HIDE);
            }
            return;
        }
        let Some(screen) = taskbar::find(with(|u| u.monitor.clone()).as_deref()) else {
            return;
        };
        let taskbar = screen.taskbar;
        let (mut widget, old_taskbar) = with(|u| (u.widget, u.taskbar));
        if widget.is_null() || IsWindow(widget) == 0 || taskbar != old_taskbar {
            if !widget.is_null() && IsWindow(widget) != 0 {
                DestroyWindow(widget);
            }
            widget = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("AiQuotaBarWidget"),
                w!("AI Quota Bar"),
                WS_CHILD | WS_CLIPSIBLINGS,
                0, 0, 1, 1,
                taskbar,
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            if widget.is_null() {
                return;
            }
            let tip = create_tooltip(widget);
            with(|u| {
                u.widget = widget;
                u.taskbar = taskbar;
                u.tip = tip;
                u.rendered = None;
                u.pos = (0, 0, 0, 0);
            });
        }

        let mut tb: RECT = std::mem::zeroed();
        GetWindowRect(taskbar, &mut tb);
        let tray = FindWindowExW(taskbar, null_mut(), w!("TrayNotifyWnd"), null());
        let mut tr: RECT = std::mem::zeroed();
        if tray.is_null() || GetWindowRect(tray, &mut tr) == 0 {
            tr = RECT { left: tb.right, top: tb.top, right: tb.right, bottom: tb.bottom };
        }
        let vertical = tb.bottom - tb.top > tb.right - tb.left;
        let h = if vertical { s_min(tb.right - tb.left, 48) } else { tb.bottom - tb.top };
        let dpi = GetDpiForWindow(taskbar).max(96);
        let scale = dpi as f32 / 96.0;

        let (dark, hover, show_remaining) = with(|u| (u.dark, u.hover, u.show_remaining));
        let model = build_model(dark, hover, show_remaining);
        let key = (model.clone(), h, dpi);
        let needs = with(|u| u.rendered.as_ref() != Some(&key));
        let (mut w, mut wh) = with(|u| (u.pos.2, u.pos.3));
        if needs {
            let frame = render::render(&model, h, scale);
            w = frame.w;
            wh = frame.h;
            present(widget, &frame);
            with(|u| u.rendered = Some(key));
        }
        let gap = (2.0 * scale) as i32;
        let pos = if vertical {
            (0, (tr.top - tb.top - wh - gap).max(0), w, wh)
        } else {
            // 副螢幕沒有系統匣，保留時鐘的寬度；主螢幕找不到系統匣時也保留一段。
            let reserve = ((if screen.primary { 200 } else { taskbar::SECONDARY_CLOCK_RESERVE }) as f32 * scale) as i32;
            let limit = taskbar::right_limit(tb.left, tb.right, &taskbar::obstacles(taskbar), reserve);
            ((limit - tb.left - w - gap).max(0), 0, w, wh)
        };
        let moved = with(|u| std::mem::replace(&mut u.pos, pos) != pos);
        // 工作列自己的 XAML 內容也是子視窗，保持在最上層才看得到。
        let on_top = GetWindow(taskbar, GW_CHILD) == widget;
        if moved || !on_top || IsWindowVisible(widget) == 0 {
            SetWindowPos(widget, HWND_TOP, pos.0, pos.1, pos.2, pos.3, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
    }
}

fn s_min(a: i32, b: i32) -> i32 {
    a.min(b)
}

fn refresh_all() {
    with(|u| {
        for tx in &u.workers {
            let _ = tx.send(());
        }
    });
}

/// 隱藏／顯示整個工作列小工具（設定會記住）。
fn toggle_widget() {
    let hidden = with(|u| {
        u.widget_hidden = !u.widget_hidden;
        u.rendered = None;
        u.widget_hidden
    });
    reg_set_dword(REG_APP, "WidgetHidden", hidden as u32);
    ensure_widget();
}

/// 把 BGRA 像素做成 HICON。
unsafe fn make_icon(size: i32, px: &[[u8; 4]]) -> HICON {
    let dc = GetDC(null_mut());
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
    let color = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    ReleaseDC(null_mut(), dc);
    if color.is_null() {
        return null_mut();
    }
    std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut [u8; 4], px.len());
    let mask_bits = vec![0u8; ((size + 15) / 16 * 2 * size) as usize];
    let mask = CreateBitmap(size, size, 1, 1, mask_bits.as_ptr() as *const _);
    let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&info);
    DeleteObject(mask);
    DeleteObject(color);
    icon
}

fn tray_data(ctl: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = ctl;
    nid.uID = TRAY_ID;
    nid
}

/// 系統匣圖示：左鍵顯示／隱藏工作列小工具，右鍵開選單。
unsafe fn tray_add(ctl: HWND) {
    let size = 32;
    let mut nid = tray_data(ctl);
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_APP_TRAY;
    nid.hIcon = make_icon(size, &render::tray_icon(size));
    let tip: Vec<u16> = "AI Quota Bar（左鍵：顯示／隱藏，右鍵：選單）".encode_utf16().collect();
    let n = tip.len().min(nid.szTip.len() - 1);
    nid.szTip[..n].copy_from_slice(&tip[..n]);
    Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn tray_remove(ctl: HWND) {
    let nid = tray_data(ctl);
    Shell_NotifyIconW(NIM_DELETE, &nid);
}

unsafe fn show_menu() {
    let (ctl, widget) = with(|u| (u.ctl, u.widget));
    let menu = CreatePopupMenu();
    let add = |id: usize, text: &str, checked: bool| {
        let flags = MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED };
        AppendMenuW(menu, flags, id, wide(text).as_ptr());
    };
    add(CMD_REFRESH, "立即更新", false);
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    for p in ALL {
        let s = slot(p);
        let mut label = format!("顯示 {}", p.name());
        if let Some(FetchError::NotSetUp(_) | FetchError::Disabled(_)) = &s.error {
            label += "（無法使用）";
        }
        add(CMD_TOGGLE_BASE + p.index(), &label, !HIDDEN[p.index()].load(Ordering::Relaxed));
    }
    // Copilot 帳號：列出 gh 登入的 github.com 帳號，單選。
    let accounts = providers::gh_accounts();
    if accounts.len() > 1 {
        let sub = CreatePopupMenu();
        let chosen = providers::copilot_login();
        for (i, (login, active)) in accounts.iter().enumerate() {
            let checked = match &chosen {
                Some(c) => c == login,
                None => *active,
            };
            let flags = MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED };
            AppendMenuW(sub, flags, CMD_COPILOT_BASE + i, wide(login).as_ptr());
        }
        AppendMenuW(menu, MF_POPUP, sub as usize, wide("Copilot 帳號").as_ptr());
    }
    // 顯示在哪個螢幕的工作列。
    let screens = taskbar::screens();
    if screens.len() > 1 {
        let sub = CreatePopupMenu();
        let current = taskbar::find(with(|u| u.monitor.clone()).as_deref()).map(|s| s.device);
        for (i, sc) in screens.iter().enumerate() {
            let checked = current.as_deref() == Some(sc.device.as_str());
            let flags = MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED };
            AppendMenuW(sub, flags, CMD_MONITOR_BASE + i, wide(&taskbar::label(i, sc)).as_ptr());
        }
        AppendMenuW(menu, MF_POPUP, sub as usize, wide("顯示在").as_ptr());
    }
    let hidden_now = with(|u| u.widget_hidden);
    add(CMD_SHOW_REMAINING, "顯示剩餘百分比", with(|u| u.show_remaining));
    add(CMD_TOGGLE_WIDGET, if hidden_now { "顯示工作列小工具" } else { "隱藏工作列小工具" }, false);
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    add(CMD_OPEN_CLAUDE, "開啟 Claude 用量頁面", false);
    add(CMD_AUTOSTART, "開機時自動啟動", reg_has_value(REG_RUN, RUN_NAME));
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    add(CMD_EXIT, "結束", false);
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(ctl);
    let _ = widget;
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, ctl, null()) as usize;
    // 從系統匣叫出的選單：讓它在點別處時能正常關閉（Windows 文件建議的做法）。
    PostMessageW(ctl, WM_NULL, 0, 0);
    DestroyMenu(menu);
    match cmd {
        CMD_REFRESH => refresh_all(),
        CMD_AUTOSTART => set_autostart(!reg_has_value(REG_RUN, RUN_NAME)),
        CMD_OPEN_CLAUDE => {
            ShellExecuteW(null_mut(), w!("open"), w!("https://claude.ai/settings/usage"), null(), null(), SW_SHOWNORMAL);
        }
        CMD_EXIT => {
            DestroyWindow(ctl);
        }
        CMD_TOGGLE_WIDGET => toggle_widget(),
        CMD_SHOW_REMAINING => {
            let on = with(|u| {
                u.show_remaining = !u.show_remaining;
                u.show_remaining
            });
            reg_set_dword(REG_APP, "ShowRemaining", on as u32);
            ensure_widget();
        }
        c if c >= CMD_MONITOR_BASE && c - CMD_MONITOR_BASE < screens.len() => {
            let device = screens[c - CMD_MONITOR_BASE].device.clone();
            reg_set_string(REG_APP, "Monitor", &device);
            with(|u| {
                u.monitor = Some(device);
                u.rendered = None;
            });
            ensure_widget();
        }
        c if c >= CMD_COPILOT_BASE && c - CMD_COPILOT_BASE < accounts.len() => {
            let login = accounts[c - CMD_COPILOT_BASE].0.clone();
            reg_set_string(REG_APP, "CopilotAccount", &login);
            providers::set_copilot_login(Some(login));
            // 舊帳號的數字不能留著，等新帳號查完再顯示。
            SLOTS.lock().unwrap()[Provider::Copilot.index()] = None;
            with(|u| {
                let _ = u.workers[Provider::Copilot.index()].send(());
            });
            ensure_widget();
        }
        c if (CMD_TOGGLE_BASE..CMD_TOGGLE_BASE + 4).contains(&c) => {
            let i = c - CMD_TOGGLE_BASE;
            let now_hidden = !HIDDEN[i].load(Ordering::Relaxed);
            HIDDEN[i].store(now_hidden, Ordering::Relaxed);
            save_hidden();
            if !now_hidden {
                with(|u| {
                    let _ = u.workers[i].send(());
                });
            }
            ensure_widget();
        }
        _ => {}
    }
}

// ---------- 視窗程序 ----------

unsafe extern "system" fn ctl_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let created = UI.with(|u| u.borrow().as_ref().map(|u| u.taskbar_created));
    match m {
        WM_APP_DATA | WM_TIMER => {
            ensure_widget();
            0
        }
        WM_SETTINGCHANGE => {
            with(|u| u.dark = system_dark());
            ensure_widget();
            0
        }
        WM_APP_TRAY => {
            match lp as u32 {
                WM_LBUTTONUP => toggle_widget(),
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(),
                _ => {}
            }
            0
        }
        WM_ENDSESSION => {
            // 登出／關機：Windows 收到回覆後就會結束程式，先等換 token 做完。
            claude_auth::wait_idle(std::time::Duration::from_secs(15));
            0
        }
        WM_DESTROY => {
            // 換 token 換到一半不能結束，否則新登入可能沒寫回檔案。
            claude_auth::wait_idle(std::time::Duration::from_secs(30));
            let widget = with(|u| u.widget);
            if !widget.is_null() && IsWindow(widget) != 0 {
                DestroyWindow(widget);
            }
            tray_remove(h);
            PostQuitMessage(0);
            0
        }
        _ if Some(m) == created => {
            // explorer.exe 重啟：子視窗跟著舊工作列消失，系統匣圖示也要重新加。
            with(|u| u.widget = null_mut());
            tray_add(h);
            ensure_widget();
            0
        }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}

unsafe extern "system" fn widget_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match m {
        WM_MOUSEMOVE => {
            if !with(|u| u.hover) {
                with(|u| u.hover = true);
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: h,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut tme);
                ensure_widget();
            }
            0
        }
        WM_MOUSELEAVE => {
            with(|u| u.hover = false);
            ensure_widget();
            0
        }
        WM_LBUTTONUP => {
            refresh_all();
            0
        }
        WM_RBUTTONUP => {
            show_menu();
            0
        }
        WM_NOTIFY => {
            let hdr = &*(lp as *const NMHDR);
            if hdr.code == TTN_GETDISPINFOW {
                let info = &mut *(lp as *mut NMTTDISPINFOW);
                let text = wide(&tooltip_text());
                with(|u| {
                    u.tip_text = text;
                    info.lpszText = u.tip_text.as_mut_ptr();
                });
                return 0;
            }
            DefWindowProcW(h, m, wp, lp)
        }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}

// ---------- 進入點 ----------

/// 只有 debug 版有：把目前的 Claude token 當成被拒絕，強制走一次真的更新流程。
/// 報告只記狀態碼、回應欄位名稱與錯誤碼，不記任何 token。
#[cfg(debug_assertions)]
fn debug_force_claude_refresh(report: &str) {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join(".claude"));
    let log = std::cell::RefCell::new(Vec::new());
    let result = match claude_auth::load(&dir) {
        Err(e) => Err(e),
        Ok(c) => {
            let post = |body: &str| {
                let r = claude_auth::post_token(body);
                match &r {
                    Ok((status, text)) => {
                        let v: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
                        let keys: Vec<&String> = v.as_object().map(|o| o.keys().collect()).unwrap_or_default();
                        log.borrow_mut().push(format!("token POST status={status} keys={keys:?} error={}", v["error"]));
                    }
                    Err(e) => log.borrow_mut().push(format!("token POST failed: {e}")),
                }
                r
            };
            claude_auth::refresh(&dir, Some(&c.access), &post).map(|_| ())
        }
    };
    let mut log = log.into_inner();
    log.push(format!("result={:?}", result.as_ref().map_err(|e| e.message().to_string())));
    let _ = std::fs::write(report, log.join("\n"));
}

/// `--dump <檔案>`：查一次全部，把提示文字寫到檔案後結束（驗證用）。
fn dump(path: &str) {
    providers::set_copilot_login(reg_get_string(REG_APP, "CopilotAccount"));
    for p in ALL {
        store(p, p.fetch(), None);
    }
    let _ = std::fs::write(path, tooltip_text());
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    #[cfg(debug_assertions)]
    if let Some(i) = args.iter().position(|a| a == "--debug-force-claude-refresh") {
        debug_force_claude_refresh(args.get(i + 1).map(String::as_str).unwrap_or("refresh-report.txt"));
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--dump") {
        dump(args.get(i + 1).map(String::as_str).unwrap_or("ai-quota-dump.txt"));
        return;
    }
    unsafe {
        CreateMutexW(null(), 0, w!("Local\\AiQuotaBar"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        let hinst = GetModuleHandleW(null());
        for (name, proc_) in [
            (w!("AiQuotaBarCtl"), ctl_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT),
            (w!("AiQuotaBarWidget"), widget_proc),
        ] {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc_),
                hInstance: hinst,
                lpszClassName: name,
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
        }
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_WIN95_CLASSES,
        };
        InitCommonControlsEx(&icc);

        let ctl = CreateWindowExW(
            WS_EX_TOOLWINDOW, w!("AiQuotaBarCtl"), w!("AI Quota Bar"), WS_POPUP,
            0, 0, 0, 0, null_mut(), null_mut(), hinst, null(),
        );
        load_hidden();
        providers::set_copilot_login(reg_get_string(REG_APP, "CopilotAccount"));
        UI.with(|u| {
            *u.borrow_mut() = Some(Ui {
                ctl,
                taskbar: null_mut(),
                widget: null_mut(),
                tip: null_mut(),
                tip_text: vec![0],
                taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
                dark: system_dark(),
                hover: false,
                rendered: None,
                pos: (0, 0, 0, 0),
                workers: Vec::new(),
                monitor: reg_get_string(REG_APP, "Monitor"),
                widget_hidden: reg_get_dword(HKEY_CURRENT_USER, REG_APP, "WidgetHidden") == Some(1),
                show_remaining: reg_get_dword(HKEY_CURRENT_USER, REG_APP, "ShowRemaining") == Some(1),
            })
        });
        CTL.store(ctl as isize, Ordering::Relaxed);
        let workers = ALL.iter().map(|p| spawn_worker(*p)).collect();
        with(|u| u.workers = workers);
        tray_add(ctl);
        ensure_widget();
        // 系統匣寬度會變（圖示增減），定時重新對齊。
        SetTimer(ctl, TIMER_PLACE, 2000, None);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
