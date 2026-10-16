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

/// 国服客户端 UA。
const CLIENT_UA_CN: &str = "CLI/2.63.2 CodeBuddy/2.63.2";
/// 国际版客户端 UA（官方 WorkBuddyAI.exe 5.6.2 抓包）。
const CLIENT_UA_INTL: &str = "WorkBuddy/5.6.2 WorkBuddy/5.6.2 CLI/2.147.0";
/// 国服 Origin/Referer。
const ORIGIN_CN: &str = "https://www.codebuddy.cn";
/// 国际版 Origin/Referer。
const ORIGIN_INTL: &str = "https://www.workbuddy.ai";

/// 该账号所属区域对应的客户端 UA。
///
/// # 为什么必须按区域分流（★ 2026-10-16 补回：Rust 重写时丢失）
///
/// 国际版上游**按 UA 判定来源客户端**。只发国服 CLI 的 UA 时，实测
/// （同一账号、同一模型、同一端点 `POST /v2/chat/completions`）：
///
/// ```text
/// 官方国际版客户端（WorkBuddyAI.exe 5.6.2） → HTTP 200，正常流式
/// 网关（国服 UA）                          → HTTP 403 code=11140 request illegal
/// ```
///
/// 即**账号与内容都没问题，是请求身份不被国际版接受**。更麻烦的是这个 403
/// 在客户端界面上还会被渲染成「API 密钥无效」，排查方向会被彻底带偏 ——
/// **UA 错了不报错，只会 403**。
///
/// ⚠ **国服不得跟着改**：国服实测正常，动它的请求身份没有收益，只会引入
/// 一类新的不确定性（Go 侧 `TestCNDoesNotUseWorkBuddyUA` 专门防这个「修过头」）。
///
/// 与 Go `clientUAFor`（`go-gateway/internal/upstream/headers.go`）同口径，
/// 回归证据见 `go-gateway/internal/upstream/intl_headers_test.go`。
pub fn client_ua_for(a: &Auth) -> &'static str {
    if a.is_intl() {
        CLIENT_UA_INTL
    } else {
        CLIENT_UA_CN
    }
}

/// 该账号所属区域对应的 Origin/Referer。
pub fn origin_referer_for(a: &Auth) -> &'static str {
    if a.is_intl() {
        ORIGIN_INTL
    } else {
        ORIGIN_CN
    }
}

/// 由会话键**确定性**派生会话 id（`X-Conversation-ID`）。
///
/// # 为什么要稳定派生，而不是每次新造一个
///
/// 官方客户端**整段会话复用同一个** `X-Conversation-ID`。若我们每次请求都新造
/// 一个随机 UUID，那在多轮对话里就是「每轮换一个会话」—— 上游看到的是一串
/// 互不相干的单轮请求，与官方客户端的行为不一致。
///
/// # 为什么必须有它（★ 2026-10-16 补回：Rust 重写时丢失）
///
/// 上游的**渠道校验**会看会话头族。请求里完全没有会话 id 时，它回：
///
/// ```text
/// HTTP 400 {"code":11-128,"msg":"conversation id missing",
///           "displayTips":"这次调用没通过渠道校验，可能不是官方客户端发出的"}
/// ```
///
/// 官方三套身份里，**VSCode / CLI 一直带**这套头，**只有桌面端一个都没带** ——
/// 而桌面端正是所有 WorkBuddy 账号默认的那套身份。
///
/// 无种子（一次性探针，如专家市场探测）时退回随机 UUID，与 VSCode/CLI 的
/// 历史行为一致。派生口径与参考实现的 `conversation_uuid` 相同：
/// `uuid5(NAMESPACE_URL, seed)` 且**大写字面量**。
pub fn conversation_uuid(seed: &str) -> String {
    let s = seed.trim();
    if s.is_empty() {
        return uuid::Uuid::new_v4().to_string().to_uppercase();
    }
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, s.as_bytes())
        .to_string()
        .to_uppercase()
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
    headers.insert("User-Agent", HeaderValue::from_static(client_ua_for(a)));
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
pub fn chat_headers(a: &Auth, conversation_seed: &str) -> HeaderMap {
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
    // 国际版额外声明**客户端身份**（对照官方 WorkBuddyAI.exe 5.6.2 抓包）。
    //
    // ★ 2026-10-16 补回：Rust 重写时这七个头整体丢失，国际版又回到
    //「请求不像官方客户端」的状态。缺了它们，上游仍可能把请求判成非官方
    // 客户端（渠道校验 400/11-128、风控 3012 都指向同一类判定）。
    //
    // ⚠ **只在国际版路径上发**：国服上游不认它们，多发等于改变国服的请求
    // 画像，而国服是主力通道、当前实测良好。Go 侧 `TestCNHasNoIntlOnlyHeaders`
    // 专门钉住这条 —— 「不知道国服要不要」时**不要**顺手也发一份。
    if a.is_intl() {
        put(&mut h, "X-IDE-Type", "WorkBuddy");
        put(&mut h, "X-IDE-Name", "WorkBuddy");
        put(&mut h, "X-IDE-Version", "5.6.2");
        put(&mut h, "X-Agent-Intent", "craft");
        put(&mut h, "X-Agent-Purpose", "conversation");
        put(&mut h, "X-Agent-Type", "main");
        put(&mut h, "x-codebuddy-request", "1");
    }
    // 会话头族：**三套身份都要带**（官方 VSCode/CLI 一直带，桌面端原先一个都没有）。
    //
    // ★ 2026-10-16 补回：Rust 重写时整体丢失 ⇒ 上游渠道校验回
    //   400 / code 11-128 `conversation id missing`。见 conversation_uuid 的说明。
    //
    // ⚠ 与「国际版专属身份头」不同，这一族**不按区域分流**：参考实现里三套
    //   身份、两个区域都发（差异只在桌面端的 UA/版本号），故这里也不分。
    let conv = conversation_uuid(conversation_seed);
    // Message-ID 与 Request-ID **同值**：参考实现里两者就是同一个 msg_id。
    let msg_id = uuid::Uuid::new_v4().simple().to_string();
    put(&mut h, "X-Conversation-ID", &conv);
    put(&mut h, "X-Conversation-Request-ID", &msg_id);
    put(&mut h, "X-Conversation-Message-ID", &msg_id);
    put(&mut h, "X-Request-ID", &msg_id);
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
    // 修复点 4：与 chat 路径同一个 UA —— 故同样按区域分流（不是恒为国服 UA）：
    // 国际版账号打的是 workbuddy.ai，发国服 CLI 的 UA 会被它判成非官方客户端。
    h.insert("User-Agent", HeaderValue::from_static(client_ua_for(a)));
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
        let h = chat_headers(&cn(), "sess-1");
        assert_eq!(h["authorization"], "Bearer at");
        assert_eq!(h["x-user-id"], "u1");
        assert_eq!(h["x-enterprise-id"], "e1");
        assert_eq!(h["x-domain"], "https://copilot.tencent.com");
        assert_eq!(h["x-product"], "SaaS");
        assert_eq!(h["origin"], ORIGIN_CN);
        assert_eq!(h["user-agent"], CLIENT_UA_CN);
    }

    // ---------------------------------------------------------------------
    // 国际版请求身份（★ 2026-10-16 补回 Rust 重写时丢失的一整块）
    //
    // 守的是所有者 2026-09-27 报的真实现场：「国际版不能使用」。
    // 逐条对应 Go 侧 `intl_headers_test.go` 的断言 —— 那边有、这边没有，
    // 正是这些头在重写时丢失却无人发现的原因。
    // ---------------------------------------------------------------------

    /// 国际版必须发 WorkBuddy 客户端 UA，而不是国服 CLI 的。
    ///
    /// 这是本次修复的**核心断言**：用国服 UA 打国际版会 403 11140，
    /// 而那个 403 在客户端界面上会被显示成「API 密钥无效」，把排查方向带偏。
    #[test]
    fn intl_uses_workbuddy_client_ua() {
        let h = chat_headers(&intl(), "sess-1");
        let ua = h["user-agent"].to_str().unwrap();
        assert!(
            ua.starts_with("WorkBuddy/"),
            "国际版 UA 必须以 WorkBuddy/ 开头（否则上游回 403 11140），实际 {ua:?}"
        );
        assert_ne!(ua, CLIENT_UA_CN, "国际版不得复用国服 CLI 的 UA——那正是本次缺陷");
    }

    /// 国服**不得**被改成国际版 UA（防「修过头」）。
    ///
    /// 国服当前实测正常，动它的请求身份没有收益，只会引入新的不确定性。
    #[test]
    fn cn_does_not_use_workbuddy_ua() {
        let h = chat_headers(&cn(), "sess-1");
        let ua = h["user-agent"].to_str().unwrap();
        assert_eq!(ua, CLIENT_UA_CN, "国服 UA 应保持不变");
        assert!(!ua.starts_with("WorkBuddy/"), "国服不得发 WorkBuddy 客户端 UA");
    }

    /// 国际版必须声明客户端身份头（抓包对照出来的差异项）。
    #[test]
    fn intl_sends_ide_and_agent_headers() {
        let h = chat_headers(&intl(), "sess-1");
        for (k, v) in [
            ("x-ide-type", "WorkBuddy"),
            ("x-ide-name", "WorkBuddy"),
            ("x-ide-version", "5.6.2"),
            ("x-agent-intent", "craft"),
            ("x-agent-purpose", "conversation"),
            ("x-agent-type", "main"),
            ("x-codebuddy-request", "1"),
        ] {
            assert_eq!(h[k], v, "国际版应发 {k}={v}");
        }
    }

    /// 国服路径**不该**带这些国际版专属头。
    ///
    /// 理由同 `cn_does_not_use_workbuddy_ua`：国服没要求它们，多发等于改变
    /// 国服的请求画像，而国服是主力通道。**不许**因为「顺手」也发一份。
    #[test]
    fn cn_has_no_intl_only_headers() {
        let h = chat_headers(&cn(), "sess-1");
        for k in [
            "x-ide-type",
            "x-ide-name",
            "x-ide-version",
            "x-agent-intent",
            "x-agent-purpose",
            "x-agent-type",
            "x-codebuddy-request",
        ] {
            assert!(h.get(k).is_none(), "国服不该发国际版专属头 {k}");
        }
    }

    /// 积分查询路径的 UA 也必须跟随区域（它与 chat 共用同一个分流函数）。
    #[test]
    fn resource_headers_ua_follows_region() {
        assert_eq!(resource_headers(&cn())["user-agent"], CLIENT_UA_CN);
        assert_eq!(resource_headers(&intl())["user-agent"], CLIENT_UA_INTL);
    }

    // ---------------------------------------------------------------------
    // 会话头族（★ 2026-10-16 补回：Rust 重写时整体丢失）
    //
    // 缺了它，上游渠道校验回 400 / code 11-128 `conversation id missing`，
    // 报文是「这次调用没通过渠道校验，可能不是官方客户端发出的」。
    // ---------------------------------------------------------------------

    /// 三套身份、两个区域**都必须**带全套会话头。
    ///
    /// 这是本组的主断言：官方 VSCode / CLI 一直带，桌面端原先一个都没有。
    #[test]
    fn conversation_headers_are_always_present() {
        for (name, h) in [("cn", chat_headers(&cn(), "sess-1")), ("intl", chat_headers(&intl(), "sess-1"))] {
            for k in [
                "x-conversation-id",
                "x-conversation-request-id",
                "x-conversation-message-id",
                "x-request-id",
            ] {
                assert!(
                    h.get(k).is_some_and(|v| !v.is_empty()),
                    "{name} 会话头族缺 {k} —— 上游会回 400/11-128 conversation id missing"
                );
            }
        }
    }

    /// 会话 id 必须由种子**确定性派生**：同一会话 → 同一个 id。
    ///
    /// 若每次新造随机 UUID，多轮对话在上游看来就是「每轮都是新会话」，
    /// 与官方客户端（整段会话复用同一个 X-Conversation-ID）的行为不一致。
    #[test]
    fn conversation_id_is_stable_for_one_session() {
        let a = chat_headers(&cn(), "sess-1")["x-conversation-id"].clone();
        let b = chat_headers(&cn(), "sess-1")["x-conversation-id"].clone();
        assert_eq!(a, b, "同一会话种子必须派生出同一个会话 id");

        let other = chat_headers(&cn(), "sess-2")["x-conversation-id"].clone();
        assert_ne!(a, other, "不同会话必须得到不同 id");
    }

    /// Message-ID 与 Request-ID 是**同一个值**（参考实现里就是同一个 msg_id）。
    #[test]
    fn message_id_equals_request_id() {
        let h = chat_headers(&cn(), "sess-1");
        assert_eq!(h["x-conversation-message-id"], h["x-request-id"]);
    }

    /// 无种子（一次性探针）退回随机 UUID —— 与官方 VSCode/CLI 的历史行为一致。
    #[test]
    fn empty_seed_falls_back_to_random_uuid() {
        let a = chat_headers(&cn(), "")["x-conversation-id"].clone();
        let b = chat_headers(&cn(), "")["x-conversation-id"].clone();
        assert_ne!(a, b, "无种子时每次应不同（随机 UUID）");
        assert_eq!(a.to_str().unwrap().len(), 36, "应为 UUID 字面量形态");
        assert_eq!(
            a.to_str().unwrap(),
            a.to_str().unwrap().to_uppercase(),
            "会话 id 应为大写字面量（与参考实现一致）"
        );
    }

    /// 派生结果就是 `uuid5(NAMESPACE_URL, seed)` —— 钉住算法，防止被换成别的哈希。
    #[test]
    fn conversation_uuid_matches_uuid5_namespace_url() {
        let want = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, b"seed-abc")
            .to_string()
            .to_uppercase();
        assert_eq!(conversation_uuid("seed-abc"), want);
        // 两侧空白应被忽略（会话键可能带空格）。
        assert_eq!(conversation_uuid("  seed-abc  "), want);
    }

    /// 安全红线：chat 请求绝不能带 X-Refresh-Token。
    #[test]
    fn chat_headers_never_carry_refresh_token() {
        let h = chat_headers(&cn(), "sess-1");
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
        let h = chat_headers(&a, "sess-1");
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
        assert_eq!(h["user-agent"], CLIENT_UA_CN, "缺 UA 会被 WAF 判 403 code=10085");
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
