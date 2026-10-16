//! 插件市场：从 GitHub/Gitee 仓库加载 market.json 索引并远程安装插件。
//! 仓库根目录放置 market.json（插件索引数组），每个插件一个子目录：
//!   market.json → [ { id, name, version, author, description, permissions,
//!                     size, screenshots[], homepage, path }, ... ]
//!   <path>/plugin.json + <path>/index.html

use serde_json::Value;
use std::fs;
use tauri::{AppHandle, Emitter, State};

use crate::AppState;

const DEFAULT_MARKET_URL: &str = "https://github.com/fyh386/plugin-market";
/// 单个插件文件大小上限（index.html 为单文件插件，正常几十 KB）
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

/// 把用户配置的市场地址解析成 raw 基地址（不含 market.json）。
/// 支持：owner/repo 简写、GitHub/Gitee 完整仓库 URL（可带 /tree/分支）、
/// 直接指向 market.json 的 raw 链接（原样返回，由调用方判断）
fn resolve_market(url: &str) -> Result<String, String> {
    let u = url.trim().trim_end_matches('/');
    if u.is_empty() {
        return Err("市场地址为空，请先在设置里配置".into());
    }
    if u.ends_with(".json") {
        return Ok(u.to_string());
    }
    let had_scheme = u.contains("://");
    let rest = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://")).unwrap_or(u);
    let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let body: &[&str] = if had_scheme { &segs[1..] } else { &segs[..] };
    let is_github = !had_scheme || u.contains("github.com");
    match body {
        [owner, repo] if !owner.is_empty() && !repo.is_empty() => {
            let (tpl, branch) = if is_github {
                ("https://raw.githubusercontent.com/{o}/{r}/{b}", "main")
            } else {
                ("https://gitee.com/{o}/{r}/raw/{b}", "master")
            };
            Ok(tpl.replace("{o}", owner).replace("{r}", repo).replace("{b}", branch))
        }
        [owner, repo, "tree", branch] if !owner.is_empty() && !repo.is_empty() && !branch.is_empty() => {
            let (tpl, _) = if is_github {
                ("https://raw.githubusercontent.com/{o}/{r}/{b}", "")
            } else {
                ("https://gitee.com/{o}/{r}/raw/{b}", "")
            };
            Ok(tpl.replace("{o}", owner).replace("{r}", repo).replace("{b}", branch))
        }
        _ => Err(format!("无法识别的市场地址: {url}（支持 owner/repo 或仓库 URL）")),
    }
}

fn market_url_of(state: &State<'_, AppState>) -> String {
    let u = state.config.lock().unwrap().settings.general.market_url.trim().to_string();
    if u.is_empty() { DEFAULT_MARKET_URL.into() } else { u }
}

fn fetch_index(url: &str) -> Result<Value, String> {
    let base = resolve_market(url)?;
    let index_url = if base.ends_with(".json") { base } else { format!("{base}/market.json") };
    let client = crate::http_client(std::time::Duration::from_secs(12))?;
    let resp = client.get(&index_url).send().map_err(|e| format!("拉取市场索引失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("市场索引请求返回 {}（地址: {index_url}）", resp.status()));
    }
    let text = resp.text().map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("market.json 不是合法 JSON: {e}"))?;
    // 兼容两种格式：裸数组或 { plugins: [...] }
    let list = match &v {
        Value::Array(_) => v,
        Value::Object(_) => v.get("plugins").cloned().unwrap_or(Value::Array(vec![])),
        _ => return Err("market.json 格式不正确（应为插件数组或 {plugins:[...]}）".into()),
    };
    Ok(list)
}

/// 拉取市场索引（market.json）。返回插件数组。
#[tauri::command]
pub fn market_index(state: State<'_, AppState>) -> Result<Value, String> {
    let url = market_url_of(&state);
    fetch_index(&url)
}

/// 从市场安装（或更新）插件：按索引里的 path 下载 plugin.json + index.html，
/// 校验后落入用户插件目录（同名内置插件 = 覆盖层更新），随后 dock 重载生效。
#[tauri::command]
pub fn market_install(
    id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let url = market_url_of(&state);
    let mut base = resolve_market(&url)?;
    if base.ends_with(".json") {
        // 裸 market.json 直链：取其所在目录作为插件根，
        // 即 market.json 与插件目录同层即可安装（本地/静态站点场景）
        match base.rfind('/') {
            Some(i) if i > "https://".len() => base.truncate(i),
            _ => return Err("无法从 market.json 链接推导插件目录".into()),
        }
    }
    let list = fetch_index(&url)?;
    let entry = list
        .as_array()
        .and_then(|a| a.iter().find(|e| e.get("id").and_then(|v| v.as_str()) == Some(id.as_str())))
        .cloned()
        .ok_or_else(|| format!("市场索引中不存在插件 {id}"))?;
    let path = entry.get("path").and_then(|v| v.as_str()).unwrap_or(id.as_str());
    if path.contains("..") || path.starts_with('/') || path.contains('\\') {
        return Err("插件路径不合法".into());
    }

    let client = crate::http_client(std::time::Duration::from_secs(20))?;
    let dir = base.trim_end_matches('/');
    let mf_url = format!("{dir}/{path}/plugin.json");
    let html_url = format!("{dir}/{path}/index.html");
    let mf = client.get(&mf_url).send().map_err(|e| format!("下载 plugin.json 失败: {e}"))?;
    if !mf.status().is_success() {
        return Err(format!("plugin.json 下载返回 {}（{mf_url}）", mf.status()));
    }
    let mf_text = mf.text().map_err(|e| e.to_string())?;
    let html = client.get(&html_url).send().map_err(|e| format!("下载 index.html 失败: {e}"))?;
    if !html.status().is_success() {
        return Err(format!("index.html 下载返回 {}（{html_url}）", html.status()));
    }
    let html_bytes = html.bytes().map_err(|e| e.to_string())?;
    if html_bytes.len() > MAX_FILE_BYTES {
        return Err("插件文件超过 2MB 上限".into());
    }
    // manifest 必须能解析且 id 与索引一致，防止装错包
    let m: crate::plugins::PluginManifest =
        serde_json::from_str(&mf_text).map_err(|e| format!("market 插件 plugin.json 解析失败: {e}"))?;
    if m.id != id {
        return Err(format!("插件包 id（{}）与索引不符（{id}）", m.id));
    }

    // 先落到临时目录，复用注册表的校验+安装流程
    let tmp = std::env::temp_dir().join(format!("tbp-market-{}-{}", std::process::id(), id));
    if tmp.exists() {
        fs::remove_dir_all(&tmp).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let r = (|| -> Result<String, String> {
        fs::write(tmp.join("plugin.json"), &mf_text).map_err(|e| e.to_string())?;
        fs::write(tmp.join("index.html"), &html_bytes).map_err(|e| e.to_string())?;
        let mut reg = state.registry.lock().unwrap();
        let data_dir = state.config.lock().unwrap().data_dir.clone();
        reg.upsert_from_dir(&tmp, &data_dir)
    })();
    let _ = fs::remove_dir_all(&tmp);
    let installed_id = r?;

    crate::flog("[market] ", &format!("installed {installed_id} from {url}"));
    let _ = app.emit_to("dock", "plugins-changed", ());
    Ok(installed_id)
}

#[cfg(test)]
mod tests {
    use super::resolve_market;

    #[test]
    fn resolve_shorthand_and_urls() {
        // owner/repo 简写 → github raw main
        assert_eq!(
            resolve_market("taskbarplus/plugin-market").unwrap(),
            "https://raw.githubusercontent.com/taskbarplus/plugin-market/main"
        );
        // 完整 GitHub URL
        assert_eq!(
            resolve_market("https://github.com/a/b").unwrap(),
            "https://raw.githubusercontent.com/a/b/main"
        );
        // 指定分支
        assert_eq!(
            resolve_market("https://github.com/a/b/tree/dev").unwrap(),
            "https://raw.githubusercontent.com/a/b/dev"
        );
        // Gitee 默认 master
        assert_eq!(
            resolve_market("https://gitee.com/a/b").unwrap(),
            "https://gitee.com/a/b/raw/master"
        );
        // 裸 market.json 直链原样返回
        assert_eq!(
            resolve_market("https://x.com/market.json").unwrap(),
            "https://x.com/market.json"
        );
        // 非法地址
        assert!(resolve_market("not a repo url").is_err());
        assert!(resolve_market("").is_err());
    }
}
