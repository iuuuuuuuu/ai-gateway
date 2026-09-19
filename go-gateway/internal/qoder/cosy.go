package qoder

// cosy.go 实现 Qoder 的 COSY 请求签名。
//
// 算法（三层，缺一不可）：
//
//	① cosy-key = base64(RSA_PKCS1v15(临时 AES 密钥))
//	② info     = base64(AES-128-CBC(身份 JSON, key=iv=临时密钥))
//	③ 签名      = MD5(payloadB64 + "\n" + cosyKey + "\n" + date + "\n" + body + "\n" + pathSig)
//	Authorization: Bearer COSY.<payloadB64>.<签名>
//
// **四个必须逐字对齐的细节**（改任何一个都会 101 Signature invalid，实测）：
//
//	1. 临时密钥是 **16 个 ASCII 字符**（hex[:16]），不是 16 个随机字节；
//	2. 身份 JSON 必须**按 key 排序 + 无空白**序列化；
//	3. 签名里的 pathSig 是 URL path **去掉 /algo 前缀**；
//	4. GET 请求的 body 用**空串**签名，不是 "{}"。
//
// 有效性验证（2026-09-18 对照实验，见 uitest/probe-qoder-signature-control.cjs）：
//
//	签名完好 + 伪造 token → 403 {"code":"105","message":"Login expired"}  ← 过了签名关
//	signature 改坏         → 403 {"code":"101","message":"Signature invalid"}
//	cosy-key  改坏         → 403 {"code":"101","message":"Signature invalid"}
//	info      改坏         → 403 {"code":"105","message":"Login expired"}
//
// 损坏与完好得到**不同**响应 ⇒ 上游确实在校验签名，且本算法有效。
// 若哪天这个探测脚本开始报 101，说明上游改了签名规则，需要重新逆向。

import (
	"crypto/aes"
	"crypto/cipher"
	"crypto/md5"
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"net/http"
	"net/url"
	"os"
	"sort"
	"strings"
	"time"
)

// serverPubKeyPEM Qoder 桌面客户端硬编码的 RSA 公钥。
//
// 来源：Sliverkiss/qoderwork2api internal/upstream/cosy.go（MIT）。
// **不要随意替换** —— 它必须与上游当前的私钥配对，换错会导致全部签名失败。
// 实测（2026-09-18）该公钥仍有效。
const serverPubKeyPEM = `-----BEGIN PUBLIC KEY-----
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDA8iMH5c02LilrsERw9t6Pv5Nc
4k6Pz1EaDicBMpdpxKduSZu5OANqUq8er4GM95omAGIOPOh+Nx0spthYA2BqGz+l
6HRkPJ7S236FZz73In/KVuLnwI8JJ2CbuJap8kvheCCZpmAWpb/cPx/3Vr/J6I17
XcW+ML9FoCI6AOvOzwIDAQAB
-----END PUBLIC KEY-----`

var serverPubKey *rsa.PublicKey

func init() {
	block, _ := pem.Decode([]byte(serverPubKeyPEM))
	if block == nil {
		// 编译期常量出错属于程序缺陷，直接 panic 比带着坏密钥运行好 ——
		// 后者会让每个请求都签名失败，排查方向会被引向网络/凭证。
		panic("qoder: 内置 RSA 公钥 PEM 解析失败")
	}
	k, err := x509.ParsePKIXPublicKey(block.Bytes)
	if err != nil {
		panic("qoder: 内置 RSA 公钥解析失败: " + err.Error())
	}
	pk, ok := k.(*rsa.PublicKey)
	if !ok {
		panic("qoder: 内置公钥不是 RSA")
	}
	serverPubKey = pk
}

// ServerPubKeyBits 返回内置公钥的模数位数，供自检与诊断使用。
func ServerPubKeyBits() int { return serverPubKey.N.BitLen() }

// CosySession 一次签名会话（每账号每请求重建；DT/DRT 变化时必须重建）。
type CosySession struct {
	MachineID    string
	MachineToken string
	MachineType  string

	// tempKey 16 字节 ASCII（AES-128 密钥，同时用作 IV）。
	tempKey []byte
	// cosyKey base64(RSA(tempKey))。
	cosyKey string
	// info base64(AES-CBC(身份 JSON))。
	info string
}

// NewCosySession 用凭证构建签名会话。
//
// dt/drt 显式传入而不是从 Cred 读：刷新后两者会变，而签名里内嵌了它们
// （身份 JSON 的 security_oauth_token / refresh_token），必须用**最新值**重建。
func NewCosySession(c *Cred, dt, drt string) (*CosySession, error) {
	if c == nil {
		return nil, fmt.Errorf("凭证为空")
	}
	if c.MachineID == "" || c.MachineToken == "" || c.MachineType == "" {
		return nil, fmt.Errorf("缺少机器指纹（MachineID/MachineToken/MachineType 都必须有）")
	}

	// 细节 ①：16 个 ASCII 字符，不是 16 个随机字节。
	tempKey := []byte(hexShort(16))

	wrapped, err := rsa.EncryptPKCS1v15(rand.Reader, serverPubKey, tempKey)
	if err != nil {
		return nil, fmt.Errorf("RSA 包裹临时密钥失败: %w", err)
	}
	cosyKey := base64.StdEncoding.EncodeToString(wrapped)

	identity := map[string]string{
		"name":                 c.Nickname,
		"aid":                  c.UID,
		"uid":                  c.UID,
		"yx_uid":               "",
		"organization_id":      "",
		"organization_name":    "",
		"user_type":            "personal_professional_trial",
		"security_oauth_token": dt,
		"refresh_token":        drt,
	}
	// 细节 ②：按 key 排序 + 无空白。
	infoPlain := jsonSortedCompact(identity)

	infoCipher, err := aesCBCEncrypt(infoPlain, tempKey)
	if err != nil {
		return nil, fmt.Errorf("加密身份失败: %w", err)
	}

	return &CosySession{
		MachineID:    c.MachineID,
		MachineToken: c.MachineToken,
		MachineType:  c.MachineType,
		tempKey:      tempKey,
		cosyKey:      cosyKey,
		info:         base64.StdEncoding.EncodeToString(infoCipher),
	}, nil
}

// jsonSortedCompact 按 key 排序、无空白序列化（签名对字节敏感，不能用 json.Marshal）。
func jsonSortedCompact(m map[string]string) []byte {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	var sb strings.Builder
	sb.WriteByte('{')
	for i, k := range keys {
		if i > 0 {
			sb.WriteByte(',')
		}
		kb, _ := json.Marshal(k)
		vb, _ := json.Marshal(m[k])
		sb.Write(kb)
		sb.WriteByte(':')
		sb.Write(vb)
	}
	sb.WriteByte('}')
	return []byte(sb.String())
}

// aesCBCEncrypt AES-128-CBC，key = iv = tempKey，PKCS7 padding。
func aesCBCEncrypt(plain, tempKey []byte) ([]byte, error) {
	block, err := aes.NewCipher(tempKey)
	if err != nil {
		return nil, err
	}
	padLen := aes.BlockSize - len(plain)%aes.BlockSize
	padded := make([]byte, len(plain)+padLen)
	copy(padded, plain)
	for i := len(plain); i < len(padded); i++ {
		padded[i] = byte(padLen)
	}
	out := make([]byte, len(padded))
	cipher.NewCBCEncrypter(block, tempKey).CryptBlocks(out, padded)
	return out, nil
}

// AuthHeader 计算单次请求的 Authorization 头。
func (s *CosySession) AuthHeader(body, rawURL string) (string, error) {
	payload := map[string]string{
		"cosyVersion": "0.1.43",
		"ideVersion":  "",
		"info":        s.info,
		"requestId":   NewUUID(),
		"version":     "v1",
	}
	payloadB64 := base64.StdEncoding.EncodeToString(jsonSortedCompact(payload))

	u, err := url.Parse(rawURL)
	if err != nil {
		return "", fmt.Errorf("解析请求地址失败: %w", err)
	}
	// 细节 ③：去掉 /algo 前缀。
	pathSig := strings.TrimPrefix(u.Path, "/algo")
	date := fmt.Sprintf("%d", time.Now().Unix())

	sigInput := payloadB64 + "\n" + s.cosyKey + "\n" + date + "\n" + body + "\n" + pathSig
	sum := md5.Sum([]byte(sigInput))
	return "Bearer COSY." + payloadB64 + "." + hex.EncodeToString(sum[:]), nil
}

// ApplyHeaders 把 COSY 要求的全部请求头写到 req 上。
//
// uid 单独传参（而不是从别处取）是照搬参考实现的接口形状 —— 它在
// ApplyHeaders(req, body, rawURL, uid, sse, modelKey) 里显式接收 uid，
// 因为 `cosy-user` 头必须与签名里 identity 的 uid **一致**。
//
// ⚠ 曾经踩过的坑：我在重写时把 uid 参数去掉了，于是 `cosy-user` 头丢失，
// 上游回 **101 Signature invalid**（而不是提示"缺头"）。
// 排查花了很久，因为：
//   · 签名算法本身完全正确（payload/info/签名逐字节比对一致）；
//   · 本地单元测试全绿（我们只验自己的算法，不验头是否齐全）；
//   · 错误码指向"签名"而非"缺头"，把人引向算法。
// 结论：**与上游约定的头清单必须逐项核对**，并用真实请求验证
//（见 signature_live_test.go 的 TestLiveSignatureValidity）。
//
// body 必须与实际发送的字节**完全一致**（签名覆盖它）；
// GET 请求传空串（细节 ④）。
//
// sse 只影响 cache-control；accept 恒为 text/event-stream（与客户端插件一致，
// 实测改成 application/json 会改变上游行为）。
func (s *CosySession) ApplyHeaders(req *http.Request, body, rawURL, uid string, sse bool, modelKey string) error {
	auth, err := s.AuthHeader(body, rawURL)
	if err != nil {
		return err
	}
	h := req.Header
	h.Set("cosy-data-policy", "agree")
	h.Set("content-type", "application/json")
	h.Set("cosy-machinetype", s.MachineType)
	h.Set("cosy-clienttype", "5")
	h.Set("cosy-date", fmt.Sprintf("%d", time.Now().Unix()))
	// cosy-user 是必填头：缺失会让上游判 101 Signature invalid（见上面的踩坑记录）。
	h.Set("cosy-user", uid)
	h.Set("cosy-key", s.cosyKey)
	h.Set("accept", "text/event-stream")
	if sse {
		h.Set("cache-control", "no-cache")
	}
	h.Set("cosy-clientip", "169.254.198.161")
	h.Set("authorization", auth)
	h.Set("accept-encoding", "identity")
	h.Set("cosy-version", "0.1.43")
	h.Set("cosy-machineid", s.MachineID)
	h.Set("cosy-machinetoken", s.MachineToken)
	h.Set("login-version", "v2")
	h.Set("user-agent", userAgent())
	// ---- 官方推理请求协议要求的业务头 ----
	//
	// 来自上游 qoderwork2api 的 PR #3（`internal/upstream/cosy.go`），其注释称
	// 这三个头是「官方推理请求协议要求（非 work 模式默认值），**缺失会产生差异**」。
	//
	// ⚠ 这是对方的逆向结论，我们**未做 A/B 验证**。但它们零风险：
	// 缺失时我们本来就是"不发的状态"，补上只会更接近官方客户端。
	h.Set("cosy-business-product", "cli")
	h.Set("cosy-business-type", "agent")
	h.Set("cosy-scene", "assistant")
	if modelKey != "" {
		h.Set("x-model-key", modelKey)
		h.Set("x-model-source", "system")
	}
	return nil
}

// userAgent 返回 Cosy 请求使用的 User-Agent。
//
// # 为什么做成可覆盖的（上游发现的坑，但我们尚未复现）
//
// 上游 qoderwork2api 的 PR #3 报告：Go 默认串 `Go-http-client/*` 会被
// 反爬**针对 SSE 长连接先放行再 reset**（表现为 `unexpected EOF`），
// 且拦截针对的是**该特定串**而非"工具 UA"大类 —— 故他们默认改成
// 官方 Node 客户端的 `node`，并留环境变量热覆盖。
//
// # ⚠ 我们为什么不直接把默认值改掉
//
// 我们的 Qoder 对话**目前是正常的**（实测能拿到内容），并未复现该现象。
// 在没复现、也没做过 A/B 的前提下改默认 UA，是拿**正在工作**的能力
// 去赌一条逆向结论 —— 万一对方的环境与我们的上游路由不同，改完反而
// 从"能用"变"不能用"。
//
// 故：**保持现有默认值不变**，但提供环境变量开关 —— 这样一旦真的
// 遇到 SSE 被 reset，改一个环境变量即可验证，无需重新编译。
//
// 验证方法（若将来出现 unexpected EOF）：
//
//	$env:QODER_USER_AGENT = "node"; 重启网关 → 观察是否消失
func userAgent() string {
	if ua := strings.TrimSpace(os.Getenv("QODER_USER_AGENT")); ua != "" {
		return ua
	}
	return "Go-http-client/2.0"
}
