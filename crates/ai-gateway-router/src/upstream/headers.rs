//! 上游请求头构造（common / chat / billing / refresh 四类）。
//!
//! 对应 Go 源文件 `internal/upstream/headers.go`。
//!
//! # 两条硬约束
//!
//! 1. **Origin/Referer 必须跟随账号区域**：上游按 Origin 判定来源区域，
//!    跨区域发送会被拒或落到错误的服务，因此不能恒为国服。
//! 2. **绝不在 chat 请求里携带 `X-Refresh-Token`**：该头只允许出现在 refresh 端点。
//!    安全红线，勿动。

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::RequestBuilder;

use crate::auth::Auth;

/// 客户端 UA。所有请求共用（模型配置接口另有专用 UA）。
const CLIENT_UA: &str = "CLI/2.63.2 CodeBuddy/2.63.2";
/// 国服 Origin/Referer。
const ORIGIN_CN: &str = "https://www.codebuddy.cn";
/// 国际版 Origin/Referer。
const ORIGIN_INTL: &str = "https://www.workbuddy.ai";

/// 该账号所属区域对应的 Origin/Referer。
pub fn origin_referer_for(a: &Auth) -> &'static str {
    if a.is_intl() {
        ORIGIN_INTL
    } else {
        ORIGIN_CN
    }
}

/// 所有 API 共享的请求头。
fn common(headers: &mut HeaderMap, a: &Auth) {
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    headers.insert(
        "Accept",
        HeaderValue::from_static("application/json, text/plain, */*"),
    );
    headers.insert("X-Requested-With", HeaderValue::from_static("XMLHttpRequest"));
    let origin = origin_referer_for(a);
    if let Ok(v) = HeaderValue::from_str(origin) {
        headers.insert("Origin", v.clone());
        headers.insert("Referer", v);
    }
    headers.insert("User-Agent", HeaderValue::from_static(CLIENT_UA));
}

/// 插入 header，忽略非法值（凭证里的 uid 等理论上可能含非 ASCII）。
fn put(headers: &mut HeaderMap, key: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.insert(key, v);
    }
}

/// chat 专属头：在 common 之上加账号头。
///
/// 缺省字段用 `X-No-*` 约定（与 CodeBuddy 官方 CLI 一致）—— 上游据此区分
/// 「字段缺失」与「字段为空」，直接省略会被判成协议错误。
pub fn chat_headers(a: &Auth) -> HeaderMap {
    let mut h = HeaderMap::new();
    common(&mut h, a);
    if a.access_token.is_empty() {
        h.insert("X-No-Authorization", HeaderValue::from_static("1"));
    } else {
        put(&mut h, "Authorization", &format!("Bearer {}", a.access_token));
    }
    if a.uid.is_empty() {
        h.insert("X-No-User-Id", HeaderValue::from_static("1"));
    } else {
        put(&mut h, "X-User-Id", &a.uid);
    }
    if a.enterprise_id.is_empty() {
        h.insert("X-No-Enterprise-Id", HeaderValue::from_static("1"));
    } else {
        put(&mut h, "X-Enterprise-Id", &a.enterprise_id);
    }
    // 安全红线：绝不在 chat 请求里携带 X-Refresh-Token。
    if a.domain.is_empty() {
        h.insert("X-No-Department-Info", HeaderValue::from_static("1"));
    } else {
        put(&mut h, "X-Domain", &a.domain);
    }
    h.insert("X-Product", HeaderValue::from_static("SaaS"));
    h
}

/// billing 接口请求头。
pub fn billing_headers(a: &Auth) -> HeaderMap {
    let mut h = HeaderMap::new();
    put(&mut h, "Authorization", &format!("Bearer {}", a.access_token));
    h.insert("Accept", HeaderValue::from_static("application/json"));
    h.insert("Content-Type", HeaderValue::from_static("application/json"));
    if !a.uid.is_empty() {
        put(&mut h, "X-User-Id", &a.uid);
    }
    if !a.enterprise_id.is_empty() {
        put(&mut h, "X-Enterprise-Id", &a.enterprise_id);
        put(&mut h, "X-Tenant-Id", &a.enterprise_id);
    }
    if !a.domain.is_empty() {
        put(&mut h, "X-Domain", &a.domain);
    }
    h
}

/// refresh 端点专属头（`X-Refresh-Token` 只允许出现在这里）。
pub fn refresh_headers(a: &Auth) -> HeaderMap {
    let mut h = HeaderMap::new();
    common(&mut h, a);
    put(&mut h, "X-Refresh-Token", &a.refresh_token);
    if !a.enterprise_id.is_empty() {
        put(&mut h, "X-Enterprise-Id", &a.enterprise_id);
    }
    h.insert("X-Auth-Refresh-Source", HeaderValue::from_static("workbuddy"));
    h
}

/// 把 header map 应用到请求构造器。
pub fn apply(req: RequestBuilder, headers: HeaderMap) -> RequestBuilder {
    req.headers(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cn() -> Auth {
        Auth {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            uid: "u1".into(),
            enterprise_id: "e1".into(),
            domain: "https://copilot.tencent.com".into(),
            ..Default::default()
        }
    }

    fn intl() -> Auth {
        Auth {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            uid: "u2".into(),
            domain: "https://www.workbuddy.ai".into(),
            ..Default::default()
        }
    }

    #[test]
    fn origin_follows_region() {
        assert_eq!(origin_referer_for(&cn()), ORIGIN_CN);
        assert_eq!(origin_referer_for(&intl()), ORIGIN_INTL);
    }

    #[test]
    fn chat_headers_include_account_fields() {
        let h = chat_headers(&cn());
        assert_eq!(h["authorization"], "Bearer at");
        assert_eq!(h["x-user-id"], "u1");
        assert_eq!(h["x-enterprise-id"], "e1");
        assert_eq!(h["x-domain"], "https://copilot.tencent.com");
        assert_eq!(h["x-product"], "SaaS");
        assert_eq!(h["origin"], ORIGIN_CN);
        assert_eq!(h["user-agent"], CLIENT_UA);
    }

    /// 安全红线：chat 请求绝不能带 X-Refresh-Token。
    #[test]
    fn chat_headers_never_carry_refresh_token() {
        let h = chat_headers(&cn());
        assert!(
            h.get("x-refresh-token").is_none(),
            "chat 请求不得携带 refresh token"
        );
    }

    /// 缺省字段用 X-No-* 约定，而不是直接省略。
    #[test]
    fn chat_headers_use_x_no_convention() {
        let a = Auth {
            access_token: String::new(),
            uid: String::new(),
            enterprise_id: String::new(),
            domain: String::new(),
            ..Default::default()
        };
        let h = chat_headers(&a);
        assert_eq!(h["x-no-authorization"], "1");
        assert_eq!(h["x-no-user-id"], "1");
        assert_eq!(h["x-no-enterprise-id"], "1");
        assert_eq!(h["x-no-department-info"], "1");
        assert!(h.get("authorization").is_none());
    }

    #[test]
    fn refresh_headers_carry_refresh_token() {
        let h = refresh_headers(&cn());
        assert_eq!(h["x-refresh-token"], "rt");
        assert_eq!(h["x-auth-refresh-source"], "workbuddy");
    }

    #[test]
    fn billing_headers_use_tenant_id() {
        let h = billing_headers(&cn());
        assert_eq!(h["x-tenant-id"], "e1");
        assert_eq!(h["x-user-id"], "u1");
    }
}
