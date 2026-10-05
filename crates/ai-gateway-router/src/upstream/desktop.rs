//! 桌面端事件上报与序列构造（对应 Go `desktop.go` + `report.go`）。
//!
//! ★ `DesktopEvent` 与全部序列构造器都是**纯函数**（无网络），可独立测试。
//!
//! # 三条指纹通道，绝不能混用
//!
//! 三者的**路径都是 `/v2/report`**，差别只在基址 + 请求头 + 事件形状：
//!
//! | 通道 | 基址 | 判据头 | 事件形状 |
//! |---|---|---|---|
//! | CLI 域活跃 | `billingBase(a)`（`codebuddy.cn` / `workbuddy.ai`） | billing 头族 | `chat_request_send` |
//! | 桌面指纹 | `chatBase(a)`（`copilot.tencent.com`） | `extName=workbuddy-desktop` | `desktopFingerprint` 22 键 |
//! | Web 指纹 | `webBase(a)`（`workbuddy.cn` / `workbuddy.ai`） | `x-client-platform: web` | 浏览器形状 |
//!
//! 用错指纹的失败形态是**HTTP 200 但静默不计分** —— 没有错误、没有日志，
//! 只能靠「进度条不动」反推。这是本文件最需要小心的地方。
//!
//! 另有一处同形的陷阱：`X-Client-Platform` 在 growth 域任务接口是
//! `miniprogram`（见 [`crate::upstream::client::MP_PLATFORM`]），在 report 域
//! 事件通道里**不存在**这个头（小程序上报靠 `source: "mini_program"` 指纹）。
//! 两个口径互换任一方向都会让上游按另一种来源解析。
//!
//! # 为什么这些方法必须写在 `impl Client`
//!
//! [`Client`] 的 `http` / `chat` / `cfg` 是**私有字段**，私有可见性只覆盖
//! 定义它的模块及其子模块（兄弟模块 `upstream::desktop` **读不到**）。
//! 请求管道留在 `client.rs`，本文件按域拼路径与构造事件。

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::auth::Auth;
use crate::error::{GatewayError, Result};
use crate::upstream::client::Client;
use crate::upstream::tasks::itoa64;

/// desktop 与 web 指纹共用的上报路径（基址不同）。
pub const DESKTOP_REPORT_PATH: &str = "/v2/report";

/// 外观主题设置（桌面域）。
///
/// 纯 API set 只留痕不计分，必须再补一条 `appearance_skin_apply` 事件才点亮
/// `Hp_Appearance`（实测口径）—— 见 [`appearance_apply_event`]。
pub const APPEARANCE_SET_PATH: &str = "/v2/user-asset/appearance/set";

/// 专家市场列表路径（桌面域）。
pub const MARKET_EXPERT_LIST_PATH: &str = "/portal/operation-platform/market/expert/list";

/// 真实对话路径（桌面域，SSE）。
pub const CHAT_COMPLETIONS_PATH: &str = "/v2/chat/completions";

/// 实测桌面客户端 UA（5.5.6 内嵌 CLI 2.137.1）。
pub const DESKTOP_UA: &str = "WorkBuddy/5.5.6 WorkBuddy/5.5.6 CLI/2.137.1";

/// 浏览器 UA。
///
/// 成长中心的领奖与 web 域上报都用它 —— 用桌面 UA 调网页接口会形成
/// 「CLI 调网页」的矛盾指纹。
pub const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";

/// 开学季/校园日活动的关联 id。
///
/// 事件带上它才会被归到该活动；不带则**不计分**（上游实测）。
pub const SCHOOL_SEASON_ACTIVITY_ID: &str = "school_open_day_2026";

/// billing 域请求需要补的 Go 默认 UA。
///
/// `client.rs` 里的同名常量 `GO_HTTP_UA` 是**私有**的（兄弟模块读不到），
/// 故这里重写同一个字面量 —— 值必须与
/// `crates/ai-gateway-router/src/upstream/client.rs` 里的 `GO_HTTP_UA` 一致，
/// 否则同一个 billing 域上会出现两种客户端指纹。
const BILLING_GO_UA: &str = "Go-http-client/1.1";

/// 夜猫时段起点（CST 整点，含）。
const NIGHT_WINDOW_START_HOUR: u32 = 23;
/// 夜猫时段终点（CST 整点，不含）。
const NIGHT_WINDOW_END_HOUR: u32 = 8;

/// 桌面指纹事件：业务字段任意，公共指纹由上报函数注入。
///
/// 用 `BTreeMap` 而非 `HashMap`：Go 的 `encoding/json` 序列化 `map[string]any`
/// 时**按 key 排序**，`BTreeMap` 天然同序 —— 报文逐字节可比对。
pub type DesktopEvent = BTreeMap<String, serde_json::Value>;

// ──────────────────────────── 纯函数工具 ────────────────────────────

/// 由 uid 稳定派生一个 36 位 hex 设备标识（machineId / sessionId 复用）。
///
/// 用派生而非随机：同一账号每次生成相同的设备标识，模拟「固定的一台机器」。
/// 每次上报都换 machineId 等于每次都是一台新设备，这在风控视角下比固定设备更可疑。
pub fn derive_id(a: &Auth, salt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{salt}:{}", a.uid).as_bytes());
    let sum = hasher.finalize();
    hex::encode(&sum[..18]) // 36 hex chars
}

/// 取字符串末尾 `n` 个字符；不足则整体返回（避免切片越界）。
///
/// 与 Go `tail` 同口径（按**字节**截断），只是多了字符边界回退：
/// Go 允许切出半个 UTF-8 字符，Rust 的 `&str` 索引会 panic，
/// 故向前找到最近的字符边界。请求 id 全是 ASCII，两者结果一致。
pub fn tail(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut start = s.len() - n;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

/// 当前毫秒时间戳。
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 当前纳秒时间戳（对应 Go `time.Now().UnixNano()`）。
fn unix_nano() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// 插入 header，忽略非法值（凭证里的 uid 等理论上可能含非 ASCII）。
///
/// 与 [`crate::upstream::headers`] 里的同名私有函数同口径，这里复制一份是
/// 因为它是**私有**的（跨模块读不到），而本文件需要构造非标准头族。
fn put_header(headers: &mut reqwest::header::HeaderMap, key: &'static str, value: &str) {
    if let Ok(v) = reqwest::header::HeaderValue::from_str(value) {
        headers.insert(key, v);
    }
}

/// 构造一个事件：`{"eventCode": code}` 再合并 `extra`（后者覆盖同名键）。
///
/// 对应 Go 各序列构造器里那个局部 `mk` 闭包。
fn event(code: &str, extra: serde_json::Value) -> DesktopEvent {
    let mut m = DesktopEvent::new();
    m.insert(
        "eventCode".to_string(),
        serde_json::Value::String(code.to_string()),
    );
    if let serde_json::Value::Object(o) = extra {
        for (k, v) in o {
            m.insert(k, v);
        }
    }
    m
}

/// 公共桌面指纹字段（22 键）。
///
/// 这些字段本身不参与业务判据，但缺失会让事件看起来不像真实客户端产生的；
/// 业务键里若出现同名键，调用方给出的值优先（见 [`Client::report_desktop_events`]）。
pub fn desktop_fingerprint(a: &Auth) -> DesktopEvent {
    let now = now_ms();
    let mut m = DesktopEvent::new();
    m.insert("timezone".into(), serde_json::json!("Asia/Shanghai"));
    m.insert("reportDelay".into(), serde_json::json!(2000));
    m.insert("userId".into(), serde_json::json!(a.uid));
    m.insert("username".into(), serde_json::json!(a.nickname));
    m.insert("userNickname".into(), serde_json::json!(a.nickname));
    m.insert("product".into(), serde_json::json!("SaaS"));
    m.insert("releaseDate".into(), serde_json::json!(1789036585355i64));
    m.insert(
        "commit".into(),
        serde_json::json!("5f9692923c93033111c51ad7b003eb80204a9b75"),
    );
    m.insert("ideName".into(), serde_json::json!("WorkBuddy"));
    m.insert("ideType".into(), serde_json::json!("WorkBuddy"));
    m.insert("ideVersion".into(), serde_json::json!("5.5.6"));
    m.insert("machineId".into(), serde_json::json!(derive_id(a, "machine")));
    m.insert("sessionId".into(), serde_json::json!(derive_id(a, "session")));
    m.insert("extName".into(), serde_json::json!("workbuddy-desktop"));
    m.insert("extVersion".into(), serde_json::json!("5.5.6"));
    m.insert("os".into(), serde_json::json!("win32"));
    m.insert("arch".into(), serde_json::json!("x64"));
    m.insert("osVersion".into(), serde_json::json!("10.0.26220"));
    m.insert("cpuCores".into(), serde_json::json!(20));
    m.insert("memorySize".into(), serde_json::json!(24));
    m.insert("timestamp".into(), serde_json::json!(now));
    m.insert("presentAt".into(), serde_json::json!(now));
    m
}

/// 当前是否处于夜猫时段（23:00–08:00 CST）。
///
/// 供 `black_cat` 类时段敏感任务判断：窗口外做行为不计分，
/// 与其发一次无效上报再等一个永远不会达标的轮询，不如直接跳过并说明原因。
///
/// 与 scheduler 的窗口口径必须一致（那边是排程触发时机，这边是动作可行性判断）。
/// 两边都按 CST 固定偏移而非本地时区：窗口来自上游活动规则，
/// 换机器/改系统时区不应改变判定结果。
pub fn in_night_window(now: chrono::DateTime<chrono::Utc>) -> bool {
    use chrono::Timelike;
    let h = (now + chrono::Duration::hours(8)).hour();
    h >= NIGHT_WINDOW_START_HOUR || h < NIGHT_WINDOW_END_HOUR
}

// ──────────────────────────── 判据事件链构造 ────────────────────────────
//
// 设计：全部是**纯函数**（除时间戳外无副作用、不触网），把「事件形状」与
// 「何时上报、如何重试、如何回读」彻底分开。好处有三个：
//   - 形状可单测：断言每个事件码与关键字段，不必起假上游；
//   - 编排层可复用：多条任务链共享 chat_sequence 这个基座（模板/灵感/画布
//     都建立在一次成功的对话之上）；
//   - 改动集中：上游若调整判据字段，只改这里，编排层不动。

/// 一次「桌面端成功对话」的完整事件链（6 条）。
///
/// 这是多数桌面任务的共同基座：服务端把对话成功回执（`isSuccessful=true`）
/// 视为「客户端真的完成了一次对话」的证据，其余任务事件 JOIN 到它上面
/// （靠 `conversationId`/`requestId` 关联）。缺少成功回执的链会被判为无效对话。
pub fn chat_sequence(
    conversation_id: &str,
    request_id: &str,
    message_id: &str,
    model_id: &str,
    model_name: &str,
) -> Vec<DesktopEvent> {
    let assistant = format!("{message_id}-assistant");
    vec![
        event(
            "agent_task_created",
            serde_json::json!({
                "source": "LOCAL", "name": "working", "task_target": "local", "mode": "craft",
                "requestModelId": model_id, "requestModelName": model_name,
                "has_repo": false, "repo_type": "none", "workspace_type": "empty",
                "has_connector": false, "connector_types": [],
                "has_mention": false, "mention_types": [],
                "has_template": false, "action": "", "template_name": "",
                "has_expert": false, "expert_id": "", "expert_name": "", "expert_industry_id": "",
                "has_skill": false, "skill_names": [],
                "conversationId": conversation_id, "messageId": message_id,
                "buddyId": "", "buddyName": "",
            }),
        ),
        event(
            "chat_message_send",
            serde_json::json!({
                "messageId": assistant, "historyCount": 0,
                "isContextTruncated": false, "currentStepCount": 1,
                "traceId": request_id, "rootRequestId": request_id,
                "parentConversationId": conversation_id,
                "agentName": "cli", "agentType": "main",
            }),
        ),
        event(
            "chat_request_send",
            serde_json::json!({
                "inputLength": 24, "isPlan": false, "isAutoExecuteTerminal": false,
                "isAutoModify": false, "codebaseEnable": false, "maxToken": 0,
                "maxSteps": 500, "temperature": 0, "maxRetries": 0,
                "mentionContexts": [], "knowledgeId": [], "knowledgeName": [],
                "codebaseId": "", "mentionContextCount": 0, "command": "",
                "recommendId": "", "skillId": "", "skillCount": 0, "totalCount": 0,
                "traceId": request_id, "rootRequestId": request_id,
                "parentConversationId": conversation_id,
                "agentName": "cli", "agentType": "main",
                "codebuddy.session_id": conversation_id,
                "codebuddy.conversation_request_id": request_id,
            }),
        ),
        event(
            "chat_message_response",
            serde_json::json!({
                "messageId": assistant, "responseModelId": model_id,
                "inputToken": 120, "outputToken": 80, "totalToken": 200,
                "cachedTokens": 0, "cachedWriteTokens": 0, "cachedMissTokens": 0,
                "isSuccessful": true, "messageErrorCode": "", "finishReason": "stop",
                "firstTokenAt": now_ms(), "traceId": request_id,
                "conversationId": conversation_id,
                "rootRequestId": request_id, "parentConversationId": conversation_id,
                "agentName": "cli", "agentType": "main",
                "codebuddy.session_id": conversation_id,
                "codebuddy.conversation_request_id": request_id,
            }),
        ),
        event(
            "chat_message_status",
            serde_json::json!({
                "messageId": assistant, "messageErrorCode": "0",
                "traceId": request_id, "rootRequestId": request_id,
                "parentConversationId": conversation_id,
                "agentName": "cli", "agentType": "main",
            }),
        ),
        event(
            "chat_request_response",
            serde_json::json!({
                "mode": "craft", "toolCallCount": 0,
                "inputToken": 120, "outputToken": 80, "totalToken": 200,
                "cachedTokens": 0, "cachedWriteTokens": 0, "cachedMissTokens": 0,
                "isSuccessful": true, "messageErrorCode": "", "finishReason": "stop",
                "rootRequestId": request_id, "parentConversationId": conversation_id,
            }),
        ),
    ]
}

/// 「进入 Buddy 应用」五连事件。
///
/// `buddy_id` 用企鹅教师助手：它同时是 `Buddy_App_QQ` 的判据应用，
/// 因此同一组事件可覆盖「进入任一应用」(`Buddy_App`) 与「进入企鹅教师助手」
/// (`Buddy_App_QQ`) 两个任务 —— 少一次完整事件链，也少一份重复的设备行为。
pub fn buddy_app_sequence(buddy_id: &str, buddy_name: &str) -> Vec<DesktopEvent> {
    let base = |code: &str, extra: serde_json::Value| {
        let mut ev = event(code, extra);
        ev.insert("mode".into(), serde_json::json!("LOCAL"));
        ev.insert("buddyId".into(), serde_json::json!(buddy_id));
        ev.insert("buddyName".into(), serde_json::json!(buddy_name));
        ev
    };
    let with_ids = |extra: serde_json::Value| {
        let mut m = serde_json::json!({
            "elementId": buddy_id,
            "elementName": buddy_name,
        });
        if let (Some(dst), serde_json::Value::Object(src)) = (m.as_object_mut(), extra) {
            for (k, v) in src {
                dst.insert(k, v);
            }
        }
        m
    };
    vec![
        base("buddyapp_discover_click", serde_json::json!({})),
        base(
            "buddyapp_show",
            with_ids(serde_json::json!({"position": 2})),
        ),
        base(
            "buddyapp_enter_click",
            with_ids(serde_json::json!({"position": 2, "isFirstPage": "1"})),
        ),
        base("buddyapp_auth_confirm_click", with_ids(serde_json::json!({}))),
        base(
            "buddyapp_bindaccount_skip_click",
            with_ids(serde_json::json!({})),
        ),
    ]
}

/// 「定时任务创建成功」事件。
pub fn automation_create_event(name: &str) -> DesktopEvent {
    event(
        "automated_task_create_suc",
        serde_json::json!({
            "name": name,
            "source": "manually", "modelId": "fast-model", "modelIsThinking": true,
            "connectorCount": 0, "skills": "", "skillCount": 0,
            "scheduleType": "once", "mode": "LOCAL",
        }),
    )
}

/// 「使用模板创建任务」事件组，JOIN 一条完整对话链。
pub fn template_use_sequence(
    conversation_id: &str,
    request_id: &str,
    template_id: &str,
    template_name: &str,
) -> Vec<DesktopEvent> {
    let mut events = chat_sequence(
        conversation_id,
        request_id,
        &format!("msg-{template_id}"),
        "fast-model",
        "fast-model",
    );
    events.push(event(
        "agent_task_created_with_template",
        serde_json::json!({
            "mode": "working",
            "isCustomModel": false, "id": template_id, "name": template_name,
            "requestId": request_id,
        }),
    ));
    events.push(event(
        "template_used",
        serde_json::json!({"template_id": template_id, "task_mode": "working"}),
    ));
    events
}

/// 「灵感案例做同款」事件组（JOIN 对话链）。
///
/// 判据是 `playbook_prompt_send`（在 Dialog 里真的发送了 Prompt），
/// 而非卡片曝光或点击 —— 只发曝光事件不会计分。
pub fn playbook_prompt_sequence(
    conversation_id: &str,
    request_id: &str,
    case_id: &str,
    case_name: &str,
) -> Vec<DesktopEvent> {
    let mut events = chat_sequence(
        conversation_id,
        request_id,
        "msg-pb",
        "fast-model",
        "fast-model",
    );
    let payload = serde_json::json!({
        "id": case_id, "name": case_name, "type": "document",
        "categoryId": "", "categoryName": "",
    });
    // 合并顺序与 Go 一致：先 eventCode，再 extra，最后 payload（payload 覆盖同名键）。
    let with_payload = |code: &str, extra: serde_json::Value| {
        let mut m = event(code, extra);
        if let serde_json::Value::Object(o) = payload.clone() {
            for (k, v) in o {
                m.insert(k, v);
            }
        }
        m
    };
    events.push(event(
        "web_element_click",
        serde_json::json!({
            "pageName": "playbook_detail",
            "elementId": "playbook_ctaClick", "elementName": case_name, "source": "discover",
        }),
    ));
    events.push(with_payload(
        "playbook_cta_click",
        serde_json::json!({"source": "discover", "position": 0}),
    ));
    events.push(with_payload(
        "playbook_prompt_send",
        serde_json::json!({"conversationId": conversation_id, "requestId": request_id}),
    ));
    events
}

/// 「设计创意画布」事件组（JOIN 对话链）。
pub fn design_canvas_sequence(conversation_id: &str, request_id: &str) -> Vec<DesktopEvent> {
    let mut events = chat_sequence(
        conversation_id,
        request_id,
        "msg-canvas",
        "fast-model",
        "fast-model",
    );
    events.push(event(
        "wbx_design_canvas_task_create",
        serde_json::json!({
            "conversationId": conversation_id,
            "requestId": request_id, "source": "summon_keyword", "cost": 12000,
            "isSuccessful": true,
        }),
    ));
    events.push(event(
        "wbx_design_canvas_open",
        serde_json::json!({
            "conversationId": conversation_id,
            "requestId": request_id, "id": format!("ardot-file-{}", tail(request_id, 8)),
            "source": "summon_keyword", "type": "page", "cost": 13000, "isSuccessful": true,
        }),
    ));
    events
}

/// 「真实对话 + 技能加载」事件组（`skill_1` 判据）。
///
/// 判据是 `skill_info` 事件（桌面指纹）JOIN 真实会话：客户端在模型发起工具调用
/// （即加载技能）时上报。因此 `chat_message_response` 的 `finishReason` 必须置为
/// `tool_calls` —— 用 `stop` 表示模型只是正常回答，与「加载了技能」矛盾。
pub fn skill_sequence(
    conversation_id: &str,
    request_id: &str,
    skill_name: &str,
    skill_id: &str,
    skill_version: &str,
) -> Vec<DesktopEvent> {
    let message_id = format!("msg-{}", tail(request_id, 8));
    let mut events = chat_sequence(
        conversation_id,
        request_id,
        &message_id,
        "fast-model",
        "fast-model",
    );
    for ev in events.iter_mut() {
        if ev.get("eventCode").and_then(|v| v.as_str()) == Some("chat_message_response") {
            ev.insert("finishReason".into(), serde_json::json!("tool_calls"));
        }
    }
    events.push(event(
        "skill_info",
        serde_json::json!({
            "id": skill_name,
            "skillId": skill_id,
            "skillVersion": skill_version,
            "toolStatus": "success",
            "fileCount": 56,
            "source": "workbuddy-desktop",
            "conversationId": conversation_id, "requestId": request_id, "messageId": message_id,
            "requestModelId": "fast-model", "requestModelName": "fast-model",
            "traceId": request_id,
        }),
    ));
    events
}

/// 皮肤生效事件（`Hp_Appearance` 判据）。
pub fn appearance_apply_event(resource_key: &str) -> DesktopEvent {
    event(
        "appearance_skin_apply",
        serde_json::json!({
            "action": "apply", "source": "settings_close",
            "id": resource_key, "vipLevel": 0, "series": "", "type": "unknown",
        }),
    )
}

// ──────────────────────────── 专家市场 ────────────────────────────

/// 专家市场的单个专家（市场列表响应的子集）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MarketExpert {
    /// 专家 id（`ex_` 前缀）。
    #[serde(default)]
    pub expert_id: String,
    /// 专家类型。
    #[serde(default)]
    pub expert_type: String,
    /// 中文展示名。
    #[serde(default)]
    pub display_name_zh: String,
    /// 中文职业/头衔。
    #[serde(default)]
    pub profession_zh: String,
    /// 版本。
    #[serde(default)]
    pub version: String,
    /// 分类列表（上游是 `[]any`，元素不保证是字符串）。
    #[serde(default)]
    pub categories: Vec<serde_json::Value>,
}

/// 专家的分类标记（事件载荷里的 `type` 字段），默认 `expert-all`。
pub fn expert_category(e: &MarketExpert) -> String {
    if let Some(first) = e.categories.first() {
        if let Some(s) = first.as_str() {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    "expert-all".to_string()
}

/// 专家版本，缺省补 `1.0.0`（上游部分条目 version 为空）。
pub fn expert_version(e: &MarketExpert) -> String {
    if e.version.is_empty() {
        "1.0.0".to_string()
    } else {
        e.version.clone()
    }
}

/// 「召唤平台专家」事件组，需配合 [`Client::chat_with_model`] 与
/// [`expert_actual_use_event`] 构成一次完整的「召唤 + 使用」。
pub fn expert_summon_sequence(e: &MarketExpert) -> Vec<DesktopEvent> {
    vec![
        event(
            "web_element_click",
            serde_json::json!({
                "source": e.expert_id, "type": expert_category(e), "version": expert_version(e),
                "elementId": "expert_summon_click", "elementName": "立即召唤",
                "pageURL": "/C:/Program%20Files/WorkBuddy/resources/app.asar/renderer/index.html",
            }),
        ),
        event(
            "expert_summon_click",
            serde_json::json!({
                "id": e.expert_id, "name": e.display_name_zh,
                "expertTitle": e.profession_zh, "type": "expert-all", "position": 0,
                "expertType": e.expert_type, "version": expert_version(e), "mode": "LOCAL",
            }),
        ),
        event(
            "expert_summoned",
            serde_json::json!({
                "id": e.expert_id, "name": e.display_name_zh,
                "expertTitle": e.profession_zh, "type": "expert-all",
            }),
        ),
    ]
}

/// 「专家真实使用」事件。
///
/// `request_id` 必须是服务端真实分配的 id（见 [`Client::chat_with_model`]），
/// 自造 id 不计数。`mode` 区分 craft 与 LOCAL 两种口径，由调用方按目标任务传
/// （`Expert_lighthouse` 的判据要求 LOCAL）。
pub fn expert_actual_use_event(
    e: &MarketExpert,
    conversation_id: &str,
    request_id: &str,
    mode: &str,
) -> DesktopEvent {
    let mut cost: i64 = 9000;
    let mut type_field = serde_json::json!(expert_category(e));
    if mode == "LOCAL" {
        // LOCAL 口径的真实样本里 type 为空、cost=0，对齐它避免形状不一致。
        type_field = serde_json::json!("");
        cost = 0;
    }
    event(
        "expert_actual_use",
        serde_json::json!({
            "id": e.expert_id, "name": e.display_name_zh, "expertTitle": e.profession_zh,
            "type": type_field, "expertType": e.expert_type, "source": "builtin",
            "version": expert_version(e), "cost": cost, "characterCount": 14, "mode": mode,
            "conversationId": conversation_id, "requestId": request_id,
            "messageId": format!("msg-{}", tail(request_id, 8)),
            "requestModelId": "fast-model", "requestModelName": "fast-model",
        }),
    )
}

// ──────────────────────────── SSE requestId 解析 ────────────────────────────

/// 服务端 requestId 的形状：`cmb-` 前缀 + 32 hex，或裸 32 hex。
///
/// 用形状校验而非「取第一个 id 字段」：SSE 帧里 id 字段不止一个
///（`chatcmpl-xxx` 之类是客户端可自造的会话 id），只有这个形状才是服务端分配的
/// **真实** requestId，而专家类任务的判据恰恰要求它是真实的（自造 UUID 不计分）。
fn is_sse_id(s: &str) -> bool {
    let hex = s.strip_prefix("cmb-").unwrap_or(s);
    hex.len() == 32
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 从一行 SSE 文本里提取符合形状的 id 值；无则返回 `None`。
fn extract_sse_id(line: &str) -> Option<String> {
    let idx = line.find("\"id\":\"")?;
    let rest = &line[idx + 6..];
    let end = rest.find('"')?;
    if end == 0 {
        // Go: `if end <= 0 { return "" }` —— 空 id 不算命中。
        return None;
    }
    let id = &rest[..end];
    if is_sse_id(id) {
        Some(id.to_string())
    } else {
        None
    }
}

/// 从 SSE 流中解析服务端分配的 requestId，并读干剩余流。
///
/// 读干是有意的：提前 return 会让连接停在半开状态，上游侧看到的是一次
/// 「发起后立刻断开」的异常会话；同时残留连接会占住连接池。
/// 因此这里读完（有上限，避免异常流无限增长）再返回。
///
/// 未找到合法 id 时返回错误 —— 调用方必须知道「拿不到真实 id」，
/// 否则会用空 id 去发判据事件，得到一次静默不计分的上报。
async fn read_sse_request_id(mut resp: reqwest::Response) -> Result<String> {
    /// 1MB 上限：正常对话流远小于此；超过说明上游返回异常内容，
    /// 继续读只是浪费带宽且可能永远读不完。
    const MAX_BUF: usize = 1 << 20;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if buf.len() >= MAX_BUF {
            break;
        }
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let remain = MAX_BUF - buf.len();
                buf.extend_from_slice(&chunk[..chunk.len().min(remain)]);
            }
            Ok(None) => break,
            Err(e) => return Err(GatewayError::Transport(e.to_string())),
        }
    }
    for line in buf.split(|b| *b == b'\n') {
        if let Some(id) = extract_sse_id(&String::from_utf8_lossy(line)) {
            return Ok(id);
        }
    }
    Err(GatewayError::Other(
        "SSE 中未找到服务端 requestId".to_string(),
    ))
}

// ──────────────────────────── report.go 的两类小程序事件 ────────────────────────────

/// 小程序（微信容器）指纹的 `chat_request_send` 事件（16 字段）。
///
/// 与桌面版 [`ChatRequestEvent`] 的差异（都来自实测的上游小程序客户端形状）：
///
/// ```text
/// source     = "mini_program"     ← 服务端据此判定来自小程序
/// ideName    = "wx_app_cloud"
/// ideType    = "WorkBuddy_MP"
/// extName    = "workbuddy-mp"     ← 关键指纹，缺了不计分
/// extVersion = "SaaS"
/// mode       = "chat"
/// activityId = school_open_day_2026
/// ```
///
/// 桌面版那套字段（`requestModelId` / `agentName` / `traceId` 等）小程序**不发**，
/// 因此这里刻意用独立结构体而不是给 `ChatRequestEvent` 加字段 —— 后者会让桌面版
/// 也带上小程序字段，把两种指纹混成一个（上游会看出异常）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MiniChatRequestEvent {
    #[serde(rename = "eventCode")]
    event_code: String,
    timestamp: i64,
    #[serde(rename = "reportDelay")]
    report_delay: i64,
    source: String,
    #[serde(rename = "ideName")]
    ide_name: String,
    #[serde(rename = "ideType")]
    ide_type: String,
    #[serde(rename = "extName")]
    ext_name: String,
    #[serde(rename = "extVersion")]
    ext_version: String,
    mode: String,
    #[serde(rename = "conversationId")]
    conversation_id: String,
    #[serde(rename = "requestId")]
    request_id: String,
    #[serde(rename = "inputLength")]
    input_length: i64,
    /// 活动归属。**空值时整个字段不出现在报文里**（`omitempty`）。
    ///
    /// ★ 这条不是风格问题，是语义问题：上游按「有没有 activityId」区分两件事 ——
    /// 带了才算「开学季/校园日活动」的对话（`school_season`），
    /// 不带才算「成长任务集」里的对话（`Sequential_Tasks_1/_3`）。
    /// 若这里不加 `skip_serializing_if`，给 `Sequential_Tasks_1` 发的事件会带上
    /// `"activityId":""` —— 那与「没有该字段」在服务端是两种输入。
    #[serde(rename = "activityId", skip_serializing_if = "String::is_empty")]
    activity_id: String,
    #[serde(rename = "mentionContexts")]
    mention_contexts: Vec<serde_json::Value>,
    #[serde(rename = "mentionContextCount")]
    mention_context_count: i64,
    #[serde(rename = "userId")]
    user_id: String,
}

/// 小程序「专家实际使用」事件（判据 `expert_actual_use`，15 字段）。
///
/// 与 school 域的专家事件是**两套口径**，勿混：这里没有 `conversationId`，
/// 也没有 `activityId` —— 判据是「哪个专家被真实用过」，不是「哪次对话」。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MpExpertUseEvent {
    #[serde(rename = "eventCode")]
    event_code: String,
    timestamp: i64,
    #[serde(rename = "reportDelay")]
    report_delay: i64,
    source: String,
    #[serde(rename = "ideName")]
    ide_name: String,
    #[serde(rename = "ideType")]
    ide_type: String,
    #[serde(rename = "extName")]
    ext_name: String,
    #[serde(rename = "extVersion")]
    ext_version: String,
    id: String,
    name: String,
    #[serde(rename = "expertTitle")]
    expert_title: String,
    #[serde(rename = "type")]
    type_field: String,
    #[serde(rename = "characterCount")]
    character_count: i64,
    #[serde(rename = "expertType")]
    expert_type: String,
    #[serde(rename = "userId")]
    user_id: String,
}

/// 客户端 `chat_request_send` 事件的完整形状（36 字段）。
///
/// 字段与官方客户端一致；`conversationId` 由调用方生成，无需真实会话
/// （服务端不校验会话一致性）。
///
/// ★ **一个 `omitempty` 都没有**：Go 的 struct tag 全是裸 `json:"..."`，
/// 因此零值字段（`false` / `0` / `""` / `[]`）在报文里**照样出现**。
/// 少发一个字段与发零值在服务端是两种输入 —— 这是刻意的，别「优化」成
/// `skip_serializing_if`。
///
/// ★ **`userId` 缺失 ⇒ 200 但静默丢弃**（实测）。故它必须始终出现，
/// 且必须来自 `a.uid`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ChatRequestEvent {
    #[serde(rename = "eventCode")]
    event_code: String,
    timestamp: i64,
    #[serde(rename = "reportDelay")]
    report_delay: i64,
    mode: String,
    #[serde(rename = "conversationId")]
    conversation_id: String,
    #[serde(rename = "requestId")]
    request_id: String,
    #[serde(rename = "inputLength")]
    input_length: i64,
    #[serde(rename = "requestModelId")]
    request_model_id: String,
    #[serde(rename = "requestModelName")]
    request_model_name: String,
    #[serde(rename = "isPlan")]
    is_plan: bool,
    #[serde(rename = "isAutoExecuteTerminal")]
    is_auto_execute_terminal: bool,
    #[serde(rename = "isAutoModify")]
    is_auto_modify: bool,
    #[serde(rename = "codebaseEnable")]
    codebase_enable: bool,
    #[serde(rename = "maxToken")]
    max_token: i64,
    #[serde(rename = "maxSteps")]
    max_steps: i64,
    temperature: i64,
    #[serde(rename = "maxRetries")]
    max_retries: i64,
    #[serde(rename = "mentionContexts")]
    mention_contexts: Vec<serde_json::Value>,
    #[serde(rename = "knowledgeId")]
    knowledge_id: Vec<serde_json::Value>,
    #[serde(rename = "knowledgeName")]
    knowledge_name: Vec<serde_json::Value>,
    #[serde(rename = "codebaseId")]
    codebase_id: String,
    #[serde(rename = "mentionContextCount")]
    mention_context_count: i64,
    command: String,
    #[serde(rename = "expertId")]
    expert_id: String,
    #[serde(rename = "recommendId")]
    recommend_id: String,
    #[serde(rename = "skillId")]
    skill_id: String,
    #[serde(rename = "skillCount")]
    skill_count: i64,
    #[serde(rename = "totalCount")]
    total_count: i64,
    #[serde(rename = "fileUri")]
    file_uri: String,
    #[serde(rename = "presentAt")]
    present_at: i64,
    #[serde(rename = "traceId")]
    trace_id: String,
    #[serde(rename = "rootRequestId")]
    root_request_id: String,
    #[serde(rename = "parentConversationId")]
    parent_conversation_id: String,
    #[serde(rename = "agentName")]
    agent_name: String,
    #[serde(rename = "agentType")]
    agent_type: String,
    #[serde(rename = "userId")]
    user_id: String,
}

/// 构造一条 `chat_request_send` 事件（纯函数，模型字段可定制）。
///
/// 与 Go `ReportChatActivity` / `ReportChatActivityModel` 共用的形状：
/// 后者只把 `requestModelId` / `requestModelName` 换成调用方给的值
/// （部分任务的判据是「用过某个**特定模型**对话」，如 `Model_chat_GLM5.2`）。
fn chat_request_event(
    a: &Auth,
    conversation_id: &str,
    request_id: &str,
    model_id: &str,
    model_name: &str,
) -> ChatRequestEvent {
    let now = now_ms();
    ChatRequestEvent {
        event_code: "chat_request_send".to_string(),
        timestamp: now,
        report_delay: 0,
        mode: "craft".to_string(),
        conversation_id: conversation_id.to_string(),
        request_id: request_id.to_string(),
        input_length: 12,
        request_model_id: model_id.to_string(),
        request_model_name: model_name.to_string(),
        is_plan: false,
        is_auto_execute_terminal: false,
        is_auto_modify: false,
        codebase_enable: false,
        max_token: 0,
        max_steps: 0,
        temperature: 0,
        max_retries: 0,
        mention_contexts: Vec::new(),
        knowledge_id: Vec::new(),
        knowledge_name: Vec::new(),
        codebase_id: String::new(),
        mention_context_count: 0,
        command: String::new(),
        expert_id: String::new(),
        recommend_id: String::new(),
        skill_id: String::new(),
        skill_count: 0,
        total_count: 0,
        file_uri: String::new(),
        present_at: now,
        trace_id: String::new(),
        root_request_id: conversation_id.to_string(),
        parent_conversation_id: conversation_id.to_string(),
        agent_name: "default".to_string(),
        agent_type: "conversation".to_string(),
        user_id: a.uid.clone(),
    }
}

// ──────────────────────────── 域调用 ────────────────────────────

impl Client {
    /// billing 域请求，**直接发已序列化的字节**。
    ///
    /// ★ 为什么不能复用 [`Client::billing_json`]：它的入参是
    /// `Option<&serde_json::Value>`，结构体必须先转成 `Value` 才能传进去 ——
    /// 而本 crate 未启用 `serde_json/preserve_order`，`Value` 的对象是
    /// `BTreeMap`，中转一次就会把**字段声明序重排成字典序**。
    ///
    /// 这个差别是**真实的行为偏离**，不是格式偏好：Go 的
    /// `json.Marshal([]chatRequestEvent{ev})` 直接序列化结构体，键序即
    /// struct 声明序且稳定，上游完全可以把它当指纹的一部分读。实测口径里
    /// 「形状不对 ⇒ HTTP 200 但静默不计分」是最难排查的一类失败，所以这里
    /// 宁可多写几行也不让键序被悄悄改掉。
    ///
    /// 头族与 [`Client::billing_json`] 完全一致（`BillingHeaders` + Go 默认 UA），
    /// 信封解析同样走 [`Client::do_json`]，两者只差「谁来序列化请求体」。
    async fn billing_post_raw(&self, a: &Auth, path: &str, raw: Vec<u8>) -> Result<Vec<u8>> {
        let url = format!("{}{path}", self.billing_base(a));
        let req = self
            .http()
            .post(url)
            .headers(crate::upstream::headers::billing_headers(a))
            .header(reqwest::header::USER_AGENT, BILLING_GO_UA)
            .body(raw);
        self.do_json(req).await
    }

    /// 发送一条对话活跃上报（`chat_request_send`）。
    ///
    /// `conversation_id` 由调用方生成（形如 `wb2api-<毫秒>`）；同一会话的多条
    /// 上报共用它，但 `request_id` 必须各不相同（服务端按事件去重）。
    /// `request_id` 为空时回落到 `conversation_id`。
    ///
    /// ★ 走 **billing 域**（国服 = `codebuddy.cn`，国际版 = `workbuddy.ai`），
    /// 与成长域的 `growth_json`（chat 域）分属两个域，不可混用。
    ///
    /// 错误语义与 [`Client::billing_json`] 一致：HTTP 非 2xx 或业务 `code != 0`
    /// → [`GatewayError::UpstreamError`]。
    pub async fn report_chat_activity(
        &self,
        a: &Auth,
        conversation_id: &str,
        request_id: &str,
    ) -> Result<()> {
        self.report_chat_activity_model(
            a,
            conversation_id,
            request_id,
            "deepseek-v4-flash",
            "DeepSeek V4 Flash",
        )
        .await
    }

    /// 同 [`Client::report_chat_activity`]，但把模型字段对齐到指定模型。
    ///
    /// 事件形状完全一致、只有模型字段不同，故复用同一事件结构 ——
    /// 避免维护两份「客户端事件形状」定义而逐渐漂移。
    pub async fn report_chat_activity_model(
        &self,
        a: &Auth,
        conversation_id: &str,
        request_id: &str,
        model_id: &str,
        model_name: &str,
    ) -> Result<()> {
        let request_id = if request_id.is_empty() {
            conversation_id
        } else {
            request_id
        };
        let ev = chat_request_event(a, conversation_id, request_id, model_id, model_name);
        // ★ 用 `to_vec` 直接序列化结构体（而不是 `json!([ev])`）：中转 `Value`
        // 会把键序重排成字典序，破坏 Go 的 struct 声明序。详见 `billing_post_raw`。
        let raw = serde_json::to_vec(&vec![ev])
            .map_err(|e| GatewayError::Other(format!("chat activity marshal: {e}")))?;
        self.billing_post_raw(a, DESKTOP_REPORT_PATH, raw)
            .await
            .map(|_| ())
    }

    /// 以桌面客户端指纹上报一批事件。
    ///
    /// 空事件列表直接返回 `Ok(())`（不报错）：调用方按动作表派发时可能因状态
    /// 跳过全部事件，此时「什么都没发」是正确行为而非错误。
    ///
    /// 业务字段**覆盖**同名指纹字段（可用于对齐真实设备）。
    pub async fn report_desktop_events(&self, a: &Auth, events: &[DesktopEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let fp = desktop_fingerprint(a);
        let arr: Vec<DesktopEvent> = events
            .iter()
            .map(|ev| {
                let mut m = fp.clone();
                for (k, v) in ev {
                    m.insert(k.clone(), v.clone());
                }
                m
            })
            .collect();
        let raw = serde_json::to_vec(&arr)
            .map_err(|e| GatewayError::Other(format!("desktop events marshal: {e}")))?;
        let base = self.chat_base(a).to_string();
        let mut h = reqwest::header::HeaderMap::new();
        put_header(&mut h, "Authorization", &format!("Bearer {}", a.access_token));
        put_header(&mut h, "Accept", "application/json, text/plain, */*");
        put_header(&mut h, "Content-Type", "application/json;charset=UTF-8");
        put_header(&mut h, "User-Agent", DESKTOP_UA);
        put_header(&mut h, "X-Domain", &base);
        put_header(&mut h, "X-Product", "SaaS");
        // X-Request-ID 拼上纳秒尾巴：同账号的多次上报不能共用同一个请求 id。
        put_header(
            &mut h,
            "X-Request-ID",
            &format!("{}{}", derive_id(a, "req"), itoa64(unix_nano() % 1_000_000)),
        );
        if !a.uid.is_empty() {
            put_header(&mut h, "X-User-Id", &a.uid);
        }
        let req = self
            .http()
            .post(format!("{base}{DESKTOP_REPORT_PATH}"))
            .headers(h)
            .body(raw);
        self.do_json(req).await.map(|_| ())
    }

    /// 应用外观主题（桌面域）。
    ///
    /// 单独留痕用：`Hp_Appearance` 的判据是随后那条 `appearance_skin_apply` 事件，
    /// 本调用让服务端侧存在「该账号确实设置过这个主题」的记录，使事件更自洽。
    pub async fn set_appearance_theme(&self, a: &Auth, resource_key: &str) -> Result<()> {
        let raw = serde_json::to_vec(&serde_json::json!({
            "kind": "theme", "resource_key": resource_key,
        }))
        .map_err(|e| GatewayError::Other(format!("appearance marshal: {e}")))?;
        let base = self.chat_base(a).to_string();
        let mut h = reqwest::header::HeaderMap::new();
        put_header(&mut h, "Authorization", &format!("Bearer {}", a.access_token));
        put_header(&mut h, "Accept", "application/json, text/plain, */*");
        put_header(&mut h, "Content-Type", "application/json;charset=UTF-8");
        put_header(&mut h, "User-Agent", DESKTOP_UA);
        put_header(&mut h, "X-Product", "SaaS");
        if !a.uid.is_empty() {
            put_header(&mut h, "X-User-Id", &a.uid);
        }
        let req = self
            .http()
            .post(format!("{base}{APPEARANCE_SET_PATH}"))
            .headers(h)
            .body(raw);
        self.do_json(req).await.map(|_| ())
    }

    /// 以 Web 浏览器指纹上报单条事件。
    ///
    /// 与桌面指纹的区别不只是基址：web 域事件是浏览器形状
    ///（`os`/`arch`/`userAgent`/`machineId`），没有 `ideName`/`extName` 那一族
    /// 桌面字段。混用会得到「桌面字段出现在网页事件里」的矛盾指纹。
    pub async fn report_web_event(
        &self,
        a: &Auth,
        event_code: &str,
        page_url: &str,
        element_id: &str,
        element_name: &str,
    ) -> Result<()> {
        let ev = event(
            event_code,
            serde_json::json!({
                "timestamp": now_ms(), "reportDelay": 0,
                "pageURL": page_url, "elementId": element_id, "elementName": element_name,
                "os": "Win32", "arch": "", "osVersion": "10.0", "userAgent": WEB_UA,
                "machineId": derive_id(a, "webmachine"), "userId": a.uid,
                "userNickname": a.nickname, "enterpriseId": a.enterprise_id,
            }),
        );
        let raw = serde_json::to_vec(&vec![ev])
            .map_err(|e| GatewayError::Other(format!("web event marshal: {e}")))?;
        let base = self.web_base(a).to_string();
        let mut h = reqwest::header::HeaderMap::new();
        put_header(&mut h, "Authorization", &format!("Bearer {}", a.access_token));
        put_header(&mut h, "Content-Type", "application/json");
        put_header(&mut h, "Accept", "application/json");
        put_header(&mut h, "x-client-platform", "web");
        put_header(&mut h, "Origin", &base);
        put_header(&mut h, "Referer", page_url);
        put_header(&mut h, "User-Agent", WEB_UA);
        if !a.uid.is_empty() {
            put_header(&mut h, "X-User-Id", &a.uid);
        }
        let req = self
            .http()
            .post(format!("{base}{DESKTOP_REPORT_PATH}"))
            .headers(h)
            .body(raw);
        self.do_json(req).await.map(|_| ())
    }

    /// 发一条真实桌面指纹对话，返回 `(conversationID, requestID)`。
    ///
    /// 为什么需要真实对话而不是全部靠事件伪造：专家类与 `skill_1` 的判据事件要求
    /// `requestId` 是**服务端真实产生**的 id，自造 UUID 不计分。服务端在 SSE 流的
    /// 首个 data 帧里回这个 id，故必须真的发一次请求并读流。
    ///
    /// 读干流是有意的：不读完会在连接池里留下半开的 SSE 连接，
    /// 上游侧则看到一个「发起后立刻断开」的异常会话。
    pub async fn chat_with_model(
        &self,
        a: &Auth,
        model_id: &str,
        prompt: &str,
        expert_id: &str,
    ) -> Result<(String, String)> {
        let conversation_id = format!("wb2api-conv-{}", itoa64(unix_nano()));
        let body = serde_json::json!({
            "model": model_id,
            "messages": [
                {"role": "system", "content": "You are a helpful assistant. 当前处于中文环境，使用简体中文回答。"},
                {"role": "user", "content": prompt},
            ],
            "agent": "cli",
            "temperature": 1,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        let raw = serde_json::to_vec(&body)
            .map_err(|e| GatewayError::Other(format!("chat body marshal: {e}")))?;
        let base = self.chat_base(a).to_string();
        let mut h = reqwest::header::HeaderMap::new();
        put_header(&mut h, "Authorization", &format!("Bearer {}", a.access_token));
        put_header(&mut h, "Content-Type", "application/json");
        put_header(&mut h, "Accept", "text/event-stream");
        put_header(&mut h, "User-Agent", DESKTOP_UA);
        put_header(&mut h, "X-Domain", &base);
        put_header(&mut h, "X-Product", "SaaS");
        put_header(&mut h, "X-User-Id", &a.uid);
        put_header(&mut h, "X-Conversation-ID", &conversation_id);
        put_header(&mut h, "X-Request-ID", &itoa64(unix_nano()));
        put_header(&mut h, "X-Agent-Intent", "craft");
        put_header(&mut h, "X-Agent-Type", "main");
        put_header(&mut h, "X-IDE-Name", "WorkBuddy");
        put_header(&mut h, "X-IDE-Type", "WorkBuddy");
        put_header(&mut h, "X-IDE-Version", "5.5.6");
        put_header(&mut h, "x-codebuddy-request", "1");
        if !expert_id.is_empty() {
            put_header(&mut h, "X-Expert-Id", expert_id);
        }
        // ★ 用 chat 客户端：SSE 可能持续很久，短 RPC 的总超时会中途掐断流。
        let resp = self
            .chat_http()
            .post(format!("{base}{CHAT_COMPLETIONS_PATH}"))
            .headers(h)
            .body(raw)
            .send()
            .await
            .map_err(|e| GatewayError::Transport(e.to_string()))?;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(GatewayError::Other(format!(
                "chat http {}",
                resp.status().as_u16()
            )));
        }
        let rid = read_sse_request_id(resp).await?;
        Ok((conversation_id, rid))
    }

    /// 拉取专家市场的真实专家列表。
    ///
    /// 为什么必须「真实」：专家类任务的判据校验专家 id 在平台上真实存在，
    /// 自造 id 的上报会被静默丢弃（HTTP 200 但不计分）。因此 id 只能从本接口取。
    pub async fn market_expert_list(
        &self,
        a: &Auth,
        expert_type: &str,
    ) -> Result<Vec<MarketExpert>> {
        let mut body = serde_json::json!({
            "page": 1, "page_size": 20, "sort_by": "reco_rank", "sort_order": "desc",
        });
        if !expert_type.is_empty() {
            body["expert_type"] = serde_json::json!(expert_type);
        }
        let raw = serde_json::to_vec(&body)
            .map_err(|e| GatewayError::Other(format!("expert list body marshal: {e}")))?;
        let base = self.chat_base(a).to_string();
        let mut h = reqwest::header::HeaderMap::new();
        put_header(&mut h, "Authorization", &format!("Bearer {}", a.access_token));
        put_header(&mut h, "Content-Type", "application/json");
        put_header(&mut h, "User-Agent", DESKTOP_UA);
        put_header(&mut h, "X-Domain", &base);
        put_header(&mut h, "X-Product", "SaaS");
        if !a.uid.is_empty() {
            put_header(&mut h, "X-User-Id", &a.uid);
        }
        let req = self
            .http()
            .post(format!("{base}{MARKET_EXPERT_LIST_PATH}"))
            .headers(h)
            .body(raw);
        let data = self.do_json(req).await?;
        #[derive(serde::Deserialize)]
        struct Out {
            #[serde(default)]
            experts: Vec<MarketExpert>,
        }
        let out: Out = serde_json::from_slice(&data)
            .map_err(|e| GatewayError::UpstreamParse(format!("expert list parse: {e}")))?;
        Ok(out.experts)
    }

    /// 发一条小程序指纹的对话事件，点亮「校园日」任务。
    ///
    /// 为什么必须用小程序指纹而不是复用桌面版上报：实测（2026-09-18，真实账号）
    /// 桌面版形状的事件发出去 HTTP 200 但 growth 域进度**恒为 0**；
    /// 换成这套小程序指纹 + `activityId` 后 `current` 从 0 变 1、
    /// `accept_status` 变 completed、claim 成功到账 100 积分 + 5 能量。
    pub async fn report_school_season_activity(
        &self,
        a: &Auth,
        conversation_id: &str,
    ) -> Result<()> {
        self.report_mini_chat_activity(a, conversation_id, SCHOOL_SEASON_ACTIVITY_ID)
            .await
    }

    /// 发一条小程序指纹的对话事件，**不带 `activityId`**。
    ///
    /// 这是 `Sequential_Tasks_1` / `_3`（成长任务集里的小程序对话任务）的判据。
    ///
    /// ★ 与 [`Client::report_school_season_activity`] 的唯一差别就是 `activityId`
    /// 的有无 —— 而服务端正是靠它区分两个任务集（2026-09-27 实测确认）：
    ///
    /// ```text
    /// 带 activityId=school_open_day_2026 → 计入「校园日」活动（school_season）
    /// 不带 activityId                   → 计入「成长任务集」（Sequential_Tasks_1/_3）
    /// ```
    ///
    /// 因此**绝不能**复用 `report_school_season_activity` 去点 `Sequential_Tasks_1`：
    /// 那会给事件带上活动归属，服务端不会把它算作成长任务集的对话 ——
    /// 表现为 HTTP 200 但 growth 域进度恒为 0，是最难排查的一类失败。
    pub async fn report_mp_chat_activity(&self, a: &Auth, conversation_id: &str) -> Result<()> {
        self.report_mini_chat_activity(a, conversation_id, "").await
    }

    /// 小程序指纹对话上报的公共实现（`activity_id` 空 = 不带该字段）。
    async fn report_mini_chat_activity(
        &self,
        a: &Auth,
        conversation_id: &str,
        activity_id: &str,
    ) -> Result<()> {
        let conversation_id = if conversation_id.is_empty() {
            format!("wb-run-{}", now_ms())
        } else {
            conversation_id.to_string()
        };
        let ev = MiniChatRequestEvent {
            event_code: "chat_request_send".to_string(),
            timestamp: now_ms(),
            report_delay: 0,
            source: "mini_program".to_string(),
            ide_name: "wx_app_cloud".to_string(),
            ide_type: "WorkBuddy_MP".to_string(),
            ext_name: "workbuddy-mp".to_string(),
            ext_version: "SaaS".to_string(),
            mode: "chat".to_string(),
            conversation_id: conversation_id.clone(),
            request_id: conversation_id,
            input_length: 12,
            activity_id: activity_id.to_string(),
            mention_contexts: Vec::new(),
            mention_context_count: 0,
            user_id: a.uid.clone(),
        };
        let raw = serde_json::to_vec(&vec![ev])
            .map_err(|e| GatewayError::Other(format!("mini chat activity marshal: {e}")))?;
        self.billing_post_raw(a, DESKTOP_REPORT_PATH, raw)
            .await
            .map(|_| ())
    }

    /// 上报「专家被真实使用」，点亮 `Sequential_Tasks_2`。
    ///
    /// `expert_id` 必须是**专家市场里真实存在**的 `ex_` id（由
    /// [`Client::market_expert_list`] 取得）：自造 id 的上报会 HTTP 200 但不计分
    /// —— 服务端校验该 id 在平台上存在。
    ///
    /// `extVersion` 刻意用 `2.2.8` 而不是对话事件的 `SaaS`：这两个事件来自小程序内
    /// **不同的 SDK 版本**，照抄官方形状而不是统一成一个值（统一反而会成为指纹异常）。
    pub async fn report_mp_expert_use(
        &self,
        a: &Auth,
        expert_id: &str,
        expert_name: &str,
        expert_type: &str,
    ) -> Result<()> {
        let expert_name = if expert_name.is_empty() {
            expert_id
        } else {
            expert_name
        };
        let expert_type = if expert_type.is_empty() {
            "agent"
        } else {
            expert_type
        };
        let ev = MpExpertUseEvent {
            event_code: "expert_actual_use".to_string(),
            timestamp: now_ms(),
            report_delay: 0,
            source: "mini_program".to_string(),
            ide_name: "wx_app_cloud".to_string(),
            ide_type: "WorkBuddy_MP".to_string(),
            ext_name: "workbuddy-mp".to_string(),
            ext_version: "2.2.8".to_string(),
            id: expert_id.to_string(),
            // ★ Go 侧这里是 expertID（不是 expertName）：`Name` 与 `ID` 同值，
            // 人类可读的名字走 `ExpertTitle`。
            name: expert_id.to_string(),
            expert_title: expert_name.to_string(),
            type_field: "send_message".to_string(),
            character_count: 12,
            expert_type: expert_type.to_string(),
            user_id: a.uid.clone(),
        };
        let raw = serde_json::to_vec(&vec![ev])
            .map_err(|e| GatewayError::Other(format!("mp expert use marshal: {e}")))?;
        self.billing_post_raw(a, DESKTOP_REPORT_PATH, raw)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn account() -> Auth {
        Auth {
            access_token: "at-test".into(),
            uid: "uid-test".into(),
            enterprise_id: "ent-test".into(),
            nickname: "nick-test".into(),
            domain: "https://copilot.tencent.com".into(),
            ..Default::default()
        }
    }

    // ── 假上游 ──

    /// 假上游收到的一次请求。
    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            let want = name.to_ascii_lowercase();
            self.headers
                .iter()
                .find(|(k, _)| k.to_ascii_lowercase() == want)
                .map(|(_, v)| v.as_str())
        }
        fn has_header(&self, name: &str) -> bool {
            self.header(name).is_some()
        }
        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.body).expect("请求体必须是 JSON")
        }
    }

    fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// 从**原始报文字节**里按出现顺序取出第一个 JSON 对象的顶层键。
    ///
    /// ★ 不能用 `serde_json` 解析后再取 `.keys()`：`serde_json::Map` 默认是
    /// `BTreeMap`（本 crate 未开 `preserve_order`），解析这一步就把真实键序丢成
    /// 字典序 —— 那样断言「键序」永远只能看到排序后的结果，等于没断。
    ///
    /// 而键序是**上游可观测的字节序列**：Go 用 `json.Marshal` 序列化 struct，
    /// 序即字段声明序。生产侧若图省事写成 `serde_json::json!([ev])` 中转一次，
    /// 线上就会静默变成字典序（`chat_request_event_has_all_36_go_keys_in_wire_order`
    /// 正是这样抓到过这个真 bug）。所以只能在原始文本上扫。
    ///
    /// 只认深度为 1 且紧跟 `:` 的字符串（嵌套对象的键深度 ≥2，会被跳过；
    /// 字符串**值**后面跟的是 `,` 或 `}`，不会被误认成键）。
    fn wire_key_order(body: &str) -> Vec<String> {
        let b = body.as_bytes();
        let mut i = 0usize;
        while i < b.len() && b[i] != b'{' {
            i += 1;
        }
        if i >= b.len() {
            return Vec::new();
        }
        i += 1;
        let mut keys = Vec::new();
        let mut depth = 1usize;
        while i < b.len() && depth > 0 {
            match b[i] {
                b'"' => {
                    let start = i + 1;
                    let mut j = start;
                    while j < b.len() && b[j] != b'"' {
                        // 转义序列整体跳过，避免把 `\"` 当成字符串结尾。
                        if b[j] == b'\\' {
                            j += 1;
                        }
                        j += 1;
                    }
                    if depth == 1 {
                        let mut k = j + 1;
                        while k < b.len() && (b[k] as char).is_ascii_whitespace() {
                            k += 1;
                        }
                        if k < b.len() && b[k] == b':' {
                            keys.push(body[start..j].to_string());
                        }
                    }
                    i = j + 1;
                }
                b'{' | b'[' => {
                    depth += 1;
                    i += 1;
                }
                b'}' | b']' => {
                    depth -= 1;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        keys
    }

    /// 起一个假上游（`std::net::TcpListener`；tokio 的 `net` feature 未启用）。
    ///
    /// 返回 `(base_url, seen)`。响应体固定为 `resp_body`。
    fn spawn_fake_upstream(
        status: u16,
        resp_body: &'static str,
    ) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for stream in listener.incoming() {
                let mut s = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let mut raw: Vec<u8> = Vec::new();
                let mut buf = [0u8; 4096];
                while find_subslice(&raw, b"\r\n\r\n").is_none() {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => raw.extend_from_slice(&buf[..n]),
                    }
                }
                let header_end = match find_subslice(&raw, b"\r\n\r\n") {
                    Some(p) => p + 4,
                    None => raw.len(),
                };
                let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
                let mut lines = head.split("\r\n");
                let req_line = lines.next().unwrap_or("");
                let mut parts = req_line.split(' ');
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let mut headers: Vec<(String, String)> = Vec::new();
                let mut content_length = 0usize;
                for l in lines {
                    if let Some((k, v)) = l.split_once(':') {
                        let (k, v) = (k.trim().to_string(), v.trim().to_string());
                        if k.eq_ignore_ascii_case("content-length") {
                            content_length = v.parse().unwrap_or(0);
                        }
                        headers.push((k, v));
                    }
                }
                while raw.len() < header_end + content_length {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => raw.extend_from_slice(&buf[..n]),
                    }
                }
                let body = String::from_utf8_lossy(&raw[header_end.min(raw.len())..]).to_string();
                sink.lock().unwrap().push(Seen {
                    method,
                    path,
                    headers,
                    body,
                });
                let reason = match status {
                    200 => "OK",
                    400 => "Bad Request",
                    500 => "Internal Server Error",
                    _ => "Status",
                };
                let out = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                );
                let _ = s.write_all(out.as_bytes());
                let _ = s.flush();
            }
        });
        (base, seen)
    }

    /// 起一个假上游，并把三个域（chat / billing / web）全部指向它。
    fn client_for(base: &str) -> Client {
        Client::new(crate::upstream::ClientConfig {
            chat_base_cn: base.to_string(),
            billing_base_cn: base.to_string(),
            web_base_cn: base.to_string(),
            ..Default::default()
        })
        .unwrap()
    }

    // ── derive_id 已知答案向量 ──

    /// `sha256(salt + ":" + uid)` 前 18 字节的 hex —— 36 字符。
    ///
    /// 向量是外部算出的（PowerShell `SHA256` + 手工截断），不是「跑一遍记下来」：
    /// 这样才挡得住「把 salt 与 uid 顺序写反」「截 16 字节」「hex 大写」这类改动。
    #[test]
    fn derive_id_known_answer_vectors() {
        let a = account();
        assert_eq!(derive_id(&a, "machine"), "b5ab3577010213b820f289591bd4024a603b");
        assert_eq!(derive_id(&a, "session"), "c31cd10a7a8bcfa777506407d88e04fb65a1");
        assert_eq!(
            derive_id(&a, "webmachine"),
            "aa8450b508767cb23519c9606b0186f16715"
        );
        assert_eq!(derive_id(&a, "req"), "e7f1f3acab0253f9c8ce9d448dab7db687fe");
        // 空 uid：冒号仍在（salt + ":" + ""），不是 salt 本身。
        let empty = Auth::default();
        assert_eq!(
            derive_id(&empty, "machine"),
            "5c057823f103aac6120c69b83efc21ae30f5"
        );
    }

    #[test]
    fn derive_id_is_stable_and_36_hex() {
        let a = account();
        assert_eq!(derive_id(&a, "machine"), derive_id(&a, "machine"));
        assert_ne!(derive_id(&a, "machine"), derive_id(&a, "session"));
        let id = derive_id(&a, "machine");
        assert_eq!(id.len(), 36);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    // ── tail ──

    #[test]
    fn tail_edge_cases() {
        assert_eq!(tail("abcdefghij", 4), "ghij");
        assert_eq!(tail("abc", 8), "abc");
        assert_eq!(tail("", 4), "");
        assert_eq!(tail("abcd", 4), "abcd");
        // 多字节字符：不 panic（Go 允许切出半个字符，Rust 回退到字符边界）。
        let s = "请求标识";
        assert!(s.len() > 3);
        let t = tail(s, 3);
        assert!(s.ends_with(&t));
    }

    // ── desktop_fingerprint ──

    #[test]
    fn desktop_fingerprint_has_22_keys_with_go_values() {
        let a = account();
        let fp = desktop_fingerprint(&a);
        assert_eq!(fp.len(), 22, "指纹键数必须与 Go desktopFingerprint 一致");
        assert_eq!(fp["timezone"], serde_json::json!("Asia/Shanghai"));
        assert_eq!(fp["reportDelay"], serde_json::json!(2000));
        assert_eq!(fp["userId"], serde_json::json!("uid-test"));
        assert_eq!(fp["username"], serde_json::json!("nick-test"));
        assert_eq!(fp["userNickname"], serde_json::json!("nick-test"));
        assert_eq!(fp["product"], serde_json::json!("SaaS"));
        assert_eq!(fp["releaseDate"], serde_json::json!(1789036585355i64));
        assert_eq!(
            fp["commit"],
            serde_json::json!("5f9692923c93033111c51ad7b003eb80204a9b75")
        );
        assert_eq!(fp["ideName"], serde_json::json!("WorkBuddy"));
        assert_eq!(fp["ideType"], serde_json::json!("WorkBuddy"));
        assert_eq!(fp["ideVersion"], serde_json::json!("5.5.6"));
        assert_eq!(fp["extName"], serde_json::json!("workbuddy-desktop"));
        assert_eq!(fp["extVersion"], serde_json::json!("5.5.6"));
        assert_eq!(fp["os"], serde_json::json!("win32"));
        assert_eq!(fp["arch"], serde_json::json!("x64"));
        assert_eq!(fp["osVersion"], serde_json::json!("10.0.26220"));
        assert_eq!(fp["cpuCores"], serde_json::json!(20));
        assert_eq!(fp["memorySize"], serde_json::json!(24));
        // machineId / sessionId 是派生的，不是随机。
        assert_eq!(fp["machineId"], serde_json::json!(derive_id(&a, "machine")));
        assert_eq!(fp["sessionId"], serde_json::json!(derive_id(&a, "session")));
        assert_eq!(fp["timestamp"], fp["presentAt"]);
    }

    // ── 事件链 ──

    #[test]
    fn chat_sequence_emits_six_events_in_go_order() {
        let events = chat_sequence("conv-1", "req-1", "msg-1", "fast-model", "fast-model");
        let codes: Vec<&str> = events
            .iter()
            .map(|e| e["eventCode"].as_str().unwrap())
            .collect();
        assert_eq!(
            codes,
            vec![
                "agent_task_created",
                "chat_message_send",
                "chat_request_send",
                "chat_message_response",
                "chat_message_status",
                "chat_request_response",
            ]
        );
        // ★ 成功回执是整个链的判据：isSuccessful 必须为 true。
        assert_eq!(events[3]["isSuccessful"], serde_json::json!(true));
        assert_eq!(events[5]["isSuccessful"], serde_json::json!(true));
        // assistant 消息 id 由 messageID 派生。
        assert_eq!(events[1]["messageId"], serde_json::json!("msg-1-assistant"));
        assert_eq!(events[3]["messageId"], serde_json::json!("msg-1-assistant"));
        assert_eq!(events[4]["messageId"], serde_json::json!("msg-1-assistant"));
        // 关联字段：requestId / conversationId 全链一致。
        assert_eq!(events[2]["rootRequestId"], serde_json::json!("req-1"));
        assert_eq!(events[2]["parentConversationId"], serde_json::json!("conv-1"));
        assert_eq!(events[3]["conversationId"], serde_json::json!("conv-1"));
        // 模型字段透传到两个事件。
        assert_eq!(events[0]["requestModelId"], serde_json::json!("fast-model"));
        assert_eq!(events[3]["responseModelId"], serde_json::json!("fast-model"));
        // 点号键（Go 侧原样保留）。
        assert_eq!(
            events[2]["codebuddy.session_id"],
            serde_json::json!("conv-1")
        );
        assert_eq!(
            events[2]["codebuddy.conversation_request_id"],
            serde_json::json!("req-1")
        );
        // 空切片是 [] 而不是 null。
        assert_eq!(events[2]["mentionContexts"], serde_json::json!([]));
        assert_eq!(events[0]["connector_types"], serde_json::json!([]));
        // chat_request_send 的 maxSteps 是 500（不是 0，与 report.go 的 0 不同）。
        assert_eq!(events[2]["maxSteps"], serde_json::json!(500));
        assert_eq!(events[2]["inputLength"], serde_json::json!(24));
        assert!(events[3]["firstTokenAt"].as_i64().unwrap() > 1_600_000_000_000);
    }

    #[test]
    fn buddy_app_sequence_emits_five_events() {
        let events = buddy_app_sequence("buddy-1", "企鹅教师助手");
        let codes: Vec<&str> = events
            .iter()
            .map(|e| e["eventCode"].as_str().unwrap())
            .collect();
        assert_eq!(
            codes,
            vec![
                "buddyapp_discover_click",
                "buddyapp_show",
                "buddyapp_enter_click",
                "buddyapp_auth_confirm_click",
                "buddyapp_bindaccount_skip_click",
            ]
        );
        for ev in &events {
            assert_eq!(ev["mode"], serde_json::json!("LOCAL"));
            assert_eq!(ev["buddyId"], serde_json::json!("buddy-1"));
            assert_eq!(ev["buddyName"], serde_json::json!("企鹅教师助手"));
        }
        // 只有 discover_click 没有 element 字段。
        assert!(!events[0].contains_key("elementId"));
        assert_eq!(events[1]["position"], serde_json::json!(2));
        assert_eq!(events[2]["position"], serde_json::json!(2));
        assert_eq!(events[2]["isFirstPage"], serde_json::json!("1"));
        assert_eq!(events[3]["elementId"], serde_json::json!("buddy-1"));
        assert_eq!(events[4]["elementName"], serde_json::json!("企鹅教师助手"));
    }

    #[test]
    fn automation_create_event_shape() {
        let ev = automation_create_event("每日报表");
        assert_eq!(ev["eventCode"], serde_json::json!("automated_task_create_suc"));
        assert_eq!(ev["name"], serde_json::json!("每日报表"));
        assert_eq!(ev["source"], serde_json::json!("manually"));
        assert_eq!(ev["modelId"], serde_json::json!("fast-model"));
        assert_eq!(ev["modelIsThinking"], serde_json::json!(true));
        assert_eq!(ev["scheduleType"], serde_json::json!("once"));
        assert_eq!(ev["mode"], serde_json::json!("LOCAL"));
        assert_eq!(ev.len(), 10);
    }

    #[test]
    fn template_use_sequence_appends_two_events() {
        let events = template_use_sequence("conv-t", "req-t", "tpl-1", "周报模板");
        assert_eq!(events.len(), 8, "6 条对话链 + 2 条模板事件");
        let created = &events[6];
        assert_eq!(
            created["eventCode"],
            serde_json::json!("agent_task_created_with_template")
        );
        assert_eq!(created["mode"], serde_json::json!("working"));
        assert_eq!(created["isCustomModel"], serde_json::json!(false));
        assert_eq!(created["id"], serde_json::json!("tpl-1"));
        assert_eq!(created["name"], serde_json::json!("周报模板"));
        assert_eq!(created["requestId"], serde_json::json!("req-t"));
        let used = &events[7];
        assert_eq!(used["eventCode"], serde_json::json!("template_used"));
        assert_eq!(used["template_id"], serde_json::json!("tpl-1"));
        assert_eq!(used["task_mode"], serde_json::json!("working"));
        // 对话链的 messageId 是 msg-<templateID>。
        assert_eq!(events[0]["messageId"], serde_json::json!("msg-tpl-1"));
    }

    #[test]
    fn playbook_prompt_sequence_merges_payload_into_last_two() {
        let events = playbook_prompt_sequence("conv-p", "req-p", "case-1", "案例名");
        assert_eq!(events.len(), 9);
        assert_eq!(events[6]["eventCode"], serde_json::json!("web_element_click"));
        assert_eq!(events[6]["pageName"], serde_json::json!("playbook_detail"));
        assert_eq!(
            events[6]["elementId"],
            serde_json::json!("playbook_ctaClick")
        );
        // 判据事件：playbook_prompt_send 必须带 conversationId/requestId + 案例载荷。
        let prompt = &events[8];
        assert_eq!(
            prompt["eventCode"],
            serde_json::json!("playbook_prompt_send")
        );
        assert_eq!(prompt["conversationId"], serde_json::json!("conv-p"));
        assert_eq!(prompt["requestId"], serde_json::json!("req-p"));
        assert_eq!(prompt["id"], serde_json::json!("case-1"));
        assert_eq!(prompt["name"], serde_json::json!("案例名"));
        assert_eq!(prompt["type"], serde_json::json!("document"));
        assert_eq!(prompt["categoryId"], serde_json::json!(""));
        // cta_click 同样带载荷。
        assert_eq!(events[7]["id"], serde_json::json!("case-1"));
        assert_eq!(events[7]["position"], serde_json::json!(0));
        assert_eq!(events[7]["source"], serde_json::json!("discover"));
        assert_eq!(events[0]["messageId"], serde_json::json!("msg-pb"));
    }

    #[test]
    fn design_canvas_sequence_uses_request_id_tail() {
        let events = design_canvas_sequence("conv-c", "req-c");
        assert_eq!(events.len(), 8);
        let create = &events[6];
        assert_eq!(
            create["eventCode"],
            serde_json::json!("wbx_design_canvas_task_create")
        );
        assert_eq!(create["cost"], serde_json::json!(12000));
        assert_eq!(create["source"], serde_json::json!("summon_keyword"));
        assert_eq!(create["isSuccessful"], serde_json::json!(true));
        let open = &events[7];
        assert_eq!(
            open["eventCode"],
            serde_json::json!("wbx_design_canvas_open")
        );
        assert_eq!(open["cost"], serde_json::json!(13000));
        assert_eq!(open["type"], serde_json::json!("page"));
        assert_eq!(
            open["id"],
            serde_json::json!(format!("ardot-file-{}", tail("req-c", 8)))
        );
        assert_eq!(events[0]["messageId"], serde_json::json!("msg-canvas"));
    }

    #[test]
    fn skill_sequence_rewrites_finish_reason_to_tool_calls() {
        let events = skill_sequence("conv-s", "0123456789abcdef", "技能名", "skill-1", "1.2.3");
        assert_eq!(events.len(), 7);
        // ★ 判据：加载了技能 ⇒ finishReason 必须是 tool_calls（不是 stop）。
        let resp = &events[3];
        assert_eq!(resp["eventCode"], serde_json::json!("chat_message_response"));
        assert_eq!(resp["finishReason"], serde_json::json!("tool_calls"));
        // 其余事件不受影响。
        assert_eq!(events[5]["finishReason"], serde_json::json!("stop"));
        let info = &events[6];
        assert_eq!(info["eventCode"], serde_json::json!("skill_info"));
        assert_eq!(info["id"], serde_json::json!("技能名"));
        assert_eq!(info["skillId"], serde_json::json!("skill-1"));
        assert_eq!(info["skillVersion"], serde_json::json!("1.2.3"));
        assert_eq!(info["toolStatus"], serde_json::json!("success"));
        assert_eq!(info["fileCount"], serde_json::json!(56));
        assert_eq!(info["source"], serde_json::json!("workbuddy-desktop"));
        // messageId 用 requestID 末 8 位。
        assert_eq!(info["messageId"], serde_json::json!("msg-89abcdef"));
        assert_eq!(events[0]["messageId"], serde_json::json!("msg-89abcdef"));
    }

    #[test]
    fn appearance_apply_event_shape() {
        let ev = appearance_apply_event("skin-dark");
        assert_eq!(ev["eventCode"], serde_json::json!("appearance_skin_apply"));
        assert_eq!(ev["action"], serde_json::json!("apply"));
        assert_eq!(ev["source"], serde_json::json!("settings_close"));
        assert_eq!(ev["id"], serde_json::json!("skin-dark"));
        assert_eq!(ev["vipLevel"], serde_json::json!(0));
        assert_eq!(ev["series"], serde_json::json!(""));
        assert_eq!(ev["type"], serde_json::json!("unknown"));
        assert_eq!(ev.len(), 7);
    }

    // ── 专家 ──

    fn expert() -> MarketExpert {
        MarketExpert {
            expert_id: "ex_1".into(),
            expert_type: "agent".into(),
            display_name_zh: "财务助手".into(),
            profession_zh: "资深财务".into(),
            version: "2.0.0".into(),
            categories: vec![serde_json::json!("finance"), serde_json::json!("other")],
        }
    }

    #[test]
    fn expert_category_and_version_defaults() {
        assert_eq!(expert_category(&expert()), "finance");
        assert_eq!(expert_version(&expert()), "2.0.0");
        // 空分类 / 非字符串首元素 → expert-all。
        let mut e = expert();
        e.categories = vec![];
        assert_eq!(expert_category(&e), "expert-all");
        e.categories = vec![serde_json::json!(42)];
        assert_eq!(expert_category(&e), "expert-all");
        e.categories = vec![serde_json::json!("")];
        assert_eq!(expert_category(&e), "expert-all");
        // version 为空 → 1.0.0。
        let mut e2 = expert();
        e2.version = String::new();
        assert_eq!(expert_version(&e2), "1.0.0");
    }

    #[test]
    fn expert_summon_sequence_three_events() {
        let events = expert_summon_sequence(&expert());
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["eventCode"], serde_json::json!("web_element_click"));
        assert_eq!(events[0]["source"], serde_json::json!("ex_1"));
        assert_eq!(events[0]["type"], serde_json::json!("finance"));
        assert_eq!(events[0]["version"], serde_json::json!("2.0.0"));
        assert_eq!(
            events[0]["elementId"],
            serde_json::json!("expert_summon_click")
        );
        assert_eq!(events[1]["eventCode"], serde_json::json!("expert_summon_click"));
        assert_eq!(events[1]["type"], serde_json::json!("expert-all"));
        assert_eq!(events[1]["mode"], serde_json::json!("LOCAL"));
        assert_eq!(events[1]["position"], serde_json::json!(0));
        assert_eq!(events[2]["eventCode"], serde_json::json!("expert_summoned"));
        assert_eq!(events[2]["id"], serde_json::json!("ex_1"));
    }

    #[test]
    fn expert_actual_use_local_vs_craft() {
        let e = expert();
        // craft 口径：type = 分类，cost = 9000。
        let craft = expert_actual_use_event(&e, "conv-x", "0123456789abcdef", "craft");
        assert_eq!(craft["eventCode"], serde_json::json!("expert_actual_use"));
        assert_eq!(craft["type"], serde_json::json!("finance"));
        assert_eq!(craft["cost"], serde_json::json!(9000));
        assert_eq!(craft["mode"], serde_json::json!("craft"));
        assert_eq!(craft["source"], serde_json::json!("builtin"));
        assert_eq!(craft["characterCount"], serde_json::json!(14));
        assert_eq!(craft["messageId"], serde_json::json!("msg-89abcdef"));
        assert_eq!(craft["requestModelId"], serde_json::json!("fast-model"));
        // LOCAL 口径：type 为空串、cost = 0（对齐真实样本）。
        let local = expert_actual_use_event(&e, "conv-x", "0123456789abcdef", "LOCAL");
        assert_eq!(local["type"], serde_json::json!(""));
        assert_eq!(local["cost"], serde_json::json!(0));
        assert_eq!(local["mode"], serde_json::json!("LOCAL"));
        assert_eq!(local["id"], serde_json::json!("ex_1"));
    }

    // ── SSE id ──

    #[test]
    fn sse_id_shape_matches_go_regex() {
        assert!(is_sse_id("0123456789abcdef0123456789abcdef"));
        assert!(is_sse_id("cmb-0123456789abcdef0123456789abcdef"));
        // 长度不对 / 大写 / 非 hex / 空。
        assert!(!is_sse_id("0123456789abcdef0123456789abcde"));
        assert!(!is_sse_id("0123456789ABCDEF0123456789abcdef"));
        assert!(!is_sse_id("chatcmpl-0123456789abcdef0123456789abcdef"));
        assert!(!is_sse_id("cmb-0123456789abcdef0123456789abcde"));
        assert!(!is_sse_id(""));
    }

    #[test]
    fn extract_sse_id_takes_first_well_formed_id() {
        let line = r#"data: {"id":"chatcmpl-abc","x":1}"#;
        // 形状不符 → None（★ 自造 id 不能当真实 requestId）。
        assert_eq!(extract_sse_id(line), None);
        let good = r#"data: {"id":"cmb-0123456789abcdef0123456789abcdef"}"#;
        assert_eq!(
            extract_sse_id(good).as_deref(),
            Some("cmb-0123456789abcdef0123456789abcdef")
        );
        // 空 id → None（Go 的 `end <= 0`）。
        assert_eq!(extract_sse_id(r#"{"id":""}"#), None);
        // 没有 id 字段 → None。
        assert_eq!(extract_sse_id("data: [DONE]"), None);
    }

    // ── 夜猫窗口 ──

    #[test]
    fn night_window_boundaries_in_cst() {
        use chrono::TimeZone;
        let at = |h: u32, m: u32| {
            // 构造 UTC 时刻，使 CST(=UTC+8) 恰为 h:m。
            let utc_h = (h + 24 - 8) % 24;
            let day = if h < 8 { 1 } else { 2 };
            chrono::Utc
                .with_ymd_and_hms(2026, 3, day, utc_h, m, 0)
                .unwrap()
        };
        assert!(!in_night_window(at(22, 59)));
        assert!(in_night_window(at(23, 0)));
        assert!(in_night_window(at(23, 59)));
        assert!(in_night_window(at(0, 0)));
        assert!(in_night_window(at(7, 59)));
        assert!(!in_night_window(at(8, 0)));
        assert!(!in_night_window(at(12, 0)));
    }

    // ── 端到端（假上游） ──

    #[tokio::test]
    async fn report_chat_activity_posts_to_billing_domain_with_user_id() {
        let (base, seen) = spawn_fake_upstream(200, r#"{"code":0,"data":{}}"#);
        let client = client_for(&base);
        client
            .report_chat_activity(&account(), "wb2api-conv-1", "req-1")
            .await
            .unwrap();

        let got = seen.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].method, "POST");
        assert_eq!(got[0].path, DESKTOP_REPORT_PATH);
        // CLI 域头族：Go UA + billing 身份头（不是桌面 UA、不是 mp 头）。
        assert_eq!(got[0].header("user-agent"), Some("Go-http-client/1.1"));
        assert_eq!(got[0].header("authorization"), Some("Bearer at-test"));
        assert_eq!(got[0].header("x-user-id"), Some("uid-test"));
        assert_eq!(got[0].header("x-tenant-id"), Some("ent-test"));
        assert_eq!(
            got[0].header("x-domain"),
            Some("https://copilot.tencent.com")
        );
        assert!(!got[0].has_header("x-client-platform"));

        let body = got[0].json();
        let arr = body.as_array().expect("上报体是单元素数组");
        assert_eq!(arr.len(), 1);
        let ev = &arr[0];
        // ★ 缺 userId ⇒ 上游 200 但静默丢弃（不计分）。
        assert_eq!(ev["userId"], serde_json::json!("uid-test"));
        assert_eq!(ev["eventCode"], serde_json::json!("chat_request_send"));
        assert_eq!(ev["conversationId"], serde_json::json!("wb2api-conv-1"));
        assert_eq!(ev["requestId"], serde_json::json!("req-1"));
        assert_eq!(ev["requestModelId"], serde_json::json!("deepseek-v4-flash"));
        assert_eq!(ev["inputLength"], serde_json::json!(12));
        assert_eq!(ev["mode"], serde_json::json!("craft"));
        assert_eq!(ev.as_object().unwrap().len(), 36);
    }

    /// `chatRequestEvent` 的 36 个键**逐字**钉死：既锁集合，也锁线上字节序。
    ///
    /// 只断长度是不够的：把 `userId` 换成 `user_id` 再删掉另一个字段，长度仍是
    /// 36 而测试照样绿 —— 但上游会因为认不出字段而静默丢弃（缺 `userId` 的
    /// 失败形态就是 200 + 不计分）。
    ///
    /// ★ **键序也要锁，且必须从原始报文里读**（`wire_key_order`，不走 `serde_json`）：
    /// 本条测试最初就是先在解析后的 `Value` 上取 `.keys()`，结果只看到字典序、
    /// 断言必然失败 —— 而那次失败暴露的是**生产侧的真 bug**：`report_chat_activity_model`
    /// 曾写成 `serde_json::json!([ev])` 再交给 `billing_json`，中转一次就把 struct
    /// 声明序重排成字典序，线上字节与 Go 的 `json.Marshal([]chatRequestEvent{ev})`
    /// 不一致。实现已改为 `to_vec` 直序列化；本断言负责不让它退化回去。
    ///
    /// 序断言（严格强于集合断言：两边 36 个键全等且位置一一对应）即已覆盖集合，
    /// 故不再单独排一次序。
    #[tokio::test]
    async fn chat_request_event_has_all_36_go_keys_in_wire_order() {
        let (base, seen) = spawn_fake_upstream(200, r#"{"code":0,"data":{}}"#);
        let client = client_for(&base);
        client
            .report_chat_activity(&account(), "c", "r")
            .await
            .unwrap();

        let got = seen.lock().unwrap();
        // ★ 从 `body`（原始报文字符串）取序，不是 `got[0].json()`。
        let keys = wire_key_order(&got[0].body);
        assert_eq!(keys.len(), 36, "线上报文里应恰好有 36 个顶层键");

        let expected = [
            "eventCode",
            "timestamp",
            "reportDelay",
            "mode",
            "conversationId",
            "requestId",
            "inputLength",
            "requestModelId",
            "requestModelName",
            "isPlan",
            "isAutoExecuteTerminal",
            "isAutoModify",
            "codebaseEnable",
            "maxToken",
            "maxSteps",
            "temperature",
            "maxRetries",
            "mentionContexts",
            "knowledgeId",
            "knowledgeName",
            "codebaseId",
            "mentionContextCount",
            "command",
            "expertId",
            "recommendId",
            "skillId",
            "skillCount",
            "totalCount",
            "fileUri",
            "presentAt",
            "traceId",
            "rootRequestId",
            "parentConversationId",
            "agentName",
            "agentType",
            "userId",
        ];
        assert_eq!(expected.len(), 36, "Go report.go:36-73 就是 36 个 tag");
        assert_eq!(keys, expected, "线上键集合或键序与 Go 不一致");
        // 零值字段也必须出现（Go tag 全是裸 json:"..."，一个 omitempty 都没有）。
        // ★ 值检查可以走解析后的 `Value`：只关心「在不在、值是多少」，不关心序。
        let body = got[0].json();
        assert_eq!(body[0]["isPlan"], serde_json::json!(false));
        assert_eq!(body[0]["command"], serde_json::json!(""));
        assert_eq!(body[0]["mentionContexts"], serde_json::json!([]));
        assert_eq!(body[0]["knowledgeId"], serde_json::json!([]));
    }

    /// 证明 `wire_key_order` 有牙：它必须能分辨「struct 直序列化」与「经 `Value` 中转」。
    ///
    /// 上面那条键序断言的全部价值都押在这个 helper 真的在读**报文原序**上。如果它
    /// 悄悄退化成「解析后取 `.keys()`」，拿到的是 `BTreeMap` 的字典序，那么无论生产
    /// 侧写成 `to_vec` 还是 `json!([ev])`，断言都会绿 —— 守卫就变成了摆设。
    ///
    /// 这里在测试内部把同一条事件按两条路径各序列化一次（不碰生产代码），断言：
    /// 两条路径的**键集合相同**（所以只比集合是抓不到回归的），但 `wire_key_order`
    /// 读出的**序不同**，且分别等于 Go 声明序与字典序。
    #[test]
    fn wire_key_order_distinguishes_declaration_order_from_sorted_order() {
        let a = account();
        let ev = chat_request_event(&a, "c", "r", "m-id", "M Name");

        // 路径 A：直接序列化结构体 —— Go `json.Marshal([]chatRequestEvent{ev})` 的等价物。
        let direct = serde_json::to_string(&vec![ev.clone()]).unwrap();
        // 路径 B：经 `serde_json::Value` 中转 —— 曾经的写法，键序被重排成字典序。
        let via_value = serde_json::to_string(&serde_json::json!([ev])).unwrap();

        let direct_keys = wire_key_order(&direct);
        let via_value_keys = wire_key_order(&via_value);

        // 集合相同：只断集合的测试对这条回归是盲的。
        let mut s1 = direct_keys.clone();
        let mut s2 = via_value_keys.clone();
        s1.sort();
        s2.sort();
        assert_eq!(s1, s2, "两条路径的键集合应当相同");
        assert_eq!(s1.len(), 36);

        // 序不同，且各自符合预期。
        assert_ne!(
            direct_keys, via_value_keys,
            "若两条路径键序相同，说明 serde_json 开了 preserve_order，本守卫需重新设计"
        );
        assert_eq!(direct_keys[0], "eventCode", "直序列化 = Go 声明序");
        assert_eq!(direct_keys[35], "userId");
        let mut sorted_expected = s2.clone();
        sorted_expected.sort();
        assert_eq!(via_value_keys, sorted_expected, "中转 = 字典序");
        assert_eq!(via_value_keys[0], "agentName");
    }

    #[tokio::test]
    async fn report_chat_activity_falls_back_to_conversation_id() {
        let (base, seen) = spawn_fake_upstream(200, r#"{"code":0,"data":{}}"#);
        let client = client_for(&base);
        client
            .report_chat_activity(&account(), "conv-only", "")
            .await
            .unwrap();
        let got = seen.lock().unwrap();
        let body = got[0].json();
        assert_eq!(body[0]["requestId"], serde_json::json!("conv-only"));
    }

    #[tokio::test]
    async fn report_chat_activity_model_overrides_model_fields() {
        let (base, seen) = spawn_fake_upstream(200, r#"{"code":0,"data":{}}"#);
        let client = client_for(&base);
        client
            .report_chat_activity_model(&account(), "c", "r", "glm-5.2", "GLM 5.2")
            .await
            .unwrap();
        let got = seen.lock().unwrap();
        let body = got[0].json();
        assert_eq!(body[0]["requestModelId"], serde_json::json!("glm-5.2"));
        assert_eq!(body[0]["requestModelName"], serde_json::json!("GLM 5.2"));
    }

    #[tokio::test]
    async fn report_chat_activity_surfaces_business_error() {
        let (base, _seen) = spawn_fake_upstream(200, r#"{"code":14018,"msg":"boom"}"#);
        let client = client_for(&base);
        let err = client
            .report_chat_activity(&account(), "c", "r")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("code=14018"), "实际 {err}");
    }

    #[tokio::test]
    async fn report_desktop_events_merges_fingerprint_and_business_keys() {
        let (base, seen) = spawn_fake_upstream(200, r#"{"code":0,"data":{}}"#);
        let client = client_for(&base);
        let mut ev = DesktopEvent::new();
        ev.insert("eventCode".into(), serde_json::json!("automated_task_create_suc"));
        // 业务键覆盖同名指纹键（对齐真实设备）。
        ev.insert("ideVersion".into(), serde_json::json!("9.9.9"));
        client
            .report_desktop_events(&account(), &[ev])
            .await
            .unwrap();

        let got = seen.lock().unwrap();
        assert_eq!(got[0].path, DESKTOP_REPORT_PATH);
        // 桌面指纹头族：desktopUA + X-Domain + X-Product，且**不带** mp 头。
        assert_eq!(
            got[0].header("user-agent"),
            Some("WorkBuddy/5.5.6 WorkBuddy/5.5.6 CLI/2.137.1")
        );
        assert_eq!(got[0].header("x-product"), Some("SaaS"));
        assert_eq!(got[0].header("x-domain"), Some(base.as_str()));
        assert_eq!(
            got[0].header("content-type"),
            Some("application/json;charset=UTF-8")
        );
        assert!(!got[0].has_header("x-client-platform"));
        // X-Request-ID = deriveID(req) + 纳秒尾巴。
        let rid = got[0].header("x-request-id").unwrap();
        assert!(rid.starts_with(&derive_id(&account(), "req")), "实际 {rid}");
        assert!(rid.len() > 36);

        let body = got[0].json();
        let ev = &body[0];
        assert_eq!(ev["eventCode"], serde_json::json!("automated_task_create_suc"));
        assert_eq!(ev["userId"], serde_json::json!("uid-test"));
        assert_eq!(ev["extName"], serde_json::json!("workbuddy-desktop"));
        assert_eq!(ev["machineId"], serde_json::json!(derive_id(&account(), "machine")));
        // 业务键胜出。
        assert_eq!(ev["ideVersion"], serde_json::json!("9.9.9"));
        assert_eq!(ev.as_object().unwrap().len(), 23, "22 指纹 + 1 业务键");
    }

    #[tokio::test]
    async fn report_desktop_events_empty_short_circuits() {
        let (base, seen) = spawn_fake_upstream(200, r#"{"code":0,"data":{}}"#);
        let client = client_for(&base);
        client.report_desktop_events(&account(), &[]).await.unwrap();
        assert_eq!(
            seen.lock().unwrap().len(),
            0,
            "空事件列表必须不发请求（Go len(events)==0 → nil）"
        );
    }

    #[tokio::test]
    async fn set_appearance_theme_posts_kind_and_resource_key() {
        let (base, seen) = spawn_fake_upstream(200, r#"{"code":0,"data":{}}"#);
        let client = client_for(&base);
        client
            .set_appearance_theme(&account(), "skin-1")
            .await
            .unwrap();
        let got = seen.lock().unwrap();
        assert_eq!(got[0].path, APPEARANCE_SET_PATH);
        let body = got[0].json();
        assert_eq!(body["kind"], serde_json::json!("theme"));
        assert_eq!(body["resource_key"], serde_json::json!("skin-1"));
        assert_eq!(
            got[0].header("user-agent"),
            Some("WorkBuddy/5.5.6 WorkBuddy/5.5.6 CLI/2.137.1")
        );
        // ★ 与桌面事件上报不同：这里**没有** X-Domain。
        assert!(!got[0].has_header("x-domain"));
    }

    #[tokio::test]
    async fn report_web_event_uses_web_fingerprint() {
        let (base, seen) = spawn_fake_upstream(200, r#"{"code":0,"data":{}}"#);
        let client = client_for(&base);
        client
            .report_web_event(&account(), "web_click", "https://www.workbuddy.cn/home", "el-1", "按钮")
            .await
            .unwrap();
        let got = seen.lock().unwrap();
        assert_eq!(got[0].path, DESKTOP_REPORT_PATH);
        assert_eq!(got[0].header("x-client-platform"), Some("web"));
        assert_eq!(got[0].header("referer"), Some("https://www.workbuddy.cn/home"));
        assert_eq!(got[0].header("accept"), Some("application/json"));
        assert!(got[0].header("user-agent").unwrap().starts_with("Mozilla/5.0"));
        // ★ 浏览器形状：没有桌面那一族字段。
        let body = got[0].json();
        let ev = &body[0];
        assert_eq!(ev["eventCode"], serde_json::json!("web_click"));
        assert_eq!(ev["os"], serde_json::json!("Win32"));
        assert_eq!(ev["arch"], serde_json::json!(""));
        assert_eq!(ev["userAgent"], serde_json::json!(WEB_UA));
        assert_eq!(
            ev["machineId"],
            serde_json::json!(derive_id(&account(), "webmachine"))
        );
        assert_eq!(ev["userId"], serde_json::json!("uid-test"));
        assert_eq!(ev["enterpriseId"], serde_json::json!("ent-test"));
        assert!(!ev.as_object().unwrap().contains_key("ideName"));
        assert!(!ev.as_object().unwrap().contains_key("extName"));
        assert_eq!(ev.as_object().unwrap().len(), 14);
    }

    #[tokio::test]
    async fn mp_chat_activity_omits_activity_id_but_school_season_keeps_it() {
        // ★ 这一对断言是「两个任务集」的判据：带 activityId → 校园日；
        //   不带 → Sequential_Tasks_1/_3。字段「缺席」与「空串」在服务端是两种输入。
        let (base, seen) = spawn_fake_upstream(200, r#"{"code":0,"data":{}}"#);
        let client = client_for(&base);
        client
            .report_mp_chat_activity(&account(), "conv-mp")
            .await
            .unwrap();
        client
            .report_school_season_activity(&account(), "conv-school")
            .await
            .unwrap();

        let got = seen.lock().unwrap();
        assert_eq!(got.len(), 2);

        // ★ 线上字节序（见 `wire_key_order`）：这两条也走 billing 域的 struct 直序列化，
        //   若退化回 `json!([ev])` 中转，键序会被重排成字典序。
        assert_eq!(
            wire_key_order(&got[0].body),
            [
                "eventCode",
                "timestamp",
                "reportDelay",
                "source",
                "ideName",
                "ideType",
                "extName",
                "extVersion",
                "mode",
                "conversationId",
                "requestId",
                "inputLength",
                "mentionContexts",
                "mentionContextCount",
                "userId",
            ],
            "不带 activityId 时的线上键序（Go report.go:140-164 声明序）"
        );
        assert_eq!(
            wire_key_order(&got[1].body),
            [
                "eventCode",
                "timestamp",
                "reportDelay",
                "source",
                "ideName",
                "ideType",
                "extName",
                "extVersion",
                "mode",
                "conversationId",
                "requestId",
                "inputLength",
                "activityId",
                "mentionContexts",
                "mentionContextCount",
                "userId",
            ],
            "带 activityId 时它插在 inputLength 与 mentionContexts 之间（Go 声明序）"
        );

        let mp = got[0].json();
        let mp_ev = mp[0].as_object().unwrap();
        assert!(
            !mp_ev.contains_key("activityId"),
            "不带 activityId 时该字段必须整体消失，实际 {:?}",
            mp_ev.get("activityId")
        );
        assert_eq!(mp_ev.len(), 15, "16 字段减掉 omitempty 的 activityId");
        assert_eq!(mp_ev["source"], serde_json::json!("mini_program"));
        assert_eq!(mp_ev["ideName"], serde_json::json!("wx_app_cloud"));
        assert_eq!(mp_ev["ideType"], serde_json::json!("WorkBuddy_MP"));
        assert_eq!(mp_ev["extName"], serde_json::json!("workbuddy-mp"));
        assert_eq!(mp_ev["extVersion"], serde_json::json!("SaaS"));
        assert_eq!(mp_ev["mode"], serde_json::json!("chat"));
        assert_eq!(mp_ev["userId"], serde_json::json!("uid-test"));
        assert_eq!(mp_ev["conversationId"], mp_ev["requestId"]);

        let school = got[1].json();
        let sc_ev = school[0].as_object().unwrap();
        assert_eq!(
            sc_ev["activityId"],
            serde_json::json!(SCHOOL_SEASON_ACTIVITY_ID)
        );
        assert_eq!(sc_ev.len(), 16);
    }

    #[tokio::test]
    async fn mp_chat_activity_generates_conversation_id_when_empty() {
        let (base, seen) = spawn_fake_upstream(200, r#"{"code":0,"data":{}}"#);
        let client = client_for(&base);
        client
            .report_mp_chat_activity(&account(), "")
            .await
            .unwrap();
        let got = seen.lock().unwrap();
        let body = got[0].json();
        let cid = body[0]["conversationId"].as_str().unwrap();
        assert!(cid.starts_with("wb-run-"), "实际 {cid}");
    }

    #[tokio::test]
    async fn report_mp_expert_use_shape_and_defaults() {
        let (base, seen) = spawn_fake_upstream(200, r#"{"code":0,"data":{}}"#);
        let client = client_for(&base);
        // expertName / expertType 都留空 → 回落 expertID / "agent"。
        client
            .report_mp_expert_use(&account(), "ex_9", "", "")
            .await
            .unwrap();
        let got = seen.lock().unwrap();
        // ★ 线上字节序（见 `wire_key_order`）：同一条 struct 直序列化路径。
        assert_eq!(
            wire_key_order(&got[0].body),
            [
                "eventCode",
                "timestamp",
                "reportDelay",
                "source",
                "ideName",
                "ideType",
                "extName",
                "extVersion",
                "id",
                "name",
                "expertTitle",
                "type",
                "characterCount",
                "expertType",
                "userId",
            ],
            "线上键序（Go report.go:261-277 声明序）"
        );
        let body = got[0].json();
        let ev = &body[0];
        assert_eq!(ev["eventCode"], serde_json::json!("expert_actual_use"));
        assert_eq!(ev["source"], serde_json::json!("mini_program"));
        assert_eq!(ev["extName"], serde_json::json!("workbuddy-mp"));
        // ★ 刻意是 2.2.8（不是对话事件的 SaaS）——两个事件来自不同 SDK 版本。
        assert_eq!(ev["extVersion"], serde_json::json!("2.2.8"));
        assert_eq!(ev["id"], serde_json::json!("ex_9"));
        // ★ Go 侧 Name 填的是 expertID，人类可读名走 ExpertTitle。
        assert_eq!(ev["name"], serde_json::json!("ex_9"));
        assert_eq!(ev["expertTitle"], serde_json::json!("ex_9"));
        assert_eq!(ev["type"], serde_json::json!("send_message"));
        assert_eq!(ev["characterCount"], serde_json::json!(12));
        assert_eq!(ev["expertType"], serde_json::json!("agent"));
        assert_eq!(ev["userId"], serde_json::json!("uid-test"));
        // 这套口径**没有** conversationId / activityId。
        assert!(!ev.as_object().unwrap().contains_key("conversationId"));
        assert!(!ev.as_object().unwrap().contains_key("activityId"));
        assert_eq!(ev.as_object().unwrap().len(), 15);
    }

    #[tokio::test]
    async fn market_expert_list_sends_filters_and_parses() {
        let (base, seen) = spawn_fake_upstream(
            200,
            r#"{"code":0,"data":{"experts":[
                {"expert_id":"ex_1","expert_type":"agent","display_name_zh":"财务助手",
                 "profession_zh":"资深财务","version":"2.0.0","categories":["finance"]},
                {"expert_id":"ex_2"}
            ]}}"#,
        );
        let client = client_for(&base);
        let out = client.market_expert_list(&account(), "agent").await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].expert_id, "ex_1");
        assert_eq!(out[0].display_name_zh, "财务助手");
        assert_eq!(expert_category(&out[0]), "finance");
        assert_eq!(expert_version(&out[0]), "2.0.0");
        // 缺字段的条目按默认值补（version → 1.0.0、category → expert-all）。
        assert_eq!(out[1].expert_id, "ex_2");
        assert_eq!(expert_category(&out[1]), "expert-all");
        assert_eq!(expert_version(&out[1]), "1.0.0");

        let got = seen.lock().unwrap();
        assert_eq!(got[0].path, MARKET_EXPERT_LIST_PATH);
        assert_eq!(
            got[0].header("user-agent"),
            Some("WorkBuddy/5.5.6 WorkBuddy/5.5.6 CLI/2.137.1")
        );
        let body = got[0].json();
        assert_eq!(body["page"], serde_json::json!(1));
        assert_eq!(body["page_size"], serde_json::json!(20));
        assert_eq!(body["sort_by"], serde_json::json!("reco_rank"));
        assert_eq!(body["sort_order"], serde_json::json!("desc"));
        assert_eq!(body["expert_type"], serde_json::json!("agent"));
    }

    #[tokio::test]
    async fn market_expert_list_omits_empty_expert_type() {
        let (base, seen) = spawn_fake_upstream(200, r#"{"code":0,"data":{"experts":[]}}"#);
        let client = client_for(&base);
        assert!(client
            .market_expert_list(&account(), "")
            .await
            .unwrap()
            .is_empty());
        let got = seen.lock().unwrap();
        let body = got[0].json();
        assert!(
            body.as_object().unwrap().get("expert_type").is_none(),
            "expert_type 为空时不该出现"
        );
    }

    #[tokio::test]
    async fn market_expert_list_parse_failure_is_upstream_parse() {
        let (base, _seen) = spawn_fake_upstream(200, r#"{"code":0,"data":{"experts":"nope"}}"#);
        let client = client_for(&base);
        let err = client.market_expert_list(&account(), "").await.unwrap_err();
        assert!(
            matches!(err, GatewayError::UpstreamParse(_)),
            "解析失败应为 UpstreamParse，实际 {err:?}"
        );
        assert!(err.to_string().contains("expert list parse"), "实际 {err}");
    }

    #[tokio::test]
    async fn chat_with_model_reads_server_assigned_request_id() {
        let (base, seen) = spawn_fake_upstream(
            200,
            "data: {\"id\":\"chatcmpl-client-made\",\"choices\":[]}\n\n\
             data: {\"id\":\"0123456789abcdef0123456789abcdef\",\"choices\":[]}\n\n\
             data: [DONE]\n\n",
        );
        let client = client_for(&base);
        let (cid, rid) = client
            .chat_with_model(&account(), "fast-model", "你好", "ex_1")
            .await
            .unwrap();
        assert!(cid.starts_with("wb2api-conv-"), "实际 {cid}");
        // ★ 取的是**形状合法**的那个 id，不是第一个 id 字段。
        assert_eq!(rid, "0123456789abcdef0123456789abcdef");

        let got = seen.lock().unwrap();
        assert_eq!(got[0].path, CHAT_COMPLETIONS_PATH);
        assert_eq!(got[0].header("accept"), Some("text/event-stream"));
        assert_eq!(got[0].header("x-conversation-id"), Some(cid.as_str()));
        assert_eq!(got[0].header("x-expert-id"), Some("ex_1"));
        assert_eq!(got[0].header("x-ide-version"), Some("5.5.6"));
        assert_eq!(got[0].header("x-codebuddy-request"), Some("1"));
        assert_eq!(got[0].header("x-agent-intent"), Some("craft"));
        let body = got[0].json();
        assert_eq!(body["model"], serde_json::json!("fast-model"));
        assert_eq!(body["stream"], serde_json::json!(true));
        assert_eq!(body["agent"], serde_json::json!("cli"));
        assert_eq!(body["temperature"], serde_json::json!(1));
        assert_eq!(body["stream_options"]["include_usage"], serde_json::json!(true));
        assert_eq!(body["messages"][1]["content"], serde_json::json!("你好"));
    }

    #[tokio::test]
    async fn chat_with_model_errors_when_no_valid_sse_id() {
        let (base, _seen) =
            spawn_fake_upstream(200, "data: {\"id\":\"chatcmpl-only\"}\n\ndata: [DONE]\n\n");
        let client = client_for(&base);
        let err = client
            .chat_with_model(&account(), "fast-model", "hi", "")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("未找到服务端 requestId"),
            "实际 {err}"
        );
    }

    #[tokio::test]
    async fn chat_with_model_reports_non_200() {
        let (base, _seen) = spawn_fake_upstream(500, r#"{"code":500,"msg":"boom"}"#);
        let client = client_for(&base);
        let err = client
            .chat_with_model(&account(), "fast-model", "hi", "")
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "chat http 500");
    }
}
