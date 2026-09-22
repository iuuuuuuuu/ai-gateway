package server

// dynamic_config_test.go 钉住「配置热重载让 /v1/models 立刻看到新产品的模型」。
//
// # 为什么需要它（2026-09-21 所有者报的缺陷）
//
// 所有者原话：
//
//	「兼容网关启动之后，我再添加的 zcode 和 qoder 账号，模型清单路由
//	  也没有显示 qoder 和 zcode 支持的账号，应该要自动重启或者热重载的」
//	「而且 /v1/models 接口，也没有返回 qoder 和 zcode 支持的模型，这也是个 bug」
//
// 根因：宿主在新增/刷新账号后重写配置文件，而 Go 网关只在启动时读一次。
//
// 这组测试钉住的是**修复后的行为**：调用 ReloadDynamic 之后，
// `/v1/models` 的产出必须立刻反映新清单 —— 不需要重启进程。
//
// ⚠ 只测 ReloadDynamic 还不够：还要证明它**不破坏**其余字段
//（监听地址、Pool、各回调都不能被换掉），故有 TestReloadDynamicKeepsOtherFields。

import (
	"testing"

	"workbuddy2api/internal/pool"
)

// TestDynamicReloadUpdatesProductModels 热重载后模型清单立刻生效。
func TestDynamicReloadUpdatesProductModels(t *testing.T) {
	// 启动时：没有任何产品的清单（模拟"网关先启动、账号后添加"）。
	h := newTestHandlerForChannels(nil)

	if got := h.productModels(); len(got) != 0 {
		t.Fatalf("前提不成立：启动时清单应为空，实际 %+v", got)
	}

	// 宿主在新增 Qoder/ZCode 账号后重写配置 → 触发热重载。
	h.ReloadDynamic(&DynamicConfig{
		ProductModels: map[string][]string{
			"qoder": {"Qwen3.8-Max", "Qwen3.8-Flash"},
			"zcode": {"GLM-5.3", "GLM-5.3-Flash"},
		},
	})

	got := h.productModels()
	if len(got) != 4 {
		t.Fatalf("热重载后应有 4 条，实际 %d：%+v", len(got), got)
	}

	// ① 这些模型必须出现在 /v1/models 里（这是所有者要的结果）。
	data := listModelsRaw(t, h)
	for _, id := range []string{"Qwen3.8-Max", "Qwen3.8-Flash", "GLM-5.3", "GLM-5.3-Flash"} {
		m := findModel(t, data, id)
		chs := channelsOf(t, m)
		if len(chs) == 0 {
			t.Errorf("%s 应有 channels（热重载后才出现的模型），实际没有", id)
			continue
		}
		// ② 渠道里必须标出正确的平台 —— 这正是"模型清单路由"要显示的东西。
		prod, _ := chs[0]["product"].(string)
		want := "qoder"
		if id == "GLM-5.3" || id == "GLM-5.3-Flash" {
			// 这两个同时属于 workbuddy 的静态表与 zcode，故只要包含 zcode 即可。
			if !hasChannelOf(chs, "zcode") {
				t.Errorf("%s 应含 zcode 渠道，实际 %+v", id, chs)
			}
			continue
		}
		if prod != want {
			t.Errorf("%s 的渠道应为 %s，实际 %s（channels=%+v）", id, want, prod, chs)
		}
	}
}

// hasChannelOf 判断 channels 里是否含某平台。
func hasChannelOf(chs []map[string]any, product string) bool {
	for _, c := range chs {
		if c["product"] == product {
			return true
		}
	}
	return false
}

// TestDynamicReloadIsIdempotent 重复热重载同一份配置不出重复项。
//
// 监听器是**轮询**的，同一次文件变更可能被观察到多次（例如宿主连续写两次）。
// 幂等性因此是必需的：若每次重载都追加，模型清单会指数膨胀。
func TestDynamicReloadIsIdempotent(t *testing.T) {
	h := newTestHandlerForChannels(nil)
	cfg := &DynamicConfig{ProductModels: map[string][]string{"qoder": {"Qwen3.8-Max"}}}

	for i := 0; i < 5; i++ {
		h.ReloadDynamic(cfg)
	}
	if got := h.productModels(); len(got) != 1 {
		t.Errorf("重复热重载应仍是 1 条，实际 %d：%+v", len(got), got)
	}
}

// TestDynamicReloadNilIsNoop nil 配置不得把现有清单清空。
//
// 为什么重要：监听器在解析失败时会走"保留旧配置"的分支，
// 若那条路径不小心传了 nil，用户的模型清单会**静默消失**。
func TestDynamicReloadNilIsNoop(t *testing.T) {
	h := newTestHandlerForChannels(map[string][]string{"zcode": {"GLM-5.3"}})
	before := len(h.productModels())

	h.ReloadDynamic(nil)

	if got := len(h.productModels()); got != before {
		t.Errorf("nil 热重载不该改变清单：before=%d after=%d", before, got)
	}
}

// TestDynamicReloadKeepsNonDynamicFields 热重载**只能**动 DynamicConfig 里的字段。
//
// # 为什么这条必须存在
//
// 本机制的整个安全性建立在"只换数据类字段"上。若哪天有人图省事
// 把 Pool / Upstream / 监听地址也一起换掉，会出这些事：
//
//	· 换掉 Pool → 在途请求失去账号归属，且旧池的后台 goroutine 泄漏
//	· 换掉 Upstream → 连接池被丢弃，正在进行的 SSE 断流
//	· 换掉监听地址 → 与实际 listener 不一致，界面显示错端口
//
// 故这里逐项断言"没被换掉"。
func TestDynamicReloadKeepsNonDynamicFields(t *testing.T) {
	p := pool.New("")
	h := NewHandler(Config{
		Pool:          p,
		APIKey:        "sk-original",
		MaxRotate:     7,
		ProductModels: map[string][]string{"zcode": {"GLM-5.3"}},
	})

	// 用一份"什么都想改"的配置去热重载。
	h.ReloadDynamic(&DynamicConfig{
		ProductModels: map[string][]string{"qoder": {"Qwen3.8-Max"}},
		PromptMode:    "custom",
		PromptText:    "新提示词",
	})

	// 不可热更新的字段必须原样。
	if h.cfg.Pool != p {
		t.Error("Pool 被热重载换掉了 —— 会泄漏后台 goroutine 并让在途请求失去归属")
	}
	if h.cfg.APIKey != "sk-original" {
		t.Errorf("APIKey 被改动：%q（安全相关字段不该被配置热重载静默替换）", h.cfg.APIKey)
	}
	if h.cfg.MaxRotate != 7 {
		t.Errorf("MaxRotate 被改动：%d", h.cfg.MaxRotate)
	}

	// 可热更新的字段必须生效。
	d := h.DynamicSnapshot()
	if d.PromptMode != "custom" || d.PromptText != "新提示词" {
		t.Errorf("提示词应被热重载：mode=%q text=%q", d.PromptMode, d.PromptText)
	}
	if len(d.ProductModels["qoder"]) != 1 {
		t.Errorf("模型清单应被热重载，实际 %+v", d.ProductModels)
	}
}

// TestDynamicSnapshotFallsBackToCfg 未走 NewHandler 时回落读 h.cfg。
//
// # 为什么必须有这条（2026-09-21 修正）
//
// 本包与其它包里有大量测试直接字面量构造 `&Handler{cfg: ...}`，绕过
// NewHandler —— 那条路径不会初始化 dynamic。若 DynamicSnapshot 在
// 未初始化时返回**空结构**，那些调用方拿到的模型清单会静默变成空，
// 表现为"既有测试莫名红了"，而根因（没走 NewHandler）从现象上看不出来。
//
// 回落到 h.cfg 让两种构造方式得到同一份初值。
func TestDynamicSnapshotFallsBackToCfg(t *testing.T) {
	// 刻意不走 NewHandler：模拟"直接字面量构造"的既有用法。
	h := &Handler{cfg: Config{ProductModels: map[string][]string{"zcode": {"GLM-5.3"}}}}

	d := h.DynamicSnapshot()
	if d == nil {
		t.Fatal("DynamicSnapshot 返回 nil —— 会让 productModels panic")
	}
	if len(d.ProductModels["zcode"]) != 1 {
		t.Errorf("未初始化时应回落到 h.cfg，实际 %+v", d.ProductModels)
	}
	// 端到端：productModels 必须能读到它（这正是既有测试依赖的行为）。
	if got := h.productModels(); len(got) != 1 {
		t.Errorf("productModels 应回落到 cfg 并读出 1 条，实际 %d：%+v", len(got), got)
	}
}

// TestDynamicSnapshotEmptyCfgIsSafe 完全空的 Handler 也不 panic。
//
// 兜底路径：连 cfg 都没填（零值 Config）时，应返回空清单而不是崩。
func TestDynamicSnapshotEmptyCfgIsSafe(t *testing.T) {
	h := &Handler{}
	if d := h.DynamicSnapshot(); d == nil {
		t.Fatal("DynamicSnapshot 返回 nil")
	}
	if len(h.productModels()) != 0 {
		t.Error("零值配置时应返回空清单（降级但不报错）")
	}
}
