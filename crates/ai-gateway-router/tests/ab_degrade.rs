//! 线级验证：连败降权在**真实链接的库**里是什么行为。
//!
//! 与 `pool.rs` 里的单测互补 —— 那些测的是函数内部；这条测的是
//! **外部调用方能拿到什么**，并且跑在独立的 integration 二进制里，
//! 证明这套 API 确实是 `pub` 的、跨 crate 边界仍可用
//! （单测在同一 crate 内，即使误标成 `pub(crate)` 也照样能过）。
//!
//! 守的核心事实：`Client`（未知 4xx 的兜底分类）与传输层失败这两类
//! **没有权威分类**的失败，过去「只换号不罚」⇒ 账号永不冷却、每轮被重选。
//! 连败降权给它们一个**连败兜底**：攒够阈值才临时出池。
//!
//! ⚠ 判据陷阱（写下来免得后人踩）：
//! - `consecutive_fails` 达阈后是 **0**（达阈即清零供下轮累计），
//!   所以**不能用它判断是否降权**，要看 `degrade_until` / `degraded()`。
//! - `Status.cooling` 是「此刻不接客」的投影，把降权也算进去了 ——
//!   判断「这是不是降权」要看 `cool_kind == "degrade"`。

use std::time::Duration;

use ai_gateway_router::auth::Auth;
use ai_gateway_router::pool::Pool;

/// 造一个最小可用账号。
fn account(uid: &str) -> Auth {
    Auth {
        uid: uid.to_string(),
        access_token: format!("at-{uid}"),
        refresh_token: format!("rt-{uid}"),
        // 未来时刻：避免测试路径去触发 token refresh。
        expires_at: 2_000_000_000,
        domain: "https://copilot.tencent.com".to_string(),
        product: String::new(),
        ..Default::default()
    }
}

#[test]
fn single_failure_never_degrades_but_accumulates() {
    let mut pool = Pool::new(String::new());
    pool.add(account("u1"));
    pool.set_degrade(3, Duration::from_secs(600));

    pool.note_failures("u1");

    assert_eq!(pool.consecutive_fails("u1"), 1, "单次失败必须计数（否则机制不生效）");
    assert!(!pool.degraded("u1"), "单次失败绝不能降权（防雪崩）");
    assert!(pool.degrade_until("u1").is_none());
}

#[test]
fn reaching_the_threshold_degrades_and_clears_the_counter() {
    let mut pool = Pool::new(String::new());
    pool.add(account("u1"));
    pool.set_degrade(3, Duration::from_secs(600));

    for _ in 0..3 {
        pool.note_failures("u1");
    }

    assert!(pool.degraded("u1"), "达阈必须降权");
    assert!(
        pool.degrade_until("u1").is_some(),
        "降权必须有截止时刻（判据看它，不看计数）"
    );
    assert_eq!(
        pool.consecutive_fails("u1"),
        0,
        "达阈后计数清零供下一轮累计 —— 0 不代表没降权"
    );
}

#[test]
fn a_degraded_account_is_excluded_from_selection() {
    let mut pool = Pool::new(String::new());
    pool.add(account("u1"));
    pool.set_degrade(2, Duration::from_secs(600));

    assert!(!pool.available_uids().is_empty(), "降权前应可选");
    assert!(pool.degraded("u1") == false, "降权前 degraded 应为 false");

    pool.note_failures("u1");
    pool.note_failures("u1");

    assert!(pool.degraded("u1"), "达阈后 degraded 必须为 true");
    assert!(
        !pool.available_uids().contains(&"u1".to_string()),
        "降权后必须被排除出可选集合：{:?}",
        pool.available_uids()
    );
}

/// 降权必须落进 `healthy()` 这条**唯一选号闸门**，而不是只体现在查询函数上。
///
/// ★ 这条是**补强**来的：故障注入「让 `degraded()` 恒返回 false」时，
/// 其余用例全绿、只有一条红 —— 说明「查询函数说降权」与「选号闸门真的排除」
/// 是两件事。`servable_now()` 是 `healthy()` 的对外投影，用它钉住后者。
#[test]
fn degrade_reaches_the_servable_gate_not_just_a_query() {
    let mut pool = Pool::new(String::new());
    pool.add(account("u1"));
    pool.set_degrade(2, Duration::from_secs(600));

    let before = pool.servable_now();
    assert!(before, "降权前 servable_now 应为 true");

    pool.note_failures("u1");
    pool.note_failures("u1");

    assert!(
        !pool.servable_now(),
        "降权必须让 servable_now() 变假 —— 它才是选号闸门的投影；\
         若这里仍为 true，说明判据没接进 healthy()"
    );
}

#[test]
fn success_clears_the_counter_and_returns_to_pool() {
    let mut pool = Pool::new(String::new());
    pool.add(account("u1"));
    pool.set_degrade(3, Duration::from_secs(600));

    // 攒到差一次就达阈
    pool.note_failures("u1");
    pool.note_failures("u1");
    assert_eq!(pool.consecutive_fails("u1"), 2);

    // 一次成功必须清零（`note_success` 内部收口，调用方不必另行 clear）
    pool.note_success("u1");
    assert_eq!(pool.consecutive_fails("u1"), 0, "成功必须清零连败计数");

    // 再攒同样次数仍不达阈 ⇒ 证明清零真的发生
    pool.note_failures("u1");
    pool.note_failures("u1");
    assert!(!pool.degraded("u1"), "清零后重新计数，不该达阈");
}

#[test]
fn degrade_does_not_extend_while_already_degraded() {
    let mut pool = Pool::new(String::new());
    pool.add(account("u1"));
    pool.set_degrade(2, Duration::from_secs(600));

    pool.note_failures("u1");
    pool.note_failures("u1");
    let first = pool.degrade_until("u1").expect("应已降权");

    // 降权期内再攒够一轮
    pool.note_failures("u1");
    pool.note_failures("u1");
    let second = pool.degrade_until("u1").expect("仍在降权");

    assert_eq!(
        first, second,
        "降权期内再达阈不得延长（防用户重试把降权越堆越厚）"
    );
}

#[test]
fn threshold_zero_disables_the_mechanism() {
    let mut pool = Pool::new(String::new());
    pool.add(account("u1"));
    pool.set_degrade(0, Duration::from_secs(600));

    for _ in 0..20 {
        pool.note_failures("u1");
    }

    assert!(
        !pool.degraded("u1"),
        "threshold=0 必须彻底关闭降权（不能有 max(1,..) 式保护把它变回开启）"
    );
}

#[test]
fn unknown_uid_is_safe() {
    let mut pool = Pool::new(String::new());
    pool.set_degrade(1, Duration::from_secs(60));

    // 不该 panic
    pool.note_failures("nobody");
    pool.clear_degrade("nobody");
    assert!(!pool.degraded("nobody"));
    assert_eq!(pool.consecutive_fails("nobody"), 0);
}

#[test]
fn degrade_config_is_readable_from_outside() {
    let mut pool = Pool::new(String::new());
    assert_eq!(
        pool.degrade_threshold(),
        ai_gateway_router::pool::DEGRADE_THRESHOLD,
        "未注入时应等于常量缺省"
    );

    pool.set_degrade(7, Duration::from_secs(120));
    assert_eq!(pool.degrade_threshold(), 7, "注入后必须读到注入值");
    assert_ne!(
        pool.degrade_threshold(),
        ai_gateway_router::pool::DEGRADE_THRESHOLD,
        "注入值必须不同于缺省，否则「接线生效」与「接线漏了」无法区分"
    );
}
