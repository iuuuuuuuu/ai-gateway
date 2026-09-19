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
	"encoding/base64"
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

	// NoRoute 用户手动禁用：**只不接流量**。
	// 由宿主的账号页「停止接流量」开关写入凭证文件的 `account.no_route`。
	//
	// 为什么用凭证文件传递而不是宿主的账号库：网关的池是**扫描
	// auths/ 目录**建立的，它根本不读宿主的 accounts.json。若只改账号库，
	// 界面上的开关就是个摆设 —— 网关照常把请求路由到这个账号。
	//
	// 命名与 WorkBuddy 侧一致（`no_route`），刻意不用 `disabled`：
	// 池自己也有一组 disabled（session 死 / 额度冻结），两者语义不同，
	// 混用会让「禁用即停养号」那个 bug 复现。
	NoRoute bool

	// JWT start-plan 令牌（OAuth 登录时上游一并返回）。
	//
	// 用途：**额度查询**用它（不是 Credential）。参考实现明确写了这一点。
	// 若用户只导入了 Credential（方式 B），则 JWT 为空 → 额度查不到，
	// 界面应显示"未知"而不是 0（0 会被误读成"额度耗尽"）。
	JWT string

	// DeviceMid 控制面请求（额度/账单）必需的设备标识（UUID 形态）。
	//
	// ## 为什么需要它（这是我最初漏掉的一环）
	//
	// 上游对控制面要求 `X-Device-Mid`，**缺了直接 400 code=3001
	// "parameter error"**（实测，见 identity.go 的注释）。
	// 我最初把这个 3001 误判成"需要 JWT"，于是界面上做了「额度未知」——
	// 而实际上**只要补上这个头就能查到额度**。
	//
	// ## 为什么是随机生成的
	//
	// 实测上游**只校验格式**（UUID 形态），随机生成的也通过 ——
	// 不需要是注册过的设备。持久化的意义是"同一账号始终表现为同一台设备"，
	// 否则服务端会看到"一个账号被大量设备查询"。
	DeviceMid string

	// JWTIssuedAt JWT 的签发时刻（Unix 秒）；0 = 未知。
	//
	// 参考实现说明：这个 JWT **没有 exp 字段**、不因时间过期 ——
	// 8 天前签发的仍能查额度。只有上游回 401/3012 才表示需要重新登录。
	// 故这里只记录年龄供展示，**不据此判定过期**。
	JWTIssuedAt int64

	// AccountID 上游**账号**标识（从 JWT 的 `user_id` 解出）；空 = 未知。
	//
	// # 为什么需要它（这解决了"重复账号"）
	//
	// `UID` 是 **credential 的哈希**：同一把 key 重复导入会得到同一个 uid
	// （幂等，实测已验证 —— 见 uitest/z15-uid-consistency.cjs）。
	//
	// 但**同一个上游账号可以有多把 key**：
	//
	//	智谱账号 19331730795565300
	//	  ├─ key A → uid zcode-aaaa…（本地第 1 条账号）
	//	  └─ key B → uid zcode-bbbb…（本地第 2 条，用户看来就是"重复"）
	//
	// 两把 key 的哈希不同，故 uid 去重**抓不到**这种情况 ——
	// 而用户眼里那就是同一个账号出现了两次。
	//
	// 实测（uitest/diag-zcode-dupe-origin.cjs）：本机三条凭证的 user_id
	// 全不同 → 它们是**真不同的账号**。但这不代表"多把 key"不存在，
	// 故记录 AccountID 让界面能识别并标注。
	//
	// # 不能自动删
	//
	// 多把 key 可能是**故意的**（一把给套餐、一把给别的用途），
	// 且各自的额度可能不同。故只**标注**，由用户决定是否清理。
	AccountID string

	// FilePath 来源文件路径；保存时写回此处。
	FilePath string
}

// AccountIDFromJWT 从 JWT 里解出上游的 `user_id`（**不验签**）。
//
// # 用途
//
// 判断"本地这几条账号是不是同一个上游账号的多把 key"。
// uid 是凭证哈希，抓不到这种情况；`user_id` 才是账号级标识。
//
// # 为什么只取 `user_id` 而不复用 jwtIssuedAt 那套
//
// 两者都在 payload 里，但用途不同、失败语义也不同：
// 签发时刻解不出只影响"年龄展示"，而账号标识解不出会影响**去重判断** ——
// 故这里出错一律返回空串（"未知"），绝**不猜**。
// 猜错会让界面把两个不同账号标成"重复"，那比不标注更糟。
func AccountIDFromJWT(token string) string {
	parts := strings.Split(strings.TrimSpace(token), ".")
	if len(parts) != 3 {
		return ""
	}
	// base64url → 标准 base64
	payload := strings.NewReplacer("-", "+", "_", "/").Replace(parts[1])
	if pad := len(payload) % 4; pad != 0 {
		payload += strings.Repeat("=", 4-pad)
	}
	raw, err := base64.StdEncoding.DecodeString(payload)
	if err != nil {
		return ""
	}
	var claims struct {
		UserID string `json:"user_id"`
		Sub    string `json:"sub"`
	}
	if err := json.Unmarshal(raw, &claims); err != nil {
		return ""
	}
	// 优先 user_id；退到 sub（实测两者相同，但 user_id 更明确）
	if claims.UserID != "" {
		return claims.UserID
	}
	return claims.Sub
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

// FileBaseName 该凭证对应的文件名（**不含目录**）。
//
// ## 为什么要这个方法而不是到处拼 `"zcode-" + uid`
//
// uid 本身已经带 `zcode-` 前缀（见 CredKey），再拼一次会得到
// `zcode-zcode-xxx.json` —— 实测踩到过，文件名难看且容易让人以为
// 有两个前缀层级。
//
// 统一在这里生成，保证"落盘"与"扫描"用同一个口径。
func (c *Cred) FileBaseName() string {
	uid := strings.TrimSpace(c.UID)
	if uid == "" {
		uid = CredKey(c.Credential)
	}
	// 已有前缀就不再重复加
	if strings.HasPrefix(uid, "zcode-") {
		return sanitizeFileName(uid) + ".json"
	}
	return "zcode-" + sanitizeFileName(uid) + ".json"
}

// sanitizeFileName 把任意字符串变成安全的文件名片段（防路径穿越）。
//
// 安全项：uid 可能是从凭证派生的，也可能来自用户手写的文件 ——
// 直接拼进路径的话，异常的 uid（如 "../../etc/passwd"）会写到目录外。
func sanitizeFileName(s string) string {
	out := make([]rune, 0, len(s))
	for _, r := range s {
		switch {
		case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r >= '0' && r <= '9', r == '-', r == '_':
			out = append(out, r)
		default:
			out = append(out, '_')
		}
	}
	if len(out) == 0 {
		return "unknown"
	}
	return string(out)
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
	// 上游账号标识：优先读已存的，没有就从 JWT 里现解一个
	c.AccountID = rawStr(doc, "account_id")
	if c.AccountID == "" && c.JWT != "" {
		c.AccountID = AccountIDFromJWT(c.JWT)
	}
	c.Provider = ParseProvider(firstNonEmpty(rawStr(doc, "provider"), rawStr(doc, "vendor")))

	// 用户手动禁用（宿主写入的 `account.no_route`）。
	//
	// 两个位置都读：宿主的 `account` 段（与 WorkBuddy 一致）与顶层
	//（手写凭证文件时用户可能直接写在顶层，不接受它会让禁用静默失效）。
	c.NoRoute = rawBool(doc, "no_route") || rawBoolAt(doc, "account", "no_route")

	// 控制面设备标识。缺失时**补一个**而不是留空 ——
	// 空值会让额度查询恒回 3001（实测），而那看起来像"这个账号没有额度"。
	c.DeviceMid = rawStr(doc, "device_mid")
	if !IsUUID(c.DeviceMid) {
		c.DeviceMid = NewDeviceMid()
	}

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
//
// ## ⚠ 必须保留宿主写入的 `account.no_route`
//
// 这个标记是**宿主**写的（账号页的「停止接流量」开关），而 SaveAtomic 是
// **网关**写的。若这里整体覆盖，网关的任何一次写回都会把用户的禁用标记
// **悄悄抹掉** —— 那个账号会重新开始接流量，而用户以为它还停着。
//
// 保留策略：**从磁盘读回再合并**，而不是只用内存里的字段重建。
// 这样宿主后来加的字段（我们还不认识的）也不会被丢掉。
func (c *Cred) SaveAtomic() error {
	if c.FilePath == "" {
		return fmt.Errorf("凭证没有来源路径，无法写回")
	}

	// 先读回现有内容（可能不存在 —— 那是新建，正常）
	doc := map[string]any{}
	if raw, err := os.ReadFile(c.FilePath); err == nil {
		_ = json.Unmarshal(raw, &doc)
	}

	doc["uid"] = c.UID
	doc["nickname"] = c.Nickname
	doc["provider"] = string(c.Provider)
	doc["credential"] = c.Credential
	if c.JWT != "" {
		doc["jwt"] = c.JWT
	}
	if c.JWTIssuedAt > 0 {
		doc["jwt_issued_at"] = c.JWTIssuedAt
	}
	// 上游账号标识：让界面能识别"同一账号的多把 key"（见 Cred.AccountID）。
	// 有 JWT 而字段为空时现解一次，保证写入的文件里始终有它。
	if c.AccountID == "" && c.JWT != "" {
		c.AccountID = AccountIDFromJWT(c.JWT)
	}
	if c.AccountID != "" {
		doc["account_id"] = c.AccountID
	}
	// 持久化 deviceMid：同一个账号必须始终表现为同一台设备
	//（否则服务端会看到"一个账号被大量不同设备查询"）。
	if IsUUID(c.DeviceMid) {
		doc["device_mid"] = c.DeviceMid
	}

	// 保留禁用标记（见上面的说明）。
	//
	// 用**读回的值**而不是 c.NoRoute：c.NoRoute 是上次加载时读到的，
	// 期间用户在界面上可能改过 —— 磁盘上的才是最新的。
	if existing := readNoRoute(doc); existing {
		acc, _ := doc["account"].(map[string]any)
		if acc == nil {
			acc = map[string]any{}
		}
		acc["no_route"] = true
		doc["account"] = acc
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

// readNoRoute 从已解析的文档里读禁用标记（`account.no_route` 或顶层）。
func readNoRoute(doc map[string]any) bool {
	if b, ok := doc["no_route"].(bool); ok && b {
		return true
	}
	if acc, ok := doc["account"].(map[string]any); ok {
		if b, ok := acc["no_route"].(bool); ok && b {
			return true
		}
	}
	return false
}

// rawBool 读一个布尔字段（缺失或类型不符都返回 false）。
// 缺失返回 false 而不是报错：三态语义里「未声明」等价于「不禁用」——
// 若报错，历史凭证文件（没有这个键）会全部解析失败。
func rawBool(m map[string]json.RawMessage, key string) bool {
	raw, ok := m[key]
	if !ok {
		return false
	}
	var b bool
	return json.Unmarshal(raw, &b) == nil && b
}

// rawBoolAt 读嵌套对象里的布尔字段（如 `account.no_route`）。
func rawBoolAt(m map[string]json.RawMessage, outer, key string) bool {
	raw, ok := m[outer]
	if !ok {
		return false
	}
	var nested map[string]json.RawMessage
	if json.Unmarshal(raw, &nested) != nil {
		return false
	}
	return rawBool(nested, key)
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
