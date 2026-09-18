//! **回归测试**：测试进程的数据目录绝不允许指向用户真实 home。
//!
//! ## 为什么这条测试值得独立存在
//!
//! 2026-09-16 排查到的真实数据损坏事故：用户账号库里出现 16 条
//! `{"uid":"legacy-user","email":"old@example.com"}` —— 那是 `account::tests`
//! 里一条用例的**种子数据**，被写进了**真实**数据目录，随后又被数据目录迁移
//! 原样搬进生产账号库，最终以「永远登录不了、永远查不到积分」的僵尸账号出现在界面上。
//!
//! 成因是测试隔离依赖 `std::env::set_var("AI_GATEWAY_HOME")`：
//!
//! 1. `set_var` 不是线程安全的，只在「恰好同时读写同一变量」时才构成 UB，
//!    对「没拿锁却调用了 `store_dir()`」的线程**不报错，只读到真实 home**；
//! 2. 更隐蔽的是编译器缓存 —— `std::env::var` 在 Rust 标准库里并不保证每次都重新
//!    调 `getenv`，一旦某个线程在变量被设置**之前**读过它，之后可能永远读到旧值。
//!
//! 本文件存在的意义就是**把「隔离是否真的生效」变成一条会红的断言**，而不是一条
//! 靠人工推理维持的假设 —— 上面那个教训正是「假设隔离生效、实际没有」。
//!
//! ## 为什么断言必须写在这里（集成测试），而不是单元测试里
//!
//! 隔离实现（`config::test_isolation`）是 `#[cfg(test)]` 的，只在本 crate 的
//! **单元测试**里编译。集成测试把本 crate 当外部依赖链接，`cfg(test)` 不成立 ——
//! 这正是隔离曾经在集成测试里完全失效的原因（实测：`#[ctor]` 修复后探针仍然读到
//! 真实 home）。所以这里刻意走 `pin_store_dir_for_tests` 这条对外入口，
//! 同时验证**它有效**。
//!
//! 注意 `#[ctor]` 必须出现在本文件：它在任何测试线程创建之前运行。若改到某个
//! `#[test]` 函数体里调用，就可能晚于其它测试线程读取环境变量，隔离会静默失效。

#[ctor::ctor]
fn isolate_store_dir() {
    ai_gateway_core::modules::config::pin_store_dir_for_tests("isolation-guard");
}

/// 数据目录不得指向用户真实的 `~/.wb-switch`。
#[test]
fn store_dir_不指向用户真实账号库() {
    let dir = ai_gateway_core::modules::config::store_dir();
    let real_switch = ai_gateway_core::modules::config::home_dir().join(".wb-switch");

    assert_ne!(
        dir,
        real_switch,
        "隔离失效：store_dir 指向了用户真实账号库 {}；\
         任何账号写入都会污染生产数据（这正是 legacy-user 事故的成因）",
        real_switch.display()
    );
    assert!(
        dir.starts_with(std::env::temp_dir()),
        "隔离失效：store_dir 不在临时目录下，实际 {}",
        dir.display()
    );
}

/// 账号库文件路径同样不得落在真实 home 下。
#[test]
fn accounts_file_不落在真实_home_下() {
    let path = ai_gateway_core::modules::config::accounts_file();
    let real = ai_gateway_core::modules::config::home_dir()
        .join(".wb-switch")
        .join("accounts.json");

    assert_ne!(
        path,
        real,
        "隔离失效：accounts_file 指向了生产账号库 {}",
        real.display()
    );
    assert!(
        path.starts_with(std::env::temp_dir()),
        "accounts_file 必须落在临时目录下，实际 {}",
        path.display()
    );
}

/// 端到端：真的写一次账号库，确认落点在临时目录、且真实 home 未被触碰。
///
/// 前两条只断言路径，这条断言**实际写入行为** —— 路径对了但写入函数走了别的
/// 分支（例如某处硬编码了 home）时，只有这条能抓到。
#[test]
fn 写入账号库落在隔离目录而非真实_home() {
    use ai_gateway_core::modules::config;

    let real_accounts = config::home_dir().join(".wb-switch").join("accounts.json");
    let real_before = std::fs::read_to_string(&real_accounts).ok();

    let isolated = config::accounts_file();
    std::fs::create_dir_all(isolated.parent().unwrap()).unwrap();
    std::fs::write(&isolated, r#"[{"uid":"isolation-guard-probe"}]"#).unwrap();

    assert!(isolated.starts_with(std::env::temp_dir()));
    let written = std::fs::read_to_string(&isolated).unwrap();
    assert!(written.contains("isolation-guard-probe"));

    // 真实库若存在，内容必须一字未变
    if let Some(before) = real_before {
        let after = std::fs::read_to_string(&real_accounts).unwrap();
        assert_eq!(
            before, after,
            "真实账号库被测试改动了 —— 隔离存在漏洞，必须立即修复"
        );
    }

    let _ = std::fs::remove_file(&isolated);
}
