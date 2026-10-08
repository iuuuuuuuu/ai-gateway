//! Qoder 派发层（Go `internal/qoder/dispatch.go` 的移植）。
//!
//! 三件事：**模型名校验**、**令牌刷新**、**把上游嵌套 SSE 包装成标准 OpenAI SSE**。
//!
//! # 为什么「查不到就原样发过去」是错的
//!
//! Qoder 上游对**不认识的模型 key 不报错** —— 它静默回退到免费通道，HTTP 200、
//! 正常返回内容。实测：伪造的 `zzz-not-a-real-model-xyz` 与真实模型得到**逐项相同**
//! 的响应画像（usage 帧 `billable=false`、`credits=0`）。后果是用量统计里出现
//! 「`deepseek-v4.1-flash` · Qoder」这类**幽灵数据**。
//!
//! ★ 判据必须是**模型清单**，绝不能用 `billable` / `credits`：
//!
//! - 合法的免费模型（`qfmodel`，即 Qwen3.8-Flash）同样回 `billable=false`、credits 极小
//!   ⇒ 用计费字段判会**误杀**它；
//! - `billable` 只出现在**最后一个** usage 帧里，那时内容早已下发，**来不及撤回**。
//!
//! # 缓存里存的是**完整清单**，不是「名字 → key」映射表
//!
//! 若缓存直接存映射表、快速路径命中就 `return`，同一个假模型会**第一次被拦、
//! 之后 10 分钟内全部放行**，且 `Available` 拿不到 —— 用户看到「有时拦有时不拦」，
//! 极难在单测里发现。故两条路径共用 [`resolve_in_list`]，判定必然一致。
//!
//! # 拿不到清单时**不许拦**
//!
//! 清单拉取失败返回空清单，语义是「我们不知道上游有什么」，**不是**「上游什么都
//! 没有」。拦下去会让一次短暂网络抖动变成「所有模型都不可用」。

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{pin_mut, Stream, StreamExt};

use crate::auth::Auth;
use crate::error::{GatewayError, ModelNotFoundError, Result};
use crate::upstream::client::ChatOutcome;

use super::client::{model_key_of, normalize_model_name, ChatCall, DynamicModel, QoderClient};
use super::cred::QoderCred;
use super::region::{region_from_domain, QoderRegion};
use super::sse::payload_of_line;
use super::stream::QoderStream;
use super::token;
use super::translate::build_agent_body;

/// 模型清单缓存时长。
///
/// 上游加/改模型是低频事件；缓存太久会让新上线的模型在一段时间内不可用。
const MODEL_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

/// 单行 SSE 的上限。
///
/// Go 侧把 `bufio.Scanner` 的缓冲区上限设成 4MB：上游单个分片可能很大（一次性
/// 返回整段长文本时），默认 64KB 会直接报 `token too long` 并**中断整条流**，
/// 而那个错误信息完全不提「缓冲区」，很容易被误判成上游返回了坏数据。
///
/// 与 Go 的**有意偏离**：Go 的超限行为是停止扫描（整条流就此结束），这里改为
/// **丢掉那一行、继续翻译后面的内容** —— 一行超长不该让整段回答消失。
const MAX_SSE_LINE: usize = 4 * 1024 * 1024;

/// 一个区域的模型清单缓存。
struct ModelCache {
    models: Vec<DynamicModel>,
    fetched: Instant,
}

/// Qoder 派发器：持有 HTTP 层与按区域分组的模型清单缓存。
pub(crate) struct Dispatch {
    client: QoderClient,
    /// 按区域分组（`QoderRegion` 实现了 `Hash`）。
    ///
    /// ⚠ 取锁**绝不跨 `.await`**：拉清单是网络调用，必须在发请求前释放锁
    /// （Go 侧同样先释放读锁再发请求）。
    cache: Mutex<HashMap<QoderRegion, ModelCache>>,
}

impl Dispatch {
    pub(crate) fn new() -> Self {
        Self {
            client: QoderClient::new(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 发一次对话（Go `Dispatch.ChatStream`）。
    pub(crate) async fn chat_stream(&self, a: &Auth, openai_body: &[u8]) -> Result<ChatOutcome> {
        let Some(mut cr) = cred_of(a) else {
            return Err(GatewayError::Other(
                "账号缺少 Qoder 凭证（Product=qoder 但凭证为空）".to_string(),
            ));
        };
        // Go 的 modelNameOf 就是 modelKeyOf：从请求体里取 `model`。
        // ⚠ forward 已经剥掉 `qoder:` 前缀并重写过 body，这里拿到的是**裸模型名**。
        let client_model = model_key_of(openai_body);

        let (model_key, models, ok) = self.resolve_model_key(&mut cr, &client_model).await;
        if !ok {
            // ★ 在**发请求之前**拒绝，绝不降级为原样透传。
            crate::logline!(
                "qoder dispatch uid={} region={}: 上游没有模型 {:?}（清单 {} 个），已拒绝（不发请求）",
                a.uid,
                cr.region.label(),
                client_model,
                models.len()
            );
            return Err(GatewayError::ModelNotFound(Box::new(
                ModelNotFoundError::new(client_model, cr.region.label(), available_keys(&models)),
            )));
        }

        let agent_body = build_agent_body(openai_body, &model_key)
            .map_err(|e| GatewayError::Other(format!("构造 Qoder 请求体失败: {e}")))?;

        // ★ 刷新失败**只记日志**，继续用旧令牌：刷新是网络调用，一次抖动不该让
        //   整个请求失败（Go 同款口径）。真正失效时下一步的上游调用会给出错误。
        if token::needs_refresh(&cr, token::REFRESH_SKEW) && !cr.drt.is_empty() {
            if let Err(e) = token::refresh_token(&mut cr, self.client.rpc()).await {
                crate::logline!("qoder dispatch uid={}: 刷新令牌失败，沿用现有令牌: {e}", a.uid);
            }
        }

        match self.client.chat_stream(&mut cr, &agent_body).await {
            // 上游非 2xx：**不是** Err，交给调用方按状态码分类（冷却/轮转策略）。
            Ok(ChatCall::Rejected { status, body }) => Ok(ChatOutcome::Failed { status, body }),
            // ★ 传给流的 model 是**客户端模型名**，不是上游 key：响应里的 `model`
            //   要与用户请求的一致，否则前端会显示成另一个名字。
            Ok(ChatCall::Stream(resp)) => Ok(ChatOutcome::Stream(wrap_stream(resp, client_model))),
            Err(e) => Err(e.into_gateway()),
        }
    }

    /// 解析出上游模型 key（Go `resolveModelKey`）。
    ///
    /// 返回 `(key, 清单, 是否放行)`。
    async fn resolve_model_key(
        &self,
        cr: &mut QoderCred,
        client_model: &str,
    ) -> (String, Vec<DynamicModel>, bool) {
        if client_model.is_empty() {
            let models = self.cached_models(cr).await;
            let (key, ok) = resolve_in_list(&models, client_model);
            return (key, models, ok);
        }

        // ★ 快速路径（缓存命中）**也必须**过校验：曾经直接查映射表命中就 return，
        //   于是同一个假模型第一次被拦、之后 10 分钟内全部放行。
        let cached = {
            let guard = lock(&self.cache);
            match guard.get(&cr.region) {
                Some(c) if c.fetched.elapsed() < MODEL_CACHE_TTL && !c.models.is_empty() => {
                    Some(c.models.clone())
                }
                _ => None,
            }
        };
        let models = match cached {
            Some(m) => m,
            None => self.refresh_models(cr).await,
        };
        let (key, ok) = resolve_in_list(&models, client_model);
        (key, models, ok)
    }

    /// 缓存有效就返回缓存（★ **无 `len > 0` 条件**），否则重新拉。
    async fn cached_models(&self, cr: &mut QoderCred) -> Vec<DynamicModel> {
        {
            let guard = lock(&self.cache);
            if let Some(c) = guard.get(&cr.region) {
                if c.fetched.elapsed() < MODEL_CACHE_TTL {
                    return c.models.clone();
                }
            }
        }
        self.refresh_models(cr).await
    }

    /// 拉取并缓存清单；失败返回**空清单**（= 「本次不校验模型名，原样透传」）。
    async fn refresh_models(&self, cr: &mut QoderCred) -> Vec<DynamicModel> {
        match self.client.fetch_models(cr).await {
            Ok(models) => {
                let mut guard = lock(&self.cache);
                guard.insert(
                    cr.region,
                    ModelCache {
                        models: models.clone(),
                        fetched: Instant::now(),
                    },
                );
                models
            }
            Err(e) => {
                crate::logline!(
                    "qoder dispatch uid={}: 拉取模型清单失败（本次不校验模型名，原样透传）: {e}",
                    cr.uid
                );
                Vec::new()
            }
        }
    }
}

/// 按账号构造 Qoder 凭证（Go `credOf`）。
///
/// 与 Go 的两处差异，都是为了补上 Rust 侧 `Auth` 不承载的字段：
///
/// 1. **机器指纹从凭证文件读回**：`Auth` 刻意不带产品专属字段（见
///    [`QoderCred::to_auth`]），而指纹是 COSY 签名的必需项，故这里重新
///    [`QoderCred::load_file`]。读不到就用默认值，随后由
///    [`QoderCred::ensure_fingerprint`] 就地补上。
/// 2. **令牌以 `Auth` 为准**：账号池是令牌的权威视图（刷新后的写回也在那里），
///    故 `dt`/`drt`/`dt_expires_at` 一律覆盖文件里的值。
///
/// ⚠ 这里**不落盘**（不调 `save_atomic`）：只有令牌刷新才需要写回，而且写回是在
/// [`token::refresh_token`] 内部完成的 —— 一次查询/一次对话不该触发磁盘写入。
///
/// 返回 `None` 表示「Product=qoder 但两个令牌都是空的」：那是凭证加载层的缺陷，
/// 与其发一次注定失败的请求，不如给出指向明确的错误。
pub(crate) fn cred_of(a: &Auth) -> Option<QoderCred> {
    if a.access_token.is_empty() && a.refresh_token.is_empty() {
        return None;
    }
    let mut cr = QoderCred::load_file(&a.file_path).unwrap_or_default();
    cr.uid = a.uid.clone();
    cr.nickname = a.nickname.clone();
    cr.dt = a.access_token.clone();
    cr.drt = a.refresh_token.clone();
    cr.dt_expires_at = a.expires_at;
    cr.no_route = a.no_route;
    cr.file_path = a.file_path.clone();
    // ★ 域名认不出区域时按国服（`endpoints()` 的默认行为），但**不写回**
    //   `Auth.domain` —— 让用户还能在界面上把它改对。
    let region = region_from_domain(&a.domain);
    cr.region = if region == QoderRegion::Unknown {
        QoderRegion::CN
    } else {
        region
    };
    Some(cr)
}

/// 清单里出现过的 key，去重并**排序**。
///
/// 排序是刻意的：错误文案会进日志与界面，若顺序随机，同一条错误每次显示的清单
/// 顺序都不同，看起来像「清单在变」。
fn available_keys(models: &[DynamicModel]) -> Vec<String> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for m in models {
        if m.key.is_empty() {
            continue;
        }
        seen.insert(m.key.as_str());
    }
    seen.into_iter().map(|s| s.to_string()).collect()
}

/// 在清单里解析客户端模型名（Go `resolveInList`，**纯函数**）。
///
/// 五条分支，顺序即优先级：
///
/// 1. 客户端没写模型名 → 取清单第一个；清单空则返回空 key 且**放行**。
/// 2. `qmodel*` 前缀 → **逃生口**：原样透传、**不校验**。上游随时可能上线清单里
///    还没有的 key，而清单有 10 分钟缓存；没有这个口子，新模型在缓存过期前完全
///    不可用。用户明确写 `qmodel*` 即表示「我知道自己在写上游 key」。
/// 3. 与 `key` 比（`ToLower(TrimSpace(client))` vs `ToLower(key)`）。
/// 4. 与 `display_name` 比（**两侧都归一化**）。只 lower 客户端那侧是不够的 ——
///    实测抓到：原来写 `ToLower(client)` 与 `ToLower(NormalizeModelName(display))`
///    比，于是用户写 `DeepSeek_Flash` / `deepseek flash` / 两边带空格的变体
///    **全部匹配不上**，把**本来可用**的模型判成「上游没有」而拒绝。
/// 5. 清单为空 → 原样透传（见模块文档「拿不到清单时不许拦」）；否则拒绝。
pub(crate) fn resolve_in_list(models: &[DynamicModel], client_model: &str) -> (String, bool) {
    if client_model.is_empty() {
        return match models.first() {
            Some(m) => (m.key.clone(), true),
            None => (String::new(), true),
        };
    }
    if client_model.starts_with("qmodel") {
        return (client_model.to_string(), true);
    }
    let lower = client_model.trim().to_lowercase();
    let norm = normalize_model_name(client_model);
    for m in models {
        if m.key.to_lowercase() == lower {
            return (m.key.clone(), true);
        }
        if !m.display_name.is_empty() && normalize_model_name(&m.display_name) == norm {
            return (m.key.clone(), true);
        }
    }
    if models.is_empty() {
        return (client_model.to_string(), true);
    }
    (String::new(), false)
}

/// 把上游的嵌套 SSE 响应体包装成标准 OpenAI SSE 的 [`reqwest::Response`]。
///
/// 调用方（[`crate::forward`]）只会拿它的 `bytes_stream()`，并可能先
/// `probe_first_frame` 嗅探首帧 —— 那部分字节会被原样回放，故这里必须是**纯流式**
/// 翻译，不能缓冲整流（缓冲会把「首字节延迟」变成「整段生成延迟」）。
fn wrap_stream(resp: reqwest::Response, model: String) -> reqwest::Response {
    let upstream = resp.bytes_stream();
    let body = reqwest::Body::wrap_stream(translate_stream(upstream, model));
    reqwest::Response::from(axum::http::Response::new(body))
}

/// 上游字节流 → 标准 OpenAI SSE 字节流。
///
/// 有状态翻译（[`QoderStream`]），逐行喂入。收尾三分支与 Go 的 `openAIStream.fill`
/// 逐字对应：
///
/// - 上游正常结束 / 给了 `data: [DONE]` → [`QoderStream::finish`]（只在没给过
///   `finish_reason` 时补一帧，避免同一条流里出现两次）；
/// - 流内错误（`{"error":...}` 或顶层 `{"code":...}` 信封）或读流失败 →
///   [`QoderStream::error_frame`]，即错误帧 + `[DONE]`。
///
/// ⚠ **不能**让这条流经过 [`crate::upstream::sse::SseFramesIter`]：那个迭代器会按
/// 白名单**重写**每一帧（补 `usage: null`、换 `id`），而 Qoder 的帧由
/// [`QoderStream`] 自己产出，口径不同。
pub(crate) fn translate_stream<S, E>(
    upstream: S,
    model: String,
) -> impl Stream<Item = std::result::Result<Bytes, std::io::Error>>
where
    S: Stream<Item = std::result::Result<Bytes, E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    async_stream::stream! {
        let mut tr = QoderStream::new(&model);
        let mut pending: Vec<u8> = Vec::new();
        // 超长行丢弃模式：丢掉字节直到下一个换行。
        let mut skipping = false;
        // 流内错误 / 读流失败：写一帧错误 + [DONE] 后收尾（**不再**补 finish 帧）。
        let mut fatal: Option<String> = None;
        pin_mut!(upstream);

        'outer: while let Some(item) = upstream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => {
                    fatal = Some(format!("读取流失败: {e}"));
                    break 'outer;
                }
            };
            pending.extend_from_slice(&chunk);
            loop {
                if skipping {
                    match pending.iter().position(|&b| b == b'\n') {
                        Some(p) => {
                            pending.drain(..=p);
                            skipping = false;
                            continue;
                        }
                        None => {
                            pending.clear();
                            break;
                        }
                    }
                }
                let Some(p) = pending.iter().position(|&b| b == b'\n') else {
                    if pending.len() > MAX_SSE_LINE {
                        pending.clear();
                        skipping = true;
                    }
                    break;
                };
                let rest = pending.split_off(p + 1);
                let mut raw = std::mem::replace(&mut pending, rest);
                raw.pop(); // 去掉换行符
                // 只对**完整行**做 UTF-8 转换：跨 chunk 切断的多字节字符此时已拼齐。
                let line = String::from_utf8_lossy(&raw).into_owned();
                match payload_of_line(&line) {
                    // 空行 / 注释（`: heartbeat`）/ 非 data 行 / 空 payload：跳过。
                    None => {}
                    // 上游自己给了 [DONE]：与 Go 的 `io.EOF` 同义，去收尾。
                    Some(None) => break 'outer,
                    Some(Some(payload)) => match tr.push_payload(&payload) {
                        Ok(out) => {
                            if !out.is_empty() {
                                yield Ok::<Bytes, std::io::Error>(Bytes::from(out));
                            }
                        }
                        Err(msg) => {
                            fatal = Some(msg);
                            break 'outer;
                        }
                    },
                }
            }
        }

        // 最后一行可能没有换行符（Go 的 ScanLines 也会返回最后一个非空行）。
        if fatal.is_none() && !skipping && !pending.is_empty() {
            let raw = std::mem::take(&mut pending);
            let line = String::from_utf8_lossy(&raw).into_owned();
            if let Some(Some(payload)) = payload_of_line(&line) {
                match tr.push_payload(&payload) {
                    Ok(out) => {
                        if !out.is_empty() {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(out));
                        }
                    }
                    Err(msg) => fatal = Some(msg),
                }
            }
        }

        match fatal {
            Some(msg) => yield Ok::<Bytes, std::io::Error>(Bytes::from(tr.error_frame(&msg))),
            None => yield Ok::<Bytes, std::io::Error>(Bytes::from(tr.finish())),
        }
    }
}

/// 取锁；中毒（持锁线程 panic）时恢复内容而不是继续 panic。
///
/// 缓存只是性能优化，**没有不变量**需要保护 —— 为一次 panic 让整个网关不可用
/// 得不偿失。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(key: &str, display: &str) -> DynamicModel {
        DynamicModel {
            key: key.to_string(),
            display_name: display.to_string(),
            enable: true,
            ..Default::default()
        }
    }

    fn list() -> Vec<DynamicModel> {
        vec![
            model("qmodel-a", "Qwen3.8-Max-Preview"),
            model("qmodel-b", "DeepSeek-V4-Pro"),
            model("", "空 key 的模型"),
        ]
    }

    #[test]
    fn empty_client_model_takes_the_first_key() {
        assert_eq!(resolve_in_list(&list(), ""), ("qmodel-a".to_string(), true));
        assert_eq!(resolve_in_list(&[], ""), (String::new(), true));
    }

    #[test]
    fn qmodel_prefix_is_a_documented_escape_hatch() {
        // 逃生口刻意**不校验**：新模型可能还没进清单（清单有 10 分钟缓存）。
        assert_eq!(
            resolve_in_list(&list(), "qmodel-not-listed-yet"),
            ("qmodel-not-listed-yet".to_string(), true)
        );
        // ★ 前缀判定**大小写敏感**（Go 用 `strings.HasPrefix`，逐字节比）。
        //   但这不等于「QModel-x 会被拒」—— 它只是**不走逃生口**，随后仍会
        //   落到下面的 key 大小写不敏感匹配。故这里用一个清单里**确实没有**的
        //   名字，才能观察到"没走逃生口"这一事实。
        assert_eq!(
            resolve_in_list(&list(), "QModel-not-listed-yet"),
            (String::new(), false),
            "大写前缀不走逃生口，且清单里查不到 ⇒ 拒绝"
        );
        // 对照：小写前缀即使清单里没有也放行。
        assert_eq!(
            resolve_in_list(&list(), "qmodel-not-listed-yet").1,
            true,
            "小写前缀走逃生口"
        );
    }

    #[test]
    fn exact_key_match_is_case_insensitive_and_trimmed() {
        assert_eq!(resolve_in_list(&list(), "qmodel-b"), ("qmodel-b".to_string(), true));
        assert_eq!(resolve_in_list(&list(), "  QMODEL-B  "), ("qmodel-b".to_string(), true));
    }

    #[test]
    fn display_name_variants_all_resolve_because_both_sides_normalize() {
        // ★ 回归：只归一化客户端一侧会让这些变体全部匹配不上，把可用模型判成
        //   「上游没有」而拒绝。
        for variant in [
            "Qwen3.8-Max-Preview",
            "qwen3.8-max-preview",
            "Qwen3.8_Max_Preview",
            "  Qwen3.8 Max Preview  ",
            "qwen3.8--max--preview",
        ] {
            assert_eq!(
                resolve_in_list(&list(), variant),
                ("qmodel-a".to_string(), true),
                "变体 {variant:?} 应当解析到 qmodel-a"
            );
        }
    }

    #[test]
    fn unknown_model_is_rejected_when_the_list_is_authoritative() {
        // 伪造的模型名：上游会静默回退免费通道，必须在发请求前拦住。
        assert_eq!(
            resolve_in_list(&list(), "zzz-not-a-real-model-xyz"),
            (String::new(), false)
        );
    }

    #[test]
    fn empty_list_passes_through_instead_of_blocking_everything() {
        // 清单拉取失败 ⇒ 空清单 ⇒ 「我们不知道上游有什么」⇒ 不许拦。
        assert_eq!(
            resolve_in_list(&[], "zzz-not-a-real-model-xyz"),
            ("zzz-not-a-real-model-xyz".to_string(), true)
        );
    }

    #[test]
    fn available_keys_dedupes_skips_empty_and_sorts() {
        let mut models = list();
        models.push(model("qmodel-a", "重复 key"));
        assert_eq!(available_keys(&models), vec!["qmodel-a", "qmodel-b"]);
        assert!(available_keys(&[]).is_empty());
    }

    #[test]
    fn cred_of_maps_unknown_region_to_cn_without_touching_the_auth() {
        let a = Auth {
            access_token: "dt-placeholder-token".to_string(),
            refresh_token: "drt-placeholder-token".to_string(),
            expires_at: 1_900_000_000,
            domain: String::new(),
            uid: "00000000-0000-4000-8000-000000000001".to_string(),
            nickname: "占位昵称".to_string(),
            product: crate::auth::PRODUCT_QODER.to_string(),
            no_route: true,
            file_path: String::new(),
            ..Default::default()
        };
        let cr = cred_of(&a).expect("有令牌就该构造出凭证");
        assert_eq!(cr.region, QoderRegion::CN);
        assert_eq!(cr.dt, "dt-placeholder-token");
        assert_eq!(cr.drt, "drt-placeholder-token");
        assert_eq!(cr.dt_expires_at, 1_900_000_000);
        assert!(cr.no_route);
        // ★ 不写回：Auth 是 `&`，本来就改不了 —— 这里顺带钉住契约。
        assert!(a.domain.is_empty());
    }

    #[test]
    fn cred_of_keeps_intl_region_and_defaults_the_rest() {
        let a = Auth {
            access_token: "dt-x".to_string(),
            domain: "qoder.sh".to_string(),
            ..Default::default()
        };
        let cr = cred_of(&a).unwrap();
        assert_eq!(cr.region, QoderRegion::Intl);
        assert!(cr.drt.is_empty());
        // 文件不存在 ⇒ 指纹为空，随后由 ensure_fingerprint 就地补。
        assert!(cr.machine_id.is_empty());
    }

    #[test]
    fn cred_of_returns_none_only_when_both_tokens_are_missing() {
        assert!(cred_of(&Auth::default()).is_none());
        // 只有 drt 也算有凭证：刷新路径会把它换成 dt（Go 同样处理）。
        let only_drt = Auth {
            refresh_token: "drt-only".to_string(),
            ..Default::default()
        };
        assert!(cred_of(&only_drt).is_some());
    }

    /// 收集一条翻译流产生的全部字节。
    async fn drain<S, E>(upstream: S) -> String
    where
        S: Stream<Item = std::result::Result<Bytes, E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        let s = translate_stream(upstream, "Qwen3.8-Max-Preview".to_string());
        pin_mut!(s);
        let mut out: Vec<u8> = Vec::new();
        while let Some(item) = s.next().await {
            out.extend_from_slice(&item.expect("翻译流不该产出 Err"));
        }
        String::from_utf8(out).expect("翻译结果必须是合法 UTF-8")
    }

    fn bytes(s: String) -> std::result::Result<Bytes, std::io::Error> {
        Ok(Bytes::from(s))
    }

    #[tokio::test]
    async fn translate_stream_emits_openai_frames_then_done() {
        let inner = r#"{"choices":[{"delta":{"content":"你好"}}]}"#;
        let frame = format!("data: {}\n\n", json!({ "body": inner }));
        let text = drain(futures_util::stream::iter(vec![bytes(frame)])).await;
        assert!(text.contains(r#""object":"chat.completion.chunk""#), "{text}");
        assert!(text.contains(r#""content":"你好""#), "{text}");
        assert!(text.contains(r#""role":"assistant""#), "{text}");
        // 上游没给 finish_reason ⇒ 收尾要补一帧（否则客户端一直等）。
        assert!(text.contains(r#""finish_reason":"stop""#), "{text}");
        assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
    }

    #[tokio::test]
    async fn translate_stream_turns_a_top_level_error_envelope_into_an_error_frame() {
        // 实测形状：HTTP 200，首帧就是队列提示，四个内容分支全不匹配。
        let frame = format!(
            "data: {}\n\n",
            json!({ "code": "403", "message": r#"{"isQueued":true,"queueCount":7309}"# })
        );
        let text = drain(futures_util::stream::iter(vec![bytes(frame)])).await;
        assert!(text.contains(r#""type":"upstream_error""#), "{text}");
        assert!(text.contains(r#""code":"qoder_stream_error""#), "{text}");
        assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
        // 错误路径**不**补 finish 帧。
        assert!(!text.contains(r#""finish_reason":"stop""#), "{text}");
    }

    #[tokio::test]
    async fn translate_stream_stops_at_upstream_done_and_skips_heartbeats() {
        let inner = r#"{"choices":[{"delta":{"content":"a"}}]}"#;
        let payload = format!("data: {}\n\n", json!({ "body": inner }));
        let text = drain(futures_util::stream::iter(vec![
            bytes(": heartbeat\n\n".to_string()),
            bytes("event: ping\n\n".to_string()),
            bytes(payload),
            bytes("data: [DONE]\n\n".to_string()),
            // [DONE] 之后的垃圾不该被翻译（Go 在 [DONE] 处就返回 io.EOF）。
            bytes(format!("data: {}\n\n", json!({ "body": r#"{"choices":[{"delta":{"content":"不该出现"}}]}"# }))),
        ]))
        .await;
        assert!(text.contains(r#""content":"a""#), "{text}");
        assert!(!text.contains("不该出现"), "{text}");
        assert_eq!(text.matches("data: [DONE]").count(), 1, "{text}");
    }

    #[tokio::test]
    async fn translate_stream_reports_a_read_failure_as_an_error_frame() {
        let items = vec![
            bytes("data: {\"body\":\"{\\\"choices\\\":[{\\\"delta\\\":{\\\"content\\\":\\\"a\\\"}}]}\"}\n\n".to_string()),
            Err::<Bytes, std::io::Error>(std::io::Error::other("boom")),
        ];
        let text = drain(futures_util::stream::iter(items)).await;
        assert!(text.contains(r#""content":"a""#), "{text}");
        assert!(text.contains("读取流失败"), "{text}");
        assert!(text.contains(r#""type":"upstream_error""#), "{text}");
    }
}
