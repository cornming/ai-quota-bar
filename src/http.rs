//! 用 Windows 內建的 WinHTTP 發 HTTPS GET，不必另外帶 TLS 函式庫。

use std::ptr::{null, null_mut};
use windows_sys::Win32::Networking::WinHttp::*;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct Handle(*mut core::ffi::c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

/// GET `https://{host}{path}`，回傳 (HTTP 狀態碼, 內容)。
pub fn get(host: &str, path: &str, headers: &[(&str, &str)]) -> Result<(u32, String), String> {
    request("GET", host, path, headers, None, 20_000)
}

/// POST JSON。`timeout_ms` 是每個階段（連線、送出、接收）的上限。
pub fn post_json(host: &str, path: &str, body: &str, timeout_ms: i32) -> Result<(u32, String), String> {
    request(
        "POST",
        host,
        path,
        &[("Content-Type", "application/json"), ("Accept", "application/json")],
        Some(body.as_bytes()),
        timeout_ms,
    )
}

fn request(
    method: &str,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout_ms: i32,
) -> Result<(u32, String), String> {
    unsafe {
        let session = Handle(WinHttpOpen(
            wide(concat!("ai-quota-bar/", env!("CARGO_PKG_VERSION"))).as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        ));
        if session.0.is_null() {
            return Err("無法初始化網路連線".into());
        }
        WinHttpSetTimeouts(session.0, timeout_ms, timeout_ms, timeout_ms, timeout_ms);
        let conn = Handle(WinHttpConnect(session.0, wide(host).as_ptr(), INTERNET_DEFAULT_HTTPS_PORT as u16, 0));
        if conn.0.is_null() {
            return Err(format!("無法連線到 {host}"));
        }
        let req = Handle(WinHttpOpenRequest(
            conn.0,
            wide(method).as_ptr(),
            wide(path).as_ptr(),
            null(),
            null(),
            null(),
            WINHTTP_FLAG_SECURE,
        ));
        if req.0.is_null() {
            return Err("無法建立連線要求".into());
        }
        let hdrs: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        let hw: Vec<u16> = hdrs.encode_utf16().collect();
        let (bp, bl) = match body {
            Some(b) => (b.as_ptr() as *const core::ffi::c_void, b.len() as u32),
            None => (null(), 0),
        };
        if WinHttpSendRequest(req.0, hw.as_ptr(), hw.len() as u32, bp, bl, bl, 0) == 0
            || WinHttpReceiveResponse(req.0, null_mut()) == 0
        {
            return Err(format!("連不上 {host}，請檢查網路"));
        }
        let mut status: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut status as *mut u32 as *mut _,
            &mut len,
            null_mut(),
        );
        let mut body = Vec::new();
        loop {
            let mut avail: u32 = 0;
            if WinHttpQueryDataAvailable(req.0, &mut avail) == 0 || avail == 0 {
                break;
            }
            let start = body.len();
            body.resize(start + avail as usize, 0u8);
            let mut read: u32 = 0;
            if WinHttpReadData(req.0, body[start..].as_mut_ptr() as *mut _, avail, &mut read) == 0 {
                return Err("讀取回應失敗".into());
            }
            body.truncate(start + read as usize);
            if body.len() > 4 << 20 {
                return Err("回應太大".into());
            }
        }
        Ok((status, String::from_utf8_lossy(&body).into_owned()))
    }
}
