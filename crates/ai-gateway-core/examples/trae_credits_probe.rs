//! Trae 积分链路活体探测（**只读**：不写账号库、不写缓存、不写设备文件）。
//!
//! 用途：用**真实凭证**打一遍宿主「Trae 账号详情页」的三条账，验证 2026-10-03
//! 业主报的「三条 HTTP 404」已经修好。三条账分别是：
//!   1. 付费身份  POST /trae/api/v2/pay/ide_user_pay_status
//!   2. 权益包    POST /trae/api/v2/pay/ide_user_ent_usage
//!   3. IDE 总额  （与 2 同一份响应里的 usage_summary）
//!
//! 运行（数据目录指向哪份就探测哪份）：
//!   $env:AI_GATEWAY_HOME = "D:\...\inst-1004\home"
//!   cargo run -p ai-gateway-core --example trae_credits_probe -- 2593120869167228
//!
//! ★ 为什么必须是 example 而不是 #[cfg(test)]：
//!   config.rs 的 pin_test_home() 是 #[ctor]，会在测试二进制里把 AI_GATEWAY_HOME
//!   钉到临时目录 —— 测试里永远读不到真实凭证。example 不是 cfg(test)，ctor 不生效。
//!
//! ★ 本探针只调用 fetch_credit_detail（三个 HTTP 请求 + 三个读文件），
//!   **不调用** refresh_all_pay_status / daily_snapshot 等任何写路径。

use ai_gateway_core::modules::{config, trae_account, trae_credits, trae_device};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let uid = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "2593120869167228".to_string());

    let store = config::store_dir();
    let accounts = trae_account::accounts_file();
    let device = store.join("trae_device.json");
    println!("=== Trae 积分链路活体探测（只读）===");
    println!("store_dir  = {}", store.display());
    println!("账号库     = {} ({})", accounts.display(), exists(&accounts));
    println!("设备身份   = {} ({})", device.display(), exists(&device));
    println!("uid        = {uid}");

    let dev = trae_device::resolve_device(&uid);
    println!("设备条目   = {}", if dev.device_id.is_empty() { "空" } else { "已解析" });
    match trae_account::find_account(&uid) {
        Some(acc) => {
            let realm = trae_account::account_realm(&acc);
            let jwt_len = acc.get("jwt").and_then(|v| v.as_str()).map(str::len).unwrap_or(0);
            println!("realm      = {:?} (jwt {jwt_len} 字节)", realm.realm);
        }
        None => println!("realm      = 账号不在库中"),
    }

    let detail = trae_credits::fetch_credit_detail(&uid).await;
    println!("--- /api/trae/credits/detail 的响应 ---");
    match serde_json::to_string_pretty(&detail) {
        Ok(s) => println!("{s}"),
        Err(e) => println!("(序列化失败: {e})"),
    }

    let errors = detail
        .get("errors")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let packs = detail
        .get("packs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let total = detail.get("total").and_then(|v| v.as_i64());
    let identity = detail.get("payIdentity").and_then(|v| v.as_str());
    let expire = detail.get("payExpireAt").and_then(|v| v.as_str());
    println!("--- 判据 ---");
    println!("errors 条数        = {}", errors.len());
    println!("packs 条数         = {}", packs.len());
    println!("total              = {total:?}");
    println!("payIdentity        = {identity:?}");
    println!("payExpireAt        = {expire:?}");
    if errors.is_empty() && !packs.is_empty() {
        println!("结论               = 通过（三条账都能取到）");
    } else {
        println!("结论               = 未通过");
        std::process::exit(1);
    }
}

fn exists(p: &std::path::Path) -> &'static str {
    if p.exists() {
        "存在"
    } else {
        "缺失"
    }
}
