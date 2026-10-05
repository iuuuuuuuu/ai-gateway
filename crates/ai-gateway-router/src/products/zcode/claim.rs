//! ZCode **套餐预览与领取**端点原语（Go `internal/zcode/claim.go` 的对应物）。
//!
//! # 为什么单独成模块（不是塞进 `cli.rs`）
//!
//! 这两个端点是**两条路径共用的原语**：
//!
//! ```text
//!   CLI 子命令        zcode-login preview-plan / claim-plan
//!   定时调度器        claim_scheduler（按周期查 preview、领可领的套餐）
//! ```
//!
//! 定时路径要复用**同一份**实现：塞进 `cli.rs` 的私有函数里，调度器要么
//! 重复实现一遍（两份实现必然漂移），要么跨模块调不到（私有）。
//! 故这里放成 `pub(crate)`，两边都直接调。
//!
//! # 两个端点是两回事（别混）
//!
//! ```text
//!   billing/balance   （client.rs 的 fetch_quota）  我现在**有多少**
//!   billing/preview   （本模块 fetch_plan_preview） 我还能**领什么**
//! ```
//!
//! 前者回答"余额"，后者回答"还能领什么"。混用会得到「有额度但领不了」
//! 或「能领但看不到额度」这类误导性结论。
//!
//! # 三条必须逐字保留的语义（Go 注释里写得很明确）
//!
//! 1. **host 走 `quota_host`（恒 `zcode.z.ai`），不是 `biz_host`** ——
//!    两个服务商的 billing 端点都只在那一个 host 上有数据（Go `claim.go:95-96`）。
//!    用各自 `biz_host` 会得到 `HTTP 200 {"code":500,"msg":"404 NOT_FOUND"}`，
//!    那是 200，会被误当成「查到了但没有」。
//! 2. **preview 必带 query** `?app_version=<AppVersion>&platform=<PlatformArch>`
//!    —— 实测不带这两个参数上游回 `3001 parameter error`（Go `claim.go:115-117`）。
//! 3. **preview 用 [`Identity::preview_headers`]，claim 用 [`Identity::claim_headers`]**
//!    —— 这是**两个不同的头集**，混用是**静默失败**（上游按"是否与真机一致"
//!    判风控，多余的头本身就是可区分特征）。
//!
//! 另有一条同样容易写错的：preview 的 `Authorization` **有 JWT 才发** ——
//! 缺 JWT 时**一个认证头都不发**，匿名 preview 是合法的（拿空清单而不是 401）。

use serde_json::Value;

use super::client::Client;
use super::cred::ZcodeCred;
use super::errors::{self, ErrKind, UpstreamError};
// ★ `Identity` 只在测试里用（`preview_url_public` 的参数类型）——
// 生产代码里 `identity` 是 `Client` 的字段，不需要单独引入这个名字。
#[cfg(test)]
use super::identity::Identity;

/// 可领套餐列表端点（Go `claim.go:97`）。
pub(crate) const PREVIEW_PATH: &str = "/api/v1/zcode-plan/billing/preview";

/// 领取端点（Go `claim.go:100`）。
pub(crate) const CLAIM_PATH: &str = "/api/v1/zcode-plan/billing/claim";

/// 上游响应的读取上限（Go 两处都是 `1<<20`）。
const MAX_CLAIM_BODY: usize = 1 << 20;

/// 一个**可领取**的套餐（Go `PlanOffer`，`claim.go:58`）。
///
/// `raw` 保留上游原始对象：`priority` 这类字段没有专门字段，但事件/调度
/// 路径要用它排序（见 `warm.rs` 的 `priority_of`），故原样留着。
#[derive(Debug, Clone, Default)]
pub(crate) struct PlanOffer {
    pub plan_id: String,
    pub name: String,
    pub status: String,
    pub ends_at: i64,
    pub raw: Value,
}

/// 可领套餐查询结果（Go `PlanPreview`，`claim.go:69`）。
#[derive(Debug, Clone, Default)]
pub(crate) struct PlanPreview {
    pub server_time: i64,
    pub offers: Vec<PlanOffer>,
    /// 上游 `data` 原样保留（供界面展示未解析字段）。
    ///
    /// ★ `#[allow(dead_code)]`：Go 的 `PlanPreview.Raw` 带 `json:"-"`，即
    /// **不参与 CLI 序列化** —— 它只是给同进程内的调用方留的备用视图，而
    /// Rust 侧的 `preview-plan` 只输出 `serverTime`/`offers`/`claimable`。
    /// 保留字段的理由与 Go 相同：上游若新增字段，解析器不认但界面可能要看；
    /// 删掉它会让「上游给了、我们丢了」在代码里看不出来。
    #[allow(dead_code)]
    pub raw: Value,
}

/// 领取结果（Go `ClaimResult`，`claim.go:80`）。
#[derive(Debug, Clone, Default)]
pub(crate) struct ClaimResult {
    /// 是否成功领到（`already_claimed` 也算「已达成目标」，故为 `true`）。
    ///
    /// ★ `#[allow(dead_code)]`：CLI 侧 `claim_plan_json` 输出的是
    /// `alreadyClaimed` 与 `message`（Go `runClaimPlan` 也没输出 `OK`）。
    /// `warm.rs` 的 `claim_offered_plans` 用的是 `already_claimed` 分支而不是
    /// 这个字段。保留是因为 Go `ClaimResult.OK` 的语义（「已领过 = 达成目标」）
    /// 是这个类型最核心的一句话，删掉字段就等于丢掉这个约定。
    #[allow(dead_code)]
    pub ok: bool,
    pub code: String,
    pub msg: String,
    /// ★ `1003 already_claimed` 置位 —— **不是错误**，见 [`Client::claim_plan`]。
    pub already_claimed: bool,
    pub claimed_at: i64,
}

/// 领取端点可能要求验证码时，上游回的业务码。
pub(crate) const CODE_ALREADY_CLAIMED: &str = "1003";

/// 本模块两个原语的错误类型。
///
/// # ★ 为什么不能只返回 `String`
///
/// 定时调度器（`claim_scheduler.rs`）要实现 Go `claim_scheduler.go:493-530`
/// 的 `classifyClaimError`，而它**必须读 `status` 与 `code`**：
///
/// ```text
///   code 非空            ⇒ 先查业务码表
///   否则 status == 404   ⇒ ClaimFailNotFound
///   401 ⇒ LoginRequired；>= 400 ⇒ HTTPError；否则 Unknown
/// ```
///
/// 其中 **`404` 那条只能靠 HTTP 状态判**：Go `:507-517` 明确记录该响应体是
/// `{"error_msg":"404 Route Not Found"}`，里面**没有 `code` 字段** ——
/// [`errors::extract_code`] 对它三个分支全不命中，返回空串。
/// 若在这里把 [`UpstreamError`] `to_string()` 掉，`404` 与 `code` 就都丢了，
/// 调度器只剩「正则反解 `Display`」这条脆弱得多的路（`HTTP 404` 与
/// `HTTP 429 code=1113 余额不足` 两种形态都得猜）。
///
/// 故错误必须**带类型**地穿过模块边界。
pub(crate) enum ClaimError {
    /// 上游返回的错误（保留 `status`/`code`/`msg`/`kind`，供调度器分类）。
    Upstream(UpstreamError),
    /// 本地校验/参数错误（Go 侧对应 `fmt.Errorf`，`classifyClaimError` 归为
    /// `ClaimFailUnknown`）。与上游错误**必须可区分** —— 调度器对前者的处置
    /// 是「不罚账号」，对后者才是「记一次失败」。
    Local(String),
}

impl std::fmt::Display for ClaimError {
    /// ★ `Upstream` 分支**逐字沿用** [`UpstreamError`] 的 `Display`
    ///（即 Go 的 `Error.Error()`，`HTTP {status} code={code} {msg}`），
    /// 这样 `warm` / CLI 那些只消费文本的调用方看到的文案与 Go 完全一致。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimError::Upstream(e) => write!(f, "{e}"),
            ClaimError::Local(s) => write!(f, "{s}"),
        }
    }
}

impl std::fmt::Debug for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 与 `Display` 同款：这个类型的 Debug 只用于测试与日志，
        // 派生 Debug 会把 `UpstreamError` 的内部字段铺开，噪声大于价值。
        write!(f, "{self}")
    }
}

impl ClaimError {
    /// 上游 HTTP 状态码；`Local` 没有这个概念 ⇒ `0`。
    ///
    /// 调度器的 `404 ⇒ NotFound` 判据走这里（见类型文档）。
    pub(crate) fn status(&self) -> u16 {        match self {
            ClaimError::Upstream(e) => e.status,
            ClaimError::Local(_) => 0,
        }
    }

    /// 上游业务码；`Local` 与「无码的响应」都是空串。
    pub(crate) fn code(&self) -> &str {
        match self {
            ClaimError::Upstream(e) => &e.code,
            ClaimError::Local(_) => "",
        }
    }

    /// 是否「上游要求人机验证」（`warm` 的换 param 重试据此判定）。
    pub(crate) fn is_captcha_required(&self) -> bool {
        match self {
            ClaimError::Upstream(e) => {
                e.kind == ErrKind::CaptchaRequired || e.code == errors::code::CAPTCHA_REQUIRED
            }
            ClaimError::Local(_) => false,
        }
    }
}

impl Client {
    /// 查**可领取**的套餐。对应 Go `FetchPlanPreview`（`claim.go:111-202`）。
    ///
    /// 三条头/参数约束见模块头。
    pub(crate) async fn fetch_plan_preview(
        &self,
        cr: &ZcodeCred,
    ) -> Result<PlanPreview, ClaimError> {
        // ⚠ 必须带 query：实测不带 app_version/platform 回 3001。
        let url = format!(
            "{}{}?app_version={}&platform={}",
            self.quota_host(),
            PREVIEW_PATH,
            url_encode(&self.identity.app_version),
            url_encode(&self.identity.platform_arch())
        );

        let mut h = reqwest::header::HeaderMap::new();
        // 极简头：preview 不发身份头 bundle。
        for (k, v) in self.identity.preview_headers(&cr.device_mid) {
            put(&mut h, &k, &v).map_err(ClaimError::Local)?;
        }
        // Authorization：**有 JWT 才发**。缺 JWT 时一个认证头都不发 ——
        // 匿名 preview 合法（拿空清单，而不是 401）。
        let jwt = cr.jwt.trim();
        if !jwt.is_empty() {
            put(&mut h, "authorization", &format!("Bearer {jwt}")).map_err(ClaimError::Local)?;
        }

        let resp = self
            .http
            .get(&url)
            .headers(h)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| ClaimError::Local(format!("查询可领套餐失败: {e}")))?;
        let status = resp.status().as_u16();
        let raw = super::client::read_limited(resp, MAX_CLAIM_BODY).await;
        let body = String::from_utf8_lossy(&raw).into_owned();

        if status >= 400 {
            return Err(ClaimError::Upstream(upstream_error(status, &body)));
        }

        let doc: Value = serde_json::from_slice(&raw).map_err(|e| {
            ClaimError::Local(format!(
                "解析可领套餐响应失败: {e}（原始：{}）",
                errors::truncate(&body, 200)
            ))
        })?;

        // 业务码非 0 ⇒ 上游用 HTTP 200 报错。
        let code = doc.get("code").map(to_i64).unwrap_or(0);
        if code != 0 {
            // ★ 这里**必须**保住类型：上游用 HTTP 200 + 业务码报错是常态
            // （3007 验证码、3001 参数错都走这条），擦成 String 会让调度器
            // 把它们全归成 Unknown。
            //
            // 旧实现额外做了一次「msg 不在文本里就补上去」—— 那是给 `String`
            // 版本打补丁；`upstream_error` 已经用 `msg_of`（先 `msg` 后
            // `error.message`）填了 `e.msg`，`Display` 必然已含它，故那一步
            // 在有类型之后是恒假的，去掉。
            return Err(ClaimError::Upstream(upstream_error(status, &body)));
        }

        let data = doc.get("data");
        let mut out = PlanPreview {
            server_time: data
                .and_then(|d| d.get("server_time"))
                .map(to_i64)
                .unwrap_or(0),
            offers: Vec::new(),
            raw: data.cloned().unwrap_or(Value::Null),
        };
        if let Some(plans) = data.and_then(|d| d.get("plans")).and_then(|p| p.as_array()) {
            for p in plans {
                out.offers.push(PlanOffer {
                    plan_id: first_non_empty(&[str_of(p, "plan_id"), str_of(p, "planId")]),
                    name: str_of(p, "name"),
                    // ⚠ 三种拼写都要认（snake / 带 claim 前缀 / camelCase）——
                    // 只认两种会漏掉 camelCase 的 `claimStatus`，而那是**实际
                    // 响应里出现过**的一种。
                    status: first_non_empty(&[
                        str_of(p, "status"),
                        str_of(p, "claim_status"),
                        str_of(p, "claimStatus"),
                    ]),
                    ends_at: to_i64(first_of(p, &["ends_at", "endsAt"])),
                    raw: p.clone(),
                });
            }
        }
        Ok(out)
    }

    /// 领取一个套餐。对应 Go `ClaimPlan`（`claim.go:216-287`）。
    ///
    /// # ⚠ 这是**写操作**
    ///
    /// 只应由用户在界面上显式点击（或 CLI 子命令）触发，**不做定时自动抢**。
    ///
    /// # `1003 already_claimed` 不是错误
    ///
    /// 目标（拿到该套餐）**已经达成**。当错误报的话，用户重复点会以为每次都失败。
    pub(crate) async fn claim_plan(
        &self,
        cr: &ZcodeCred,
        plan_id: &str,
    ) -> Result<ClaimResult, ClaimError> {
        let plan_id = plan_id.trim();
        if plan_id.is_empty() {
            // 本地拦下：不拦的话会发 `{"plan_id":""}` 上去，上游回 3001
            // 「parameter error」—— 那个文案完全看不出是「我们没传 ID」，
            // 排查方向会被引向「是不是缺 X-Device-Mid」。
            //
            // ★ `Local` 而非 `Upstream`：调度器对前者的处置是
            // `ClaimFailUnknown` + 不罚账号（Go `classifyClaimError` 同款）。
            return Err(ClaimError::Local("套餐 ID 不能为空".to_string()));
        }

        let payload = serde_json::json!({ "plan_id": plan_id }).to_string();
        let url = format!("{}{}", self.quota_host(), CLAIM_PATH);

        let mut h = reqwest::header::HeaderMap::new();
        for (k, v) in self.identity.claim_headers(&cr.device_mid) {
            put(&mut h, &k, &v).map_err(ClaimError::Local)?;
        }
        put(&mut h, "content-type", "application/json").map_err(ClaimError::Local)?;
        // ★ 这条路径用 **JWT**，不是 `x-api-key` —— 参考实现的 claim 客户端
        // 不发 x-api-key（那是对话面的双头做法，两条路径不同）。
        let jwt = cr.jwt.trim();
        if !jwt.is_empty() {
            put(&mut h, "authorization", &format!("Bearer {jwt}")).map_err(ClaimError::Local)?;
        }
        // 验证码头：**仅在已求到时**。与对话路径同一取舍：没有就不发，
        // 让上游如实回 3007，而不是我们猜。
        if !cr.captcha_param.is_empty() {
            put(&mut h, "x-aliyun-captcha-verify-param", &cr.captcha_param)
                .map_err(ClaimError::Local)?;
            if !cr.captcha_region.is_empty() {
                put(&mut h, "x-aliyun-captcha-verify-region", &cr.captcha_region)
                    .map_err(ClaimError::Local)?;
            }
        }

        let resp = self
            .http
            .post(&url)
            .headers(h)
            .body(payload)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| ClaimError::Local(format!("领取套餐失败: {e}")))?;
        let status = resp.status().as_u16();
        let raw = super::client::read_limited(resp, MAX_CLAIM_BODY).await;
        let body = String::from_utf8_lossy(&raw).into_owned();

        let code = code_of(&body);
        let msg = msg_of(&body);

        // `1003 already_claimed` ⇒ 成功。见方法文档。
        if code == CODE_ALREADY_CLAIMED {
            return Ok(ClaimResult {
                ok: true,
                code,
                msg,
                already_claimed: true,
                claimed_at: 0,
            });
        }
        // ⚠ `code != "" && code != "0"` 才算错误：上游成功时可能给空码或 "0"。
        if status >= 400 || (!code.is_empty() && code != "0") {
            return Err(ClaimError::Upstream(upstream_error(status, &body)));
        }

        // `claimed_at` 取不到不算错（Go 侧 `_ = json.Unmarshal`）。
        let claimed_at = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("data").and_then(|d| d.get("claimed_at")).map(to_i64))
            .unwrap_or(0);
        Ok(ClaimResult {
            ok: true,
            code,
            msg,
            already_claimed: false,
            claimed_at,
        })
    }
}

// ───────────────────────────── 辅助（Go `claim.go:290-351`） ─────────────────────────────

/// 取字符串字段；缺失或类型不对 ⇒ 空串（Go `strOf`，`claim.go:290`）。
pub(crate) fn str_of(m: &Value, key: &str) -> String {
    m.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// 上游响应体里的业务码。对应 Go `codeOf`（`claim.go:306-320`）。
///
/// 上游形状有**两种**（都实测见过）：
///
/// ```text
///   {"code":3001,"msg":"parameter error"}           ← 控制面
///   {"error":{"code":"1113","message":"余额不足"}}   ← 对话面
/// ```
///
/// 故两个都要认 —— 否则会把「有业务码的错误」当成「没有码」，进而用 HTTP
/// 状态码兜底分类，**那正是把 1113 误判成「限流」的根源**。
///
/// ★ 与 [`errors::extract_code`] 的差异（刻意保留 Go 的语义）：
/// - Go 的 `codeOf` 只看 `code` 与 `error.code`，**不看** `error.type`；
/// - Go 显式**跳过 `"0"`**（`"0"` 会继续去看 `error.code`）。
pub(crate) fn code_of(body: &str) -> String {
    let Ok(doc) = serde_json::from_str::<Value>(body) else {
        return String::new();
    };
    let top = any_to_code(doc.get("code"));
    if !top.is_empty() && top != "0" {
        return top;
    }
    any_to_code(doc.get("error").and_then(|e| e.get("code")))
}

/// 上游响应体里的可读文案。对应 Go `msgOf`（`claim.go:322-333`）：
/// `firstNonEmpty(msg, error.message)`。
pub(crate) fn msg_of(body: &str) -> String {
    let Ok(doc) = serde_json::from_str::<Value>(body) else {
        return String::new();
    };
    let msg = doc.get("msg").and_then(|v| v.as_str()).unwrap_or("");
    if !msg.trim().is_empty() {
        return msg.trim().to_string();
    }
    doc.get("error")
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 把任意形状的码转成字符串。对应 Go `anyToCode`（`claim.go:339-351`）。
///
/// ★ 数字用 `%d` 而不是 `%v`（Go 侧注释点明）：后者会输出 `3001.0`。
/// 上游有时发数字、有时发字符串，只认一种会漏判。
pub(crate) fn any_to_code(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => {
            // 与 `%d`（int64 强转）同款：小数部分截断，不是四舍五入。
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(f) = n.as_f64() {
                format!("{}", f.trunc() as i64)
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// 按 Go `toInt64` 的口径取整数（字符串里是数字也认）。
fn to_i64(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/// 按顺序取第一个存在的字段（Go `firstAny`）。
fn first_of<'a>(v: &'a Value, keys: &[&str]) -> &'a Value {
    for k in keys {
        if let Some(x) = v.get(*k) {
            if !x.is_null() {
                return x;
            }
        }
    }
    &Value::Null
}

/// 取第一个非空字符串。
fn first_non_empty(vals: &[String]) -> String {
    for v in vals {
        if !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    String::new()
}

/// query 参数转义（Go `url.QueryEscape` 的口径：空格 ⇒ `+`）。
///
/// 这里的取值只有 `app_version`（形如 `3.14.0`）与 `platform`（`win32-x64`），
/// 都不含需要转义的字符 —— 但**照抄 Go 的行为**，免得将来取值变化时静默出错。
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 往头表里塞一个头（值空则跳过）。与 `client.rs` 的同名私有函数同款 ——
/// 那个是私有的，这里照抄一份以免改动既有文件。
fn put(
    h: &mut reqwest::header::HeaderMap,
    name: &str,
    value: &str,
) -> Result<(), String> {
    if value.is_empty() {
        return Ok(());
    }
    let n = reqwest::header::HeaderName::from_bytes(name.as_bytes())
        .map_err(|e| format!("请求头名非法 {name}: {e}"))?;
    let v = reqwest::header::HeaderValue::from_str(value)
        .map_err(|e| format!("请求头 {name} 的值非法（含换行或控制字符）: {e}"))?;
    h.insert(n, v);
    Ok(())
}

/// 构造分类后的上游错误。
///
/// ★ 与 `client.rs` 的 `UpstreamError::new(status, body)` 有一处**刻意的差异**：
/// Go 的 `FetchPlanPreview`/`ClaimPlan` 用的是 `codeOf`/`msgOf`（只认 `code`
/// 与 `error.code`），不是 `extractCode`/`extractMessage`。故这里显式覆盖
/// `code`/`msg`，让 `Display`（= Go 的 `Error.Error()`）逐字对齐。
///
/// # ⚠ 覆盖是**无条件**的 —— 空串也是结论
///
/// `codeOf`/`msgOf` 的**窄**口径相对 `extractCode`/`extractMessage` 的**宽**
/// 口径，其价值一半在于「取到什么」，另一半在于「**取不到时不硬凑**」。
/// 上游发 `{"error":{"type":"11102"}}` 时 Go 的 `Code` 就是 `""`；
/// 发 `{"error_msg":"404 Route Not Found"}` 时 Go 的 `Msg` 就是 `""`。
///
/// 若写成 `if !code.is_empty() { e.code = code }`，那个空串就被误读成
/// 「没数据，保留原值」，于是宽口径的结果从缝里漏出来 —— 实测两处：
///
/// | body | Go `Error` | 带守卫时的漏出值 |
/// |---|---|---|
/// | `{"error":{"type":"11102","message":"m"}}` | `Code=""` | `"11102"`（`extract_code` 认 `type`） |
/// | `{"error_msg":"404 Route Not Found"}` | `Msg=""` | 整条 body（`extract_message` 兜底截断原文） |
///
/// 后者正是调度器判 `ClaimFailNotFound` 的那条路径，文案必须是干净的
/// `HTTP 404`（见本模块类型文档 `:106-111`）。非 JSON 的 HTML 错误页同理。
///
/// 分类（`kind`）仍然交给 [`errors::classify`] —— 它与 Go `Classify` 同源，
/// 且在 `error.type` 上**刻意比 `codeOf` 宽**，这正是我们想要的（分类要认得
/// Anthropic 形状的 1113，而 `Error.Code` 不该跟着变宽）。
fn upstream_error(status: u16, body: &str) -> UpstreamError {
    let mut e = UpstreamError::new(status, body);
    // ★ **无条件**覆盖，不是「非空才覆盖」：Go 那边是结构体字面量
    // `&Error{Kind: Classify(...), Status: …, Code: codeOf(body), Msg: msgOf(body)}`
    // —— 四个字段**全部赋值**。若只在不为空时覆盖，`extract_code` /
    // `extract_message` 的值就会从缝里漏出来，两处实测差异：
    //   ① `{"error":{"type":"11102","message":"m"}}`：`codeOf` 不看 `error.type`
    //      ⇒ Go 的 `Code` 是 `""`，而 `extract_code` 认 type ⇒ 漏出 "11102"；
    //   ② `{"error_msg":"404 Route Not Found"}`：`msgOf` 只认 `msg` /
    //      `error.message` ⇒ Go 的 `Msg` 是 `""`，而 `extract_message` 退到
    //      「截断原文」⇒ 漏出整条 body。
    // 后者正是调度器判 `ClaimFailNotFound` 的那条路径，文案必须是干净的
    // `HTTP 404`（`claim.rs` 类型文档 `:106-111`）。
    e.code = code_of(body);
    e.msg = msg_of(body);
    // 分类要按覆盖后的码重算：`classify` 内部走的是 `extract_code`，
    // 而两者对 `error.type` 的处理不同。只在「原来的分类什么都没判出来」
    // 时才用业务码兜底，避免把 Anthropic 形状的 1113 判丢。
    if e.kind == ErrKind::None {
        e.kind = kind_of_code(&e.code);
    }
    e
}

/// 业务码 ⇒ 分类（与 Go `Classify` 的业务码分支同表）。
fn kind_of_code(code: &str) -> ErrKind {
    use super::errors::code as c;
    match code {
        c::AUTH_MISSING => ErrKind::AuthMissing,
        c::AUTH_FAILED => ErrKind::AuthFailed,
        c::QUOTA_EXHAUSTED => ErrKind::NoResourcePack,
        c::MODEL_NOT_FOUND => ErrKind::ModelNotFound,
        c::PLAN_REQUIRED => ErrKind::PlanRequired,
        c::CAPTCHA_REQUIRED => ErrKind::CaptchaRequired,
        _ => ErrKind::None,
    }
}

/// preview 的 URL（供测试与调度器断言）。
#[cfg(test)]
fn preview_url_public(identity: &Identity) -> String {
    format!(
        "{}?app_version={}&platform={}",
        PREVIEW_PATH,
        url_encode(&identity.app_version),
        url_encode(&identity.platform_arch())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_match_the_go_constants() {
        assert_eq!(PREVIEW_PATH, "/api/v1/zcode-plan/billing/preview");
        assert_eq!(CLAIM_PATH, "/api/v1/zcode-plan/billing/claim");
    }

    #[test]
    fn code_of_recognizes_both_upstream_shapes() {
        // 控制面：数字码 + msg
        assert_eq!(code_of(r#"{"code":3001,"msg":"parameter error"}"#), "3001");
        // 对话面：字符串码嵌在 error 里
        assert_eq!(
            code_of(r#"{"error":{"code":"1113","message":"余额不足"}}"#),
            "1113"
        );
        // 字符串形态的顶层码
        assert_eq!(code_of(r#"{"code":"1003"}"#), "1003");
        // code=0 ⇒ 跳过，继续看 error.code
        assert_eq!(code_of(r#"{"code":0,"error":{"code":"1003"}}"#), "1003");
        // 无码
        assert_eq!(code_of("{}"), "");
        assert_eq!(code_of("not json"), "");
    }

    #[test]
    fn any_to_code_formats_numbers_without_a_decimal_point() {
        // Go 用 %d 而不是 %v —— 后者会给出 "3001.0"。
        let v: Value = serde_json::from_str(r#"{"code":3001.0}"#).unwrap();
        assert_eq!(any_to_code(v.get("code")), "3001");
        let v: Value = serde_json::from_str(r#"{"code":"3001"}"#).unwrap();
        assert_eq!(any_to_code(v.get("code")), "3001");
        let v: Value = serde_json::from_str(r#"{"code":true}"#).unwrap();
        assert_eq!(any_to_code(v.get("code")), "");
    }

    #[test]
    fn msg_of_prefers_msg_then_error_message() {
        assert_eq!(msg_of(r#"{"msg":"a","error":{"message":"b"}}"#), "a");
        assert_eq!(msg_of(r#"{"error":{"message":"b"}}"#), "b");
        assert_eq!(msg_of(r#"{"code":0}"#), "");
    }

    #[test]
    fn str_of_ignores_non_string_values() {
        let v: Value = serde_json::from_str(r#"{"a":"x","b":1,"c":null}"#).unwrap();
        assert_eq!(str_of(&v, "a"), "x");
        assert_eq!(str_of(&v, "b"), "");
        assert_eq!(str_of(&v, "c"), "");
        assert_eq!(str_of(&v, "missing"), "");
    }

    #[test]
    fn preview_url_carries_app_version_and_platform() {
        let id = Identity::new();
        let u = preview_url_public(&id);
        assert!(u.starts_with("/api/v1/zcode-plan/billing/preview?app_version="));
        assert!(u.contains("&platform="));
        // 平台必须是 Node 命名（win32-x64），上游严格校验。
        assert!(u.contains("win32-x64") || u.contains("darwin-") || u.contains("linux-"));
    }

    #[test]
    fn url_encode_escapes_like_go_query_escape() {
        assert_eq!(url_encode("3.14.0"), "3.14.0");
        assert_eq!(url_encode("win32-x64"), "win32-x64");
        assert_eq!(url_encode("a b"), "a+b");
        assert_eq!(url_encode("a/b"), "a%2Fb");
    }

    #[test]
    fn already_claimed_is_recognized_as_success_code() {
        assert_eq!(CODE_ALREADY_CLAIMED, "1003");
    }

    #[test]
    fn upstream_error_overrides_code_and_msg_with_go_semantics() {
        // `error.type` 是 Anthropic 形状：`extract_code` 会认它，
        // 但 Go 的 `codeOf` 不认 —— 覆盖后应回到 Go 的行为。
        let e = upstream_error(400, r#"{"error":{"type":"11102","message":"m"}}"#);
        assert_eq!(e.code, "");
        assert_eq!(e.msg, "m");
        // 分类仍要能认出模型不存在（`extract_code` 认得 type）。
        assert_eq!(e.kind, ErrKind::ModelNotFound);
    }

    #[test]
    fn upstream_error_display_matches_go_error_error() {
        let e = upstream_error(429, r#"{"code":1113,"msg":"余额不足"}"#);
        assert_eq!(e.to_string(), "HTTP 429 code=1113 余额不足");
    }

    #[test]
    fn claim_error_upstream_keeps_status_for_the_404_route_not_found_case() {
        // ★ 钉住 task-29 阻塞点：`404 Route Not Found` 的响应体里**没有** code，
        // 调度器只能靠 `status` 判 `ClaimFailNotFound`（Go `claim_scheduler.go:507-517`）。
        // 若 `ClaimError` 把 `UpstreamError` 擦成 String，这条信息就永远丢了。
        let e = ClaimError::Upstream(upstream_error(
            404,
            r#"{"error_msg":"404 Route Not Found"}"#,
        ));
        assert_eq!(e.status(), 404);
        assert!(e.code().is_empty(), "code 应为空：{}", e.code());
        // 「没有 code」不等于「认证失败」—— 别让兜底分类把它拐到 LoginRequired。
        match &e {
            ClaimError::Upstream(u) => assert_ne!(u.kind, ErrKind::AuthFailed),
            _ => unreachable!(),
        }
        // 文案也要与 Go 的 `Error.Error()` 一致（无 code 时只有 HTTP 段）。
        assert_eq!(e.to_string(), "HTTP 404");
    }

    #[test]
    fn claim_error_local_is_distinguishable_from_upstream() {
        let e = ClaimError::Local("套餐 ID 不能为空".to_string());
        assert_eq!(e.status(), 0);
        assert!(e.code().is_empty());
        assert!(!e.is_captcha_required());
        assert_eq!(e.to_string(), "套餐 ID 不能为空");
    }

    #[test]
    fn claim_error_detects_captcha_required_from_kind_and_code() {
        // 判据看**两处**：`kind`（分类器给的）与 `code`（业务码字符串）。
        let by_code = ClaimError::Upstream(upstream_error(400, r#"{"code":3007,"msg":"captcha"}"#));
        assert!(by_code.is_captcha_required());
        // HTTP 200 + 业务码也是常态（上游用 200 报错）。
        let by_kind = ClaimError::Upstream(upstream_error(200, r#"{"code":3007,"msg":"captcha"}"#));
        assert!(by_kind.is_captcha_required());
        let other = ClaimError::Upstream(upstream_error(429, r#"{"code":1113,"msg":"余额不足"}"#));
        assert!(!other.is_captcha_required());
    }

    #[test]
    fn kind_of_code_maps_quota_to_no_resource_pack() {
        // ★ 1113 ⇒ NoResourcePack，**不是** QuotaExhausted：
        // 实测同账号 billing/balance 显示 3 亿 token 几乎未用却回 1113，
        // 说明额度与资源包在不同通道上。
        assert_eq!(kind_of_code("1113"), ErrKind::NoResourcePack);
        assert_eq!(kind_of_code("3007"), ErrKind::CaptchaRequired);
        assert_eq!(kind_of_code("1001"), ErrKind::AuthMissing);
        assert_eq!(kind_of_code("1000"), ErrKind::AuthFailed);
        assert_eq!(kind_of_code("3101"), ErrKind::PlanRequired);
        assert_eq!(kind_of_code("2999"), ErrKind::None);
    }
}
