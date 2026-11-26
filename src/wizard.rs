//! HUD 風格的「環境設定」精靈視窗。
//!
//! 獨立的無邊框分層視窗（與工作列小工具、滑鼠提示共用同一套深／淺色 HUD 主題），
//! 支援高 DPI、拖曳移動、Esc 關閉、F5 重新檢查、複製指令到剪貼簿、開啟官網與
//! 啟動可見的 PowerShell 引導安裝／登入視窗。

use crate::http::wide;
use crate::providers::ALL;
use crate::render::{self, HitBox, SetupAction, SetupView};
use crate::setup;
use std::cell::RefCell;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const TIMER_NOTICE: usize = 1;
const NOTICE_MS: u64 = 3200;
const CF_UNICODETEXT: u32 = 13;

struct State {
    hwnd: HWND,
    hits: Vec<HitBox>,
    shadow: i32,
    size: (i32, i32),
    hover: Option<SetupAction>,
    pressed: Option<SetupAction>,
    tracking: bool,
    light: bool,
    scale: f32,
    cli_installed: [bool; 4],
    running: [bool; 4],
    notice: Option<(String, Instant)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            hwnd: null_mut(),
            hits: Vec::new(),
            shadow: 0,
            size: (0, 0),
            hover: None,
            pressed: None,
            tracking: false,
            light: false,
            scale: 1.0,
            cli_installed: [false; 4],
            running: [false; 4],
            notice: None,
        }
    }
}

thread_local! {
    static WIZ: RefCell<State> = RefCell::new(State::default());
}

fn probe_clis() -> [bool; 4] {
    [
        setup::cli_installed(ALL[0]),
        setup::cli_installed(ALL[1]),
        setup::cli_installed(ALL[2]),
        setup::cli_installed(ALL[3]),
    ]
}

/// 註冊精靈視窗類別（程式啟動時呼叫一次）。
pub unsafe fn register(hinst: HMODULE, icon: HICON) {
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst,
        hIcon: icon,
        lpszClassName: w!("AiQuotaBarSetup"),
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        ..std::mem::zeroed()
    };
    RegisterClassW(&wc);
}

pub fn is_open() -> bool {
    WIZ.with(|w| {
        let h = w.borrow().hwnd;
        !h.is_null() && unsafe { IsWindow(h) != 0 }
    })
}

/// 開啟（或切回前景）環境設定精靈視窗。
pub unsafe fn open(anchor: HWND, light: bool) {
    let cli = probe_clis();
    if is_open() {
        let h = WIZ.with(|w| {
            let mut st = w.borrow_mut();
            st.light = light;
            st.cli_installed = cli;
            st.hwnd
        });
        redraw(false);
        ShowWindow(h, SW_RESTORE);
        SetForegroundWindow(h);
        return;
    }

    let hinst = GetModuleHandleW(null());
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_APPWINDOW,
        w!("AiQuotaBarSetup"),
        w!("AI Quota Bar — 環境設定"),
        WS_POPUP,
        0,
        0,
        1,
        1,
        null_mut(),
        null_mut(),
        hinst,
        null(),
    );
    if hwnd.is_null() {
        return;
    }

    // 依小工具所在螢幕（或滑鼠所在螢幕）決定 DPI 與置中位置。
    let mon = if !anchor.is_null() && IsWindow(anchor) != 0 {
        MonitorFromWindow(anchor, MONITOR_DEFAULTTONEAREST)
    } else {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY)
    };
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(mon, &mut mi);
    let wr = mi.rcWork;

    // 先放到目標螢幕上讓 GetDpiForWindow 回傳該螢幕正確 DPI。
    SetWindowPos(hwnd, null_mut(), wr.left, wr.top, 1, 1, SWP_NOZORDER | SWP_NOACTIVATE);
    let dpi = GetDpiForWindow(hwnd).max(96);
    let scale = dpi as f32 / 96.0;

    WIZ.with(|w| {
        let mut st = w.borrow_mut();
        st.hwnd = hwnd;
        st.light = light;
        st.scale = scale;
        st.cli_installed = cli;
        st.hover = None;
        st.pressed = None;
        st.tracking = false;
        st.notice = None;
    });

    redraw(false);
    let (fw, fh) = WIZ.with(|w| w.borrow().size);
    let cx = (wr.left + (wr.right - wr.left - fw) / 2).max(wr.left).min((wr.right - fw).max(wr.left));
    let cy = (wr.top + (wr.bottom - wr.top - fh) / 2).max(wr.top).min((wr.bottom - fh).max(wr.top));
    SetWindowPos(hwnd, HWND_TOP, cx, cy, fw, fh, SWP_SHOWWINDOW);
    SetForegroundWindow(hwnd);
}

/// 程式結束時一併關閉精靈視窗。
pub unsafe fn close() {
    let h = WIZ.with(|w| w.borrow().hwnd);
    if !h.is_null() && IsWindow(h) != 0 {
        DestroyWindow(h);
    }
}

/// 當查詢結果更新或主題切換時，若視窗開著就即時重畫。
pub unsafe fn sync(light: bool, reprobe_cli: bool) {
    if !is_open() {
        return;
    }
    WIZ.with(|w| {
        let mut st = w.borrow_mut();
        st.light = light;
        if reprobe_cli {
            st.cli_installed = probe_clis();
        }
    });
    redraw(false);
}

/// 某家服務的 PowerShell 設定視窗已關閉：解除執行中狀態、重掃 CLI 並重畫。
pub unsafe fn on_provider_done(idx: usize, light: bool) {
    WIZ.with(|w| {
        let mut st = w.borrow_mut();
        if idx < st.running.len() {
            st.running[idx] = false;
        }
        st.light = light;
        st.cli_installed = probe_clis();
        if !st.hwnd.is_null() {
            st.notice = Some(("✓ 設定視窗已關閉，正在重新檢查額度…".into(), Instant::now()));
            SetTimer(st.hwnd, TIMER_NOTICE, NOTICE_MS as u32, None);
        }
    });
    if is_open() {
        redraw(false);
    }
}

unsafe fn set_notice(msg: String) {
    let h = WIZ.with(|w| {
        let mut st = w.borrow_mut();
        st.notice = Some((msg, Instant::now()));
        st.hwnd
    });
    if !h.is_null() && IsWindow(h) != 0 {
        SetTimer(h, TIMER_NOTICE, NOTICE_MS as u32, None);
        redraw(false);
    }
}

unsafe fn redraw(resize_window: bool) {
    let (hwnd, scale, light, hover, pressed, cli, running, notice) = WIZ.with(|w| {
        let mut st = w.borrow_mut();
        if let Some((_, at)) = &st.notice {
            if at.elapsed() >= Duration::from_millis(NOTICE_MS) {
                st.notice = None;
            }
        }
        (
            st.hwnd,
            st.scale,
            st.light,
            st.hover,
            st.pressed,
            st.cli_installed,
            st.running,
            st.notice.as_ref().map(|(s, _)| s.clone()),
        )
    });
    if hwnd.is_null() || IsWindow(hwnd) == 0 {
        return;
    }
    let items = crate::build_setup_items(&cli, &running);
    let view = SetupView { items: &items, hover, pressed, notice: notice.as_deref(), light };
    let (frame, shadow, hits) = render::setup_wizard(&view, scale);
    let old_size = WIZ.with(|w| {
        let mut st = w.borrow_mut();
        let prev = st.size;
        st.hits = hits;
        st.shadow = shadow;
        st.size = (frame.w, frame.h);
        prev
    });
    if resize_window && old_size != (frame.w, frame.h) {
        SetWindowPos(hwnd, null_mut(), 0, 0, frame.w, frame.h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
    crate::present(hwnd, &frame);
}

fn hit_at(x: i32, y: i32) -> Option<SetupAction> {
    WIZ.with(|w| w.borrow().hits.iter().find(|hb| hb.contains(x, y)).map(|hb| hb.action))
}

fn inside_card(x: i32, y: i32) -> bool {
    WIZ.with(|w| {
        let st = w.borrow();
        let s = st.shadow;
        x >= s && y >= s && x < st.size.0 - s && y < st.size.1 - s
    })
}

unsafe fn copy_to_clipboard(hwnd: HWND, text: &str) -> bool {
    let u16s = wide(text);
    let bytes = u16s.len() * std::mem::size_of::<u16>();
    let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes);
    if hmem.is_null() {
        return false;
    }
    let ptr = GlobalLock(hmem) as *mut u16;
    if ptr.is_null() {
        GlobalFree(hmem);
        return false;
    }
    std::ptr::copy_nonoverlapping(u16s.as_ptr(), ptr, u16s.len());
    GlobalUnlock(hmem);
    if OpenClipboard(hwnd) == 0 {
        GlobalFree(hmem);
        return false;
    }
    EmptyClipboard();
    let ok = !SetClipboardData(CF_UNICODETEXT, hmem).is_null();
    CloseClipboard();
    if !ok {
        GlobalFree(hmem);
    }
    ok
}

unsafe fn handle_action(hwnd: HWND, action: SetupAction) {
    match action {
        SetupAction::Close => {
            DestroyWindow(hwnd);
        }
        SetupAction::Refresh => {
            WIZ.with(|w| w.borrow_mut().cli_installed = probe_clis());
            crate::refresh_after_setup(None);
            set_notice("正在重新偵測環境與查詢額度…".into());
        }
        SetupAction::Docs(p) => {
            let url = wide(setup::spec(p).docs_url);
            ShellExecuteW(null_mut(), w!("open"), url.as_ptr(), null(), null(), SW_SHOWNORMAL);
        }
        SetupAction::Copy(p) => {
            let cli = WIZ.with(|w| w.borrow().cli_installed[p.index()]);
            let sp = setup::spec(p);
            let cmd = if cli { sp.login_cmd } else { sp.install_cmd };
            if copy_to_clipboard(hwnd, cmd) {
                set_notice(format!("✓ 已複製指令：{cmd}"));
            } else {
                set_notice("無法存取剪貼簿，請再試一次".into());
            }
        }
        SetupAction::Run(p, mode) => {
            let idx = p.index();
            let already = WIZ.with(|w| w.borrow().running[idx]);
            if already {
                return;
            }
            WIZ.with(|w| w.borrow_mut().running[idx] = true);
            match setup::launch(p, mode, move || crate::post_setup_done(idx)) {
                Ok(()) => {
                    // 若該服務先前被隱藏，順便取消隱藏，設定好就能立刻在工作列看到。
                    crate::unhide_provider(p);
                    set_notice(format!("已開啟 PowerShell 視窗設定 {}，按 Enter 確認後開始。", p.name()));
                }
                Err(e) => {
                    WIZ.with(|w| w.borrow_mut().running[idx] = false);
                    set_notice(format!("⚠ {e}"));
                }
            }
        }
    }
}

fn mouse_xy(lp: LPARAM) -> (i32, i32) {
    ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32)
}

unsafe extern "system" fn wnd_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match m {
        WM_MOUSEMOVE => {
            let (x, y) = mouse_xy(lp);
            let need_track = WIZ.with(|w| {
                let mut st = w.borrow_mut();
                if !st.tracking {
                    st.tracking = true;
                    true
                } else {
                    false
                }
            });
            if need_track {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: h,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut tme);
            }
            let next = hit_at(x, y);
            let changed = WIZ.with(|w| {
                let mut st = w.borrow_mut();
                if st.hover != next {
                    st.hover = next;
                    true
                } else {
                    false
                }
            });
            if changed {
                redraw(false);
            }
            0
        }
        WM_MOUSELEAVE => {
            let changed = WIZ.with(|w| {
                let mut st = w.borrow_mut();
                st.tracking = false;
                let was = st.hover.take().is_some();
                was
            });
            if changed {
                redraw(false);
            }
            0
        }
        WM_SETCURSOR => {
            if (lp & 0xFFFF) as u32 == HTCLIENT {
                let is_hand = WIZ.with(|w| w.borrow().hover.is_some());
                SetCursor(LoadCursorW(null_mut(), if is_hand { IDC_HAND } else { IDC_ARROW }));
                return 1;
            }
            DefWindowProcW(h, m, wp, lp)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = mouse_xy(lp);
            if let Some(act) = hit_at(x, y) {
                WIZ.with(|w| w.borrow_mut().pressed = Some(act));
                SetCapture(h);
                redraw(false);
            } else if inside_card(x, y) {
                // 點在按鈕以外的面板區域時可拖曳視窗。
                SendMessageW(h, WM_NCLBUTTONDOWN, HTCAPTION as usize, 0);
            }
            0
        }
        WM_LBUTTONUP => {
            let (x, y) = mouse_xy(lp);
            let fired = WIZ.with(|w| {
                let mut st = w.borrow_mut();
                st.pressed.take()
            });
            if let Some(act) = fired {
                ReleaseCapture();
                if hit_at(x, y) == Some(act) {
                    handle_action(h, act);
                } else {
                    redraw(false);
                }
            }
            0
        }
        WM_CAPTURECHANGED => {
            let changed = WIZ.with(|w| w.borrow_mut().pressed.take().is_some());
            if changed {
                redraw(false);
            }
            0
        }
        WM_KEYDOWN => {
            match wp as u16 {
                VK_ESCAPE => {
                    DestroyWindow(h);
                }
                VK_F5 => {
                    handle_action(h, SetupAction::Refresh);
                }
                _ => {}
            }
            0
        }
        WM_TIMER if wp == TIMER_NOTICE => {
            KillTimer(h, TIMER_NOTICE);
            WIZ.with(|w| w.borrow_mut().notice = None);
            redraw(false);
            0
        }
        WM_DPICHANGED => {
            let new_dpi = (wp & 0xFFFF).max(96) as f32;
            WIZ.with(|w| w.borrow_mut().scale = new_dpi / 96.0);
            let suggested = *(lp as *const RECT);
            redraw(false);
            let (fw, fh) = WIZ.with(|w| w.borrow().size);
            SetWindowPos(h, null_mut(), suggested.left, suggested.top, fw, fh, SWP_NOZORDER | SWP_NOACTIVATE);
            0
        }
        WM_CLOSE => {
            DestroyWindow(h);
            0
        }
        WM_DESTROY => {
            KillTimer(h, TIMER_NOTICE);
            WIZ.with(|w| {
                let mut st = w.borrow_mut();
                st.hwnd = null_mut();
                st.hover = None;
                st.pressed = None;
                st.tracking = false;
                st.notice = None;
            });
            0
        }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}
