package qoder

// dispatch.go 实现 server.QoderUpstream 接口（把 qoder 包接进网关的派发层）。
//
// ## 为什么要一个适配器而不是让 Client 直接实现接口
//
// server 的接口是**按 server 的需要**定义的（窄：发请求 / 聚合），
// 而 Client 的方法签名是为 qoder 自己的使用场景设计的。
// 直接让 Client 去满足 server 的接口会把两边的关注点绑死：
// 将来 server 想多要一个能力，就得改 Client 的公开 API。
//
// 适配器把"接口适配"这件事收在一个文件里，两边都能独立演进。
//
// ## 模型名映射的职责
//
// 客户端发来的是**它看到的模型名**（如 qwen3-max），而上游要的是**模型 key**
// （如 qmodel_preview）。映射表来自上游的模型列表接口，缓存在这里。
//
// 映射不到时的处理很关键：**回退用原样名字**而不是报错。
// 理由：上游的 key 可能变化，而"原样发过去"至少给了上游一个机会去理解；
// 直接报错会让用户看到"模型不存在"，而实际可能只是我们缓存过期了。
// 上游若真的不认识，它会回一个明确的错误，比我们猜错更好。

import (
	"context"
	"fmt"
	"io"
	"log"
	"strings"
	"sync"
	"time"

	"workbuddy2api/internal/auth"
)

// Dispatch 实现 server.QoderUpstream。
type Dispatch struct {
	client *Client

	// modelKeyOf 客户端模型名 → 上游模型 key（大小写不敏感）。
	//
	// 缓存策略：按账号区域分别缓存（两区的模型清单不同），
	// TTL 10 分钟 —— 上游加模型是低频事件，而每次请求都拉清单
	// 会白白消耗额度并增加延迟。
	mu       sync.RWMutex
	modelMap map[Region]modelCache
}

type modelCache struct {
	m       map[string]string
	fetched time.Time
}

// modelCacheTTL 模型清单的缓存时长。
//
// 10 分钟：上游加/改模型是低频事件；而缓存太久会让新模型在一段时间内
// 不可用（用户看到"模型不存在"）。这个折中与网关其它缓存口径一致。
const modelCacheTTL = 10 * time.Minute

// NewDispatch 构造派发适配器。
func NewDispatch(c *Client) *Dispatch {
	if c == nil {
		c = New()
	}
	return &Dispatch{client: c, modelMap: map[Region]modelCache{}}
}

// ChatStream 实现 server.QoderUpstream。
//
// 流程：解析客户端模型名 → 映射成上游 key → 构造 Qoder 请求体
//      → 编码 + 签名 → 发送 → 把嵌套 SSE 惰性翻译成 OpenAI 帧。
func (d *Dispatch) ChatStream(ctx context.Context, a *auth.Auth, openAIBody []byte) (io.ReadCloser, int, []byte, error) {
	cr := credOf(a)
	if cr == nil {
		return nil, 0, nil, fmt.Errorf("账号缺少 Qoder 凭证（Product=qoder 但凭证为空）")
	}

	// 客户端模型名 → 上游 key
	clientModel := modelNameOf(openAIBody)
	modelKey := d.resolveModelKey(ctx, cr, clientModel)

	// OpenAI 请求体 → Qoder 请求体
	agentBody, err := BuildAgentBody(openAIBody, modelKey)
	if err != nil {
		return nil, 0, nil, fmt.Errorf("构造 Qoder 请求体失败: %w", err)
	}

	// 令牌临近过期先刷新（与 WorkBuddy 路径的口径一致）。
	if cr.NeedsRefresh(refreshSkew) && cr.DRT != "" {
		if err := cr.RefreshToken(ctx, d.client.http()); err != nil {
			// 刷新失败不直接放弃：可能是网络抖动，用现有令牌试一次。
			// 真过期了上游会回 105，那时再报错也不迟。
			log.Printf("qoder dispatch uid=%s: 刷新令牌失败，沿用现有令牌: %v", a.UID, err)
		}
	}

	body, status, respBody, err := d.client.ChatStream(ctx, cr, agentBody)
	if err != nil {
		return nil, 0, nil, err
	}
	if body == nil {
		// 上游拒绝（status >= 400）：把原始响应体交给调用方分类处理。
		return nil, status, respBody, nil
	}

	// 把嵌套 SSE 翻译成标准 OpenAI SSE。
	// 调用方（server）只看到 OpenAI 形状，与 WorkBuddy 路径逐字一致。
	return NewOpenAIStream(body, clientModel), status, nil, nil
}

// Aggregate 实现 server.QoderUpstream（非流式聚合）。
func (d *Dispatch) Aggregate(r io.Reader, model string) (map[string]any, error) {
	return AggregateQoder(r, model)
}

// resolveModelKey 把客户端模型名解析成上游 key。
//
// 解析不到时**回退原样名字**（见文件头注释：给上游一个机会，
// 它若真不认识会回明确错误，比我们猜错更好）。
func (d *Dispatch) resolveModelKey(ctx context.Context, cr *Cred, clientModel string) string {
	if clientModel == "" {
		// 客户端没指定模型：用上游的第一个可用模型（而不是空 key —— 空 key 会被拒）
		if models := d.cachedModels(ctx, cr); len(models) > 0 {
			return models[0].Key
		}
		return ""
	}

	// 已经是上游 key 的形态（qmodel_ 前缀）→ 直接用
	if strings.HasPrefix(clientModel, "qmodel") {
		return clientModel
	}

	d.mu.RLock()
	cache, ok := d.modelMap[cr.Region]
	d.mu.RUnlock()
	if ok {
		if key, hit := cache.m[strings.ToLower(clientModel)]; hit {
			return key
		}
	}

	// 缓存未命中：刷新一次再查
	models := d.refreshModels(ctx, cr)
	lower := strings.ToLower(clientModel)
	for _, m := range models {
		if strings.ToLower(m.Key) == lower {
			return m.Key
		}
		if m.DisplayName != "" && strings.ToLower(NormalizeModelName(m.DisplayName)) == lower {
			return m.Key
		}
	}
	return clientModel // 回退原样
}

// cachedModels 返回缓存的模型清单（过期或缺失时刷新）。
func (d *Dispatch) cachedModels(ctx context.Context, cr *Cred) []DynamicModel {
	d.mu.RLock()
	cache, ok := d.modelMap[cr.Region]
	d.mu.RUnlock()
	if ok && time.Since(cache.fetched) < modelCacheTTL {
		return nil // 只有映射表，没有完整清单；调用方走 refresh
	}
	return d.refreshModels(ctx, cr)
}

// refreshModels 拉取并缓存模型清单。
//
// 失败时返回 nil（调用方回退原样名字）—— 不能因为"拉清单失败"
// 就让对话请求也失败：清单只是用来做名字映射的辅助信息。
func (d *Dispatch) refreshModels(ctx context.Context, cr *Cred) []DynamicModel {
	models, err := d.client.FetchModels(ctx, cr)
	if err != nil {
		log.Printf("qoder dispatch uid=%s: 拉取模型清单失败（将回退原样模型名）: %v", cr.UID, err)
		return nil
	}
	m := ResolveModelMap(models)
	d.mu.Lock()
	d.modelMap[cr.Region] = modelCache{m: m, fetched: time.Now()}
	d.mu.Unlock()
	return models
}

// ---------------------------------------------------------------------------
// auth.Auth ↔ qoder.Cred 的桥接
// ---------------------------------------------------------------------------

// credOf 从 auth.Auth 构造 Qoder 凭证。
//
// ## 为什么需要这个转换
//
// 网关的账号池统一用 auth.Auth（因为选号逻辑要跨产品共用），
// 而 Qoder 的签名需要额外的字段（机器指纹、dt/drt 的明确语义）。
// 两个结构各有存在的理由，桥接放在这一层。
//
// 令牌刷新后的写回：Cred 持有 FilePath，SaveAtomic 会写回原文件，
// 故刷新结果会持久化（与 WorkBuddy 的 SaveAtomic 同语义）。
func credOf(a *auth.Auth) *Cred {
	if a == nil {
		return nil
	}
	c := &Cred{
		UID:         a.UID,
		Nickname:    a.Nickname,
		DT:          a.AccessToken,
		DRT:         a.RefreshToken,
		DTExpiresAt: a.ExpiresAt,
		FilePath:    a.FilePath,
		// 区域由域名判定（与加载层口径一致）
		Region: RegionFromDomain(a.Domain),
	}
	if c.Region == RegionUnknown {
		// 域名认不出区域时按国服（endpointsOf 的默认行为），
		// 但**不写回** Domain —— 让用户能在界面上修正。
		c.Region = RegionCN
	}
	return c
}

// modelNameOf 从 OpenAI 请求体里取模型名。
func modelNameOf(body []byte) string { return modelKeyOf(body) }
