//! 邮件提醒：最小 IMAP over TLS 客户端（LOGIN + STATUS，只读）
//! 用授权码登录（QQ/163/Gmail 等），查询指定文件夹未读数，不修改任何邮件状态。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use serde_json::{json, Value};

fn imap_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 一行 IMAP 响应：tag 前缀 + 其余
fn read_line(stream: &mut impl Read, buf: &mut Vec<u8>) -> Result<String, String> {
    let mut byte = [0u8; 1];
    buf.clear();
    loop {
        stream.read(&mut byte).map_err(|e| e.to_string())?;
        if byte[0] == b'\n' { break; }
        if byte[0] != b'\r' { buf.push(byte[0]); }
        if buf.len() > 65536 { return Err("IMAP 响应行过长".into()); }
    }
    String::from_utf8(buf.clone()).map_err(|_| "IMAP 响应非 UTF-8".into())
}

fn send(stream: &mut impl Write, line: &str) -> Result<(), String> {
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(b"\r\n").map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())
}

/// 读取直到出现 "tag OK"/"tag NO"/"tag BAD" 的完成行，返回完成行内容
fn read_until_done(stream: &mut impl Read, tag: &str, buf: &mut Vec<u8>) -> Result<String, String> {
    let prefix = tag.to_string() + " ";
    loop {
        let line = read_line(stream, buf)?;
        if line.starts_with(&prefix) {
            if line.contains(" OK") { return Ok(line); }
            return Err(format!("IMAP 错误: {line}"));
        }
        // 未标记的协议数据行，继续读
        if buf.is_empty() && line.is_empty() { return Err("IMAP 连接中断".into()); }
    }
}

/// 查询未读邮件数。
/// args: host / port(默认993) / user / pass / folder(默认 INBOX)
pub fn unread(args: &Value) -> Result<Value, String> {
    let host = args.get("host").and_then(|v| v.as_str()).ok_or("缺少参数 host")?;
    let port = args.get("port").and_then(|v| v.as_u64()).unwrap_or(993) as u16;
    let user = args.get("user").and_then(|v| v.as_str()).ok_or("缺少参数 user")?;
    let pass = args.get("pass").and_then(|v| v.as_str()).ok_or("缺少参数 pass")?;
    let folder = args.get("folder").and_then(|v| v.as_str()).unwrap_or("INBOX");

    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = ServerName::try_from(host.to_string()).map_err(|_| "非法主机名".to_string())?;
    let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name).map_err(|e| e.to_string())?;

    let mut tcp = TcpStream::connect((host, port)).map_err(|e| format!("连接失败: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(20))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(20))).ok();
    let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
    let mut buf = Vec::new();

    // 问候语
    let mut greeting = read_line(&mut tls, &mut buf)?;
    if !greeting.starts_with("* OK") && !greeting.starts_with("* PREAUTH") {
        return Err(format!("IMAP 问候异常: {greeting}"));
    }

    // LOGIN
    let tag = "A1";
    send(&mut tls, &format!("{tag} LOGIN \"{}\" \"{}\"", imap_quote(user), imap_quote(pass)))?;
    greeting = read_until_done(&mut tls, tag, &mut buf)?;
    if greeting.contains("AUTHENTICATIONFAILED") || greeting.contains("NO ") {
        // NO 分支已由 read_until_done 返回 Err；此处兜底
    }

    // STATUS folder (UNSEEN MESSAGES)
    let tag2 = "A2";
    send(&mut tls, &format!("{tag2} STATUS \"{}\" (UNSEEN MESSAGES)", imap_quote(folder)))?;
    let mut status_line = String::new();
    let prefix = tag2.to_string() + " ";
    loop {
        let line = read_line(&mut tls, &mut buf)?;
        if line.starts_with(&prefix) {
            if !line.contains(" OK") {
                let _ = send(&mut tls, "A3 LOGOUT");
                return Err(format!("STATUS 失败: {line}"));
            }
            break;
        }
        if line.starts_with("* STATUS") {
            status_line = line;
        }
    }

    let _ = send(&mut tls, "A3 LOGOUT");

    // 解析 * STATUS "INBOX" (UNSEEN 3 MESSAGES 120)
    let unseen = parse_status_num(&status_line, "UNSEEN").unwrap_or(0);
    let total = parse_status_num(&status_line, "MESSAGES").unwrap_or(0);
    Ok(json!({ "unseen": unseen, "total": total }))
}

/// 解析 STATUS 行中的数字字段；容忍 "(" 粘连，如 (UNSEEN 3
fn parse_status_num(line: &str, key: &str) -> Option<u64> {
    let mut it = line.split_whitespace();
    while let Some(tok) = it.next() {
        let t = tok.trim_matches(|c| c == '(' || c == ')');
        if t.eq_ignore_ascii_case(key) {
            return it
                .next()
                .map(|n| n.trim_matches(|c| c == '(' || c == ')'))
                .and_then(|n| n.parse().ok());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_escapes_specials() {
        assert_eq!(imap_quote("a\"b\\c"), "a\\\"b\\\\c");
    }

    #[test]
    fn status_parse_helper() {
        let line = "* STATUS \"INBOX\" (UNSEEN 3 MESSAGES 120)";
        assert_eq!(parse_status_num(line, "UNSEEN"), Some(3));
        assert_eq!(parse_status_num(line, "MESSAGES"), Some(120));
        assert_eq!(parse_status_num(line, "RECENT"), None);
        // 无括号变体
        let line2 = "* STATUS INBOX (UNSEEN 0 MESSAGES 7)";
        assert_eq!(parse_status_num(line2, "MESSAGES"), Some(7));
    }
}
