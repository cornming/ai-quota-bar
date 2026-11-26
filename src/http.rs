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
    let (status, bytes, _) = request_raw("GET", host, path, headers, None, 20_000, 4 << 20, true)?;
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

/// POST JSON。`timeout_ms` 是每個階段（連線、送出、接收）的上限。
pub fn post_json(host: &str, path: &str, body: &str, timeout_ms: i32) -> Result<(u32, String), String> {
    let (status, bytes, _) = request_raw(
        "POST",
        host,
        path,
        &[("Content-Type", "application/json"), ("Accept", "application/json")],
        Some(body.as_bytes()),
        timeout_ms,
        4 << 20,
        true,
    )?;
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

/// 解析 `https://host/path?query`（不接受非 HTTPS 或無效主機）。
pub fn parse_https_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if host.is_empty() || host.contains('@') || host.contains(':') || host.chars().any(char::is_whitespace) {
        return None;
    }
    Some((host, path))
}

/// 下載二進位檔：僅允許 HTTPS，且每一跳重新導向都驗證主機在白名單內（最多 5 跳）。
pub fn download_https(
    url: &str,
    allowed_host: impl Fn(&str) -> bool,
    max_bytes: usize,
    timeout_ms: i32,
) -> Result<Vec<u8>, String> {
    let mut current = url.to_string();
    for _ in 0..=5 {
        let (host, path) = parse_https_url(&current).ok_or_else(|| "下載網址不是有效的 HTTPS 連結".to_string())?;
        if !allowed_host(host) {
            return Err(format!("拒絕連線到非預期的主機：{host}"));
        }
        let (status, bytes, location) = request_raw(
            "GET",
            host,
            path,
            &[("Accept", "application/octet-stream")],
            None,
            timeout_ms,
            max_bytes,
            false,
        )?;
        match status {
            200 => return Ok(bytes),
            301 | 302 | 303 | 307 | 308 => {
                let next = location.ok_or_else(|| "重新導向缺少 Location 標頭".to_string())?;
                current = next;
            }
            s => return Err(format!("下載失敗（HTTP {s}）")),
        }
    }
    Err("重新導向次數過多".into())
}

fn request_raw(
    method: &str,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout_ms: i32,
    max_bytes: usize,
    follow_redirects: bool,
) -> Result<(u32, Vec<u8>, Option<String>), String> {
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
        if !follow_redirects {
            let policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
            WinHttpSetOption(
                req.0,
                WINHTTP_OPTION_REDIRECT_POLICY,
                &policy as *const u32 as *const _,
                std::mem::size_of::<u32>() as u32,
            );
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
        let mut location = None;
        if !follow_redirects && matches!(status, 301 | 302 | 303 | 307 | 308) {
            let mut loc_len = 0u32;
            WinHttpQueryHeaders(req.0, WINHTTP_QUERY_LOCATION, null(), null_mut(), &mut loc_len, null_mut());
            if loc_len >= 2 {
                let mut loc_buf = vec![0u16; (loc_len as usize + 1) / 2];
                if WinHttpQueryHeaders(
                    req.0,
                    WINHTTP_QUERY_LOCATION,
                    null(),
                    loc_buf.as_mut_ptr() as *mut _,
                    &mut loc_len,
                    null_mut(),
                ) != 0
                {
                    let end = loc_buf.iter().position(|&c| c == 0).unwrap_or(loc_buf.len());
                    location = Some(String::from_utf16_lossy(&loc_buf[..end]));
                }
            }
            return Ok((status, Vec::new(), location));
        }
        let mut body = Vec::new();
        loop {
            let mut avail: u32 = 0;
            if WinHttpQueryDataAvailable(req.0, &mut avail) == 0 || avail == 0 {
                break;
            }
            let start = body.len();
            if start.saturating_add(avail as usize) > max_bytes {
                return Err("回應太大".into());
            }
            body.resize(start + avail as usize, 0u8);
            let mut read: u32 = 0;
            if WinHttpReadData(req.0, body[start..].as_mut_ptr() as *mut _, avail, &mut read) == 0 {
                return Err("讀取回應失敗".into());
            }
            body.truncate(start + read as usize);
        }
        Ok((status, body, location))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_urls_strictly() {
        assert_eq!(
            parse_https_url("https://github.com/cornming/ai-quota-bar/releases/download/v0.6.0/a.exe"),
            Some(("github.com", "/cornming/ai-quota-bar/releases/download/v0.6.0/a.exe"))
        );
        assert_eq!(parse_https_url("https://github.com"), Some(("github.com", "/")));
        assert_eq!(parse_https_url("http://github.com/a"), None);
        assert_eq!(parse_https_url("https://user@github.com/a"), None);
        assert_eq!(parse_https_url("https://github.com:443/a"), None);
        assert_eq!(parse_https_url("https:///a"), None);
    }
}
