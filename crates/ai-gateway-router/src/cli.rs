//! 网关 CLI 子命令的公共设施（参数解析、JSON 输出）。
//!
//! # 为什么会有这个模块
//!
//! Go 侧网关有两条入口：
//!
//! 1. **HTTP 服务**（`main()` 的默认路径）—— 由 `-config` 拉起；
//! 2. **CLI 子命令**（`qoder-login` / `zcode-login`）—— 由宿主以
//!    `gateway.exe qoder-login <sub> [flags]` 的子进程方式调用，
//!    用于登录/额度/模型/活动等**配置阶段**的事。
//!
//! 2026-10-01 的 Rust 重写**只移植了第 1 条**：`bin/gateway.rs` 的 `main()`
//! 只解析 `-config` 与位置参数（端口），没有子命令派发。后果不是报错而是
//! **静默挂死** —— 网关把 `qoder-login quota --uid x` 当成「无 config 启动」，
//! 起了一个 HTTP 服务并**永不退出**，于是宿主 `Command::output()` 永久阻塞。
//!
//! 本模块提供子命令侧的公共件，两个产品的 `cli.rs` 都基于它。
//!
//! # 与宿主的退出码契约（★ 容易搞错，务必按此实现）
//!
//! `crates/ai-gateway-core/src/modules/qoder_login.rs::run_login_cmd` 的判据是：
//!
//! ```text
//! out.status.success() 为 false  ⇒  把 stderr（其次 stdout）当错误消息，**丢弃 stdout**
//! out.status.success() 为 true   ⇒  必须能把 stdout 解析成 JSON（serde_json::from_str）
//! ```
//!
//! 由此推出两条硬规则：
//!
//! - **业务失败（查不到额度、上游拒绝）必须返回 0**，把原因放进 JSON 的
//!   `{"status":"error","message":"…"}`。返回非 0 会让宿主丢掉 message，
//!   用户在界面上只看到一句没有原因的空白。Go 侧 `runQuota` 的注释
//!   （`internal/qoder/logincli.go:338-342`）写的就是这个理由。
//! - **参数错误返回 2**，且只写 stderr（此时 stdout 必须是空的 —— 宿主在
//!   非 0 退出时根本不看 stdout，往里写 JSON 只会让排查时误判）。
//!
//! ⚠ 唯一的例外是 `zcode-login warm`：它连「上游说 status != ok」也只写
//! stdout 并返回 0（Go 侧刻意如此，见 `internal/zcode/logincli.go`），
//! 由 Rust 宿主自己判 `status` 字段。别在 CLI 侧替它报错。

use serde_json::Value;

/// 一个子命令的标志位集合，语义**刻意对齐 Go 的 `flag` 包**。
///
/// 对齐的三个点（不对齐会让「宿主传的固定形状」在别处炸掉）：
///
/// 1. `-x` / `--x` / `-x=v` / `-x v` 四种写法都接受；
/// 2. 布尔标志写 `-x` 即为 true，也可 `-x=false`；
/// 3. **遇到第一个非标志参数后停止解析**，其余原样进 `positional`
///    （Go 原文：*Flag parsing stops just before the first non-flag argument*）。
pub struct FlagSet {
    /// 子命令全名，只用于错误消息（Go 侧 `flag.NewFlagSet(name, …)`）。
    name: String,
    /// 字符串标志：名字 + 默认值 + 是否必填。
    strings: Vec<(&'static str, String, bool)>,
    /// 布尔标志：名字 + 默认值。
    bools: Vec<(&'static str, bool)>,
    /// 解析后填入的值（`None` = 用户没给，取默认值）。
    given: Vec<(&'static str, String)>,
    /// 非标志参数（Go 语义：第一个非标志参数之后的所有参数）。
    positional: Vec<String>,
}

impl FlagSet {
    /// 新建。`name` 形如 `"qoder-login quota"`，只出现在错误消息里。
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            strings: Vec::new(),
            bools: Vec::new(),
            given: Vec::new(),
            positional: Vec::new(),
        }
    }

    /// 注册一个字符串标志。`required = true` 时缺省即报错（Go 侧是各
    /// `runXxx` 里手写的 `if strings.TrimSpace(*uid) == ""` 检查）。
    pub fn string(mut self, name: &'static str, default: &str, required: bool) -> Self {
        self.strings.push((name, default.to_string(), required));
        self
    }

    /// 注册一个布尔标志。
    pub fn boolean(mut self, name: &'static str, default: bool) -> Self {
        self.bools.push((name, default));
        self
    }

    /// 解析。错误消息**刻意与 Go 的 `flag` 包逐字一致**，便于对照排查。
    ///
    /// ⚠ 调用方拿到 `Err` 后必须 `return 2`（并且**不要**往 stdout 写 JSON）。
    pub fn parse(&mut self, args: &[String]) -> Result<(), String> {
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            // Go：`-` 单独一个算非标志参数（用于分隔），故这里要求长度 > 1。
            if !a.starts_with('-') || a.len() == 1 {
                // 停止解析，其余全部当位置参数（Go 语义）。
                self.positional.extend(args[i..].iter().cloned());
                break;
            }
            let body = a.trim_start_matches('-');
            // 空串（`--`）在 Go 里也是"停止解析"的信号。
            if body.is_empty() {
                self.positional.extend(args[i + 1..].iter().cloned());
                break;
            }
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };

            if let Some((bname, _)) = self.bools.iter().find(|(n, _)| *n == name).copied() {
                // `-x` 单独出现 = true（Go 的 bool 标志语义）。
                let v = match inline {
                    Some(s) => parse_bool(&s)
                        .ok_or_else(|| format!("invalid boolean value {s:?} for -{name}"))?,
                    None => true,
                };
                self.given.push((bname, v.to_string()));
                i += 1;
                continue;
            }

            if let Some(idx) = self.strings.iter().position(|(n, _, _)| *n == name) {
                let sname = self.strings[idx].0;
                let v = match inline {
                    Some(s) => s,
                    None => {
                        let next = args.get(i + 1).cloned().ok_or_else(|| {
                            format!("flag needs an argument: -{name}")
                        })?;
                        i += 1;
                        next
                    }
                };
                self.given.push((sname, v));
                i += 1;
                continue;
            }

            return Err(format!("flag provided but not defined: -{name}"));
        }

        // 必填校验（相当于 Go 侧各 runXxx 里的手动检查）。
        for (name, _, required) in &self.strings {
            if *required && self.get(name).trim().is_empty() {
                return Err(format!("缺少 --{name} 参数"));
            }
        }
        Ok(())
    }

    /// 取字符串标志（未给则返回注册时的默认值）。
    ///
    /// ★ 同名标志出现多次时**取最后一次** —— Go 的 `flag` 包每遇到一次就
    /// `Set` 一次，故后者覆盖前者。用 `find`（首次）会让
    /// `--uid a --uid b` 取到 `a`，与 Go 分叉。
    pub fn get(&self, name: &str) -> String {
        if let Some((_, v)) = self.given.iter().rev().find(|(n, _)| *n == name) {
            return v.clone();
        }
        self.strings
            .iter()
            .find(|(n, _, _)| *n == name)
            .map(|(_, d, _)| d.clone())
            .unwrap_or_default()
    }

    /// 取布尔标志（同名多次时同样取最后一次，理由见 [`FlagSet::get`]）。
    pub fn flag(&self, name: &str) -> bool {
        if let Some((_, v)) = self.given.iter().rev().find(|(n, _)| *n == name) {
            return parse_bool(v).unwrap_or(false);
        }
        self.bools
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, d)| *d)
            .unwrap_or(false)
    }

    /// 位置参数（Go 语义：第一个非标志参数及其后的全部）。
    pub fn positional(&self) -> &[String] {
        &self.positional
    }

    /// 语法错误时写 stderr 的文案。与 Go `flag` 的 `ContinueOnError` 同型：
    /// 先打错误，再打 usage。
    pub fn fail(&self, err: &str) {
        eprintln!("{err}");
        eprintln!("用法: {} [选项]", self.name);
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

/// 输出单行 JSON 到 stdout（Go 侧 `writeJSON` 的同型）。
///
/// 宿主用 `serde_json::from_str(stdout.trim())` 解析，故**必须单行**且
/// 前后不能有多余文字。
pub fn write_json(v: &Value) {
    match serde_json::to_string(v) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("序列化输出失败: {e}"),
    }
}

/// 业务失败的统一出口：**写 stdout 的 JSON、返回 0**。
///
/// 见模块头「与宿主的退出码契约」——返回非 0 会让宿主丢掉 `message`。
pub fn write_business_error(message: &str) -> i32 {
    write_json(&serde_json::json!({"status": "error", "message": message}));
    0
}

/// 带 `uid` 字段的业务失败出口：**写 stdout 的 JSON、返回 0**。
///
/// 对应 Go 各 `runXxx` 里的
/// `writeJSON(map[string]any{"status": "error", "uid": …, "message": …})`
/// （见 `internal/qoder/logincli.go:357` / `internal/zcode/logincli.go` 同款）。
///
/// 与 [`write_business_error`] 的唯一差别是多一个 `uid`：宿主要靠它把失败
/// 归到具体账号上（`qoder_login.rs` / `zcode_login.rs` 都读这个键）。两者
/// **都必须返回 0** —— 见模块头「与宿主的退出码契约」。
///
/// ⚠ 刻意**不**接受 `Option<&str>`：Go 那边 `uid` 缺失时走的是不带 uid 的
/// `writeJSON`（如「没有可用凭证」这种与账号无关的失败），两条路径在调用点
/// 就该分开，别在这里做分支。
pub fn write_business_error_uid(uid: &str, message: &str) -> i32 {
    write_json(&serde_json::json!({
        "status": "error",
        "uid": uid,
        "message": message,
    }));
    0
}

/// 参数错误的统一出口：**只写 stderr、返回 2**。
pub fn usage_error(err: &str) -> i32 {
    eprintln!("{err}");
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把 `&[&str]` 转成 `parse` 要的 `&[String]`（少写一堆 `.to_string()`）。
    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// 建一个常用的 qoder 型 flagset：`--uid` 必填、`--auth-dir` 有默认、`--verbose` 布尔。
    fn sample() -> FlagSet {
        FlagSet::new("qoder-login quota")
            .string("uid", "", true)
            .string("auth-dir", "/default/auths", false)
            .boolean("verbose", false)
    }

    // ==================== 四种写法（Go flag 兼容）====================

    #[test]
    fn accepts_single_dash_with_separate_value() {
        let mut f = sample();
        f.parse(&args(&["-uid", "u1"])).expect("应解析成功");
        assert_eq!(f.get("uid"), "u1");
    }

    #[test]
    fn accepts_double_dash_with_separate_value() {
        let mut f = sample();
        f.parse(&args(&["--uid", "u1"])).expect("应解析成功");
        assert_eq!(f.get("uid"), "u1");
    }

    #[test]
    fn accepts_double_dash_with_inline_value() {
        let mut f = sample();
        f.parse(&args(&["--uid=u1"])).expect("应解析成功");
        assert_eq!(f.get("uid"), "u1");
    }

    #[test]
    fn accepts_single_dash_with_inline_value() {
        let mut f = sample();
        f.parse(&args(&["-uid=u1"])).expect("应解析成功");
        assert_eq!(f.get("uid"), "u1");
    }

    #[test]
    fn inline_value_keeps_later_equals_signs() {
        // `split_once('=')` 只切第一个 `=` —— token 里含 `=` 时必须完整保留。
        let mut f = sample();
        f.parse(&args(&["--uid=a=b=c"])).expect("应解析成功");
        assert_eq!(f.get("uid"), "a=b=c");
    }

    #[test]
    fn later_flag_overrides_earlier() {
        // Go 的 flag 包「后者覆盖前者」，`given` 只取第一个命中会取错值。
        let mut f = sample();
        f.parse(&args(&["--uid", "first", "--uid", "second"]))
            .expect("应解析成功");
        assert_eq!(f.get("uid"), "second");
    }

    // ==================== 布尔标志 ====================

    #[test]
    fn bare_bool_flag_means_true() {
        let mut f = sample();
        f.parse(&args(&["--uid", "u1", "-verbose"])).expect("应解析成功");
        assert!(f.flag("verbose"));
    }

    #[test]
    fn bool_flag_accepts_explicit_false() {
        // 默认 true + 显式 `-verbose=false` ⇒ false（证明显式值压过默认值）。
        let mut f = FlagSet::new("x")
            .string("uid", "", true)
            .boolean("verbose", true);
        f.parse(&args(&["--uid", "u1", "-verbose=false"]))
            .expect("应解析成功");
        assert!(!f.flag("verbose"));
    }

    #[test]
    fn bare_bool_flag_does_not_consume_next_arg() {
        // ★ 关键：`-verbose --uid u1` 里 `--uid` 不能被当成 verbose 的值。
        let mut f = sample();
        f.parse(&args(&["-verbose", "--uid", "u1"])).expect("应解析成功");
        assert!(f.flag("verbose"));
        assert_eq!(f.get("uid"), "u1");
    }

    #[test]
    fn invalid_bool_value_reports_go_style_error() {
        let mut f = sample();
        let err = f
            .parse(&args(&["--uid", "u1", "-verbose=maybe"]))
            .expect_err("应报错");
        // 与 Go `flag` 的 `invalid boolean value "maybe" for -verbose` 同型。
        assert_eq!(err, "invalid boolean value \"maybe\" for -verbose");
    }

    // ==================== 位置参数（Go 的"首个非标志即停止"）====================

    #[test]
    fn stops_parsing_at_first_non_flag_argument() {
        let mut f = sample();
        f.parse(&args(&["--uid", "u1", "extra", "--verbose"]))
            .expect("应解析成功");
        assert_eq!(f.get("uid"), "u1");
        // `--verbose` 在 `extra` 之后 ⇒ 不解析，整体进 positional。
        assert!(!f.flag("verbose"));
        assert_eq!(f.positional(), args(&["extra", "--verbose"]).as_slice());
    }

    #[test]
    fn lone_dash_is_a_positional_argument() {
        // Go：`-` 单独一个不算标志。若把它当标志，`body` 会是空串并被
        // 误判成 `--`，从而吞掉后面的 `--uid`。
        // ★ 这里 uid 注册成**非必填**（默认空）—— 必填时 `parse` 会因缺参报错，
        // 就没法观察到"`--uid` 被当位置参数吞掉"这一行为了。
        let mut f = FlagSet::new("x")
            .string("uid", "", false)
            .boolean("verbose", false);
        f.parse(&args(&["-", "--uid", "u1"])).expect("应解析成功");
        assert_eq!(f.positional(), args(&["-", "--uid", "u1"]).as_slice());
        // `--uid` 没被解析（落在 positional 里）⇒ 取默认空值，而不是 "u1"。
        assert_eq!(f.get("uid"), "");
    }

    #[test]
    fn double_dash_stops_parsing_and_is_not_positional() {
        let mut f = sample().string("uid", "d", false);
        f.parse(&args(&["--uid", "u1", "--", "--verbose", "tail"]))
            .expect("应解析成功");
        assert_eq!(f.get("uid"), "u1");
        assert!(!f.flag("verbose"));
        // `--` 本身不进 positional（Go 语义）。
        assert_eq!(f.positional(), args(&["--verbose", "tail"]).as_slice());
    }

    #[test]
    fn double_dash_at_end_yields_empty_positional() {
        let mut f = sample();
        f.parse(&args(&["--uid", "u1", "--"])).expect("应解析成功");
        assert!(f.positional().is_empty());
    }

    // ==================== 错误文案（宿主/日志要能 grep）====================

    #[test]
    fn unknown_flag_reports_go_style_error() {
        let mut f = sample();
        let err = f.parse(&args(&["--nope"])).expect_err("应报错");
        assert_eq!(err, "flag provided but not defined: -nope");
    }

    #[test]
    fn missing_value_reports_go_style_error() {
        let mut f = sample();
        let err = f.parse(&args(&["--uid"])).expect_err("应报错");
        assert_eq!(err, "flag needs an argument: -uid");
    }

    #[test]
    fn required_missing_reports_chinese_error() {
        let mut f = sample();
        let err = f.parse(&args(&[])).expect_err("应报错");
        assert_eq!(err, "缺少 --uid 参数");
    }

    #[test]
    fn required_rejects_whitespace_only_value() {
        // 前端/宿主偶尔传空串或空格；`trim()` 后为空即视为没给。
        let mut f = sample();
        let err = f.parse(&args(&["--uid", "   "])).expect_err("应报错");
        assert_eq!(err, "缺少 --uid 参数");
    }

    #[test]
    fn required_with_nonempty_default_never_fails() {
        // ★ 已知语义：必填校验查的是「给的值 + 默认值」，故默认值非空时
        // 该标志实际上不必填。注册必填标志时默认值必须留空。
        let mut f = FlagSet::new("x").string("uid", "fallback", true);
        f.parse(&args(&[])).expect("默认值非空 ⇒ 不该报错");
        assert_eq!(f.get("uid"), "fallback");
    }

    // ==================== 取值 ====================

    #[test]
    fn missing_optional_flag_falls_back_to_default() {
        let mut f = sample();
        f.parse(&args(&["--uid", "u1"])).expect("应解析成功");
        assert_eq!(f.get("auth-dir"), "/default/auths");
        assert!(!f.flag("verbose")); // 布尔默认 false
    }

    #[test]
    fn unregistered_names_return_empty_and_false() {
        let mut f = sample();
        f.parse(&args(&["--uid", "u1"])).expect("应解析成功");
        assert_eq!(f.get("does-not-exist"), "");
        assert!(!f.flag("does-not-exist"));
    }

    #[test]
    fn get_before_parse_returns_defaults() {
        // 派发层可能先读默认值（如 `--auth-dir`）再决定要不要解析。
        let f = sample();
        assert_eq!(f.get("auth-dir"), "/default/auths");
        assert!(f.positional().is_empty());
    }

    // ==================== parse_bool ====================

    #[test]
    fn parse_bool_accepts_go_strconv_set() {
        // Go `strconv.ParseBool` 接受的全部形式（`flag` 包用的就是它）。
        for s in ["1", "t", "T", "true", "TRUE", "True"] {
            assert_eq!(parse_bool(s), Some(true), "{s} 应为 true");
        }
        for s in ["0", "f", "F", "false", "FALSE", "False"] {
            assert_eq!(parse_bool(s), Some(false), "{s} 应为 false");
        }
    }

    #[test]
    fn parse_bool_rejects_everything_else() {
        // ★ 刻意**不**接受 "yes"/"on"/"" —— Go 也不接受，放宽会让两端口径分叉。
        for s in ["", "yes", "no", "on", "off", "2", "tRuE", " true"] {
            assert_eq!(parse_bool(s), None, "{s:?} 应被拒绝");
        }
    }

    // ==================== 退出码契约 ====================

    #[test]
    fn business_error_returns_zero() {
        // ★ 模块头的核心契约：业务失败必须返回 0，否则宿主会丢弃 message。
        assert_eq!(write_business_error("上游拒绝"), 0);
    }

    #[test]
    fn business_error_with_uid_returns_zero() {
        // ★ 与 write_business_error 同契约（返回 0），只是多一个 uid。
        // 宿主 `qoder_login.rs` / `zcode_login.rs` 都靠 stdout 里的 uid 归因，
        // 若这里返回非 0，宿主会丢掉整个 JSON、用户看不到原因。
        assert_eq!(write_business_error_uid("u1", "上游拒绝"), 0);
    }

    #[test]
    fn usage_error_returns_two() {
        // ★ 参数错误必须返回 2（宿主 `run_login_cmd` 把非 0 当 stderr 消息）。
        assert_eq!(usage_error("缺少 --uid 参数"), 2);
    }

    #[test]
    fn exit_codes_are_distinct() {
        // 防回归：两者若都写成同一个数，宿主就无法区分「参数错」与「业务错」。
        assert_ne!(write_business_error("x"), usage_error("x"));
    }
}
