//! Qoder 上游 HTTP 客户端（Go `internal/qoder/client.go` 的移植）。
//!
//! ★ **两条鉴权路径必须分清**（混用会静默失败）：
//!
//! - **Gateway 域**（`gateway.qoder.com.cn` / `api3.qoder.sh`）：模型列表、对话 ——
//!   必须 COSY 签名（见 [`super::cosy`]）。
//! - **OpenAPI 域**（`openapi.qoder.com.cn` / `openapi.qoder.sh`）：额度、用户信息、
//!   令牌刷新 —— 只需 Bearer DT，**不签名**（见 [`super::token`]）。
//!
//! 为什么这里自建 `reqwest::Client`：契约里 [`QoderUpstream`](super::QoderUpstream)
//! 收到的是 [`crate::upstream::client::Client`]，但它的字段全私有、端点也是
//! WorkBuddy 专属（它的 `refresh_token` / `fetch_models` 打的是别的地址），
//! 既拿不到配置也用不上，故 Qoder 的 HTTP 层自己持有客户端。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::cosy::CosySession;
use super::cred::{truncate_str, QoderCred};
use super::encoding::qoder_encode;
use crate::error::GatewayError;

/// 模型清单端点（拼在 Gateway 域之后）。
pub(crate) const MODELS_PATH: &str = "/algo/api/v2/model/list?Encode=1";

/// 对话端点（拼在 Gateway 域之后）。
///
/// 注意这是**嵌套 SSE** 格式（`FetchKeys=llm_model_result`），与标准 OpenAI 流不同，
/// 解析逻辑见 [`super::sse`] / [`super::stream`]。
pub(crate) const CHAT_PATH: &str = "/algo/api/v2/service/pro/sse/agent_chat_generation\
?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1";

/// 建连超时（Go `Transport` 的 `DialContext` 默认口径）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// 短请求总超时（Go `Client.Timeout = 30s`，用于模型清单）。
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
/// 长流：**不设总超时**，只设"每次读"的空闲超时。
///
/// 长回答可能持续数分钟（含推理），总超时会在中途掐断连接 —— 表现为"回答被截断"。
/// Go 侧对应 `Transport.ResponseHeaderTimeout = 60s`（首字节）与无总超时的 client。
const CHAT_READ_TIMEOUT: Duration = Duration::from_secs(60);
/// 模型清单响应体读取上限（Go `io.LimitReader(resp.Body, 4<<20)`）。
const MAX_MODELS_BYTES: usize = 4 << 20;
/// 上游非 2xx 时的错误体读取上限（Go `io.LimitReader(resp.Body, 1<<20)`）。
///
/// `pub(crate)`：`campaign.rs` 等兄弟模块也用同一个上限（每个 OpenAPI 端点
/// 的响应都要限读，否则一个坏上游能把内存吃满）。
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 1 << 20;

/// 模型清单里的一项（Go `DynamicModel`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct DynamicModel {
    #[serde(default)]
    pub(crate) key: String,
    #[serde(default)]
    pub(crate) display_name: String,
    #[serde(default)]
    pub(crate) enable: bool,
    /// 是否为推理模型。Go 侧由 `logincli` / 探针命令读取，网关路径不用。
    #[serde(default)]
    pub(crate) is_reasoning: bool,
    /// 是否为视觉模型。同上。
    #[serde(default)]
    pub(crate) is_vl: bool,
    #[serde(default)]
    pub(crate) max_input_tokens: i64,
    /// 价格系数。同上（Go `logincli` 的 `priceFactor`）。
    #[serde(default)]
    pub(crate) price_factor: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context_config: Option<std::collections::BTreeMap<String, ContextOption>>,
}

/// `context_config` 的一项（Go `ContextOption`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ContextOption {
    #[serde(default)]
    pub(crate) token_count: i64,
    #[serde(default)]
    pub(crate) is_default: bool,
}

impl DynamicModel {
    /// 该模型的最大上下文（Go `DynamicModel.MaxContextTokens`）。
    ///
    /// 取 `context_config` 里 `token_count` 的最大值；都不大于 0 时回落
    /// `max_input_tokens`。网关路径用它填 `model_config.max_input_tokens`
    /// （见 [`super::translate::ModelMeta`]）—— 上游据此判断能不能收下这段上下文，
    /// 缺字段时会回落成官方客户端默认值。
    pub(crate) fn max_context_tokens(&self) -> i64 {
        let mut best = 0i64;
        if let Some(cfg) = &self.context_config {
            for opt in cfg.values() {
                if opt.token_count > best {
                    best = opt.token_count;
                }
            }
        }
        if best > 0 {
            best
        } else {
            self.max_input_tokens
        }
    }
}

/// 对话端点的两种结果。
///
/// 与 Go 的 `(io.ReadCloser, int, []byte, error)` 对应：上游非 2xx 时 Go 返回
/// **err 为 nil**、由调用方按状态码分类，故这里也把"上游拒绝"表达成正常返回值。
pub(crate) enum ChatCall {
    /// 上游 2xx：**尚未翻译**的嵌套 SSE 响应体。
    Stream(reqwest::Response),
    /// 上游非 2xx：状态码 + 错误体（已截断到 1MB）。
    Rejected { status: u16, body: String },
}

/// HTTP 层错误：区分"传输层失败"与"其它"。
///
/// 分开是为了让调用方能按仓库口径上报（传输层失败用
/// [`GatewayError::Transport`]，配置/签名/解析类用 [`GatewayError::Other`]）。
#[derive(Debug)]
pub(crate) enum QoderHttpError {
    /// 连接、超时、读流中断。
    Transport(String),
    /// 缺令牌、签名失败、响应解析失败等。
    Other(String),
}

impl QoderHttpError {
    /// 转成网关错误（传输层失败会被 forward 记成可轮转的传输错误）。
    pub(crate) fn into_gateway(self) -> GatewayError {
        match self {
            Self::Transport(m) => GatewayError::Transport(m),
            Self::Other(m) => GatewayError::Other(m),
        }
    }
}

/// Qoder 的 HTTP 层：两个 `reqwest::Client`。
///
/// - `rpc`：短请求（模型清单、令牌刷新），带 30s 总超时。
/// - `chat`：长流，**无总超时**，只有每次读的空闲超时。
///
/// 两者都**禁 HTTP/2**、**真直连**，理由见 [`base_builder`]。
pub(crate) struct QoderClient {
    rpc: reqwest::Client,
    chat: reqwest::Client,
}

impl QoderClient {
    /// 构造。`reqwest` 的 `build()` 只在 TLS 后端初始化失败时报错 —— 那是进程级
    /// 故障（Go 侧 `New()` 不会失败，但也没有等价的可恢复路径），故直接 panic
    /// 并给出可读的说明，而不是静默降级成"每个请求都传输失败"。
    pub(crate) fn new() -> Self {
        Self {
            rpc: build(base_builder().timeout(RPC_TIMEOUT)),
            chat: build(base_builder().read_timeout(CHAT_READ_TIMEOUT)),
        }
    }

    /// 构造带代理的客户端（Go `SetProxy`）。
    ///
    /// `proxy` 为 `None` 或全空白时等价于 [`Self::new`]（真直连）。地址无法解析时
    /// 返回 `Err`，由调用方决定是否退回直连 —— Go `newCLI` 的做法是打印一行提示后
    /// 直连，而不是让整个命令失败。
    ///
    /// 实现上复用 [`base_builder`]：它先 `no_proxy()` 清空匹配器（含系统代理），
    /// 再 `proxy()` 推入我们要的那一个，最终语义恰好是"只走这个代理"。
    ///
    /// ★ 只换传输层，**不动超时**：`rpc` 仍是 30s 总超时（Go 侧每个请求另有
    /// `context.WithTimeout(ctx, c.timeout())`，与 `http.Client.Timeout` 无关）。
    pub(crate) fn new_with_proxy(proxy: Option<&str>) -> Result<Self, String> {
        let raw = proxy.unwrap_or("").trim();
        if raw.is_empty() {
            return Ok(Self::new());
        }
        let p = reqwest::Proxy::all(raw).map_err(|e| format!("解析代理地址失败: {e}"))?;
        Ok(Self {
            rpc: build(base_builder().proxy(p.clone()).timeout(RPC_TIMEOUT)),
            chat: build(base_builder().proxy(p).read_timeout(CHAT_READ_TIMEOUT)),
        })
    }

    /// 短请求客户端（模型清单、令牌刷新、额度、用户信息）。
    pub(crate) fn rpc(&self) -> &reqwest::Client {
        &self.rpc
    }

    /// 按宿主配置决定是否走代理（对应 Go `cmd/server/main.go` 的 qoder 接线）。
    ///
    /// # 为什么 Qoder 也要走代理
    ///
    /// 国际版打的是 `api3.qoder.sh` / `openapi.qoder.sh`（`.sh` 域）——**国际流量**。
    /// 实测（本仓库 `auth.rs` 的注释）：`openapi.qoder.sh` 直连 **10.3 s**，
    /// 走代理 **1.2 s**（慢 8.7 倍）。更糟的是直连经常直接超时，表现为
    /// 「qoder 一会儿能用一会儿不能用」。
    ///
    /// # ★★ 这是此前真实存在的缺陷
    ///
    /// [`super::mod::QoderUpstream::new`] 用的是 [`Self::new`]（**真直连**），
    /// 而 [`Self::new_with_proxy`] 只在 `qoder-login` 登录流程里被用到 ⇒
    /// **网关的对话与模型清单从来没走过代理**。于是 `proxy_scope.intl = true`
    /// 对 Qoder 完全无效：用户在界面上打开了国际版代理，qoder 通道照样直连。
    ///
    /// # 判据为什么只看 `proxy_scope.intl`
    ///
    /// 与 ZCode 同款（`zcode.z.ai` 同理）：**产品级**判据，不看单个账号的
    /// `region`。理由是一个进程只有一对 `reqwest::Client`，而两个区域的账号
    /// 共用它们 —— 按账号区域切换代理需要每账号一个 client，代价远大于收益。
    /// 国际版是慢的那一侧，故以它为准。
    ///
    /// 设置失败只打日志、不阻断启动（失败后保持直连）—— 与 ZCode 一致。
    pub(crate) fn apply_host_proxy(&mut self) {
        let cfg = crate::products::zcode::client::host_config();
        let addr = cfg.proxy.trim();
        if !addr.is_empty() && cfg.proxy_intl {
            match Self::new_with_proxy(Some(addr)) {
                Ok(c) => {
                    *self = c;
                    crate::logline!("Qoder 走代理：{addr}");
                }
                Err(e) => crate::logline!("⚠ Qoder 代理设置失败（将直连）：{e}"),
            }
        }
        // 不满足条件时保持 [`Self::new`] 的真直连 —— `base_builder()` 里的
        // `no_proxy()` 已经清掉了环境代理，故不需要显式 `SetProxy("")`。
    }

    /// 拉取模型清单（需 COSY 签名）。
    ///
    /// 签名细节：GET 请求的 body 用**空串**（不是 `"{}"` —— 那会 403 Signature
    /// invalid，Go 侧的注释专门警告过这一点）。
    ///
    /// 指纹在这里**只补内存、不落盘**：调用方负责持久化，避免一次查询就触发磁盘写入。
    pub(crate) async fn fetch_models(&self, cr: &mut QoderCred) -> Result<Vec<DynamicModel>, String> {
        if cr.dt.is_empty() {
            return Err("账号没有可用令牌".to_string());
        }
        cr.ensure_fingerprint();
        let raw_url = format!("{}{}", cr.region.gateway(), MODELS_PATH);
        let sess = CosySession::new(cr, &cr.dt, &cr.drt)?;
        let mut headers = reqwest::header::HeaderMap::new();
        sess.apply_headers(&mut headers, "", &raw_url, &cr.uid, false, "")?;

        let resp = self
            .rpc
            .get(&raw_url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| format!("拉取模型列表失败: {e}"))?;
        let status = resp.status().as_u16();
        let raw = read_limited(resp, MAX_MODELS_BYTES).await;
        let text = String::from_utf8_lossy(&raw).into_owned();
        if status >= 400 {
            return Err(format!(
                "拉取模型列表失败（HTTP {status}）：{}",
                truncate_str(&text, 200)
            ));
        }
        parse_models(&text)
    }

    /// POST 对话端点。
    ///
    /// ★ 请求体**先编码再签名**：端点 URL 里带 `Encode=1`，签名覆盖的是**实际发送
    /// 的字节**（编码后的字符串）。直接发明文 JSON 会让上游无法解析 —— 表现为
    /// **流挂起**，且没有任何报错。
    pub(crate) async fn chat_stream(
        &self,
        cr: &mut QoderCred,
        body: &[u8],
    ) -> Result<ChatCall, QoderHttpError> {
        if cr.dt.is_empty() {
            return Err(QoderHttpError::Other("账号没有可用令牌".to_string()));
        }
        cr.ensure_fingerprint();
        let raw_url = format!("{}{}", cr.region.gateway(), CHAT_PATH);
        let sess = CosySession::new(cr, &cr.dt, &cr.drt).map_err(QoderHttpError::Other)?;

        let encoded = qoder_encode(body);
        let model_key = model_key_of(body);
        let mut headers = reqwest::header::HeaderMap::new();
        // 第 5 参 sse=true；第 6 参的 model 从**原始明文 body** 取。
        sess.apply_headers(&mut headers, &encoded, &raw_url, &cr.uid, true, &model_key)
            .map_err(QoderHttpError::Other)?;

        let resp = self
            .chat
            .post(&raw_url)
            .headers(headers)
            .body(encoded)
            .send()
            .await
            .map_err(|e| QoderHttpError::Transport(format!("连接上游失败: {e}")))?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let raw = read_limited(resp, MAX_ERROR_BODY_BYTES).await;
            return Ok(ChatCall::Rejected {
                status,
                body: String::from_utf8_lossy(&raw).into_owned(),
            });
        }
        Ok(ChatCall::Stream(resp))
    }

    /// 查询额度（Go `FetchQuota`）。**不签名**，只带 `Bearer DT`。
    ///
    /// ★ 两条路径不能混：这里打的是 OpenAPI 域，不是 Gateway 域。
    ///
    /// ★★ `expiresAt` 上游给的是**毫秒**且用 `253402214400000`（9999 年）表示
    /// "永不过期"，必须归零 —— 否则界面格式化出来是天文数字。
    ///
    /// ⚠ 新账号会返回 `isQuotaExceeded: true` 且 `total: 0`，那是"还没有可用额度"
    /// （如未领取活动额度），不是"用超了"。两个信号都保留，让界面自己判断。
    pub(crate) async fn fetch_quota(&self, cr: &QoderCred) -> Result<Quota, String> {
        let raw = self
            .openapi_get(cr, "/api/v2/quota/usage", "查询额度失败")
            .await?;

        let q: QuotaWire = serde_json::from_str(&raw).map_err(|e| {
            format!(
                "额度响应解析失败: {e}（原始内容：{}）",
                truncate_str(&raw, 160)
            )
        })?;

        // 毫秒 → 秒；超过 2100-01-01 视为"永不过期/未知"。
        let mut expires_at = q.expires_at_ms / 1000;
        if expires_at > 4102444800 {
            expires_at = 0;
        }
        // 套餐名：真实字段是 `userType`；`planTierName` 只在上游真给了值时用
        // （实测响应里根本没有它 —— 别信参考实现）。
        let plan = if q.plan_tier_name.is_empty() {
            q.user_type
        } else {
            q.plan_tier_name
        };

        Ok(Quota {
            remaining: (q.user_quota.remaining + q.add_on_quota.remaining) as i64,
            total: (q.user_quota.total + q.add_on_quota.total) as i64,
            used: q.user_quota.used as i64,
            exceeded: q.is_quota_exceeded,
            plan_tier_name: plan,
            usage_percent: q.total_usage_percentage,
            expires_at,
        })
    }

    /// 查询用户信息（Go `FetchUserInfo`）。**不签名**。用于登录/导入后补齐昵称。
    pub(crate) async fn fetch_user_info(&self, cr: &QoderCred) -> Result<UserInfo, String> {
        let raw = self
            .openapi_get(cr, "/api/v1/userinfo", "查询用户信息失败")
            .await?;
        serde_json::from_str(&raw).map_err(|e| format!("用户信息解析失败: {e}"))
    }

    /// OpenAPI 域的 GET：`Bearer DT` + `Accept` + `User-Agent`，1MB 上限。
    ///
    /// `fail_prefix` 是失败文案前缀（Go 侧两处分别写 `查询额度失败` /
    /// `查询用户信息失败`），所以做成参数而不是常量。
    async fn openapi_get(
        &self,
        cr: &QoderCred,
        path: &str,
        fail_prefix: &str,
    ) -> Result<String, String> {
        if cr.dt.is_empty() {
            return Err("账号没有可用令牌".to_string());
        }
        let url = format!("{}{path}", cr.region.openapi());
        let resp = self
            .rpc
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", cr.dt))
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, "Go-http-client/2.0")
            .send()
            .await
            .map_err(|e| format!("{fail_prefix}: {e}"))?;
        let status = resp.status().as_u16();
        let raw = read_limited(resp, MAX_ERROR_BODY_BYTES).await;
        let text = String::from_utf8_lossy(&raw).into_owned();
        if status >= 400 {
            return Err(format!(
                "{fail_prefix}（HTTP {status}）：{}",
                truncate_str(&text, 200)
            ));
        }
        Ok(text)
    }
}

/// 额度（Go `Quota`）。
///
/// 字段与上游实测响应一一对应 —— 改字段名前请先回去核对那份响应。
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Quota {
    /// 剩余额度（用户套餐 + 附加包）。
    pub(crate) remaining: i64,
    /// 总额度。
    pub(crate) total: i64,
    /// 已用额度。
    pub(crate) used: i64,
    /// 上游是否判定已超额。⚠ 新账号也会是 `true`（`total` 为 0 时）。
    pub(crate) exceeded: bool,
    /// 套餐/账号等级（实测来自 `userType`，如 `personal_standard`）。
    pub(crate) plan_tier_name: String,
    /// 用量百分比（0..1）。
    pub(crate) usage_percent: f64,
    /// 到期时刻（Unix **秒**）；0 = 未知/永不过期。
    pub(crate) expires_at: i64,
}

/// 额度响应的线上结构（Go `FetchQuota` 里的匿名 struct）。
#[derive(Debug, Deserialize)]
struct QuotaWire {
    #[serde(default, rename = "userQuota")]
    user_quota: QuotaPart,
    #[serde(default, rename = "addOnQuota")]
    add_on_quota: AddOnPart,
    #[serde(default, rename = "isQuotaExceeded")]
    is_quota_exceeded: bool,
    #[serde(default, rename = "totalUsagePercentage")]
    total_usage_percentage: f64,
    /// ⚠ **毫秒**时间戳。
    #[serde(default, rename = "expiresAt")]
    expires_at_ms: i64,
    #[serde(default, rename = "userType")]
    user_type: String,
    #[serde(default, rename = "planTierName")]
    plan_tier_name: String,
}

#[derive(Debug, Default, Deserialize)]
struct QuotaPart {
    #[serde(default)]
    total: f64,
    #[serde(default)]
    used: f64,
    #[serde(default)]
    remaining: f64,
}

#[derive(Debug, Default, Deserialize)]
struct AddOnPart {
    #[serde(default)]
    total: f64,
    #[serde(default)]
    remaining: f64,
}

/// 用户信息（Go `UserInfo`）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub(crate) struct UserInfo {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) username: String,
}

/// 公共的 client 构造参数。
///
/// - `http1_only()`：Go 侧把 `TLSNextProto` 置空 = **禁 HTTP/2**，否则流式响应会被
///   上游以 `stream INTERNAL_ERROR` 中断。
/// - `no_proxy()`：Go 的 Transport 没设 `Proxy`（= 真直连）；`reqwest` 默认会读环境
///   变量代理，必须显式关掉才等价。
fn base_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .http1_only()
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_max_idle_per_host(20)
        .pool_idle_timeout(Duration::from_secs(90))
}

fn build(builder: reqwest::ClientBuilder) -> reqwest::Client {
    builder
        .build()
        .expect("构造 Qoder HTTP 客户端失败（TLS 后端初始化异常）")
}

/// 按上限读响应体。
///
/// 读错误按 Go 的 `raw, _ := io.ReadAll(...)` 口径**忽略**（截断后的原文照样用于
/// 报错文案）。超出上限的部分丢弃，不会继续拉取。
pub(crate) async fn read_limited(mut resp: reqwest::Response, limit: usize) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        if out.len() >= limit {
            break;
        }
        let room = limit - out.len();
        if chunk.len() > room {
            out.extend_from_slice(&chunk[..room]);
            break;
        }
        out.extend_from_slice(&chunk);
    }
    out
}

/// 解析模型清单响应（Go `FetchModels` 的解析段）。
fn parse_models(raw: &str) -> Result<Vec<DynamicModel>, String> {
    let api: Map<String, Value> = serde_json::from_str(raw).map_err(|e| {
        format!(
            "模型列表解析失败: {e}（原始内容：{}）",
            truncate_str(raw, 160)
        )
    })?;
    let Some(chat) = api.get("chat") else {
        return Err(format!(
            "模型列表响应里没有 chat 场景（可用键：{}）",
            keys_of(&api)
        ));
    };
    let models: Vec<DynamicModel> =
        serde_json::from_value(chat.clone()).map_err(|e| format!("chat 场景解析失败: {e}"))?;
    let total = models.len();
    let enabled: Vec<DynamicModel> = models
        .into_iter()
        .filter(|m| m.enable && !m.key.is_empty())
        .collect();
    if enabled.is_empty() {
        // ★ 这里计的是**解析总数**，不是过滤后数量（Go 逐字）。
        return Err(format!(
            "上游没有返回任何启用的模型（共解析 {total} 个，全部被禁用）"
        ));
    }
    Ok(enabled)
}

/// 列出顶层键（Go `keysOf`）。
///
/// Go 侧是 map 遍历顺序（不稳定）；Rust 的 `serde_json::Map` 是 `BTreeMap`，
/// 天然有序 —— 单测不要假设 Go 的顺序。
fn keys_of(api: &Map<String, Value>) -> String {
    api.keys().cloned().collect::<Vec<_>>().join(", ")
}

/// 从请求体里取 `model`（Go `modelKeyOf`）。解析失败或类型不对时返回空串。
pub(crate) fn model_key_of(body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(default)]
        model: String,
    }
    serde_json::from_slice::<Probe>(body)
        .map(|p| p.model)
        .unwrap_or_default()
}

/// 归一化模型名（Go `NormalizeModelName`，逐字移植）。
///
/// 两侧都要归一化才能匹配上：只把客户端那侧 `ToLower` 的话，用户写
/// `DeepSeek_Flash` / `deepseek flash` / 两边带空格 的变体**全都匹配不上**，
/// 于是把**本来可用**的模型判成"上游没有"而拒绝。
pub(crate) fn normalize_model_name(s: &str) -> String {
    let s = s.trim().to_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        match c {
            'a'..='z' | '0'..='9' | '.' => {
                out.push(c);
                prev_dash = false;
            }
            ' ' | '_' | '-' => {
                if !prev_dash && !out.is_empty() {
                    out.push('-');
                    prev_dash = true;
                }
            }
            // 其它字符（中文等）原样保留：上游确有中文展示名的模型。
            _ => {
                out.push(c);
                prev_dash = false;
            }
        }
    }
    out.trim_matches('-').to_string()
}

/// 现算"归一化名 → 上游 key"映射（Go `ResolveModelMap`）。
///
/// ★ 网关路径**不维护**这张表（见 [`super::dispatch`] 的说明：快速路径若直接查表
/// 命中就 return，同一个假模型会"第一次被拦、之后 10 分钟内全部放行"）。
/// Go 侧只有探针命令用它，保留以对齐移植面。
#[allow(dead_code)]
pub(crate) fn resolve_model_map(
    models: &[DynamicModel],
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for m in models {
        let name = if m.display_name.is_empty() {
            m.key.clone()
        } else {
            normalize_model_name(&m.display_name)
        };
        out.insert(name, m.key.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_model_name_matches_the_go_table() {
        // 逐字来自 Go `cred_test.go:553-567 TestNormalizeModelName`。
        let cases = [
            ("Qwen3.8-Max-Preview", "qwen3.8-max-preview"),
            ("DeepSeek-V4-Pro", "deepseek-v4-pro"),
            ("GLM-5.2", "glm-5.2"),
            ("  Spaces  Around  ", "spaces-around"),
            ("Under_Score", "under-score"),
            ("Double--Dash", "double-dash"),
            ("-Leading", "leading"),
            ("Trailing-", "trailing"),
        ];
        for (input, want) in cases {
            assert_eq!(normalize_model_name(input), want, "input={input:?}");
        }
    }

    #[test]
    fn normalize_model_name_keeps_cjk_and_dots() {
        assert_eq!(normalize_model_name("  Qwen 3.8 旗舰版 "), "qwen-3.8-旗舰版");
        assert_eq!(normalize_model_name(""), "");
        assert_eq!(normalize_model_name("---"), "");
    }

    #[test]
    fn model_key_of_tolerates_garbage() {
        assert_eq!(model_key_of(br#"{"model":"qwen3.8-flash"}"#), "qwen3.8-flash");
        assert_eq!(model_key_of(br#"{"messages":[]}"#), "");
        assert_eq!(model_key_of(b"not json"), "");
        // 类型不对时 Go 的 json.Unmarshal 报错并返回空串。
        assert_eq!(model_key_of(br#"{"model":123}"#), "");
    }

    #[test]
    fn parse_models_keeps_only_enabled_with_key() {
        let raw = r#"{"chat":[{"key":"a","enable":true},{"key":"b","enable":false},
                      {"key":"","enable":true},{"enable":true}]}"#;
        let got = parse_models(raw).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "a");
    }

    #[test]
    fn parse_models_reports_parsed_total_when_everything_is_disabled() {
        let raw = r#"{"chat":[{"key":"a","enable":false},{"key":"b","enable":false}]}"#;
        let err = parse_models(raw).unwrap_err();
        assert!(err.contains("共解析 2 个"), "{err}");
    }

    #[test]
    fn parse_models_reports_missing_chat_scene_with_available_keys() {
        let err = parse_models(r#"{"image":[{}]}"#).unwrap_err();
        assert!(err.contains("没有 chat 场景"), "{err}");
        assert!(err.contains("image"), "{err}");
    }

    #[test]
    fn parse_models_reports_broken_json_with_excerpt() {
        let err = parse_models("not json at all").unwrap_err();
        assert!(err.starts_with("模型列表解析失败"), "{err}");
        assert!(err.contains("not json at all"), "{err}");
    }

    #[test]
    fn max_context_tokens_prefers_largest_option_then_falls_back() {
        let raw = r#"{"key":"k","max_input_tokens":100,"context_config":{
            "a":{"token_count":200,"is_default":false},
            "b":{"token_count":300,"is_default":true}}}"#;
        let m: DynamicModel = serde_json::from_str(raw).unwrap();
        assert_eq!(m.max_context_tokens(), 300);

        let m2: DynamicModel =
            serde_json::from_str(r#"{"key":"k","max_input_tokens":100}"#).unwrap();
        assert_eq!(m2.max_context_tokens(), 100);

        // 全为 0（或负）时不取 0 之外的怪值，回落 max_input_tokens。
        let m3: DynamicModel = serde_json::from_str(
            r#"{"key":"k","max_input_tokens":7,"context_config":{"a":{"token_count":0}}}"#,
        )
        .unwrap();
        assert_eq!(m3.max_context_tokens(), 7);
    }

    #[test]
    fn dynamic_model_tolerates_missing_and_null_fields() {
        let m: DynamicModel = serde_json::from_str(r#"{"key":"k","context_config":null}"#).unwrap();
        assert_eq!(m.key, "k");
        assert!(!m.enable);
        assert_eq!(m.price_factor, 0.0);
        assert!(m.context_config.is_none());
    }

    #[test]
    fn resolve_model_map_prefers_normalized_display_name() {
        let models = vec![
            DynamicModel {
                key: "up-1".into(),
                display_name: "Qwen3.8-Flash".into(),
                enable: true,
                ..Default::default()
            },
            DynamicModel {
                key: "up-2".into(),
                ..Default::default()
            },
        ];
        let map = resolve_model_map(&models);
        assert_eq!(map.get("qwen3.8-flash").map(String::as_str), Some("up-1"));
        assert_eq!(map.get("up-2").map(String::as_str), Some("up-2"));
    }

    #[test]
    fn keys_of_is_sorted_because_rust_map_is_a_btree() {
        let api: Map<String, Value> = serde_json::from_str(r#"{"chat":[],"image":[]}"#).unwrap();
        assert_eq!(keys_of(&api), "chat, image");
    }
}
