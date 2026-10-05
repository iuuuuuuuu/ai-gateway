//! 账号凭证解析与原子写回。
//!
//! 对应 Go 源文件 `internal/auth/auth.go`。
//!
//! # 磁盘双形态
//!
//! | 形态   | 结构                                 | 来源               |
//! |--------|--------------------------------------|--------------------|
//! | 嵌套形 | `{"auth":{...},"account":{...}}`     | 插件 OAuth 输出    |
//! | 扁平形 | `{"accessToken":...,"uid":...}`      | 手写 / 旧版        |
//!
//! 判定依据是**顶层是否存在 `auth` 键**（与 Go 侧 `probe["auth"]` 一致），
//! 而不是逐个字段嗅探——避免半嵌套文件被误判成扁平形而丢字段。
//!
//! # 时间戳量级
//!
//! 上游混用秒/毫秒两种精度，[`normalize_epoch`] 按量级统一成秒。
//! 注意这是 Go 侧已有的启发式（阈值 `1e12`），不是精确判定。

use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::error::{GatewayError, Result};

/// 账号所属产品（空串等价于 [`ProductWorkBuddy`]）。
///
/// 与 Go 侧 `auth.ProductWorkBuddy` / `ProductQoder` / `ProductZcode` 逐字一致。
pub const PRODUCT_WORKBUDDY: &str = "workbuddy";
/// QoderWork。
pub const PRODUCT_QODER: &str = "qoder";
/// ZCode（Z.AI / 智谱 GLM 编码套餐）。
pub const PRODUCT_ZCODE: &str = "zcode";
/// Trae（字节跳动 Trae / TRAE SOLO）。
///
/// 与 Go 侧 `auth.ProductTrae` 同口径。Trae 有**两个区域**（国服 / 国际版）
/// 共用同一个产品标识，区域由凭证的 `domain` 承载（见 [`region_of_domain`]）。
pub const PRODUCT_TRAE: &str = "trae";

/// 归一化后的账号凭证。
#[derive(Debug, Clone, Default)]
pub struct Auth {
    /// 访问令牌。
    pub access_token: String,
    /// 刷新令牌。
    pub refresh_token: String,
    /// 过期时刻（Unix 秒）。
    pub expires_at: i64,
    /// 区域域名（国服 / 国际版）。
    pub domain: String,
    /// 账号 UID。
    pub uid: String,
    /// 企业 ID。
    pub enterprise_id: String,
    /// 昵称。
    pub nickname: String,
    /// 账号所属产品；空串等价于 [`PRODUCT_WORKBUDDY`]。
    ///
    /// 与 Go 侧 `Auth.Product` 同口径。**读取请一律走 [`Auth::product_of`]**，
    /// 直接比较空串容易漏判（见其注释）。
    pub product: String,
    /// 用户手动禁用：**只不参与选号**，养号任务照跑。
    ///
    /// 由宿主在导出凭证时写入 `account.no_route`。与 `pool` 里的 `disabled` 区分：
    ///
    /// - `NoRoute` → 用户意图「别把流量给它」；签到/上报/成长任务仍应执行
    /// - `disabled` → 网关判定该号已死（session 死 / 额度冻结），任务也跳过
    ///
    /// 与 Go 侧 `Auth.NoRoute` 一致。
    pub no_route: bool,
    /// 来源文件路径；refresh 后原子写回此处。
    pub file_path: String,
    /// 仍有剩余积分的套餐中最早到期时刻（Unix 秒）；0 = 未知。
    ///
    /// 由凭证文件的 `credit` 块带入（宿主查询积分后写入）或由签到任务刷新。
    /// 账号池据此做「先烧快过期额度」的分层选号。只读元数据，不参与 token 刷新写回判定。
    pub soonest_expire_at: i64,
}

impl Auth {
    /// 报告 token 是否将在 `within` 内过期（或已过期/无 expiry）。
    ///
    /// 与 Go 侧 `NeedsRefresh` 一致：`expires_at <= 0` 视为需要刷新
    ///（无过期信息时保守刷新，避免带着可能已失效的 token 发请求）。
    pub fn needs_refresh(&self, within: std::time::Duration) -> bool {
        if self.expires_at <= 0 {
            return true;
        }
        let now = chrono::Utc::now().timestamp();
        now + within.as_secs() as i64 >= self.expires_at
    }

    /// 是否国际版。
    ///
    /// 判据与 [`region_of_domain`] 完全一致 —— **只此一处**，与 Go 侧
    /// `(*Auth).IsIntl` 同口径。
    pub fn is_intl(&self) -> bool {
        region_of_domain(&self.domain) == Region::Intl
    }

    /// 返回账号所属区域。
    ///
    /// 与 Go 侧 `(*Auth).Region` 一致：依据凭证里的 `domain` 字段判定，
    /// 空域名按国服（历史行为）。
    pub fn region(&self) -> Region {
        region_of_domain(&self.domain)
    }

    /// 返回账号所属产品，空串归一成 [`PRODUCT_WORKBUDDY`]。
    ///
    /// 与 Go 侧 `(*Auth).ProductOf` 同口径：**归一化而非保留空串** ——
    /// 调用方写 `a.product_of() == PRODUCT_WORKBUDDY` 比 `== ""` 更不容易错。
    pub fn product_of(&self) -> &str {
        if self.product.is_empty() {
            PRODUCT_WORKBUDDY
        } else {
            &self.product
        }
    }

    /// 是否 QoderWork 账号。
    pub fn is_qoder(&self) -> bool {
        self.product == PRODUCT_QODER
    }

    /// 是否 ZCode 账号。
    pub fn is_zcode(&self) -> bool {
        self.product == PRODUCT_ZCODE
    }

    /// 是否 Trae 账号。
    ///
    /// 判据是**产品字段**而不是 uid 前缀：Trae 凭证的 uid 形如 `trae-<user_id>`，
    /// 但那是凭证加载层拼出来的；产品归属的唯一真相是 [`Auth::product`]。
    pub fn is_trae(&self) -> bool {
        self.product == PRODUCT_TRAE
    }
}

/// 账号所属服务区域。
///
/// 存在的意义：**同名模型在两个区域可能是不同的后端模型**，能力并不一致。
/// 实测（2026-09-16，`/v3/config` 元数据 + 逐账号发图验证）：
///
/// ```text
/// glm-5.3  国服「旗舰模型，擅长复杂软件工程与长程 Agent 任务」out=64000
///          国际版「能力均衡，适合日常使用」                out=48000
///          —— 国服后端能读图；国际版后端把图片换成固定占位符（token 增量
///          恒为 +29，与图片体积无关），模型只能回答「无法查看图片」。
/// ```
///
/// 因此「同一个 glm-5.3 时好时坏」的真实原因是**选号随机命中了两个不同后端**，
/// 而不是模型本身不稳定。见 `pool::PickForModelRegion` 与 server 的区域路由。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Region {
    /// 不限区域（默认；等价于引入本概念之前的行为）。
    Any,
    /// 国服（`*.workbuddy.cn` / `*.codebuddy.cn`）。
    CN,
    /// 国际版（`*.workbuddy.ai` / `*.codebuddy.ai`）。
    Intl,
}

impl Region {
    /// 与 Go 侧 `Region.String()` 逐字一致（`"cn"` / `"intl"` / `"any"`）。
    pub fn as_str(self) -> &'static str {
        match self {
            Region::CN => "cn",
            Region::Intl => "intl",
            Region::Any => "any",
        }
    }

    /// 解析区域名；未知/空一律按 [`Region::Any`]（与 Go 侧调用方口径一致）。
    pub fn parse(s: &str) -> Region {
        match s.trim().to_ascii_lowercase().as_str() {
            "cn" => Region::CN,
            "intl" => Region::Intl,
            _ => Region::Any,
        }
    }
}

/// 国际版域名白名单（**完整域名**）。
///
/// # ⚠⚠ 为什么必须是完整域名，不能再用 `ends_with(".ai")`（2026-09-22 修）
///
/// Go 侧原判据是 `strings.HasSuffix(domain, ".ai")`。它同时**过宽**又**过窄**：
///
/// - **过窄（真缺陷）**：Qoder 国际版的域名是 `qoder.sh`（`.sh` 不是 `.ai`）
///   ⇒ 被判成**国服** ⇒ `proxy_scope.intl` 对它**不生效** ⇒ 永远走直连。
///   实测 `openapi.qoder.sh`：直连 10.3s，走代理 1.2s（**慢 8.7 倍**）。
///   而 `qoderAuthOf` 的注释写着「域名用于区域判定（qoder.sh = 国际版）」
///   —— 写那段代码时以为判据认得出它，实际认不出。两处口径分叉。
/// - **过宽**：任何以 `.ai` 结尾的域名都算国际版。今天恰好没有反例，
///   但那是巧合 —— 判据不该依赖「恰好没人用别的 .ai 域名」。
///
/// 故改为**白名单精确匹配**：列出每个产品真实使用的域名，未列出的按国服处理
/// （与「历史只有国服账号」的向后兼容口径一致）。
///
/// ⚠ 新增产品/域名时**必须同步这里**，否则它的国际版账号会静默走直连 ——
/// 表现是「某个产品的国际版特别慢/连不上」，而其它产品正常。
///
/// ⚠⚠ 本项对 Rust 版是**真实缺陷修复**而非对齐：移植前 Rust 的
/// `Auth::is_intl` 正是那个旧的 `ends_with(".ai")` 判据，故 Qoder 国际版
/// （`qoder.sh`）与 ZCode 国际版（`api.z.ai`）在 Rust 侧**全部被判成国服**
/// ⇒ 走直连、`proxy_scope.intl` 对它们不生效。Rust 侧另有 `pool::region_of`
/// 曾按同一旧判据实现，本次一并统一到本函数。
const INTL_DOMAINS: &[&str] = &[
    // WorkBuddy 国际版（也是短信登录用的那个域）。
    "workbuddy.ai",
    // 未在任何真实凭证中观察到，但本仓库多处注释（config.rs / update.rs /
    // types.ts / client.go）都把「*.workbuddy.ai / *.codebuddy.ai」并列为国际版。
    // 保留它：真出现时能正确走代理，不出现时零成本。
    "codebuddy.ai",
    // Qoder 国际版的凭证域名（`qoder.Region.Domain()` 写的就是它），
    // 另有 API 域 openapi.qoder.sh / api3.qoder.sh。
    "qoder.sh",
    // Qoder 国际版授权页（`qoder.RegionFromDomain` 判为 intl）。
    "qoder.com",
    // ZCode（Z.AI）的 API 域（`zcode.DomainOfProvider` 写的就是它）。
    "z.ai",
];

/// 按**完整域名**判定区域。
///
/// 匹配规则：域名等于清单项，或以 `.<清单项>` 结尾（覆盖子域）。
/// 例如清单里有 `qoder.sh` 时，`api3.qoder.sh` 与 `qoder.sh` 都命中，
/// 而 `notqoder.sh` / `qoder.sh.evil.com` 都不命中。
///
/// 空域名按国服（历史行为，见 [`Region`] 的注释）。
pub fn region_of_domain(domain: &str) -> Region {
    let mut d = domain.trim().to_ascii_lowercase();
    if d.is_empty() {
        return Region::CN;
    }
    // 去掉可能的协议前缀与路径，容忍凭证里写成 URL 的情况。
    if let Some(i) = d.find("://") {
        d = d[i + 3..].to_string();
    }
    if let Some(i) = d.find(['/', '?', '#']) {
        d = d[..i].to_string();
    }
    // 去掉端口。用 rfind 与 Go 侧 LastIndex 一致（IPv6 字面量不是本场景的输入）。
    if let Some(i) = d.rfind(':') {
        d = d[..i].to_string();
    }
    for base in INTL_DOMAINS {
        if d == *base || d.ends_with(&format!(".{base}")) {
            return Region::Intl;
        }
    }
    Region::CN
}

/// 嵌套形凭证文件（插件 OAuth 输出）。
#[derive(Debug, Deserialize)]
struct NestedAuth {
    auth: Option<NestedAuthBlock>,
    account: Option<NestedAccountBlock>,
    credit: Option<CreditBlock>,
}

#[derive(Debug, Deserialize)]
struct NestedAuthBlock {
    #[serde(rename = "accessToken", default)]
    access_token: String,
    #[serde(rename = "refreshToken", default)]
    refresh_token: String,
    #[serde(rename = "expiresAt", default)]
    expires_at: i64,
    #[serde(default)]
    domain: String,
}

#[derive(Debug, Deserialize)]
struct NestedAccountBlock {
    #[serde(default)]
    uid: String,
    #[serde(rename = "enterpriseId", default)]
    enterprise_id: String,
    #[serde(default)]
    nickname: String,
    /// 用户手动禁用 = **只不接流量**，养号任务照跑。
    ///
    /// 与池里的 `disabled` 完全不同：那个是网关自己判定的（连续 3 次
    /// 12153 session 死 / 额度冻结），跑了也白跑，所以养号任务该跳过。
    /// 而这个只是「别把请求路由到它」，签到 / 活跃上报 / 成长任务等仍应照跑
    /// —— 所有者明确过这个语义。
    #[serde(default)]
    no_route: bool,
}

/// 扁平形凭证文件（手写 / 旧版）。
#[derive(Debug, Deserialize)]
struct FlatAuth {
    #[serde(rename = "accessToken", default)]
    access_token: String,
    #[serde(rename = "refreshToken", default)]
    refresh_token: String,
    #[serde(rename = "expiresAt", default)]
    expires_at: i64,
    #[serde(default)]
    domain: String,
    #[serde(default)]
    uid: String,
    #[serde(rename = "enterpriseId", default)]
    enterprise_id: String,
    #[serde(default)]
    nickname: String,
    /// 扁平形同样支持 `no_route`（两种形状的字段语义必须一致，
    /// 否则同一个账号换种写法就会「突然开始接流量」）。
    #[serde(default)]
    no_route: bool,
    /// 扁平形把 credit 字段平铺在顶层（兼容旧版手写凭证）。
    #[serde(rename = "soonestExpireAt", default)]
    soonest_expire_at: i64,
}

/// 凭证文件里的积分到期元数据（宿主写入，网关只读）。
#[derive(Debug, Deserialize)]
struct CreditBlock {
    /// 最近到期时刻。上游混用秒/毫秒，按量级归一。
    #[serde(rename = "soonestExpireAt", default)]
    soonest_expire_at: i64,
}

/// 解析凭证字节，兼容嵌套形与扁平形。
///
/// 返回 [`GatewayError::AuthParse`] 变体对应 Go 侧的
/// `storage_parse_error` / `parse_error: missing accessToken`。
pub fn parse(raw: &[u8]) -> Result<Auth> {
    if raw.is_empty() {
        return Err(GatewayError::AuthParse {
            path: String::new(),
            msg: "empty auth storage".into(),
        });
    }

    // 先用宽松结构探查顶层是否存在 "auth" 键 —— 与 Go 侧 probe 判定一致。
    let probe: serde_json::Value = serde_json::from_slice(raw).map_err(|e| {
        GatewayError::AuthParse {
            path: String::new(),
            msg: format!("storage_parse_error: {e}"),
        }
    })?;

    let a = if probe.get("auth").is_some() {
        let n: NestedAuth = serde_json::from_value(probe).map_err(|e| GatewayError::AuthParse {
            path: String::new(),
            msg: format!("storage_parse_error: {e}"),
        })?;
        let (at, rt, ea, dom) = match n.auth {
            Some(b) => (b.access_token, b.refresh_token, b.expires_at, b.domain),
            None => (String::new(), String::new(), 0, String::new()),
        };
        let (uid, eid, nick, no_route) = match n.account {
            Some(b) => (b.uid, b.enterprise_id, b.nickname, b.no_route),
            None => (String::new(), String::new(), String::new(), false),
        };
        Auth {
            access_token: at,
            refresh_token: rt,
            expires_at: ea,
            domain: dom,
            uid,
            enterprise_id: eid,
            nickname: nick,
            no_route,
            soonest_expire_at: normalize_epoch(n.credit.map(|c| c.soonest_expire_at).unwrap_or(0)),
            ..Default::default()
        }
    } else {
        let f: FlatAuth = serde_json::from_value(probe).map_err(|e| GatewayError::AuthParse {
            path: String::new(),
            msg: format!("storage_parse_error: {e}"),
        })?;
        Auth {
            access_token: f.access_token,
            refresh_token: f.refresh_token,
            expires_at: f.expires_at,
            domain: f.domain,
            uid: f.uid,
            enterprise_id: f.enterprise_id,
            nickname: f.nickname,
            no_route: f.no_route,
            soonest_expire_at: normalize_epoch(f.soonest_expire_at),
            ..Default::default()
        }
    };

    if a.access_token.trim().is_empty() {
        return Err(GatewayError::AuthParse {
            path: String::new(),
            msg: "parse_error: missing accessToken".into(),
        });
    }
    Ok(a)
}

/// 把秒/毫秒 epoch 统一成秒（上游混用两种精度）。
///
/// 阈值 1e12：小于它视为秒（2001 年后、2286 年前的秒级时间戳都在此区间），
/// 大于等于它视为毫秒。与 Go 侧 `normalizeEpoch` 同阈值、同语义。
pub fn normalize_epoch(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    if n > 1_000_000_000_000 {
        return n / 1000;
    }
    n
}

impl Auth {
    /// 以嵌套形原子写回 [`Auth::file_path`]（tmp + rename），保持插件可读格式。
    ///
    /// 防御：access_token 为空时拒绝写回，避免误用空凭证覆盖有效文件。
    /// 与 Go 侧 `SaveAtomic` 一致（Go 版全程持 mutex；Rust 版由调用方
    /// 通过 `&mut self` / 上层锁保证独占，见 `pool` 模块的写回路径）。
    pub fn save_atomic(&self) -> Result<()> {
        if self.access_token.trim().is_empty() {
            return Err(GatewayError::AuthParse {
                path: self.file_path.clone(),
                msg: format!("save refused: empty accessToken (uid={})", self.uid),
            });
        }
        let path = self.file_path.clone();
        if path.is_empty() {
            return Err(GatewayError::AuthSave {
                path,
                msg: "no FilePath set".into(),
            });
        }

        let mut doc = serde_json::Map::new();
        let mut auth = serde_json::Map::new();
        auth.insert("accessToken".into(), self.access_token.clone().into());
        auth.insert("refreshToken".into(), self.refresh_token.clone().into());
        auth.insert("expiresAt".into(), self.expires_at.into());
        auth.insert("domain".into(), self.domain.clone().into());
        let mut account = serde_json::Map::new();
        account.insert("uid".into(), self.uid.clone().into());
        account.insert("enterpriseId".into(), self.enterprise_id.clone().into());
        account.insert("nickname".into(), self.nickname.clone().into());

        // ★★ `account.no_route` 必须原样保留（2026-09-30 修，对应 SPEC 附录 A-1）。
        //
        // 它是**宿主**写的（账号页的「停止接流量」开关），而本方法是**网关**
        // 写的（令牌刷新时）。令牌刷新很频繁 —— 若这里整体覆盖，用户的禁用
        // 标记会在几分钟内被**悄悄抹掉**：那个账号重新开始接流量，而用户以为
        // 它还停着。症状是「明明关了它，请求还是走到它上面」。
        //
        // 保留策略：**从磁盘读回再合并**，而不是只用内存字段重建。这样宿主
        // 后来加的、我们还不认识的字段也不会丢（`credit` 段就是先例）。
        if let Ok(raw) = std::fs::read(&path) {
            if let Ok(existing) = serde_json::from_slice::<serde_json::Value>(&raw) {
                if let Some(obj) = existing.as_object() {
                    // 未知的顶层字段：先原样搬过来，随后被下面显式写入的段覆盖。
                    for (k, v) in obj {
                        if k != "auth" && k != "account" {
                            doc.insert(k.clone(), v.clone());
                        }
                    }
                    // 未知的 account 字段（含 `no_route`）：读回磁盘值。
                    if let Some(acct) = obj.get("account").and_then(|v| v.as_object()) {
                        for (k, v) in acct {
                            if k != "uid" && k != "enterpriseId" && k != "nickname" {
                                account.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
            }
        }
        doc.insert("auth".into(), auth.into());
        doc.insert("account".into(), account.into());

        // 积分到期元数据必须原样保留：它由宿主或签到任务写入，而 token 刷新
        // 会重写整个文件。若这里丢掉，一次保活就会抹掉选号依据
        //（表现为分层均衡静默退化成原来的三因子随机）。
        if self.soonest_expire_at > 0 {
            let mut credit = serde_json::Map::new();
            credit.insert("soonestExpireAt".into(), self.soonest_expire_at.into());
            doc.insert("credit".into(), credit.into());
        }

        let value = serde_json::Value::Object(doc);
        let raw = serde_json::to_string_pretty(&value)
            .map_err(|e| GatewayError::AuthSave { path: path.clone(), msg: e.to_string() })?;

        atomic_write(Path::new(&path), raw.as_bytes())
            .map_err(|e| GatewayError::AuthSave { path: path.clone(), msg: e })?;
        Ok(())
    }
}

/// 原子写文件：同目录写 `.tmp` 再 rename。
///
/// 与 Go 侧 `os.WriteFile(tmp) + os.Rename` 同策略：同目录保证 rename 不跨设备。
pub fn atomic_write(path: &Path, data: &[u8]) -> std::result::Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).map_err(|e| format!("write tmp {}: {e}", tmp.display()))?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // 清理残留 tmp，避免堆积
            let _ = std::fs::remove_file(&tmp);
            Err(format!("rename {} -> {}: {e}", tmp.display(), path.display()))
        }
    }
}

/// 扫描并解析 `dir` 下 `workbuddy*.json`；解析失败的文件静默跳过。
///
/// 返回顺序与 Go 侧 `filepath.Glob` 一致（字典序，由 `read_dir` + sort 保证，
/// 避免依赖平台返回顺序导致启动选号不确定）。
pub fn load_dir(dir: &str) -> Result<Vec<Auth>> {
    let d = Path::new(dir);
    if !d.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(d)
        .map_err(|e| GatewayError::Other(format!("read_dir {dir}: {e}")))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("workbuddy") && n.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();

    let mut out = Vec::new();
    for f in files {
        let raw = match std::fs::read(&f) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut a = match parse(&raw) {
            Ok(a) => a,
            Err(_) => continue,
        };
        a.file_path = f.to_string_lossy().to_string();
        out.push(a);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NESTED: &str = r#"{
      "auth": {
        "accessToken": "at-123",
        "refreshToken": "rt-456",
        "expiresAt": 1700000000,
        "domain": "https://copilot.tencent.com"
      },
      "account": {
        "uid": "u1",
        "enterpriseId": "e1",
        "nickname": "猫猫"
      },
      "credit": { "soonestExpireAt": 1735689600000 }
    }"#;

    const FLAT: &str = r#"{
      "accessToken": "at-flat",
      "refreshToken": "rt-flat",
      "expiresAt": 1700000000,
      "domain": "https://copilot.tencent.com",
      "uid": "u2",
      "enterpriseId": "e2",
      "nickname": "flat",
      "soonestExpireAt": 1735689600
    }"#;

    #[test]
    fn parse_nested_shape() {
        let a = parse(NESTED.as_bytes()).unwrap();
        assert_eq!(a.access_token, "at-123");
        assert_eq!(a.refresh_token, "rt-456");
        assert_eq!(a.expires_at, 1700000000);
        assert_eq!(a.uid, "u1");
        assert_eq!(a.enterprise_id, "e1");
        assert_eq!(a.nickname, "猫猫");
        // 毫秒 → 秒
        assert_eq!(a.soonest_expire_at, 1735689600);
    }

    #[test]
    fn parse_flat_shape() {
        let a = parse(FLAT.as_bytes()).unwrap();
        assert_eq!(a.access_token, "at-flat");
        assert_eq!(a.uid, "u2");
        assert_eq!(a.nickname, "flat");
        assert_eq!(a.soonest_expire_at, 1735689600);
    }

    #[test]
    fn shape_detection_uses_top_level_auth_key() {
        // 没有 "auth" 键 → 扁平形，即使内部字段长得很像嵌套形
        let weird = r#"{"accessToken":"x","uid":"u9"}"#;
        let a = parse(weird.as_bytes()).unwrap();
        assert_eq!(a.uid, "u9");
        assert_eq!(a.access_token, "x");
    }

    #[test]
    fn missing_access_token_rejected() {
        let bad = r#"{"uid":"u1"}"#;
        let err = parse(bad.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("missing accessToken"), "{err}");

        // 嵌套形缺 accessToken 同样拒绝
        let bad2 = r#"{"auth":{"refreshToken":"r"},"account":{"uid":"u1"}}"#;
        let err2 = parse(bad2.as_bytes()).unwrap_err();
        assert!(err2.to_string().contains("missing accessToken"), "{err2}");
    }

    #[test]
    fn empty_and_invalid_input_rejected() {
        assert!(parse(b"").is_err());
        assert!(parse(b"not json").is_err());
    }

    #[test]
    fn normalize_epoch_handles_seconds_and_millis() {
        assert_eq!(normalize_epoch(0), 0);
        assert_eq!(normalize_epoch(-5), 0);
        assert_eq!(normalize_epoch(1735689600), 1735689600);
        assert_eq!(normalize_epoch(1735689600000), 1735689600);
    }

    #[test]
    fn is_intl_matches_every_real_domain() {
        // 对齐 Go：`internal/auth/region_domain_test.go::TestIsIntlMatchesEveryRealDomain`。
        //
        // 域名取值来自代码里的字面量（不是猜的）：
        //   - WorkBuddy: smslogin.go 的 www.workbuddy.ai / www.workbuddy.cn，
        //     以及凭证里的 copilot.tencent.com / www.codebuddy.cn
        //   - Qoder: region.go 的 Region.Domain() → qoder.sh / qoder.com.cn
        //   - ZCode: dispatch.go 的 DomainOfProvider → api.z.ai / open.bigmodel.cn
        let cases: &[(&str, bool, &str)] = &[
            // ---- 国际版：必须判 true，否则拿不到代理 ----
            ("www.workbuddy.ai", true, "WorkBuddy 国际版（短信登录域）"),
            ("workbuddy.ai", true, "裸根域"),
            ("www.codebuddy.ai", true, "注释里并列为国际版（未在真实凭证中出现）"),
            ("qoder.sh", true, "Qoder 国际版（凭证里写的就是它）"),
            ("api3.qoder.sh", true, "Qoder 国际版网关域"),
            ("openapi.qoder.sh", true, "Qoder 国际版 OpenAPI 域"),
            ("qoder.com", true, "Qoder 国际版授权页"),
            ("api.z.ai", true, "ZCode（Z.AI）API 域"),
            ("z.ai", true, "Z.AI 裸根域"),
            // ---- 国服：必须判 false，否则会被无谓地绕进代理 ----
            ("copilot.tencent.com", false, "WorkBuddy 国服"),
            ("www.codebuddy.cn", false, "CodeBuddy 国服"),
            ("www.workbuddy.cn", false, "WorkBuddy 国服（短信登录用）"),
            ("qoder.com.cn", false, "Qoder 国服"),
            ("openapi.qoder.com.cn", false, "Qoder 国服 OpenAPI 域"),
            ("gateway.qoder.com.cn", false, "Qoder 国服网关域"),
            ("open.bigmodel.cn", false, "ZCode 国服（智谱）"),
            ("", false, "空域名按国服（历史行为）"),
        ];
        for (domain, want, why) in cases {
            let a = Auth { domain: (*domain).into(), ..Default::default() };
            assert_eq!(a.is_intl(), *want, "IsIntl({domain:?}) 应为 {want}（{why}）");
            // Region() 必须与 IsIntl 一致（两者同源，别再分叉）。
            let want_region = if *want { Region::Intl } else { Region::CN };
            assert_eq!(
                a.region(),
                want_region,
                "Region({domain:?}) 应与 IsIntl 一致（{why}）"
            );
        }
    }

    #[test]
    fn is_intl_does_not_match_lookalike_domains() {
        // 对齐 Go：`TestIsIntlDoesNotMatchLookalikeDomains`。
        //
        // 后缀匹配的另一个毛病是「过宽」。正确的匹配是「等于，或以 `.＋清单项`
        // 结尾」，故 `api3.qoder.sh` 命中子域，而下面这些**只是形似**。
        for d in [
            "notqoder.sh",
            "qoder.sh.evil.com",
            "evil-workbuddy.ai.com", // .ai 在中间，TLD 是 .com
            "fakez.ai",
            "z.ai.evil.com",
            "qoder.shx",
            "myqoder.com",
        ] {
            assert!(
                !(&Auth { domain: d.into(), ..Default::default() }).is_intl(),
                "IsIntl({d:?}) 不应为 true —— 它只是形似，不是我们的域名"
            );
        }
    }

    #[test]
    fn is_intl_is_case_and_noise_tolerant() {
        // 对齐 Go：`TestIsIntlIsCaseAndNoiseTolerant`。
        //
        // 这些不是假想：手写凭证与旧版导出都出现过带 URL 形态的 domain。
        for d in [
            "WWW.WorkBuddy.AI",
            "  www.workbuddy.ai  ",
            "https://www.workbuddy.ai",
            "https://www.workbuddy.ai/v2/chat",
            "www.workbuddy.ai:443",
            "https://qoder.sh/",
        ] {
            assert!(
                (&Auth { domain: d.into(), ..Default::default() }).is_intl(),
                "IsIntl({d:?}) 应为 true —— 归一化后它就是国际版域名"
            );
        }
        // 国服侧同样要容忍噪声（别把带端口的国服域误判成国际版）。
        for d in [
            "HTTPS://Copilot.Tencent.COM",
            "copilot.tencent.com:443",
            "https://qoder.com.cn/path",
        ] {
            assert!(
                !(&Auth { domain: d.into(), ..Default::default() }).is_intl(),
                "IsIntl({d:?}) 不应为 true（是国服域名）"
            );
        }
    }

    #[test]
    fn needs_refresh_semantics() {
        use std::time::Duration;
        let mut a = Auth::default();
        // expires_at<=0 → 需要刷新（无过期信息，保守）
        a.expires_at = 0;
        assert!(a.needs_refresh(Duration::from_secs(600)));

        // 已过期 → 需要刷新
        a.expires_at = 1;
        assert!(a.needs_refresh(Duration::from_secs(600)));

        // 远期 → 不需要
        a.expires_at = chrono::Utc::now().timestamp() + 7200;
        assert!(!a.needs_refresh(Duration::from_secs(600)));

        // 落在 skew 窗口内 → 需要
        a.expires_at = chrono::Utc::now().timestamp() + 60;
        assert!(a.needs_refresh(Duration::from_secs(600)));
    }

    #[test]
    fn save_atomic_roundtrip_preserves_credit() {
        let dir = std::env::temp_dir().join(format!("wbswg-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let fp = dir.join("workbuddy-u1.json");

        let mut a = Auth {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 1700000000,
            uid: "u1".into(),
            soonest_expire_at: 1735689600,
            ..Default::default()
        };
        a.file_path = fp.to_string_lossy().to_string();
        a.save_atomic().unwrap();

        let raw = std::fs::read(&fp).unwrap();
        let back = parse(&raw).unwrap();
        assert_eq!(back.access_token, "at");
        assert_eq!(back.refresh_token, "rt");
        assert_eq!(back.uid, "u1");
        // credit 块必须保留
        assert_eq!(back.soonest_expire_at, 1735689600);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_atomic_refuses_empty_token() {
        let mut a = Auth::default();
        a.file_path = r"C:\whatever.json".into();
        let err = a.save_atomic().unwrap_err();
        assert!(err.to_string().contains("empty accessToken"), "{err}");
    }

    #[test]
    fn save_atomic_requires_file_path() {
        let a = Auth {
            access_token: "at".into(),
            ..Default::default()
        };
        let err = a.save_atomic().unwrap_err();
        assert!(err.to_string().contains("no FilePath"), "{err}");
    }

    #[test]
    fn load_dir_scans_and_skips_bad_files() {
        let dir = std::env::temp_dir().join(format!("wbswg-dirtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(
            dir.join("workbuddy-a.json"),
            r#"{"accessToken":"a","uid":"a"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("workbuddy-b.json"),
            r#"{"accessToken":"b","uid":"b"}"#,
        )
        .unwrap();
        // 坏文件：缺 accessToken → 跳过
        std::fs::write(dir.join("workbuddy-bad.json"), r#"{"uid":"x"}"#).unwrap();
        // 非 workbuddy 前缀 → 不扫
        std::fs::write(dir.join("other.json"), r#"{"accessToken":"o","uid":"o"}"#).unwrap();

        let loaded = load_dir(&dir.to_string_lossy()).unwrap();
        assert_eq!(loaded.len(), 2, "应扫到 2 个有效凭证: {loaded:?}");
        let mut uids: Vec<_> = loaded.iter().map(|a| a.uid.as_str()).collect();
        uids.sort();
        assert_eq!(uids, vec!["a", "b"]);
        // file_path 应被回填
        assert!(loaded.iter().all(|a| !a.file_path.is_empty()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_dir_missing_directory_returns_empty() {
        let out = load_dir("/nonexistent/path/for/wbswg").unwrap();
        assert!(out.is_empty());
    }
}
