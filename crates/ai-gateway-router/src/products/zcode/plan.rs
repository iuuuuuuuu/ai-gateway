//! 套餐类型判定（Go `internal/zcode/plan.go` 的对应物，250 行）。
//!
//! 上游在 `GET /api/v1/client/configs` 的 `data.configs.startPlanPreview` 里
//! 写明了「新账号体验套餐」在各模型上的**赠送量**。拿它与额度查询回来的
//! 实际总量比对，就能判出这个账号现在是什么套餐 —— **不硬编码数字**
//! （3,000,000 / 5,000,000 是上游可改的运营参数）。
//!
//! # 为什么需要它
//!
//! 所有者的实测反馈：「这个体验套餐是 9月23号23:59 过期时间，这是我刚登录的
//! 新账号赠送的额度，要区分好」。体验套餐到期后**未用完的部分会失效**，而
//! 付费套餐是按周期续期的 —— 两者在界面上的含义完全不同，判错会给出
//! **错误的到期警告**。
//!
//! # 判据为什么用 `==` 而不是 `<=`
//!
//! 付费套餐的总额通常**大于**赠送量，用 `<=` 会把「刚订阅的小额付费套餐」
//! 误判成体验套餐。漏判（返回 `Paid` 而实际是 `Trial`）只是没标出体验套餐，
//! 误判则会给出错误的到期警告 —— 两害相权取其轻。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::client::{self, Client};

/// 体验套餐配置的缓存时长。对应 Go `startPlanTTL = time.Hour`（`plan.go:99`）。
///
/// 为什么不每次查：这是**静态配置**（运营参数），而且 `quota` 子命令每次调用
/// 都会走到这里 —— 每次都多一个上游往返，白白拖慢账号页刷新。
const START_PLAN_TTL: Duration = Duration::from_secs(3600);

/// 取体验套餐配置的超时。对应 Go `context.WithTimeout(ctx, 20*time.Second)`。
const START_PLAN_TIMEOUT: Duration = Duration::from_secs(20);

/// 响应体上限。对应 Go `io.LimitReader(resp.Body, 8<<20)`。
const MAX_START_PLAN_BODY: usize = 8 << 20;

/// 套餐类型。对应 Go `PlanKind`（`plan.go:61-70`）。
///
/// ★ 序列化值就是 Go 的字符串常量 —— 宿主 `zcode_account.rs` 直接把它写进
/// 账号库的 `planKind` 字段，界面据此区分体验/付费/按量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PlanKind {
    /// 判不出来。**不猜** —— 上游配置取不到时就是这个。
    #[default]
    #[serde(rename = "unknown")]
    Unknown,
    /// 体验套餐（新账号赠送，每日重置，到期后未用完的部分失效）。
    #[serde(rename = "trial")]
    Trial,
    /// 付费套餐（按订阅周期发放与续期）。
    #[serde(rename = "paid")]
    Paid,
    /// 按量付费（API Key）：没有预发额度，按实际用量计费。
    #[serde(rename = "api_key")]
    ApiKey,
}

impl PlanKind {
    /// 落盘/输出用的字符串。与 Go 的 `string(kind)` 逐字一致。
    pub fn as_str(self) -> &'static str {
        match self {
            PlanKind::Unknown => "unknown",
            PlanKind::Trial => "trial",
            PlanKind::Paid => "paid",
            PlanKind::ApiKey => "api_key",
        }
    }

    /// 界面上的类型名。对应 Go `PlanLabel`（`plan.go:222-233`）。
    ///
    /// ★ `#[allow(dead_code)]`：`quota` 子命令只把 `as_str()`（`"trial"` 等）
    /// 交给宿主，**中文标签由宿主自己映射**（`zcode_login.rs` 侧决定措辞）。
    /// 留着是因为这两个函数是「Go 侧同一概念的唯一出处」：措辞若将来要在
    /// Rust 侧用（例如 `warm` 的 message），照抄这里比重新发明准确。
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            PlanKind::Trial => "体验套餐",
            PlanKind::Paid => "付费套餐",
            PlanKind::ApiKey => "按量付费",
            PlanKind::Unknown => "未知",
        }
    }

    /// 界面上的补充说明。对应 Go `PlanNote`（`plan.go:239-250`）。
    ///
    /// ★ 体验套餐那句「到期后未用完的部分会失效」是**重点**：用户的直觉是
    /// 「额度被用完了」，而真相是到期作废 —— 不说清他会以为是程序算错了。
    #[allow(dead_code)]
    pub fn note(self) -> &'static str {
        match self {
            PlanKind::Trial => {
                "新账号赠送的体验额度：每日重置，且到期后未用完的部分会失效，不是被用完了。"
            }
            PlanKind::Paid => "付费套餐额度：按订阅周期发放与续期。",
            PlanKind::ApiKey => "按量付费（API Key）：没有预发额度，按实际用量计费。",
            PlanKind::Unknown => "未能识别套餐类型（可能是上游配置取不到）。",
        }
    }
}

/// 体验套餐里某个模型上的赠送量。对应 Go `StartPlanEntitlement`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StartPlanEntitlement {
    #[serde(rename = "showName", default)]
    pub show_name: String,
    #[serde(rename = "grantUnits", default)]
    pub grant_units: f64,
    #[serde(default)]
    pub period: String,
    #[serde(rename = "unitType", default)]
    pub unit_type: String,
}

/// 体验套餐的静态描述。对应 Go `StartPlanPreview`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StartPlanPreview {
    #[serde(default)]
    pub name: String,
    #[serde(rename = "planId", default)]
    pub plan_id: String,
    #[serde(default)]
    pub entitlements: Vec<StartPlanEntitlement>,
}

/// 进程级缓存：`(配置, 写入时刻)`。
///
/// ★ 与 Go 一样**只缓存成功**。Go 里那个 `startPlanFail` 写了却从没被读过
/// （`plan.go:98` 声明、`:130` 赋值，全文件再无引用），是死代码；Rust 侧
/// 不移植它 —— 缓存失败会让「上游临时抖动」变成「一小时内永远取不到」。
type Cache = Option<(StartPlanPreview, Instant)>;

fn cache() -> &'static Mutex<Cache> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// 清空缓存（**只给测试用**）。
///
/// 生产路径上缓存是 1 小时的静态配置，没有失效需求；但测试之间会互相污染
/// （同一个 `Client` 的 override 指向不同假上游时，前一个测试的成功结果会
/// 让后一个测试根本发不出请求）。
#[cfg(test)]
pub(crate) fn clear_cache_for_test() {
    if let Ok(mut g) = cache().lock() {
        *g = None;
    }
}

impl Client {
    /// 取体验套餐的静态描述（带 1 小时缓存）。对应 Go `FetchStartPlanPreview`
    /// （`plan.go:102-162`）。
    ///
    /// # host 的选择
    ///
    /// Go 把 URL 硬编码成 `https://zcode.z.ai/api/v1/client/configs`（不经任何
    /// base 推导）。Rust 侧走 [`Client::config_origin`] —— 它的生产取值**就是**
    /// 同一个常量 [`client::CONFIG_ORIGIN`]，但可被 `set_control_base_for_test`
    /// 重定向，否则这个函数根本无法测试。
    pub(crate) async fn fetch_start_plan_preview(&self) -> Result<StartPlanPreview, String> {
        if let Ok(g) = cache().lock() {
            if let Some((sp, at)) = g.as_ref() {
                if at.elapsed() < START_PLAN_TTL {
                    return Ok(sp.clone());
                }
            }
        }

        // ★ 平台/版本取值说明：Go 在这里硬编码 `app_version=DefaultAppVersion` +
        // `platform=win32-x64`。Rust 侧用 `identity` 的同名字段 —— 在受支持平台
        //（Windows x64）上两者**逐字相同**（`platform_arch()` 就是 Node 命名的
        // `win32-x64`），且与 `client.rs::fetch_config_models` 打同一个端点时的
        // 取值保持一致。上游严格校验这两个值（错值 ⇒ 3001），故不能各写各的。
        let url = format!(
            "{}{}?app_version={}&platform={}",
            self.config_origin(),
            client::CONFIG_PATH,
            self.identity.app_version,
            self.identity.platform_arch()
        );

        let mut h = reqwest::header::HeaderMap::new();
        // ★ 刻意**不发** `X-Device-Mid`（Go 侧也没发）：这是静态配置查询，
        // 与「哪个账号/哪台设备」无关。多发一个设备头只是多一个可区分特征。
        client::put(&mut h, "accept", "application/json")?;
        for (k, v) in self.identity.headers() {
            client::put(&mut h, &k, &v)?;
        }

        let resp = self
            .http
            .get(&url)
            .headers(h)
            .timeout(START_PLAN_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("取体验套餐配置失败: {e}"))?;
        let status = resp.status().as_u16();
        let raw = client::read_limited(resp, MAX_START_PLAN_BODY).await;

        if status >= 400 {
            return Err(format!("取体验套餐配置失败（HTTP {status}）"));
        }
        // ⚠ 刻意**不带**原始内容（Go `plan.go:148` 的 `体验套餐配置解析失败: %w` 同款）：
        // 这个错误最终只会被 `classify_plan` 的调用方丢成 `PlanKind::Unknown`，
        // 正文塞进去没人看得到，反而让这条路径与 Go 分叉。
        let doc: Value = serde_json::from_slice(&raw)
            .map_err(|e| format!("体验套餐配置解析失败: {e}"))?;

        let sp = doc
            .pointer("/data/configs/startPlanPreview")
            .cloned()
            .map(|v| serde_json::from_value::<StartPlanPreview>(v).unwrap_or_default())
            .unwrap_or_default();
        if sp.entitlements.is_empty() {
            return Err("上游配置里没有 startPlanPreview".to_string());
        }

        if let Ok(mut g) = cache().lock() {
            *g = Some((sp.clone(), Instant::now()));
        }
        Ok(sp)
    }
}

/// 判定套餐类型。对应 Go `ClassifyPlan`（`plan.go:169-205`）。
///
/// ```text
/// q == None              ⇒ Unknown（连额度都没有，无从判起）
/// q.entries 为空         ⇒ ApiKey （按量付费账号没有任何预发额度桶）
/// sp 取不到/无 entitlements ⇒ Unknown（**不猜**）
/// 每个桶都能与赠送量对上  ⇒ Trial
/// 否则                   ⇒ Paid
/// ```
///
/// ★ 「每个桶都能对上」是**全称**判定：只要有一个桶对不上，就是付费套餐。
/// 体验套餐的桶是上游按 `startPlanPreview` 精确发放的，不会多也不会少。
pub fn classify_plan(q: Option<&client::Quota>, sp: Option<&StartPlanPreview>) -> PlanKind {
    let Some(q) = q else {
        return PlanKind::Unknown;
    };
    if q.entries.is_empty() {
        // 没有任何额度桶 ⇒ 这个账号没领过预发额度，是按量付费。
        return PlanKind::ApiKey;
    }
    let Some(sp) = sp else {
        return PlanKind::Unknown;
    };
    if sp.entitlements.is_empty() {
        return PlanKind::Unknown;
    }

    let mut matched = 0usize;
    for e in &q.entries {
        for g in &sp.entitlements {
            if !same_model_name(&e.show_name, &g.show_name) {
                continue;
            }
            // ★ `==` 不是 `<=`，且要求 `total > 0`；内层**只试第一个同名项**
            // 就 break（Go 侧同款）—— 同名项若有多个，后面的不算数。
            if e.total as f64 == g.grant_units && e.total > 0 {
                matched += 1;
            }
            break;
        }
    }
    if matched == q.entries.len() {
        PlanKind::Trial
    } else {
        PlanKind::Paid
    }
}

/// 模型名归一后比较。对应 Go `sameModelName`（`plan.go:210-219`）。
///
/// 归一：转小写 → 去首尾空白 → 依次去掉 `-`、`_`、空格。
/// 上游在额度桶里写 `GLM-5.3`、在配置里可能写 `glm 5.3`，不归一会全判不中。
pub fn same_model_name(a: &str, b: &str) -> bool {
    normalize_model_name(a) == normalize_model_name(b)
}

fn normalize_model_name(s: &str) -> String {
    s.to_lowercase()
        .trim()
        .chars()
        .filter(|c| *c != '-' && *c != '_' && *c != ' ')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::products::zcode::client::{Quota, QuotaEntry};

    fn entry(show_name: &str, total: i64) -> QuotaEntry {
        QuotaEntry {
            show_name: show_name.to_string(),
            total,
            ..Default::default()
        }
    }

    /// 上游真实的 `startPlanPreview`（字段名与数值逐字抄 Go `plan_test.go:22-38`：
    /// GLM-5.3 赠送 3,000,000，GLM-5.3-Flash 赠送 5,000,000）。
    fn trial_fixture() -> StartPlanPreview {
        StartPlanPreview {
            name: "免费 GLM 旗舰模型体验".to_string(),
            plan_id: "start-plan".to_string(),
            entitlements: vec![
                StartPlanEntitlement {
                    show_name: "GLM-5.3".to_string(),
                    grant_units: 3_000_000.0,
                    period: "daily".to_string(),
                    unit_type: "token".to_string(),
                },
                StartPlanEntitlement {
                    show_name: "GLM-5.3-Flash".to_string(),
                    grant_units: 5_000_000.0,
                    period: "daily".to_string(),
                    unit_type: "token".to_string(),
                },
            ],
        }
    }

    fn quota(entries: Vec<QuotaEntry>) -> Quota {
        Quota {
            entries,
            ..Default::default()
        }
    }

    #[test]
    fn kinds_match_the_go_string_constants() {
        // 宿主 `zcode_account.rs` 直接把这些字符串写进账号库，改动会静默失效。
        assert_eq!(PlanKind::Unknown.as_str(), "unknown");
        assert_eq!(PlanKind::Trial.as_str(), "trial");
        assert_eq!(PlanKind::Paid.as_str(), "paid");
        assert_eq!(PlanKind::ApiKey.as_str(), "api_key");
    }

    #[test]
    fn nil_quota_is_unknown_but_empty_entries_is_api_key() {
        // ★ 两者看着像，语义完全不同：没有额度对象 ⇒ 判不了；
        // 有对象但一个桶都没有 ⇒ 这就是按量付费账号的形态。
        assert_eq!(classify_plan(None, Some(&trial_fixture())), PlanKind::Unknown);
        assert_eq!(
            classify_plan(Some(&quota(vec![])), Some(&trial_fixture())),
            PlanKind::ApiKey
        );
    }

    #[test]
    fn missing_or_empty_start_plan_is_unknown_never_guessed() {
        // Go `plan_test.go:100`：空 entitlements ⇒ Unknown，不许猜。
        assert_eq!(
            classify_plan(Some(&quota(vec![entry("GLM-5.3", 3_000_000)])), None),
            PlanKind::Unknown
        );
        assert_eq!(
            classify_plan(
                Some(&quota(vec![entry("GLM-5.3", 3_000_000)])),
                Some(&StartPlanPreview::default())
            ),
            PlanKind::Unknown
        );
    }

    #[test]
    fn every_bucket_matching_the_grant_units_is_trial() {
        let q = quota(vec![
            entry("GLM-5.3", 3_000_000),
            entry("GLM-5.3-Flash", 5_000_000),
        ]);
        assert_eq!(classify_plan(Some(&q), Some(&trial_fixture())), PlanKind::Trial);
    }

    #[test]
    fn one_bucket_not_matching_is_paid() {
        // 全称判定：有一个桶对不上 ⇒ 付费套餐。
        let q = quota(vec![
            entry("GLM-5.3", 3_000_000),
            entry("GLM-5.3-Flash", 9_999_999),
        ]);
        assert_eq!(classify_plan(Some(&q), Some(&trial_fixture())), PlanKind::Paid);
    }

    #[test]
    fn a_larger_paid_bucket_is_not_mistaken_for_trial() {
        // ★ 这是「用 `==` 而不是 `<=`」的理由：付费套餐总额**大于**赠送量。
        // 若写成 `<=`，这条会被判成体验套餐 ⇒ 给出错误的到期警告。
        let q = quota(vec![entry("GLM-5.3", 50_000_000)]);
        assert_eq!(classify_plan(Some(&q), Some(&trial_fixture())), PlanKind::Paid);
    }

    #[test]
    fn a_smaller_bucket_is_paid_too() {
        // 反向同理：小于赠送量也对不上（`==` 是双向的）。
        let q = quota(vec![entry("GLM-5.3", 100)]);
        assert_eq!(classify_plan(Some(&q), Some(&trial_fixture())), PlanKind::Paid);
    }

    #[test]
    fn a_zero_total_bucket_never_counts_as_trial() {
        // `total > 0` 那条守卫：赠送量为 0 的条目不该让「total=0」的桶算命中。
        let sp = StartPlanPreview {
            entitlements: vec![StartPlanEntitlement {
                show_name: "GLM-5.3".to_string(),
                grant_units: 0.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let q = quota(vec![entry("GLM-5.3", 0)]);
        assert_eq!(classify_plan(Some(&q), Some(&sp)), PlanKind::Paid);
    }

    #[test]
    fn matching_ignores_case_and_separators() {
        // 上游额度桶写 `GLM-5.3`，配置里写 `glm 5.3` —— 必须判为同一个模型。
        assert!(same_model_name("GLM-5.3", "glm 5.3"));
        assert!(same_model_name("GLM-5.3-Flash", "glm_5.3_flash"));
        assert!(same_model_name("  GLM-5.3  ", "GLM-5.3"));
        assert!(!same_model_name("GLM-5.3", "GLM-5.3-Flash"));
    }

    #[test]
    fn only_the_first_same_named_entitlement_is_considered() {
        // Go 侧内层命中同名后 `break`：同名项有多个时，后面的不算数。
        // 构造两个同名项，第一个对不上、第二个对得上 —— 结果应是 Paid。
        let sp = StartPlanPreview {
            entitlements: vec![
                StartPlanEntitlement {
                    show_name: "GLM-5.3".to_string(),
                    grant_units: 1.0,
                    ..Default::default()
                },
                StartPlanEntitlement {
                    show_name: "GLM-5.3".to_string(),
                    grant_units: 3_000_000.0,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let q = quota(vec![entry("GLM-5.3", 3_000_000)]);
        assert_eq!(classify_plan(Some(&q), Some(&sp)), PlanKind::Paid);
    }

    #[test]
    fn labels_and_notes_are_non_empty_for_every_kind() {
        for k in [
            PlanKind::Unknown,
            PlanKind::Trial,
            PlanKind::Paid,
            PlanKind::ApiKey,
        ] {
            assert!(!k.label().is_empty(), "{k:?} 的 label 不能为空");
            assert!(!k.note().is_empty(), "{k:?} 的 note 不能为空");
        }
    }

    #[test]
    fn plan_kind_serializes_with_the_go_strings() {
        // 宿主按字符串读 `planKind`，serde 的 rename 必须与 `as_str()` 一致。
        for k in [
            PlanKind::Unknown,
            PlanKind::Trial,
            PlanKind::Paid,
            PlanKind::ApiKey,
        ] {
            let v = serde_json::to_value(k).expect("应能序列化");
            assert_eq!(v.as_str().unwrap_or(""), k.as_str(), "{k:?} 的序列化值不一致");
        }
    }

    // ==================== 真发请求那条路径 ====================
    //
    // 上面全是纯函数测试；`fetch_start_plan_preview` 的网络路径（URL 拼装、
    // 形状守卫、缓存）此前**一条都没覆盖** —— 而「query 少了 app_version 上游
    // 回 3001」正是这条路径最容易出错的地方。

    /// 一个只回固定响应的假上游，返回它的 base URL。
    ///
    /// ★ 为什么手写 HTTP/1.1 而不用 axum：`tokio` 的 feature 里**没有 `net`**
    /// （见 `Cargo.toml`），拿不到 `tokio::net::TcpListener`；`std::net::TcpListener`
    /// 不受该 feature 门控（它只门控 `tokio::net`）。一个 `std::thread` 足够，
    /// 且**不触网**（只监听 127.0.0.1）。与 `client.rs:2448` 的
    /// `spawn_fake_upstream` 同款 —— 那份是私有的，且行为绑死在验证码场景上。
    ///
    /// ★ `Connection: close` 不能省：reqwest 默认复用连接，假上游发完不回
    /// 就会让下一次请求挂住（`client.rs` 当年踩过）。
    fn spawn_stub_upstream(
        listener: std::net::TcpListener,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        status: u16,
        body: &'static str,
    ) -> String {
        let addr = format!("http://{}", listener.local_addr().expect("local_addr"));
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            for conn in listener.incoming() {
                let Ok(mut s) = conn else { continue };
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    match s.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let end = buf
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n".as_slice())
                    .unwrap_or(buf.len());
                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                // 记下整段请求头，供「有没有发 X-Device-Mid」这类断言用。
                seen.lock().unwrap().push(format!("{path}\n{head}"));
                let reason = if status == 200 { "OK" } else { "ERR" };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            }
        });
        addr
    }

    /// 起一个假上游 + 一个指向它的 `Client`，返回 `(client, seen)`。
    fn client_against_stub(status: u16, body: &'static str) -> (
        crate::products::zcode::client::Client,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let base = spawn_stub_upstream(l, seen.clone(), status, body);
        let mut c = crate::products::zcode::client::Client::new();
        c.set_control_base_for_test(&base);
        (c, seen)
    }

    /// 缓存是**进程级** `OnceLock`，而测试默认并行跑 ⇒ 必须串行化。
    ///
    /// 中毒时放行（`into_inner`）：一个测试 panic 不该让其余全红。
    static CACHE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_cache() -> std::sync::MutexGuard<'static, ()> {
        let g = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cache_for_test();
        g
    }

    /// 上游真实形状（`{code, data{configs{startPlanPreview}}}`）。
    const START_PLAN_BODY: &str = r#"{"code":0,"data":{"configs":{"startPlanPreview":{"name":"免费 GLM 旗舰模型体验","planId":"start-plan","entitlements":[{"showName":"GLM-5.3","grantUnits":3000000,"period":"daily","unitType":"token"}]}}}}"#;

    #[tokio::test]
    async fn fetch_start_plan_preview_carries_app_version_and_platform() {
        // ★ 实测：不带 `?app_version=&platform=` 时上游回 **3001 parameter error**。
        // 这条断言钉住 query 的两个键都在，且 `platform` 是 Node 命名的形态。
        let _g = lock_cache();
        let (c, seen) = client_against_stub(200, START_PLAN_BODY);
        let sp = c.fetch_start_plan_preview().await.expect("应能取到");
        assert_eq!(sp.name, "免费 GLM 旗舰模型体验");

        let path = seen.lock().unwrap()[0].clone();
        let url = path.lines().next().unwrap_or("");
        assert!(
            url.starts_with("/api/v1/client/configs?"),
            "路径不对：{url}"
        );
        assert!(url.contains("app_version="), "缺 app_version：{url}");
        assert!(url.contains("platform=win32-x64"), "platform 不对：{url}");
    }

    #[tokio::test]
    async fn fetch_start_plan_preview_does_not_send_x_device_mid() {
        // ★ 这是静态配置查询，与「哪台设备」无关；多发一个设备头只是多一个
        // 可区分特征（Go 侧也没发）。
        let _g = lock_cache();
        let (c, seen) = client_against_stub(200, START_PLAN_BODY);
        c.fetch_start_plan_preview().await.expect("应能取到");
        let raw = seen.lock().unwrap()[0].clone().to_ascii_lowercase();
        assert!(!raw.contains("x-device-mid"), "不该发 X-Device-Mid：{raw}");
    }

    #[tokio::test]
    async fn fetch_start_plan_preview_parses_the_entitlements() {
        let _g = lock_cache();
        let (c, _seen) = client_against_stub(200, START_PLAN_BODY);
        let sp = c.fetch_start_plan_preview().await.expect("应能取到");
        assert_eq!(sp.plan_id, "start-plan");
        assert_eq!(sp.entitlements.len(), 1);
        assert_eq!(sp.entitlements[0].show_name, "GLM-5.3");
        assert_eq!(sp.entitlements[0].grant_units, 3_000_000.0);
    }

    #[tokio::test]
    async fn fetch_start_plan_preview_is_cached_for_the_ttl() {
        // 这是静态运营配置，而 `quota` 子命令每次调用都会走到这里 ——
        // 不缓存会白白多一个上游往返、拖慢账号页刷新。
        let _g = lock_cache();
        let (c, seen) = client_against_stub(200, START_PLAN_BODY);
        c.fetch_start_plan_preview().await.expect("第一次应成功");
        c.fetch_start_plan_preview().await.expect("第二次应命中缓存");
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "第二次不该再打上游（缓存没生效）"
        );
    }

    #[tokio::test]
    async fn fetch_start_plan_preview_rejects_a_missing_start_plan_preview() {
        // ★ 形状守卫：上游没给 `startPlanPreview` 时报错，而不是回一个空壳 ——
        // 空壳会被 `classify_plan` 当成「有 sp 但没有 entitlements」，虽然也落到
        // Unknown，但错误信息会消失，排障时看不出是上游改了配置。
        let _g = lock_cache();
        let (c, _seen) = client_against_stub(200, r#"{"code":0,"data":{"configs":{}}}"#);
        let e = c.fetch_start_plan_preview().await.expect_err("应报错");
        assert_eq!(e, "上游配置里没有 startPlanPreview");
    }

    #[tokio::test]
    async fn fetch_start_plan_preview_reports_the_http_status() {
        let _g = lock_cache();
        let (c, _seen) = client_against_stub(500, "{}");
        let e = c.fetch_start_plan_preview().await.expect_err("应报错");
        assert_eq!(e, "取体验套餐配置失败（HTTP 500）");
    }

    #[tokio::test]
    async fn fetch_start_plan_preview_does_not_cache_a_failure() {
        // 失败不该写缓存 —— 否则上游短暂 5xx 会让接下来一小时都判 Unknown。
        let _g = lock_cache();
        let (c, seen) = client_against_stub(500, "{}");
        let _ = c.fetch_start_plan_preview().await;
        let _ = c.fetch_start_plan_preview().await;
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "失败被缓存了（第二次没重试）"
        );
    }
}
