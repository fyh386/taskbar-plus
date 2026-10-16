//! Docker 容器面板：通过命名管道 \\.\pipe\docker_engine 调 Docker HTTP API
//! 无需 Docker CLI / TCP 端口暴露，Docker Desktop 默认即开此管道。
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, GetLastError};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, ReadFile, WriteFile, OPEN_EXISTING};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;

const PIPE: &str = "\\\\.\\pipe\\docker_engine";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 打开管道并发送一个 HTTP/1.1 请求（Connection: close），读至空闲或上限。
/// req_head 须已含 \r\n\r\n 结尾的完整头部（body 暂不支持，logs/list/restart 均无需）。
fn pipe_request(req_head: &str, idle_ms: u64, cap: usize) -> Result<Vec<u8>, String> {
    let name = wide(PIPE);
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ
                | windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        if err == ERROR_FILE_NOT_FOUND {
            return Err("未检测到 Docker（请确认 Docker Desktop 已安装并启动）".into());
        }
        return Err(format!("打开 Docker 管道失败（错误码 {err}）"));
    }

    let result = (|| {
        let bytes = req_head.as_bytes();
        let mut written: u32 = 0;
        let ok = unsafe {
            WriteFile(handle, bytes.as_ptr() as *const _, bytes.len() as u32, &mut written, std::ptr::null_mut())
        };
        if ok == 0 {
            return Err(format!("写入 Docker 管道失败（错误码 {}）", unsafe { GetLastError() }));
        }

        let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
        let mut idle_since = Instant::now();
        let deadline = Instant::now() + Duration::from_millis(idle_ms.saturating_mul(6).max(idle_ms));
        loop {
            if out.len() >= cap {
                break;
            }
            let mut avail: u32 = 0;
            unsafe { PeekNamedPipe(handle, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut avail, std::ptr::null_mut()) };
            if avail > 0 {
                let want = (avail as usize).min(cap - out.len()).min(256 * 1024);
                let mut buf = vec![0u8; want.max(1)];
                let mut got: u32 = 0;
                let ok = unsafe { ReadFile(handle, buf.as_mut_ptr() as *mut _, buf.len() as u32, &mut got, std::ptr::null_mut()) };
                if ok != 0 && got > 0 {
                    out.extend_from_slice(&buf[..got as usize]);
                    idle_since = Instant::now();
                    continue;
                }
            }
            if Instant::now() - idle_since >= Duration::from_millis(idle_ms) {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(out)
    })();

    unsafe { CloseHandle(handle) };
    result
}

/// 拆分 HTTP 响应：返回 (状态码, body)。兼容 chunked 编码。
fn split_response(raw: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("Docker 返回了无法解析的响应（无响应头）")?;
    let head = String::from_utf8_lossy(&raw[..head_end]);
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("无法解析 Docker 响应状态码")?;
    let body = &raw[head_end + 4..];
    let chunked = head.to_ascii_lowercase().lines().any(|l| {
        let l = l.trim();
        l.starts_with("transfer-encoding:") && l.to_ascii_lowercase().contains("chunked")
    });
    let body = if chunked { dechunk(body) } else { body.to_vec() };
    Ok((status, body))
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let line_end = match data.windows(2).position(|w| w == b"\r\n") {
            Some(p) => p,
            None => break,
        };
        let size_str = String::from_utf8_lossy(&data[..line_end]);
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        data = &data[line_end + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size {
            out.extend_from_slice(data);
            break;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size..];
        if data.len() >= 2 && &data[..2] == b"\r\n" {
            data = &data[2..];
        }
    }
    out
}

fn request(method: &str, path: &str) -> Result<(u16, Vec<u8>), String> {
    let head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nUser-Agent: tbp-docker\r\nConnection: close\r\n\r\n");
    let raw = pipe_request(&head, 400, 512 * 1024)?;
    split_response(&raw)
}

pub fn list() -> Result<Value, String> {
    let (status, body) = request("GET", "/containers/json?all=true")?;
    if status != 200 {
        return Err(format!("Docker 返回 {status}"));
    }
    let arr: Vec<Value> = serde_json::from_slice(&body).map_err(|e| format!("解析容器列表失败: {e}"))?;
    let mut out = Vec::new();
    for c in arr {
        let names = c["Names"].as_array().cloned().unwrap_or_default();
        let name = names
            .first()
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .trim_start_matches('/')
            .to_string();
        let state = c["State"].as_str().unwrap_or("").to_string();
        out.push(json!({
            "id": c["Id"].as_str().unwrap_or("")[..12.min(c["Id"].as_str().unwrap_or("").len())].to_string(),
            "name": name,
            "image": c["Image"].as_str().unwrap_or(""),
            "state": state,
            "status": c["Status"].as_str().unwrap_or(""),
            "running": state == "running",
        }));
    }
    Ok(json!({ "containers": out }))
}

pub fn restart(id: &str) -> Result<Value, String> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("非法容器 id".into());
    }
    let (status, _) = request("POST", &format!("/containers/{id}/restart?t=5"))?;
    if status == 204 || status == 304 {
        Ok(json!({ "ok": true }))
    } else {
        Err(format!("重启失败（Docker 返回 {status}）"))
    }
}

pub fn logs(id: &str) -> Result<Value, String> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("非法容器 id".into());
    }
    let (status, body) = request("GET", &format!("/containers/{id}/logs?stdout=1&stderr=1&tail=200"))?;
    if status != 200 {
        return Err(format!("读取日志失败（Docker 返回 {status}）"));
    }
    // Docker 多路复用流：8 字节帧头 [stream:u8, 0,0,0, size:u32 BE] + payload
    let mut text = String::new();
    let mut data: &[u8] = &body;
    while data.len() >= 8 {
        let size = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
        if size == 0 || data.len() < 8 + size {
            break;
        }
        text.push_str(&String::from_utf8_lossy(&data[8..8 + size]));
        data = &data[8 + size..];
    }
    if text.is_empty() {
        text = String::from_utf8_lossy(&body).into_owned();
    }
    let text = if text.len() > 128 * 1024 { text[..128 * 1024].to_string() } else { text };
    Ok(json!({ "logs": text }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dechunk_simple() {
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(dechunk(raw), b"Wikipedia");
    }

    #[test]
    fn dechunk_with_ext() {
        let raw = b"4;foo=bar\r\nWiki\r\n0\r\n\r\n";
        assert_eq!(dechunk(raw), b"Wiki");
    }
}
