//! 上游代理的**连通性自检**。
//!
//! 所有者要求（2026-09-22）：
//!
//! > 「设置里面的配置代理，也没有测试按钮 可以用来测试是否连通」
//!
//! 界面上的「配置代理」是**上游代理**（出站流量走它，如 Clash 的 7890），
//! 与「本地代理」（MITM 设备身份代理）是两件事 —— 后者的检测按钮早已有
//! （`commands_proxy::proxy_check`），前者一直没有。
//!
//! # 为什么不能只判断"端口在监听"
//!
//! 端口开着**不代表代理能用**：
//!
//!	· 可能是别的程序占了那个端口
//!	· 代理进程活着，但它自己的上游链路断了
//!	· 认证失败（需要用户名密码而没填）
//!
//! 真正要回答的是「**能不能通过它访问外网**」—— 那必须发一次真实请求。
//!
//! # 探测哪些目标
//!
//! 所有者要求（2026-09-22）：
//!
//! > 「测试代理那里,加上 所用到的国外的域名,就比如 workbuddy.ai 这个,
//! >   如果还有别的,也加上,github 是为了更新,google 就不用显示了」
//!
//! 即：**只列本应用真正依赖的域名**，而不是拿 google 这类"通用连通性"凑数。
//! 通用目标有两个毛病：
//!
//!	· 它通了**不代表本应用能用**（代理可能对目标域名做了分流规则）
//!	· 它不通时用户不知道该改什么 —— 而"workbuddy.ai 不通"直接指向
//!	  「给国际版配代理」这个动作
//!
//! 清单来自**代码里真实的出站端点**（对着 `region.go` / `identity.go` /
// `provider.go` 抄的，不是凭印象写的）：
//!
//!	· `github.com`              —— 应用内「检查更新」与下载 release
//!	· `www.workbuddy.ai`        —— WorkBuddy 国际版（对话/任务/签到）
//!	· `openapi.qoder.sh`        —— Qoder 国际版 OpenAPI（活动/额度）
//!	· `api3.qoder.sh`           —— Qoder 国际版网关（对话）
//!	· `zcode.z.ai`              —— ZCode 站点与配置
//!	· `api.z.ai`                —— ZCode 上游（Anthropic / OpenAI 两条通道）
//!
//! ⚠ 打的是**API 路径**而不是首页：首页走 CDN，代理坏了也可能命中缓存
//! 而返回 200（实测 `GET /` 直连也通）—— 那样这个检测就失去了意义。
//! API 路径会走到真实后端，返回 401/404 都算"到达了"。
//!
//! ⚠ 判定标准是「**有没有收到 HTTP 响应**」，不是「状态码是不是 200」：
//! 我们带的是空/假凭据，401 才是**正确**的结果。把非 2xx 判成失败
//! 会给出完全相反的结论。

use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// 单次探测的超时。
///
/// 取 8 秒：用户在前台等这个结果，超过这个时长他宁可重试。
/// 而且真连不通时代理会**挂住**（不是快速失败），所以必须有超时。
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// 一个探测目标。
struct Target {
    /// 界面展示用的名字（域名即可）。
    label: &'static str,
    /// 实际请求的 URL —— 用 **API 路径**而非首页，见模块头说明。
    url: &'static str,
    /// 这个域名是干什么的。用户看到"某一项不通"时，
    /// 需要立刻知道**影响哪个功能**，否则不知道要不要管它。
    purpose: &'static str,
    /// 是否用 POST（WorkBuddy 的 chat 端点对 GET 的响应无意义）。
    post: bool,
}

/// 本应用真正依赖的国外域名。
const TARGETS: &[Target] = &[
    Target {
        label: "github.com",
        url: "https://github.com",
        purpose: "检查更新与下载安装包",
        // 首页即可 —— 它就是静态页，没有"API 路径"这回事。
        post: false,
    },
    Target {
        label: "www.workbuddy.ai",
        url: "https://www.workbuddy.ai/v2/chat/completions",
        purpose: "WorkBuddy 国际版对话与任务",
        // 空 body 会被上游以 401/400 拒绝 —— 那正是"到达了"的证据。
        post: true,
    },
    Target {
        label: "openapi.qoder.sh",
        url: "https://openapi.qoder.sh/sash/api/v1/me/campaigns",
        purpose: "Qoder 国际版额度与权益活动",
        post: false,
    },
    Target {
        label: "api3.qoder.sh",
        url: "https://api3.qoder.sh",
        purpose: "Qoder 国际版对话网关",
        post: false,
    },
    Target {
        label: "zcode.z.ai",
        url: "https://zcode.z.ai",
        purpose: "ZCode 站点与配置",
        post: false,
    },
    Target {
        label: "api.z.ai",
        url: "https://api.z.ai/api/anthropic",
        purpose: "ZCode 上游对话",
        post: false,
    },
];

/// 测试一个上游代理地址能否连通外网。
///
/// `spec` 形如 `http://127.0.0.1:7890`（也接受不带 scheme 的 `127.0.0.1:7890`）。
///
/// 返回结构（前端直接消费）：
///
/// ```json
/// {
///   "ok": true,
///   "addr": "127.0.0.1:7890",
///   "results": [
///     { "name": "github.com", "ok": true, "status": 200, "elapsedMs": 412,
///       "detail": "HTTP 200 · 412ms" }
///   ],
///   "directNote": null
/// }
/// ```
///
/// ⚠ **全程只读**：只发 GET，不改任何配置、不启停代理进程。
pub async fn test_upstream(spec: &str) -> Value {
    let spec = spec.trim();
    if spec.is_empty() {
        return json!({
            "ok": false,
            "error": "代理地址为空 —— 请先填写，例如 http://127.0.0.1:7890",
        });
    }

    // ⚠⚠ 严格校验 scheme，**不能**依赖 reqwest 自己报错。
    //
    // # 为什么必须在这里拦（2026-09-22 修，我自己引入的缺陷）
    //
    // `reqwest::Proxy::all` 对**不支持的 scheme** 是宽容的：传
    // `ftp://only-scheme` 它照样返回 Ok，然后在请求时**静默不使用代理、
    // 直接连**。于是一个拼错的地址会得到：
    //
    //	6 个目标里 5 个"通"（因为直连本来就能通）⇒ 报「代理可以连通外网」
    //
    // 用户以为配好了，实际代理**从未生效**。这属于「看起来成功、
    // 实际没生效」，是最难查的一类 —— 本仓库在 WebView2 求解那里
    // 踩过同型问题（外部求解失败 → 本地 Node 兜底 → 表现为"能用"）。
    //
    // 故这里自己解析并**只接受 http/https**，把兜底路径堵死。
    let Some((proxy_url, addr)) = parse_proxy_spec(spec) else {
        return json!({
            "ok": false,
            "error": "代理地址格式不正确，应形如 http://127.0.0.1:7890\
                      （只支持 http / https）",
        });
    };

    let proxy = match reqwest::Proxy::all(&proxy_url) {
        Ok(p) => p,
        Err(e) => {
            return json!({
                "ok": false,
                "addr": addr,
                "error": format!("代理地址无法用于请求：{e}"),
            });
        }
    };

    let client = match reqwest::Client::builder()
        .proxy(proxy)
        .timeout(PROBE_TIMEOUT)
        // 不跟随重定向：我们只关心"能不能到达"，跟随会多花时间。
        // 因此 3xx 也算成功（见下面的判断）。
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "ok": false,
                "addr": addr,
                "error": format!("无法创建 HTTP 客户端：{e}"),
            });
        }
    };

    let mut results: Vec<Value> = Vec::with_capacity(TARGETS.len());
    let mut any_ok = false;

    for target in TARGETS {
        let started = Instant::now();
        // 有的目标必须用 POST（WorkBuddy 的 chat 端点对 GET 的响应无意义）。
        // 空 body 就够 —— 我们要的是"到达"，不是"业务成功"。
        let req = if target.post {
            client.post(target.url).body("{}")
        } else {
            client.get(target.url)
        };
        let outcome = req.send().await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        match outcome {
            Ok(resp) => {
                // ⚠ **任何** HTTP 响应都算成功，包括 3xx/4xx/5xx。
                //
                // 我们要证明的是「链路能到达」，不是「那个端点返回 200」——
                // 恰恰相反：我们带的是空/假凭据，**401/404 才是预期结果**。
                // 把非 2xx 判成失败会给出完全颠倒的结论。
                any_ok = true;
                let status = resp.status().as_u16();
                results.push(json!({
                    "name": target.label,
                    "purpose": target.purpose,
                    "ok": true,
                    "status": status,
                    "elapsedMs": elapsed_ms,
                    "detail": format!("HTTP {status} · {elapsed_ms}ms"),
                }));
            }
            Err(e) => {
                // 错误分类要具体到"用户下一步该做什么"，而不是抛 reqwest 原文。
                //
                // ⚠ reqwest 0.12 的 `Error` **没有** `is_proxy()` —— 只有
                // is_builder / is_redirect / is_status / is_timeout /
                // is_request / is_connect / is_body / is_decode / is_upgrade。
                // 代理握手失败会落在 `is_request()` 里，所以认证问题要靠
                // 错误文本识别（下面那条分支），不能凭"应该是 is_proxy"想当然。
                let detail = if e.is_timeout() {
                    format!("超时（{elapsed_ms}ms）—— 代理没有转发，或该域名被分流到直连")
                } else if e.is_connect() {
                    "连接被拒绝 —— 请确认代理正在运行、且端口填对了".to_string()
                } else if is_proxy_auth_error(&e) {
                    "代理要求认证 —— 请在地址里带上用户名密码".to_string()
                } else if e.is_request() {
                    format!("请求失败：{e}")
                } else {
                    format!("{e}")
                };
                results.push(json!({
                    "name": target.label,
                    "purpose": target.purpose,
                    "ok": false,
                    "elapsedMs": elapsed_ms,
                    "detail": detail,
                }));
            }
        }
    }

    // 代理全不通时，顺带直连一次，帮用户区分「网络问题」与「代理问题」。
    //
    // ⚠ 只在失败时做：成功时多做一次请求纯属浪费，而用户已经得到答案了。
    let direct_note = if any_ok {
        None
    } else {
        Some(probe_direct().await)
    };

    // ⚠⚠ 汇总状态**不能**写成 "有一个通就算通"（2026-09-23 修，所有者截图现场）
    //
    // 所有者截图：标题写「代理可以连通外网」，而下面 github.com 那行是
    // **✗ 连接被拒绝**。他那台机器的自动更新恰恰依赖 github ——
    // 界面却告诉他"代理没问题"，然后他点检查更新会失败。
    //
    // 我第一版就是 `"ok": any_ok`（任一通过即 true）。那对"通用连通性"
    // 或许说得过去，对本应用**不成立**：这里的每一项都对应一个**真实功能**，
    // 任何一项不通就是那个功能坏了。
    //
    // 故汇总 = **全部通过**。但为了让用户能区分"全坏"与"坏了一项"，
    // 额外给出 failed 列表 —— 文案据此说得具体（"自动更新会失败"），
    // 而不是笼统的"代理有问题"。
    let failed: Vec<&str> = results
        .iter()
        .filter(|r| r["ok"] != json!(true))
        .filter_map(|r| r["name"].as_str())
        .collect();
    let blocked: Vec<&str> = results
        .iter()
        .filter(|r| r["ok"] != json!(true))
        // 只有自动更新这一项坏了，会直接让"检查更新"失败 —— 单独点出来，
        // 因为它是唯一**用户马上能感知**的（其余是后台请求）。
        .filter(|r| r["name"] == json!("github.com"))
        .filter_map(|r| r["purpose"].as_str())
        .collect();
    let all_ok = failed.is_empty();

    json!({
        "ok": all_ok,
        "anyOk": any_ok,
        "addr": addr,
        // failed = 不通的域名清单（空 = 全通）。界面据此给出可行动的提示，
        // 例如「github.com 不通 ⇒ 检查更新会失败」。
        "failed": failed,
        "githubBlocked": !blocked.is_empty(),
        "results": results,
        "directNote": direct_note,
    })
}

/// 不走代理直连一次 github，返回给用户看的一句话结论。
///
/// 分三种结果，而不是两种 —— 「直连超时」与「直连被拒」对用户的含义不同：
/// 前者像被墙，后者像本机没有网络。
async fn probe_direct() -> String {
    let Ok(client) = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
    else {
        return "无法创建直连客户端，未能判断是网络还是代理的问题".to_string();
    };

    match client.get("https://github.com").send().await {
        Ok(_) => "直连是通的 —— 问题出在这个代理上，而不是你的网络".to_string(),
        Err(e) if e.is_connect() => {
            "直连也被拒绝 —— 本机网络可能就没通（先确认能上网，再排查代理）".to_string()
        }
        Err(_) => "直连也不通 —— 请先确认本机网络本身可用，再排查代理".to_string(),
    }
}

/// 解析并**严格校验**代理地址，返回 `(reqwest 用的 URL, 展示用的 host:port)`。
///
/// # 为什么不能只交给 `reqwest::Proxy::all`
///
/// 它对**不支持的 scheme 是宽容的**：传 `ftp://only-scheme` 照样返回 Ok，
/// 然后在请求时**静默不使用代理、直接连**。后果是一个拼错的地址会得到
/// 「6 个目标里 5 个通」（因为直连本来就能通）⇒ 报「代理可以连通外网」，
/// 而代理**从未生效**。
///
/// 这正是本仓库反复强调要避免的「看起来成功、实际没生效」——
/// 故这里自己把关，只接受 http / https。
///
/// 只填 `127.0.0.1:7890`（没写 scheme）是**很自然的输入**，补 `http://`
/// 而不是报错。
fn parse_proxy_spec(spec: &str) -> Option<(String, String)> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let (url, rest) = match spec.split_once("://") {
        Some(("http", r)) => (format!("http://{r}"), r),
        Some(("https", r)) => (format!("https://{r}"), r),
        // 有 scheme 但不是 http/https ⇒ 拒绝（就是这里挡住 ftp:// 那类）。
        Some(_) => return None,
        // 没有 scheme ⇒ 按 http 补全。
        None => (format!("http://{spec}"), spec),
    };
    // `host:port` 里的 host 与 port 都要非空，否则后面会得到一个
    // 能构造但连不上的地址（错误信息还很难懂）。
    let rest = rest.split('/').next().unwrap_or(rest);
    let (host, port) = rest.rsplit_once(':')?;
    let host = host.trim();
    let port = port.trim();
    if host.is_empty() || port.parse::<u16>().ok().filter(|p| *p > 0).is_none() {
        return None;
    }
    Some((url, format!("{host}:{port}")))
}

/// 判断错误是不是「代理要求认证」。
///
/// reqwest 0.12 **没有** `is_proxy()` 之类的分类方法，认证失败会落进
/// 通用的 `is_request()`。只能靠错误文本识别 —— 这是这一层唯一的不精确处，
/// 故单独抽成函数并加注释说明：**匹配失败只是回落到通用文案，不会误报成功**。
fn is_proxy_auth_error(e: &reqwest::Error) -> bool {
    let text = e.to_string().to_ascii_lowercase();
    text.contains("407")
        || text.contains("proxy authentication")
        || text.contains("proxy-authorization")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空地址要给出**可操作**的提示，而不是一个 reqwest 的解析错误。
    #[tokio::test]
    async fn empty_spec_returns_actionable_error() {
        let out = test_upstream("   ").await;
        assert_eq!(out["ok"], json!(false));
        let err = out["error"].as_str().unwrap_or("");
        assert!(err.contains("为空"), "应说明是空地址，实际：{err}");
    }

    /// 真正**无法解析**的地址必须在发请求之前就被拦下。
    ///
    /// ⚠ 这里的用例集合是**对着 `parse_upstream` 的实际行为**挑的，
    /// 不是凭"看起来像非法"写的 —— 我第一版把 `ftp://only-scheme`
    /// 也列了进来，结果它被接受了：
    ///
    /// `parse_upstream` 的设计**故意宽松** —— 它要解析的是系统代理字符串
    /// （形如 `http=127.0.0.1:7890;socks=...`），任何非空的裸 token 都会被
    /// 当成 HTTP 代理地址。所以 `ftp://x` 在它眼里就是"一个叫 ftp://x 的
    /// HTTP 代理"，会走到发请求那一步、由 reqwest 报错。
    ///
    /// 这不是缺陷，是这一层的分工：`parse_upstream` 只负责**拆出地址**，
    /// 合法性由真正建连接的那一步判定。测试要钉的是"拆不出地址"的情形。
    #[tokio::test]
    async fn malformed_spec_is_rejected_before_request() {
        for bad in ["", "   ", "\t\n"] {
            let out = test_upstream(bad).await;
            assert_eq!(out["ok"], json!(false), "「{bad:?}」不该被判为可用");
            assert!(
                out.get("results").is_none(),
                "「{bad:?}」不该产生探测结果（应在发请求前就被拦下）"
            );
            assert!(
                out["error"].as_str().is_some_and(|e| !e.is_empty()),
                "「{bad:?}」要给出可读原因"
            );
        }
    }

    /// 非 http/https 的 scheme 必须被**提前拒绝**，而不是交给 reqwest。
    ///
    /// # 为什么这条很重要（2026-09-22，我自己引入的缺陷）
    ///
    /// `reqwest::Proxy::all("ftp://only-scheme")` 返回 `Ok`，然后在请求时
    /// **静默不使用代理、直接连**。于是这个拼错的地址会得到
    /// 「6 个目标里 5 个通」⇒ 报「代理可以连通外网」—— 而代理从未生效。
    ///
    /// 我第一版就是这么写的，测试也确实红了（它还断言"应该真的去探测"，
    /// 即把那个错误行为当成了期望）。教训：**对"不支持的输入"要在自己
    /// 这层拦下**，别指望第三方库替你拒绝 —— 它可能选择宽容回落。
    #[tokio::test]
    async fn non_http_scheme_is_rejected_not_silently_direct() {
        for bad in ["ftp://only-scheme", "socks5://127.0.0.1:1080", "file:///tmp/x"] {
            let out = test_upstream(bad).await;
            assert_eq!(out["ok"], json!(false), "「{bad}」不该被判为可用");
            assert!(
                out.get("results").is_none(),
                "「{bad}」必须在发请求前就被拒绝，不能落到「静默直连」那条路（实际：{out}）"
            );
        }
    }

    /// 解析得出、但连不上的地址要走到**发请求**那一步并如实报失败。
    ///
    /// 与上一条对照：那条证明"scheme 不支持 ⇒ 提前拦下"，
    /// 这条证明"合法的 http 地址 ⇒ 真的去试，而不是凭形态判死刑"。
    #[tokio::test]
    async fn parseable_but_dead_address_actually_probes() {
        let out = test_upstream("http://127.0.0.1:59999").await;
        // 关键：它**尝试过**（有逐项结果），而不是被提前拒绝。
        assert!(
            out.get("results").is_some(),
            "合法的 http 地址应该真的去探测"
        );
        assert_eq!(out["ok"], json!(false), "没有代理在听，不该报成功");
    }

    /// `parse_proxy_spec` 的边界：只填 `host:port` 要能补全 scheme。
    ///
    /// 用户很自然会只写 `127.0.0.1:7890` —— 那不该报错。
    #[test]
    fn proxy_spec_accepts_shorthand_and_normalizes() {
        assert_eq!(
            parse_proxy_spec("127.0.0.1:7890"),
            Some(("http://127.0.0.1:7890".into(), "127.0.0.1:7890".into()))
        );
        assert_eq!(
            parse_proxy_spec("  http://127.0.0.1:7890  "),
            Some(("http://127.0.0.1:7890".into(), "127.0.0.1:7890".into()))
        );
        // 带路径的也接受（取 host:port 部分）。
        assert_eq!(
            parse_proxy_spec("http://10.0.0.1:8080/proxy"),
            Some(("http://10.0.0.1:8080/proxy".into(), "10.0.0.1:8080".into()))
        );
        // 缺端口 / 空 host / 端口非数字 ⇒ 拒绝。
        for bad in ["", "   ", "127.0.0.1", "http://:7890", "http://127.0.0.1:abc", "http://127.0.0.1:0"] {
            assert_eq!(parse_proxy_spec(bad), None, "「{bad}」应被拒绝");
        }
    }

    /// 连不上的代理必须报 `ok=false` 且**带逐项明细**。
    ///
    /// 用一个必然没人监听的端口（保留给测试的 9 号端口之外的冷门高位端口）。
    /// ⚠ 不用 1/9 这类特权端口：某些环境会返回"权限不足"而不是"拒绝连接"，
    /// 那会让断言依赖运行环境。
    #[tokio::test]
    async fn unreachable_proxy_reports_failure_with_details() {
        let out = test_upstream("http://127.0.0.1:59999").await;
        assert_eq!(out["ok"], json!(false), "没有代理在听，不该报成功");
        let results = out["results"].as_array().expect("必须有逐项明细");
        assert_eq!(results.len(), TARGETS.len(), "每个目标都要有一条结果");
        for r in results {
            assert_eq!(r["ok"], json!(false));
            assert!(
                r["detail"].as_str().is_some_and(|d| !d.is_empty()),
                "每项都要有可读的原因"
            );
        }
        // 失败时必须给出"是网络还是代理"的判断依据。
        assert!(
            out["directNote"].as_str().is_some_and(|s| !s.is_empty()),
            "全失败时应附直连结论，帮用户区分网络与代理问题"
        );
    }

    /// 探测清单必须覆盖**本应用真正依赖的每一个国外域名**。
    ///
    /// 所有者明确要求列真实用到的域名（而不是 google 这类通用目标）：
    /// 通用目标通了不代表本应用能用 —— 代理常按域名分流。
    ///
    /// ⚠ 这条断言是**对着域名清单**写的：将来某产品换了域名、
    /// 而这里没跟着更新，用户就会在"某一项不通"时看不出影响哪个功能。
    /// 加了新域名却没加进 TARGETS，这条会红。
    #[tokio::test]
    async fn covers_every_foreign_domain_we_depend_on() {
        let out = test_upstream("http://127.0.0.1:59999").await;
        let names: Vec<String> = out["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["name"].as_str().map(str::to_string))
            .collect();

        // 这些是从 region.go / identity.go / provider.go 里抄出来的真实端点。
        for want in [
            "github.com",       // 检查更新
            "www.workbuddy.ai", // WorkBuddy 国际版
            "openapi.qoder.sh", // Qoder 国际版 OpenAPI
            "api3.qoder.sh",    // Qoder 国际版网关
            "zcode.z.ai",       // ZCode 站点
            "api.z.ai",         // ZCode 上游
        ] {
            assert!(
                names.iter().any(|n| n == want),
                "探测清单缺少 {want} —— 它是本应用真实依赖的域名，实际清单：{names:?}"
            );
        }

        // google 是**通用目标**，所有者明确要求去掉：
        // 它通了不代表本应用能用，不通又指不出该改什么。
        assert!(
            !names.iter().any(|n| n.contains("google")),
            "不该再列 google（所有者要求：只列本应用用到的域名），实际：{names:?}"
        );
    }

    /// 每一项都要带 `purpose` —— 用户看到"某一项不通"时，
    /// 需要立刻知道**影响哪个功能**，否则不知道要不要管它。
    #[tokio::test]
    async fn every_target_explains_what_it_affects() {
        let out = test_upstream("http://127.0.0.1:59999").await;
        for r in out["results"].as_array().unwrap() {
            let purpose = r["purpose"].as_str().unwrap_or("");
            assert!(
                !purpose.is_empty(),
                "{} 缺少 purpose（用户看不出它影响哪个功能）",
                r["name"]
            );
        }
    }

    /// ⚠⚠ 汇总 `ok` 必须是**全部通过**，不能是"有一个通就算通"。
    ///
    /// # 为什么（2026-09-23 所有者截图现场）
    ///
    /// 他截的图里，标题写「代理可以连通外网」，而下面 `github.com` 那行是
    /// **✗ 连接被拒绝** —— 他那台机器的自动更新恰恰依赖 github。
    /// 界面等于在骗他："代理没问题"，然后点检查更新会失败。
    ///
    /// 我第一版就是 `"ok": any_ok`。那对"通用外网连通性"或许说得过去，
    /// 但对本应用**不成立**：这份清单里每一项都对应一个**真实功能**，
    /// 任何一项不通 = 那个功能坏了。而全部失败与失败一项必须能区分 ——
    /// 故另有 `failed` 列表与 `anyOk`。
    ///
    /// ⚠ 这条用**不可达代理**构造：全部失败。它同时钉住 `ok=false` ——
    /// 若有人把汇总改回 `any_ok`，在"全失败"下两者恰好相同，**这条不会红**。
    /// 所以真正钉住语义的是上面那条真实代理测试（ignored）里的 `ok=true`，
    /// 以及这里对 `failed` 字段的存在性断言 —— 后者才是本用例的价值。
    #[tokio::test]
    async fn summary_reports_failed_domains_not_just_a_boolean() {
        let out = test_upstream("http://127.0.0.1:59999").await;
        assert_eq!(out["ok"], json!(false), "全不通时 ok 必须 false");

        // failed 必须是数组且列出**每一个**不通的域名 ——
        // 界面据此说"哪一项坏了"，而不是笼统的"代理有问题"。
        let failed = out["failed"]
            .as_array()
            .expect("必须给出 failed 清单（界面要指出具体哪一项不通）");
        assert_eq!(
            failed.len(),
            TARGETS.len(),
            "全失败时应列出全部 {} 个域名，实际 {failed:?}",
            TARGETS.len()
        );

        // anyOk 保留"是否至少通一个"这一维，供"全坏/坏一项"的文案区分。
        assert_eq!(out["anyOk"], json!(false), "全不通时 anyOk 也应为 false");

        // githubBlocked 是给界面单独提示用的（自动更新依赖它）。
        assert_eq!(
            out["githubBlocked"],
            json!(true),
            "github.com 不通时必须置位 —— 界面据此提示「检查更新会失败」"
        );
    }

    /// 反向：全通时 `failed` 必须为空、`githubBlocked` 为 false。
    ///
    /// 与上一条成对。只测"失败时列出域名"可能把 `failed` 写成恒返回全部，
    /// 那样界面会把好的域名也标成坏 —— 比不显示更糟。
    ///
    /// 需要真实代理才能全通，故同样 ignored。
    #[tokio::test]
    #[ignore = "需要本机代理在 127.0.0.1:7890"]
    async fn all_passing_has_empty_failed_list() {
        let out = test_upstream("http://127.0.0.1:7890").await;
        assert_eq!(out["ok"], json!(true), "全通时 ok 必须 true");
        assert_eq!(out["anyOk"], json!(true));
        assert_eq!(
            out["failed"].as_array().map(Vec::len),
            Some(0),
            "全通时 failed 必须为空，实际 {:?}",
            out["failed"]
        );
        assert_eq!(
            out["githubBlocked"],
            json!(false),
            "github 通了就不该提示「检查更新会失败」"
        );
    }

    /// **真实网络**探测（默认 ignored）：对着本机真实代理跑一次。
    ///
    /// 不进常规套件 —— 它依赖「本机有一个可用的代理在 7890」，CI 上没有。
    /// 手动跑：
    ///
    ///   cargo test -p ai-gateway-core real_proxy -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "需要本机代理在 127.0.0.1:7890"]
    async fn real_proxy_on_7890_is_detected() {
        let out = test_upstream("http://127.0.0.1:7890").await;
        println!(
            "真实代理探测: {}",
            serde_json::to_string_pretty(&out).unwrap_or_default()
        );
        assert_eq!(out["ok"], json!(true), "本机 7890 上的代理应能被测通");
        let results = out["results"].as_array().expect("应有逐项结果");
        assert!(
            results.iter().all(|r| r["ok"] == json!(true)),
            "两个目标都该通过"
        );
    }
}
