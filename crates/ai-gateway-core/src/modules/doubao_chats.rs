//! 豆包对话：客户端状态备份/恢复 + 官方 IM API 对话导出。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/tasks/doubao_chats.rs` 与
//! `commands/doubao.rs` 的 D1/D2 部分。
//!
//! ## 三件不同的事
//!
//! | 功能 | 数据 | 说明 |
//! |---|---|---|
//! | 客户端状态备份 | `IndexedDB` / `DoubaoStorage` | 会话列表缓存、技能配置 |
//! | 对话导出 | 官方 IM API | 真正的对话正文（markdown + json） |
//!
//! **重要认知**：对话正文存在豆包的**云端**（按账号归属），本地备份的是客户端状态。
//! 恢复客户端状态并重新登录后，完整历史会从云端重新同步下来 ——
//! 因此「备份对话」不会丢历史，但也不能用它把对话搬到另一个账号。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::modules::app_profile::{profile_for, TargetApp};
use crate::modules::config;
use crate::modules::doubao_account;
use crate::modules::switcher::copy;

/// IM API 网关 UA —— **缺客户端标识会被拒**（`SamanthaDoubao/2.27.12` 是必需的）。
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 SamanthaDoubao/2.27.12";

/// 会话列表查询串。
///
/// 设备指纹参数（`web_id` / `tea_uuid` / `fp`）**缺一个就会被网关判 `712010702`**
/// —— 实测对照确认，不是可选参数。
const RECENT_QS: &str = "version_code=20800&language=zh&device_platform=web&doubao_device_platform=desktop\
&aid=582478&real_aid=582478&pkg_type=release_version&device_id=199439841787403\
&pc_version=2.27.12&doubao_pc_version=2.27.12&region=CN&sys_region=CN&samantha_web=1\
&web_platform=desktop&use-olympus-account=1&runtime=web&runtime_version=3.35.4\
&client_platform=pc_client&chromium_version=147.0.7727.149&channel=win\
&web_id=7681679574816589346&tea_uuid=199439841787403&fp=verify_199439841787403\
&web_tab_id=4db30b53-185f-4e73-99aa-6d29921381fc";

/// 单会话消息查询串（与列表相比只换 `aid` / `web_platform`）。
const SINGLE_QS: &str = "version_code=20800&language=zh&device_platform=web&doubao_device_platform=web\
&aid=497858&real_aid=497858&pkg_type=release_version&device_id=199439841787403\
&pc_version=2.27.12&doubao_pc_version=2.27.12&region=CN&sys_region=CN&samantha_web=1\
&web_platform=web&use-olympus-account=1&runtime=web&runtime_version=3.35.4\
&client_platform=pc_client&chromium_version=147.0.7727.149&channel=win\
&web_id=7681679574816589346&tea_uuid=199439841787403&fp=verify_199439841787403\
&web_tab_id=4db30b53-185f-4e73-99aa-6d29921381fc";

/// 豆包数据目录。
fn user_data_dir() -> PathBuf {
    profile_for(TargetApp::Doubao, &config::store_dir()).data_dir
}

/// 对话备份根目录（`<store_dir>/doubao_chats/<uid>/`）。
fn chat_backup_root(uid: &str) -> PathBuf {
    config::store_dir().join("doubao_chats").join(uid)
}

// ---------------------------------------------------------------------------
// 客户端状态备份 / 恢复
// ---------------------------------------------------------------------------

/// 收集需要备份的客户端状态目录：`<profile>/<IndexedDB 子目录>` 与 `<profile>/DoubaoStorage`。
///
/// `IndexedDB` 下只取豆包自己的库（`chrome_doubao-` / `https_www.doubao.com`）——
/// 全量拷贝会把飞书 iframe、浏览器扩展的库一起搬走，体积翻好几倍且毫无用处。
fn chat_source_dirs(user_data: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for profile in crate::modules::switcher::chromium::profile_dirs(user_data) {
        let name = profile
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let idb = profile.join("IndexedDB");
        if let Ok(entries) = std::fs::read_dir(&idb) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let dir_name = entry.file_name().to_string_lossy().to_string();
                if dir_name.starts_with("chrome_doubao-")
                    || dir_name.starts_with("https_www.doubao.com")
                {
                    out.push((format!("{name}/IndexedDB/{dir_name}"), path));
                }
            }
        }
        let storage = profile.join("DoubaoStorage");
        if storage.is_dir() {
            out.push((format!("{name}/DoubaoStorage"), storage));
        }
    }
    out
}

/// 备份指定账号的客户端对话状态。
pub fn backup_chatdata(uid: &str) -> Result<Value, String> {
    doubao_account::ensure_uid_safe(uid)?;
    let user_data = user_data_dir();
    if !user_data.is_dir() {
        return Err("未找到豆包数据目录，请确认已安装并至少启动过一次豆包".to_string());
    }
    let sources = chat_source_dirs(&user_data);
    if sources.is_empty() {
        return Err("未发现可备份的对话数据（豆包可能从未产生过对话）".to_string());
    }

    // 备份前必须关闭客户端：IndexedDB 的 leveldb 在运行中被独占，
    // 强行读取会得到不一致的中间状态。
    close_doubao()?;

    let root = chat_backup_root(uid);
    // 整体覆盖：先删干净，避免上一次备份的残留文件混进新备份
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).map_err(|e| format!("创建备份目录失败: {e}"))?;

    let mut files = 0usize;
    for (rel, src) in &sources {
        let dest = root.join(rel.replace('/', "\\"));
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if copy_dir_counted(src, &dest).is_ok() {
            files += count_files(&dest);
        }
    }

    let meta = json!({
        "schemaVersion": 1,
        "userId": uid,
        "files": files,
        "backedAt": config::utc_iso(),
    });
    let _ = std::fs::write(
        root.join("chat_backup_meta.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    );

    Ok(json!({
        "ok": true,
        "userId": uid,
        "files": files,
        "path": root.to_string_lossy(),
    }))
}

/// 恢复指定账号的客户端对话状态。
pub fn restore_chatdata(uid: &str) -> Result<Value, String> {
    doubao_account::ensure_uid_safe(uid)?;
    let root = chat_backup_root(uid);
    if !root.is_dir() {
        return Err(format!("账号 {uid} 没有对话备份"));
    }
    let user_data = user_data_dir();
    close_doubao()?;

    let mut restored = 0usize;
    for entry in std::fs::read_dir(&root).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue; // 跳过 chat_backup_meta.json
        }
        let dest = user_data.join(entry.file_name());
        if copy_dir_counted(&path, &dest).is_ok() {
            restored += 1;
        }
    }

    Ok(json!({
        "ok": true,
        "userId": uid,
        "profiles": restored,
    }))
}

/// 备份信息（界面展示「是否已备份 / 大小 / 时间」）。
pub fn chatdata_info(uid: &str) -> Value {
    let root = chat_backup_root(uid);
    if !root.is_dir() {
        return json!({"backed": false});
    }
    let meta = std::fs::read_to_string(root.join("chat_backup_meta.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| json!({}));
    json!({
        "backed": true,
        "files": meta.get("files").cloned().unwrap_or(json!(0)),
        "backedAt": meta.get("backedAt").cloned(),
        "sizeBytes": dir_size(&root),
        "path": root.to_string_lossy(),
    })
}

/// 递归拷贝并返回拷贝的文件数。
fn copy_dir_counted(src: &Path, dest: &Path) -> std::io::Result<usize> {
    copy::copy_dir_contents(src, dest)?;
    Ok(count_files(dest))
}

/// 统计目录下的文件数。
fn count_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let path = e.path();
            if path.is_dir() {
                count_files(&path)
            } else {
                1
            }
        })
        .sum()
}

/// 统计目录总字节数。
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let path = e.path();
            if path.is_dir() {
                dir_size(&path)
            } else {
                e.metadata().map(|m| m.len()).unwrap_or(0)
            }
        })
        .sum()
}

/// 优雅关闭豆包客户端。
fn close_doubao() -> Result<(), String> {
    let prof = profile_for(TargetApp::Doubao, &config::store_dir());
    let store = config::store_dir();
    let args = crate::modules::switcher::RunArgs {
        action: crate::modules::switcher::Action::BackupCurrent,
        target_app: TargetApp::Doubao,
        user_id: None,
        proxy_port: None,
        include_indexeddb: false,
        expected_current_uid: String::new(),
        store_dir: store,
    };
    let sess = crate::modules::switcher::Session::new(&args);
    // Session 已按 target_app 构造好档案，直接用即可
    let _ = prof;
    crate::modules::switcher::proc::stop_app(&sess, &crate::modules::switcher::NullSink)
}

// ---------------------------------------------------------------------------
// 对话导出（官方 IM API）
// ---------------------------------------------------------------------------

/// 生成 UUID v4（用 SHA-256 拼随机源，避免为单个字段引入依赖）。
fn uuid_v4() -> String {
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(n.to_le_bytes());
    let digest = hasher.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant RFC 4122
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// 豆包 IM 网关要求 `ttwid` / `sid_guard` 以 **URL 编码**形式出现（`|` → `%7C`）；
/// 传原始 `|` / `,` / `:` 会报 `712010702`。
///
/// 账号池统一存原始（可读）形式，发送前编码；已含 `%` 的值视为已编码原样透传，
/// 避免双重编码。
fn cookie_enc(value: &str) -> String {
    if value.contains('%') {
        value.to_string()
    } else {
        urlencoding::encode(value).into_owned()
    }
}

/// 组装对话 API 的 Cookie。
fn build_cookie(acc: &Value) -> Result<String, String> {
    let uid = acc.get("user_id").and_then(Value::as_str).unwrap_or("");
    let sid = acc
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if sid.is_empty() {
        return Err(format!(
            "账号 {uid} 未录入 sessionid（请在编辑账号中填写，或开启本地代理自动抓取）"
        ));
    }
    let mut parts = vec![
        format!("sessionid={sid}"),
        format!("sessionid_ss={sid}"),
        format!("sid_tt={sid}"),
    ];
    if let Some(guard) = acc
        .get("sid_guard")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("sid_guard={}", cookie_enc(guard)));
    }
    match acc
        .get("ttwid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(ttwid) => parts.push(format!("ttwid={}", cookie_enc(ttwid))),
        // 缺 ttwid 只警告不失败：部分接口仍可工作，直接报错会让用户以为完全不可用
        None => eprintln!(
            "[doubao] 账号 {uid} 无 ttwid，对话 API 可能拒绝（登录校验不合法）；\
             开启本地代理后访问豆包可自动抓取"
        ),
    }
    Ok(parts.join("; "))
}

/// 直连 HTTP 客户端（不走系统代理：本地 MITM 未启动时是死端口）。
fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .no_proxy()
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败: {e}"))
}

/// 调一次 IM API。
async fn api_post(url: &str, cookie: &str, body: &Value) -> Result<Value, String> {
    let resp = client()?
        .post(url)
        .header("content-type", "application/json; encoding=utf-8")
        .header("cookie", cookie)
        .header("agw-js-conv", "str")
        .header("user-agent", UA)
        .header("accept", "application/json, text/plain, */*")
        .header("referer", "https://www.doubao.com/")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| format!("API 请求失败: {e}"))?;
    let text = resp.text().await.map_err(|e| format!("响应读取失败: {e}"))?;
    serde_json::from_str(&text).map_err(|_| {
        format!(
            "响应不是 JSON: {}",
            text.chars().take(200).collect::<String>()
        )
    })
}

/// 取响应的 `(code, description)`。
fn status_of(body: &Value) -> (i64, String) {
    let code = body
        .get("status_code")
        .or_else(|| body.get("code"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let desc = body
        .get("status_desc")
        .or_else(|| body.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (code, desc)
}

/// 拉取会话列表。
///
/// `conv_version` 首次请求必须是 **int 0**（传字符串会触发 `712010702`，实测确认）；
/// 翻页失败降级为警告：豆包客户端自身从不翻页，非首跳可能不被网关支持。
async fn fetch_recent_convs(cookie: &str, limit: usize) -> Result<Vec<Value>, String> {
    let mut convs: Vec<Value> = Vec::new();
    // int 0 起步；翻页后换成服务端给的 next_conv_version（字符串）
    let mut cursor = json!(0i64);
    for _ in 0..10 {
        let body = json!({
            "cmd": 3200,
            "uplink_body": {"pull_recent_conv_chain_uplink_body": {
                "limit": limit.min(50), "message_count_per_conv": 0, "api_version": 1,
                "conv_version": cursor, "direction": 3,
                "option": {"not_need_message": true, "need_complete_conversation": true,
                            "need_coco_bot": true, "need_pc_pin_chain": true, "pc_pin_query_type": 0,
                            "exclude_archive": true, "only_archive": false}}},
            "sequence_id": uuid_v4(), "channel": 2, "version": "1",
        });
        let url = format!("https://www.doubao.com/im/chain/recent_conv?{RECENT_QS}");
        let response = api_post(&url, cookie, &body).await?;
        let (code, desc) = status_of(&response);
        if code != 0 {
            if cursor.as_i64() == Some(0) {
                return Err(format!("会话列表请求失败: {code} {desc}"));
            }
            eprintln!(
                "[doubao] 会话列表翻页失败（已拉 {} 个）: {code} {desc}",
                convs.len()
            );
            break;
        }
        let chain = &response["downlink_body"]["pull_recent_conv_chain_downlink_body"];
        for cell in chain
            .get("cells")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let conv = &cell["conversation"];
            if conv.get("conversation_id").and_then(Value::as_str).is_some() {
                convs.push(conv.clone());
            }
        }
        if !chain
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            break;
        }
        let next = chain
            .get("next_conv_version")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if next.is_empty() || Value::String(next.clone()) == cursor {
            break;
        }
        cursor = json!(next);
    }
    Ok(convs)
}

/// 拉取单个会话的消息（从最新向前翻页）。
async fn fetch_messages(cookie: &str, conv_id: &str, max_pages: usize) -> Result<Vec<Value>, String> {
    let mut msgs: Vec<Value> = Vec::new();
    // 2^53-1 = 从最新开始
    let mut anchor: i64 = 9_007_199_254_740_991;
    for _ in 0..max_pages {
        let body = json!({
            "cmd": 3100,
            "uplink_body": {"pull_singe_chain_uplink_body": {
                "conversation_id": conv_id, "anchor_index": anchor, "conversation_type": 3,
                "direction": 1, "limit": 20, "ext": {}, "filter": {"index_list": []},
                "evaluate_ab_params": "", "evaluate_common_params": ""}},
            "sequence_id": uuid_v4(), "channel": 2, "version": "1",
        });
        let url = format!("https://www.doubao.com/im/chain/single?{SINGLE_QS}");
        let response = api_post(&url, cookie, &body).await?;
        let (code, desc) = status_of(&response);
        if code != 0 {
            return Err(format!("会话消息请求失败: {code} {desc}"));
        }
        let page = response["downlink_body"]["pull_singe_chain_downlink_body"]["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if page.is_empty() {
            break;
        }
        let page_len = page.len();
        let mut combined = page; // 页内新→旧，向前拼接
        combined.append(&mut msgs);
        msgs = combined;

        // index_in_conv 是字符串形式的大整数序列号；任一解析失败即终止翻页
        // （上游 python 版同款语义：宁可少拉，也不要因错误序号重复拉取）
        let mut oldest: Option<i64> = Some(i64::MAX);
        for m in &msgs[..page_len] {
            let value = match m.get("index_in_conv") {
                None | Some(Value::Null) => 0,
                Some(Value::String(s)) if s.is_empty() => 0,
                Some(Value::Number(num)) => num.as_i64().unwrap_or(0),
                Some(Value::String(s)) => match s.parse::<i64>() {
                    Ok(v) => v,
                    Err(_) => {
                        oldest = None;
                        break;
                    }
                },
                _ => 0,
            };
            oldest = Some(oldest.unwrap_or(i64::MAX).min(value));
        }
        let Some(oldest) = oldest else { break };
        if oldest <= 0 {
            break; // 已到会话开头
        }
        anchor = oldest;
    }
    Ok(msgs)
}

/// 从消息记录提取正文：`content_block` 的 text 优先，`brief` / `tts_content` 兜底。
fn message_text(m: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(blocks) = m.get("content_block").and_then(Value::as_array) {
        for block in blocks {
            if let Some(text) = block
                .get("content")
                .and_then(|c| c.get("text_block"))
                .and_then(|t| t.get("text"))
                .and_then(Value::as_str)
            {
                if !text.trim().is_empty() {
                    parts.push(text.trim().to_string());
                }
            }
        }
    }
    if parts.is_empty() {
        for key in ["brief", "tts_content", "content"] {
            if let Some(text) = m.get(key).and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    parts.push(text.trim().to_string());
                    break;
                }
            }
        }
    }
    parts.join("\n\n")
}

/// 时间戳 → 本地可读。
fn fmt_local_ts(value: &Value) -> String {
    let secs = value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()));
    match secs.filter(|s| *s != 0) {
        Some(s) => chrono::DateTime::from_timestamp(s, 0)
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default(),
        None => String::new(),
    }
}

/// 渲染为 Markdown。
fn to_markdown(payload: &Value) -> String {
    let uid = payload.get("userId").and_then(Value::as_str).unwrap_or("");
    let exported_at = payload
        .get("exportedAt")
        .and_then(Value::as_str)
        .unwrap_or("");
    let convs = payload
        .get("conversations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut out = String::new();
    out.push_str(&format!("# 豆包对话导出（账号 {uid}）\n\n"));
    out.push_str(&format!(
        "*导出时间：{exported_at} · 共 {} 个会话*\n\n---\n\n",
        convs.len()
    ));

    for conv in &convs {
        let name = conv
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("（未命名会话）");
        out.push_str(&format!("## {name}\n\n"));
        // 键名必须与 export_account 写入的 payload 一致（camelCase）——
        // 这里曾误用上游原始响应的 snake_case 键，导致「更新时间 · N 条消息」整行
        // 静默消失（updated 恒为空串）。
        let updated = fmt_local_ts(conv.get("updateTime").unwrap_or(&Value::Null));
        let msgs = conv
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !updated.is_empty() {
            out.push_str(&format!("*更新：{updated} · {} 条消息*\n\n", msgs.len()));
        }
        for m in &msgs {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("");
            let who = match role {
                "user" => "🧑 用户",
                "assistant" => "🤖 豆包",
                other => other,
            };
            let time = fmt_local_ts(m.get("create_time").unwrap_or(&Value::Null));
            let time_part = time
                .split(' ')
                .nth(1)
                .map(|t| format!(" `{t}`"))
                .unwrap_or_default();
            let text = message_text(m);
            if text.is_empty() {
                continue;
            }
            out.push_str(&format!("**{who}**{time_part}\n\n{text}\n\n"));
        }
        out.push_str("---\n\n");
    }
    out
}

/// 导出指定账号的对话。
pub async fn export_account(
    uid: &str,
    limit_convs: usize,
    max_pages: usize,
) -> Result<Value, String> {
    doubao_account::ensure_uid_safe(uid)?;
    let acc = doubao_account::find_account(uid).ok_or_else(|| "账号不存在".to_string())?;
    let cookie = build_cookie(&acc)?;

    let convs = fetch_recent_convs(&cookie, limit_convs).await?;
        let mut exported: Vec<Value> = Vec::new();
        let mut total_messages = 0usize;

        for conv in convs.iter().take(limit_convs) {
            let Some(conv_id) = conv.get("conversation_id").and_then(Value::as_str) else {
                continue;
            };
            match fetch_messages(&cookie, conv_id, max_pages).await {
                Ok(messages) => {
                    total_messages += messages.len();
                    exported.push(json!({
                        "conversationId": conv_id,
                        "name": conv.get("name"),
                        // 统一转成 camelCase，渲染侧只认这一套键名
                        "updateTime": conv.get("update_time"),
                        "messages": messages,
                    }));
                }
                Err(e) => {
                    // 单个会话失败不中断整体导出：用户要的是「尽可能多的对话」
                    eprintln!("[doubao] 会话 {conv_id} 导出失败: {e}");
                    exported.push(json!({
                        "conversationId": conv_id,
                        "name": conv.get("name"),
                        "error": e,
                    }));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }

    let payload = json!({
        "userId": uid,
        "exportedAt": config::utc_iso(),
        "conversations": exported,
    });

    // 产物落到 exports/ 目录
    let export_dir = config::store_dir().join("exports");
    std::fs::create_dir_all(&export_dir).map_err(|e| format!("创建导出目录失败: {e}"))?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let json_path = export_dir.join(format!("doubao_chats_{uid}_{stamp}.json"));
    let md_path = export_dir.join(format!("doubao_chats_{uid}_{stamp}.md"));

    std::fs::write(
        &json_path,
        serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写入 JSON 失败: {e}"))?;
    std::fs::write(&md_path, to_markdown(&payload)).map_err(|e| format!("写入 Markdown 失败: {e}"))?;

    Ok(json!({
        "ok": true,
        "conversations": exported.len(),
        "messages": total_messages,
        "jsonPath": json_path.to_string_lossy(),
        "mdPath": md_path.to_string_lossy(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_编码避免双重编码() {
        // 原始值需要编码
        assert_eq!(cookie_enc("a|b"), "a%7Cb");
        // 已编码的值原样透传（含 % 即视为已编码）
        assert_eq!(cookie_enc("a%7Cb"), "a%7Cb");
        assert_eq!(cookie_enc("x%2Cy"), "x%2Cy");
    }

    #[test]
    fn 组装_cookie_含必需字段并编码_sid_guard() {
        let acc = json!({
            "user_id": "123",
            "session_id": "sid-abc",
            "sid_guard": "g|1|2",
            "ttwid": "t|9",
        });
        let cookie = build_cookie(&acc).unwrap();
        assert!(cookie.contains("sessionid=sid-abc"));
        assert!(cookie.contains("sessionid_ss=sid-abc"));
        assert!(cookie.contains("sid_tt=sid-abc"));
        assert!(
            cookie.contains("sid_guard=g%7C1%7C2"),
            "sid_guard 必须 URL 编码，否则网关报 712010702: {cookie}"
        );
        assert!(cookie.contains("ttwid=t%7C9"));
    }

    #[test]
    fn 无_sessionid_时明确报错并指引() {
        let err = build_cookie(&json!({"user_id": "123"})).unwrap_err();
        assert!(err.contains("未录入 sessionid"));
        assert!(err.contains("123"));
    }

    #[test]
    fn 缺_ttwid_仍可组装_cookie() {
        let acc = json!({"user_id": "1", "session_id": "s"});
        let cookie = build_cookie(&acc).expect("缺 ttwid 不应直接失败");
        assert!(cookie.contains("sessionid=s"));
        assert!(!cookie.contains("ttwid"));
    }

    #[test]
    fn 提取消息正文的优先级() {
        // content_block 优先
        let with_blocks = json!({
            "content_block": [
                {"content": {"text_block": {"text": "第一段"}}},
                {"content": {"text_block": {"text": "第二段"}}}
            ],
            "brief": "不该被用到",
        });
        assert_eq!(message_text(&with_blocks), "第一段\n\n第二段");

        // 无 content_block → brief
        assert_eq!(message_text(&json!({"brief": "摘要"})), "摘要");
        // 无 brief → tts_content
        assert_eq!(message_text(&json!({"tts_content": "语音文本"})), "语音文本");
        // 都没有 → 空串
        assert_eq!(message_text(&json!({})), "");
        // 空白内容应被忽略
        assert_eq!(
            message_text(&json!({"content_block": [{"content": {"text_block": {"text": "   "}}}]})),
            ""
        );
    }

    #[test]
    fn uuid_v4_形态且不重复() {
        let a = uuid_v4();
        let b = uuid_v4();
        assert_ne!(a, b, "同一进程内连续生成必须不同");
        assert_eq!(a.len(), 36);
        assert_eq!(a.chars().filter(|c| *c == '-').count(), 4);
        let parts: Vec<&str> = a.split('-').collect();
        assert!(parts[2].starts_with('4'), "version 位: {}", parts[2]);
        assert!(matches!(parts[3].chars().next(), Some('8') | Some('9') | Some('a') | Some('b')));
    }

    #[test]
    fn 状态码解析() {
        assert_eq!(status_of(&json!({"status_code": 0})).0, 0);
        assert_eq!(status_of(&json!({"code": 712010702})).0, 712010702);
        assert_eq!(status_of(&json!({})).0, 0, "缺字段视为成功");
        assert_eq!(
            status_of(&json!({"status_code": 5, "status_desc": "出错了"})).1,
            "出错了"
        );
    }

    #[test]
    fn 时间格式化对空值返回空串() {
        assert_eq!(fmt_local_ts(&Value::Null), "");
        assert_eq!(fmt_local_ts(&json!(0)), "");
        assert_eq!(fmt_local_ts(&json!("")), "");
        // 合法时间戳应有输出
        assert!(!fmt_local_ts(&json!(1_800_000_000i64)).is_empty());
    }

    #[test]
    fn markdown_渲染含标题与角色标记() {
        let payload = json!({
            "userId": "123",
            "exportedAt": "2026-01-01 00:00:00",
            "conversations": [{
                "conversationId": "c1",
                "name": "测试会话",
                "updateTime": 1_800_000_000i64,
                "messages": [
                    {"role": "user", "create_time": 1_800_000_000i64,
                     "content_block": [{"content": {"text_block": {"text": "你好"}}}]},
                    {"role": "assistant", "create_time": 1_800_000_010i64,
                     "content_block": [{"content": {"text_block": {"text": "你好呀"}}}]}
                ]
            }]
        });
        let md = to_markdown(&payload);
        assert!(md.contains("# 豆包对话导出（账号 123）"));
        assert!(md.contains("## 测试会话"));
        assert!(md.contains("🧑 用户"));
        assert!(md.contains("🤖 豆包"));
        assert!(md.contains("你好"));
        assert!(md.contains("你好呀"));
        assert!(md.contains("2 条消息"));
    }

    #[test]
    fn markdown_对空会话列表不panic() {
        let md = to_markdown(&json!({"userId": "1", "conversations": []}));
        assert!(md.contains("共 0 个会话"));
    }

    #[test]
    fn 备份信息在未备份时标记_backed_为假() {
        use crate::modules::config::test_isolation::Isolated;
        let _iso = Isolated::new("chat-info");
        let info = chatdata_info("123456");
        assert_eq!(info["backed"], false);
    }

    #[test]
    fn 非法_uid_被拒绝() {
        assert!(backup_chatdata("../evil").is_err());
        assert!(restore_chatdata("a/b").is_err());
        assert!(chatdata_info("ok").get("backed").is_some());
    }

    #[test]
    fn 统计目录文件数与大小() {
        let dir = std::env::temp_dir().join(format!(
            "ai-gateway-chat-size-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "12345").unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), "123").unwrap();
        assert_eq!(count_files(&dir), 2);
        assert_eq!(dir_size(&dir), 8);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
