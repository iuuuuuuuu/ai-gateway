//! CLI 任务模式：`<exe> --task-run <key>` 执行后退出。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/tasks/mod.rs` 的 `run_cli_task`。
//!
//! ## 为什么放在 core 而不是 src-tauri
//!
//! 任务模式**刻意不启动 Tauri**（见 `main.rs` 的分流注释）：单实例插件会把
//! 计划任务触发当成「用户又开了一个实例」而立刻退出，导致定时任务静默不执行。
//! 既然不碰 Tauri，逻辑就属于 core —— server 形态与 CLI 也能复用同一套任务。
//!
//! ## 输出契约
//!
//! stdout 最后一行是 JSON（成功为任务结果，失败为 `{"ok":false,"error":...}`）。
//! 便于计划任务日志排查，也便于将来做双轨对照。

use serde_json::json;

/// 已知任务键。
pub const TASK_KEYS: [&str; 5] = [
    "trae-checkin",
    "trae-credits-snapshot",
    "doubao-keepalive",
    "doubao-renew",
    "doubao-quota",
];

/// 执行一个 CLI 任务，返回进程退出码。
pub fn run_cli_task(name: &str) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            println!("{}", json!({"ok": false, "error": format!("运行时初始化失败: {e}")}));
            return 1;
        }
    };

    let result = runtime.block_on(async { dispatch(name).await });

    match result {
        Ok(value) => {
            println!("{}", serde_json::to_string(&value).unwrap_or_default());
            0
        }
        Err(e) => {
            println!("{}", json!({"ok": false, "error": e}));
            1
        }
    }
}

/// 按任务键分派。
async fn dispatch(name: &str) -> Result<serde_json::Value, String> {
    match name {
        // Trae 每日签到：跑一轮全部账号
        "trae-checkin" => {
            let accounts = crate::modules::trae_account::load_accounts();
            if accounts.is_empty() {
                return Ok(json!({"ok": true, "skipped": true, "reason": "没有 Trae 账号"}));
            }
            let summary = crate::modules::trae_checkin::run_round(&accounts, 1).await;
            let value = summary.to_json();
            // 按日落库：没有这一步，计划任务跑出来的签到结果只存在于返回值里，
            // 界面上的「今日签到状态」和成功率趋势永远看不到（`skip` 也记，
            // 与 apps_ops::trae_checkin_run 同规则：跳过要让用户看见原因）。
            crate::modules::trae_checkin_results::record_today(
                summary
                    .outcomes
                    .iter()
                    .map(|o| (o.user_id.clone(), o.name.clone(), o.status.to_string())),
            );
            // 统一事件流：计划任务跑出来的结果同样要能在账号卡片上查到
            // （与 webui/桌面端共用同一个 helper，口径不分叉；skip 也记）。
            crate::modules::apps_ops::record_checkin_task_records(&summary);
            Ok(value)
        }
        // 豆包会话保活：启动客户端 → 等待 → 关闭，触发服务端滑动续期
        "doubao-keepalive" => {
            use crate::modules::app_profile::TargetApp;
            use crate::modules::switcher::{self, Action, RunArgs};
            // 账号池为空 ⇒ 没有任何会话需要保活（保活的作用对象就是池里那些
            // sessionid），直接跳过。与上面 trae-checkin 的空池短路同规则。
            //
            // 必须**成功**短路：否则应用内调度（`apps_ops::run_in_app_due_tasks`
            // 每 5 分钟一轮）会整天重试这个永远不会变的条件，把
            // `doubao_health.json` 刷满同一条失败事件（HISTORY_MAX = 400，
            // 一天 288 次就够把历史冲干净）。跳过不记健康事件：健康史统计的是
            // 「跑没跑成」，没跑就计一次成败会让成功率虚高。
            if crate::modules::doubao_account::load_accounts().is_empty() {
                return Ok(json!({"ok": true, "skipped": true, "reason": "没有豆包账号"}));
            }
            let args = RunArgs {
                action: Action::KeepAlive,
                target_app: TargetApp::Doubao,
                user_id: None,
                proxy_port: None,
                include_indexeddb: false,
                expected_current_uid: String::new(),
                store_dir: crate::modules::config::store_dir(),
            };
            let outcome = switcher::run_action(args, &switcher::LogSink);
            // 时间戳只在成功时写：它是「上次保活**成功**的时间点」，失败也写会让
            // 界面显示一个从未成功过的时刻（与 `apps_ops::doubao_keepalive`、
            // `commands_apps::doubao_keepalive` 两个入口同规则）。
            if outcome.is_ok() {
                let _ = crate::modules::doubao_account::set_last_keepalive(
                    &crate::modules::config::utc_iso(),
                );
            }
            // 健康史无论成败都记：保活是静默动作，不记的话用户看不到它跑没跑成。
            let _ = crate::modules::doubao_health::record_keepalive(
                outcome.is_ok(),
                outcome.as_deref().unwrap_or("保活失败"),
            );
            // 退出码必须如实反映成败：`run_in_app_due_tasks` 只对退出码 0 记账
            // （那里的注释：「只有成功才记账：失败要留给下一轮重试，否则一次
            // 网络抖动就等于当天不再尝试」）。这里恒返 `Ok` 会让一次网络抖动
            // 直接吃掉当天的全部重试机会 —— 业主机上真实发生过。
            match outcome {
                Ok(message) => Ok(json!({"ok": true, "message": message})),
                Err(e) => Err(e),
            }
        }
        // 豆包 HTTP 续期巡检（不启动客户端，只跑凭证续期请求）
        "doubao-renew" => {
            let summary = crate::modules::doubao_session::run_renewal(false).await;
            let value = summary.to_json();
            let _ = crate::modules::doubao_health::record_renew(&value);
            Ok(value)
        }
        // 豆包额度巡检：批量查询并回写额度缓存
        "doubao-quota" => Ok(crate::modules::doubao_quota::run_batch().await),
        // Trae 积分日快照：为趋势图留数据，不依赖界面是否打开
        "trae-credits-snapshot" => {
            let accounts = crate::modules::trae_account::load_accounts();
            if accounts.is_empty() {
                return Ok(json!({"ok": true, "skipped": true, "reason": "没有 Trae 账号"}));
            }
            let mut snapshotted = 0usize;
            let mut errors: Vec<String> = Vec::new();
            for acc in &accounts {
                let Some(uid) = acc.get("user_id").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                match crate::modules::trae_credits::fetch_credit_total(uid).await {
                    Ok(Some(total)) => {
                        // 写快照失败不该让整个任务失败：趋势图少一个点
                        // 比整轮巡检报错更可接受
                        let _ = crate::modules::trae_credits::daily_snapshot(uid, total, 0);
                        snapshotted += 1;
                    }
                    Ok(None) => errors.push(format!("{uid}: 服务端未返回积分总数")),
                    Err(e) => errors.push(format!("{uid}: {e}")),
                }
            }
            Ok(json!({
                "ok": true,
                "snapshotted": snapshotted,
                "total": accounts.len(),
                "errors": errors,
            }))
        }
        other => Err(format!(
            "未知任务: {other}（可用：{}）",
            TASK_KEYS.join(" / ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本文件源码（守卫测试用）。
    ///
    /// 为什么用「读源码」而不是行为测试：`real_app_control_blocked()` 在
    /// `#[cfg(test)]` 下恒为真（见 `switcher::proc`），单元测试里任何动作都
    /// 不可能真的成功启动客户端，也就构造不出「保活失败但代码路径不同」的对照。
    /// 而这里要守的恰恰是**失败路径的返回值形状**，所以直接钉住源码。
    fn source() -> String {
        include_str!("cli_task.rs").replace("\r\n", "\n")
    }

    /// 取 `dispatch` 里某个分支的源码（从 `"<key>" => {` 到该分支的收尾 `}`）。
    fn branch_of(key: &str) -> String {
        let src = source();
        let head = format!("        \"{key}\" => {{");
        let start = src.find(&head).expect("分支必须存在");
        let rest = &src[start + head.len()..];
        let end = rest.find("\n        }").expect("分支必须有收尾括号");
        rest[..end].to_string()
    }

    #[test]
    fn 未知任务返回非零并列出可用任务() {
        let code = run_cli_task("no-such-task");
        assert_eq!(code, 1, "未知任务必须返回非零退出码");
    }

    #[test]
    fn 任务键与计划任务模块一一对应() {
        use crate::modules::scheduler::TaskKind;
        for kind in TaskKind::ALL {
            assert!(
                TASK_KEYS.contains(&kind.cli_key()),
                "计划任务 {} 的 CLI 键 {} 未在 TASK_KEYS 中登记",
                kind.task_name(),
                kind.cli_key()
            );
        }
    }

    #[test]
    fn 保活失败必须让退出码非零() {
        let body = branch_of("doubao-keepalive");
        assert!(
            body.contains("Err(e) => Err(e)"),
            "保活失败必须原样上抛 Err（退出码才会非零）。恒返 Ok 会让应用内调度\
             把一次失败记成「当天已完成」而不再重试；实际分支源码：\n{body}"
        );
        assert!(
            !body.contains("Ok(json!({\"ok\": true, \"message\": outcome}))"),
            "保活结果不得被无条件包装成 Ok —— 那正是「失败也退出码 0」的成因"
        );
    }

    #[test]
    fn 保活只在成功时刷新时间戳() {
        let body = branch_of("doubao-keepalive");
        assert!(
            body.contains("if outcome.is_ok() {"),
            "set_last_keepalive 必须包在 outcome.is_ok() 里：这个时间戳的语义是\
             「上次保活成功的时间」，失败也写会让界面显示一个从未成功过的时刻"
        );
    }

    #[test]
    fn 保活没有账号时跳过且不记健康事件() {
        let _iso =
            crate::modules::config::test_isolation::Isolated::new("cli_task_keepalive_skip");
        let code = run_cli_task("doubao-keepalive");
        assert_eq!(code, 0, "没有账号属于「无事可做」，不是失败");
        assert!(
            crate::modules::doubao_health::load().is_empty(),
            "跳过不得写健康事件：健康史统计的是「跑没跑成」，没跑就计一次成败会让成功率虚高"
        );
        assert!(
            crate::modules::doubao_account::last_keepalive_at().is_none(),
            "跳过不是保活成功，不得刷新时间戳"
        );
    }

    #[test]
    fn 保活失败时退出码非零且不刷新时间戳() {
        let _iso =
            crate::modules::config::test_isolation::Isolated::new("cli_task_keepalive_fail");
        crate::modules::doubao_account::save_pool(&json!({
            "accounts": [{"user_id": "u1", "name": "测试账号"}],
            "last_keepalive_at": null
        }))
        .expect("写账号池");
        let code = run_cli_task("doubao-keepalive");
        assert_eq!(
            code, 1,
            "启动不了客户端必须返回非零：应用内调度只对退出码 0 记账，恒返 0 会让\
             一次网络抖动吃掉当天的全部重试机会"
        );
        assert!(
            crate::modules::doubao_account::last_keepalive_at().is_none(),
            "失败的尝试不得写「上次保活时间」"
        );
        let events = crate::modules::doubao_health::load();
        let last = events.last().cloned().unwrap_or(serde_json::Value::Null);
        assert_eq!(
            last.get("ok").and_then(serde_json::Value::as_bool),
            Some(false),
            "失败必须记一条健康事件（保活是静默动作，不记用户看不到）"
        );
    }
}
