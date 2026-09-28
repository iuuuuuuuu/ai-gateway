package upstream

// devicetoken_test.go 锁定 WorkBuddy 设备 token（`X-Device-Token`）的注入与降级。
//
// # 守的是什么（2026-09-28，所有者要求走 B 路线）
//
// 官方 WorkBuddy 客户端每个请求都带 `X-Device-Token`（`v3:` 形态，约 1030 字符），
// 网关此前完全不带。该 token 由官方自带的腾讯 TuringShield SDK 生成，
// 网关（Go）无法直接加载那个 N-API 模块，故经宿主本地服务转发
//（架构与 ZCode 验证码求解同款）。
//
// # 两组断言，缺一不可
//
//	① **带上了**：配了服务且拿得到 token 时，国际版请求必须带 `X-Device-Token`
//	② **降级**：拿不到 token 时**不发该头**，请求照常发出 ——
//	   这是最关键的一条：它保证"设备凭证"这个**可选增强**
//	   永远不会变成"国际版对话全挂"的单点故障。
//
// 另外锁定：**国服不发**（我们只有国际版的抓包证据，不该凭"国际版有"
// 就推断国服也要）。

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
)

// fakeDeviceTokenService 起一个假的宿主设备 token 服务。
//
// 返回的 srv 会记录被调用次数，供"是否真的去取了"的断言使用。
func fakeDeviceTokenService(t *testing.T, token string, fail bool) (*httptest.Server, *int32) {
	t.Helper()
	var calls int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&calls, 1)
		// 鉴权头必须按约定发送（与 Rust 侧 handle() 的校验一致）
		if got := r.Header.Get("X-Device-Token-Service-Token"); got != "svc-token" {
			w.WriteHeader(http.StatusUnauthorized)
			_, _ = io.WriteString(w, `{"ok":false,"error":"unauthorized"}`)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		if fail {
			_, _ = io.WriteString(w, `{"ok":false,"error":"node 不可用"}`)
			return
		}
		body, _ := json.Marshal(map[string]any{"ok": true, "tokens": []string{token}})
		_, _ = w.Write(body)
	}))
	t.Cleanup(srv.Close)
	return srv, &calls
}

// captureChatRequest 起一个假上游，把收到的 chat 请求头回传出来。
func captureChatRequest(t *testing.T, seen *http.Header) *httptest.Server {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if strings.Contains(r.URL.Path, "chat/completions") {
			*seen = r.Header.Clone()
		}
		w.Header().Set("Content-Type", "text/event-stream")
		_, _ = io.WriteString(w, "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n")
	}))
	t.Cleanup(srv.Close)
	return srv
}

const sampleDeviceToken = "v3:AAAAAaDleH/S5OaPPFdm5gQXdsnAAAAAAAAAAAAAAAAAAAAAAAAAAAA"

// TestDeviceTokenAttachedForIntl 配了服务时，国际版请求必须带上该头。
func TestDeviceTokenAttachedForIntl(t *testing.T) {
	svc, calls := fakeDeviceTokenService(t, sampleDeviceToken, false)
	var seen http.Header
	up := captureChatRequest(t, &seen)

	c := routableClient(t, up.URL)
	c.DeviceToken = NewDeviceTokenProvider(svc.URL, "svc-token")

	rc, status, _, err := c.ChatStream(intlAuth(), []byte(`{"model":"m","messages":[]}`))
	if err != nil {
		t.Fatalf("ChatStream 出错: %v", err)
	}
	if rc != nil {
		rc.Close()
	}
	if status != 200 {
		t.Fatalf("期望 200，实际 %d", status)
	}
	if got := seen.Get("X-Device-Token"); got != sampleDeviceToken {
		t.Errorf("国际版请求应带 X-Device-Token=%q，实际 %q", sampleDeviceToken, got)
	}
	if atomic.LoadInt32(calls) == 0 {
		t.Error("应真的去调用了宿主设备 token 服务")
	}
}

// TestDeviceTokenNotAttachedForCN 国服**不发**该头。
//
// 理由：抓包证据只覆盖国际版（workbuddy.ai）。官方国服客户端走的是另一套
// （codebuddy.cn / copilot.tencent.com），我们没有它的证据 ——
// 不该凭"国际版有"就推断"国服也要"。多发给国服一个它不认的头，
// 风险大于收益（国服当前实测良好，不动它）。
func TestDeviceTokenNotAttachedForCN(t *testing.T) {
	svc, calls := fakeDeviceTokenService(t, sampleDeviceToken, false)
	var seen http.Header
	up := captureChatRequest(t, &seen)

	c := routableClient(t, up.URL)
	c.DeviceToken = NewDeviceTokenProvider(svc.URL, "svc-token")

	rc, status, _, err := c.ChatStream(cnAuth(), []byte(`{"model":"m","messages":[]}`))
	if err != nil {
		t.Fatalf("ChatStream 出错: %v", err)
	}
	if rc != nil {
		rc.Close()
	}
	if status != 200 {
		t.Fatalf("期望 200，实际 %d", status)
	}
	if got := seen.Get("X-Device-Token"); got != "" {
		t.Errorf("国服请求**不该**带 X-Device-Token（无抓包证据），实际 %q", got)
	}
	if n := atomic.LoadInt32(calls); n != 0 {
		t.Errorf("国服不该去调用设备 token 服务，实际调用了 %d 次", n)
	}
}

// TestDeviceTokenDegradesWhenServiceFails ★ 最关键的一条：拿不到就**不发**，
// 请求照常成功。
//
// # 为什么这是最关键的一条
//
// 该头是"锦上添花"（防未来风控标记），**不是准入条件** ——
// 实测不带它，健康账号照样 200。
//
// 若因拿不到就拒绝请求，等于把一个**可选增强**变成**新的单点故障**：
// 宿主服务没起来 / node 缺失 / 用户没装官方客户端，都会让所有国际版对话不可用。
//
// 本用例锁死"降级而非失败"这个取舍。
func TestDeviceTokenDegradesWhenServiceFails(t *testing.T) {
	svc, _ := fakeDeviceTokenService(t, "", true) // 服务返回 ok:false
	var seen http.Header
	up := captureChatRequest(t, &seen)

	c := routableClient(t, up.URL)
	c.DeviceToken = NewDeviceTokenProvider(svc.URL, "svc-token")

	rc, status, _, err := c.ChatStream(intlAuth(), []byte(`{"model":"m","messages":[]}`))
	if err != nil {
		t.Fatalf("拿不到设备 token 时**不该**让请求失败，实际出错: %v", err)
	}
	if rc != nil {
		rc.Close()
	}
	if status != 200 {
		t.Fatalf("拿不到设备 token 时请求应照常成功（降级），实际 %d", status)
	}
	if got := seen.Get("X-Device-Token"); got != "" {
		t.Errorf("服务失败时不该发该头，实际 %q", got)
	}
}

// TestDeviceTokenDegradesWhenServiceUnreachable 服务**连不上**时同样降级。
//
// 与上一条的区别：那条是"服务在但说失败"，这条是"服务根本不在"
// （端口没人听）。两者都要降级，不能只处理一种。
func TestDeviceTokenDegradesWhenServiceUnreachable(t *testing.T) {
	var seen http.Header
	up := captureChatRequest(t, &seen)

	c := routableClient(t, up.URL)
	// 127.0.0.1:1 上不会有服务在听
	c.DeviceToken = NewDeviceTokenProvider("http://127.0.0.1:1/device-token", "svc-token")

	rc, status, _, err := c.ChatStream(intlAuth(), []byte(`{"model":"m","messages":[]}`))
	if err != nil {
		t.Fatalf("服务连不上时**不该**让请求失败，实际出错: %v", err)
	}
	if rc != nil {
		rc.Close()
	}
	if status != 200 {
		t.Fatalf("服务连不上时请求应照常成功（降级），实际 %d", status)
	}
}

// TestDeviceTokenNotConfigured 未配置服务时行为与本特性引入前**逐字相同**。
//
// 这是向后兼容的保证：老配置没有 `wb_device_token_url`，
// 升级后不该有任何行为变化。
func TestDeviceTokenNotConfigured(t *testing.T) {
	var seen http.Header
	up := captureChatRequest(t, &seen)

	c := routableClient(t, up.URL)
	// 不设置 c.DeviceToken（nil）

	rc, status, _, err := c.ChatStream(intlAuth(), []byte(`{"model":"m","messages":[]}`))
	if err != nil {
		t.Fatalf("未配置时不该出错: %v", err)
	}
	if rc != nil {
		rc.Close()
	}
	if status != 200 {
		t.Fatalf("期望 200，实际 %d", status)
	}
	if got := seen.Get("X-Device-Token"); got != "" {
		t.Errorf("未配置服务时不该发该头，实际 %q", got)
	}
}

// TestNewDeviceTokenProviderEmptyURLIsNil 空 URL = 不启用（返回 nil）。
func TestNewDeviceTokenProviderEmptyURLIsNil(t *testing.T) {
	for _, u := range []string{"", "   ", "\t"} {
		if p := NewDeviceTokenProvider(u, "tok"); p != nil {
			t.Errorf("URL=%q 时应返回 nil（= 不启用），实际 %#v", u, p)
		}
	}
	if p := NewDeviceTokenProvider("http://127.0.0.1:9/x", ""); p == nil {
		t.Error("有 URL 时不该返回 nil（token 可以为空）")
	}
}

// TestDeviceTokenRejectsMalformed 形态不符的 token 要当失败处理。
//
// 宁可漏发，也不发一个上游不认的怪值 —— 实测官方形态恒为 `v3:` 前缀、
// 约 1030 字符。发一个 `abc` 出去只会让上游多一次无谓的风控判定。
func TestDeviceTokenRejectsMalformed(t *testing.T) {
	for _, bad := range []string{"abc", "", "v2:xxx", "V3:upper"} {
		svc, _ := fakeDeviceTokenService(t, bad, false)
		p := NewDeviceTokenProvider(svc.URL, "svc-token")
		if tok, err := p.Fetch(); err == nil {
			t.Errorf("形态 %q 应被拒绝，实际返回 %q", bad, tok)
		}
	}
}

// TestDeviceTokenFetchOK 正常路径：能取到、且原样返回。
func TestDeviceTokenFetchOK(t *testing.T) {
	svc, calls := fakeDeviceTokenService(t, sampleDeviceToken, false)
	p := NewDeviceTokenProvider(svc.URL, "svc-token")
	tok, err := p.Fetch()
	if err != nil {
		t.Fatalf("Fetch 出错: %v", err)
	}
	if tok != sampleDeviceToken {
		t.Errorf("token 应原样返回，期望 %q 实际 %q", sampleDeviceToken, tok)
	}
	if atomic.LoadInt32(calls) != 1 {
		t.Errorf("应调用一次，实际 %d", atomic.LoadInt32(calls))
	}
}

// TestDeviceTokenWrongServiceTokenRejected 鉴权不对要失败（同机 IPC 防护）。
func TestDeviceTokenWrongServiceTokenRejected(t *testing.T) {
	svc, _ := fakeDeviceTokenService(t, sampleDeviceToken, false)
	p := NewDeviceTokenProvider(svc.URL, "wrong-token")
	if _, err := p.Fetch(); err == nil {
		t.Error("服务令牌不对时应失败（防同机其它进程误用该端口）")
	}
}
