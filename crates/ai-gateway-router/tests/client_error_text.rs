//! 线级验证：`ErrKind::Client` 的文案在**真实链接的库**里是什么样。
//!
//! 与 `classify.rs` 里的单测互补 —— 那些测的是函数内部；这条测的是
//! **外部调用方能拿到什么**，并且跑在独立的 integration 二进制里，
//! 证明这些函数确实是 `pub` 的、跨 crate 边界仍可用（单测在同一 crate 内，
//! 即使误标成 `pub(crate)` 也照样能过）。
//!
//! 守的核心事实：`Client` 是上游 4xx 的**兜底分类**，它必须说「请求有问题」，
//! 而不是「账号池挂了」—— 后者会把用户引向重启/重登/换密钥这三条无效路径。

use ai_gateway_router::upstream::classify::{friendly_message, ErrKind};

#[test]
fn client_error_is_never_reported_as_account_outage() {
    let msg = friendly_message(ErrKind::Client, 400, "");
    assert!(!msg.is_empty(), "Client 不得落进空串兜底（那会让调用方报「账号全挂」）");
    assert!(
        !msg.contains("all accounts unavailable"),
        "不得复用账号池不可用的措辞：{msg}"
    );
    assert!(msg.contains("请求"), "必须指向请求侧：{msg}");
    assert!(
        msg.contains("换账号") && msg.contains("等待"),
        "必须否掉两条无效出路：{msg}"
    );
}

#[test]
fn client_error_keeps_original_but_scrubs_auth_codes() {
    // 真实形态：上游 4xx 响应体里常直接带 "code":"403"
    let body = r#"{"error":{"code":"403","message":"forbidden by upstream"}}"#;
    let msg = friendly_message(ErrKind::Client, 403, body);

    // 原文要留下（排查线索）
    assert!(msg.contains("forbidden by upstream"), "原文必须保留：{msg}");
    // 但裸 401/403 必须已被就地改写，否则 DSH 会判成「密钥无效」
    let scrubbed = ai_gateway_router::clienttext::client_safe_message(&msg);
    assert_eq!(
        scrubbed, msg,
        "文案里仍有裸 401/403，DSH 会判成 AUTH：{msg}"
    );
}

#[test]
fn other_kinds_still_say_what_to_do() {
    // 回归护栏：补 Client 分支不该影响既有文案
    assert_eq!(friendly_message(ErrKind::None, 200, ""), "");
    assert!(friendly_message(ErrKind::SoftRate, 429, "").contains("429"));
    assert!(friendly_message(ErrKind::Server, 502, "").contains("502"));
    assert!(friendly_message(ErrKind::SessionDead, 401, "").contains("重新登录"));
    assert!(friendly_message(ErrKind::HardCredit, 402, "").contains("充值"));
}
