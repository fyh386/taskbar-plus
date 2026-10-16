//! 配置存储：全局 settings.json + 每个插件的用户配置
//! 路径: %APPDATA%/com.taskbarplus.app/settings.json

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginUserConfig {
    /// 用户注入的自定义 CSS（仅作用于该插件 iframe）
    #[serde(default)]
    pub custom_css: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub width: Option<u32>,
    /// 用户覆盖的配置（与插件默认配置合并后提供给插件）
    #[serde(default)]
    pub config: Value,
}

fn default_true() -> bool {
    true
}

/// 全新安装（无 settings.json）时的默认启用名单：只开系统监控 + 快捷输入，
/// 其余插件默认关闭，用户按需自行打开
pub fn default_enabled_for(id: &str) -> bool {
    matches!(id, "builtin.monitor" | "builtin.quick-input")
}

impl Default for PluginUserConfig {
    fn default() -> Self {
        Self { custom_css: String::new(), enabled: true, width: None, config: json!({}) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralSettings {
    /// 停靠位置: right | left | center
    pub position: String,
    /// 插件区总长度上限(px)，超出后按住拖拽平移查看
    pub max_width: u32,
    /// 插件间距 px
    pub spacing: u32,
    /// 背景不透明度 %
    pub opacity: u32,
    /// dock 条背景颜色（#rrggbb），空串 = 默认深色
    pub bg_color: String,
    /// dock 条背景颜色不透明度 %（0 = 完全透明）
    pub bg_opacity: u32,
    /// dock 条背景图片（data URL，空串 = 无）
    pub bg_image: String,
    pub autostart: bool,
    pub hide_on_blur: bool,
    /// 呼出快捷输入的热键
    pub hotkey_input: String,
    /// 允许未签名插件
    pub allow_unsigned: bool,
    /// 全局自定义 CSS（同时作用于宿主文档与所有插件 iframe，空串 = 无）
    #[serde(default)]
    pub custom_css: String,
    /// 预设皮肤：default | glass | light | contrast | prism | nebula | gradient | fluid
    #[serde(default)]
    pub theme: String,
    /// 插件市场索引地址：GitHub 仓库（owner/repo 或完整 URL）、Gitee 仓库，
    /// 或直接指向 market.json 的 raw 链接。仓库根目录需放置 market.json 索引
    #[serde(default)]
    pub market_url: String,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            position: "right".into(),
            max_width: 1280,
            spacing: 8,
            opacity: 100,
            bg_color: String::new(),
            bg_opacity: 92,
            bg_image: String::new(),
            autostart: true,
            hide_on_blur: true,
            hotkey_input: "Control+`".into(),
            allow_unsigned: false,
            custom_css: String::new(),
            theme: "default".into(),
            market_url: "https://github.com/fyh386/plugin-market".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub general: GeneralSettings,
    /// 插件用户配置，key 为插件 id
    pub plugins: std::collections::HashMap<String, PluginUserConfig>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { general: GeneralSettings::default(), plugins: Default::default() }
    }
}

pub struct ConfigStore {
    pub settings: Settings,
    pub data_dir: PathBuf,
    /// 本次启动是否全新安装（settings.json 原本不存在）。
    /// 全新安装按 default_enabled_for 决定插件默认开关；老用户配置一律保持原样
    pub fresh_install: bool,
}

impl ConfigStore {
    pub fn load(data_dir: &std::path::Path) -> Self {
        fs::create_dir_all(data_dir.join("plugins")).ok();
        let file = data_dir.join("settings.json");
        let existed = file.exists();
        let settings = fs::read_to_string(&file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { settings, data_dir: data_dir.to_path_buf(), fresh_install: !existed }
    }

    pub fn save(&self) -> Result<(), String> {
        let file = self.data_dir.join("settings.json");
        let tmp = self.data_dir.join("settings.json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&self.settings).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        fs::rename(&tmp, &file).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 新建条目时按「全新安装默认名单 / 老用户默认开」取值，
    /// 避免「调宽度却顺手启用插件」之类的副作用
    fn entry_or_default(&mut self, id: &str) -> &mut PluginUserConfig {
        let on = if self.fresh_install { default_enabled_for(id) } else { true };
        self.settings.plugins.entry(id.to_string())
            .or_insert_with(|| PluginUserConfig { enabled: on, ..Default::default() })
    }

    pub fn plugin_entry(&self, id: &str) -> PluginUserConfig {
        self.settings.plugins.get(id).cloned().unwrap_or_else(|| {
            let mut e = PluginUserConfig::default();
            if self.fresh_install {
                e.enabled = default_enabled_for(id);
            }
            e
        })
    }

    pub fn set_plugin_enabled(&mut self, id: &str, enabled: bool) {
        self.entry_or_default(id).enabled = enabled;
        self.save().ok();
    }

    pub fn set_plugin_width(&mut self, id: &str, width: u32) {
        self.entry_or_default(id).width = Some(width);
        self.save().ok();
    }

    pub fn set_plugin_css(&mut self, id: &str, css: &str) {
        self.entry_or_default(id).custom_css = css.to_string();
        self.save().ok();
    }

    /// 合并插件默认配置与用户覆盖配置
    pub fn merged_plugin_config(&self, id: &str, defaults: &Value) -> Value {
        let user = self.plugin_entry(id).config;
        merge_json(defaults, &user)
    }

    pub fn set_plugin_config(&mut self, id: &str, config: Value) -> Result<(), String> {
        self.entry_or_default(id).config = config;
        self.save()
    }

    pub fn set_general(&mut self, patch: Value) -> Result<GeneralSettings, String> {
        let mut g = serde_json::to_value(&self.settings.general).map_err(|e| e.to_string())?;
        merge_json_mut(&mut g, &patch);
        self.settings.general = serde_json::from_value(g).map_err(|e| e.to_string())?;
        self.save()?;
        Ok(self.settings.general.clone())
    }
}

/// 浅合并：obj 中已有的 key 用 patch 覆盖，obj 的新 key 加入
pub fn merge_json(base: &Value, patch: &Value) -> Value {
    let mut out = base.clone();
    merge_json_mut(&mut out, patch);
    out
}

pub fn merge_json_mut(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                match b.get_mut(k) {
                    Some(bv) => merge_json_mut(bv, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tbp-test-config-{}-{}", tag, std::process::id()));
        if d.exists() {
            std::fs::remove_dir_all(&d).unwrap();
        }
        d
    }

    #[test]
    fn merge_json_deep_override() {
        let base = json!({ "a": 1, "nested": { "x": "d", "y": "keep" }, "arr": [1, 2] });
        let patch = json!({ "nested": { "x": "u" }, "new": true });
        let out = merge_json(&base, &patch);
        assert_eq!(out["a"], 1);
        assert_eq!(out["nested"]["x"], "u");
        assert_eq!(out["nested"]["y"], "keep");
        assert_eq!(out["arr"], json!([1, 2]));
        assert_eq!(out["new"], true);
        // 原值不被修改
        assert_eq!(base["nested"]["x"], "d");
    }

    #[test]
    fn merge_json_non_object_patch_replaces() {
        let base = json!({ "k": { "a": 1 } });
        let out = merge_json(&base, &json!("scalar"));
        assert_eq!(out, json!("scalar"));
    }

    #[test]
    fn load_missing_file_gives_defaults() {
        let d = tmp_dir("defaults");
        let store = ConfigStore::load(&d);
        assert_eq!(store.settings.general.position, "right");
        assert_eq!(store.settings.general.max_width, 1280);
        assert!(store.settings.general.autostart);
        assert!(d.join("plugins").is_dir(), "应自动创建 plugins 目录");
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let d = tmp_dir("roundtrip");
        {
            let mut store = ConfigStore::load(&d);
            store.set_plugin_enabled("com.x.a", false);
            store.set_plugin_width("com.x.a", 233);
            store.set_general(json!({ "spacing": 4 })).unwrap();
        }
        let store = ConfigStore::load(&d);
        assert_eq!(store.plugin_entry("com.x.a").enabled, false);
        assert_eq!(store.plugin_entry("com.x.a").width, Some(233));
        assert_eq!(store.settings.general.spacing, 4);
        // 未触碰的 general 字段保持默认
        assert_eq!(store.settings.general.max_width, 1280);
    }

    #[test]
    fn merged_plugin_config_prefers_user() {
        let d = tmp_dir("merge");
        let defaults = json!({ "repos": [], "mode": "rebase", "deep": { "a": 1, "b": 2 } });
        {
            let mut store = ConfigStore::load(&d);
            store.set_plugin_config("p", json!({ "mode": "merge", "deep": { "b": 9 } })).unwrap();
        }
        let store = ConfigStore::load(&d);
        let merged = store.merged_plugin_config("p", &defaults);
        assert_eq!(merged["mode"], "merge");
        assert_eq!(merged["deep"]["a"], 1);
        assert_eq!(merged["deep"]["b"], 9);
        assert_eq!(merged["repos"], json!([]));
    }

    #[test]
    fn general_patch_partial() {
        let d = tmp_dir("gpatch");
        let mut store = ConfigStore::load(&d);
        let g = store.set_general(json!({ "opacity": 60, "position": "left" })).unwrap();
        assert_eq!(g.opacity, 60);
        assert_eq!(g.position, "left");
        assert_eq!(g.max_width, 1280, "未 patch 的字段不变");
        // 非法字段应报错而不污染配置
        assert!(store.set_general(json!({ "max_width": "bad" })).is_err());
    }

    #[test]
    fn plugin_entry_default_is_enabled() {
        let d = tmp_dir("entry");
        let store = ConfigStore::load(&d);
        // 全新安装：只有系统监控 + 快捷输入默认启用，其余默认关闭
        assert!(store.fresh_install);
        assert!(store.plugin_entry("builtin.monitor").enabled);
        assert!(store.plugin_entry("builtin.quick-input").enabled);
        assert!(!store.plugin_entry("builtin.weather").enabled);
        assert!(!store.plugin_entry("anything").enabled);
        assert_eq!(store.plugin_entry("anything").width, None);
        // 手动设置宽度不应顺手启用插件（fresh 下保持各自默认）
        let mut s2 = store;
        s2.set_plugin_width("builtin.weather", 100);
        assert!(!s2.settings.plugins["builtin.weather"].enabled);
        s2.set_plugin_enabled("builtin.weather", true);
        assert!(s2.settings.plugins["builtin.weather"].enabled);
    }
}
