//! Qoder 计费周期（`nextResetAt`）的端点调用与解析。
//!
//! 对应 Go `go-gateway/internal/qoder/userstatus.go`（371 行）。逐行照搬。
//!
//! # 这个文件解决什么
//!
//! 界面要能说清「本周期已用多少 / 还有多久重置」，而这两件事都需要**下次重置
//! 时刻**。我方现有的三个端点都拿不到它：
//!
//! ```text
//! /algo/api/v2/model/list     模型清单
//! /algo/api/v2/service/...    对话（SSE）
//! /api/v2/quota/usage         额度余量（`expiresAt` 是**套餐到期**，
//!                             不是周期重置；无到期套餐报 9999-12-31 哨兵）
//! ```
//!
//! 真正的来源是 `/algo/api/v3/user/status?Encode=1`，它**只认机器身份签名**
//!（不认 Bearer 会话），响应里顶层字段 `nextResetAt` 就是下次重置时刻
//!（**epoch 毫秒**）。
//!
//! # 认证方式与其它端点完全不同（照抄参考实现，不要「顺手统一」）
//!
//! 参考实现 `test-bin\recon\qoder-echoping07\auth\auth.go`：
//!
//! ```text
//! :43-48  CN.AuthBase = "https://gateway.qoder.com.cn"（与 QoderRegion::gateway() 一致）
//! :137-151 appcode = "cosy"；secret 默认 "d2FyLCB3YXIgbmV2ZXIgY2hhbmdlcw=="
//!          （= 明文 "war, war never changes"，可用 $env:QODER_SIGNATURE_SECRET 覆盖）
//! :157-163 Sign(date) = md5(appCode + "&" + secret + "&" + date)
//! :614-630 commonHeaders：appcode / date / signature 三个头是这条通道的钥匙
//! :632-672 postEncoded：请求体 = QoderEncode(json)，响应 = 直接解成 map
//! :725-736 userStatusInnerStruct：**字段顺序必须与官方客户端一致**，
//!          且 token 字段**发空**（只有 userId 有值）
//! :810-829 FetchUserStatus：URL = AuthBase + "/algo/api/v3/user/status?Encode=1"
//! :879-900 ParseAccountStatus：响应是**扁平**的，`nextResetAt` 在顶层
//! :752-753 注释原文：「NextResetAtMs is the subscription refresh instant in
//!          epoch millis. **0 means the gateway did not report one.**」
//! ```
//!
//! ⚠ 机器指纹**不伪造**：参考实现 `bridge.go:78-100` 是本地 uuid 随机编一套
//!（machineID/machineToken/machineType 全是 uuid.New()）。我方已实测国际版 host
//! 对残缺/伪造的指纹会回 **HTTP 200 + 结构合法的 JSON**，只是少一条活动 ——
//! 不报错、不告警（见 [`super::machine_identity`] 的文件头注释）。故这里一律走
//! [`machine_identity_of`]，拿不到就**不发**那几个头，绝不发空串或编造值。

use std::time::Duration;

use md5::{Digest, Md5};
use serde_json::{Map, Value};

use super::client::QoderClient;
use super::cosy::user_agent;
use super::cred::{truncate_str, QoderCred};
use super::encoding::qoder_encode;
use super::machine_identity::machine_identity_of;

/// 计费周期查询端点（拼在 [`QoderRegion::gateway`](super::region::QoderRegion::gateway) 上）。
///
/// `?Encode=1` 表示请求体经 [`qoder_encode`] 编码 —— 与 `MODELS_PATH` / `CHAT_PATH`
/// 同一约定。
pub(crate) const USER_STATUS_PATH: &str = "/algo/api/v3/user/status?Encode=1";

/// 这条通道的固定应用码（参考实现 `auth.go:137`）。
const USER_STATUS_APP_CODE: &str = "cosy";

/// 桌面客户端类型（参考实现 `auth.go:625` 固定 5）。
const USER_STATUS_CLIENT_TYPE: &str = "5";

/// HTTP Date 头格式，**必须是 GMT**。
///
/// 签名把 date 串原样纳入摘要，格式错一个字符签名就不对。
const USER_STATUS_DATE_LAYOUT: &str = "%a, %d %b %Y %H:%M:%S GMT";

/// 覆盖签名密钥的环境变量（参考实现 `auth.go:143-151`）。
const USER_STATUS_SECRET_ENV: &str = "QODER_SIGNATURE_SECRET";

/// 参考实现里的默认密钥（`auth.go:138`）。
///
/// 它是 base64 的 "war, war never changes"，在公开仓库里是**明文常量**，不属于
/// 凭据泄漏 —— 但允许用环境变量覆盖，便于上游换密钥时不动代码。
const USER_STATUS_DEFAULT_SECRET: &str = "d2FyLCB3YXIgbmV2ZXIgY2hhbmdlcw==";

/// 上限：2100-01-01T00:00:00Z。
///
/// 上游对**没有到期时间**的套餐会报 9999-12-31（253402214400000）这种哨兵值 ——
/// 那不是「下次重置」，是「永不过期」。参考实现 `bridge\session.go:40-49` 同值
/// 同理由，并明确要求「must never be mistaken for the instant the allowance
/// actually refreshes」。
///
/// ⚠ `crate::usage` 里有一份同值常量 [`crate::usage::MAX_PLAUSIBLE_RESET_MS`]：
/// 那个模块是**零产品依赖**的底层统计层（`server` 依赖它），让它反向依赖本模块
/// 会成环，故各留一份。两处都有测试钉住取值，改一处必须改另一处。
pub(crate) const MAX_PLAUSIBLE_RESET_MS: i64 = 4_102_444_800_000;

/// 判断一个下次重置时刻是否**可能是真的**。
///
/// 0 / 负数 = 上游没报；≥ 2100 年 = 哨兵值。两者都不是一个周期。
pub(crate) fn plausible_reset_ms(ms: i64) -> bool {
    ms > 0 && ms < MAX_PLAUSIBLE_RESET_MS
}

/// `/user/status` 里我方关心的字段（参考实现 `auth.go:738-764`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct AccountStatus {
    /// 账号类型（实测形如 `"teams"` / `"personal"`）。
    pub(crate) user_type: String,
    /// 套餐标识（实测形如 `"PLAN_TIER_TEAM"`）。
    pub(crate) plan: String,
    /// 套餐展示名。
    pub(crate) user_tag: String,
    /// 组织名（个人账号为空）。
    pub(crate) org_name: String,
    /// 订阅额度下次重置时刻（**epoch 毫秒**）。
    ///
    /// ⚠ **0 表示上游没有报**，不是「立刻重置」。调用方必须据此保留上一次的值，
    /// 而不是把周期清零（参考实现 `auth.go:752-753` 的原话）。
    pub(crate) next_reset_at_ms: i64,
    /// 额度是否已用尽。
    pub(crate) is_quota_exceeded: bool,
    /// 用量百分比（0..1）。
    pub(crate) total_usage_percentage: f64,
}

/// 取签名密钥（环境变量优先）。
fn user_status_secret() -> String {
    match std::env::var(USER_STATUS_SECRET_ENV) {
        Ok(v) if !v.is_empty() => v,
        _ => USER_STATUS_DEFAULT_SECRET.to_string(),
    }
}

/// 生成 HTTP Date 头（**UTC**，GMT 后缀）。
///
/// 用 `chrono` 的英文缩写月份/星期，与 Go 的 `Mon, 02 Jan 2006 15:04:05 GMT`
/// 逐字一致（`%a`/`%b` 在 chrono 里是英文缩写，不受 locale 影响）。
fn user_status_date(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format(USER_STATUS_DATE_LAYOUT).to_string()
}

/// 计算签名：`md5(appCode + "&" + secret + "&" + date)`。
///
/// 注意签名**只覆盖 date**，不含请求体 —— 请求体已由 `?Encode=1` 那套编码保护
/// （参考实现 `auth.go:157-163` 逐字如此）。
fn user_status_sign(date: &str) -> String {
    let mut h = Md5::new();
    h.update(USER_STATUS_APP_CODE.as_bytes());
    h.update(b"&");
    h.update(user_status_secret().as_bytes());
    h.update(b"&");
    h.update(date.as_bytes());
    hex::encode(h.finalize())
}

/// 序列化且**不转义 HTML**。
///
/// `serde_json::to_string` 默认**不**转义 `<` `>` `&`（与 Go 的 `json.Marshal`
/// 相反 —— Go 默认转义，必须 `SetEscapeHTML(false)`）。故这里直接用
/// `to_string` 即等价于参考实现的 `marshalNoEscape`，并且不补尾换行
///（Go 的 `Encoder.Encode` 会补一个 `\n`，参考实现把它剪掉）。
fn marshal_no_escape<T: serde::Serialize>(v: &T) -> Result<String, String> {
    serde_json::to_string(v).map_err(|e| format!("序列化请求体失败: {e}"))
}

/// `/user/status` 请求体的内层（Go `userStatusInner`）。
///
/// ⚠ **字段顺序必须与官方客户端的 dict 插入顺序一致**（参考实现
/// `auth.go:725-736` 的注释原文：「Field order MUST match the official client's
/// dict insertion order for signature compatibility」）—— 签名覆盖的是序列化后的
/// 字节，换个顺序摘要就变了。**不要**给这几个字段加 `skip_serializing_if`：
/// 参考实现是把空串**发出去**的。
///
/// ★★ 为什么必须是 `struct` 而不是 `json!({...})`：
/// `serde_json` 的 `Map` 默认是 **`BTreeMap`**（本 crate 没开 `preserve_order`
/// feature），`json!` 宏构造它时会按**键的字典序**排列 —— 于是
/// `userId/personalToken/securityOauthToken/refreshToken/needRefresh/authInfo`
/// 会变成 `authInfo/needRefresh/personalToken/refreshToken/securityOauthToken/userId`。
/// 序列化 `struct` 时字段**按声明顺序**直接写出，不经过 `Map`，与 Go 的结构体
/// 序列化逐字节等价。（本移植的第一版用了 `json!`，被
/// `build_user_status_body_keeps_field_order_and_empty_tokens` 抓到。）
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UserStatusInner {
    /// `userId` —— 这条通道**唯一**的有效载荷。
    user_id: String,
    personal_token: String,
    security_oauth_token: String,
    refresh_token: String,
    need_refresh: bool,
    /// `authInfo` —— 参考实现发 `{}`（`json.RawMessage("{}")`）。
    auth_info: Value,
}

/// 请求体的外层（与 `jobToken` 通道同一个外壳形状）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UserStatusOuter {
    /// 内层 JSON 的**文本**（不是对象）—— 参考实现 `userStatusOuter.Payload` 是
    /// `string`，序列化后是带引号的字符串。
    payload: String,
    encode_version: String,
}

/// 构造 `/user/status` 的请求体（内层 + 外层，再整体编码）。
fn build_user_status_body(uid: &str) -> Result<String, String> {
    // token 字段**发空**（参考实现 `auth.go:725-736`：只有 userId 有值）。
    // 这个端点靠机器身份签名认证，不需要会话令牌。
    let inner = UserStatusInner {
        user_id: uid.to_string(),
        personal_token: String::new(),
        security_oauth_token: String::new(),
        refresh_token: String::new(),
        need_refresh: false,
        auth_info: serde_json::json!({}),
    };
    let inner_text = marshal_no_escape(&inner)?;
    let outer = UserStatusOuter {
        payload: inner_text,
        encode_version: "1".to_string(),
    };
    Ok(qoder_encode(marshal_no_escape(&outer)?.as_bytes()))
}

impl QoderClient {
    /// 查询账号状态，取回计费周期相关的字段。
    ///
    /// 与其它端点一样：超时走 `RPC_TIMEOUT`（客户端自带），响应体限读 1 MiB，
    /// `>=400` 直接报错并把响应片段带上（**不静默降级成零值** —— 那会让
    /// 「查不到」看起来像「没有周期」）。
    ///
    /// `base` 为空时用账号所属区域的 Gateway 域；测试传本地假上游地址。
    pub(crate) async fn fetch_user_status_with_base(
        &self,
        cr: &QoderCred,
        base: &str,
    ) -> Result<AccountStatus, String> {
        if cr.uid.is_empty() {
            // userId 是这个端点的**唯一**有效载荷，空 uid 必然查不到东西 ——
            // 早失败，别让上游用一个语义不明的 200 把它盖过去。
            return Err("账号缺少 uid，无法查询计费周期".to_string());
        }
        let base = if base.is_empty() {
            cr.region.gateway()
        } else {
            base.to_string()
        };
        let url = format!("{base}{USER_STATUS_PATH}");
        let body = build_user_status_body(&cr.uid)?;
        let date = user_status_date(chrono::Utc::now());

        let mut req = self
            .rpc()
            .post(&url)
            .header("appcode", USER_STATUS_APP_CODE)
            .header("login-version", "v2")
            .header(reqwest::header::ACCEPT, "application/json")
            .header("accept-encoding", "identity")
            .header("cosy-version", "0.1.43")
            .header("cosy-clienttype", USER_STATUS_CLIENT_TYPE)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("date", &date)
            .header("signature", user_status_sign(&date))
            .header(reqwest::header::USER_AGENT, user_agent())
            .body(body);

        // ⚠ 机器指纹**不伪造**：拿不到就一个都不发（见文件头注释）。
        //
        // 这里刻意**不读** `cr.machine_token` —— 那个值由 `cred.rs` 的
        // `ensure_fingerprint` 本地编造（`hex_short(32)` + 固定 `"5"`），国服
        // host 不校验所以没暴露，国际版会静默降级成「少一条数据」。
        // `campaign.rs` 的 `apply_campaign_headers` 出于同一理由也只认
        // [`machine_identity_of`]。
        let id = machine_identity_of();
        if id.usable() {
            req = req
                .header("cosy-machinetoken", &id.machine_token)
                .header("cosy-machinetype", &id.machine_type)
                .header("cosy-machinecode", &id.machine_code);
        }
        // cosy-machineid：参考实现发的是本地编造的 uuid（`bridge.go:82`）。
        // 我方用凭证里**持久化**的 `machine.id`（与 `cosy.rs` 的会话通道同源）——
        // 它是「同一账号 + 同一设备」的稳定标识，而这条通道正是机器身份通道。
        //
        // ★★ 这正是「必须优先按 FilePath 重新加载凭证」的理由：`auth::Auth`
        // **没有**机器指纹字段，从池里重建凭证会让这个头**静默不发**。
        if !cr.machine_id.is_empty() {
            req = req.header("cosy-machineid", &cr.machine_id);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("查询计费周期失败: {e}"))?;
        let status = resp.status().as_u16();
        let raw = super::client::read_limited(resp, 1 << 20).await;
        let text = String::from_utf8_lossy(&raw).into_owned();
        if status >= 400 {
            return Err(format!(
                "查询计费周期失败（HTTP {status}）：{}",
                truncate_str(&text, 200)
            ));
        }

        let top: Map<String, Value> = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(_) => {
                return Err(format!(
                    "计费周期响应不是合法 JSON：{}",
                    truncate_str(&text, 160)
                ))
            }
        };
        match parse_account_status(&top) {
            Some(st) => Ok(st),
            None => Err(format!(
                "计费周期响应里没有可识别字段：{}",
                truncate_str(&text, 160)
            )),
        }
    }

    /// 生产入口：用账号所属区域的 Gateway 域。
    pub(crate) async fn fetch_user_status(
        &self,
        cr: &QoderCred,
    ) -> Result<AccountStatus, String> {
        self.fetch_user_status_with_base(cr, "").await
    }
}

/// 从 `/user/status` 的响应里取出账号状态。
///
/// 返回 `None` 表示「这个响应不是我认识的形状」（三个特征键全空）。调用方应据此
/// **保留上一次的缓存**，而不是把周期清零 —— 参考实现 `auth.go:879-900` 就是
/// 这么做的（它的返回签名是 `(AccountStatus, bool)`）。
pub(crate) fn parse_account_status(raw: &Map<String, Value>) -> Option<AccountStatus> {
    if let Some(st) = parse_account_status_flat(raw) {
        return Some(st);
    }
    // 兜底：若上游把结果包在 `{"code":0,"data":{…}}` 里。
    //
    // ⚠ 这一层**未在真上游验证过**（参考实现只读顶层）。加它是因为包一层信封是
    // 这个 API 家族的常见形态（`/sash/...` 那几个就是这样），而漏读的代价是
    // 「永远拿不到周期」却看不出原因。
    if let Some(data) = raw.get("data").and_then(|d| d.as_object()) {
        if let Some(st) = parse_account_status_flat(data) {
            return Some(st);
        }
    }
    None
}

fn parse_account_status_flat(raw: &Map<String, Value>) -> Option<AccountStatus> {
    let user_type = str_field(raw, "userType");
    let plan = str_field(raw, "plan");
    // 三键全空 ⇒ 形状不认识（参考实现 `auth.go:888` 的判据）。
    if user_type.is_empty() && plan.is_empty() && !raw.contains_key("nextResetAt") {
        return None;
    }
    Some(AccountStatus {
        user_type,
        plan,
        user_tag: str_field(raw, "userTag"),
        org_name: str_field(raw, "orgName"),
        next_reset_at_ms: to_int64_ms(raw.get("nextResetAt")),
        is_quota_exceeded: raw
            .get("isQuotaExceeded")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        total_usage_percentage: raw
            .get("totalUsagePercentage")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
    })
}

/// 取字符串字段；缺失或类型不对 ⇒ 空串。
fn str_field(m: &Map<String, Value>, key: &str) -> String {
    m.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// 把 JSON 数字取成 `int64` 毫秒。
///
/// ⚠ 只认数字：响应里 `nextResetAt` 是**数字**。字符串形态**不猜** ——
/// 猜错会把「未知」变成一个看起来很确定的错误时刻。
///
/// 同时接受 `as_i64` 与 `as_f64`（serde_json 对整数值会给出 `i64`，而
/// `1.79e12` 这种带小数点的形态只有 `f64`）。
fn to_int64_ms(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        _ => 0,
    }
}

/// 决定最终采用哪个「下次重置时刻」。
///
/// # 优先级（照抄参考实现 `bridge\session.go:193-202`）
///
/// ```text
/// ① `/user/status` 的 nextResetAt —— **权威**；
/// ② 它缺失时退回 `/api/v2/quota/usage` 的 expiresAt。
/// ```
///
/// 参考实现的原话：「the gateway's nextResetAt is authoritative, and the OpenAPI
/// expiresAt is only a fallback for when it is missing. Plans without an expiry
/// report the far-future 9999-12-31 sentinel in expiresAt, which must never be
/// mistaken for the instant the allowance actually refreshes.」
///
/// 返回 `0` = 两边都没给出可信值 ⇒ 调用方**保留**上一次的周期，不要清零。
///
/// ⚠ **生产路径只传 `None`**（`billing_cycle.rs` 的 fetcher 不调额度端点）：
/// `/api/v2/quota/usage` 的 `expiresAt` 是**套餐到期**，不是周期重置。多打一个
/// 端点换来一个语义不同的数字，只会让「周期」看起来更确定而实际更错。
/// 第二个参数保留是为了让「兜底分支」本身有测试覆盖（与 Go 同款）。
pub(crate) fn resolve_next_reset_ms(st: Option<&AccountStatus>, quota_expires_at: Option<i64>) -> i64 {
    let mut from_status = st.map(|s| s.next_reset_at_ms).unwrap_or(0);
    if !plausible_reset_ms(from_status) {
        from_status = 0;
    }
    if from_status != 0 {
        return from_status;
    }
    match quota_expires_at {
        Some(v) if plausible_reset_ms(v) => v,
        _ => 0,
    }
}

/// 单次查询的超时（与 [`super::client`] 的 `RPC_TIMEOUT` 同量级）。
///
/// 保留常量是为了让「这条通道有超时」在代码里可见 —— 实际超时由客户端的
/// `rpc` 实例携带。
#[allow(dead_code)]
pub(crate) const USER_STATUS_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::encoding::qoder_decode;
    use super::super::region::QoderRegion;
    use md5::{Digest, Md5};
    use serde_json::{Map, Value};


    // ─────────────── 计费周期：解析与可信度净化（Go `userstatus_test.go`） ───────────────

    fn status_with(next_reset: Option<i64>) -> AccountStatus {
        AccountStatus {
            user_type: "teams".to_string(),
            plan: "PLAN_TIER_TEAM".to_string(),
            next_reset_at_ms: next_reset.unwrap_or(0),
            ..Default::default()
        }
    }

    /// ① 可信值直通（`nextResetAt` 是权威来源）。
    #[test]
    fn resolve_next_reset_ms_passes_a_plausible_value_through() {
        let good = 1_790_000_000_000i64;
        assert_eq!(resolve_next_reset_ms(Some(&status_with(Some(good))), None), good);
        // 兜底参数在场也不该改变结果 —— 权威值优先。
        assert_eq!(
            resolve_next_reset_ms(Some(&status_with(Some(good))), Some(1_800_000_000_000)),
            good,
            "nextResetAt 是权威来源，不该被 expiresAt 顶掉"
        );
    }

    /// ② `nextResetAt` 缺失（0 / 无状态）⇒ 0（调用方据此**保留**上一周期）。
    #[test]
    fn resolve_next_reset_ms_returns_zero_when_missing() {
        assert_eq!(resolve_next_reset_ms(None, None), 0, "没有状态时应为 0");
        assert_eq!(
            resolve_next_reset_ms(Some(&status_with(Some(0))), None),
            0,
            "0 = 上游没报，**不是**「立刻重置」"
        );
        assert_eq!(
            resolve_next_reset_ms(Some(&status_with(None)), None),
            0,
            "字段缺失同样应为 0"
        );
    }

    /// ③ 不可信值（负数 / 9999 哨兵 / 超上限 / 恰好等于上限）⇒ 0。
    ///
    /// 哨兵（9999-12-31）的值大于一切真实边界 —— 一旦被当成周期，界面会显示
    /// 一个永远不到的重置时刻。
    #[test]
    fn resolve_next_reset_ms_rejects_implausible_values() {
        const SENTINEL: i64 = 253_402_214_400_000; // 9999-12-31
        for bad in [
            -1i64,
            SENTINEL,
            MAX_PLAUSIBLE_RESET_MS,     // 恰好等于上限 ⇒ 拒绝
            MAX_PLAUSIBLE_RESET_MS + 1, // 超过上限 ⇒ 拒绝
            1i64 << 62,
        ] {
            assert_eq!(
                resolve_next_reset_ms(Some(&status_with(Some(bad))), None),
                0,
                "不可信值 {bad} 应被净化为 0"
            );
        }
    }

    /// ③b `plausible_reset_ms` 的边界判据本身（与 `usage.rs` 的同名常量同值）。
    #[test]
    fn plausible_reset_ms_matches_the_usage_layer_constant() {
        assert_eq!(MAX_PLAUSIBLE_RESET_MS, crate::usage::MAX_PLAUSIBLE_RESET_MS, "两处常量必须同值");
        for (ms, want) in [
            (0i64, false),
            (-1, false),
            (1, true),
            (MAX_PLAUSIBLE_RESET_MS - 1, true),
            (MAX_PLAUSIBLE_RESET_MS, false),
            (253_402_214_400_000, false),
        ] {
            assert_eq!(plausible_reset_ms(ms), want, "plausible_reset_ms({ms})");
        }
    }

    /// ③c Quota 兜底分支：只在 `nextResetAt` 缺失时生效（生产不接，仅测试覆盖）。
    #[test]
    fn resolve_next_reset_ms_uses_quota_only_as_a_fallback() {
        let good = 1_790_000_000_000i64;
        assert_eq!(resolve_next_reset_ms(None, Some(good)), good, "缺失时应回退到 expiresAt");
        // 兜底值本身也要过可信度闸门（无到期套餐报 9999-12-31 哨兵）。
        assert_eq!(resolve_next_reset_ms(None, Some(253_402_214_400_000)), 0, "哨兵不该被采纳");
        assert_eq!(resolve_next_reset_ms(None, Some(-1)), 0);
        assert_eq!(resolve_next_reset_ms(Some(&status_with(Some(0))), Some(0)), 0);
    }

    /// 响应解析：扁平形状（真上游形态）。
    #[test]
    fn parse_account_status_reads_the_flat_shape() {
        let raw: Map<String, Value> = serde_json::from_str(
            r#"{"userType":"teams","plan":"PLAN_TIER_TEAM","userTag":"团队版",
                "orgName":"某组织","nextResetAt":1790000000000,
                "isQuotaExceeded":true,"totalUsagePercentage":0.25}"#,
        )
        .unwrap();
        let st = parse_account_status(&raw).expect("应能解析扁平形状");
        assert_eq!(st.user_type, "teams");
        assert_eq!(st.plan, "PLAN_TIER_TEAM");
        assert_eq!(st.user_tag, "团队版");
        assert_eq!(st.org_name, "某组织");
        assert_eq!(st.next_reset_at_ms, 1_790_000_000_000);
        assert!(st.is_quota_exceeded);
        assert!((st.total_usage_percentage - 0.25).abs() < 1e-9);
    }

    /// 响应解析：信封形状（`{"code":0,"data":{…}}`）与「形状不认识」的判据。
    #[test]
    fn parse_account_status_handles_envelope_and_rejects_unknown_shape() {
        let enveloped: Map<String, Value> =
            serde_json::from_str(r#"{"code":0,"data":{"plan":"P","nextResetAt":123}}"#).unwrap();
        let st = parse_account_status(&enveloped).expect("信封形状应能解析");
        assert_eq!(st.plan, "P");
        assert_eq!(st.next_reset_at_ms, 123);

        // 三个特征键全空 ⇒ 形状不认识（调用方据此**保留**上一次的缓存）。
        let unknown: Map<String, Value> = serde_json::from_str(r#"{"foo":1,"bar":"x"}"#).unwrap();
        assert!(parse_account_status(&unknown).is_none(), "不认识的形状应返回 None");

        // 只有 nextResetAt 在场也算认识（它是我们的目标字段）。
        let only_reset: Map<String, Value> =
            serde_json::from_str(r#"{"nextResetAt":456}"#).unwrap();
        assert_eq!(parse_account_status(&only_reset).unwrap().next_reset_at_ms, 456);
    }

    /// `nextResetAt` 是**数字**；字符串形态**不猜**（猜错会把「未知」变成错误的确定值）。
    #[test]
    fn to_int64_ms_accepts_numbers_only() {
        assert_eq!(to_int64_ms(Some(&serde_json::json!(1234))), 1234);
        assert_eq!(to_int64_ms(Some(&serde_json::json!(1234.9))), 1234, "浮点截断");
        assert_eq!(to_int64_ms(Some(&serde_json::json!("1234"))), 0, "字符串不猜");
        assert_eq!(to_int64_ms(Some(&serde_json::json!(null))), 0);
        assert_eq!(to_int64_ms(None), 0);
    }

    /// 请求体：字段顺序与空 token 必须与官方客户端一致（签名覆盖序列化字节）。
    ///
    /// ★★ **必须在原始字符串上断言顺序**，不能解析回 `Value` 再取 keys ——
    /// `serde_json` 的 `Map` 是 `BTreeMap`，解析后 keys() 恒为**字典序**，
    /// 那样断言等于什么都没测（本移植第一版就踩了：生产侧已修好，测试仍红）。
    #[test]
    fn build_user_status_body_keeps_field_order_and_empty_tokens() {
        let encoded = build_user_status_body("uid-placeholder-0001").expect("构造请求体");
        let decoded = qoder_decode(&encoded).expect("应能解码回来");
        let outer_text = String::from_utf8(decoded).unwrap();
        let outer: Value = serde_json::from_str(&outer_text).unwrap();
        assert_eq!(outer["encodeVersion"], "1", "外层编码版本固定为 1");

        // ① 外层顺序：payload 在前、encodeVersion 在后（与 Go 的 struct 声明序一致）。
        let payload_at = outer_text.find("\"payload\"").expect("外层缺 payload");
        let version_at = outer_text.find("\"encodeVersion\"").expect("外层缺 encodeVersion");
        assert!(payload_at < version_at, "外层字段顺序应为 payload → encodeVersion");

        // ② 内层顺序：在**原始文本**上按出现位置判定。
        let inner = outer["payload"].as_str().unwrap();
        let want = [
            "userId",
            "personalToken",
            "securityOauthToken",
            "refreshToken",
            "needRefresh",
            "authInfo",
        ];
        let mut last = 0usize;
        for (i, k) in want.iter().enumerate() {
            let needle = format!("\"{k}\"");
            let at = inner
                .find(&needle)
                .unwrap_or_else(|| panic!("内层缺字段 {k}（原始：{inner}）"));
            assert!(
                at >= last,
                "字段顺序变了（{k} 出现在 {i} 位之前）⇒ 签名会失效。原始：{inner}"
            );
            last = at;
        }
        assert_eq!(
            inner.matches("\":").count(),
            want.len(),
            "内层字段数应恰好是 6 个（多了或少了都会让上游解析失败）：{inner}"
        );

        // ③ 取值与「空串要发出去、不能省略」。
        let payload: Value = serde_json::from_str(inner).unwrap();
        assert_eq!(payload["userId"], "uid-placeholder-0001");
        assert_eq!(payload["personalToken"], "");
        assert_eq!(payload["securityOauthToken"], "");
        assert_eq!(payload["refreshToken"], "");
        assert_eq!(payload["needRefresh"], false);
        assert_eq!(payload["authInfo"], serde_json::json!({}));
        for k in ["personalToken", "securityOauthToken", "refreshToken"] {
            assert!(inner.contains(&format!("\"{k}\":\"\"")), "{k} 必须是空串而不是被省略");
        }
    }

    /// 签名：`md5(appCode + "&" + secret + "&" + date)`，且只覆盖 date。
    #[test]
    fn user_status_sign_matches_the_reference_formula() {
        let date = "Mon, 02 Jan 2006 15:04:05 GMT";
        let mut h = Md5::new();
        h.update(b"cosy&");
        h.update(USER_STATUS_DEFAULT_SECRET.as_bytes());
        h.update(b"&");
        h.update(date.as_bytes());
        let want = hex::encode(h.finalize());
        assert_eq!(user_status_sign(date), want);
        assert_eq!(user_status_sign(date).len(), 32, "MD5 十六进制应为 32 字符");

        // 换个 date ⇒ 签名必须变（证明 date 确实进了摘要）。
        assert_ne!(user_status_sign(date), user_status_sign("Tue, 03 Jan 2006 15:04:05 GMT"));
    }

    /// Date 头格式必须是 GMT 且为英文缩写（签名把它原样纳入摘要）。
    #[test]
    fn user_status_date_is_gmt_formatted() {
        let t = chrono::DateTime::parse_from_rfc3339("2026-01-02T15:04:05Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let s = user_status_date(t);
        assert_eq!(s, "Fri, 02 Jan 2026 15:04:05 GMT");
        assert!(s.ends_with(" GMT"), "必须是 GMT 后缀");
    }

    // ─────────────── 真 HTTP 往返（本地回环，不触外网） ───────────────
    //
    // 上面那些用例是纯函数级的。这一条走**真的 reqwest 请求**：
    // 起一个 std TcpListener 假上游 → `fetch_user_status_with_base` →
    // 校验收到的**方法/路径/头/请求体**，再校验解析回来的字段。
    // 它同时钉住了「请求体编码 → 上游能解回来」这条最容易静默失效的链路。

    /// 极简 HTTP/1.1 假上游：收一个请求，记下来，回一份给定响应。
    fn serve_once(
        response_body: &'static str,
        status_line: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 回环端口");
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            // 读到 header 结束 + content-length 指定的 body 收满为止。
            loop {
                let n = match sock.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let len: usize = text
                        .lines()
                        .find_map(|l| {
                            let l = l.to_ascii_lowercase();
                            l.strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + len {
                        break;
                    }
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).to_string());
            let resp = format!(
                "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        });
        (format!("http://{addr}"), rx)
    }

    /// 端到端：真发一个 `/user/status`，核对方法/路径/签名头/机器指纹头/请求体。
    #[tokio::test]
    async fn fetch_user_status_sends_the_machine_identity_request() {
        let flat = r#"{"userType":"teams","plan":"PLAN_TIER_TEAM","userTag":"团队版","orgName":"placeholder-org","nextResetAt":1793318400000,"isQuotaExceeded":false,"totalUsagePercentage":0.125}"#;
        let (base, rx) = serve_once(flat, "HTTP/1.1 200 OK");

        let client = QoderClient::new();
        let cr = QoderCred {
            uid: "uid-placeholder-0001".to_string(),
            dt: "dt-placeholder".to_string(),
            drt: "drt-placeholder".to_string(),
            machine_id: "machineid-placeholder-0001".to_string(),
            region: QoderRegion::CN,
            ..Default::default()
        };
        let st = client
            .fetch_user_status_with_base(&cr, &base)
            .await
            .expect("应能取回账号状态");

        // ① 解析结果正确。
        assert_eq!(st.user_type, "teams");
        assert_eq!(st.plan, "PLAN_TIER_TEAM");
        assert_eq!(st.next_reset_at_ms, 1_793_318_400_000);
        assert_eq!(
            resolve_next_reset_ms(Some(&st), None),
            1_793_318_400_000,
            "可信值应直通"
        );

        // ② 上游收到的请求形状。
        let raw = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("应收到请求");
        let head = raw.split("\r\n\r\n").next().unwrap();
        assert!(
            head.starts_with("POST /algo/api/v3/user/status?Encode=1 "),
            "方法/路径不对：{}",
            head.lines().next().unwrap_or("")
        );
        let lower = head.to_ascii_lowercase();
        for h in [
            "appcode: cosy",
            "login-version: v2",
            "cosy-clienttype: 5",
            "cosy-version: 0.1.43",
            "accept-encoding: identity",
        ] {
            assert!(lower.contains(h), "缺头 {h}：\n{head}");
        }
        // 机器身份通道的三个头 + machineid（★ 从**凭证文件**读回的指纹）。
        assert!(
            lower.contains("cosy-machineid: machineid-placeholder-0001"),
            "cosy-machineid 必须来自凭证（丢了就是「静默不发」那个缺陷）：\n{head}"
        );
        assert!(
            !lower.contains("authorization:"),
            "这条通道**不认 Bearer**，不该带 authorization 头：\n{head}"
        );
        // 签名是 32 位十六进制，且 date 是 GMT。
        let sig = head
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("signature:"))
            .map(|l| l.split(':').nth(1).unwrap().trim().to_string())
            .expect("必须有 signature 头");
        assert_eq!(sig.len(), 32, "签名应是 MD5 十六进制：{sig}");
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
        let date = head
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("date:"))
            .map(|l| l.split(':').skip(1).collect::<Vec<_>>().join(":").trim().to_string())
            .expect("必须有 date 头");
        assert!(date.ends_with("GMT"), "date 必须是 GMT：{date}");
        // 签名确实由 date 算出（换 date 就对不上）。
        assert_eq!(sig, user_status_sign(&date), "签名必须与发出的 date 一致");

        // ③ 请求体：解回来验证字段顺序与空 token（编码链路端到端正确）。
        let body = raw.split("\r\n\r\n").nth(1).unwrap();
        let decoded = qoder_decode(body).expect("请求体应是合法的 QoderEncode");
        let decoded = String::from_utf8(decoded).unwrap();
        let outer: Value = serde_json::from_str(&decoded).unwrap();
        assert_eq!(outer["encodeVersion"], "1");
        let inner = outer["payload"].as_str().unwrap();
        let mut last = 0usize;
        for k in [
            "userId",
            "personalToken",
            "securityOauthToken",
            "refreshToken",
            "needRefresh",
            "authInfo",
        ] {
            let at = inner.find(&format!("\"{k}\"")).unwrap_or_else(|| panic!("缺 {k}: {inner}"));
            assert!(at >= last, "字段顺序变了（{k}）⇒ 上游签名会失效：{inner}");
            last = at;
        }
        assert!(inner.contains("\"userId\":\"uid-placeholder-0001\""));
    }

    /// 端到端错误路径：上游 500 ⇒ `Err`（**不静默降级成零值**）。
    #[tokio::test]
    async fn fetch_user_status_reports_an_upstream_error_instead_of_zero() {
        let (base, _rx) = serve_once(r#"{"error":"boom"}"#, "HTTP/1.1 500 Internal Server Error");
        let client = QoderClient::new();
        let cr = QoderCred {
            uid: "uid-placeholder-0001".to_string(),
            dt: "dt-placeholder".to_string(),
            machine_id: "machineid-placeholder-0001".to_string(),
            region: QoderRegion::CN,
            ..Default::default()
        };
        let err = client
            .fetch_user_status_with_base(&cr, &base)
            .await
            .expect_err("500 必须报错");
        assert!(err.contains("500"), "错误里应带状态码：{err}");
        assert!(err.contains("查询计费周期失败"), "错误文案：{err}");
    }

    /// 端到端：形状不认识的 200 ⇒ `Err`（调用方据此保留上一次的周期）。
    #[tokio::test]
    async fn fetch_user_status_rejects_an_unrecognised_200() {
        let (base, _rx) = serve_once(r#"{"foo":1}"#, "HTTP/1.1 200 OK");
        let client = QoderClient::new();
        let cr = QoderCred {
            uid: "uid-placeholder-0001".to_string(),
            dt: "dt-placeholder".to_string(),
            machine_id: "machineid-placeholder-0001".to_string(),
            region: QoderRegion::CN,
            ..Default::default()
        };
        let err = client
            .fetch_user_status_with_base(&cr, &base)
            .await
            .expect_err("形状不认识应报错");
        assert!(err.contains("没有可识别字段"), "错误文案：{err}");
    }

    /// 空 uid ⇒ 早失败（**不发请求**）。
    #[tokio::test]
    async fn fetch_user_status_fails_fast_without_a_uid() {
        let client = QoderClient::new();
        let cr = QoderCred {
            uid: String::new(),
            dt: "dt-placeholder".to_string(),
            ..Default::default()
        };
        // 故意给一个必然连不上的地址：若是「先请求再判 uid」这里会超时而不是立刻返回。
        let err = client
            .fetch_user_status_with_base(&cr, "http://127.0.0.1:1")
            .await
            .expect_err("空 uid 应早失败");
        assert!(err.contains("缺少 uid"), "错误文案：{err}");
    }
}
