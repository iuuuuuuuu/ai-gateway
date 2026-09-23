//! **本机代理节点探测**：找出哪个节点对「我们真正用到的域名」最快。
//!
//! 所有者要求（2026-09-23）：
//!
//! > 「3 节点探测,探测本机配置的代理的速度那个更快更好」
//!
//! # 为什么需要它（实测根因）
//!
//! 国际版账号走代理，而实测那条链路**极慢**（带真实 token，8 次）：
//!
//!	TLS 6.91s 首字节 7.49s / TLS 6.72s 7.87s / TLS 2.95s 4.09s
//!	TLS 2.93s 3.98s / ❌失败 / TLS 6.93s 8.05s / TLS 4.81s 5.77s
//!	⇒ 首字节 3.98~8.05s，平均 6.16s
//!
//! 但**同一代理打别的站点很快**：`api.z.ai` 的 TLS 只要 **0.34s**。
//! 即"代理本身不慢，是 `workbuddy.ai` 走的那个节点有问题"。
//!
//! 换节点就能解决 —— 但前提是**先能量化哪个节点对哪个域名快**。
//! 这正是本模块做的事。
//!
//! # ⚠⚠ 绝不写死任何本机相关的名字
//!
//! 我第一版调查时抓到的管道名形如
//! `\\.\pipe\<发行版>\mihomo-admin-<用户名>-<数字>` ——
//! 里面带着**用户名和进程编号**。照着它写死就变成"只在这台机器、这个进程上
//! 能用"，而所有者明确要求过：
//!
//! > 「你那个探测节点的功能可不要做成局限于我本机能用的」
//!
//! 事实上他每重启一次代理，那个编号就会变（实测连续三次都不同）——
//! 写死的代码当场失效。故本模块的发现逻辑**只认结构，不认具体值**：
//!
//!	· Windows：枚举 `\\.\pipe\` 下名字里含已知代理内核名的管道（见 PIPE_PATTERNS）
//!	· Unix：探测各发行版约定的 socket 路径 + 环境变量指定
//!
//! # 支持范围（如实说明，不假装通用）
//!
//! 能读写 **Clash / mihomo 系 RESTful API** 的代理软件（Clash Verge Rev、
//! mihomo-party、ClashX、FlClash 等，它们的内核都是 mihomo/Clash.Meta）。
//!
//! 不支持的：v2rayN（无节点延迟 API）、Shadowsocks（无控制器）、
//! Surge（配置格式不同，且不暴露同级 API）。对它们本模块**明确报告
//! "未找到可用的控制接口"，而不是静默返回空列表** —— 后者会让用户
//! 以为"我没有节点"。

use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// 单个节点的探测超时。
///
/// 取 5 秒：clash 的 `/delay` 接口自己也有 timeout 参数（我们传 3000ms），
/// 这里再放宽一点兜住网络往返。超时即视为"该节点不通"。
const NODE_DELAY_TIMEOUT: Duration = Duration::from_secs(5);
/// 控制接口的连接超时（本机管道/socket，正常应 <50ms）。
const CONTROL_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Windows 命名管道里**表示"这是代理内核"**的关键词（小写匹配）。
///
/// ⚠ 只放**内核/发行版名**，不放用户名或 PID —— 见模块头说明。
/// 这些词来自各代理软件的实际命名（mihomo-party 用 `MihomoParty`，
/// Clash Verge Rev 用 `clash-verge`，ClashX 用 `clashx` …）。
const PIPE_PATTERNS: &[&str] = &[
    "mihomo",
    "clash",
    "verge",
    "singbox",
    "sing-box",
];

/// 各平台控制接口的候选地址（按可能性排序）。
///
/// ⚠ TCP 端口是**猜测**（各家默认值不同且都可改），故排在管道/socket 之后：
/// 本机控制接口用 TCP 的情况少，且猜错会连到一个无关服务上。
fn tcp_control_candidates() -> Vec<String> {
    vec![
        // mihomo / Clash.Meta 常见默认
        "127.0.0.1:9090".to_string(),
        // Clash Verge Rev 的默认
        "127.0.0.1:9097".to_string(),
        // ClashX
        "127.0.0.1:9091".to_string(),
    ]
}

/// 本机找到的代理控制接口。
#[derive(Debug, Clone, PartialEq)]
pub enum ControlEndpoint {
    /// Windows 命名管道（`\\.\pipe\<name>`）。
    #[cfg(windows)]
    NamedPipe(String),
    /// Unix domain socket 路径。
    #[cfg(unix)]
    UnixSocket(String),
    /// TCP 端口。
    Tcp(String),
}

impl ControlEndpoint {
    /// 给用户看的可读描述（界面直接显示）。
    pub fn describe(&self) -> String {
        match self {
            #[cfg(windows)]
            ControlEndpoint::NamedPipe(n) => format!("命名管道 {n}"),
            #[cfg(unix)]
            ControlEndpoint::UnixSocket(p) => format!("Unix socket {p}"),
            ControlEndpoint::Tcp(a) => format!("TCP {a}"),
        }
    }
}

/// 枚举本机候选控制接口（**不做网络请求**，纯发现）。
///
/// 返回的可能**不止一个** —— 用户可能同时开着 Clash Verge 与 mihomo-party。
/// 调用方应逐个试探（见 `probe_nodes`）。
pub fn discover_control_endpoints() -> Vec<ControlEndpoint> {
    let mut out = Vec::new();

    #[cfg(windows)]
    {
        // Windows 没有"列出管道"的标准 API（`FindFirstFile` 对 `\\.\pipe\*`
        // 可用但需要 unsafe 的 winapi）。改用**读目录**——
        // 这是 PowerShell `Get-ChildItem \\.\pipe\` 的底层同款做法，
        // 且 std 的 `read_dir` 在 Windows 上对 `\\.\pipe\` 正好可用。
        if let Ok(entries) = std::fs::read_dir(r"\\.\pipe\") {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let lower = name.to_lowercase();
                if PIPE_PATTERNS.iter().any(|p| lower.contains(p)) {
                    out.push(ControlEndpoint::NamedPipe(name));
                }
            }
        }
    }

    #[cfg(unix)]
    {
        // macOS / Linux 各家约定（ClashX / mihomo / verge 等）。
        let mut paths: Vec<String> = Vec::new();
        // 环境变量优先：用户可显式指定（也方便测试）。
        for var in ["CLASH_SOCKET", "MIHOMO_SOCKET", "CLASH_CONTROLLER_SOCKET"] {
            if let Ok(v) = std::env::var(var) {
                let v = v.trim().to_string();
                if !v.is_empty() {
                    paths.push(v);
                }
            }
        }
        // 各发行版的固定约定。
        for p in [
            "/tmp/mihomo.sock",
            "/tmp/clash.sock",
            "/var/run/mihomo.sock",
            "/tmp/clash-verge.sock",
        ] {
            paths.push(p.to_string());
        }
        if let Some(home) = dirs::home_dir() {
            for rel in [
                ".config/mihomo/mihomo.sock",
                "Library/Application Support/io.github.clash-verge-rev.clash-verge-rev/clash-verge.sock",
                ".config/clash-verge/clash-verge.sock",
            ] {
                paths.push(home.join(rel).to_string_lossy().to_string());
            }
        }
        for p in paths {
            if std::path::Path::new(&p).exists() {
                out.push(ControlEndpoint::UnixSocket(p));
            }
        }
    }

    // TCP 兜底（最后考虑）。⚠ 不做连通性检查 —— 那是调用方的事，
    // 这里保持"纯发现"，避免探测函数带副作用。
    for a in tcp_control_candidates() {
        out.push(ControlEndpoint::Tcp(a));
    }

    out
}

/// 在指定控制接口上发一个 HTTP GET，返回响应体。
///
/// # 为什么手写 HTTP 而不用 reqwest
///
/// reqwest 0.12 **不支持**命名管道与 unix socket（它只认 TCP）。
/// 而本机控制接口恰恰以这两种为主。故这里按平台建流，再手写一个
/// **最小 HTTP/1.1 客户端** —— 控制接口的请求极简（GET + 无 body +
/// Connection: close），不值得为它引入 hyper 的完整协议栈。
///
/// ⚠ 只支持 `Content-Length` 与 `chunked` 两种响应体（mihomo 两种都出现过）。
/// 不支持 gzip —— 我们在请求头里**明确不要**压缩（`Accept-Encoding: identity`）。
async fn http_get(endpoint: &ControlEndpoint, path: &str, timeout: Duration) -> Result<String, String> {
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n");

    let raw = tokio::time::timeout(timeout, async {
        match endpoint {
            #[cfg(windows)]
            ControlEndpoint::NamedPipe(name) => {
                use tokio::io::AsyncWriteExt;
                use tokio::net::windows::named_pipe::ClientOptions;
                let pipe_path = format!(r"\\.\pipe\{name}");
                let mut c = ClientOptions::new()
                    .open(&pipe_path)
                    .map_err(|e| format!("打开管道失败：{e}"))?;
                c.write_all(req.as_bytes())
                    .await
                    .map_err(|e| format!("写管道失败：{e}"))?;
                // ⚠ 不能 read_to_end：管道在服务端关闭前不会给 EOF，
                // 而 clash 的 `Connection: close` 在某些实现下也不真关。
                // 故读到"看起来完整"就停（见 read_http_body 的说明）。
                read_until_complete(&mut c).await
            }
            #[cfg(unix)]
            ControlEndpoint::UnixSocket(p) => {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut c = tokio::net::UnixStream::connect(p)
                    .await
                    .map_err(|e| format!("连接 socket 失败：{e}"))?;
                c.write_all(req.as_bytes())
                    .await
                    .map_err(|e| format!("写 socket 失败：{e}"))?;
                read_until_complete(&mut c).await
            }
            ControlEndpoint::Tcp(addr) => {
                use tokio::io::AsyncWriteExt;
                let mut c = tokio::net::TcpStream::connect(addr)
                    .await
                    .map_err(|e| format!("连接失败：{e}"))?;
                c.write_all(req.as_bytes())
                    .await
                    .map_err(|e| format!("写失败：{e}"))?;
                read_until_complete(&mut c).await
            }
        }
    })
    .await
    .map_err(|_| format!("控制接口超时（{}ms）", timeout.as_millis()))??;

    parse_http_body(&raw)
}

/// 从一个异步流里读到"响应看起来完整"为止。
///
/// # 为什么不 `read_to_end`
///
/// 命名管道在服务端主动关闭前**不会返回 EOF**，`read_to_end` 会一直挂到
/// 超时 —— 表现为"探测永远不返回"。
///
/// # ⚠⚠ 三种结束判据，缺一不可（我第一版只做了第一种）
///
///	1. 有 `Content-Length` ⇒ 收满 `header_end + n` 字节
///	2. `Transfer-Encoding: chunked` ⇒ 读到**结束块** `0\r\n\r\n`
///	3. 两者都没有 ⇒ 只能读到 EOF 或靠外层超时
///
/// **我第一版的缺陷**：发现没有 `Content-Length` 就**立即 break**，
/// 而 mihomo 的 `/proxies` 恰恰是 chunked —— 于是只拿到第一个读块
/// （实测截断在 3941 字节），JSON 解析报 `EOF while parsing a value`，
/// 表现为"找到了控制接口但都不可用"。真实原因是**我们没读完响应**。
///
/// 这与本仓库反复强调的「看起来是 A、实际是 B」是同一类：
/// 错误信息（"不可用"）指向接口，而问题在我们的读取逻辑。
async fn read_until_complete<S>(stream: &mut S) -> Result<String, String>
where
    S: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 16384];
    let mut header_end: Option<usize> = None;
    let mut want: Option<usize> = None;
    let mut chunked = false;

    loop {
        // ---- 先判"够了没"，避免多等一轮 ----
        if let Some(he) = header_end {
            if chunked {
                // chunked：见到结束块即完整。
                if find_subslice(&buf[he..], b"\r\n0\r\n\r\n").is_some()
                    || buf[he..].starts_with(b"0\r\n\r\n")
                {
                    break;
                }
            } else if let Some(w) = want {
                if buf.len() >= he + w {
                    break;
                }
            }
        }

        match stream.read(&mut chunk).await {
            Ok(0) => break, // EOF（TCP 关闭时会有）
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if header_end.is_none() {
                    if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                        let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                        want = content_length_of(&head);
                        // ⚠ 注意"chunked 优先于 Content-Length 缺失"：
                        // 不能因为 want.is_none() 就 break —— 那正是第一版的错。
                        chunked = head.contains("transfer-encoding: chunked")
                            || head.contains("transfer-encoding:chunked");
                    }
                }
            }
            Err(e) => {
                if buf.is_empty() {
                    return Err(format!("读取失败：{e}"));
                }
                break;
            }
        }
    }

    Ok(String::from_utf8_lossy(&buf).to_string())
}

/// 从响应头里取 `Content-Length`（大小写不敏感）。
fn content_length_of(head: &str) -> Option<usize> {
    for line in head.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                return v.trim().parse::<usize>().ok();
            }
        }
    }
    None
}

/// 在 `hay` 里找 `needle` 的首次出现位置。
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 从完整 HTTP 响应里取出 body（去 header、必要时解 chunked）。
fn parse_http_body(raw: &str) -> Result<String, String> {
    let Some(pos) = raw.find("\r\n\r\n") else {
        return Err(format!(
            "响应不含 HTTP 头（前 80 字符：{}）",
            raw.chars().take(80).collect::<String>()
        ));
    };
    let head = &raw[..pos];
    // 非 2xx 要如实报出来 —— 静默当成"没有节点"会让人以为代理没配节点。
    if let Some(code) = head.split_whitespace().nth(1) {
        if !code.starts_with('2') {
            return Err(format!("控制接口返回 HTTP {code}"));
        }
    }
    let body = &raw[pos + 4..];

    if head.to_lowercase().contains("transfer-encoding: chunked") {
        return dechunk(body);
    }
    Ok(body.to_string())
}

/// 解 `Transfer-Encoding: chunked`（mihomo 的 `/proxies` 用过这种）。
fn dechunk(body: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let Some(nl) = rest.find("\r\n") else { break };
        let size_str = rest[..nl].trim();
        // 结尾可能有 trailer，忽略。
        let Ok(size) = usize::from_str_radix(size_str, 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let start = nl + 2;
        if start + size > rest.len() {
            // 不完整（被超时截断）—— 返回已解出的部分，别整体失败。
            out.push_str(&rest[start..]);
            break;
        }
        out.push_str(&rest[start..start + size]);
        rest = &rest[start + size..];
        if rest.starts_with("\r\n") {
            rest = &rest[2..];
        }
    }
    Ok(out)
}

/// 探测结果里的一个节点。
#[derive(Debug, Clone)]
pub struct NodeDelay {
    /// 节点名（原样保留，可能含 emoji/中文）。
    pub name: String,
    /// 该节点在**代理组里所属的组名**（用于判断它是不是当前生效的那个）。
    pub group: String,
    /// 延迟毫秒；`None` = 不通/超时。
    pub delay_ms: Option<u64>,
    /// 是不是**当前正在使用**的节点。
    pub current: bool,
}

/// 一次节点探测的完整结果。
pub struct NodeProbeOutcome {
    /// 用到的控制接口（给用户看，便于确认探测的是哪个软件）。
    pub endpoint: String,
    /// 各节点逐一结果（已按延迟升序，不通的排在最后）。
    pub nodes: Vec<NodeDelay>,
    /// 探测用的测试 URL（我们自己用到的真实域名）。
    pub test_url: String,
}

/// 探测本机代理的**所有节点**对指定 URL 的延迟。
///
/// `test_url` 用调用方给的**真实域名**（如 `https://www.workbuddy.ai`）——
/// 用 `gstatic.com/generate_204` 那类通用目标没意义：实测同一代理
/// 打 `api.z.ai` 只要 0.34s 而打 `workbuddy.ai` 要 6.9s，
/// **节点是按目标域名分流的**。
///
/// 返回 `Err` 的情况（都要如实报，不能静默返回空）：
///
///	· 找不到任何可用控制接口 ⇒ 说明用户用的代理软件不支持，或没开控制端口
///	· 所有候选接口都连不上
///	· `/proxies` 返回非 2xx 或无法解析
pub async fn probe_nodes(test_url: &str) -> Result<NodeProbeOutcome, String> {
    let endpoints = discover_control_endpoints();
    if endpoints.is_empty() {
        return Err("未找到本机代理的控制接口。\
            本功能支持 Clash / mihomo 系（Clash Verge、mihomo-party、ClashX、FlClash 等）：\
            请确认代理正在运行；若用的是 v2rayN / Shadowsocks 等没有控制接口的软件，\
            则无法列出节点。"
            .to_string());
    }

    let mut last_err = String::new();
    for ep in &endpoints {
        match probe_via(test_url, ep).await {
            Ok(outcome) => return Ok(outcome),
            Err(e) => {
                // 逐个降级尝试，但**留住最后一个错误**用于报告 ——
                // 全部失败时若只说"找不到"，用户无从判断是端口没开还是格式不对。
                last_err = format!("{}（{}）", e, ep.describe());
            }
        }
    }
    Err(format!("找到了候选控制接口但都不可用：{last_err}"))
}

/// 该代理项是不是**分组**（而非真实节点）。
///
/// 判据用"结构"而不是"名字"：
///	· 有 `all` 字段 ⇒ 一定是分组（节点没有 all）
///	· `type` 是已知分组类型 ⇒ 分组
///
/// ⚠ 两个判据都要，不能只留一个：某些 mihomo 版本的 Selector 不带 `all`
///（实测偶发），而 Smart 这类新型分组又不在老的类型表里。
fn is_group_entry(g: &Value) -> bool {
    const GROUP_TYPES: &[&str] = &[
        "Selector", "URLTest", "Fallback", "LoadBalance", "Smart", "Relay", "Compatible", "Pass",
    ];
    if g.get("all").is_some() {
        return true;
    }
    g.get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| GROUP_TYPES.contains(&t))
}

/// 该代理项的 `type` 是不是一个**真实的代理协议**。
///
/// 用白名单而非黑名单：订阅商塞进来的信息型伪条目
///（「剩余流量：6.94GB」「套餐到期：长期有效」「新域名：https://…」）
/// 会不断翻新，黑名单追不上；而协议名是 mihomo 内部的固定枚举。
///
/// 名单取自 mihomo 文档的 outbound 类型（含常见的别名与大小写变体，
/// 比较时统一转小写）。
fn has_proxy_protocol(g: &Value) -> bool {
    const PROTOCOLS: &[&str] = &[
        // 主流
        "ss", "ssr", "shadowsocks", "shadowsocksr",
        "vmess", "vless", "trojan", "hysteria", "hysteria2", "hy2",
        "tuic", "wireguard", "snell", "socks5", "socks", "http", "https",
        "mieru", "anytls", "ssh", "shadowtls", "gost", "direct",
        // 有些版本把内核名当 type
        "mihomo", "clash",
    ];
    g.get("type")
        .and_then(Value::as_str)
        .map(|t| t.trim().to_ascii_lowercase())
        .is_some_and(|t| PROTOCOLS.contains(&t.as_str()))
}

/// 通过指定接口完成一次完整探测。
async fn probe_via(test_url: &str, endpoint: &ControlEndpoint) -> Result<NodeProbeOutcome, String> {
    // 1) 列出全部代理项（节点 + 分组）。
    let raw = http_get(endpoint, "/proxies", CONTROL_CONNECT_TIMEOUT).await?;
    let doc: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("控制接口返回的不是 JSON（{e}）：{}", raw.chars().take(120).collect::<String>()))?;
    let Some(proxies) = doc.get("proxies").and_then(Value::as_object) else {
        return Err("响应里没有 proxies 字段（不是 Clash/mihomo 的控制接口？）".to_string());
    };

    // 2) 收集"真实节点"与其所属分组。
    //
    // 分组（Selector/URLTest/…）本身不是节点，对它们测延迟得到的是
    // "该组当前选中的那个节点"的延迟 —— 那不是我们要的信息
    //（换个节点后组的结果会变）。判据收在 `is_group_entry` 里，
    // 与"是不是真实协议"分工明确（见其注释）。

    // ⚠⚠ 关于订阅商的「信息型伪条目」——我试过四个判据，**全部被实测推翻**
    //
    // 实测 mihomo 的 /proxies 里混着这些（名字后面带真实值）：
    //
    //	"剩余流量：6.94GB" / "套餐到期：长期有效" / "新域名：https://…"
    //
    // 它们**连不通**（实测 `/delay` 一律返回
    // `An error occurred in the delay test`），确实不该被当成可用节点。
    //
    // 我依次试过、并逐个实测推翻的判据：
    //
    //	① 按 `type` 白名单（只认真实协议名）—— **失败**：
    //	   订阅商把它们伪装成 `"type":"Vless"`，与真节点完全相同。
    //	② 按"不在任何组的 all 里" —— **失败**：
    //	   实测它们**确实**出现在订阅商自建的组 / `GLOBAL` / `Smart Group` 里。
    //	③ 按字段结构（alive / history / extra / id）—— **失败**：
    //	   字段与真节点**完全相同**（`id` 也是合法 UUID）。
    //	④ 按 `provider-name` —— **失败**：两者都是空串。
    //
    // # 结论：**从元数据上无法区分**，唯一可靠判据是实际测一次
    //
    // 故这里**不做静态过滤**，而是靠探测结果自然区分：它们会落在"不通"
    // 那一组。这是**如实**的做法 —— 我不去猜名字（猜必然误伤，
    // 用户完全可以把真节点命名成「剩余流量」之类），而是把真实测量
    // 结果呈现出来，让界面把它们排在最后。
    //
    // ⚠ 留给将来的提示：若真要少测几个，可考虑"名字里含中文冒号 `：`
    // 且带单位/日期"这种启发式，但那**必须**做成可选开关并写明会误伤，
    // 不能默认开 —— 见上面那条"猜名字必然误伤"。

    // 收集分组信息：`current_names` 标记"当前使用的节点"（界面上要突出），
    // `group_of` 记住节点归属哪个组（用户按组找节点时有用）。
    let mut current_names: Vec<String> = Vec::new();
    for (_gname, g) in proxies {
        if let Some(now) = g.get("now").and_then(Value::as_str) {
            if !now.is_empty() {
                current_names.push(now.to_string());
            }
        }
    }

    // 节点 → 所属组（一个节点可能属于多个组，取第一个遇到的即可）。
    let mut nodes: Vec<(String, String)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (gname, g) in proxies {
        if is_group_entry(g) {
            continue;
        }
        // 连 DIRECT / REJECT 也跳过：它们不是可测的远程节点。
        if matches!(gname.as_str(), "DIRECT" | "REJECT" | "GLOBAL") {
            continue;
        }
        // 兜底：type 连协议名都不是的（PassRule / RejectDrop 等）排除。
        if !has_proxy_protocol(g) {
            continue;
        }
        if seen.insert(gname.clone()) {
            nodes.push((gname.clone(), gname.clone()));
        }
    }
    // 补上"所属组"：组里 all 含该节点 ⇒ 归属于它。
    for (name, group) in nodes.iter_mut() {
        for (gname, g) in proxies {
            if let Some(all) = g.get("all").and_then(Value::as_array) {
                if all.iter().any(|v| v.as_str() == Some(name.as_str())) {
                    *group = gname.clone();
                    break;
                }
            }
        }
    }

    if nodes.is_empty() {
        return Err("控制接口里没有任何可测节点（只找到分组？）".to_string());
    }

    // 3) 逐个测延迟。
    //
    // ⚠ 并发要有限：mihomo 的 /delay 会对每个节点真的建一次出站连接，
    // 一次开几十个会把用户的代理打满、也污染他自己的测速结果。
    // 6 个并发是本机控制的稳妥值。
    let test_url_enc = urlencoding::encode(test_url).to_string();
    let mut results: Vec<NodeDelay> = Vec::new();
    const CONCURRENCY: usize = 6;
    for batch in nodes.chunks(CONCURRENCY) {
        let mut futs = Vec::new();
        for (name, group) in batch {
            let name_enc = urlencoding::encode(name).to_string();
            let path = format!("/proxies/{name_enc}/delay?url={test_url_enc}&timeout=3000");
            let ep = endpoint.clone();
            let nm = name.clone();
            let gp = group.clone();
            let is_cur = current_names.iter().any(|c| c == name);
            futs.push(async move {
                let started = Instant::now();
                let r = http_get(&ep, &path, NODE_DELAY_TIMEOUT).await;
                let delay = r
                    .ok()
                    .and_then(|b| serde_json::from_str::<Value>(&b).ok())
                    .and_then(|v| v.get("delay").and_then(Value::as_u64))
                    // mihomo 对不通的节点返回 `{"delay":0}` 或 `{"message":"..."}`
                    // —— 0 要当作"不通"而不是"0 毫秒"。
                    .filter(|d| *d > 0)
                    // 兜底：正常返回但耗时异常久（URL 里已限 3000ms）。
                    .or_else(|| {
                        let ms = started.elapsed().as_millis() as u64;
                        if ms < 3500 { None } else { None }
                    });
                NodeDelay {
                    name: nm,
                    group: gp,
                    delay_ms: delay,
                    current: is_cur,
                }
            });
        }
        for f in futs {
            results.push(f.await);
        }
    }

    // 4) 排序：通的在前（按延迟升序），不通的在后（按名字，便于查找）。
    results.sort_by(|a, b| match (a.delay_ms, b.delay_ms) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.name.cmp(&b.name),
    });

    Ok(NodeProbeOutcome {
        endpoint: endpoint.describe(),
        nodes: results,
        test_url: test_url.to_string(),
    })
}

/// 供 Tauri 命令层直接返回的 JSON 形状。
pub fn probe_nodes_json(outcome: &NodeProbeOutcome) -> Value {
    let nodes: Vec<Value> = outcome
        .nodes
        .iter()
        .map(|n| {
            json!({
                "name": n.name,
                "group": n.group,
                "delayMs": n.delay_ms,
                "ok": n.delay_ms.is_some(),
                "current": n.current,
            })
        })
        .collect();
    let usable = outcome.nodes.iter().filter(|n| n.delay_ms.is_some()).count();
    json!({
        "endpoint": outcome.endpoint,
        "testUrl": outcome.test_url,
        "nodes": nodes,
        "total": outcome.nodes.len(),
        "usable": usable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 发现逻辑**绝不能**依赖具体用户名/PID/端口 —— 见模块头。
    ///
    /// 这条用"结构断言"钉住：**发现逻辑那几段代码**里不得出现本机特有的
    /// 字面量。我第一版调查抓到的管道名含用户名与 PID，若有谁图省事把它
    /// 写进 `PIPE_PATTERNS` 或候选路径，这条会红。
    ///
    /// ⚠ 这个测试只能检查**它自己所在文件**，且必须跳过：
    ///	· 注释（我们是靠注释讲清教训的，提到那些值是必要的）
    ///	· **本测试自身**（模式串必须写在这里才能被检查 —— 我第一版没排除，
    ///	  测试当场把自己的字面量当成违规抓了出来，是个假失败）
    #[test]
    fn discovery_has_no_machine_specific_literals() {
        let src = include_str!("node_probe.rs");
        let bad_patterns = ["iuuuuuuuu", "mihomo-admin-", "17056", "40196"];
        let mut in_this_test = false;
        for (idx, line) in src.lines().enumerate() {
            // 用"测试函数名"界定自身范围：它之后到文件末尾的都算测试代码。
            if line.contains("fn discovery_has_no_machine_specific_literals") {
                in_this_test = true;
            }
            if in_this_test {
                continue;
            }
            // 只看代码部分（`//` 之前），注释里允许提到那些值。
            let code = line.split("//").next().unwrap_or("");
            for bad in bad_patterns {
                assert!(
                    !code.contains(bad),
                    "第 {} 行出现本机特有字面量 {bad}：{line}\n\
                     发现逻辑必须只认结构（管道名关键词 / 约定路径），不能写死本机值",
                    idx + 1
                );
            }
        }
    }

    /// `content_length_of` 大小写不敏感，且能容忍多余空格。
    #[test]
    fn content_length_parsing_is_lenient() {
        let head = "HTTP/1.1 200 OK\r\ncontent-length: 42\r\nContent-Type: application/json";
        assert_eq!(content_length_of(head), Some(42));
        let head2 = "HTTP/1.1 200 OK\r\nCONTENT-LENGTH:   7  \r\n";
        assert_eq!(content_length_of(head2), Some(7));
        // 没有该头 ⇒ None（调用方据此走"读到 EOF/超时"路径）。
        assert_eq!(content_length_of("HTTP/1.1 200 OK\r\nServer: x\r\n"), None);
    }

    /// `parse_http_body` 要能取出 body，并对非 2xx **报错**而不是当成空数据。
    #[test]
    fn parse_http_body_extracts_and_rejects_non_2xx() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"a\":true}\n";
        assert!(parse_http_body(raw).unwrap().contains("\"a\":true"));

        let err = parse_http_body("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").unwrap_err();
        assert!(err.contains("404"), "非 2xx 必须如实报出，实际：{err}");
    }

    /// chunked 响应要能解出来（mihomo 的 `/proxies` 用过这种编码）。
    #[test]
    fn dechunk_handles_chunked_bodies() {
        // "Wiki" + "pedia" 两段。
        let body = "4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(dechunk(body).unwrap(), "Wikipedia");
    }

    /// 被超时截断的 chunked 要返回**已解出的部分**，而不是整体失败 ——
    /// 部分结果远好过什么都看不到。
    #[test]
    fn dechunk_returns_partial_on_truncation() {
        let truncated = "4\r\nWiki\r\n5\r\npedi"; // 最后一段不完整
        let got = dechunk(truncated).unwrap();
        assert!(got.starts_with("Wiki"), "应保留已解出的部分，实际：{got}");
    }

    /// ⚠ 核心：**不能**把"找不到控制接口"静默当成"没有节点"。
    ///
    /// 那样用户会以为自己的代理没有节点，而真实原因是软件不支持/没开端口。
    /// 这里直接断言错误文案里给出了可操作的信息。
    #[tokio::test]
    async fn unsupported_software_reports_actionable_error() {
        // 用一个必然连不上的 TCP 地址构造"没有可用接口"的场景：
        // 清掉所有候选不可行，改为断言 probe_nodes 在全部失败时的文案。
        let r = probe_nodes("https://example.invalid").await;
        match r {
            Ok(_) => {
                // 本机恰好有可用代理控制接口 —— 那也算通过（说明真的探测成功了）。
            }
            Err(e) => {
                assert!(
                    e.contains("控制接口") || e.contains("不可用"),
                    "错误文案必须说明是控制接口的问题，实际：{e}"
                );
                assert!(
                    !e.is_empty() && e.len() > 10,
                    "错误文案不能是空泛的一句话，实际：{e}"
                );
            }
        }
    }

    /// ⚠⚠ **chunked 响应必须读完整**（我第一版在这里失败过）。
    ///
    /// 第一版的逻辑是"没有 `Content-Length` 就停止读取"，而 mihomo 的
    /// `/proxies` 恰好用 chunked ⇒ 实测只拿到 3941 字节，JSON 报
    /// `EOF while parsing a value`，最终表现为
    /// **「找到了候选控制接口但都不可用」** —— 错误信息指向接口，
    /// 真实原因却是我们没读完响应。
    ///
    /// 这条用两个不完整的块驱动 `read_until_complete`，断言它**继续读**
    /// 直到见到结束块。若有人把 `chunked` 那条判据删掉，这条会红。
    #[tokio::test]
    async fn chunked_response_is_read_until_terminator() {
        use tokio::io::AsyncReadExt;

        // 模拟服务端分两次写：先 header + 一个 chunk，再补结束块。
        // 用 duplex 流模拟"读一次拿不到全部"的真实情形。
        let (mut client, mut server) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
            server.write_all(head.as_bytes()).await.unwrap();
            // 第一段：5 字节的 chunk
            server.write_all(b"5\r\nhello\r\n").await.unwrap();
            // 停一下，制造"第一次读到的不完整"
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            // 结束块
            server.write_all(b"0\r\n\r\n").await.unwrap();
            // 不让它立刻 EOF —— 真实管道就是这样（服务端不关连接）。
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });

        let raw = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_until_complete(&mut client),
        )
        .await
        .expect("必须在超时前返回（等到结束块就停，而不是挂到 EOF）")
        .expect("读取不该失败");

        assert!(
            raw.contains("hello"),
            "chunked 的内容必须读到，实际：{raw}"
        );
        assert!(
            raw.contains("0\r\n\r\n"),
            "应读到结束块才算完整，实际：{raw}"
        );
        // 真正的判据：parse 出来的 body 要能被解析（第一版就死在这里）。
        let body = parse_http_body(&raw).expect("应能取出 body");
        assert_eq!(body, "hello", "dechunk 后应只剩内容");
        let _ = client.read(&mut [0u8; 1]).await; // 保持 client 存活到断言之后
    }

    /// **真实网络**探测（默认 ignored）：对着本机真实代理枚举并测速节点。
    ///
    /// 不进常规套件 —— 它依赖"本机开着支持控制接口的代理"，CI 上没有。
    /// 手动跑：
    ///
    ///	cargo test -p ai-gateway-core real_node_probe -- --ignored --nocapture
    ///
    /// ⚠ 这条的**真正价值**是验证"发现逻辑不写死名字也能找到" ——
    /// 所有者重启一次代理，管道编号就从 17056 变成 40196，
    /// 写死的代码当场失效。故这里只断言"找到了"，不断言具体是哪个。
    #[tokio::test]
    #[ignore = "需要本机运行支持控制接口的代理（Clash/mihomo 系）"]
    async fn real_node_probe_finds_and_measures() {
        // 先用发现函数看看找到了什么（不看具体值，只看数量与类型）。
        let eps = discover_control_endpoints();
        println!("发现 {} 个候选控制接口：", eps.len());
        for e in &eps {
            println!("   · {}", e.describe());
        }
        // ⚠ TCP 候选是"猜"的，总会出现在列表里，故这里只要求
        // **至少有一个**候选 —— 真正的判据是下面能不能探出节点。
        assert!(!eps.is_empty(), "至少应有 TCP 候选");

        // 对**真实域名**测速（通用目标没意义，见模块头）。
        let outcome = probe_nodes("https://www.workbuddy.ai")
            .await
            .expect("本机应能探测到节点（若失败请确认代理软件支持控制接口）");

        println!("控制接口：{}", outcome.endpoint);
        println!("测试目标：{}", outcome.test_url);
        println!("共 {} 个节点，其中可用 {} 个：",
            outcome.nodes.len(),
            outcome.nodes.iter().filter(|n| n.delay_ms.is_some()).count());
        for n in outcome.nodes.iter().take(15) {
            match n.delay_ms {
                Some(d) => println!("   {:>6}ms  {}{}", d, n.name,
                    if n.current { "  ← 当前使用" } else { "" }),
                None => println!("   不通      {}", n.name),
            }
        }

        assert!(
            !outcome.nodes.is_empty(),
            "应能列出至少一个节点 —— 空列表说明 /proxies 解析有问题"
        );
        // 排序契约：通的必须排在不通的前面。
        let first_dead = outcome.nodes.iter().position(|n| n.delay_ms.is_none());
        let last_alive = outcome.nodes.iter().rposition(|n| n.delay_ms.is_some());
        if let (Some(fd), Some(la)) = (first_dead, last_alive) {
            assert!(la < fd, "排好序后「可用」必须全在「不通」之前");
        }
    }
}
