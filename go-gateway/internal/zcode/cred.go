// Package zcode 实现 ZCode（Z.AI / 智谱 GLM 编码套餐）的账号凭证与上游调用。
//
// 参考实现：TriDefender/zcode-api（TypeScript/Bun，MIT）。
// 本包在其基础上做三处**刻意的简化**（都有实测依据，见各注释）：
//
//  1. **不实现客户端签名**（Client Request Signing V4，Ed25519 + 工作量证明
//     + KDF 派生）。实测证明上游不校验签名头 —— 见 signing.go 的详细说明。
//     这省掉参考实现里约 600 行密码学逻辑。
//  2. **只走上游的 OpenAI 端点**，不做 Anthropic 翻译。网关内部就是 OpenAI
//     格式，走 OpenAI 端点 = 零翻译。
//  3. **不做协议转换**（OpenAI ↔ Anthropic ↔ Responses）—— 网关已有，
//     且我们只用一个端点。
//
// ## 与另两个产品的差异（决定了它必须独立成包）
//
//	              WorkBuddy        Qoder                ZCode
//	────────────  ───────────────  ───────────────────  ─────────────────────
//	鉴权          Bearer token     COSY 签名            Bearer {key}.{secret}
//	刷新          refreshToken     deviceToken/refresh  **无**（凭证长期有效）
//	请求格式      OpenAI           Qoder 私有           **OpenAI**（零翻译）
//	响应形状      OpenAI           嵌套 SSE             **OpenAI**（零翻译）
//	区域/服务商   国服/国际版       国服/国际版          **Z.AI / 智谱**（两服务商）
//	机器指纹      无               MachineID+Token      deviceMid（仅伪装用）
package zcode

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// Cred 一个 ZCode 账号的凭证。
type Cred struct {
	// UID 本地账号标识。
	//
	// 为什么需要它：上游的凭证是 `{apiKey}.{secret}` 字符串，**没有账号 ID**
	// （不像 Qoder 有 user_id）。但账号池需要一个稳定的主键来跟踪冷却、额度、
	// 到期等状态，故本地生成一个。
	//
	// 生成规则见 CredKey：用 apiKeyId 的哈希前 12 位 —— 这样同一个凭证
	// 重复导入会得到**相同**的 UID（幂等），而不是每次都新增一个账号。
	UID string

	// Nickname 昵称（用户可改）。
	Nickname string

	// Credential 完整凭证字符串，形如 `{apiKey}.{secret}` 或 `{apiKey}`。
	//
	// 刻意**不做结构化拆分**（见 design.md D4）：
	//   · 上游要的就是这个字符串；
	//   · 智谱的 secret 是**可选**的（单段也合法），结构化存储要处理"有/无"；
	//   · 只有签名逻辑才需要拆分，而我们不实现签名。
	Credential string

	// Provider 服务商（"zai" / "bigmodel"）。
	Provider Provider

	// JWT start-plan 令牌（OAuth 登录时上游一并返回）。
	//
	// 用途：**额度查询**用它（不是 Credential）。参考实现明确写了这一点。
	// 若用户只导入了 Credential（方式 B），则 JWT 为空 → 额度查不到，
	// 界面应显示"未知"而不是 0（0 会被误读成"额度耗尽"）。
	JWT string

	// JWTIssuedAt JWT 的签发时刻（Unix 秒）；0 = 未知。
	//
	// 参考实现说明：这个 JWT **没有 exp 字段**、不因时间过期 ——
	// 8 天前签发的仍能查额度。只有上游回 401/3012 才表示需要重新登录。
	// 故这里只记录年龄供展示，**不据此判定过期**。
	JWTIssuedAt int64

	// FilePath 来源文件路径；保存时写回此处。
	FilePath string
}

// CredKey 从凭证字符串派生稳定的账号 UID。
//
// 为什么需要：上游凭证没有账号 ID，但账号池需要稳定主键。
//
// 为什么用哈希而不是直接用凭证：凭证是**秘密**，不能出现在日志、状态文件、
// 界面 URL 里。哈希后既稳定（同一凭证 → 同一 UID，重复导入幂等）又不可逆。
//
// 取 12 位十六进制（48 位）：碰撞概率在"一个人几十个账号"的量级下可忽略，
// 又足够短便于界面展示与排障。
func CredKey(credential string) string {
	return "zcode-" + shortHash(strings.TrimSpace(credential))
}

// shortHash 对字符串做稳定短哈希（FNV-1a 64 位，取 12 位十六进制）。
//
// 为什么不用 crypto/sha256：这里**不需要密码学强度**（不是防碰撞攻击，
// 只是要一个稳定的短标识），而 FNV 更简单、无依赖、够快。
// 若将来需要防恶意构造（有人故意造碰撞来顶掉别人的账号），再换 SHA-256。
func shortHash(s string) string {
	const (
		offset64 = 14695981039346656037
		prime64  = 1099511628211
	)
	var h uint64 = offset64
	for i := 0; i < len(s); i++ {
		h ^= uint64(s[i])
		h *= prime64
	}
	const hexDigits = "0123456789abcdef"
	out := make([]byte, 12)
	for i := 11; i >= 0; i-- {
		out[i] = hexDigits[h&0xf]
		h >>= 4
	}
	return string(out)
}

// Parse 解析凭证字符串（用户粘贴的形态）。
//
// 接受的形态：
//
//	"{apiKey}.{secret}"   两段（Z.AI 必需；智谱也可）
//	"{apiKey}"            单段（智谱的 secret 可选）
//
// 拒绝的形态：空、纯空白、含换行（粘贴时常带上）。
//
// 为什么**不校验格式细节**（如长度、字符集）：上游的凭证形态由上游决定，
// 我们猜错会让合法凭证被拒。宁可放行让上游去拒（它的错误信息更权威）。
func Parse(credential string) (*Cred, error) {
	// 粘贴常带首尾空白与换行 —— 先清理，否则会拼出坏请求头。
	c := strings.TrimSpace(credential)
	// 去掉所有换行（多行粘贴的情况）
	c = strings.ReplaceAll(c, "\r", "")
	c = strings.ReplaceAll(c, "\n", "")
	c = strings.TrimSpace(c)

	if c == "" {
		return nil, fmt.Errorf("凭证为空")
	}
	// 凭证里不该有空格 —— 有的话多半是粘贴了多余内容（如 "apiKey: xxx"）
	if strings.ContainsAny(c, " \t") {
		return nil, fmt.Errorf("凭证里含空格，请只粘贴凭证本身（形如 apiKey.secret）")
	}

	return &Cred{
		UID:        CredKey(c),
		Credential: c,
		Provider:   ProviderUnknown, // 由调用方指定或按域名推断
	}, nil
}

// LoadFile 从磁盘加载 ZCode 凭证文件。
//
// 兼容三种形态：
//
//	嵌套形（宿主落盘）  {"credential":"...","provider":"zai","jwt":"...","nickname":"..."}
//	扁平形（手写）      {"apiKey":"...","secret":"..."} 或 {"credential":"..."}
//	裸字符串文件        {"...key.secret..."}（仅一个 JSON 字符串）
func LoadFile(path string) (*Cred, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	c, err := parseBytes(raw, path)
	if err != nil {
		return nil, err
	}
	c.FilePath = path
	if c.UID == "" {
		c.UID = CredKey(c.Credential)
	}
	return c, nil
}

// parseBytes 解析凭证字节；path 仅用于错误信息与文件名兜底。
func parseBytes(raw []byte, path string) (*Cred, error) {
	if len(raw) == 0 {
		return nil, fmt.Errorf("凭证文件为空: %s", filepath.Base(path))
	}

	// 形态三：整个文件是一个 JSON 字符串
	var bare string
	if err := json.Unmarshal(raw, &bare); err == nil && strings.TrimSpace(bare) != "" {
		c, err := Parse(bare)
		if err != nil {
			return nil, err
		}
		return c, nil
	}

	var doc map[string]json.RawMessage
	if err := json.Unmarshal(raw, &doc); err != nil {
		return nil, fmt.Errorf("凭证不是合法 JSON: %w", err)
	}

	c := &Cred{}

	// 先处理 {apiKey, secret} **分成两个字段**的情况 —— 必须在读
	// `api_key` 单字段之前，否则会先拿到 apiKey、把 secret 丢掉
	// （实测踩到：`{"api_key":"k2","secret":"s2"}` 被解析成 "k2"）。
	key := firstNonEmpty(rawStr(doc, "api_key_id"), rawStr(doc, "apiKeyId"), rawStr(doc, "key"))
	secret := firstNonEmpty(rawStr(doc, "api_key_secret"), rawStr(doc, "apiKeySecret"), rawStr(doc, "secret"))
	// `api_key` 字段本身可能是"完整的凭证"（含点）也可能是"只有 key 部分"。
	// 判别依据：含点 = 完整凭证；不含点 = 只有 key。
	apiKeyField := firstNonEmpty(rawStr(doc, "api_key"), rawStr(doc, "apiKey"))
	if apiKeyField != "" {
		if strings.Contains(apiKeyField, ".") {
			// 完整凭证，直接用
			c.Credential = apiKeyField
		} else if key == "" {
			key = apiKeyField
		}
	}
	if c.Credential == "" {
		if key != "" && secret != "" {
			c.Credential = key + "." + secret
		} else if key != "" {
			c.Credential = key
		}
	}
	// `credential` 字段优先级最高（宿主落盘用的就是它）
	if v := rawStr(doc, "credential"); v != "" {
		c.Credential = v
	}
	// 嵌套形：{"auth":{"credential":"..."}}
	if c.Credential == "" {
		if authRaw, ok := doc["auth"]; ok {
			var auth map[string]json.RawMessage
			if json.Unmarshal(authRaw, &auth) == nil {
				c.Credential = firstNonEmpty(rawStr(auth, "credential"), rawStr(auth, "apiKey"), rawStr(auth, "api_key"))
			}
		}
	}
	if strings.TrimSpace(c.Credential) == "" {
		return nil, fmt.Errorf("凭证文件里找不到凭证字段（支持 credential / apiKey / api_key 等）: %s", filepath.Base(path))
	}
	// 清理（与 Parse 同一口径）
	cleaned, err := Parse(c.Credential)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", filepath.Base(path), err)
	}
	c.Credential = cleaned.Credential

	c.UID = firstNonEmpty(rawStr(doc, "uid"), rawStr(doc, "id"))
	c.Nickname = rawStr(doc, "nickname")
	c.JWT = firstNonEmpty(rawStr(doc, "jwt"), rawStr(doc, "token"))
	c.JWTIssuedAt = rawInt(doc, "jwt_issued_at")
	c.Provider = ParseProvider(firstNonEmpty(rawStr(doc, "provider"), rawStr(doc, "vendor")))
	return c, nil
}

// LoadDir 扫描目录下 zcode*.json，返回成功解析的凭证与失败清单。
//
// 与 qoder.LoadDir 同样的约定：**把失败报出来**而不是静默跳过 ——
// ZCode 的凭证多半是用户手写的（粘贴导入），格式错误必须可见。
func LoadDir(dir string) (creds []*Cred, failed []string, err error) {
	files, err := filepath.Glob(filepath.Join(dir, "zcode*.json"))
	if err != nil {
		return nil, nil, err
	}
	for _, f := range files {
		c, err := LoadFile(f)
		if err != nil {
			failed = append(failed, filepath.Base(f)+": "+err.Error())
			continue
		}
		creds = append(creds, c)
	}
	return creds, failed, nil
}

// SaveAtomic 原子写回凭证（导入或用户改昵称后调用）。
func (c *Cred) SaveAtomic() error {
	if c.FilePath == "" {
		return fmt.Errorf("凭证没有来源路径，无法写回")
	}
	doc := map[string]any{
		"uid":      c.UID,
		"nickname": c.Nickname,
		"provider": string(c.Provider),
		"credential": c.Credential,
	}
	if c.JWT != "" {
		doc["jwt"] = c.JWT
	}
	if c.JWTIssuedAt > 0 {
		doc["jwt_issued_at"] = c.JWTIssuedAt
	}
	raw, err := json.MarshalIndent(doc, "", "  ")
	if err != nil {
		return err
	}
	tmp := c.FilePath + ".tmp"
	// 0600：凭证是敏感数据。
	if err := os.WriteFile(tmp, raw, 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, c.FilePath)
}

// DefaultAuthDir 默认凭证目录（与宿主侧必须一致）。
func DefaultAuthDir() string {
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return filepath.Join(".wb-switch", "zcode", "auths")
	}
	return filepath.Join(home, ".wb-switch", "zcode", "auths")
}

// MaskedCredential 返回脱敏后的凭证（供日志与界面展示）。
//
// 凭证是秘密：日志可能被用户贴到 issue 里。只留前 6 位与后 4 位。
func (c *Cred) MaskedCredential() string {
	return MaskSecret(c.Credential)
}

// MaskSecret 脱敏任意秘密字符串。
func MaskSecret(s string) string {
	if len(s) <= 12 {
		return "****"
	}
	return s[:6] + "…" + s[len(s)-4:]
}

func rawStr(m map[string]json.RawMessage, key string) string {
	raw, ok := m[key]
	if !ok {
		return ""
	}
	var s string
	if err := json.Unmarshal(raw, &s); err != nil {
		return ""
	}
	return strings.TrimSpace(s)
}

func rawInt(m map[string]json.RawMessage, key string) int64 {
	raw, ok := m[key]
	if !ok {
		return 0
	}
	var n int64
	if err := json.Unmarshal(raw, &n); err != nil {
		return 0
	}
	return n
}

func firstNonEmpty(vals ...string) string {
	for _, v := range vals {
		if strings.TrimSpace(v) != "" {
			return strings.TrimSpace(v)
		}
	}
	return ""
}
