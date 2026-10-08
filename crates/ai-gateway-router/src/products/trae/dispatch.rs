//! Trae 产品的 dispatch 层：把 OpenAI 形状的请求整理成 Trae 上游的形状。
//!
//! 本层只做三件事（结构对标 products/zcode/dispatch.rs）：
//!
//! 1. **取凭证** —— 从 [Auth] 的 file_path 重新读盘拿 [TraeCred]。[Auth] 的字段集
//!    刻意保持不动：Trae 的区域不走 Auth.domain（理由见 [super::cred::to_auth] 的 doc）。
//! 2. **区域判定** —— Trae 凭证的 domain 恒为空，于是 [crate::auth::region_of_domain]
//!    恒判 CN，池子的区域过滤对 Trae 账号完全失效（这是 products/trae/cred.rs 刻意的
//!    取舍）。[realm] 的模型名推断在这里只用来**拒绝**明显跨区的请求，绝不用来切换主机：
//!    CN 的令牌发到 SG 主机在形态上与「拿别人凭证试」一致，会撞上游风控（4017），
//!    代价远大于一次直接失败。
//! 3. **模型名规范化 + 请求体组装** —— 见 [resolve_model] 与 [assemble_body]。
//!
//! 本层**不做**额度预检，也**不做**目录预检：Trae 没有额度查询接口；而
//! test-bin/trae-upstream-probe 的活体实测显示，目录外的 config_name 也会先返回
//! 4011（账号级限流）而不是 4001，拿它做预检只会误伤健康账号。
//!
//! 与目录预检相反，**撞池自愈**（见 [other_function]）是必须做的：上游的
//! 4001 / 4023 既可能是「模型名真的不存在」，也可能是「function 池选错了」，
//! 而它不在响应里区分这两者。静态目录也帮不上忙 —— 两条独立活体实测互相矛盾：
//! glm-5.3 在 CN 的 chat_v3 快照里，可 chat_v3 发它却回 4001；kimi-k2.6 的快照
//! 只把它算给 solo，可 chat_v3 发它却回了 200。所以收到一次 4001 / 4023 后，
//! 本层改用**另一条产品线**的池重发**一次**请求（请求体只改 function，设备身份
//! 跟着池走）。两个池都拒，才说明这个模型名在上游确实不存在。
//!
//! 与 test-bin/recon/trae-wangqi233/src/trae-client.js 的对应关系：
//!
//! - resolveModelOptions -> [resolve_model]（含 rebuildDerivedMaps 的四写法索引，
//!   已扁平化成一张小写别名表 [ALIASES]）
//! - llmUtilsChat 的 body 组装 -> [assemble_body]
//! - 账号级 failover（MAX_ACCOUNT_FAILOVERS）与 retryWithBackoff 刻意**不**在本层实现：
//!   轮转与退避归 forward.rs，本层只负责把一次请求打到**一个**账号上。
//! - TRAE_MAX_CONTEXT=on 的 user_message_context.model_info 路径刻意**不**实现：
//!   该开关默认 off，且未获配额的账号会吃 4001；要填 model_info 还需要一张
//!   per-model 元数据表（multimodal 等），而网关手上只有别名表。
//! - options.model_name / options.function / options.config_name 三个调用方覆盖项
//!   在 JS 的生产调用点本就不生效（llmUtilsChat 只传一个参数），网关也没有对应字段。
//! - tools / tool_choice 在 JS 里同样被丢弃（全文件没有工具调用处理），这里保持一致。

use serde_json::{Map, Value};

use super::client::{self, ChatCall, Client};
use super::cred::TraeCred;
use super::device;
use super::realm::{self, Realm};
use crate::auth::Auth;
use crate::error::{GatewayError, ModelNotFoundError};

/// 上游的 function 池名：轻量对话，模型名认不出时的落点（JS 的 auto 分支）。
pub const FUNC_INLINE_CHAT: &str = "inline_chat";
/// 上游的 function 池名：通用对话，非 SOLO 产品的默认池。
pub const FUNC_CHAT_V3: &str = "chat_v3";
/// 上游的 function 池名：SOLO 产品专用池（自带上下文配额，见 device.rs）。
pub const FUNC_SOLO_WORK_LITE: &str = "solo_work_lite";
/// 上游会拒绝的 max_tokens 上限（对应 JS 的 MAX_TOKENS_LIMIT = 128000）。
pub const MAX_TOKENS_LIMIT: u64 = 128_000;

/// 另一条产品线的 function 池名；[FUNC_INLINE_CHAT] 与未知值返回 None。
///
/// 撞池自愈只在两条真实产品线之间换：inline_chat 不是产品线池，auto 的请求本来
/// 就不带 config_name，把它换成一个产品线池只会发出一个更差的请求。
pub fn other_function(current: &str) -> Option<&'static str> {
    match current {
        FUNC_CHAT_V3 => Some(FUNC_SOLO_WORK_LITE),
        FUNC_SOLO_WORK_LITE => Some(FUNC_CHAT_V3),
        _ => None,
    }
}

/// Trae 产品上游的 dispatch 层。
pub struct Dispatch {
    client: Client,
}

impl Dispatch {
    /// 用客户端的 HTTP 资源构造。
    ///
    /// 调用方（products/trae/mod.rs）必须先 [Client::new] 再 [Client::apply_host_proxy]，
    /// 最后把客户端交进来；本层不再碰代理配置。
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// 只读访问底层客户端（留作目录校验与测试入口）。
    #[allow(dead_code)]
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// 把一次 OpenAI 形状的请求打到 Trae 上游。
    ///
    /// 返回的 [ChatCall] 里，流式响应已经由 client 层翻译成标准 OpenAI SSE
    /// （网关的 server 层用 data 加空格的形状回放，未翻译的 Trae 行组会被静默丢掉）。
    ///
    /// 上游的业务错误在「首帧」就被定性（见 super::client 的 chat_stream）：
    /// 额度耗尽走 [GatewayError::QuotaExhausted]，模型不存在走 [GatewayError::ModelNotFound]，
    /// 其余落 [ChatCall::Rejected]，HTTP 载体如实带回，交给 forward.rs 的 classify 判。
    ///
    /// ⚠ 与 zcode 的对应实现一样，这里**必须**在失败时打一行日志：server 层会把
    /// 非 [GatewayError] 的错误归成「无法连接上游（网络超时 / 连接被拒）」，把排查
    /// 方向引向网络与代理，而真因只存在于错误文本里。
    pub async fn chat_stream(
        &self,
        a: &Auth,
        openai_body: &[u8],
    ) -> Result<ChatCall, GatewayError> {
        let Some(cr) = cred_of(a) else {
            return Err(GatewayError::Other(
                "账号缺少 Trae 凭证（Product=trae 但凭证为空）".to_string(),
            ));
        };

        // 设备指纹与 SOLO 判定：两者都只读，且决定了发到上游的 function 池名。
        let store_dir = device::store_dir_of_cred_file(&cr.file_path);
        let profile = device::resolve(cr.realm, store_dir.as_deref());
        let default_fn = if profile.is_solo {
            FUNC_SOLO_WORK_LITE
        } else {
            FUNC_CHAT_V3
        };

        let client_model = client::model_of(openai_body);
        let resolved = resolve_model(&client_model, default_fn);

        if let Some(want) = realm_conflict(resolved.config_name.as_deref(), cr.realm) {
            crate::logline!(
                "⚠ Trae 模型与账号区域不匹配：model={client_model} 属于 {}，账号 {} 属于 {}；\
                 请求未发出（跨区令牌会撞上游风控）",
                want.label(),
                super::mask_uid(&cr.uid),
                cr.realm.label(),
            );
            return Err(GatewayError::ModelNotFound(Box::new(
                ModelNotFoundError::new(client_model.clone(), cr.realm.label(), Vec::new()),
            )));
        }

        let obj = match serde_json::from_slice::<Value>(openai_body) {
            Ok(Value::Object(obj)) => obj,
            Ok(_) => {
                let msg = "Trae 请求体不是 JSON 对象".to_string();
                crate::logline!("⚠ Trae dispatch: uid={} {msg}", super::mask_uid(&cr.uid));
                return Err(GatewayError::Other(msg));
            }
            Err(e) => {
                let msg = format!("Trae 请求体不是合法 JSON：{e}");
                crate::logline!("⚠ Trae dispatch: uid={} {msg}", super::mask_uid(&cr.uid));
                return Err(GatewayError::Other(msg));
            }
        };

        // 组装要能复用：撞池自愈会拿同一个 obj 再组装一次（只差 function）。
        let assemble = |r: &ResolvedModel| assemble_body(&obj, r, &client_model);
        let body = match assemble(&resolved) {
            Ok(b) => b,
            Err(e) => {
                crate::logline!(
                    "⚠ Trae dispatch: uid={} 组装上游请求体失败：{e}",
                    super::mask_uid(&cr.uid)
                );
                return Err(GatewayError::Other(e));
            }
        };

        let mut call = self
            .client
            .chat_stream(&cr, &profile, &body, &client_model)
            .await;

        // 撞池自愈：只在「上游说这个模型名不成立」时考虑换池，且只换一次。本层自己
        // 判定的区域冲突在上面就返回了，走到这里的 ModelNotFound 只可能来自上游的
        // 4001 / 4023（见 super::client 的 chat_stream）。
        let mut swapped = false;
        if matches!(&call, Err(e) if e.model_not_found().is_some()) {
            if let Some(alt) = other_function(&resolved.function) {
                crate::logline!(
                    "Trae dispatch: uid={} 上游回 4001/4023（function={}），改发另一条产品线的池 {} 重试一次",
                    super::mask_uid(&cr.uid),
                    resolved.function,
                    alt,
                );
                let alt_resolved = ResolvedModel {
                    function: alt.to_string(),
                    config_name: resolved.config_name.clone(),
                };
                match assemble(&alt_resolved) {
                    Ok(alt_body) => {
                        // 设备身份必须跟着池走：x-device-brand / x-os-version 都按
                        // 产品线取值，只翻 is_solo 会发出自相矛盾的指纹。
                        let alt_profile =
                            device::switch_product_line(&profile, alt == FUNC_SOLO_WORK_LITE);
                        swapped = true;
                        call = self
                            .client
                            .chat_stream(&cr, &alt_profile, &alt_body, &client_model)
                            .await;
                    }
                    // 同一份 obj 刚刚组装成功过，这里不该失败；真失败了也不能吞掉
                    // 原始错误，只留一行日志。
                    Err(e) => crate::logline!(
                        "⚠ Trae dispatch: uid={} 换池后重组装请求体失败：{e}",
                        super::mask_uid(&cr.uid)
                    ),
                }
            }
        }

        match call {
            Ok(call) => Ok(call),
            Err(e) => {
                crate::logline!(
                    "trae dispatch: ChatStream 失败 uid={} realm={} model={} config_name={} function={} -> {e}",
                    super::mask_uid(&cr.uid),
                    cr.realm.as_str(),
                    client_model,
                    resolved.config_name.as_deref().unwrap_or("-"),
                    resolved.function,
                );
                // 换池那次也吃了 4001/4023 ⇒ 现在才敢说「两边都试过了」：client 层
                // 只知道第一个池，它的 note 说的是「参数与这个池不符」，会让人以为
                // 换个池就能用。
                match (swapped, e) {
                    (true, GatewayError::ModelNotFound(mnf)) => {
                        Err(GatewayError::ModelNotFound(Box::new(mnf.with_note(
                            format!(
                                "两条产品线的 function 池（chat_v3 / solo_work_lite）都拒绝了「{client_model}」：上游认为这个模型名在这个账号上不存在。"
                            ),
                        ))))
                    }
                    (_, e) => Err(e),
                }
            }
        }
    }

    /// 非流式聚合：入参是**未翻译**的上游原始行组流，出参是 OpenAI 形状的 chat.completion。
    ///
    /// forward.rs 在流式分支里自己拼 prefix + rest 之后调用这里（走的是产品上游的
    /// aggregate，而不是 upstream/sse.rs 的那个 —— 后者只认标准 OpenAI SSE）。
    pub fn aggregate(&self, sse: &str, model: &str) -> Result<Map<String, Value>, String> {
        super::translate::aggregate(sse, model)
    }
}

/// 从 [Auth] 重新读盘拿 Trae 凭证。
///
/// 刻意不用 Auth 里的字段（Trae 的 token 带 Cloud-IDE-JWT 前缀、realm 与区域码都不在
/// Auth 里），而是按 file_path 读宿主写的凭证文件 —— 与 zcode 的 cred_of 同款：
/// 网关进程与宿主进程会同时改这份文件（宿主写 account.no_route，网关写刷新后的
/// token），每次重新读盘是唯一能看见最新状态的做法。
pub fn cred_of(a: &Auth) -> Option<TraeCred> {
    if a.access_token.is_empty() {
        return None;
    }
    TraeCred::load_file(&a.file_path).ok()
}
/// 解析后的上游模型选择（对应 JS resolveModelOptions 的返回值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    /// 上游的 function 池名（inline_chat / chat_v3 / solo_work_lite）。
    pub function: String,
    /// 上游的 config_name。None 表示**不带**（JS 的 auto 分支：既不写 config_name
    /// 也不写 model）。
    pub config_name: Option<String>,
}

/// 别名表：小写客户端模型名 -> 上游 config_name（大小写**原样**，上游认这个）。
///
/// 表由 test-bin/recon/trae-wangqi233/model-config.json 复刻 rebuildDerivedMaps 得到：
/// 每条模型贡献 {key, key.toLowerCase(), config_name, config_name.toLowerCase()} 四种写法，
/// 把四种写法统一压成小写后去重，得 58 个小写别名 -> 18 个上游 config_name（零冲突）。
/// 顺序即 model-config.json 的声明顺序，等长并列时以先出现者胜（见 [resolve_model]）。
///
/// ★ 记住要同步维护这张表：它是**唯一**的模型名真相，改上游目录时要一起改。
const ALIASES: [(&str, &str); 58] = [
    ("claude-opus-4-7", "glm-5.2"),
    ("glm-5.2", "glm-5.2"),
    ("claude-opus-4-6", "glm-5.2"),
    ("claude-opus-4-5", "glm-5.2"),
    ("claude-opus-4-5-20251101", "glm-5.2"),
    ("claude-sonnet-4-6", "glm-5.2"),
    ("claude-sonnet-4-5", "glm-5.2"),
    ("claude-sonnet-4-5-20250929", "glm-5.2"),
    ("claude-3.5-sonnet", "glm-5.2"),
    ("claude-3.7-sonnet", "glm-5.2"),
    ("claude-sonnet-4", "glm-5.2"),
    ("claude-haiku-4-5", "glm-5-turbo"),
    ("glm-5-turbo", "glm-5-turbo"),
    ("claude-haiku-4-5-20251001", "glm-5-turbo"),
    ("deepseek-v3", "DeepSeek-V4-Pro"),
    ("deepseek-v4-pro", "DeepSeek-V4-Pro"),
    ("deepseek-v4-flash", "DeepSeek-V4-Flash"),
    ("deepseek-r1", "custom_model_deepseek_reasoner"),
    ("custom_model_deepseek_reasoner", "custom_model_deepseek_reasoner"),
    ("glm-5.1", "glm-5-turbo"),
    ("glm-5", "glm-5"),
    ("glm-5v-turbo", "glm-5-turbo"),
    ("glm-4.7", "glm-5"),
    ("glm-4.6", "glm-5"),
    ("doubao-seed-code", "Doubao-Seed-2.0-Code"),
    ("doubao-seed-2.0-code", "Doubao-Seed-2.0-Code"),
    ("doubao-1.8", "Doubao-Seed-2.1-Turbo"),
    ("doubao-seed-2.1-turbo", "Doubao-Seed-2.1-Turbo"),
    ("doubao-1-6", "Doubao-Seed-2.0-Code"),
    ("minimax-m3", "minimax-m3"),
    ("minimax-m2.7", "minimax-m3"),
    ("kimi-k2.6", "kimi-k2.6"),
    ("kimi-k2", "kimi-k2.6"),
    ("qwen3.7-plus", "qwen-3.7-plus"),
    ("qwen-3.7-plus", "qwen-3.7-plus"),
    ("qwen3.6-plus", "qwen-3.7-plus"),
    ("qwen3-coder", "qwen-3.7-plus"),
    ("gpt-4o", "custom_model_gpt-5"),
    ("custom_model_gpt-5", "custom_model_gpt-5"),
    ("gpt-4o-mini", "custom_model_gpt-5"),
    ("gemini-2.0-flash", "custom_model_gemini"),
    ("custom_model_gemini", "custom_model_gemini"),
    ("gemini-2.5-pro", "custom_model_gemini"),
    ("kimi-k2-5", "kimi-k2.6"),
    ("qwen-3-5", "qwen-3.7-plus"),
    ("doubao-seed-2-1-pro", "Doubao-Seed-2.1-Pro"),
    ("doubao-seed-2.1-pro", "Doubao-Seed-2.1-Pro"),
    ("doubao-seed-2-1-turbo", "Doubao-Seed-2.1-Turbo"),
    ("kimi-k2-7-code", "kimi-k2.7-code"),
    ("kimi-k2.7-code", "kimi-k2.7-code"),
    ("deepseek-v3-1", "DeepSeek-V4-Flash"),
    ("kimi-k3", "kimi-k3"),
    ("doubao_1_6", "Doubao-Seed-2.0-Code"),
    ("qwen-3.6-plus", "qwen-3.7-plus"),
    ("qwen-3.5", "qwen-3.7-plus"),
    ("kimi-k2.5", "kimi-k2.6"),
    ("glm-5.3", "glm-5.3"),
    ("qwen3.8-max", "qwen3.8-max"),
];

/// Trae 侧**客户端可发**的模型名清单（= 上面别名表的键，按表内顺序去重）。
///
/// 用途：给 `Pool::set_product_models_fallback` 注入一份 Trae 兜底清单，
/// 让**无前缀**的模型名在 Trae 不提供时被排除掉。
///
/// # 为什么必须有它（真实事故）
///
/// 2026-10-04 实测：客户端发**裸名** `deepseek-v4.1-flash`（Trae 根本没有这个模型），
/// 请求被路由到了 Trae 账号上并成功返回 —— 因为宿主下发的 `product_models` 里
/// **只有 qoder / zcode**，没有 workbuddy 也没有 trae ⇒ `product_offers_model` 对这两个
/// 产品都回「不知道」⇒ `product_may_serve` 在**没有任何兜底清单**时 `return true`。
/// workbuddy 靠网关自补的兜底清单躲过一劫，trae 没有 ⇒ 裸名被误路由。
///
/// # 为什么用这张表而不是 state.vscdb
///
/// 客户端的 `AI.agent.model.model_list_map` **不能**用：「智能体客户端一键导入」
/// 会往里写入**我们自己的**模型（见 trae_agent.rs 的 `write_db`），把
/// `deepseek-v4.1-flash` 也声明成 Trae 的模型 —— 那正是要修的 bug。
///
/// # 为什么只当「兜底」而不是「声明」
///
/// 兜底清单只进 `product_may_serve`（排除用），**不进** `product_declares_model`
/// （声明用）—— 后者若把猜测算成声明，会重演 2026-09-22 那类事故（19 个 WorkBuddy
/// 账号「声明」支持 GLM-5.3，把唯一一个真支持的 qoder 压下去，上游回 400 model_not_in_region）。
/// 别名表本身只是**客户端写法**的映射，不等于「上游此刻真的提供」，故它不够格当声明。
///
/// 显式 `trae:xxx` 请求走 `pick_for_model_product_region`，根本不查这张清单 ⇒
/// 清单不全也**不会**让显式请求失效（兜底是安全的）。
pub fn static_trae_model_ids() -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(ALIASES.len());
    for (alias, _) in ALIASES.iter() {
        if !out.iter().any(|s| s == alias) {
            out.push((*alias).to_string());
        }
    }
    out
}

/// 模型名规范化：移植 trae-client.js 的 resolveModelOptions（含 rebuildDerivedMaps 的索引）。
///
/// 四步，与 JS 逐条对应：
///
/// 1. **auto / 空名** -> [FUNC_INLINE_CHAT] 且 config_name 为空（JS 的 auto 分支刻意
///    不带任何模型字段）。
/// 2. **别名表精确命中**（不分大小写；表里已把 JS 的四种写法压成小写，语义等价于
///    JS 对 MODEL_TO_FUNCTION 的四键索引）。
/// 3. **最长命中的模糊匹配**：lower == key 或 lower 含 key 或 key 含 lower，
///    取 key 最长者 —— JS 的原话是「Prefer longest key match so "glm-5-turbo"
///    does not incorrectly hit "glm-5"」。
/// 4. **都不中** -> 把**原样**的客户端模型名当 config_name 透传（注释原文：
///    「Unknown model name: pass through as config_name (SOLO may support it directly)」）。
///
/// default_fn 是上游的 function 池名（[FUNC_SOLO_WORK_LITE] 或 [FUNC_CHAT_V3]）。
/// 表里 58 条的 function **全部**是 chat_v3（脚本逐条核对过），于是 JS 那句
/// 「defaultFn 不是 chat_v3 时把 chat_v3 改写成 defaultFn」在实现上恒等价于
/// 「命中即用 default_fn」—— 这就是本表没有 function 列的原因。
///
/// ⚠ 有记录的偏离：JS 的 TRAE_AUTO_FUNCTION 环境变量覆盖未移植（网关不靠环境变量
/// 决定 function 池；SOLO 判定改由 device::resolve 的 decide_solo 承载 —— 环境变量
/// TRAE_PRODUCT 优先，其次真实客户端目录名，都没有证据时默认 SOLO）。打错池时由
/// [other_function] 的「换池重发一次」兜底，不再指望默认值一次猜对。
/// 另外 JS 相等判定用的是「最长且**并列时**由对象键插入序决定」，这里退化成
/// 「表内先出现者胜」。脚本对全部 58 个键及其所有子串（3164 个输入）暴力核对过：
/// 只有 11 个退化残片（如 "glm-5."、"o-1"）会并列，且全都不可能是真实模型名
/// （两个等长且互相包含的字符串必然相同）。
pub fn resolve_model(model_name: &str, default_fn: &str) -> ResolvedModel {
    let lower = model_name.to_lowercase();
    if lower.is_empty() || lower == "auto" {
        return ResolvedModel {
            function: FUNC_INLINE_CHAT.to_string(),
            config_name: None,
        };
    }
    if let Some((_, cfg)) = ALIASES.iter().find(|(alias, _)| *alias == lower) {
        return ResolvedModel {
            function: default_fn.to_string(),
            config_name: Some((*cfg).to_string()),
        };
    }
    let mut best: Option<&str> = None;
    let mut best_len = 0usize;
    for (key, cfg) in ALIASES.iter() {
        if lower == *key || lower.contains(*key) || key.contains(lower.as_str()) {
            if key.len() > best_len {
                best = Some(cfg);
                best_len = key.len();
            }
        }
    }
    if let Some(cfg) = best {
        return ResolvedModel {
            function: default_fn.to_string(),
            config_name: Some(cfg.to_string()),
        };
    }
    ResolvedModel {
        function: default_fn.to_string(),
        config_name: Some(model_name.to_string()),
    }
}

/// 判断解析出的上游模型名是否**明显**属于另一个区域。
///
/// 只在 [realm::infer_realm_for_model] 给出**唯一**答案时才生效（它自身对
/// 「两个区域都命中」的歧义模型返回 None，例如 kimi-k2.6 同时命中 CN 的
/// kimi-k2.6 与 SG 的 kimi-k），因此本函数只会拦下确凿的跨区请求。
///
/// 为什么只拒绝、不换主机：CN 的令牌发到 SG 主机在形态上与「拿别人的凭证试」
/// 一致，会撞上游风控（4017），代价远大于一次直接失败。
///
/// ★ 判据用**上游 config_name** 而不是客户端模型名：客户端名字是别名表里的旧称
/// （例如 deepseek-v3 -> DeepSeek-V4-Pro），拿它去撞区域表会把 CN 账号能正常服务的
/// 模型误判成 SG 专属。
fn realm_conflict(config_name: Option<&str>, cred_realm: Realm) -> Option<Realm> {
    let want = realm::infer_realm_for_model(config_name?, None)?;
    if want == cred_realm {
        None
    } else {
        Some(want)
    }
}

/// 把 OpenAI 形状的请求体组装成 Trae 上游的形状（移植 trae-client.js 的 body 组装）。
///
/// 顶层只带这些字段：messages、function、stream，加上 config_name / model 与
/// max_tokens、temperature、top_p、presence_penalty、frequency_penalty、stop、seed、n。
/// 其余字段（tools、tool_choice、response_format、stream_options、user、logprobs…）
/// **一律丢弃** —— JS 生产路径同样只搬这几个（全文件没有工具调用处理）。
///
/// ⚠ 有记录的偏离：
/// - 键序：serde_json 的 Map 默认按字典序输出，JS 按插入序。JSON 对象无序，
///   接收方不受影响（与 products/zcode/dispatch.rs 的 replace_model 同款取舍）。
/// - JS 的 String() 强转只对字符串/数字/布尔逐字对齐；对象型 content 在 JS 里会变成
///   "[object Object]"，这里序列化成 JSON 文本。真实 OpenAI 客户端不会这么发。
pub fn assemble_body(
    obj: &Map<String, Value>,
    resolved: &ResolvedModel,
    model: &str,
) -> Result<Vec<u8>, String> {
    let Some(Value::Array(messages)) = obj.get("messages") else {
        return Err("Trae 请求体缺少 messages 数组".to_string());
    };

    let mut out_messages: Vec<Value> = Vec::with_capacity(messages.len());
    for m in messages {
        let Some(mobj) = m.as_object() else {
            return Err("Trae 请求体的 messages 里有非对象元素".to_string());
        };
        let mut item = Map::new();
        if let Some(role) = mobj.get("role") {
            item.insert("role".to_string(), role.clone());
        }
        let content = match mobj.get("content") {
            Some(Value::Array(parts)) => {
                let mut out_parts: Vec<Value> = Vec::with_capacity(parts.len());
                for p in parts {
                    if let Value::String(text) = p {
                        out_parts.push(text_part(text));
                    } else {
                        out_parts.push(p.clone());
                    }
                }
                Value::Array(out_parts)
            }
            other => {
                let text = match other {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Bool(false)) => String::new(),
                    Some(Value::Bool(true)) => "true".to_string(),
                    Some(v) => v.to_string(),
                };
                Value::Array(vec![text_part(&text)])
            }
        };
        item.insert("content".to_string(), content);
        out_messages.push(Value::Object(item));
    }

    // JS: stream !== false（只有显式 false 才是 false，缺省与 null 都是 true）。
    let stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(true);
    let mut body = Map::new();
    body.insert("messages".to_string(), Value::Array(out_messages));
    body.insert(
        "function".to_string(),
        Value::String(resolved.function.clone()),
    );
    body.insert("stream".to_string(), Value::Bool(stream));

    // JS: if (configName && funcName !== 'inline_chat') 带上 config_name 与 model，
    // 同时注释警告**不要**加 agent_type / device_id / ide_version（服务端会回 4023）。
    match resolved.config_name.as_deref() {
        Some(c) if !c.is_empty() && resolved.function != FUNC_INLINE_CHAT => {
            body.insert("config_name".to_string(), Value::String(c.to_string()));
            body.insert("model".to_string(), Value::String(c.to_string()));
        }
        _ => {
            // JS 的 else 分支：model_name 取客户端原样的模型名，但 auto 与空名都不写。
            // （config_name 只会在 auto 分支为空，此时 model 通常就是 auto；
            //  仍然照抄 JS，因为 "AUTO" 这种大写写法在 JS 里 model !== 'auto' 成立。）
            if !model.is_empty() && model != "auto" {
                body.insert("model".to_string(), Value::String(model.to_string()));
            }
        }
    }

    if let Some(Value::Number(n)) = obj.get("max_tokens") {
        let f = n.as_f64().unwrap_or(0.0);
        if f != 0.0 {
            if f > MAX_TOKENS_LIMIT as f64 {
                body.insert("max_tokens".to_string(), Value::from(MAX_TOKENS_LIMIT));
            } else {
                body.insert("max_tokens".to_string(), Value::Number(n.clone()));
            }
        }
    }

    for key in [
        "temperature",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
    ] {
        if let Some(Value::Number(n)) = obj.get(key) {
            body.insert(key.to_string(), Value::Number(n.clone()));
        }
    }

    match obj.get("stop") {
        Some(Value::Array(a)) => {
            body.insert("stop".to_string(), Value::Array(a.clone()));
        }
        Some(Value::String(s)) if !s.is_empty() => {
            body.insert("stop".to_string(), Value::Array(vec![Value::String(s.clone())]));
        }
        Some(Value::Number(n)) if n.as_f64().unwrap_or(0.0) != 0.0 => {
            body.insert(
                "stop".to_string(),
                Value::Array(vec![Value::String(n.to_string())]),
            );
        }
        Some(Value::Bool(true)) => {
            body.insert(
                "stop".to_string(),
                Value::Array(vec![Value::String("true".to_string())]),
            );
        }
        _ => {}
    }

    if let Some(Value::Number(n)) = obj.get("seed") {
        if n.as_f64().unwrap_or(0.0) != 0.0 {
            body.insert("seed".to_string(), Value::Number(n.clone()));
        }
    }

    if let Some(Value::Number(n)) = obj.get("n") {
        if n.as_f64().unwrap_or(0.0) > 1.0 {
            body.insert("n".to_string(), Value::Number(n.clone()));
        }
    }

    serde_json::to_vec(&Value::Object(body)).map_err(|e| format!("序列化上游请求体失败：{e}"))
}

/// 包一个 type=text 的内容块。
fn text_part(text: &str) -> Value {
    let mut part = Map::new();
    part.insert("type".to_string(), Value::String("text".to_string()));
    part.insert("text".to_string(), Value::String(text.to_string()));
    Value::Object(part)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PRODUCT_TRAE;

    /// 把一段 JSON 字面量解析成对象（测试专用）。
    fn obj(text: &str) -> Map<String, Value> {
        match serde_json::from_str::<Value>(text).expect("测试用 JSON 必须合法") {
            Value::Object(m) => m,
            _ => panic!("测试用 JSON 必须是对象"),
        }
    }

    /// 把组装结果解析回 Value，便于断言。
    fn out(body: &[u8]) -> Value {
        serde_json::from_slice(body).expect("组装结果必须能解析")
    }

    fn auth_with(access_token: &str, file_path: &str) -> Auth {
        Auth {
            access_token: access_token.to_string(),
            file_path: file_path.to_string(),
            product: PRODUCT_TRAE.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn auto_and_empty_names_fall_back_to_inline_chat() {
        for name in ["auto", "AUTO", "Auto", ""] {
            let r = resolve_model(name, FUNC_CHAT_V3);
            assert_eq!(r.function, FUNC_INLINE_CHAT, "输入 {name}");
            assert_eq!(r.config_name, None, "输入 {name}");
        }
    }

    #[test]
    fn aliases_are_matched_without_regard_to_case() {
        for name in ["glm-5.2", "GLM-5.2", "Glm-5.2"] {
            let r = resolve_model(name, FUNC_CHAT_V3);
            assert_eq!(r.function, FUNC_CHAT_V3);
            assert_eq!(r.config_name.as_deref(), Some("glm-5.2"), "输入 {name}");
        }
        // 上游 config_name 的原样大小写必须保住（上游按这个字符串认模型）。
        let r = resolve_model("deepseek-v4-pro", FUNC_CHAT_V3);
        assert_eq!(r.config_name.as_deref(), Some("DeepSeek-V4-Pro"));
        let r = resolve_model("Doubao-Seed-2.0-Code", FUNC_CHAT_V3);
        assert_eq!(r.config_name.as_deref(), Some("Doubao-Seed-2.0-Code"));
    }

    #[test]
    fn the_longest_alias_wins_over_its_own_prefix() {
        // JS 的原话：Prefer longest key match so "glm-5-turbo" does not hit "glm-5".
        let r = resolve_model("glm-5-turbo", FUNC_CHAT_V3);
        assert_eq!(r.config_name.as_deref(), Some("glm-5-turbo"));
        // 模糊步：所有命中项都更长，取最长者。
        for (name, want) in [
            ("qwen", "qwen-3.7-plus"),
            ("doubao", "Doubao-Seed-2.1-Turbo"),
            ("minimax", "minimax-m3"),
            ("gemini", "custom_model_gemini"),
        ] {
            let r = resolve_model(name, FUNC_CHAT_V3);
            assert_eq!(r.config_name.as_deref(), Some(want), "输入 {name}");
        }
    }

    #[test]
    fn an_unknown_name_passes_through_verbatim() {
        let r = resolve_model("My-Custom-Model-1", FUNC_CHAT_V3);
        assert_eq!(r.function, FUNC_CHAT_V3);
        assert_eq!(r.config_name.as_deref(), Some("My-Custom-Model-1"));
    }

    #[test]
    fn the_solo_pool_rewrites_every_hit_to_solo_work_lite() {
        // 表里 58 条的 function 全是 chat_v3，于是「defaultFn 覆盖」等价于「命中即用 default_fn」。
        for name in ["glm-5.2", "auto", "", "My-Custom-Model-1"] {
            let r = resolve_model(name, FUNC_SOLO_WORK_LITE);
            assert_eq!(
                r.function,
                if name.is_empty() || name.eq_ignore_ascii_case("auto") {
                    FUNC_INLINE_CHAT
                } else {
                    FUNC_SOLO_WORK_LITE
                },
                "输入 {name}"
            );
        }
    }

    #[test]
    fn the_alias_table_is_lowercase_and_conflict_free() {
        assert_eq!(ALIASES.len(), 58, "别名表条数与 model-config.json 复刻结果不符");
        let mut seen: Vec<&str> = Vec::with_capacity(ALIASES.len());
        for (alias, cfg) in ALIASES.iter() {
            assert_eq!(
                *alias,
                alias.to_lowercase(),
                "别名 {alias} 不是小写（查表输入恒为小写）"
            );
            assert!(!cfg.is_empty(), "别名 {alias} 的 config_name 为空");
            assert!(!seen.contains(alias), "别名 {alias} 重复");
            seen.push(alias);
        }
        let mut cfgs: Vec<&str> = ALIASES.iter().map(|(_, c)| *c).collect();
        cfgs.sort_unstable();
        cfgs.dedup();
        assert_eq!(cfgs.len(), 18, "上游 config_name 去重后应为 18 个");
    }

    #[test]
    fn realm_conflict_only_fires_on_a_proven_cross_region_model() {
        // 国服专属：glm 只在 CN 的专属清单里。
        assert_eq!(realm_conflict(Some("glm-5.2"), Realm::Cn), None);
        assert_eq!(realm_conflict(Some("glm-5.2"), Realm::Sg), Some(Realm::Cn));
        // 国际版专属：gpt-5 只在 SG 的专属清单里（custom_model_gpt-5 也含 gpt-5）。
        assert_eq!(realm_conflict(Some("custom_model_gpt-5"), Realm::Sg), None);
        assert_eq!(
            realm_conflict(Some("custom_model_gpt-5"), Realm::Cn),
            Some(Realm::Sg)
        );
        // 歧义模型（kimi-k2.6 同时命中 CN 的 kimi-k2.6 与 SG 的 kimi-k）不拦。
        assert_eq!(realm_conflict(Some("kimi-k2.6"), Realm::Sg), None);
        assert_eq!(realm_conflict(Some("kimi-k2.6"), Realm::Cn), None);
        // 没有 config_name（auto 分支）时无从判定。
        assert_eq!(realm_conflict(None, Realm::Sg), None);
        // 别名表的旧称不能被当成区域依据：deepseek-v3 在别名表里指向 CN 的
        // DeepSeek-V4-Pro，但字符串本身在 SG 清单里（deepseek-v3）。
        assert_eq!(realm_conflict(Some("DeepSeek-V4-Pro"), Realm::Cn), None);
        assert_eq!(
            realm_conflict(Some("DeepSeek-V4-Pro"), Realm::Sg),
            Some(Realm::Cn)
        );
    }

    #[test]
    fn assemble_body_mirrors_the_js_shape() {
        let input = obj(
            r#"{
                "model": "glm-5.2",
                "messages": [{"role": "user", "content": "你好"}],
                "temperature": 0.5,
                "top_p": 0.9,
                "presence_penalty": -1,
                "frequency_penalty": 1.25,
                "max_tokens": 200000,
                "stop": "END",
                "seed": 42,
                "n": 2,
                "stream": true,
                "tools": [{"type": "function"}],
                "tool_choice": "auto"
            }"#,
        );
        let resolved = resolve_model("glm-5.2", FUNC_CHAT_V3);
        let body = out(&assemble_body(&input, &resolved, "glm-5.2").unwrap());

        assert_eq!(body["function"], "chat_v3");
        assert_eq!(body["config_name"], "glm-5.2");
        assert_eq!(body["model"], "glm-5.2");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], MAX_TOKENS_LIMIT);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["presence_penalty"], -1);
        assert_eq!(body["frequency_penalty"], 1.25);
        assert_eq!(body["stop"], serde_json::json!(["END"]));
        assert_eq!(body["seed"], 42);
        assert_eq!(body["n"], 2);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(body["messages"][0]["content"][0]["text"], "你好");
        // JS 生产路径没有工具调用处理，网关一致丢弃。
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("user_message_context").is_none());
    }

    #[test]
    fn assemble_body_caps_max_tokens_and_keeps_small_values() {
        let resolved = resolve_model("glm-5.2", FUNC_CHAT_V3);
        let base = r#"{"messages": [{"role": "user", "content": "x"}]"#;
        let cap = obj(&format!("{base}, \"max_tokens\": 128001}}"));
        assert_eq!(
            out(&assemble_body(&cap, &resolved, "glm-5.2").unwrap())["max_tokens"],
            MAX_TOKENS_LIMIT
        );
        let exact = obj(&format!("{base}, \"max_tokens\": 128000}}"));
        assert_eq!(
            out(&assemble_body(&exact, &resolved, "glm-5.2").unwrap())["max_tokens"],
            MAX_TOKENS_LIMIT
        );
        let small = obj(&format!("{base}, \"max_tokens\": 4096}}"));
        assert_eq!(
            out(&assemble_body(&small, &resolved, "glm-5.2").unwrap())["max_tokens"],
            4096
        );
        // JS 的 options?.max_tokens 判真：0 被跳过。
        let zero = obj(&format!("{base}, \"max_tokens\": 0}}"));
        assert!(out(&assemble_body(&zero, &resolved, "glm-5.2").unwrap())
            .get("max_tokens")
            .is_none());
    }

    #[test]
    fn assemble_body_follows_the_js_truthiness_for_optional_fields() {
        let resolved = resolve_model("glm-5.2", FUNC_CHAT_V3);
        let base = r#"{"messages": [{"role": "user", "content": "x"}]"#;
        // seed 为 0 时不发；n 为 1 时不发。
        let b = obj(&format!("{base}, \"seed\": 0, \"n\": 1}}"));
        let v = out(&assemble_body(&b, &resolved, "glm-5.2").unwrap());
        assert!(v.get("seed").is_none());
        assert!(v.get("n").is_none());
        // stop 是空数组时 JS 的 if (options.stop) 成立（空数组为真）⇒ 照发。
        let b = obj(&format!("{base}, \"stop\": []}}"));
        assert_eq!(
            out(&assemble_body(&b, &resolved, "glm-5.2").unwrap())["stop"],
            serde_json::json!([])
        );
        // 非数字的采样参数一律丢弃。
        let b = obj(&format!(
            "{base}, \"temperature\": \"0.5\", \"top_p\": null, \"stop\": 1.5}}"
        ));
        let v = out(&assemble_body(&b, &resolved, "glm-5.2").unwrap());
        assert!(v.get("temperature").is_none());
        assert!(v.get("top_p").is_none());
        assert_eq!(v["stop"], serde_json::json!(["1.5"]));
    }

    #[test]
    fn assemble_body_keeps_stream_false_and_omits_the_model_for_auto() {
        let resolved = resolve_model("glm-5.2", FUNC_CHAT_V3);
        let b = obj(r#"{"messages": [{"role": "user", "content": "x"}], "stream": false}"#);
        assert_eq!(
            out(&assemble_body(&b, &resolved, "glm-5.2").unwrap())["stream"],
            false
        );

        let auto = resolve_model("auto", FUNC_CHAT_V3);
        let b = obj(r#"{"model": "auto", "messages": [{"role": "user", "content": "x"}]}"#);
        let v = out(&assemble_body(&b, &auto, "auto").unwrap());
        assert_eq!(v["function"], FUNC_INLINE_CHAT);
        assert!(v.get("config_name").is_none());
        assert!(v.get("model").is_none());
        // auto 之外的名字即使没有 config_name 也要带上（JS 的 else 分支）。
        let b = obj(r#"{"messages": [{"role": "user", "content": "x"}]}"#);
        let v = out(&assemble_body(&b, &auto, "some-model").unwrap());
        assert_eq!(v["model"], "some-model");
    }

    #[test]
    fn assemble_body_normalizes_content_of_every_shape() {
        let resolved = resolve_model("glm-5.2", FUNC_CHAT_V3);
        let b = obj(
            r#"{"messages": [
                {"role": "user", "content": [{"type": "text", "text": "a"}, "b", 3]},
                {"role": "assistant", "content": "c"},
                {"role": "system", "content": 7},
                {"role": "tool", "content": true},
                {"role": "user", "content": false},
                {"role": "user", "content": null}
            ]}"#,
        );
        let v = out(&assemble_body(&b, &resolved, "glm-5.2").unwrap());
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 6);
        assert_eq!(msgs[0]["content"][0], serde_json::json!({"type": "text", "text": "a"}));
        assert_eq!(msgs[0]["content"][1], serde_json::json!({"type": "text", "text": "b"}));
        assert_eq!(msgs[0]["content"][2], serde_json::json!(3));
        assert_eq!(msgs[1]["content"], serde_json::json!([{"type": "text", "text": "c"}]));
        assert_eq!(msgs[2]["content"], serde_json::json!([{"type": "text", "text": "7"}]));
        assert_eq!(msgs[3]["content"], serde_json::json!([{"type": "text", "text": "true"}]));
        assert_eq!(msgs[4]["content"], serde_json::json!([{"type": "text", "text": ""}]));
        assert_eq!(msgs[5]["content"], serde_json::json!([{"type": "text", "text": ""}]));
    }

    #[test]
    fn assemble_body_rejects_a_body_without_messages() {
        let resolved = resolve_model("glm-5.2", FUNC_CHAT_V3);
        assert!(assemble_body(&obj(r#"{"model": "glm-5.2"}"#), &resolved, "glm-5.2").is_err());
        assert!(assemble_body(
            &obj(r#"{"messages": "不是数组"}"#),
            &resolved,
            "glm-5.2"
        )
        .is_err());
        assert!(assemble_body(&obj(r#"{"messages": [1]}"#), &resolved, "glm-5.2").is_err());
    }

    #[test]
    fn cred_of_requires_a_token_and_a_readable_file() {
        assert!(cred_of(&auth_with("", "")).is_none());
        // 有令牌但文件不可读：返回 None（不 panic、不去碰真实目录）。
        assert!(cred_of(&auth_with("Cloud-IDE-JWT x.y.z", "")).is_none());
        assert!(cred_of(&auth_with(
            "Cloud-IDE-JWT x.y.z",
            "D:/no/such/dir/trae-1.json"
        ))
        .is_none());
    }

    #[test]
    fn aggregate_reads_the_raw_upstream_line_groups() {
        // 入参必须是**未翻译**的上游原文（forward.rs 把 prefix + rest 原样交过来）。
        let upstream = concat!(
            "id:1\nevent:output\ndata:{\"content\":\"你\"}\n\n",
            "id:2\nevent:output\ndata:{\"content\":\"好\",\"reasoning\":\"嗯\"}\n\n",
            "id:3\nevent:done\ndata:{\"finish_reason\":\"stop\"}\n\n"
        );
        let d = Dispatch::new(Client::new());
        let m = d.aggregate(upstream, "glm-5.2").expect("聚合必须成功");
        assert_eq!(m["model"], "glm-5.2");
        assert_eq!(m["choices"][0]["message"]["content"], "你好");
        assert_eq!(m["choices"][0]["message"]["reasoning_content"], "嗯");
        assert_eq!(m["choices"][0]["finish_reason"], "stop");
        assert_eq!(m["object"], "chat.completion");
    }

    #[test]
    fn aggregate_surfaces_an_upstream_error_event() {
        let upstream = "id:1\nevent:error\ndata:{\"code\":4008,\"message\":\"配额已用尽\"}\n\n";
        let d = Dispatch::new(Client::new());
        let err = d.aggregate(upstream, "glm-5.2").unwrap_err();
        assert!(err.contains("4008"), "错误文本要带上游业务码：{err}");
        assert!(err.contains("配额已用尽"), "错误文本要带上游原话：{err}");
    }

    #[test]
    fn a_dispatch_can_be_built_from_a_fresh_client() {
        let d = Dispatch::new(Client::new());
        // 只为确认构造链可用（client() 是留给目录校验的只读入口）。
        let _ = d.client();
    }

    #[test]
    fn only_the_two_real_pools_have_a_counterpart() {
        assert_eq!(other_function(FUNC_CHAT_V3), Some(FUNC_SOLO_WORK_LITE));
        assert_eq!(other_function(FUNC_SOLO_WORK_LITE), Some(FUNC_CHAT_V3));
        assert_eq!(other_function(FUNC_INLINE_CHAT), None, "auto 的落点不是产品线池");
        assert_eq!(other_function(""), None);
        assert_eq!(other_function("no_such_pool"), None);
    }

    #[test]
    fn the_pool_swap_changes_nothing_but_the_function_field() {
        // 撞池自愈靠的就是这条不变量：换池只换 function，config_name / model 不动。
        let o = obj(r#"{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}]}"#);
        let a = resolve_model("glm-5.3", FUNC_CHAT_V3);
        let b = resolve_model("glm-5.3", FUNC_SOLO_WORK_LITE);
        assert_eq!(a.config_name, b.config_name, "别名命中与池无关");
        assert_eq!(a.function, FUNC_CHAT_V3);
        assert_eq!(b.function, FUNC_SOLO_WORK_LITE);
        let ba = out(&assemble_body(&o, &a, "glm-5.3").unwrap());
        let bb = out(&assemble_body(&o, &b, "glm-5.3").unwrap());
        let (Value::Object(ma), Value::Object(mb)) = (&ba, &bb) else {
            panic!("组装结果必须是 JSON 对象");
        };
        assert_eq!(ma.len(), mb.len(), "换池不该增删字段");
        assert_eq!(
            ma.get("config_name"),
            mb.get("config_name"),
            "config_name 必须逐字相同"
        );
        let diff: Vec<&str> = ma
            .iter()
            .filter(|(k, v)| mb.get(k.as_str()) != Some(*v))
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(diff, vec!["function"], "两次组装只允许 function 不同");
    }
}
