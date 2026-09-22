package upstream

// 盲盒接口（2026-09-20 补，对照所有者给的参考脚本 workbuddyv3）。
//
// 参考脚本的 `t_blindbox`：
//
//	GET  /v2/activity/growth/buddy/quota  → {"balance":N,"affordable":M}
//	POST /v2/activity/growth/buddy/open   {"count":1}
//	     → {"results":[{"instance":{…},"template":{"name":…,"rarity":…}}]}
//
// 逐项对照后，参考脚本的 23 项任务里我们**只缺这一个**
//（另一项 workstation_expert 需要杀进程重启桌面端，不适合自动化）。

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
)

// blindboxServer 造一个假的上游：按路径回固定 JSON，并记录收到的请求体。
func blindboxServer(t *testing.T, quotaBody, openBody string) (*httptest.Server, *[]string) {
	t.Helper()
	var paths []string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		paths = append(paths, r.Method+" "+r.URL.Path)
		w.Header().Set("Content-Type", "application/json")
		if strings.Contains(r.URL.Path, "/buddy/quota") {
			_, _ = w.Write([]byte(quotaBody))
			return
		}
		if strings.Contains(r.URL.Path, "/buddy/open") {
			_, _ = w.Write([]byte(openBody))
			return
		}
		w.WriteHeader(http.StatusNotFound)
		_, _ = w.Write([]byte(`{"code":404}`))
	}))
	t.Cleanup(srv.Close)
	return srv, &paths
}

// blindboxClient 造一个把 growth 请求指到假上游的 client。
func blindboxClient(t *testing.T, srv *httptest.Server) (*Client, *auth.Auth) {
	t.Helper()
	c := New()
	// growth 域走 chatBase；把 CN base 指向假服务器。
	c.ChatBaseCN = srv.URL
	c.BillingBaseCN = srv.URL
	return c, &auth.Auth{UID: "u-blind", AccessToken: "t", Domain: "copilot.tencent.com"}
}

// TestBlindboxQuotaParses 配额解析。
func TestBlindboxQuotaParses(t *testing.T) {
	srv, paths := blindboxServer(t, `{"code":0,"data":{"balance":42,"affordable":4}}`, `{}`)
	c, a := blindboxClient(t, srv)

	q, err := c.GrowthBlindboxQuota(a)
	if err != nil {
		t.Fatalf("查配额失败: %v", err)
	}
	if q.Balance != 42 || q.Affordable != 4 {
		t.Errorf("配额解析错：balance=%d affordable=%d（期望 42 / 4）", q.Balance, q.Affordable)
	}
	if len(*paths) == 0 {
		t.Error("应发出一次请求")
	}
}

// TestBlindboxOpenParsesItems 开盒结果解析（instance 优先、template 回退）。
func TestBlindboxOpenParsesItems(t *testing.T) {
	body := `{"code":0,"data":{"results":[
		{"instance":{"name":"小红花","rarity":3},"template":{"name":"备用名","rarity":"R"}},
		{"instance":{},"template":{"name":"只用模板名","rarity":"SR"}}
	]}}`
	srv, _ := blindboxServer(t, `{"code":0,"data":{}}`, body)
	c, a := blindboxClient(t, srv)

	items, err := c.GrowthBlindboxOpen(a, 2)
	if err != nil {
		t.Fatalf("开盒失败: %v", err)
	}
	if len(items) != 2 {
		t.Fatalf("应解析出 2 个物品，实际 %d", len(items))
	}
	if items[0].Name != "小红花" {
		t.Errorf("instance.name 优先，实际 %q", items[0].Name)
	}
	// ⚠ 稀有度可能是**数字**也可能是字符串（参考实现两种都遇到过）。
	// 若用 `string` 接数字，整个开盒结果会解析失败 —— 这里锁住两种都行。
	if items[0].Rarity != "3" {
		t.Errorf("数字稀有度应转成 %q，实际 %q", "3", items[0].Rarity)
	}
	if items[1].Name != "只用模板名" {
		t.Errorf("instance 为空时应回退 template.name，实际 %q", items[1].Name)
	}
	if items[1].Rarity != "SR" {
		t.Errorf("字符串稀有度应原样保留，实际 %q", items[1].Rarity)
	}
}

// TestBlindboxOpenClampsCount 数量上限：一次不能开满，否则吃掉后续玩法的能量。
func TestBlindboxOpenClampsCount(t *testing.T) {
	srv, _ := blindboxServer(t, `{"code":0,"data":{}}`, `{"code":0,"data":{"results":[]}}`)
	c, a := blindboxClient(t, srv)

	// 传 99 也不该报错（应被夹到上限）
	if _, err := c.GrowthBlindboxOpen(a, 99); err != nil {
		t.Fatalf("超量请求应被夹到上限而不是报错: %v", err)
	}
	// 0 / 负数应明确报错（调用方逻辑错误，不该静默）
	if _, err := c.GrowthBlindboxOpen(a, 0); err == nil {
		t.Error("count=0 应报错（那是调用方逻辑错误）")
	}
	if _, err := c.GrowthBlindboxOpen(a, -1); err == nil {
		t.Error("负数应报错")
	}
}

// TestBlindboxOpenSendsClampedCount 请求体里发的应是**夹过的**数量。
func TestBlindboxOpenSendsClampedCount(t *testing.T) {
	var gotBody map[string]any
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		if strings.Contains(r.URL.Path, "/buddy/open") {
			var m map[string]any
			_ = json.NewDecoder(r.Body).Decode(&m)
			gotBody = m
			_, _ = w.Write([]byte(`{"code":0,"data":{"results":[]}}`))
			return
		}
		_, _ = w.Write([]byte(`{"code":0,"data":{}}`))
	}))
	defer srv.Close()

	c, a := blindboxClient(t, srv)
	if _, err := c.GrowthBlindboxOpen(a, 99); err != nil {
		t.Fatal(err)
	}
	if gotBody == nil {
		t.Fatal("没收到开盒请求体")
	}
	// 99 应被夹到 MaxBlindboxOpens
	if v, _ := gotBody["count"].(float64); int(v) != MaxBlindboxOpens {
		t.Errorf("请求体里的 count 应是夹过的 %d，实际 %v", MaxBlindboxOpens, gotBody["count"])
	}
}
