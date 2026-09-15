//! CA / 叶子证书管理（原 `device_proxy.py` `ensure_ca` / `leaf_cert` 的 Rust 版）。
//!
//! 兼容性红线：老版本 Python 生成的 CA（RSA 2048、CN=`TraeDeviceProxyCA`、PKCS#1 私钥）
//! 必须原样加载 —— 用户已将其安装进 Windows 受信任根存储，换 CA 等于强制所有用户重装证书。
//!
//! 本仓库的 rustls 后端是 **ring**（与 `reqwest` 的 `rustls-tls` 保持一致，避免同时
//! 激活两个 CryptoProvider、也避免 aws-lc-sys 的 cmake/nasm 构建依赖）。ring 的
//! `KeyPair::from_pem` **只支持 PKCS#8**（`PRIVATE KEY`），而历史 CA 私钥是
//! **PKCS#1**（`RSA PRIVATE KEY`）—— 直接喂给 rcgen 会解析失败，导致「用户已装好
//! 证书却提示未生成 CA」的死循环。故此处自带一个最小 PKCS#1 → PKCS#8 包装器
//! （见 [`pkcs1_pem_to_pkcs8_pem`]）：只加一层 ASN.1 外壳，不动密钥本身。
//!
//! 叶子证书不再像 Python 版那样写临时文件：rcgen 在内存内签名 + LRU 缓存 ServerConfig，
//! 私钥永不落盘（Python 版的 atexit 清理 / 残留清扫随之简化为「启动清扫一次历史残留」）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rcgen::{CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use time::{Duration, OffsetDateTime};
use tokio_rustls::rustls::{
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    ServerConfig,
};

/// 叶子证书 ServerConfig 缓存上限（Python 版为 50，内存内缓存无临时文件可放宽）
const LEAF_CACHE_MAX: usize = 512;

/// 证书序列号：纳秒时间戳 + 原子计数器（只需进程内唯一，无需密码学随机）
static SERIAL_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_serial() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (SERIAL_COUNTER.fetch_add(1, Ordering::Relaxed) << 24)
}

/// MITM 证书颁发机构：持有 CA 签发器，按域名在内存内签发叶子证书并缓存 ServerConfig。
/// 结构对齐 hudsucker 的 RcgenAuthority（叶子复用 CA 密钥对，属于 MITM 代理通行做法）。
pub struct CaAuthority {
    issuer: Issuer<'static, KeyPair>,
    provider: Arc<CryptoProvider>,
    cache: Mutex<HashMap<String, Arc<ServerConfig>>>,
}

impl CaAuthority {
    /// 按域名生成（或取缓存）TLS 服务端配置。叶子证书 CN/SAN=域名，有效期 10 年。
    pub fn gen_server_config(&self, host: &str) -> Arc<ServerConfig> {
        if let Some(cfg) = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(host) {
            return Arc::clone(cfg);
        }
        let cfg = Arc::new(self.build_server_config(host));
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= LEAF_CACHE_MAX {
            cache.clear();
        }
        cache.insert(host.to_string(), Arc::clone(&cfg));
        cfg
    }

    fn build_server_config(&self, host: &str) -> ServerConfig {
        let not_before = OffsetDateTime::now_utc() - Duration::days(1);
        // rcgen 0.14：new() 接受字符串 SAN，内部自动识别 IP 地址
        let mut params = CertificateParams::new(vec![host.to_string()])
            .expect("leaf certificate params");
        params.distinguished_name.push(DnType::CommonName, host);
        params.not_before = not_before;
        params.not_after = not_before + Duration::days(3650);
        params.is_ca = IsCa::NoCa;
        params.serial_number = Some(next_serial().into());
        // AKI：OpenSSL 3.2+ 严格校验非自签证书必须带 Authority Key Identifier
        params.use_authority_key_identifier_extension = true;

        let cert = params
            .signed_by(&self.issuer.key(), &self.issuer)
            .expect("failed to sign leaf certificate");
        let key_der = PrivatePkcs8KeyDer::from(self.issuer.key().serialize_der());

        let mut cfg = ServerConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(cert)], key_der.into())
            .expect("failed to build server config");
        // 仅广播 http/1.1：对齐 Python 版（未设置 ALPN，客户端回落 HTTP/1.1），
        // 避免引入 h2 分支后与请求日志/签到改写逻辑出现行为分叉
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        cfg
    }
}

/// 确保数据目录下存在可用 CA：已有则加载（兼容历史 RSA CA），缺失则生成并落盘。
/// 文件布局：`<certs_dir>/{ca.crt, ca.key, ca.cer}`（ca.cer 为 DER，供 certutil 安装；
/// 证书状态靠 CN 字符串 `TraeDeviceProxyCA` 匹配，**不可改名**）。
/// 返回供代理使用的签发器；仅需「证书文件存在」的调用方可忽略返回值。
pub fn ensure_ca(certs_dir: &std::path::Path) -> Result<CaAuthority, String> {
    sweep_legacy_leaf_files(certs_dir);
    let cert_pem_path = certs_dir.join("ca.crt");
    let key_pem_path = certs_dir.join("ca.key");
    let cer_der_path = certs_dir.join("ca.cer");

    std::fs::create_dir_all(certs_dir).map_err(|e| format!("创建证书目录失败: {e}"))?;

    let issuer = if cert_pem_path.exists() && key_pem_path.exists() {
        let cert_pem = std::fs::read_to_string(&cert_pem_path)
            .map_err(|e| format!("读取 CA 证书失败: {e}"))?;
        let key_pem = std::fs::read_to_string(&key_pem_path)
            .map_err(|e| format!("读取 CA 私钥失败: {e}"))?;
        let issuer = load_issuer(&cert_pem, &key_pem)?;
        // ca.cer 缺失则从 ca.crt(PEM) 补导出 DER：老版本/异常过程可能只留下
        // ca.crt+ca.key，certutil 安装依赖 ca.cer，缺失会在 UAC 后立即失败（闪退）
        if !cer_der_path.exists() {
            let der = pem_to_der(&cert_pem)?;
            std::fs::write(&cer_der_path, der).map_err(|e| format!("补写 ca.cer 失败: {e}"))?;
        }
        issuer
    } else {
        let issuer = generate_ca()?;
        // 先落盘再使用：ca.cer(DER) 供 certutil 安装，ca.crt/ca.key 供下次启动加载
        std::fs::write(&cert_pem_path, issuer.cert_pem.as_bytes())
            .map_err(|e| format!("写入 ca.crt 失败: {e}"))?;
        std::fs::write(&key_pem_path, issuer.key_pem.as_bytes())
            .map_err(|e| format!("写入 ca.key 失败: {e}"))?;
        std::fs::write(&cer_der_path, issuer.cert_der)
            .map_err(|e| format!("写入 ca.cer 失败: {e}"))?;
        harden_ca_dir(certs_dir);
        issuer.issuer
    };

    let provider = Arc::new(load_crypto_provider());
    Ok(CaAuthority { issuer, provider, cache: Mutex::new(HashMap::new()) })
}

struct GeneratedCa {
    issuer: Issuer<'static, KeyPair>,
    cert_pem: String,
    key_pem: String,
    cert_der: Vec<u8>,
}

/// 生成自签 CA（CN=`TraeDeviceProxyCA`，10 年有效期，ECDSA P-256）
fn generate_ca() -> Result<GeneratedCa, String> {
    let key_pair = KeyPair::generate().map_err(|e| format!("生成 CA 密钥失败: {e}"))?;
    // 私钥 PEM 必须在 key_pair 移交 Issuer 之前序列化
    let key_pem = key_pair.serialize_pem();
    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "TraeDeviceProxyCA");
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::days(1);
    params.not_after = now + Duration::days(3650);
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.serial_number = Some(next_serial().into());

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("自签 CA 失败: {e}"))?;
    let cert_pem = cert.pem();
    let cert_der = cert.der().to_vec();
    let issuer = Issuer::from_ca_cert_pem(&cert_pem, key_pair)
        .map_err(|e| format!("构建 CA 签发器失败: {e}"))?;
    Ok(GeneratedCa { issuer, cert_pem, key_pem, cert_der })
}

/// 从 PEM 加载已有 CA。
///
/// 历史 CA 私钥是 PKCS#1（`RSA PRIVATE KEY`），而 ring 后端只认 PKCS#8 ——
/// 这里先做一次 PKCS#1 → PKCS#8 包装再交给 rcgen（见模块头注释）。
fn load_issuer(cert_pem: &str, key_pem: &str) -> Result<Issuer<'static, KeyPair>, String> {
    let key_pem = pkcs1_pem_to_pkcs8_pem(key_pem)?;
    let key_pair = KeyPair::from_pem(&key_pem).map_err(|e| format!("解析 CA 私钥失败: {e}"))?;
    Issuer::from_ca_cert_pem(cert_pem, key_pair).map_err(|e| format!("解析 CA 证书失败: {e}"))
}

// ---------------------------------------------------------------------------
// 最小 PKCS#1 → PKCS#8 包装
// ---------------------------------------------------------------------------

/// PEM 主体 → DER（按标签提取 base64 并解码）。
fn pem_body_to_der(pem: &str, label: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let body = pem
        .split(begin.as_str())
        .nth(1)
        .and_then(|s| s.split(end.as_str()).next())?;
    let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(cleaned.as_bytes()).ok()
}

/// DER → PEM。
fn der_to_pem(der: &[u8], label: &str) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// DER 长度编码（短形式 / 长形式）。
fn der_len(n: usize) -> Vec<u8> {
    if n < 0x80 {
        return vec![n as u8];
    }
    let mut bytes = Vec::new();
    let mut v = n;
    while v > 0 {
        bytes.push((v & 0xff) as u8);
        v >>= 8;
    }
    bytes.reverse();
    let mut out = vec![0x80 | bytes.len() as u8];
    out.extend_from_slice(&bytes);
    out
}

/// TLV 拼接。
fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend_from_slice(&der_len(body.len()));
    out.extend_from_slice(body);
    out
}

/// PKCS#1 `RSAPrivateKey` → PKCS#8 `PrivateKeyInfo`（仅加外壳，密钥本体不动）。
///
/// PKCS#8 结构：
/// ```text
/// PrivateKeyInfo ::= SEQUENCE {
///   version                   INTEGER (0),
///   privateKeyAlgorithm       AlgorithmIdentifier { OID 1.2.840.113549.1.1.1, NULL },
///   privateKey                OCTET STRING  -- 里面就是 PKCS#1 的 DER
/// }
/// ```
fn pkcs1_der_to_pkcs8_der(pkcs1: &[u8]) -> Vec<u8> {
    // rsaEncryption OID 1.2.840.113549.1.1.1（内容字节，不含 tag/len）
    const RSA_OID_BODY: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
    let alg = der_tlv(
        0x30,
        &[
            der_tlv(0x06, RSA_OID_BODY),
            // parameters: NULL（RSA 的 AlgorithmIdentifier 必须带 NULL）
            der_tlv(0x05, &[]),
        ]
        .concat(),
    );
    let mut body = der_tlv(0x02, &[0x00]); // version = 0
    body.extend_from_slice(&alg);
    body.extend_from_slice(&der_tlv(0x04, pkcs1)); // privateKey OCTET STRING
    der_tlv(0x30, &body)
}

/// 私钥 PEM 规范化：`RSA PRIVATE KEY`（PKCS#1）包装成 `PRIVATE KEY`（PKCS#8），
/// 其余形态原样返回（已经是 PKCS#8 / EC 的交给 rcgen 自行处理）。
fn pkcs1_pem_to_pkcs8_pem(key_pem: &str) -> Result<String, String> {
    if !key_pem.contains("BEGIN RSA PRIVATE KEY") {
        return Ok(key_pem.to_string());
    }
    let der = pem_body_to_der(key_pem, "RSA PRIVATE KEY")
        .ok_or_else(|| "ca.key 缺少 RSA PRIVATE KEY PEM 块".to_string())?;
    Ok(der_to_pem(&pkcs1_der_to_pkcs8_der(&der), "PRIVATE KEY"))
}

/// PEM(CERTIFICATE) → DER：提取 base64 主体并解码（供补写 ca.cer）
fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    pem_body_to_der(pem, "CERTIFICATE").ok_or_else(|| "ca.crt 缺少 CERTIFICATE PEM 块".to_string())
}

/// rustls CryptoProvider：全进程共享单例（ring 后端，与 reqwest 的 rustls-tls 一致）
fn load_crypto_provider() -> CryptoProvider {
    // install_default 幂等：已被其他模块安装过则忽略
    let provider = tokio_rustls::rustls::crypto::ring::default_provider();
    let _ = provider.clone().install_default();
    provider
}

/// 启动时清扫历史残留：Python 版叶子证书临时文件（`leaf_*.crt/.key`）私钥曾落盘，全部删除
fn sweep_legacy_leaf_files(certs_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(certs_dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("leaf_") && (name.ends_with(".crt") || name.ends_with(".key")) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 收紧 CA 目录 ACL（仅 Windows，尽力而为）：移除继承、仅当前用户完全控制，
/// 防止同机低权限账户读取 CA 私钥。自验证失败自动 `/reset` 回滚（fail-open：
/// ACL 仅纵深防御，绝不能因此破坏代理自身的 CA 读写）。
fn harden_ca_dir(certs_dir: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let user = std::env::var("USERNAME").unwrap_or_default();
        if user.is_empty() {
            return;
        }
        let run = |args: &[&str]| {
            std::process::Command::new("icacls")
                .arg(certs_dir)
                .args(args)
                .creation_flags(0x08000000) // CREATE_NO_WINDOW
                .output()
        };
        // 记录收紧前已有文件：NTFS 动态继承下 /inheritance:r 移除目录可继承 ACE
        // 时，已有子文件的继承 ACE 会被同步清空（DACL 变空 → 连属主都拒绝访问）
        let existing: Vec<std::path::PathBuf> = certs_dir
            .read_dir()
            .map(|it| it.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        // grant 必须带 (OI)(CI) 继承标志：否则目录 DACL 无可继承 ACE，
        // 已有子文件继承 ACE 被动态清空、新建子文件依赖进程默认 DACL ——
        // 旧实现（无标志）正是用户「certutil 提权也读不到 ca.cer」的根因。
        // icacls 退出码必须检查：grant 侧失败（如用户名解析失败）而
        // /inheritance:r 已生效时，目录会变成空 DACL（protected + 零 ACE）
        let hardened = run(&["/inheritance:r", "/grant:r", &format!("{user}:(OI)(CI)F")])
            .map(|o| o.status.success())
            .unwrap_or(false);
        // 自验证双探针：① 收紧前已有的文件收紧后必须仍可读（能发现继承 ACE
        // 被动态清空的真实伤害）；② 目录下新建临时文件可读（校验未来子文件的
        // 继承行为）。任一失败立即 /reset 回滚 —— fail-open：ACL 仅纵深防御，
        // 绝不能因此破坏代理自身的 CA 读写
        let existing_ok = hardened && existing.iter().all(|p| std::fs::File::open(p).is_ok());
        let probe_ok = existing_ok && probe_new_file_readable(certs_dir);
        if !probe_ok {
            let _ = run(&["/reset", "/T"]);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = certs_dir;
    }
}

/// 在目录下新建临时文件并重读，验证子文件继承到的 ACL 允许当前用户读写；
/// 用于发现「目录 DACL 收紧后变空/丢失访问权」的坏状态。尽力而为。
#[cfg(target_os = "windows")]
fn probe_new_file_readable(dir: &std::path::Path) -> bool {
    let probe = dir.join(".acl_probe");
    let ok = std::fs::write(&probe, b"probe")
        .and_then(|_| std::fs::read(&probe).map(|d| d == b"probe"))
        .unwrap_or(false);
    let _ = std::fs::remove_file(&probe);
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ai_gateway_ca_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn missing_ca_cer_is_regenerated_from_pem() {
        let tmp = temp_dir("regen");
        let _ca = ensure_ca(&tmp).expect("ensure_ca should generate");
        assert!(tmp.join("ca.cer").exists());
        // 模拟老版本/异常过程只留下 ca.crt+ca.key：删除 ca.cer 后重载应自动补写
        std::fs::remove_file(tmp.join("ca.cer")).unwrap();
        let _ca2 = ensure_ca(&tmp).expect("reload");
        let der = std::fs::read(tmp.join("ca.cer")).unwrap();
        assert!(!der.is_empty());
        let pem = std::fs::read_to_string(tmp.join("ca.crt")).unwrap();
        assert_eq!(der, pem_to_der(&pem).unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn generated_ca_roundtrip_and_leaf() {
        let tmp = temp_dir("roundtrip");
        let ca = ensure_ca(&tmp).expect("ensure_ca should generate");
        // 文件三件套齐全
        assert!(tmp.join("ca.crt").exists());
        assert!(tmp.join("ca.key").exists());
        assert!(tmp.join("ca.cer").exists());
        // 再次加载：兼容自生成的 PEM
        let ca2 = ensure_ca(&tmp).expect("reload");
        let _ = ca2.gen_server_config("api.trae.cn");

        let cfg = ca.gen_server_config("api.trae.cn");
        assert!(!cfg.alpn_protocols.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 叶子证书缓存：同域名二次取用是同一个 Arc（不重复签发）
    #[test]
    fn leaf_config_is_cached_per_host() {
        let tmp = temp_dir("leafcache");
        let ca = ensure_ca(&tmp).expect("ensure_ca");
        let a = ca.gen_server_config("api.trae.cn");
        let b = ca.gen_server_config("api.trae.cn");
        let c = ca.gen_server_config("other.trae.cn");
        assert!(Arc::ptr_eq(&a, &b), "同域名应命中缓存");
        assert!(!Arc::ptr_eq(&a, &c), "不同域名应各自签发");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 历史残留的 Python 版叶子证书临时文件（私钥曾落盘）必须在启动时清扫
    #[test]
    fn legacy_leaf_files_are_swept() {
        let tmp = temp_dir("sweep");
        std::fs::write(tmp.join("leaf_api.trae.cn.crt"), b"x").unwrap();
        std::fs::write(tmp.join("leaf_api.trae.cn.key"), b"x").unwrap();
        std::fs::write(tmp.join("ca.crt"), b"keep").unwrap();
        sweep_legacy_leaf_files(&tmp);
        assert!(!tmp.join("leaf_api.trae.cn.crt").exists());
        assert!(!tmp.join("leaf_api.trae.cn.key").exists());
        assert!(tmp.join("ca.crt").exists(), "非 leaf_ 前缀的文件不得误删");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PEM ↔ DER 辅助：标签必须精确匹配（CERTIFICATE 不能吃掉 PRIVATE KEY）
    #[test]
    fn pem_body_extraction_is_label_scoped() {
        let cert_pem = der_to_pem(&[1, 2, 3, 4], "CERTIFICATE");
        let key_pem = der_to_pem(&[9, 9], "RSA PRIVATE KEY");
        assert_eq!(pem_body_to_der(&cert_pem, "CERTIFICATE").unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(pem_body_to_der(&key_pem, "RSA PRIVATE KEY").unwrap(), vec![9, 9]);
        assert!(pem_body_to_der(&cert_pem, "RSA PRIVATE KEY").is_none());
        assert!(pem_body_to_der("not a pem", "CERTIFICATE").is_none());
    }

    /// DER 长度编码：短形式（<128）与长形式（>=128）都要正确
    #[test]
    fn der_length_encoding() {
        assert_eq!(der_len(0), vec![0x00]);
        assert_eq!(der_len(127), vec![0x7f]);
        assert_eq!(der_len(128), vec![0x81, 0x80]);
        assert_eq!(der_len(300), vec![0x82, 0x01, 0x2c]);
    }

    /// PKCS#1 → PKCS#8：外壳必须是标准 PrivateKeyInfo（version=0 + rsaEncryption + OCTET STRING）
    #[test]
    fn pkcs1_wrapped_into_pkcs8_shell() {
        let pkcs1 = vec![0x30, 0x03, 0x02, 0x01, 0x00]; // 任意 PKCS#1 形态的字节串
        let pkcs8 = pkcs1_der_to_pkcs8_der(&pkcs1);
        // 外层 SEQUENCE
        assert_eq!(pkcs8[0], 0x30);
        // version INTEGER 0
        assert_eq!(&pkcs8[2..5], &[0x02, 0x01, 0x00]);
        // rsaEncryption OID 必须出现
        let oid = [0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
        assert!(
            pkcs8.windows(oid.len()).any(|w| w == oid),
            "缺少 rsaEncryption OID"
        );
        // 原始 PKCS#1 必须原样出现在 OCTET STRING 里（密钥本体不动）
        assert!(pkcs8.windows(pkcs1.len()).any(|w| w == pkcs1.as_slice()));
    }

    /// 非 PKCS#1 私钥必须原样透传（不做无谓改写，避免破坏 PKCS#8/EC 私钥）
    #[test]
    fn non_pkcs1_key_passes_through_untouched() {
        let pkcs8 = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n";
        assert_eq!(pkcs1_pem_to_pkcs8_pem(pkcs8).unwrap(), pkcs8);
    }

    /// 端到端：自签 CA 落盘 → 再加载 → 能签发叶子证书
    /// （这条用例覆盖 PKCS#1 兼容分支之外的主路径，回归「换后端导致 CA 读不出来」）
    #[test]
    fn reloaded_ca_can_sign_leaf() {
        let tmp = temp_dir("sign");
        ensure_ca(&tmp).expect("generate");
        let ca = ensure_ca(&tmp).expect("reload");
        let cfg = ca.gen_server_config("api.trae.cn");
        assert_eq!(cfg.alpn_protocols, vec![b"http/1.1".to_vec()]);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
