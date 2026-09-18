package zcode

// client.go 上游调用：对话 / 额度 / 模型清单。
//
// ## 为什么只走 OpenAI 端点
//
// 上游同时提供 OpenAI 与 Anthropic 两种端点，而网关**内部就是 OpenAI 格式**
//（`/v1/chat/completions` 直通，另两个协议入口先转成 OpenAI 再进来）。
//
// 走上游 OpenAI 端点 = **零翻译**。走 Anthropic 端点则要双向翻译
//（请求 + SSE 流），多出两个易错点却没有任何收益。
//
// 顺带的好处：响应是**标准 OpenAI 形状**，可以直接复用网关既有的
// `upstream.Stream` / `upstream.Aggregate`，不需要像 Qoder 那样写翻译层。

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"
)

// Client ZCode 上游客户端。
type Client struct {
	// HTTP 出站客户端。nil 时用 New() 的默认（含连接池与超时）。
	HTTP *http.Client
	// Timeout 短请求（额度 / 模型清单）的总时长上限。
	Timeout time.Duration
	// Identity 身份头（伪装成官方客户端）。见 identity.go。
	Identity Identity
}

// New 生产默认客户端。
//
// ## ⚠ 关于 HTTP/2：**不要**照抄 Qoder 的"禁用 h2"
//
// Qoder 的 `New()` 里有 `TLSNextProto: map[...]{}`（置空）来禁用 h2 ——
// 那是必要的，因为 Qoder 的 gateway 对 h2 不友好、流会中断。
//
// **ZCode 相反：必须保留 h2。** 排查过程记录在这里，因为它很容易被误判：
//
// 现象：测试时报
//
//	malformed HTTP response "\x00\x00\x12\x04\x00\x00\x00\x00..."
//
// `\x00\x00\x12\x04` 是 HTTP/2 的 SETTINGS 帧（type=4）。Go 把 h2 帧
// 当 h1 响应文本解析 —— 这个错误信息**完全不提协议版本**，
// 很容易被误判成"上游返回了坏数据"或"网络问题"。
//
// 排查结论（三组对照，见 uitest/probe-zcode-transport-stability.cjs）：
//
//	A 默认（声明 h2 + 有 h2 处理器）      20/20
//	B 置空 TLSNextProto + NextProtos=h1   20/20（另有一次 context deadline）
//	C 置空 TLSNextProto，不设 NextProtos   20/20
//
// **三种配置都稳定通过** —— 说明那个报错**不是** transport 配置问题，
// 而是**网络层波动**（本机到上游的链路偶发不稳定）。复跑 6 轮实测全过。
//
// 我最初把原因归给"置空 TLSNextProto 禁用了 h2"，那是**错的**：
// Go 里 `TLSNextProto: nil` 与"不设该字段"是同一个意思（都会注册默认
// h2 处理器），只有设成**非 nil 的空 map** 才真的禁用。
//
// 故本配置保持 Go 的默认行为（支持 h2），这是最稳的：
//   · 上游尊重 ALPN（实测 12/12），声明 h2 就走 h2；
//   · 不引入任何非常规配置，行为与标准 Go 客户端一致。
func New() *Client {
	tr := &http.Transport{
		MaxIdleConns:        100,
		MaxIdleConnsPerHost: 20,
		IdleConnTimeout:     90 * time.Second,
		// 刻意保持默认：不禁用 h2（见上面的排查记录）。
	}
	return &Client{
		HTTP: &http.Client{
			Transport: tr,
			// 短请求 60s；流式对话另走无总超时的 client（见 streamHTTP）。
			Timeout: 60 * time.Second,
		},
		Timeout:  30 * time.Second,
		Identity: DefaultIdentity(),
	}
}

func (c *Client) http() *http.Client {
	if c.HTTP != nil {
		return c.HTTP
	}
	return http.DefaultClient
}

func (c *Client) timeout() time.Duration {
	if c.Timeout > 0 {
		return c.Timeout
	}
	return 30 * time.Second
}

// streamHTTP 流式请求专用 client：复用 Transport 但不设总超时。
//
// 为什么不直接用 c.http()：它带 60s 总超时，长回答会在中途被掐断
//（表现为"回答被截断"）。
func (c *Client) streamHTTP() *http.Client {
	base := c.http()
	tr, ok := base.Transport.(*http.Transport)
	if !ok || tr == nil {
		return &http.Client{Transport: base.Transport}
	}
	clone := tr.Clone()
	clone.ResponseHeaderTimeout = 60 * time.Second
	return &http.Client{Transport: clone}
}

// ---------------------------------------------------------------------------
// 对话
// ---------------------------------------------------------------------------

// ChatPath 对话端点路径（拼在 OpenAIBase 后）。
const ChatPath = "/chat/completions"

// StreamChat 发起流式对话，返回上游原始流。
//
// openAIBody 是客户端发来的 OpenAI 请求体 —— **原样透传**（见文件头注释：
// 上游收的也是 OpenAI 格式，无需翻译）。
//
// 返回值语义与 qoder.Client.ChatStream 一致：
//
//	rc != nil              → 成功，调用方负责 Close
//	rc == nil, status >= 400 → 上游拒绝，respBody 是原始响应体
func (c *Client) StreamChat(ctx context.Context, cr *Cred, openAIBody []byte) (io.ReadCloser, int, []byte, error) {
	if cr == nil || cr.Credential == "" {
		return nil, 0, nil, fmt.Errorf("账号没有凭证")
	}

	rawURL := cr.Provider.OpenAIBase() + ChatPath
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, rawURL, bytes.NewReader(openAIBody))
	if err != nil {
		return nil, 0, nil, err
	}
	c.applyHeaders(req, cr, true)

	resp, err := c.streamHTTP().Do(req)
	if err != nil {
		return nil, 0, nil, fmt.Errorf("连接 ZCode 上游失败: %w", err)
	}
	if resp.StatusCode >= 400 {
		raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
		resp.Body.Close()
		return nil, resp.StatusCode, raw, nil
	}
	return resp.Body, resp.StatusCode, nil, nil
}

// applyHeaders 写上游请求头。
//
// ## 认证头的两种形状（按端点区分）
//
//	OpenAI 端点     Authorization: Bearer {credential}
//	Anthropic 端点  x-api-key: {credential}  +  Authorization: Bearer {credential}
//	                +  anthropic-version: 2023-06-01
//
// 本包**只走 OpenAI 端点**，故只发 Authorization。Anthropic 那套保留在
// 注释里，便于将来切换时不用重新逆向。
//
// ## 身份头是可选的
//
// 实测（见 signing.go 的对照实验）：不带身份头也能通过认证。
// 照发的理由是"让代理在指纹层与官方客户端不可区分"（参考实现的注释口径），
// 成本为零而"被风控识别为第三方代理"的代价可能很高。
//
// 但**不因缺头而失败** —— 头缺失只是少一层伪装。
func (c *Client) applyHeaders(req *http.Request, cr *Cred, stream bool) {
	req.Header.Set("Authorization", "Bearer "+cr.Credential)
	req.Header.Set("Content-Type", "application/json")
	if stream {
		req.Header.Set("Accept", "text/event-stream")
	} else {
		req.Header.Set("Accept", "application/json")
	}
	// 身份头（伪装成官方客户端）
	for k, v := range c.Identity.Headers() {
		if v != "" {
			req.Header.Set(k, v)
		}
	}
}

// ---------------------------------------------------------------------------
// 额度
// ---------------------------------------------------------------------------

// Quota 账号额度。
type Quota struct {
	// Remaining 剩余额度（各 balance 项求和）。
	Remaining int64
	// Total 总额度。
	Total int64
	// Used 已用额度。
	Used int64
	// Entries 明细（可能有多项，如"编码套餐"+"赠送"）。
	Entries []QuotaEntry
	// ServerTime 上游服务器时间（Unix 秒）；0 = 未提供。
	ServerTime int64
}

// QuotaEntry 单条额度明细。
type QuotaEntry struct {
	ShowName   string
	Remaining  int64
	Total      int64
	Used       int64
	UnitType   string
	ExpiresAt  int64 // Unix 秒；0 = 未知
}

// ErrNoJWT 表示该账号没有 JWT，无法查询额度。
//
// 单独成型是刻意的：界面据此显示"额度未知（需要 OAuth 登录）"，
// 而不是显示 0 —— 0 会被用户误读成"额度耗尽"。
var ErrNoJWT = fmt.Errorf("该账号没有 JWT（仅导入了凭证），无法查询额度；请在账号页重新登录")

// FetchQuota 查询额度。
//
// ## 为什么用 JWT 而不是 Credential
//
// 参考实现明确写了：billing 接口用 OAuth 换来的 **jwt**（start-plan 令牌），
// 不是 `{apiKey}.{secret}`。故只导入了凭证的账号（方式 B）**查不到额度**。
//
// 这种情况返回 ErrNoJWT（而不是空结果）—— 让调用方能区分
// "没有 JWT" 与 "查到了但额度是 0"。
func (c *Client) FetchQuota(ctx context.Context, cr *Cred) (*Quota, error) {
	if cr == nil {
		return nil, fmt.Errorf("账号为空")
	}
	if strings.TrimSpace(cr.JWT) == "" {
		return nil, ErrNoJWT
	}

	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	// 参考实现的查询串：app_version + platform（billing 网关要求稳定指纹）
	q := fmt.Sprintf("/api/v1/zcode-plan/billing/balance?app_version=%s&platform=%s",
		c.Identity.AppVersion, c.Identity.PlatformArch())

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, cr.Provider.BizHost()+q, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+cr.JWT)
	req.Header.Set("Accept", "application/json")
	for k, v := range c.Identity.Headers() {
		if v != "" {
			req.Header.Set(k, v)
		}
	}

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("查询 ZCode 额度失败: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		return nil, NewError(resp.StatusCode, string(raw))
	}

	var doc struct {
		Code int    `json:"code"`
		Msg  string `json:"msg"`
		Data struct {
			Balances []struct {
				ShowName    string `json:"show_name"`
				Remaining   any    `json:"remaining_units"`
				Total       any    `json:"total_units"`
				Used        any    `json:"used_units"`
				UnitType    string `json:"unit_type"`
				UnitTypeAlt string `json:"unitType"`
				ExpiresAt   any    `json:"expires_at"`
				ExpiresAlt  any    `json:"expiresAt"`
			} `json:"balances"`
			ServerTime int64 `json:"server_time"`
		} `json:"data"`
	}
	if err := json.Unmarshal(raw, &doc); err != nil {
		return nil, fmt.Errorf("额度响应解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}
	// 业务码非 0 = 失败（上游用 HTTP 200 + code 报错）
	if doc.Code != 0 {
		return nil, &Error{Kind: Classify(resp.StatusCode, string(raw)), Status: resp.StatusCode,
			Code: fmt.Sprintf("%d", doc.Code), Msg: doc.Msg}
	}

	out := &Quota{ServerTime: doc.Data.ServerTime}
	for _, b := range doc.Data.Balances {
		e := QuotaEntry{
			ShowName:  b.ShowName,
			Remaining: toInt64(b.Remaining),
			Total:     toInt64(b.Total),
			Used:      toInt64(b.Used),
			UnitType:  firstNonEmpty(b.UnitType, b.UnitTypeAlt),
			// 字段名 snake_case 与 camelCase 都接受（参考实现两种都读，
			// 实测两种都存在）
			ExpiresAt: toInt64(firstAny(b.ExpiresAt, b.ExpiresAlt)),
		}
		out.Entries = append(out.Entries, e)
		out.Remaining += e.Remaining
		out.Total += e.Total
		out.Used += e.Used
	}
	return out, nil
}

// SoonestExpiry 返回明细中最早的到期时刻（Unix 秒）；0 = 全部未知。
//
// 用途：跨产品路由的**到期分层**（先烧快过期的额度）。
func (q *Quota) SoonestExpiry() int64 {
	var earliest int64
	for _, e := range q.Entries {
		if e.ExpiresAt <= 0 {
			continue
		}
		if earliest == 0 || e.ExpiresAt < earliest {
			earliest = e.ExpiresAt
		}
	}
	return earliest
}

// ---------------------------------------------------------------------------
// 模型清单
// ---------------------------------------------------------------------------

// Model 一个可用模型。
type Model struct {
	ID            string `json:"id"`
	Name          string `json:"name"`
	ContextWindow int64  `json:"contextWindow"`
	MaxOutput     int64  `json:"maxOutputTokens"`
	Reasoning     bool   `json:"reasoning"`
	// Vision 是否视觉模型。
	//
	// ⚠ 必须来自上游的 `capabilities.vision`，**不能用"id 里含 v"的启发式**。
	//
	// 实测（2026-09-19，uitest/probe-zcode-models.cjs）：上游当前返回的
	// `GLM-5.3-Flash` 带 `capabilities.vision=true`，而它的 id 里**没有 v** ——
	// 启发式会漏判，用户会以为模型不支持图片。
	//
	// （参考实现用的就是启发式，`routes-openai.ts` 的注释自认是 heuristic。）
	Vision bool `json:"vision"`
	// Source 该模型信息的来源（"upstream" / "builtin"）。
	//
	// 为什么记录来源：本项目既有约定是**不硬编码模型清单**。内置清单只是
	// 离线兜底，界面应能区分"上游真值"与"内置兜底"，否则清单过期时无人知道。
	Source string `json:"source"`
}

// ConfigPath 上游**免认证**的客户端配置端点（提供模型清单）。
//
// 实测（2026-09-19）：`GET https://zcode.z.ai/api/v1/client/configs` 返回 200，
// **无需任何认证**，含：
//
//	data.builtinModels[]   {modelId, name, contextWindow, maxCompletionTokens,
//	                        capabilities.vision, reasoning.defaultLevel}
//	data.builtinProviders[] {id, schema, baseUrl, models[]}
//
// ## 为什么用这个而不是硬编码
//
// 实测它只返回 **2 个模型**（GLM-5.3 / GLM-5.3-Flash），而参考实现硬编码了
// **11 个** —— 那 9 个是过时的。硬编码会让用户看到一堆用不了的模型。
//
// ## 为什么不用 `{base}/models`
//
// 那个端点存在（假凭证返回 401，说明活着），但**需要认证**；
// 而本接口免认证，且在拿到凭证前（登录阶段）就能用来展示可用模型。
const ConfigPath = "/api/v1/client/configs"

// ConfigOrigin 免认证配置端点的 origin。
const ConfigOrigin = "https://zcode.z.ai"

// FetchModels 拉取模型清单（**优先走免认证的 client/configs**）。
//
// 失败时返回错误 —— 调用方应回退到 BuiltinModels() 并标注来源。
func (c *Client) FetchModels(ctx context.Context, cr *Cred) ([]Model, error) {
	// 主路径：免认证的 client/configs（不依赖凭证，信息也最全）
	if models, err := c.fetchConfigModels(ctx); err == nil && len(models) > 0 {
		return models, nil
	}

	// 备选：走账号自己端点的 /models（需要认证，且通常只给 id）
	if cr == nil || cr.Credential == "" {
		return nil, fmt.Errorf("拉取模型清单失败（config 端点不可用，且账号没有凭证）")
	}
	return c.fetchProviderModels(ctx, cr)
}

// fetchConfigModels 从免认证的 client/configs 拉模型清单。
func (c *Client) fetchConfigModels(ctx context.Context) ([]Model, error) {
	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, ConfigOrigin+ConfigPath, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Accept", "application/json")
	// 这个端点免认证，但带上身份头更接近官方客户端行为
	for k, v := range c.Identity.Headers() {
		if v != "" {
			req.Header.Set(k, v)
		}
	}

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("拉取 ZCode 客户端配置失败: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 4<<20))
	if resp.StatusCode >= 400 {
		return nil, NewError(resp.StatusCode, string(raw))
	}

	var doc struct {
		Code int `json:"code"`
		Data struct {
			BuiltinModels []struct {
				ModelID             string `json:"modelId"`
				Name                string `json:"name"`
				ContextWindow       int64  `json:"contextWindow"`
				MaxCompletionTokens int64  `json:"maxCompletionTokens"`
				Capabilities        struct {
					Vision bool `json:"vision"`
				} `json:"capabilities"`
				Reasoning *struct {
					DefaultLevel string `json:"defaultLevel"`
				} `json:"reasoning"`
			} `json:"builtinModels"`
		} `json:"data"`
	}
	if err := json.Unmarshal(raw, &doc); err != nil {
		return nil, fmt.Errorf("客户端配置解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}
	if doc.Code != 0 {
		return nil, fmt.Errorf("客户端配置返回错误码 %d", doc.Code)
	}

	out := make([]Model, 0, len(doc.Data.BuiltinModels))
	for _, m := range doc.Data.BuiltinModels {
		id := strings.TrimSpace(m.ModelID)
		if id == "" {
			continue
		}
		out = append(out, Model{
			ID:            id,
			Name:          firstNonEmpty(m.Name, id),
			ContextWindow: m.ContextWindow,
			MaxOutput:     m.MaxCompletionTokens,
			// 视觉以 capabilities.vision 为准（不用启发式，见 Model.Vision 的注释）
			Vision:   m.Capabilities.Vision,
			Reasoning: m.Reasoning != nil,
			Source:   "upstream",
		})
	}
	if len(out) == 0 {
		return nil, fmt.Errorf("客户端配置里没有模型（builtinModels 为空）")
	}
	return out, nil
}

// fetchProviderModels 从账号自己端点的 /models 拉清单（备选路径）。
func (c *Client) fetchProviderModels(ctx context.Context, cr *Cred) ([]Model, error) {
	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	rawURL := cr.Provider.OpenAIBase() + ModelListPath
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, rawURL, nil)
	if err != nil {
		return nil, err
	}
	c.applyHeaders(req, cr, false)

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("拉取 ZCode 模型清单失败: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		return nil, NewError(resp.StatusCode, string(raw))
	}

	var doc struct {
		Data []struct {
			ID string `json:"id"`
		} `json:"data"`
	}
	if err := json.Unmarshal(raw, &doc); err != nil {
		return nil, fmt.Errorf("模型清单解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}
	if len(doc.Data) == 0 {
		return nil, fmt.Errorf("上游模型清单为空（原始内容：%s）", truncate(string(raw), 160))
	}

	// 用内置清单补齐能力字段（该端点通常只给 id）
	builtin := builtinModelIndex()
	out := make([]Model, 0, len(doc.Data))
	for _, m := range doc.Data {
		if strings.TrimSpace(m.ID) == "" {
			continue
		}
		if b, ok := builtin[strings.ToLower(m.ID)]; ok {
			b.Source = "upstream"
			out = append(out, b)
			continue
		}
		// 上游有但我们清单里没有的新模型：保守默认值 + 标注来源，
		// 而不是丢弃它（丢弃会让用户"看不到自己套餐里的新模型"）。
		out = append(out, Model{ID: m.ID, Name: m.ID, Source: "upstream"})
	}
	return out, nil
}

// ModelListPath 账号端点的模型清单路径（备选路径用）。
const ModelListPath = "/models"

// BuiltinModels 内置模型清单（**离线兜底**，非真值来源）。
//
// 来源：参考实现的 `provider/models.ts`（硬编码 11 个）。
//
// ⚠ **这是兜底，不是权威来源。** 实测（2026-09-19）上游 `client/configs`
// 当前只返回 **2 个模型**（GLM-5.3 / GLM-5.3-Flash），这 11 个里的大多数
// 已经过时。仅在上游接口不可用时使用，且 Source 标为 "builtin"
// 让界面能区分。
//
// 视觉标记沿用参考实现（`glm-4.6v` / `glm-5v-turbo`）—— 但注意那是**启发式**
// 的结果，实测上游的真实视觉模型是 `GLM-5.3-Flash`（id 里没有 v）。
// 这进一步说明：**不要依赖内置清单**，它只是网络不通时的降级显示。
func BuiltinModels() []Model {
	out := make([]Model, 0, len(builtinModels))
	for _, m := range builtinModels {
		m.Source = "builtin"
		out = append(out, m)
	}
	return out
}

func builtinModelIndex() map[string]Model {
	idx := make(map[string]Model, len(builtinModels))
	for _, m := range builtinModels {
		idx[strings.ToLower(m.ID)] = m
	}
	return idx
}

// builtinModels 内置清单（来源：参考实现 provider/models.ts）。
var builtinModels = []Model{
	{ID: "glm-4.5-air", Name: "GLM 4.5 Air", ContextWindow: 131072, MaxOutput: 98304, Reasoning: true},
	{ID: "glm-4.6", Name: "GLM 4.6", ContextWindow: 200000, MaxOutput: 131072, Reasoning: true},
	{ID: "glm-4.6v", Name: "GLM 4.6V", ContextWindow: 131072, MaxOutput: 32768},
	{ID: "glm-4.7", Name: "GLM 4.7", ContextWindow: 200000, MaxOutput: 131072, Reasoning: true},
	{ID: "glm-5", Name: "GLM 5", ContextWindow: 200000, MaxOutput: 64000, Reasoning: true},
	{ID: "glm-5-turbo", Name: "GLM 5 Turbo", ContextWindow: 200000, MaxOutput: 64000, Reasoning: true},
	{ID: "glm-5v-turbo", Name: "GLM 5V Turbo", ContextWindow: 200000, MaxOutput: 131072},
	{ID: "glm-5.1", Name: "GLM 5.1", ContextWindow: 200000, MaxOutput: 64000, Reasoning: true},
	{ID: "glm-5.2", Name: "GLM 5.2", ContextWindow: 1000000, MaxOutput: 128000, Reasoning: true},
	{ID: "glm-5.3", Name: "GLM 5.3", ContextWindow: 1000000, MaxOutput: 128000, Reasoning: true},
	{ID: "glm-5.3-flash", Name: "GLM 5.3 Flash", ContextWindow: 1000000, MaxOutput: 128000, Reasoning: true},
}

// ---------------------------------------------------------------------------
// 非流式聚合
// ---------------------------------------------------------------------------

// AggregateOpenAI 把标准 OpenAI SSE 流聚合成一个 chat.completion 响应。
//
// ## 为什么本包要实现它（而不复用 upstream.Aggregate）
//
// server 包的 `ProductUpstream` 接口要求每个产品**自洽** ——
// 它不能反向依赖 `upstream` 包（那会让"产品适配"与"WorkBuddy 实现"耦合）。
//
// ## 为什么这么简单（对比 Qoder）
//
// ZCode 上游的响应**本来就是标准 OpenAI 格式**（这是选 OpenAI 端点的收益），
// 故这里只需把 delta 拼起来，不需要像 Qoder 那样先解开嵌套的 `body` 字段。
func AggregateOpenAI(r io.Reader, model string) (map[string]any, error) {
	var content, reasoning strings.Builder
	finish := ""
	id := ""
	created := int64(0)
	modelOut := ""

	br := bufio.NewReaderSize(r, 64*1024)
	for {
		line, err := br.ReadString('\n')
		trimmed := strings.TrimSpace(line)
		if strings.HasPrefix(trimmed, "data:") {
			payload := strings.TrimSpace(strings.TrimPrefix(trimmed, "data:"))
			if payload == "[DONE]" {
				break
			}
			if payload != "" {
				var chunk struct {
					ID      string `json:"id"`
					Created int64  `json:"created"`
					Model   string `json:"model"`
					Choices []struct {
						Delta struct {
							Content          string `json:"content"`
							ReasoningContent string `json:"reasoning_content"`
						} `json:"delta"`
						Message struct {
							Content          string `json:"content"`
							ReasoningContent string `json:"reasoning_content"`
						} `json:"message"`
						FinishReason string `json:"finish_reason"`
					} `json:"choices"`
				}
				if json.Unmarshal([]byte(payload), &chunk) == nil {
					if chunk.ID != "" {
						id = chunk.ID
					}
					if chunk.Created != 0 {
						created = chunk.Created
					}
					if chunk.Model != "" {
						modelOut = chunk.Model
					}
					if len(chunk.Choices) > 0 {
						c := chunk.Choices[0]
						// delta 是流式增量；message 是非流式整体（有些实现两者都给）
						content.WriteString(firstNonEmpty(c.Delta.Content, c.Message.Content))
						reasoning.WriteString(firstNonEmpty(c.Delta.ReasoningContent, c.Message.ReasoningContent))
						if c.FinishReason != "" {
							finish = c.FinishReason
						}
					}
				}
			}
		}
		if err != nil {
			break // io.EOF 或读错误：已读到的内容仍返回
		}
	}

	if finish == "" {
		finish = "stop"
	}
	if id == "" {
		id = "chatcmpl-zcode"
	}
	if created == 0 {
		created = time.Now().Unix()
	}
	if modelOut == "" {
		modelOut = model
	}

	msg := map[string]any{"role": "assistant", "content": content.String()}
	// reasoning_content 只在非空时给：空字符串会让部分客户端显示空的思考块
	if reasoning.Len() > 0 {
		msg["reasoning_content"] = reasoning.String()
	}
	return map[string]any{
		"id":      id,
		"object":  "chat.completion",
		"created": created,
		"model":   modelOut,
		"choices": []any{map[string]any{
			"index":         0,
			"message":       msg,
			"finish_reason": finish,
		}},
	}, nil
}

// modelOf 从 OpenAI 请求体里取 model 字段。
func modelOf(body []byte) string {
	var b struct {
		Model string `json:"model"`
	}
	if err := json.Unmarshal(body, &b); err != nil {
		return ""
	}
	return b.Model
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

// toInt64 把上游可能返回的多种数值形态转成 int64。
//
// 上游有时返回数字、有时返回字符串（如 "1234"）。只认数字会让额度
// 静默变成 0 —— 而 0 会被误读成"额度耗尽"。
func toInt64(v any) int64 {
	switch x := v.(type) {
	case float64:
		return int64(x)
	case int64:
		return x
	case int:
		return int64(x)
	case string:
		var n float64
		if _, err := fmt.Sscanf(strings.TrimSpace(x), "%f", &n); err == nil {
			return int64(n)
		}
	case json.Number:
		if n, err := x.Int64(); err == nil {
			return n
		}
	}
	return 0
}

func firstAny(vals ...any) any {
	for _, v := range vals {
		if v == nil {
			continue
		}
		switch x := v.(type) {
		case string:
			if strings.TrimSpace(x) != "" {
				return v
			}
		case float64:
			if x != 0 {
				return v
			}
		default:
			return v
		}
	}
	return nil
}
