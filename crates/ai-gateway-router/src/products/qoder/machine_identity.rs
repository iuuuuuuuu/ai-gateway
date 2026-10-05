//! 机器指纹（machineToken / machineType / machineCode）的获取
//! （Go `internal/qoderclient/machine_identity.go` 的移植）。
//!
//! # 为什么需要它
//!
//! 国际版活动接口对残缺请求回的是 **HTTP 200 + 结构合法的 JSON**，只是少一条
//! 活动 —— 不报错、不告警：
//!
//! ```text
//! openapi.qoder.sh + Bearer + Cosy-ClientType        → 1 条活动
//! + 伪造的 Cosy-MachineToken/Cosy-MachineType        → 1 条活动
//! + **真实的** 那两个头                               → 2 条活动（含「每天领 100 Credits」）
//! ```
//!
//! 这就是它藏了这么久的原因。真实值**不在客户端磁盘上**（`auth.v1.dat` 只有
//! token/refreshToken，`auth.machine-id` 只是个 UUID），而是由客户端自带的
//! 阿里云风控 SDK 在运行时产出：
//!
//! ```text
//! resources/umid/runtime-info.exe → stdout 一行 JSON:
//! {"machineToken":"…","machineType":"…","machineCode":"…","vmInfo":{…}}
//! ```
//!
//! # 设计取舍
//!
//! - **只读、不落盘**：这些值属于「这台机器」而不是「这个账号」，写进 per-account
//!   凭证会让换机器后带着旧机器的指纹，反而更可疑。
//! - **进程内缓存**：它们在一次进程生命周期内不变，却要被用在**每个请求**上；
//!   不缓存就要每个请求 spawn 一个进程。与"不落盘"不矛盾。
//! - **失败不致命**：拿不到时返回零值，调用方据 [`MachineIdentity::usable`]
//!   决定**不发**那几个头（退回"降级但可用"的老行为）。
//! - **不伪造**：宁可少一个活动，也不要拿假指纹去请求 —— 假值对严格校验的
//!   host 毫无帮助，还会把"设备指纹"这个概念污染掉。

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

/// 风控 SDK 产出的机器指纹三元组。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct MachineIdentity {
    #[serde(default, rename = "machineToken")]
    pub(crate) machine_token: String,
    #[serde(default, rename = "machineType")]
    pub(crate) machine_type: String,
    #[serde(default, rename = "machineCode")]
    pub(crate) machine_code: String,
}

impl MachineIdentity {
    /// 三个值是否齐全。任一为空都不该拿去请求。
    pub(crate) fn usable(&self) -> bool {
        !self.machine_token.is_empty()
            && !self.machine_type.is_empty()
            && !self.machine_code.is_empty()
    }
}

/// 单次取指纹的超时。
///
/// 实测本机约 30~80ms。给 5 秒是**宽裕**值：SDK 在某些虚拟化环境里会做更多探测
/// （`vmInfo` 会报 `isVm=true`，那种机器更慢）。超时后放弃，不阻塞调用方 ——
/// 领取活动不值得为指纹卡住。
const RUNTIME_INFO_TIMEOUT: Duration = Duration::from_secs(5);

/// 该可执行文件的候选名（按平台）。
///
/// Windows 实测是 `runtime-info.exe`；其它平台的文件名未实测，故一并列出常见形态，
/// 不存在就跳过（不猜、不报错）。
fn runtime_info_names() -> &'static [&'static str] {
    if cfg!(windows) {
        &["runtime-info.exe", "runtime-info"]
    } else {
        &["runtime-info", "runtime-info.exe"]
    }
}

/// 候选目录（**通用，不写死本机路径**）。
///
/// 覆盖三种实测/可能的布局：
///
/// ① 随客户端安装：`<install>/resources/umid/` ← 实测本机
/// ② 多版本布局：`<install>/.qoder-versions/<v>/resources/umid/`
/// ③ 用户级缓存：`~/.qoder/.bin/.umid-<platform>-<hash>/` ← 实测本机也有
///
/// ⚠ 一律**枚举**而不是拼死一个路径：客户端的版本号与哈希后缀会变
/// （实测 `.umid-win32-x64-48d1294f147c9d89` 里那段哈希是内容哈希）。
fn runtime_info_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    // ③ 用户级缓存：枚举 ~/.qoder/.bin/ 下所有 .umid-* 目录。
    if let Some(home) = dirs::home_dir() {
        let bin = home.join(".qoder").join(".bin");
        if let Ok(entries) = std::fs::read_dir(&bin) {
            for e in entries.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with(".umid-") {
                        dirs.push(e.path());
                    }
                }
            }
        }
    }

    // ①② 随客户端安装：枚举常见安装根，再看 resources/umid 与
    //     .qoder-versions/*/resources/umid。
    for root in install_roots() {
        dirs.push(root.join("resources").join("umid"));
        let vers = root.join(".qoder-versions");
        if let Ok(entries) = std::fs::read_dir(&vers) {
            for e in entries.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    dirs.push(e.path().join("resources").join("umid"));
                }
            }
        }
    }
    dirs
}

/// 客户端可能的安装根目录。
///
/// 与 `client_import` 的 `app_dirs` 取舍相同：**列多个而不是只认一个** ——
/// 只认 CN 会在国际版机器上静默找不到，反之亦然。
///
/// ⚠ Windows 上每个名字都额外加一层 `Programs\`：实测安装器两种布局都有
/// （`%LOCALAPPDATA%\Qoder` 与 `%LOCALAPPDATA%\Programs\Qoder`）。
fn install_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        let la = PathBuf::from(la);
        for n in ["Qoder", "Qoder CN", "QoderCN"] {
            roots.push(la.join(n));
            roots.push(la.join("Programs").join(n));
        }
        for n in ["Programs\\Qoder", "Programs\\Qoder CN"] {
            // Go 侧对这两个名字也再加一层 Programs\，逐字保留这个（冗余但无害的）
            // 行为：`LA\Programs\Qoder` 与 `LA\Programs\Programs\Qoder` 都试一遍。
            roots.push(la.join(n));
            roots.push(la.join("Programs").join(n));
        }
    }
    // macOS / Linux 的常见位置（未实测，保守列出；不存在就跳过）。
    roots.push(PathBuf::from("/Applications/Qoder.app/Contents/Resources"));
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".qoder").join("app"));
    }
    roots
}

/// 文件是否存在且不是目录（Go `fileExists`）。
fn file_exists(p: &std::path::Path) -> bool {
    std::fs::metadata(p).map(|m| !m.is_dir()).unwrap_or(false)
}

/// 找出可用的 `runtime-info` 可执行文件。
pub(crate) fn find_runtime_info() -> Option<PathBuf> {
    for dir in runtime_info_dirs() {
        for name in runtime_info_names() {
            let p = dir.join(name);
            if file_exists(&p) {
                return Some(p);
            }
        }
    }
    None
}

/// 进程级缓存。
static IDENTITY: OnceLock<MachineIdentity> = OnceLock::new();

/// 取本机机器指纹（带进程级缓存）。
///
/// 失败时返回零值 —— 调用方据 [`MachineIdentity::usable`] 决定是否发那几个头。
/// **不返回 error**：调用方（活动查询/领取）没有任何有意义的补救动作，
/// 而把 error 一路传上去只会让"拿不到指纹"看起来像"功能坏了"。
pub(crate) fn machine_identity_of() -> &'static MachineIdentity {
    IDENTITY.get_or_init(|| match find_runtime_info() {
        Some(exe) => run_runtime_info(&exe),
        None => MachineIdentity::default(),
    })
}

/// 跑一次可执行文件并解析 stdout 的 JSON。
///
/// 抽出来是为了让测试能对着一个假的可执行文件断言解析逻辑
/// （不需要真的装 Qoder 客户端）。
pub(crate) fn run_runtime_info(exe: &std::path::Path) -> MachineIdentity {
    let mut child = match Command::new(exe)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        // 不继承环境变量的**全部**内容 —— SDK 只依赖系统信息，而我们不该把
        // 网关进程的凭据类变量传给它。见 [`minimal_env`]。
        .env_clear()
        .envs(minimal_env())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return MachineIdentity::default(),
    };

    // 手写超时：`std::process` 没有 `Command::output` 的超时版本，而
    // `wait_with_output()` 会一直等到进程退出。这里用「后台线程收 stdout +
    // 主线程限时 wait」，超时就 kill 并放弃。
    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => return MachineIdentity::default(),
    };
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        // 读错误按 Go 的 `raw, _ := ...` 口径忽略。
        let _ = stdout.read_to_end(&mut buf);
        buf
    });

    if !wait_timeout(&mut child, RUNTIME_INFO_TIMEOUT) {
        // 超时（含 SDK 卡住）：杀掉，避免留下僵尸进程。
        let _ = child.kill();
        let _ = child.wait();
        // 线程会随管道关闭而结束，但仍要 join 以免泄漏。
        let _ = reader.join();
        return MachineIdentity::default();
    }

    let raw = match reader.join() {
        Ok(b) => b,
        Err(_) => return MachineIdentity::default(),
    };
    parse_runtime_info(&raw)
}

/// 限时等待子进程退出；返回 `true` 表示已退出。
fn wait_timeout(child: &mut std::process::Child, limit: Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            // 出错时按"拿不到"处理，不再等。
            Err(_) => return false,
            Ok(None) => {}
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 从 SDK 的 stdout 里抠出 JSON 并解析。
///
/// ⚠ 输出可能带前后空白/多行，取**第一个 `{` 到最后一个 `}`** 的片段。
/// 直接解析整个 stdout 在 SDK 多打一行日志时会失败。
fn parse_runtime_info(raw: &[u8]) -> MachineIdentity {
    let Some(start) = raw.iter().position(|&b| b == b'{') else {
        return MachineIdentity::default();
    };
    let Some(end) = raw.iter().rposition(|&b| b == b'}') else {
        return MachineIdentity::default();
    };
    if end <= start {
        return MachineIdentity::default();
    }
    serde_json::from_slice::<MachineIdentity>(&raw[start..=end]).unwrap_or_default()
}

/// 传给 SDK 的最小环境变量集合。
///
/// Windows 上必须保留 `SystemRoot`（缺了 DLL 加载会失败），其余按需保留。
/// **刻意不透传**整个环境：网关进程里可能有 API key 等敏感变量，
/// 没理由交给一个第三方 SDK。
fn minimal_env() -> Vec<(String, String)> {
    const KEEP: &[&str] = &[
        "SystemRoot",
        "windir",
        "TEMP",
        "TMP",
        "PATH",
        "HOME",
        "USERPROFILE",
    ];
    KEEP.iter()
        .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| ((*k).to_string(), v)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usable_requires_all_three() {
        let full = MachineIdentity {
            machine_token: "t".into(),
            machine_type: "y".into(),
            machine_code: "c".into(),
        };
        assert!(full.usable());
        for missing in 0..3 {
            let mut m = full.clone();
            match missing {
                0 => m.machine_token.clear(),
                1 => m.machine_type.clear(),
                _ => m.machine_code.clear(),
            }
            assert!(!m.usable(), "missing index {missing}");
        }
        assert!(!MachineIdentity::default().usable());
    }

    #[test]
    fn parses_the_sdk_line_and_ignores_surrounding_noise() {
        // 前后带日志/换行时也必须能解析（SDK 版本变化会多打行）。
        let raw = b"some log line\n{\"machineToken\":\"P1gAAbR\",\"machineType\":\"300a54ab\",\
                    \"machineCode\":\"b3c4e04f\",\"vmInfo\":{\"isVm\":true}}\ntrailing\n";
        let id = parse_runtime_info(raw);
        assert_eq!(id.machine_token, "P1gAAbR");
        assert_eq!(id.machine_type, "300a54ab");
        assert_eq!(id.machine_code, "b3c4e04f");
        assert!(id.usable());
    }

    #[test]
    fn garbage_stdout_yields_a_zero_value_instead_of_an_error() {
        // 调用方无从补救，故一律退化成"拿不到"，不报错。
        assert_eq!(parse_runtime_info(b""), MachineIdentity::default());
        assert_eq!(parse_runtime_info(b"no braces here"), MachineIdentity::default());
        assert_eq!(parse_runtime_info(b"}{"), MachineIdentity::default());
        assert_eq!(parse_runtime_info(b"{\"machineToken\":\"t\"}"), {
            // 只有一个字段时能解析，但 usable() 为 false。
            let m = MachineIdentity {
                machine_token: "t".into(),
                ..Default::default()
            };
            assert!(!m.usable());
            m
        });
    }

    #[test]
    fn minimal_env_never_leaks_the_whole_environment() {
        // 钉住"刻意不透传"这一点：返回的键必须都在白名单里。
        for (k, _) in minimal_env() {
            assert!(
                [
                    "SystemRoot",
                    "windir",
                    "TEMP",
                    "TMP",
                    "PATH",
                    "HOME",
                    "USERPROFILE"
                ]
                .contains(&k.as_str()),
                "unexpected key leaked to the SDK: {k}"
            );
        }
    }

    #[test]
    fn runtime_info_names_prefers_the_platform_binary() {
        let names = runtime_info_names();
        if cfg!(windows) {
            assert_eq!(names[0], "runtime-info.exe");
        } else {
            assert_eq!(names[0], "runtime-info");
        }
    }

    #[test]
    fn missing_executable_is_not_an_error() {
        let bogus = std::env::temp_dir().join("definitely-not-here-runtime-info.exe");
        let _ = std::fs::remove_file(&bogus);
        assert_eq!(run_runtime_info(&bogus), MachineIdentity::default());
    }
}
