package pool

// bare_model_routing_test.go —— 钉住「裸名模型被路由到不提供它的产品」这个缺陷。
//
// # 所有者的现场（2026-09-21）
//
//	发 `Qwen3.8-Flash`（裸名）→ 400 model_not_in_region
//	发 `qoder:Qwen3.8-Flash`   → 成功
//
// 原文：
//
//	「然后qoder这个也有问题  Qwen3.8-Flash  报错不存在,加上
//	 qoder:Qwen3.8-Flash  这样子才能调用」
//
// # 根因
//
// `product_models` 里**只有 qoder / zcode，没有 workbuddy**
//（宿主那份来自用户的「限制使用的模型」白名单，默认为空 ⇒ 不写该键）。
//
// 而旧逻辑是：
//
//	if !p.productMayServeLocked(e.a.ProductOf(), model) { scoped[uid] = true }
//
// `productMayServeLocked` 对"清单未知"返回 **true（不排除）**。于是
// WorkBuddy 账号留在候选集里，而它没有 `Qwen3.8-Flash` ⇒ 请求撞上第一个
// WorkBuddy 账号就回 11102 —— 压根轮不到真正有这个模型的 qoder 账号。
//
// 关键错误是：**把"不知道"当成了"能满足"**。
//
// # 修法
//
// 分两轮：
//
//	① 明确声明提供该模型的产品 → 优先
//	② 清单未知的产品 → 只作兜底（①挑不到时才用）
//	③ 有清单且明确没有 → 排除
//
// 本文件锁住这三条，尤其是"未知不得抢占已声明的产品"。

import (
	"testing"

	"workbuddy2api/internal/auth"
)

// qwenFixture 造一个"WorkBuddy 清单未知 + Qoder 声明提供 Qwen3.8-Flash"的池。
//
// 复现所有者的真实配置：`product_models` 缺 workbuddy。
func qwenFixture(t *testing.T) *Pool {
	t.Helper()
	p := newRoutingTestPool(t)
	p.SetMultiProduct(true, 0)
	p.SetProductModels(map[string][]string{
		"qoder": {"Qwen3.8-Flash", "GLM-5.3-Flash"},
		"zcode": {"GLM-5.3-Flash"},
		// ⚠ 故意不含 workbuddy —— 这是现场的关键条件
	})

	// WorkBuddy 账号：它有别的模型，但**没有** Qwen3.8-Flash
	p.Add(&auth.Auth{UID: "wb-1", Product: auth.ProductWorkBuddy})
	// Qoder 账号：有这个模型
	p.Add(&auth.Auth{UID: "qd-1", Product: auth.ProductQoder})
	return p
}

// TestBareModelPrefersDeclaringProduct 裸名模型必须优先去**声明提供它**的产品。
//
// 这是本次缺陷的核心断言。旧实现下 WorkBuddy 账号也在候选集里，
// 于是"挑到哪个"取决于遍历顺序 —— 有一半概率错，而错了就是 11102。
func TestBareModelPrefersDeclaringProduct(t *testing.T) {
	p := qwenFixture(t)

	// 多试几次：选号含加权/随机，一次通过不足以说明问题。
	for i := 0; i < 50; i++ {
		got := p.PickForModel("Qwen3.8-Flash", map[string]bool{})
		if got == nil {
			t.Fatalf("第 %d 次挑不到账号（应能挑到 qoder）", i)
		}
		if got.Product != auth.ProductQoder {
			t.Fatalf("第 %d 次挑到了 %s 账号（%s）—— "+
				"而只有 qoder 声明提供 Qwen3.8-Flash；"+
				"WorkBuddy 没有它，发过去会回 11102 model_not_in_region",
				i, got.Product, got.UID)
		}
	}
}

// TestBareModelDoesNotCrossToUnknownWhenDeclaredBusy 已声明该模型的平台不可用时，
// 不能回退到清单未知的产品。未知不是能力承诺：Qoder 独有的 Qwen3.8-Flash
// 不能因为 Qoder 账号被禁用就误派给不提供此模型的 WorkBuddy。
//
// # 为什么用 Disable 而不是 Cooldown（我第一版写错了，记下来）
//
// 第一版用 `p.Cooldown("qd-1", CoolSoft, time.Hour, ...)`，结果红了：
// 拾到的仍是 qoder。**那不是缺陷** —— 池子有"全冷却兜底"
// （pickRotation / pickEarliestExpiryLocked），设计就是"宁可给一个冷却中的
// 账号，也不要 503"。所以冷却**拦不住**选号，拿它构造"不可用"是错的，
// 测出来的是我的误解而不是代码行为。
//
// `Disabled` 是硬条件（见 entry.healthy），任何选号路径都不该越过它。
func TestBareModelDoesNotCrossToUnknownWhenDeclaredBusy(t *testing.T) {
	p := qwenFixture(t)

	// 让 qoder 账号不可用。WorkBuddy 清单未知，但并不代表它有 Qwen。
	p.Disable("qd-1", "测试：模拟声明的产品不可用")

	got := p.PickForModel("Qwen3.8-Flash", map[string]bool{})
	if got != nil {
		t.Fatalf("声明平台不可用时不应误派到未知产品，实际 %s/%s", got.Product, got.UID)
	}
}

// TestKnownProductWithoutModelStillExcluded 有清单且明确没有该模型的产品，
// **永远**排除（这是旧实现就有的正确行为，不能因为本次修复而丢掉）。
func TestKnownProductWithoutModelStillExcluded(t *testing.T) {
	p := newRoutingTestPool(t)
	p.SetMultiProduct(true, 0)
	p.SetProductModels(map[string][]string{
		"workbuddy": {"deepseek-v4.1-flash"},
		"qoder":     {"Qwen3.8-Flash"},
	})
	p.Add(&auth.Auth{UID: "wb-1", Product: auth.ProductWorkBuddy})
	p.Add(&auth.Auth{UID: "qd-1", Product: auth.ProductQoder})

	// workbuddy 清单里没有 Qwen3.8-Flash ⇒ 不该选它
	for i := 0; i < 30; i++ {
		got := p.PickForModel("Qwen3.8-Flash", map[string]bool{})
		if got == nil {
			t.Fatal("应能挑到 qoder 账号")
		}
		if got.Product != auth.ProductQoder {
			t.Fatalf("workbuddy 清单里没有该模型，不该被选中（实际 %s）", got.UID)
		}
	}
}

// TestWorkBuddyModelStillRoutesToWorkBuddy 反向验证：WorkBuddy 自己的模型
// 仍要去 WorkBuddy（不能"修 A 弄坏 B"）。
func TestWorkBuddyModelStillRoutesToWorkBuddy(t *testing.T) {
	p := newRoutingTestPool(t)
	p.SetMultiProduct(true, 0)
	p.SetProductModels(map[string][]string{
		"workbuddy": {"deepseek-v4.1-flash"},
		"qoder":     {"Qwen3.8-Flash"},
	})
	p.Add(&auth.Auth{UID: "wb-1", Product: auth.ProductWorkBuddy})
	p.Add(&auth.Auth{UID: "qd-1", Product: auth.ProductQoder})

	for i := 0; i < 30; i++ {
		got := p.PickForModel("deepseek-v4.1-flash", map[string]bool{})
		if got == nil {
			t.Fatal("应能挑到 workbuddy 账号")
		}
		if got.Product != auth.ProductWorkBuddy {
			t.Fatalf("WorkBuddy 的模型被路由到了 %s", got.Product)
		}
	}
}

// TestNoProductListKeepsOldBehavior 完全没有清单时（多产品关闭/未注入），
// 行为与改动前一致：不排除任何账号。
//
// 这是**回滚点**：清单缺失不该让任何账号失去资格。
func TestNoProductListKeepsOldBehavior(t *testing.T) {
	p := newRoutingTestPool(t)
	p.SetMultiProduct(true, 0)
	// 故意不 SetProductModels
	p.Add(&auth.Auth{UID: "wb-1", Product: auth.ProductWorkBuddy})
	p.Add(&auth.Auth{UID: "qd-1", Product: auth.ProductQoder})

	seen := map[string]bool{}
	for i := 0; i < 60; i++ {
		got := p.PickForModel("whatever-model", map[string]bool{})
		if got == nil {
			t.Fatal("无清单时不该挑不到账号")
		}
		seen[got.UID] = true
	}
	if !seen["wb-1"] || !seen["qd-1"] {
		t.Errorf("无清单时两个产品的账号都应参与，实际只用到 %v", seen)
	}
}

// TestDeclaresDiffersFromMayServe "声明"与"可服务"必须是两个语义。
//
// 前者用于**优先**，后者用于**否定**；混用正是本次缺陷的成因。
func TestDeclaresDiffersFromMayServe(t *testing.T) {
	p := newRoutingTestPool(t)
	p.SetMultiProduct(true, 0)
	p.SetProductModels(map[string][]string{"qoder": {"Qwen3.8-Flash"}})

	p.mu.RLock()
	defer p.mu.RUnlock()

	// workbuddy 清单未知：
	//   mayServe = true  （不排除 —— 兜底时仍可用）
	//   declares = false （不算声明 —— 不该抢占已声明的产品）
	if !p.productMayServeLocked(auth.ProductWorkBuddy, "Qwen3.8-Flash") {
		t.Error("清单未知时应 mayServe=true（否则账号被误排除）")
	}
	if p.productDeclaresModelLocked(auth.ProductWorkBuddy, "Qwen3.8-Flash") {
		t.Error("清单未知时不该 declares=true（否则它会抢走该模型）")
	}

	// qoder 明确声明：
	if !p.productDeclaresModelLocked(auth.ProductQoder, "Qwen3.8-Flash") {
		t.Error("清单里有的模型应 declares=true")
	}
	// qoder 明确没有的：
	if p.productDeclaresModelLocked(auth.ProductQoder, "GLM-5.3") {
		t.Error("清单里没有的模型不该 declares=true")
	}
}

// newRoutingTestPool 造一个**不落盘、不受全局节流影响**的池。
//
// # 为什么不能用 `New("test-bare-model")`（我第一版这么写，红了）
//
// `New(stateFp)` 在 stateFp **非空**时会 `p.load()` + `startFlusher()`：
//
//	· load() 会读磁盘上那个同名 state 文件 —— 我的测试因此**不是自足的**，
//	  结果取决于机器上有没有残留文件（实测在完整跑 `go test ./...` 时红了，
//	  单独跑也红，因为文件被上一次运行写下了）
//	· startFlusher() 起了后台 goroutine，测试结束也不停
//
// 既有测试（expiry_test / bench_test / multiproduct_test）一律用 `New("")`，
// 照它们来。另外把 `minPickGap` 置 0：否则同一账号在节流窗口内不会被再次
// 选中，而我的用例要连续挑 50 次来暴露"随机挑错产品"的问题。
func newRoutingTestPool(t *testing.T) *Pool {
	t.Helper()
	prev := minPickGap
	minPickGap = 0
	t.Cleanup(func() { minPickGap = prev })
	return New("")
}
