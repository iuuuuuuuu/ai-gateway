package upstream

// 诊断：SetProxy + SetProxyScope 之后，国际版那一对 client 到底挂没挂代理？
//
// 背景（所有者报的真实缺陷）：配了代理，国际版账号流量仍不走代理。
// 已确证的事实链：
//   ① 探针代理能正常记录 CONNECT（工具已自证可信）
//   ② ChatStream 里加的 DIAG 埋点显示，网关**确实**选了 intlChatHTTP 分支
//   ③ 但探针代理零连接，请求却成功了（HTTP 200）
//   ⇒ 唯一解释：intlChatHTTP 存在，但它的 Transport **没有真的挂上代理**。
//
// 本文件用属性测试把「挂没挂」变成可断言的事实 —— 不靠读代码推断。

import (
	"net/http"
	"testing"

	"workbuddy2api/internal/auth"
)

// proxyOf 问 transport：给它一个真实形状的请求，Proxy() 返回什么。
//
// 这是最直接的判据：不看字段怎么赋值，只看**真正出站时会走哪个代理**。
//
// ⚠ 必须区分「Proxy 字段为 nil」与「Proxy 是个返回 nil 的函数」：
//   - 真直连的 transport（newDirectTransport）把 Proxy 字段设为 **nil**，
//     此时**直接调用** tr.Proxy(req) 会 nil 解引用 panic（实测踩到）。
//   - ProxyFromEnvironment 在无环境变量时返回 nil，但字段本身非 nil。
// 两者出站行为一样（都不走代理），但调用方式不同，故要分开处理。
func proxyOf(t *testing.T, label string, c *http.Client) string {
	t.Helper()
	if c == nil {
		return "<nil client>"
	}
	tr, ok := c.Transport.(*http.Transport)
	if !ok || tr == nil {
		return "<transport 不是 *http.Transport>"
	}
	if tr.Proxy == nil {
		return "<nil —— 没挂代理>"
	}
	req, _ := http.NewRequest("GET", "https://www.workbuddy.ai/", nil)
	pu, err := tr.Proxy(req)
	if err != nil {
		return "<Proxy() 报错: " + err.Error() + ">"
	}
	if pu == nil {
		return "<nil —— 没挂代理>"
	}
	return pu.String()
}

// TestProxyScopeAttachesProxyToIntlClients 国际版那一对 client 必须挂上代理。
//
// 这条测试是本次缺陷的**回归防线**：只要 intlHTTP/intlChatHTTP 存在，
// 它们的 Proxy() 就必须返回配置的代理地址。
func TestProxyScopeAttachesProxyToIntlClients(t *testing.T) {
	const addr = "http://127.0.0.1:57897"

	c := New()

	// New() 之后未配代理：国际版那一对**应为 nil**（既有契约，不能破）
	if c.intlHTTP != nil || c.intlChatHTTP != nil {
		t.Fatalf("New() 之后 intlHTTP/intlChatHTTP 应为 nil，实际 %v / %v",
			c.intlHTTP != nil, c.intlChatHTTP != nil)
	}

	if err := c.SetProxy(addr); err != nil {
		t.Fatalf("SetProxy 失败: %v", err)
	}

	// ⚠ 关键断言 1：SetProxy 之后，国际版**短 RPC** 必须挂着代理。
	//
	// 缺陷就藏在这里：若 SetProxy 只建了 intlHTTP 却没给它挂 transport，
	// 或者挂的是 ProxyFromEnvironment，那么本机没有 HTTPS_PROXY 时它
	// 就等于直连 —— 现象正是「配了代理却不走代理」。
	got := proxyOf(t, "intlHTTP", c.intlHTTP)
	if got != addr {
		t.Errorf("SetProxy 之后 intlHTTP 的代理应为 %s，实际 %s", addr, got)
	}

	// ⚠ 关键断言 2：国际版**聊天**同理（ChatStream 走的就是它）。
	gotChat := proxyOf(t, "intlChatHTTP", c.intlChatHTTP)
	if gotChat != addr {
		t.Errorf("SetProxy 之后 intlChatHTTP 的代理应为 %s，实际 %s", addr, gotChat)
	}

	// ⚠ 注意：**只调 SetProxy 不调 SetProxyScope** 时，国服那一对是
	// ProxyFromEnvironment（老行为：没填 scope 就只跟环境变量走，
	// 见 applyProxy 里 `scope == nil` 的分支）。因此这里**不能**断言它等于
	// 显式代理 —— 那是 SetProxyScope 的职责。国际版才是 SetProxy 就直接
	// 挂显式代理的那一路（老行为如此）。
	//
	// 本机没有 HTTPS_PROXY，所以国服的 Proxy() 返回 nil（= 实际直连），
	// 这与「挂了 ProxyFromEnvironment 但环境里没值」是同一现象。
	if got := proxyOf(t, "HTTP(仅SetProxy)", c.HTTP); got == addr {
		t.Logf("提示：仅 SetProxy 时国服也拿到了显式代理（%s）—— 若这是有意的，", got)
		t.Logf("     请在 SetProxy 的注释里写明；当前代码看起来是交给 SetProxyScope 决定。")
	}
}

// TestProxyScopeRespectsSwitches 两个开关必须精确生效。
//
// 所有者会按区域关掉代理（如「国服直连、国际版走代理」）。开关错位会让
// 一个本该国服直连的请求绕道代理，或反过来让国际版直连 —— 后者正是本次现象。
func TestProxyScopeRespectsSwitches(t *testing.T) {
	const addr = "http://127.0.0.1:57897"

	cases := []struct {
		name     string
		cn, intl bool
		// 期望：该区域的 client 的 Proxy() 返回值
		wantCN   string
		wantIntl string
	}{
		{
			name: "国际版开、国服关（默认口径）",
			cn:   false, intl: true,
			// 国服关 = 真直连（连环境变量也不用）→ Proxy() 必须是 nil
			wantCN: "<nil —— 没挂代理>",
			// 国际版开 = 显式代理
			wantIntl: addr,
		},
		{
			name: "两个都开",
			cn:   true, intl: true,
			wantCN:   addr,
			wantIntl: addr,
		},
		{
			name: "两个都关",
			cn:   false, intl: false,
			wantCN:   "<nil —— 没挂代理>",
			wantIntl: "<nil —— 没挂代理>",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			c := New()
			if err := c.SetProxy(addr); err != nil {
				t.Fatalf("SetProxy: %v", err)
			}
			if err := c.SetProxyScope(tc.cn, tc.intl); err != nil {
				t.Fatalf("SetProxyScope: %v", err)
			}

			// 国服：看短 RPC 那一套（HTTP）
			if got := proxyOf(t, "HTTP", c.HTTP); got != tc.wantCN {
				t.Errorf("国服 HTTP 期望 %s，实际 %s", tc.wantCN, got)
			}
			// 国际版：**优先**看聊天那一套（ChatStream 实际用的）
			if tc.intl && c.intlChatHTTP == nil {
				t.Errorf("intl=true 时 intlChatHTTP 不该为 nil（那会让国际版回落直连）")
			} else if c.intlChatHTTP != nil {
				if got := proxyOf(t, "intlChatHTTP", c.intlChatHTTP); got != tc.wantIntl {
					t.Errorf("国际版 intlChatHTTP 期望 %s，实际 %s", tc.wantIntl, got)
				}
			}
		})
	}
}

// TestIntlChatPathUsesProxiedClient 端到端：国际版账号选出的 client 必须挂着代理。
//
// 前两条测的是「transport 挂没挂」；这条测的是**选择逻辑**是否指向了那个
// 已挂代理的 client —— 两者分开测，才能在失败时立刻知道是「没挂上」还是
// 「选错了」。
func TestIntlChatPathUsesProxiedClient(t *testing.T) {
	const addr = "http://127.0.0.1:57897"

	c := New()
	if err := c.SetProxy(addr); err != nil {
		t.Fatalf("SetProxy: %v", err)
	}
	if err := c.SetProxyScope(false, true); err != nil {
		t.Fatalf("SetProxyScope: %v", err)
	}

	intl := &auth.Auth{UID: "probe-intl", Domain: "www.workbuddy.ai", AccessToken: "x"}
	chosen := c.chatClientFor(intl)
	if chosen == nil {
		t.Fatal("chatClientFor 返回 nil")
	}
	got := proxyOf(t, "chosen", chosen)
	if got != addr {
		t.Errorf("国际版账号选中的 chat client 应走代理 %s，实际 %s", addr, got)
	}

	// 反面：国服账号必须**不**走代理（国服开关是关的）
	cn := &auth.Auth{UID: "probe-cn", Domain: "copilot.tencent.com", AccessToken: "x"}
	gotCN := proxyOf(t, "cn", c.chatClientFor(cn))
	if gotCN != "<nil —— 没挂代理>" {
		t.Errorf("国服账号应直连（Proxy()=nil），实际 %s", gotCN)
	}
}
