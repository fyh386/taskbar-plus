//! 本地抓包代理（类 Fiddler）：HTTP 直连转发 + HTTPS CONNECT MITM。
//!
//! - 只绑 127.0.0.1；系统代理切换写 HKCU（无需管理员），停止时还原原设置
//! - CA 首次启动时生成并安装到「当前用户」Root 存储
//!   （certutil -user -addstore Root，无需管理员）；按目标域名动态签发证书
//! - 上游 TLS 只协商 http/1.1（不.offer h2），MITM 后按 HTTP/1.1 明文解析
//! - 每个完整请求/响应（含 header/body，body 截断 256KB）以事件流
//!   （emit_to "dock" / "proxy"）推给前端插件
//! - 非 443 端口的 CONNECT 不做 MITM，仅盲转发并记录 CONNECT 信息

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

const BODY_CAP: usize = 256 * 1024; // 事件体截断上限
const HEAD_CAP: usize = 64 * 1024;
const REQ_BODY_HARD_CAP: usize = 32 * 1024 * 1024;

static RUNNING: AtomicBool = AtomicBool::new(false);
static REQ_ID: AtomicU64 = AtomicU64::new(0);
static CUR_PORT: AtomicU32 = AtomicU32::new(0);
static HANDLE: OnceLock<Arc<Mutex<Option<AppHandle>>>> = OnceLock::new();
static PREV_PROXY: Mutex<Option<(u32, String)>> = Mutex::new(None);
static CA: OnceLock<Arc<Ca>> = OnceLock::new();
static HOST_CERTS: OnceLock<Mutex<HashMap<String, Arc<rustls::ServerConfig>>>> = OnceLock::new();
static CLIENT_CFG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
static PROXY_LOG: OnceLock<std::path::PathBuf> = OnceLock::new();

/// 调试日志（release 无控制台，写入 data_dir/proxy.log）
fn plog(msg: &str) {
    if let Some(p) = PROXY_LOG.get() {
        let line = format!("{} {msg}\n", chrono_free_now());
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

struct Ca {
    _key_pem: String,
    cert: rcgen::Certificate,
    key: rcgen::KeyPair,
}

// ==================== 启动 / 停止 / 状态 ====================

pub fn start(app: AppHandle, data_dir: &std::path::Path, port: u16) -> Result<Value, String> {
    if RUNNING.load(Ordering::SeqCst) {
        return Ok(json!({ "running": true, "port": CUR_PORT.load(Ordering::SeqCst) }));
    }
    ensure_ca(app.clone(), data_dir)?;
    HANDLE.get_or_init(|| Arc::new(Mutex::new(None))).lock().unwrap().replace(app.clone());

    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("代理端口绑定失败: {e}"))?;
    let actual = listener.local_addr().map_err(|e| e.to_string())?.port();
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    CUR_PORT.store(actual as u32, Ordering::SeqCst);
    RUNNING.store(true, Ordering::SeqCst);

    std::thread::spawn(move || {
        while RUNNING.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((s, _)) => {
                    let app = HANDLE.get().and_then(|h| h.lock().unwrap().clone());
                    if let Some(app) = app {
                        std::thread::spawn(move || {
                            let _ = handle_conn(s, app);
                        });
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => break,
            }
        }
    });

    set_system_proxy(actual)?;
    Ok(json!({ "running": true, "port": actual }))
}

pub fn stop() -> Result<Value, String> {
    if !RUNNING.swap(false, Ordering::SeqCst) {
        return Ok(json!({ "running": false }));
    }
    clear_system_proxy();
    Ok(json!({ "running": false }))
}

pub fn status() -> Value {
    json!({
        "running": RUNNING.load(Ordering::SeqCst),
        "port": CUR_PORT.load(Ordering::SeqCst),
        "requests": REQ_ID.load(Ordering::SeqCst),
    })
}

// ==================== CA ====================

fn hosts_file() -> String { "tbp-proxy".to_string() }

fn ensure_ca(app: AppHandle, data_dir: &std::path::Path) -> Result<(), String> {
    if CA.get().is_some() { return Ok(()); }
    let dir = data_dir.join("proxy");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let _ = PROXY_LOG.set(dir.join("proxy.log"));
    let cert_path = dir.join("ca.crt");
    let key_path = dir.join("ca.key");

    let (cert, key) = if cert_path.exists() && key_path.exists() {
        let cert_pem = std::fs::read_to_string(&cert_path).map_err(|e| e.to_string())?;
        let key_pem = std::fs::read_to_string(&key_path).map_err(|e| e.to_string())?;
        let key = rcgen::KeyPair::from_pem(&key_pem).map_err(|e| format!("CA 密钥解析失败: {e}"))?;
        // rcgen 0.13 官方重载路径：from_ca_cert_pem 恢复 params（保留 SKI/序列号），原密钥重签
        let params = rcgen::CertificateParams::from_ca_cert_pem(&cert_pem)
            .map_err(|e| format!("CA 证书解析失败: {e}"))?;
        let cert = params.self_signed(&key).map_err(|e| format!("CA 证书恢复失败: {e}"))?;
        (cert, key)
    } else {
        let key = rcgen::KeyPair::generate().map_err(|e| format!("CA 密钥生成失败: {e}"))?;
        let mut params = rcgen::CertificateParams::new(vec![hosts_file()])
            .map_err(|e| format!("CA 参数生成失败: {e}"))?;
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(rcgen::DnType::CommonName, "TaskBar Plus Proxy CA");
        let cert = params.self_signed(&key).map_err(|e| format!("CA 证书生成失败: {e}"))?;
        std::fs::write(&cert_path, cert.pem()).map_err(|e| e.to_string())?;
        std::fs::write(&key_path, key.serialize_pem()).map_err(|e| e.to_string())?;
        // 首次：安装到当前用户受信任根（无需管理员）
        install_ca_user_store(app, &cert_path)?;
        (cert, key)
    };

    let _ = CA.set(Arc::new(Ca { _key_pem: key.serialize_pem(), cert, key }));
    Ok(())
}

fn install_ca_user_store(_app: AppHandle, cert_path: &std::path::Path) -> Result<(), String> {
    let out = std::process::Command::new("certutil")
        .args(["-user", "-addstore", "Root"])
        .arg(cert_path)
        .output()
        .map_err(|e| format!("certutil 启动失败: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "CA 安装失败: {}（可手动导入 {}",
            String::from_utf8_lossy(&out.stderr),
            cert_path.display()
        ));
    }
    Ok(())
}

/// 取目标域名的 MITM 服务端配置（带缓存）
fn host_server_config(host: &str) -> Result<Arc<rustls::ServerConfig>, String> {
    let cache = HOST_CERTS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cfg) = cache.lock().unwrap().get(host) {
        return Ok(cfg.clone());
    }
    let ca = CA.get().ok_or("CA 未初始化")?;
    let key = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
    let params = rcgen::CertificateParams::new(vec![host.to_string()]).map_err(|e| e.to_string())?;
    let child = params.signed_by(&key, &ca.cert, &ca.key).map_err(|e| e.to_string())?;

    let cert_der = rustls::pki_types::CertificateDer::from(child.der().to_vec());
    let ca_der = rustls::pki_types::CertificateDer::from(
        rcgen::Certificate::der(&ca.cert).to_vec(),
    );
    // 服务端签名直接用 rcgen 生成的 PKCS#8 私钥（rustls ring 后端支持）
    let signing = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()),
    );
    let cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der, ca_der], signing)
        .map_err(|e| e.to_string())?;
    let cfg = Arc::new(cfg);
    cache.lock().unwrap().insert(host.to_string(), cfg.clone());
    Ok(cfg)
}

fn client_config() -> Arc<rustls::ClientConfig> {
    CLIENT_CFG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut cfg = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(cfg)
    }).clone()
}

// ==================== 系统代理 ====================

fn reg(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("reg")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn set_system_proxy(port: u16) -> Result<(), String> {
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    let prev_enable = reg(&["query", key, "/v", "ProxyEnable"])
        .ok()
        .and_then(|s| {
            let tok = s.split_whitespace().last()?.to_string();
            tok.parse::<u32>().ok()
        })
        .unwrap_or(0);
    let prev_server = reg(&["query", key, "/v", "ProxyServer"])
        .ok()
        .filter(|s| s.contains("ProxyServer"))
        .and_then(|s| {
            // 解析 "ProxyServer    REG_SZ    127.0.0.1:8888"
            s.lines().find(|l| l.contains("ProxyServer"))
                .map(|l| l.split("REG_SZ").nth(1).unwrap_or("").trim().to_string())
        })
        .unwrap_or_default();
    *PREV_PROXY.lock().unwrap() = Some((prev_enable, prev_server));

    reg(&["add", key, "/v", "ProxyEnable", "/t", "REG_DWORD", "/d", "1", "/f"]).map(|_| ())?;
    reg(&["add", key, "/v", "ProxyServer", "/t", "REG_SZ", "/d", &format!("127.0.0.1:{port}"), "/f"]).map(|_| ())?;
    refresh_ie_settings();
    Ok(())
}

fn clear_system_proxy() {
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    if let Some((enable, server)) = PREV_PROXY.lock().unwrap().take() {
        let _ = reg(&["add", key, "/v", "ProxyEnable", "/t", "REG_DWORD", "/d", &enable.to_string(), "/f"]);
        if server.is_empty() {
            let _ = reg(&["delete", key, "/v", "ProxyServer", "/f"]);
        } else {
            let _ = reg(&["add", key, "/v", "ProxyServer", "/t", "REG_SZ", "/d", &server, "/f"]);
        }
    } else {
        let _ = reg(&["add", key, "/v", "ProxyEnable", "/t", "REG_DWORD", "/d", "0", "/f"]);
    }
    refresh_ie_settings();
}

fn refresh_ie_settings() {
    use windows_sys::Win32::Networking::WinInet::{
        INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED, InternetSetOptionW,
    };
    unsafe {
        InternetSetOptionW(std::ptr::null_mut(), INTERNET_OPTION_SETTINGS_CHANGED, std::ptr::null_mut(), 0);
        InternetSetOptionW(std::ptr::null_mut(), INTERNET_OPTION_REFRESH, std::ptr::null_mut(), 0);
    }
}

// ==================== HTTP 解析 / 转发 ====================

fn read_head(s: &mut impl Read) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::with_capacity(2048);
    let mut byte = [0u8; 1];
    loop {
        match s.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => {
                buf.push(byte[0]);
                if buf.ends_with(b"\r\n\r\n") { return Ok(Some(buf)); }
                if buf.len() > HEAD_CAP { return Ok(None); }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn parse_head(head: &[u8]) -> Option<(String, Vec<(String, String)>)> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let first = lines.next()?.to_string();
    let mut headers = Vec::new();
    for l in lines {
        if l.is_empty() { continue; }
        if let Some(i) = l.find(':') {
            headers.push((l[..i].trim().to_string(), l[i + 1..].trim().to_string()));
        }
    }
    Some((first, headers))
}

fn body_framing(headers: &[(String, String)], is_req: bool, method: &str) -> (Option<u64>, bool) {
    let chunked = headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.to_lowercase().contains("chunked"));
    let cl = headers.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<u64>().ok());
    let no_body = !is_req && (method == "HEAD") ;
    if no_body { return (Some(0), false); }
    (cl, chunked)
}

/// 读请求体（完整读入内存，硬上限 REQ_BODY_HARD_CAP）
fn read_req_body(s: &mut impl Read, cl: Option<u64>, chunked: bool) -> std::io::Result<(Vec<u8>, bool)> {
    let mut body = Vec::new();
    match (cl, chunked) {
        (Some(0), _) => Ok((body, false)),
        (Some(n), false) => {
            let n = n as usize;
            if n > REQ_BODY_HARD_CAP { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "请求体过大")); }
            body.resize(n, 0);
            s.read_exact(&mut body)?;
            Ok((body, false))
        }
        (_, true) => {
            let mut total = 0usize;
            loop {
                let mut size_line = String::new();
                let mut b = [0u8; 1];
                while s.read(&mut b)? > 0 {
                    if b[0] == b'\n' { break; }
                    if b[0] != b'\r' { size_line.push(b[0] as char); }
                }
                let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
                if size == 0 {
                    // 尾部的 trailer 以空行结束
                    let mut t = [0u8; 2];
                    let _ = s.read_exact(&mut t);
                    break;
                }
                total += size;
                if total > REQ_BODY_HARD_CAP { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "请求体过大")); }
                let start = body.len();
                body.resize(start + size, 0);
                s.read_exact(&mut body[start..])?;
                let mut crlf = [0u8; 2];
                s.read_exact(&mut crlf)?;
            }
            Ok((body, false))
        }
        (None, false) => Ok((body, false)), // 无长度请求体视为空
    }
}

/// 写请求（重写 Content-Length，去掉代理相关头）
fn write_req(s: &mut (impl Read + Write), method: &str, path: &str, headers: &[(String, String)], body: &[u8]) -> std::io::Result<()> {
    write!(s, "{method} {path} HTTP/1.1\r\n")?;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("proxy-connection")
            || k.eq_ignore_ascii_case("proxy-authenticate")
            || k.eq_ignore_ascii_case("proxy-authorization")
            || k.eq_ignore_ascii_case("content-length")
            || k.eq_ignore_ascii_case("transfer-encoding")
            || k.eq_ignore_ascii_case("connection")
            || k.eq_ignore_ascii_case("expect")
        { continue; }
        write!(s, "{k}: {v}\r\n")?;
    }
    write!(s, "Content-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
    s.write_all(body)?;
    s.flush()
}

/// 响应体流式转发 + 旁路捕获（最多留 BODY_CAP 用于上报）
fn relay_resp_body(src: &mut impl Read, dst: &mut impl Write, cl: Option<u64>, chunked: bool) -> std::io::Result<(Vec<u8>, usize, bool)> {
    let mut captured = Vec::new();
    let mut total = 0usize;
    let mut truncated = false;
    let mut buf = [0u8; 8192];
    if chunked {
        // 原样转发 chunked 帧，同时解码捕获
        loop {
            let mut line = Vec::new();
            let mut b = [0u8; 1];
            loop {
                let n = src.read(&mut b)?;
                if n == 0 { return Ok((captured, total, truncated)); }
                line.push(b[0]);
                dst.write_all(&b)?;
                if b[0] == b'\n' { break; }
            }
            let sz = usize::from_str_radix(String::from_utf8_lossy(&line).trim(), 16).unwrap_or(0);
            if sz == 0 {
                // 转发 trailer 到空行
                loop {
                    let n = src.read(&mut b)?;
                    if n == 0 { break; }
                    dst.write_all(&b)?;
                    if b[0] == b'\n' { break; }
                }
                dst.flush()?;
                return Ok((captured, total, truncated));
            }
            let mut remain = sz;
            let blen = buf.len();
            while remain > 0 {
                let n = src.read(&mut buf[..remain.min(blen)])?;
                if n == 0 { return Ok((captured, total, truncated)); }
                total += n;
                if captured.len() < BODY_CAP {
                    let take = n.min(BODY_CAP - captured.len());
                    captured.extend_from_slice(&buf[..take]);
                } else { truncated = true; }
                dst.write_all(&buf[..n])?;
                remain -= n;
            }
            let mut crlf = [0u8; 2];
            src.read_exact(&mut crlf)?;
            dst.write_all(&crlf)?;
        }
    }
    match cl {
        Some(n) => {
            let mut remain = n;
            let blen = buf.len();
            while remain > 0 {
                let n2 = src.read(&mut buf[..(remain as usize).min(blen)])?;
                if n2 == 0 { break; }
                total += n2;
                if captured.len() < BODY_CAP {
                    let take = n2.min(BODY_CAP - captured.len());
                    captured.extend_from_slice(&buf[..take]);
                } else { truncated = true; }
                dst.write_all(&buf[..n2])?;
                remain -= n2 as u64;
            }
        }
        None => loop {
            let n2 = src.read(&mut buf)?;
            if n2 == 0 { break; }
            total += n2;
            if captured.len() < BODY_CAP {
                let take = n2.min(BODY_CAP - captured.len());
                captured.extend_from_slice(&buf[..take]);
            } else { truncated = true; }
            dst.write_all(&buf[..n2])?;
        },
    }
    dst.flush()?;
    Ok((captured, total, truncated))
}

// ==================== 事件上报 ====================

fn headers_json(headers: &[(String, String)]) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in headers {
        m.insert(k.clone(), Value::String(v.clone()));
    }
    Value::Object(m)
}

fn body_json(body: &[u8], truncated: bool) -> Value {
    let binary = std::str::from_utf8(body).is_err();
    json!({
        "text": if binary { String::new() } else { String::from_utf8_lossy(body).to_string() },
        "binary": binary,
        "size": body.len(),
        "truncated": truncated,
    })
}

fn log_exchange(
    app: &AppHandle,
    method: &str,
    url: &str,
    status: u16,
    req_h: &[(String, String)],
    req_b: &[u8],
    req_trunc: bool,
    resp_h: &[(String, String)],
    resp_b: &[u8],
    resp_trunc: bool,
    started: Instant,
) {
    let id = REQ_ID.fetch_add(1, Ordering::SeqCst) + 1;
    // 记录 UA：辨认流量来源（浏览器 / 桌面应用 / 插件）——"这条请求是谁发的"一查便知
    for (k, v) in req_h {
        if k.eq_ignore_ascii_case("user-agent") {
            plog(&format!("{method} {url} UA={v}"));
            break;
        }
    }
    let payload = json!({
        "id": id,
        "ts": chrono_free_now(),
        "method": method,
        "url": url,
        "status": status,
        "durationMs": started.elapsed().as_millis() as u64,
        "reqHeaders": headers_json(req_h),
        "reqBody": body_json(req_b, req_trunc),
        "respHeaders": headers_json(resp_h),
        "respBody": body_json(resp_b, resp_trunc),
    });
    let _ = app.emit_to("dock", "proxy", payload);
}

/// 免依赖的秒级时间戳
fn chrono_free_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ==================== 连接处理 ====================

fn handle_conn(client: TcpStream, app: AppHandle) -> std::io::Result<()> {
    // accept 自非阻塞 listener，套接字继承非阻塞态：rustls 读写遇 10035 直接报错
    // 中断 MITM（HTTPS 抓包失败的根因）。显式还原为阻塞 + 读超时。
    client.set_nonblocking(false)?;
    client.set_read_timeout(Some(Duration::from_secs(120)))?;
    client.set_nodelay(true).ok();
    let mut c = client;
    let head = match read_head(&mut c)? {
        Some(h) => h,
        None => return Ok(()),
    };
    let (first, headers) = match parse_head(&head) {
        Some(v) => v,
        None => return Ok(()),
    };
    let parts: Vec<&str> = first.split_whitespace().collect();
    if parts.len() == 3 && parts[0].eq_ignore_ascii_case("CONNECT") {
        return handle_connect(c, app, parts[1], &headers);
    }
    if parts.len() == 3 {
        return handle_absolute(c, app, parts[0], parts[1], parts[2], &headers, &head);
    }
    Ok(())
}

/// HTTP 明文（absolute-form）：每个请求新建上游，Connection: close
fn handle_absolute(
    mut client: TcpStream,
    app: AppHandle,
    method: &str,
    target: &str,
    version: &str,
    headers: &[(String, String)],
    _raw_head: &[u8],
) -> std::io::Result<()> {
    let started = Instant::now();
    let host = headers.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.clone())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "缺少 Host"))?;
    let (h, p) = split_host_port(&host, 80);
    let mut upstream = TcpStream::connect((h.as_str(), p))?;
    upstream.set_read_timeout(Some(Duration::from_secs(120)))?;
    upstream.set_nodelay(true).ok();

    let (cl, chunked) = body_framing(headers, true, method);
    let mut req_body = Vec::new();
    let mut req_trunc = false;
    if cl.unwrap_or(0) > 0 || chunked {
        // Expect: 100-continue 先放行
        if headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("expect") && v.contains("100")) {
            client.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        }
        let (b, t) = read_req_body(&mut client, cl, chunked)?;
        req_body = b; req_trunc = t;
    }

    // 转 origin-form 路径
    let path = target.trim_start_matches("http://").trim_start_matches("https://");
    let path = path.find('/').map(|i| &path[i..]).unwrap_or("/").to_string();
    write_req(&mut upstream, method, &path, headers, &req_body)?;

    // 读响应头 → 原样回写 → 流式转发体
    let resp_head = match read_head(&mut upstream)? {
        Some(h) => h,
        None => return Ok(()),
    };
    client.write_all(&resp_head)?;
    let (rfirst, rheaders) = parse_head(&resp_head).unwrap_or_default();
    let status: u16 = rfirst.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let (rcl, rchunked) = body_framing(&rheaders, false, method);
    let (cap, _total, rtrunc) = relay_resp_body(&mut upstream, &mut client, rcl, rchunked)?;

    log_exchange(&app, method, &format!("http://{host}{path}"), status, headers, &req_body, req_trunc, &rheaders, &cap, rtrunc, started);
    let _ = version;
    Ok(())
}

/// HTTPS CONNECT：443 走 MITM，其余盲转发
fn handle_connect(mut client: TcpStream, app: AppHandle, authority: &str, _headers: &[(String, String)]) -> std::io::Result<()> {
    let (host, port) = split_host_port(authority, 443);
    let upstream = match TcpStream::connect((host.as_str(), port)) {
        Ok(u) => u,
        Err(_) => {
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n");
            return Ok(());
        }
    };
    upstream.set_read_timeout(Some(Duration::from_secs(120))).ok();
    upstream.set_nodelay(true).ok();

    if port != 443 {
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
        log_tunnel_connect(&app, &host, port);
        return tunnel_raw(client, upstream);
    }
    plog(&format!("MITM begin {host}"));

    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;

    // 客户端侧 TLS（MITM）
    let scfg = match host_server_config(&host) {
        Ok(c) => c,
        Err(e) => { plog(&format!("host_server_config({host}) failed: {e}")); return Ok(()); }
    };
    let mut server_conn = match rustls::ServerConnection::new(scfg) {
        Ok(s) => s,
        Err(e) => { plog(&format!("ServerConnection::new failed: {e:?}")); return Ok(()); }
    };
    let mut c_tls = rustls::Stream::new(&mut server_conn, &mut client);

    // 上游侧 TLS（真实服务器，只协商 http/1.1）
    let ccfg = client_config();
    let sni = match rustls::pki_types::ServerName::try_from(host.clone()) {
        Ok(s) => s,
        Err(e) => { plog(&format!("ServerName({host}) failed: {e}")); return Ok(()); }
    };
    let mut client_conn = match rustls::ClientConnection::new(ccfg, sni) {
        Ok(c) => c,
        Err(e) => { plog(&format!("ClientConnection::new failed: {e:?}")); return Ok(()); }
    };
    let mut upstream = upstream;
    let mut s_tls = rustls::Stream::new(&mut client_conn, &mut upstream);

    loop {
        let head = match read_head(&mut c_tls) {
            Ok(Some(h)) => h,
            Ok(None) => break,
            Err(e) => { plog(&format!("c_tls read_head failed: {e}")); break; }
        };
        let (first, headers) = match parse_head(&head) {
            Some(v) => v,
            None => break,
        };
        let parts: Vec<&str> = first.split_whitespace().collect();
        if parts.len() != 3 { break; }
        let (method, path, _ver) = (parts[0], parts[1], parts[2]);
        let (cl, chunked) = body_framing(&headers, true, method);
        let mut req_body = Vec::new();
        let mut req_trunc = false;
        if cl.unwrap_or(0) > 0 || chunked {
            if headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("expect") && v.contains("100")) {
                let _ = c_tls.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            }
            match read_req_body(&mut c_tls, cl, chunked) {
                Ok((b, t)) => { req_body = b; req_trunc = t; }
                Err(_) => break,
            }
        }
        let started = Instant::now();
        if let Err(e) = write_req(&mut s_tls, method, path, &headers, &req_body) {
            plog(&format!("s_tls write_req failed: {e}"));
            break;
        }
        let url = format!("https://{host}{path}");

        // 响应
        let resp_head = match read_head(&mut s_tls) {
            Ok(Some(h)) => h,
            Ok(None) => {
                log_exchange(&app, method, &url, 0, &headers, &req_body, req_trunc, &[], &[], false, started);
                break;
            }
            Err(e) => {
                plog(&format!("s_tls read_head failed: {e}"));
                log_exchange(&app, method, &url, 0, &headers, &req_body, req_trunc, &[], &[], false, started);
                break;
            }
        };
        if let Err(e) = c_tls.write_all(&resp_head) {
            plog(&format!("c_tls write resp_head failed: {e}"));
            break;
        }
        let (rfirst, rheaders) = parse_head(&resp_head).unwrap_or_default();
        let status: u16 = rfirst.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let (rcl, rchunked) = body_framing(&rheaders, false, method);
        let (cap, _total, rtrunc) = match relay_resp_body(&mut s_tls, &mut c_tls, rcl, rchunked) {
            Ok(v) => v,
            Err(_) => break,
        };
        log_exchange(&app, method, &url, status, &headers, &req_body, req_trunc, &rheaders, &cap, rtrunc, started);

        let close_req = headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close"));
        if close_req { break; }
    }
    Ok(())
}

fn log_tunnel_connect(app: &AppHandle, host: &str, port: u16) {
    let id = REQ_ID.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = app.emit_to("dock", "proxy", json!({
        "id": id,
        "ts": chrono_free_now(),
        "method": "CONNECT",
        "url": format!("{host}:{port}"),
        "status": 0,
        "durationMs": 0u64,
        "tunnel": true,
        "reqHeaders": {},
        "reqBody": { "text": "", "binary": false, "size": 0, "truncated": false },
        "respHeaders": {},
        "respBody": { "text": "", "binary": false, "size": 0, "truncated": false },
    }));
}

/// 盲转发（非 443 CONNECT）
fn tunnel_raw(mut a: TcpStream, mut b: TcpStream) -> std::io::Result<()> {
    a.set_read_timeout(None).ok();
    b.set_read_timeout(None).ok();
    let mut a2 = a.try_clone()?;
    let mut b2 = b.try_clone()?;
    let t1 = std::thread::spawn(move || {
        let _ = std::io::copy(&mut a, &mut b2);
    });
    let t2 = std::thread::spawn(move || {
        let _ = std::io::copy(&mut b, &mut a2);
    });
    let _ = t1.join();
    let _ = t2.join();
    Ok(())
}

fn split_host_port(authority: &str, default_port: u16) -> (String, u16) {
    let authority = authority.trim();
    if let Some(i) = authority.rfind(':') {
        if let Ok(p) = authority[i + 1..].parse::<u16>() {
            return (authority[..i].trim_matches(['[', ']']).to_string(), p);
        }
    }
    (authority.trim_matches(['[', ']']).to_string(), default_port)
}
