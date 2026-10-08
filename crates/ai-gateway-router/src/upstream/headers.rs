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

/// billing 接口请求头（`billingJSON` 管道专用：签到 / 活跃上报 / 领奖 / 任务）。
///
/// ⚠ **不要**往里加 `Origin`/`Referer`/`User-Agent`：
///   · `tasks.rs` 的成长任务列表要求**不带 UA**（带上会被上游裁到 18 项，实测），
///   · `desktop.rs` 的活跃上报自带 `BILLING_GO_UA`，
///   · 它们各自的头族都是与 Go 侧逐字对齐过的，动一个就会静默改变上游看到的口径。
///
/// 积分查询（`user_resource_detail`）需要的是另一套头 —— 见 [`resource_headers`]，
/// 那个接口的 WAF 会拒绝「看起来像裸 API 客户端」的请求（2026-10-08 实测）。
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

/// 积分查询（`POST /v2/billing/meter/get-user-resource`）专用请求头。
///
/// # ★★ 2026-10-08 修复：国服账号「消耗积分」恒为 0
///
/// ## 现场
///
/// 业主报障：「workbuddy 国服账号错误的显示积分为 0」。截图里 **15 个国服账号全部 0**、
/// 12 个国际版账号正常（130 / 100 / 350 …）。
///
/// 网关日志给出直接证据 —— **19 个 uid、每个 202 次**同一句：
///
/// ```text
/// credit-refresh <uid>: upstream client (http 403):
///   {"code":10085,"msg":"请求不合法，如有疑问请联系客服","requestId":"…"}
/// ```
///
/// ## 定因（隔离实测，逐项二分）
///
/// 同一个 token、同一个 URL、同一个 body，只换请求头：
///
/// | 请求头组合 | 结果 |
/// | --- | --- |
/// | 裸 `Authorization`/`Accept`/`Content-Type`/`X-User-Id`/`X-Domain` | **403 code=10085** |
/// | 上面 + `X-Client-Platform: web` | **200** |
/// | 上面 + `Referer` | **200** |
/// | 上面 + `User-Agent` | **200** |
/// | 上面 + `Origin`（**单独加它无效**） | 403 |
///
/// ⇒ 计费后台的 WAF 拒绝「看起来像裸 API 客户端」的请求，只要带上**任意一个**
/// 浏览器/客户端特征头就放行。**不是 token 过期、不是区域错、不是并发限流**
///（同一 token 连打 12 次全 200；直连与走 `127.0.0.1:7890` 代理都是 200）。
///
/// ## 为什么宿主侧一直是对的，只有网关错
///
/// 宿主 `ai-gateway-core/src/modules/credits.rs` 的 `resource_auth_headers` 早就带了
/// `X-Client-Platform: web` + `Origin` + `Referer`（照 WorkBuddy 用户中心的 Axios
/// 拦截器写的）。而网关这条后台巡检走的是自己那套头，没带 —— 同一个接口、两套头表，
/// 于是「界面上点刷新能出数、网关后台巡检恒 403」，界面读的是网关的池快照 ⇒ 显示 0。
///
/// ## 为什么单独一个函数，而不是改 `billing_headers`
///
/// 后者被 `billingJSON` 管道共用（签到 / 活跃上报 / 领奖 / 任务列表），那几处的头族
/// 与 Go 侧逐字对齐，其中任务列表**必须不带 UA**（带上会被上游裁到 18 项）。
/// 往共用头里加 UA/Origin 会静默改掉它们的口径 —— 实测一次性踩红 7 个用例。
///
/// ⚠ `Origin`/`Referer` 必须跟随账号区域：国际版请求带 `codebuddy.cn` 的 Origin
/// 会被按「非本区来源」拒绝（宿主 `request_origin` 的注释记着同一条）。
pub fn resource_headers(a: &Auth) -> HeaderMap {
    let mut h = HeaderMap::new();
    put(&mut h, "Authorization", &format!("Bearer {}", a.access_token));
    // 与宿主 `resource_auth_headers` 同形（它照用户中心的 Axios 拦截器写的）。
    h.insert(
        "Accept",
        HeaderValue::from_static("application/json, text/plain, */*"),
    );
    h.insert("Content-Type", HeaderValue::from_static("application/json"));
    // 修复点 1：声明「这是 Web 用户中心发来的请求」（WAF 的放行特征之一）。
    h.insert("X-Client-Platform", HeaderValue::from_static("web"));
    // 修复点 2/3：Origin/Referer 跟随区域（与 `common` 同一个函数，不另写判断）。
    let origin = origin_referer_for(a);
    if let Ok(v) = HeaderValue::from_str(origin) {
        h.insert("Origin", v.clone());
        h.insert("Referer", v);
    }
    // 修复点 4：与 chat 路径同一个 UA。
    h.insert("User-Agent", HeaderValue::from_static(CLIENT_UA));
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

    /// ★★ 2026-10-08 回归：积分查询头必须带 WAF 放行特征，否则国服恒 403（code=10085），
    /// 表现为「国服账号消耗积分恒为 0」。
    ///
    /// 三个特征头**任一**即可通过（隔离实测），但这里逐个钉住 —— 少一个就退回 403，
    /// 而 403 的表现只是「积分显示 0」，不报错、不冷却账号，极易再次漏掉。
    #[test]
    fn resource_headers_carry_the_waf_bypass_markers() {
        let h = resource_headers(&cn());
        assert_eq!(
            h["x-client-platform"], "web",
            "缺 X-Client-Platform 会被 WAF 判 403 code=10085"
        );
        assert_eq!(h["user-agent"], CLIENT_UA, "缺 UA 会被 WAF 判 403 code=10085");
        assert_eq!(
            h["referer"], ORIGIN_CN,
            "缺 Referer 会被 WAF 判 403 code=10085"
        );
        assert_eq!(h["origin"], ORIGIN_CN);
        assert_eq!(
            h["accept"], "application/json, text/plain, */*",
            "与宿主 resource_auth_headers 同形"
        );
    }

    /// 国际版积分查询的 Origin/Referer 必须跟着区域走 —— 带 `codebuddy.cn` 会被按
    /// 「非本区来源」拒绝（宿主 `request_origin` 记着同一条）。
    #[test]
    fn resource_headers_follow_the_account_region() {
        let h = resource_headers(&intl());
        assert_eq!(h["origin"], ORIGIN_INTL);
        assert_eq!(h["referer"], ORIGIN_INTL);
        assert_eq!(h["x-client-platform"], "web", "特征头与区域无关");
    }

    /// 反向护栏：`billing_headers` 是**共用**头（签到 / 活跃上报 / 领奖 / 任务列表），
    /// 其中任务列表**必须不带 UA**（带上会被上游裁到 18 项，实测）。
    ///
    /// 2026-10-08 修积分问题时曾把 UA/Origin 加进这里，一次性踩红 7 个用例 ——
    /// 本测试把「不要往共用头里塞特征头」钉死。
    #[test]
    fn billing_headers_stay_free_of_feature_headers() {
        let h = billing_headers(&cn());
        assert!(
            h.get("user-agent").is_none(),
            "billing_headers 不得带 UA（任务列表口径要求无 UA）"
        );
        assert!(h.get("origin").is_none(), "billing_headers 不得带 Origin");
        assert!(h.get("referer").is_none(), "billing_headers 不得带 Referer");
        assert!(
            h.get("x-client-platform").is_none(),
            "billing_headers 不得带 X-Client-Platform（那是 tasks/desktop 各自的口径）"
        );
    }
}
