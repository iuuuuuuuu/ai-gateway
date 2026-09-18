package zcode

// 回归测试：`no_route`（用户手动禁用）必须被正确读取与保留。
//
// ## 为什么单独测这一条
//
// 「停止接流量」开关的完整链路是**跨进程跨语言**的：
//
//	宿主（Rust）账号页开关
//	  → 写凭证文件的 `account.no_route`
//	    → 网关（Go）LoadDir 读它
//	      → 池的 healthy() 据此排除该账号
//
// 我最初只做了链路两端（宿主写元信息、Go 有 NoRoute 概念），
// **中间一段断了**：宿主的开关只改了它自己的 accounts.json，
// 而网关**根本不读那个文件**（池是扫 auths/ 目录建立的）。
//
// 结果：界面上的开关是个**摆设** —— 用户以为停掉了，网关照常把请求路由过去。

import (
	"os"
	"path/filepath"
	"testing"
)

// TestLoadReadsNoRouteFromAccountBlock 读宿主写入的 `account.no_route`。
//
// 形状与 WorkBuddy 一致（宿主用同一个键名），故 Go 侧必须读它。
func TestLoadReadsNoRouteFromAccountBlock(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "zcode-x.json")
	os.WriteFile(p, []byte(`{
		"uid": "zcode-x",
		"provider": "zai",
		"credential": "key-id.secret",
		"account": { "uid": "zcode-x", "no_route": true }
	}`), 0o600)

	c, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if !c.NoRoute {
		t.Error("应读到 account.no_route=true（宿主的「停止接流量」开关）—— " +
			"读不到的话界面上的开关完全不生效")
	}
}

// TestLoadReadsNoRouteFromTopLevel 顶层 `no_route` 也要认。
//
// 用户手写凭证文件时可能直接写在顶层；不接受它会让禁用静默失效。
func TestLoadReadsNoRouteFromTopLevel(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "zcode-y.json")
	os.WriteFile(p, []byte(`{"uid":"zcode-y","provider":"zai","credential":"k.s","no_route":true}`), 0o600)

	c, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if !c.NoRoute {
		t.Error("应读到顶层的 no_route")
	}
}

// TestNoRouteDefaultsFalse 没有该键 → false（三态语义：未声明 = 不禁用）。
//
// 若把"缺失"当成错误，所有历史凭证文件都会解析失败。
func TestNoRouteDefaultsFalse(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "zcode-z.json")
	os.WriteFile(p, []byte(`{"uid":"zcode-z","provider":"zai","credential":"k.s"}`), 0o600)

	c, err := LoadFile(p)
	if err != nil {
		t.Fatalf("缺 no_route 不应导致解析失败: %v", err)
	}
	if c.NoRoute {
		t.Error("未声明时应为 false")
	}
}

// TestSaveAtomicPreservesNoRoute 网关写回时**不得抹掉**宿主的禁用标记。
//
// ## 为什么这条很关键
//
// `SaveAtomic` 是**网关**写的（令牌刷新、额度回填都会触发），
// 而 `no_route` 是**宿主**写的。若写回时整体覆盖内存里的字段，
// 网关的任何一次写回都会把用户的禁用标记悄悄抹掉 ——
// 那个账号会重新开始接流量，而用户以为它还停着。
//
// 这是"两个写入方共用一个文件"时的经典竞态，故显式测。
func TestSaveAtomicPreservesNoRoute(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "zcode-preserve.json")
	os.WriteFile(p, []byte(`{
		"uid": "zcode-preserve",
		"provider": "zai",
		"credential": "old.cred",
		"account": { "uid": "zcode-preserve", "no_route": true }
	}`), 0o600)

	// 加载 → 改一个无关字段 → 写回（模拟网关回填额度后的写回）
	c, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	c.Nickname = "改了昵称"
	if err := c.SaveAtomic(); err != nil {
		t.Fatal(err)
	}

	// 重新加载：no_route 必须还在
	again, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if !again.NoRoute {
		t.Error("网关写回**抹掉了**宿主的禁用标记 —— 账号会重新开始接流量，" +
			"而用户以为它还停着")
	}
	if again.Nickname != "改了昵称" {
		t.Error("昵称应被写入")
	}
	if again.Credential != "old.cred" {
		t.Errorf("凭证应保留，实际 %q", again.Credential)
	}
}

// TestSaveAtomicKeepsUnknownFields 写回时保留我们不认识的字段。
//
// 宿主可能往凭证文件里加新字段（版本升级时一边写一边读）。
// 若网关写回时整体重建，那些字段会被丢掉 —— 表现为升级后配置莫名消失。
func TestSaveAtomicKeepsUnknownFields(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "zcode-unknown.json")
	os.WriteFile(p, []byte(`{
		"uid": "zcode-unknown",
		"provider": "zai",
		"credential": "k.s",
		"credit": { "remaining": 100, "total": 200 },
		"future_field": "宿主以后才会用的字段"
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
		if !containsStr(text, want) {
			t.Errorf("写回丢了字段 %q —— 网关整体重建文档会丢掉宿主写入的未知字段", want)
		}
	}
}

func containsStr(haystack, needle string) bool {
	return len(haystack) >= len(needle) && (func() bool {
		for i := 0; i+len(needle) <= len(haystack); i++ {
			if haystack[i:i+len(needle)] == needle {
				return true
			}
		}
		return false
	})()
}
