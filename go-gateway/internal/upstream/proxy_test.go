package upstream

import (
	"net/http"
	"testing"
)

// ---------------------------------------------------------------------------
// 出站代理支持
//
// 背景（实机排查）：国际版 workbuddy.ai 在国内直连不稳定（实测 wsarecv 超时），
// 走代理才稳。而 Go 的 http.ProxyFromEnvironment **只读环境变量**、不读 Windows
// 注册表，所以「浏览器能走系统代理」不代表网关也能 —— 必须显式配置。
// ---------------------------------------------------------------------------

// TestSetProxyAppliesToBothClients 代理必须同时作用到 HTTP 与 ChatHTTP。
func TestSetProxyAppliesToBothClients(t *testing.T) {
	c := New()
	if err := c.SetProxy("http://127.0.0.1:7890"); err != nil {
		t.Fatalf("设置代理失败: %v", err)
	}

	for _, tc := range []struct {
		name   string
		client *http.Client
	}{{"HTTP", c.HTTP}, {"ChatHTTP", c.ChatHTTP}} {
		tr, ok := tc.client.Transport.(*http.Transport)
		if !ok {
			t.Fatalf("%s 的 Transport 类型=%T", tc.name, tc.client.Transport)
		}
		if tr.Proxy == nil {
			t.Fatalf("%s 未设置 Proxy 函数", tc.name)
		}
		req, _ := http.NewRequest("GET", "https://www.workbuddy.ai/v2/chat/completions", nil)
		got, err := tr.Proxy(req)
		if err != nil {
			t.Fatalf("%s 解析代理失败: %v", tc.name, err)
		}
		if got == nil || got.String() != "http://127.0.0.1:7890" {
			t.Fatalf("%s 代理=%v，期望 http://127.0.0.1:7890", tc.name, got)
		}
	}
}

// TestSetProxyKeepsTransportShared 设代理后仍须共享同一个 Transport（连接池不重复）。
func TestSetProxyKeepsTransportShared(t *testing.T) {
	c := New()
	if err := c.SetProxy("http://127.0.0.1:7890"); err != nil {
		t.Fatal(err)
	}
	if c.ChatHTTP.Transport != c.HTTP.Transport {
		t.Error("设代理后 ChatHTTP 与 HTTP 仍须共享同一个 Transport")
	}
	// 总时长的差异必须保持：ChatHTTP 无总时长，靠 ResponseHeaderTimeout 兜底
	if c.ChatHTTP.Timeout != 0 {
		t.Errorf("ChatHTTP.Timeout=%v 应为 0", c.ChatHTTP.Timeout)
	}
	if c.HTTP.Timeout == 0 {
		t.Error("HTTP.Timeout 不应为 0（短 RPC 需要总时长上限）")
	}
}

// TestSetProxyTolerateHostPortOnly 用户常只填 host:port，应自动补 http:// 前缀。
func TestSetProxyTolerateHostPortOnly(t *testing.T) {
	c := New()
	if err := c.SetProxy("127.0.0.1:7890"); err != nil {
		t.Fatalf("应容忍无 scheme 的写法: %v", err)
	}
	tr := c.ChatHTTP.Transport.(*http.Transport)
	req, _ := http.NewRequest("GET", "https://example.com", nil)
	got, _ := tr.Proxy(req)
	if got == nil || got.Host != "127.0.0.1:7890" {
		t.Fatalf("代理 host=%v，期望 127.0.0.1:7890", got)
	}
}

// TestSetProxyEmptyFallsBackToEnv 空串 = 不用显式代理，回落环境变量（保持旧行为）。
func TestSetProxyEmptyFallsBackToEnv(t *testing.T) {
	c := New()
	// 先设一个显式代理，再清空 —— 这样才有一个**确定性**可断言的对象：
	// 「清空之后不得再指向刚设的那个地址」。
	//
	// 不能用 t.Setenv 清空 HTTPS_PROXY 后断言「必然直连」：httpproxy 的配置在
	// 进程内**首次读取即缓存**（net/http 的 envProxyOnce），t.Setenv 无法生效；
	// 而且开发机常有 HTTP_PROXY/HTTPS_PROXY（实测 127.0.0.1:7897），
	// 断言「直连」会让用例依赖运行环境（CI 绿、本机红）。
	const explicit = "http://127.0.0.1:1"
	if err := c.SetProxy(explicit); err != nil {
		t.Fatal(err)
	}
	if c.ProxyURL() != explicit {
		t.Fatalf("ProxyURL()=%q，应为 %q", c.ProxyURL(), explicit)
	}
	if err := c.SetProxy(""); err != nil {
		t.Fatal(err)
	}
	if c.ProxyURL() != "" {
		t.Errorf("空串时 ProxyURL()=%q，应为空", c.ProxyURL())
	}
	tr := c.ChatHTTP.Transport.(*http.Transport)
	if tr.Proxy == nil {
		t.Fatal("仍应有 Proxy 函数（回落 ProxyFromEnvironment）")
	}
	req, _ := http.NewRequest("GET", "https://www.workbuddy.ai", nil)
	got, err := tr.Proxy(req)
	if err != nil {
		t.Fatalf("Proxy(req) 报错: %v", err)
	}
	if got != nil && got.String() == explicit {
		t.Fatalf("清空后仍指向刚设的显式代理 %s —— SetProxy(\"\") 没有回落环境变量", explicit)
	}
	// 与标准库行为一致即可（环境变量可能有值，且被首次读取缓存，故不断言必然为 nil）。
	want, werr := http.ProxyFromEnvironment(req)
	if werr != nil {
		t.Fatalf("ProxyFromEnvironment 报错: %v", werr)
	}
	if (got == nil) != (want == nil) {
		t.Errorf("空串应回落 ProxyFromEnvironment：got=%v want=%v", got, want)
	} else if got != nil && got.String() != want.String() {
		t.Errorf("空串应回落 ProxyFromEnvironment：got=%v want=%v", got, want)
	}
}

// TestSetProxyRejectsInvalid 非法地址必须报错（而不是静默直连）。
func TestSetProxyRejectsInvalid(t *testing.T) {
	c := New()
	for _, bad := range []string{"http://", "://nohost", "http://:8080"} {
		if err := c.SetProxy(bad); err == nil {
			t.Errorf("%q 应被拒绝", bad)
		}
	}
}

// TestSetProxyAcceptsSocks5 socks5 代理地址应被接受（用户在 Clash 等里可能用它）。
func TestSetProxyAcceptsSocks5(t *testing.T) {
	c := New()
	if err := c.SetProxy("socks5://127.0.0.1:7891"); err != nil {
		t.Fatalf("socks5 地址应可解析: %v", err)
	}
	if c.ProxyURL() != "socks5://127.0.0.1:7891" {
		t.Errorf("ProxyURL()=%q", c.ProxyURL())
	}
	tr := c.ChatHTTP.Transport.(*http.Transport)
	req, _ := http.NewRequest("GET", "https://example.com", nil)
	got, _ := tr.Proxy(req)
	if got == nil || got.Scheme != "socks5" {
		t.Fatalf("scheme=%v，期望 socks5", got)
	}
}

// TestNewDefaultsToEnvProxy 未调 SetProxy 时，New() 的 Transport 应已挂
// ProxyFromEnvironment（尊重 HTTPS_PROXY，行为与改动前一致）。
//
// 注意：Go 的 httpproxy 配置在**首次读取时缓存**（internal/httpproxy 的
// onceClose 语义），t.Setenv 之后就生效需要重置缓存。这里直接用
// http.ProxyURL 与 ProxyFromEnvironment 的等价性断言，不依赖缓存时序：
// 只要 Transport.Proxy 非 nil 且是 ProxyFromEnvironment 的返回值语义即可。
func TestNewDefaultsToEnvProxy(t *testing.T) {
	c := New()
	tr := c.ChatHTTP.Transport.(*http.Transport)
	if tr.Proxy == nil {
		t.Fatal("New() 应默认挂 ProxyFromEnvironment（而非 nil）")
	}
	// 与标准库的 ProxyFromEnvironment 行为一致性：同一个请求应得到同样结果。
	// （直接函数值比较不可靠，因为 http.Transport 可能包装过。）
	req, _ := http.NewRequest("GET", "https://www.workbuddy.ai", nil)
	got, err := tr.Proxy(req)
	if err != nil {
		t.Fatalf("Proxy(req) 报错: %v", err)
	}
	want, werr := http.ProxyFromEnvironment(req)
	if werr != nil {
		t.Fatalf("ProxyFromEnvironment 报错: %v", werr)
	}
	if (got == nil) != (want == nil) {
		t.Errorf("与 ProxyFromEnvironment 行为不一致：got=%v want=%v", got, want)
	}
	if got != nil && want != nil && got.String() != want.String() {
		t.Errorf("与 ProxyFromEnvironment 结果不一致：got=%v want=%v", got, want)
	}
}
