//! Trae（Trae Work / Trae CN）接入：把本网关注册成 Trae 的「自定义模型」。
//!
//! ## 为什么不是写配置文件
//!
//! Trae 是 icube 系 Electron 应用，模型列表**不在** JSON/TOML 配置文件里，而是存在
//! `User/globalStorage/state.vscdb`（SQLite）的 `ItemTable` 表里，键名带登录 uid：
//!
//! - `{uid}:AI.agent.model.model_list_map` —— SOLO 侧，实测 **9 个组**
//!   （`assistant` / `solo_agent_lite` / `solo_coder` / `solo_agent_remote` /
//!   `solo_work_lite` / `solo_work_remote` / `solo_design_lite` /
//!   `solo_design_remote` / `agent`）
//! - `{uid}_AI.agent.model.model_list_map` —— IDE 侧，实测 **7 个组**
//!   （`builder` / `builder_v3` / `chat_v3` / `code_reviewer` /
//!   `code_review_summary` / `refactor` / `solo_agent`）
//!
//! ## ★★ 这两个键**不是**互为镜像（早期设计文档在这里写错了）
//!
//! 两边的自定义模型**各自独立存在**，写一边不会更新另一边，而且**字段集合不同**：
//! 冒号键的条目有 `config_name` 与 `use_remote_service`，下划线键的条目**两者都没有**
//! （其余 52 个字段逐字相同）。由此得出两条硬性实现要求：
//!
//! 1. **两个键都要写**；同一台机器上同时装了 Trae Work 与 Trae CN 时是**两个库**，
//!    两个库也都要写（实测 `Trae CN` 库里只有下划线键）。
//! 2. **组名按库里实际存在的顶层数组键逐个追加**，绝不硬编码组名列表 ——
//!    拿 9 个 SOLO 组名去写 IDE 库会把 `chat_v3` 等组**整组漏掉**。
//!
//! ## `ak` 加密
//!
//! 客户端把 API Key 存成 `ak` = `base64(nonce[12] || ciphertext || tag[16])`，
//! AES-256-GCM，密钥是**编译进客户端的常量**（无 DPAPI / safeStorage / keytar /
//! 机器派生 / 服务端解密）。材料见 [`KEY_MATERIAL_HEX`] / [`KEY_MASK_HEX`] / [`AAD`]，
//! 已对真实库 3/3 验证（密文可解、tag 通过、同 nonce 重加密逐字节相同）。
//! 出站时客户端把它解出来放进 `Authorization: Bearer <明文>`。
//!
//! ## 安全约定
//!
//! - 写入前一律 [`backup_files`] 备份整个 `state.vscdb`；
//! - 只在**已存在的** `ItemTable` 键上做读-改-写，键不存在就**什么都不做**；
//! - 回写**保持该键原有的存储类型**（BLOB 存 BLOB、TEXT 存 TEXT）——
//!   `switcher::vscdb` 已记录过同类事故：类型变了客户端就读不出来；
//! - 环境变量 [`ENV_DATA_DIR`] 可把目标整体改到别处（真实实例实测用），
//!   且**设置后绝不回落真实目录**：路径写错时必须「什么都不写」，
//!   而不是「去写所有者的库」。

use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use rand::RngCore as _;
use serde_json::{json, Map, Value};

use crate::modules::agent_import::{backup_files, ImportOutcome};
use crate::modules::switcher::vscdb::vscdb_path;

/// 客户端内置密钥材料（32 字节的 hex 形态，`A64`）。
const KEY_MATERIAL_HEX: &str = "66346768376a6b396c6b6c6e30717332747578796a6b396c6e30717332747535";
/// 与密钥材料前若干字节异或的掩码（`B20` 的前 16 个 hex 字符 = 8 字节）。
const KEY_MASK_HEX: &str = "71733274753576773878";
/// 异或轮数。
///
/// ★ 客户端源码是 `for (let i = 0; i < 8; i++) KEY[i] ^= B[i]` —— **8 次**。
/// 早期设计文档写成 `for i in 0..7`（7 次）是笔误：那样算出来的密钥第 7 字节
/// 是 `0x4e` 而不是 `0x4e ^ 0x78 = 0x36`，与实测派生密钥对不上。
const XOR_ROUNDS: usize = 8;
/// GCM nonce 长度（密文最前面的 12 字节）。
const NONCE_LEN: usize = 12;
/// GCM auth tag 长度（密文最后面的 16 字节）。
const TAG_LEN: usize = 16;
/// GCM 附加认证数据（10 个 ASCII 字节）。
const AAD: &[u8] = b"1741255278";

/// 覆盖 Trae 数据目录的环境变量（真实实例实测必需）。
pub const ENV_DATA_DIR: &str = "AI_GATEWAY_TRAE_DATA_DIR";

/// `ItemTable` 里模型表键的后缀（前缀是登录 uid，两种分隔符各一份）。
const KEY_SUFFIX: &str = "AI.agent.model.model_list_map";
/// Trae 侧本网关使用的 provider 标识。
const PROVIDER: &str = "custom_openai_compatible";
/// `name` / `config_name` 的前缀形态：`custom_openai_compatible//<MODEL>`。
const NAME_PREFIX: &str = "custom_openai_compatible//";
/// Trae 要的是**完整** chat 端点，不是 OpenAI 客户端那种 `/v1` 基址。
const CHAT_PATH: &str = "/v1/chat/completions";
/// 自定义模型：强制客户端直连（这正是离线可用的原因）。
const CLIENT_CONNECT: bool = true;

/// 无模板时补齐的一批 `null` 字段。
///
/// 实测真实条目里有、而早期设计文档没列 —— 一并落成 `null`，
/// 避免客户端读到 `undefined`（Trae 自己的写入路径也是把它们落成 `null`）。
const NULL_FIELDS: [&str; 21] = [
    "is_new",
    "is_beta",
    "core_memory_enable_type",
    "fee_model_level",
    "max_mode",
    "is_dollar_max",
    "is_max_default",
    "tob_commercial_info",
    "tags",
    "thinking_enable",
    "temperature",
    "top_p",
    "top_k",
    "feedback_group_link",
    "is_internal_usage_limit",
    "is_l4_repo_restricted",
    "hot_info",
    "enable_show_widget",
    "enable_todo_thought_extract",
    "todo_thought_extract_threshold",
    "reasoning_effort_options",
];

// ---------------------------------------------------------------------------
// ak 加解密
// ---------------------------------------------------------------------------

/// 十六进制串 → 字节；含非法字符或长度为奇数时返回 `None`。
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i < b.len() {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

/// 派生 AES-256-GCM 密钥：材料前 8 字节与掩码逐字节异或。
fn derived_key() -> [u8; 32] {
    // 两个常量都是编译期写死的 hex，解不出来只可能是有人改错了常量。
    // 用 `expect` 立刻炸，**绝不静默降级成一把错密钥** ——
    // 那会写出客户端永远解不开的 `ak`，且故障现场离这里很远。
    let material = decode_hex(KEY_MATERIAL_HEX).expect("KEY_MATERIAL_HEX 必须是合法 hex");
    let mask = decode_hex(KEY_MASK_HEX).expect("KEY_MASK_HEX 必须是合法 hex");

    let mut key = [0u8; 32];
    let n = material.len().min(key.len());
    key[..n].copy_from_slice(&material[..n]);
    for i in 0..XOR_ROUNDS.min(mask.len()) {
        key[i] ^= mask[i];
    }
    key
}

fn cipher() -> Result<Aes256Gcm, String> {
    Aes256Gcm::new_from_slice(&derived_key()).map_err(|e| format!("初始化 AES-256-GCM 失败: {e}"))
}

/// 用指定 nonce 加密（测试黄金向量用；生产走 [`encrypt_ak`] 的随机 nonce）。
fn encrypt_ak_with_nonce(plain: &str, nonce: &[u8]) -> Result<String, String> {
    if nonce.len() != NONCE_LEN {
        return Err(format!("nonce 长度非法（{}，应为 {NONCE_LEN}）", nonce.len()));
    }
    let ct = cipher()?
        .encrypt(
            Nonce::from_slice(nonce),
            aes_gcm::aead::Payload {
                msg: plain.as_bytes(),
                aad: AAD,
            },
        )
        .map_err(|_| "AES-256-GCM 加密失败".to_string())?;

    let mut blob = Vec::with_capacity(NONCE_LEN + ct.len());
    blob.extend_from_slice(nonce);
    blob.extend_from_slice(&ct);
    Ok(base64::engine::general_purpose::STANDARD.encode(blob))
}

/// 把明文 API Key 加密成 Trae 的 `ak`：`base64(nonce[12] || ciphertext || tag[16])`。
pub fn encrypt_ak(plain: &str) -> Result<String, String> {
    let mut nonce = [0u8; NONCE_LEN];
    // OS CSPRNG（Windows 上落到 BCryptGenRandom），与 `trae_oauth` 的做法一致。
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    encrypt_ak_with_nonce(plain, &nonce)
}

/// 解密 Trae 的 `ak`（单元测试与「是否已接入」判定用）。
pub fn decrypt_ak(blob: &str) -> Result<String, String> {
    let buf = base64::engine::general_purpose::STANDARD
        .decode(blob.trim())
        .map_err(|e| format!("ak 不是合法 base64: {e}"))?;
    if buf.len() < NONCE_LEN + TAG_LEN {
        return Err(format!(
            "ak 太短（{}，至少需要 {}）",
            buf.len(),
            NONCE_LEN + TAG_LEN
        ));
    }
    // aes-gcm 的 `decrypt` 期望 `ciphertext || tag`（tag 在后），
    // 而 `ak` 的布局是 `nonce || ciphertext || tag` —— 掐头即可，尾段天然对齐。
    let plain = cipher()?
        .decrypt(
            Nonce::from_slice(&buf[..NONCE_LEN]),
            aes_gcm::aead::Payload {
                msg: &buf[NONCE_LEN..],
                aad: AAD,
            },
        )
        .map_err(|_| "密钥不匹配或密文已损坏".to_string())?;
    String::from_utf8(plain).map_err(|e| format!("解密结果不是 UTF-8: {e}"))
}

// ---------------------------------------------------------------------------
// 目标目录解析
// ---------------------------------------------------------------------------

/// 一个 Trae 应用：数据根目录 + 其 `state.vscdb`。
#[derive(Clone, Debug, PartialEq, Eq)]
struct TraeDb {
    root: PathBuf,
    vscdb: PathBuf,
}

/// 解析一个根下的 `state.vscdb`。
///
/// 两种布局都认：
/// 1. 标准 icube 布局 `<root>/User/globalStorage/state.vscdb`
/// 2. 平铺布局 `<root>/state.vscdb`（真实实例实测时把单个库文件拷进一个空目录）
fn vscdb_under(root: &Path) -> Option<PathBuf> {
    let nested = vscdb_path(root);
    if nested.is_file() {
        return Some(nested);
    }
    let flat = root.join("state.vscdb");
    if flat.is_file() {
        return Some(flat);
    }
    None
}

/// 候选应用数据目录（存在且**确实有** `state.vscdb` 的）。
///
/// 只认 `TRAE SOLO CN` 与 `Trae CN`：
/// - `%APPDATA%\Trae`（无后缀）是**陈旧残留**（另一个 uid、0 个自定义模型），排除；
/// - `TRAE SOLO` 是 Trae Work 的旧目录名，本任务不认（用户没装）；
/// - `TRAE CN` 与 `Trae CN` 是同一目录（Windows 大小写不敏感），
///   故按 realpath 去重。
///
/// 拆成纯函数内核是为了让测试不必 `set_var("APPDATA")` ——
/// 环境变量是进程全局的，并行测试下会互相串扰（见 `trae_discover` 的同类注释）。
fn data_dirs_under(appdata: &str) -> Vec<PathBuf> {
    if appdata.trim().is_empty() {
        return Vec::new();
    }
    let root = PathBuf::from(appdata);
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for name in ["TRAE SOLO CN", "Trae CN"] {
        let dir = root.join(name);
        if !dir.is_dir() {
            continue;
        }
        if vscdb_under(&dir).is_none() {
            continue;
        }
        let canon = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen.contains(&canon) {
            continue;
        }
        seen.push(canon);
        out.push(dir);
    }
    out
}

/// 解析最终要处理的库列表（纯内核，便于测试）。
///
/// - `override_raw` 为 `None` ⇒ 走 `%APPDATA%` 候选；
/// - `override_raw` 为 `Some` ⇒ **只用覆盖值**。解析不出库就返回**空列表**，
///   ★ **绝不回落真实目录** —— 覆盖值写错时必须是什么都不写。
fn resolve_dbs_with(override_raw: Option<&str>, appdata: &str) -> Vec<TraeDb> {
    if let Some(raw) = override_raw {
        let raw = raw.trim();
        if raw.is_empty() {
            return Vec::new();
        }
        let p = PathBuf::from(raw);
        if p.is_file() {
            let root = p
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| p.clone());
            return vec![TraeDb { root, vscdb: p }];
        }
        return match vscdb_under(&p) {
            Some(vscdb) => vec![TraeDb { root: p, vscdb }],
            None => Vec::new(),
        };
    }

    let mut out: Vec<TraeDb> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for root in data_dirs_under(appdata) {
        let Some(vscdb) = vscdb_under(&root) else {
            continue;
        };
        let canon = std::fs::canonicalize(&vscdb).unwrap_or_else(|_| vscdb.clone());
        if seen.contains(&canon) {
            continue;
        }
        seen.push(canon);
        out.push(TraeDb { root, vscdb });
    }
    out
}

/// 覆盖值（未设置 ⇒ `None`）。
fn env_override() -> Option<String> {
    std::env::var(ENV_DATA_DIR).ok()
}

/// 当前要处理的库列表（环境变量覆盖优先）。
fn resolve_dbs() -> Vec<TraeDb> {
    resolve_dbs_with(
        env_override().as_deref(),
        &std::env::var("APPDATA").unwrap_or_default(),
    )
}

/// 候选应用数据目录（Lead 点名要求回报的就是这个）。
pub fn trae_data_dirs() -> Vec<PathBuf> {
    resolve_dbs().into_iter().map(|d| d.root).collect()
}

/// 实际要读写的 `state.vscdb` 路径列表（已去重、只含真实存在的文件）。
pub fn trae_vscdb_paths() -> Vec<PathBuf> {
    resolve_dbs().into_iter().map(|d| d.vscdb).collect()
}

// ---------------------------------------------------------------------------
// 条目构造
// ---------------------------------------------------------------------------

fn default_features() -> Value {
    json!({
        "provider": { "enable": true, "data": { "provider_name": PROVIDER } },
        "context_windows": {
            "enable": true,
            "data": {
                "dev_context": null,
                "max_context": null,
                "max_context_list": null,
                "dev_turns": null,
                "max_turns": null,
            }
        }
    })
}

fn default_icon() -> Value {
    json!({
        "light": "https://lf-cdn.trae.com.cn/obj/trae-com-cn/model/default-custom-light.svg",
        "dark": "https://lf-cdn.trae.com.cn/obj/trae-com-cn/model/default-custom-dark.svg",
    })
}

/// 构造一条 Trae 自定义模型条目。
///
/// `template` 传同库中已有的自定义模型条目时，会**整份照抄其字段**再覆盖受管字段 ——
/// 不同 Trae 版本、不同键的字段集合都不一样（实测冒号键 56 个字段、下划线键 54 个），
/// 硬编码字段集合会漏掉新版本新增的字段。
///
/// ★ 任务书给的签名是 `-> Value`；这里改成 `-> Result<Value, String>`，
/// 因为 [`encrypt_ak`] 可失败：静默降级会写出一个 `ak` 为空、**看起来正常但永远连不上**
/// 的条目，那比直接报错难查得多。
///
/// ★ `custom_model_id` 由服务端 RPC `api/ide/v1/add_custom_model` 下发，**不可复现**。
/// 策略（任务书指定）：模板里有就复用，没有就**省略该键** —— 绝不编造随机数。
/// 缺这个键不影响选择与使用（客户端去重键会退化，且比较函数对 `undefined` 返回真），
/// 只影响设置页里的「编辑/删除」。
///
/// ★ 模板由 [`find_template`] 选取，而它**优先取同名条目**。于是本函数的实际语义是：
///
/// - **覆盖已有条目**（库里已有 `custom_openai_compatible//<MODEL>`）：复用**它自己的**
///   `custom_model_id`。实测库里 id 与 name 一一对应（`kimi-k3` 恒为 `1992103298`、
///   `gpt-5.6-sol` 恒为 `2005145218`，跨 9+7 个组都不变），复用自己那份 = **身份保持**；
///   若改用别的模型的 id，客户端比较函数会判成「两个不同的模型」，是真实的语义漂移。
/// - **新模型**（库里没有同名条目）：退到「模板里有就复用、没有就省略」。
///
/// 换句话说：只有「新模型」才可能出现「借用同组其它条目的 id」这一情况，而它与
/// 「省略该键」相比没有额外风险 —— 两者都不改变既有条目的身份。
pub fn build_trae_model_entry(
    model: &str,
    base_url: &str,
    api_key: &str,
    template: Option<&Value>,
) -> Result<Value, String> {
    let ak = encrypt_ak(api_key)?;
    let name = format!("{NAME_PREFIX}{model}");

    let mut obj: Map<String, Value> = template
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let has_template = !obj.is_empty();

    // ── 受管字段：一律覆盖 ────────────────────────────────────────────
    obj.insert("name".to_string(), json!(name));
    obj.insert("display_name".to_string(), json!(model));
    obj.insert("provider".to_string(), json!(PROVIDER));
    obj.insert("base_url".to_string(), json!(base_url));
    obj.insert("is_custom_base_url".to_string(), json!(true));
    obj.insert("ak".to_string(), json!(ak));
    obj.insert("sk".to_string(), json!(""));
    obj.insert("auth_type".to_string(), json!(0));
    obj.insert("region".to_string(), Value::Null);
    obj.insert("client_connect".to_string(), json!(CLIENT_CONNECT));
    // ★ 不变量：`use_remote_service == !client_connect`。
    //
    // 实测客户端在**每次发请求时现场计算**该字段（`ai-modules-chat`：
    // `use_remote_service: !y.client_connect`），并不读落盘值；而且下划线键
    // （IDE 侧）的真实条目里根本没有这个字段。这里仍然**一律写入**：
    // 一是任务书把它列为必备不变量，二是将来若出现读盘路径，
    // `false` 才是与 `client_connect:true` 自洽的取值。写它至多是惰性数据。
    obj.insert(
        "use_remote_service".to_string(),
        json!(!CLIENT_CONNECT),
    );
    obj.insert("is_preset".to_string(), json!(false));
    obj.insert("config_source".to_string(), json!(3));
    obj.insert("model_type".to_string(), json!("chat_model"));
    obj.insert("multimodal".to_string(), json!(true));
    obj.insert("builder".to_string(), json!(true));
    obj.insert("status".to_string(), json!(true));
    obj.insert("selectable".to_string(), json!(true));
    obj.insert("is_default".to_string(), json!(false));
    obj.insert("custom_config".to_string(), json!(""));
    obj.insert("commercial_info".to_string(), Value::Null);

    // `config_name` 跟着模板走：下划线键的真实条目**没有**这个字段，
    // 硬塞一个该子系统从未见过的字段是不可验证的改动。
    // （`use_remote_service` 不适用这条 —— 它是任务书指定的不变量，故上面一律写。）
    if !has_template || obj.contains_key("config_name") {
        obj.insert("config_name".to_string(), json!(name));
    }

    // ── 无模板时补默认值（取自实测可用的 `gpt-5.6-sol` 条目）──────────
    if !has_template {
        obj.insert("custom_model_type".to_string(), json!(""));
        obj.insert("max_tokens".to_string(), json!(32768));
        obj.insert("max_turn".to_string(), json!(200));
        obj.insert("prompt_max_tokens".to_string(), json!(267232));
        obj.insert("features".to_string(), default_features());
        obj.insert("icon".to_string(), default_icon());
        obj.insert(
            "context_window_size".to_string(),
            json!({ "max": null, "default": null }),
        );
        obj.insert(
            "max_turns".to_string(),
            json!({ "default": null, "max": null }),
        );
        obj.insert(
            "saas_usage".to_string(),
            json!({ "max": null, "default": null }),
        );
        for key in NULL_FIELDS {
            obj.entry(key.to_string()).or_insert(Value::Null);
        }
    }

    Ok(Value::Object(obj))
}

/// 从已有组里挑一条自定义模型条目当模板。
///
/// `name` 是本次要写入的条目名（`custom_openai_compatible//<MODEL>`）。查找顺序：
///
/// 1. **同名条目**（`entry.name == name`）—— 它正是接下来要被**原地替换**的那一条；
/// 2. 其它 `provider == custom_openai_compatible` 条目；
/// 3. 任何 `provider` 以 `custom` 开头的条目；
/// 4. 都没有则 `None`（调用方走内置默认字段集）。
///
/// ## ★ 为什么「同名优先」不是锦上添花而是必须
///
/// 实测客户端 `@byted-icube/solo-lite` 的条目比较函数是
///
/// ```text
/// e.configSource===t.configSource && e.provider===t.provider && e.name===t.name &&
/// (e.customModelId===undefined || t.customModelId===undefined ||
///  e.customModelId===t.customModelId)
/// ```
///
/// —— `custom_model_id` **只在两边都非 `undefined` 时才参与比较**。于是：
///
/// - 「同名但换了 id」会被判成**两个不同的模型**（真实的语义漂移，实测已发生：
///   `kimi-k3` 从 `1992103298` 变成 `2005145218`）；
/// - 「其中一个 id 为 `undefined`」则一律相等。
///
/// 更关键的是，库里的 `custom_model_id` 与 `name` 是**一一对应**的：
/// 实测 9 个 SOLO 组与 7 个 IDE 组里 `kimi-k2.6` 恒为 `1262397954`、
/// `kimi-k3` 恒为 `1992103298`、`gpt-5.6-sol` 恒为 `2005145218`，
/// **同一个 id 从不跨 name 复用**。因此把「别的模型」的 id 复制给本条目的旧写法，
/// 会写出**库里从未出现过**的形态；而复制「它自己的」id 则是身份保持。
fn find_template<'a>(groups: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    let mut preferred: Option<&Value> = None;
    let mut fallback: Option<&Value> = None;
    for value in groups.values() {
        let Some(arr) = value.as_array() else {
            continue;
        };
        for entry in arr {
            // 1) 同名条目：优先级最高，命中即返回。
            if entry.get("name").and_then(Value::as_str) == Some(name) {
                return Some(entry);
            }
            let provider = entry
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if provider == PROVIDER {
                if preferred.is_none() {
                    preferred = Some(entry);
                }
            } else if fallback.is_none() && provider.starts_with("custom") {
                fallback = Some(entry);
            }
        }
    }
    preferred.or(fallback)
}

/// 在一个 `model_list_map` JSON 文本上追加/更新模型条目，返回新的 JSON 文本。
///
/// ★ **组名来源是输入 JSON 里「值本身是数组」的顶层键**，不硬编码任何组名 ——
/// 9 组 SOLO 库与 7 组 IDE 库都自动正确，也不会新增/删除组名。
///
/// 同名条目**原地替换**而不是追加：点两次「接入」不该产生重复条目
/// （其余 11 个客户端的导入同样是幂等的）。
///
/// 注意 `serde_json` 未启用 `preserve_order`，对象键在反序列化时即被排序，
/// 因此写出的文本里组名与字段名是字典序 —— 语义不变（客户端按名字取值），
/// 与 `switcher::vscdb` 的既有取舍一致。
fn upsert_into_map(
    existing: &str,
    models: &[String],
    base_url: &str,
    api_key: &str,
) -> Result<String, String> {
    let mut root: Value = serde_json::from_str(existing.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("model_list_map 不是合法 JSON: {e}"))?;
    let groups = root
        .as_object_mut()
        .ok_or_else(|| "model_list_map 顶层不是对象".to_string())?;

    // 模型在外层、组在内层：模板要**按模型逐个取**（同名条目优先），
    // 因此不能像旧实现那样只取一次再复用。
    for model in models {
        let name = format!("{NAME_PREFIX}{model}");
        // 先取模板（把 `groups` 重借用为不可变）并 `cloned` 结束借用，随后才能 `values_mut`。
        let template: Option<Value> = find_template(groups, &name).cloned();
        let entry = build_trae_model_entry(model, base_url, api_key, template.as_ref())?;
        let name_value = entry.get("name").cloned().unwrap_or(Value::Null);
        for value in groups.values_mut() {
            let Some(arr) = value.as_array_mut() else {
                continue;
            };
            match arr.iter().position(|e| e.get("name") == Some(&name_value)) {
                Some(index) => arr[index] = entry.clone(),
                None => arr.push(entry.clone()),
            }
        }
    }

    serde_json::to_string(&root).map_err(|e| format!("序列化 model_list_map 失败: {e}"))
}

// ---------------------------------------------------------------------------
// state.vscdb 读写
// ---------------------------------------------------------------------------

/// 找出库里全部模型表键（`…AI.agent.model.model_list_map`）。
///
/// 前缀是登录 uid，分隔符可能是 `:`（SOLO 侧）或 `_`（IDE 侧），故按后缀匹配。
/// 行数只有几十到几百，直接全表扫再在 Rust 侧过滤 ——
/// 用 `LIKE` 的话键里的 `_` 会被当成单字符通配符。
fn model_map_keys(conn: &rusqlite::Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT key FROM ItemTable")
        .map_err(|e| format!("查询 ItemTable 失败: {e}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("读取 ItemTable 失败: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        let key = row.map_err(|e| format!("读取 ItemTable 失败: {e}"))?;
        if key.ends_with(KEY_SUFFIX) {
            out.push(key);
        }
    }
    out.sort();
    Ok(out)
}

/// 读一个键的文本值 + 原存储类型（BLOB ⇒ `true`）。
fn read_value(conn: &rusqlite::Connection, key: &str) -> Result<Option<(String, bool)>, String> {
    let mut stmt = conn
        .prepare("SELECT value FROM ItemTable WHERE key = ?1")
        .map_err(|e| format!("查询 {key} 失败: {e}"))?;
    let mut rows = stmt
        .query(rusqlite::params![key])
        .map_err(|e| format!("读取 {key} 失败: {e}"))?;
    let Some(row) = rows.next().map_err(|e| format!("读取 {key} 失败: {e}"))? else {
        return Ok(None);
    };
    let value = row
        .get::<_, rusqlite::types::Value>(0)
        .map_err(|e| format!("读取 {key} 失败: {e}"))?;
    Ok(match value {
        rusqlite::types::Value::Text(s) => Some((s, false)),
        // 值列在真实库里两种存储类都出现过；非 UTF-8 的 BLOB 不是模型表，保守跳过。
        rusqlite::types::Value::Blob(b) => String::from_utf8(b).ok().map(|s| (s, true)),
        _ => None,
    })
}

/// 写回一个键，**保持原存储类型**。
fn write_value(
    conn: &rusqlite::Connection,
    key: &str,
    text: &str,
    blob: bool,
) -> Result<(), String> {
    const SQL: &str = "INSERT INTO ItemTable (key, value) VALUES (?1, ?2) \
                       ON CONFLICT(key) DO UPDATE SET value = excluded.value";
    let result = if blob {
        conn.execute(SQL, rusqlite::params![key, text.as_bytes()])
    } else {
        conn.execute(SQL, rusqlite::params![key, text])
    };
    result
        .map(|_| ())
        .map_err(|e| format!("写入 {key} 失败: {e}"))
}

/// 把一个网关写进某个 `state.vscdb` 的全部模型表键，返回写入的键数。
///
/// 返回 `0` 表示该库里没有模型表键 —— **此时不做任何修改**。
fn write_db(db: &Path, base_url: &str, api_key: &str, models: &[String]) -> Result<usize, String> {
    if !db.is_file() {
        return Err(format!("state.vscdb 不存在: {}", db.display()));
    }
    // 显式 READ_WRITE（**不带 CREATE**）：路径写错时宁可报错，
    // 也不要在磁盘上凭空造一个空库出来。
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .map_err(|e| format!("打开 {} 失败: {e}", db.display()))?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));

    let keys = model_map_keys(&conn)?;
    if keys.is_empty() {
        return Ok(0);
    }

    let mut written = 0usize;
    for key in keys {
        let Some((text, blob)) = read_value(&conn, &key)? else {
            continue;
        };
        let new_text = upsert_into_map(&text, models, base_url, api_key)?;
        if new_text == text {
            continue;
        }
        write_value(&conn, &key, &new_text, blob)?;
        written += 1;
    }
    Ok(written)
}

/// 该库是否已经接入本网关（UI 的「已配置」标记）。
///
/// 判据：某个模型表键里存在 `provider == custom_openai_compatible`、
/// `base_url` 指向本网关、且 `ak` 能解回本网关 api_key 的条目。
/// 先做一次廉价的子串过滤再解析 JSON —— 大多数机器上这里要读近 400 KB 文本。
pub fn is_configured(vscdb: &Path, gateway_base: &str, api_key: &str) -> bool {
    let base = gateway_base.trim().trim_end_matches('/');
    if base.is_empty() {
        return false;
    }
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        vscdb,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return false;
    };
    let Ok(keys) = model_map_keys(&conn) else {
        return false;
    };
    for key in keys {
        let Ok(Some((text, _))) = read_value(&conn, &key) else {
            continue;
        };
        if !text.contains(PROVIDER) || !text.contains(base) {
            continue;
        }
        let Ok(root) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) else {
            continue;
        };
        let Some(groups) = root.as_object() else {
            continue;
        };
        for value in groups.values() {
            let Some(arr) = value.as_array() else {
                continue;
            };
            for entry in arr {
                if entry.get("provider").and_then(Value::as_str) != Some(PROVIDER) {
                    continue;
                }
                let url = entry
                    .get("base_url")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !url.starts_with(base) {
                    continue;
                }
                let ak = entry.get("ak").and_then(Value::as_str).unwrap_or_default();
                if decrypt_ak(ak).as_deref() == Ok(api_key) {
                    return true;
                }
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// 导入入口
// ---------------------------------------------------------------------------

/// 把网关接入本机的 Trae（Trae Work / Trae CN 全写）。
pub fn import_trae(
    gateway_base: &str,
    api_key: &str,
    models: &[String],
) -> Result<ImportOutcome, String> {
    import_trae_into(&trae_vscdb_paths(), gateway_base, api_key, models)
}

/// [`import_trae`] 的纯内核：显式接收库路径，便于用临时库做测试。
///
/// ★ 单独拆出来的另一个原因：**任何单元测试都不得调用 [`import_trae`]** ——
/// 未设 [`ENV_DATA_DIR`] 时它会解析到所有者的**真实**库并写入。
fn import_trae_into(
    dbs: &[PathBuf],
    gateway_base: &str,
    api_key: &str,
    models: &[String],
) -> Result<ImportOutcome, String> {
    if api_key.trim().is_empty() {
        return Err("请先在网关设置里填写 API Key（客户端必须携带凭据）".to_string());
    }
    let base = gateway_base.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("网关地址为空，无法写入 Trae 模型库".to_string());
    }
    let base_url = format!("{base}{CHAT_PATH}");
    let models: Vec<String> = if models.is_empty() {
        vec!["deepseek-v4-flash".to_string()]
    } else {
        models.to_vec()
    };

    let existing: Vec<PathBuf> = dbs.iter().filter(|p| p.is_file()).cloned().collect();
    if existing.is_empty() {
        return Err(format!(
            "未检测到 Trae（%APPDATA%\\TRAE SOLO CN 或 %APPDATA%\\Trae CN 下没有 state.vscdb；可用 {ENV_DATA_DIR} 指定数据目录）"
        ));
    }

    let backup = backup_files("trae", &existing)?;

    let mut written: Vec<String> = Vec::new();
    let mut keys_total = 0usize;
    for db in &existing {
        let count = write_db(db, &base_url, api_key, &models)?;
        if count > 0 {
            keys_total += count;
            written.push(db.to_string_lossy().to_string());
        }
    }
    if keys_total == 0 {
        return Err(format!(
            "Trae 的 state.vscdb 里没有 {KEY_SUFFIX} 键（未做任何修改）"
        ));
    }

    Ok(ImportOutcome {
        target: "trae".to_string(),
        backup_dir: backup.to_string_lossy().to_string(),
        files: written,
        models,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// 黄金向量：**不含任何真实凭据**，可安全进公开仓库。
    /// 由 Node/OpenSSL 独立实现产出并反向自检通过（`test-bin/trae-agent-test/golden.mjs`）。
    const GOLDEN_NONCE_HEX: &str = "000102030405060708090a0b";
    const GOLDEN_PLAIN: &str = "sk-golden-vector-0123456789";
    const GOLDEN_AK: &str = "AAECAwQFBgcICQoLhgfVv9l6gvQTLMRyY8rXjyd3znO1wdlDFpSz0wWG1DB+LDAyOButellbDQ==";
    /// 实测派生密钥（任务书给的同一串，独立验证过）。
    const GOLDEN_KEY_HEX: &str = "1747551c425f1d4e6c6b6c6e30717332747578796a6b396c6e30717332747535";

    const TEST_BASE_URL: &str = "http://127.0.0.1:7864/v1/chat/completions";

    /// IDE 侧实测的 7 个组（**按字节序**，便于与序列化后的键序比对）。
    ///
    /// 注意 `code_review_summary` 排在 `code_reviewer` **之前**：`serde_json`
    /// 默认用 `BTreeMap`，比的是字节，`_`(0x5F) < `r`(0x72)。
    const IDE_GROUPS: [&str; 7] = [
        "builder",
        "builder_v3",
        "chat_v3",
        "code_review_summary",
        "code_reviewer",
        "refactor",
        "solo_agent",
    ];
    /// SOLO 侧实测的 9 个组（**按字节序**）。
    const SOLO_GROUPS: [&str; 9] = [
        "agent",
        "assistant",
        "solo_agent_lite",
        "solo_agent_remote",
        "solo_coder",
        "solo_design_lite",
        "solo_design_remote",
        "solo_work_lite",
        "solo_work_remote",
    ];

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ai-gateway-trae-agent-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一个「每组一条预设模型」的 model_list_map。
    fn map_with_groups(groups: &[&str]) -> String {
        let mut obj = Map::new();
        for g in groups {
            obj.insert(
                (*g).to_string(),
                json!([{ "name": "preset-model", "provider": "trae", "base_url": "https://example.com" }]),
            );
        }
        serde_json::to_string(&Value::Object(obj)).unwrap()
    }

    fn group_len(v: &Value, group: &str) -> usize {
        v.get(group)
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0)
    }

    // ── 加密 ─────────────────────────────────────────────────────────

    #[test]
    fn 派生密钥与客户端实测一致() {
        assert_eq!(to_hex(&derived_key()), GOLDEN_KEY_HEX);
    }

    #[test]
    fn 固定nonce加密等于黄金向量且可解回明文() {
        let nonce = decode_hex(GOLDEN_NONCE_HEX).unwrap();
        let ak = encrypt_ak_with_nonce(GOLDEN_PLAIN, &nonce).unwrap();
        assert_eq!(ak, GOLDEN_AK, "加密结果必须与独立实现（Node/OpenSSL）逐字节相同");
        assert_eq!(decrypt_ak(GOLDEN_AK).unwrap(), GOLDEN_PLAIN);
    }

    #[test]
    fn 加解密往返对随机nonce与多语言明文都成立() {
        for plain in [
            "sk-abc-123",
            "含中文的密钥-0123456789",
            "emoji-🔐-key",
            "",
            &"x".repeat(300),
        ] {
            let ak = encrypt_ak(plain).unwrap();
            assert_eq!(decrypt_ak(&ak).unwrap(), plain, "往返失败: {plain:?}");
        }
    }

    #[test]
    fn 同一明文两次加密的密文不同但都能解回() {
        let a = encrypt_ak(GOLDEN_PLAIN).unwrap();
        let b = encrypt_ak(GOLDEN_PLAIN).unwrap();
        assert_ne!(a, b, "随机 nonce 必须让密文不同（否则等于泄露明文相等性）");
        assert_eq!(decrypt_ak(&a).unwrap(), GOLDEN_PLAIN);
        assert_eq!(decrypt_ak(&b).unwrap(), GOLDEN_PLAIN);
    }

    #[test]
    fn 过短或非法base64的密文被拒绝() {
        assert!(decrypt_ak("").is_err());
        assert!(decrypt_ak("not base64!!!").is_err());
        // 27 字节 < 12 + 16，必须走「太短」分支而不是 panic
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 27]);
        assert!(decrypt_ak(&short).is_err());
    }

    #[test]
    fn 篡改密文会被认证标签拒绝() {
        let mut buf = base64::engine::general_purpose::STANDARD
            .decode(GOLDEN_AK)
            .unwrap();
        let mid = NONCE_LEN + 2;
        buf[mid] ^= 0x01;
        let tampered = base64::engine::general_purpose::STANDARD.encode(&buf);
        assert!(decrypt_ak(&tampered).is_err(), "GCM 必须拒绝被篡改的密文");
    }

    // ── 组名处理 ─────────────────────────────────────────────────────

    #[test]
    fn 只有七个IDE组的库写入后仍是七组且每组多一条() {
        let out = upsert_into_map(
            &map_with_groups(&IDE_GROUPS),
            &["m1".to_string(), "m2".to_string()],
            TEST_BASE_URL,
            "gw-key",
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let groups = v.as_object().unwrap();

        assert_eq!(groups.len(), 7, "组数不得变化");
        let names: Vec<&str> = groups.keys().map(String::as_str).collect();
        assert_eq!(names, IDE_GROUPS.to_vec(), "组名集合必须原样保留");
        for g in IDE_GROUPS {
            assert_eq!(group_len(&v, g), 3, "组 {g} 应多出 2 条（1 预设 + 2 新增）");
        }
        // 反向：绝不能把 SOLO 组名硬塞进 IDE 库
        for g in SOLO_GROUPS {
            assert!(
                !groups.contains_key(g),
                "IDE 库不得凭空多出 SOLO 组 {g}"
            );
        }
    }

    #[test]
    fn 九个SOLO组的库写入后九组都多一条() {
        let out = upsert_into_map(
            &map_with_groups(&SOLO_GROUPS),
            &["m1".to_string()],
            TEST_BASE_URL,
            "gw-key",
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let groups = v.as_object().unwrap();

        assert_eq!(groups.len(), 9);
        assert_eq!(
            groups.keys().map(String::as_str).collect::<Vec<_>>(),
            SOLO_GROUPS.to_vec()
        );
        for g in SOLO_GROUPS {
            assert_eq!(group_len(&v, g), 2, "组 {g} 应多出 1 条");
        }
    }

    #[test]
    fn 非数组的顶层键被原样保留且不追加条目() {
        let mut obj = Map::new();
        obj.insert("builder".to_string(), json!([]));
        obj.insert("some_scalar".to_string(), json!("keep-me"));
        obj.insert("some_object".to_string(), json!({ "a": 1 }));
        let text = serde_json::to_string(&Value::Object(obj)).unwrap();

        let out = upsert_into_map(&text, &["m1".to_string()], TEST_BASE_URL, "k").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["some_scalar"], json!("keep-me"));
        assert_eq!(v["some_object"], json!({ "a": 1 }));
        assert_eq!(group_len(&v, "builder"), 1);
    }

    #[test]
    fn 重复导入是幂等的而不是不断追加() {
        let once = upsert_into_map(
            &map_with_groups(&IDE_GROUPS),
            &["m1".to_string()],
            TEST_BASE_URL,
            "k",
        )
        .unwrap();
        let twice = upsert_into_map(&once, &["m1".to_string()], TEST_BASE_URL, "k").unwrap();
        let v: Value = serde_json::from_str(&twice).unwrap();
        for g in IDE_GROUPS {
            assert_eq!(group_len(&v, g), 2, "重复导入不得产生重复条目（组 {g}）");
        }
    }

    #[test]
    fn 顶层不是对象时拒绝写入() {
        assert!(upsert_into_map("[1,2,3]", &["m".to_string()], TEST_BASE_URL, "k").is_err());
        assert!(upsert_into_map("not json", &["m".to_string()], TEST_BASE_URL, "k").is_err());
    }

    // ── 条目字段 ─────────────────────────────────────────────────────

    #[test]
    fn 不变量use_remote_service等于非client_connect() {
        let entry = build_trae_model_entry("m", TEST_BASE_URL, "gw-key", None).unwrap();
        let client_connect = entry["client_connect"].as_bool().unwrap();
        assert!(client_connect, "必须强制客户端直连");
        assert_eq!(
            entry["use_remote_service"].as_bool().unwrap(),
            !client_connect,
            "use_remote_service 必须等于 !client_connect"
        );
    }

    #[test]
    fn 新条目的ak能解回网关api_key且base_url是完整chat路径() {
        let entry = build_trae_model_entry("deepseek-v4-flash", TEST_BASE_URL, "gw-key", None).unwrap();
        assert_eq!(entry["base_url"], json!(TEST_BASE_URL));
        assert!(entry["base_url"].as_str().unwrap().ends_with("/v1/chat/completions"));
        assert_eq!(entry["ak"].as_str().unwrap().len() % 4, 0);
        assert_eq!(decrypt_ak(entry["ak"].as_str().unwrap()).unwrap(), "gw-key");
    }

    #[test]
    fn 无模板时字段齐全且不编造custom_model_id() {
        let entry = build_trae_model_entry("m", TEST_BASE_URL, "k", None).unwrap();
        let obj = entry.as_object().unwrap();
        for key in [
            "name",
            "config_name",
            "display_name",
            "provider",
            "base_url",
            "is_custom_base_url",
            "ak",
            "sk",
            "auth_type",
            "region",
            "client_connect",
            "use_remote_service",
            "is_preset",
            "config_source",
            "custom_model_type",
            "model_type",
            "multimodal",
            "builder",
            "status",
            "selectable",
            "is_default",
            "custom_config",
            "max_tokens",
            "max_turn",
            "prompt_max_tokens",
            "context_window_size",
            "max_turns",
            "saas_usage",
            "commercial_info",
            "features",
            "icon",
        ] {
            assert!(obj.contains_key(key), "缺字段 {key}");
        }
        for key in NULL_FIELDS {
            assert!(obj.contains_key(key), "缺 null 字段 {key}");
        }
        assert_eq!(obj["name"], json!("custom_openai_compatible//m"));
        assert_eq!(obj["config_name"], obj["name"]);
        assert_eq!(obj["config_source"], json!(3));
        assert_eq!(obj["is_preset"], json!(false));
        assert_eq!(obj["sk"], json!(""));
        assert_eq!(obj["auth_type"], json!(0));
        assert!(obj["region"].is_null());
        assert!(
            obj.get("custom_model_id").is_none(),
            "custom_model_id 由服务端下发，无模板时必须省略而不是编造"
        );
    }

    #[test]
    fn 模板字段被整份照抄但受管字段被覆盖() {
        let template = json!({
            "name": "custom_openai_compatible//gpt-5.6-sol",
            "config_name": "custom_openai_compatible//gpt-5.6-sol",
            "provider": "custom_openai_compatible",
            "custom_model_id": "2005145218",
            "max_tokens": 32768,
            "max_turn": 200,
            "prompt_max_tokens": 267232,
            "future_field_we_do_not_know": { "a": 1 },
        });
        let entry = build_trae_model_entry("new-model", TEST_BASE_URL, "k", Some(&template)).unwrap();

        assert_eq!(entry["custom_model_id"], json!("2005145218"), "复用同库已有 id");
        assert_eq!(entry["future_field_we_do_not_know"], json!({ "a": 1 }));
        assert_eq!(entry["max_tokens"], json!(32768), "模板的容量参数应保留");
        assert_eq!(entry["name"], json!("custom_openai_compatible//new-model"));
        assert_eq!(entry["config_name"], entry["name"]);
        assert_eq!(entry["display_name"], json!("new-model"));
        assert_eq!(entry["base_url"], json!(TEST_BASE_URL));
        assert_eq!(decrypt_ak(entry["ak"].as_str().unwrap()).unwrap(), "k");
    }

    #[test]
    fn 下划线键形态的模板不引入config_name但仍然写不变量字段() {
        // 实测 IDE 侧条目**没有** config_name / use_remote_service
        let template = json!({
            "name": "custom_openai_compatible//kimi-k3",
            "provider": "custom_openai_compatible",
            "custom_model_id": "1992103298",
            "max_tokens": 32000,
        });
        let entry = build_trae_model_entry("m", TEST_BASE_URL, "k", Some(&template)).unwrap();
        assert!(
            entry.get("config_name").is_none(),
            "模板没有的字段不该硬塞（下划线键的真实条目就没有 config_name）"
        );
        assert!(
            entry.get("use_remote_service").is_some(),
            "use_remote_service 是任务书指定的不变量，一律写入"
        );
    }

    #[test]
    fn 模板优先取custom_openai_compatible其次任何custom前缀() {
        // 本次要写入的条目名：库里没有同名条目，故走「按 provider 挑模板」的老路径。
        let wanted = "custom_openai_compatible//brand-new-model";

        let mut groups = Map::new();
        groups.insert(
            "a".to_string(),
            json!([
                { "name": "preset", "provider": "trae" },
                { "name": "other-custom", "provider": "custom_anthropic", "marker": "fallback" },
            ]),
        );
        groups.insert(
            "b".to_string(),
            json!([{ "name": "ours", "provider": "custom_openai_compatible", "marker": "preferred" }]),
        );
        let t = find_template(&groups, wanted).unwrap();
        assert_eq!(t["marker"], json!("preferred"));

        let mut only_fallback = Map::new();
        only_fallback.insert(
            "a".to_string(),
            json!([
                { "name": "preset", "provider": "trae" },
                { "name": "other-custom", "provider": "custom_anthropic", "marker": "fallback" },
            ]),
        );
        assert_eq!(
            find_template(&only_fallback, wanted).unwrap()["marker"],
            json!("fallback")
        );

        let mut none = Map::new();
        none.insert("a".to_string(), json!([{ "name": "preset", "provider": "trae" }]));
        assert!(find_template(&none, wanted).is_none());
    }

    /// ★ 同名条目必须压过「任何其它 `custom_openai_compatible` 条目」。
    ///
    /// 实测依据：库里的 `custom_model_id` 与 `name` 一一对应
    ///（`kimi-k3` 恒为 `1992103298`、`gpt-5.6-sol` 恒为 `2005145218`，跨 9+7 个组不变），
    /// 而客户端比较函数只在**两边 id 都非 `undefined`** 时才比较 id —— 把别的模型的 id
    /// 复制给同名条目会让客户端把它判成「另一个模型」，是真实的语义漂移。
    #[test]
    fn 同名条目压过其它custom_openai_compatible条目() {
        let wanted = "custom_openai_compatible//kimi-k3";
        let mut groups = Map::new();
        // 组 a 里先放一条**别的** `custom_openai_compatible` 条目（带它自己的 id）。
        groups.insert(
            "a".to_string(),
            json!([{
                "name": "custom_openai_compatible//gpt-5.6-sol",
                "provider": "custom_openai_compatible",
                "custom_model_id": "2005145218",
                "marker": "other-custom-entry",
            }]),
        );
        // 组 b 里才是同名条目，且组名在字典序上靠后 —— 证明不是「先遍历到谁就用谁」。
        groups.insert(
            "b".to_string(),
            json!([{
                "name": "custom_openai_compatible//kimi-k3",
                "provider": "custom_openai_compatible",
                "custom_model_id": "1992103298",
                "marker": "same-name",
            }]),
        );
        let t = find_template(&groups, wanted).unwrap();
        assert_eq!(
            t["marker"],
            json!("same-name"),
            "同名条目必须优先，否则会把别的模型的 custom_model_id 复制过来"
        );
        assert_eq!(t["custom_model_id"], json!("1992103298"));
    }

    /// ★ 端到端语义：覆盖已有条目时 `custom_model_id` 必须保持**它自己**那份不变。
    #[test]
    fn 覆盖已有条目时保持它自己的custom_model_id不变() {
        let text = json!({
            "solo_work_lite": [
                {
                    "name": "custom_openai_compatible//gpt-5.6-sol",
                    "provider": "custom_openai_compatible",
                    "custom_model_id": "2005145218",
                },
                {
                    "name": "custom_openai_compatible//kimi-k3",
                    "provider": "custom_openai_compatible",
                    "custom_model_id": "1992103298",
                    "base_url": "https://new-api.suyuansoft.com/v1/chat/completions",
                },
            ],
            "solo_coder": [
                {
                    "name": "custom_openai_compatible//kimi-k3",
                    "provider": "custom_openai_compatible",
                    "custom_model_id": "1992103298",
                },
            ],
        })
        .to_string();

        let out = upsert_into_map(&text, &["kimi-k3".to_string()], TEST_BASE_URL, "k").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        for g in ["solo_work_lite", "solo_coder"] {
            let entry = v[g]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["name"] == json!("custom_openai_compatible//kimi-k3"))
                .unwrap();
            assert_eq!(
                entry["custom_model_id"],
                json!("1992103298"),
                "覆盖同名条目时必须复用它自己的 id（组 {g}）"
            );
            assert_eq!(entry["base_url"], json!(TEST_BASE_URL), "受管字段仍要被覆盖");
        }
        // 没被请求的条目必须原样保留（含它自己的 id）。
        let untouched = v["solo_work_lite"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == json!("custom_openai_compatible//gpt-5.6-sol"))
            .unwrap();
        assert_eq!(untouched["custom_model_id"], json!("2005145218"));
    }

    /// ★ 新模型（库里没有同名条目）沿用老策略：模板里有 id 就复用，没有就省略该键。
    #[test]
    fn 新模型借用模板的custom_model_id而库里没有同名条目时() {
        let text = json!({
            "solo_work_lite": [{
                "name": "custom_openai_compatible//kimi-k3",
                "provider": "custom_openai_compatible",
                "custom_model_id": "1992103298",
            }],
        })
        .to_string();
        let out = upsert_into_map(&text, &["deepseek-v4-flash".to_string()], TEST_BASE_URL, "k")
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let added = v["solo_work_lite"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == json!("custom_openai_compatible//deepseek-v4-flash"))
            .unwrap();
        assert_eq!(added["custom_model_id"], json!("1992103298"));
        // 而「库里完全没有 custom 条目」时必须省略该键（不编造）。
        let bare = json!({ "solo_work_lite": [{ "name": "preset", "provider": "trae" }] }).to_string();
        let out2 = upsert_into_map(&bare, &["m".to_string()], TEST_BASE_URL, "k").unwrap();
        let v2: Value = serde_json::from_str(&out2).unwrap();
        let added2 = v2["solo_work_lite"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == json!("custom_openai_compatible//m"))
            .unwrap();
        assert!(added2.get("custom_model_id").is_none(), "无模板时绝不编造 id");
    }

    // ── 目录解析 ─────────────────────────────────────────────────────

    /// 造一个 `<appdata>\<name>\User\globalStorage\state.vscdb`。
    fn make_app(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        let gs = dir.join("User").join("globalStorage");
        std::fs::create_dir_all(&gs).unwrap();
        std::fs::write(gs.join("state.vscdb"), b"SQLite format 3\0").unwrap();
        dir
    }

    #[test]
    fn 候选目录只有TRAE_SOLO_CN与Trae_CN且排除无后缀Trae() {
        let root = temp_dir("appdata");
        let solo_cn = make_app(&root, "TRAE SOLO CN");
        let trae_cn = make_app(&root, "Trae CN");
        // 陈旧残留与旧目录名都要排除
        make_app(&root, "Trae");
        make_app(&root, "TRAE SOLO");

        let dirs = data_dirs_under(&root.to_string_lossy());
        assert_eq!(dirs, vec![solo_cn, trae_cn]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 只有目录没有库文件时不作为候选() {
        let root = temp_dir("appdata-empty");
        std::fs::create_dir_all(root.join("TRAE SOLO CN")).unwrap();
        std::fs::create_dir_all(root.join("Trae CN")).unwrap();
        assert!(data_dirs_under(&root.to_string_lossy()).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 无覆盖值时解析出两个库() {
        let root = temp_dir("appdata-two");
        make_app(&root, "TRAE SOLO CN");
        make_app(&root, "Trae CN");
        let dbs = resolve_dbs_with(None, &root.to_string_lossy());
        assert_eq!(dbs.len(), 2);
        assert!(dbs[0].vscdb.ends_with("User/globalStorage/state.vscdb"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 覆盖值指向单个库文件时直接使用该文件() {
        let root = temp_dir("override-file");
        let file = root.join("state.vscdb");
        std::fs::write(&file, b"SQLite format 3\0").unwrap();
        // 同一个根下也放一个「真实候选」，用来证明覆盖值优先
        make_app(&root, "TRAE SOLO CN");

        let dbs = resolve_dbs_with(Some(&file.to_string_lossy()), &root.to_string_lossy());
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].vscdb, file);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 覆盖值指向平铺目录时找到其中的state_vscdb() {
        let root = temp_dir("override-flat");
        let flat = root.join("copy-dir");
        std::fs::create_dir_all(&flat).unwrap();
        let db = flat.join("state.vscdb");
        std::fs::write(&db, b"SQLite format 3\0").unwrap();

        let dbs = resolve_dbs_with(Some(&flat.to_string_lossy()), &root.to_string_lossy());
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].vscdb, db);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 覆盖值指向标准数据目录时解析到嵌套库() {
        let root = temp_dir("override-nested");
        let app = make_app(&root, "TRAE SOLO CN");
        let dbs = resolve_dbs_with(Some(&app.to_string_lossy()), &root.to_string_lossy());
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].vscdb, vscdb_path(&app));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ★ 安全底线：覆盖值无效时**必须什么都不写**，绝不能回落真实目录。
    #[test]
    fn 覆盖值无效时返回空列表而绝不回落真实目录() {
        let root = temp_dir("override-bogus");
        make_app(&root, "TRAE SOLO CN");
        make_app(&root, "Trae CN");

        for bogus in [
            "Z:\\definitely\\missing\\path",
            "not-a-real-path-at-all",
            "   ",
            "",
        ] {
            let dbs = resolve_dbs_with(Some(bogus), &root.to_string_lossy());
            assert!(
                dbs.is_empty(),
                "覆盖值 {bogus:?} 无效时必须返回空列表（否则会写所有者的真实库）"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── 库读写 ───────────────────────────────────────────────────────

    /// 建一个真实结构的临时库；`text_keys` 里的键按 TEXT 存，其余按 BLOB 存。
    fn make_db(dir: &Path, keys: &[(&str, &str)], text_keys: &[&str]) -> PathBuf {
        let db = dir.join("state.vscdb");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
        )
        .unwrap();
        for (key, value) in keys {
            if text_keys.contains(key) {
                conn.execute(
                    "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                    rusqlite::params![key, value],
                )
                .unwrap();
            } else {
                conn.execute(
                    "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                    rusqlite::params![key, value.as_bytes()],
                )
                .unwrap();
            }
        }
        drop(conn);
        db
    }

    fn typeof_value(db: &Path, key: &str) -> String {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.query_row(
            "SELECT typeof(value) FROM ItemTable WHERE key = ?1",
            rusqlite::params![key],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
    }

    fn read_text(db: &Path, key: &str) -> String {
        let conn = rusqlite::Connection::open(db).unwrap();
        let mut stmt = conn
            .prepare("SELECT value FROM ItemTable WHERE key = ?1")
            .unwrap();
        let mut rows = stmt.query(rusqlite::params![key]).unwrap();
        let row = rows.next().unwrap().unwrap();
        // 值列可能是 TEXT 也可能是 BLOB（真实库里两种都存在），
        // 直接 `get::<_, String>` 会在 BLOB 列上以 InvalidColumnType 失败。
        match row.get::<_, rusqlite::types::Value>(0).unwrap() {
            rusqlite::types::Value::Text(s) => s,
            rusqlite::types::Value::Blob(b) => String::from_utf8(b).unwrap(),
            other => panic!("值列既不是 TEXT 也不是 BLOB: {other:?}"),
        }
    }

    #[test]
    fn 两个键都被写入且各自保持原有存储类型() {
        let dir = temp_dir("db-two-keys");
        let colon = "2593120869167228:AI.agent.model.model_list_map";
        let under = "2593120869167228_AI.agent.model.model_list_map";
        let db = make_db(
            &dir,
            &[
                (colon, &map_with_groups(&SOLO_GROUPS)),
                (under, &map_with_groups(&IDE_GROUPS)),
                ("unrelated.key", "keep"),
            ],
            // 真实库里两个键都是 TEXT，这里故意把冒号键存成 BLOB 以验证「保持原类型」
            &[under],
        );
        assert_eq!(typeof_value(&db, colon), "blob");
        assert_eq!(typeof_value(&db, under), "text");

        let written = write_db(&db, TEST_BASE_URL, "gw-key", &["m1".to_string()]).unwrap();
        assert_eq!(written, 2, "两个键都必须被写入");

        assert_eq!(typeof_value(&db, colon), "blob", "BLOB 列必须仍是 BLOB");
        assert_eq!(typeof_value(&db, under), "text", "TEXT 列必须仍是 TEXT");

        let solo: Value = serde_json::from_str(&read_text(&db, colon)).unwrap();
        let ide: Value = serde_json::from_str(&read_text(&db, under)).unwrap();
        assert_eq!(solo.as_object().unwrap().len(), 9);
        assert_eq!(ide.as_object().unwrap().len(), 7);
        for g in SOLO_GROUPS {
            assert_eq!(group_len(&solo, g), 2);
        }
        for g in IDE_GROUPS {
            assert_eq!(group_len(&ide, g), 2);
        }
        // 无关键不受影响
        assert_eq!(read_text(&db, "unrelated.key"), "keep");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 库里没有模型表键时不写任何东西() {
        let dir = temp_dir("db-no-keys");
        let db = make_db(&dir, &[("something.else", "keep")], &["something.else"]);
        assert_eq!(write_db(&db, TEST_BASE_URL, "gw-key", &["m1".to_string()]).unwrap(), 0);
        assert_eq!(read_text(&db, "something.else"), "keep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 库文件不存在时报错而不是创建空库() {
        let dir = temp_dir("db-missing");
        let missing = dir.join("nope.vscdb");
        assert!(write_db(&missing, TEST_BASE_URL, "k", &["m".to_string()]).is_err());
        assert!(!missing.exists(), "绝不能凭空创建库文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_configured只有在本网关真被写入后才为真() {
        let dir = temp_dir("db-configured");
        let colon = "1:AI.agent.model.model_list_map";
        let db = make_db(&dir, &[(colon, &map_with_groups(&SOLO_GROUPS))], &[colon]);

        assert!(!is_configured(&db, "http://127.0.0.1:7864", "gw-key"));
        write_db(&db, TEST_BASE_URL, "gw-key", &["m1".to_string()]).unwrap();
        assert!(is_configured(&db, "http://127.0.0.1:7864", "gw-key"));
        // 网关地址或 api_key 对不上就不算「已接入」
        assert!(!is_configured(&db, "http://127.0.0.1:9999", "gw-key"));
        assert!(!is_configured(&db, "http://127.0.0.1:7864", "other-key"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 备份 ─────────────────────────────────────────────────────────

    /// 二进制库也必须能备份（`backup_files` 原本只走 `read_to_string`，
    /// 对 SQLite 会直接 `InvalidData` 报错 —— 这条测试就是那个修复的护栏）。
    #[test]
    fn 备份能原样保存二进制库并写出清单() {
        let dir = temp_dir("backup");
        let db = dir.join("state.vscdb");
        // 含非 UTF-8 字节，确保走的是二进制分支
        let payload: Vec<u8> = b"SQLite format 3\0".iter().copied().chain([0xff, 0xfe, 0x00, 0x80]).collect();
        std::fs::write(&db, &payload).unwrap();

        let backup = backup_files("trae", &[db.clone()]).unwrap();
        let saved = backup.join("0-state.vscdb");
        assert!(saved.is_file(), "备份文件必须存在: {}", saved.display());
        assert_eq!(std::fs::read(&saved).unwrap(), payload, "备份必须逐字节一致");
        assert!(backup.join("manifest.json").is_file(), "清单必须存在");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文本文件仍然走原来的文本路径（保证既有 11 个目标的备份逐字节不变）。
    #[test]
    fn 备份文本文件仍然逐字节一致() {
        let dir = temp_dir("backup-text");
        let f = dir.join("config.toml");
        let text = "[provider]\nkey = \"值\"\n";
        std::fs::write(&f, text).unwrap();
        let backup = backup_files("trae", &[f.clone()]).unwrap();
        assert_eq!(
            std::fs::read_to_string(backup.join("0-config.toml")).unwrap(),
            text
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 导入全流程（用临时库，绝不碰真实目录）────────────────────────

    #[test]
    fn 导入会备份并同时写入两个库的全部键() {
        let dir = temp_dir("import");
        let work = dir.join("work");
        let cn = dir.join("cn");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&cn).unwrap();
        let work_db = make_db(
            &work,
            &[
                ("1:AI.agent.model.model_list_map", &map_with_groups(&SOLO_GROUPS)),
                ("1_AI.agent.model.model_list_map", &map_with_groups(&IDE_GROUPS)),
            ],
            &[
                "1:AI.agent.model.model_list_map",
                "1_AI.agent.model.model_list_map",
            ],
        );
        let cn_db = make_db(
            &cn,
            &[("2_AI.agent.model.model_list_map", &map_with_groups(&IDE_GROUPS))],
            &["2_AI.agent.model.model_list_map"],
        );

        let outcome = import_trae_into(
            &[work_db.clone(), cn_db.clone()],
            "http://127.0.0.1:7864/",
            "gw-key",
            &["deepseek-v4-flash".to_string(), "kimi-k3".to_string()],
        )
        .unwrap();

        assert_eq!(outcome.target, "trae");
        assert_eq!(outcome.models.len(), 2);
        assert_eq!(outcome.files.len(), 2, "两个库都要写");
        assert!(std::path::Path::new(&outcome.backup_dir).is_dir(), "必须创建备份");

        // 9 组 + 7 组都各自多两条
        let solo: Value = serde_json::from_str(&read_text(&work_db, "1:AI.agent.model.model_list_map")).unwrap();
        for g in SOLO_GROUPS {
            assert_eq!(group_len(&solo, g), 3);
        }
        let ide: Value = serde_json::from_str(&read_text(&work_db, "1_AI.agent.model.model_list_map")).unwrap();
        for g in IDE_GROUPS {
            assert_eq!(group_len(&ide, g), 3);
        }
        let cn_ide: Value = serde_json::from_str(&read_text(&cn_db, "2_AI.agent.model.model_list_map")).unwrap();
        assert_eq!(cn_ide.as_object().unwrap().len(), 7);
        for g in IDE_GROUPS {
            assert_eq!(group_len(&cn_ide, g), 3);
        }

        // 备份里必须有一份「写入前」的库，且与写入后的库不同
        let backup_dir = std::path::PathBuf::from(&outcome.backup_dir);
        let before = std::fs::read_dir(&backup_dir)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().ends_with("state.vscdb"));
        assert!(before.is_some(), "备份目录里必须有库文件");
        assert_ne!(
            std::fs::read(before.unwrap().path()).unwrap(),
            std::fs::read(&work_db).unwrap(),
            "备份必须是写入前的状态"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 导入在没有任何库时报错() {
        let dir = temp_dir("import-none");
        let err = import_trae_into(&[], "http://127.0.0.1:7864", "gw-key", &["m".to_string()])
            .unwrap_err();
        assert!(err.contains("未检测到 Trae"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 导入在没有模型表键时报错且不改动库() {
        let dir = temp_dir("import-nokeys");
        let db = make_db(&dir, &[("x.y", "keep")], &["x.y"]);
        let err = import_trae_into(
            &[db.clone()],
            "http://127.0.0.1:7864",
            "gw-key",
            &["m".to_string()],
        )
        .unwrap_err();
        assert!(err.contains("没有"), "{err}");
        assert_eq!(read_text(&db, "x.y"), "keep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 导入拒绝空的api_key与空的网关地址() {
        let dir = temp_dir("import-badargs");
        let db = make_db(
            &dir,
            &[("1:AI.agent.model.model_list_map", &map_with_groups(&SOLO_GROUPS))],
            &["1:AI.agent.model.model_list_map"],
        );
        assert!(import_trae_into(&[db.clone()], "http://127.0.0.1:7864", "", &["m".to_string()]).is_err());
        assert!(import_trae_into(&[db.clone()], "   ", "k", &["m".to_string()]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 真实实例实测（默认跳过）──────────────────────────────────────

    /// 真实实例实测用的 API Key。
    ///
    /// 默认是一个明显的占位串（**不是**任何真实凭据），真实跑时由
    /// `AI_GATEWAY_TRAE_REAL_KEY` 注入 —— 这样密钥不会出现在仓库里，
    /// 也不影响其余单测。断言只比较「写进去的」与「解出来的」，所以
    /// 占位串同样能证明加解密闭环。
    fn api_key_for_real_run() -> String {
        std::env::var("AI_GATEWAY_TRAE_REAL_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sk-real-instance-probe".to_string())
    }

    /// 真实实例实测：**只在 [`ENV_DATA_DIR`] 被显式设置时才运行**。
    ///
    /// 未设置时立即返回 —— 这条守卫是**安全底线**：没有它，
    /// 普通 `cargo test --lib` 就会去写所有者正在使用的 `state.vscdb`。
    ///
    /// 用法（验收 ③）：
    /// ```text
    /// $env:AI_GATEWAY_TRAE_DATA_DIR = "D:\...\test-bin\trae-agent-test"
    /// cargo test -p ai-gateway-core --lib --offline trae_真实实例 -- --nocapture
    /// ```
    #[test]
    fn 真实实例导入只作用于覆盖目录() {
        let Ok(raw) = std::env::var(ENV_DATA_DIR) else {
            eprintln!("跳过：未设置 {ENV_DATA_DIR}");
            return;
        };
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            eprintln!("跳过：{ENV_DATA_DIR} 为空");
            return;
        }

        let dbs = trae_vscdb_paths();
        assert!(!dbs.is_empty(), "覆盖值 {raw} 下没有解析到 state.vscdb");
        // 双保险：解析出来的路径必须落在覆盖值之内
        let canon_root = std::fs::canonicalize(if std::path::Path::new(&raw).is_file() {
            std::path::Path::new(&raw).parent().unwrap().to_path_buf()
        } else {
            std::path::PathBuf::from(&raw)
        })
        .unwrap_or_else(|_| std::path::PathBuf::from(&raw));
        for db in &dbs {
            let canon = std::fs::canonicalize(db).unwrap_or_else(|_| db.clone());
            assert!(
                canon.starts_with(&canon_root),
                "解析出的库 {} 不在覆盖目录 {} 之内，拒绝继续",
                canon.display(),
                canon_root.display()
            );
        }

        let outcome = import_trae(
            "http://127.0.0.1:7864",
            &api_key_for_real_run(),
            &["deepseek-v4-flash".to_string(), "kimi-k3".to_string()],
        )
        .unwrap();

        eprintln!("真实实例导入结果: {outcome:?}");
        let key = api_key_for_real_run();
        for db in &dbs {
            assert!(
                is_configured(db, "http://127.0.0.1:7864", &key),
                "{} 写入后应被判定为已接入",
                db.display()
            );
        }
    }
}
