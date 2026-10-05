//! 夜猫窗口判定（对应 Go `internal/scheduler/nightowl.go` 的时间判定部分）。
//!
//! 夜猫子任务在夜猫时段（23:00–08:00 CST）补一次 `black_cat` 任务。
//!
//! # 与活跃上报的关系
//!
//! 两者都发 `chat_request_send` 事件，形状完全相同，只是**触发时机**与**条数**
//! 不同 —— 活跃上报每天 10 点发若干条点亮连登；夜猫子只在窗口内补 1 条以点亮
//! `black_cat`。因此复用同一个上报方法，不重复实现事件构造。

use chrono::{DateTime, FixedOffset, Timelike, Utc};

/// 夜猫窗口起点小时（CST）。
pub const NIGHT_WINDOW_START_HOUR: u32 = 23;
/// 夜猫窗口终点小时（CST）。
pub const NIGHT_WINDOW_END_HOUR: u32 = 8;
/// CST = UTC+8（固定偏移）。
pub const CST_OFFSET_SECONDS: i32 = 8 * 3600;

/// 当前 CST 时间。
///
/// ★ 不用 `Asia/Shanghai` 时区名：Windows 上若无 tzdata，按名字查时区会失败，
/// 而固定偏移已足够 —— 中国自 1991 年起不再使用夏令时。
pub fn cst_now() -> DateTime<FixedOffset> {
    Utc::now().with_timezone(&cst_offset())
}

fn cst_offset() -> FixedOffset {
    FixedOffset::east_opt(CST_OFFSET_SECONDS).expect("+8h is a valid offset")
}

/// 当前是否处于夜猫窗口。
pub fn within_night_window() -> bool {
    within_night_window_at(cst_now())
}

/// 给定的 CST 时刻是否处于夜猫窗口。
///
/// 抽成带参函数是为了**可测**：直接用 `now()` 的版本只能在恰好处于边界的时刻
/// 验证，测试若自己重抄一遍判定逻辑就成了假测试。
pub fn within_night_window_at(cst: DateTime<FixedOffset>) -> bool {
    let h = cst.hour();
    h >= NIGHT_WINDOW_START_HOUR || h < NIGHT_WINDOW_END_HOUR
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> DateTime<FixedOffset> {
        cst_offset()
            .with_ymd_and_hms(2026, 9, 20, h, m, 0)
            .single()
            .expect("valid cst time")
    }

    /// 窗口边界：23:00 起、08:00 止（含起点、不含终点）。
    #[test]
    fn window_covers_23_to_8() {
        // 窗口内
        assert!(within_night_window_at(at(23, 0)), "23:00 应算窗口内");
        assert!(within_night_window_at(at(23, 59)), "23:59 应算窗口内");
        assert!(within_night_window_at(at(0, 0)), "00:00 应算窗口内");
        assert!(within_night_window_at(at(3, 30)), "03:30 应算窗口内");
        assert!(within_night_window_at(at(7, 59)), "07:59 应算窗口内");
        // 窗口外
        assert!(!within_night_window_at(at(8, 0)), "08:00 是窗口终点，应算窗口外");
        assert!(!within_night_window_at(at(12, 0)), "12:00 应算窗口外");
        assert!(!within_night_window_at(at(22, 59)), "22:59 应算窗口外");
    }

    /// 默认排程时点 01:00 必须落在窗口内 —— 否则夜猫子任务永远不会真正上报，
    /// 而它正是「补 black_cat」的唯一动作。
    #[test]
    fn default_schedule_hour_is_inside_the_window() {
        assert!(
            within_night_window_at(at(1, 0)),
            "默认夜猫子时点 01:00 必须落在窗口内"
        );
    }

    /// 窗口是**跨日**的（23 点与 0 点在同一窗口里），不是简单的 `8 <= h < 23` 取反。
    /// 这条钉住的是「边界两侧同属一个窗口」的语义，防止有人把它简写成区间判断。
    #[test]
    fn window_spans_midnight() {
        let before = at(23, 30);
        let after = at(0, 30);
        assert!(within_night_window_at(before));
        assert!(within_night_window_at(after));
        // 两者在同一天日期上相差近 23 小时，但同属一个窗口
        assert!(after < before, "同日 00:30 早于 23:30（窗口跨过午夜）");
    }

    /// CST 偏移恒为 +8（不随夏令时变化）。
    #[test]
    fn cst_offset_is_fixed_plus_8() {
        assert_eq!(cst_offset().local_minus_utc(), 8 * 3600);
    }
}
