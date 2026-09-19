//! qoder_login.rs 通过内嵌网关二进制完成 Qoder 的 OAuth 设备流登录。
//!
//! ## 为什么走子进程而不是在 Rust 里重写一遍
//!
//! COSY 签名（RSA 包 AES + AES-CBC 身份 + MD5）与设备流协议已经在 Go 侧
//! `internal/qoder` 实现并**经真实上游验证通过**（两区签名均被接受）。
//! 在 Rust 里重写一份意味着：
//!   · 两套实现要各自跟进上游改动（签名算法一旦变动，漏改一边就静默失效）；
//!   · 需要重复踩「16 个 ASCII 字符的临时密钥」「cosy-user 头不能少」这些坑；
//!   · 无法复用已有的 46 个单元测试与真机验证脚本。
//!
//! 因此宿主只做**编排**：调 `gateway.exe qoder-login url|poll` 两个子命令，
//! 把结果转成前端可用的 JSON。协议实现始终只有一份。
//!
//! ## 两个子命令的形状
//!
//! ```text
//! gateway.exe qoder-login url  --region cn|intl
//!   → stdout: {"authUrl":"...","sessionId":"...","region":"cn"}
//!
//! gateway.exe qoder-login poll --session <id> --auth-dir <dir>
//!   → stdout: {"status":"pending"}
//!             {"status":"ok","uid":"...","nickname":"...","region":"cn"}
//!             {"status":"error","message":"..."}
//! ```
//!
//! ⚠ 上面必须写成 ```text 围栏块，**不能**只用 4 空格缩进 ——
//! rustdoc 会把缩进块当成 **Rust doctest** 编译，而这里的 `→` 不是合法
//! Rust 记号，于是 `cargo test` 报 `unknown start of token: \u{2192}`
//! （`cargo build` 却正常，因为构建不跑 doctest）。实测踩到过。
//!
//! 会话状态由 Go 侧自己持有（内存 + 临时文件），宿主只传 sessionId。

use std::path::PathBuf;
use std::process::Command;

use serde_json::{json, Value};

use crate::modules::{gateway, qoder_account};

/// 调 Go 侧登录子命令的通用执行器。
fn run_login_cmd(args: &[&str]) -> Result<Value, String> {
    let exe = gateway::resolve_gateway_exe()
        .ok_or_else(|| "找不到网关可执行文件，无法登录 Qoder（请先安装或配置网关）".to_string())?;

    let out = Command::new(&exe)
        .arg("qoder-login")
        .args(args)
        .output()
        .map_err(|e| format!("启动登录子命令失败: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    if !out.status.success() {
        // stderr 优先（Go 侧把可读错误写那里），其次 stdout，最后给退出码
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            format!("退出码 {:?}", out.status.code())
        };
        return Err(format!("登录失败: {detail}"));
    }

    serde_json::from_str(stdout.trim())
        .map_err(|e| format!("登录子命令返回了非法 JSON: {e}（原始输出：{}）", truncate(stdout.trim(), 200)))
}

/// 发起 Qoder 登录，返回用户在浏览器打开的授权链接。
///
/// 不做网络请求、不写磁盘 —— 只是生成 PKCE 与机器指纹。
pub fn login_start(region: &str) -> Result<Value, String> {
    let region = normalize_region(region)?;

    // ⚠ **必须把 auth_dir 传下去** —— 我漏了这一处，造成两个真实故障：
    //
    //  1. **隔离失效**：不传时 Go 侧用它的默认目录
    //     （`~/.wb-switch/qoder/auths`），于是独立实例的登录会话
    //     写进了**使用者的真实数据目录**（实测确认：独立实例跑一次
    //     start，真实目录里就多一个会话文件）。
    //
    //  2. **poll 永远失败**：start 把会话写进 A 目录，而 poll 带着
    //     auth_dir 去 B 目录找 —— 找不到就报
    //     「登录会话不存在或已过期，请重新点击「登录」」。
    //     这正是使用者反馈的现象：「出来链接 就提我重新点击登录」。
    //
    // 对照：ZCode 侧的 `login_start` **有**传（见 zcode_login.rs）。
    // 同一个仓库里两个产品写法不一致，漏的那个就出了这个问题。
    let auth_dir = qoder_account::auth_dir();
    qoder_account::ensure_dirs()?;
    let auth_dir_s = auth_dir.to_string_lossy().to_string();

    let r = run_login_cmd(&["url", "--region", region, "--auth-dir", &auth_dir_s])?;

    // 校验 Go 侧返回了必需字段：缺 authUrl 时界面会显示一个空链接，
    // 用户点不开却不知道原因 —— 提前报错更好。
    if r.get("authUrl").and_then(Value::as_str).unwrap_or("").is_empty() {
        return Err(format!("登录子命令没有返回授权链接（原始：{r}）"));
    }
    if r.get("sessionId").and_then(Value::as_str).unwrap_or("").is_empty() {
        return Err(format!("登录子命令没有返回会话标识（原始：{r}）"));
    }
    Ok(r)
}

/// 轮询一次登录结果。
///
/// 三种返回（对应 Go 侧的三种状态）：
///
/// ```text
/// 待授权 → {"status":"pending"}
/// 成功   → {"status":"ok","uid":...,"account":{...}}  （并已登记进账号库）
/// 失败   → Err(...)
/// ```
///
/// **成功时顺带登记账号库**：让调用方（界面）只需调一个命令，
/// 不必再单独调一次 upsert —— 少一步就少一处「忘了调」的可能。
pub fn login_poll(session_id: &str) -> Result<Value, String> {
    if session_id.trim().is_empty() {
        return Err("会话标识为空，无法轮询（请先发起登录）".into());
    }
    let auth_dir = qoder_account::auth_dir();
    qoder_account::ensure_dirs()?;
    let auth_dir_s = auth_dir.to_string_lossy().to_string();

    let r = run_login_cmd(&["poll", "--session", session_id, "--auth-dir", &auth_dir_s])?;

    let status = r.get("status").and_then(Value::as_str).unwrap_or("");
    match status {
        "pending" => Ok(r),
        "ok" => {
            // 登记进账号库（区域从 Go 侧返回里取，取不到则留空让用户自己选）
            let uid = r.get("uid").and_then(Value::as_str).unwrap_or("").to_string();
            if uid.is_empty() {
                return Err(format!("登录成功但没返回 uid（原始：{r}）"));
            }
            let region = r.get("region").and_then(Value::as_str).unwrap_or("");
            let nickname = r.get("nickname").and_then(Value::as_str).unwrap_or("");

            let acc = qoder_account::upsert_account(
                &uid,
                &json!({ "region": region, "nickname": nickname }),
            )?;

            Ok(json!({
                "status": "ok",
                "uid": uid,
                "account": acc.to_view(),
            }))
        }
        "error" => Err(r
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("登录失败（上游未给出原因）")
            .to_string()),
        other => Err(format!("登录子命令返回了未知状态 {other:?}（原始：{r}）")),
    }
}

/// 规范化区域参数。
///
/// 只接受 cn / intl：区域决定授权页域名，猜错会让用户打开错误区域的页面。
/// 空值**不默认成国服** —— 那正是"国际版账号被当成国服"这类问题的来源。
fn normalize_region(region: &str) -> Result<&'static str, String> {
    match region.trim().to_lowercase().as_str() {
        "cn" => Ok("cn"),
        "intl" | "global" | "international" => Ok("intl"),
        "" => Err("请选择区域（国服或国际版）：两区的授权页不同，不能默认".into()),
        other => Err(format!("未知区域 {other:?}（应为 cn 或 intl）")),
    }
}

/// 刷新一个账号的**额度、到期时间、可用模型**。
///
/// 与 ZCode 侧同构（见 `zcode_login::refresh_account` 的注释）：
/// `Client.FetchQuota` / `FetchModels` 早已实现却**没有生产者调用**，
/// 于是界面上额度恒为 0、看不到支持模型。
///
/// ## Qoder 的一个特别之处
///
/// 新登录的账号通常返回 `total: 0` 且 `isQuotaExceeded: true` ——
/// 那**不是**"用超了"，而是"还没有分配额度"。故这里把两个信号都
/// 回传给界面，由它决定怎么措辞（而不是在这里武断地说"已超额"）。
pub fn refresh_account(uid: &str) -> Result<Value, String> {
    let uid = uid.trim();
    if uid.is_empty() {
        return Err("账号 uid 不能为空".into());
    }

    let auth_dir = qoder_account::auth_dir();
    let auth_dir_s = auth_dir.to_string_lossy().to_string();

    // ---- 额度 ----
    let mut credits: Option<i64> = None;
    let mut credits_total: Option<i64> = None;
    let mut expire_at: Option<i64> = None;
    let mut plan_tier = String::new();
    let mut exceeded = false;
    let mut usage_percent = 0.0f64;
    let mut quota_error: Option<String> = None;

    match run_login_cmd(&["quota", "--uid", uid, "--auth-dir", &auth_dir_s]) {
        Ok(r) => {
            if r.get("status").and_then(Value::as_str) == Some("ok") {
                credits = r.get("remaining").and_then(Value::as_i64);
                credits_total = r.get("total").and_then(Value::as_i64);
                let e = r.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
                if e > 0 {
                    expire_at = Some(e);
                }
                plan_tier = r.get("planTierName").and_then(Value::as_str).unwrap_or("").to_string();
                exceeded = r.get("exceeded").and_then(Value::as_bool).unwrap_or(false);
                usage_percent = r.get("usagePercent").and_then(Value::as_f64).unwrap_or(0.0);
            } else {
                quota_error = Some(
                    r.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("额度查询失败")
                        .to_string(),
                );
            }
        }
        Err(e) => quota_error = Some(e),
    }

    // ---- 模型 ----
    let mut models: Vec<Value> = Vec::new();
    let mut models_error: Option<String> = None;
    match run_login_cmd(&["models", "--uid", uid, "--auth-dir", &auth_dir_s]) {
        Ok(r) => {
            if r.get("status").and_then(Value::as_str) == Some("ok") {
                if let Some(arr) = r.get("models").and_then(Value::as_array) {
                    models = arr.clone();
                }
            } else {
                models_error = Some(
                    r.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("模型清单查询失败")
                        .to_string(),
                );
            }
        }
        Err(e) => models_error = Some(e),
    }

    // ---- 写回账号库 ----
    let mut patch = serde_json::Map::new();
    if let Some(c) = credits {
        patch.insert("credits".into(), json!(c));
    }
    if let Some(t) = credits_total {
        patch.insert("creditsTotal".into(), json!(t));
    }
    if let Some(e) = expire_at {
        patch.insert("expireAt".into(), json!(e));
    }
    // 模型清单落库（界面显示 + 网关的渠道标签）。
    //
    // ⚠ 只在**成功取到**时写 —— 取不到时写空数组会抹掉上一次的好数据，
    // 界面从"有 2 个模型"变成"没有模型"，而原因只是一次网络抖动。
    if models_error.is_none() {
        patch.insert("models".into(), json!(models));
    }
    let acc = qoder_account::upsert_account(uid, &Value::Object(patch))?;

    Ok(json!({
        "status": "ok",
        "account": acc.to_view(),
        "quota": {
            "remaining": credits,
            "total": credits_total,
            "expiresAt": expire_at,
            "planTierName": plan_tier,
            "exceeded": exceeded,
            "usagePercent": usage_percent,
            "error": quota_error,
        },
        "models": models,
        "modelsError": models_error,
    }))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect::<String>() + "…"
}

/// 从**Qoder 客户端自己的登录态**导入凭证（**一键导入，主路径**）。
///
/// # 为什么这才是主路径
///
/// 网页授权在本机**走不通**：授权链接的 `redirect_uri` 是
/// `qoder-work-cn://` —— 一个**自定义协议**，只有真正的 Qoder 客户端
/// 才会注册它。我们不是它，浏览器授权完成后**无处回调**。
///
/// ⚠ 但要说清楚：我们的**轮询**端点形状是对的。实测拿会话里的
/// nonce/verifier 去打 `/api/v1/deviceToken/poll` 得到
///
/// ```text
/// 401 {"errorCode":"Unauthorized","errorMessage":"User not authenticated"}
/// ```
///
/// 那正是"**还没授权**"的预期响应。所以不是请求写错了，而是
/// **授权这一步在本机根本没有完成的条件**。
///
/// 而客户端已经登录了 —— 它的登录态就在它的数据目录里
///（Electron safeStorage 加密，由 DPAPI 绑定当前 Windows 用户）。
/// 读它就够了：**用户什么都不用点**。
///
/// # 实现为什么在 Go 侧
///
/// DPAPI 与 Electron 的密钥解包（`os_crypt.encrypted_key` → AES-256-GCM）
/// 已在 Go 侧实现并**用真实客户端数据实测通过**（`internal/qoderclient`）。
/// 在 Rust 重写意味着两套实现要各自跟进客户端的格式变化。
///
/// 本函数只做**编排**：调 `gateway.exe qoder-login import-client`，
/// 把结果登记进账号库。
pub fn import_from_client(client_dir: &str) -> Result<Value, String> {
    qoder_account::ensure_dirs()?;
    let auth_dir = qoder_account::auth_dir();
    let auth_dir_s = auth_dir.to_string_lossy().to_string();

    let mut args = vec!["import-client", "--auth-dir", auth_dir_s.as_str()];
    let dir = client_dir.trim();
    if !dir.is_empty() {
        args.push("--client-dir");
        args.push(dir);
    }

    let r = run_login_cmd(&args)?;

    let uid = r.get("uid").and_then(Value::as_str).unwrap_or("").to_string();
    if uid.is_empty() {
        return Err(format!("导入成功但没返回 uid（原始：{r}）"));
    }
    let nickname = r.get("nickname").and_then(Value::as_str).unwrap_or("").to_string();
    let region = r.get("region").and_then(Value::as_str).unwrap_or("cn").to_string();
    let client = r.get("clientDir").and_then(Value::as_str).unwrap_or("").to_string();
    // 头像也一并存下来 —— 使用者的反馈是"已授权后不显示头像"，
    // 而客户端登录态里本来就有 `user.avatarUrl`。
    let avatar = r.get("avatarUrl").and_then(Value::as_str).unwrap_or("").to_string();

    // 登记进账号库。
    //
    // ⚠ 只传 Qoder 存在的键（nickname / region）。Qoder 没有
    // `provider` 概念（那是 ZCode 的：Z.AI vs 智谱）—— 传了会被
    // apply_patch 静默忽略，但那是"看起来生效了其实没有"的隐患，
    // 不如一开始就不传。
    let acc = qoder_account::upsert_account(
        &uid,
        &json!({ "nickname": nickname, "region": region, "avatarUrl": avatar }),
    )?;

    Ok(json!({
        "status": "ok",
        "uid": uid,
        "nickname": nickname,
        "region": region,
        "clientDir": client,
        "expiresAt": r.get("expiresAt").cloned().unwrap_or(Value::Null),
        "account": acc.to_view(),
    }))
}

/// 导入已有凭证文件（方式 B）。
///
/// 支持两种来源：
///   · 单个文件 → 复制进 auths/ 并登记
///   · 目录     → 扫描其中的 qoder*.json，逐个导入
///
/// 返回每个文件的导入结果（成功/失败原因），而不是遇到第一个失败就中断 ——
/// 用户批量导入时最想知道的是"哪些成了、哪些没成"。
pub fn import_credentials(path: &str) -> Result<Value, String> {
    let src = PathBuf::from(path.trim());
    if !src.exists() {
        return Err(format!("路径不存在: {}", src.display()));
    }
    qoder_account::ensure_dirs()?;
    let dst_dir = qoder_account::auth_dir();

    // 收集待导入文件
    let files: Vec<PathBuf> = if src.is_dir() {
        let mut v: Vec<PathBuf> = std::fs::read_dir(&src)
            .map_err(|e| format!("读取目录失败: {e}"))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension().map(|x| x == "json").unwrap_or(false)
                    && p.file_name()
                        .map(|n| n.to_string_lossy().starts_with("qoder"))
                        .unwrap_or(false)
            })
            .collect();
        v.sort();
        v
    } else {
        vec![src.clone()]
    };

    if files.is_empty() {
        return Err(format!(
            "目录里没有找到 qoder*.json 凭证文件: {}",
            src.display()
        ));
    }

    let mut imported = Vec::new();
    let mut failed = Vec::new();

    for f in &files {
        match import_one(f, &dst_dir) {
            Ok((uid, region)) => {
                // 登记进账号库（昵称留空，等下次额度/信息查询补齐）
                let _ = qoder_account::upsert_account(&uid, &json!({ "region": region }));
                imported.push(json!({
                    "file": f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    "uid": uid,
                    "region": region,
                }));
            }
            Err(e) => failed.push(json!({
                "file": f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                "error": e,
            })),
        }
    }

    Ok(json!({
        "imported": imported,
        "failed": failed,
        "importedCount": imported.len(),
        "failedCount": failed.len(),
    }))
}

/// 导入单个凭证文件：解析 → 判区域 → 复制到 auths/。
///
/// 返回 (uid, region)。
fn import_one(src: &PathBuf, dst_dir: &PathBuf) -> Result<(String, String), String> {
    let raw = std::fs::read_to_string(src).map_err(|e| format!("读取失败: {e}"))?;
    let doc: Value = serde_json::from_str(&raw).map_err(|e| format!("不是合法 JSON: {e}"))?;

    // 兼容嵌套形与扁平形（与 Go 侧 Parse 的口径一致）
    let (uid, domain) = if let Some(auth) = doc.get("auth") {
        (
            doc.pointer("/account/uid").and_then(Value::as_str).unwrap_or("").to_string(),
            auth.get("domain").and_then(Value::as_str).unwrap_or("").to_string(),
        )
    } else {
        let uid = doc
            .get("uid")
            .or_else(|| doc.get("user_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        (uid, doc.get("domain").and_then(Value::as_str).unwrap_or("").to_string())
    };

    // uid 缺失时从文件名兜底（qoder-<uid>.json / qoderwork-<uid>.json）
    let uid = if uid.trim().is_empty() {
        let base = src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        base.strip_prefix("qoder-")
            .or_else(|| base.strip_prefix("qoderwork-"))
            .and_then(|s| s.strip_suffix(".json"))
            .unwrap_or("")
            .to_string()
    } else {
        uid
    };

    if uid.trim().is_empty() {
        return Err("无法确定账号 uid（文件里没有 uid 字段，文件名也不匹配 qoder-<uid>.json）".into());
    }

    // 区域判定（与 Go 侧 RegionFromDomain 一致）
    let region = region_of_domain(&domain);

    // 复制到 auths/（文件名统一成 qoder-<uid>.json，让 Go 侧能扫到）
    let dst = dst_dir.join(format!("qoder-{}.json", qoder_account::sanitize_uid(&uid)));
    std::fs::copy(src, &dst).map_err(|e| format!("复制到 {} 失败: {e}", dst.display()))?;

    Ok((uid, region))
}

/// 按域名判区域（与 Go 侧口径一致，两处必须同步）。
fn region_of_domain(domain: &str) -> String {
    let d = domain.trim().to_lowercase();
    if d.is_empty() {
        return String::new(); // 未知 → 留空，让用户自己选
    }
    if d.ends_with(".com.cn") || d.ends_with("qoder.com.cn") {
        return "cn".into();
    }
    if d.ends_with(".sh") || d.ends_with("qoder.sh") || d.ends_with("qoder.com") {
        return "intl".into();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_region_rejects_empty() {
        // 空值不能默认成国服 —— 那正是"国际版被当国服"这类问题的来源
        assert!(normalize_region("").is_err());
        assert!(normalize_region("  ").is_err());
    }

    #[test]
    fn normalize_region_accepts_known() {
        assert_eq!(normalize_region("cn").unwrap(), "cn");
        assert_eq!(normalize_region("CN").unwrap(), "cn");
        assert_eq!(normalize_region("intl").unwrap(), "intl");
        assert_eq!(normalize_region("global").unwrap(), "intl");
        assert_eq!(normalize_region("International").unwrap(), "intl");
    }

    #[test]
    fn normalize_region_rejects_unknown() {
        assert!(normalize_region("us").is_err());
        assert!(normalize_region("qoder").is_err());
    }

    #[test]
    fn region_of_domain_matches_go_side() {
        // 与 Go 侧 RegionFromDomain 必须一致（两处不同步会导致导入后区域标错）
        assert_eq!(region_of_domain("qoder.com.cn"), "cn");
        assert_eq!(region_of_domain("openapi.qoder.com.cn"), "cn");
        assert_eq!(region_of_domain("qoder.sh"), "intl");
        assert_eq!(region_of_domain("api3.qoder.sh"), "intl");
        assert_eq!(region_of_domain("qoder.com"), "intl");
        // 未知不猜
        assert_eq!(region_of_domain(""), "");
        assert_eq!(region_of_domain("example.com"), "");
    }

    #[test]
    fn truncate_is_char_safe() {
        // 中文按字符截断，不能按字节（按字节会把一个汉字劈成两半）
        let s = "中文字符串测试";
        let t = truncate(s, 3);
        assert!(t.starts_with("中文字"));
        assert!(t.ends_with('…'));
        // 不超长时原样返回
        assert_eq!(truncate("abc", 10), "abc");
    }
}
