//! 各家額度共用的資料模型、時間工具與文字格式。

use std::time::{SystemTime, UNIX_EPOCH};

/// 一個額度視窗，例如「5 小時」「每週」「每月聊天」。
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    /// 工作列上的短標籤，例如 "5H"、"7D"。
    pub short: String,
    /// 提示文字裡的完整名稱，例如 "5 小時"。
    pub name: String,
    /// 已用百分比 0–100。
    pub used: f64,
    /// 重置時間（Unix 秒）。
    pub resets_at: Option<i64>,
    /// 額外說明，例如 "199/200 次"。
    pub detail: Option<String>,
}

/// 某一家目前的額度。`windows` 前兩個會顯示在工作列上。
#[derive(Clone, Debug, PartialEq)]
pub struct Quota {
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    /// 只在提示文字顯示的其他視窗。
    pub extra: Vec<Window>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FetchError {
    /// 找不到登入資料或工具沒安裝；工作列不顯示這家。
    NotSetUp(String),
    /// 登入過期，需要使用者開一下對應工具。
    LoginExpired(String),
    /// 為了避免誤用額度而停用，直到程式重開。
    Disabled(String),
    /// 伺服器說查詢太頻繁（HTTP 429）；查詢會自動放慢。
    RateLimited(String),
    /// 其他暫時性錯誤（網路、伺服器）。
    Other(String),
}

impl FetchError {
    pub fn message(&self) -> &str {
        match self {
            FetchError::NotSetUp(m)
            | FetchError::LoginExpired(m)
            | FetchError::Disabled(m)
            | FetchError::RateLimited(m)
            | FetchError::Other(m) => m,
        }
    }
}

/// 連續被限流 `streak` 次後，下次查詢要等多久（秒）：每次加倍，最長 30 分鐘。
pub fn backoff_secs(base: u64, streak: u32) -> u64 {
    const MAX: u64 = 30 * 60;
    if streak == 0 {
        return base;
    }
    base.saturating_mul(1u64 << streak.min(16)).min(MAX).max(base)
}

/// 使用量的嚴重程度，決定顏色。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Level {
    Normal,
    Warn,
    Crit,
}

pub const WARN_AT: f64 = 75.0;
pub const CRIT_AT: f64 = 90.0;

pub fn level(used: f64) -> Level {
    if used >= CRIT_AT {
        Level::Crit
    } else if used >= WARN_AT {
        Level::Warn
    } else {
        Level::Normal
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// 解析 RFC 3339 時間（"2026-11-24T12:40:00.414610+00:00"、"2026-12-01T00:00:00Z"）
/// 或純日期（"2026-12-01"，視為 UTC 午夜）成 Unix 秒。
pub fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if s.get(4..5)? != "-" || s.get(7..8)? != "-" {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    if s.len() == 10 {
        return Some(days * 86400);
    }
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        let n = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
        rest = &r[n..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) => {
            let h: i64 = o[1..3].parse().ok()?;
            let m: i64 = o[4..6].parse().ok()?;
            let v = h * 3600 + m * 60;
            if o.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    Some(days * 86400 + hh * 3600 + mm * 60 + ss - offset)
}

/// 公曆日期轉成自 1970-01-01 起的天數（Howard Hinnant 演算法）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// 依視窗長度（秒）給短標籤與名稱。
pub fn window_label(seconds: i64) -> (String, String) {
    match seconds {
        18000 => ("5H".into(), "5 小時".into()),
        604800 => ("7D".into(), "每週".into()),
        86400 => ("1D".into(), "每日".into()),
        s if (2_419_200..=2_678_400).contains(&s) => (format!("{}D", s / 86400), format!("{} 天", s / 86400)),
        s if s % 86400 == 0 => (format!("{}D", s / 86400), format!("{} 天", s / 86400)),
        s => (format!("{}H", (s + 1800) / 3600), format!("{} 小時", (s + 1800) / 3600)),
    }
}

/// 「2 小時 7 分」這種剩餘時間文字。
pub fn fmt_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    if d > 0 {
        format!("{d} 天 {h} 小時")
    } else if h > 0 {
        format!("{h} 小時 {m} 分")
    } else {
        format!("{} 分", m.max(1))
    }
}

/// 百分比文字：小於 10 且非整數時留一位小數。
pub fn fmt_pct(v: f64) -> String {
    let r = (v * 10.0).round() / 10.0;
    if r < 10.0 && r.fract() != 0.0 {
        format!("{r:.1}%")
    } else {
        format!("{}%", r.round() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_times() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2026-12-01"), Some(1_796_083_200));
        assert_eq!(parse_time("2026-12-01T00:00:00.000Z"), Some(1_796_083_200));
        assert_eq!(parse_time("2026-11-24T12:40:00.414610+00:00"), Some(1_795_524_000));
        assert_eq!(parse_time("2026-11-24T20:40:00+08:00"), Some(1_795_524_000));
        assert_eq!(parse_time("garbage"), None);
        assert_eq!(parse_time("2026-11-24T12:40:00 PST"), None);
    }

    #[test]
    fn labels_windows() {
        assert_eq!(window_label(18000).0, "5H");
        assert_eq!(window_label(604800).0, "7D");
        assert_eq!(window_label(2592000), ("30D".into(), "30 天".into()));
        assert_eq!(window_label(3600).0, "1H");
    }

    #[test]
    fn backoff_doubles_up_to_30_minutes() {
        assert_eq!(backoff_secs(300, 0), 300);
        assert_eq!(backoff_secs(300, 1), 600);
        assert_eq!(backoff_secs(300, 2), 1200);
        assert_eq!(backoff_secs(300, 3), 1800);
        assert_eq!(backoff_secs(300, 40), 1800);
        assert_eq!(backoff_secs(2400, 1), 2400, "本來就比上限長時維持原間隔");
    }

    #[test]
    fn levels_and_text() {
        assert_eq!(level(74.9), Level::Normal);
        assert_eq!(level(75.0), Level::Warn);
        assert_eq!(level(95.0), Level::Crit);
        assert_eq!(fmt_pct(0.25), "0.3%");
        assert_eq!(fmt_pct(16.0), "16%");
        assert_eq!(fmt_pct(49.13), "49%");
        assert_eq!(fmt_duration(7620), "2 小時 7 分");
        assert_eq!(fmt_duration(30), "1 分");
        assert_eq!(fmt_duration(90000), "1 天 1 小時");
    }
}
