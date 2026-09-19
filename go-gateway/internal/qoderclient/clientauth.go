// Package qoderclient 读 **Qoder 客户端自己**的登录凭证，供一键导入。
//
// # 为什么做这个（以及它比网页授权好在哪）
//
// 所有者的反馈：「我用网页授权根本就不行，就无法登陆」。
//
// ## 网页授权为什么不行
//
// 授权链接是：
//
//	https://qoder.com.cn/device/selectAccounts?challenge=...&redirect_uri=qoder-work-cn%3A%2F%2F
//
// 那个 `redirect_uri` 是**自定义协议**（`qoder-work-cn://`），本机
// **没有注册这个协议**（我们不是 Qoder 客户端，不会去注册它）——
// 浏览器授权完成后无处回调。
//
// ⚠ 但要说清楚：我们的**轮询**端点形状是**对的**。实测拿会话里的
// nonce/verifier 去打 `/api/v1/deviceToken/poll` 得到
//
//	401 {"errorCode":"Unauthorized","errorMessage":"User not authenticated"}
//
// 那正是"**还没授权**"的预期响应（POST 才会 CSRFInvalid）。
// 也就是说：不是我们的请求写错了，而是**授权这一步在本机根本走不通**。
//
// ## 客户端凭证为什么可行
//
// Qoder 客户端**已经登录了**，它把登录态落在
// `%APPDATA%\com.qodercn.app.stable\auth.v1.dat`。
//
// 客户端源码（app.asar，逐字）：
//
//	class xte {
//	  constructor(t) { this.filePath = join(t, "auth.v1.dat") }
//	  load() {
//	    if (!existsSync(this.filePath) || !safeStorage.isEncryptionAvailable()) return null;
//	    const t = safeStorage.decryptString(readFileSync(this.filePath));
//	    const A = JSON.parse(t);
//	    return A.schemaVersion !== 1 || typeof A.token !== "string" || ...
//	  }
//	}
//
// 即 **Electron `safeStorage`**，Windows 上的两段式格式：
//
//	1. `Local State` → `os_crypt.encrypted_key` = base64("DPAPI" + DPAPI(AES-256 key))
//	2. `auth.v1.dat` → "v10" + nonce(12) + ciphertext + tag(16)，AES-256-GCM
//
// ## 我曾误判过一次（记录以免重犯）
//
// 我早期说它"是自定义加密、本机拿不到" —— **错的**。我当时拿 DPAPI 的
// GUID 头（`01000000d08c9ddf...`）去比 `auth.v1.dat` 的第 3 字节之后，
// 没匹配上就下了结论。实际上那个头只在**裸 DPAPI** 格式里出现，
// 而 v10 格式的载荷是 **AES-GCM 密文**，DPAPI 只用来保护密钥。
//
// 教训：说"拿不到"之前，先看**客户端自己的源码**怎么读它 ——
// 那里写着确切的 API（`safeStorage.decryptString`）。
//
// ## 边界
//
// 这是**用户自己机器上、用户自己的**凭证，用户明确要求"直接导入"。
// 密钥由 DPAPI 绑定当前 Windows 用户 —— 它挡的是别的用户，不是本机用户。
// 本包**只读**，绝不改动客户端文件。
package qoderclient

import (
	"crypto/aes"
	"crypto/cipher"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// ClientAuth 客户端 `auth.v1.dat` 解出来的登录态。
type ClientAuth struct {
	SchemaVersion int    `json:"schemaVersion"`
	Token         string `json:"token"`
	RefreshToken  string `json:"refreshToken"`
	ExpiresAt     string `json:"expiresAt"`
	User          struct {
		ID        string `json:"id"`
		Name      string `json:"name"`
		Email     string `json:"email"`
		Phone     string `json:"phone"`
		AvatarURL string `json:"avatarUrl"`
	} `json:"user"`
}

// 客户端的应用数据目录（候选）。
//
// 实测本机是 `com.qodercn.app.stable`（CN 稳定版）。国际版/其它渠道
// 的名字不同，故列多个 —— 只认一个会在别的版本上静默扫不到。
func appDirs() []string {
	appdata := os.Getenv("APPDATA")
	if appdata == "" {
		return nil
	}
	names := []string{
		// 实测（本机）
		"com.qodercn.app.stable",
		// 其它可能的渠道（保守列出，不存在就跳过）
		"com.qoder.app.stable",
		"com.qodercn.app",
		"com.qoder.app",
		"Qoder",
		"QoderCN",
	}
	out := make([]string, 0, len(names))
	for _, n := range names {
		out = append(out, filepath.Join(appdata, n))
	}
	return out
}

// FindClientAppDir 返回第一个**同时有** Local State 与 auth.v1.dat 的目录。
func FindClientAppDir() (string, bool) {
	for _, d := range appDirs() {
		if fileExists(filepath.Join(d, "Local State")) && fileExists(filepath.Join(d, "auth.v1.dat")) {
			return d, true
		}
	}
	return "", false
}

// ReadClientAuth 读并解密客户端的登录态。
//
// 任何一步失败都返回 error（而不是空结构）—— 调用方据此给用户
// **准确的**原因，而不是笼统的"读不到"。
func ReadClientAuth() (*ClientAuth, error) {
	dir, ok := FindClientAppDir()
	if !ok {
		return nil, fmt.Errorf("找不到 Qoder 客户端的登录数据（请先在客户端里登录一次）")
	}
	return ReadClientAuthFrom(dir)
}

// ReadClientAuthFrom 从指定目录读（可测试）。
func ReadClientAuthFrom(dir string) (*ClientAuth, error) {
	// ---- 1. 取 AES 密钥 ----
	lsRaw, err := os.ReadFile(filepath.Join(dir, "Local State"))
	if err != nil {
		return nil, fmt.Errorf("读 Local State 失败: %w", err)
	}
	var ls struct {
		OSCrypt struct {
			EncryptedKey string `json:"encrypted_key"`
		} `json:"os_crypt"`
	}
	if err := json.Unmarshal(lsRaw, &ls); err != nil {
		return nil, fmt.Errorf("Local State 不是合法 JSON: %w", err)
	}
	if ls.OSCrypt.EncryptedKey == "" {
		return nil, fmt.Errorf("Local State 里没有 os_crypt.encrypted_key（客户端可能还没登录过）")
	}

	encKey, err := base64.StdEncoding.DecodeString(ls.OSCrypt.EncryptedKey)
	if err != nil {
		return nil, fmt.Errorf("encrypted_key 不是合法 base64: %w", err)
	}
	// Electron 在密钥前加了字面量 "DPAPI"
	if len(encKey) < 5 || string(encKey[:5]) != "DPAPI" {
		return nil, fmt.Errorf("encrypted_key 缺少 DPAPI 前缀（长度 %d）", len(encKey))
	}
	aesKey, err := dpapiUnprotect(encKey[5:])
	if err != nil {
		return nil, fmt.Errorf("DPAPI 解出 AES 密钥失败: %w", err)
	}
	if len(aesKey) != 32 {
		return nil, fmt.Errorf("AES 密钥长度是 %d（应为 32）", len(aesKey))
	}

	// ---- 2. 解 auth.v1.dat ----
	blob, err := os.ReadFile(filepath.Join(dir, "auth.v1.dat"))
	if err != nil {
		return nil, fmt.Errorf("读 auth.v1.dat 失败: %w", err)
	}
	if len(blob) < 3 || string(blob[:3]) != "v10" {
		return nil, fmt.Errorf("auth.v1.dat 前缀不是 v10（实际 %q）", safePrefix(blob, 3))
	}
	body := blob[3:]
	if len(body) < 12+16 {
		return nil, fmt.Errorf("auth.v1.dat 太短（%d 字节）", len(body))
	}

	nonce := body[:12]
	tag := body[len(body)-16:]
	ct := body[12 : len(body)-16]

	block, err := aes.NewCipher(aesKey)
	if err != nil {
		return nil, fmt.Errorf("构造 AES 失败: %w", err)
	}
	gcm, err := cipher.NewGCM(block)
	if err != nil {
		return nil, fmt.Errorf("构造 GCM 失败: %w", err)
	}
	plain, err := gcm.Open(nil, nonce, append(append([]byte{}, ct...), tag...), nil)
	if err != nil {
		return nil, fmt.Errorf("AES-GCM 解密失败（密钥不匹配或数据损坏）: %w", err)
	}

	var auth ClientAuth
	if err := json.Unmarshal(plain, &auth); err != nil {
		return nil, fmt.Errorf("解密结果不是合法 JSON: %w", err)
	}
	if auth.Token == "" {
		return nil, fmt.Errorf("解密结果里没有 token 字段")
	}
	return &auth, nil
}

// MachineID 读客户端的机器标识（COSY 签名需要）。
//
// 客户端把它放在 `auth.machine-id`（实测是本机 UUID）。
// 缺失时返回空串 —— 调用方可以自行生成一个。
func MachineID(dir string) string {
	b, err := os.ReadFile(filepath.Join(dir, "auth.machine-id"))
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(b))
}

func fileExists(p string) bool {
	st, err := os.Stat(p)
	return err == nil && !st.IsDir()
}

func safePrefix(b []byte, n int) string {
	if len(b) < n {
		n = len(b)
	}
	return string(b[:n])
}

// Region 由应用目录推断（CN 渠道 vs 国际版）。
//
// 判据用目录名里的 `cn`：`com.qodercn.app.stable` → CN。
// 猜错会让请求打到错误端点，故调用方应让用户能改。
func Region(dir string) string {
	if strings.Contains(strings.ToLower(filepath.Base(dir)), "cn") {
		return "cn"
	}
	return "intl"
}
