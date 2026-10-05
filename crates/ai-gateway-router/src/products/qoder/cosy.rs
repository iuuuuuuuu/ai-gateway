//! COSY 签名（RSA + AES-CBC + MD5）。
//!
//! # 算法三层
//!
//! ```text
//! ① cosy-key = base64(RSA_PKCS1v15(临时 AES 密钥))
//! ② info     = base64(AES-128-CBC(身份 JSON, key = iv = 临时密钥))
//! ③ 签名     = MD5(payloadB64 + "\n" + cosyKey + "\n" + date + "\n" + body + "\n" + pathSig)
//! Authorization: Bearer COSY.<payloadB64>.<签名>
//! ```
//!
//! # ⚠ 四个必须逐字对齐的细节（改任一个都 `101 Signature invalid`，实测）
//!
//! 1. 临时密钥是 **16 个 ASCII 字符**（[`crate::products::qoder::cred::hex_short`]），
//!    **不是** 16 个随机字节；
//! 2. 身份 JSON 必须**按 key 排序 + 无空白**（[`json_sorted_compact`]）；
//! 3. 签名里的 `pathSig` 是 URL path **去掉 `/algo` 前缀**；
//! 4. **GET 请求的 body 用空串签名**，不是 `"{}"`。
//!
//! # 有效性对照实验（2026-09-18）
//!
//! 签名完好 + 伪造 token → `403 {"code":"105","message":"Login expired"}`（过了签名关）；
//! signature 改坏 → `403 {"code":"101","message":"Signature invalid"}`；
//! cosy-key 改坏 → 同样 101；info 改坏 → 105。
//! ⇒ 损坏与完好响应不同，证明上游确实校验签名。
//!
//! # ⚠ 头清单必须逐项核对（血泪教训）
//!
//! 重写时去掉 uid 参数 ⇒ `cosy-user` 头丢失 ⇒ 上游回 **101 Signature invalid**
//!（不是"缺头"提示）。排查困难因为：签名算法逐字节正确、本地单测全绿
//!（只验算法不验头齐全）、错误码指向"签名"把人引向算法。
//! 结论：**与上游约定的头清单必须逐项核对**，见 [`CosySession::apply_headers`]。

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use aes::Aes128;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockEncryptMut, KeyIvInit};
use cbc::Encryptor;
use md5::{Digest, Md5};
use rsa::pkcs1v15::Pkcs1v15Encrypt;
use rsa::pkcs8::DecodePublicKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPublicKey;

use super::cred::{hex_short, json_sorted_compact, new_uuid, QoderCred};

type Aes128CbcEnc = Encryptor<Aes128>;

/// 内置 RSA 公钥 PEM（RSA 1024）。
///
/// 来源：Sliverkiss/qoderwork2api（MIT），实测 2026-09-18 仍有效。
/// **不要替换** —— 换错密钥会让每个请求都签名失败。
const SERVER_PUB_KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDA8iMH5c02LilrsERw9t6Pv5Nc\n\
4k6Pz1EaDicBMpdpxKduSZu5OANqUq8er4GM95omAGIOPOh+Nx0spthYA2BqGz+l\n\
6HRkPJ7S236FZz73In/KVuLnwI8JJ2CbuJap8kvheCCZpmAWpb/cPx/3Vr/J6I17\n\
XcW+ML9FoCI6AOvOzwIDAQAB\n\
-----END PUBLIC KEY-----\n";

/// 解析内置公钥；失败即 panic。
///
/// 与 Go 侧 `init()` 同口径：坏密钥会让**每个**请求都签名失败，而现象是
/// "账号用不了"，排查方向会被引向网络/凭证。直接 panic 更诚实。
fn server_pub_key() -> &'static RsaPublicKey {
    static KEY: OnceLock<RsaPublicKey> = OnceLock::new();
    KEY.get_or_init(|| {
        RsaPublicKey::from_public_key_pem(SERVER_PUB_KEY_PEM).unwrap_or_else(|e| {
            panic!("qoder: 内置 RSA 公钥 PEM 解析失败: {e}");
        })
    })
}

/// 内置公钥位数，供自检诊断。
///
/// Go 侧是导出 API（`ServerPubKeyBits`），被 `cosy_test.go` 与
/// `signature_live_test.go` 用作**自检**：位数不对说明 PEM 被换过，
/// 而现象是"每个请求都签名失败"。Rust 侧同样只被单测调用，故显式放行。
#[allow(dead_code)]
pub(crate) fn server_pub_key_bits() -> usize {
    server_pub_key().n().bits()
}

/// COSY 会话：一次请求签名所需的全部材料。
///
/// `temp_key` 只在构造期用于派生 `cosy_key` / `info`，之后不再使用；
/// 保留字段是为了对齐 Go 结构体并便于诊断。
///
/// 派生 `Debug` 与 Go 的 `%+v` 同口径：`temp_key` 会出现在调试输出里，
/// 但它是**每次请求现生成的临时 AES 密钥**（不是长期凭据），且只在签名
/// 过程中存活，故无需脱敏。
#[derive(Debug)]
pub(crate) struct CosySession {
    pub(crate) machine_id: String,
    pub(crate) machine_token: String,
    pub(crate) machine_type: String,
    #[allow(dead_code)]
    temp_key: Vec<u8>,
    pub(crate) cosy_key: String,
    pub(crate) info: String,
}

impl CosySession {
    /// 构造会话。
    ///
    /// `dt` / `drt` **显式传参而不是从 `cred` 读**：刷新后两者会变，
    /// 而签名里内嵌了它们，必须用最新值重建。
    pub(crate) fn new(cred: &QoderCred, dt: &str, drt: &str) -> Result<Self, String> {
        if cred.machine_id.is_empty() || cred.machine_token.is_empty() || cred.machine_type.is_empty()
        {
            return Err("缺少机器指纹（MachineID/MachineToken/MachineType 都必须有）".to_string());
        }

        // ★ 16 个 ASCII 字符，不是 16 个随机字节
        let temp_key = hex_short(16).into_bytes();

        let wrapped = server_pub_key()
            .encrypt(&mut rand::thread_rng(), Pkcs1v15Encrypt, &temp_key)
            .map_err(|e| format!("RSA 包裹临时密钥失败: {e}"))?;
        let cosy_key = B64.encode(&wrapped);

        // 身份 JSON：9 个键，BTreeMap 保证按 key 排序
        let mut identity: BTreeMap<String, String> = BTreeMap::new();
        identity.insert("name".into(), cred.nickname.clone());
        identity.insert("aid".into(), cred.uid.clone());
        identity.insert("uid".into(), cred.uid.clone());
        identity.insert("yx_uid".into(), String::new());
        identity.insert("organization_id".into(), String::new());
        identity.insert("organization_name".into(), String::new());
        identity.insert("user_type".into(), "personal_professional_trial".into());
        identity.insert("security_oauth_token".into(), dt.to_string());
        identity.insert("refresh_token".into(), drt.to_string());
        let info_plain = json_sorted_compact(&identity);

        let info_cipher =
            aes_cbc_encrypt(&info_plain, &temp_key).map_err(|e| format!("加密身份失败: {e}"))?;
        let info = B64.encode(&info_cipher);

        Ok(Self {
            machine_id: cred.machine_id.clone(),
            machine_token: cred.machine_token.clone(),
            machine_type: cred.machine_type.clone(),
            temp_key,
            cosy_key,
            info,
        })
    }

    /// 生成 `Authorization` 头的值。
    ///
    /// `body` 是**实际发送的字节**（GET 请求传空串）。
    pub(crate) fn auth_header(&self, body: &str, raw_url: &str) -> Result<String, String> {
        let mut payload: BTreeMap<String, String> = BTreeMap::new();
        payload.insert("cosyVersion".into(), "0.1.43".into());
        payload.insert("ideVersion".into(), String::new());
        payload.insert("info".into(), self.info.clone());
        payload.insert("requestId".into(), new_uuid());
        payload.insert("version".into(), "v1".into());
        let payload_b64 = B64.encode(json_sorted_compact(&payload));

        let u = url::Url::parse(raw_url).map_err(|e| format!("解析请求地址失败: {e}"))?;
        // ★ 去掉 `/algo` 前缀
        let path_sig = u.path().strip_prefix("/algo").unwrap_or(u.path());
        let date = now_unix().to_string();

        let sig_input = format!(
            "{}\n{}\n{}\n{}\n{}",
            payload_b64, self.cosy_key, date, body, path_sig
        );
        let sum = Md5::digest(sig_input.as_bytes());

        Ok(format!("Bearer COSY.{}.{}", payload_b64, hex::encode(sum)))
    }

    /// 把上游约定的**全部**头写进 `headers`。
    ///
    /// ⚠ 清单必须逐项齐全：漏掉 `cosy-user` 会让上游回 `101 Signature invalid`，
    /// 而错误码指向"签名"会把排查引向算法（算法其实是对的）。
    pub(crate) fn apply_headers(
        &self,
        headers: &mut reqwest::header::HeaderMap,
        body: &str,
        raw_url: &str,
        uid: &str,
        sse: bool,
        model_key: &str,
    ) -> Result<(), String> {
        let auth = self.auth_header(body, raw_url)?;
        let now = now_unix().to_string();

        let mut set = |k: &str, v: &str| {
            if let (Ok(name), Ok(val)) = (
                reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                reqwest::header::HeaderValue::from_str(v),
            ) {
                headers.insert(name, val);
            }
        };

        set("cosy-data-policy", "agree");
        set("content-type", "application/json");
        set("cosy-machinetype", &self.machine_type);
        set("cosy-clienttype", "5");
        set("cosy-date", &now);
        set("cosy-user", uid);
        set("cosy-key", &self.cosy_key);
        set("accept", "text/event-stream");
        if sse {
            set("cache-control", "no-cache");
        }
        set("cosy-clientip", "169.254.198.161");
        set("authorization", &auth);
        set("accept-encoding", "identity");
        set("cosy-version", "0.1.43");
        set("cosy-machineid", &self.machine_id);
        set("cosy-machinetoken", &self.machine_token);
        set("login-version", "v2");
        set("user-agent", &user_agent());
        // 官方推理协议业务头（来自上游 PR #3，注释称"缺失会产生差异"，
        // 我们未做 A/B 验证，但加上零风险）
        set("cosy-business-product", "cli");
        set("cosy-business-type", "agent");
        set("cosy-scene", "assistant");
        if !model_key.is_empty() {
            set("x-model-key", model_key);
            set("x-model-source", "system");
        }
        Ok(())
    }
}

/// 出站 User-Agent。
///
/// 默认值**刻意不改**：上游 PR #3 报告 Go 默认串会被反爬针对 SSE 长连接
/// "先放行再 reset"（`unexpected EOF`），但**我们未复现** ——
/// 不能拿正在工作的能力去赌一条逆向结论。留环境变量热覆盖。
pub(crate) fn user_agent() -> String {
    match std::env::var("QODER_USER_AGENT") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => "Go-http-client/2.0".to_string(),
    }
}

/// 当前 Unix 秒。
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// AES-128-CBC + PKCS7，key 与 iv **都是** `temp_key`。
///
/// 与 Go 侧 `aesCBCEncrypt` 逐字节等价（含"长度恰好是块整数倍时仍补一整块"）。
pub(crate) fn aes_cbc_encrypt(plain: &[u8], temp_key: &[u8]) -> Result<Vec<u8>, String> {
    let mut buf = plain.to_vec();
    let pt_len = buf.len();
    // PKCS7 最多补一整块，留足空间
    buf.resize(pt_len + 16, 0);
    let ct = Aes128CbcEnc::new_from_slices(temp_key, temp_key)
        .map_err(|e| format!("AES 密钥长度非法: {e}"))?
        .encrypt_padded_mut::<Pkcs7>(&mut buf, pt_len)
        .map_err(|e| format!("AES 填充失败: {e}"))?;
    Ok(ct.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::products::qoder::cred::QoderCred;
    use crate::products::qoder::region::QoderRegion;

    fn sample_cred() -> QoderCred {
        QoderCred {
            uid: "00000000-0000-4000-8000-000000000001".to_string(),
            nickname: "tester".to_string(),
            dt: "dt-token".to_string(),
            drt: "drt-token".to_string(),
            dt_expires_at: 0,
            machine_id: "11111111-1111-4111-8111-111111111111".to_string(),
            machine_token: "0123456789abcdef0123456789abcdef".to_string(),
            machine_type: "5".to_string(),
            region: QoderRegion::CN,
            no_route: false,
            file_path: String::new(),
        }
    }

    #[test]
    fn public_key_parses_and_is_1024_bits() {
        assert_eq!(server_pub_key_bits(), 1024);
    }

    #[test]
    fn session_requires_full_fingerprint() {
        let mut c = sample_cred();
        c.machine_id = String::new();
        let err = CosySession::new(&c, "dt", "drt").unwrap_err();
        assert!(err.contains("机器指纹"), "文案应指出机器指纹: {err}");

        c = sample_cred();
        c.machine_token = String::new();
        assert!(CosySession::new(&c, "dt", "drt").is_err());

        c = sample_cred();
        c.machine_type = String::new();
        assert!(CosySession::new(&c, "dt", "drt").is_err());
    }

    #[test]
    fn cosy_key_is_base64_of_rsa_block_and_info_is_base64() {
        let s = CosySession::new(&sample_cred(), "dt", "drt").unwrap();
        let key = B64.decode(s.cosy_key.as_bytes()).expect("cosy_key 应是 base64");
        assert_eq!(key.len(), 128, "RSA-1024 密文应当是 128 字节");
        let info = B64.decode(s.info.as_bytes()).expect("info 应是 base64");
        assert_eq!(info.len() % 16, 0, "AES-CBC 密文应当是块整数倍");
        assert_eq!(s.temp_key.len(), 16, "临时密钥是 16 个 ASCII 字符");
        assert!(
            s.temp_key.iter().all(|b| b.is_ascii_hexdigit()),
            "临时密钥必须全部是 ASCII 十六进制字符"
        );
    }

    #[test]
    fn identity_json_is_sorted_and_compact() {
        // 身份 JSON 的字节序列直接进 AES，排序/空白变了签名就变。
        let m: BTreeMap<String, String> = [
            ("uid", "u"),
            ("aid", "u"),
            ("name", "n"),
            ("yx_uid", ""),
            ("organization_id", ""),
            ("organization_name", ""),
            ("user_type", "personal_professional_trial"),
            ("security_oauth_token", "dt"),
            ("refresh_token", "drt"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let got = String::from_utf8(json_sorted_compact(&m)).unwrap();
        assert_eq!(
            got,
            "{\"aid\":\"u\",\"name\":\"n\",\"organization_id\":\"\",\"organization_name\":\"\",\
\"refresh_token\":\"drt\",\"security_oauth_token\":\"dt\",\"uid\":\"u\",\
\"user_type\":\"personal_professional_trial\",\"yx_uid\":\"\"}"
        );
        assert!(!got.contains(' '), "必须是紧凑形式（无空白）");
    }

    #[test]
    fn aes_cbc_matches_go_semantics_padding_always_adds_a_block() {
        // 长度恰好 16 字节时，PKCS7 仍要补满一整块 ⇒ 输出 32 字节。
        let key = b"0123456789abcdef";
        let out = aes_cbc_encrypt(&[0u8; 16], key).unwrap();
        assert_eq!(out.len(), 32);
        // 空输入也要补一整块
        assert_eq!(aes_cbc_encrypt(b"", key).unwrap().len(), 16);
        // 15 字节补 1 字节
        assert_eq!(aes_cbc_encrypt(&[0u8; 15], key).unwrap().len(), 16);
    }

    #[test]
    fn auth_header_shape_and_path_sig_strips_algo() {
        let s = CosySession::new(&sample_cred(), "dt", "drt").unwrap();
        let url = "https://gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation?Encode=1";
        let h = s.auth_header("BODY", url).unwrap();
        assert!(h.starts_with("Bearer COSY."), "头形如 Bearer COSY.x.y: {h}");
        let parts: Vec<&str> = h.trim_start_matches("Bearer COSY.").split('.').collect();
        assert_eq!(parts.len(), 2, "应当是 payload.签名 两段");
        assert_eq!(parts[1].len(), 32, "MD5 十六进制应当是 32 字符");
        // payload 里必须内嵌 info
        let payload: serde_json::Value =
            serde_json::from_slice(&B64.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(payload["info"], s.info);
        assert_eq!(payload["cosyVersion"], "0.1.43");
        assert_eq!(payload["version"], "v1");
        assert!(payload["requestId"].as_str().unwrap().len() == 36);
    }

    #[test]
    fn signature_covers_the_four_sensitive_parts() {
        let s = CosySession::new(&sample_cred(), "dt", "drt").unwrap();
        let url = "https://gateway.qoder.com.cn/algo/api/v2/model/list?Encode=1";
        let a = s.auth_header("", url).unwrap();
        let b = s.auth_header("", url).unwrap();
        // requestId 每次都变 ⇒ 签名必然不同（但格式必须一致）
        assert_ne!(a, b, "requestId 应当是随机的");
        assert_eq!(a.len(), b.len());
        // 同一 session 用不同 body 必须得到不同签名
        let c = s.auth_header("{}", url).unwrap();
        let sig_of = |h: &str| h.rsplit('.').next().unwrap().to_string();
        assert_ne!(sig_of(&a), sig_of(&c), "body 参与签名");
        // 不同 path 必须得到不同签名（pathSig 去掉 /algo 前缀）
        let d = s.auth_header("", "https://gateway.qoder.com.cn/algo/api/v1/userinfo").unwrap();
        assert_ne!(sig_of(&a), sig_of(&d), "path 参与签名");
    }

    #[test]
    fn apply_headers_contains_every_agreed_header() {
        let s = CosySession::new(&sample_cred(), "dt", "drt").unwrap();
        let mut h = reqwest::header::HeaderMap::new();
        s.apply_headers(
            &mut h,
            "BODY",
            "https://gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation?Encode=1",
            "00000000-0000-4000-8000-000000000001",
            true,
            "qmodel_preview",
        )
        .unwrap();

        // ⚠ 这份清单是"与上游约定"的完整清单，漏一项就可能 101 Signature invalid。
        let expected = [
            "cosy-data-policy",
            "content-type",
            "cosy-machinetype",
            "cosy-clienttype",
            "cosy-date",
            "cosy-user",
            "cosy-key",
            "accept",
            "cache-control",
            "cosy-clientip",
            "authorization",
            "accept-encoding",
            "cosy-version",
            "cosy-machineid",
            "cosy-machinetoken",
            "login-version",
            "user-agent",
            "cosy-business-product",
            "cosy-business-type",
            "cosy-scene",
            "x-model-key",
            "x-model-source",
        ];
        for k in expected {
            assert!(h.contains_key(k), "缺少约定头 {k}");
        }
        assert_eq!(h.get("cosy-user").unwrap(), "00000000-0000-4000-8000-000000000001");
        assert_eq!(h.get("cosy-clienttype").unwrap(), "5");
        assert_eq!(h.get("cosy-clientip").unwrap(), "169.254.198.161");
        assert_eq!(h.get("x-model-source").unwrap(), "system");
        assert!(h.get("authorization").unwrap().to_str().unwrap().starts_with("Bearer COSY."));
    }

    #[test]
    fn apply_headers_omits_model_headers_when_key_empty_and_cache_control_when_not_sse() {
        let s = CosySession::new(&sample_cred(), "dt", "drt").unwrap();
        let mut h = reqwest::header::HeaderMap::new();
        s.apply_headers(
            &mut h,
            "",
            "https://gateway.qoder.com.cn/algo/api/v2/model/list?Encode=1",
            "uid",
            false,
            "",
        )
        .unwrap();
        assert!(!h.contains_key("x-model-key"), "modelKey 为空时不应发 x-model-key");
        assert!(!h.contains_key("x-model-source"));
        assert!(!h.contains_key("cache-control"), "非 SSE 不发 cache-control");
    }

    #[test]
    fn user_agent_default_is_unchanged_go_string() {
        // 不设环境变量时必须仍是 Go 默认串（改了就是拿可用性赌一条未复现的逆向结论）
        std::env::remove_var("QODER_USER_AGENT");
        assert_eq!(user_agent(), "Go-http-client/2.0");
    }
}
