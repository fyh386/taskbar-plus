//! 剪贴板：后台监视（1s 轮询去重）+ 历史记录 + 写回/清空
//! 前端通过 clipboard.* API 访问（需 clipboard 权限）

use serde_json::json;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;
const MAX_ITEMS: usize = 50;

pub struct ClipboardState {
    pub items: Mutex<Vec<(i64, String)>>, // (时间戳, 文本)，新的在前
    last: Mutex<String>,
}

impl ClipboardState {
    pub fn new() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
            last: Mutex::new(String::new()),
        }
    }
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn read_text() -> Option<String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    unsafe {
        // 其他应用可能正持有剪贴板，失败直接跳过本次采样
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let h = GetClipboardData(CF_UNICODETEXT);
        if h.is_null() {
            CloseClipboard();
            return None;
        }
        let p = GlobalLock(h);
        if p.is_null() {
            CloseClipboard();
            return None;
        }
        let mut wide = Vec::new();
        let mut cur = p as *const u16;
        while *cur != 0 {
            wide.push(*cur);
            cur = cur.add(1);
        }
        GlobalUnlock(h);
        CloseClipboard();
        String::from_utf16(&wide).ok()
    }
}

pub fn write_text(text: &str) -> Result<(), String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows_sys::Win32::Foundation::HANDLE;
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("无法打开剪贴板（可能被其他应用占用）".into());
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
        if h.is_null() {
            CloseClipboard();
            return Err("内存分配失败".into());
        }
        let p = GlobalLock(h);
        if p.is_null() {
            CloseClipboard();
            return Err("内存锁定失败".into());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), p as *mut u16, wide.len());
        GlobalUnlock(h);
        SetClipboardData(CF_UNICODETEXT, h as HANDLE);
        CloseClipboard();
        Ok(())
    }
}

/// 后台监视：1s 轮询，内容变化时入库并广播事件
pub fn start_watcher(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let Some(text) = read_text() else { continue };
            let text = text.trim().to_string();
            if text.is_empty() || text.len() > 64 * 1024 {
                continue;
            }
            // ClipboardState 是 AppState 的字段（未单独 manage）
            let state = app.state::<crate::AppState>();
            let mut last = state.clipboard.last.lock().unwrap();
            if *last == text {
                continue;
            }
            *last = text.clone();
            drop(last);

            let mut items = state.clipboard.items.lock().unwrap();
            items.insert(0, (now_ts(), text.clone()));
            items.truncate(MAX_ITEMS);
            let total = items.len();
            drop(items);

            let _ = app.emit_to("dock", "clipboard", json!({ "text": text, "total": total }));
        }
    });
}

pub fn history(state: &ClipboardState, n: usize) -> Vec<serde_json::Value> {
    state
        .items
        .lock()
        .unwrap()
        .iter()
        .take(n)
        .map(|(ts, t)| json!({ "ts": ts, "text": t }))
        .collect()
}

pub fn clear(state: &ClipboardState) {
    state.items.lock().unwrap().clear();
}
