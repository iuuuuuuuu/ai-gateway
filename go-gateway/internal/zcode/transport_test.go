package zcode

// 回归测试：Transport 必须以 `http.DefaultTransport` 为模板。
//
// ## 为什么必须锁住这一条
//
// 这是一个**确定性**的坑，且失败现象极具误导性。实测对照
//（`uitest/diag-zcode-transport-compare.cjs`，各 8 次真实请求）：
//
//	配置                                        结果
//	──────────────────────────────────────────  ────────────
//	A 手工 `&http.Transport{}`                    ✓ 8/8
//	B 手工 + ForceAttemptHTTP2=true               ✓ 8/8
//	C **`(&http.Transport{}).Clone()`**           ✗ **0/8 全失败**
//	D `http.DefaultTransport.Clone()`             ✓ 8/8
//
// C 之所以坏：手工构造的 Transport 里 `ForceAttemptHTTP2` 是 false，
// `Clone()` 会触发 Go 的 `onceSetNextProtoDefaults()`，于是 ALPN 声明了
// h2 但 h2 处理器没注册 —— 服务端发 h2 帧、客户端按 h1 解析：
//
//	net/http: HTTP/1.x transport connection broken:
//	malformed HTTP response "\x00\x00\x12\x04..."
//
// 那串字节是 HTTP/2 的 SETTINGS 帧，但错误信息**完全不提协议版本**，
// 极易被误判成"上游返回坏数据"或"网络波动"（我误判了两次）。
//
// 这个测试**不需要网络** —— 它只断言 Transport 的关键字段，
// 而那正是决定 h2 行为的东西。

import (
	"net/http"
	"testing"
)

// TestTransportBasedOnDefault 生产客户端的 Transport 必须保留 h2 能力。
func TestTransportBasedOnDefault(t *testing.T) {
	c := New()

	tr, ok := c.HTTP.Transport.(*http.Transport)
	if !ok || tr == nil {
		t.Fatalf("Transport 应是 *http.Transport，实际 %T", c.HTTP.Transport)
	}

	// ForceAttemptHTTP2 是 h2 能否启用的关键开关。
	//
	// 手工构造的 `&http.Transport{}` 这里是 false —— 而 `Clone()` 之后
	// ALPN 仍会协商 h2，于是帧被当 h1 解析。故必须为 true。
	if !tr.ForceAttemptHTTP2 {
		t.Error(
			"ForceAttemptHTTP2 必须为 true —— 否则 Clone() 后 ALPN 协商成 h2 " +
				"但 h2 处理器缺失，请求会以 malformed HTTP response 失败。" +
				"修法：以 http.DefaultTransport 为模板 Clone，不要手工构造 &http.Transport{}",
		)
	}

	// 连接池参数应被我们覆盖过（证明走的是 New() 的配置路径）
	if tr.MaxIdleConnsPerHost != 20 {
		t.Errorf("MaxIdleConnsPerHost 应为 20，实际 %d", tr.MaxIdleConnsPerHost)
	}
}

// TestStreamHTTPKeepsH2 streamHTTP 的 clone 必须保持 h2 能力。
//
// 这是实际发流式请求用的 client —— 它若坏了，流式对话全部失败，
// 而错误信息仍是那句不提协议版本的 "malformed HTTP response"。
func TestStreamHTTPKeepsH2(t *testing.T) {
	c := New()
	sc := c.streamHTTP()

	tr, ok := sc.Transport.(*http.Transport)
	if !ok || tr == nil {
		t.Fatalf("流式 Transport 应是 *http.Transport，实际 %T", sc.Transport)
	}
	if !tr.ForceAttemptHTTP2 {
		t.Error("流式 client 的 ForceAttemptHTTP2 必须为 true（见 TestTransportBasedOnDefault）")
	}
	// 流式专用：响应头超时应设过（避免长回答被总超时掐断）
	if tr.ResponseHeaderTimeout <= 0 {
		t.Error("流式 client 应设 ResponseHeaderTimeout（否则长回答会被掐断）")
	}
	// 流式 client 不应带总超时
	if sc.Timeout != 0 {
		t.Errorf("流式 client 不应设总 Timeout（会掐断长回答），实际 %v", sc.Timeout)
	}
}

// TestNewIsIdempotent New() 多次调用互不影响。
//
// 若 New() 里直接改 `http.DefaultTransport`（而不是 Clone），
// 会污染**整个进程**的默认客户端 —— 那是很难排查的全局副作用。
func TestNewIsIdempotent(t *testing.T) {
	before := http.DefaultTransport.(*http.Transport).MaxIdleConnsPerHost

	c1 := New()
	tr1 := c1.HTTP.Transport.(*http.Transport)
	tr1.MaxIdleConnsPerHost = 999 // 故意改坏，验证不影响别人

	c2 := New()
	tr2 := c2.HTTP.Transport.(*http.Transport)
	if tr2.MaxIdleConnsPerHost != 20 {
		t.Errorf("第二个 New() 应得到干净的配置（20），实际 %d —— 说明共享了同一个 Transport", tr2.MaxIdleConnsPerHost)
	}

	after := http.DefaultTransport.(*http.Transport).MaxIdleConnsPerHost
	if before != after {
		t.Errorf("New() **不得**修改全局 http.DefaultTransport（%d → %d）", before, after)
	}
}
