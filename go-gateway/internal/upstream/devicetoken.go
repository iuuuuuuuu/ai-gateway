// devicetoken.go 向宿主索取 WorkBuddy 设备 token（`X-Device-Token`）。
//
// # 背景（2026-09-28，所有者要求走 B 路线）
//
// 官方 WorkBuddy 客户端**每个请求**都带 `X-Device-Token`（`v3:` 形态，
// 约 1030 字符），而网关此前完全不带。该 token 由官方自带的腾讯
// TuringShield SDK 生成 —— 网关是 Go 进程，无法直接加载那个 N-API 模块，
// 故采用与 ZCode 验证码**完全相同**的架构：宿主开本地服务，网关 HTTP 调用。
//
// # 架构（与 captcha_webview.rs 同款）
//
//	网关(Go) ──HTTP POST /device-token──▶ 宿主本地服务(Rust)
//	                                        │
//	                                        └─ Node 子进程加载 turing_sdk.node
//	                                           返回 v3 token
//	◀──────── {"ok":true,"tokens":[...]} ───┘
//
// # 关键实测数据（决定本实现的形状）
//
//	生成耗时：首次 122ms，后续 39~51ms
//	**每次调用都不同**（按请求签发）⇒ 不能缓存固定值
//
// # 失败必须**静默降级**
//
// 拿不到 token 时**不发**该头，请求照常发出 —— 理由：
//
//	· 该头是"锦上添花"（防未来风控标记），不是准入条件
//	  （实测：不带它，健康账号照样 200）
//	· 若因拿不到 token 就拒绝请求，等于把"一个可选增强"
//	  变成"新的单点故障" —— 那是拿可用性换完整性，方向错
//
// 故所有错误路径都返回空串 + 记一行日志，绝不上抛。
package upstream

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"sync"
	"time"
)

// DeviceTokenProvider 向宿主服务索取设备 token。
//
// 零值不可用；用 NewDeviceTokenProvider 构造。nil 表示该特性未启用。
type DeviceTokenProvider struct {
	url   string
	token string
	http  *http.Client

	// mu 串行化并发请求。
	//
	// 为什么不并发取：turing-sdk 的 configure 是**进程级单例**且
	// "初始化后不可改 channelId"（实测报错原文：
	// "Turing SDK channelId cannot change after initialization"）。
	// 宿主侧那个 Node 子进程同样有单例语义，并发调用只会让它们互相干扰。
	// 而单次生成仅 40~50ms，串行完全够用。
	mu sync.Mutex
}

// NewDeviceTokenProvider 构造 provider；url 为空时返回 nil（= 不启用）。
func NewDeviceTokenProvider(url, token string) *DeviceTokenProvider {
	if strings.TrimSpace(url) == "" {
		return nil
	}
	return &DeviceTokenProvider{
		url:   url,
		token: token,
		http: &http.Client{
			// 超时取 3s：宿主侧实测 40~120ms，留足一个数量级余量；
			// 再长会让"宿主卡住"直接拖慢每个聊天请求。
			Timeout: 3 * time.Second,
		},
	}
}

// Enabled 报告该 provider 是否可用。
func (p *DeviceTokenProvider) Enabled() bool { return p != nil && p.url != "" }

// deviceTokenResp 宿主服务的响应体。
type deviceTokenResp struct {
	OK     bool     `json:"ok"`
	Tokens []string `json:"tokens"`
	Error  string   `json:"error"`
}

// Fetch 取一个设备 token。**任何失败都返回空串 + error**，
// 调用方应把空串当作"本次不发该头"处理，而不是拒绝请求。
func (p *DeviceTokenProvider) Fetch() (string, error) {
	if !p.Enabled() {
		return "", fmt.Errorf("device token provider 未启用")
	}

	p.mu.Lock()
	defer p.mu.Unlock()

	req, err := http.NewRequest(http.MethodPost, p.url, bytes.NewReader([]byte(`{"count":1}`)))
	if err != nil {
		return "", fmt.Errorf("构造请求失败: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")
	if p.token != "" {
		req.Header.Set("X-Device-Token-Service-Token", p.token)
	}

	resp, err := p.http.Do(req)
	if err != nil {
		return "", fmt.Errorf("调用宿主设备 token 服务失败: %w", err)
	}
	defer resp.Body.Close()

	raw, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return "", fmt.Errorf("读取响应失败: %w", err)
	}
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("宿主服务返回 %d: %s", resp.StatusCode, truncate(string(raw), 160))
	}

	var out deviceTokenResp
	if err := json.Unmarshal(raw, &out); err != nil {
		return "", fmt.Errorf("解析响应失败: %w", err)
	}
	if !out.OK || len(out.Tokens) == 0 {
		msg := out.Error
		if msg == "" {
			msg = "未返回 token"
		}
		return "", fmt.Errorf("宿主未给出设备 token: %s", msg)
	}
	tok := strings.TrimSpace(out.Tokens[0])
	if !strings.HasPrefix(tok, "v3:") {
		// 形态不符就当作失败 —— 宁可漏发，也不发一个上游不认的怪值。
		// （实测官方形态恒为 `v3:` 前缀、约 1030 字符。）
		return "", fmt.Errorf("设备 token 形态异常（应以 v3: 开头）")
	}
	return tok, nil
}
