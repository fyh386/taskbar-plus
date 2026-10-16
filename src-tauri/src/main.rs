//! TaskBar Plus — 可插拔 Windows 任务栏插件工具
//! 核心架构：单 dock 窗口 + 沙箱 iframe 插件 + 统一 host API 网关（plugin_call）

// release 构建不弹控制台黑窗；debug 保留控制台便于开发
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod anchor;
mod clipboard;
mod config;
mod docker;
mod git;
mod hit;
mod hosts;
mod mail;
mod market;
mod plugins;
mod ports;
mod proxy;
mod stats;
mod translate;

use config::ConfigStore;
use plugins::PluginRegistry;
use serde_json::{json, Value};
use stats::StatsState;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

pub struct AppState {
    pub config: Mutex<ConfigStore>,
    pub registry: Mutex<PluginRegistry>,
    pub stats: StatsState,
    pub clipboard: clipboard::ClipboardState,
    /// 前端上报的 dock 内容宽度（px）
    pub dock_width: AtomicU32,
    /// 展开弹出层时额外的高度（px）
    pub dock_extra: AtomicU32,
    /// fs.save_dialog 选出的保存路径白名单：fs.write_base64 只允许写这些
    /// 一次性路径（选框授予 → 写入消费），防止 'fs' 权限插件写任意位置
    pub save_paths: Mutex<Vec<String>>,
}

impl AppState {
    pub fn new(data_dir: std::path::PathBuf) -> Self {
        std::fs::create_dir_all(&data_dir).ok();
        let config = ConfigStore::load(&data_dir);
        let registry = PluginRegistry::load(&data_dir);
        Self {
            config: Mutex::new(config),
            registry: Mutex::new(registry),
            stats: StatsState::new(),
            clipboard: clipboard::ClipboardState::new(),
            dock_width: AtomicU32::new(480),
            dock_extra: AtomicU32::new(0),
            save_paths: Mutex::new(Vec::new()),
        }
    }
}

fn emit_plugins_changed(app: &AppHandle) {
    let _ = app.emit_to("dock", "plugins-changed", ());
}

fn emit_settings_changed(app: &AppHandle, state: &State<'_, AppState>) {
    let s = json!(state.config.lock().unwrap().settings);
    let _ = app.emit_to("dock", "settings-changed", s);
}

// ==================== 命令：插件 ====================

#[tauri::command]
fn plugins_list(state: State<'_, AppState>) -> Value {
    let cfg = state.config.lock().unwrap();
    let reg = state.registry.lock().unwrap();
    json!(plugins::runtime_list(&reg, &cfg))
}

#[tauri::command]
fn plugin_source(id: String, state: State<'_, AppState>) -> Option<String> {
    let reg = state.registry.lock().unwrap();
    reg.find(&id).map(|(_, html, _)| html.clone())
}

#[tauri::command]
fn plugin_set_enabled(id: String, enabled: bool, app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    state.config.lock().unwrap().set_plugin_enabled(&id, enabled);
    emit_plugins_changed(&app);
    Ok(())
}

#[tauri::command]
fn plugin_set_width(id: String, width: u32, app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let reg = state.registry.lock().unwrap();
    let manifest = reg.find(&id).map(|(m, _, _)| m.clone()).ok_or("插件不存在")?;
    let w = width.clamp(manifest.width.min.min(20), manifest.width.max);
    drop(reg);
    state.config.lock().unwrap().set_plugin_width(&id, w);
    emit_plugins_changed(&app);
    Ok(())
}

#[tauri::command]
fn plugin_set_css(id: String, css: String, app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    state.config.lock().unwrap().set_plugin_css(&id, &css);
    // 只发 settings-changed：dock 原地重注样式即可，无需重建 iframe
    emit_settings_changed(&app, &state);
    Ok(())
}

#[tauri::command]
fn plugin_uninstall(id: String, app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    {
        let mut reg = state.registry.lock().unwrap();
        let data_dir = state.config.lock().unwrap().data_dir.clone();
        reg.uninstall(&id, &data_dir)?;
    }
    emit_plugins_changed(&app);
    Ok(())
}

#[tauri::command]
fn plugin_import_dir(src: String, app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    let id = {
        let mut reg = state.registry.lock().unwrap();
        let data_dir = state.config.lock().unwrap().data_dir.clone();
        reg.install_from_dir(std::path::Path::new(&src), &data_dir)?
    };
    emit_plugins_changed(&app);
    Ok(id)
}

/// 重新扫描插件目录（内置 + 用户目录，含同名覆盖层）并通知 dock 重载。
/// 插件安装/更新后的统一入口：改文件 → reload → 生效，全程无需重启程序。
#[tauri::command]
fn plugins_reload(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    {
        let mut reg = state.registry.lock().unwrap();
        let data_dir = state.config.lock().unwrap().data_dir.clone();
        *reg = plugins::PluginRegistry::load(&data_dir);
    }
    emit_plugins_changed(&app);
    Ok(())
}

/// 用户插件目录的内容签名：目录数 + 各 plugin.json/index.html 最大 mtime。
/// 用于后台轮询发现插件安装/更新/删除，触发 plugins_reload。
fn plugins_dir_signature(data_dir: &std::path::Path) -> (usize, i64) {
    let dir = data_dir.join("plugins");
    let mut count = 0usize;
    let mut newest = 0i64;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            count += 1;
            for f in ["plugin.json", "index.html"] {
                if let Ok(md) = std::fs::metadata(p.join(f)) {
                    if let Ok(t) = md.modified() {
                        if let Ok(ms) = t.duration_since(std::time::UNIX_EPOCH) {
                            newest = newest.max(ms.as_millis() as i64);
                        }
                    }
                }
            }
        }
    }
    (count, newest)
}

/// 插件 fs.* API 的沙箱路径解析（通用文件能力，插件无需主程序发版即可读写文件）：
/// - 只允许相对路径，禁止盘符 / 根 / .. 组件
/// - "exe/" 前缀解析到 exe 同级目录，其余解析到数据目录（%APPDATA%\com.taskbarplus.app）
/// - 目标必须位于对应根目录之内（canonicalize 后校验，防符号链接逃逸）
fn resolve_fs_path(state: &AppState, path: &str) -> Result<std::path::PathBuf, String> {
    use std::path::{Component, PathBuf};
    let rel = path.trim();
    if rel.is_empty() || rel.len() > 512 {
        return Err("非法路径".into());
    }
    let (root, sub) = if let Some(s) = rel.strip_prefix("exe/") {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        (exe.parent().map(|p| p.to_path_buf()).ok_or("无法定位 exe 目录")?, s.to_string())
    } else {
        (state.config.lock().unwrap().data_dir.clone(), rel.to_string())
    };
    let sub_path = PathBuf::from(&sub);
    for c in sub_path.components() {
        match c {
            Component::Normal(_) => {}
            _ => return Err("路径不允许包含 .. / 绝对路径 / 盘符".into()),
        }
    }
    let full = root.join(&sub_path);
    // canonicalize 在 Windows 返回 \\?\ 前缀路径，统一剥掉再比较；
    // 目标不存在时退化为未规范化形式（组件校验已禁止 .. / 绝对路径）
    let canon = |p: &std::path::Path| {
        let c = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let s = c.to_string_lossy().into_owned();
        std::path::PathBuf::from(s.strip_prefix("\\\\?\\").unwrap_or(&s))
    };
    let root_canon = canon(&root);
    let full_canon = canon(&full);
    if !full_canon.starts_with(&root_canon) {
        return Err("路径越出允许目录".into());
    }
    Ok(full)
}

#[tauri::command]
fn plugin_import_zip(zip_path: String, app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    let staging = state.config.lock().unwrap().data_dir.join("_staging");
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let file = std::fs::File::open(&zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("ZIP 读取失败: {e}"))?;
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = f.name().replace('\\', "/");
        if name.contains("..") || name.starts_with('/') {
            continue;
        }
        let rel = name.splitn(2, '/').nth(1).unwrap_or(name.as_str()).to_string();
        if rel.is_empty() {
            continue;
        }
        let out = staging.join(&rel);
        if f.is_dir() {
            std::fs::create_dir_all(&out).ok();
        } else {
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p).ok();
            }
            let mut buf = Vec::new();
            use std::io::Read;
            f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            std::fs::write(&out, &buf).map_err(|e| e.to_string())?;
        }
    }
    let id = {
        let mut reg = state.registry.lock().unwrap();
        let data_dir = state.config.lock().unwrap().data_dir.clone();
        let r = reg.install_from_dir(&staging, &data_dir);
        std::fs::remove_dir_all(&staging).ok();
        r?
    };
    emit_plugins_changed(&app);
    Ok(id)
}

#[tauri::command]
fn plugin_import_dir_dialog(app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    let dir = rfd::FileDialog::new()
        .set_title("选择插件文件夹（包含 plugin.json）")
        .pick_folder()
        .ok_or("已取消")?;
    plugin_import_dir(dir.to_string_lossy().into_owned(), app, state)
}

#[tauri::command]
fn plugin_import_zip_dialog(app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    let file = rfd::FileDialog::new()
        .set_title("选择插件包 (.zip)")
        .add_filter("插件包", &["zip"])
        .pick_file()
        .ok_or("已取消")?;
    plugin_import_zip(file.to_string_lossy().into_owned(), app, state)
}

// ==================== 命令：设置 ====================

#[tauri::command]
fn settings_get(state: State<'_, AppState>) -> Value {
    json!(state.config.lock().unwrap().settings)
}

#[tauri::command]
fn settings_set(
    patch: Value,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Value, String> {
    let autostart = patch.get("autostart").and_then(|v| v.as_bool());
    let g = state.config.lock().unwrap().set_general(patch)?;
    if let Some(on) = autostart {
        set_autostart(on).map_err(|e| format!("设置写入失败: {e}"))?;
    }
    emit_settings_changed(&app, &state);
    serde_json::to_value(&g).map_err(|e| e.to_string())
}

/// 开机自启：写注册表 Run 键
fn set_autostart(enable: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    if enable {
        let v = format!("\"{}\" --silent", exe.display());
        let out = git::silent_command("reg")
            .args(["add", key, "/v", "TaskBarPlus", "/t", "REG_SZ", "/d", &v, "/f"])
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
    } else {
        let out = git::silent_command("reg")
            .args(["delete", key, "/v", "TaskBarPlus", "/f"])
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() && !String::from_utf8_lossy(&out.stderr).contains("找不到") {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
    }
    Ok(())
}

#[tauri::command]
fn plugin_config_get(id: String, state: State<'_, AppState>) -> Result<Value, String> {
    let cfg = state.config.lock().unwrap();
    let reg = state.registry.lock().unwrap();
    let defaults = reg.find(&id).map(|(m, _, _)| m.config.clone()).unwrap_or(json!({}));
    Ok(cfg.merged_plugin_config(&id, &defaults))
}

#[tauri::command]
fn plugin_config_set(id: String, cfg_val: Value, app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    state.config.lock().unwrap().set_plugin_config(&id, cfg_val)?;
    emit_plugins_changed(&app);
    Ok(())
}

#[tauri::command]
fn get_data_dir(state: State<'_, AppState>) -> String {
    state.config.lock().unwrap().data_dir.display().to_string()
}

// ==================== 命令：dock / 窗口 ====================

#[tauri::command]
fn dock_report_size(
    width: u32,
    extra_height: u32,
    hit_rects: Option<Vec<[i64; 4]>>,
    app: AppHandle,
    state: State<'_, AppState>,
) {
    state.dock_width.store(width.max(60), Ordering::Relaxed);
    state.dock_extra.store(extra_height.min(1000), Ordering::Relaxed);
    // 可点击区域上报：安装命中子类并更新穿透白名单。
    // None/空 = 整个窗口可点击（未展开/右键未开），不做穿透，保证行为安全。
    if let Some(win) = app.get_webview_window("dock") {
        if let Ok(hwnd) = win.hwnd() {
            let hwnd = hwnd.0 as windows_sys::Win32::Foundation::HWND;
            hit::install(hwnd);
            let scale = hit::scale_for(hwnd);
            let rects: Vec<[i32; 4]> = hit_rects
                .as_ref()
                .map(|r| {
                    r.iter()
                        .map(|a| [a[0] as i32, a[1] as i32, a[2] as i32, a[3] as i32])
                        .collect()
                })
                .unwrap_or_default();
            let active = !rects.is_empty();
            // WM_NCHITTEST 子类白名单：可点击区 = 条带 ∪ 弹层，其余穿透到后方程序。
            // 注意：曾实测 SetWindowRgn 会让 WebView2 的 DComp 合成错裁弹层，禁用。
            hit::set_regions(active.then(|| hit::HitRegions { scale, rects }));
        }
    }
    anchor::apply_anchor(&app, state.dock_width.load(Ordering::Relaxed));
}

/// 设置窗口「关窗/最小化即隐藏」：预声明窗口被 close 后是销毁而非隐藏，
/// 一旦销毁 `get_webview_window` 永远拿不到（"设置打不开"的根因），
/// 拦截关闭请求改为隐藏，状态保留、随开随有。
/// 最小化（Win+M、任务栏按钮、SC_MINIMIZE）同样转为隐藏：
/// 最小化态的窗口 show 不会还原（停在 -32000 坐标，表现为"点了没反应"）。
pub fn settings_hide_on_close(win: &tauri::WebviewWindow) {
    let w = win.clone();
    win.on_window_event(move |e| {
        match e {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = w.hide();
            }
            tauri::WindowEvent::Resized(size) if size.width == 0 && size.height == 0 => {
                // winit 用 0×0 表示最小化
                let _ = w.hide();
            }
            _ => {}
        }
    });
}

fn show_settings(w: &tauri::WebviewWindow) {
    // 顺序很关键：对隐藏窗口先 unminimize 会落入 -32000 最小化态；
    // 先 show 再 restore 才能同时覆盖「隐藏」与「最小化」两种来源。
    let r_show = w.show().err().map(|e| e.to_string());
    let r_restore = w.unminimize().err().map(|e| e.to_string());
    let r_focus = w.set_focus().err().map(|e| e.to_string());
    flog("[settings] ", &format!("show={r_show:?} restore={r_restore:?} focus={r_focus:?}"));
}

#[tauri::command]
fn open_settings(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("settings") {
        // 防御：Tauri 管理器里可能残留引用而 OS 窗口已被销毁（任务管理器
        // 单独结束窗口等），此时 show 静默失败、FindWindow 也找不到。
        let alive = w.hwnd()
            .map(|h| unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(h.0 as *mut core::ffi::c_void)
            } != 0)
            .unwrap_or(false);
        flog("[settings] ", &format!("existing window alive={alive}"));
        if alive {
            show_settings(&w);
            return Ok(());
        }
        // 清掉残留引用，否则同 label 重建会报「窗口已存在」
        let _ = w.destroy();
    }
    // 窗口已被销毁（旧版本关闭过/异常退出）：运行时按原配置重建。
    // index.html 的 bootstrap 按窗口 label 分流到设置页，label 必须保持 "settings"
    let win = tauri::WebviewWindowBuilder::new(&app, "settings", tauri::WebviewUrl::App("index.html".into()))
        .title("TaskBar Plus 设置")
        .inner_size(880.0, 580.0)
        .min_inner_size(720.0, 500.0)
        .resizable(true)
        .visible(false)
        .build()
        .map_err(|e| format!("创建设置窗口失败: {e}"))?;
    settings_hide_on_close(&win);
    show_settings(&win);
    flog("[settings] ", "window rebuilt");
    Ok(())
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("仅允许 http(s) 链接".into());
    }
    git::silent_command("cmd")
        .args(["/C", "start", "", &url])
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn js_log(msg: String) {
    println!("[js] {msg}");
    flog("[js] ", &msg);
}

/// Rust 侧写同一日志文件（诊断穿透/代理问题）
pub fn flog(tag: &str, msg: &str) {
    if let Ok(mut p) = std::env::var("APPDATA") {
        p.push_str(r"\com.taskbarplus.app\tbp.log");
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let line = format!("{} {tag}{msg}\n", local_now());
            let _ = f.write_all(line.as_bytes());
        }
    }
}

fn local_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_time(secs)
}

// 简单本地时间格式化（UTC，避免引入 chrono 依赖；日志仅用于排查）
fn format_unix_time(secs: u64) -> String {
    let mut days = secs / 86400;
    let rem = secs % 86400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let mut y: u64 = 1970;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let diy = if leap { 366 } else { 365 };
        if days < diy {
            break;
        }
        days -= diy;
        y += 1;
    }
    const MONTHS: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mut mo = 0usize;
    let mut mdays = MONTHS[mo] + if mo == 1 && leap { 1 } else { 0 };
    while days >= mdays {
        days -= mdays;
        mo += 1;
        mdays = MONTHS[mo] + if mo == 1 && leap { 1 } else { 0 };
    }
    format!("{y:04}-{:02}-{:02} {h:02}:{mi:02}:{s:02}", mo + 1, days + 1)
}

#[tauri::command]
fn debug_dom(window: tauri::WebviewWindow) -> Result<String, String> {
    let _ = window.eval(
        "window.__TAURI__.core.invoke('write_debug', { content: document.getElementById('dock').outerHTML })",
    );
    Ok("ok".into())
}

#[tauri::command]
fn write_debug(content: String) -> Result<(), String> {
    let p = std::env::temp_dir().join("tbp-dock-dom.html");
    std::fs::write(&p, content).map_err(|e| e.to_string())?;
    println!("[debug] dom written to {}", p.display());
    Ok(())
}

/// 插件 API：session.write —— 把会话历史保存为 exe 同级 ai-sessions/ 目录下的独立文件。
/// 文件名只允许 [A-Za-z0-9._-]，杜绝路径穿越；目录不存在时自动创建。
#[tauri::command]
fn session_write(file: String, content: String) -> Result<String, String> {
    if file.is_empty() || file.len() > 128 || !file.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_') {
        return Err("非法文件名（仅允许字母数字 . - _，最长 128 字符）".into());
    }
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("ai-sessions")))
        .ok_or_else(|| "无法定位 exe 目录".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(&file);
    std::fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

#[tauri::command]
fn debug_eval(window: tauri::WebviewWindow, js: String) -> Result<(), String> {
    window.eval(&js).map_err(|e| e.to_string())
}

// ==================== 插件 API 网关 ====================

/// 构建宿主 HTTP 客户端。抓包代理运行中时回环走本地代理（与浏览器等应用
/// 行为一致，宿主自身流量也进抓包列表，保活/盯盘/webdav 请求可见可查）。
/// TLS 信任依赖 rustls native-roots 读取系统根存储——抓包 MITM 根证书
/// 启动时已装入「当前用户」根存储，故 HTTPS 回环不会被拒。
pub(crate) fn http_client(timeout: std::time::Duration) -> Result<reqwest::blocking::Client, String> {
    let mut b = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .user_agent("taskbar-plus-host/1.0");
    let st = proxy::status();
    if st["running"].as_bool() == Some(true) {
        let port = st["port"].as_u64().unwrap_or(0);
        if port > 0 {
            crate::flog("[http] ", &format!("via capture proxy 127.0.0.1:{port}"));
            b = b.proxy(
                reqwest::Proxy::all(format!("http://127.0.0.1:{port}"))
                    .map_err(|e| e.to_string())?,
            );
        }
    }
    b.build().map_err(|e| e.to_string())
}

#[tauri::command]
async fn plugin_call(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    api: String,
    args: Value,
) -> Result<Value, String> {
    let manifest = {
        let reg = state.registry.lock().unwrap();
        reg.find(&id).map(|(m, _, _)| m.clone()).ok_or_else(|| format!("插件 {id} 不存在"))?
    };
    if let Some(err) = plugins::check_permission(&manifest, &api) {
        return Err(err);
    }

    match api.as_str() {
        "stats.subscribe" => {
            state.stats.subscribe(&app);
            Ok(Value::Null)
        }
        "stats.unsubscribe" => {
            state.stats.unsubscribe();
            Ok(Value::Null)
        }
        "stats.clean_memory" => {
            let n = tauri::async_runtime::spawn_blocking(stats::clean_memory)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({ "processes": n }))
        }
        "translate.text" => {
            let text = str_arg(&args, "text")?;
            let sl = args.get("sl").and_then(|v| v.as_str()).unwrap_or("auto").to_string();
            let tl = args.get("tl").and_then(|v| v.as_str()).unwrap_or("zh-CN").to_string();
            let provider = args.get("provider").and_then(|v| v.as_str()).unwrap_or("google").to_string();
            let r = tauri::async_runtime::spawn_blocking(move || translate::translate(&text, &sl, &tl, &provider))
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({ "ok": r.ok, "text": r.text, "error": r.error }))
        }
        "config.get" => {
            let cfg = state.config.lock().unwrap();
            Ok(cfg.merged_plugin_config(&id, &manifest.config))
        }
        "config.set" => {
            let c = args.get("config").cloned().unwrap_or(Value::Null);
            state.config.lock().unwrap().set_plugin_config(&id, c)?;
            Ok(Value::Null)
        }
        "git.status" => {
            let repo = str_arg(&args, "path")?;
            Ok(serde_json::to_value(git::status(&repo)).unwrap())
        }
        "git.branches" => {
            let repo = str_arg(&args, "path")?;
            Ok(serde_json::to_value(git::branches(&repo)).unwrap())
        }
        "git.switch" => {
            let repo = str_arg(&args, "path")?;
            let branch = str_arg(&args, "branch")?;
            Ok(serde_json::to_value(git::switch_branch(&repo, &branch)).unwrap())
        }
        "git.pull" => {
            let repo = str_arg(&args, "path")?;
            Ok(serde_json::to_value(git::pull(&repo)).unwrap())
        }
        "git.push" => {
            let repo = str_arg(&args, "path")?;
            Ok(serde_json::to_value(git::push(&repo)).unwrap())
        }
        "git.log" => {
            let repo = str_arg(&args, "path")?;
            let n = args.get("n").and_then(|v| v.as_u64()).unwrap_or(10) as u32;
            Ok(serde_json::to_value(git::log(&repo, n)).unwrap())
        }
        "git.conflicts" => {
            let repo = str_arg(&args, "path")?;
            Ok(serde_json::to_value(git::conflicts(&repo)).unwrap())
        }
        "git.outgoing" => {
            let repo = str_arg(&args, "path")?;
            let branch = str_arg(&args, "branch").unwrap_or_default();
            Ok(serde_json::to_value(git::outgoing(&repo, &branch)).unwrap())
        }
        "git.last_pull_stat" => {
            let repo = str_arg(&args, "path")?;
            Ok(serde_json::to_value(git::last_pull_stat(&repo)).unwrap())
        }
        "git.abort" => {
            let repo = str_arg(&args, "path")?;
            Ok(serde_json::to_value(git::abort_merge(&repo)).unwrap())
        }
        "git.continue" => {
            let repo = str_arg(&args, "path")?;
            Ok(serde_json::to_value(git::continue_merge(&repo)).unwrap())
        }
        "http.get" => {
            let url = str_arg(&args, "url")?;
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err("仅允许 http(s) 请求".into());
            }
            tauri::async_runtime::spawn_blocking(move || {
                let client = http_client(std::time::Duration::from_secs(10))?;
                let resp = client.get(&url).send().map_err(|e| e.to_string())?;
                let status = resp.status().as_u16();
                let body = resp.text().map_err(|e| e.to_string())?;
                let body = if body.len() > 512 * 1024 { body[..512 * 1024].to_string() } else { body };
                Ok(json!({ "status": status, "body": body }))
            })
            .await
            .map_err(|e| e.to_string())?
        }
        "http.post" => {
            let url = str_arg(&args, "url")?;
            let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let headers = args.get("headers").cloned().unwrap_or_else(|| json!({}));
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err("仅允许 http(s) 请求".into());
            }
            tauri::async_runtime::spawn_blocking(move || {
                let client = http_client(std::time::Duration::from_secs(30))?;
                let mut req = client.post(&url);
                if let Some(obj) = headers.as_object() {
                    for (k, v) in obj {
                        if let Some(s) = v.as_str() {
                            req = req.header(k, s);
                        }
                    }
                }
                let resp = req.body(body).send().map_err(|e| e.to_string())?;
                let status = resp.status().as_u16();
                let text = resp.text().map_err(|e| e.to_string())?;
                let text = if text.len() > 512 * 1024 { text[..512 * 1024].to_string() } else { text };
                Ok(json!({ "status": status, "body": text }))
            })
            .await
            .map_err(|e| e.to_string())?
        }
        "http.request" => {
            // 通用 HTTP 请求（类 Postman 插件 + WebDAV 等任意方法）：method/url/headers/body
            // - method 允许任意 HTTP 动词（PROPFIND/MKCOL/MOVE…），仅校验 token 字符
            // - body 为文本；bodyBase64 传二进制（上传文件），二者取其一
            // - contentType 可覆盖默认的 application/json（上传需按文件类型设置）
            // - maxBytes 调整响应上限（默认 512KB，最大 32MB）
            // - 响应非 UTF-8 时附 bodyBase64（二进制下载）
            let url = str_arg(&args, "url")?;
            let method = args.get("method").and_then(|v| v.as_str()).unwrap_or("GET").to_uppercase();
            if !method.chars().all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)) {
                return Err("非法 HTTP 方法".into());
            }
            let headers = args.get("headers").cloned().unwrap_or_else(|| json!({}));
            let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let body_b64 = args.get("bodyBase64").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let content_type = args.get("contentType").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let max_bytes = args.get("maxBytes").and_then(|v| v.as_u64()).unwrap_or(512 * 1024).clamp(1024, 32 * 1024 * 1024) as usize;
            // timeoutMs：总超时覆盖（默认 60s，保活探测等场景按需收紧，1s~120s）
            let timeout = std::time::Duration::from_millis(
                args.get("timeoutMs").and_then(|v| v.as_u64()).unwrap_or(60_000).clamp(1_000, 120_000),
            );
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err("仅允许 http(s) 请求".into());
            }
            tauri::async_runtime::spawn_blocking(move || {
                let client = http_client(timeout)?;
                let m = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
                let mut req = client.request(m, &url);
                if let Some(obj) = headers.as_object() {
                    for (k, v) in obj {
                        if let Some(s) = v.as_str() {
                            req = req.header(k, s);
                        }
                    }
                }
                let text_body = if !body.is_empty() && method != "GET" && method != "HEAD" { Some(body) } else { None };
                let has_body = !body_b64.is_empty() || text_body.is_some();
                if !body_b64.is_empty() {
                    let bytes = base64_decode(&body_b64)?;
                    req = req.body(bytes);
                } else if let Some(text_body) = text_body {
                    req = req.body(text_body);
                }
                if !content_type.is_empty() {
                    req = req.header("Content-Type", content_type);
                } else if has_body {
                    req = req.header("Content-Type", "application/json");
                }
                let resp = req.send().map_err(|e| e.to_string())?;
                let status = resp.status().as_u16();
                let resp_headers: serde_json::Map<String, Value> = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.to_string(), Value::String(v.to_str().unwrap_or("").into())))
                    .collect();
                let bytes = resp.bytes().map_err(|e| e.to_string())?;
                let bytes = if bytes.len() > max_bytes { &bytes[..max_bytes] } else { &bytes[..] };
                let mut out = json!({ "status": status, "headers": Value::Object(resp_headers) });
                match std::str::from_utf8(bytes) {
                    Ok(text) => out["body"] = Value::String(text.into()),
                    Err(_) => {
                        out["body"] = Value::String(String::new());
                        out["bodyBase64"] = Value::String(base64_encode(bytes));
                    }
                }
                Ok(out)
            })
            .await
            .map_err(|e| e.to_string())?
        }
        "clipboard.history" => {
            let n = args.get("n").and_then(|v| v.as_u64()).unwrap_or(30) as usize;
            Ok(json!(clipboard::history(&state.clipboard, n)))
        }
        "clipboard.set" => {
            let text = str_arg(&args, "text")?;
            clipboard::write_text(&text)?;
            Ok(Value::Null)
        }
        "clipboard.clear" => {
            clipboard::clear(&state.clipboard);
            Ok(Value::Null)
        }
        "mail.unread" => {
            // IMAP over TLS 查未读（QQ/163/Gmail 等授权码登录），同步短查询直接执行
            mail::unread(&args)
        }
        "shell.open" => {
            let url = str_arg(&args, "url")?;
            // http(s) 直接打开；快捷启动等场景允许 file:// 协议与本地绝对路径
            //（.exe / .lnk / 目录），走 start 由系统关联程序处理
            let is_web = url.starts_with("https://") || url.starts_with("http://");
            let is_file = url.starts_with("file:///");
            let is_local_path = std::path::Path::new(&url).is_absolute()
                && !url.starts_with("http")
                && (url.to_lowercase().ends_with(".exe")
                    || url.to_lowercase().ends_with(".lnk")
                    || std::path::Path::new(&url).is_dir());
            if !(is_web || is_file || is_local_path) {
                return Err("仅允许打开 http(s) 链接、file:// 或本地程序快捷方式".into());
            }
            git::silent_command("cmd")
                .args(["/C", "start", "", &url])
                .spawn()
                .map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        "proxy.start" => {
            let port = args.get("port").and_then(|v| v.as_u64()).unwrap_or(8899).clamp(1024, 65535) as u16;
            let data_dir = state.config.lock().unwrap().data_dir.clone();
            tauri::async_runtime::spawn_blocking(move || proxy::start(app, &data_dir, port))
                .await
                .map_err(|e| e.to_string())?
        }
        "proxy.stop" => proxy::stop(),
        "proxy.status" => Ok(proxy::status()),
        "net.ports" => Ok(ports::ports()),
        "net.kill" => {
            let pid = args.get("pid").and_then(|v| v.as_u64()).ok_or("缺少参数 pid")? as u32;
            ports::kill(pid)
        }
        "docker.list" => docker::list(),
        "docker.restart" => docker::restart(&str_arg(&args, "id")?),
        "docker.logs" => docker::logs(&str_arg(&args, "id")?),
        "hosts.read" => hosts::read(),
        "hosts.apply" => hosts::apply(&str_arg(&args, "content")?),
        "session.write" => {
            let file = str_arg(&args, "file")?;
            let content = str_arg(&args, "content")?;
            Ok(json!({ "path": session_write(file, content)? }))
        }
        "fs.read" => {
            let p = resolve_fs_path(&state, &str_arg(&args, "path")?)?;
            let content = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
            Ok(json!({ "content": content }))
        }
        "fs.write" => {
            let p = resolve_fs_path(&state, &str_arg(&args, "path")?)?;
            let content = str_arg(&args, "content")?;
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&p, content).map_err(|e| e.to_string())?;
            Ok(json!({ "path": p.display().to_string() }))
        }
        "fs.append" => {
            let p = resolve_fs_path(&state, &str_arg(&args, "path")?)?;
            let content = str_arg(&args, "content")?;
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p).map_err(|e| e.to_string())?;
            f.write_all(content.as_bytes()).map_err(|e| e.to_string())?;
            Ok(json!({ "path": p.display().to_string() }))
        }
        "fs.save_dialog" => {
            // 另存为：插件下载文件时由用户选择保存路径
            // （WebView2 的 blob 下载只会静默落到系统下载目录，无对话框）
            // bodyBase64 非空 = 选完直接写入；缺省 = 只选路径（下载完成后再
            // 由 fs.write_base64 写入——大文件先弹框再下载，避免干等）
            let default_name = str_arg(&args, "defaultName")?;
            let body_b64 = args.get("bodyBase64").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let picked = rfd::FileDialog::new()
                .set_title("保存文件")
                .set_file_name(&default_name)
                .save_file();
            match picked {
                Some(p) => {
                    let ps = p.display().to_string();
                    state.save_paths.lock().unwrap().push(ps.clone());
                    if !body_b64.is_empty() {
                        let bytes = base64_decode(&body_b64)?;
                        if let Some(parent) = p.parent() {
                            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                        }
                        std::fs::write(&p, &bytes).map_err(|e| e.to_string())?;
                        // 已直接写入：白名单中的该路径即刻消费掉
                        let mut list = state.save_paths.lock().unwrap();
                        if let Some(i) = list.iter().position(|x| x == &ps) { list.remove(i); }
                    }
                    Ok(json!({ "path": ps }))
                }
                None => Ok(json!({ "cancelled": true })),
            }
        }
        "fs.write_base64" => {
            // 配合 fs.save_dialog 的延迟写入：路径必须来自保存对话框（一次性消费）
            let p = str_arg(&args, "path")?;
            let bytes = base64_decode(&str_arg(&args, "bodyBase64")?)?;
            {
                let mut list = state.save_paths.lock().unwrap();
                match list.iter().position(|x| x == &p) {
                    Some(i) => { list.remove(i); }
                    None => return Err("路径未经过保存对话框授权".into()),
                }
            }
            let path = std::path::Path::new(&p);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(path, &bytes).map_err(|e| e.to_string())?;
            Ok(json!({ "path": p }))
        }
        _ => Err(format!("未知 API: {api}")),
    }
}

fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("缺少参数 {key}"))
}

// base64 编解码（http.request 二进制上传/下载用）
fn base64_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(input)
        .map_err(|e| format!("bodyBase64 解码失败: {e}"))
}

// ==================== 入口 ====================

fn main() {
    // panic 也写入日志文件（release 无控制台）
    std::panic::set_hook(Box::new(|info| {
        eprintln!("panic: {info}");
        if let Ok(mut p) = std::env::var("APPDATA") {
            p.push_str(r"\com.taskbarplus.app\tbp.log");
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
                let _ = f.write_all(format!("{} [panic] {info}\n", local_now()).as_bytes());
            }
        }
    }));

    // 状态必须在 Builder 前创建并 manage：配置窗口在 setup 之前就会加载页面并调用命令
    let data_dir = std::env::var("APPDATA")
        .map(|a| std::path::Path::new(&a).join("com.taskbarplus.app"))
        .unwrap_or_else(|_| std::env::temp_dir().join("taskbar-plus"));
    let state = AppState::new(data_dir);

    tauri::Builder::default()
        // 单实例：第二个进程直接退出，避免两个 dock 窗口叠加导致点击/显示异常
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {}))
        .manage(state)
        .setup(|app| {
            // 预声明的设置窗口同样关窗即隐藏（close 即销毁，销毁后无法再打开）
            let has_settings = app.get_webview_window("settings").is_some();
            flog("[settings] ", &format!("setup: predeclared exists={has_settings}"));
            if let Some(w) = app.get_webview_window("settings") {
                settings_hide_on_close(&w);
            }
            // 自启对账：历史上开关打开时注册表写入可能失败（配置 true 但 Run 键
            // 无值），每次启动按配置幂等重写一次，保证配置与实际状态一致
            {
                let st = app.state::<AppState>();
                let on = st.config.lock().unwrap().settings.general.autostart;
                match set_autostart(on) {
                    Ok(()) => flog("[boot] ", &format!("autostart synced on={on}")),
                    Err(e) => flog("[boot] ", &format!("autostart sync FAILED on={on}: {e}")),
                }
            }
            // 初始锚定 + 后台跟随任务栏变化
            let handle = app.handle().clone();
            anchor::apply_anchor(&handle, 480);
            anchor::start_anchor_loop(handle);
            // 剪贴板后台监视
            clipboard::start_watcher(app.handle().clone());
            // 翻译引擎预热：提前完成 DNS/TLS 握手，首次翻译 <0.5s
            translate::warmup();
            // 插件热重载：轮询用户插件目录签名，变化即重扫 + 通知 dock 重载。
            // 轮询比 ReadDirectoryChangesW 简单可靠（目录可能整树替换），2s 一次开销可忽略。
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<AppState>();
                let data_dir = state.config.lock().unwrap().data_dir.clone();
                let mut sig = plugins_dir_signature(&data_dir);
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    let new_sig = plugins_dir_signature(&data_dir);
                    if new_sig != sig {
                        sig = new_sig;
                        let state = handle.state::<AppState>();
                        {
                            let mut reg = state.registry.lock().unwrap();
                            let data_dir = state.config.lock().unwrap().data_dir.clone();
                            *reg = plugins::PluginRegistry::load(&data_dir);
                        }
                        emit_plugins_changed(&handle);
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            plugins_list,
            plugin_source,
            plugin_set_enabled,
            plugin_set_width,
            plugin_set_css,
            plugin_uninstall,
            plugin_import_dir,
            plugins_reload,
            plugin_import_zip,
            plugin_import_dir_dialog,
            plugin_import_zip_dialog,
            settings_get,
            settings_set,
            plugin_config_get,
            plugin_config_set,
            get_data_dir,
            dock_report_size,
            open_settings,
            open_url,
            quit_app,
            js_log,
            debug_dom,
            write_debug,
            debug_eval,
            session_write,
            plugin_call,
            market::market_index,
            market::market_install,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
