//! Trae 设备指纹与设备密钥对。
//!
//! # 为什么指纹必须**读真实客户端**而不是自铸
//!
//! 参考实现实测（auth.js:1163-1167，2026-09-09）：
//!
//! > TRAE_PER_ACCOUNT_DEVICE_IDS=on sends each pool account's minted identity.
//! > **DEFAULT OFF**, measured 2026-09-09: minted (unregistered) device ids get
//! > **SILENTLY DROPPED** by the upstream — TCP/TLS connects but no response ever
//! > arrives (the old shared fingerprint at least gets a fast 4017/answer).
//!
//! 也就是说：**自铸的设备标识会被上游静默丢弃**（连接建立、永不回包），表现为
//! 「请求卡死」而不是任何可诊断的错误码。所以这里的取值顺序与 JS 参考实现
//! auth.js:1136-1152 的 getDeviceInfo 逐项一致：
//!
//! ```text
//!     machine_id = storage['telemetry.machineId'] || TRAE_MACHINE_ID || ''
//!     device_id  = TRAE_DEVICE_ID || soloDeviceId || hashDeviceId(machine_id) || ''
//! ```
//!
//! soloDeviceId 来自 storage.json 里那个形如
//! `iCubeAuthInfo://icube-dc:4496320041074586` 的键（正则见 extractSoloDeviceId）
//! —— **SOLO 真实流量用的是 aha device id（纯数字），不是 hash(machineId)**。
//!
//! # 找不到真实客户端时怎么办
//!
//! 回退到宿主写下的 trae_device.json（OAuth 登录时生成并持久化的身份，见
//! ai-gateway-core::modules::trae_oauth::device_identity）。它同样是**已注册**的
//! 身份，比编一个随机数安全得多。两份都拿不到时返回空串，让上游按「未知设备」
//! 处理 —— 这与参考实现在读不到 storage.json 时的行为一致。

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::realm::{self, Realm};

/// 宿主身份文件（与账号库同目录）。网关只读，绝不写。
pub const DEVICE_FILE: &str = "trae_device.json";

/// getDeviceInfo 的兜底值（auth.js:1143-1148）。
pub const DEFAULT_DEVICE_MODEL_SOLO: &str = "83DG";
pub const DEFAULT_DEVICE_MODEL_TRAE: &str = "82RF";
pub const DEFAULT_CPU: &str = "Intel";
pub const DEFAULT_OS_NAME: &str = "windows";
pub const DEFAULT_OS_VERSION_SOLO: &str = "Windows 11 Pro";
pub const DEFAULT_OS_VERSION_TRAE: &str = "Windows 10";

/// 非 SOLO 产品线的 x-ide-version-code 兜底（auth.js:381）。
pub const DEFAULT_IDE_VERSION_CODE: &str = "20260401";

/// SOLO 产品线的 x-ide-version 兜底。
///
/// auth.js:1072-1096 优先读已装客户端 product.json 的 appVersion，读不到才用
/// 区域默认。网关拿不到那个文件（也不该去读用户安装目录），故直接用实测值：
/// 国服 SOLO 0.1.38、国际版 SOLO 0.1.65。
pub const SOLO_IDE_VERSION_CN: &str = "0.1.38";
pub const SOLO_IDE_VERSION_SG: &str = "0.1.65";

/// 从真实客户端读到的设备标识。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceIds {
    pub machine_id: String,
    pub device_id: String,
    pub sqm_id: String,
    pub dev_device_id: String,
    /// 数据目录名含 SOLO ⇒ 客户端是 TRAE SOLO。
    pub is_solo: bool,
}

/// 一次对话请求要用的全部设备信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceProfile {
    pub machine_id: String,
    pub device_id: String,
    pub device_model: String,
    pub cpu: String,
    pub os_name: String,
    pub os_version: String,
    pub is_solo: bool,
}

fn env_trim(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// %APPDATA%（可能被重定向），回退 ~/AppData/Roaming。
fn roaming_root() -> Option<PathBuf> {
    if let Some(v) = env_trim("APPDATA") {
        return Some(PathBuf::from(v));
    }
    dirs::home_dir().map(|h| h.join("AppData").join("Roaming"))
}

/// 该区域下所有候选 storage.json（按产品目录顺序）。
pub fn storage_json_paths(realm: Realm) -> Vec<PathBuf> {
    let Some(root) = roaming_root() else {
        return Vec::new();
    };
    realm::realm_config(realm)
        .product_dirs
        .iter()
        .map(|name| {
            root.join(name)
                .join("User")
                .join("globalStorage")
                .join("storage.json")
        })
        .collect()
}

/// 在指定根目录下按「最新的 mtime 优先」挑一个可用的 storage.json。
///
/// 与参考实现 resolveProductDataDir 同口径：**同一区域内多个产品目录同时存在时
/// 取 storage.json 最近被写过的那一个**（用户实际在用的那个），而不是列表里的第一个。
fn read_storage_under(root: &Path, realm: Realm) -> Option<(DeviceIds, PathBuf)> {
    let mut best: Option<(DeviceIds, PathBuf, u64)> = None;
    for name in realm::realm_config(realm).product_dirs {
        let path = root
            .join(name)
            .join("User")
            .join("globalStorage")
            .join("storage.json");
        let Ok(raw) = std::fs::read(&path) else { continue };
        let Ok(v) = serde_json::from_slice::<Value>(&raw) else {
            continue;
        };
        let machine_id = v
            .get("telemetry.machineId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if machine_id.is_empty() {
            continue;
        }
        let ids = DeviceIds {
            machine_id,
            device_id: solo_device_id_of(&v),
            sqm_id: str_at(&v, "telemetry.sqmId"),
            dev_device_id: str_at(&v, "telemetry.devDeviceId"),
            is_solo: name.to_ascii_uppercase().contains("SOLO"),
        };
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if best.as_ref().map(|(_, _, m)| mtime > *m).unwrap_or(true) {
            best = Some((ids, path, mtime));
        }
    }
    best.map(|(ids, path, _)| (ids, path))
}

fn str_at(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 从 storage.json 里取 SOLO 的真实 aha device id。
///
/// 键形如 `iCubeAuthInfo://icube-dc:4496320041074586`，取**第一个**匹配
/// 数字结尾的键（参考实现 extractSoloDeviceId）。
fn solo_device_id_of(v: &Value) -> String {
    let Some(map) = v.as_object() else {
        return String::new();
    };
    for key in map.keys() {
        if let Some(rest) = key.strip_prefix("iCubeAuthInfo://icube-dc:") {
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                return rest.to_string();
            }
        }
    }
    String::new()
}

/// 读本机真实客户端指纹；没有可用客户端时返回空值。
pub fn read_device_ids(realm: Realm) -> DeviceIds {
    roaming_root()
        .and_then(|root| read_storage_under(&root, realm))
        .map(|(ids, _)| ids)
        .unwrap_or_default()
}

/// JS hashDeviceId（auth.js:1207-1216）的逐位复刻。
///
/// ```text
///     let hash = 0;
///     for (...) { const char = machineId.charCodeAt(i); hash = ((hash << 5) - hash) + char; hash = hash & hash; }
///     return Math.abs(hash).toString().padStart(19, '0');
///
/// ```
/// hash & hash 在 JS 里是把值强制回 **Int32**（按位运算的语义），故这里用 i32
/// 承载并按 32 位回绕。返回 19 位左补零的十进制串。
pub fn hash_device_id(machine_id: &str) -> String {
    if machine_id.is_empty() {
        return String::new();
    }
    let mut hash: i32 = 0;
    for ch in machine_id.chars() {
        let code = ch as u32 as i32;
        hash = hash.wrapping_shl(5).wrapping_sub(hash).wrapping_add(code);
        hash &= hash;
    }
    // Math.abs(i32::MIN) 是 2147483648，用 i64 承接才不会溢出。
    let abs = (hash as i64).abs();
    format!("{abs:019}")
}

/// 设备标识的取值顺序（auth.js:1145）：环境变量 > SOLO aha id > 哈希 > 空。
pub fn device_id_from(machine_id: &str, solo_device_id: &str) -> String {
    if let Some(v) = env_trim("TRAE_DEVICE_ID") {
        return v;
    }
    if !solo_device_id.is_empty() {
        return solo_device_id.to_string();
    }
    hash_device_id(machine_id)
}

/// x-ide-version。
///
/// 顺序：环境变量 TRAE_IDE_VERSION > SOLO 区域默认 > 区域表默认（后者自己还认
/// TRAE_IDE_VERSION_CN / _SG）。
pub fn ide_version(realm: Realm, is_solo: bool) -> String {
    if let Some(v) = env_trim("TRAE_IDE_VERSION") {
        return v;
    }
    if is_solo {
        return match realm {
            Realm::Cn => SOLO_IDE_VERSION_CN.to_string(),
            Realm::Sg => SOLO_IDE_VERSION_SG.to_string(),
        };
    }
    realm::ide_version(realm)
}

/// x-ide-version-code。
///
/// 顺序：环境变量 TRAE_IDE_VERSION_CODE > SOLO 用**当天日期** YYYYMMDD >
/// 区域默认 20260401。SOLO 用日期是参考实现 getIdeVersionCode 的行为 ——
/// SOLO 是滚动发布，上游按日期校验版本号。
pub fn ide_version_code(is_solo: bool) -> String {
    if let Some(v) = env_trim("TRAE_IDE_VERSION_CODE") {
        return v;
    }
    if is_solo {
        return chrono::Local::now().format("%Y%m%d").to_string();
    }
    DEFAULT_IDE_VERSION_CODE.to_string()
}

/// 由凭证文件路径反推宿主的 store 目录。
///
/// `<data>/trae/auths/trae-<uid>.json` ⇒ `<data>`。网关**不读**
/// AI_GATEWAY_HOME（见 gateway.rs:2557-2559 的契约注释），路径只能这样反推。
///
/// ★ 必须是**三层**父目录：宿主的设备身份写在 `store_dir()/trae_device.json`
/// （见 trae_oauth.rs:468），而凭证目录是 `store_dir()/trae/auths`。
/// 只剥两层会得到 `store_dir()/trae`，那里**没有** trae_device.json，
/// `read_identity` 会静默返回 None，机器标识退回空值 —— 上游对未知设备
/// 直接静默丢弃（TCP/TLS 连上但不回包）。
pub fn store_dir_of_cred_file(file_path: &str) -> Option<PathBuf> {
    let p = Path::new(file_path);
    p.parent()?.parent()?.parent().map(|d| d.to_path_buf())
}

/// 读宿主写下的设备身份 trae_device.json（machineId / deviceId）。
///
/// 与 trae_oauth::device_identity 同口径：machineId 必须是 **64 位 hex**、
/// deviceId 非空，否则视为无效（不猜、不补）。
pub fn read_identity(store_dir: &Path) -> Option<(String, String)> {
    let raw = std::fs::read(store_dir.join(DEVICE_FILE)).ok()?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    let m = v.get("machineId").and_then(Value::as_str).unwrap_or("").trim();
    let d = v.get("deviceId").and_then(Value::as_str).unwrap_or("").trim();
    if m.len() == 64 && !d.is_empty() {
        Some((m.to_string(), d.to_string()))
    } else {
        None
    }
}

/// 读设备私钥（PKCS#8 PEM）。刷新时构造 DeviceProof 用；缺了就刷新不了。
pub fn read_private_key(store_dir: &Path) -> Option<String> {
    let raw = std::fs::read(store_dir.join(DEVICE_FILE)).ok()?;
    let v: Value = serde_json::from_slice::<Value>(&raw).ok()?;
    let k = v
        .get("devicePrivateKey")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if k.is_empty() {
        None
    } else {
        Some(k)
    }
}

/// 读设备公钥（SPKI PEM）。
pub fn read_public_key(store_dir: &Path) -> Option<String> {
    let raw = std::fs::read(store_dir.join(DEVICE_FILE)).ok()?;
    let v: Value = serde_json::from_slice::<Value>(&raw).ok()?;
    let k = v
        .get("devicePublicKey")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if k.is_empty() {
        None
    } else {
        Some(k)
    }
}

/// 组装一次对话请求要用的设备信息。
///
/// store_dir 由调用方从凭证文件路径反推（store_dir_of_cred_file）。
pub fn resolve(realm: Realm, store_dir: Option<&Path>) -> DeviceProfile {
    let mut ids = read_device_ids(realm);
    let mut is_solo = ids.is_solo;
    // 真实客户端目录读到了指纹 ⇒ 它的目录名就是产品线的**证据**（哪怕名字里没有 SOLO）。
    let client_gave_identity = !ids.machine_id.is_empty();

    // 真实客户端读不到（未安装 / 数据目录被清空）时，退回宿主身份文件。
    if ids.machine_id.is_empty() {
        if let Some(dir) = store_dir {
            if let Some((m, d)) = read_identity(dir) {
                ids.machine_id = m;
                ids.device_id = d;
            }
        }
    }

    // 产品线判定：环境变量优先（与参考实现 isSoloProduct 一致），否则看真实
    // 客户端目录名；两者都没有证据时**默认 SOLO**。
    //
    // 为什么无证据时选 SOLO：两条产品线的池子覆盖面是**单边**的 —— CN 的
    // solo_work_lite 覆盖别名表全部 18 个 config_name，chat_v3 只覆盖 15 个
    // （缺 Doubao-Seed-2.0-Code / glm-5-turbo / kimi-k2.6；实测 2026-10-03）。
    // 「读不到客户端」与产品线无关：本机 %APPDATA% 里的 TRAE SOLO CN 就是个
    // 2 字节的空壳，网关跳过它之后若把「读不到」当成「非 SOLO」，拿到的正是
    // 服务不了这几个模型的 chat_v3。反向不成立：目录名真读到且不含 SOLO 时
    // client_gave_identity 为真，decide_solo 会照目录名给出非 SOLO。
    is_solo = decide_solo(
        env_trim("TRAE_PRODUCT").as_deref(),
        is_solo,
        client_gave_identity,
    );

    let device_id = device_id_from(&ids.machine_id, &ids.device_id);

    DeviceProfile {
        machine_id: ids.machine_id,
        device_id,
        device_model: env_trim("TRAE_DEVICE_MODEL").unwrap_or_else(|| {
            if is_solo {
                DEFAULT_DEVICE_MODEL_SOLO.to_string()
            } else {
                DEFAULT_DEVICE_MODEL_TRAE.to_string()
            }
        }),
        cpu: env_trim("TRAE_CPU").unwrap_or_else(|| DEFAULT_CPU.to_string()),
        os_name: env_trim("TRAE_OS_NAME").unwrap_or_else(|| DEFAULT_OS_NAME.to_string()),
        os_version: env_trim("TRAE_OS_VERSION").unwrap_or_else(|| {
            if is_solo {
                DEFAULT_OS_VERSION_SOLO.to_string()
            } else {
                DEFAULT_OS_VERSION_TRAE.to_string()
            }
        }),
        is_solo,
    }
}

/// 产品线判定：环境变量优先，其次真实客户端目录名，都没有证据时默认 SOLO。
///
/// 抽成纯函数只为可测 —— [resolve] 会去读 %APPDATA%，测试没法穷举那台机器。
fn decide_solo(env: Option<&str>, client_named_solo: bool, client_gave_identity: bool) -> bool {
    match env {
        // 与参考实现 isSoloProduct 同口径：只看是否含 solo（不分大小写）。
        Some(v) => v.to_ascii_lowercase().contains("solo"),
        // 目录名给出的证据优先于默认值；没有证据才落默认的 SOLO。
        None => !client_gave_identity || client_named_solo,
    }
}

/// 把一份设备档案整体切到另一条产品线（撞池自愈用）。
///
/// 光翻 is_solo 会让请求头自相矛盾：x-device-brand 与 x-os-version 都按产品线取值
/// （83DG / Windows 11 Pro 属 SOLO，82RF / Windows 10 属 IDE）。这里按 [resolve] 里
/// 同一套规则把三者一起重算，环境变量覆盖照旧优先。
pub fn switch_product_line(p: &DeviceProfile, is_solo: bool) -> DeviceProfile {
    let mut out = p.clone();
    out.is_solo = is_solo;
    out.device_model = env_trim("TRAE_DEVICE_MODEL").unwrap_or_else(|| {
        if is_solo {
            DEFAULT_DEVICE_MODEL_SOLO.to_string()
        } else {
            DEFAULT_DEVICE_MODEL_TRAE.to_string()
        }
    });
    out.os_version = env_trim("TRAE_OS_VERSION").unwrap_or_else(|| {
        if is_solo {
            DEFAULT_OS_VERSION_SOLO.to_string()
        } else {
            DEFAULT_OS_VERSION_TRAE.to_string()
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trae-device-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_storage(root: &Path, product: &str, body: &str) {
        let dir = root.join(product).join("User").join("globalStorage");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("storage.json"), body).unwrap();
    }

    #[test]
    fn hash_device_id_is_nineteen_digits_and_stable() {
        // 与 JS 参考实现同一条 machineId 的结果一致（19 位左补零）。
        assert_eq!(
            hash_device_id("635ec39867ce055784d1ad21c251c4c82c9bb92f55b31a9abda0606b552b5a1d"),
            "0000000002130797930"
        );
        assert_eq!(hash_device_id(""), "");
        assert_eq!(hash_device_id("abc").len(), 19);
    }

    #[test]
    fn solo_device_id_is_read_from_the_icube_dc_key() {
        let v: Value =
            serde_json::from_str(r#"{"iCubeAuthInfo://icube-dc:4496320041074586":1,"other":2}"#)
                .unwrap();
        assert_eq!(solo_device_id_of(&v), "4496320041074586");
        let none: Value = serde_json::from_str(r#"{"iCubeAuthInfo://icube-dc:abc":1}"#).unwrap();
        assert_eq!(solo_device_id_of(&none), "");
    }

    #[test]
    fn storage_scan_prefers_the_most_recently_written_file() {
        let root = temp_root("mtime");
        let old = r#"{"telemetry.machineId":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;
        let new = r#"{"telemetry.machineId":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#;
        write_storage(&root, "TRAE SOLO CN", old);
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_storage(&root, "Trae CN", new);
        let (ids, path) = read_storage_under(&root, Realm::Cn).unwrap();
        assert!(ids.machine_id.starts_with("bbbb"));
        assert!(path.to_string_lossy().contains("Trae CN"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn storage_scan_skips_empty_or_broken_files() {
        let root = temp_root("broken");
        write_storage(&root, "TRAE SOLO CN", "{}");
        write_storage(&root, "Trae CN", "not json at all");
        assert!(read_storage_under(&root, Realm::Cn).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_solo_directory_is_recognised_as_the_solo_product() {
        let root = temp_root("solo");
        write_storage(
            &root,
            "TRAE SOLO CN",
            r#"{"telemetry.machineId":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","iCubeAuthInfo://icube-dc:4496320041074586":1}"#,
        );
        let (ids, _) = read_storage_under(&root, Realm::Cn).unwrap();
        assert!(ids.is_solo);
        assert_eq!(ids.device_id, "4496320041074586");
        assert_eq!(
            device_id_from(&ids.machine_id, &ids.device_id),
            "4496320041074586"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn store_dir_is_three_levels_above_the_credential_file() {
        // <data>/trae/auths/trae-<uid>.json ⇒ <data>：device 文件在 <data> 根下。
        let p = store_dir_of_cred_file("C:/Users/x/.wb-switch/trae/auths/trae-1.json").unwrap();
        assert_eq!(p, PathBuf::from("C:/Users/x/.wb-switch"));
        // 与宿主写入位置一致时才可能读到身份：<data>/trae_device.json
        assert_eq!(p.join(DEVICE_FILE), PathBuf::from("C:/Users/x/.wb-switch/trae_device.json"));
    }

    #[test]
    fn ide_version_code_falls_back_to_the_documented_constant() {
        if env_trim("TRAE_IDE_VERSION_CODE").is_none() {
            assert_eq!(ide_version_code(false), DEFAULT_IDE_VERSION_CODE);
            // SOLO 用当天日期（8 位数字）。
            let code = ide_version_code(true);
            assert_eq!(code.len(), 8);
            assert!(code.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    #[test]
    fn ide_version_prefers_the_solo_defaults() {
        if env_trim("TRAE_IDE_VERSION").is_none() {
            assert_eq!(ide_version(Realm::Cn, true), SOLO_IDE_VERSION_CN);
            assert_eq!(ide_version(Realm::Sg, true), SOLO_IDE_VERSION_SG);
            assert_eq!(ide_version(Realm::Cn, false), "3.3.67");
        }
    }

    #[test]
    fn resolve_never_invents_an_identity() {
        // 既没有真实客户端、也没有 store_dir ⇒ 只能给空串，绝不编造。
        let profile = resolve(Realm::Cn, None);
        if profile.machine_id.is_empty() {
            assert_eq!(profile.device_id, "");
        }
    }

    #[test]
    fn the_product_line_needs_evidence_to_stay_non_solo() {
        assert!(decide_solo(None, true, true), "目录名含 SOLO ⇒ SOLO");
        assert!(
            !decide_solo(None, false, true),
            "目录名给出了非 SOLO 的证据 ⇒ 照它走"
        );
        assert!(
            decide_solo(None, false, false),
            "没有任何证据时默认 SOLO（chat_v3 覆盖不全）"
        );
        assert!(decide_solo(Some("solo"), false, true), "环境变量优先");
        assert!(decide_solo(Some("SOLO"), false, true), "只看是否含 solo，不分大小写");
        assert!(!decide_solo(Some("trae"), true, true), "环境变量优先");
    }

    #[test]
    fn switching_the_product_line_keeps_the_fingerprint_and_the_coherence() {
        let p = resolve(Realm::Cn, None);
        let other = switch_product_line(&p, !p.is_solo);
        assert_eq!(other.is_solo, !p.is_solo);
        assert_eq!(other.machine_id, p.machine_id, "设备指纹不随产品线变");
        assert_eq!(other.device_id, p.device_id);
        // 机型码 / OS 版本是两条产品线各自的取值，必须跟着新身份走（环境变量显式
        // 覆盖时当然不翻）。
        if env_trim("TRAE_DEVICE_MODEL").is_none() {
            assert_ne!(other.device_model, p.device_model, "83DG / 82RF 各属一条线");
        }
        if env_trim("TRAE_OS_VERSION").is_none() {
            assert_ne!(other.os_version, p.os_version, "Windows 11 Pro / 10 各属一条线");
        }
        assert_eq!(switch_product_line(&other, p.is_solo), p, "翻两次回到原样");
    }
}
