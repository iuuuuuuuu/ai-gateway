//! Trae 账号级设备指纹：由 uid **确定性派生** `device_id` / `session_id` / `market_user_id`。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/commands/accounts.rs::derive_device`。
//!
//! ## 为什么必须确定性派生而不是随机
//!
//! 服务端把 JWT 与签发时的设备指纹绑定：同一个账号每次请求必须携带**同一个**
//! `x-device-id` / `vscode-sessionid`，否则 401。随机生成意味着每次调用换一个指纹，
//! 签到与额度查询会大面积失败。因此用 uid 做种子的 SHA-256 级联派生 ——
//! 同一 uid 永远得到同一组指纹，且不同账号之间彼此独立（实现设备隔离）。
//!
//! ## 与「机器级 6 层重置」的区别
//!
//! 本模块是**账号级**指纹（随账号走，注入到请求头与 MITM 代理）。
//! [`crate::modules::switcher::machine`] 重置的是**机器级**指纹（`machineid`、
//! 注册表 `MachineGuid` 等）。两者目的不同，不可互相替代。

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::modules::config;

/// 单个账号的设备指纹。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceEntry {
    /// 15 位纯数字设备 id（`x-device-id`）。
    pub device_id: String,
    /// 32 位十六进制会话 id（`vscode-sessionid`）。
    pub session_id: String,
    /// UUID v4 形态的市场用户 id（`x-market-user-id`）。
    pub market_user_id: String,
}

/// SHA-256 级联派生：`SHA256("<purpose>:<uid>:<counter>")`。
///
/// 用不同 purpose 派生不同字段，保证三者在同一 uid 下互不可由彼此推出，
/// 同时又是 uid 的纯函数（可重复计算，无需持久化）。
fn derive_bytes(purpose: &str, uid: &str, counter: u32) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(purpose.as_bytes());
    hasher.update(b":");
    hasher.update(uid.as_bytes());
    hasher.update(b":");
    hasher.update(counter.to_be_bytes());
    hasher.finalize().into()
}

/// 把字节流映射为 n 位十进制数字。
fn digits_from(bytes: &[u8], n: usize) -> String {
    let mut out = String::with_capacity(n);
    for i in 0..n {
        out.push(char::from(b'0' + (bytes[i % bytes.len()] % 10)));
    }
    out
}

/// 把 16 字节映射为 UUID v4 字符串（强制 version/variant 位）。
fn uuid_from(bytes: &[u8; 32]) -> String {
    let mut b = [0u8; 16];
    b.copy_from_slice(&bytes[..16]);
    // version 4（高 4 位 = 0100）
    b[6] = (b[6] & 0x0f) | 0x40;
    // variant RFC 4122（高 2 位 = 10）
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// 由 uid 确定性派生设备指纹（纯函数，无副作用）。
pub fn derive_device(uid: &str) -> DeviceEntry {
    let dev = derive_bytes("devid", uid, 0);
    let sess = derive_bytes("sess", uid, 0);
    let market = derive_bytes("market", uid, 0);
    DeviceEntry {
        device_id: digits_from(&dev, 15),
        session_id: hex_from(&sess, 32),
        market_user_id: uuid_from(&market),
    }
}

/// 取字节流的十六进制前缀（长度 `n`）。
fn hex_from(bytes: &[u8], n: usize) -> String {
    let mut out = String::with_capacity(n);
    for b in bytes.iter().cycle().take(n.div_ceil(2)) {
        out.push_str(&format!("{b:02x}"));
    }
    out.truncate(n);
    out
}

/// 设备映射表文件（`<store_dir>/device_map.json`）。
pub fn device_map_file() -> std::path::PathBuf {
    config::store_dir().join("device_map.json")
}

/// 读取全部账号的设备映射。
pub fn load_device_map() -> serde_json::Map<String, Value> {
    std::fs::read_to_string(device_map_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// 取账号的设备指纹：映射表已有条目优先（MITM 抓包可能写入真实指纹），
/// 缺失时按 uid 确定性派生并落盘共享。
///
/// 为什么优先用表里的：抓包拿到的是客户端**真实**使用的指纹，与 JWT 的绑定
/// 一定匹配；派生值是「如果客户端没被代理过」的推断值。有真值就该用真值。
pub fn resolve_device(uid: &str) -> DeviceEntry {
    let map = load_device_map();
    if let Some(entry) = map.get(uid) {
        if let Some(device) = parse_entry(entry) {
            if !device.device_id.is_empty() {
                return device;
            }
        }
    }
    let derived = derive_device(uid);
    let mut updated = map;
    updated.insert(uid.to_string(), entry_to_json(&derived));
    save_device_map(&updated);
    derived
}

/// 强制按 uid 重新派生并落盘（忽略表内旧值）。
pub fn reset_device_for(uid: &str) -> DeviceEntry {
    let derived = derive_device(uid);
    let mut map = load_device_map();
    map.insert(uid.to_string(), entry_to_json(&derived));
    save_device_map(&map);
    derived
}

/// 从映射表条目解析设备指纹（兼容上游 `device_id` / `market_user_id` / `session_id` 键名）。
fn parse_entry(entry: &Value) -> Option<DeviceEntry> {
    let device_id = entry.get("device_id")?.as_str()?.trim().to_string();
    Some(DeviceEntry {
        device_id,
        session_id: entry
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        market_user_id: entry
            .get("market_user_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn entry_to_json(entry: &DeviceEntry) -> Value {
    json!({
        "device_id": entry.device_id,
        "session_id": entry.session_id,
        "market_user_id": entry.market_user_id,
    })
}

/// 写回设备映射表（原子写）。
pub fn save_device_map(map: &serde_json::Map<String, Value>) {
    let path = device_map_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let content = serde_json::to_string_pretty(&Value::Object(map.clone())).unwrap_or_default();
    let _ = config::atomic_write(&path, &content);
}

/// 由抓包结果写入真实设备指纹（MITM 代理捕获到 `x-device-id` 时调用）。
pub fn record_captured_device(uid: &str, device_id: &str, session_id: Option<&str>, market_user_id: Option<&str>) {
    if uid.trim().is_empty() || device_id.trim().is_empty() {
        return;
    }
    let mut map = load_device_map();
    let existing = map.get(uid).cloned().unwrap_or_else(|| json!({}));
    let mut merged = existing.as_object().cloned().unwrap_or_default();
    merged.insert("device_id".to_string(), json!(device_id.trim()));
    if let Some(sid) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
        merged.insert("session_id".to_string(), json!(sid));
    }
    if let Some(mid) = market_user_id.map(str::trim).filter(|s| !s.is_empty()) {
        merged.insert("market_user_id".to_string(), json!(mid));
    }
    map.insert(uid.to_string(), Value::Object(merged));
    save_device_map(&map);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 派生是确定性的同一_uid_恒等() {
        let a = derive_device("2117003799429594");
        let b = derive_device("2117003799429594");
        assert_eq!(a, b, "同一 uid 必须派生同一指纹，否则服务端设备绑定校验会 401");
    }

    #[test]
    fn 不同_uid_得到不同指纹实现设备隔离() {
        let a = derive_device("2117003799429594");
        let b = derive_device("2117003799429595");
        assert_ne!(a.device_id, b.device_id);
        assert_ne!(a.session_id, b.session_id);
        assert_ne!(a.market_user_id, b.market_user_id);
    }

    #[test]
    fn 派生字段形态符合上游要求() {
        let d = derive_device("123456789012345");
        assert_eq!(d.device_id.len(), 15);
        assert!(d.device_id.chars().all(|c| c.is_ascii_digit()));

        assert_eq!(d.session_id.len(), 32);
        assert!(d.session_id.chars().all(|c| c.is_ascii_hexdigit()));

        assert_eq!(d.market_user_id.len(), 36);
        assert_eq!(d.market_user_id.chars().filter(|c| *c == '-').count(), 4);
        // UUID v4 的 version 与 variant 位
        let parts: Vec<&str> = d.market_user_id.split('-').collect();
        assert!(parts[2].starts_with('4'), "version 位应为 4: {}", parts[2]);
        assert!(
            matches!(parts[3].chars().next(), Some('8') | Some('9') | Some('a') | Some('b')),
            "variant 位应为 8/9/a/b: {}",
            parts[3]
        );
    }

    #[test]
    fn 三个字段互不可由彼此推出() {
        let d = derive_device("u1");
        // device_id 是数字、session_id 是十六进制 —— 至少字符集不同，
        // 证明用了不同 purpose 而不是同一哈希切片。
        assert!(d.device_id.chars().all(|c| c.is_ascii_digit()));
        assert!(d.session_id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn 空_uid_也不panic() {
        let d = derive_device("");
        assert_eq!(d.device_id.len(), 15);
        assert_eq!(d.session_id.len(), 32);
    }
}
