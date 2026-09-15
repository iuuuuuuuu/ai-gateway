package pool

import (
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// ---------------------------------------------------------------------------
// 区域（realm）过滤回归
//
// 场景：同一个模型名在两个区域可能是不同的服务。
// 客户端用 `cn:` / `global:` 前缀显式指定区域时，选号必须尊重该约束，
// 否则会把请求发给不支持该模型的区域（上游返回 11102）。
// ---------------------------------------------------------------------------

// realmPool 构造一个含国服 + 国际版账号的池。
func realmPool(t *testing.T) *Pool {
	t.Helper()
	p := New("")
	p.Add(&auth.Auth{
		UID: "cn-1", AccessToken: "t", Domain: "copilot.tencent.com",
		SoonestExpireAt: time.Now().Add(24 * time.Hour).Unix(),
	})
	p.Add(&auth.Auth{
		UID: "intl-1", AccessToken: "t", Domain: "www.workbuddy.ai",
		SoonestExpireAt: time.Now().Add(24 * time.Hour).Unix(),
	})
	return p
}

// TestAuthRealmDetection 账号区域判定（依据登录域名后缀）。
func TestAuthRealmDetection(t *testing.T) {
	cases := []struct {
		domain string
		want   string
	}{
		{"www.workbuddy.ai", auth.RealmGlobal},
		{"workbuddy.ai", auth.RealmGlobal},
		{"WWW.WORKBUDDY.AI", auth.RealmGlobal}, // 大小写不敏感
		{"copilot.tencent.com", auth.RealmCN},
		{"www.codebuddy.cn", auth.RealmCN},
		{"", auth.RealmCN},
	}
	for _, c := range cases {
		a := &auth.Auth{Domain: c.domain}
		if got := a.Realm(); got != c.want {
			t.Errorf("Domain=%q Realm()=%q want %q", c.domain, got, c.want)
		}
	}
	// nil 安全
	var nilAuth *auth.Auth
	if nilAuth.Realm() != auth.RealmCN {
		t.Error("nil 账号应视为国服（且不 panic）")
	}
}

// TestPickForModelRealmGlobal 指定 global 只选国际版账号。
func TestPickForModelRealmGlobal(t *testing.T) {
	p := realmPool(t)
	// 多试几次：选号含随机性，必须每次都命中正确区域
	for i := 0; i < 30; i++ {
		got := p.PickForModelRealm("gpt-6-astra", auth.RealmGlobal, nil)
		if got == nil {
			t.Fatal("应选出国际版账号")
		}
		if got.Realm() != auth.RealmGlobal {
			t.Fatalf("指定 global 却选出 %s（domain=%s）", got.UID, got.Domain)
		}
	}
}

// TestPickForModelRealmCN 指定 cn 只选国服账号。
func TestPickForModelRealmCN(t *testing.T) {
	p := realmPool(t)
	for i := 0; i < 30; i++ {
		got := p.PickForModelRealm("deepseek-v4-flash", auth.RealmCN, nil)
		if got == nil {
			t.Fatal("应选出国服账号")
		}
		if got.Realm() != auth.RealmCN {
			t.Fatalf("指定 cn 却选出 %s（domain=%s）", got.UID, got.Domain)
		}
	}
}

// TestPickForModelRealmEmptyUnrestricted 空 realm = 不限制（保持既有行为）。
func TestPickForModelRealmEmptyUnrestricted(t *testing.T) {
	p := realmPool(t)
	seen := map[string]bool{}
	for i := 0; i < 60; i++ {
		got := p.PickForModelRealm("glm-5.2", "", nil)
		if got == nil {
			t.Fatal("空 realm 应能选出账号")
		}
		seen[got.UID] = true
	}
	// 不限制时应两个区域都可能被选中（分层同档 + 加权随机）
	if len(seen) < 2 {
		t.Errorf("空 realm 应不限制区域，但只选中了 %v", seen)
	}
}

// TestPickForModelRealmNoMatchReturnsNil 指定区域无可用账号时返回 nil。
//
// 这是刻意的设计：不回退到「不限制区域」—— 回退会让显式指定失效，
// 客户端拿到的是难以理解的「模型不存在」错误，而不是「该区域无账号」。
func TestPickForModelRealmNoMatchReturnsNil(t *testing.T) {
	p := New("")
	// 池里只有国服账号
	p.Add(&auth.Auth{
		UID: "cn-1", AccessToken: "t", Domain: "copilot.tencent.com",
		SoonestExpireAt: time.Now().Add(24 * time.Hour).Unix(),
	})

	if got := p.PickForModelRealm("gpt-6-astra", auth.RealmGlobal, nil); got != nil {
		t.Errorf("指定 global 但池里只有国服账号，应返回 nil，实际 %s", got.UID)
	}
	// 对照：不指定区域时能选出来
	if got := p.PickForModelRealm("glm-5.2", "", nil); got == nil {
		t.Error("不指定区域时应能选出账号")
	}
}

// TestPickForModelRealmRotation 轮转模式下同样尊重区域约束。
func TestPickForModelRealmRotation(t *testing.T) {
	p := realmPool(t)
	p.SetRotation(true)
	defer p.SetRotation(false)

	for i := 0; i < 10; i++ {
		got := p.PickForModelRealm("gpt-6-astra", auth.RealmGlobal, nil)
		if got == nil {
			t.Fatal("轮转模式下应选出国际版账号")
		}
		if got.Realm() != auth.RealmGlobal {
			t.Fatalf("轮转模式指定 global 却选出 %s（domain=%s）", got.UID, got.Domain)
		}
	}
}

// TestPickForModelRealmUnknownValueIgnored 无法识别的 realm 视为不限制。
func TestPickForModelRealmUnknownValueIgnored(t *testing.T) {
	p := realmPool(t)
	// 归一化后为空 → 不限制；两个区域都可能被选中
	seen := map[string]bool{}
	for i := 0; i < 60; i++ {
		got := p.PickForModelRealm("glm-5.2", "GLOBAL", nil) // 大写不被识别
		if got == nil {
			t.Fatal("无法识别的 realm 应视为不限制")
		}
		seen[got.UID] = true
	}
	if len(seen) < 2 {
		t.Errorf("无法识别的 realm 应不限制区域，但只选中了 %v", seen)
	}
}

// TestPickForModelStillWorks 既有入口（无 realm 参数）行为不变。
func TestPickForModelStillWorks(t *testing.T) {
	p := realmPool(t)
	if got := p.PickForModel("glm-5.2", nil); got == nil {
		t.Error("PickForModel 应保持可用（等价于不限制区域）")
	}
}
