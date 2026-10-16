//! 插件注册表：内置插件（编译期嵌入）+ 用户插件（数据目录加载）
//! 插件格式：一个文件夹 = plugin.json + index.html（单文件插件，内联 CSS/JS）

use include_dir::{include_dir, Dir};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

static BUILTIN_PLUGINS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../plugins");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidthLimit {
    #[serde(default)]
    pub min: u32,
    #[serde(default = "default_max")]
    pub max: u32,
    #[serde(default = "default_width")]
    pub default: u32,
}

fn default_max() -> u32 {
    400
}
fn default_width() -> u32 {
    160
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    /// 需要的权限: network | shell | git | stats
    pub permissions: Vec<String>,
    /// 功能截图地址（http(s) 图片链接，市场卡片展示用）
    #[serde(default)]
    pub screenshots: Vec<String>,
    /// 插件主页 / 源码仓库地址
    #[serde(default)]
    pub homepage: String,
    /// 默认配置（用户配置合并在其上）
    pub config: Value,
    pub width: WidthLimit,
}

impl Default for PluginManifest {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: "未命名".into(),
            version: "0.0.0".into(),
            author: String::new(),
            description: String::new(),
            permissions: vec![],
            screenshots: vec![],
            homepage: String::new(),
            config: json!({}),
            width: WidthLimit { min: 40, max: 400, default: 160 },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginRuntime {
    #[serde(flatten)]
    pub manifest: PluginManifest,
    pub enabled: bool,
    pub resolved_width: u32,
    /// 用户是否手动调过宽度（entry.width 有值）。手动宽度优先于内容自适应：
    /// 条带内容再宽也不顶开卡片，只被裁剪；未调过则维持「内容多宽卡片多宽」
    pub width_customized: bool,
    /// 插件包大小（index.html 字节数）
    pub size: usize,
    pub builtin: bool,
    /// 用户为该插件注入的自定义 CSS（与全局 CSS 合并后注入 iframe）
    pub custom_css: String,
}

pub struct PluginRegistry {
    /// (manifest, html_source, builtin)
    pub plugins: Vec<(PluginManifest, String, bool)>,
}

impl PluginRegistry {
    pub fn load(data_dir: &std::path::Path) -> Self {
        let mut plugins = Vec::new();

        // 1. 内置插件（嵌入二进制）
        println!("[plugins] embedded dir entries: {}", BUILTIN_PLUGINS.files().count() + BUILTIN_PLUGINS.dirs().count());
        for dir in BUILTIN_PLUGINS.dirs() {
            // 注意：include_dir 子目录中 File 的路径是根相对的，get_file("x") 不可靠，直接遍历 files()
            let mut mf = None;
            let mut html = None;
            for f in dir.files() {
                match f.path().file_name().and_then(|s| s.to_str()) {
                    Some("plugin.json") => mf = f.contents_utf8(),
                    Some("index.html") => html = f.contents_utf8(),
                    _ => {}
                }
            }
            if let (Some(mf), Some(html)) = (mf, html) {
                match serde_json::from_str::<PluginManifest>(mf) {
                    Ok(mut m) => {
                        if m.id.is_empty() {
                            m.id = dir.path().file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        }
                        plugins.push((m, html.to_string(), true));
                    }
                    Err(e) => println!("[plugins] parse error in {}: {e}", dir.path().display()),
                }
            } else {
                println!("[plugins] missing files in {}", dir.path().display());
            }
        }

        // 2. 用户插件目录: <data_dir>/plugins/<id>/
        // 用户插件可覆盖同名内置插件：这是"更新插件"的官方路径——把新版插件目录
        // 放进用户插件目录即可，无需重新编译主程序；卸载覆盖层后回退到内置版本。
        let user_dir = data_dir.join("plugins");
        if let Ok(entries) = fs::read_dir(&user_dir) {
            for e in entries.flatten() {
                let p = e.path();
                if !p.is_dir() {
                    continue;
                }
                let mf = p.join("plugin.json");
                let html = p.join("index.html");
                if let (Ok(mfs), Ok(hs)) = (fs::read_to_string(&mf), fs::read_to_string(&html)) {
                    if let Ok(mut m) = serde_json::from_str::<PluginManifest>(&mfs) {
                        if m.id.is_empty() {
                            m.id = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        }
                        if let Some(pos) = plugins.iter().position(|(x, _, _)| x.id == m.id) {
                            plugins.remove(pos); // 覆盖内置（或旧版用户插件）
                        }
                        plugins.push((m, hs, false));
                    }
                }
            }
        }

        Self { plugins }
    }

    pub fn find(&self, id: &str) -> Option<&(PluginManifest, String, bool)> {
        self.plugins.iter().find(|(m, _, _)| m.id == id)
    }

    /// 安装用户插件（从目录复制）
    pub fn install_from_dir(&mut self, src: &std::path::Path, data_dir: &std::path::Path) -> Result<String, String> {
        let mf_src = src.join("plugin.json");
        let mfs = fs::read_to_string(&mf_src).map_err(|_| "目录中缺少 plugin.json".to_string())?;
        let m: PluginManifest = serde_json::from_str(&mfs).map_err(|e| format!("plugin.json 解析失败: {e}"))?;
        let id = if m.id.is_empty() {
            src.file_name().map(|s| s.to_string_lossy().into_owned()).ok_or("无法确定插件 id")?
        } else {
            m.id.clone()
        };
        if self.find(&id).is_some() {
            return Err(format!("插件 {id} 已存在"));
        }
        let dst = data_dir.join("plugins").join(&id);
        if dst.exists() {
            fs::remove_dir_all(&dst).map_err(|e| e.to_string())?;
        }
        copy_dir(src, &dst)?;
        *self = Self::load(data_dir);
        Ok(id)
    }

    /// 安装或更新（插件市场入口）：已存在的用户层先移除再装新；
    /// 与内置同名的市场插件会落成用户目录覆盖层——这是「更新内置插件」的官方路径
    pub fn upsert_from_dir(&mut self, src: &std::path::Path, data_dir: &std::path::Path) -> Result<String, String> {
        let mf_src = src.join("plugin.json");
        let mfs = fs::read_to_string(&mf_src).map_err(|_| "目录中缺少 plugin.json".to_string())?;
        let m: PluginManifest = serde_json::from_str(&mfs).map_err(|e| format!("plugin.json 解析失败: {e}"))?;
        let id = if m.id.is_empty() {
            src.file_name().map(|s| s.to_string_lossy().into_owned()).ok_or("无法确定插件 id")?
        } else {
            m.id.clone()
        };
        let dst = data_dir.join("plugins").join(&id);
        if dst.exists() {
            fs::remove_dir_all(&dst).map_err(|e| e.to_string())?;
        }
        copy_dir(src, &dst)?;
        *self = Self::load(data_dir);
        Ok(id)
    }

    /// 卸载用户插件（内置插件不可卸载，只能禁用）
    pub fn uninstall(&mut self, id: &str, data_dir: &std::path::Path) -> Result<(), String> {
        let (_, _, builtin) = self.find(id).ok_or("插件不存在")?.clone();
        if builtin {
            return Err("内置插件只能禁用，不能卸载".to_string());
        }
        let p = data_dir.join("plugins").join(id);
        if p.exists() {
            fs::remove_dir_all(&p).map_err(|e| e.to_string())?;
        }
        *self = Self::load(data_dir);
        Ok(())
    }
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for e in fs::read_dir(src).map_err(|e| e.to_string())?.flatten() {
        let from = e.path();
        let to = dst.join(e.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// 构造设置界面用的运行时列表
pub fn runtime_list(
    registry: &PluginRegistry,
    config: &crate::config::ConfigStore,
) -> Vec<PluginRuntime> {
    registry
        .plugins
        .iter()
        .map(|(m, html, builtin)| {
            let entry = config.plugin_entry(&m.id);
            PluginRuntime {
                resolved_width: entry.width.unwrap_or(m.width.default).clamp(m.width.min.min(20), m.width.max),
                width_customized: entry.width.is_some(),
                size: html.len(),
                enabled: entry.enabled,
                manifest: m.clone(),
                builtin: *builtin,
                custom_css: entry.custom_css.clone(),
            }
        })
        .collect()
}

/// 权限检查：返回错误信息或 None
pub fn check_permission(manifest: &PluginManifest, api: &str) -> Option<String> {
    let need = match api {
        a if a.starts_with("http.") => Some("network"),
        a if a.starts_with("shell.") => Some("shell"),
        a if a.starts_with("git.") => Some("git"),
        a if a.starts_with("clipboard.") => Some("clipboard"),
        a if a.starts_with("session.") => Some("session"),
        a if a.starts_with("fs.") => Some("fs"),
        a if a.starts_with("mail.") => Some("network"),
        a if a.starts_with("proxy.") => Some("proxy"),
        a if a.starts_with("hosts.") => Some("hosts"),
        a if a.starts_with("net.") => Some("net"),
        a if a.starts_with("docker.") => Some("docker"),
        _ => None, // stats.* / config.* / dock.* 对所有插件开放
    };
    if let Some(p) = need {
        if !manifest.permissions.iter().any(|x| x == p) {
            return Some(format!("插件未声明 \"{p}\" 权限（在 plugin.json permissions 中添加）"));
        }
    }
    None
}

#[allow(dead_code)]
pub fn user_plugin_dir(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("plugins")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tbp-test-plg-{}-{}", tag, std::process::id()));
        if d.exists() {
            std::fs::remove_dir_all(&d).unwrap();
        }
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_plugin(dir: &std::path::Path, manifest_extra: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("index.html"), "<html><body>ok</body></html>").unwrap();
        std::fs::write(dir.join("plugin.json"), manifest_extra).unwrap();
    }

    #[test]
    fn permission_matrix() {
        let m_net = PluginManifest { permissions: vec!["network".into()], ..Default::default() };
        let m_shell = PluginManifest { permissions: vec!["shell".into()], ..Default::default() };
        let m_git = PluginManifest { permissions: vec!["git".into()], ..Default::default() };
        let m_none = PluginManifest::default();

        // network 权限
        assert!(check_permission(&m_net, "http.get").is_none());
        assert!(check_permission(&m_none, "http.get").is_some());
        // shell 权限
        assert!(check_permission(&m_shell, "shell.open").is_none());
        assert!(check_permission(&m_none, "shell.open").is_some());
        // git 权限（前缀匹配整个组）
        assert!(check_permission(&m_git, "git.status").is_none());
        assert!(check_permission(&m_git, "git.pull").is_none());
        assert!(check_permission(&m_none, "git.push").is_some());
        // 无需权限的 API
        assert!(check_permission(&m_none, "stats.subscribe").is_none());
        assert!(check_permission(&m_none, "config.get").is_none());
        assert!(check_permission(&m_none, "config.set").is_none());
        assert!(check_permission(&m_none, "dock.expand").is_none());
        // clipboard 权限
        let m_cb = PluginManifest { permissions: vec!["clipboard".into()], ..Default::default() };
        assert!(check_permission(&m_cb, "clipboard.history").is_none());
        assert!(check_permission(&m_cb, "clipboard.set").is_none());
        assert!(check_permission(&m_none, "clipboard.history").is_some());
        // http.request 走 network 权限
        assert!(check_permission(&m_net, "http.request").is_none());
        assert!(check_permission(&m_none, "http.request").is_some());
    }

    #[test]
    fn manifest_defaults_and_parse() {
        let m: PluginManifest = serde_json::from_str("{}").unwrap();
        assert_eq!(m.width.default, 160);
        assert_eq!(m.width.max, 400);

        let m: PluginManifest = serde_json::from_str(
            r#"{"id":"a.b","permissions":["network"],"width":{"min":60,"max":300,"default":120}}"#,
        )
        .unwrap();
        assert_eq!(m.id, "a.b");
        assert_eq!(m.width.min, 60);
        assert_eq!(m.width.max, 300);
        // 缺省字段填充
        assert_eq!(m.name, "未命名");
        assert_eq!(m.version, "0.0.0");
        assert!(m.config.is_object());
    }

    #[test]
    fn builtin_plugins_all_load() {
        // 内置插件必须全部解析成功（数量 ≥ 4 且 id 非空）
        let reg = PluginRegistry::load(std::env::temp_dir().as_path());
        let builtins: Vec<_> = reg.plugins.iter().filter(|(_, _, b)| *b).collect();
        assert!(builtins.len() >= 4, "内置插件数量异常: {}", builtins.len());
        for (m, html, _) in &reg.plugins {
            assert!(!m.id.is_empty());
            assert!(html.contains("</html>"), "{} 的 index.html 不完整", m.id);
            assert!(m.width.min <= m.width.default && m.width.default <= m.width.max,
                "{} width 限制非法", m.id);
        }
    }

    #[test]
    fn registry_discovers_user_plugins() {
        let data = tmp_dir("discover");
        write_plugin(
            &data.join("plugins").join("com.test.hello"),
            r#"{"id":"com.test.hello","name":"Hello","permissions":[]}"#,
        );
        let reg = PluginRegistry::load(&data);
        let found = reg.find("com.test.hello");
        assert!(found.is_some());
        assert_eq!(found.unwrap().1, "<html><body>ok</body></html>");
        assert!(!found.unwrap().2, "用户插件不应标记为内置");
    }

    #[test]
    fn install_and_uninstall_user_plugin() {
        let data = tmp_dir("install");
        let src = tmp_dir("src");
        write_plugin(&src.join("my-plugin"),
            r#"{"id":"com.test.mine","name":"Mine","permissions":["network"]}"#);

        let mut reg = PluginRegistry::load(&data);
        let id = reg.install_from_dir(&src.join("my-plugin"), &data).unwrap();
        assert_eq!(id, "com.test.mine");
        assert!(data.join("plugins").join("com.test.mine").join("index.html").exists());

        // 重复安装应报错
        assert!(reg.install_from_dir(&src.join("my-plugin"), &data).is_err());

        // 卸载后消失
        reg.uninstall("com.test.mine", &data).unwrap();
        assert!(reg.find("com.test.mine").is_none());
        assert!(!data.join("plugins").join("com.test.mine").exists());
    }

    #[test]
    fn install_rejects_bad_manifest() {
        let data = tmp_dir("badmanifest");
        let src = tmp_dir("badsrc");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("plugin.json"), "not json {{{").unwrap();
        let mut reg = PluginRegistry::load(&data);
        assert!(reg.install_from_dir(&src, &data).is_err());
    }

    #[test]
    fn builtin_cannot_be_uninstalled() {
        let data = tmp_dir("builtinlock");
        let reg = PluginRegistry::load(&data);
        let id = reg.plugins.iter().find(|(_, _, b)| *b).map(|(m, _, _)| m.id.clone()).unwrap();
        let mut reg = reg;
        assert!(reg.uninstall(&id, &data).is_err());
    }

    #[test]
    fn runtime_list_resolves_width_and_enabled() {
        let data = tmp_dir("runtime");
        write_plugin(&data.join("plugins").join("w"),
            r#"{"id":"w","width":{"min":50,"max":200,"default":100}}"#);
        let reg = PluginRegistry::load(&data);

        let mut cfg = crate::config::ConfigStore::load(&data);
        cfg.set_plugin_width("w", 500); // 超过 max，应被 clamp
        cfg.set_plugin_enabled("w", false);

        let list = runtime_list(&reg, &cfg);
        let w = list.iter().find(|p| p.manifest.id == "w").unwrap();
        assert_eq!(w.resolved_width, 200);
        assert!(!w.enabled);
    }
}
