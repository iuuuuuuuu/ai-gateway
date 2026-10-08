//! 网关日志的逐行时间戳。
//!
//! # 为什么需要它（业主现场，2026-10-08）
//!
//! 网关日志此前**每一行都没有时间**：文件里只有正文，没有 `[2026-10-08 …]`
//! 这类前缀。于是所有跨源对齐都做不到 ——
//!
//!   · 客户端插件（billion-context）的日志有时间戳，网关日志没有；
//!   · 排查「断流为什么发生」时，唯一能把两边对上的办法是拿**字节数 / 帧数**
//!     去猜，而不是比时间；
//!   · 每日维护窗口（本地 00:00 杀旧 sidecar、起新 sidecar）与断流的毫秒级
//!     关系，正是靠插件侧的时间戳才发现的，网关侧完全对不上。
//!
//! 根因不是忘了打，而是**宿主把子进程的 stdout/stderr 直接重定向进文件**
//! （`ai-gateway-core` 的 `open_router_log()` + `cmd.stdout(out).stderr(err)`），
//! 没有经过任何带时间戳的日志框架 —— `eprintln!` 写什么，文件里就是什么。
//!
//! # 为什么用宏而不是换日志库
//!
//! 换 `tracing` / `log` 要动 200+ 处调用点与 Cargo 依赖，而宿主对 stdout
//! 有**结构化依赖**：`qoder-login` / `zcode-login` 等子命令把 JSON 写 stdout，
//! 宿主 `serde_json::from_str(stdout.trim())` 直接解析（见
//! `ai-gateway-core/src/modules/qoder_login.rs`）。给 stdout 加前缀会**直接**
//! 把那些子命令打坏。
//!
//! 故本模块只提供一条**加前缀的 stderr 通道**：
//!
//!   · `[stamp]` —— 本地时间戳，格式 `2026-10-08 14:41:32.123 +08:00`
//!   · `[crate::logline!]` —— 把 `eprintln!` 换成它即得到带时间戳的一行
//!
//! **stdout 一律不动**（`cli.rs` 的子命令 JSON 输出仍是裸 `println!`）。

use std::sync::OnceLock;

/// 进程启动时固定下来的时区偏移文案（`+08:00`）。
///
/// 每次调用都查一次时区是浪费，而且同一进程内不可能变（用户改系统时区
/// 不会影响已运行进程的 `Local` 语义）。启动时取一次即可。
fn offset_text() -> &'static str {
    static OFFSET: OnceLock<String> = OnceLock::new();
    OFFSET.get_or_init(|| {
        // `%:z` 是 `+08:00` 形态（带冒号）；不带冒号的 `%z` 在跨时区排查时
        // 容易被误读，故取带冒号的。
        chrono::Local::now().format("%:z").to_string()
    })
}

/// 一行日志的时间戳前缀：`2026-10-08 14:41:32.123 +08:00`。
///
/// 毫秒是**必须**的：断流事件在同一秒内成簇出现
/// （实测三条同族事件相隔 129 ms / 770 ms），秒级粒度无法区分先后。
pub fn stamp() -> String {
    format!(
        "{} {}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        offset_text()
    )
}

/// 带时间戳的 stderr 日志行（等价于 `eprintln!`，但每行前面多一个时间戳）。
///
/// 用法与 `eprintln!` 逐字相同：
///
/// ```ignore
/// logline!("chat uid={uid}: 上游流读取失败：{e}");
/// ```
///
/// ⚠ **不要**在 `cli.rs` 的 stdout 输出上用它 —— 那里的字节会被宿主当 JSON 解析。
#[macro_export]
macro_rules! logline {
    () => {
        eprintln!("{}", $crate::logging::stamp())
    };
    ($($arg:tt)*) => {
        eprintln!("{} {}", $crate::logging::stamp(), format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 时间戳必须同时含日期、毫秒与时区偏移 —— 少任何一样都无法与
    /// 插件侧日志（`2026-10-08T05:37:42.945Z`）对齐。
    #[test]
    fn stamp_has_date_millis_and_offset() {
        let s = stamp();
        // 形如 "2026-10-08 14:50:59.516 +08:00" ⇒ 30 字符
        //（日期 10 + 空格 1 + 时间 12 + 空格 1 + 偏移 6）。
        assert_eq!(s.len(), 30, "时间戳长度不符：{s:?}");
        assert_eq!(&s[4..5], "-", "缺日期分隔：{s:?}");
        assert_eq!(&s[10..11], " ", "日期与时间之间必须是空格：{s:?}");
        assert_eq!(&s[13..14], ":", "缺时间分隔：{s:?}");
        assert_eq!(&s[19..20], ".", "缺毫秒分隔：{s:?}");
        assert_eq!(&s[23..24], " ", "毫秒与时区之间必须是空格：{s:?}");
        let off = &s[24..];
        assert!(
            off.starts_with('+') || off.starts_with('-'),
            "时区偏移必须以正负号开头：{s:?}"
        );
        assert_eq!(off.len(), 6, "时区偏移应为 ±HH:MM：{s:?}");
        assert_eq!(&off[3..4], ":", "时区偏移应带冒号：{s:?}");
        assert!(s[..4].chars().all(|c| c.is_ascii_digit()), "{s:?}");
        assert!(s[20..23].chars().all(|c| c.is_ascii_digit()), "{s:?}");
        assert!(
            off[1..3].chars().all(|c| c.is_ascii_digit())
                && off[4..6].chars().all(|c| c.is_ascii_digit()),
            "{s:?}"
        );
    }

    /// 同一进程内时区偏移必须恒定（`OnceLock` 的语义保证）。
    #[test]
    fn offset_is_stable() {
        assert_eq!(offset_text(), offset_text());
    }

    /// 连续两次取值必须单调不减 —— 用它排事件顺序时，时钟回拨会让对齐失效。
    #[test]
    fn stamp_is_non_decreasing() {
        let a = stamp();
        let b = stamp();
        assert!(b >= a, "时间戳出现回退：{a:?} -> {b:?}");
    }
}
