//! Hosts 切换器：读取/写入 %windir%\System32\drivers\etc\hosts
//! 写入需要管理员权限：先尝试直写，失败后 PowerShell -Verb RunAs 提权（弹 UAC），
//! 成功后再 ipconfig /flushdns。
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn hosts_path() -> PathBuf {
    if let Ok(w) = std::env::var("windir") {
        PathBuf::from(w).join("System32/drivers/etc/hosts")
    } else {
        PathBuf::from(r"C:\Windows\System32\drivers\etc\hosts")
    }
}

pub fn read() -> Result<Value, String> {
    let p = hosts_path();
    match std::fs::read_to_string(&p) {
        Ok(content) => Ok(json!({ "path": p.display().to_string(), "content": content })),
        Err(e) => Err(format!("读取 hosts 失败: {e}")),
    }
}

fn flush_dns() -> Result<(), String> {
    let out = std::process::Command::new("ipconfig")
        .args(["/flushdns"])
        .output()
        .map_err(|e| format!("flushdns 启动失败: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

pub fn apply(content: &str) -> Result<Value, String> {
    let p = hosts_path();
    let path_str = p.display().to_string();

    // 1) 先尝试直写（某些环境当前用户已有权限）
    let direct = std::fs::write(&p, content);
    if direct.is_err() {
        // 2) PowerShell 提权写入：内容 base64 传参避免引号/编码问题
        let b64 = base64_encode(content.as_bytes());
        let script = format!(
            "$b=[Convert]::FromBase64String('{b64}');[IO.File]::WriteAllText('{}',[Text.Encoding]::UTF8.GetString($b))",
            path_str.replace('\'', "''")
        );
        // -EncodedCommand 需要 UTF-16LE 编码的 base64
        let wide: Vec<u8> = script
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        let enc = base64_encode(&wide);
        let st = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("Start-Process powershell -Verb RunAs -Wait -ArgumentList '-NoProfile','-EncodedCommand','{enc}'"),
            ])
            .status()
            .map_err(|e| format!("提权写入启动失败: {e}"))?;
        if !st.success() {
            return Err("提权写入被拒绝（UAC 取消或失败）".into());
        }
    }

    // 3) 回读校验
    let after = std::fs::read_to_string(&p).map_err(|e| format!("回读校验失败: {e}"))?;
    if after != content {
        return Err("写入后内容校验不一致（可能被杀软拦截）".into());
    }
    let _ = flush_dns();
    Ok(json!({ "ok": true, "path": path_str, "elevated": direct.is_err() }))
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
    fn base64_roundtrip_simple() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode("中文内容".as_bytes()), "5Lit5paH5YaF5a65");
    }
}
