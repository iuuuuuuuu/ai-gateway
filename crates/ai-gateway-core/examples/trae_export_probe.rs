//! Trae 凭证导出链路验证（真实实例实测，**只写网关凭证目录**）。
//!
//! 用途：把宿主账号库 `<store_dir>/trae_accounts.json` 里的 Trae 账号
//! 导出成网关能加载的两段式凭证 `<store_dir>/trae/auths/trae-<uid>.json`。
//!
//! 这正是宿主 GUI 在 `start_gateway` 与 `sync_if_changed` 里自动做的事
//!（见 trae_account::export_accounts_to_gateway / export_if_changed），
//! 本探针把它单独跑一遍并打印全过程，用于验证真实实例。
//!
//! 运行：
//!   cargo run -p ai-gateway-core --example trae_export_probe
//!
//! ★ 只读账号库：不写 trae_accounts.json，不碰 trae_device.json。
//!   唯一被写的是导出目录（网关凭证，本来就要生成）。
//! ★ 打印的 token 一律脱敏，不把完整凭证写进日志。

use ai_gateway_core::modules::trae_account as ta;

fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn tail(s: &str, n: usize) -> String {
    let v: Vec<char> = s.chars().collect();
    v[v.len().saturating_sub(n)..].iter().collect()
}

/// 宿主写出的网关原生配置路径（只读展示，不改）。
fn native_cfg_path() -> std::path::PathBuf {
    ta::store_dir().join("gateway").join("gateway_native_config.json")
}

/// 把凭证里的 token 换成脱敏占位后再打印，证明「形状对不对」而不是泄露凭证。
fn print_cred_shape(path: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        println!("    (读不出来)");
        return;
    };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else {
        println!("    (不是合法 JSON)");
        return;
    };
    if let Some(t) = v
        .pointer("/auth/token")
        .and_then(|x| x.as_str())
        .map(str::to_string)
    {
        let masked = format!("<{}...{} 共 {} 字节>", head(&t, 17), tail(&t, 6), t.len());
        if let Some(obj) = v.get_mut("auth").and_then(|a| a.as_object_mut()) {
            obj.insert("token".to_string(), serde_json::Value::String(masked));
        }
    }
    match serde_json::to_string_pretty(&v) {
        Ok(s) => {
            for line in s.lines() {
                println!("    {line}");
            }
        }
        Err(e) => println!("    (序列化失败: {e})"),
    }
}

fn main() {
    println!("=== Trae 凭证导出链路验证（只写网关凭证目录）===");

    let store = ta::store_dir();
    let accounts_file = ta::accounts_file();
    let auth_dir = ta::auth_dir();
    let device_file = store.join("trae_device.json");
    let cfg_file = native_cfg_path();
    println!("store_dir    = {}", store.display());
    println!("账号库        = {}", accounts_file.display());
    println!("凭证目录      = {}", auth_dir.display());
    println!(
        "设备身份      = {} ({})",
        device_file.display(),
        if device_file.exists() { "存在" } else { "缺失" }
    );
    println!(
        "网关原生配置  = {} ({})",
        cfg_file.display(),
        if cfg_file.exists() { "存在" } else { "缺失" }
    );

    if std::env::var_os("AI_GATEWAY_HOME").is_none() {
        println!("⚠ AI_GATEWAY_HOME 未设置 ⇒ 操作的是真实用户目录");
    }

    // ---- 1. 账号库 ----
    let accounts = ta::load_accounts();
    println!("\n---- 1. 账号库（{} 条）----", accounts.len());
    for a in &accounts {
        let uid = a.get("user_id").and_then(|x| x.as_str()).unwrap_or("");
        let jwt = a.get("jwt").and_then(|x| x.as_str()).unwrap_or("");
        let rt = a.get("refresh_token").and_then(|x| x.as_str()).unwrap_or("");
        let guess = ta::account_realm(a);
        println!(
            "  uid={uid} name={} realm={} evidence={}",
            ta::display_name(a),
            guess.realm.as_str(),
            guess.evidence.as_str()
        );
        println!(
            "      jwt = {}...{}（{} 字节） refresh_token = {} 字节 disabled={}",
            head(jwt, 17),
            tail(jwt, 6),
            jwt.len(),
            rt.len(),
            a.get("disabled").and_then(|x| x.as_bool()).unwrap_or(false)
        );
    }

    // ---- 2. 建目录 ----
    println!("\n---- 2. ensure_dirs ----");
    if let Err(e) = ta::ensure_dirs() {
        println!("失败：{e}");
        std::process::exit(2);
    }
    println!("ok: {}", auth_dir.display());

    // ---- 3. 导出 ----
    println!("\n---- 3. export_accounts_to_gateway ----");
    match ta::export_accounts_to_gateway() {
        Ok((written, changed)) => println!("written={written} changed={changed:?}"),
        Err(e) => {
            println!("失败：{e}");
            std::process::exit(3);
        }
    }

    // ---- 4. 幂等（内容一致时不应再写盘）----
    println!("\n---- 4. 再导出一次（应无 changed）----");
    match ta::export_accounts_to_gateway() {
        Ok((written, changed)) => println!(
            "written={written} changed={changed:?} ⇒ {}",
            if changed.is_empty() {
                "幂等 ok"
            } else {
                "⚠ 非幂等（每次都在改盘）"
            }
        ),
        Err(e) => println!("失败：{e}"),
    }

    // ---- 5. 指纹门 ----
    println!("\n---- 5. export_if_changed（指纹未变 ⇒ false 正常）----");
    println!("返回值 = {}", ta::export_if_changed());

    // ---- 6. 产物 ----
    println!("\n---- 6. 凭证目录内容 ----");
    match std::fs::read_dir(&auth_dir) {
        Ok(entries) => {
            for e in entries.filter_map(|e| e.ok()) {
                let path = e.path();
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                println!("  {} ({} 字节)", path.display(), size);
                print_cred_shape(&path);
            }
        }
        Err(e) => println!("读取失败：{e}"),
    }

    println!("\n=== 完成 ===");
}
