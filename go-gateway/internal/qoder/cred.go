// Package qoder 实现 QoderWork 的账号凭证与签名。
//
// 与 WorkBuddy 的关键差异（决定了它必须独立成包）：
//
//	WorkBuddy                      Qoder
//	─────────────────────────────  ─────────────────────────────────────
//	Bearer <accessToken>            COSY 签名（RSA 包 AES + AES-CBC 身份 + MD5）
//	accessToken + refreshToken      dt-（约 30 天）+ drt-（约 1 年，轮换）
//	无机器指纹                       必需 MachineID / MachineToken / MachineType
//	OAuth 授权码                     PKCE 设备流（发起 + 轮询两步）
//	/v2/chat/completions            /algo/api/v2/...（签名时要去掉 /algo 前缀）
//
// 参考实现：Sliverkiss/qoderwork2api（MIT）。本包在其基础上做了三处改动：
//  1. **双区支持** —— 参考实现只支持国服（硬编码 qoder.com.cn）；
//     实测国际版（.sh / qoder.com）与国服同构，同一个 client_id 在两区都有效，
//     故把域名做成可配置项（见 Region）。
//  2. 字段与错误类型对齐本仓库既有风格（三态语义、可读错误）。
//  3. 补上单元测试（参考实现几乎没有测试）。
package qoder

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
)

// Cred 一个 Qoder 账号的凭证与运行时状态。
type Cred struct {
	// UID 上游账号标识。
	UID string
	// Nickname 昵称（可能为空）。
	Nickname string

	// DT 当前 Bearer 令牌（参考实现里叫 dt-），DRT 刷新令牌（drt-）。
	//
	// 命名沿用参考实现（上游 COSY 签名层直接引用这两个值），但语义已明确：
	// DT 会过期、DRT 用于换新 DT 并在每次刷新时轮换。
	DT  string
	DRT string
	// DTExpiresAt DT 的过期时刻（Unix 秒）；0 = 未知（则每次都会尝试刷新）。
	DTExpiresAt int64

	// 机器指纹：COSY 签名的必需输入，跨重启必须保持不变。
	//
	// 为什么必须持久化：上游把「同一账号 + 同一机器指纹」视为同一设备。
	// 每次重启都换一组指纹会让上游看到「同一账号从大量不同设备登录」，
	// 可能触发风控。参考实现把它落在 state.json 里，本包沿用。
	MachineID    string
	MachineToken string
	MachineType  string

	// Region 服务区域（国服 / 国际版）。见 region.go。
	Region Region

	// NoRoute 用户手动禁用：**只不接流量**。
	//
	// 由宿主的账号页「停止接流量」开关写入凭证文件的 `account.no_route`。
	//
	// 为什么用凭证文件传递而不是宿主的账号库：网关的池是**扫描
	// auths/ 目录**建立的，它根本不读宿主的 accounts.json。若只改账号库，
	// 界面上的开关就是个摆设 —— 网关照常把请求路由到这个账号
	//（ZCode 侧实测确认过同样的缺陷）。
	//
	// 命名与 WorkBuddy 侧一致（`no_route`），刻意不用 `disabled`：
	// 池自己也有一组 disabled（session 死 / 额度冻结），两者语义不同。
	NoRoute bool

	// FilePath 来源文件路径；刷新后原子写回此处。
	FilePath string

	mu sync.Mutex
}

// Lock / Unlock 供同进程内其他包在改写字段期间加锁（与 auth.Auth 同约定）。
func (c *Cred) Lock()   { c.mu.Lock() }
func (c *Cred) Unlock() { c.mu.Unlock() }

// AuthInvalidError 凭证已失效（DRT 刷新失败）—— 需要重新登录。
type AuthInvalidError struct{ Msg string }

func (e *AuthInvalidError) Error() string { return "auth_invalid: " + e.Msg }

// LoadFile 从磁盘加载 Qoder 凭证文件。
//
// 兼容三种形态（实测来源各不相同）：
//
//	嵌套形（OAuth 落盘，参考实现产出）
//	  {"auth":{"accessToken":"...","refreshToken":"...","expiresAt":<unix>,
//	           "domain":"..."},"account":{"uid":"...","nickname":"..."}}
//	扁平形（手写 / 旧版）
//	  {"token":"...","refresh_token":"...","uid":"...","nickname":"..."}
//	本仓库宿主导出的形态
//	  {"accessToken":"...","refreshToken":"...","uid":"...","domain":"..."}
//
// 解析失败一律返回 error（不静默跳过）—— 调用方需要区分「文件不存在」
// 与「文件存在但格式不对」，后者是用户配置问题、必须让他看到。
func LoadFile(path string) (*Cred, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	return Parse(raw, path)
}

// Parse 解析凭证字节；path 仅用于回填 FilePath 与从文件名兜底取 uid。
func Parse(raw []byte, path string) (*Cred, error) {
	if len(raw) == 0 {
		return nil, fmt.Errorf("凭证文件为空: %s", filepath.Base(path))
	}
	var probe map[string]json.RawMessage
	if err := json.Unmarshal(raw, &probe); err != nil {
		return nil, fmt.Errorf("凭证不是合法 JSON: %w", err)
	}

	var c Cred
	if _, nested := probe["auth"]; nested {
		var n struct {
			Auth struct {
				AccessToken  string `json:"accessToken"`
				RefreshToken string `json:"refreshToken"`
				ExpiresAt    int64  `json:"expiresAt"`
				Domain       string `json:"domain"`
			} `json:"auth"`
			Account struct {
				UID      string `json:"uid"`
				Nickname string `json:"nickname"`
				// 用户手动禁用（宿主账号页的「停止接流量」开关）
				NoRoute bool `json:"no_route"`
			} `json:"account"`
			// 机器指纹 —— **必须读回来**。
			//
			// 此前这里没读，于是 `SaveAtomic` 写出去的 machine 段在下次
			// 加载时被丢掉，`EnsureFingerprint` 又生成一套新的。
			// 指纹的语义是"同一账号 + 同一设备"，每次换就等于让上游看到
			// 「同一账号从大量不同设备登录」—— 正是 Cred.MachineID 注释里
			// 警告过的风险，却因为读写不对称而实际发生着。
			Machine struct {
				ID    string `json:"id"`
				Token string `json:"token"`
				Type  string `json:"type"`
			} `json:"machine"`
		}
		if err := json.Unmarshal(raw, &n); err != nil {
			return nil, fmt.Errorf("嵌套形解析失败: %w", err)
		}
		c.DT = n.Auth.AccessToken
		c.DRT = n.Auth.RefreshToken
		c.DTExpiresAt = n.Auth.ExpiresAt
		c.UID = n.Account.UID
		c.Nickname = n.Account.Nickname
		c.NoRoute = n.Account.NoRoute
		c.MachineID = n.Machine.ID
		c.MachineToken = n.Machine.Token
		c.MachineType = n.Machine.Type
		c.Region = RegionFromDomain(n.Auth.Domain)
	} else {
		var f struct {
			Token        string `json:"token"`
			AccessToken  string `json:"accessToken"`
			RefreshToken string `json:"refresh_token"`
			RefreshToken2 string `json:"refreshToken"`
			ExpiresAt    int64  `json:"expires_at_unix"`
			ExpiresAt2   int64  `json:"expiresAt"`
			UID          string `json:"uid"`
			UserID       string `json:"user_id"`
			Nickname     string `json:"nickname"`
			Domain       string `json:"domain"`
			// 用户手动禁用（手写凭证文件时可能直接写在顶层）
			NoRoute bool `json:"no_route"`
		}
		if err := json.Unmarshal(raw, &f); err != nil {
			return nil, fmt.Errorf("扁平形解析失败: %w", err)
		}
		c.DT = firstNonEmpty(f.Token, f.AccessToken)
		c.DRT = firstNonEmpty(f.RefreshToken, f.RefreshToken2)
		c.DTExpiresAt = firstNonZero(f.ExpiresAt, f.ExpiresAt2)
		c.UID = firstNonEmpty(f.UID, f.UserID)
		c.Nickname = f.Nickname
		c.NoRoute = f.NoRoute
		c.Region = RegionFromDomain(f.Domain)
	}

	if c.DT == "" && c.DRT == "" {
		return nil, fmt.Errorf("凭证里既没有 accessToken 也没有 refreshToken: %s", filepath.Base(path))
	}
	if c.UID == "" {
		// 兜底：从文件名 qoder-<uid>.json / qoderwork-<uid>.json 提取
		base := filepath.Base(path)
		for _, p := range []string{"qoder-", "qoderwork-"} {
			if strings.HasPrefix(base, p) && strings.HasSuffix(base, ".json") {
				c.UID = strings.TrimSuffix(strings.TrimPrefix(base, p), ".json")
				break
			}
		}
	}
	c.FilePath = path
	// 区域未知时**不猜**：保持 RegionUnknown，由调用方决定（界面上让用户选）。
	if c.Region == "" {
		c.Region = RegionUnknown
	}
	return &c, nil
}

// LoadDir 扫描目录下 qoder*.json，返回成功解析的凭证与失败数。
//
// 与 auth.LoadDir 的差别：那个静默跳过解析失败的文件；这里把失败**报出来**，
// 因为 Qoder 的凭证格式是用户可能手写的（导入方式），格式错误必须可见。
func LoadDir(dir string) (creds []*Cred, failed []string, err error) {
	files, err := filepath.Glob(filepath.Join(dir, "qoder*.json"))
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

// SaveAtomic 原子写回凭证（刷新后调用）。
//
// 保持**嵌套形**（OAuth 的标准形态），这样参考实现与本仓库可以互相读取。
//
// ## ⚠ 必须保留宿主写入的 `account.no_route`
//
// 这个标记是**宿主**写的（账号页的「停止接流量」开关），而本方法是
// **网关**写的（令牌刷新时）。Qoder 的令牌刷新很频繁，若这里整体覆盖，
// 用户的禁用标记会在几分钟内被**悄悄抹掉** —— 那个账号重新开始接流量，
// 而用户以为它还停着。
//
// 保留策略：**从磁盘读回再合并**，而不是只用内存字段重建。
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

	doc["auth"] = map[string]any{
		"accessToken":  c.DT,
		"refreshToken": c.DRT,
		"expiresAt":    c.DTExpiresAt,
		"domain":       c.Region.Domain(),
	}
	account, _ := doc["account"].(map[string]any)
	if account == nil {
		account = map[string]any{}
	}
	account["uid"] = c.UID
	account["nickname"] = c.Nickname
	doc["account"] = account

	// ⚠ 机器指纹**必须落盘**（这是我漏掉的一环）。
	//
	// `EnsureFingerprint` 会生成它们，但此前 `SaveAtomic` 不写 ——
	// 于是每次重新加载都生成一套**新的**指纹。而指纹的语义是
	// "同一账号 + 同一设备"，每次换就等于：
	//
	//	上游看到「同一账号从大量不同设备登录」→ 可能触发风控
	//
	// 我自己在 Cred.MachineID 的注释里写了这个风险，却没在保存时落实。
	// 实测发现：`qoder-login import-client` 落盘的凭证里没有 machine 段，
	// 重新加载后 MachineID 为空（回归测试 tmp_imported_cred_test.go 抓到）。
	//
	// 形状照参考实现（嵌套 `machine` 段）—— 与 LoadFile 的解析对应。
	if c.MachineID != "" || c.MachineToken != "" || c.MachineType != "" {
		machine, _ := doc["machine"].(map[string]any)
		if machine == nil {
			machine = map[string]any{}
		}
		if c.MachineID != "" {
			machine["id"] = c.MachineID
		}
		if c.MachineToken != "" {
			machine["token"] = c.MachineToken
		}
		if c.MachineType != "" {
			machine["type"] = c.MachineType
		}
		doc["machine"] = machine
	}

	raw, err := json.MarshalIndent(doc, "", "  ")
	if err != nil {
		return err
	}
	tmp := c.FilePath + ".tmp"
	// 0600：凭证是敏感数据，不给同机其他用户读。
	if err := os.WriteFile(tmp, raw, 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, c.FilePath)
}

// EnsureFingerprint 确保机器指纹存在；缺失时生成并标记 changed=true。
//
// 调用方在 changed 为真时应落盘 —— 指纹必须跨重启稳定，否则上游会看到
// 「同一账号从大量设备登录」（见 Cred 里 MachineID 的注释）。
func (c *Cred) EnsureFingerprint() (changed bool) {
	if c.MachineID == "" {
		c.MachineID = uuid4()
		changed = true
	}
	if c.MachineToken == "" {
		c.MachineToken = hexShort(32)
		changed = true
	}
	if c.MachineType == "" {
		// 参考实现固定用 "5"（对应桌面客户端类型）。不要臆造其它值。
		c.MachineType = "5"
		changed = true
	}
	return changed
}

// ---------------------------------------------------------------------------
// 小工具（与参考实现保持一致的算法，不要"顺手优化"——签名对字节敏感）
// ---------------------------------------------------------------------------

// uuid4 生成 UUIDv4。用于机器指纹与请求 ID。
func uuid4() string {
	b := make([]byte, 16)
	_, _ = rand.Read(b)
	b[6] = (b[6] & 0x0f) | 0x40
	b[8] = (b[8] & 0x3f) | 0x80
	return fmt.Sprintf("%08x-%04x-%04x-%04x-%012x",
		b[0:4], b[4:6], b[6:8], b[8:10], b[10:16])
}

// hexShort 生成 n 个字符的随机十六进制串。
//
// ⚠ 签名层用它生成临时 AES 密钥，**必须是 n 个 ASCII 字符**（不是 n 个随机字节）——
// 这是与桌面客户端对齐的关键，改错会导致签名验证失败。
func hexShort(n int) string {
	b := make([]byte, (n+1)/2)
	_, _ = rand.Read(b)
	return hex.EncodeToString(b)[:n]
}

// NewUUID 导出给同包其它文件用（签名层的 requestId 需要它）。
func NewUUID() string { return uuid4() }

// randRead 是 crypto/rand.Read 的薄封装，便于集中处理"读随机数失败"。
//
// 为什么不在各处直接调 crypto/rand.Read：Go 1.24+ 起它**永不返回错误**
// （失败即 panic），但显式忽略返回值会让静态检查报错。包一层更清晰。
func randRead(b []byte) (int, error) { return rand.Read(b) }

// sha256Sum / base64RawURL 供 PKCE 使用（与参考实现逐字一致：RawURLEncoding）。
func sha256Sum(b []byte) []byte {
	s := sha256.Sum256(b)
	return s[:]
}

func base64RawURL(b []byte) string { return base64.RawURLEncoding.EncodeToString(b) }

func firstNonEmpty(vals ...string) string {
	for _, v := range vals {
		if strings.TrimSpace(v) != "" {
			return v
		}
	}
	return ""
}

func firstNonZero(vals ...int64) int64 {
	for _, v := range vals {
		if v != 0 {
			return v
		}
	}
	return 0
}
