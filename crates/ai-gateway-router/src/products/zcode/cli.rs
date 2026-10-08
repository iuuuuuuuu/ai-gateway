//! `zcode-login` 子命令（Go `internal/zcode/logincli.go` 的对应物）。
//!
//! 由 `bin/gateway.rs::dispatch_subcommand` 派发，对应 Go `cmd/server/main.go:119`：
//! `os.Exit(zcode.RunLoginCLI(os.Args[2:], zcode.DefaultAuthDir()))`。
//!
//! # 与宿主的契约
//!
//! 见 [`crate::cli`] 的模块头。要点：**业务失败返回 0**（原因放 stdout 的
//! `{"status":"error","message":…}`），**参数错误返回 2**（只写 stderr）。
//!
//! ★ **唯一例外 = `warm`**：连「上游说 `status != ok`」也只写 stdout 并返回 0
//! （Go 侧刻意如此，`logincli.go` 里 `runWarm` 从不返回非 0）；由 Rust 宿主
//! `ai-gateway-core::modules::zcode_login::warm_account` 自己判 `status` 字段。

use crate::cli;
use serde_json::Value;

use super::captcha;
use super::claim::{ClaimResult, PlanOffer};
use super::client::Client;
use super::cred::{self, ZcodeCred};
use super::login;
use super::plan;
use super::provider::{self, Provider};
use super::warm;

/// 全部子命令（`zcode\cli.rs` 的 `run` 用）。
///
/// ⚠ `start`/`poll` 是 OAuth 设备流的**备选**路径；主路径是 `import`
/// （凭证是用户可复制的一串，见 Go `main.go:118` 的注释）。
pub const SUBCOMMANDS: &[&str] = &[
    "start",
    "poll",
    "import",
    "quota",
    "models",
    "preview-plan",
    "claim-plan",
    "warm",
];

/// 入口。`args` = 子命令及其后的全部参数（不含 `zcode-login` 本身）。
///
/// ★ `async`：`bin/gateway.rs` 是 `#[tokio::main] async fn main()`，CLI 路径本来
/// 就在 tokio 运行时里（`dispatch_subcommand(&args).await` 是 `main()` 的第一句）。
/// reqwest 是 async 的，用同步签名只能靠「另起线程 + 另起 runtime」硬凑 ——
/// 白白多一个线程、多一个 runtime，且连接池无法复用。
pub async fn run(args: &[String]) -> i32 {
    let Some(sub) = args.first().map(|s| s.as_str()) else {
        // Go `logincli.go:251-254`：无子命令时打**用法行**（不是「缺少子命令」）。
        return cli::usage_error(
            "用法: zcode-login <start|poll|import|quota|models|preview-plan|claim-plan|warm> [选项]",
        );
    };
    let rest = &args[1..];
    // ★ 顺序与 Go `logincli.go:256-295` 的 case 表一致（见 [`SUBCOMMANDS`]）。
    match sub {
        "start" => run_start(rest).await,
        "poll" => run_poll(rest).await,
        "import" => run_import(rest).await,
        "quota" => run_quota(rest).await,
        "models" => run_models(rest).await,
        "preview-plan" => run_preview_plan(rest).await,
        "claim-plan" => run_claim_plan(rest).await,
        "warm" => run_warm(rest).await,
        other => cli::usage_error(&format!(
            "未知子命令 {other:?}（应为 start / poll / import / quota / models / preview-plan / claim-plan / warm）"
        )),
    }
}

/// `start` —— 发起 OAuth 设备流（Go `runStart`，`logincli.go:307-342`）。
///
/// # 退出码
///
/// ```text
/// 0  成功（stdout 是 {"sessionId","authUrl","provider","expiresAt","pollIntervalS"}）
/// 2  参数错误（flag 解析失败 / --provider 缺失或非法）
/// 1  StartLogin 失败 / 会话落盘失败
/// ```
///
/// # 为什么会话必须落盘
///
/// 宿主是**两次独立调用**：
///
/// ```text
/// gateway.exe zcode-login start --provider zai   （进程 A，退出）
/// gateway.exe zcode-login poll  --session <id>    （进程 B，新进程）
/// ```
///
/// 进程 A 生成的 `pollToken` 是进程 B 轮询时的 Bearer，不落盘就会 401。
/// 会话写在 `authDir` 的**同级** `sessions/`（不是里面）—— `authDir` 会被
/// `load_dir` 用 glob `zcode*.json` 扫描，会话混进去会被当成凭证解析并报错。
async fn run_start(args: &[String]) -> i32 {
    let mut fs = cli::FlagSet::new("zcode-login start")
        .string("provider", "", false)
        .string("auth-dir", &cred::default_auth_dir(), false);
    if let Err(e) = fs.parse(args) {
        fs.fail(&e);
        return 2;
    }
    let auth_dir = fs.get("auth-dir");

    // ★ 与 Go 的 `ParseProvider(*provider) == ProviderUnknown` 判定等价：
    // 空串、空白、未知拼写都落进 `Unknown`，统一报同一条文案。
    let provider = provider::parse_provider(&fs.get("provider"));
    if provider == Provider::Unknown {
        return cli::usage_error("必须指定服务商（--provider zai 或 --provider bigmodel）");
    }

    let mut client = Client::new();
    client.apply_host_proxy();

    let session = match client.start_login(provider).await {
        Ok(s) => s,
        Err(e) => {
            // 业务失败 ⇒ stderr + 1（宿主 `run_login_cmd` 会把 stderr 当错误消息）。
            crate::logline!("{e}");
            return 1;
        }
    };

    if let Err(e) = login::save_session(&auth_dir, &session) {
        crate::logline!("保存登录会话失败: {e}");
        return 1;
    }

    // 宿主 `zcode_login.rs:79-86` 需要**非空** `authUrl` 与 `sessionId`。
    cli::write_json(&serde_json::json!({
        "sessionId": session.flow_id,
        "authUrl": session.authorize_url,
        "provider": session.provider.as_str(),
        "expiresAt": session.expires_at,
        "pollIntervalS": session.poll_interval_sec,
    }));
    0
}

/// `poll` —— 轮询一次授权状态（Go `runPoll`，`logincli.go:344-405`）。
///
/// # 退出码
///
/// ```text
/// 0  授权成功（status=ok）**或**尚未完成（status=pending）
/// 2  参数错误（缺 --session、flag 解析失败）
/// 1  会话读不出来 / 真失败（4xx、code≠0、status=failed）
/// ```
///
/// ★ 「尚未完成」是**退出码 0** 的中间态，不是错误：宿主
/// `zcode_login.rs:109-110` 见到 `status=pending` 就原样透传给前端继续轮询。
/// 把它当错误会让界面在用户还没点完授权时就弹红。
///
/// ★ 真失败时**删掉会话文件**（Go `:378`）：会话已经不可能再推进了，
/// 留着只会让用户反复点到同一条死会话。
async fn run_poll(args: &[String]) -> i32 {
    let mut fs = cli::FlagSet::new("zcode-login poll")
        // ★ `required: false` —— 必填校验内部会 **trim**，而 Go `:351` 是
        // `*sessionID == ""`（不 trim）。注册成 required 会让 `--session "   "`
        // 提前变成参数错误 2，而 Go 会一路走到 `loadSession` 并以 1 收场。
        // 故这里让 `FlagSet` 只管解析，空值判断自己做（见下）。
        .string("session", "", false)
        .string("auth-dir", &cred::default_auth_dir(), false);
    if let Err(e) = fs.parse(args) {
        fs.fail(&e);
        return 2;
    }
    let session_id = fs.get("session");
    let auth_dir = fs.get("auth-dir");
    // ★ **刻意不 trim**：Go `logincli.go:351` 是 `*sessionID == ""`，而同一文件里
    // `quota`/`models`/`preview-plan`/`claim-plan`/`warm` 用的都是
    // `strings.TrimSpace(*uid) == ""`。这处不对称是上游（Go 侧）的真实行为：
    // `--session "   "` 会一路走到 `loadSession`，被 `sanitizeName` 变成 `___`，
    // 然后以「登录会话不存在或已过期」+ 退出码 1 收场 —— 而不是参数错误 2。
    //
    // 为什么不去「统一」它：宿主传的是 `start` 返回的 sessionId，永远不会是
    // 空白；两边的可观测差异只在这条谁也走不到的路径上。为它引入一个与 Go
    // 不同的分支，等于用一个自造的差异换掉一个真实的差异。
    if session_id.is_empty() {
        return cli::usage_error("缺少 --session 参数");
    }

    let session = match login::load_session(&auth_dir, &session_id) {
        Ok(s) => s,
        Err(e) => {
            crate::logline!("{e}");
            return 1;
        }
    };

    let mut client = Client::new();
    client.apply_host_proxy();

    let tokens = match client.poll_once(&session).await {
        Ok(t) => t,
        Err(e) if e.is_pending() => {
            // ★ 中间态 ⇒ `{"status":"pending"}` + **0**（宿主据此继续轮询）。
            cli::write_json(&serde_json::json!({ "status": "pending" }));
            return 0;
        }
        Err(e) => {
            // 真失败：会话已无推进可能，删掉它再报错。
            login::remove_session(&auth_dir, &session_id);
            crate::logline!("{e}");
            return 1;
        }
    };

    let cred = match client.exchange_credential(&tokens).await {
        Ok(c) => c,
        Err(e) => {
            login::remove_session(&auth_dir, &session_id);
            crate::logline!("兑换凭证失败: {e}");
            return 1;
        }
    };

    if let Err(e) = std::fs::create_dir_all(&auth_dir) {
        crate::logline!("创建凭证目录失败: {e}");
        return 1;
    }
    let mut cred = cred;
    cred.file_path = std::path::Path::new(&auth_dir)
        .join(cred.file_base_name())
        .to_string_lossy()
        .to_string();
    if let Err(e) = cred.save_atomic() {
        crate::logline!("保存凭证失败: {e}");
        return 1;
    }

    // 用完删会话（Go 同款）—— 成功的会话不该留在磁盘上。
    login::remove_session(&auth_dir, &session_id);

    // 宿主 `zcode_login.rs:112-117` 需要**非空** `uid`，然后读 `provider`。
    cli::write_json(&serde_json::json!({
        "status": "ok",
        "uid": cred.uid,
        "provider": cred.provider.as_str(),
        "credential": cred.masked_credential(),
        "hasJwt": !cred.jwt.is_empty(),
    }));
    0
}

/// `import` —— 导入用户粘贴的凭证（Go `runImport`，`logincli.go:882-944`）。
/// 实现在文件后半（与 `jwt_issued_at` 放一起，便于对照 Go 的相邻顺序）。

/// `quota` —— 查额度（Go `runQuota`，`logincli.go:636-759`）。
///
/// # 退出码（★ 与 `models` 刻意不一致，不要统一）
///
/// ```text
/// 0  ok / no_jwt / 上游错误（业务失败写 stdout 的 status=error）
/// 2  参数错误（缺 --uid、flag 解析失败）
/// 1  凭证找不到（loadCredByUID 失败）
/// ```
///
/// 宿主 `zcode_login.rs:533` 只读 stdout 的 JSON；`1` 那条路会让宿主把 stderr
/// 当错误消息显示（「在 … 里找不到 uid=… 的凭证」比一句空白有用）。
async fn run_quota(args: &[String]) -> i32 {
    let mut fs = cli::FlagSet::new("zcode-login quota")
        // ★ `required: false`：`FlagSet` 的必填校验走 `fs.fail()`，那会**多打
        // 一行 usage**。Go 侧缺 `--uid` 不是 `flag` 解析错误、而是 `runQuota`
        // 里手写的检查，只打一句 `缺少 --uid 参数`（`:644`）。
        // 兄弟文件 `products/qoder/cli.rs` 也是全用 `required: false`。
        .string("uid", "", false)
        .string("auth-dir", &cred::default_auth_dir(), false);
    if let Err(e) = fs.parse(args) {
        fs.fail(&e);
        return 2;
    }
    let uid = fs.get("uid");
    let auth_dir = fs.get("auth-dir");
    if uid.trim().is_empty() {
        // 与 `FlagSet` 的必填校验文案一致（注册成 required 时它就是这么报的）。
        return cli::usage_error("缺少 --uid 参数");
    }

    let cred = match load_cred_by_uid(&auth_dir, uid.trim()) {
        Ok(c) => c,
        Err(e) => {
            crate::logline!("{e}");
            return 1;
        }
    };

    if cred.jwt.trim().is_empty() {
        // 如实说明原因，而不是报错或返回 0 —— 界面据此显示「额度未知（需要 OAuth 登录）」
        // 而不是 0（0 会被读成「额度耗尽」）。
        cli::write_json(&serde_json::json!({
            "status": "no_jwt",
            "uid": cred.uid,
            "message": "该账号只导入了对话凭证，没有额度查询用的 JWT",
        }));
        return 0;
    }

    let mut client = Client::new();
    client.apply_host_proxy();
    let mut cred = cred;
    let q = match client.fetch_quota(&mut cred).await {
        Ok(q) => q,
        Err(e) => {
            // 把上游错误回传（宿主据此显示具体原因），但**退出码为 0** ——
            // 「查不到额度」不是命令执行失败，不该让调用方当成崩溃。
            cli::write_json(&serde_json::json!({
                "status": "error",
                "uid": cred.uid,
                "message": e,
            }));
            return 0;
        }
    };

    // entries：**11 键**，字段名与 Go 逐字一致（camelCase）。
    let entries: Vec<Value> = q
        .entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "showName": e.show_name,
                "remaining": e.remaining,
                "total": e.total,
                "used": e.used,
                "unitType": e.unit_type,
                // ⚠ 这是**每日周期**的结束（实测当天 23:59:59），不是套餐到期。
                "expiresAt": e.expires_at,
                "periodStart": e.period_start,
                "periodEnd": e.period_end,
                // 该桶来自哪个套餐 —— 界面据此说「这 300 万是体验套餐送的」
                "planId": e.plan_id,
                "entitlementId": e.entitlement_id,
                "grantUnits": e.grant_units,
            })
        })
        .collect();

    // plans：**8 键**。★ 刻意不含 `user_plan_id` —— 那个字段 Rust 结构体里有、
    // Go 的 CLI 输出里没有。直接 `to_value(&Plan)` 会多出这个键。
    let plans: Vec<Value> = q
        .plans
        .iter()
        .map(|p| {
            let ents: Vec<Value> = p
                .entitlements
                .iter()
                .map(|e| {
                    // ★ **6 键**：Rust 的 `PlanEntitlement` 有 8 个字段
                    //（多 `meter`/`capabilities`），Go 的 CLI 只输出 6 个。
                    serde_json::json!({
                        "entitlementId": e.entitlement_id,
                        "showName": e.show_name,
                        "grantUnits": e.grant_units,
                        "period": e.period,
                        "unitType": e.unit_type,
                        "effectiveAt": e.effective_at,
                    })
                })
                .collect();
            serde_json::json!({
                "planId": p.plan_id,
                "name": p.name,
                "description": p.description,
                "status": p.status,
                "priority": p.priority,
                "startsAt": p.starts_at,
                "endsAt": p.ends_at,
                "entitlements": ents,
            })
        })
        .collect();

    // 套餐类型（体验 / 付费 / 按量）—— 由 `plan.rs` 按上游静态配置判定。
    // ★ 取不到配置时返回 unknown（**不猜**），不是报错。
    let plan_kind = match client.fetch_start_plan_preview().await {
        Ok(sp) => plan::classify_plan(Some(&q), Some(&sp)),
        Err(_) => plan::PlanKind::Unknown,
    };

    cli::write_json(&serde_json::json!({
        "status": "ok",
        "uid": cred.uid,
        "remaining": q.remaining,
        "total": q.total,
        "used": q.used,
        // 最早到期时刻 —— 界面用它显示「最快要过期的额度」
        "expiresAt": q.soonest_expiry(),
        "entries": entries,
        // 套餐信息（含**套餐整体到期**）—— 与 entries 的每日周期到期不同
        "plans": plans,
        // 套餐整体到期（Unix 秒；0 = 无套餐/未知）
        "planExpiresAt": q.plan_expiry(),
        "planKind": plan_kind.as_str(),
        // 该凭证**自己的**上游账号标识。
        // 宿主需要它来判断「客户端登录态里的身份是否属于这个账号」；没有它
        // 宿主只能猜 —— 实测踩到过：3 个不同账号的 accountId 全变成同一个。
        "accountId": account_id_of(&cred),
    }));
    0
}

/// 取凭证的上游账号标识。对应 Go `accountIDOf`（`logincli.go:765-776`）。
///
/// 优先用凭证里已存的 `account_id`；没有就从 JWT 里现解一次；都没有则空串。
fn account_id_of(c: &ZcodeCred) -> String {
    if !c.account_id.trim().is_empty() {
        return c.account_id.clone();
    }
    if !c.jwt.trim().is_empty() {
        return cred::account_id_from_jwt(&c.jwt);
    }
    String::new()
}

/// 业务失败的出口，带 `uid` 字段。**用 crate 级 [`cli::write_business_error_uid`]**。
///
/// ★ 曾经这里有一份本地副本，后来 Lead 把同名函数加进了 `src\cli.rs`（那是他的
/// 范围），两边**返回 0** 的契约一致，故删掉本地那份只留这个别名说明。
///
/// ★ `uid` 传什么要逐字照 Go：`models` 里「找不到凭证」那条用的是
/// **用户输入的原值**（此刻没有凭证），其它路径才用 `c.UID`。

/// 按 uid 找凭证。对应 Go `loadCredByUID`（`logincli.go:833-850`）。
///
/// 先试 `{dir}/{uid}.json`（文件名约定），失败再 `load_dir` 逐个比对
/// `c.uid == uid || c.uid == want`。★ 那个 `want`（补了 `zcode-` 前缀的形式）
/// 是必须的：文件名与 `uid` 字段不总是同一个值。
fn load_cred_by_uid(dir: &str, uid: &str) -> Result<ZcodeCred, String> {
    let want = if uid.starts_with("zcode-") {
        uid.to_string()
    } else {
        format!("zcode-{uid}")
    };
    let direct = format!("{}/{}", dir.trim_end_matches(['/', '\\']), want);
    let direct = format!("{direct}.json");
    if std::path::Path::new(&direct).exists() {
        if let Ok(c) = cred::load_file(&direct) {
            return Ok(c);
        }
    }
    let (creds, _errs) = cred::load_dir(dir);
    for c in creds {
        if c.uid == uid || c.uid == want {
            return Ok(c);
        }
    }
    Err(format!("在 {dir} 里找不到 uid={uid} 的凭证"))
}

/// `import` —— 导入已有凭证（Go `runImport`，`logincli.go:882-944`）。
///
/// ★ 这是 ZCode 的**主路径**（不是 `start`/`poll`）：ZCode 的凭证是用户可
/// 复制的字符串，而 Qoder 那种只能靠 OAuth 拿。见 `login.go:5-30`。
///
/// # 退出码
///
/// ```text
/// 0  ok
/// 2  参数错误（缺 --credential、flag 解析失败）
/// 1  凭证解析失败 / 建目录失败 / 保存失败
/// ```
///
/// 与 `quota`/`models` 不同，这里的失败**必须**返回非 0 —— 宿主
/// `zcode_login.rs:159` 靠退出码判断「导入是否成功」，而不是读 stdout 的 status。
async fn run_import(args: &[String]) -> i32 {
    let mut fs = cli::FlagSet::new("zcode-login import")
        // ★ `required: false` —— 必填校验内部会 **trim**，而 Go `:902` 是
        // `*credential == ""`（不 trim）。注册成 required 会让
        // `--credential "   "` 提前变成参数错误 2，而 Go 会一路走到
        // `Parse` 并由它报「凭证为空」+ 退出码 1。
        // 故这里让 `FlagSet` 只管解析，空值判断自己做（见下）。
        .string("credential", "", false)
        .string("provider", "", false)
        .string("nickname", "", false)
        .string("jwt", "", false)
        .string("auth-dir", &cred::default_auth_dir(), false);
    if let Err(e) = fs.parse(args) {
        fs.fail(&e);
        return 2;
    }
    let credential = fs.get("credential");
    if credential.is_empty() {
        // Go 用的是 `*credential == ""`（**不 trim**）—— 传全空格会走到
        // `Parse` 并由它报「凭证为空」。保持逐字一致。
        return cli::usage_error("缺少 --credential 参数");
    }

    let mut cred = match cred::parse_credential(&credential) {
        Ok(c) => c,
        Err(e) => {
            crate::logline!("{e}");
            return 1;
        }
    };

    cred.provider = provider::parse_provider(&fs.get("provider"));
    if cred.provider == Provider::Unknown {
        // ★ 未指定时**回退到 Z.AI 而不是报错**：导入场景用户往往不填。
        // 但不能猜成 bigmodel —— 猜错会让账号打到错误端点、必然失败。
        cred.provider = Provider::Zai;
    }
    cred.nickname = fs.get("nickname");

    let jwt = fs.get("jwt");
    if !jwt.trim().is_empty() {
        // 额度查询**只认 JWT**，不认 `{apiKey}.{secret}`。客户端把它们分成
        // 两个 provider 条目落盘（coding-plan 是凭证、start-plan 是 JWT），
        // 扫描器把同一条配置里的两条配对导入，JWT 就通过这里并进去。
        // 少了这一步，用户会看到「额度未知」—— 而他明明有额度。
        cred.jwt = jwt.trim().to_string();
        cred.jwt_issued_at = jwt_issued_at(&cred.jwt);
        // 上游账号标识 —— 让界面能识别「同一账号的多把 key」。
        cred.account_id = cred::account_id_from_jwt(&cred.jwt);
    }

    let auth_dir = fs.get("auth-dir");
    if let Err(e) = std::fs::create_dir_all(&auth_dir) {
        crate::logline!("创建凭证目录失败: {e}");
        return 1;
    }
    cred.file_path = std::path::Path::new(&auth_dir)
        .join(cred.file_base_name())
        .to_string_lossy()
        .to_string();
    if let Err(e) = cred.save_atomic() {
        crate::logline!("保存凭证失败: {e}");
        return 1;
    }

    cli::write_json(&serde_json::json!({
        "status": "ok",
        "uid": cred.uid,
        "provider": cred.provider.as_str(),
        "file": std::path::Path::new(&cred.file_path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
    }));
    0
}

/// 从 JWT 里取 `iat`（签发时刻）。对应 Go `jwtIssuedAt`（`logincli.go:859-880`）。
///
/// ★ **解不开或没有 `iat` 时返回 0，不报错** —— 它只是个展示字段（排障看
/// token 年龄用），为它中断导入是不值得的。
///
/// ★ 这个 JWT **没有 `exp` 字段**、不因时间过期 —— 故**不能**据此判断
/// 「是否过期」，只有上游回 401/3012 才表示需要重新登录。
fn jwt_issued_at(token: &str) -> i64 {
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 {
        return 0;
    }
    use base64::Engine;
    let raw = match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]) {
        Ok(r) => r,
        // 有些实现带 padding，兼容一下（Go 侧同样先 `RawURLEncoding` 再 `URLEncoding`）。
        Err(_) => match base64::engine::general_purpose::URL_SAFE.decode(parts[1]) {
            Ok(r) => r,
            Err(_) => return 0,
        },
    };
    let v: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    v.get("iat").and_then(|x| x.as_i64()).unwrap_or(0)
}

/// `models` —— 查模型清单（Go `runModels`，`logincli.go:785-831`）。
///
/// # 退出码（★ 与 `quota` 刻意不一致，不要统一）
///
/// ```text
/// 0  ok / **连「找不到凭证」也是 0** / 上游错误
/// 2  参数错误
/// ```
///
/// ★ 这里**永不返回 1** —— 与 `quota`（找不到凭证 ⇒ 1）分叉。Go 侧就是
/// 这么写的：`runModels` 里 `loadCredByUID` 失败时 `writeJSON(status=error)`
/// 然后 `return 0`。别「顺手统一」，那会让宿主的错误处理路径变化。
async fn run_models(args: &[String]) -> i32 {
    let mut fs = cli::FlagSet::new("zcode-login models")
        // ★ `required: false`：同 `quota` —— Go `:792` 是手写检查、只打一句
        // `缺少 --uid 参数`，而 `FlagSet` 的必填校验会多打一行 usage。
        .string("uid", "", false)
        .string("auth-dir", &cred::default_auth_dir(), false);
    if let Err(e) = fs.parse(args) {
        fs.fail(&e);
        return 2;
    }
    let uid = fs.get("uid");
    let auth_dir = fs.get("auth-dir");
    if uid.trim().is_empty() {
        return cli::usage_error("缺少 --uid 参数");
    }

    let cred = match load_cred_by_uid(&auth_dir, uid.trim()) {
        Ok(c) => c,
        Err(e) => {
            // ★ 注意这里是**用户输入的原值**（`uid`），不是 `c.uid` ——
            // 此刻根本没有凭证可读。Go 侧 `logincli.go:799` 就是 `*uid`。
            return cli::write_business_error_uid(&uid, &e);
        }
    };

    let mut client = Client::new();
    client.apply_host_proxy();
    let models = match client.fetch_models(Some(&cred)).await {
        Ok(m) => m,
        Err(e) => {
            // 这里才有凭证，用 `c.uid`（Go `logincli.go:806` 同款）。
            return cli::write_business_error_uid(&cred.uid, &e);
        }
    };

    // ★ **7 键**，且是 `maxOutput` —— Rust 的 `Model` serde 名是
    // `maxOutputTokens`（那是给对话面用的）。直接 `to_value(&Model)` 会让宿主
    // 读不到 `maxOutput`，模型列表里「最大输出」恒为空。
    let list: Vec<Value> = models
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "name": m.name,
                "contextWindow": m.context_window,
                "maxOutput": m.max_output,
                "reasoning": m.reasoning,
                "vision": m.vision,
                // builtin 来源 = 上游拿不到时的离线兜底清单
                "source": m.source,
            })
        })
        .collect();

    cli::write_json(&serde_json::json!({
        "status": "ok",
        "uid": cred.uid,
        "count": list.len(),
        // ★ 空清单必须是 `[]` 而不是 `null`（宿主按数组读）。
        "models": list,
    }));
    0
}

/// 一个可领套餐 → JSON（**4 键**）。
///
/// ★ 抽成函数只为了可测：Go `logincli.go:493-500` 的手写 map 只有这四个键，
/// 而 Rust 的 `PlanOffer` 还有一个 `raw`（上游原始对象）**不该**透出去 ——
/// 它可能含上游内部字段，且会让宿主的 JSON 体积膨胀。
fn plan_offer_json(o: &PlanOffer) -> Value {
    serde_json::json!({
        "planId": o.plan_id,
        "name": o.name,
        "status": o.status,
        "endsAt": o.ends_at,
    })
}

/// 领取结果 → JSON。抽出来只为了能单独测「已领过」那条文案分支。
fn claim_plan_json(uid: &str, plan_id: &str, res: &ClaimResult) -> Value {
    serde_json::json!({
        "status": "ok",
        "uid": uid,
        "planId": plan_id,
        "alreadyClaimed": res.already_claimed,
        "claimedAt": res.claimed_at,
        // 「已领过」不是错误，但也不该说成「领取成功」—— 用户会以为自己
        // 刚拿到了东西。
        "message": if res.already_claimed {
            "该套餐此前已领取过（不是错误）"
        } else {
            "领取成功"
        },
    })
}

/// `preview-plan` —— 查可领套餐（Go `runPreviewPlan`，`logincli.go:464-504`）。
///
/// # 退出码
///
/// ```text
/// 0  ok（含「上游业务失败」）**或** no_jwt
/// 2  参数错误（缺 --uid、flag 解析失败）
/// 1  凭证加载不出来
/// ```
///
/// # ⚠ 端点与 `quota` **不是同一个**
///
/// ```text
/// quota        → billing/balance   （已经生效的套餐 + 余额）
/// preview-plan → billing/preview   （**还能领**的套餐）
/// ```
///
/// 把两者混起来会得到一个「看起来能用但永远说没有可领套餐」的命令。
///
/// # ⚠ 请求必须带 query 与 UUID 形态的 `X-Device-Mid`
///
/// 实测：不带 `?app_version=&platform=` 时上游回 **3001 parameter error**。
/// 头集由 `claim.rs::fetch_plan_preview` 内部处理（`preview_headers`，比 claim
/// 更简 —— 多送身份头本身就是可区分特征）。
async fn run_preview_plan(args: &[String]) -> i32 {
    let mut fs = cli::FlagSet::new("zcode-login preview-plan")
        .string("uid", "", false)
        .string("auth-dir", &cred::default_auth_dir(), false);
    if let Err(e) = fs.parse(args) {
        fs.fail(&e);
        return 2;
    }
    let auth_dir = fs.get("auth-dir");

    let cred = match load_cred_for_plan(&auth_dir, &fs.get("uid")) {
        Ok(Some(c)) => c,
        // no_jwt 的 JSON 已写好；参数错误的 usage 也已写好。
        Ok(None) => return 0,
        Err(code) => return code,
    };

    let mut client = Client::new();
    client.apply_host_proxy();

    let pv = match client.fetch_plan_preview(&cred).await {
        Ok(p) => p,
        Err(e) => {
            // 上游业务失败 ⇒ **退出码 0**，原因写进 stdout 的 status=error。
            return cli::write_business_error_uid(&cred.uid, &e.to_string());
        }
    };

    // offers：**4 键**（Go `logincli.go:493-500` 的 map 只有这四个）。
    //
    // ★ 空清单必须是 `[]` 而不是 `null`（宿主按数组读；Go 用
    // `make([]map[string]any, 0, len)` 也是这个口径）。
    let offers: Vec<Value> = pv.offers.iter().map(plan_offer_json).collect();

    cli::write_json(&serde_json::json!({
        "status": "ok",
        "uid": cred.uid,
        "serverTime": pv.server_time,
        "offers": offers,
        "claimable": offers.len(),
    }));
    0
}

/// `claim-plan` —— 领取指定套餐（Go `runClaimPlan`，`logincli.go:518-559`）。
///
/// # ⚠ 这是**写操作**，只由用户显式触发
///
/// 与 `warm` 同一取舍：**绝不挂进 `claim_scheduler` 的自动轮询**。自动抢领会让
/// 账号表现出非人类的活动模式，而上游正是按活动模式判风控的。
///
/// ⚠ 注意区分：`warm` 另有「宿主启动后每账号一次」的自动入口（幂等），
/// 而**本命令没有** —— 领取的时机由 `claim_scheduler` 决定，不受影响。
///
/// # 本命令**不主动求解验证码**
///
/// 只把调用方给的 `Cred.CaptchaParam` 转发出去（Go `claim.go:249`）。求解是
/// `warm` 那条路径的职责（它自己 `mintCaptcha` 并重试两次）。
///
/// # 退出码
///
/// ```text
/// 0  ok（含「上游业务失败」）**或** no_jwt
/// 2  参数错误（缺 --uid / 缺 --plan-id、flag 解析失败）
/// 1  凭证加载不出来
/// ```
///
/// ★ `code == "1003"`（此前已领取过）是**成功，不是错误** —— `claim.rs` 已把它
/// 归成 `ClaimResult{already_claimed:true}` 且不返回 `Err`。
async fn run_claim_plan(args: &[String]) -> i32 {
    let mut fs = cli::FlagSet::new("zcode-login claim-plan")
        .string("uid", "", false)
        .string("plan-id", "", false)
        .string("auth-dir", &cred::default_auth_dir(), false);
    if let Err(e) = fs.parse(args) {
        fs.fail(&e);
        return 2;
    }
    let plan_id = fs.get("plan-id");
    let auth_dir = fs.get("auth-dir");

    // ★ 先拦 `--plan-id` 再加载凭证（与 Go `logincli.go:524-528` 同序）。
    //
    // 不拦的话会发 `{"plan_id":""}` 上去，上游回 3001「parameter error」——
    // 那个文案完全看不出是「我们没传 ID」。
    if plan_id.trim().is_empty() {
        return cli::usage_error("缺少 --plan-id 参数（先用 preview-plan 查看可领的套餐）");
    }

    let cred = match load_cred_for_plan(&auth_dir, &fs.get("uid")) {
        Ok(Some(c)) => c,
        Ok(None) => return 0,
        Err(code) => return code,
    };

    let mut client = Client::new();
    client.apply_host_proxy();

    let res = match client.claim_plan(&cred, &plan_id).await {
        Ok(r) => r,
        Err(e) => {
            // 上游业务失败 ⇒ **退出码 0**，原因写进 stdout 的 status=error。
            return cli::write_business_error_uid(&cred.uid, &e.to_string());
        }
    };

    cli::write_json(&claim_plan_json(&cred.uid, &plan_id, &res));
    0
}

/// `warm` —— 上报激活事件并领取全部可领套餐（Go `runWarm`，`logincli.go:582-634`）。
///
/// # ⚠ 这是**写操作**，两个入口
///
/// 1. 用户显式执行本命令。
/// 2. 宿主启动后每账号各一次（幂等，见 `warm_account` 与
///    `multi_product_credit_patrol::warm_zcode_accounts`）。
///
/// 但**绝不挂进 `claim_scheduler` 的自动轮询**：反复自动「激活 + 领取」会让
/// 账号表现出非人类的活动模式，而上游正是按活动模式判风控的。
///
/// # 退出码
///
/// ```text
/// 0  ok（含「上游业务失败」）/ 连账号都没有（no_jwt 也是 0）
/// 2  参数错误
/// 1  只有「凭证加载不出来」才是 1
/// ```
///
/// ★ 上游业务状态**回退出码 0**，结果写进 JSON 的 `events`/`claim`/`skipped`
/// 三个字段（**空串 = 成功**）。否则宿主会把「上游拒绝了这次领取」当成
/// 「网关命令执行失败」，用户看到的是笼统的「命令失败」而不是具体原因。
/// 宿主 `zcode_login.rs:949` 自己判 `status` 字段。
async fn run_warm(args: &[String]) -> i32 {
    let mut fs = cli::FlagSet::new("zcode-login warm")
        // ★ `required: false`：同 `quota` —— Go `:643`/`:792` 那条家族都是
        // 手写检查（经 `loadCredForPlan`，`:434` 也是 `TrimSpace`），只打一句
        // `缺少 --uid 参数`，不带 usage 行。
        .string("uid", "", false)
        .string("auth-dir", &cred::default_auth_dir(), false);
    if let Err(e) = fs.parse(args) {
        fs.fail(&e);
        return 2;
    }
    let auth_dir = fs.get("auth-dir");

    // 复用 `load_cred_for_plan`：激活的事件 body 需要 user_id（从 JWT 解）、领取
    // 需要 `Authorization: Bearer <jwt>`，所以缺 JWT 时**如实说明**而不是发一堆
    // 必然失败的请求上去。
    let mut cred = match load_cred_for_plan(&auth_dir, &fs.get("uid")) {
        Ok(Some(c)) => c,
        Ok(None) => return 0, // no_jwt 已写 stdout
        Err(code) => return code,
    };

    // ★ 求解器装在这里而不是 `run` 开头：只有本子命令要求解，且必须在凭证确实
    // 加载出来**之后** —— 早退路径（缺 --uid / 账号不存在 / 无 JWT）不该碰共享
    // 单例。原因见 [`install_captcha_solver`] 的注释（实测污染过一次）。
    install_captcha_solver(&auth_dir);

    let client = {
        let mut c = Client::new();
        c.apply_host_proxy();
        c
    };

    let rep = match client.warm_account(&mut cred).await {
        Ok(r) => r,
        Err(e) => {
            return cli::write_business_error_uid(&cred.uid, &e);
        }
    };

    // 汇总一句话，让宿主/用户不必自己拼三个字段的语义。
    // 口径与 `WarmReport` 一致：空串 = 该阶段成功。
    let summary = if !rep.events.is_empty() {
        "激活事件未发出，未执行领取".to_string()
    } else if !rep.skipped.is_empty() {
        format!("激活事件已发出；未执行领取：{}", rep.skipped)
    } else if !rep.claim.is_empty() {
        format!("激活事件已发出；领取部分失败：{}", rep.claim)
    } else if !rep.claimed.is_empty() {
        format!("激活事件已发出；已领取 {} 个套餐", rep.claimed.len())
    } else {
        "激活完成".to_string()
    };

    cli::write_json(&serde_json::json!({
        "status": "ok",
        "uid": cred.uid,
        "events": rep.events,
        "claim": rep.claim,
        "skipped": rep.skipped,
        "plans": rep.plans.iter().map(warm_plan_json).collect::<Vec<_>>(),
        "claimed": rep.claimed,
        "message": summary,
    }));
    0
}

/// `WarmPlan` → JSON。★ 手写是因为 Go 上 `name`/`priority`/`alreadyClaimed`/
/// `message` 都是 `omitempty`，而 `planId` 与 `ok` **总是出现**（含 `ok:false`）。
fn warm_plan_json(p: &warm::WarmPlan) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("planId".into(), Value::String(p.plan_id.clone()));
    if !p.name.is_empty() {
        m.insert("name".into(), Value::String(p.name.clone()));
    }
    if p.priority != 0 {
        m.insert("priority".into(), Value::from(p.priority));
    }
    m.insert("ok".into(), Value::Bool(p.ok));
    if p.already_claimed {
        m.insert("alreadyClaimed".into(), Value::Bool(true));
    }
    if !p.message.is_empty() {
        m.insert("message".into(), Value::String(p.message.clone()));
    }
    Value::Object(m)
}

/// 按 uid 载入凭证，并**强制要求 JWT**。对应 Go `loadCredForPlan`（`logincli.go:433-452`）。
///
/// `preview-plan` / `claim-plan` / `warm` 三个子命令共用：这几个端点都只认 JWT
/// （`Authorization: Bearer <jwt>`），不认 `{apiKey}.{secret}` 对话凭证。缺 JWT 时
/// **如实说明**，而不是发一个必然 401 的请求上去（那会让用户以为「账号没资格」，
/// 实际只是「导入时没带 JWT」）。
///
/// 返回 `Ok(None)` = 已把 `no_jwt` 写到 stdout 且应 `return 0`；
/// `Err(code)` = 已把错误写到 stderr 且应 `return code`。
fn load_cred_for_plan(auth_dir: &str, uid: &str) -> Result<Option<ZcodeCred>, i32> {
    if uid.trim().is_empty() {
        cli::usage_error("缺少 --uid 参数");
        return Err(2);
    }
    let c = match load_cred_by_uid(auth_dir, uid.trim()) {
        Ok(c) => c,
        Err(e) => {
            crate::logline!("{e}");
            return Err(1);
        }
    };
    if c.jwt.trim().is_empty() {
        cli::write_json(&serde_json::json!({
            "status": "no_jwt",
            "uid": c.uid,
            "message": "该账号只导入了对话凭证，没有套餐查询/领取用的 JWT",
        }));
        return Ok(None);
    }
    Ok(Some(c))
}

/// 把验证码求解器的参数从网关配置装到共享单例上。对应 Go `installCaptchaSolver`
/// （`logincli.go:211-232`）。
///
/// ⚠ **只在真正要求解的子命令里调**（当前只有 `warm`），且要在凭证加载成功
/// **之后**调 —— 无条件调用会污染包级单例。
///
/// # 为什么需要它（这是一处真实的接线缺口，不是预防性加固）
///
/// 求解器**只在服务模式被配置**：Go 的 `cmd/server/main.go` 在 `flag.Parse()`
/// 之前就拦截了 `qoder-login` / `zcode-login` 并 `os.Exit`，配置**根本不读**。
/// 于是 `gateway.exe zcode-login warm --uid …` 走到 `mintCaptcha` →
/// `solveCaptcha` → `SharedCaptchaSolver()`（dir 与 externalURL 都是空）→
/// `UnavailableReason()` 返回「求解器组件未安装」。
///
/// 后果是 `warm` 这条**宿主要用的路径**永远拿不到 param，领取阶段必回 3007 ——
/// 而服务模式的同一功能是好的。这正是「新路径接好了、旧的前置检查把它挡在门外」。
///
/// # ⚠ 读配置的那一步必须是纯的
///
/// 曾经把它写成「读配置 + 直接 SetDir/SetExternalSolver」的一体式函数，并在
/// `RunLoginCLI` 开头**无条件**调用。实测证伪：那会**污染包级共享求解器**。
/// Go 侧 `captcha_link_test.go` 明写着本包的不变量：「本包测试从不给
/// `SharedCaptchaSolver()` 设 dir / 外部求解服务，故 `UnavailableReason()` 必非空
/// ⇒ `solveCaptcha` 确定性失败」。而本机
/// `~/.wb-switch/gateway/gateway_native_config.json` 真实存在（候选②命中），于是
/// 任意一次 `RunLoginCLI` 都会把这个不变量翻掉。
///
/// 故读配置只返回值（[`load_captcha_config`]），「装到求解器上」这一步只在这里做。
fn install_captcha_solver(auth_dir: &str) {
    let cfg = load_captcha_config(auth_dir);
    if cfg.dir.is_empty() && cfg.url.is_empty() {
        // 一个都没配：不必碰单例，让 `unavailable_reason()` 保持原样。
        return;
    }

    let cs = captcha::shared_captcha_solver();
    if !cfg.dir.is_empty() {
        cs.set_dir(&cfg.dir);
    }
    // 外部求解（宿主 WebView2）优先：真实浏览器环境，比模拟环境更快更稳。
    if !cfg.url.is_empty() {
        cs.set_external_solver(&cfg.url, &cfg.token);
    }
    // 配置完仍不可用就**如实报**到 stderr（**不阻断子命令**）：否则用户只会看到
    // 3007，而不知道为什么。
    let reason = cs.unavailable_reason();
    if !reason.is_empty() {
        crate::logline!("⚠ ZCode 验证码求解器不可用：{reason}（本次领取会因 3007 失败）");
    }
}

/// 网关配置里的验证码求解器参数（纯数据）。
struct CaptchaConfig {
    dir: String,
    url: String,
    token: String,
}

/// 从网关配置文件里读出验证码求解器的三个参数。对应 Go `loadCaptchaConfig`
/// （`logincli.go:150-201`）。
///
/// ★ 本函数必须是**纯的**（只读文件、只返回值），理由见 [`install_captcha_solver`]。
///
/// 三个候选位置（顺序有意义，照抄 Go 与 `qoder/logincli.go` 的 `loadProxyConfig`）：
///
/// ```text
/// ① <data>/gateway/gateway_native_config.json        （authDir 往上两级）
/// ② $HOME/.wb-switch/gateway/gateway_native_config.json
/// ③ $AI_GATEWAY_HOME/gateway/gateway_native_config.json
/// ```
///
/// 用 `auth_dir` 往上找而不是写死 `%USERPROFILE%` —— 既支持自定义数据目录，
/// 也让测试能指向临时目录。
fn load_captcha_config(auth_dir: &str) -> CaptchaConfig {
    let empty = CaptchaConfig {
        dir: String::new(),
        url: String::new(),
        token: String::new(),
    };

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    // `filepath.Dir(filepath.Dir(authDir))` —— 注意是**往上两级**：凭证目录是
    // `<data>/zcode/auths`，配置在 `<data>/gateway/`。
    let up2 = std::path::Path::new(auth_dir)
        .parent()
        .and_then(|p| p.parent());
    if let Some(d) = up2 {
        if !d.as_os_str().is_empty() {
            candidates.push(d.join("gateway").join("gateway_native_config.json"));
        }
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(
            home.join(".wb-switch")
                .join("gateway")
                .join("gateway_native_config.json"),
        );
    }
    if let Ok(env) = std::env::var("AI_GATEWAY_HOME") {
        let env = env.trim();
        if !env.is_empty() {
            candidates.push(
                std::path::Path::new(env)
                    .join("gateway")
                    .join("gateway_native_config.json"),
            );
        }
    }

    for p in candidates {
        let Ok(raw) = std::fs::read(&p) else {
            continue;
        };
        let Ok(doc) = serde_json::from_slice::<Value>(&raw) else {
            continue;
        };
        // ⚠ 这几个 captcha 键在 `pool` 段**内**，不在顶层 —— 顶层解不出来会静默
        // 拿到空串，表现与「文件不存在」一模一样。
        //
        // `zcode_captcha_enabled` 刻意**不解析**：老配置里可能留着 false，据此
        // 关闭会让升级后的用户「功能永远关着」（与 `client.rs` 的
        // `configure_captcha_from_host` 同口径）。
        let pick = |key: &str| -> String {
            doc.pointer(&format!("/pool/{key}"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        return CaptchaConfig {
            dir: pick("zcode_captcha_dir"),
            url: pick("zcode_captcha_solver_url"),
            token: pick("zcode_captcha_solver_token"),
        };
    }
    // 找不到配置文件**不是错误**：调用方拿零值，求解器保持不可用，与「用户没配
    // 求解器」表现一致（而不是凭空报一个读文件失败）。
    empty
}

/// 造一个只属于本进程的临时数据目录（`<tmp>/<name>-<pid>`）。
///
/// ★ 测试**绝不能**用真实的 `~/.wb-switch`：`load_captcha_config` 的候选②就是它，
/// 而本机那个文件真实存在 —— 用真实目录会让测试的结果取决于开发机的配置。
#[cfg(test)]
fn temp_data_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subcommand_table_matches_the_go_switch() {
        // Go `logincli.go:256-295` 的 case 表，顺序一致。
        assert_eq!(
            SUBCOMMANDS,
            &[
                "start",
                "poll",
                "import",
                "quota",
                "models",
                "preview-plan",
                "claim-plan",
                "warm"
            ]
        );
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ==================== 派发与退出码契约 ====================

    #[tokio::test]
    async fn no_subcommand_prints_usage_and_returns_two() {
        // Go `logincli.go:251-254`：无子命令 ⇒ 打**用法行** + return 2。
        assert_eq!(run(&[]).await, 2);
    }

    #[tokio::test]
    async fn unknown_subcommand_returns_two() {
        // Go `logincli.go` 的 `default:` 分支。
        assert_eq!(run(&args(&["nope"])).await, 2);
    }

    #[tokio::test]
    async fn every_subcommand_is_dispatched_and_none_is_unknown() {
        // ★ 这条防的是「加了子命令但忘了接线」：未知子命令与已知子命令都返回 2，
        // 光看退出码区分不出来，必须比对 stderr 文案 —— 这里退而求其次，
        // 断言每个名字都**不被**当成未知子命令（用 `SUBCOMMANDS` 驱动）。
        for sub in SUBCOMMANDS {
            // 只给子命令名、不给参数 ⇒ 每个 `runXxx` 都会走到参数校验并返回 2；
            // 关键是**不能**出现「未知子命令」那条文案。
            let code = run(&args(&[sub])).await;
            assert_eq!(code, 2, "{sub} 缺参数时应返回 2");
        }
    }

    // ==================== quota ====================

    #[tokio::test]
    async fn quota_without_uid_returns_two() {
        // 缺 `--uid` ⇒ 2（宿主此时不看 stdout）。
        assert_eq!(run(&args(&["quota"])).await, 2);
    }

    #[tokio::test]
    async fn quota_with_blank_uid_returns_two() {
        // `--uid ""` 与 `--uid "   "` 都算没给（Go 侧 `TrimSpace` 同款）。
        assert_eq!(run(&args(&["quota", "--uid", "   "])).await, 2);
    }

    #[tokio::test]
    async fn quota_with_unknown_flag_returns_two() {
        assert_eq!(run(&args(&["quota", "--uid", "u1", "--nope"])).await, 2);
    }

    #[test]
    fn quota_exit_code_is_one_only_when_credential_is_missing() {
        // ★ 契约固化：`quota` 的 1 **只**由「找不到凭证」触发。
        // 用一个不存在的目录，确保 `load_cred_by_uid` 走失败分支。
        let dir = std::env::temp_dir().join("zcode-test-no-such-dir-quota");
        let d = dir.to_string_lossy().to_string();
        let err = load_cred_by_uid(&d, "nobody").expect_err("不存在的目录必须找不到凭证");
        // 文案与 Go `loadCredByUID` 的最后一行一致（`%s`/`%s` 依次是 dir、uid）。
        assert_eq!(err, format!("在 {d} 里找不到 uid=nobody 的凭证"));
    }

    // ==================== load_cred_by_uid ====================

    #[test]
    fn load_cred_by_uid_falls_back_to_the_zcode_prefixed_filename() {
        // ★ 这条是 Go `loadCredByUID` 的核心行为：`uid` 不以 `zcode-` 开头时，
        // 文件名要先补前缀。少了这一步，用户给 `uid=abc` 而文件叫
        // `zcode-abc.json` 就永远找不到。
        let dir = std::env::temp_dir().join(format!("zcode-uid-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let d = dir.to_string_lossy().to_string();
        let body = serde_json::json!({
            "uid": "abc",
            "credential": "key.secret",
            "provider": "zai",
        })
        .to_string();
        std::fs::write(dir.join("zcode-abc.json"), body).expect("应能写入测试凭证");

        let c = load_cred_by_uid(&d, "abc").expect("应能按补前缀后的文件名找到");
        assert_eq!(c.uid, "abc");

        // 传完整文件名形式也应命中（先试直接路径）。
        let c2 = load_cred_by_uid(&d, "zcode-abc").expect("完整形式也应找到");
        assert_eq!(c2.uid, "abc");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_cred_by_uid_matches_on_the_uid_field_not_just_the_filename() {
        // 文件名与 `uid` 字段不总是同一个值 ⇒ 回退路径要按字段比对。
        let dir = std::env::temp_dir().join(format!("zcode-uid-fallback-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let d = dir.to_string_lossy().to_string();
        // 文件名是 `zcode-other.json`，但 `uid` 字段是 `wanted`。
        let body = serde_json::json!({
            "uid": "wanted",
            "credential": "key.secret",
            "provider": "bigmodel",
        })
        .to_string();
        std::fs::write(dir.join("zcode-other.json"), body).expect("应能写入测试凭证");

        let c = load_cred_by_uid(&d, "wanted").expect("应能按 uid 字段回退找到");
        assert_eq!(c.uid, "wanted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ==================== account_id_of ====================

    #[test]
    fn account_id_of_prefers_the_stored_field() {
        // 凭证里已存 `account_id` ⇒ 直接用，不必去解 JWT。
        let mut c = ZcodeCred::default();
        c.account_id = "acct-from-file".to_string();
        assert_eq!(account_id_of(&c), "acct-from-file");
    }

    #[test]
    fn account_id_of_is_empty_when_nothing_is_available() {
        // 两者都没有 ⇒ 空串（**不猜**，也不报错）。
        let c = ZcodeCred::default();
        assert_eq!(account_id_of(&c), "");
    }

    #[test]
    fn account_id_of_falls_back_to_the_jwt() {
        // 没有存的字段但 JWT 里有 `sub` ⇒ 现解一次。
        let mut c = ZcodeCred::default();
        let payload = serde_json::json!({"sub": "acct-from-jwt"});
        let enc = base64_url(&serde_json::to_vec(&payload).expect("应能序列化"));
        c.jwt = format!("h.{enc}.s");
        assert_eq!(account_id_of(&c), "acct-from-jwt");
    }

    /// base64url 无填充编码（JWT 段的形态）。
    fn base64_url(b: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    }

    // ==================== load_cred_for_plan（preview/claim/warm 共用） ====================

    #[test]
    fn load_cred_for_plan_without_uid_returns_two() {
        // Go `loadCredForPlan:434-437`：uid trim 空 ⇒ stderr + **2**。
        assert_eq!(load_cred_for_plan("nowhere", ""), Err(2));
        assert_eq!(load_cred_for_plan("nowhere", "   "), Err(2));
    }

    #[test]
    fn load_cred_for_plan_missing_credential_returns_one() {
        // Go `:438-442`：`loadCredByUID` 失败 ⇒ stderr + **1**（不是 0）。
        let d = temp_data_dir("zcode-forplan-missing");
        let ds = d.to_string_lossy().to_string();
        assert_eq!(load_cred_for_plan(&ds, "nobody"), Err(1));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn load_cred_for_plan_without_jwt_writes_no_jwt_and_returns_zero() {
        // ★ 关键分支：只有凭证、没有 JWT ⇒ `Ok(None)`（调用方 return 0），
        // **不是**错误。宿主据此显示「额度未知」而不是「账号没资格」。
        let d = temp_data_dir("zcode-forplan-nojwt");
        let ds = d.to_string_lossy().to_string();
        std::fs::write(
            d.join("zcode-plain.json"),
            serde_json::json!({
                "uid": "plain",
                "credential": "key.secret",
                "provider": "bigmodel",
            })
            .to_string(),
        )
        .expect("应能写入测试凭证");

        let got = load_cred_for_plan(&ds, "plain").expect("no_jwt 不是错误");
        assert!(got.is_none(), "缺 JWT 时应返回 Ok(None)");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn load_cred_for_plan_with_jwt_returns_the_credential() {
        let d = temp_data_dir("zcode-forplan-jwt");
        let ds = d.to_string_lossy().to_string();
        std::fs::write(
            d.join("zcode-withjwt.json"),
            serde_json::json!({
                "uid": "withjwt",
                "credential": "key.secret",
                "provider": "bigmodel",
                "jwt": "h.e30.s",
            })
            .to_string(),
        )
        .expect("应能写入测试凭证");

        let got = load_cred_for_plan(&ds, "withjwt")
            .expect("有 JWT 时应成功")
            .expect("应返回凭证");
        assert_eq!(got.uid, "withjwt");
        assert_eq!(got.jwt, "h.e30.s");
        let _ = std::fs::remove_dir_all(&d);
    }

    // ==================== load_captcha_config（必须是纯函数） ====================

    #[test]
    fn load_captcha_config_reads_the_three_keys_from_inside_pool() {
        // ★ 三个键在 `pool` 段**内**。放到顶层会静默拿到空串，表现与「文件不存在」
        // 一模一样 —— 这条测试就是钉住这个位置。
        let d = temp_data_dir("zcode-capcfg-pool");
        let gw = d.join("gateway");
        std::fs::create_dir_all(&gw).expect("应能建 gateway 目录");
        std::fs::write(
            gw.join("gateway_native_config.json"),
            serde_json::json!({
                "pool": {
                    "zcode_captcha_dir": "/opt/captcha",
                    "zcode_captcha_solver_url": "http://127.0.0.1:9/solve",
                    "zcode_captcha_solver_token": "tok-1",
                }
            })
            .to_string(),
        )
        .expect("应能写入配置");

        // authDir 是 `<data>/zcode/auths` ⇒ 往上两级是 `<data>`。
        let auth_dir = d.join("zcode").join("auths");
        let cfg = load_captcha_config(&auth_dir.to_string_lossy());
        assert_eq!(cfg.dir, "/opt/captcha");
        assert_eq!(cfg.url, "http://127.0.0.1:9/solve");
        assert_eq!(cfg.token, "tok-1");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn load_captcha_config_ignores_the_same_keys_at_the_top_level() {
        // 反向钉住：顶层那三个键**不算数**（Go 的结构体只解 `pool` 段）。
        let d = temp_data_dir("zcode-capcfg-toplevel");
        let gw = d.join("gateway");
        std::fs::create_dir_all(&gw).expect("应能建 gateway 目录");
        std::fs::write(
            gw.join("gateway_native_config.json"),
            serde_json::json!({
                "zcode_captcha_dir": "/wrong",
                "zcode_captcha_solver_url": "http://wrong",
                "zcode_captcha_solver_token": "wrong",
            })
            .to_string(),
        )
        .expect("应能写入配置");

        let auth_dir = d.join("zcode").join("auths");
        let cfg = load_captcha_config(&auth_dir.to_string_lossy());
        assert_eq!(cfg.dir, "", "顶层键必须被忽略");
        assert_eq!(cfg.url, "");
        assert_eq!(cfg.token, "");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn load_captcha_config_does_not_parse_the_enabled_flag() {
        // ★ 刻意**不解析** `zcode_captcha_enabled`：老配置里可能留着 false，
        // 据此关闭会让升级后的用户「功能永远关着」（与 main.go 同口径）。
        let d = temp_data_dir("zcode-capcfg-enabled");
        let gw = d.join("gateway");
        std::fs::create_dir_all(&gw).expect("应能建 gateway 目录");
        std::fs::write(
            gw.join("gateway_native_config.json"),
            serde_json::json!({
                "pool": {
                    "zcode_captcha_enabled": false,
                    "zcode_captcha_dir": "/opt/captcha",
                }
            })
            .to_string(),
        )
        .expect("应能写入配置");

        let auth_dir = d.join("zcode").join("auths");
        let cfg = load_captcha_config(&auth_dir.to_string_lossy());
        assert_eq!(cfg.dir, "/opt/captcha", "enabled=false 不该让目录失效");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn load_captcha_config_is_pure_and_leaves_the_shared_solver_untouched() {
        // ★★ 这条是本模块最重要的不变量：`load_captcha_config` **只读文件、
        // 只返回值**，绝不碰共享求解器单例。
        //
        // 历史：曾经写成「读配置 + 直接 SetDir/SetExternalSolver」的一体式函数，
        // 并在 CLI 入口无条件调用 ⇒ 污染了包级单例，把
        // 「本包测试从不给 SharedCaptchaSolver() 设 dir」这个不变量翻掉。
        let d = temp_data_dir("zcode-capcfg-pure");
        let gw = d.join("gateway");
        std::fs::create_dir_all(&gw).expect("应能建 gateway 目录");
        std::fs::write(
            gw.join("gateway_native_config.json"),
            serde_json::json!({
                "pool": {
                    "zcode_captcha_dir": "/should/not/be/installed",
                    "zcode_captcha_solver_url": "http://should-not-be-installed",
                }
            })
            .to_string(),
        )
        .expect("应能写入配置");

        let solver = captcha::shared_captcha_solver();
        let before_dir = solver.dir();
        let before_external = solver.external_solve_enabled();

        let auth_dir = d.join("zcode").join("auths");
        let cfg = load_captcha_config(&auth_dir.to_string_lossy());
        assert_eq!(cfg.dir, "/should/not/be/installed", "返回值要读到");

        assert_eq!(solver.dir(), before_dir, "load 不该改单例的 dir");
        assert_eq!(
            solver.external_solve_enabled(),
            before_external,
            "load 不该改单例的外部求解开关"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn load_captcha_config_falls_through_to_the_home_candidate() {
        // ⚠ 本机 `~/.wb-switch/gateway/gateway_native_config.json`（候选②）**真实
        // 存在** —— Go 侧 `logincli.go:136-146` 记的就是这件事，那里还因此污染过
        // 一次共享求解器。所以这条测试**不能**假设「找不到候选就返回零值」。
        //
        // 它断言的是**回退顺序**：auth-dir 派生的那条不存在时，结果必须与候选②
        // 完全一致（候选②也不存在时才是零值）。
        let cfg = load_captcha_config("relative/nonexistent/auths");

        let Some(home) = dirs::home_dir() else {
            assert_eq!(cfg.dir, "");
            return;
        };
        let p = home
            .join(".wb-switch")
            .join("gateway")
            .join("gateway_native_config.json");
        let Ok(raw) = std::fs::read(&p) else {
            // 候选②也不存在 ⇒ 只能是零值（「找不到配置文件不是错误」）。
            assert_eq!(cfg.dir, "");
            assert_eq!(cfg.url, "");
            assert_eq!(cfg.token, "");
            return;
        };

        let doc: Value = serde_json::from_slice(&raw).expect("候选②应是合法 JSON");
        let pick = |k: &str| -> String {
            doc.pointer(&format!("/pool/{k}"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        assert_eq!(cfg.dir, pick("zcode_captcha_dir"));
        assert_eq!(cfg.url, pick("zcode_captcha_solver_url"));
        assert_eq!(cfg.token, pick("zcode_captcha_solver_token"));
    }

    // ==================== warm ====================

    #[tokio::test]
    async fn warm_without_uid_returns_two() {
        // Go `runWarm:586-588`（flag.Parse 错）与 `loadCredForPlan:434-437`。
        assert_eq!(run(&args(&["warm"])).await, 2);
    }

    #[tokio::test]
    async fn warm_with_unknown_flag_returns_two() {
        assert_eq!(run(&args(&["warm", "--nope", "x"])).await, 2);
    }

    #[tokio::test]
    async fn warm_with_missing_credential_returns_one() {
        // ★ 唯一返回 1 的路径：凭证加载不出来（`loadCredForPlan` 的 `return 1`）。
        let d = temp_data_dir("zcode-warm-missing");
        let ds = d.to_string_lossy().to_string();
        let code = run(&args(&["warm", "--uid", "nobody", "--auth-dir", &ds])).await;
        assert_eq!(code, 1, "凭证找不到才是 1");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn warm_without_jwt_returns_zero() {
        // ★ 缺 JWT 走 `no_jwt`：**退出码 0**（宿主显示「额度未知」而不是命令失败）。
        // 这也顺带钉住「求解器装不上也不影响早退路径」。
        let d = temp_data_dir("zcode-warm-nojwt");
        let ds = d.to_string_lossy().to_string();
        std::fs::write(
            d.join("zcode-plain.json"),
            serde_json::json!({
                "uid": "plain",
                "credential": "key.secret",
                "provider": "bigmodel",
            })
            .to_string(),
        )
        .expect("应能写入测试凭证");

        assert_eq!(run(&args(&["warm", "--uid", "plain", "--auth-dir", &ds])).await, 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    // ==================== WarmPlan → JSON 的 omitempty 口径 ====================

    #[test]
    fn warm_plan_json_always_keeps_plan_id_and_ok() {
        // Go `WarmPlan`：`planId` 与 `ok` **没有** omitempty ⇒ 永远出现
        // （含 `ok:false` —— 界面靠它区分「失败」与「没这个字段」）。
        let p = warm::WarmPlan {
            plan_id: "plan-1".to_string(),
            ..Default::default()
        };
        let v = warm_plan_json(&p);
        assert_eq!(v["planId"], serde_json::json!("plan-1"));
        assert_eq!(v["ok"], serde_json::json!(false));
    }

    #[test]
    fn warm_plan_json_omits_empty_optional_fields() {
        // 其余四个字段都是 omitempty ⇒ 空值/零值**不该出现**。
        let p = warm::WarmPlan {
            plan_id: "plan-1".to_string(),
            ok: true,
            ..Default::default()
        };
        let v = warm_plan_json(&p);
        let obj = v.as_object().expect("应是对象");
        assert!(!obj.contains_key("name"), "空 name 不该出现");
        assert!(!obj.contains_key("priority"), "0 priority 不该出现");
        assert!(!obj.contains_key("alreadyClaimed"), "false 不该出现");
        assert!(!obj.contains_key("message"), "空 message 不该出现");
        assert_eq!(obj.len(), 2, "只剩 planId 与 ok");
    }

    #[test]
    fn warm_plan_json_keeps_non_empty_optional_fields() {
        let p = warm::WarmPlan {
            plan_id: "plan-2".to_string(),
            name: "体验套餐".to_string(),
            priority: 5,
            ok: true,
            already_claimed: true,
            message: "此前已领取过（不是错误）".to_string(),
        };
        let v = warm_plan_json(&p);
        assert_eq!(v["name"], serde_json::json!("体验套餐"));
        assert_eq!(v["priority"], serde_json::json!(5));
        assert_eq!(v["alreadyClaimed"], serde_json::json!(true));
        assert_eq!(v["message"], serde_json::json!("此前已领取过（不是错误）"));
    }

    // ==================== import ====================

    #[tokio::test]
    async fn import_without_credential_returns_two() {
        // Go `logincli.go:902-905`：`*credential == ""` ⇒ 2。
        assert_eq!(run(&args(&["import"])).await, 2);
        assert_eq!(run(&args(&["import", "--credential", ""])).await, 2);
    }

    #[tokio::test]
    async fn import_with_a_whitespace_credential_is_not_a_usage_error() {
        // ★ 这条钉住 Go 侧第二处**真实的不对称**（第一处见
        // `poll_with_a_whitespace_session_is_not_a_usage_error`）：
        //
        // ```text
        // logincli.go:902  import  *credential == ""    （不 trim）
        // logincli.go:434  loadCredForPlan  TrimSpace   （trim）
        // ```
        //
        // `--credential "   "` 因此**不是**参数错误：`is_empty()` 为假，
        // 它会走到 `parse_credential`，被那里 trim 成空串后报「凭证为空」，
        // 以退出码 **1** 收场（而不是 2）。
        //
        // 这正是 `run_import` 的 `FlagSet` 必须用 `required: false` 的原因 ——
        // 必填校验内部会 trim，会把这条路径提前变成 2。
        assert_eq!(run(&args(&["import", "--credential", "   "])).await, 1);
    }

    #[tokio::test]
    async fn import_with_unknown_flag_returns_two() {
        assert_eq!(
            run(&args(&["import", "--credential", "k.s", "--nope"])).await,
            2
        );
    }

    #[tokio::test]
    async fn import_rejects_a_credential_with_a_space_and_returns_one() {
        // `parse_credential` 的另一条错误路径：多半是用户粘成了
        // `apiKey: xxx` 这种带说明文字的形式。Go 侧同样是 1。
        assert_eq!(
            run(&args(&["import", "--credential", "key with space"])).await,
            1
        );
    }

    #[tokio::test]
    async fn import_writes_the_credential_file_and_returns_zero() {
        let dir = temp_data_dir("cli-import-ok");
        let auth = dir.join("auths");
        let a = auth.to_string_lossy().to_string();
        assert_eq!(
            run(&args(&[
                "import",
                "--credential",
                "testkey.testsecret",
                "--nickname",
                "单元测试",
                "--auth-dir",
                &a,
            ]))
            .await,
            0
        );

        // 落盘可读回 —— 这是 `import` 存在的意义（凭证要能被后续子命令加载）。
        // ★ `load_dir` 返回 `(成功, 失败清单)` 而不是 `Result`（它刻意**不**因
        // 单个坏文件整体失败，见 cred.rs:669-675）。这里顺带断言失败清单为空。
        let (list, failed) = cred::load_dir(&a);
        assert!(failed.is_empty(), "不该有解析失败的文件：{failed:?}");
        assert_eq!(list.len(), 1, "应恰好写入一份凭证");
        let c = &list[0];
        assert_eq!(c.credential, "testkey.testsecret");
        assert_eq!(c.nickname, "单元测试");
        assert!(c.uid.starts_with("zcode-"), "uid 应带 zcode- 前缀：{}", c.uid);
        // 文件名与 uid 对应，后续 `--uid` 才能找到它。
        assert!(
            std::path::Path::new(&c.file_path)
                .file_name()
                .map(|s| s.to_string_lossy().contains(&c.uid))
                .unwrap_or(false),
            "文件名应含 uid：{}",
            c.file_path
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn import_falls_back_to_zai_when_provider_is_missing_or_unknown() {
        // ★ 未指定 provider 时**回退到 Z.AI 而不是报错**（导入场景用户往往不填），
        // 但**不能猜成 bigmodel** —— 猜错会让账号打到错误端点、必然失败。
        for p in [None, Some(""), Some("   "), Some("unknown-provider")] {
            let dir = temp_data_dir(&format!("cli-import-prov-{:?}", p.map(str::len)));
            let auth = dir.join("auths");
            let a = auth.to_string_lossy().to_string();
            let mut argv = vec!["import", "--credential", "testkey.testsecret", "--auth-dir", &a];
            if let Some(p) = p {
                argv.push("--provider");
                argv.push(p);
            }
            assert_eq!(run(&args(&argv)).await, 0, "provider={p:?} 应成功");
            let (list, failed) = cred::load_dir(&a);
            assert!(failed.is_empty(), "不该有解析失败的文件：{failed:?}");
            assert_eq!(
                list[0].provider,
                Provider::Zai,
                "provider={p:?} 应回退到 Z.AI"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[tokio::test]
    async fn import_keeps_an_explicit_bigmodel_provider() {
        // 显式给了就得**照用** —— 回退只针对「没给/认不出」。
        let dir = temp_data_dir("cli-import-bigmodel");
        let auth = dir.join("auths");
        let a = auth.to_string_lossy().to_string();
        assert_eq!(
            run(&args(&[
                "import",
                "--credential",
                "testkey.testsecret",
                "--provider",
                "bigmodel",
                "--auth-dir",
                &a,
            ]))
            .await,
            0
        );
        let (list, failed) = cred::load_dir(&a);
        assert!(failed.is_empty(), "不该有解析失败的文件：{failed:?}");
        assert_eq!(list[0].provider, Provider::Bigmodel);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn import_with_jwt_sets_the_account_id_from_the_token() {
        // `--jwt` 是给额度查询用的（`quota` 只认 JWT，不认 `key.secret`）。
        // 假 JWT 解不出 `account_id` ⇒ 空串，但**不该**让导入失败。
        let dir = temp_data_dir("cli-import-jwt");
        let auth = dir.join("auths");
        let a = auth.to_string_lossy().to_string();
        assert_eq!(
            run(&args(&[
                "import",
                "--credential",
                "testkey.testsecret",
                "--jwt",
                "not-a-real-jwt",
                "--auth-dir",
                &a,
            ]))
            .await,
            0
        );
        let (list, failed) = cred::load_dir(&a);
        assert!(failed.is_empty(), "不该有解析失败的文件：{failed:?}");
        assert_eq!(list[0].jwt, "not-a-real-jwt");
        assert_eq!(list[0].jwt_issued_at, 0, "解不开的 JWT ⇒ iat 记 0，不报错");
        assert_eq!(list[0].account_id, "", "解不开的 JWT ⇒ account_id 空串");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ==================== start / poll ====================

    #[tokio::test]
    async fn start_without_provider_returns_two() {
        // Go `logincli.go:316-320`：`ParseProvider == ProviderUnknown` ⇒ 2。
        // 三种拼写都落进 Unknown（空串 / 空白 / 未知名字）。
        for p in ["", "   ", "unknown-provider"] {
            assert_eq!(
                run_start(&args(&["--provider", p])).await,
                2,
                "--provider {p:?} 应返回 2"
            );
        }
    }

    #[tokio::test]
    async fn start_without_the_provider_flag_at_all_returns_two() {
        // flag 没给 ⇒ 默认空串 ⇒ 同样落进 Unknown。
        assert_eq!(run_start(&args(&[])).await, 2);
    }

    #[tokio::test]
    async fn start_with_unknown_flag_returns_two() {
        assert_eq!(
            run_start(&args(&["--provider", "zai", "--nope", "x"])).await,
            2
        );
    }

    #[tokio::test]
    async fn poll_without_session_returns_two() {
        // Go `logincli.go:351-354`：缺 `--session` ⇒ 2。
        assert_eq!(run_poll(&args(&[])).await, 2);
    }

    #[tokio::test]
    async fn poll_with_a_whitespace_session_is_not_a_usage_error() {
        // ★ 这条钉住的是 Go 侧一处**真实的不对称**，别去「统一」它：
        //
        // ```text
        // logincli.go:351  poll          *sessionID == ""            （不 trim）
        // logincli.go:434  loadCredForPlan  TrimSpace(uid) == ""     （trim）
        // logincli.go:643  quota         TrimSpace(*uid) == ""       （trim）
        // logincli.go:792  models        TrimSpace(*uid) == ""       （trim）
        // logincli.go:902  import        *credential == ""           （不 trim）
        // ```
        //
        // `--session "   "` 因此**不是**参数错误：它会走到 `loadSession`，
        // 被 `sanitize_name` 变成 `___`，最后以「登录会话不存在或已过期」+
        // 退出码 **1** 收场。
        //
        // 宿主传的永远是 `start` 返回的 sessionId，这条路径谁也走不到；为它
        // 引入一个与 Go 不同的分支，等于用一个自造的差异换掉一个真实的差异。
        let dir = temp_data_dir("cli-poll-blank");
        let auth = dir.join("zcode").join("auths");
        assert_eq!(
            run_poll(&args(&[
                "--session",
                "   ",
                "--auth-dir",
                &auth.to_string_lossy(),
            ]))
            .await,
            1,
            "空白 session 不是参数错误（Go 不 trim）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn poll_with_unknown_flag_returns_two() {
        assert_eq!(run_poll(&args(&["--session", "s", "--nope", "x"])).await, 2);
    }

    #[tokio::test]
    async fn poll_with_a_missing_session_file_returns_one() {
        // Go `logincli.go:363-366`：`loadSession` 失败 ⇒ stderr + **1**。
        //
        // ★ 这条路径**不该**发网络请求 —— 会话读不出来就没什么可轮询的。
        let dir = temp_data_dir("cli-poll-missing");
        let auth = dir.join("zcode").join("auths");
        let code = run_poll(&args(&[
            "--session",
            "no-such-flow",
            "--auth-dir",
            &auth.to_string_lossy(),
        ]))
        .await;
        assert_eq!(code, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn start_and_poll_share_the_same_session_directory() {
        // ★ 跨进程契约：`start` 写的会话必须能被 `poll` 按同一个
        // `--auth-dir` 找到。这条测试钉住的是「两个子命令用同一个
        // sessions_dir 推导」这件事 —— 各写一份推导逻辑迟早会分叉。
        let dir = temp_data_dir("cli-session-dir");
        let auth = dir.join("zcode").join("auths");
        let auth_s = auth.to_string_lossy().to_string();

        let s = login::LoginSession {
            provider: Provider::Zai,
            flow_id: "flow-shared".to_string(),
            poll_token: "tok".to_string(),
            authorize_url: "https://example.invalid/a".to_string(),
            expires_at: 1,
            poll_interval_sec: 2,
            created_at: 0,
        };
        login::save_session(&auth_s, &s).expect("应能写会话");

        // poll 侧按同一个 auth-dir 找得到。
        let back = login::load_session(&auth_s, "flow-shared").expect("poll 侧应能读到 start 写的会话");
        assert_eq!(back.flow_id, "flow-shared");
        assert_eq!(back.poll_token, "tok");

        // ★ 会话目录是 authDir 的**同级**，不在里面 —— 否则 `load_dir` 的
        // glob `zcode*.json` 会把它当凭证解析并报错。
        assert!(auth_s.ends_with("auths"));
        assert!(!login::sessions_dir(&auth_s).starts_with(&auth_s));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ==================== preview-plan / claim-plan ====================

    #[tokio::test]
    async fn preview_plan_without_uid_returns_two() {
        assert_eq!(run_preview_plan(&args(&[])).await, 2);
        assert_eq!(run_preview_plan(&args(&["--uid", "  "])).await, 2);
    }

    #[tokio::test]
    async fn preview_plan_with_unknown_flag_returns_two() {
        assert_eq!(
            run_preview_plan(&args(&["--uid", "u", "--nope", "x"])).await,
            2
        );
    }

    #[tokio::test]
    async fn preview_plan_with_missing_credential_returns_one() {
        let dir = temp_data_dir("cli-preview-missing");
        let auth = dir.join("auths");
        assert_eq!(
            run_preview_plan(&args(&[
                "--uid",
                "nobody",
                "--auth-dir",
                &auth.to_string_lossy(),
            ]))
            .await,
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn claim_plan_without_plan_id_returns_two_with_its_own_wording() {
        // ★ Go `logincli.go:524-528`：缺 `--plan-id` 有**专门文案**
        //（指引用户先跑 preview-plan），而不是笼统的「缺少参数」。
        //
        // 顺序也重要：`--plan-id` 的检查**在**加载凭证之前 —— 否则用户会先
        // 收到「找不到凭证」，而真正的问题是命令少了个参数。
        assert_eq!(run_claim_plan(&args(&[])).await, 2);
        assert_eq!(run_claim_plan(&args(&["--plan-id", "  "])).await, 2);
    }

    #[tokio::test]
    async fn claim_plan_checks_plan_id_before_touching_credentials() {
        // 用一个**不存在**的 auth-dir：若实现先加载凭证，这里会返回 1 而不是 2。
        let dir = temp_data_dir("cli-claim-order");
        let auth = dir.join("auths");
        assert_eq!(
            run_claim_plan(&args(&[
                "--uid",
                "nobody",
                "--auth-dir",
                &auth.to_string_lossy(),
            ]))
            .await,
            2,
            "缺 --plan-id 必须在加载凭证之前被拦下"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn claim_plan_with_unknown_flag_returns_two() {
        assert_eq!(
            run_claim_plan(&args(&["--uid", "u", "--plan-id", "p", "--nope", "x"])).await,
            2
        );
    }

    #[tokio::test]
    async fn claim_plan_with_missing_credential_returns_one() {
        let dir = temp_data_dir("cli-claim-missing");
        let auth = dir.join("auths");
        assert_eq!(
            run_claim_plan(&args(&[
                "--uid",
                "nobody",
                "--plan-id",
                "plan-1",
                "--auth-dir",
                &auth.to_string_lossy(),
            ]))
            .await,
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_offer_json_has_exactly_the_four_go_keys() {
        // Go `logincli.go:493-500` 的手写 map 只有这四个键。
        let o = PlanOffer {
            plan_id: "plan-1".to_string(),
            name: "体验套餐".to_string(),
            status: "claimable".to_string(),
            ends_at: 1790000000,
            raw: serde_json::json!({ "plan_id": "plan-1", "priority": 9, "internal": "x" }),
        };
        let v = plan_offer_json(&o);
        let obj = v.as_object().expect("应是对象");
        assert_eq!(obj.len(), 4, "★ `raw` 不该透出去：{v}");
        assert_eq!(v["planId"], serde_json::json!("plan-1"));
        assert_eq!(v["name"], serde_json::json!("体验套餐"));
        assert_eq!(v["status"], serde_json::json!("claimable"));
        assert_eq!(v["endsAt"], serde_json::json!(1790000000i64));
        assert!(obj.get("raw").is_none());
        assert!(obj.get("priority").is_none());
    }

    #[test]
    fn claim_plan_json_says_success_for_a_fresh_claim() {
        let res = ClaimResult {
            ok: true,
            code: "0".to_string(),
            msg: String::new(),
            already_claimed: false,
            claimed_at: 1790000000,
        };
        let v = claim_plan_json("uid-1", "plan-1", &res);
        assert_eq!(v["status"], serde_json::json!("ok"));
        assert_eq!(v["uid"], serde_json::json!("uid-1"));
        assert_eq!(v["planId"], serde_json::json!("plan-1"));
        assert_eq!(v["alreadyClaimed"], serde_json::json!(false));
        assert_eq!(v["claimedAt"], serde_json::json!(1790000000i64));
        assert_eq!(v["message"], serde_json::json!("领取成功"));
    }

    #[test]
    fn claim_plan_json_marks_already_claimed_as_success_not_an_error() {
        // ★ 上游 code 1003（此前已领取过）**不是错误**：`claim.rs` 已把它归成
        // `already_claimed:true` 且不返回 `Err`。这里钉住 CLI 侧的文案 ——
        // 说成「领取成功」会让用户以为自己刚拿到了东西。
        let res = ClaimResult {
            ok: true,
            code: "1003".to_string(),
            msg: "already claimed".to_string(),
            already_claimed: true,
            claimed_at: 0,
        };
        let v = claim_plan_json("uid-1", "plan-1", &res);
        assert_eq!(v["status"], serde_json::json!("ok"));
        assert_eq!(v["alreadyClaimed"], serde_json::json!(true));
        assert_eq!(
            v["message"],
            serde_json::json!("该套餐此前已领取过（不是错误）")
        );
    }

    #[test]
    fn claim_plan_json_keeps_claimed_at_zero_when_absent() {
        // Go 的 `ClaimResult.ClaimedAt` 是 `int64` 且**无** omitempty ⇒ 恒出现。
        // 1003 那条路径上游不给时间，落成 0 而不是缺字段。
        let res = ClaimResult {
            ok: true,
            code: "1003".to_string(),
            msg: String::new(),
            already_claimed: true,
            claimed_at: 0,
        };
        let v = claim_plan_json("u", "p", &res);
        assert_eq!(v["claimedAt"], serde_json::json!(0));
        assert!(v.as_object().unwrap().contains_key("claimedAt"));
    }
}
