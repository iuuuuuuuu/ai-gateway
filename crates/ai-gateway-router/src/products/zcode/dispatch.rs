//! ZCode 调度层：凭证桥接、额度预检、模型名规范化。
//!
//! 对应 Go `internal/zcode/dispatch.go`（455 行）。
//!
//! ## 与 Qoder 的差异（Go 文件头的对照表）
//!
//! | | Qoder | ZCode |
//! |---|---|---|
//! | 请求体 | 私有格式 + 编码 | **OpenAI 原样透传** |
//! | 响应形状 | 嵌套 SSE（需翻译） | **标准 OpenAI（零翻译）** |
//! | 签名 | COSY（必需） | 无（实测不需要） |
//!
//! ⇒ 本层只做三件事：**取凭证**、**额度预检**、**模型名规范化**，然后把请求转给
//! [`super::client::Client`]。响应原样返回（唯一的例外见 `client.rs` 的 3007 处理）。

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use super::client::{self, ChatCall, Client, Quota};
use super::cred::{self, ZcodeCred};
use super::provider::{self, Provider};
use crate::auth::Auth;
use crate::error::GatewayError;

/// 额度预检的缓存时长。
///
/// 额度查询是**控制面**请求，每次对话都打会增加一次网络往返；而上游的额度状态
/// 是低频变化的。2 分钟是「用户刷新页面能看到新值」与「不给控制面添压力」的折中。
const QUOTA_PROBE_TTL: Duration = Duration::from_secs(2 * 60);

/// 模型清单的缓存时长。
///
/// 模型清单来自**免认证**的 config 端点，每次请求都拉会凭空多一次网络往返；
/// 而上游改模型是低频事件。10 分钟足够，同时保证「上游新增模型」最多 10 分钟内可见。
const MODEL_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

/// 一次额度预检的结果（含失败原因，故可缓存失败）。
#[derive(Debug, Clone)]
struct QuotaProbe {
    checked_at: Instant,
    /// 判定为「不可用」时的用户可读原因；`None` = 放行。
    err: Option<String>,
}

/// 某个服务商的模型名映射缓存。
///
/// `keys` = 客户端模型名（小写）→ 上游模型 id。`None` 表示「拉取失败，别用缓存」。
#[derive(Debug, Clone, Default)]
struct ModelCache {
    keys: Option<HashMap<String, String>>,
    fetched: Option<Instant>,
}

/// ZCode 调度器。
pub struct Dispatch {
    client: Client,
    /// 额度预检缓存。★ **只对 JWT 账号启用**（纯 API key 账号没有可靠的额度查询凭证）。
    quota_cache: Mutex<HashMap<String, QuotaProbe>>,
    /// 模型清单缓存，**按服务商分别缓存**。
    model_map: RwLock<HashMap<Provider, ModelCache>>,
}

impl Dispatch {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            quota_cache: Mutex::new(HashMap::new()),
            model_map: RwLock::new(HashMap::new()),
        }
    }

    /// 只读访问底层客户端（诊断用；暂无生产调用方）。
    #[allow(dead_code)]
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// 发起一次对话。对应 Go `ChatStream`（`dispatch.go:126-161`）。
    ///
    /// 流程 = 取客户端模型名 → 校验/规范化 → 透传给上游。
    /// ⚠ 与 Qoder 不同，这里**不需要翻译请求体**：上游的 Anthropic 端点收的就是
    /// 我们构造的 Anthropic 格式，而 `client` 内部会完成 OpenAI → Anthropic 的转换。
    pub async fn chat_stream(
        &self,
        a: &Auth,
        openai_body: &[u8],
    ) -> Result<ChatCall, GatewayError> {
        let Some(cr) = cred_of(a) else {
            return Err(GatewayError::Other(
                "账号缺少 ZCode 凭证（Product=zcode 但凭证为空）".to_string(),
            ));
        };

        // ① 额度预检。★ 明确查到过期/耗尽才拦截；控制面查询失败**不阻断对话**。
        //    Rust 侧返回 `QuotaExhausted`（`forward.rs:725-749` 会映射成 402 +
        //    `FailureKind::HardCredit`，且**不轮转**账号）—— 与 Go 的
        //    `QuotaUnavailableError`（server 层用 `QuotaExhausted() bool` 小接口识别）
        //    语义等价，而且不需要 server 反向依赖本包。
        if let Some(reason) = self.preflight_quota(&cr, &client::model_of(openai_body)).await {
            return Err(GatewayError::QuotaExhausted(reason));
        }

        // ② 模型名规范化（大小写映射）。映射不到时**回退原样**。
        let body = self.normalize_model(&cr, openai_body).await;

        // ③ 转发。
        match self.client.stream_chat(&cr, &body).await {
            Ok(call) => Ok(call),
            Err(e) => {
                // ★ 必须打日志。这个错误此前**完全不可见**：server 层把它归成
                // 「无法连接上游（网络超时 / 连接被拒）」，把排查方向引向网络与代理，
                // 而真正原因（翻译失败 / 凭证字段缺失 / 代理地址非法）只存在于这个
                // err 里。2026-09-21 定位「网关恒 503」时，正是这条日志把
                // `invalid character 'm'` 暴露出来；没有它只能靠猜。
                eprintln!(
                    "zcode dispatch: ChatStream 失败 uid={} provider={}: {}{}",
                    super::mask_uid(&cr.uid),
                    cr.provider.as_str(),
                    e,
                    malformed_body_note(&body)
                );
                Err(e)
            }
        }
    }

    /// 非流式聚合。对应 Go `Aggregate`（`dispatch.go:163-170`）。
    ///
    /// ZCode 的非流式响应也是标准 OpenAI 格式，本可复用网关既有的聚合器；
    /// 但 `ProductUpstream` 接口要求本方法**自洽**（不能反向依赖 `upstream` 包），
    /// 故这里调用本包自己的等价实现。
    pub fn aggregate(
        &self,
        sse: &str,
        model: &str,
    ) -> Result<Map<String, Value>, String> {
        client::aggregate_openai(sse, model)
    }

    /// 额度预检。对应 Go `preflightQuota`（`dispatch.go:297-338`）。
    ///
    /// ## 为什么要预检
    ///
    /// **ZCode 过期套餐的对话端点可能返回 HTTP 200 但长期不产出任何帧** ——
    /// 这会让用户一直等到客户端超时，而不是立刻看到「套餐过期了」。
    /// 额度查询是控制面，响应明确且可缓存，因此只对 JWT 账号每 2 分钟预检一次；
    /// API key-only 账号保持原路径（没有可靠的查询凭证）。
    ///
    /// ## ★ 控制面查询失败**不应阻断对话**
    ///
    /// 网络抖动 / JWT 暂时不可用时，仍让上游对话接口自己给出权威结果。
    /// 只有**明确查到**过期/耗尽才拦截。三个条件是**有序 else-if**（互斥）：
    ///
    /// 1. 套餐已过期（`plan_expiry <= now`）
    /// 2. 总额度 > 0 且剩余 <= 0（耗尽）
    /// 3. 既没有余额项也没有套餐、且总额度为 0（没有可用资源包）
    async fn preflight_quota(&self, cr: &ZcodeCred, model: &str) -> Option<String> {
        if cr.jwt.trim().is_empty() {
            return None;
        }
        let key = format!("{}|{}", cr.uid, model.trim().to_lowercase());
        let now = Instant::now();
        if let Ok(cache) = self.quota_cache.lock() {
            if let Some(cached) = cache.get(&key) {
                if now.duration_since(cached.checked_at) < QUOTA_PROBE_TTL {
                    return cached.err.clone();
                }
            }
        }

        // ★ 查询可能补写 deviceMid，故这里用副本 —— 池里的凭证不该被这条路径改动。
        let mut probe_cr = cr.clone();
        let fetched: Result<Quota, String> = self.client.fetch_quota(&mut probe_cr).await;

        let mut probe = QuotaProbe {
            checked_at: now,
            err: None,
        };
        if let Ok(q) = fetched {
            let expires_at = q.plan_expiry();
            if expires_at > 0 && expires_at <= now_unix() {
                probe.err = Some(format!(
                    "ZCode 账号额度套餐已过期（模型 {model}），请刷新额度、重新登录或更换 ZCode 账号"
                ));
            } else if q.total > 0 && q.remaining <= 0 {
                probe.err = Some(format!(
                    "ZCode 账号额度已耗尽（模型 {model}），请更换 ZCode 账号或等待额度重置"
                ));
            } else if q.entries.is_empty() && q.plans.is_empty() && q.total <= 0 {
                probe.err = Some(format!(
                    "ZCode 账号没有可用额度或资源包（模型 {model}），请刷新额度、重新登录或更换 ZCode 账号"
                ));
            }
        }
        let out = probe.err.clone();
        if let Ok(mut cache) = self.quota_cache.lock() {
            cache.insert(key, probe);
        }
        out
    }

    /// 把客户端给的模型名映射成上游认的 id。对应 Go `normalizeModel`（`dispatch.go:204-250`）。
    ///
    /// ## 为什么需要映射（这不是「锦上添花」，是必需的）
    ///
    /// **上游严格区分大小写**，而客户端/内置清单用的是另一种写法。实测
    /// （2026-09-19）用内置清单里的 `GLM-5.3` 发请求，上游回
    /// `400 {"code":11102,"msg":"model [GLM-5.3] service info not found"}`；
    /// 真实可用的 id 是**小写** `glm-5.3`。若不做这层映射，用户看到的是
    /// 「模型不存在」，会以为是上游不支持那个模型，而真正原因是**大小写**。
    ///
    /// ## 映射不到时为什么回退而不是报错
    ///
    /// 上游的模型清单会变（实测当前 11 个，而参考实现硬编码了 11 个别的名字）。
    /// 若严格校验，**上游新增模型时会先失败一段时间**（我们的缓存还没刷新）。
    /// 回退原样让上游去判断，它若真不认识会回明确错误。
    async fn normalize_model(&self, cr: &ZcodeCred, body: &[u8]) -> Vec<u8> {
        let name = client::model_of(body);
        if name.is_empty() {
            // 没指定模型：原样发，让上游用它自己的默认。
            return body.to_vec();
        }
        let cached = self
            .model_map
            .read()
            .ok()
            .and_then(|m| m.get(&cr.provider).cloned());
        let cache = match cached {
            Some(c) if !cache_expired(&c) => c,
            _ => self.refresh_models(cr).await,
        };
        if let Some(keys) = cache.keys {
            let key = name.trim().to_lowercase();
            if let Some(id) = keys.get(&key) {
                if id != &name {
                    return replace_model(body, id);
                }
            }
        }
        body.to_vec()
    }

    /// 刷新某个服务商的模型名映射。对应 Go `refreshModels`（`dispatch.go:252-280`）。
    ///
    /// ★ 失败时返回**空缓存**（调用方回退原样名字）—— 不能因为「拉清单失败」
    /// 就让对话请求也失败：清单只是名字映射的辅助信息。
    async fn refresh_models(&self, cr: &ZcodeCred) -> ModelCache {
        let models = match self.client.fetch_models(Some(cr)).await {
            Ok(ms) => ms,
            Err(e) => {
                eprintln!(
                    "zcode dispatch provider={}: 拉取模型清单失败（将回退原样模型名）: {e}",
                    cr.provider.as_str()
                );
                let empty = ModelCache {
                    keys: None,
                    fetched: Some(Instant::now()),
                };
                if let Ok(mut m) = self.model_map.write() {
                    m.insert(cr.provider, empty.clone());
                }
                return empty;
            }
        };
        let mut keys: HashMap<String, String> = HashMap::with_capacity(models.len() * 4);
        for m in &models {
            // 上游要的 id 是**权威值**（实测小写 glm-5.3）。
            keys.insert(m.id.to_lowercase(), m.id.clone());
            if !m.name.is_empty() {
                // 显示名也建索引：客户端可能用「GLM-5.3」这种展示名请求，
                // 而 API 只认小写 —— 这层映射正是为了消除这个差异。
                // 同时建「空格→连字符」与「去空格」两个变体。
                keys.insert(m.name.trim().to_lowercase(), m.id.clone());
                keys.insert(m.name.replace(' ', "-").to_lowercase(), m.id.clone());
                keys.insert(m.name.replace(' ', "").to_lowercase(), m.id.clone());
            }
        }
        let cache = ModelCache {
            keys: Some(keys),
            fetched: Some(Instant::now()),
        };
        if let Ok(mut m) = self.model_map.write() {
            m.insert(cr.provider, cache.clone());
        }
        cache
    }

    /// 诊断用：当前某服务商的映射键。对应 Go `ModelKeys`（暂无生产调用方）。
    #[allow(dead_code)]
    pub fn model_keys(&self, provider: Provider) -> Vec<String> {
        self.model_map
            .read()
            .ok()
            .and_then(|m| m.get(&provider).and_then(|c| c.keys.clone()))
            .map(|k| k.into_keys().collect())
            .unwrap_or_default()
    }
}

/// 把网关账号池里的 [`Auth`] 桥接成 ZCode 的 [`ZcodeCred`]。
/// 对应 Go `credOf`（`dispatch.go:340-400`）。
///
/// ## 为什么要有这一层
///
/// 网关的账号池统一用 `auth::Auth`（选号逻辑要跨产品共用），而 ZCode 的调用需要
/// `Provider` 与 `JWT`。桥接放在这一层。**服务商的判定**优先用 `Auth::domain`
/// （加载时由 `to_auth()` 写入），它承载了服务商信息。
///
/// ## ★★ 三项**必须**从凭证文件回读，不能省（2026-09-21 实测缺陷）
///
/// **症状**：同一个账号、同一个模型，独立探针（直接 `load_dir` 得到完整
/// `ZcodeCred`）回 **HTTP 200**，而走网关恒回 `503 no_healthy_account` /
/// 「无法连接上游（网络超时）」。
///
/// **根因**：`auth::Auth` 只承载选号需要的字段（uid / 凭证串 / 产品 / 域名），
/// 而 `apply_headers` 的鉴权是「JWT 优先，无 JWT 才回落 credential」。本函数此前
/// 只填了 `credential`、没填 `jwt`，于是网关永远回落到 `{apiKey}.{secret}` 形态的
/// credential —— 那是**按量计费通道**的凭证，拿去打 start-plan 通道（Anthropic
/// Messages）必然被拒。而 `is_start_plan()` 同样只看 JWT，于是**端点也会选错**。
///
/// **三字段的用途**：
/// - `jwt`：鉴权头 + 端点选择（`is_start_plan`），缺它必失败
/// - `device_mid`：`metadata.user_id` 的 device_id（与 `x-device-mid` 同源）
/// - `account_id`：`x-session-id` / trace-id 的稳定标识来源
///
/// **为什么不改 `Auth` 的字段集**：`Auth` 的字段集是**跨产品共用**的，给 ZCode 单独
/// 加三个字段会污染其它产品；而凭证文件就在 `file_path` 上，回读是唯一不破坏
/// 抽象的做法。
///
/// 读不到文件（凭证被删 / 路径失效）时保持零值，行为与修复前一致（由 `apply_headers`
/// 的 credential 回落兜底），**不 panic 也不报错**。
pub fn cred_of(a: &Auth) -> Option<ZcodeCred> {
    if a.access_token.is_empty() {
        return None;
    }
    let mut provider = provider::provider_of_domain(&a.domain);
    if provider == Provider::Unknown {
        // 域名认不出服务商时按 Z.AI（与 `endpoints()` 的默认行为一致）。
        provider = Provider::Zai;
    }
    let mut c = ZcodeCred {
        uid: a.uid.clone(),
        nickname: a.nickname.clone(),
        credential: a.access_token.clone(),
        provider,
        file_path: a.file_path.clone(),
        no_route: a.no_route,
        ..Default::default()
    };
    if let Ok(full) = cred::load_file(&a.file_path) {
        c.jwt = full.jwt;
        c.jwt_issued_at = full.jwt_issued_at;
        c.device_mid = full.device_mid;
        c.account_id = full.account_id;
        c.captcha_param = full.captcha_param;
        c.captcha_region = full.captcha_region;
    }
    Some(c)
}

/// 只替换请求体里的 `model` 字段，其余字节**完全不动**。
/// 对应 Go `replaceModel`（`dispatch.go:433-455`）。
///
/// ## 为什么要逐字段重建而不是 unmarshal/marshal 整个对象
///
/// 后者会**丢失未知字段**（客户端可能带了上游支持但我们不认识的字段），
/// 也会改变数字的表示（如 `1.0` → `1`）与键的顺序。Go 用 `json.RawMessage`
/// 只替换那一个值。
///
/// ## ⚠ 已知且接受的偏差
///
/// `serde_json` 未开 `preserve_order`（Cargo.toml 不可改）⇒ `Map` 是 BTreeMap，
/// 重建后**键序会变成字典序**。Go 的 `RawMessage` 方案保序，Rust 做不到。
/// 语义无影响（JSON 对象无序），但字节层面不同 —— 这一点已列入给 lead 的报告。
/// 解析/序列化失败时一律返回原 body（不冒险改坏请求体）。
pub fn replace_model(body: &[u8], new_model: &str) -> Vec<u8> {
    let Ok(Value::Object(mut doc)) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    doc.insert(
        "model".to_string(),
        Value::String(new_model.to_string()),
    );
    serde_json::to_vec(&doc).unwrap_or_else(|_| body.to_vec())
}

/// 日志用的截断。对应 Go `truncateForLog`。
pub fn truncate_for_log(b: &[u8], n: usize) -> String {
    let s = String::from_utf8_lossy(b);
    if s.len() <= n {
        return s.into_owned();
    }
    // 按字符边界切，避免把多字节字符切成非法 UTF-8（Go 的 `s[:n]` 会）。
    let cut = floor_char_boundary(&s, n);
    format!("{}…(截断)", &s[..cut])
}

/// 请求体不是合法 JSON 时的附注。对应 Go `malformedBodyNote`。
///
/// ## 为什么只在病态状态才打日志
///
/// 请求体含**用户对话内容**，无条件打进日志等于把用户隐私写进磁盘日志文件 ——
/// 本仓库对这类泄露有明确立场（见 `cred::mask_secret` 的注释：「日志可能被用户
/// 贴到 issue 里」）。
///
/// 但「body 不是合法 JSON」是**病态状态**：正常客户端与我们的改写链都只会产出
/// 合法 JSON，走到这里说明请求体在到达本函数前就被破坏了；那种情况下没有原文就
/// 完全无法定位（实测 `invalid character 'm'` 这个报错本身不透露任何关于
/// 「哪一段坏了」的信息），而此时的 body 也几乎**不可能**是正常的用户对话。
///
/// 返回空串表示「body 正常，无需附注」。
pub fn malformed_body_note(body: &[u8]) -> String {
    if serde_json::from_slice::<Value>(body).is_ok() {
        return String::new();
    }
    format!(
        "\n  ⚠ 请求体不是合法 JSON（这本身即是缺陷线索），前 200 字节：{}",
        truncate_for_log(body, 200)
    )
}

/// 脱敏 uid。日志里不出现完整账号标识。
///
/// 本仓库是**公开仓库**，日志可能被用户贴到 issue 里 —— 只留前 8 位足够定位，
/// 又不足以反查账号。对应 Go `main.go` 的 `maskUID`。
/// （实现放在 [`super::mask_uid`]，这里只做一次转发，避免同一份逻辑两处维护。）
#[cfg(test)]
fn mask_uid(uid: &str) -> String {
    super::mask_uid(uid)
}

fn cache_expired(c: &ModelCache) -> bool {
    match c.fetched {
        Some(t) => t.elapsed() > MODEL_CACHE_TTL,
        None => true,
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{PRODUCT_ZCODE, PRODUCT_WORKBUDDY};
    use serde_json::json;

    fn auth(uid: &str, token: &str, domain: &str) -> Auth {
        Auth {
            uid: uid.to_string(),
            nickname: "nick".to_string(),
            access_token: token.to_string(),
            domain: domain.to_string(),
            file_path: String::new(),
            product: PRODUCT_ZCODE.to_string(),
            ..Default::default()
        }
    }

    /// 一个**必然拒绝连接**的本地端点（刚 bind 就 drop ⇒ 立刻 ECONNREFUSED）。
    ///
    /// ## 为什么必须这样造"控制面不可达"
    ///
    /// `fetch_quota` / `fetch_models` 的生产基址是**常量与端点表**
    ///（`provider::QUOTA_HOST`、`CONFIG_ORIGIN`、`endpoints().openai_base`），
    /// 不受 `anthropic_base_override` 影响 ⇒ 任何"控制面失败"的测试都会**真的打
    /// `zcode.z.ai`**。而反复高频请求已经吃过 **3012 风控**（lead 明确警告过），
    /// 且真实上游可达时"失败"根本不会发生 —— 测试会以 `GLM-5.3` 被成功映射成
    /// 小写 `glm-5.3` 而失败，测的是"这台机器能不能连上 zcode.z.ai"。
    ///
    /// 指向死端口后，「控制面不可达」成为**确定的本地事实**：不触网、不吃风控、
    /// 不依赖网络状况，且仍走**真实代码路径**（连接失败 ⇒ keys 为 None ⇒ 回退原名），
    /// 比注入假缓存更可信。
    fn dead_endpoint() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        drop(l);
        format!("http://127.0.0.1:{port}")
    }

    /// 控制面指向死端点的 Dispatch（不触网）。
    fn offline_dispatch() -> Dispatch {
        let mut c = Client::new();
        c.set_control_base_for_test(&dead_endpoint());
        Dispatch::new(c)
    }

    // ── cred_of ──

    #[test]
    fn cred_of_rejects_empty_credential() {
        assert!(cred_of(&auth("u1", "", "api.z.ai")).is_none());
    }

    #[test]
    fn cred_of_maps_domain_to_provider() {
        let c = cred_of(&auth("u1", "api.key", "open.bigmodel.cn")).unwrap();
        assert_eq!(c.provider, Provider::Bigmodel);
        assert_eq!(c.credential, "api.key");
        assert_eq!(c.uid, "u1");
        assert_eq!(c.nickname, "nick");
    }

    #[test]
    fn cred_of_defaults_unknown_domain_to_zai() {
        // 域名认不出服务商时按 Z.AI（与 endpoints() 的默认行为一致）。
        let c = cred_of(&auth("u1", "api.key", "")).unwrap();
        assert_eq!(c.provider, Provider::Zai);
        let c = cred_of(&auth("u1", "api.key", "example.com")).unwrap();
        assert_eq!(c.provider, Provider::Zai);
    }

    #[test]
    fn cred_of_keeps_zero_values_when_file_unreadable() {
        // 读不到文件**不 panic 也不报错**：由 apply_headers 的 credential 回落兜底。
        let c = cred_of(&auth("u1", "api.key", "api.z.ai")).unwrap();
        assert!(c.jwt.is_empty());
        assert!(c.device_mid.is_empty());
        assert!(c.account_id.is_empty());
        // ★ 没有 JWT ⇒ 端点走服务商通道（不是 start-plan）。
        assert!(!c.is_start_plan());
    }

    #[test]
    fn cred_of_carries_no_route_through() {
        // 池的 healthy() 会因 no_route 排除该账号；漏掉它界面开关就完全不生效。
        let mut a = auth("u1", "api.key", "api.z.ai");
        a.no_route = true;
        assert!(cred_of(&a).unwrap().no_route);
    }

    #[test]
    fn cred_of_ignores_product_field() {
        // 桥接不依赖 product 字段（选路由 forward 层负责），只依赖 domain + 凭证。
        let mut a = auth("u1", "api.key", "api.z.ai");
        a.product = PRODUCT_WORKBUDDY.to_string();
        assert!(cred_of(&a).is_some());
    }

    // ── replace_model ──

    #[test]
    fn replace_model_only_rewrites_the_model_field() {
        let body = br#"{"model":"GLM-5.3","messages":[{"role":"user","content":"hi"}],"unknown_field":{"a":1}}"#;
        let out = replace_model(body, "glm-5.3");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v.get("model").and_then(|m| m.as_str()), Some("glm-5.3"));
        // 未知字段必须保留（丢字段 = 客户端带的参数被静默吞掉）。
        assert_eq!(
            v.pointer("/unknown_field/a").and_then(|x| x.as_u64()),
            Some(1)
        );
        assert_eq!(
            v.pointer("/messages/0/content").and_then(|x| x.as_str()),
            Some("hi")
        );
    }

    #[test]
    fn replace_model_returns_input_on_broken_json() {
        assert_eq!(replace_model(b"not json", "x").to_vec(), b"not json".to_vec());
        // 顶层不是对象 ⇒ 原样返回。
        assert_eq!(replace_model(b"[1,2]", "x").to_vec(), b"[1,2]".to_vec());
    }

    // ── malformed_body_note ──

    #[test]
    fn malformed_body_note_is_empty_for_valid_json() {
        assert!(malformed_body_note(br#"{"model":"x"}"#).is_empty());
        assert!(malformed_body_note(b"null").is_empty());
    }

    #[test]
    fn malformed_body_note_reports_first_bytes_for_broken_json() {
        let note = malformed_body_note(b"model=glm-5.3&messages=x");
        assert!(note.contains("请求体不是合法 JSON"), "{note}");
        assert!(note.contains("model=glm-5.3"), "{note}");
    }

    #[test]
    fn malformed_body_note_truncates_long_bodies() {
        let long = "x".repeat(1000);
        let note = malformed_body_note(long.as_bytes());
        assert!(note.contains("…(截断)"), "应标注截断");
        assert!(note.len() < 400, "只该带前 200 字节：{}", note.len());
    }

    // ── truncate_for_log ──

    #[test]
    fn truncate_for_log_keeps_short_text_intact() {
        assert_eq!(truncate_for_log(b"abc", 10), "abc");
    }

    #[test]
    fn truncate_for_log_never_cuts_multibyte_chars() {
        // ★ 口径是**字节**（Go `s[:n]`），Rust 只额外保证切在字符边界上。
        // 300 个「错」= 900 字节 ⇒ 切到 100 字节只能留 33 个字符
        //（33×3 = 99，再切一个就是 102 > 100）。断言"100 个字符"是把字节当成了字符。
        let s = "错".repeat(300);
        assert_eq!(s.len(), 900, "前提：3 字节/字符");
        let out = truncate_for_log(s.as_bytes(), 100);
        assert!(out.ends_with("…(截断)"));
        let kept = out.strip_suffix("…(截断)").expect("有截断标注");
        assert_eq!(kept.chars().filter(|c| *c == '错').count(), 33);
        assert!(kept.len() <= 100, "截断按字节：留 {} 字节", kept.len());
        assert!(kept.len() > 100 - 3, "不该少切一整个字符：留 {} 字节", kept.len());
        assert!(!kept.contains('\u{fffd}'), "不该出现替换字符");
        // 恰好不超限时**一个字节都不该动**（否则日志里短文本也会被加标注）。
        let exact = "错".repeat(33);
        assert_eq!(truncate_for_log(exact.as_bytes(), 99), exact);
    }

    // ── mask_uid ──

    #[test]
    fn mask_uid_hides_the_tail() {
        assert_eq!(mask_uid("short"), "short");
        assert_eq!(mask_uid("12345678"), "12345678");
        assert_eq!(mask_uid("123456789"), "12345678…");
    }

    // ── 模型缓存 ──

    #[test]
    fn cache_expired_treats_never_fetched_as_expired() {
        assert!(cache_expired(&ModelCache::default()));
        assert!(!cache_expired(&ModelCache {
            keys: Some(HashMap::new()),
            fetched: Some(Instant::now()),
        }));
        assert!(cache_expired(&ModelCache {
            keys: Some(HashMap::new()),
            fetched: Some(Instant::now() - MODEL_CACHE_TTL - Duration::from_secs(1)),
        }));
    }

    #[test]
    fn model_keys_is_empty_before_any_refresh() {
        let d = Dispatch::new(Client::new());
        assert!(d.model_keys(Provider::Zai).is_empty());
    }

    // ── 额度预检（不触网的分支）──

    #[tokio::test]
    async fn preflight_skips_accounts_without_jwt() {
        // 纯 API key 账号没有可靠的额度查询凭证 ⇒ 保持原路径。
        let d = Dispatch::new(Client::new());
        let cr = ZcodeCred {
            uid: "u1".to_string(),
            credential: "api.key".to_string(),
            ..Default::default()
        };
        assert!(d.preflight_quota(&cr, "glm-5.3").await.is_none());
    }

    #[tokio::test]
    async fn preflight_does_not_block_when_control_plane_fails() {
        // ★ 控制面查询失败**不应阻断对话**。控制面指向死端点 ⇒ 失败是确定的本地
        // 事实（见 `dead_endpoint` 的注释：不这样就会真打 zcode.z.ai，吃 3012 风控）。
        let d = offline_dispatch();
        let cr = ZcodeCred {
            uid: "u1".to_string(),
            credential: "api.key".to_string(),
            jwt: "header.payload.sig".to_string(),
            // 用一个必然解析失败的 deviceMid，避免走 mint 分支。
            device_mid: "not-a-uuid".to_string(),
            ..Default::default()
        };
        assert!(
            d.preflight_quota(&cr, "glm-5.3").await.is_none(),
            "查不到额度时必须放行（预检只是优化，不能成为可用性单点）"
        );
        // 且**不得**把"查询失败"缓存成"额度耗尽"：第二次同样放行。
        assert!(d.preflight_quota(&cr, "glm-5.3").await.is_none());
    }

    #[tokio::test]
    async fn normalize_model_passes_through_when_no_model_named() {
        let d = Dispatch::new(Client::new());
        let cr = ZcodeCred {
            uid: "u1".to_string(),
            credential: "api.key".to_string(),
            ..Default::default()
        };
        let body = br#"{"messages":[]}"#;
        assert_eq!(d.normalize_model(&cr, body).await.to_vec(), body.to_vec());
    }

    #[tokio::test]
    async fn normalize_model_falls_back_to_original_on_refresh_failure() {
        // 拉清单失败 ⇒ 回退原样名字（清单只是辅助信息，不能因此让对话失败）。
        // ★ 用死端点让"失败"成为确定事实：否则真实上游可达时 `GLM-5.3` 会被成功
        // 映射成小写 `glm-5.3`（实测的权威 id），测试就变成"这台机器能不能连上
        // zcode.z.ai"，还会吃 3012 风控。
        let d = offline_dispatch();
        let cr = ZcodeCred {
            uid: "u1".to_string(),
            credential: "api.key".to_string(),
            ..Default::default()
        };
        let body = br#"{"model":"GLM-5.3"}"#;
        assert_eq!(d.normalize_model(&cr, body).await.to_vec(), body.to_vec());
        // 失败也**不阻断**后续调用：仍回退原样（缓存里是"已知不可用"，不是 panic）。
        assert_eq!(d.normalize_model(&cr, body).await.to_vec(), body.to_vec());
    }

    // ── aggregate 转发 ──

    #[test]
    fn aggregate_delegates_to_the_openai_aggregator() {
        let d = Dispatch::new(Client::new());
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\ndata: [DONE]\n";
        let out = d.aggregate(sse, "glm-5.3").unwrap();
        assert_eq!(
            out.get("choices")
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
                .and_then(|c| c.pointer("/message/content"))
                .and_then(|c| c.as_str()),
            Some("hi")
        );
    }

    #[test]
    fn aggregate_returns_empty_message_for_empty_stream() {
        let d = Dispatch::new(Client::new());
        let out = d.aggregate("", "glm-5.3").unwrap();
        assert_eq!(out.get("model").and_then(|m| m.as_str()), Some("glm-5.3"));
        assert!(out.get("choices").is_some());
    }

    // ── 额度响应形状（经由 client::assemble_quota 的端到端口径）──

    #[test]
    fn quota_shape_used_by_preflight_matches_upstream() {
        // 预检依赖 plan_expiry / total / remaining / entries / plans，这里钉住契约：
        // 用一份贴近实测的响应体确认这些字段都能取到。
        let doc = json!({
            "code": 0,
            "data": {
                "server_time": 1789781287,
                "balances": [{
                    "show_name": "GLM-5.3",
                    "remaining_units": 3000000,
                    "total_units": 3000000,
                    "used_units": 0,
                    "expires_at": 1790000000,
                    "plan_id": "zcode-v3-start-plan-0817",
                    "entitlement_id": "ent-1"
                }],
                "plans": [{
                    "plan_id": "zcode-v3-start-plan-0817",
                    "name": "ZCode Start Plan",
                    "status": "active",
                    "priority": 1,
                    "ends_at": 1790179199,
                    "entitlements": [{"entitlement_id": "ent-1", "grant_units": 3000000}]
                }]
            }
        });
        let q = client::assemble_quota_for_test(&doc);
        assert_eq!(q.plan_expiry(), 1790179199);
        assert_eq!(q.total, 3000000);
        assert_eq!(q.remaining, 3000000);
        assert_eq!(q.entries.len(), 1);
        assert_eq!(q.plans.len(), 1);
        assert!(q.active_plan().is_some());
    }
}
