package qoder

// 回归测试：`no_route`（用户手动禁用）必须被读取与保留。
//
// 与 ZCode 侧同一类缺陷、同一套修法（详见 zcode 包的 noroute_test.go）。
// Qoder 这边更危险一点：**令牌刷新很频繁**，而 SaveAtomic 是刷新时调用的，
// 故若写回时整体覆盖，用户的禁用标记会在几分钟内被悄悄抹掉。

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// TestLoadReadsNoRoute 从 `account.no_route` 与顶层两处都要能读到。
func TestLoadReadsNoRoute(t *testing.T) {
	dir := t.TempDir()

	cases := []struct {
		name string
		body string
	}{
		{
			"嵌套形 account.no_route",
			`{"auth":{"accessToken":"dt-x","refreshToken":"drt-x","expiresAt":1,"domain":"qoder.com.cn"},
			  "account":{"uid":"q1","nickname":"n","no_route":true}}`,
		},
		{
			"扁平形顶层 no_route",
			`{"accessToken":"dt-y","refreshToken":"drt-y","uid":"q2","no_route":true}`,
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			p := filepath.Join(dir, "qoder-"+strings.ReplaceAll(tc.name, " ", "_")+".json")
			os.WriteFile(p, []byte(tc.body), 0o600)
			c, err := LoadFile(p)
			if err != nil {
				t.Fatal(err)
			}
			if !c.NoRoute {
				t.Error("应读到 no_route=true（宿主「停止接流量」开关）—— " +
					"读不到的话界面上的开关完全不生效")
			}
		})
	}
}

// TestNoRouteDefaultsFalse 缺键 → false（三态语义：未声明 = 不禁用）。
func TestNoRouteDefaultsFalse(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "qoder-plain.json")
	os.WriteFile(p, []byte(`{"auth":{"accessToken":"dt","refreshToken":"drt","expiresAt":1,"domain":"qoder.com.cn"},"account":{"uid":"q"}}`), 0o600)

	c, err := LoadFile(p)
	if err != nil {
		t.Fatalf("缺 no_route 不应导致解析失败: %v", err)
	}
	if c.NoRoute {
		t.Error("未声明时应为 false")
	}
}

// TestSaveAtomicPreservesNoRoute 令牌刷新写回时**不得抹掉**宿主的禁用标记。
//
// ## 为什么 Qoder 这边更危险
//
// Qoder 的 DT 会过期，刷新是**常规路径**（不像 ZCode 凭证长期有效、
// 几乎不写回）。若写回时整体重建文档，用户的禁用标记会在**几分钟内**
// 被悄悄抹掉 —— 账号重新接流量，而用户以为它还停着。
func TestSaveAtomicPreservesNoRoute(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "qoder-preserve.json")
	os.WriteFile(p, []byte(`{
		"auth":{"accessToken":"dt-old","refreshToken":"drt-old","expiresAt":100,"domain":"qoder.com.cn"},
		"account":{"uid":"q-preserve","nickname":"旧","no_route":true}
	}`), 0o600)

	c, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	// 模拟一次令牌刷新
	c.DT = "dt-new"
	c.DRT = "drt-new"
	c.DTExpiresAt = 999999
	if err := c.SaveAtomic(); err != nil {
		t.Fatal(err)
	}

	again, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if !again.NoRoute {
		t.Error("令牌刷新写回**抹掉了**宿主的禁用标记 —— " +
			"账号会重新开始接流量，而用户以为它还停着")
	}
	if again.DT != "dt-new" || again.DRT != "drt-new" {
		t.Error("刷新后的令牌应被写入")
	}
	if again.UID != "q-preserve" {
		t.Errorf("uid 应保留，实际 %q", again.UID)
	}
}

// TestSaveAtomicKeepsUnknownFields 写回时保留不认识的字段。
func TestSaveAtomicKeepsUnknownFields(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "qoder-unknown.json")
	os.WriteFile(p, []byte(`{
		"auth":{"accessToken":"dt","refreshToken":"drt","expiresAt":1,"domain":"qoder.com.cn"},
		"account":{"uid":"q-unknown"},
		"credit":{"remaining":50},
		"future_field":"宿主以后才会用的字段"
	}`), 0o600)

	c, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if err := c.SaveAtomic(); err != nil {
		t.Fatal(err)
	}
	raw, _ := os.ReadFile(p)
	text := string(raw)
	for _, want := range []string{"credit", "remaining", "future_field"} {
		if !strings.Contains(text, want) {
			t.Errorf("写回丢了字段 %q —— 整体重建文档会丢掉宿主写入的未知字段", want)
		}
	}
}
