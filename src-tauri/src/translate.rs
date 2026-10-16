//! 多引擎翻译（全部免 API Key）：
//! - auto：mymemory 与 google **并发竞速**，取最快成功结果（google 国内常不可达，短超时快速失败）
//! - google：Google gtx 免费接口
//! - mymemory：MyMemory 免费接口（匿名每日约 5000 字符）
//! - translators：本机 Python `translators` 库桥接（37 引擎，需用户自行 pip install translators）
//!
//! 性能：共享 reqwest Client（连接复用）+ 启动预热（DNS/TLS 握手提前完成），
//! 热路径延迟 = 最快引擎的响应时间。

use serde_json::json;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 翻译结果缓存（重复/相似内容瞬时返回；容量满时整体清空，简单高效）
static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn cache_key(text: &str, from: &str, to: &str, provider: &str) -> String {
    format!("{provider}|{from}|{to}|{text}")
}

fn cache_get(k: &str) -> Option<String> {
    let c = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let v = c.lock().unwrap().get(k).cloned();
    if c.lock().unwrap().len() > 500 {
        c.lock().unwrap().clear(); // 简单粗暴的容量控制
    }
    v
}

fn cache_put(k: String, v: String) {
    let c = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut m = c.lock().unwrap();
    if m.len() > 500 {
        m.clear();
    }
    m.insert(k, v);
}

fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

pub struct TranslateResult {
    pub ok: bool,
    pub text: String,
    pub error: String,
}

fn ok(text: String) -> TranslateResult {
    TranslateResult { ok: true, text, error: String::new() }
}

fn err(e: impl Into<String>) -> TranslateResult {
    TranslateResult { ok: false, text: String::new(), error: e.into() }
}

fn client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(4)
            .build()
            .expect("http client")
    })
}

/// 启动预热：提前完成 DNS/TLS 握手，保证首次翻译也在 0.5s 内
pub fn warmup() {
    std::thread::spawn(|| {
        let _ = mymemory("hi", "en", "zh-CN", Duration::from_secs(5));
        let _ = google("hi", "auto", "zh-CN", Duration::from_secs(3));
    });
}

/// 统一入口（阻塞调用，请在 spawn_blocking 中执行）
pub fn translate(text: &str, from: &str, to: &str, provider: &str) -> TranslateResult {
    // 缓存命中：瞬时返回（<5ms）
    let key = cache_key(text, from, to, provider);
    if let Some(hit) = cache_get(&key) {
        return ok(hit);
    }
    let r = match provider {
        "mymemory" => mymemory(text, from, to, Duration::from_secs(10)),
        "translators" => python_translators(text, from, to),
        "google" => google(text, from, to, Duration::from_secs(10)),
        _ => race(text, from, to),
    };
    if r.ok {
        cache_put(key, r.text.clone());
    }
    r
}

/// 双引擎竞速：谁先成功用谁；总超时 3.5s（远快于串行回退）
fn race(text: &str, from: &str, to: &str) -> TranslateResult {
    let (tx, rx) = std::sync::mpsc::channel::<TranslateResult>();
    let tm = text.to_string();
    let fm = from.to_string();
    let tl = to.to_string();
    let tx2 = tx.clone();
    std::thread::spawn(move || {
        let _ = tx2.send(mymemory(&tm, &fm, &tl, Duration::from_secs(6)));
    });
    let tg = text.to_string();
    let fg = from.to_string();
    let tg2 = to.to_string();
    std::thread::spawn(move || {
        // google 国内多不可达：1.2s 短超时快速失败，不拖累竞速
        let _ = tx.send(google(&tg, &fg, &tg2, Duration::from_millis(1200)));
    });

    let deadline = Instant::now() + Duration::from_millis(3500);
    let mut errors = Vec::new();
    while Instant::now() < deadline {
        let remain = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remain.max(Duration::from_millis(50))) {
            Ok(r) => {
                if r.ok {
                    return r;
                }
                errors.push(r.error);
            }
            Err(_) => break,
        }
    }
    err(if errors.is_empty() {
        "所有引擎超时".to_string()
    } else {
        errors.join("；")
    })
}

fn google(text: &str, from: &str, to: &str, timeout: Duration) -> TranslateResult {
    let sl = if from == "auto" { "auto".to_string() } else { from.to_string() };
    let url = format!(
        "https://translate.googleapis.com/translate_a/single?client=gtx&sl={}&tl={}&dt=t&q={}",
        sl,
        to,
        urlencoding::encode(text)
    );
    let resp = match client().get(&url).timeout(timeout).send() {
        Ok(r) => r,
        Err(e) => return err(format!("google 网络错误: {e}")),
    };
    if !resp.status().is_success() {
        return err(format!("google HTTP {}", resp.status()));
    }
    let body = match resp.text() {
        Ok(b) => b,
        Err(e) => return err(e.to_string()),
    };
    let data: serde_json::Value = match serde_json::from_str(&body) {
        Ok(d) => d,
        Err(e) => return err(format!("google 响应解析失败: {e}")),
    };
    let out: String = data[0]
        .as_array()
        .map(|arr| arr.iter().filter_map(|x| x[0].as_str()).collect())
        .unwrap_or_default();
    if out.trim().is_empty() {
        return err("google 空结果");
    }
    ok(out.trim().to_string())
}

fn mymemory(text: &str, from: &str, to: &str, timeout: Duration) -> TranslateResult {
    if text.len() > 4000 {
        return err("MyMemory 匿名接口限制 4000 字符，请改用 google 或 translators");
    }
    // MyMemory 不支持 auto 源语言：按文本是否含中文判断（CJK → zh-CN，否则 en）
    let from = if from == "auto" {
        if has_cjk(text) {
            "zh-CN".to_string()
        } else {
            "en".to_string()
        }
    } else {
        from.to_string()
    };
    // 避免源=目标（如中文文本翻译成中文）触发接口报错
    if from.eq_ignore_ascii_case(to) {
        return err("源语言与目标语言相同");
    }
    let url = format!(
        "https://api.mymemory.translated.net/get?q={}&langpair={}|{}",
        urlencoding::encode(text),
        urlencoding::encode(&from),
        urlencoding::encode(to)
    );
    let resp = match client().get(&url).timeout(timeout).send() {
        Ok(r) => r,
        Err(e) => return err(format!("mymemory 网络错误: {e}")),
    };
    if !resp.status().is_success() {
        return err(format!("mymemory HTTP {}", resp.status()));
    }
    let body = match resp.text() {
        Ok(b) => b,
        Err(e) => return err(e.to_string()),
    };
    let data: serde_json::Value = match serde_json::from_str(&body) {
        Ok(d) => d,
        Err(e) => return err(format!("mymemory 响应解析失败: {e}")),
    };
    match data["responseData"]["translatedText"].as_str() {
        Some(t) => {
            let t = t.trim();
            // 过滤接口错误话术
            if t.is_empty()
                || t.starts_with("MYMEMORY WARNING")
                || t.starts_with("PLEASE SELECT")
                || t.starts_with("QUERY LENGTH")
            {
                let msg = data["responseStatus"]
                    .as_i64()
                    .map(|s| format!("MyMemory 状态 {s}（匿名额度可能已用完）"))
                    .unwrap_or_else(|| "MyMemory 无有效结果".into());
                err(msg)
            } else {
                ok(t.to_string())
            }
        }
        None => err("MyMemory 响应缺少译文"),
    }
}

/// 本机 Python translators 库桥：静默执行 python，base64 传参避免转义问题
fn python_translators(text: &str, from: &str, to: &str) -> TranslateResult {
    let payload = json!({ "q": text, "fr": from, "to": to });
    let b64 = base64_encode(payload.to_string().as_bytes());
    let script = r#"
import sys, json, base64
try:
    import translators as ts
except ImportError:
    print(json.dumps({"ok": False, "error": "未安装 translators 库，请先执行: pip install translators"}))
    sys.exit(0)
try:
    d = json.loads(base64.b64decode(sys.argv[1]).decode("utf-8"))
    kw = {} if d["fr"] == "auto" else {"from_language": d["fr"]}
    r = ts.translate_text(d["q"], translator="bing", to_language=d["to"], **kw)
    print(json.dumps({"ok": True, "text": r}))
except Exception as e:
    print(json.dumps({"ok": False, "error": str(e)}))
"#;
    for py in ["python", "py"] {
        let mut cmd = crate::git::silent_command(py);
        if py == "py" {
            cmd.arg("-3");
        }
        let out = cmd.arg("-c").arg(script).arg(&b64).output();
        let out = match out {
            Ok(o) => o,
            Err(_) => continue, // 尝试下一个解释器
        };
        if out.stdout.is_empty() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("No module named") {
                return err("未安装 translators 库，请先执行: pip install translators");
            }
            return err(format!("python 执行失败: {}", stderr.lines().next().unwrap_or("")));
        }
        let line = String::from_utf8_lossy(&out.stdout);
        let line = line.trim();
        let data: serde_json::Value = match serde_json::from_str(line) {
            Ok(d) => d,
            Err(e) => {
                return err(format!("python 输出解析失败: {e}: {}", &line[..line.len().min(200)]))
            }
        };
        if data["ok"].as_bool() == Some(true) {
            return ok(data["text"].as_str().unwrap_or("").trim().to_string());
        }
        return err(data["error"].as_str().unwrap_or("未知错误"));
    }
    err("未找到 python 解释器（或 translators 库），可改用 auto/mymemory 引擎")
}

fn base64_encode(data: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TBL[(n >> 18) as usize & 63] as char);
        out.push(TBL[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TBL[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TBL[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        let s = b"Hello, \xe4\xb8\x96\xe7\x95\x8c!";
        assert_eq!(base64_encode(s), "SGVsbG8sIOS4lueVjCE=");
    }

    /// 真实网络自测：中英互译（自动方向）+ 延迟断言。常规 `cargo test` 跳过，
    /// 显式运行：cargo test -- --ignored translate_live
    #[test]
    #[ignore]
    fn translate_live() {
        warmup();
        std::thread::sleep(Duration::from_millis(2000));

        // 与前端一致的自动方向逻辑
        let auto_dir = |text: &str, tl: &str| -> String {
            let cjk = text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
            if cjk && tl.starts_with("zh") {
                "en".into()
            } else if !cjk && tl == "en" {
                "zh-CN".into()
            } else {
                tl.into()
            }
        };

        let cases: &[&str] = &[
            "Hello world, this is a latency test.",
            "这是一段中文文本，用于测试自动翻译。",
            "TaskBar Plus is a pluggable taskbar tool.",
        ];
        // 冷路径（预热后首轮）：<1.5s
        for text in cases {
            let tl = auto_dir(text, "zh-CN");
            let t0 = Instant::now();
            let r = translate(text, "auto", &tl, "auto");
            let dt = t0.elapsed();
            println!("  cold [->{}] {:.3}s ok={} out={}", tl, dt.as_secs_f32(), r.ok, r.text);
            assert!(r.ok, "翻译失败: {}", r.error);
            assert_ne!(r.text.trim(), text.trim(), "译文不应等于原文");
            assert!(dt < Duration::from_millis(1500), "冷路径延迟超标: {:.3}s", dt.as_secs_f32());
        }
        // 热路径（用户体验指标）：5 条不同文本（避开缓存），平均 <0.5s，最大 <0.8s
        let hot_samples: Vec<String> = (0..5)
            .map(|i| format!("The quick brown fox jumps over the lazy dog, round {i}."))
            .collect();
        let mut sum = Duration::ZERO;
        let mut max_hot = Duration::ZERO;
        for (i, text) in hot_samples.iter().enumerate() {
            let tl = auto_dir(text, "zh-CN");
            let t0 = Instant::now();
            let r = translate(text, "auto", &tl, "auto");
            let dt = t0.elapsed();
            sum += dt;
            if dt > max_hot {
                max_hot = dt;
            }
            println!("  hot#{} [->{}] {:.3}s ok={} out={}", i, tl, dt.as_secs_f32(), r.ok, r.text);
            assert!(r.ok, "热路径翻译失败: {}", r.error);
        }
        let n = hot_samples.len() as u32;
        let avg = sum / n;
        println!("  热路径平均 {:.3}s / 最大 {:.3}s（要求平均 <0.5s）", avg.as_secs_f32(), max_hot.as_secs_f32());
        assert!(avg < Duration::from_millis(500), "热路径平均延迟超标: {:.3}s", avg.as_secs_f32());
        assert!(max_hot < Duration::from_millis(800), "热路径最大延迟超标: {:.3}s", max_hot.as_secs_f32());
    }
}
