package zcode

// dispatch.go 实现 server.ProductUpstream 接口（把 zcode 包接进网关的派发层）。
//
// ## 与 Qoder 的实现差异（本文件比 Qoder 的简单得多）
//
//	维度          Qoder                        ZCode
//	────────────  ───────────────────────────  ─────────────────────────
//	请求体        Qoder 私有格式 + 编码          **OpenAI 原样透传**
//	响应形状      嵌套 SSE（需翻译）             **标准 OpenAI**（零翻译）
//	签名          COSY（必需）                  无（实测不需要）
//
// 所以本适配器只做两件事：**取模型名**（透传即可）与**转发**。
// 这正是 design.md D1 选 OpenAI 端点的收益 —— 省掉了整个翻译层。

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"strings"
	"sync"
	"time"

	"workbuddy2api/internal/auth"
)

// Dispatch 实现 server.ProductUpstream。
type Dispatch struct {
	client *Client

	// authDir 凭证目录（由 main.go 通过 SetAuthDir 告知）。
	//
	// 供排程的自动领取遍历凭证用。空串 = 用 DefaultAuthDir()。
	authDir string

	// 模型清单缓存（按服务商分别缓存）。
	//
	// 为什么缓存：模型清单来自免认证的 config 端点，但每次请求都拉
	// 会增加延迟（一次网络往返）。上游改模型是低频事件，10 分钟足够。
	mu       sync.RWMutex
	modelMap map[Provider]modelCache
}

type modelCache struct {
	// keys 客户端模型名（小写）→ 上游模型 id（本包用 id 直接作请求体里的 model）
	keys    map[string]string
	fetched time.Time
}

// modelCacheTTL 模型清单的缓存时长。
const modelCacheTTL = 10 * time.Minute

// NewDispatch 构造派发适配器。
func NewDispatch(c *Client) *Dispatch {
	if c == nil {
		c = New()
	}
	return &Dispatch{client: c, modelMap: map[Provider]modelCache{}}
}

// SetAuthDir 记录凭证目录（供产品日常任务读凭证）。
//
// # 为什么需要它（所有者 2026-09-20 要求"自动领取"）
//
// 排程的自动领取要遍历凭证并调 claim —— 而凭证目录是**部署期配置**
//（`pool.zcode_auth_dir`，见 cmd/server/main.go 的加载逻辑）。
//
// 不在 Dispatch 里硬编码默认值（那会让"配置指向别处"的部署读错目录，
// 而且错得静默），而是由 main.go 在接线时显式告知。
func (d *Dispatch) SetAuthDir(dir string) {
	if d == nil {
		return
	}
	d.authDir = dir
}

// LoadCreds 读取全部 ZCode 凭证（供产品日常任务用）。
//
// 目录为空时返回 `("", nil)` 的默认目录 —— 与 main.go 的加载口径一致
//（那里的 `if zcodeDir == "" { zcodeDir = zcode.DefaultAuthDir() }`）。
func (d *Dispatch) LoadCreds() (creds []*Cred, failed []string, err error) {
	if d == nil {
		return nil, nil, fmt.Errorf("zcode dispatch 未初始化")
	}
	dir := d.authDir
	if dir == "" {
		dir = DefaultAuthDir()
	}
	return LoadDir(dir)
}

// FetchPlanPreview 查该账号当前可领的套餐（转发到 Client）。
func (d *Dispatch) FetchPlanPreview(ctx context.Context, cr *Cred) (*PlanPreview, error) {
	return d.client.FetchPlanPreview(ctx, cr)
}

// ClaimPlan 领取指定套餐（转发到 Client）。
func (d *Dispatch) ClaimPlan(ctx context.Context, cr *Cred, planID string) (*ClaimResult, error) {
	return d.client.ClaimPlan(ctx, cr, planID)
}

// ChatStream 实现 server.ProductUpstream。
//
// 流程：取客户端模型名 → 校验/规范化 → 透传给上游。
//
// **注意**：与 Qoder 不同，这里**不需要翻译请求体** ——
// 上游的 OpenAI 端点收的就是 OpenAI 格式。故直接把 openAIBody 转发。
func (d *Dispatch) ChatStream(ctx context.Context, a *auth.Auth, openAIBody []byte) (io.ReadCloser, int, []byte, error) {
	cr := credOf(a)
	if cr == nil {
		return nil, 0, nil, fmt.Errorf("账号缺少 ZCode 凭证（Product=zcode 但凭证为空）")
	}

	// 模型名规范化（把客户端名映射成上游 id）。
	//
	// 映射不到时**回退原样**：上游若真不认识会回明确的错误，
	// 比我们猜错更好（与 qoder 的同名方法同一策略）。
	body := d.normalizeModel(ctx, cr, openAIBody)

	rc, status, respBody, err := d.client.StreamChat(ctx, cr, body)
	if err != nil {
		return nil, 0, nil, err
	}
	// 上游的响应**已经是标准 OpenAI**（流式与非流式都是），
	// 故不需要任何包装 —— 直接返回给调用方。
	return rc, status, respBody, nil
}

// Aggregate 实现 server.ProductUpstream（非流式聚合）。
//
// ZCode 的非流式响应也是标准 OpenAI 格式，故直接用网关既有的聚合器。
// 但 server 包的接口要求本方法自洽（不能反向依赖 upstream 包），
// 故这里实现一个等价的聚合。
func (d *Dispatch) Aggregate(r io.Reader, model string) (map[string]any, error) {
	return AggregateOpenAI(r, model)
}

// normalizeModel 把客户端模型名映射成上游 id（映射不到时回退原样）。
//
// ## 为什么需要映射（这不是"锦上添花"，是必需的）
//
// **上游严格区分大小写**，而客户端/内置清单用的是另一种写法。实测
// （2026-09-19，uitest/z7-live-zcode.cjs）用内置清单里的 `GLM-5.3`
// 发请求，上游回：
//
//	400 {"code":11102,"msg":"model [GLM-5.3] service info not found"}
//
// 而真实可用的 id 是**小写** `glm-5.3`（`GET /api/coding/paas/v4/models`
// 实测返回的 11 个模型全是小写：glm-4.5 / glm-4.6 / glm-5.3 / glm-5.3-flash …）。
//
// 若不做这层映射，用户看到的是"模型不存在"，会以为是上游不支持那个模型，
// 而真正原因是**大小写**。
//
// ## 映射不到时为什么回退而不是报错
//
// 上游的模型清单会变（实测当前 11 个，而参考实现硬编码了 11 个别的名字）。
// 若严格校验，**上游新增模型时会先失败一段时间**（我们的缓存还没刷新）；
// 回退原样让上游去判断，它若真不认识会回明确错误。
func (d *Dispatch) normalizeModel(ctx context.Context, cr *Cred, body []byte) []byte {
	name := modelNameOf(body)
	if name == "" {
		return body // 没指定模型：原样发，让上游用它自己的默认
	}

	d.mu.RLock()
	cache, ok := d.modelMap[cr.Provider]
	d.mu.RUnlock()

	// 缓存未命中或已过期 → 刷新
	if !ok || time.Since(cache.fetched) > modelCacheTTL {
		cache = d.refreshModels(ctx, cr)
	}

	if cache.keys != nil {
		key := strings.ToLower(strings.TrimSpace(name))
		if id, hit := cache.keys[key]; hit {
			if id != name {
				return replaceModel(body, id)
			}
			return body
		}
	}
	return body
}

// refreshModels 拉取并缓存模型映射表。
//
// 失败时返回空缓存（调用方回退原样名字）—— 不能因为"拉清单失败"
// 就让对话请求也失败：清单只是名字映射的辅助信息。
func (d *Dispatch) refreshModels(ctx context.Context, cr *Cred) modelCache {
	models, err := d.client.FetchModels(ctx, cr)
	if err != nil {
		log.Printf("zcode dispatch provider=%s: 拉取模型清单失败（将回退原样模型名）: %v",
			cr.Provider, err)
		return modelCache{fetched: time.Now()}
	}
	keys := make(map[string]string, len(models)*4)
	for _, m := range models {
		// 上游要的 id 是**权威值**（实测小写 glm-5.3）
		keys[strings.ToLower(m.ID)] = m.ID
		if m.Name != "" {
			// 显示名也建索引：客户端可能用「GLM-5.3」这种展示名请求，
			// 而 API 只认小写 —— 这层映射正是为了消除这个差异。
			keys[strings.ToLower(strings.TrimSpace(m.Name))] = m.ID
			keys[strings.ToLower(strings.ReplaceAll(m.Name, " ", "-"))] = m.ID
			keys[strings.ToLower(strings.ReplaceAll(m.Name, " ", ""))] = m.ID
		}
	}
	cache := modelCache{keys: keys, fetched: time.Now()}
	d.mu.Lock()
	d.modelMap[cr.Provider] = cache
	d.mu.Unlock()
	return cache
}

// ModelKeys 返回某服务商当前缓存的模型名清单（供诊断/测试）。
func (d *Dispatch) ModelKeys(provider Provider) []string {
	d.mu.RLock()
	defer d.mu.RUnlock()
	cache, ok := d.modelMap[provider]
	if !ok {
		return nil
	}
	out := make([]string, 0, len(cache.keys))
	for k := range cache.keys {
		out = append(out, k)
	}
	return out
}

// ---------------------------------------------------------------------------
// auth.Auth ↔ zcode.Cred 的桥接
// ---------------------------------------------------------------------------

// credOf 从 auth.Auth 构造 ZCode 凭证。
//
// 网关的账号池统一用 auth.Auth（选号逻辑要跨产品共用），而 ZCode 的调用
// 需要 Provider 与 JWT。桥接放在这一层。
//
// **服务商的判定**：优先用 auth.Auth.Domain（加载时写入），
// 它承载了服务商信息（见 main.go 的 zcodeAuthOf）。
func credOf(a *auth.Auth) *Cred {
	if a == nil {
		return nil
	}
	c := &Cred{
		UID:      a.UID,
		Nickname: a.Nickname,
		// ZCode 的凭证就是 AccessToken 字段（复用 auth.Auth 的令牌位）
		Credential: a.AccessToken,
		FilePath:   a.FilePath,
		Provider:   ProviderOfDomain(a.Domain),
	}
	if c.Provider == ProviderUnknown {
		// 域名认不出服务商时按 Z.AI（endpointsOf 的默认行为）
		c.Provider = ProviderZAI
	}
	return c
}

// ProviderOfDomain 从域名判定服务商（与加载层口径一致）。
//
// 为什么用 Domain 承载服务商：auth.Auth 已有 Domain 字段（WorkBuddy 用它
// 判区域），复用它避免给池加一个只对 ZCode 有意义的字段。
func ProviderOfDomain(domain string) Provider {
	d := strings.ToLower(strings.TrimSpace(domain))
	switch {
	case d == "":
		return ProviderUnknown
	case strings.Contains(d, "bigmodel"), strings.Contains(d, "zhipu"):
		return ProviderBigmodel
	case strings.Contains(d, "z.ai"), strings.Contains(d, "zai"):
		return ProviderZAI
	default:
		return ProviderUnknown
	}
}

// DomainOfProvider 反向映射（加载时写入 auth.Auth.Domain）。
func DomainOfProvider(p Provider) string {
	switch p {
	case ProviderBigmodel:
		return "open.bigmodel.cn"
	default:
		return "api.z.ai"
	}
}

// modelNameOf 从 OpenAI 请求体里取模型名。
func modelNameOf(body []byte) string { return modelOf(body) }

// replaceModel 替换请求体里的 model 字段（其余字段原样保留）。
//
// 为什么要逐字段重建而不是 unmarshal/marshal 整个对象：
// 后者会**丢失未知字段**（客户端可能带了上游支持但我们不认识的字段），
// 也会改变数字的表示（如 `1.0` → `1`）与键的顺序。
//
// 用 json.RawMessage 只替换 model 那一个值，其余字节完全不动。
func replaceModel(body []byte, newModel string) []byte {
	var doc map[string]json.RawMessage
	if err := json.Unmarshal(body, &doc); err != nil {
		return body // 解析失败：原样返回（不冒险改坏请求体）
	}
	quoted, err := json.Marshal(newModel)
	if err != nil {
		return body
	}
	doc["model"] = quoted
	out, err := json.Marshal(doc)
	if err != nil {
		return body
	}
	return out
}
