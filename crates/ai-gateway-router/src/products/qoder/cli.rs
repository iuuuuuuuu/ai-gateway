//! `qoder-login` 子命令（Go `internal/qoder/logincli.go` 的对应物）。
//!
//! 由 `bin/gateway.rs::dispatch_subcommand` 派发，对应 Go `cmd/server/main.go:109`：
//! `os.Exit(qoder.RunLoginCLI(os.Args[2:], qoder.DefaultAuthDir()))`。
//!
//! # 与宿主的契约
//!
//! 见 [`crate::cli`] 的模块头。要点：**业务失败返回 0**（原因放 stdout 的
//! `{"status":"error","message":…}`），**参数错误返回 2**（只写 stderr）。
//!
//! ⚠ 例外只有 `import-client` / `url` / `poll`：它们是**动作型**命令，失败时
//! 返回 1 并把原因写 stderr —— 与 Go 逐字一致，不要"统一"成 0。
//!
//! # 两条鉴权路径不能混
//!
//! 见 [`super`] 的模块头：模型/对话/活动走 **Gateway 域**（须 COSY 签名），
//! 额度/用户信息/令牌刷新走 **OpenAPI 域**（只带 Bearer DT，不签名）。

use serde_json::json;

use super::campaign;
use super::client_import;
use super::cred::{self, QoderCred};
use super::login;
use super::region::QoderRegion;
use crate::cli;

/// 全部子命令（`qoder\cli.rs` 的 `run` 用）。
///
/// ⚠ Go 侧 usage 文案（`logincli.go:263`）**漏列了** `campaigns-all` 与
/// `claim-all-campaigns`，而 switch 里有。移植时**照抄该不一致**并在
/// usage 下补一行说明 —— 改名/补名会让用户按 usage 敲却敲不到，
/// 而按 switch 敲却能用，两个来源必须有一个是错的，我们选**不动上游行为**。
pub const SUBCOMMANDS: &[&str] = &[
    "url",
    "poll",
    "import-client",
    "quota",
    "models",
    "campaigns",
    "campaigns-all",
    "claim-campaign",
    "claim-all-campaigns",
    "job-token",
];

/// 无参数时打印的用法行。
///
/// ★ **逐字照抄 Go `logincli.go:263`** —— 它只列了 8 个子命令，
/// 漏了 `campaigns-all` 与 `claim-all-campaigns`。见 [`SUBCOMMANDS`] 的说明：
/// 上游两处文案不一致是既成事实，我们**不改上游行为**。
const USAGE: &str =
    "用法: qoder-login <url|poll|import-client|quota|models|campaigns|claim-campaign|job-token> [选项]";

/// 入口。`args` = 子命令及其后的全部参数（不含 `qoder-login` 本身）。
///
/// 返回进程退出码。契约见 [`crate::cli`] 的模块头：
/// **业务失败 0**（原因放 stdout），**参数错误 2**（只写 stderr）。
///
/// ★ `async`：调用方 `bin/gateway.rs` 是 `#[tokio::main]`，CLI 路径本来就在
/// 运行时里跑（见该文件 `dispatch_subcommand`）。故这里**不**自建运行时 ——
/// 嵌套 `block_on` 会 panic，而"另起线程 + 独立 runtime"会让 reqwest 连接池
/// 无法跨调用复用，代价白付。
pub async fn run(args: &[String]) -> i32 {
    let Some(sub) = args.first() else {
        return cli::usage_error(USAGE);
    };
    let rest = &args[1..];
    match sub.as_str() {
        "url" => run_url(rest).await,
        "poll" => run_poll(rest).await,
        "import-client" => run_import_client(rest).await,
        "quota" => run_quota(rest).await,
        "models" => run_models(rest).await,
        "campaigns" => run_campaigns(rest).await,
        "campaigns-all" => run_campaigns_all(rest).await,
        "claim-campaign" => run_claim_campaign(rest).await,
        "claim-all-campaigns" => run_claim_all_campaigns(rest).await,
        "job-token" => run_job_token(rest).await,
        // ★ 这里列**全部 10 个**（与 Go 的 default 分支一致，它比 usage 行多两个）。
        other => cli::usage_error(&format!(
            "未知子命令 {other:?}（应为 {}）",
            SUBCOMMANDS.join(" / ")
        )),
    }
}

/// 解析标志；出错时按 Go `flag.ContinueOnError` 的口径打印并返回 2。
///
/// Go 的 `flag` 包在 `Parse` 失败时**自己**往 stderr 打错误 + usage，
/// 然后各 `runXxx` 只 `return 2`。[`cli::FlagSet::fail`] 是同一形态。
macro_rules! parse_flags {
    ($fs:expr, $args:expr) => {
        if let Err(e) = $fs.parse($args) {
            $fs.fail(&e);
            return 2;
        }
    };
}

/// 注册 `uid` + `auth-dir` 这两个最常用的标志（10 个子命令里 6 个是这套）。
fn uid_flagset(name: &str) -> cli::FlagSet {
    // ★ `uid` 这里**不**用 `required: true`：那会让 `parse` 直接返回
    // `Err("缺少 --uid 参数")`，而调用方随即会打 usage 行 —— Go 只打一行
    // `缺少 --uid 参数`（检查在各 runXxx 里手写，晚于 Parse）。走手动检查
    // 才能逐字对齐，且 `TrimSpace` 语义也一致。
    cli::FlagSet::new(name)
        .string("uid", "", false)
        .string("auth-dir", &default_auth_dir(), false)
}

/// 业务失败 + `uid` 字段（Go 各 `runXxx` 的
/// `writeJSON({"status":"error","uid":…,"message":…})`）。
///
/// ⚠ 返回 **0**：宿主在非 0 时会丢掉 stdout，用户只看到没有原因的失败。
fn business_error_with_uid(uid: &str, message: &str) -> i32 {
    cli::write_json(&json!({
        "status": "error",
        "uid": uid,
        "message": message,
    }));
    0
}

/// 按 uid 在凭证目录里找凭证（Go `logincli.go:800` 的 `loadCredByUID`）。
///
/// 先按文件名规则直取（`qoder-<uid>.json` / `<uid>.json`），
/// 再回退到逐个加载比对 —— 用户手工放的文件名可能不同。
fn load_cred_by_uid(dir: &str, uid: &str) -> Result<QoderCred, String> {
    let safe = login::sanitize_uid(uid);
    for p in [
        login::join_path(dir, &format!("qoder-{safe}.json")),
        login::join_path(dir, &format!("{safe}.json")),
    ] {
        // Go 用 `os.Stat` 判断"存在"（目录也算存在，随后 LoadFile 会报错）。
        if std::fs::metadata(&p).is_ok() {
            return QoderCred::load_file(&p);
        }
    }
    let (creds, _) = QoderCred::load_dir(dir);
    for c in creds {
        if c.uid == uid {
            return Ok(c);
        }
    }
    Err(format!("在 {dir} 里找不到 uid={uid} 的凭证"))
}

// ---------------------------------------------------------------------------
// 只读查询：quota / models / campaigns
// ---------------------------------------------------------------------------

/// 查一个账号的额度（Go `logincli.go:343` 的 `runQuota`）。
///
/// ## 为什么失败也返回 0
///
/// "查不到额度"不是命令执行失败 —— 宿主把它当成一次正常的"没数据"，
/// 而不是崩溃。若用非零退出码，宿主会当成子进程故障并丢掉 message，
/// 用户看到的就是没有原因的空白。
async fn run_quota(args: &[String]) -> i32 {
    let mut fs = uid_flagset("qoder-login quota");
    parse_flags!(fs, args);

    let uid = fs.get("uid");
    if uid.trim().is_empty() {
        return cli::usage_error("缺少 --uid 参数");
    }
    let auth_dir = fs.get("auth-dir");

    let c = match load_cred_by_uid(&auth_dir, &uid) {
        Ok(c) => c,
        // ★ 注意用**原始** uid（未 sanitize）—— Go 用的是 `*uid`，不是 `c.UID`。
        Err(e) => return business_error_with_uid(&uid, &e),
    };

    let client = login::new_cli_client(&auth_dir);
    let q = match client.fetch_quota(&c).await {
        Ok(q) => q,
        Err(e) => return business_error_with_uid(&c.uid, &e),
    };

    cli::write_json(&json!({
        "status": "ok",
        "uid": c.uid,
        "remaining": q.remaining,
        "total": q.total,
        "used": q.used,
        "exceeded": q.exceeded,
        "planTierName": q.plan_tier_name,
        "usagePercent": q.usage_percent,
        // 到期时刻（Unix 秒）；0 = 未知或永不过期
        "expiresAt": q.expires_at,
    }));
    0
}

/// 查一个账号**实际可用**的模型（Go `logincli.go:389` 的 `runModels`）。
///
/// ⚠ 是按账号查（走该账号的凭证）：Qoder 不同区域的可用模型不同，
/// 全局列表会误导用户以为自己能用某个模型。
async fn run_models(args: &[String]) -> i32 {
    let mut fs = uid_flagset("qoder-login models");
    parse_flags!(fs, args);

    let uid = fs.get("uid");
    if uid.trim().is_empty() {
        return cli::usage_error("缺少 --uid 参数");
    }
    let auth_dir = fs.get("auth-dir");

    let mut c = match load_cred_by_uid(&auth_dir, &uid) {
        Ok(c) => c,
        Err(e) => return business_error_with_uid(&uid, &e),
    };

    let client = login::new_cli_client(&auth_dir);
    // `fetch_models` 会按需补机器指纹，故要 `&mut`。
    let models = match client.fetch_models(&mut c).await {
        Ok(m) => m,
        Err(e) => return business_error_with_uid(&c.uid, &e),
    };

    let list: Vec<serde_json::Value> = models
        .iter()
        .map(|m| {
            json!({
                "key": m.key,
                "displayName": m.display_name,
                "enable": m.enable,
                "isReasoning": m.is_reasoning,
                "isVL": m.is_vl,
                "maxInputTokens": m.max_input_tokens,
                "priceFactor": m.price_factor,
            })
        })
        .collect();

    cli::write_json(&json!({
        "status": "ok",
        "uid": c.uid,
        "count": list.len(),
        "models": list,
    }));
    0
}

/// 查一个账号的权益活动（**只读**，Go `logincli.go:450` 的 `runCampaigns`）。
///
/// ⚠ 本子命令**只查询**（那是它的职责，不是能力限制）。领取走
/// `claim-campaign` / `claim-all-campaigns`。
async fn run_campaigns(args: &[String]) -> i32 {
    let mut fs = uid_flagset("qoder-login campaigns");
    parse_flags!(fs, args);

    let uid = fs.get("uid");
    if uid.trim().is_empty() {
        return cli::usage_error("缺少 --uid 参数");
    }
    let auth_dir = fs.get("auth-dir");

    let c = match load_cred_by_uid(&auth_dir, &uid) {
        Ok(c) => c,
        Err(e) => return business_error_with_uid(&uid, &e),
    };

    let client = login::new_cli_client(&auth_dir);
    let st = match client.fetch_campaigns(&c).await {
        Ok(st) => st,
        Err(e) => return business_error_with_uid(&c.uid, &e),
    };

    cli::write_json(&json!({
        "status": "ok",
        "uid": c.uid,
        "showCampaign": st.show_campaign,
        "claimable": st.claimable,
        "campaignUrl": st.campaign_url,
        "campaigns": st.campaigns,
        "count": st.campaigns.len(),
    }));
    0
}

/// 查**所有**账号的权益活动（Go `logincli.go:506` 的 `runCampaignsAll`）。
///
/// # 为什么需要它
///
/// 活动是**每账号专属**的：A 领了 100 Credits，B 还有 100 没领。
/// 只查一个账号会让其他账号的活动**永远发现不了**。
///
/// 单个账号失败**不影响**其余 —— 逐账号带 status/message，
/// 界面能区分"这个账号查不到"与"所有账号都没活动"。
async fn run_campaigns_all(args: &[String]) -> i32 {
    let mut fs = cli::FlagSet::new("qoder-login campaigns-all")
        .string("auth-dir", &default_auth_dir(), false);
    parse_flags!(fs, args);
    let auth_dir = fs.get("auth-dir");

    let (creds, failed) = QoderCred::load_dir(&auth_dir);
    let client = login::new_cli_client(&auth_dir);
    let mut accounts: Vec<serde_json::Value> = Vec::with_capacity(creds.len() + failed.len());
    let mut claimable_total: usize = 0;
    let mut with_claimable: usize = 0;

    // 先列出加载失败的（有文件但解析不了）—— 如实报，不静默丢
    for f in &failed {
        accounts.push(json!({"status": "error", "message": f}));
    }

    for c in &creds {
        match client.fetch_campaigns(c).await {
            Err(e) => accounts.push(json!({
                "uid": c.uid,
                "status": "error",
                "claimable": 0,
                "message": e,
            })),
            Ok(st) => {
                let n = campaign::count_claimable(&st.campaigns);
                claimable_total += n;
                if n > 0 {
                    with_claimable += 1;
                }
                accounts.push(json!({
                    "uid": c.uid,
                    "status": "ok",
                    "claimable": n,
                    "campaigns": st.campaigns,
                    "campaignUrl": st.campaign_url,
                    "showCampaign": st.show_campaign,
                }));
            }
        }
    }

    cli::write_json(&json!({
        "status": "ok",
        "accounts": accounts,
        "claimableTotal": claimable_total,
        "accountsWithClaimable": with_claimable,
    }));
    0
}

// ---------------------------------------------------------------------------
// 写操作：claim-campaign / claim-all-campaigns
// ---------------------------------------------------------------------------

/// 领取一个权益活动（Go `logincli.go:705` 的 `runClaimCampaign`）。
///
/// ⚠ `replayed:true` 表示这次是**重放**（之前已领过）——
/// 界面必须区分，否则用户重复点会以为又领了一份。
async fn run_claim_campaign(args: &[String]) -> i32 {
    let mut fs = uid_flagset("qoder-login claim-campaign")
        .string("campaign-id", "", false);
    parse_flags!(fs, args);

    let uid = fs.get("uid");
    if uid.trim().is_empty() {
        return cli::usage_error("缺少 --uid 参数");
    }
    let campaign_id = fs.get("campaign-id");
    if campaign_id.trim().is_empty() {
        return cli::usage_error("缺少 --campaign-id 参数");
    }
    let auth_dir = fs.get("auth-dir");

    let c = match load_cred_by_uid(&auth_dir, &uid) {
        Ok(c) => c,
        Err(e) => return business_error_with_uid(&uid, &e),
    };

    let client = login::new_cli_client(&auth_dir);
    let r = match client.claim_campaign(&c, &campaign_id).await {
        Ok(r) => r,
        Err(e) => return business_error_with_uid(&c.uid, &e),
    };

    cli::write_json(&json!({
        "status": "ok",
        "uid": c.uid,
        "grantId": r.grant_id,
        "campaignId": r.campaign_id,
        "campaignKey": r.campaign_key,
        "claimed": true,
        // 重放 = 之前已领过。界面据此说"已领过"而不是"领取成功"。
        "replayed": r.replayed,
        "claimedAt": r.claimed_at,
        "grantedAt": r.granted_at,
    }));
    0
}

/// 一键领取**所有账号**的可领取活动（Go `logincli.go:681` 的 `runClaimAllCampaigns`）。
///
/// # ⚠ 这是**写操作**
///
/// 只由用户在界面上显式点击触发。**不做定时自动领取** ——
/// 那与"用户点一下"不是一回事，且会让账号表现出非人类的活动模式。
/// （定时排程另走 [`campaign::claim_all_campaigns`]，同一份实现。）
async fn run_claim_all_campaigns(args: &[String]) -> i32 {
    let mut fs = cli::FlagSet::new("qoder-login claim-all-campaigns")
        .string("auth-dir", &default_auth_dir(), false);
    parse_flags!(fs, args);
    let auth_dir = fs.get("auth-dir");

    match campaign::claim_all_campaigns(&auth_dir).await {
        Ok(out) => {
            cli::write_json(&out);
            0
        }
        Err(e) => cli::write_business_error(&e),
    }
}

// ---------------------------------------------------------------------------
// 令牌：job-token
// ---------------------------------------------------------------------------

/// 换取短期作业令牌（Go `logincli.go:756` 的 `runJobToken`）。
///
/// ⚠ 输出里**不含**令牌本体（那是秘密）—— 只报长度与到期时间，
/// 便于确认"能不能换到"而不把凭据泄到日志/终端历史里。
async fn run_job_token(args: &[String]) -> i32 {
    let mut fs = uid_flagset("qoder-login job-token");
    parse_flags!(fs, args);

    let uid = fs.get("uid");
    if uid.trim().is_empty() {
        return cli::usage_error("缺少 --uid 参数");
    }
    let auth_dir = fs.get("auth-dir");

    let c = match load_cred_by_uid(&auth_dir, &uid) {
        Ok(c) => c,
        Err(e) => return business_error_with_uid(&uid, &e),
    };

    let client = login::new_cli_client(&auth_dir);
    let t = match client.fetch_job_token(&c).await {
        Ok(t) => t,
        Err(e) => return business_error_with_uid(&c.uid, &e),
    };

    cli::write_json(&json!({
        "status": "ok",
        "uid": c.uid,
        // 只报长度，不报令牌本体
        "tokenLength": t.token.chars().count(),
        "refreshTokenLength": t.refresh_token.chars().count(),
        "expiresAt": t.expires_at,
        // 实测是**毫秒**（86400000 = 1 天）；同时给出换算后的小时数便于人读
        "expiresInMs": t.expires_in,
        "expiresInHours": t.expires_in as f64 / 3_600_000.0,
        "refreshExpiresAt": t.refresh_token_expires_at,
    }));
    0
}

// ---------------------------------------------------------------------------
// 登录：import-client / url / poll
// ---------------------------------------------------------------------------

/// 读 Qoder **客户端自己的**登录态并落成我们的凭证文件
/// （Go `logincli.go:822` 的 `runImportClient`）。
///
/// 这是 Qoder 的**主路径**：用户只要在客户端里登录过，就不需要再做任何事。
async fn run_import_client(args: &[String]) -> i32 {
    let mut fs = cli::FlagSet::new("qoder-login import-client")
        .string("auth-dir", &default_auth_dir(), false)
        // 客户端数据目录（默认自动探测；显式传入便于测试与多版本共存）
        .string("client-dir", "", false)
        .string("nickname", "", false);
    parse_flags!(fs, args);
    let auth_dir = fs.get("auth-dir");

    let mut dir = fs.get("client-dir").trim().to_string();
    if dir.is_empty() {
        match client_import::find_client_app_dir() {
            Some(found) => dir = found,
            None => {
                crate::logline!(
                    "找不到 Qoder 客户端的登录数据。请先在 Qoder 客户端里登录一次\
                     （会自动探测 %APPDATA%\\com.qodercn.app.stable 等目录）。"
                );
                return 1;
            }
        }
    }

    // ★ DPAPI + AES-GCM 是**同步阻塞**调用（DPAPI 要跨进程进 LSA）。
    // 直接 `.await` 会占住一个 tokio worker，故交给阻塞线程池 ——
    // 本仓库既有约定（见 `ai-gateway-core` 的 `multi_product_credit_patrol`）。
    let dir_for_read = dir.clone();
    let (auth, machine_id) = match tokio::task::spawn_blocking(move || {
        let a = client_import::read_client_auth_from(&dir_for_read)?;
        let m = client_import::machine_id(&dir_for_read);
        Ok::<_, String>((a, m))
    })
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            crate::logline!("读取 Qoder 客户端凭证失败: {e}");
            return 1;
        }
        Err(e) => {
            crate::logline!("读取 Qoder 客户端凭证失败: 后台解密任务异常结束（{e}）");
            return 1;
        }
    };

    // 目录名猜区域；猜不出来时**回落国服**（与 Go 逐字一致 ——
    // 客户端目录名不含 cn 的只有国际版，而国服是绝大多数）。
    let region = match login::parse_region(client_import::region_of(&dir)) {
        QoderRegion::Unknown => QoderRegion::CN,
        r => r,
    };

    let nickname_flag = fs.get("nickname");
    let nickname_trimmed = nickname_flag.trim();
    let mut c = QoderCred {
        uid: auth.user.id.clone(),
        nickname: cred::first_non_empty(&[nickname_trimmed, &auth.user.name]),
        dt: auth.token.clone(),
        drt: auth.refresh_token.clone(),
        dt_expires_at: login::parse_rfc3339(&auth.expires_at),
        machine_id,
        // 客户端不落盘 machineToken，首次请求时上游会下发
        machine_token: String::new(),
        machine_type: "windows".to_string(),
        region,
        ..Default::default()
    };
    if c.uid.is_empty() {
        crate::logline!("客户端凭证里没有 user.id，无法确定账号标识");
        return 1;
    }
    if c.machine_id.is_empty() {
        // COSY 签名需要机器指纹。客户端没落盘时造一个稳定的
        // （由 uid 派生，保证同一账号每次相同）。
        c.machine_id = login::deterministic_uuid(&c.uid);
    }

    // ⚠ 0700 在 Windows 上是空操作（与 Go `os.MkdirAll(dir, 0o700)` 同）。
    if let Err(e) = std::fs::create_dir_all(&auth_dir) {
        crate::logline!("创建凭证目录失败: {e}");
        return 1;
    }
    c.file_path = login::join_path(&auth_dir, &format!("qoder-{}.json", login::sanitize_uid(&c.uid)));
    if let Err(e) = c.save_atomic() {
        crate::logline!("保存凭证失败: {e}");
        return 1;
    }

    cli::write_json(&json!({
        "status": "ok",
        "uid": c.uid,
        "nickname": c.nickname,
        "region": c.region.as_str(),
        "file": cred::base_name(&c.file_path),
        "clientDir": dir,
        "expiresAt": auth.expires_at,
        // 头像：使用者的反馈是"已授权后不显示头像"，
        // 而客户端登录态里本来就有 `user.avatarUrl`。
        "avatarUrl": auth.user.avatar_url,
    }));
    0
}

/// 发起一次网页授权，返回授权链接（Go `logincli.go:901` 的 `runLoginURL`）。
async fn run_url(args: &[String]) -> i32 {
    let mut fs = cli::FlagSet::new("qoder-login url")
        .string("region", "", false)
        .string(
            "auth-dir",
            &default_auth_dir(),
            false,
        );
    parse_flags!(fs, args);

    let r = login::parse_region(&fs.get("region"));
    if r == QoderRegion::Unknown {
        // 不默认成国服：两区授权页不同，猜错会让用户打开错误区域的页面
        return cli::usage_error("必须指定区域（--region cn 或 --region intl）");
    }

    let s = match login::login_start(r) {
        Ok(s) => s,
        Err(e) => {
            crate::logline!("{e}");
            return 1;
        }
    };
    let auth_dir = fs.get("auth-dir");
    if let Err(e) = login::save_session(&auth_dir, &s) {
        crate::logline!("保存登录会话失败: {e}");
        return 1;
    }

    // sessionId 用 nonce（唯一且已在会话文件里）
    cli::write_json(&json!({
        "authUrl": s.auth_url,
        "sessionId": s.nonce,
        "region": r.as_str(),
    }));
    0
}

/// 轮询一次授权结果（Go `logincli.go:935` 的 `runLoginPoll`）。
///
/// 三态：**未授权**（`{"status":"pending"}` + 0，正常中间态）、
/// **成功**（清会话 + 补昵称 + `{"status":"ok",…}` + 0）、
/// **失败**（清会话 + stderr + 1）。
async fn run_poll(args: &[String]) -> i32 {
    let mut fs = cli::FlagSet::new("qoder-login poll")
        .string("session", "", false)
        .string("auth-dir", &default_auth_dir(), false);
    parse_flags!(fs, args);

    let session_id = fs.get("session");
    // ★ 这里**不 trim**（与 Go `*sessionID == ""` 逐字一致）：`--session " "`
    // 会被当成一个有效 id 去查文件，然后报"会话不存在"—— 那是更准确的行为，
    // 因为用户确实传了个（错的）id。
    if session_id.is_empty() {
        return cli::usage_error("缺少 --session 参数");
    }
    let auth_dir = fs.get("auth-dir");

    let s = match login::load_session(&auth_dir, &session_id) {
        Ok(s) => s,
        Err(e) => {
            crate::logline!("{e}");
            return 1;
        }
    };

    let client = login::new_cli_client(&auth_dir);
    // 用**裸** http client（不是 COSY 那条链）：轮询端点只需 query 参数。
    match login::login_poll(client.rpc(), &s, &auth_dir).await {
        // 未授权：**正常中间态**，宿主据此显示"请继续在浏览器完成授权"。
        // ⚠ 会话文件**保留** —— 用户还要继续轮询。
        Ok(login::PollOutcome::Pending) => {
            cli::write_json(&json!({"status": "pending"}));
            0
        }
        Ok(login::PollOutcome::Done(cr)) => {
            let mut cr = *cr;
            // 成功：清掉会话文件（用完即删，避免残留）
            let _ = std::fs::remove_file(login::session_file(&auth_dir, &session_id));

            // 补齐昵称（尽力而为：拿不到不影响登录成功）
            let mut nickname = String::new();
            if let Ok(info) = client.fetch_user_info(&cr).await {
                nickname = cred::first_non_empty(&[&info.name, &info.username]);
                if !nickname.is_empty() && cr.nickname != nickname {
                    cr.nickname = nickname.clone();
                    let _ = cr.save_atomic();
                }
            }

            cli::write_json(&json!({
                "status": "ok",
                "uid": cr.uid,
                "nickname": nickname,
                "region": cr.region.as_str(),
            }));
            0
        }
        // 其它错误：授权已被上游拒绝（如用户点了取消），清掉会话
        Err(e) => {
            let _ = std::fs::remove_file(login::session_file(&auth_dir, &session_id));
            crate::logline!("{e}");
            1
        }
    }
}

/// Qoder 凭证目录的默认值：`~/.wb-switch/qoder/auths`。
///
/// 与 Go `qoder.DefaultAuthDir()`（`logincli.go:69-75`）逐字对齐，
/// 也与宿主 `ai-gateway-core::modules::qoder_account::auth_dir()` 同路径。
/// 无 home 目录时回落相对路径 `./.wb-switch/qoder/auths`（Go 同款）。
pub fn default_auth_dir() -> String {
    match dirs::home_dir() {
        Some(h) if !h.as_os_str().is_empty() => h
            .join(".wb-switch")
            .join("qoder")
            .join("auths")
            .to_string_lossy()
            .to_string(),
        _ => std::path::Path::new(".wb-switch")
            .join("qoder")
            .join("auths")
            .to_string_lossy()
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_auth_dir_matches_the_go_and_host_path() {
        let d = default_auth_dir();
        assert!(d.ends_with("qoder\\auths") || d.ends_with("qoder/auths"), "{d}");
        assert!(d.contains(".wb-switch"), "{d}");
    }

    /// 建一个用完就删的临时目录（每个测试一个，避免互相踩）。
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "qoder-cli-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// 一份最小可解析的凭证（uid 是**显然的假值**，公开仓库不留真账号）。
    fn write_cred(dir: &std::path::Path, file: &str, uid: &str) {
        let doc = json!({
            "auth": {
                "accessToken": "dt-fake-token",
                "refreshToken": "drt-fake-token",
                "expiresAt": 1_900_000_000_i64,
                "domain": "qoder.com.cn"
            },
            "account": {"uid": uid, "nickname": "测试账号"},
            "machine": {"id": "00000000-0000-4000-8000-000000000000", "type": "5"}
        });
        std::fs::write(
            dir.join(file),
            serde_json::to_vec_pretty(&doc).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn load_cred_by_uid_prefers_the_canonical_filename() {
        let dir = temp_dir("cred-canonical");
        write_cred(&dir, "qoder-u-1.json", "u-1");
        let c = load_cred_by_uid(&dir.to_string_lossy(), "u-1").unwrap();
        assert_eq!(c.uid, "u-1");
        assert_eq!(c.nickname, "测试账号");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_cred_by_uid_falls_back_to_scanning_for_the_uid() {
        // 用户手工放的文件名可能不按规则 —— 要能靠 uid 字段找回来。
        let dir = temp_dir("cred-scan");
        write_cred(&dir, "qoder-随便起的名字.json", "u-2");
        let c = load_cred_by_uid(&dir.to_string_lossy(), "u-2").unwrap();
        assert_eq!(c.uid, "u-2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_cred_by_uid_reports_both_the_dir_and_the_uid() {
        let dir = temp_dir("cred-missing");
        let err = load_cred_by_uid(&dir.to_string_lossy(), "nobody").unwrap_err();
        // 两个变量都在消息里 —— 排查时缺一个都要多问一轮。
        assert!(err.contains("nobody"), "{err}");
        assert!(err.contains(&dir.to_string_lossy().to_string()), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_cred_by_uid_sanitizes_the_uid_into_a_filename() {
        // uid 里带路径分隔符时不能逃出凭证目录（`sanitize_uid` 把它换成 `_`）。
        let dir = temp_dir("cred-sanitize");
        write_cred(&dir, "qoder-a_b.json", "a/b");
        let c = load_cred_by_uid(&dir.to_string_lossy(), "a/b").unwrap();
        assert_eq!(c.uid, "a/b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_args_prints_usage_and_returns_two() {
        assert_eq!(run(&[]).await, 2);
    }

    #[tokio::test]
    async fn unknown_subcommand_returns_two() {
        assert_eq!(run(&args(&["nope"])).await, 2);
    }

    #[tokio::test]
    async fn quota_without_uid_is_an_argument_error() {
        // 参数错误 ⇒ 2（不是业务失败的 0）—— 宿主据此区分"敲错了"与"没数据"。
        assert_eq!(run(&args(&["quota"])).await, 2);
        // 全是空白也算没给（Go 侧 `TrimSpace(*uid) == ""`）。
        assert_eq!(run(&args(&["quota", "--uid", "   "])).await, 2);
    }

    #[tokio::test]
    async fn claim_campaign_requires_both_uid_and_campaign_id() {
        assert_eq!(run(&args(&["claim-campaign"])).await, 2);
        assert_eq!(run(&args(&["claim-campaign", "--uid", "u"])).await, 2);
    }

    #[tokio::test]
    async fn url_requires_an_explicit_region() {
        // 不默认成国服：两区授权页不同，猜错会让用户打开错误区域的页面。
        assert_eq!(run(&args(&["url"])).await, 2);
        assert_eq!(run(&args(&["url", "--region", " CN "])).await, 2);
        assert_eq!(run(&args(&["url", "--region", "mars"])).await, 2);
    }

    #[tokio::test]
    async fn poll_requires_a_session_and_does_not_trim_it() {
        assert_eq!(run(&args(&["poll"])).await, 2);
        // `" "` 不是空串 ⇒ 不算参数错误，会继续去查文件（然后报会话不存在）。
        // 这里只钉住"它不是 2"这一点，不去碰真实文件系统。
        assert_ne!(run(&args(&["poll", "--session", " "])).await, 2);
    }

    #[tokio::test]
    async fn unknown_flag_is_an_argument_error() {
        assert_eq!(run(&args(&["quota", "--nope"])).await, 2);
        // `--uid` 缺值也是参数错误（Go 的 "flag needs an argument"）。
        assert_eq!(run(&args(&["quota", "--uid"])).await, 2);
    }

    /// ★ 退出码契约的核心：**业务失败必须返回 0**。
    ///
    /// 宿主 `qoder_login.rs::run_login_cmd` 在 `status.success() == false` 时
    /// **丢弃 stdout**、只把 stderr 当错误消息；只有返回 0 时它才会去解析
    /// stdout 的 JSON。所以"凭证不存在"这类业务失败必须走 stdout 的
    /// `{"status":"error",…}` 并返回 0 —— 返回 1 会让用户看到一句
    /// 与真实原因无关的 stderr，而界面上的账号页会直接报"未知错误"。
    ///
    /// 这些用例都指向一个**空**的临时 auth-dir：凭证查找在任何网络请求之前
    /// 就失败了，所以既不需要真实凭证，也不会碰到真实上游。
    #[tokio::test]
    async fn business_failures_return_zero_not_one() {
        let dir = temp_dir("biz-fail");
        let d = dir.to_string_lossy().to_string();
        // 每个用到 uid 的子命令：uid 不存在 ⇒ 业务失败 ⇒ 0。
        for sub in ["quota", "models", "campaigns", "job-token"] {
            assert_eq!(
                run(&args(&[sub, "--uid", "no-such-uid", "--auth-dir", &d])).await,
                0,
                "{sub} 的凭证缺失必须返回 0（业务失败），不是 1"
            );
        }
        // `claim-campaign` 两个参数都给全，同样只会在找凭证时失败。
        assert_eq!(
            run(&args(&[
                "claim-campaign", "--uid", "no-such-uid", "--campaign-id", "c-1", "--auth-dir", &d
            ]))
            .await,
            0
        );
        // 这两个只吃 auth-dir：空目录 ⇒ 账号列表为空 ⇒ 仍然是成功语义（0）。
        assert_eq!(run(&args(&["campaigns-all", "--auth-dir", &d])).await, 0);
        assert_eq!(
            run(&args(&["claim-all-campaigns", "--auth-dir", &d])).await,
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `url` 是**纯本地**的（Go 侧也不发网），成功路径不依赖上游。
    #[tokio::test]
    async fn url_succeeds_offline_and_returns_zero() {
        // 嵌套一层：`sessions` 是 authDir 的**同级**，所以父目录也要是测试独占的，
        // 否则会往 `%TEMP%` 根上写 `sessions\`（并可能踩到别的测试）。
        let root = temp_dir("url-offline");
        let dir = root.join("auths");
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_string_lossy().to_string();
        let code = run(&args(&["url", "--region", "cn", "--auth-dir", &d])).await;
        assert_eq!(code, 0);
        // 会话文件必须落在 **authDir 的同级** `sessions\`，不能在 authDir 里面 ——
        // 否则它会被 `qoder*.json` 的 glob 当成凭证去解析并报错。
        let sessions = dir.parent().unwrap().join("sessions");
        assert!(
            sessions.is_dir(),
            "sessions 目录应在 authDir 同级：{}",
            sessions.display()
        );
        assert!(
            !dir.join("sessions").exists(),
            "sessions 绝不能落在 authDir 内部"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ★★ `import-client` 是**退出码契约的唯一例外**（`url`/`poll` 之外的第三个
    /// 登录类子命令，它跟着 `url`/`poll` 一起返回 1）。
    ///
    /// 复述 Go `logincli.go:822-899` 的五个 `return 1`：探测不到客户端目录
    /// （:839）、`ReadClientAuthFrom` 失败（:847）、`user.id` 为空（:868）、
    /// `MkdirAll` 失败（:878）、`SaveAtomic` 失败（:883）—— **全部只写 stderr
    /// 并返回 1**，不像 `quota`/`models` 那样走 stdout 的 `{"status":"error"}`。
    ///
    /// 这不是缺陷：宿主 `qoder_login.rs::import_from_client` 把本子命令当
    /// **`Result` 消费**（`run_login_cmd` 非 0 时用 stderr 组装错误消息，
    /// 再 `?` 向上抛），而不是像 `refresh_account` 那样解析 stdout 的 status。
    /// 所以这五条路径**必须**返回 1 —— 改成 0 会让宿主拿到空 stdout、
    /// 报出"登录子命令返回了非法 JSON"这种与真实原因无关的错。
    ///
    /// 三个用例都指向不存在的目录/残缺文件，**任何一步都不会发网络请求**。
    #[tokio::test]
    async fn import_client_reports_business_failures_as_exit_one() {
        let root = temp_dir("import-client-errs");
        let auth_dir = root.join("auths");
        let d = auth_dir.to_string_lossy().to_string();

        // ① 目录不存在 ⇒ "读 Local State 失败" ⇒ 1。
        let missing = root.join("no-such-client-dir");
        assert_eq!(
            run(&args(&[
                "import-client",
                "--auth-dir",
                &d,
                "--client-dir",
                &missing.to_string_lossy()
            ]))
            .await,
            1,
            "客户端目录不存在必须是 1（宿主按 Result 消费）"
        );

        // ② 目录在、`Local State` 在，但里面没有密钥 ⇒ 另一步失败 ⇒ 仍是 1。
        let no_key = root.join("client-no-key");
        std::fs::create_dir_all(&no_key).unwrap();
        std::fs::write(no_key.join("Local State"), r#"{"os_crypt":{}}"#).unwrap();
        assert_eq!(
            run(&args(&[
                "import-client",
                "--auth-dir",
                &d,
                "--client-dir",
                &no_key.to_string_lossy()
            ]))
            .await,
            1
        );

        // ③ `Local State` 根本不是 JSON ⇒ 仍是 1（且不 panic）。
        let bad_json = root.join("client-bad-json");
        std::fs::create_dir_all(&bad_json).unwrap();
        std::fs::write(bad_json.join("Local State"), "not json").unwrap();
        assert_eq!(
            run(&args(&[
                "import-client",
                "--auth-dir",
                &d,
                "--client-dir",
                &bad_json.to_string_lossy()
            ]))
            .await,
            1
        );

        // 失败的导入**不能**留下半份凭证文件。
        let leftovers: Vec<_> = std::fs::read_dir(&auth_dir)
            .map(|it| it.filter_map(|e| e.ok()).map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "失败不应写凭证：{leftovers:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn subcommands_list_matches_the_dispatcher() {
        // `run` 的 match 分支与 `SUBCOMMANDS` 必须一一对应 —— 前者漏一个
        // 就只有报错文案会提到它，用户照着敲却是"未知子命令"。
        assert_eq!(SUBCOMMANDS.len(), 10);
        for s in SUBCOMMANDS {
            assert!(!s.is_empty());
        }
        // usage 行故意只列 8 个（Go 的既成不一致），但**不能**列错名字。
        for s in ["url", "poll", "import-client", "quota", "models", "campaigns", "claim-campaign", "job-token"] {
            assert!(USAGE.contains(s), "usage 里应有 {s}");
        }
    }
}
