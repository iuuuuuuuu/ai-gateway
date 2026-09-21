//! TRAE 本地登录态捕获（原 `device_proxy.py` `--capture-local` / `capture_from_local` 迁移）。
//!
//! 背景：当前版本 TRAE 的鉴权请求（`api.trae.cn`）不走系统代理，MITM 代理抓不到
//! Cloud-IDE-JWT。但 TRAE 把登录态存在本地，此处直接提取并经 [`update_account_jwt`]
//! 写回账号库，作为代理方案的兜底（**仅 Windows**：依赖 DPAPI + Chromium Cookies 解密）。
//!
//! 扫描两个应用目录：TRAE SOLO CN（Trae Work）与 Trae CN（Trae IDE），
//! 设置 `TRAE_APP_DIR` 时只扫指定目录。
//!
//! ## 三处扫描位置，缺一处就一个账号都导入不了
//!
//! 实测（2026-09，Trae CN / TRAE SOLO CN）凭证**只出现在第三处**：
//!
//! | 位置 | 内容 | 实测结果 |
//! |---|---|---|
//! | `Network\Cookies` | Chromium cookie 库（DPAPI + AES-256-GCM） | 只剩 csdn / bilibili 等第三方 cookie |
//! | `Local Storage\leveldb` | web 侧 KV | 无 |
//! | `logs\<启动时间>\window1\exthost\...\completion.log` | 扩展把请求头打进日志 | **唯一的真实凭证** |
//!
//! 只扫前两处时，「发现本机账号」能正常认出账号，但导入**一个都进不来** ——
//! 界面上表现为「识别到了账号却不能自动填写」。因此第三处不是锦上添花，
//! 而是新版本客户端下唯一有效的来源。
//!
//! 另注：`Cache\Cache_Data` 里也有 `Cloud-IDE-JWT ${e}` 形态的命中，但那全是
//! 前端 bundle 的**模板串**（`Authorization: \`Cloud-IDE-JWT ${e}\``），
//! 不是凭证 —— 正则要求 token 是三段 base64，模板串不匹配，因此不会误命中；
//! 且该目录不在扫描范围内，无需为它付出 IO 代价。
//!
//! ## 与 [`crate::modules::app_profile`] 的关系
//!
//! 应用数据目录一律经 `app_profile::profile_for(TargetApp::TraeWork / Trae)` 取，
//! 不再自己拼 `%APPDATA%\...` —— 换机器/换安装路径时只需改档案表一处。

use std::path::{Path, PathBuf};

use crate::modules::app_profile::{profile_for, TargetApp};
use crate::modules::config;
use crate::modules::device_proxy::handler::{extract_user_id, update_account_jwt, valid_cloud_ide_jwt, ProxyCtx};
use crate::modules::device_proxy::logger::{ProxyLog, RequestLogger};

/// 候选应用数据目录（对齐 Python `_trae_app_dirs`）：Trae Work + Trae CN；
/// 设置 `TRAE_APP_DIR` 时只扫指定目录（兼容旧环境变量）
pub fn trae_app_dirs() -> Vec<PathBuf> {
    if let Ok(env) = std::env::var("TRAE_APP_DIR") {
        if !env.is_empty() {
            return vec![PathBuf::from(env)];
        }
    }
    let store = config::store_dir();
    vec![
        profile_for(TargetApp::TraeWork, &store).data_dir,
        profile_for(TargetApp::Trae, &store).data_dir,
    ]
}

/// 离线场景构造 [`ProxyCtx`]（复用 handler 的账号写回通路；不启动代理、不发事件）
pub fn offline_ctx() -> ProxyCtx {
    let store = config::store_dir();
    let logs = store.join("logs");
    let captured = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    ProxyCtx {
        log: ProxyLog::new(logs.join("proxy.log"), None, captured),
        req_logger: std::sync::Arc::new(RequestLogger::new(logs)),
        targets: Vec::new(),
        auto_capture_jwt: true,
        data_dir: store,
    }
}

/// 一次本地捕获的汇总。
///
/// 为什么不是一个数字：界面需要区分「新增了账号」与「刷新了账号的 JWT」——
/// 前者是「发现了新账号」，后者只是「凭证变新了」。只有一个总数时用户分不清
/// 「什么都没找到」和「找到了但都只是刷新」，而这两者该给的提示完全不同。
///
/// `skipped` 是命中但因**新 token 更旧**被防降级拦下的次数
///（见 [`crate::modules::device_proxy::handler::update_account_jwt`]）——
/// 它是「账号确实在本机、但本机那份比账号库里的旧」的信号，值得单独报出来。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureSummary {
    pub appended: usize,
    pub updated: usize,
    pub skipped: usize,
}

impl CaptureSummary {
    /// 命中但没产生写入（JWT 与账号库一致）的次数。
    pub fn unchanged(&self) -> usize {
        self.skipped
    }

    /// 是否一个账号都没碰到。
    pub fn is_empty(&self) -> bool {
        self.appended == 0 && self.updated == 0 && self.skipped == 0
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "appended": self.appended,
            "updated": self.updated,
            "skipped": self.skipped,
            "total": self.appended + self.updated,
        })
    }
}

/// 把 `update_account_jwt` 的状态串归入汇总。
///
/// `update_account_jwt_and_refresh` 的返回语义（见其文档）：
/// `appended` 新账号 / `updated` JWT 更新 / `skipped` 防降级拦下 / `unchanged` 无变化。
fn tally(summary: &mut CaptureSummary, status: &str) {
    match status {
        "appended" => summary.appended += 1,
        "updated" => summary.updated += 1,
        "skipped" => summary.skipped += 1,
        // `unchanged` 等其它取值不计数：账号没被改动
        _ => {}
    }
}

/// 解密 TRAE 本地 Cookies + 扫描 Local Storage leveldb，提取 Cloud-IDE-JWT 写回账号库
///（对齐 Python `capture_from_local`）。
#[cfg(windows)]
pub fn capture_from_local() -> Result<serde_json::Value, String> {
    let ctx = offline_ctx();
    let mut summary = CaptureSummary::default();
    for app_dir in trae_app_dirs() {
        match capture_from_app_dir(&ctx, &app_dir, &mut summary) {
            Ok(()) => {}
            Err(e) => ctx
                .log
                .log(&format!("[local] 目录 {} 捕获失败: {e}", app_dir.display())),
        }
    }
    ctx.log.log(&format!(
        "[local] 本地捕获完成，新增 {} / 更新 {} / 跳过 {}",
        summary.appended, summary.updated, summary.skipped
    ));
    Ok(serde_json::json!({
        "ok": true,
        // `captured` 保持旧语义（本次有写入的账号数），老调用方不受影响
        "captured": summary.appended + summary.updated,
        "summary": summary.to_json(),
    }))
}

#[cfg(not(windows))]
pub fn capture_from_local() -> Result<serde_json::Value, String> {
    Err("本地捕获仅支持 Windows（需 DPAPI + Chromium Cookies 解密）".into())
}

// ---------------- Windows 解密细节（对齐 Python `_chrome_aes_key`/`_decrypt_cookie`） ----------------

/// 读 `Local State` → `os_crypt.encrypted_key`（DPAPI 包裹的 AES-256 密钥）
#[cfg(windows)]
fn chrome_aes_key(app_dir: &Path) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let lp = app_dir.join("Local State");
    let raw = std::fs::read_to_string(&lp).map_err(|e| format!("Local State 读取失败: {e}"))?;
    let state: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("Local State 解析失败: {e}"))?;
    let b64 = state
        .get("os_crypt")
        .and_then(|v| v.get("encrypted_key"))
        .and_then(serde_json::Value::as_str)
        .ok_or("Local State 无 os_crypt.encrypted_key")?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("encrypted_key base64 解码失败: {e}"))?;
    if raw.len() < 5 || &raw[..5] != b"DPAPI" {
        return Err("encrypted_key 前缀非 DPAPI".into());
    }
    dpapi_unprotect(&raw[5..])
}

/// DPAPI 解密（复用 [`crate::modules::vscode_cn_inject`] 同款 Win32 调用）。
#[cfg(windows)]
fn dpapi_unprotect(encrypted: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    unsafe {
        let mut data_in = CRYPT_INTEGER_BLOB {
            cbData: encrypted.len() as u32,
            pbData: encrypted.as_ptr() as *mut u8,
        };
        let mut data_out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        CryptUnprotectData(&mut data_in, None, None, None, None, 0, &mut data_out)
            .map_err(|e| format!("DPAPI CryptUnprotectData 失败: {e}"))?;
        if data_out.pbData.is_null() || data_out.cbData == 0 {
            return Err("DPAPI 返回空数据".to_string());
        }
        let slice = std::slice::from_raw_parts(data_out.pbData, data_out.cbData as usize);
        let result = slice.to_vec();
        let _ = LocalFree(HLOCAL(data_out.pbData as _));
        Ok(result)
    }
}

/// 单个 cookie 密文解密：`v10` 为 AES-256-GCM（`'v10'` + nonce12 + ct + tag16），
/// 旧格式直接 DPAPI。key 缺失时 v10 放弃（返回 `None`，调用方回退明文 value）。
#[cfg(windows)]
fn decrypt_cookie(enc: &[u8], key: Option<&[u8]>) -> Option<String> {
    if enc.len() >= 3 && &enc[..3] == b"v10" {
        use aes_gcm::aead::Aead;
        use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
        if enc.len() < 19 {
            return None;
        }
        let key = key?;
        let cipher = Aes256Gcm::new_from_slice(key).ok()?;
        let plain = cipher.decrypt(Nonce::from_slice(&enc[3..15]), &enc[15..]).ok()?;
        return Some(String::from_utf8_lossy(&plain).to_string());
    }
    dpapi_unprotect(enc).ok().map(|v| String::from_utf8_lossy(&v).to_string())
}

/// 从一段文本里找 Cloud-IDE-JWT（对齐 Python `_find_cloud_ide_jwt`）：
/// 先匹配显式前缀，命中即返回（无效也直接 `None`，不落入通用扫描）；
/// 否则通用三段 JWT 逐个校验，返回首个通过者（格式化为 `Cloud-IDE-JWT <jwt>`）。
pub fn find_cloud_ide_jwt(blob: &str) -> Option<String> {
    use std::sync::OnceLock;
    static PREFIX_RE: OnceLock<regex::Regex> = OnceLock::new();
    static JWT_RE: OnceLock<regex::Regex> = OnceLock::new();
    if blob.is_empty() {
        return None;
    }
    let prefix = PREFIX_RE.get_or_init(|| {
        regex::Regex::new(r"Cloud-IDE-JWT\s+([A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+)")
            .expect("jwt prefix regex")
    });
    if let Some(caps) = prefix.captures(blob) {
        return valid_cloud_ide_jwt(&caps[1]);
    }
    let jwt = JWT_RE.get_or_init(|| {
        regex::Regex::new(r"[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+").expect("jwt regex")
    });
    for m in jwt.find_iter(blob) {
        if let Some(v) = valid_cloud_ide_jwt(m.as_str()) {
            return Some(v);
        }
    }
    None
}

/// 对单个应用数据目录执行 Cookies 解密 + leveldb 明文扫描（对齐 Python
/// `_capture_from_app_dir`），命中结果累加进 `summary`。
#[cfg(windows)]
fn capture_from_app_dir(
    ctx: &ProxyCtx,
    app_dir: &Path,
    summary: &mut CaptureSummary,
) -> Result<(), String> {
    ctx.log.log(&format!("[local] TRAE 数据目录: {}", app_dir.display()));
    if !app_dir.is_dir() {
        ctx.log.log("[local] 目录不存在，跳过");
        return Ok(());
    }
    let key = match chrome_aes_key(app_dir) {
        Ok(k) => Some(k),
        Err(e) => {
            ctx.log.log(&format!("[local] 未取得 AES 密钥({e})；将仅扫描明文值"));
            None
        }
    };

    // 1) Cookies 数据库：主分区 + trae-webview 分区
    let mut dbs = vec![app_dir.join("Network").join("Cookies")];
    let tw = app_dir.join("Partitions").join("trae-webview").join("Cookies");
    if tw.exists() {
        dbs.push(tw);
    }
    for db in dbs {
        if !db.exists() {
            continue;
        }
        match scan_cookies_db(ctx, &db, key.as_deref(), summary) {
            Ok(()) => {}
            Err(e) => ctx.log.log(&format!("[local] 读取 Cookies 失败 {}: {e}", db.display())),
        }
    }

    // 2) Local Storage leveldb 明文兜底扫描
    for ls in [
        app_dir.join("Local Storage").join("leveldb"),
        app_dir
            .join("Partitions")
            .join("trae-webview")
            .join("Local Storage")
            .join("leveldb"),
    ] {
        if !ls.is_dir() {
            continue;
        }
        scan_leveldb_dir(ctx, &ls, summary);
    }

    // 3) 扩展日志兜底扫描
    //
    // **实测根因**：新版本客户端的登录态既不在 `Network\Cookies`（那里只剩
    // 第三方站点 cookie，如 csdn / bilibili），也不在 `Local Storage\leveldb`。
    // 全盘搜 `Cloud-IDE-JWT` 只有一处真凭证 —— `logs\<启动时间>\window1\exthost\
    // trae.ai-code-completion\completion.log`（请求头调试输出）。只扫前两处
    // 会**一个账号都导入不了**，而发现流程却能正常认出账号
    // —— 这正是「识别到了却不能自动填写」的直接原因。
    scan_logs_dir(ctx, &app_dir.join("logs"), summary);
    Ok(())
}

/// 递归扫描应用日志目录，按字节正则提取 `Cloud-IDE-JWT`。
///
/// 与 [`scan_leveldb_dir`] 同一套提取逻辑，差别只在文件类型与遍历方式：
/// leveldb 是**平铺**的 `.ldb`/`.log`，而日志是**嵌套**的启动时间目录树。
///
/// 只取近 `LOG_DIRS_KEPT` 个启动目录（按名字倒序，`<YYYYMMDD>T<HHMMSS>` 天然可按
/// 字典序比较）：日志里可能有陈旧的、已过期的凭证，而 [`update_account_jwt`] 有
/// exp 防降级保护，扫太多只会浪费 IO 并让日志噪音变大。
#[cfg(windows)]
fn scan_logs_dir(ctx: &ProxyCtx, logs_dir: &Path, summary: &mut CaptureSummary) {
    const LOG_DIRS_KEPT: usize = 3;
    /// 单文件上限：日志可达数百 MB（实测 `aha_log` 已近 1MB/天），
    /// 超过则跳过 —— 凭证出现在 exthost 的小日志里，不值得为它读几十 MB。
    const MAX_LOG_FILE: u64 = 8 * 1024 * 1024;

    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return;
    };
    let mut run_dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    // 目录名形如 `20260918T112945`，字典序即时间序；倒序取最近的几个
    run_dirs.sort();
    run_dirs.reverse();
    run_dirs.truncate(LOG_DIRS_KEPT);

    for dir in run_dirs {
        scan_log_tree(ctx, &dir, summary, MAX_LOG_FILE);
    }
}

/// 递归扫描单个日志目录树。
#[cfg(windows)]
fn scan_log_tree(ctx: &ProxyCtx, dir: &Path, summary: &mut CaptureSummary, max_bytes: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_log_tree(ctx, &path, summary, max_bytes);
            continue;
        }
        // 只认文本日志后缀，不做全类型读取（`Cache_Data` 之类里有大量
        // 前端 bundle，含 `Cloud-IDE-JWT ${e}` 这类**模板串**，正则不会误命中，
        // 但没必要为它们付出 IO 代价）
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if !name.ends_with(".log") && !name.ends_with(".alaudalog") {
            continue;
        }
        let Ok(meta) = path.metadata() else { continue };
        if meta.len() > max_bytes {
            continue;
        }
        let Ok(data) = std::fs::read(&path) else { continue };
        scan_blob_for_jwt(ctx, &data, &name, summary);
    }
}

/// 按字节正则从一段数据里提取 JWT 并写回账号库。
///
/// 提取逻辑与 leveldb 扫描完全一致（同一个正则、同样的 `extract_user_id` 校验），
/// 因此日志兜底不会引入新的解析语义。
#[cfg(windows)]
fn scan_blob_for_jwt(ctx: &ProxyCtx, data: &[u8], label: &str, summary: &mut CaptureSummary) {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::bytes::Regex::new(r"Cloud-IDE-JWT [A-Za-z0-9_\-\.=]+").expect("jwt blob regex")
    });
    for m in re.find_iter(data) {
        let hit = String::from_utf8_lossy(m.as_bytes()).to_string();
        let Some(uid) = extract_user_id(&hit) else { continue };
        let status = update_account_jwt(ctx, &uid, &hit);
        ctx.log
            .log(&format!("[local] 命中日志 {label} -> {status}"));
        tally(summary, status);
    }
}

/// 复制 Cookies 库到临时目录后只读打开（规避客户端运行时文件锁；
/// `-wal`/`-shm` 一并复制避免读到未 checkpoint 的空库），命中结果累加进 `summary`
#[cfg(windows)]
fn scan_cookies_db(
    ctx: &ProxyCtx,
    db: &Path,
    key: Option<&[u8]>,
    summary: &mut CaptureSummary,
) -> Result<(), String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let td = std::env::temp_dir().join(format!("aigw_trae_ck_{}_{nanos}", std::process::id()));
    std::fs::create_dir_all(&td).map_err(|e| format!("临时目录创建失败: {e}"))?;
    let tmp_db = td.join("Cookies");
    let copy_res = std::fs::copy(db, &tmp_db).map_err(|e| format!("Cookies 复制失败: {e}"));
    if copy_res.is_err() {
        let _ = std::fs::remove_dir_all(&td);
        return Err(copy_res.unwrap_err());
    }
    for suffix in ["-wal", "-shm"] {
        let side = db.with_file_name(format!("Cookies{suffix}"));
        if side.exists() {
            let _ = std::fs::copy(&side, td.join(format!("Cookies{suffix}")));
        }
    }

    let result = (|| -> Result<(), String> {
        let conn = rusqlite::Connection::open_with_flags(
            &tmp_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| format!("打开 Cookies 库失败: {e}"))?;
        let mut stmt = conn
            .prepare("SELECT host_key, name, value, encrypted_value FROM cookies")
            .map_err(|e| format!("查询 cookies 表失败: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })
            .map_err(|e| format!("遍历 cookies 失败: {e}"))?;
        for r in rows.flatten() {
            let (host, name, value, enc) = r;
            // 明文 value 优先作 blob；密文解密成功则覆盖（解密失败回退明文，对齐 Python）
            let mut blob = value;
            if !enc.is_empty() {
                if let Some(d) = decrypt_cookie(&enc, key) {
                    blob = d;
                }
            }
            let Some(hit) = find_cloud_ide_jwt(&blob) else { continue };
            let Some(uid) = extract_user_id(&hit) else { continue };
            let status = update_account_jwt(ctx, &uid, &hit);
            ctx.log.log(&format!("[local] 命中 Cookies host={host} name={name} -> {status}"));
            tally(summary, status);
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&td);
    result
}

/// leveldb 目录明文扫描：`.ldb`/`.log` 文件按字节正则找 `Cloud-IDE-JWT <jwt>`
///（对齐 Python：不做结构化解析，命中即写回）
#[cfg(windows)]
fn scan_leveldb_dir(ctx: &ProxyCtx, ls: &Path, summary: &mut CaptureSummary) {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::bytes::Regex::new(r"Cloud-IDE-JWT [A-Za-z0-9_\-\.=]+").expect("leveldb jwt regex")
    });
    let Ok(entries) = std::fs::read_dir(ls) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.ends_with(".ldb") || name.ends_with(".log")) {
            continue;
        }
        let Ok(data) = std::fs::read(entry.path()) else { continue };
        for m in re.find_iter(&data) {
            let hit = String::from_utf8_lossy(m.as_bytes()).to_string();
            let Some(uid) = extract_user_id(&hit) else { continue };
            let status = update_account_jwt(ctx, &uid, &hit);
            ctx.log.log(&format!("[local] 命中 leveldb {name} -> {status}"));
            tally(summary, status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::config::test_isolation::Isolated;

    /// `find_cloud_ide_jwt`：显式前缀优先（无效即止）+ 通用 JWT 校验回退
    #[test]
    fn find_jwt_prefix_and_generic() {
        use base64::Engine;
        let b64 = |v: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
        };
        let header = b64(&serde_json::json!({"alg": "RS256"}));
        let payload = b64(&serde_json::json!({"data": {"id": "u1"}}));
        let jwt = format!("{header}.{payload}.sig");
        // 显式前缀
        let hit = find_cloud_ide_jwt(&format!("blob Cloud-IDE-JWT {jwt} tail")).unwrap();
        assert_eq!(hit, format!("Cloud-IDE-JWT {jwt}"));
        // 通用扫描（无前缀）
        let hit2 = find_cloud_ide_jwt(&format!("x {jwt} y")).unwrap();
        assert_eq!(hit2, format!("Cloud-IDE-JWT {jwt}"));
        // 前缀命中但无效（HS256）→ 直接 None，不落入通用扫描
        let h2 = b64(&serde_json::json!({"alg": "HS256"}));
        let bad = format!("Cloud-IDE-JWT {h2}.{payload}.sig");
        assert!(find_cloud_ide_jwt(&bad).is_none());
        // 空串
        assert!(find_cloud_ide_jwt("").is_none());
    }

    /// 应用数据目录来自档案表（不是硬编码 %APPDATA% 拼接）
    #[test]
    fn app_dirs_come_from_profile_table() {
        let _iso = Isolated::new("local-capture-dirs");
        let dirs = trae_app_dirs();
        assert_eq!(dirs.len(), 2);
        let store = config::store_dir();
        assert_eq!(dirs[0], profile_for(TargetApp::TraeWork, &store).data_dir);
        assert_eq!(dirs[1], profile_for(TargetApp::Trae, &store).data_dir);
        assert_ne!(dirs[0], dirs[1], "两个 Trae 应用目录必须分开");
    }

    /// TRAE_APP_DIR 覆盖时只扫指定目录（兼容旧环境变量）
    #[test]
    fn trae_app_dir_env_overrides() {
        let _iso = Isolated::new("local-capture-env");
        std::env::set_var("TRAE_APP_DIR", r"D:\fake\trae");
        let dirs = trae_app_dirs();
        std::env::remove_var("TRAE_APP_DIR");
        assert_eq!(dirs, vec![PathBuf::from(r"D:\fake\trae")]);
    }

    /// 离线 ctx 的路径全部落在 store_dir 下
    #[test]
    fn offline_ctx_uses_store_dir() {
        let iso = Isolated::new("local-capture-ctx");
        let ctx = offline_ctx();
        assert_eq!(ctx.data_dir, iso.dir());
        assert!(ctx.targets.is_empty(), "离线捕获不需要域名列表");
        assert!(ctx.auto_capture_jwt);
    }

    /// 目录不存在时静默返回、汇总保持为空（不报错）
    #[cfg(windows)]
    #[test]
    fn missing_app_dir_returns_zero() {
        let _iso = Isolated::new("local-capture-missing");
        let ctx = offline_ctx();
        let missing = std::env::temp_dir().join("aigw_definitely_missing_dir_12345");
        let mut summary = CaptureSummary::default();
        capture_from_app_dir(&ctx, &missing, &mut summary).unwrap();
        assert!(summary.is_empty(), "目录不存在时不应产生任何计数");
    }

    /// 状态串 → 计数归集：四个语义各归各位。
    #[test]
    fn 捕获计数按状态归集() {
        let mut s = CaptureSummary::default();
        tally(&mut s, "appended");
        tally(&mut s, "appended");
        tally(&mut s, "updated");
        tally(&mut s, "skipped");
        // 无变化的命中不计入任何写入计数
        tally(&mut s, "unchanged");
        tally(&mut s, "");
        assert_eq!(
            s,
            CaptureSummary { appended: 2, updated: 1, skipped: 1 }
        );
        assert!(!s.is_empty());
        let j = s.to_json();
        assert_eq!(j["appended"], 2);
        assert_eq!(j["updated"], 1);
        assert_eq!(j["skipped"], 1);
        // total 只算真正写盘的（跳过的不算）
        assert_eq!(j["total"], 3);

        assert!(CaptureSummary::default().is_empty());
    }

    /// 造一个 payload 可控的 Cloud-IDE-JWT。
    #[cfg(windows)]
    fn make_cloud_ide_jwt(uid: &str, exp: i64) -> String {
        use base64::Engine;
        let b64 = |v: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
        };
        let header = b64(&serde_json::json!({"alg": "RS256", "typ": "JWT"}));
        let payload = b64(&serde_json::json!({"data": {"id": uid}, "exp": exp}));
        format!("Cloud-IDE-JWT {header}.{payload}.sig")
    }

    /// 日志兜底：新版本客户端把真实凭证只写在扩展日志里。
    ///
    /// 回归：早期只扫 `Network\Cookies` + `Local Storage\leveldb`，
    /// 实测这两处**都没有**新版本客户端的主凭证 —— 结果是「发现」能认出账号，
    /// 导入却一个都进不来（正是用户报的「识别到了却不能自动填写」）。
    #[cfg(windows)]
    #[test]
    fn 日志兜底能扫到凭证() {
        let _iso = Isolated::new("local-capture-logs");
        let ctx = offline_ctx();
        let base = std::env::temp_dir().join(format!(
            "aigw-trae-logs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // 目录名形如 <YYYYMMDD>T<HHMMSS>，模拟真实布局
        let log_dir = base.join("20260918T112945").join("window1").join("exthost");
        std::fs::create_dir_all(&log_dir).unwrap();
        let jwt = make_cloud_ide_jwt("1883919207380040", chrono::Utc::now().timestamp() + 86_400);
        std::fs::write(
            log_dir.join("completion.log"),
            format!("[debug] request headers: Authorization: {jwt}\n"),
        )
        .unwrap();
        // 非日志后缀的同内容文件不应被读（前端 bundle 里的模板串同形但无害）
        std::fs::write(base.join("bundle.js"), &jwt).unwrap();

        let mut summary = CaptureSummary::default();
        scan_logs_dir(&ctx, &base, &mut summary);
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(
            summary.appended, 1,
            "日志里的凭证必须被扫到并写入账号库，实际: {summary:?}"
        );
        let acc = crate::modules::trae_account::load_accounts();
        assert_eq!(acc.len(), 1);
        assert_eq!(acc[0]["user_id"], "1883919207380040");
    }

    /// 日志兜底只取最近的几个启动目录（陈旧日志里的过期凭证不必读）。
    #[cfg(windows)]
    #[test]
    fn 日志兜底只扫最近几个启动目录() {
        let _iso = Isolated::new("local-capture-log-dirs");
        let ctx = offline_ctx();
        let base = std::env::temp_dir().join(format!(
            "aigw-trae-logdirs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let now = chrono::Utc::now().timestamp();
        // 5 个启动目录，各放一个不同 uid 的凭证
        for (i, dir) in [
            "20260101T000000",
            "20260102T000000",
            "20260103T000000",
            "20260104T000000",
            "20260105T000000",
        ]
        .iter()
        .enumerate()
        {
            let d = base.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            let uid = format!("18839192073800{i:02}");
            std::fs::write(
                d.join("main.log"),
                make_cloud_ide_jwt(&uid, now + 86_400),
            )
            .unwrap();
        }

        let mut summary = CaptureSummary::default();
        scan_logs_dir(&ctx, &base, &mut summary);
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(
            summary.appended, 3,
            "只应扫最近的 3 个启动目录，实际: {summary:?}"
        );
    }

    /// 非 Windows：本地捕获显式报错（依赖 DPAPI）
    #[cfg(not(windows))]
    #[test]
    fn capture_is_unsupported_off_windows() {
        assert!(capture_from_local().is_err());
    }
}
