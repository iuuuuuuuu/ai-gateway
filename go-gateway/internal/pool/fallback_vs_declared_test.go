package pool

// fallback_vs_declared_test.go —— 钉住「兜底清单**不得**参与『声明』竞争」。
//
// # 所有者的现场（2026-09-22）
//
//	发 `GLM-5.3`        → 400 model_not_in_region
//	发 `Auto`           → 400 model_not_in_region
//	发 `Qwen3.8-Flash`  → **200 正常**
//
// 原文：
//
//	「我在qoder客户端发送没问题,我都没重登」
//	「但是qoder这个还是不能用」
//
// # 为什么 Qwen 能通、GLM 不能 —— 差别在**重叠**
//
//	Qwen3.8-Flash  只有 qoder 声明          ⇒ 路由唯一 ⇒ 正常
//	GLM-5.3        被 workbuddy + qoder 都声明 ⇒ 两个产品竞争
//	Auto           同上
//
// # 根因：兜底清单被当成了"声明"
//
// 宿主那份 `product_models` 可能缺 `workbuddy`（它来自用户的
// 「限制使用的模型」白名单，默认为空）。网关于是**用内置静态表补一份**
// 作为兜底 —— 当时的注释写着：
//
//	「它是"声明"用的，不是"否定"用的 —— 多列几个不会让任何账号
//	  失去资格，所以宁可补全」
//
// **那句话在引入 `declared` 优先逻辑之后就失效了。**
//
// 因为 `pickForModelAny` 优先只在"明确声明"的产品里挑，而内置表里有
// `glm-5.3` / `auto` ⇒ workbuddy 也进 `declared` ⇒ 与 qoder 一起竞争 ⇒
// 池里 **19 个 WorkBuddy 账号 vs 1 个 qoder 账号** ⇒ 大概率选到
// WorkBuddy，而它其实**没有** `GLM-5.3`（静态表是猜测）⇒ 11102。
//
// # 修法
//
// 清单分两份，语义严格区分：
//
//	productModelSet          宿主给的**可信**清单 → 参与「不排除」+「声明」
//	productModelFallbackSet  网关补的**猜测**清单 → **只**参与「不排除」
//
// 本文件锁住：兜底清单让账号**不被排除**，但**不算声明**。

import (
	"testing"

	"workbuddy2api/internal/auth"
)

// overlapFixture 复现所有者的配置：workbuddy 只有兜底清单、qoder 有可信清单。
//
// 两边都"声明"了 `GLM-5.3`（workbuddy 是小写 `glm-5.3`，比较时归一）。
func overlapFixture(t *testing.T) *Pool {
	t.Helper()
	p := New("")
	t.Cleanup(func() { p.Flush() })

	// workbuddy 19 个账号（真实比例），qoder 1 个。
	for i := 0; i < 19; i++ {
		uid := "wb-" + string(rune('a'+i))
		p.Add(&auth.Auth{UID: uid, AccessToken: "t-" + uid, ExpiresAt: 9999999999})
		p.SetCredits(uid, int64(1000+i))
	}
	p.Add(&auth.Auth{UID: "qd-1", Product: auth.ProductQoder, AccessToken: "t-qd", ExpiresAt: 9999999999})
	p.SetCredits("qd-1", 500)

	p.SetMultiProduct(true, 0.3)

	// 宿主清单：只有 qoder（workbuddy 那份来自用户白名单，默认空）
	p.SetProductModels(map[string][]string{
		"qoder": {"GLM-5.3", "Auto", "Qwen3.8-Flash"},
	})
	// 网关兜底：内置静态表里有 glm-5.3 / auto（**猜测**）
	p.SetProductModelsFallback(map[string][]string{
		"workbuddy": {"glm-5.3", "auto", "deepseek-v4.1-flash"},
	})
	return p
}

// TestFallbackDoesNotWinDeclaredRace ★ 兜底清单**不得**让 workbuddy 参与声明竞争。
//
// 这是本轮修复的核心断言：`GLM-5.3` 只有 qoder 是**可信声明**，
// 所以即使 workbuddy 账号多 19 倍，也必须选到 qoder。
func TestFallbackDoesNotWinDeclaredRace(t *testing.T) {
	p := overlapFixture(t)

	// 反复挑多次：若实现把它当声明，19:1 的比例几乎必然选出 workbuddy。
	for i := 0; i < 50; i++ {
		acct := p.pickForModelAny("GLM-5.3", nil)
		if acct == nil {
			t.Fatalf("第 %d 次挑不出账号", i)
		}
		if acct.ProductOf() != auth.ProductQoder {
			t.Fatalf(
				"第 %d 次挑到了 %s 账号（%s）—— `GLM-5.3` 只有 qoder 是**可信声明**，"+
					"workbuddy 那份是网关的**猜测**（兜底清单），不该参与声明竞争。"+
					"否则 19 个 WorkBuddy 账号会压倒 1 个 qoder，而它其实没有这个模型 ⇒ 11102",
				i, acct.ProductOf(), acct.UID)
		}
	}
}

// TestFallbackStillPreventsExclusion 兜底清单仍要让该产品**不被排除**。
//
// 这是它的存在意义：网关不知道 workbuddy 提供什么时，不能把它的账号
// 全部排掉 —— 那会让 WorkBuddy 的模型（如 `deepseek-v4.1-flash`）不可用。
func TestFallbackStillPreventsExclusion(t *testing.T) {
	p := overlapFixture(t)

	// `deepseek-v4.1-flash` 只在 workbuddy 的兜底清单里。
	acct := p.pickForModelAny("deepseek-v4.1-flash", nil)
	if acct == nil {
		t.Fatal("挑不出账号 —— 兜底清单必须让 workbuddy 不被排除，" +
			"否则 WorkBuddy 自己的模型会变成不可用")
	}
	if acct.ProductOf() != auth.ProductWorkBuddy {
		t.Errorf("应挑到 workbuddy 账号，实际 %s", acct.ProductOf())
	}
}

// TestDeclaresIgnoresFallback `productDeclaresModelLocked` 不看兜底清单。
//
// 直接断言函数语义，比端到端挑选更精确 —— 后者受权重随机影响。
func TestDeclaresIgnoresFallback(t *testing.T) {
	p := overlapFixture(t)
	p.mu.RLock()
	defer p.mu.RUnlock()

	// workbuddy：兜底清单里有 glm-5.3，但可信清单里没有 ⇒ **不算声明**
	if p.productDeclaresModelLocked(auth.ProductWorkBuddy, "GLM-5.3") {
		t.Error("workbuddy 对 `GLM-5.3` 不该算「声明」—— " +
			"它只出现在网关的兜底（猜测）清单里，可信清单里没有")
	}
	// 但**不该被排除**（兜底清单的意义）
	if !p.productMayServeLocked(auth.ProductWorkBuddy, "GLM-5.3") {
		t.Error("workbuddy 对 `GLM-5.3` 不该被**排除** —— " +
			"兜底清单的作用就是「不知道时别排除」")
	}

	// qoder：可信清单里有 ⇒ 算声明
	if !p.productDeclaresModelLocked(auth.ProductQoder, "GLM-5.3") {
		t.Error("qoder 对 `GLM-5.3` 应算「声明」—— 它在宿主透传的可信清单里")
	}
}

// TestFallbackCaseInsensitive 兜底清单同样**大小写不敏感**。
//
// 内置静态表是小写（`glm-5.3`），而客户端发的是 `GLM-5.3` ——
// 不归一的话这条链路会静默失效。
func TestFallbackCaseInsensitive(t *testing.T) {
	p := overlapFixture(t)
	p.mu.RLock()
	defer p.mu.RUnlock()

	for _, m := range []string{"glm-5.3", "GLM-5.3", "Glm-5.3", " glm-5.3 "} {
		if !p.productMayServeLocked(auth.ProductWorkBuddy, m) {
			t.Errorf("兜底清单对 %q 应命中（大小写/空白归一）", m)
		}
	}
}

// TestEmptyFallbackKeepsOldBehavior 没有兜底清单时行为与改动前一致。
//
// 回归保护：这份改动只该影响"网关补过兜底清单"的场景。
func TestEmptyFallbackKeepsOldBehavior(t *testing.T) {
	p := New("")
	t.Cleanup(func() { p.Flush() })
	p.Add(&auth.Auth{UID: "wb-1", AccessToken: "t", ExpiresAt: 9999999999})
	p.SetCredits("wb-1", 1000)
	p.Add(&auth.Auth{UID: "qd-1", Product: auth.ProductQoder, AccessToken: "t", ExpiresAt: 9999999999})
	p.SetCredits("qd-1", 100)
	p.SetMultiProduct(true, 0.3)
	p.SetProductModels(map[string][]string{"qoder": {"Qwen3.8-Flash"}})
	// 刻意**不**设兜底

	p.mu.RLock()
	defer p.mu.RUnlock()
	// workbuddy 清单未知 ⇒ 不排除、也不算声明（与改动前逐字相同）
	if !p.productMayServeLocked(auth.ProductWorkBuddy, "任意模型") {
		t.Error("清单未知时不该排除")
	}
	if p.productDeclaresModelLocked(auth.ProductWorkBuddy, "任意模型") {
		t.Error("清单未知时不该算声明")
	}
}
