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
	"bytes"
	"context"
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
func New() *Client {
	return &Client{
		HTTP: &http.Client{
			// 不用 http.Transport 的默认 0（无超时）：上游偶发挂起会让
			// 一次额度查询拖住整个界面刷新。
			Timeout: 60 * time.Second,
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
// body 必须与实际发送的字节完全一致 —— 签名覆盖它。
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

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, rawURL, bytes.NewReader(body))
	if err != nil {
		return nil, 0, nil, err
	}
	if err := sess.ApplyHeaders(req, string(body), rawURL, cr.UID, true, modelKeyOf(body)); err != nil {
		return nil, 0, nil, err
	}

	resp, err := c.http().Do(req)
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

// ---------------------------------------------------------------------------
// OpenAPI 域：只需 Bearer，不签名
// ---------------------------------------------------------------------------

// Quota 账号额度（剩余 / 总量）。
type Quota struct {
	// Remaining 剩余额度（用户套餐 + 附加包）。
	Remaining int64
	// Total 总额度。
	Total int64
	// Exceeded 上游是否判定已超额。
	Exceeded bool
	// PlanTierName 套餐名（可能为空）。
	PlanTierName string
}

// FetchQuota 查询额度（不签名）。
//
// 用途：跨产品路由的**成本信号**（剩余比例），以及界面上展示余额。
// 上游把额度分成 userQuota 与 addOnQuota 两块，本方法把两者相加 ——
// 用户关心的是"还能用多少"，分开显示没有意义。
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
			Remaining float64 `json:"remaining"`
		} `json:"userQuota"`
		AddOnQuota struct {
			Total     float64 `json:"total"`
			Remaining float64 `json:"remaining"`
		} `json:"addOnQuota"`
		IsQuotaExceeded bool   `json:"isQuotaExceeded"`
		PlanTierName    string `json:"planTierName"`
	}
	if err := json.Unmarshal(raw, &q); err != nil {
		return nil, fmt.Errorf("额度响应解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}

	return &Quota{
		Remaining:    int64(q.UserQuota.Remaining + q.AddOnQuota.Remaining),
		Total:        int64(q.UserQuota.Total + q.AddOnQuota.Total),
		Exceeded:     q.IsQuotaExceeded,
		PlanTierName: q.PlanTierName,
	}, nil
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
