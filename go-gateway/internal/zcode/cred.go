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
	"log"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
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
	//
	// ## ⚠ 2026-09-21 修正：不再无条件随机
	//
	// 上面那句"随机生成的也通过"只说对了一半 —— **首次**随机没问题，
	// 但**每次启动都换一个**就会被判风控。所有者报的现象是
	//「官方不触发、我们触发」，查证结果：
	//
	//	官方客户端   固定值，持久化在 ~/.zcode/v2/telemetry-state.json
	//	我们的凭证   没有 device_mid 字段
	//	旧实现       每次进程启动 NewDeviceMid() 随机一个
	//
	// 而参考实现明确警告：「生成一次、永久复用、落盘、绝不每请求随机」。
	//
	// 现在的顺序：凭证里的 > 官方客户端那个 > 随机；取到前两者后**固化写回**
	//（见 LoadFile），此后与外部文件无关。
	//
	// 注意它也用于**对话请求体**的 `metadata.user_id.device_id`
	//（见 client.go 的 BuildAnthropicBodyWithMeta），故不只是控制面的事。
	DeviceMid string
	// deviceMidNeedsPersist 加载时发现凭证里没有 device_mid，需要固化写回。
	//
	// 为什么不直接在 parseBytes 里写：那里拿不到 `FilePath`（它由 LoadFile
	// 在 parseBytes 返回后才赋值），调用 SaveAtomic 必然失败
	//（它的第一行就是「凭证没有来源路径，无法写回」）。
	// 我第一版正是那么写的 —— 用显式标记把"要不要写"传给知道路径的调用方，
	// 而不是依赖"调用点必须晚于赋值"这种隐含顺序。
	deviceMidNeedsPersist bool

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
	//	智谱账号 12345678901234567
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

	// CaptchaParam 本次请求要带的验证码通过凭证（`X-Aliyun-Captcha-Verify-Param`）。
	//
	// ⚠ **一次性**：用一个就作废，用第二次上游必回 3007（实测）。
	// 故它不是"账号的属性"，而是"本次请求的瞬时值" —— 由调用方在发请求前
	// 现取一个填进来，用完整理掉。放在 Cred 上只是因为它随请求一起流动，
	// 而 Cred 已经是那个流动载体。
	CaptchaParam string

	// CaptchaRegion 验证码所属区域（`X-Aliyun-Captcha-Verify-Region`）。
	// 与 CaptchaParam 成对；实测缺它就是 3007。
	CaptchaRegion string
}

// isStartPlan 该凭证是否属于 **start-plan** 通道。
//
// # 为什么要区分通道（这是端点选择与凭证选择的前提）
//
// 上游有**两条**通道，端点、凭证形态、计费来源都不同：
//
//	通道          端点                                       凭证
//	────────────  ─────────────────────────────────────────  ──────────────────────
//	start-plan    zcode.z.ai/api/v1/zcode-plan/anthropic      **jwt**
//	coding-plan   {provider}/api/anthropic 或 …/coding/paas   `{apiKey}.{secret}`
//
// 抓包实测（`cdn-zcode.z.ai/zcode/config/zcode-builtin-23.json` 的 providerRules）：
//
//	account:zai-start-plan                  → zcode.z.ai/api/v1/zcode-plan/anthropic
//	account:bigmodel-start-plan             → 同上
//	account:zai-individual-coding-plan      → api.z.ai/api/anthropic
//	account:bigmodel-individual-coding-plan → open.bigmodel.cn/api/anthropic
//	account:*-offpeak-idle-plan             → zcode.z.ai/api/v1/off-peak/anthropic
//
// # 判据为什么是"有没有 jwt"
//
// 本地凭证文件里**没有**存套餐类型（只有 provider / credential / jwt /
// account_id）。而 JWT **只有 start-plan 通道会签发** —— 走 OAuth 登录时才
// 由上游一并返回。故"有 jwt"等价于"这个账号走过 start-plan 的 OAuth"。
//
// ⚠ 这个判据**不完美**：一个账号若同时有套餐与 API Key，我们会优先用
// 套餐通道（有 jwt）。那是**有意的** —— 套餐是包月额度，用它更省。
// 要纠正也很简单：删掉凭证里的 jwt 即可回落 API Key 通道。
func (c *Cred) isStartPlan() bool {
	return c != nil && strings.TrimSpace(c.JWT) != ""
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
	// ⚠ device_mid 固化（2026-09-21 所有者要求）。
	//
	// # 为什么要落盘，而不是每次读官方文件
	//
	// 所有者原话：
	//
	//	「我在想这个 deviceMid 是不是一个账号绑定一个,稳定使用
	//	  这样子是不是好一点?」→「要的」
	//
	// 他的直觉指向一个真实风险：**读官方文件是"借用"**，而官方客户端
	// 可能重装 / 换机器 / 重置状态 —— 那时 `telemetry-state.json` 会变，
	// 我们的指纹**跟着变**，又回到"同账号多设备"的老问题。
	//
	// 落盘后语义变成：**第一次借用，之后永久固化**。这正是参考实现的做法
	//（`device_mid()` 首次生成后写进 `data/device_mid`，注释：
	// 「首次生成后持久化」「生成一次、永久复用、落盘、绝不每请求随机」）。
	//
	// # 多账号时天然满足"一账号一个"
	//
	// 每个凭证文件各存各的 device_mid：
	//
	//	· 单账号（所有者的情况）：首次借用官方那个，与官方客户端一致
	//	· 多账号：各自的凭证里固化各自的值，互不影响
	//
	// 注意 deviceMid 的**语义是"设备"而非"账号"**（官方 telemetry 也是
	// 机器级、一个文件一个值），所以这里不是"刻意让每个账号不同"，
	// 而是"一旦某账号用过某个值，就让它一直用下去"—— 稳定性才是重点。
	//
	// # 为什么只在"缺失时"写（由 parseBytes 的标记决定）
	//
	// 已有 `device_mid` 时不写：那是**无谓的磁盘写**，而 `LoadFile` 在
	// 每个请求路径上都会被调用（额度查询、对话、领取…）。每次加载都
	// 重写凭证文件既慢又危险（并发写、原子替换的窗口）。
	//
	// 写入失败**不影响功能**：内存里的值仍然有效，只是下次启动会
	// 重新借用/生成。故只记日志，不返回错误 —— 让"落盘失败"变成
	// "这个账号查不到额度"是过度反应。
	if c.deviceMidNeedsPersist {
		persistDeviceMidOnce(c)
	}
	return c, nil
}

// deviceMidPersisted 已固化过 device_mid 的凭证路径集合。
//
// # 为什么需要它（并发缺陷，2026-09-21 自查发现）
//
// `LoadFile` **在每个对话请求路径上都会被调用**
//（`Dispatch.ChatStream` → `credOf` → `LoadFile`，见 dispatch.go）。
// 首次加载时多个并发请求会同时走到固化逻辑：
//
//	两者都读到"凭证里没有 device_mid" ⇒ 都置位 ⇒ 都写
//	两者用**同一个 tmp 路径**（`FilePath + ".tmp"`）⇒ 互相覆盖
//
// 后果：`os.Rename` 可能失败（一方已把 tmp 改名走），而 Windows 上
// 把文件 rename 到已存在的目标也可能失败。最坏情况是某个请求的
// `LoadFile` 返回错误 —— 而它本该是纯粹的读操作。
//
// 用"每个路径只固化一次"消除这个窗口：值本来就是确定的
//（凭证里的 > 官方文件 > 随机），重复写没有意义。
//
// 进程内即可：固化只在本进程首次加载时需要，跨进程由**磁盘上的
// device_mid 字段**保证（第二个进程读到它就不再写）。
var (
	deviceMidPersistedMu sync.Mutex
	deviceMidPersisted   = map[string]bool{}
)

// persistDeviceMidOnce 每个凭证路径最多固化一次（并发安全）。
func persistDeviceMidOnce(c *Cred) {
	deviceMidPersistedMu.Lock()
	if deviceMidPersisted[c.FilePath] {
		deviceMidPersistedMu.Unlock()
		return
	}
	// 先占位再解锁：让并发的第二个调用者直接返回，
	// 而不是排队等第一次写完又写一遍。
	deviceMidPersisted[c.FilePath] = true
	deviceMidPersistedMu.Unlock()

	// 写入失败**不影响功能**：内存里的值仍然有效，只是下次启动会
	// 重新借用/生成。故只记日志，不返回错误 —— 让"落盘失败"变成
	// "这个账号查不到额度"是过度反应。
	if err := c.SaveAtomic(); err != nil {
		log.Printf("zcode: device_mid 固化失败（本次仍有效，下次会重新借用）: %v", err)
	}
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
	//
	// ⚠⚠ 2026-09-21 修正：优先用**官方客户端已持久化的** deviceMid。
	//
	// # 所有者的问题
	//
	//	「还有zcode为什么在官方就不触发,在你这里就触发,你好好看看官方代码
	//	  还有参考实现 好好排查」
	//
	// # 查证结果（三方对照，证据链完整）
	//
	//	官方客户端   固定值，持久化在 ~/.zcode/v2/telemetry-state.json
	//	我们的凭证   字段列表里**没有** device_mid
	//	旧实现       每次进程启动 NewDeviceMid() 随机一个
	//
	// 而参考实现明确警告过这条：
	//
	//	「device fingerprint 稳定：X-Device-Mid 生成一次、永久复用、落盘、
	//	  **绝不每请求随机**」
	//
	// 每次重启换一个设备指纹，对上游风控而言就是**同一个账号被大量不同设备
	// 使用** —— 这是判"unusual activity"的典型依据，也正是官方不触发而
	// 我们触发的原因。
	//
	// # ⚠⚠⚠ 2026-09-21 二次修正：**一个账号一个设备标识**
	//
	// 所有者原话（他纠正了我一个方向性错误）：
	//
	//	「我觉得一个账号一个机器码还是有必要的,可以视为在同一个局域网内的
	//	  账号,但不能视为同一个设备上并发不同的账号」
	//
	// 他是对的，我上一版是**反的**。上一版让所有账号共用官方那一个
	// deviceMid（"与官方共享同一设备身份"），而上游看到的是：
	//
	//	设备 X → 账号 A, B, C, …, T   ← **一台设备并发 20 个账号**
	//
	// 这恰恰是**号商 / 脚本**的典型形状，比"多台设备各用一个账号"可疑得多。
	// 他的类比很准：
	//
	//	同一局域网  → 共享公网 IP   → 上游能理解（NAT 后面本来就有很多人）
	//	同一设备    → 共享 deviceMid → **不能理解**（一台机器开 20 个号？）
	//
	// 官方客户端就是"一设备一账号"（它只有一个登录账号），故正确形状是
	// **每个账号一个稳定的设备标识**。
	//
	// # 借用官方值的**唯一**条件
	//
	// 只有当"这个凭证的账号"**就是**官方客户端登录的那个账号时，才用官方
	// 的 deviceMid —— 那时我们与官方客户端是**同一个账号、同一台设备**，
	// 共享标识才是真实且一致的。
	//
	// 判据：官方 `~/.zcode/v2/credentials.json` 里的 account uuid
	//（形如 `…:account:zai-individual-coding-plan:account:{uuid}:api-key`）
	// 与凭证的 `account_id` 相等。
	//
	// 单账号用户（官方登录的账号就是他导入网关的那个）会命中此条件，
	// 于是行为与"直接用官方值"一致；多账号时其余账号各自独立。
	//
	// 其余账号：生成一个**稳定的随机** UUID 并落盘固化
	//（`NewDeviceMid` 是随机，但落盘后不再变 —— 参考实现的
	//「生成一次、永久复用、落盘、绝不每请求随机」）。
	c.DeviceMid = rawStr(doc, "device_mid")
	if !IsUUID(c.DeviceMid) {
		c.DeviceMid = mintDeviceMid(c.AccountID)
		// ⚠ 这里**只算好值**，落盘交给 LoadFile（见那里的说明）。
		//
		// 为什么不能在这里 SaveAtomic：本函数（parseBytes）拿不到
		// `c.FilePath` —— 那是 LoadFile 在本函数**返回之后**才设置的。
		// 在这里调用必然失败（SaveAtomic 的第一行就是
		// 「凭证没有来源路径，无法写回」）。我第一版正是这么写的，
		// 靠"调用点必须晚于 FilePath 赋值"这条隐含顺序才不出错 ——
		// 而那种隐含顺序正是最容易在重构时被打断的东西。
		//
		// 故把"是否需要固化"作为**显式信号**传出去，由知道路径的调用方执行。
		c.deviceMidNeedsPersist = true
	}

	return c, nil
}

// mintDeviceMid 为新账号产生一个设备标识：与官方同账号则借用，否则随机。
//
// 见调用处的长注释：**一个账号一个设备标识**是正确形状，
// 共用会让上游看到"一台设备并发一批账号"（号商特征）。
//
// 返回空串是不可能的（`NewDeviceMid` 总会给一个 UUID），
// 保留 string 返回是为了让调用点读起来自然。
func mintDeviceMid(accountID string) string {
	if accountID != "" {
		if off := officialAccountUUID(); off != "" && strings.EqualFold(off, accountID) {
			// 同一个账号 ⇒ 与官方客户端共享设备标识才是真实的。
			if v := officialDeviceMid(); v != "" {
				return v
			}
		}
	}
	// 其余账号：各自独立的随机值，落盘后永久固化。
	return NewDeviceMid()
}

// officialAccountUUID 读官方客户端**当前登录账号**的 uuid（读不到返回空串）。
//
// # 来源
//
// `~/.zcode/v2/credentials.json` 的键里嵌着账号 uuid：
//
//	account-provider:coding-plan:account:zai-individual-coding-plan:
//	    account:{uuid}:api-key
//	              ^^^^^^^^^^^^
//
// 值本身是加密的（`enc:v1:…`），但**键名是明文** —— 我们只需要那个 uuid。
//
// # 为什么要它
//
// 决定"能不能借用官方 deviceMid"：只有当凭证的账号**就是**官方登录的
// 那个账号时，共享设备标识才真实（同一账号、同一台设备）。
// 其余账号必须各自独立，否则上游会看到"一台设备并发一批账号"。
//
// 只读、容错：文件不存在 / 解析失败 / 找不到 uuid → 返回空串，
// 调用方据此走"随机"分支（那总是安全的）。
func officialAccountUUID() string {
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return ""
	}
	raw, err := os.ReadFile(filepath.Join(home, ".zcode", "v2", "credentials.json"))
	if err != nil {
		return ""
	}
	// 不整体解析 JSON：值是加密串，而我们只关心**键名**里的 uuid。
	// 用正则直接扫键，避免为一个字段依赖文件的具体 JSON 形状。
	// 形如 `account:{uuid}` 或 `…:account:{uuid}:api-key`
	re := regexp.MustCompile(`account:([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})`)
	m := re.FindSubmatch(raw)
	if len(m) < 2 {
		return ""
	}
	return string(m[1])
}

// officialDeviceMid 读官方客户端持久化的 deviceMid（读不到返回空串）。
//
// # 来源
//
// 官方客户端把它存在：
//
//	~/.zcode/v2/telemetry-state.json
//	{ "deviceMid": "{uuid}", "lastDailyActiveDate": "2026-09-21" }
//
// 本机实测该文件确实存在且含固定值。
//
// # 为什么读它而不是自己生成
//
// 见调用处（`loadCred`）的长注释：官方**固定**、我们**每次随机**，
// 那正是"官方不触发、我们触发"的差异。用官方那个值，我们的请求
// 就与官方共享同一设备身份。
//
// # 只读、容错
//
// 不写回、不修改官方文件（那是官方客户端的资产，我们只借用标识）。
// 文件不存在 / 格式不对 / 值不是 UUID → 一律返回空串，由调用方回退随机。
func officialDeviceMid() string {
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return ""
	}
	raw, err := os.ReadFile(filepath.Join(home, ".zcode", "v2", "telemetry-state.json"))
	if err != nil {
		return ""
	}
	var doc map[string]any
	if err := json.Unmarshal(raw, &doc); err != nil {
		return ""
	}
	v, _ := doc["deviceMid"].(string)
	v = strings.TrimSpace(v)
	if !IsUUID(v) {
		return ""
	}
	return v
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
