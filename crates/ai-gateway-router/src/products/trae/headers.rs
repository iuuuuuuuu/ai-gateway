//! Trae 的请求头部表。
//!
//! 逐项移植自实测通过的探针
//! （`test-bin/trae-upstream-probe/probe.py` 的 build_headers，2026-10-02 真机跑通）
//! 与 JS 参考实现 `auth.js:1162-1205` 的 buildCommonHeaders / buildStreamHeaders。
//! 两张表**逐项一致**，本文件是它们的 Rust 版。
//!
//! # 为什么每个头都不能省
//!
//! Trae 的上游把这一串头当**客户端身份**用：少一个就可能被当作未知客户端，
//! 表现为 4017（设备/会话不被识别）或干脆不响应。参考实现里这些值全部来自
//! 真实客户端（`auth.js:1136-1152`），本模块只负责把它们装进 HeaderMap。
//!
//! # Authorization 与 X-Cloudide-Token 是**两个不同的形态**
//!
//! - `Authorization` 是 `Cloud-IDE-JWT <jwt>` **整串**（带前缀）；
//! - `X-Cloudide-Token` 是**裸** JWT（不带前缀）。
//!
//! 探针与参考实现都这么写，凭证里存的也是整串 —— 所以这里必须拆开，
//! 把整串塞进 X-Cloudide-Token 会被上游拒。
//!
//! # 不自铸设备标识
//!
//! 参考实现实测（`auth.js:1163-1167`）：自铸（未注册）的 device id 会被上游
//! **静默丢弃** —— TCP/TLS 连上但永不回包。所以这里的值全部来自
//! `super::device::DeviceProfile`（真实客户端指纹或宿主身份文件）。

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use super::device::DeviceProfile;
use super::realm::{self, Realm};

/// 客户端固定 App ID（真实客户端 main.js 里的值）。
pub const APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";

/// `x-app-version`：参考实现里是**常量** `'default'`，不是版本号。
pub const APP_VERSION: &str = "default";

pub const IDE_VERSION_TYPE: &str = "stable";
pub const TRAFFIC_TYPE: &str = "prod";

/// 构造一次请求头所需的全部输入。
pub struct HeaderInput<'a> {
    /// 凭证里的 token，`Cloud-IDE-JWT <jwt>` 整串或裸 JWT 都行。
    pub token: &'a str,
    /// 宿主账号库里的**裸** user_id（纯数字），进 `x-uid`。
    pub uid: &'a str,
    pub realm: Realm,
    /// `x-ide-version`（`device::ide_version` 的产物）。
    pub ide_version: &'a str,
    /// `x-ide-version-code`（`device::ide_version_code` 的产物）。
    pub ide_version_code: &'a str,
    pub device: &'a DeviceProfile,
    /// 流式请求：追加 Accept / X-Request-ID / X-Trae-Request-ID。
    pub stream: bool,
    /// 断点续传用；有值才发 `Last-Event-ID`。
    pub last_event_id: Option<&'a str>,
}

/// 把凭证里的 token 拆成 (Authorization 值, 裸 JWT)。
///
/// 与 `trae_account::normalize_jwt` 同口径：缺前缀就补上，绝不重复补。
pub fn split_token(token: &str) -> (String, String) {
    let t = token.trim();
    let bare = t
        .strip_prefix("Cloud-IDE-JWT ")
        .or_else(|| t.strip_prefix("Bearer "))
        .unwrap_or(t)
        .trim()
        .to_string();
    (format!("Cloud-IDE-JWT {bare}"), bare)
}

/// 32 位十六进制的 trace id（uuid v4 去横线）。
fn trace_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn put(map: &mut HeaderMap, name: &'static str, value: &str) {
    // 空值**不发**：上游对空字符串与「缺头」的处理未必一致，而参考实现里
    // 读不到的值就是空串，那时它也不会发出这个头（`put` 在 ZCode 侧同款）。
    if value.is_empty() {
        return;
    }
    let Ok(v) = HeaderValue::from_str(value) else {
        // 含非 ASCII 或控制字符：宁可少一个头，也不能让整个请求构造失败。
        eprintln!("⚠ Trae 请求头 {name} 的值非法（含不可编码字符），已跳过");
        return;
    };
    map.insert(HeaderName::from_static(name), v);
}

/// 构造完整请求头。
pub fn build(input: &HeaderInput<'_>) -> HeaderMap {
    let (authorization, bare) = split_token(input.token);
    let trace = trace_id();
    let span = &trace[..16];

    let mut h = HeaderMap::new();
    put(&mut h, "content-type", "application/json");
    put(&mut h, "authorization", &authorization);
    put(&mut h, "x-cloudide-token", &bare);
    put(&mut h, "x-app-id", APP_ID);
    put(&mut h, "x-app-version", APP_VERSION);
    put(&mut h, "x-ide-version-code", input.ide_version_code);
    put(&mut h, "x-app-version-code", input.ide_version_code);
    put(&mut h, "x-custom-trace-id", &trace);
    put(&mut h, "x-flow-traceparent", &format!("04-{trace}-{span}-01"));
    put(&mut h, "x-device-brand", &input.device.device_model);
    put(&mut h, "x-device-cpu", &input.device.cpu);
    put(&mut h, "x-device-id", &input.device.device_id);
    put(&mut h, "x-machine-id", &input.device.machine_id);
    put(&mut h, "x-os-version", &input.device.os_version);
    put(&mut h, "x-device-type", &input.device.os_name);
    put(&mut h, "x-ide-version", input.ide_version);
    put(&mut h, "x-ide-version-type", IDE_VERSION_TYPE);
    put(&mut h, "request-traffic-type", TRAFFIC_TYPE);
    put(&mut h, "x-uid", input.uid);

    if input.stream {
        let rid = trace_id();
        put(&mut h, "accept", "text/event-stream");
        put(&mut h, "x-request-id", &rid);
        put(&mut h, "x-trae-request-id", &rid);
    }
    if let Some(le) = input.last_event_id {
        put(&mut h, "last-event-id", le);
    }
    h
}

/// 该区域的 chat host（区域 + 可选 region_code，见 `realm::chat_host`）。
pub fn chat_host(realm: Realm, region_code: &str) -> String {
    realm::chat_host(realm, region_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::products::trae::device::DeviceProfile;

    fn profile() -> DeviceProfile {
        DeviceProfile {
            machine_id: "a".repeat(64),
            device_id: "4496320041074586".to_string(),
            device_model: "83DG".to_string(),
            cpu: "Intel".to_string(),
            os_name: "windows".to_string(),
            os_version: "Windows 11 Pro".to_string(),
            is_solo: true,
        }
    }

    fn input<'a>(p: &'a DeviceProfile, token: &'a str, stream: bool) -> HeaderInput<'a> {
        HeaderInput {
            token,
            uid: "2593120869167228",
            realm: Realm::Cn,
            ide_version: "0.1.38",
            ide_version_code: "20260716",
            device: p,
            stream,
            last_event_id: None,
        }
    }

    fn get(h: &HeaderMap, name: &str) -> String {
        h.get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn authorization_keeps_the_prefix_and_the_bare_token_does_not() {
        let p = profile();
        let h = build(&input(&p, "Cloud-IDE-JWT eyJ.x.y", false));
        assert_eq!(get(&h, "authorization"), "Cloud-IDE-JWT eyJ.x.y");
        assert_eq!(get(&h, "x-cloudide-token"), "eyJ.x.y");
    }

    #[test]
    fn a_bare_token_gets_the_prefix_added_once() {
        let p = profile();
        let h = build(&input(&p, "eyJ.x.y", false));
        assert_eq!(get(&h, "authorization"), "Cloud-IDE-JWT eyJ.x.y");
        assert_eq!(get(&h, "x-cloudide-token"), "eyJ.x.y");
        // 已带前缀的不会被补成两遍。
        let (a, b) = split_token("Cloud-IDE-JWT Cloud-IDE-JWT x");
        assert_eq!(a, "Cloud-IDE-JWT Cloud-IDE-JWT x");
        assert_eq!(b, "Cloud-IDE-JWT x");
    }

    #[test]
    fn the_common_table_carries_every_documented_header() {
        let p = profile();
        let h = build(&input(&p, "Cloud-IDE-JWT t", false));
        for name in [
            "content-type",
            "authorization",
            "x-cloudide-token",
            "x-app-id",
            "x-app-version",
            "x-ide-version-code",
            "x-app-version-code",
            "x-custom-trace-id",
            "x-flow-traceparent",
            "x-device-brand",
            "x-device-cpu",
            "x-device-id",
            "x-machine-id",
            "x-os-version",
            "x-device-type",
            "x-ide-version",
            "x-ide-version-type",
            "request-traffic-type",
            "x-uid",
        ] {
            assert!(!get(&h, name).is_empty(), "缺少请求头 {name}");
        }
        assert_eq!(get(&h, "x-app-version"), "default");
        assert_eq!(get(&h, "x-ide-version-type"), "stable");
        assert_eq!(get(&h, "request-traffic-type"), "prod");
        assert_eq!(get(&h, "x-uid"), "2593120869167228");
        assert_eq!(get(&h, "x-device-id"), "4496320041074586");
    }

    #[test]
    fn stream_requests_add_the_event_stream_headers() {
        let p = profile();
        let plain = build(&input(&p, "Cloud-IDE-JWT t", false));
        assert!(get(&plain, "accept").is_empty());
        assert!(get(&plain, "x-request-id").is_empty());

        let s = build(&input(&p, "Cloud-IDE-JWT t", true));
        assert_eq!(get(&s, "accept"), "text/event-stream");
        // 两个 ID 头**同值**（参考实现逐字）。
        assert_eq!(get(&s, "x-request-id"), get(&s, "x-trae-request-id"));
        assert_eq!(get(&s, "x-request-id").len(), 32);
    }

    #[test]
    fn last_event_id_is_only_sent_when_present() {
        let p = profile();
        let mut i = input(&p, "Cloud-IDE-JWT t", true);
        assert!(get(&build(&i), "last-event-id").is_empty());
        i.last_event_id = Some("42");
        assert_eq!(get(&build(&i), "last-event-id"), "42");
    }

    #[test]
    fn the_traceparent_has_the_documented_shape() {
        let p = profile();
        let h = build(&input(&p, "Cloud-IDE-JWT t", false));
        let tp = get(&h, "x-flow-traceparent");
        let parts: Vec<&str> = tp.split('-').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "04");
        assert_eq!(parts[1].len(), 32);
        assert_eq!(parts[2].len(), 16);
        assert_eq!(parts[3], "01");
        // span 是 trace 的前 16 位（参考实现同一份 uuid 的切片）。
        assert_eq!(parts[2], &parts[1][..16]);
        // 两个 trace 头必须一致。
        assert_eq!(get(&h, "x-custom-trace-id"), parts[1]);
    }

    #[test]
    fn empty_values_are_never_sent_as_blank_headers() {
        // ★ 故意不用 Default：全空是我们**手工**摆出的边界，不是可以随手构造的常态。
        let p = DeviceProfile {
            machine_id: String::new(),
            device_id: String::new(),
            device_model: String::new(),
            cpu: String::new(),
            os_name: String::new(),
            os_version: String::new(),
            is_solo: false,
        };
        let h = build(&input(&p, "Cloud-IDE-JWT t", false));
        // 设备指纹全空时这几个头**不发**（而不是发空串）。
        assert!(h.get("x-device-id").is_none());
        assert!(h.get("x-machine-id").is_none());
        // 但固定值照发。
        assert_eq!(get(&h, "x-app-id"), APP_ID);
    }
}
