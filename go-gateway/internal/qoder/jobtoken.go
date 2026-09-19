package qoder

// Qoder 的**短期作业令牌**（jobToken）刷新。
//
// # 这是什么（所有者实测提供的真实请求）
//
//	POST https://openapi.qoder.com.cn/api/v1/me/jobToken
//	Authorization: Bearer <dt-...>
//	Content-Type: application/json
//	{"clientId":"732aef47-9cf2-46a2-95fe-4cebb5d0d1fa"}
//
// 响应：
//
//	{"token":"jt-...","created_at":"...","expires_at":"...","expires_in":86400000,
//	 "refresh_token":"jrt-...","refresh_token_expires_at":"...",
//	 "refresh_token_expires_in":172800000}
//
// # 与 dt/drt 的区别（别搞混）
//
//	dt- / drt-   长期登录令牌（约 30 天 / 1 年），`deviceToken/refresh` 换新
//	jt- / jrt-   短期作业令牌（约 **1 天** / 2 天），本文件这套
//
// 为什么需要它：`dt` 虽然长期，但**推理请求**用的是另一套短期凭据。
// 客户端在启动时换一次 jobToken，然后拿它去调 `/algo/...`。
//
// # clientId 是什么
//
// 实测值是固定 UUID `732aef47-9cf2-46a2-95fe-4cebb5d0d1fa`。
// 我**不猜**它的语义（可能是客户端类型的注册标识），
// 只如实照抄 —— 猜错会让整个请求失败，而照抄是所有者实测可用的。

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

// jobTokenClientID 换取 jobToken 时带的 clientId。
//
// 值是所有者实测请求里的固定 UUID。**不要"优化"成随机值或省略** ——
// 服务端可能用它做客户端校验，改动会让请求失败。
const jobTokenClientID = "732aef47-9cf2-46a2-95fe-4cebb5d0d1fa"

// JobToken 一套短期作业令牌。
type JobToken struct {
	Token string `json:"token"`
	// ExpiresIn 毫秒（实测 86400000 = 1 天）。
	//
	// ⚠ 与 `deviceToken/refresh` 一样是**毫秒**。当秒用会得到
	// 1000 天，于是永远不会刷新。
	ExpiresIn int64  `json:"expires_in"`
	CreatedAt string `json:"created_at"`
	ExpiresAt string `json:"expires_at"`

	RefreshToken string `json:"refresh_token"`
	// RefreshTokenExpiresIn 毫秒（实测 172800000 = 2 天）。
	RefreshTokenExpiresIn int64  `json:"refresh_token_expires_in"`
	RefreshTokenExpiresAt string `json:"refresh_token_expires_at"`
}

// ExpiresAtTime 把 `expires_at`（RFC3339）解析成 time.Time；失败返回零值。
func (t *JobToken) ExpiresAtTime() time.Time {
	v, err := time.Parse(time.RFC3339, strings.TrimSpace(t.ExpiresAt))
	if err != nil {
		return time.Time{}
	}
	return v
}

// FetchJobToken 换取一套短期作业令牌。
//
// ⚠ 这是**写操作**（会签发新令牌），但幂等且无副作用：官方客户端每次
// 启动都会换一次。故可以安全地在"需要时"调用。
//
// 不缓存到磁盘：它只活 1 天，且换取成本极低；落盘反而多一份敏感数据。
// 调用方（网关）在内存里按账号缓存即可。
func (c *Client) FetchJobToken(ctx context.Context, cr *Cred) (*JobToken, error) {
	if cr == nil || cr.DT == "" {
		return nil, fmt.Errorf("账号没有可用令牌")
	}

	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	body, err := json.Marshal(map[string]string{"clientId": jobTokenClientID})
	if err != nil {
		return nil, err
	}

	url := campaignHostOf(cr) + "/api/v1/me/jobToken"
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+cr.DT)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("User-Agent", "Qoder/1.0")
	// 与活动接口同一套：这两个头在 openapi 上是通用的。
	req.Header.Set("Cosy-ClientType", campaignClientType)

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("换取作业令牌失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("换取作业令牌失败（HTTP %d）：%s", resp.StatusCode, truncate(string(raw), 200))
	}

	var t JobToken
	if err := json.Unmarshal(raw, &t); err != nil {
		return nil, fmt.Errorf("作业令牌响应解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}
	if t.Token == "" {
		return nil, fmt.Errorf("作业令牌响应里没有 token（原始内容：%s）", truncate(string(raw), 160))
	}
	return &t, nil
}
