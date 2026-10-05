//! ZCode 账号凭证：解析、加载、落盘。
//!
//! 逐行移植 Go 侧 `wt-port\go-gateway\internal\zcode\cred.go`（919 行）。
//!
//! # 凭证形态（三种都要吃下）
//!
//! ```text
//! 嵌套形（宿主落盘）  {"credential":"…","provider":"zai","jwt":"…","nickname":"…"}
//! 扁平形（手写）      {"apiKey":"…","secret":"…"} 或 {"credential":"…"}
//! 裸字符串文件        {"…key.secret…"}（整个文件就是一个 JSON 字符串）
//! ```
//!
//! # 与 Go 侧的字段差异
//!
//! Go 的 `Cred` 有 `UID` 字段并**本地生成**：上游凭证是 `{apiKey}.{secret}`
//! 字符串，**没有账号 ID**（不像 Qoder 有 `user_id`）。但账号池需要一个稳定
//! 的主键来跟踪冷却、额度、到期等状态。生成规则见 [`cred_key`] —— 同一个凭证
//! 重复导入得到**相同** UID（幂等），而不是每次都新增一个账号。
//!
//! `Credential` 刻意**不做结构化拆分**（Go design.md D4）：上游要的就是这个
//! 整串；智谱的 secret 是**可选**的（单段也合法）；只有签名逻辑才需要拆分，
//! 而我们不实现签名。

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use regex::Regex;
use serde_json::{Map, Value};

use super::identity::{is_uuid, new_device_mid};
use super::provider::{domain_of_provider, parse_provider, Provider};

/// 一个 ZCode 账号的凭证。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZcodeCred {
    /// 本地账号标识（见模块文档：上游没有账号 ID，故本地生成）。
    pub uid: String,
    /// 昵称（用户可改）。
    pub nickname: String,
    /// 完整凭证字符串，形如 `{apiKey}.{secret}` 或 `{apiKey}`。
    pub credential: String,
    /// 服务商（`zai` / `bigmodel` / 未指定）。
    pub provider: Provider,
    /// 用户手动禁用：**只不接流量**。
    ///
    /// 由宿主的账号页「停止接流量」开关写入凭证文件的 `account.no_route`。
    ///
    /// 为什么用凭证文件传递而不是宿主的账号库：网关的池是**扫描 auths/ 目录**
    /// 建立的，它根本不读宿主的 accounts.json。若只改账号库，界面上的开关就是
    /// 个摆设 —— 网关照常把请求路由到这个账号。
    ///
    /// 命名与 WorkBuddy 侧一致（`no_route`），刻意**不用** `disabled`：池自己
    /// 也有一组 disabled（session 死 / 额度冻结），两者语义不同，混用会让
    /// 「禁用即停养号」那个 bug 复现。
    pub no_route: bool,
    /// start-plan 令牌（OAuth 登录时上游一并返回）。
    ///
    /// 用途：**额度查询**用它（不是 `credential`）。参考实现明确写了这一点。
    /// 若用户只导入了 credential（方式 B），则 JWT 为空 → 额度查不到，界面应
    /// 显示"未知"而不是 0（0 会被误读成"额度耗尽"）。
    pub jwt: String,
    /// 控制面请求（额度/账单）必需的设备标识（UUID 形态）。
    ///
    /// 上游对控制面要求 `X-Device-Mid`，**缺了直接 400 code=3001
    /// "parameter error"**（实测）。最初把这个 3001 误判成"需要 JWT"，于是界面
    /// 上做了「额度未知」—— 而实际上**只要补上这个头就能查到额度**。
    ///
    /// ⚠ 2026-09-21 修正：**不再无条件随机**。所有者报的现象是「官方不触发、
    /// 我们触发」，三方对照结果：
    ///
    /// ```text
    /// 官方客户端   固定值，持久化在 ~/.zcode/v2/telemetry-state.json
    /// 我们的凭证   没有 device_mid 字段
    /// 旧实现       每次进程启动 new_device_mid() 随机一个
    /// ```
    ///
    /// 每次重启换一个设备指纹，对上游风控而言就是**同一个账号被大量不同设备
    /// 使用** —— 这是判 "unusual activity" 的典型依据。
    ///
    /// 取用顺序：**凭证里的 > 官方客户端那个 > 随机**；取到前两者后**固化写回**
    ///（见 [`load_file`]），此后与外部文件无关。
    ///
    /// ⚠⚠ 二次修正（所有者纠正了一个方向性错误）：**一个账号一个设备标识**。
    /// 让所有账号共用官方那一个 deviceMid 会让上游看到「一台设备并发 20 个
    /// 账号」—— 那恰恰是**号商 / 脚本**的典型形状，比"多台设备各用一个账号"
    /// 可疑得多。故借用官方值的**唯一**条件是：这个凭证的账号**就是**官方客户端
    /// 登录的那个账号（见 [`mint_device_mid`]）。
    ///
    /// 注意它也用于**对话请求体**的 `metadata.user_id.device_id`，故不只是
    /// 控制面的事。
    pub device_mid: String,
    /// JWT 的签发时刻（Unix 秒）；0 = 未知。
    ///
    /// 参考实现说明：这个 JWT **没有 exp 字段**、不因时间过期 —— 8 天前签发的
    /// 仍能查额度。只有上游回 401/3012 才表示需要重新登录。故这里只记录年龄供
    /// 展示，**不据此判定过期**。
    pub jwt_issued_at: i64,
    /// 上游**账号**标识（从 JWT 的 `user_id` 解出）；空 = 未知。
    ///
    /// # 为什么需要它（这解决了"重复账号"）
    ///
    /// `uid` 是 **credential 的哈希**：同一把 key 重复导入会得到同一个 uid
    ///（幂等）。但**同一个上游账号可以有多把 key**：
    ///
    /// ```text
    /// 智谱账号 12345678901234567
    ///   ├─ key A → uid zcode-aaaa…（本地第 1 条账号）
    ///   └─ key B → uid zcode-bbbb…（本地第 2 条，用户看来就是"重复"）
    /// ```
    ///
    /// 两把 key 的哈希不同，故 uid 去重**抓不到**这种情况。记录 `account_id`
    /// 让界面能识别并标注。**不能自动删**：多把 key 可能是**故意的**（一把给
    /// 套餐、一把给别的用途），且各自额度可能不同 —— 只**标注**，由用户决定。
    pub account_id: String,
    /// 来源文件路径；保存时写回此处。
    pub file_path: String,
    /// 本次请求要带的验证码通过凭证（`X-Aliyun-Captcha-Verify-Param`）。
    ///
    /// ⚠ **一次性**：用一个就作废，用第二次上游必回 3007（实测）。故它不是
    /// "账号的属性"，而是"本次请求的瞬时值" —— 由调用方在发请求前现取一个填
    /// 进来，用完整理掉。放在 Cred 上只是因为它随请求一起流动，而 Cred 已经是
    /// 那个流动载体。
    pub captcha_param: String,
    /// 验证码所属区域（`X-Aliyun-Captcha-Verify-Region`）。
    /// 与 `captcha_param` 成对；实测缺它就是 3007。
    pub captcha_region: String,
    /// 加载时发现凭证里没有 `device_mid`，需要固化写回。
    ///
    /// 为什么不直接在 [`parse`] 里写：那里拿不到 `file_path`（它由
    /// [`load_file`] 在 [`parse`] 返回后才赋值），调用 [`ZcodeCred::save_atomic`]
    /// 必然失败（它的第一行就是「凭证没有来源路径，无法写回」）。故把"是否
    /// 需要固化"作为**显式信号**传出去，由知道路径的调用方执行。
    pub(crate) device_mid_needs_persist: bool,
}

impl ZcodeCred {
    /// 该凭证是否属于 **start-plan** 通道。
    ///
    /// # 为什么要区分通道（这是端点选择与凭证选择的前提）
    ///
    /// 上游有**两条**通道，端点、凭证形态、计费来源都不同：
    ///
    /// ```text
    /// 通道          端点                                       凭证
    /// ────────────  ─────────────────────────────────────────  ──────────────────
    /// start-plan    zcode.z.ai/api/v1/zcode-plan/anthropic      **jwt**
    /// coding-plan   {provider}/api/anthropic 或 …/coding/paas   `{apiKey}.{secret}`
    /// ```
    ///
    /// 抓包实测（`cdn-zcode.z.ai/zcode/config/zcode-builtin-23.json` 的
    /// providerRules）：
    ///
    /// ```text
    /// account:zai-start-plan                  → zcode.z.ai/api/v1/zcode-plan/anthropic
    /// account:bigmodel-start-plan             → 同上
    /// account:zai-individual-coding-plan      → api.z.ai/api/anthropic
    /// account:bigmodel-individual-coding-plan → open.bigmodel.cn/api/anthropic
    /// account:*-offpeak-idle-plan             → zcode.z.ai/api/v1/off-peak/anthropic
    /// ```
    ///
    /// # 判据为什么是"有没有 jwt"
    ///
    /// 本地凭证文件里**没有**存套餐类型（只有 provider / credential / jwt /
    /// account_id）。而 JWT **只有 start-plan 通道会签发** —— 走 OAuth 登录时
    /// 才由上游一并返回。故"有 jwt"等价于"这个账号走过 start-plan 的 OAuth"。
    ///
    /// ⚠ 这个判据**不完美**：一个账号若同时有套餐与 API Key，我们会优先用套餐
    /// 通道（有 jwt）。那是**有意的** —— 套餐是包月额度，用它更省。要纠正也很
    /// 简单：删掉凭证里的 jwt 即可回落 API Key 通道。
    pub fn is_start_plan(&self) -> bool {
        !self.jwt.trim().is_empty()
    }

    /// 脱敏后的凭证（供日志与界面展示）。
    ///
    /// 凭证是秘密：日志可能被用户贴到 issue 里。只留前 6 位与后 4 位。
    pub fn masked_credential(&self) -> String {
        mask_secret(&self.credential)
    }

    /// 凭证文件名（不含目录）。
    ///
    /// 防 `zcode-zcode-xxx.json`：`uid` 本身可能已经带 `zcode-` 前缀
    ///（[`cred_key`] 产出的就是那个形状）。
    pub fn file_base_name(&self) -> String {
        let mut uid = self.uid.trim().to_string();
        if uid.is_empty() {
            uid = cred_key(&self.credential);
        }
        if uid.starts_with("zcode-") {
            format!("{}.json", sanitize_file_name(&uid))
        } else {
            format!("zcode-{}.json", sanitize_file_name(&uid))
        }
    }

    /// 转成账号池用的 [`crate::auth::Auth`]（对应 Go 侧 `zcodeAuthOf`）。
    ///
    /// # 字段映射的取舍
    ///
    /// ZCode 的凭证就是一个字符串（`{apiKey}.{secret}`），故复用 `Auth` 的
    /// **`access_token`** 字段承载它（而不是新增一个只对 ZCode 有意义的字段）。
    ///
    /// **服务商**（Z.AI / 智谱）通过 `domain` 承载 —— `Auth` 已有这个字段
    ///（WorkBuddy 用它判区域），复用它避免给池加字段。下游用
    /// [`super::provider::provider_of_domain`] 反解。
    ///
    /// ⚠ ZCode **没有令牌刷新**（凭证长期有效），故 `refresh_token` /
    /// `expires_at` 留空。池的"临近过期先刷新"逻辑对它不生效 —— 这是正确的
    ///（没有可刷新的东西）。**注意**：`Auth::needs_refresh` 在 `expires_at <= 0`
    /// 时恒为 `true`，故转发层必须对产品账号跳过刷新（见 `forward.rs` 的
    /// `is_product` 分支）。
    ///
    /// ⚠ `no_route` **必须**传下去：池的 `healthy()` 会因它排除该账号。漏掉的话，
    /// 界面上的开关**完全不生效** —— 网关照常把请求路由到它。
    ///
    /// JWT **不放进** `Auth`：它只用于额度查询（不在选号/转发热路径上），而池
    /// 也不该关心它。额度查询由需要的一方直接读凭证文件完成。
    pub fn to_auth(&self) -> crate::auth::Auth {
        crate::auth::Auth {
            uid: self.uid.clone(),
            nickname: self.nickname.clone(),
            // 凭证字符串复用 access_token 字段
            access_token: self.credential.clone(),
            // 服务商通过 domain 承载（下游用 provider_of_domain 反解）
            domain: domain_of_provider(self.provider).to_string(),
            file_path: self.file_path.clone(),
            product: crate::auth::PRODUCT_ZCODE.to_string(),
            no_route: self.no_route,
            ..Default::default()
        }
    }

    /// 原子写回凭证（导入或用户改昵称后调用）。
    ///
    /// # ⚠ 必须保留宿主写入的 `account.no_route`
    ///
    /// 这个标记是**宿主**写的（账号页的「停止接流量」开关），而本方法是**网关**
    /// 写的。若这里整体覆盖，网关的任何一次写回都会把用户的禁用标记**悄悄
    /// 抹掉** —— 那个账号会重新开始接流量，而用户以为它还停着。
    ///
    /// 保留策略：**从磁盘读回再合并**，而不是只用内存里的字段重建。这样宿主
    /// 后来加的字段（我们还不认识的）也不会被丢掉。
    pub fn save_atomic(&self) -> Result<(), String> {
        if self.file_path.is_empty() {
            return Err("凭证没有来源路径，无法写回".to_string());
        }

        // 先读回现有内容（可能不存在 —— 那是新建，正常）
        let mut doc: Map<String, Value> = Map::new();
        if let Ok(raw) = std::fs::read(&self.file_path) {
            if let Ok(Value::Object(existing)) = serde_json::from_slice::<Value>(&raw) {
                doc = existing;
            }
        }

        doc.insert("uid".into(), Value::String(self.uid.clone()));
        doc.insert("nickname".into(), Value::String(self.nickname.clone()));
        doc.insert("provider".into(), Value::String(self.provider.as_str().to_string()));
        doc.insert("credential".into(), Value::String(self.credential.clone()));
        if !self.jwt.is_empty() {
            doc.insert("jwt".into(), Value::String(self.jwt.clone()));
        }
        if self.jwt_issued_at > 0 {
            doc.insert("jwt_issued_at".into(), Value::from(self.jwt_issued_at));
        }
        // 上游账号标识：让界面能识别"同一账号的多把 key"。
        // 有 JWT 而字段为空时现解一次，保证写入的文件里始终有它。
        let account_id = if self.account_id.is_empty() && !self.jwt.is_empty() {
            account_id_from_jwt(&self.jwt)
        } else {
            self.account_id.clone()
        };
        if !account_id.is_empty() {
            doc.insert("account_id".into(), Value::String(account_id));
        }
        // 持久化 deviceMid：同一个账号必须始终表现为同一台设备
        //（否则服务端会看到"一个账号被大量不同设备查询"）。
        if is_uuid(&self.device_mid) {
            doc.insert("device_mid".into(), Value::String(self.device_mid.clone()));
        }

        // 保留禁用标记（见上面的说明）。
        //
        // 用**读回的值**而不是 `self.no_route`：`self.no_route` 是上次加载时读到
        // 的，期间用户在界面上可能改过 —— 磁盘上的才是最新的。
        if read_no_route(&doc) {
            let mut acc = match doc.get("account") {
                Some(Value::Object(m)) => m.clone(),
                _ => Map::new(),
            };
            acc.insert("no_route".into(), Value::Bool(true));
            doc.insert("account".into(), Value::Object(acc));
        }

        let raw = serde_json::to_vec_pretty(&Value::Object(doc))
            .map_err(|e| format!("序列化凭证失败: {e}"))?;
        let tmp = format!("{}.tmp", self.file_path);
        std::fs::write(&tmp, &raw).map_err(|e| format!("写入 {tmp}: {e}"))?;
        // 0600：凭证是敏感数据（Go 侧 `os.WriteFile(tmp, raw, 0o600)`）。
        restrict_to_owner(Path::new(&tmp));
        std::fs::rename(&tmp, &self.file_path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("重命名 {tmp} -> {}: {e}", self.file_path)
        })
    }
}

/// 清理粘贴进来的凭证：去掉首尾空白与所有换行。
///
/// 接受的形态：
///
/// ```text
/// "{apiKey}.{secret}"   两段（Z.AI 必需；智谱也可）
/// "{apiKey}"            单段（智谱的 secret 可选）
/// ```
///
/// 拒绝的形态：空、纯空白、含空白字符（粘贴时常带上）。
///
/// 为什么**不校验格式细节**（如长度、字符集）：上游的凭证形态由上游决定，我们
/// 猜错会让合法凭证被拒。宁可放行让上游去拒（它的错误信息更权威）。
pub fn parse_credential(credential: &str) -> Result<ZcodeCred, String> {
    // 粘贴常带首尾空白与换行 —— 先清理，否则会拼出坏请求头。
    let mut c = credential.trim().to_string();
    // 去掉所有换行（多行粘贴的情况）
    c = c.replace('\r', "");
    c = c.replace('\n', "");
    let c = c.trim().to_string();

    if c.is_empty() {
        return Err("凭证为空".to_string());
    }
    // 凭证里不该有空格 —— 有的话多半是粘贴了多余内容（如 "apiKey: xxx"）
    if c.contains(' ') || c.contains('\t') {
        return Err("凭证里含空格，请只粘贴凭证本身（形如 apiKey.secret）".to_string());
    }

    Ok(ZcodeCred {
        uid: cred_key(&c),
        credential: c,
        provider: Provider::Unknown, // 由调用方指定或按域名推断
        ..Default::default()
    })
}

/// 从磁盘加载 ZCode 凭证文件（见模块文档的三种形态）。
pub fn load_file(path: &str) -> Result<ZcodeCred, String> {
    let raw = std::fs::read(path).map_err(|e| format!("读取 {path}: {e}"))?;
    let mut c = parse(&raw, path)?;
    c.file_path = path.to_string();
    if c.uid.is_empty() {
        c.uid = cred_key(&c.credential);
    }
    // ⚠ device_mid 固化（2026-09-21 所有者要求）。
    //
    // # 为什么要落盘，而不是每次读官方文件
    //
    // 所有者原话：「我在想这个 deviceMid 是不是一个账号绑定一个,稳定使用
    // 这样子是不是好一点?」→「要的」
    //
    // 他的直觉指向一个真实风险：**读官方文件是"借用"**，而官方客户端可能重装 /
    // 换机器 / 重置状态 —— 那时 `telemetry-state.json` 会变，我们的指纹**跟着
    // 变**，又回到"同账号多设备"的老问题。
    //
    // 落盘后语义变成：**第一次借用，之后永久固化**。这正是参考实现的做法
    //（`device_mid()` 首次生成后写进 `data/device_mid`）。
    //
    // # 多账号时天然满足"一账号一个"
    //
    // 每个凭证文件各存各的 device_mid：单账号（所有者的情况）首次借用官方那个，
    // 与官方客户端一致；多账号各自固化各自的值，互不影响。
    //
    // # 为什么只在缺失时写
    //
    // `load_file` **在每个请求路径上都会被调用**（额度查询 / 对话 / 领取），
    // 每次加载都重写既慢又危险。故由 [`parse`] 用一个显式标记告诉这里"要不要写"。
    if c.device_mid_needs_persist {
        persist_device_mid_once(&c);
    }
    Ok(c)
}

/// 已固化过 `device_mid` 的凭证路径集合。
///
/// # 为什么需要它（并发缺陷，2026-09-21 自查发现）
///
/// `load_file` **在每个对话请求路径上都会被调用**。首次加载时多个并发请求会
/// 同时走到固化逻辑：
///
/// ```text
/// 两者都读到"凭证里没有 device_mid" ⇒ 都置位 ⇒ 都写
/// 两者用**同一个 tmp 路径**（FilePath + ".tmp"）⇒ 互相覆盖
/// ```
///
/// 后果：`rename` 可能失败（一方已把 tmp 改名走），而 Windows 上把文件 rename
/// 到已存在的目标也可能失败。最坏情况是某个请求的 `load_file` 返回错误 ——
/// 而它本该是纯粹的读操作。
///
/// 用"每个路径只固化一次"消除这个窗口：值本来就是确定的（凭证里的 > 官方文件
/// > 随机），重复写没有意义。
///
/// 进程内即可：固化只在本进程首次加载时需要，跨进程由**磁盘上的 `device_mid`
/// 字段**保证（第二个进程读到它就不再写）。
fn persisted_set() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 每个凭证路径最多固化一次（并发安全）。
///
/// ★ `pub(crate)` 是为了 `warm.rs` 的 `warmDeviceMid`（Go `warm.go:206-215`）：
/// 那条路径给凭证现场补 `device_mid` 后要固化写回，与 `load_file` 走的是**同一个**
/// 「每个路径最多一次」的集合 —— 各持一份会让同一路径被写两次。
pub(crate) fn persist_device_mid_once(c: &ZcodeCred) {
    {
        let mut set = match persisted_set().lock() {
            Ok(g) => g,
            // 锁中毒（另一个线程 panic）不该让"纯读路径"也失败：直接放行，
            // 多写一次的代价远小于拒绝一次对话请求。
            Err(p) => p.into_inner(),
        };
        if set.contains(&c.file_path) {
            return;
        }
        // 先占位再解锁：让并发的第二个调用者直接返回，
        // 而不是排队等第一次写完又写一遍。
        set.insert(c.file_path.clone());
    }

    // 写入失败**不影响功能**：内存里的值仍然有效，只是下次启动会重新借用/生成。
    // 故只记日志，不返回错误 —— 让"落盘失败"变成"这个账号查不到额度"是过度反应。
    if let Err(e) = c.save_atomic() {
        eprintln!("zcode: device_mid 固化失败（本次仍有效，下次会重新借用）: {e}");
    }
}

/// 解析凭证字节；`path` 仅用于错误信息与文件名兜底。
pub fn parse(raw: &[u8], path: &str) -> Result<ZcodeCred, String> {
    if raw.is_empty() {
        return Err(format!("凭证文件为空: {}", base_name(path)));
    }

    // 形态三：整个文件是一个 JSON 字符串
    if let Ok(bare) = serde_json::from_slice::<String>(raw) {
        if !bare.trim().is_empty() {
            return parse_credential(&bare);
        }
    }

    let doc: Map<String, Value> = match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(m)) => m,
        Ok(_) => return Err(format!("凭证不是合法 JSON: 顶层不是对象: {}", base_name(path))),
        Err(e) => return Err(format!("凭证不是合法 JSON: {e}")),
    };

    let mut c = ZcodeCred::default();

    // 先处理 {apiKey, secret} **分成两个字段**的情况 —— 必须在读 `api_key`
    // 单字段之前，否则会先拿到 apiKey、把 secret 丢掉
    //（实测踩到：`{"api_key":"k2","secret":"s2"}` 被解析成 "k2"）。
    let mut key = first_non_empty(&[
        raw_str(&doc, "api_key_id"),
        raw_str(&doc, "apiKeyId"),
        raw_str(&doc, "key"),
    ]);
    let secret = first_non_empty(&[
        raw_str(&doc, "api_key_secret"),
        raw_str(&doc, "apiKeySecret"),
        raw_str(&doc, "secret"),
    ]);
    // `api_key` 字段本身可能是"完整的凭证"（含点）也可能是"只有 key 部分"。
    // 判别依据：含点 = 完整凭证；不含点 = 只有 key。
    let api_key_field = first_non_empty(&[raw_str(&doc, "api_key"), raw_str(&doc, "apiKey")]);
    if !api_key_field.is_empty() {
        if api_key_field.contains('.') {
            // 完整凭证，直接用
            c.credential = api_key_field;
        } else if key.is_empty() {
            key = api_key_field;
        }
    }
    if c.credential.is_empty() {
        if !key.is_empty() && !secret.is_empty() {
            c.credential = format!("{key}.{secret}");
        } else if !key.is_empty() {
            c.credential = key;
        }
    }
    // `credential` 字段优先级最高（宿主落盘用的就是它）
    let v = raw_str(&doc, "credential");
    if !v.is_empty() {
        c.credential = v;
    }
    // 嵌套形：{"auth":{"credential":"..."}}
    if c.credential.is_empty() {
        if let Some(Value::Object(auth)) = doc.get("auth") {
            c.credential = first_non_empty(&[
                raw_str(auth, "credential"),
                raw_str(auth, "apiKey"),
                raw_str(auth, "api_key"),
            ]);
        }
    }
    if c.credential.trim().is_empty() {
        return Err(format!(
            "凭证文件里找不到凭证字段（支持 credential / apiKey / api_key 等）: {}",
            base_name(path)
        ));
    }
    // 清理（与 parse_credential 同一口径）
    let cleaned = parse_credential(&c.credential)
        .map_err(|e| format!("{}: {e}", base_name(path)))?;
    c.credential = cleaned.credential;

    c.uid = first_non_empty(&[raw_str(&doc, "uid"), raw_str(&doc, "id")]);
    c.nickname = raw_str(&doc, "nickname");
    c.jwt = first_non_empty(&[raw_str(&doc, "jwt"), raw_str(&doc, "token")]);
    c.jwt_issued_at = raw_int(&doc, "jwt_issued_at");
    // 上游账号标识：优先读已存的，没有就从 JWT 里现解一个
    c.account_id = raw_str(&doc, "account_id");
    if c.account_id.is_empty() && !c.jwt.is_empty() {
        c.account_id = account_id_from_jwt(&c.jwt);
    }
    c.provider = parse_provider(&first_non_empty(&[
        raw_str(&doc, "provider"),
        raw_str(&doc, "vendor"),
    ]));

    // 用户手动禁用（宿主写入的 `account.no_route`）。
    //
    // 两个位置都读：宿主的 `account` 段（与 WorkBuddy 一致）与顶层
    //（手写凭证文件时用户可能直接写在顶层，不接受它会让禁用静默失效）。
    c.no_route = raw_bool(&doc, "no_route") || raw_bool_at(&doc, "account", "no_route");

    // 控制面设备标识。缺失时**补一个**而不是留空 ——
    // 空值会让额度查询恒回 3001（实测），而那看起来像"这个账号没有额度"。
    c.device_mid = raw_str(&doc, "device_mid");
    if !is_uuid(&c.device_mid) {
        c.device_mid = mint_device_mid(&c.account_id);
        // ⚠ 这里**只算好值**，落盘交给 load_file（见那里的说明）。
        //
        // 为什么不能在这里 save_atomic：本函数拿不到 `file_path` —— 那是
        // load_file 在本函数**返回之后**才设置的。在这里调用必然失败
        //（save_atomic 的第一行就是「凭证没有来源路径，无法写回」）。
        // 靠"调用点必须晚于 FilePath 赋值"这条隐含顺序才不出错 ——
        // 而那种隐含顺序正是最容易在重构时被打断的东西。
        c.device_mid_needs_persist = true;
    }

    Ok(c)
}

/// 为新账号产生一个设备标识：与官方同账号则借用，否则随机。
///
/// 见 [`ZcodeCred::device_mid`] 的长注释：**一个账号一个设备标识**是正确形状，
/// 共用会让上游看到"一台设备并发一批账号"（号商特征）。
///
/// 借用官方值的**唯一**条件：凭证的 `account_id` 与官方 `credentials.json` 里
/// 那个账号 uuid 相等（那时我们与官方客户端是**同一个账号、同一台设备**）。
pub fn mint_device_mid(account_id: &str) -> String {
    if !account_id.is_empty() {
        let official = official_account_uuid();
        if !official.is_empty() && official.eq_ignore_ascii_case(account_id) {
            let mid = official_device_mid();
            if !mid.is_empty() {
                return mid;
            }
        }
    }
    new_device_mid()
}

/// 读官方客户端登录的账号 uuid（读不到返回空串）。
///
/// # 来源
///
/// `~/.zcode/v2/credentials.json` 的键里嵌着账号 uuid：
///
/// ```text
/// account-provider:coding-plan:account:zai-individual-coding-plan:
///     account:{uuid}:api-key
///               ^^^^^^^^^^^^
/// ```
///
/// 值本身是加密的（`enc:v1:…`），但**键名是明文** —— 我们只需要那个 uuid。
///
/// # 为什么要它
///
/// 决定"能不能借用官方 deviceMid"：只有当凭证的账号**就是**官方登录的那个账号
/// 时，共享设备标识才真实（同一账号、同一台设备）。其余账号必须各自独立，否则
/// 上游会看到"一台设备并发一批账号"。
///
/// 只读、容错：文件不存在 / 解析失败 / 找不到 uuid → 返回空串，调用方据此走
/// "随机"分支（那总是安全的）。
pub fn official_account_uuid() -> String {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return String::new(),
    };
    let raw = match std::fs::read(home.join(".zcode").join("v2").join("credentials.json")) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    // 不整体解析 JSON：值是加密串，而我们只关心**键名**里的 uuid。
    // 用正则直接扫键，避免为一个字段依赖文件的具体 JSON 形状。
    // 形如 `account:{uuid}` 或 `…:account:{uuid}:api-key`
    //
    // ⚠ 必须按**字节**扫而不是先转 String：这个文件里可能有非 UTF-8 字节
    //（加密值），先转字符串会在转换处失败，把"读不到 uuid"变成"整个文件读不了"。
    let re = official_uuid_re();
    let hay = String::from_utf8_lossy(&raw);
    match re.captures(&hay) {
        Some(caps) => caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default(),
        None => String::new(),
    }
}

fn official_uuid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"account:([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})")
            .expect("official account uuid regex must compile")
    })
}

/// 读官方客户端持久化的 deviceMid（读不到返回空串）。
///
/// # 来源
///
/// 官方客户端把它存在 `~/.zcode/v2/telemetry-state.json`：
///
/// ```json
/// { "deviceMid": "{uuid}", "lastDailyActiveDate": "2026-09-21" }
/// ```
///
/// # 为什么读它而不是自己生成
///
/// 官方**固定**、旧实现**每次随机**，那正是"官方不触发、我们触发"的差异。
///
/// # 只读、容错
///
/// 不写回、不修改官方文件（那是官方客户端的资产，我们只借用标识）。
/// 文件不存在 / 格式不对 / 值不是 UUID → 一律返回空串，由调用方回退随机。
pub fn official_device_mid() -> String {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return String::new(),
    };
    let raw = match std::fs::read(home.join(".zcode").join("v2").join("telemetry-state.json")) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    let doc: Map<String, Value> = match serde_json::from_slice::<Value>(&raw) {
        Ok(Value::Object(m)) => m,
        _ => return String::new(),
    };
    let v = match doc.get("deviceMid").and_then(|v| v.as_str()) {
        Some(s) => s.trim().to_string(),
        None => return String::new(),
    };
    if !is_uuid(&v) {
        return String::new();
    }
    v
}

/// 扫描目录下 `zcode*.json`，返回成功解析的凭证与失败清单。
///
/// 与 `qoder::load_dir` 同样的约定：**把失败报出来**而不是静默跳过 ——
/// ZCode 的凭证多半是用户手写的（粘贴导入），格式错误必须可见。
///
/// 返回顺序为文件名字典序（Go 侧 `filepath.Glob` 的顺序），避免依赖平台返回
/// 顺序导致启动选号不确定。
pub fn load_dir(dir: &str) -> (Vec<ZcodeCred>, Vec<String>) {
    let mut files: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("zcode") && n.ends_with(".json"))
                    .unwrap_or(false)
            })
            .collect(),
        // 目录不存在等价于"没有凭证"（Go 的 Glob 也是这么做的：不报错）。
        Err(_) => return (Vec::new(), Vec::new()),
    };
    files.sort();

    let mut creds = Vec::new();
    let mut failed = Vec::new();
    for f in files {
        let path = f.to_string_lossy().to_string();
        match load_file(&path) {
            Ok(c) => creds.push(c),
            Err(e) => failed.push(format!("{}: {e}", base_name(&path))),
        }
    }
    (creds, failed)
}

/// 默认凭证目录（与宿主侧必须一致）。
pub fn default_auth_dir() -> String {
    match dirs::home_dir() {
        Some(h) if !h.as_os_str().is_empty() => h
            .join(".wb-switch")
            .join("zcode")
            .join("auths")
            .to_string_lossy()
            .to_string(),
        _ => Path::new(".wb-switch")
            .join("zcode")
            .join("auths")
            .to_string_lossy()
            .to_string(),
    }
}

/// 从 JWT 里解出上游的 `user_id`（**不验签**）。
///
/// # 用途
///
/// 判断"本地这几条账号是不是同一个上游账号的多把 key"。
/// uid 是凭证哈希，抓不到这种情况；`user_id` 才是账号级标识。
///
/// # 为什么出错一律返回空串
///
/// 签发时刻解不出只影响"年龄展示"，而账号标识解不出会影响**去重判断** ——
/// 故这里出错一律返回空串（"未知"），绝**不猜**。猜错会让界面把两个不同账号
/// 标成"重复"，那比不标注更糟。
pub fn account_id_from_jwt(token: &str) -> String {
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 {
        return String::new();
    }
    // base64url → 标准 base64
    let mut payload: String = parts[1]
        .chars()
        .map(|ch| match ch {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    let rem = payload.len() % 4;
    if rem != 0 {
        payload.push_str(&"=".repeat(4 - rem));
    }
    let raw = match jwt_b64_engine().decode(payload.as_bytes()) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    let claims: Map<String, Value> = match serde_json::from_slice::<Value>(&raw) {
        Ok(Value::Object(m)) => m,
        _ => return String::new(),
    };
    // 优先 user_id；退到 sub（实测两者相同，但 user_id 更明确）
    let user_id = claims.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
    if !user_id.is_empty() {
        return user_id.to_string();
    }
    claims
        .get("sub")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// JWT payload 的 base64 解码器。
///
/// 与 Go 的 `base64.StdEncoding.DecodeString` 对齐，但**放宽两处**（Go 的
/// 解码器本来就更宽松，严格引擎会把合法 JWT 判成非法）：
///
/// - `Indifferent`：有/无 padding 都接受（我们上面按需补过 `=`）；
/// - `allow_trailing_bits`：Go 的解码器**不校验**末字节的填充位，
///   严格引擎会报 `InvalidLastSymbol` —— 那会让一个真实可用的 JWT
///   解不出 `user_id`，症状是"账号去重突然失效"且毫无提示。
fn jwt_b64_engine() -> &'static base64::engine::GeneralPurpose {
    static E: OnceLock<base64::engine::GeneralPurpose> = OnceLock::new();
    E.get_or_init(|| {
        base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new()
                .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
                .with_decode_allow_trailing_bits(true),
        )
    })
}

/// 本地账号标识：`"zcode-" + shortHash(TrimSpace(credential))`。
pub fn cred_key(credential: &str) -> String {
    format!("zcode-{}", short_hash(credential.trim()))
}

/// FNV-1a 64 位哈希的低 48 位，12 位十六进制小写。
///
/// 与 Go 侧 `shortHash` 逐位一致（含 `offset64` / `prime64` 常量与"从低位往高位
/// 填"的取法）—— 两边对同一凭证必须得到**同一个** uid，否则同一个账号在两个
/// 实现下会被当成两个账号。
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // offset64
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3); // prime64
    }
    format!("{:012x}", h & 0xffff_ffff_ffff)
}

/// 文件名安全化：只保留 `a-z A-Z 0-9 - _`，其余替换成 `_`。
fn sanitize_file_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '-' || ch == '_';
        out.push(if ok { ch } else { '_' });
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

/// 脱敏任意秘密字符串。
pub fn mask_secret(s: &str) -> String {
    if s.len() <= 12 {
        return "****".to_string();
    }
    // ★ 与 Go 的 `s[:6] + "…" + s[len(s)-4:]` 差异：Go 按**字节**切，遇到非 ASCII
    // 会切出非法 UTF-8（Go 的 string 允许，Rust 不允许）。这里退到最近的字符边界，
    // 语义（前 6 字节 + … + 后 4 字节）不变，只是不会 panic。
    format!("{}…{}", safe_prefix(s, 6), safe_suffix(s, 4))
}

/// 取前 `n` 个**字节**，必要时退到字符边界（不会切出半个字符）。
fn safe_prefix(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 取后 `n` 个**字节**，必要时前进到字符边界。
fn safe_suffix(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut start = s.len() - n;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

fn raw_str(m: &Map<String, Value>, key: &str) -> String {
    match m.get(key).and_then(|v| v.as_str()) {
        Some(s) => s.trim().to_string(),
        None => String::new(),
    }
}

/// 读一个布尔字段（缺失或类型不符都返回 false）。
///
/// 缺失返回 false 而不是报错：三态语义里「未声明」等价于「不禁用」——
/// 若报错，历史凭证文件（没有这个键）会全部解析失败。
fn raw_bool(m: &Map<String, Value>, key: &str) -> bool {
    m.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// 读嵌套对象里的布尔字段（如 `account.no_route`）。
fn raw_bool_at(m: &Map<String, Value>, outer: &str, key: &str) -> bool {
    match m.get(outer) {
        Some(Value::Object(nested)) => raw_bool(nested, key),
        _ => false,
    }
}

fn raw_int(m: &Map<String, Value>, key: &str) -> i64 {
    m.get(key).and_then(|v| v.as_i64()).unwrap_or(0)
}

/// 从已解析的文档里读禁用标记（`account.no_route` 或顶层）。
fn read_no_route(doc: &Map<String, Value>) -> bool {
    if doc.get("no_route").and_then(|v| v.as_bool()).unwrap_or(false) {
        return true;
    }
    match doc.get("account") {
        Some(Value::Object(acc)) => {
            acc.get("no_route").and_then(|v| v.as_bool()).unwrap_or(false)
        }
        _ => false,
    }
}

fn first_non_empty(vals: &[String]) -> String {
    for v in vals {
        let t = v.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    String::new()
}

/// `filepath.Base` 的等价物（错误信息里不出现完整路径）。
fn base_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

/// 凭证是敏感数据：unix 上写 0600（Windows 上该位无效，与 Go 行为一致）。
///
/// ★ `pub(crate)` 是为了 `login.rs` 的 `save_session`：会话文件里含
/// `pollToken`（泄漏它等于让别人能抢答轮询），必须与凭证同一档权限。
/// 两处共用一个实现，避免「凭证 0600、会话 0644」这种不一致。
#[cfg(unix)]
pub(crate) fn restrict_to_owner(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub(crate) fn restrict_to_owner(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录：并发跑测试时不能共用（否则 tmp 文件互相覆盖）。
    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "zcode-cred-{tag}-{}-{}",
            std::process::id(),
            new_device_mid()
        ));
        std::fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    fn write_file(dir: &Path, name: &str, body: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, body).expect("write fixture");
        p.to_string_lossy().to_string()
    }

    /// 造一个 payload 可解的 JWT（不验签，故签名段随便填）。
    fn fake_jwt(claims_json: &str) -> String {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims_json.as_bytes());
        format!("eyJhbGciOiJub25lIn0.{payload}.sig")
    }

    #[test]
    fn short_hash_matches_the_go_implementation_bit_for_bit() {
        // FNV-1a 64 的**低 48 位**（取低 12 个十六进制位，不是取高 12 位）。
        // 锚点：FNV-1a-64("") = 0xcbf29ce484222325、FNV-1a-64("a") = 0xaf63dc4c8601ec8c
        // —— 若常量或取位方向写错，两边对同一凭证会算出不同 uid，
        // 表现为"同一个账号在 Rust 网关里凭空多出一条"。
        assert_eq!(short_hash(""), "9ce484222325");
        assert_eq!(short_hash("a"), "dc4c8601ec8c");
        assert_eq!(short_hash("k1.s1"), "f04099c69421");
    }

    #[test]
    fn cred_key_is_stable_and_prefixed() {
        let a = cred_key("k1.s1");
        assert_eq!(a, cred_key("  k1.s1  "), "trim 后必须得到同一个 uid（幂等）");
        assert!(a.starts_with("zcode-"), "uid 前缀是 zcode-：{a}");
        assert_eq!(a.len(), "zcode-".len() + 12);
        assert_ne!(a, cred_key("k2.s2"), "不同凭证必须得到不同 uid");
        // 全小写十六进制（大写会让同一个凭证在两次实现间对不上）
        assert_eq!(a, a.to_lowercase());
    }

    #[test]
    fn file_base_name_never_double_prefixes_zcode() {
        let mut c = ZcodeCred {
            uid: "zcode-abc123".into(),
            ..Default::default()
        };
        assert_eq!(c.file_base_name(), "zcode-abc123.json");

        c.uid = "plain-uid".into();
        assert_eq!(c.file_base_name(), "zcode-plain-uid.json");

        // uid 为空 ⇒ 回落到 credential 的哈希
        c.uid = String::new();
        c.credential = "k.s".into();
        assert_eq!(c.file_base_name(), format!("{}.json", cred_key("k.s")));

        // 危险字符被替换，不会写出目录穿越的文件名（`.` 与 `/` 都变成 `_`）
        c.uid = "zcode-../evil".into();
        assert_eq!(c.file_base_name(), "zcode-___evil.json");
    }

    #[test]
    fn parse_credential_strips_newlines_and_rejects_inner_spaces() {
        let c = parse_credential("  k1.s1\r\n").expect("多行粘贴必须能吃下");
        assert_eq!(c.credential, "k1.s1");
        assert_eq!(c.uid, cred_key("k1.s1"));
        assert_eq!(c.provider, Provider::Unknown);

        // 单段也合法（智谱 secret 可选）
        assert_eq!(parse_credential("k1").expect("单段").credential, "k1");

        assert!(parse_credential("   ").is_err());
        assert!(parse_credential("").is_err());
        let err = parse_credential("apiKey: k1.s1").expect_err("含空格必须拒");
        assert!(err.contains("空格"), "错误信息要能指导用户：{err}");
    }

    #[test]
    fn parse_reads_secret_before_the_single_api_key_field() {
        // ★ 这是 Go 侧实测踩到的坑：顺序错了 `{"api_key":"k2","secret":"s2"}`
        // 会被解析成 "k2"，secret 被静默丢掉 ⇒ 上游 401/1000，而文件看起来完全正常。
        let doc = br#"{"api_key":"k2","secret":"s2"}"#;
        let c = parse(doc, "x.json").expect("parse");
        assert_eq!(c.credential, "k2.s2", "secret 不能被丢掉");

        // api_key 自带点时是"完整凭证"，secret 字段应被忽略
        let doc = br#"{"api_key":"k9.s9","secret":"s2"}"#;
        assert_eq!(parse(doc, "x.json").expect("parse").credential, "k9.s9");

        // 只有 key，没有 secret ⇒ 单段
        let doc = br#"{"api_key_id":"k3"}"#;
        assert_eq!(parse(doc, "x.json").expect("parse").credential, "k3");
    }

    #[test]
    fn parse_prefers_credential_field_over_every_other_shape() {
        let doc = br#"{"api_key":"k1","secret":"s1","credential":"kc.sc"}"#;
        let c = parse(doc, "x.json").expect("parse");
        assert_eq!(c.credential, "kc.sc", "credential 字段优先级最高（宿主落盘用的就是它）");
    }

    #[test]
    fn parse_accepts_all_three_file_shapes() {
        // 形态一：嵌套形（宿主落盘）
        let doc = br#"{"credential":"k1.s1","provider":"zai","jwt":"a.b.c","nickname":"n1"}"#;
        let c = parse(doc, "x.json").expect("parse");
        assert_eq!(c.credential, "k1.s1");
        assert_eq!(c.provider, Provider::Zai);
        assert_eq!(c.nickname, "n1");

        // 形态二：扁平形 {apiKey, secret}
        let doc = br#"{"apiKey":"k2","secret":"s2"}"#;
        assert_eq!(parse(doc, "x.json").expect("parse").credential, "k2.s2");

        // 形态三：裸字符串文件
        let doc = br#""k3.s3""#;
        let c = parse(doc, "x.json").expect("parse");
        assert_eq!(c.credential, "k3.s3");
        assert_eq!(c.uid, cred_key("k3.s3"));

        // 嵌套 auth 形
        let doc = br#"{"auth":{"credential":"k4.s4"}}"#;
        assert_eq!(parse(doc, "x.json").expect("parse").credential, "k4.s4");

        // 空文件 / 非法 JSON / 找不到字段：三条错误都要点名文件
        assert!(parse(b"", "e.json").expect_err("空文件").contains("e.json"));
        assert!(parse(b"{oops", "e.json").is_err());
        let err = parse(br#"{"foo":1}"#, "e.json").expect_err("找不到凭证字段");
        assert!(err.contains("e.json") && err.contains("凭证字段"), "{err}");
    }

    #[test]
    fn parse_reads_no_route_from_both_locations() {
        // 宿主写的 account.no_route
        let doc = br#"{"credential":"k1.s1","account":{"no_route":true}}"#;
        assert!(parse(doc, "x.json").expect("parse").no_route);

        // 手写文件把开关写在顶层 —— 不接受它会让禁用**静默失效**
        let doc = br#"{"credential":"k1.s1","no_route":true}"#;
        assert!(parse(doc, "x.json").expect("parse").no_route);

        // 未声明 = 不禁用（三态语义）
        let doc = br#"{"credential":"k1.s1"}"#;
        assert!(!parse(doc, "x.json").expect("parse").no_route);

        // 类型不对也不报错（历史文件里有 "no_route": "true" 这种写法）
        let doc = br#"{"credential":"k1.s1","no_route":"true"}"#;
        assert!(!parse(doc, "x.json").expect("parse").no_route);
    }

    #[test]
    fn account_id_from_jwt_never_guesses() {
        assert_eq!(account_id_from_jwt(""), "");
        assert_eq!(account_id_from_jwt("not-a-jwt"), "");
        assert_eq!(account_id_from_jwt("a.b"), "", "段数不对 ⇒ 空串");
        assert_eq!(account_id_from_jwt("a.b.c.d"), "");
        assert_eq!(account_id_from_jwt("a.!!!.c"), "", "非法 base64 ⇒ 空串");
        // payload 不是 JSON 对象
        let junk = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"12345");
        assert_eq!(account_id_from_jwt(&format!("a.{junk}.c")), "");
    }

    #[test]
    fn account_id_from_jwt_prefers_user_id_then_sub() {
        assert_eq!(
            account_id_from_jwt(&fake_jwt(r#"{"user_id":"u-1","sub":"s-1"}"#)),
            "u-1"
        );
        assert_eq!(
            account_id_from_jwt(&fake_jwt(r#"{"sub":"s-1"}"#)),
            "s-1",
            "user_id 缺失时退到 sub"
        );
        assert_eq!(account_id_from_jwt(&fake_jwt(r#"{"user_id":""}"#)), "");
        // 带 padding 的 base64 也要能吃下（有些签发方会补 =）
        let padded = base64::engine::general_purpose::STANDARD.encode(br#"{"user_id":"u-2"}"#);
        assert_eq!(account_id_from_jwt(&format!("a.{padded}.c")), "u-2");
    }

    #[test]
    fn parse_derives_account_id_from_jwt_when_absent() {
        let jwt = fake_jwt(r#"{"user_id":"acct-9"}"#);
        let doc = format!(r#"{{"credential":"k1.s1","jwt":"{jwt}"}}"#);
        let c = parse(doc.as_bytes(), "x.json").expect("parse");
        assert_eq!(c.account_id, "acct-9");

        // 文件里已存 account_id ⇒ 以文件为准（用户可能手改过）
        let doc = format!(r#"{{"credential":"k1.s1","jwt":"{jwt}","account_id":"stored"}}"#);
        assert_eq!(parse(doc.as_bytes(), "x.json").expect("parse").account_id, "stored");
    }

    #[test]
    fn parse_always_yields_a_uuid_device_mid_and_flags_it_for_persist() {
        let doc = br#"{"credential":"k1.s1"}"#;
        let c = parse(doc, "x.json").expect("parse");
        assert!(is_uuid(&c.device_mid), "缺失时必须补一个：{}", c.device_mid);
        assert!(c.device_mid_needs_persist, "补出来的值必须被固化写回");

        // 文件里已有合法 UUID ⇒ 原样用，且**不再**触发写回
        let mid = new_device_mid();
        let doc = format!(r#"{{"credential":"k1.s1","device_mid":"{mid}"}}"#);
        let c = parse(doc.as_bytes(), "x.json").expect("parse");
        assert_eq!(c.device_mid, mid);
        assert!(!c.device_mid_needs_persist, "已有值不该触发每次请求都重写文件");

        // 文件里是坏值（不是 UUID）⇒ 换一个并固化
        let doc = br#"{"credential":"k1.s1","device_mid":"not-a-uuid"}"#;
        let c = parse(doc, "x.json").expect("parse");
        assert!(is_uuid(&c.device_mid));
        assert!(c.device_mid_needs_persist);
    }

    #[test]
    fn mint_device_mid_never_shares_one_identity_across_accounts() {
        // ★ 一个账号一个设备标识：不同 account_id 必须得到不同 deviceMid。
        // 共用会让上游看到「一台设备并发一批账号」—— 号商特征。
        let a = mint_device_mid("account-a");
        let b = mint_device_mid("account-b");
        assert!(is_uuid(&a) && is_uuid(&b));
        assert_ne!(a, b);
        // 无 account_id 时也是随机 UUID（绝不返回空串）
        assert!(is_uuid(&mint_device_mid("")));
    }

    #[test]
    fn load_file_persists_device_mid_and_save_preserves_no_route() {
        let dir = tmp_dir("persist");
        let path = write_file(
            &dir,
            "zcode-a.json",
            r#"{"credential":"k1.s1","account":{"no_route":true},"credit":{"x":1}}"#,
        );

        let c = load_file(&path).expect("load");
        assert_eq!(c.file_path, path);
        assert!(c.no_route, "account.no_route 必须被读到");
        assert!(is_uuid(&c.device_mid));

        // 固化必须真的落盘（否则每次启动换一个指纹 ⇒ 上游判 unusual activity）
        let on_disk: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read back")).expect("json");
        assert_eq!(
            on_disk.get("device_mid").and_then(|v| v.as_str()),
            Some(c.device_mid.as_str()),
            "device_mid 必须写回凭证文件"
        );
        // 未知字段与禁用标记都要活下来
        assert_eq!(on_disk.get("credit").and_then(|v| v.get("x")).and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            on_disk.get("account").and_then(|v| v.get("no_route")).and_then(|v| v.as_bool()),
            Some(true),
            "网关写回绝不能抹掉用户的「停止接流量」开关"
        );

        // 二次加载：值稳定不变（这就是"固化"的意义）
        let again = load_file(&path).expect("load again");
        assert_eq!(again.device_mid, c.device_mid);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_atomic_refuses_without_a_source_path() {
        let c = parse_credential("k1.s1").expect("parse");
        let err = c.save_atomic().expect_err("没有来源路径必须拒绝");
        assert!(err.contains("来源路径"), "{err}");
    }

    #[test]
    fn save_atomic_writes_the_host_readable_fields() {
        let dir = tmp_dir("save");
        let path = write_file(&dir, "zcode-b.json", r#"{"credential":"k1.s1"}"#);
        let mut c = load_file(&path).expect("load");
        c.nickname = "改过的昵称".into();
        c.provider = Provider::Bigmodel;
        c.jwt = fake_jwt(r#"{"user_id":"acct-7"}"#);
        c.jwt_issued_at = 1_700_000_000;
        c.save_atomic().expect("save");

        let v: Value = serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
        assert_eq!(v.get("nickname").and_then(|x| x.as_str()), Some("改过的昵称"));
        assert_eq!(v.get("provider").and_then(|x| x.as_str()), Some("bigmodel"));
        assert_eq!(v.get("uid").and_then(|x| x.as_str()), Some(c.uid.as_str()));
        assert_eq!(v.get("jwt_issued_at").and_then(|x| x.as_i64()), Some(1_700_000_000));
        assert_eq!(
            v.get("account_id").and_then(|x| x.as_str()),
            Some("acct-7"),
            "有 JWT 而 account_id 为空时必须现解一次写进去"
        );
        assert_eq!(
            v.get("device_mid").and_then(|x| x.as_str()),
            Some(c.device_mid.as_str())
        );

        // 没有 JWT 的账号不该凭空长出 jwt 字段
        let dir2 = tmp_dir("save2");
        let path2 = write_file(&dir2, "zcode-c.json", r#"{"credential":"k2.s2"}"#);
        load_file(&path2).expect("load").save_atomic().expect("save");
        let v2: Value = serde_json::from_slice(&std::fs::read(&path2).expect("read")).expect("json");
        assert!(v2.get("jwt").is_none());
        assert!(v2.get("jwt_issued_at").is_none());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn save_atomic_does_not_leave_a_tmp_file_behind() {
        let dir = tmp_dir("tmp");
        let path = write_file(&dir, "zcode-d.json", r#"{"credential":"k1.s1"}"#);
        load_file(&path).expect("load").save_atomic().expect("save");
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留 tmp：{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_dir_reports_failures_instead_of_silently_skipping() {
        let dir = tmp_dir("dir");
        write_file(&dir, "zcode-good.json", r#"{"credential":"k1.s1"}"#);
        write_file(&dir, "zcode-bad.json", r#"{"foo":1}"#);
        // 不该被扫描（前缀不符）
        write_file(&dir, "other.json", r#"{"credential":"k9.s9"}"#);
        // 也不该被扫描（后缀不符）
        write_file(&dir, "zcode-notes.txt", "hello");

        let (creds, failed) = load_dir(&dir.to_string_lossy());
        assert_eq!(creds.len(), 1, "只应加载 zcode*.json：{creds:?}");
        assert_eq!(creds[0].credential, "k1.s1");
        assert_eq!(failed.len(), 1, "格式错误必须**报出来**：{failed:?}");
        assert!(failed[0].starts_with("zcode-bad.json: "), "{failed:?}");

        // 目录不存在等价于"没有凭证"，不是错误
        let (c, f) = load_dir(&format!("{}-missing", dir.to_string_lossy()));
        assert!(c.is_empty() && f.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mask_secret_keeps_only_the_ends() {
        assert_eq!(mask_secret("short"), "****");
        assert_eq!(mask_secret("123456789012"), "****", "12 位及以下全遮");
        assert_eq!(mask_secret("1234567890123"), "123456…0123");
        // ★ 不能 panic、不能切出半个字符（Go 的字节切片在 Rust 里会 panic）
        let s = "密钥密钥密钥密钥密钥密钥密钥";
        let m = mask_secret(s);
        assert!(m.starts_with(safe_prefix(s, 6)));
        assert!(m.ends_with(safe_suffix(s, 4)));
    }

    #[test]
    fn to_auth_carries_exactly_what_the_pool_needs() {
        let mut c = parse_credential("k1.s1").expect("parse");
        c.uid = "zcode-abc".into();
        c.nickname = "n1".into();
        c.provider = Provider::Bigmodel;
        c.no_route = true;

        let a = c.to_auth();
        assert_eq!(a.uid, "zcode-abc");
        assert_eq!(a.nickname, "n1");
        assert_eq!(a.access_token, "k1.s1", "凭证字符串复用 access_token 字段");
        assert_eq!(a.domain, "open.bigmodel.cn", "服务商通过 domain 承载");
        assert_eq!(a.product, crate::auth::PRODUCT_ZCODE);
        assert!(a.is_zcode());
        // ★ 漏掉 no_route 会让界面上的「停止接流量」开关完全不生效
        assert!(a.no_route);
        // ZCode 没有令牌刷新：留空是**正确**的（没有可刷新的东西）
        assert_eq!(a.refresh_token, "");
        assert_eq!(a.expires_at, 0);
        // 服务商必须能从 domain 反解回来（池只存 domain）
        assert_eq!(
            super::super::provider::provider_of_domain(&a.domain),
            Provider::Bigmodel
        );

        // 未指定服务商 ⇒ 回落 Z.AI 域名（与 Go 的 DomainOfProvider 一致）
        let mut c2 = parse_credential("k2.s2").expect("parse");
        c2.provider = Provider::Unknown;
        assert_eq!(c2.to_auth().domain, "api.z.ai");
    }

    #[test]
    fn is_start_plan_depends_only_on_the_jwt() {
        let mut c = parse_credential("k1.s1").expect("parse");
        assert!(!c.is_start_plan(), "没有 jwt ⇒ 按量计费通道");
        c.jwt = "a.b.c".into();
        assert!(c.is_start_plan());
        c.jwt = "   ".into();
        assert!(!c.is_start_plan(), "空白 jwt 不算（否则会选错端点）");
    }

    #[test]
    fn default_auth_dir_matches_the_host_side_location() {
        let d = default_auth_dir();
        assert!(d.contains(".wb-switch"), "{d}");
        assert!(d.ends_with("zcode\\auths") || d.ends_with("zcode/auths"), "{d}");
    }

    #[test]
    fn parse_cleans_whitespace_inside_the_stored_credential() {
        // 宿主可能把带换行的凭证原样落盘 —— 必须清理，否则会拼出坏请求头
        let doc = br#"{"credential":"  k1.s1\n"}"#;
        let c = parse(doc, "x.json").expect("parse");
        assert_eq!(c.credential, "k1.s1");
        // ⚠ `parse` **不**回算 uid（Go `parseBytes` 同样不回算：它只取
        // `firstNonEmpty(doc["uid"], doc["id"])`，而 `Parse()` 算出的那个 UID 被丢弃了）。
        // 回算只发生在 `load_file` —— 见下一个测试。
        assert_eq!(c.uid, "", "文档里没有 uid/id ⇒ 保持空，交由 load_file 回算");
    }

    #[test]
    fn load_file_computes_uid_from_the_cleaned_credential() {
        // 这条钉的是**真正**的契约：`load_file` 在 uid 缺失时按**清理后**的凭证回算
        //（Go `LoadFile` 的 `if c.UID == "" { c.UID = CredKey(c.Credential) }`）。
        // 顺序很重要：先清理再算，否则带空白的凭证会算出与账号页不一致的 uid，
        // 表现为"同一个账号在网关上出现两条记录"。
        let dir = tmp_dir("uid-from-cred");
        let p = write_file(&dir, "z.json", r#"{"credential":"  k1.s1\n"}"#);
        let c = load_file(&p).expect("load_file");
        assert_eq!(c.credential, "k1.s1", "落盘前先清理");
        assert_eq!(c.uid, cred_key("k1.s1"), "uid 必须按清理后的凭证算");
        assert_eq!(c.uid, "zcode-f04099c69421", "与短哈希锚点一致");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
