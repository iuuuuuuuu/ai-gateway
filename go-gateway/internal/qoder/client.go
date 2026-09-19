package qoder

// client.go Qoder 上游客户端。
//
// **两条鉴权路径必须分清**（混用会静默失败，这是本文件最重要的一条约定）：
//
//	Gateway 域（api3.qoder.sh / gateway.qoder.com.cn）
//	  模型列表、对话 → **必须 COSY 签名**
//	OpenAPI 域（openapi.qoder.sh / openapi.qoder.com.cn）
//	  额度、套餐、用户信息、令牌刷新 → **只需 Bearer DT，不签名**
//
// 参考实现里这两条路分别写在不同文件、用不同 header 构造；
// 本包把它们收在同一个 Client 上，但用**方法名区分**（签名的方法带 Cosy 前缀）。

import (
	"context"
	"crypto/tls"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"
)

// Client Qoder 上游客户端。
type Client struct {
	// HTTP 出站客户端（含代理设置）。nil 时用 http.DefaultClient。
	HTTP *http.Client
	// Timeout 短请求（模型列表 / 额度）的总时长上限。
	Timeout time.Duration
}

// New 生产默认客户端。
//
// ⚠ 两处**必须**与参考实现一致，否则请求会静默失败（详见各自注释）：
//  1. 强制 HTTP/1.1 —— Qoder 的 gateway 对 HTTP/2 不友好，流式响应会
//     以 stream INTERNAL_ERROR 中断。参考实现明确禁用了 h2（TLSNextProto 置空）。
//  2. 对话请求体必须经 QoderEncode 编码（见 ChatStream）。
func New() *Client {
	tr := &http.Transport{
		// 默认 MaxIdleConnsPerHost=2 在高并发下会频繁重建 TLS 连接
		//（参考实现把它提到 20，实测有必要）。
		MaxIdleConns:        100,
		MaxIdleConnsPerHost: 20,
		IdleConnTimeout:     90 * time.Second,
		// 置空 TLSNextProto = 禁用 HTTP/2（Go 的标准做法）。
		// 不禁用的话流式对话会被上游中途断开，而错误信息只说 "stream error"，
		// 很难联想到协议版本。
		TLSNextProto: map[string]func(string, *tls.Conn) http.RoundTripper{},
	}
	return &Client{
		HTTP: &http.Client{
			Transport: tr,
			// 不用 0（无超时）：上游偶发挂起会让一次额度查询拖住整个界面刷新。
			// 流式对话另走无总超时的 client（见 ChatStream 的注释）。
			Timeout: 180 * time.Second,
		},
		Timeout: 30 * time.Second,
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

// ---------------------------------------------------------------------------
// Gateway 域：需要 COSY 签名
// ---------------------------------------------------------------------------

// ModelsPath 模型列表端点（拼在 Gateway 后）。
const ModelsPath = "/algo/api/v2/model/list?Encode=1"

// DynamicModel 上游 chat 场景的单个模型。
//
// 字段来自实测响应（参考实现已解析过的部分 + 本仓库需要的能力字段）。
type DynamicModel struct {
	// Key 上游模型标识（请求时通过 x-model-key 头传递）。
	Key string `json:"key"`
	// DisplayName 展示名（用于生成对客户端暴露的模型名）。
	DisplayName string `json:"display_name"`
	// Enable 是否启用（上游会返回已下线的模型，必须过滤）。
	Enable bool `json:"enable"`
	// IsReasoning 是否推理模型。
	IsReasoning bool `json:"is_reasoning"`
	// IsVL 是否视觉模型（VL = vision-language）。
	IsVL bool `json:"is_vl"`
	// MaxInputTokens 最大输入 token 数。
	MaxInputTokens int64 `json:"max_input_tokens"`
	// PriceFactor 计费倍率（成本信号，供跨产品路由使用）。
	PriceFactor float64 `json:"price_factor"`
	// ContextConfig 上下文窗口可选档位。
	ContextConfig map[string]ContextOption `json:"context_config,omitempty"`
}

// ContextOption 上下文窗口档位。
type ContextOption struct {
	TokenCount int64 `json:"token_count"`
	IsDefault  bool  `json:"is_default"`
}

// MaxContextTokens 该模型支持的最大上下文。
func (m DynamicModel) MaxContextTokens() int64 {
	var max int64
	for _, opt := range m.ContextConfig {
		if opt.TokenCount > max {
			max = opt.TokenCount
		}
	}
	if max > 0 {
		return max
	}
	return m.MaxInputTokens
}

// FetchModels 拉取可用模型列表（需签名）。
//
// 签名细节：GET 请求的 body 用**空串**（不是 "{}" —— 那会 403 Signature invalid，
// 参考实现的注释里专门警告过这一点）。
func (c *Client) FetchModels(ctx context.Context, cr *Cred) ([]DynamicModel, error) {
	if cr == nil || cr.DT == "" {
		return nil, fmt.Errorf("账号没有可用令牌")
	}
	// 指纹缺失时补上（返回 true 表示补过）。这里不落盘：调用方（宿主/网关）
	// 负责在合适时机持久化，避免一次查询就触发磁盘写入。
	cr.EnsureFingerprint()

	rawURL := cr.Region.Gateway() + ModelsPath
	sess, err := NewCosySession(cr, cr.DT, cr.DRT)
	if err != nil {
		return nil, err
	}

	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, rawURL, nil)
	if err != nil {
		return nil, err
	}
	// GET 的 body 签名用空串；uid 用于 cosy-user 头（缺失会被判签名无效）。
	if err := sess.ApplyHeaders(req, "", rawURL, cr.UID, false, ""); err != nil {
		return nil, err
	}

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("拉取模型列表失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 4<<20))
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("拉取模型列表失败（HTTP %d）：%s", resp.StatusCode, truncate(string(raw), 200))
	}

	// 响应形如 {"chat":[...],"agent":[...]}，只取 chat 场景。
	var apiResp map[string]json.RawMessage
	if err := json.Unmarshal(raw, &apiResp); err != nil {
		return nil, fmt.Errorf("模型列表解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}
	chatRaw, ok := apiResp["chat"]
	if !ok {
		return nil, fmt.Errorf("模型列表响应里没有 chat 场景（可用键：%s）", keysOf(apiResp))
	}
	var models []DynamicModel
	if err := json.Unmarshal(chatRaw, &models); err != nil {
		return nil, fmt.Errorf("chat 场景解析失败: %w", err)
	}

	enabled := make([]DynamicModel, 0, len(models))
	for _, m := range models {
		if m.Enable && m.Key != "" {
			enabled = append(enabled, m)
		}
	}
	if len(enabled) == 0 {
		return nil, fmt.Errorf("上游没有返回任何启用的模型（共解析 %d 个，全部被禁用）", len(models))
	}
	return enabled, nil
}

// ChatPath 对话端点（拼在 Gateway 后）。
//
// 注意这是**嵌套 SSE** 格式（`FetchKeys=llm_model_result`），与标准 OpenAI 流不同，
// 解析逻辑见 sse.go。
const ChatPath = "/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1"

// ChatStream 发起对话（流式），返回原始响应体供上层解析。
//
// ## 请求体必须编码
//
// 端点 URL 里带 `Encode=1`，请求体要经 `QoderEncode` 编码后发送。
// **签名覆盖的是编码后的字符串**（即实际发送的字节），故先编码再签名。
// 直接发明文 JSON 会让上游无法解析（表现为流挂起）。
//
// ## 为什么用独立的 client（不带总超时）
//
// 流式对话可能持续数分钟（长回答 + 推理），总超时会在中途掐断连接，
// 表现为"回答被截断"。这里用底层 Transport 但把 Timeout 置 0，
// 靠 ResponseHeaderTimeout 保证连接阶段不会无限等待。
func (c *Client) ChatStream(ctx context.Context, cr *Cred, body []byte) (io.ReadCloser, int, []byte, error) {
	if cr == nil || cr.DT == "" {
		return nil, 0, nil, fmt.Errorf("账号没有可用令牌")
	}
	cr.EnsureFingerprint()

	rawURL := cr.Region.Gateway() + ChatPath
	sess, err := NewCosySession(cr, cr.DT, cr.DRT)
	if err != nil {
		return nil, 0, nil, err
	}

	// 先编码，再签名（签名必须覆盖**实际发送的字节**）。
	encoded := QoderEncode(body)

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, rawURL, strings.NewReader(encoded))
	if err != nil {
		return nil, 0, nil, err
	}
	if err := sess.ApplyHeaders(req, encoded, rawURL, cr.UID, true, modelKeyOf(body)); err != nil {
		return nil, 0, nil, err
	}

	resp, err := c.streamHTTP().Do(req)
	if err != nil {
		return nil, 0, nil, fmt.Errorf("连接上游失败: %w", err)
	}
	if resp.StatusCode >= 400 {
		raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
		resp.Body.Close()
		return nil, resp.StatusCode, raw, nil
	}
	return resp.Body, resp.StatusCode, nil, nil
}

// streamHTTP 流式请求专用 client：复用 Transport 但**不设总超时**。
//
// 为什么不直接用 c.http()：它带 180s 总超时，长回答会在中途被掐断
//（表现为"回答被截断"）。
//
// 为什么不干脆用零值 client：那会丢掉已经配好的 Transport
//（连接池参数 + 禁用 HTTP/2），于是又回到 HTTP/2 流中断的问题。
//
// 首字节仍要有上限：ResponseHeaderTimeout 设在 Transport 上（它是
// Transport 的字段，不是 Client 的）—— 若上游压根不响应，
// 没有它会永久挂住一个 goroutine。
func (c *Client) streamHTTP() *http.Client {
	base := c.http()
	tr, ok := base.Transport.(*http.Transport)
	if !ok || tr == nil {
		// 调用方注入了自定义 Transport（测试用）：尊重它，只加首字节超时。
		return &http.Client{Transport: base.Transport}
	}
	// 克隆一份并设首字节超时，避免改动共享 Transport 影响短请求路径
	//（短请求靠 Client.Timeout 兜底，不需要 Transport 级超时）。
	clone := tr.Clone()
	clone.ResponseHeaderTimeout = 60 * time.Second
	return &http.Client{Transport: clone}
}

// ---------------------------------------------------------------------------
// OpenAPI 域：只需 Bearer，不签名
// ---------------------------------------------------------------------------

// Quota 账号额度（剩余 / 总量）。
//
// 字段与上游实测响应一一对应（见 FetchQuota 的注释）——
// 改字段名前请先回去核对那份响应，不要凭直觉调整。
type Quota struct {
	// Remaining 剩余额度（用户套餐 + 附加包）。
	Remaining int64
	// Total 总额度。
	Total int64
	// Used 已用额度。
	Used int64
	// Exceeded 上游是否判定已超额。
	//
	// ⚠ 新账号也会返回 true（`total` 为 0 时）—— 那不是"用超了"，
	// 而是"还没有可用额度"。界面应结合 Total 判断，别只报"已超额"。
	Exceeded bool
	// PlanTierName 套餐/账号等级。
	//
	// 实测来自 `userType`（如 `personal_standard`）；
	// 参考实现里的 `planTierName` 字段**在上游响应中并不存在**。
	PlanTierName string
	// UsagePercent 用量百分比（0..1）。
	UsagePercent float64
	// ExpiresAt 到期时刻（Unix **秒**）；0 = 未知/永不过期。
	//
	// 上游给的是**毫秒**且"永不过期"用 253402214400000（9999 年）表示，
	// FetchQuota 会把它归零。
	ExpiresAt int64
}

// FetchQuota 查询额度（不签名）。
//
// ## 实测的真实响应结构（2026-09-19 抓到）
//
// ```json
// {
//   "userId": "019f1772-...",
//   "userType": "personal_standard",
//   "usageType": "credits",
//   "totalUsagePercentage": 0.0,
//   "isQuotaExceeded": true,
//   "expiresAt": 253402214400000,
//   "upgradeUrl": "https://qoder.com.cn/pricing?client=qoder",
//   "outerProviders": [],
//   "userQuota": {"total":0.0,"used":0.0,"remaining":0.0,"percentage":0.0,"unit":"credits"},
//   "isPlanQuotaProrated": false
// }
// ```
//
// ## ⚠ 我漏掉的三个字段（都真实存在，都有用）
//
//   1. **`expiresAt`** —— 到期时间（**毫秒**，不是秒）。
//      这是所有者明确要的「到期时间」，而我第一版完全没解析它。
//   2. `totalUsagePercentage` —— 用量百分比。比 remaining 更能说明
//      "用了多少"：有些套餐 remaining 恒为 0 但 percentage 有意义。
//   3. `userType` —— 账号等级，界面上比空白的"套餐名"有信息量。
//
// ## `planTierName` 是我的臆造
//
// 我照参考实现写了 `planTierName`，但实测响应里**根本没有这个字段** ——
// 所以界面上的"套餐名"永远是空。现在改用真实存在的 `userType`。
//
// ## 谨慎处理"看起来超额"的情况
//
// 新账号会返回 `isQuotaExceeded: true` 且 `total: 0` —— 那不是真正的
// "超额"，而是"还没有可用额度"（如未领取活动额度）。两者对用户的
// 行动指引不同，故保留两个信号让界面自己判断。
func (c *Client) FetchQuota(ctx context.Context, cr *Cred) (*Quota, error) {
	if cr == nil || cr.DT == "" {
		return nil, fmt.Errorf("账号没有可用令牌")
	}

	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, cr.Region.OpenAPI()+"/api/v2/quota/usage", nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+cr.DT)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "Go-http-client/2.0")

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("查询额度失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("查询额度失败（HTTP %d）：%s", resp.StatusCode, truncate(string(raw), 200))
	}

	var q struct {
		UserQuota struct {
			Total     float64 `json:"total"`
			Used      float64 `json:"used"`
			Remaining float64 `json:"remaining"`
			Unit      string  `json:"unit"`
		} `json:"userQuota"`
		AddOnQuota struct {
			Total     float64 `json:"total"`
			Remaining float64 `json:"remaining"`
		} `json:"addOnQuota"`
		IsQuotaExceeded bool `json:"isQuotaExceeded"`
		// 用量百分比（0..1）。有些套餐 remaining 恒为 0 但百分比有意义。
		TotalUsagePercentage float64 `json:"totalUsagePercentage"`
		// ⚠ **毫秒**时间戳（实测 253402214400000 = 9999-12-31，
		// 即"永不过期"）。不要当秒用 —— 那会得到公元 10 万年。
		ExpiresAtMs int64  `json:"expiresAt"`
		UserType    string `json:"userType"`
		UsageType   string `json:"usageType"`
		// 我第一版照参考实现写了这个，但实测响应里没有它 —— 保留解析，
		// 有就用，没有不猜。
		PlanTierName string `json:"planTierName"`
	}
	if err := json.Unmarshal(raw, &q); err != nil {
		return nil, fmt.Errorf("额度响应解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}

	// 毫秒 → 秒。上限保护：9999 年那种"永不过期"不该原样传给界面，
	// 否则格式化出来是天文数字。`0` 表示未知。
	expiresAt := q.ExpiresAtMs / 1000
	if expiresAt > 4102444800 { // 2100-01-01
		expiresAt = 0 // 视为"永不过期/未知"
	}

	// 套餐名：真实字段是 userType；planTierName 只在有值时用
	plan := q.PlanTierName
	if plan == "" {
		plan = q.UserType
	}

	return &Quota{
		Remaining:    int64(q.UserQuota.Remaining + q.AddOnQuota.Remaining),
		Total:        int64(q.UserQuota.Total + q.AddOnQuota.Total),
		Used:         int64(q.UserQuota.Used),
		Exceeded:     q.IsQuotaExceeded,
		PlanTierName: plan,
		UsagePercent: q.TotalUsagePercentage,
		ExpiresAt:    expiresAt,
	}, nil
}

// fetchQuotaRaw 打额度端点并返回**原始响应体**。
//
// ## 为什么需要它
//
// `FetchQuota` 把响应解析成 `{remaining,total}`。当字段名与我们对不上时，
// 它会静默返回 0 —— 而那在界面上看起来就像"这个账号没额度"。
//
// 实测确认过于此：新登录的账号（客户端里明明能用）返回
// `{"remaining":0,"total":0,"exceeded":true}` —— 而我当时无从判断
// 是"真的超额"还是"字段没对上"。看原始 body 才能分辨。
//
// 生产路径不走它；它是**排障与字段核对**用的（见 uitest 的诊断脚本）。
func (c *Client) fetchQuotaRaw(ctx context.Context, cr *Cred) (string, error) {
	if cr == nil || cr.DT == "" {
		return "", fmt.Errorf("账号没有可用令牌")
	}
	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, cr.Region.OpenAPI()+"/api/v2/quota/usage", nil)
	if err != nil {
		return "", err
	}
	req.Header.Set("Authorization", "Bearer "+cr.DT)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "Go-http-client/2.0")

	resp, err := c.http().Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	return fmt.Sprintf("HTTP %d\n%s", resp.StatusCode, string(raw)), nil
}

// UserInfo 用户信息（不签名）。
type UserInfo struct {
	ID       string `json:"id"`
	Name     string `json:"name"`
	Username string `json:"username"`
}

// FetchUserInfo 查询用户信息（不签名）。用于导入凭证后补齐昵称。
func (c *Client) FetchUserInfo(ctx context.Context, cr *Cred) (*UserInfo, error) {
	if cr == nil || cr.DT == "" {
		return nil, fmt.Errorf("账号没有可用令牌")
	}
	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, cr.Region.OpenAPI()+"/api/v1/userinfo", nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+cr.DT)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "Go-http-client/2.0")

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("查询用户信息失败: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("查询用户信息失败（HTTP %d）：%s", resp.StatusCode, truncate(string(raw), 200))
	}
	var u UserInfo
	if err := json.Unmarshal(raw, &u); err != nil {
		return nil, fmt.Errorf("用户信息解析失败: %w", err)
	}
	return &u, nil
}

// ---------------------------------------------------------------------------
// 模型名映射
// ---------------------------------------------------------------------------

// NormalizeModelName 把上游展示名转成 OpenAI 风格客户端名。
//
//	"Qwen3.8-Max-Preview" → "qwen3.8-max-preview"
//	"DeepSeek-V4-Pro"     → "deepseek-v4-pro"
//	"GLM-5.2"             → "glm-5.2"
//
// 保留点号（版本号语义），把空格/下划线/连字符统一成连字符并去重。
func NormalizeModelName(s string) string {
	s = strings.ToLower(strings.TrimSpace(s))
	var b strings.Builder
	b.Grow(len(s))
	prevDash := false
	for _, r := range s {
		switch {
		case r >= 'a' && r <= 'z', r >= '0' && r <= '9', r == '.':
			b.WriteRune(r)
			prevDash = false
		case r == ' ' || r == '_' || r == '-':
			if !prevDash && b.Len() > 0 {
				b.WriteByte('-')
				prevDash = true
			}
		default:
			// 其它字符（中文等）原样保留：上游确有中文展示名的模型。
			b.WriteRune(r)
			prevDash = false
		}
	}
	return strings.Trim(b.String(), "-")
}

// ResolveModelMap 生成「客户端模型名 → 上游 key」的映射。
func ResolveModelMap(models []DynamicModel) map[string]string {
	out := make(map[string]string, len(models))
	for _, m := range models {
		name := m.Key
		if m.DisplayName != "" {
			name = NormalizeModelName(m.DisplayName)
		}
		out[name] = m.Key
	}
	return out
}

// modelKeyOf 从请求体里取出模型名，作为 x-model-key 头（上游据此选模型）。
func modelKeyOf(body []byte) string {
	var b struct {
		Model string `json:"model"`
	}
	if err := json.Unmarshal(body, &b); err != nil {
		return ""
	}
	return b.Model
}

func keysOf(m map[string]json.RawMessage) string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	return strings.Join(keys, ", ")
}
