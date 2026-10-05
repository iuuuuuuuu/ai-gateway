//! 权益活动（Qoder 的「每天领 100 Credits」等）
//! （Go `internal/qoder/campaign.go` + `jobtoken.go` 的移植）。
//!
//! # 这个功能最容易静默出错的地方
//!
//! 上游对**残缺请求**回的是 **HTTP 200 + 结构合法的 JSON**，只是活动列表少一条 ——
//! 不报错、不告警，从响应本身完全看不出异常。逐个头部做二分实测：
//!
//! ```text
//! 只 Cosy-ClientType                    → 1 条 [VIEW_DETAILS]
//! + 伪造的 Cosy-MachineToken/Type        → 1 条
//! + **真实的** 那两个头                  → 2 条 [CLAIM_BENEFIT(CLAIMABLE), VIEW_DETAILS]
//! ```
//!
//! 即 [`machine_identity`](super::machine_identity) 提供的两个头必须同时在场且值真实。
//! 真实值来自客户端自带的阿里云风控 SDK，**不能本地编造**。
//!
//! # 领取为什么可以做
//!
//! 我一度判定"领取要人机验证，故不代领"—— **那个判断是错的**，我把两个端点搞混了：
//!
//! ```text
//! /api/v1/zcode-plan/billing/claim        ← ZCode 的套餐申领，要验证码
//! /sash/api/v1/me/campaigns/{id}/claim    ← 本模块，**不要验证码**
//! ```
//!
//! 它只是"领取我自己的每日权益"，与官方客户端点那个「领取」按钮**完全同构**：
//! 同样的 host、同样的头、同样的令牌。没有任何风控绕过。
//!
//! # 两条鉴权路径
//!
//! 活动/作业令牌都走 **OpenAPI 域**（`openapi.qoder.com.cn` / `openapi.qoder.sh`），
//! 只需 `Bearer DT` + COSY 的**静态头**（`Cosy-ClientType` 等），**不做签名**。
//! 别把它和 Gateway 域的 COSY 签名路径混起来。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::client::{read_limited, QoderClient, MAX_ERROR_BODY_BYTES};
use super::cred::{truncate_str, QoderCred};
use super::login::path_escape;
use super::machine_identity::machine_identity_of;
use super::region::QoderRegion;

/// 活动接口的 host（国服）。
///
/// ⚠ **就是** `cr.region.openapi()`。保留这两个常量而不是直接调 `openapi()`：
/// 国际版是 `openapi.qoder.sh`（不同 TLD），显式写出来更清楚。
pub(crate) const CAMPAIGN_HOST_CN: &str = "https://openapi.qoder.com.cn";
/// 活动接口的 host（国际版）。
pub(crate) const CAMPAIGN_HOST_INTL: &str = "https://openapi.qoder.sh";

/// 活动接口要求的客户端类型。
///
/// ⚠ **必须带上**。实测：同样的 host 与 Bearer，
///
/// ```text
/// 不带 Cosy-ClientType → campaigns=0（200，看起来像"没有活动"）
/// 带   Cosy-ClientType: 10 → campaigns=2（真实数据）
/// ```
const CAMPAIGN_CLIENT_TYPE: &str = "10";

/// 换取作业令牌用的 clientId（Go `jobTokenClientID`）。
///
/// ★ 与 OAuth 的 [`super::login::OAUTH_CLIENT_ID_DEFAULT`] **是同一个值** ——
/// 已核对上游，不要"优化"成各写一份。
pub(crate) const JOB_TOKEN_CLIENT_ID: &str = "732aef47-9cf2-46a2-95fe-4cebb5d0d1fa";

/// 该账号的活动接口 host。
///
/// ⚠ 抽成函数是为了让测试能直接钉住"国际版走 `.sh`、其余（含 nil）走 `.cn`"。
pub(crate) fn campaign_host_of(cr: &QoderCred) -> &'static str {
    if cr.region == QoderRegion::Intl {
        CAMPAIGN_HOST_INTL
    } else {
        CAMPAIGN_HOST_CN
    }
}

/// 一条权益活动的收益（Go 里的匿名 struct）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Benefit {
    #[serde(default)]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) amount: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) validity: Option<Validity>,
}

/// 收益有效期。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Validity {
    #[serde(default)]
    pub(crate) mode: String,
    #[serde(default)]
    pub(crate) days: i64,
}

/// 活动页展示位的内容（Go 里是 `map[string]struct{...}`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlacementContent {
    #[serde(default, rename = "buttonText")]
    pub(crate) button_text: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default, rename = "detailUrl")]
    pub(crate) detail_url: String,
}

/// 活动页展示位。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Placement {
    #[serde(default, rename = "type")]
    pub(crate) kind: String,
    #[serde(default, rename = "campaignUrl")]
    pub(crate) campaign_url: String,
    #[serde(default)]
    pub(crate) content: BTreeMap<String, PlacementContent>,
}

/// 一条权益活动。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Campaign {
    #[serde(default, rename = "campaignId")]
    pub(crate) campaign_id: String,
    #[serde(default, rename = "campaignKey")]
    pub(crate) campaign_key: String,
    /// `"CLAIM_BENEFIT"`（可领取）/ `"VIEW_DETAILS"`（只看详情）。
    #[serde(default, rename = "actionType")]
    pub(crate) action_type: String,
    /// `"CLAIMABLE"` / `"CLAIMED"` / `""`（未知）。
    #[serde(default, rename = "claimStatus")]
    pub(crate) claim_status: String,
    #[serde(default, rename = "startAt")]
    pub(crate) start_at: i64,
    #[serde(default, rename = "endAt")]
    pub(crate) end_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) benefit: Option<Benefit>,
    #[serde(default)]
    pub(crate) placements: Vec<Placement>,
}

/// 活动总览。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct CampaignStatus {
    /// 上游是否要展示活动入口。
    #[serde(default, rename = "showCampaign")]
    pub(crate) show_campaign: bool,
    /// 是否有**可领取**的活动。
    #[serde(default)]
    pub(crate) claimable: bool,
    /// 活动页（服务端渲染的 iframe 页面）。
    #[serde(default, rename = "campaignUrl")]
    pub(crate) campaign_url: String,
    /// 活动明细。
    #[serde(default)]
    pub(crate) campaigns: Vec<Campaign>,
}

/// 领取结果。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ClaimResult {
    #[serde(default, rename = "grantId")]
    pub(crate) grant_id: String,
    /// `"CLAIMED"`。
    #[serde(default)]
    pub(crate) status: String,
    /// `true` = 这次是**重放**（之前已领过），不是新领到。
    ///
    /// ⚠ 必须把它透出给用户：否则重复点"领取"会让人以为又领了一份。
    #[serde(default)]
    pub(crate) replayed: bool,
    #[serde(default, rename = "campaignId")]
    pub(crate) campaign_id: String,
    #[serde(default, rename = "campaignKey")]
    pub(crate) campaign_key: String,
    #[serde(default, rename = "claimedAt")]
    pub(crate) claimed_at: String,
    #[serde(default, rename = "grantedAt")]
    pub(crate) granted_at: String,
}

/// 作业令牌（Go `jobtoken.go` 的 `JobToken`）。
///
/// dt/drt 是长期登录令牌（约 30 天 / 1 年）；jt/jrt 是短期作业令牌 ——
/// 推理用的是后者。
///
/// **永不落盘。**
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct JobToken {
    /// `jt-…`。**绝不外传**：CLI 只输出长度，不输出值。
    #[serde(default)]
    pub(crate) token: String,
    #[serde(default, rename = "created_at")]
    pub(crate) created_at: String,
    #[serde(default, rename = "expires_at")]
    pub(crate) expires_at: String,
    /// **毫秒**（实测 86400000 = 1 天）。
    #[serde(default, rename = "expires_in")]
    pub(crate) expires_in: i64,
    #[serde(default, rename = "refresh_token")]
    pub(crate) refresh_token: String,
    #[serde(default, rename = "refresh_token_expires_at")]
    pub(crate) refresh_token_expires_at: String,
    /// **毫秒**（实测 172800000 = 2 天）。
    #[serde(default, rename = "refresh_token_expires_in")]
    pub(crate) refresh_token_expires_in: i64,
}

/// 活动接口的公共静态头。
///
/// 抽出来是为了让查询与领取**共用同一组头** —— 分别手写必然会漂移
/// （Go 侧历史上已经因为"三件套"写漏过头）。
///
/// ⚠ `Cosy-MachineToken` / `Cosy-MachineType` / `Cosy-MachineCode`
/// **拿不到就不发**（而不是发空串或假值）：空串可能被上游当成"提供了但非法"，
/// 假值则确定无效。退回"降级但可用"的老行为，至少不会更坏。
fn apply_campaign_headers(req: reqwest::RequestBuilder, cr: &QoderCred) -> reqwest::RequestBuilder {
    let mut req = req
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", cr.dt))
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::USER_AGENT, "Qoder/1.0")
        // ① 缺了它 → 上游回空清单。
        .header("Cosy-ClientType", CAMPAIGN_CLIENT_TYPE);
    let id = machine_identity_of();
    if id.usable() {
        // ②③ 缺任一 → 上游回**残缺清单**（少掉可领取的那条），200 且不报错。
        req = req
            .header("Cosy-MachineToken", &id.machine_token)
            .header("Cosy-MachineType", &id.machine_type)
            // Code 实测不是必需（二分时只补 Token+Type 就够），但官方客户端发了它，
            // 且它是同一组身份的一部分 —— 一并带上。
            .header("Cosy-MachineCode", &id.machine_code);
    }
    req
}

impl QoderClient {
    /// 查询该账号的权益活动（**只读**）。
    pub(crate) async fn fetch_campaigns(&self, cr: &QoderCred) -> Result<CampaignStatus, String> {
        if cr.dt.is_empty() {
            return Err("账号没有可用令牌".to_string());
        }
        let url = format!("{}/sash/api/v1/me/campaigns", campaign_host_of(cr));
        let resp = apply_campaign_headers(self.rpc().get(&url), cr)
            .send()
            .await
            .map_err(|e| format!("查询权益活动失败: {e}"))?;
        let status = resp.status().as_u16();
        let raw = read_limited(resp, MAX_ERROR_BODY_BYTES).await;
        let text = String::from_utf8_lossy(&raw).into_owned();
        if status >= 400 {
            return Err(format!(
                "查询权益活动失败（HTTP {status}）：{}",
                truncate_str(&text, 200)
            ));
        }
        serde_json::from_str(&text).map_err(|e| {
            format!(
                "活动响应解析失败: {e}（原始内容：{}）",
                truncate_str(&text, 160)
            )
        })
    }

    /// 领取一个权益活动（**用自己的令牌打官方接口**）。
    ///
    /// `host` 传 `None` 时用 [`campaign_host_of`]；`Some` 仅供测试注入
    /// （见 Go `claimCampaignAt` 的说明：本功能的历史缺陷全是"头写漏了但响应仍
    /// 200"，那种缺陷只有把请求拦下来看头才能测到）。
    pub(crate) async fn claim_campaign_at(
        &self,
        cr: &QoderCred,
        host: Option<&str>,
        campaign_id: &str,
    ) -> Result<ClaimResult, String> {
        if cr.dt.is_empty() {
            return Err("账号没有可用令牌".to_string());
        }
        let campaign_id = campaign_id.trim();
        if campaign_id.is_empty() {
            return Err("活动 ID 不能为空".to_string());
        }
        let host = host.unwrap_or_else(|| campaign_host_of(cr));
        let url = format!(
            "{host}/sash/api/v1/me/campaigns/{}/claim",
            path_escape(campaign_id)
        );
        // 官方请求是 POST + **空 body**（`Content-Length: 0`）。
        let mut req = apply_campaign_headers(self.rpc().post(&url), cr)
            .header(reqwest::header::ORIGIN, host)
            // 官方带 referer 指向活动页；服务端未必校验，但照抄更安全。
            .header(reqwest::header::REFERER, format!("{host}/growth-page/activity-iframe"));
        // `body("")` 保证真的发出 `Content-Length: 0`（而不是省略该头）。
        req = req.body(Vec::<u8>::new());

        let resp = req
            .send()
            .await
            .map_err(|e| format!("领取权益失败: {e}"))?;
        let status = resp.status().as_u16();
        let raw = read_limited(resp, MAX_ERROR_BODY_BYTES).await;
        let text = String::from_utf8_lossy(&raw).into_owned();
        if status >= 400 {
            return Err(format!(
                "领取权益失败（HTTP {status}）：{}",
                truncate_str(&text, 200)
            ));
        }
        let r: ClaimResult = serde_json::from_str(&text).map_err(|e| {
            format!(
                "领取响应解析失败: {e}（原始内容：{}）",
                truncate_str(&text, 160)
            )
        })?;
        // `status` 不是 CLAIMED 时**不当作成功** —— 上游可能用 200 表达
        // "这次没领到"（如活动已结束但详情还在）。把那种情况当成功会让
        // 界面显示"领取成功"而实际没有。
        if r.status != "CLAIMED" {
            return Err(format!(
                "上游返回的状态不是已领取：{}",
                truncate_str(&text, 160)
            ));
        }
        Ok(r)
    }

    /// 领取（host 取该账号的默认值）。
    pub(crate) async fn claim_campaign(
        &self,
        cr: &QoderCred,
        campaign_id: &str,
    ) -> Result<ClaimResult, String> {
        self.claim_campaign_at(cr, None, campaign_id).await
    }

    /// 换取作业令牌（Go `FetchJobToken`）。
    ///
    /// ★ 与活动查询的区别：**不带** `Origin` / `Referer`，也**不带**机器指纹头 ——
    /// 只带 `Cosy-ClientType`。逐字照抄，别顺手"补齐"。
    pub(crate) async fn fetch_job_token(&self, cr: &QoderCred) -> Result<JobToken, String> {
        if cr.dt.is_empty() {
            return Err("账号没有可用令牌".to_string());
        }
        let url = format!("{}/api/v1/me/jobToken", campaign_host_of(cr));
        let body = serde_json::to_vec(&{
            let mut m = Map::new();
            m.insert(
                "clientId".to_string(),
                Value::String(JOB_TOKEN_CLIENT_ID.to_string()),
            );
            Value::Object(m)
        })
        .unwrap_or_else(|_| b"{}".to_vec());

        let resp = self
            .rpc()
            .post(&url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", cr.dt))
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, "Qoder/1.0")
            .header("Cosy-ClientType", CAMPAIGN_CLIENT_TYPE)
            .body(body)
            .send()
            .await
            .map_err(|e| format!("换取作业令牌失败: {e}"))?;
        let status = resp.status().as_u16();
        let raw = read_limited(resp, MAX_ERROR_BODY_BYTES).await;
        let text = String::from_utf8_lossy(&raw).into_owned();
        if status >= 400 {
            return Err(format!(
                "换取作业令牌失败（HTTP {status}）：{}",
                truncate_str(&text, 200)
            ));
        }
        let t: JobToken = serde_json::from_str(&text).map_err(|e| {
            format!(
                "作业令牌响应解析失败: {e}（原始内容：{}）",
                truncate_str(&text, 160)
            )
        })?;
        if t.token.is_empty() {
            return Err(format!(
                "作业令牌响应里没有 token（原始内容：{}）",
                truncate_str(&text, 160)
            ));
        }
        Ok(t)
    }
}

/// 统计可领取条数（Go `countClaimableCLI`）。
///
/// ★ **空状态也算可领**（`st != "CLAIMED" && st != "EXPIRED"`）—— 上游对新活动
/// 可能不给 `claimStatus`，把它当"不可领"会漏掉真正能领的那条。
pub(crate) fn count_claimable(campaigns: &[Campaign]) -> usize {
    campaigns
        .iter()
        .filter(|c| {
            let st = c.claim_status.trim().to_uppercase();
            st != "CLAIMED" && st != "EXPIRED"
        })
        .count()
}

/// 领取全部可领活动（Go `ClaimAllCampaigns`，从 CLI 抽出来供**排程侧复用**）。
///
/// 返回值就是 CLI 要打印的那个 JSON 对象（Go 直接 `writeJSON(out)`）。
///
/// 契约（逐字对齐 Go，别"顺手改得更合理"）：
///
/// - `load_dir` 读目录失败 → 返回 `Err`（调用方包成 `{"status":"error"}`）；
/// - 目录里读不出来的文件（`failed`）→ 作为 `{"status":"error","message":…}` 条目
///   混在 `accounts` 里**如实上报**，绝不静默丢掉；
/// - 每个账号：查活动失败 → `failedCount++`；没有可领目标 →
///   `nothingCount++` + `status:"nothing"`；否则逐个领取，**只有非重放**才计入
///   `claimedCount`。
pub(crate) async fn claim_all_campaigns(auth_dir: &str) -> Result<Value, String> {
    let (creds, failed) = QoderCred::load_dir(auth_dir);
    let cli = super::login::new_cli_client(auth_dir);

    let mut accounts: Vec<Value> = Vec::with_capacity(creds.len() + failed.len());
    // ★ 先把"读不出来的文件"如实报出来（Go 的循环顺序：failed 在前）。
    for f in &failed {
        accounts.push(serde_json::json!({"status": "error", "message": f}));
    }

    let mut claimed_count = 0usize;
    let mut failed_count = 0usize;
    let mut nothing_count = 0usize;

    for cr in &creds {
        let st = match cli.fetch_campaigns(cr).await {
            Ok(st) => st,
            Err(e) => {
                failed_count += 1;
                accounts.push(serde_json::json!({
                    "uid": cr.uid,
                    "status": "error",
                    "message": e,
                    "claimed": [],
                }));
                continue;
            }
        };

        let targets: Vec<&Campaign> = st
            .campaigns
            .iter()
            .filter(|c| {
                let s = c.claim_status.trim().to_uppercase();
                s != "CLAIMED" && s != "EXPIRED" && c.action_type == "CLAIM_BENEFIT"
            })
            .collect();

        if targets.is_empty() {
            nothing_count += 1;
            accounts.push(serde_json::json!({
                "uid": cr.uid,
                "status": "nothing",
                "claimed": [],
            }));
            continue;
        }

        let mut done: Vec<Value> = Vec::with_capacity(targets.len());
        for camp in targets {
            match cli.claim_campaign(cr, &camp.campaign_id).await {
                Ok(r) => {
                    if !r.replayed {
                        claimed_count += 1;
                    }
                    done.push(serde_json::json!({
                        "campaignId": r.campaign_id,
                        "ok": true,
                        "replayed": r.replayed,
                        "claimedAt": r.claimed_at,
                        "benefit": camp.benefit,
                    }));
                }
                Err(e) => {
                    failed_count += 1;
                    done.push(serde_json::json!({
                        "campaignId": camp.campaign_id,
                        "ok": false,
                        "error": e,
                        "benefit": camp.benefit,
                    }));
                }
            }
        }
        accounts.push(serde_json::json!({
            "uid": cr.uid,
            "status": "ok",
            "claimed": done,
        }));
    }

    Ok(serde_json::json!({
        "status": "ok",
        "accounts": accounts,
        "claimedCount": claimed_count,
        "failedCount": failed_count,
        "nothingCount": nothing_count,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(region: QoderRegion) -> QoderCred {
        QoderCred {
            dt: "dt-test".into(),
            region,
            ..Default::default()
        }
    }

    #[test]
    fn campaign_host_is_intl_only_for_the_intl_region() {
        // ★ 钉住 Go 的那条：nil / 未知 / CN 全部走 .cn。
        assert_eq!(campaign_host_of(&cred(QoderRegion::Intl)), CAMPAIGN_HOST_INTL);
        assert_eq!(campaign_host_of(&cred(QoderRegion::CN)), CAMPAIGN_HOST_CN);
        assert_eq!(
            campaign_host_of(&cred(QoderRegion::Unknown)),
            CAMPAIGN_HOST_CN
        );
    }

    #[test]
    fn empty_claim_status_counts_as_claimable() {
        // ★ 上游对新活动可能不给 claimStatus —— 当成"不可领"会漏掉真正能领的。
        let mk = |s: &str| Campaign {
            claim_status: s.to_string(),
            ..Default::default()
        };
        assert_eq!(count_claimable(&[mk("")]), 1);
        assert_eq!(count_claimable(&[mk("CLAIMABLE")]), 1);
        assert_eq!(count_claimable(&[mk("claimable")]), 1);
        assert_eq!(count_claimable(&[mk(" CLAIMED ")]), 0);
        assert_eq!(count_claimable(&[mk("EXPIRED")]), 0);
        assert_eq!(count_claimable(&[mk("expired")]), 0);
        assert_eq!(count_claimable(&[]), 0);
    }

    #[test]
    fn job_token_client_id_matches_the_oauth_one() {
        // ★ 两者已核对为同一个值；拆成两个常量是为了对齐 Go 的两处命名，
        //   但值必须一致 —— 不一致说明有人"优化"错了。
        assert_eq!(JOB_TOKEN_CLIENT_ID, super::super::login::OAUTH_CLIENT_ID_DEFAULT);
    }

    #[test]
    fn campaign_json_uses_the_upstream_field_names() {
        // 逐字来自 Go struct tag；字段名漂了会静默解析成空。
        let raw = r#"{
            "showCampaign": true,
            "claimable": true,
            "campaignUrl": "https://openapi.qoder.com.cn/growth-page/activity-iframe",
            "campaigns": [{
                "campaignId": "c-1", "campaignKey": "k-1",
                "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMABLE",
                "startAt": 1, "endAt": 2,
                "benefit": {"kind": "CREDITS", "amount": 100.0,
                            "validity": {"mode": "RELATIVE_DAYS", "days": 30}},
                "placements": [{"type": "POPUP", "campaignUrl": "u",
                                "content": {"zh": {"buttonText": "领取",
                                                   "description": "每天领 100 Credits",
                                                   "detailUrl": "d"}}}]
            }]
        }"#;
        let st: CampaignStatus = serde_json::from_str(raw).unwrap();
        assert!(st.show_campaign);
        assert!(st.claimable);
        assert_eq!(st.campaigns.len(), 1);
        let c = &st.campaigns[0];
        assert_eq!(c.campaign_id, "c-1");
        assert_eq!(c.action_type, "CLAIM_BENEFIT");
        assert_eq!(c.claim_status, "CLAIMABLE");
        assert_eq!(c.benefit.as_ref().unwrap().amount, 100.0);
        assert_eq!(c.benefit.as_ref().unwrap().validity.as_ref().unwrap().days, 30);
        assert_eq!(c.placements[0].content["zh"].button_text, "领取");
    }

    #[test]
    fn claim_result_round_trips_and_keeps_replayed() {
        // ★ `replayed` 必须透出：否则重复点"领取"会让人以为又领了一份。
        let raw = r#"{"grantId":"g-1","status":"CLAIMED","replayed":true,
                      "campaignId":"c-1","campaignKey":"k-1",
                      "claimedAt":"2026-09-23T10:00:00Z","grantedAt":"2026-09-23T10:00:00Z"}"#;
        let r: ClaimResult = serde_json::from_str(raw).unwrap();
        assert_eq!(r.status, "CLAIMED");
        assert!(r.replayed);
        assert_eq!(r.grant_id, "g-1");
    }

    #[test]
    fn job_token_json_uses_snake_case_tags() {
        // Go 侧全是 snake_case（与活动接口的 camelCase 不同）—— 别统一。
        let raw = r#"{"token":"jt-abc","created_at":"a","expires_at":"b",
                      "expires_in":86400000,"refresh_token":"jrt-abc",
                      "refresh_token_expires_at":"c","refresh_token_expires_in":172800000}"#;
        let t: JobToken = serde_json::from_str(raw).unwrap();
        assert_eq!(t.token, "jt-abc");
        assert_eq!(t.expires_in, 86_400_000);
        assert_eq!(t.refresh_token_expires_in, 172_800_000);
        assert_eq!(t.refresh_token, "jrt-abc");
    }

    #[test]
    fn missing_optional_campaign_fields_do_not_fail_the_parse() {
        // 上游对残缺请求回的是结构合法的 JSON，字段可能缺 —— 不能因此解析失败。
        let st: CampaignStatus = serde_json::from_str(r#"{"campaigns":[{}]}"#).unwrap();
        assert!(!st.show_campaign);
        assert_eq!(st.campaigns.len(), 1);
        assert!(st.campaigns[0].benefit.is_none());
        assert!(st.campaigns[0].placements.is_empty());
    }

    #[tokio::test]
    async fn claim_rejects_empty_campaign_id_before_any_request() {
        let cli = QoderClient::new();
        let err = cli.claim_campaign(&cred(QoderRegion::CN), "   ").await.unwrap_err();
        assert_eq!(err, "活动 ID 不能为空");
    }

    #[tokio::test]
    async fn campaign_calls_reject_a_credential_without_a_token() {
        let cli = QoderClient::new();
        let empty = QoderCred::default();
        assert_eq!(
            cli.fetch_campaigns(&empty).await.unwrap_err(),
            "账号没有可用令牌"
        );
        assert_eq!(
            cli.claim_campaign(&empty, "c-1").await.unwrap_err(),
            "账号没有可用令牌"
        );
        assert_eq!(
            cli.fetch_job_token(&empty).await.unwrap_err(),
            "账号没有可用令牌"
        );
    }
}
