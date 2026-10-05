//! 账号激活（Go `internal/zcode/warm.go` 的对应物，433 行）。
//!
//! 先报两个官方激活事件，再领取上游给出的**每一个**套餐。
//!
//! # 为什么需要它
//!
//! 刚导入的账号在上游看来「从未活动过」：额度查得到、模型列得出，但活动套餐
//! 要么拿不到 offer、要么被判异常活动。官方客户端在用户首次启动时会报两个
//! 激活事件（`app_launch` / `app_daily_active`），本模块复刻的就是那个行为。
//!
//! # ⚠ 合规立场（不要放宽）
//!
//! 1. 触发时机只有**两种**：用户显式触发（账号页按钮 / CLI 子命令），或宿主
//!    启动后**每个账号各一次**的「启动激活」（见 `multi_product_credit_patrol`
//!    的 `warm_zcode_accounts`）。官方客户端本身就在用户首次启动时报这两个
//!    事件 —— 复刻「首次启动」是**还原真实行为**，不是制造活动；
//! 2. **绝不挂进 `claim_scheduler` 的定时轮询** —— 那会变成「每 5 分钟一次
//!    激活 + 领取」，与官方客户端的真实行为不符，而上游正是按活动模式判风控的。
//!    ⚠ 幂等标记 `warmed_at` 就是这条约束的落点（落账号库、跨进程有效），
//!    见 `zcode_account::ZcodeAccount::warmed_at` —— 改动前先读懂它；
//! 3. 上报这些事件是官方客户端**真实会做**的行为，不是伪造凭证、不是绕过风控。
//!    若上游仍把它判成风险，那也接受 —— 不加额外伪装或规避。
//!
//! # 与参考实现的三处刻意分歧
//!
//! 参考实现是 `test-bin/recon/zcode-D3-vin/internal/upstream/warm.go`：
//!
//! 1. 语言/时区用**我们自己** Identity 声明的那一对（默认 `zh-CN` /
//!    `Asia/Shanghai`），**不是**它硬编码的 `en-US` / `America/New_York`。
//!    理由见 [`Client::build_activation_event`]。
//! 2. 验证码是「铸了就用」，但**铸不出来不阻断** —— 照发，让上游如实回 3007。
//! 3. 事件与领取都走本 `Client` 的出口。
//!
//! # 事件为什么不成功就不领
//!
//! 激活事件是「让账号看起来活过」的前提，套餐领取是它的收益。事件没发出去就领，
//! 等于在一个上游看来仍是「从未活动过」的账号上做写操作 —— 那正是最容易被判
//! 异常的时序。宁可只报事件、把领取留给下次。

use serde::Serialize;
use serde_json::{Map, Value};

use super::claim::{ClaimError, PlanOffer, ClaimResult};
use super::cred::{self, ZcodeCred};
use super::identity;

/// 激活事件上报端点（拼在 `quota_host()` 之后）。
///
/// ★ 用的是 `quota_host()`（恒 `zcode.z.ai`）**不是** `biz_host`：Go 的
/// `eventHost(cr)` 与 `claimHost(cr)` 都返回 `cr.Provider.QuotaHost()`。
/// 用各自 biz_host 会打到另一个域，上游回 `200 {"code":500,"msg":"404 NOT_FOUND"}`。
pub(crate) const EVENT_REPORT_PATH: &str = "/api/v1/event/report";

/// 首次启动上报的两个事件。
///
/// ⚠ **顺序有意义，且是顺序发**（任一失败即停），不是并发：官方客户端是单线程
/// 依次发的；并发会让上游看到同一 `device_mid` 的两个请求同时到达。
pub(crate) const ACTIVATION_ELEMENTS: [&str; 2] = ["app_launch", "app_daily_active"];

/// 参考实现实测的 Windows 版本号。
///
/// 事件 body 必须是官方那 16 个字段的**完整形状**（上游按 `device_mid` + 日期
/// 去重，**缺字段本身就是可区分特征**），所以这里不像请求头那样「取不到就省略」，
/// 而是给一个**真实存在过**的值。Identity 能取到真实版本时优先用真实的。
const ACTIVATION_FALLBACK_OS_VERSION: &str = "10.0.26200";

/// 参考实现实测的分辨率。
///
/// 网关是独立进程，没有读取屏幕的能力；沿用参考实现那个真实存在过的值，而不是
/// 编一个新的。改它没有任何收益，只会让我们与参考实现产生分歧。
const ACTIVATION_SCREEN_RESOLUTION: &str = "1920x1080";

/// 事件 body 的上限（Go `io.LimitReader(resp.Body, 1<<20)`）。
const MAX_EVENT_BODY: usize = 1 << 20;

/// 官方 `sendReport` 的 16 字段 body。
///
/// ⚠ 用结构体而不是 `json!` 宏是刻意的：宏会漏字段（漏一个就成了可区分特征），
/// 结构体让「16 个字段全在」成为**编译期可见**的事实，测试也能逐字段钉住。
///
/// 注意两个字段的**类型不一样**，这是官方的真实形状，不要「顺手统一」：
///
/// ```text
/// event_extra_detail  对象   {}
/// marketing_params    字符串 "{}"
/// ```
#[derive(Debug, Clone, Serialize)]
struct ActivationEvent {
    event_id: String,
    client_timezone: String,
    client_language: String,
    element_name: String,
    event_region: String,
    event_type: String,
    event_text: String,
    event_extra_detail: Map<String, Value>,
    user_id: String,
    screen_resolution: String,
    app_version: String,
    device_os_category: String,
    device_os_version: String,
    device_mid: String,
    mac_id: String,
    marketing_params: String,
}

/// 单个套餐的领取结果（Go `WarmPlan`）。
///
/// `name` / `priority` / `already_claimed` / `message` 在 Go 上是 `omitempty`；
/// 序列化在 `cli.rs` 里手写，以复刻那个「空值就不出现」的行为。
#[derive(Debug, Clone, Default)]
pub(crate) struct WarmPlan {
    pub plan_id: String,
    pub name: String,
    pub priority: i64,
    pub ok: bool,
    pub already_claimed: bool,
    pub message: String,
}

/// 一次激活的结果（Go `WarmReport`）。
///
/// 口径与参考实现一致：**空串 = 成功**。
///
/// ```text
/// events   空 = 两个事件都成功发出去了
/// claim    空 = 没有出问题的套餐
/// skipped  空 = 领取阶段确实执行了
/// ```
#[derive(Debug, Clone, Default)]
pub(crate) struct WarmReport {
    /// 本报告对应的账号。
    ///
    /// ★ `#[allow(dead_code)]`：CLI 侧 `run_warm` 用的是**凭证里的** `cred.uid`
    /// （与 Go `runWarm` 的 `c.UID` 同款），不是这里回填的值 —— 两者在正常路径
    /// 下必然相同。留着是因为它是 `WarmAccount` 的**契约组成部分**：这个函数
    /// 接受任意 `&mut ZcodeCred`，报告自己说清「我是谁的」比让调用方假设
    /// 「一定就是传进去那个」更稳（将来若有批处理路径会需要它）。
    #[allow(dead_code)]
    pub uid: String,
    /// 事件阶段的问题描述；空串 = 两个事件都成功。
    pub events: String,
    /// 领取阶段的问题（各套餐失败原因的合并）；空串 = 全部成功。
    pub claim: String,
    /// 领取阶段被整体跳过的原因；空串 = 执行了。
    pub skipped: String,
    /// 上游实际给出的套餐与逐个结果（供界面展示）。
    pub plans: Vec<WarmPlan>,
    /// 成功领取的套餐 ID（含「此前已领过」这种幂等命中）。
    pub claimed: Vec<String>,
}

impl super::client::Client {
    /// 构造官方 `sendReport` 形状的事件 body。
    ///
    /// 语言/时区取**我们自己 Identity 声明的那一对**，而不是照抄参考实现硬编码的
    /// `en-US` / `America/New_York`。理由：参考实现自己在注释里写明了它遵循的规则
    /// ——「一个真实的区域对，且与 billing 头声明的是同一对」；它警告的是**同一个
    /// 账号不能出现两个区域对**。我们的对话路径（`Identity::headers`）声明的是
    /// `zh-CN` / `Asia/Shanghai`，遥测若报 `en-US` 就正好犯了它警告的那件事。
    fn build_activation_event(&self, element: &str, user_id: &str, device_mid: &str) -> ActivationEvent {
        let id = &self.identity;
        ActivationEvent {
            // event_id 每条事件一个新 UUID（官方用的是 UUID v4）。
            event_id: identity::new_device_mid(),
            client_timezone: first_non_empty(&[
                id.client_timezone.clone(),
                "Asia/Shanghai".to_string(),
            ]),
            client_language: first_non_empty(&[id.client_language.clone(), "zh-CN".to_string()]),
            element_name: element.to_string(),
            event_region: "app".to_string(),
            event_type: "view".to_string(),
            event_text: String::new(),
            event_extra_detail: Map::new(),
            user_id: user_id.to_string(),
            screen_resolution: ACTIVATION_SCREEN_RESOLUTION.to_string(),
            app_version: id.app_version.clone(),
            // OS 分类优先用真实值（与 Identity.Headers 的 X-Os-Category 同源）。
            device_os_category: identity::os_category(&id.platform),
            device_os_version: first_non_empty(&[
                id.os_version.clone(),
                ACTIVATION_FALLBACK_OS_VERSION.to_string(),
            ]),
            device_mid: device_mid.to_string(),
            mac_id: String::new(),
            marketing_params: "{}".to_string(),
        }
    }

    /// 上报一个激活事件。
    ///
    /// ⚠ **只设 `Content-Type`，不设 `Authorization`**：官方客户端这条路径就是
    /// 匿名的，身份由 body 里的 `device_mid` 表达。多送一个认证头反而是「与真机
    /// 不一致」的可区分特征（与 `Identity::claim_headers` 的取舍完全同源）。
    async fn post_event(
        &self,
        element: &str,
        user_id: &str,
        device_mid: &str,
    ) -> Result<(), String> {
        let body = serde_json::to_vec(&self.build_activation_event(element, user_id, device_mid))
            .map_err(|e| format!("构造 {element} 事件失败: {e}"))?;

        let url = format!("{}{}", self.quota_host(), EVENT_REPORT_PATH);
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| format!("事件 {element} 上报失败: {e}"))?;

        let status = resp.status().as_u16();
        let raw = super::client::read_limited(resp, MAX_EVENT_BODY).await;
        if status >= 400 {
            let text = String::from_utf8_lossy(&raw);
            return Err(format!(
                "事件 {element} HTTP {status} {}",
                errors_truncate(text.trim(), 200)
            ));
        }
        Ok(())
    }

    /// 取该账号的 `device_mid`；缺失时现场生成并固化。
    ///
    /// `device_mid` 是这条路径的**唯一身份**（事件不带认证头），所以它必须稳定 ——
    /// 每次激活换一个，上游看到的就是「同一个账号被大量不同设备激活」，**比不发还糟**。
    /// 取到后固化写回凭证文件，此后与外部文件无关。
    fn warm_device_mid(&self, cr: &mut ZcodeCred) -> String {
        if identity::is_uuid(&cr.device_mid) {
            return cr.device_mid.clone();
        }
        cr.device_mid = identity::new_device_mid();
        if !cr.file_path.is_empty() {
            cred::persist_device_mid_once(cr);
        }
        cr.device_mid.clone()
    }

    /// 对一个账号执行一次激活：先报事件，再领上游给出的所有套餐。
    ///
    /// 返回的 report 在**上游业务错误**时也不是 `Err`（错误写进 report 的字段），
    /// 这样界面能逐阶段展示到底哪一步出了问题；只有「连账号都没有」才返回 `Err`。
    ///
    /// ★ 参数是 `&mut` —— 因为 [`Self::warm_device_mid`] 可能要给凭证现场补上
    /// `device_mid` 并固化回文件。
    pub(crate) async fn warm_account(&self, cr: &mut ZcodeCred) -> Result<WarmReport, String> {
        let mut rep = WarmReport {
            uid: cr.uid.clone(),
            ..Default::default()
        };

        // 事件 body 里的 user_id：优先从 JWT 现解（与 `account_id_from_jwt` 的口径
        // 一致：user_id 优先、其次 sub），JWT 解不出时退回加载时已解好的值。
        let user_id = first_non_empty(&[
            cred::account_id_from_jwt(&cr.jwt),
            cr.account_id.clone(),
        ]);
        if user_id.is_empty() {
            // 参考实现同款处置：没有 user_id 就发不了事件（body 里那个字段是必需的
            // 身份），但**不报错** —— 如实说明跳过原因即可。
            rep.events = "JWT 里没有 user_id/sub —— 无法上报激活事件".to_string();
            rep.skipped = "激活事件未发出，未执行领取".to_string();
            return Ok(rep);
        }

        let mid = self.warm_device_mid(cr);

        for element in ACTIVATION_ELEMENTS {
            if let Err(e) = self.post_event(element, &user_id, &mid).await {
                rep.events = e;
                break;
            }
        }
        if !rep.events.is_empty() {
            rep.skipped = "激活事件未全部成功，未执行领取".to_string();
            return Ok(rep);
        }

        self.claim_offered_plans(cr, &mut rep).await;
        Ok(rep)
    }

    /// 领取上游给出的每一个套餐，`priority` 高的先领。
    ///
    /// 「上游列出来的就是过滤器」—— 不做任何套餐名启发式判断。上游会在不同时间
    /// 给出不同的 offer 集合，用名字去猜哪些该领会随着活动变化而失效。
    async fn claim_offered_plans(&self, cr: &ZcodeCred, rep: &mut WarmReport) {
        let pv = match self.fetch_plan_preview(cr).await {
            Ok(pv) => pv,
            Err(e) => {
                rep.claim = format!("查询可领套餐失败: {e}");
                return;
            }
        };
        if pv.offers.is_empty() {
            // 没有可领套餐 = **正常状态**（活动未上线 / 已领完），不是错误。
            rep.skipped = "上游没有给出可领套餐".to_string();
            return;
        }

        let mut problems: Vec<String> = Vec::new();
        for o in sort_offers_by_priority(pv.offers) {
            if o.plan_id.trim().is_empty() {
                // 没有 plan_id 就发不出去（`claim_plan` 也会本地拦下，但在这里先记一笔
                // 能让用户看到是**哪个** offer 有问题）。
                problems.push("上游给出的某个套餐没有 plan_id".to_string());
                continue;
            }

            let mut row = WarmPlan {
                plan_id: o.plan_id.clone(),
                name: o.name.clone(),
                priority: priority_of(&o),
                ..Default::default()
            };
            match self.claim_with_captcha(cr, &o.plan_id).await {
                Err(e) => {
                    row.message = e.to_string();
                    problems.push(format!("{}: {e}", o.plan_id));
                }
                Ok(res) if res.already_claimed => {
                    row.ok = true;
                    row.already_claimed = true;
                    row.message = "此前已领取过（不是错误）".to_string();
                    rep.claimed.push(o.plan_id.clone());
                }
                Ok(_) => {
                    row.ok = true;
                    row.message = "已领取".to_string();
                    rep.claimed.push(o.plan_id.clone());
                }
            }
            rep.plans.push(row);
        }

        if !problems.is_empty() {
            rep.claim = problems.join("; ");
        }
    }

    /// 领取一个套餐，最多两次；第二次是吃到 3007 之后**重新铸一个** param 的重试。
    ///
    /// # 为什么重试必须换一个新 param
    ///
    /// `captcha_param` 是**一次性**的：同一个 param 用第二次上游必回 3007。
    /// 重发同一个毫无意义，只会白烧一次请求。
    ///
    /// # 为什么铸不出来不阻断
    ///
    /// 参考实现每次领取前都新铸一个 param，我们照做；但本项目的求解器是**可选
    /// 组件**（目录可能为空、node 可能不存在、也可能正被上游限流）。铸不出来就
    /// 照发（不带 param），让上游如实回 3007，我们把它报成含 3007 的错误 —— 那比
    /// 「因为没配求解器所以整个激活失败」诚实，也让用户能分清「是求解器没配好」
    /// 还是「上游拒绝了这次领取」。
    async fn claim_with_captcha(
        &self,
        cr: &ZcodeCred,
        plan_id: &str,
    ) -> Result<ClaimResult, ClaimError> {
        let mut last_err: Option<ClaimError> = None;
        let mut last_mint_err: Option<String> = None;

        for attempt in 1..=2 {
            // 用副本：param 是一次性的，不该留在调用方的凭证上（那会让后续别的
            // 调用误以为它还能用）。
            let mut probe = cr.clone();
            match self.mint_captcha().await {
                Ok((param, region)) => {
                    probe.captcha_param = param;
                    probe.captcha_region = region;
                }
                Err(e) => {
                    last_mint_err = Some(e);
                    probe.captcha_param.clear();
                    probe.captcha_region.clear();
                }
            }

            match self.claim_plan(&probe, plan_id).await {
                Ok(res) => return Ok(res),
                Err(e) => {
                    let captcha = e.is_captcha_required();
                    last_err = Some(e);
                    // 不是验证码问题、或已经是第二轮 ⇒ 不再重试。
                    if !captcha || attempt == 2 {
                        break;
                    }
                    // 第一轮吃了 3007 ⇒ 换一个新 param 再试一次。
                }
            }
        }

        match last_err {
            Some(e) if e.is_captcha_required() => {
                let hint = match &last_mint_err {
                    Some(m) => format!("；求解器未能铸出 param：{m}"),
                    None => String::new(),
                };
                Err(ClaimError::Local(format!(
                    "验证码连续两次被拒{hint}（原错误：{e}）"
                )))
            }
            Some(e) => Err(e),
            // 循环从 1 到 2 至少执行一轮，且每轮失败都会写 last_err；
            // 走到这里说明 claim 成功了（已 return）。留个诚实的兜底。
            None => Err(ClaimError::Local("未执行领取".to_string())),
        }
    }

    /// 铸一个一次性的验证码 param（连同它所属的 region）。
    async fn mint_captcha(&self) -> Result<(String, String), String> {
        let param = self.solve_captcha().await?;
        if param.trim().is_empty() {
            return Err("求解器返回了空 param".to_string());
        }
        Ok((param, self.captcha_region()))
    }
}

/// 读 offer 的 `priority`（缺失 = 0）。
///
/// `priority` 在 [`PlanOffer`] 里没有专门字段，但原始对象被完整保留在 `raw` 上，
/// 所以这里直接读 —— 不去改 `claim.rs` 的解析结构，避免为了排序动到领取主路径
/// （那是已经实测过的代码）。
///
/// ★ 大小写两种拼写都认（Go `firstAny(o.Raw["priority"], o.Raw["Priority"])`）。
fn priority_of(o: &PlanOffer) -> i64 {
    let Some(v) = o.raw.get("priority").or_else(|| o.raw.get("Priority")) else {
        return 0;
    };
    if let Some(n) = v.as_i64() {
        return n;
    }
    if let Some(f) = v.as_f64() {
        return f as i64;
    }
    if let Some(s) = v.as_str() {
        return s.trim().parse::<i64>().unwrap_or(0);
    }
    0
}

/// 按 `priority` **降序**稳定排序（参考实现同款）。
///
/// 用稳定排序：`priority` 相同的 offer 保持上游给出的相对顺序，那通常也是上游
/// 的意图顺序。
fn sort_offers_by_priority(offers: Vec<PlanOffer>) -> Vec<PlanOffer> {
    let mut out = offers;
    // `slice::sort_by` 是稳定排序（`sort_unstable_by` 才是非稳定的）。
    out.sort_by(|a, b| priority_of(b).cmp(&priority_of(a)));
    out
}

/// 取第一个非空值。对应 Go `firstNonEmpty`。
fn first_non_empty(vals: &[String]) -> String {
    for v in vals {
        if !v.trim().is_empty() {
            return v.clone();
        }
    }
    String::new()
}

/// 截断到 `n` 个字符。对应 Go `truncate`（按 **rune** 计，不按字节）。
///
/// 本体在 `errors.rs` 里是 `pub fn truncate`，故这里只做一个薄别名以免重复实现。
fn errors_truncate(s: &str, n: usize) -> String {
    super::errors::truncate(s, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn offer(plan_id: &str, raw: Value) -> PlanOffer {
        PlanOffer {
            plan_id: plan_id.to_string(),
            name: String::new(),
            status: String::new(),
            ends_at: 0,
            raw,
        }
    }

    #[test]
    fn event_report_path_matches_the_go_constant() {
        assert_eq!(EVENT_REPORT_PATH, "/api/v1/event/report");
    }

    #[test]
    fn activation_elements_are_the_two_official_events_in_order() {
        // ★ 顺序有意义，且是顺序发 —— 不要改成集合或并发。
        assert_eq!(ACTIVATION_ELEMENTS, ["app_launch", "app_daily_active"]);
    }

    #[test]
    fn activation_event_has_exactly_the_sixteen_official_fields() {
        let c = crate::products::zcode::client::Client::new();
        let ev = c.build_activation_event("app_launch", "u-1", "mid-1");
        let v = serde_json::to_value(&ev).expect("序列化事件");

        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "app_version",
                "client_language",
                "client_timezone",
                "device_mid",
                "device_os_category",
                "device_os_version",
                "element_name",
                "event_extra_detail",
                "event_id",
                "event_region",
                "event_text",
                "event_type",
                "mac_id",
                "marketing_params",
                "screen_resolution",
                "user_id",
            ],
            "漏一个字段就是可区分特征 —— 16 个必须全在"
        );
    }

    #[test]
    fn event_extra_detail_is_an_object_but_marketing_params_is_a_string() {
        // ★ 官方的真实形状就是这样，不要「顺手统一」成同一种类型。
        let c = crate::products::zcode::client::Client::new();
        let ev = c.build_activation_event("app_launch", "u-1", "mid-1");
        let v = serde_json::to_value(&ev).unwrap();
        assert!(v["event_extra_detail"].is_object());
        assert_eq!(v["event_extra_detail"], json!({}));
        assert!(v["marketing_params"].is_string());
        assert_eq!(v["marketing_params"], json!("{}"));
    }

    #[test]
    fn activation_event_fills_the_official_constants() {
        let c = crate::products::zcode::client::Client::new();
        let ev = c.build_activation_event("app_daily_active", "u-9", "mid-9");
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["element_name"], json!("app_daily_active"));
        assert_eq!(v["event_region"], json!("app"));
        assert_eq!(v["event_type"], json!("view"));
        assert_eq!(v["event_text"], json!(""));
        assert_eq!(v["user_id"], json!("u-9"));
        assert_eq!(v["device_mid"], json!("mid-9"));
        assert_eq!(v["mac_id"], json!(""));
        assert_eq!(v["screen_resolution"], json!("1920x1080"));
    }

    #[test]
    fn every_event_gets_a_fresh_uuid_event_id() {
        // 官方每条事件用一个新的 UUID v4；复用同一个会变成可区分特征。
        let c = crate::products::zcode::client::Client::new();
        let a = c.build_activation_event("app_launch", "u", "m");
        let b = c.build_activation_event("app_launch", "u", "m");
        assert_ne!(a.event_id, b.event_id);
        assert!(identity::is_uuid(&a.event_id), "event_id 必须是 UUID 形态");
    }

    #[test]
    fn activation_event_uses_our_own_locale_pair_not_the_reference_one() {
        // ★ 刻意与参考实现分叉：我们报 zh-CN / Asia/Shanghai（与 billing 头
        // 声明的是同一对），而不是它硬编码的 en-US / America/New_York。
        // 同一个账号出现两个区域对，正是参考实现自己警告过的事。
        let c = crate::products::zcode::client::Client::new();
        let ev = c.build_activation_event("app_launch", "u", "m");
        assert_eq!(ev.client_language, "zh-CN");
        assert_eq!(ev.client_timezone, "Asia/Shanghai");
    }

    #[test]
    fn activation_event_os_version_never_falls_back_to_empty() {
        let c = crate::products::zcode::client::Client::new();
        let ev = c.build_activation_event("app_launch", "u", "m");
        assert!(!ev.device_os_version.trim().is_empty());
        assert!(!ev.device_os_category.trim().is_empty());
    }

    #[test]
    fn priority_of_reads_both_case_spellings_and_defaults_to_zero() {
        assert_eq!(priority_of(&offer("p", json!({"priority": 7}))), 7);
        assert_eq!(priority_of(&offer("p", json!({"Priority": 9}))), 9);
        assert_eq!(priority_of(&offer("p", json!({}))), 0);
        assert_eq!(priority_of(&offer("p", json!({"priority": "3"}))), 3);
        assert_eq!(priority_of(&offer("p", json!({"priority": null}))), 0);
    }

    #[test]
    fn sort_offers_by_priority_is_descending_and_stable() {
        let offers = vec![
            offer("a", json!({"priority": 1})),
            offer("b", json!({"priority": 5})),
            offer("c", json!({"priority": 1})),
            offer("d", json!({})),
            offer("e", json!({"priority": 5})),
        ];
        let got: Vec<String> = sort_offers_by_priority(offers)
            .into_iter()
            .map(|o| o.plan_id)
            .collect();
        // ★ 稳定：同为 5 的 b 在 e 前、同为 1 的 a 在 c 前（保持上游给的相对顺序）。
        // 缺失 priority 的 d 视为 0，排最后。
        assert_eq!(got, vec!["b", "e", "a", "c", "d"]);
    }

    #[test]
    fn first_non_empty_skips_blank_strings() {
        assert_eq!(
            first_non_empty(&["".to_string(), "  ".to_string(), "x".to_string()]),
            "x"
        );
        assert_eq!(first_non_empty(&["".to_string(), " ".to_string()]), "");
    }
}
