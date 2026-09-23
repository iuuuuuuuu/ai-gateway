// Package auth 解析 WorkBuddy auth 文件（嵌套形/扁平形双形态），
// 提供 refresh 后的原子写回。
package auth

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// Auth 是归一化后的账号凭证（来源可以是插件 OAuth 嵌套形或手写扁平形）。
type Auth struct {
	// mu 串行化 RefreshToken 写与 SaveAtomic 读，防止并发写回半更新 token。
	mu sync.Mutex

	AccessToken  string
	RefreshToken string
	ExpiresAt    int64 // Unix 秒
	Domain       string
	UID          string
	EnterpriseID string
	Nickname     string
	FilePath     string // 来源文件；refresh 后原子写回此处
	// NoRoute 用户手动禁用：**只不参与选号**，养号任务照跑。
	//
	// 由宿主在导出凭证时写入 account.no_route。与 pool 里的 disabled 区分：
	//   NoRoute  → 用户意图「别把流量给它」；签到/上报/成长任务仍应执行
	//   disabled → 网关判定该账号已死（session 死 / 额度冻结）；任务跳过多余
	// 二者混用会导致「禁用即停养号」——那是所有者报过的真实缺陷。
	NoRoute bool

	// SoonestExpireAt 仍有剩余积分的套餐中最早的到期时刻（Unix 秒）；0 = 未知。
	//
	// 由凭证文件里的 credit 块带入（宿主 workbuddy-switch 查询积分后写入），
	// 或由网关自身的签到任务刷新。账号池据此做「先烧快过期额度」的分层选号。
	// 只读元数据，不参与 token 刷新与写回。
	SoonestExpireAt int64

	// Product 账号所属产品；空串等价于 ProductWorkBuddy。
	//
	// 存在的意义：两个产品的**同名模型可能是完全不同的后端**，且凭证形态、
	// 签名方式、端点全都不同。选号时需要知道"这个号是谁的"，转发时才派发到
	// 正确的上游实现（见 server 的 dispatch 层）。
	//
	// ⚠ 这个字段**不参与** WorkBuddy 单产品下的任何决策 —— 默认空串，
	// 所有既有路径的行为逐字不变（多产品路由由 pool 的开关控制）。
	Product string
}

// 产品标识。
const (
	// ProductWorkBuddy 默认产品（空串等价于此）。
	ProductWorkBuddy = "workbuddy"
	// ProductQoder QoderWork。
	ProductQoder = "qoder"
	// ProductZcode ZCode（Z.AI / 智谱 GLM 编码套餐）。
	ProductZcode = "zcode"
)

// ProductOf 返回账号所属产品，空串归一成 ProductWorkBuddy。
//
// 归一化而非保留空串：调用方写 `a.ProductOf() == ProductWorkBuddy` 比
// 到处判 `== "" || == "workbuddy"` 更不容易漏（漏判会把 WorkBuddy 账号
// 当成未知产品，走进其它产品的派发分支）。
func (a *Auth) ProductOf() string {
	if a.Product == "" {
		return ProductWorkBuddy
	}
	return a.Product
}

// IsQoder 报告该账号是否属于 Qoder。
func (a *Auth) IsQoder() bool { return a.Product == ProductQoder }

// IsZcode 报告该账号是否属于 ZCode。
func (a *Auth) IsZcode() bool { return a.Product == ProductZcode }

// Lock 供同进程内其他包（upstream.RefreshToken）在改写 Auth 字段期间加锁。
func (a *Auth) Lock() { a.mu.Lock() }

// Unlock 释放 a.Lock 获取的锁。
func (a *Auth) Unlock() { a.mu.Unlock() }

// NeedsRefresh 报告 token 是否将在 within 内过期（或已过期/无 expiry）。
func (a *Auth) NeedsRefresh(within time.Duration) bool {
	if a.ExpiresAt <= 0 {
		return true
	}
	return time.Now().Add(within).Unix() >= a.ExpiresAt
}

// Region 账号所属服务区域。
//
// 存在的意义：**同名模型在两个区域可能是不同的后端模型**，能力并不一致。
// 实测（2026-09-16，/v3/config 元数据 + 逐账号发图验证）：
//
//	glm-5.3  国服「旗舰模型，擅长复杂软件工程与长程 Agent 任务」out=64000
//	         国际版「能力均衡，适合日常使用」                out=48000
//	         —— 国服后端能读图；国际版后端把图片换成固定占位符（token 增量
//	         恒为 +29，与图片体积无关），模型只能回答「无法查看图片」。
//
// 因此「同一个 glm-5.3 时好时坏」的真实原因是**选号随机命中了两个不同后端**，
// 而不是模型本身不稳定。见 pool.PickForModelRegion 与 server 的区域路由。
type Region int

const (
	// RegionAny 不限区域（默认；等价于引入本概念之前的行为）。
	RegionAny Region = iota
	// RegionCN 国服（*.workbuddy.cn / *.codebuddy.cn）。
	RegionCN
	// RegionIntl 国际版（*.workbuddy.ai / *.codebuddy.ai）。
	RegionIntl
)

func (r Region) String() string {
	switch r {
	case RegionCN:
		return "cn"
	case RegionIntl:
		return "intl"
	default:
		return "any"
	}
}

// Region 返回账号所属区域。
//
// 判据与 upstream.IsIntl 完全一致（依据凭证里的 domain 字段），
// 两者都走下面的 regionOfDomain —— **只此一处**，见其注释。
func (a *Auth) Region() Region {
	if a.IsIntl() {
		return RegionIntl
	}
	return RegionCN
}

// IsIntl 报告账号是否属于国际版。
func (a *Auth) IsIntl() bool {
	if a == nil {
		return false
	}
	return regionOfDomain(a.Domain) == RegionIntl
}

// 各区域的**完整域名**清单。
//
// # ⚠⚠ 为什么必须是完整域名，不能再用 `HasSuffix(".ai")`（2026-09-22 修）
//
// 原判据是 `strings.HasSuffix(domain, ".ai")`。它同时**过宽**又**过窄**：
//
//	过窄（真缺陷）：Qoder 国际版的域名是 `qoder.sh`（`.sh` 不是 `.ai`）
//	    ⇒ 被判成**国服** ⇒ `proxy_scope.intl` 对它**不生效** ⇒ 永远走直连。
//	    实测 `openapi.qoder.sh`：直连 10.3s，走代理 1.2s（**慢 8.7 倍**）。
//	    而 `qoderAuthOf` 的注释写着「域名用于区域判定（qoder.sh = 国际版）」
//	    —— 写那段代码时以为判据认得出它，实际认不出。两处口径分叉。
//
//	过宽：任何以 `.ai` 结尾的域名都算国际版。今天恰好没有反例，
//	    但那是巧合 —— 判据不该依赖"恰好没人用别的 .ai 域名"。
//
// 故改为**白名单精确匹配**：列出每个产品真实使用的域名，
// 未列出的按国服处理（与「历史只有国服账号」的向后兼容口径一致）。
//
// ⚠ 新增产品/域名时**必须同步这里**，否则它的国际版账号会静默走直连 ——
// 表现是「某个产品的国际版特别慢/连不上」，而其它产品正常。
const (
	// workbuddyIntl 是 WorkBuddy 国际版（也是短信登录用的那个域）。
	workbuddyIntl = "www.workbuddy.ai"
	// qoderIntl 是 Qoder 国际版的凭证域名（qoder.Region.Domain() 写的就是它），
	// 另有 API 域 openapi.qoder.sh / api3.qoder.sh 与授权页 qoder.com。
	qoderIntl = "qoder.sh"
	// zcodeIntl 是 ZCode（Z.AI）的 API 域（zcode.DomainOfProvider 写的就是它）。
	zcodeIntl = "api.z.ai"
)

// intlDomains 国际版域名白名单（**完整域名**）。
//
// 匹配规则见 regionOfDomain：等于清单项，或以 `.<清单项>` 结尾。
// 故这里只需列到「能覆盖同族子域」的那一层。
//
// ⚠ 每一项都要有依据，不能凭"看起来像"加：
//
//	workbuddy.ai —— 真实凭证域名 www.workbuddy.ai（5 个国际版账号在用）
//	codebuddy.ai —— **未在任何真实凭证中观察到**，但本仓库多处注释
//	    （config.rs / update.rs / types.ts / client.go）都把
//	    「*.workbuddy.ai / *.codebuddy.ai」并列为国际版。
//	    保留它：真出现时能正确走代理，不出现时零成本。
//	    （旧的后缀判据 `HasSuffix(".ai")` 也是顺带覆盖它的。）
//	qoder.sh     —— 真实凭证域名（Qoder 国际版），**本次修复的主角**
//	qoder.com    —— Qoder 国际版授权页（qoder.RegionFromDomain 判为 intl）
//	z.ai         —— 真实凭证域名 api.z.ai（ZCode）
var intlDomains = []string{
	"workbuddy.ai",
	"codebuddy.ai",
	qoderIntl,
	"qoder.com",
	"z.ai",
}

// regionOfDomain 按**完整域名**判定区域。
//
// 匹配规则：域名等于清单项，或以 `.<清单项>` 结尾（覆盖子域）。
// 例如清单里有 `qoder.sh` 时，`api3.qoder.sh` 与 `qoder.sh` 都命中，
// 而 `notqoder.sh` / `qoder.sh.evil.com` 都不命中。
//
// 空域名按国服（历史行为，见 Region 的注释）。
func regionOfDomain(domain string) Region {
	d := strings.ToLower(strings.TrimSpace(domain))
	if d == "" {
		return RegionCN
	}
	// 去掉可能的协议前缀与路径，容忍凭证里写成 URL 的情况。
	if i := strings.Index(d, "://"); i >= 0 {
		d = d[i+3:]
	}
	if i := strings.IndexAny(d, "/?#"); i >= 0 {
		d = d[:i]
	}
	// 去掉端口。
	if i := strings.LastIndex(d, ":"); i >= 0 {
		d = d[:i]
	}
	for _, base := range intlDomains {
		if d == base || strings.HasSuffix(d, "."+base) {
			return RegionIntl
		}
	}
	return RegionCN
}

// Parse 兼容两种磁盘形态：
//
//	嵌套形 {"auth":{...},"account":{...}}  （插件 OAuth 输出）
//	扁平形 {"accessToken":...,"uid":...}   （手写/旧版）
func Parse(raw []byte) (*Auth, error) {
	if len(raw) == 0 {
		return nil, fmt.Errorf("empty auth storage")
	}
	var probe map[string]json.RawMessage
	if err := json.Unmarshal(raw, &probe); err != nil {
		return nil, fmt.Errorf("storage_parse_error: %w", err)
	}
	var a Auth
	if _, nested := probe["auth"]; nested {
		var n struct {
			Auth struct {
				AccessToken  string `json:"accessToken"`
				RefreshToken string `json:"refreshToken"`
				ExpiresAt    int64  `json:"expiresAt"`
				Domain       string `json:"domain"`
			} `json:"auth"`
			Account struct {
				UID          string `json:"uid"`
				EnterpriseID string `json:"enterpriseId"`
				Nickname     string `json:"nickname"`
				// NoRoute 用户手动禁用 = **只不接流量**，养号任务照跑。
				//
				// 与池里的 disabled 完全不同：那个是网关自己判定的
				//（连续 3 次 12153 session 死 / 额度冻结），跑了也白跑，
				// 所以养号任务该跳过。而这个只是「别把请求路由到它」，
				// 签到 / 活跃上报 / 成长任务等仍应照跑 —— 所有者明确过这个语义。
				NoRoute bool `json:"no_route"`
			} `json:"account"`
			Credit creditBlock `json:"credit"`
		}
		if err := json.Unmarshal(raw, &n); err != nil {
			return nil, fmt.Errorf("storage_parse_error: %w", err)
		}
		a = Auth{
			AccessToken:     n.Auth.AccessToken,
			RefreshToken:    n.Auth.RefreshToken,
			ExpiresAt:       n.Auth.ExpiresAt,
			Domain:          n.Auth.Domain,
			UID:             n.Account.UID,
			EnterpriseID:    n.Account.EnterpriseID,
			Nickname:        n.Account.Nickname,
			NoRoute:         n.Account.NoRoute,
			SoonestExpireAt: normalizeEpoch(n.Credit.SoonestExpireAt),
		}
	} else {
		var f struct {
			AccessToken  string `json:"accessToken"`
			RefreshToken string `json:"refreshToken"`
			ExpiresAt    int64  `json:"expiresAt"`
			Domain       string `json:"domain"`
			UID          string `json:"uid"`
			EnterpriseID string `json:"enterpriseId"`
			Nickname     string `json:"nickname"`
			// 扁平形同样支持 no_route（两种形状的字段语义必须一致，
			// 否则同一个账号换种写法就会「突然开始接流量」）。
			NoRoute bool `json:"no_route"`
			// 扁平形把 credit 字段平铺在顶层（兼容旧版手写凭证）。
			SoonestExpireAt int64 `json:"soonestExpireAt"`
		}
		if err := json.Unmarshal(raw, &f); err != nil {
			return nil, fmt.Errorf("storage_parse_error: %w", err)
		}
		a = Auth{
			AccessToken:     f.AccessToken,
			RefreshToken:    f.RefreshToken,
			ExpiresAt:       f.ExpiresAt,
			Domain:          f.Domain,
			UID:             f.UID,
			EnterpriseID:    f.EnterpriseID,
			Nickname:        f.Nickname,
			NoRoute:         f.NoRoute,
			SoonestExpireAt: normalizeEpoch(f.SoonestExpireAt),
		}
	}
	if strings.TrimSpace(a.AccessToken) == "" {
		return nil, fmt.Errorf("parse_error: missing accessToken")
	}
	return &a, nil
}

// creditBlock 凭证文件里的积分到期元数据（宿主写入，网关只读）。
type creditBlock struct {
	// SoonestExpireAt 最近到期时刻。上游混用秒/毫秒，此处按量级归一。
	SoonestExpireAt int64 `json:"soonestExpireAt"`
}

// normalizeEpoch 把秒/毫秒 epoch 统一成秒（上游混用两种精度）。
func normalizeEpoch(n int64) int64 {
	if n <= 0 {
		return 0
	}
	if n > 1_000_000_000_000 {
		return n / 1000
	}
	return n
}

// SaveAtomic 以嵌套形原子写回 FilePath（tmp + rename），保持嵌套形（插件可读）格式。
// 全程持 a.mu：防止与 RefreshToken 修改 token 字段并发，杜绝写回半更新。
// 防御：accessToken 为空时拒绝写回，避免误用空凭证覆盖有效文件。
func (a *Auth) SaveAtomic() error {
	a.mu.Lock()
	defer a.mu.Unlock()
	if strings.TrimSpace(a.AccessToken) == "" {
		return fmt.Errorf("save refused: empty accessToken (uid=%s)", a.UID)
	}
	if a.FilePath == "" {
		return fmt.Errorf("no FilePath set")
	}
	doc := map[string]any{
		"auth": map[string]any{
			"accessToken":  a.AccessToken,
			"refreshToken": a.RefreshToken,
			"expiresAt":    a.ExpiresAt,
			"domain":       a.Domain,
		},
		"account": map[string]any{
			"uid":          a.UID,
			"enterpriseId": a.EnterpriseID,
			"nickname":     a.Nickname,
		},
	}
	// 积分到期元数据必须原样保留：它由宿主（workbuddy-switch）或签到任务写入，
	// 而 token 刷新会重写整个文件。若这里丢掉，一次保活就会抹掉选号依据
	// （表现为分层均衡静默退化成原来的三因子随机）。
	if a.SoonestExpireAt > 0 {
		doc["credit"] = map[string]any{"soonestExpireAt": a.SoonestExpireAt}
	}
	raw, err := json.MarshalIndent(doc, "", "  ")
	if err != nil {
		return err
	}
	tmp := a.FilePath + ".tmp"
	if err := os.WriteFile(tmp, raw, 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, a.FilePath)
}

// LoadDir 扫描并解析 dir 下 workbuddy*.json；解析失败的文件静默跳过（启动日志由调用方统计）。
func LoadDir(dir string) ([]*Auth, error) {
	files, err := filepath.Glob(filepath.Join(dir, "workbuddy*.json"))
	if err != nil {
		return nil, err
	}
	var out []*Auth
	for _, f := range files {
		raw, err := os.ReadFile(f)
		if err != nil {
			continue
		}
		a, err := Parse(raw)
		if err != nil {
			continue
		}
		a.FilePath = f
		out = append(out, a)
	}
	return out, nil
}

// ---------------------------------------------------------------------------
// 区域（realm）判定
//
// 账号分两个区域：国服（cn，copilot.tencent.com / codebuddy.cn）与国际版
//（global，workbuddy.ai）。两者的可用模型、活动、端点都不同，
// 因此「把请求发给哪个区域的账号」是一个需要显式表达的约束。
//
// 判定依据是登录域名后缀，与 upstream 侧的口径一致。
// 放在 auth 包是因为 pool 需要它，而 pool 不能 import upstream（会循环依赖）。
// ---------------------------------------------------------------------------

// Realm 常量。
const (
	RealmCN     = "cn"
	RealmGlobal = "global"
)

// Realm 返回账号所属区域（"cn" / "global"）。
//
// 为什么保留这个字符串版（上游已有类型化的 Region）：模型名前缀协议
//（`cn:glm-5.2` / `global:...`）与前端展示用的都是字符串，growtask 的
// AccountResult.Realm 要直接序列化给界面。两者是**同一判定的两种表示**，
// 都委托给 IsIntl()，不存在两套独立逻辑（改判定只需改 IsIntl 一处）。
func (a *Auth) Realm() string {
	if a.IsIntl() {
		return RealmGlobal
	}
	return RealmCN
}