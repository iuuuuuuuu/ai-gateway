//! zcode_login.rs 通过内嵌网关二进制完成 ZCode 的登录与凭证导入。
//!
//! ## 与 Qoder 的差异（决定了这里的接口形状）
//!
//! Qoder 的凭证只能靠 OAuth 拿到（用户无法手抄），故那边只有
//! "发起 / 轮询" 两个入口。ZCode 的凭证是**用户可复制的字符串**
//!（`{apiKey}.{secret}`），所以**导入是主路径**，OAuth 是备选。
//!
//! 故本模块多一个 `import_credential`。
//!
//! ## 为什么走子进程而不是在 Rust 里重写
//!
//! 与 Qoder 同样的理由：协议实现（OAuth 流程、凭证兑换的四步链）
//! 已在 Go 侧 `internal/zcode` 实现并**经真实上游验证**。
//! 在 Rust 重写意味着两套实现要各自跟进上游改动。
//!
//! 宿主这一层只做**编排**：调 `gateway.exe zcode-login <子命令>`，
//! 把结果转成前端可用的 JSON。

use std::path::PathBuf;
use std::process::Command;

use serde_json::{json, Value};

use crate::modules::{gateway, zcode_account};

/// 调 Go 侧登录子命令的通用执行器。
fn run_login_cmd(args: &[&str]) -> Result<Value, String> {
    let exe = gateway::resolve_gateway_exe()
        .ok_or_else(|| "找不到网关可执行文件，无法登录 ZCode（请先安装或配置网关）".to_string())?;

    let out = Command::new(&exe)
        .arg("zcode-login")
        .args(args)
        .output()
        .map_err(|e| format!("启动登录子命令失败: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    if !out.status.success() {
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            format!("退出码 {:?}", out.status.code())
        };
        return Err(detail);
    }

    serde_json::from_str(stdout.trim())
        .map_err(|e| format!("登录子命令返回了非法 JSON: {e}（原始输出：{}）", truncate(stdout.trim(), 200)))
}

/// 规范化服务商参数。
///
/// 只接受 zai / bigmodel：两者的授权页与端点都不同，猜错会让用户
/// 打开错误服务商的页面。
pub fn normalize_provider(provider: &str) -> Result<&'static str, String> {
    match provider.trim().to_lowercase().as_str() {
        "zai" | "z.ai" | "z-ai" => Ok("zai"),
        "bigmodel" | "zhipu" | "智谱" | "glm" => Ok("bigmodel"),
        "" => Err("请选择服务商（Z.AI 或智谱）：两者的授权页不同，不能默认".into()),
        other => Err(format!("未知服务商 {other:?}（应为 zai 或 bigmodel）")),
    }
}

/// 发起 ZCode 登录，返回授权链接。
pub fn login_start(provider: &str) -> Result<Value, String> {
    let p = normalize_provider(provider)?;
    let auth_dir = zcode_account::auth_dir();
    zcode_account::ensure_dirs()?;
    let auth_dir_s = auth_dir.to_string_lossy().to_string();

    let r = run_login_cmd(&["start", "--provider", p, "--auth-dir", &auth_dir_s])?;

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
/// 三种返回：
///
/// ```text
/// 待授权 → {"status":"pending"}
/// 成功   → {"status":"ok","uid":...,"account":{...}}  （并已登记进账号库）
/// 失败   → Err(...)
/// ```
pub fn login_poll(session_id: &str) -> Result<Value, String> {
    if session_id.trim().is_empty() {
        return Err("会话标识为空，无法轮询（请先发起登录）".into());
    }
    let auth_dir = zcode_account::auth_dir();
    zcode_account::ensure_dirs()?;
    let auth_dir_s = auth_dir.to_string_lossy().to_string();

    let r = run_login_cmd(&["poll", "--session", session_id, "--auth-dir", &auth_dir_s])?;

    match r.get("status").and_then(Value::as_str).unwrap_or("") {
        "pending" => Ok(r),
        "ok" => {
            let uid = r.get("uid").and_then(Value::as_str).unwrap_or("").to_string();
            if uid.is_empty() {
                return Err(format!("登录成功但没返回 uid（原始：{r}）"));
            }
            let provider = r.get("provider").and_then(Value::as_str).unwrap_or("");
            let acc = zcode_account::upsert_account(&uid, &json!({ "provider": provider }))?;
            // ⚠ 登录成功后**立即**补全额度/套餐/模型。
            //
            // 所有者的原话：「智谱登录后显示未知,点击刷新后获取到名字,
            // 但是模型 token 等信息都没刷新」—— "显示未知"就是这个缺口：
            // 登录只落盘 + 登记账号库，不查上游；而界面读的是本地库。
            //
            // 失败不阻断（凭证已可用），只把原因透出去。
            let enrich = enrich_account_after_import(&uid, &auth_dir_s);
            Ok(json!({
                "status": "ok", "uid": uid, "account": acc.to_view(),
                "enriched": enrich.is_ok(), "enrichError": enrich.err(),
            }))
        }
        other => Err(format!("登录子命令返回了未知状态 {other:?}（原始：{r}）")),
    }
}

/// 导入用户粘贴的凭证（**ZCode 的主路径**）。
///
/// 参数：
///   - credential：形如 `{apiKey}.{secret}` 或 `{apiKey}`
///   - provider：zai / bigmodel（可选，空则回退 Z.AI）
///   - nickname：昵称（可选）
pub fn import_credential(credential: &str, provider: &str, nickname: &str) -> Result<Value, String> {
    let cred = credential.trim();
    if cred.is_empty() {
        return Err("请填写凭证（形如 apiKey.secret）".into());
    }

    let auth_dir = zcode_account::auth_dir();
    zcode_account::ensure_dirs()?;
    let auth_dir_s = auth_dir.to_string_lossy().to_string();

    // 服务商为空时**不报错**（导入场景下用户往往不填），
    // 由 Go 侧回退到 Z.AI 并如实返回 —— 界面据此提示用户可以改。
    let provider_arg = match normalize_provider(provider) {
        Ok(p) => p.to_string(),
        Err(_) if provider.trim().is_empty() => String::new(),
        Err(e) => return Err(e),
    };

    let mut args = vec!["import", "--credential", cred, "--auth-dir", auth_dir_s.as_str()];
    if !provider_arg.is_empty() {
        args.push("--provider");
        args.push(provider_arg.as_str());
    }
    if !nickname.trim().is_empty() {
        args.push("--nickname");
        args.push(nickname.trim());
    }

    let r = run_login_cmd(&args)?;

    let uid = r.get("uid").and_then(Value::as_str).unwrap_or("").to_string();
    if uid.is_empty() {
        return Err(format!("导入成功但没返回 uid（原始：{r}）"));
    }
    let prov = r.get("provider").and_then(Value::as_str).unwrap_or("");
    let acc = zcode_account::upsert_account(
        &uid,
        &json!({ "provider": prov, "nickname": nickname.trim() }),
    )?;

    // 导入后立即补全额度/套餐/模型 —— 否则界面显示「未知」，
    // 用户以为导入失败。理由详见 `enrich_account_after_import`。
    let enrich = enrich_account_after_import(&uid, &auth_dir_s);

    Ok(json!({
        "status": "ok",
        "uid": uid,
        "provider": prov,
        "account": acc.to_view(),
        "enriched": enrich.is_ok(),
        "enrichError": enrich.err(),
    }))
}

/// 从目录批量导入凭证文件（`zcode*.json`）。
///
/// 返回每个文件的导入结果（成功/失败原因），而不是遇到第一个失败就中断 ——
/// 用户批量导入时最想知道的是"哪些成了、哪些没成"。
pub fn import_from_dir(dir: &str) -> Result<Value, String> {
    let src = PathBuf::from(dir.trim());
    if !src.is_dir() {
        return Err(format!("不是目录: {}", src.display()));
    }
    zcode_account::ensure_dirs()?;
    let dst_dir = zcode_account::auth_dir();

    let mut imported = Vec::new();
    let mut failed = Vec::new();

    let mut files: Vec<PathBuf> = std::fs::read_dir(&src)
        .map_err(|e| format!("读取目录失败: {e}"))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().map(|x| x == "json").unwrap_or(false)
                && p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("zcode"))
                    .unwrap_or(false)
        })
        .collect();
    files.sort();

    if files.is_empty() {
        return Err(format!("目录里没有找到 zcode*.json 凭证文件: {}", src.display()));
    }

    for f in &files {
        match import_one(f, &dst_dir) {
            Ok((uid, provider)) => {
                let _ = zcode_account::upsert_account(&uid, &json!({ "provider": provider }));
                // 导入后立即补全额度/套餐/模型（理由见 enrich_account_after_import）
                let enrich = enrich_account_after_import(&uid, &dst_dir.to_string_lossy());
                imported.push(json!({
                    "file": f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    "uid": uid,
                    "provider": provider,
                    "enriched": enrich.is_ok(),
                    "enrichError": enrich.err(),
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

/// 导入单个凭证文件。
fn import_one(src: &PathBuf, dst_dir: &PathBuf) -> Result<(String, String), String> {
    let raw = std::fs::read_to_string(src).map_err(|e| format!("读取失败: {e}"))?;
    let doc: Value = serde_json::from_str(&raw).map_err(|e| format!("不是合法 JSON: {e}"))?;

    let uid = doc.get("uid").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if uid.is_empty() {
        return Err("文件里没有 uid 字段".into());
    }
    let provider = doc.get("provider").and_then(Value::as_str).unwrap_or("").to_string();

    // 复制到 auths/（文件名与 Go 侧口径一致）
    let dst = dst_dir.join(zcode_account::cred_file_name(&uid));
    std::fs::copy(src, &dst).map_err(|e| format!("复制到 {} 失败: {e}", dst.display()))?;

    Ok((uid, provider))
}

/// 扫描本机 ZCode 客户端的凭证，返回**可导入项**（不含凭证本体）。
///
/// 见 `zcode_scan` 的模块注释：读的是官方客户端明文落的
/// `~/.zcode/v2/config.json`，**只读**，不改动它。
pub fn scan_local() -> Result<Value, String> {
    let found = crate::modules::zcode_scan::scan();
    Ok(crate::modules::zcode_scan::scan_result_view(&found))
}

/// 导入扫描结果里**选中的**那几条（按 index）。
///
/// ## 为什么按 index 而不是把凭证回传给前端再传回来
///
/// 那样凭证要在浏览器里往返一趟：会进 DOM、可能被截图、被 devtools 复制。
/// 索引方案下**凭证从不离开进程** —— 前端只看到掩码。
///
/// ## 为什么重新扫一遍而不是缓存
///
/// 缓存要与"用户在客户端里换了账号"保持一致，多一份状态就多一处不一致。
/// 重扫的成本是读一个几 KB 的 JSON 文件，可以忽略。
pub fn import_scanned(indices: &[usize]) -> Result<Value, String> {
    if indices.is_empty() {
        return Err("没有选择要导入的凭证".into());
    }
    let found = crate::modules::zcode_scan::scan();
    if found.is_empty() {
        return Err("本机没有扫描到可导入的 ZCode 凭证".into());
    }

    zcode_account::ensure_dirs()?;
    let auth_dir = zcode_account::auth_dir();
    let auth_dir_s = auth_dir.to_string_lossy().to_string();

    let mut imported = Vec::new();
    let mut failed = Vec::new();

    for &i in indices {
        let Some((c, _)) = found.get(i) else {
            failed.push(json!({ "index": i, "error": "扫描结果里没有这一项（本机配置可能已变化）" }));
            continue;
        };

        let mut args = vec![
            "import",
            "--credential",
            c.credential.as_str(),
            "--provider",
            c.provider.as_str(),
            "--nickname",
            c.suggested_nickname.as_str(),
            "--auth-dir",
            auth_dir_s.as_str(),
        ];
        // 带上配对到的额度令牌 —— 否则导入后界面显示「额度未知」，
        // 而用户明明有额度（额度查询只认 JWT，不认对话凭证）。
        if let Some(j) = c.jwt.as_deref() {
            if !j.trim().is_empty() {
                args.push("--jwt");
                args.push(j);
            }
        }

        match run_login_cmd(&args) {
            Ok(r) => {
                let uid = r.get("uid").and_then(Value::as_str).unwrap_or("").to_string();
                if uid.is_empty() {
                    failed.push(json!({ "index": i, "origin": c.origin_name, "error": "导入没返回 uid" }));
                    continue;
                }
                let prov = r.get("provider").and_then(Value::as_str).unwrap_or("");
                // 登记到账号库；失败不阻断（凭证已在盘上，网关能用，
                // 只是界面上少一条记录 —— 如实报出来而不是静默吞掉）
                let acc = zcode_account::upsert_account(
                    &uid,
                    &json!({ "provider": prov, "nickname": c.suggested_nickname }),
                );
                match acc {
                    Ok(a) => {
                        // ⚠ 导入后**立即**把额度/套餐/模型拉回来并落库。
                        //
                        // # 为什么必须在这里做（所有者的反馈）
                        //
                        //	「智谱登录后显示未知,点击刷新后获取到名字,
                        //	  但是模型 token 等信息都没刷新」
                        //	「导入后也不自动更新状态,也不自动更新这些信息」
                        //
                        // 根因：导入只把凭证落盘 + 登记账号库，**不查上游**。
                        // 而 `zcodeListAccounts`（界面刷新读的那个）只读**本地库** ——
                        // 于是额度/套餐/模型全是空的，界面显示「未知」，
                        // 必须用户**逐个账号手点刷新**才有数据。
                        //
                        // 用户的心智模型是"导入 = 账号可用了"，看到「未知」
                        // 会以为导入失败或没额度。故导入成功后就该把它查全。
                        //
                        // ⚠ 失败**不能**让导入失败：凭证已经在盘上、网关已经能用，
                        // 只是界面暂时少几个字段。故这里只记录原因，不中断。
                        let enrich = enrich_account_after_import(&uid, &auth_dir_s);
                        imported.push(json!({
                            "index": i, "uid": uid, "provider": prov,
                            "origin": c.origin_name, "account": a.to_view(),
                            "enriched": enrich.is_ok(),
                            "enrichError": enrich.err(),
                        }));
                    }
                    Err(e) => failed.push(json!({
                        "index": i, "origin": c.origin_name,
                        "error": format!("凭证已落盘，但账号库登记失败：{e}")
                    })),
                }
            }
            Err(e) => failed.push(json!({ "index": i, "origin": c.origin_name, "error": e })),
        }
    }

    Ok(json!({
        "imported": imported,
        "failed": failed,
        "importedCount": imported.len(),
        "failedCount": failed.len(),
    }))
}

/// 导入成功后**立即**补全该账号的额度 / 套餐 / 模型。
///
/// # 它解决什么（所有者的反馈）
///
///	「智谱登录后显示未知,点击刷新后获取到名字,但是模型 token 等信息都没刷新」
///	「导入后也不自动更新状态,也不自动更新这些信息」
///
/// 现象：导入后界面上额度、套餐、模型都是「未知」，必须**逐个账号手点刷新**。
///
/// 根因：导入流程只做了"凭证落盘 + 账号库登记"两件事，**不查上游**。
/// 而界面刷新读的 `zcodeListAccounts` 只读**本地库** —— 库里没这些字段，
/// 界面自然显示「未知」。
///
/// 用户的心智模型是"导入成功 = 账号可用了"，看到「未知」会以为导入失败
/// 或以为账号没额度。故导入成功后就该顺手查全。
///
/// # 为什么复用 `refresh_account` 而不是另写一份
///
/// 额度的解析规则（每日周期 vs 套餐整体到期、套餐分类、
/// `cred_account_id` 身份判定）全都集中在 `refresh_account` 里，
/// 另写一份必然产生漂移 —— 那时同一个账号会有两套说法。
///
/// # 失败语义
///
/// 返回 `Err(原因)`，但**调用方不得因此判定导入失败**：凭证已落盘、
/// 网关已能用，只是界面暂时少几个字段。把原因透出去让用户知道
/// "导入成功但额度没查到，原因是 X" 才是如实的。
fn enrich_account_after_import(uid: &str, _auth_dir: &str) -> Result<Value, String> {
    refresh_account(uid)
}

/// 刷新一个账号的**额度、到期时间、可用模型**。
///
/// # 为什么需要它（这是我漏掉的一整条链路）
///
/// `Client.FetchQuota` 与 `Client.FetchModels` 早就实现了，但**从来
/// 没有生产者调用它们** —— 实测确认（`grep FetchQuota` 只命中测试与
/// 定义本身）。于是界面上额度恒为 0、到期时间恒为空、看不到支持模型，
/// 使用者以为是"查不到"。
///
/// 本函数补上那个缺口：调 Go 侧的 `zcode-login quota` / `models`，
/// 把结果**写回账号库**，界面下次读列表就能看到。
///
/// # 为什么额度与模型分两次调用
///
/// 它们是不同的上游端点、不同的失败模式（额度要 JWT，模型只要凭证）。
/// 合并成一个调用会让"模型能查到但额度查不到"这种情况无法如实表达 ——
/// 而那正是只导入对话凭证的账号的常态。
///
/// # 失败语义
///
/// 单项失败**不返回 Err**，而是记进返回值的 `quotaError` / `modelsError`：
/// 调用方（界面）要能显示"额度查不到，原因是 X"，而不是一个笼统的失败。
/// 只有账号本身不存在才返回 Err。
pub fn refresh_account(uid: &str) -> Result<Value, String> {
    let uid = uid.trim();
    if uid.is_empty() {
        return Err("账号 uid 不能为空".into());
    }

    let auth_dir = zcode_account::auth_dir();
    let auth_dir_s = auth_dir.to_string_lossy().to_string();

    // ---- 额度（含该凭证自己的上游账号标识）----
    let mut credits: Option<i64> = None;
    let mut credits_total: Option<i64> = None;
    let mut expire_at: Option<i64> = None;
    let mut quota_error: Option<String> = None;
    let mut quota_entries: Vec<Value> = Vec::new();
    // 该凭证**自己的**上游账号标识（Go 侧从 JWT 解出）。
    // 用于判断客户端登录态里的身份是否属于这个账号 —— 见下面的身份段。
    let mut cred_account_id = String::new();
    // 套餐信息（含**套餐整体到期**）。
    //
    // ⚠ 与 `expire_at` 不是一回事：
    //   expire_at          各模型桶的**每日周期**结束（实测当天 23:59:59）
    //   plan_expires_at    套餐**整体**到期（实测 2026-09-23 23:59:59）
    //
    // 只显示前者，用户以为"明天额度就没了"；只显示后者，他以为
    // "今天用不完就浪费了"。两个都要透出给界面。
    let mut plan_expires_at: Option<i64> = None;
    let mut plan_kind = String::new();
    let mut plans: Vec<Value> = Vec::new();

    match run_login_cmd(&["quota", "--uid", uid, "--auth-dir", &auth_dir_s]) {
        Ok(r) => {
            let status = r.get("status").and_then(Value::as_str).unwrap_or("");
            match status {
                "ok" => {
                    credits = r.get("remaining").and_then(Value::as_i64);
                    credits_total = r.get("total").and_then(Value::as_i64);
                    let e = r.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
                    if e > 0 {
                        expire_at = Some(e);
                    }
                    if let Some(arr) = r.get("entries").and_then(Value::as_array) {
                        quota_entries = arr.clone();
                    }
                    // 套餐整体到期（0 = 无套餐/未知，此时**不**覆盖成 0）
                    let pe = r.get("planExpiresAt").and_then(Value::as_i64).unwrap_or(0);
                    if pe > 0 {
                        plan_expires_at = Some(pe);
                    }
                    if let Some(k) = r.get("planKind").and_then(Value::as_str) {
                        plan_kind = k.to_string();
                    }
                    if let Some(arr) = r.get("plans").and_then(Value::as_array) {
                        plans = arr.clone();
                    }
                    // 凭证自己的账号标识（Go 侧权威来源）
                    if let Some(aid) = r.get("accountId").and_then(Value::as_str) {
                        cred_account_id = aid.trim().to_string();
                    }
                }
                "no_jwt" => {
                    // 正常状态：只导入了对话凭证，没有额度查询用的 JWT。
                    // 界面据此显示「额度未知」而不是 0。
                    quota_error = Some(
                        r.get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("该账号没有额度查询用的令牌")
                            .to_string(),
                    );
                }
                _ => {
                    quota_error = Some(
                        r.get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("额度查询失败")
                            .to_string(),
                    );
                }
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

    // ---- 身份（头像 / 显示名 / 上游账号标识）----
    //
    // 使用者的反馈：「已授权后,也不显示头像,也不显示名称」。
    // 头像与显示名来自客户端登录态里的 `oauth:*:user_info`（已解密）。
    //
    // # ⚠ 必须**确认这份身份属于当前账号**才能用
    //
    // 客户端登录态只描述**一个**账号（客户端里当前登录的那个）。
    // 无条件套到每个账号上会出大错 —— 所有者实测报的：
    //
    //	「我点击刷新只有列表中从本地导入的显示正确,其他都是显示错误」
    //
    // 我第一版就是无条件覆盖，于是 3 个**不同**账号全被贴上了同一个
    // 昵称/头像/accountId（实测：三者 accountId 全变成
    // `19331730795565300`，而它们真实身份分别是
    // `7cb298d6-…` / `10d28204-…` / `19331730795565300`）。
    //
    // 判据是**上游账号标识**：凭证自己的 account_id（上面从额度接口拿到，
    // 权威来源）与客户端身份里的 `id` 比对，一致才采用。
    //
    // ⚠ 这段必须放在取额度**之后** —— 它依赖 `cred_account_id`。
    // 我第一版把它放在前面，编译期就报「找不到值」（好在是编译错，
    // 不是运行时静默用错数据）。
    let identity = crate::modules::zcode_credstore::read_identity();
    let mut identity_patch = serde_json::Map::new();
    if let Some(id) = &identity {
        // 凭证侧未知时也允许套用：只导入凭证、没有 JWT 的账号解不出
        // account_id，那时无法比对；用客户端身份总比什么都不填好。
        // 但只要知道凭证属于别的账号，就绝不覆盖。
        let belongs = cred_account_id.is_empty() || cred_account_id == id.id;
        if belongs {
            let name = id.best_name();
            if !name.is_empty() {
                identity_patch.insert("nickname".into(), json!(name));
            }
            if !id.avatar_url.is_empty() {
                identity_patch.insert("avatarUrl".into(), json!(id.avatar_url));
            }
        }
    }
    // 凭证自己的账号标识**总是**写入 —— 那是这个账号的真实身份，
    // 与"客户端此刻登录的是谁"无关。
    if !cred_account_id.is_empty() {
        identity_patch.insert("accountId".into(), json!(cred_account_id));
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
    // 身份字段（昵称 / 头像 / 上游账号标识）—— 让界面能显示头像与名称
    for (k, v) in identity_patch {
        patch.insert(k, v);
    }
    // 模型清单落库：界面显示 + 汇总进网关的 product_models（渠道标签）
    //
    // ⚠ 只在**成功取到**时写。取不到（models_error 有值）时不写 ——
    // 写成空数组会把上一次的好数据抹掉，让界面从"有 11 个模型"
    // 变成"没有模型"，而原因只是一次网络抖动。
    //
    // ⚠ **只存 id**（不是整个模型对象）。账号库的 `models` 是
    // `Vec<String>`，而 Go 侧返回的是对象数组 —— 直接塞对象会被
    // apply_patch 的元素类型检查静默滤空（实测踩到：刷新返回 11 个，
    // 落库后却变成 0 个）。完整信息（上下文窗口等）由界面按需再查。
    if models_error.is_none() {
        let ids: Vec<String> = models
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        patch.insert("models".into(), json!(ids));
    }
    let acc = zcode_account::upsert_account(uid, &Value::Object(patch))?;

    // 重写网关配置 —— 让 `pool.product_models` 带上刚查到的模型。
    //
    // ⚠ 这里**刻意只写文件、不重启网关**：刷新是高频操作，每次重启会
    // 掐断正在进行的对话。而网关是启动时读配置的，故这份更新对**已在运行**
    // 的网关要等下次重启才生效 —— 取舍是"渠道标签晚一点出现"，
    // 而不是"每次刷新都断线"。
    //
    // 不这么做的话（所有者的实测反馈）：
    // 「智能体管理里只看到 WorkBuddy 的模型，没看到 ZCode 和 Qoder 的」
    // —— 因为启动时账号还没刷新，product_models 是空的。
    if let Err(e) = crate::modules::gateway::resync_native_config() {
        // 写配置失败**不该让刷新整体失败**：额度与模型已经落库了，
        // 那才是用户点这个按钮的主要目的。但要留下痕迹便于排查。
        eprintln!("刷新后重写网关配置失败（渠道标签可能不更新）: {e}");
    }

    Ok(json!({
        "status": "ok",
        "account": acc.to_view(),
        "identity": identity.as_ref().map(|i| json!({
            "username": i.username,
            "displayName": i.display_name,
            "id": i.id,
            "avatarUrl": i.avatar_url,
            "activeProvider": i.active_provider,
        })),
        "quota": {
            "remaining": credits,
            "total": credits_total,
            // ⚠ 各模型桶的**每日周期**结束（实测当天 23:59:59）
            "expiresAt": expire_at,
            "entries": quota_entries,
            "error": quota_error,
            // 套餐信息 —— 与上面的周期到期**不同**，见变量声明处的说明
            "plans": plans,
            // 套餐整体到期（实测体验套餐是 2026-09-23 23:59:59）
            "planExpiresAt": plan_expires_at,
            // "trial" / "paid" / "api_key" / "unknown"
            "planKind": plan_kind,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_provider_accepts_known() {
        assert_eq!(normalize_provider("zai").unwrap(), "zai");
        assert_eq!(normalize_provider("Z.AI").unwrap(), "zai");
        assert_eq!(normalize_provider("bigmodel").unwrap(), "bigmodel");
        assert_eq!(normalize_provider("智谱").unwrap(), "bigmodel");
        assert_eq!(normalize_provider("glm").unwrap(), "bigmodel");
    }

    #[test]
    fn normalize_provider_rejects_empty_and_unknown() {
        // 空值不能默认成某个服务商（两者授权页不同）
        assert!(normalize_provider("").is_err());
        assert!(normalize_provider("  ").is_err());
        assert!(normalize_provider("openai").is_err());
    }

    #[test]
    fn truncate_is_char_safe() {
        let s = "中文字符串测试";
        let t = truncate(s, 3);
        assert!(t.starts_with("中文字"));
        assert!(t.ends_with('…'));
        assert_eq!(truncate("abc", 10), "abc");
    }
}
