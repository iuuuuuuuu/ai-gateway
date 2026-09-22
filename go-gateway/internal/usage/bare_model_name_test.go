package usage

// bare_model_name_test.go —— 钉住「统计键必须是裸模型名」。
//
// # 所有者报的现场（2026-09-21）
//
// 用量列表里同时出现两行：
//
//	`国服:deepseek-v4.1-flash`    12 tokens / 1 次    ← 错
//	`deepseek-v4.1-flash · 国服`   1.6B / 4014 次     ← 对
//
// 而它们是**同一个模型的同一份用量**，只是前者把路由前缀当成了模型名的一部分。
//
// 所有者原话：
//
//	「不要再把 平台:模型名 这种类似的 单独列一个统计了,这是错误的」
//	「只有不同的版本(不同平台 比如 国内 国外 zcode workbuddy)才需要拆分开」
//
// # 修法（两处，缺一不可）
//
//  1. **写入点** `recordAt`：把模型名归一成裸名。这样新数据不会再分裂。
//  2. **装载点** `load`：把**磁盘上已有的**带前缀键归并。
//     只改 ① 是不够的 —— 所有者机器上的 usage.json 里已经存着
//     `models["zcode:GLM-5.3-Flash"]` 与 `models["国服:deepseek-v4.1-flash"]`，
//     光堵住新写入只会让它们"不再增长"，界面里那两行**依然存在**。
//
// 两处都走同一个 `bareModelName`，规则不会分叉。

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestBareModelNameStripsRoutePrefix(t *testing.T) {
	cases := []struct {
		in   string
		want string
		why  string
	}{
		{"deepseek-v4.1-flash", "deepseek-v4.1-flash", "无前缀：原样"},
		{"国服:deepseek-v4.1-flash", "deepseek-v4.1-flash", "区域前缀（所有者报的那一条）"},
		{"国际版:deepseek-v4.1-flash", "deepseek-v4.1-flash", "另一区域写法"},
		{"cn:deepseek-v4.1-flash", "deepseek-v4.1-flash", "英文区域"},
		{"zcode:GLM-5.3-Flash", "GLM-5.3-Flash", "产品前缀"},
		{"qoder:Qwen3.8-Flash", "Qwen3.8-Flash", "产品前缀（大小写保留）"},
		{"workbuddy:国服:glm-5.3", "glm-5.3", "产品+区域两层"},
		{"workbuddy:国际版:glm-5.3", "glm-5.3", "产品+区域两层（另一种区域）"},
		{"qoder:国服:Qwen3.8-Flash", "Qwen3.8-Flash", "产品+区域（英文区）"},
		// 边界：前缀后面是空的 —— 不该把整个名字切成空
		{"国服:", "国服:", "尾部空：原样保留（切了会丢模型名）"},
		// 边界：模型名本身带冒号（实测没有，但别切坏）
		{"deepseek:v3", "v3", "两段时取第二段（与路由规则一致）"},
	}
	for _, c := range cases {
		if got := bareModelName(c.in); got != c.want {
			t.Errorf("bareModelName(%q) = %q，期望 %q（%s）", c.in, got, c.want, c.why)
		}
	}
}

// TestRecordMergesPrefixedAndBare 带前缀与不带前缀的记录必须落进**同一个键**。
//
// 这条直接对应所有者看到的"两行"：合并后只剩一行。
func TestRecordMergesPrefixedAndBare(t *testing.T) {
	s := New("") // 不落盘

	// 同一模型的两种写法（模拟"历史里存了带前缀的，新请求不带前缀"）
	s.Record("u1", "国服:deepseek-v4.1-flash", Counters{Input: 5, Output: 7})
	s.Record("u1", "deepseek-v4.1-flash", Counters{Input: 100, Output: 200})

	snap := s.Snapshot(0)
	models := groupKeys(t, snap, "models")

	if len(models) != 1 {
		t.Fatalf("两种写法应归并成 1 个模型，实际 %d 个：%v", len(models), models)
	}
	if models[0] != "deepseek-v4.1-flash" {
		t.Errorf("归并后的键应是裸名，实际 %q", models[0])
	}
	// 用量要累加，不能丢
	total := modelTotal(t, snap, "deepseek-v4.1-flash")
	if total != 5+7+100+200 {
		t.Errorf("用量应累加为 %d，实际 %d", 5+7+100+200, total)
	}
}

// TestAccountModelsAlsoNormalized accountModels 维度同样不许出现带前缀的键。
//
// 界面「按账号筛选看用了哪些模型」读的就是这一维；漏了它会在那边再冒出一行。
func TestAccountModelsAlsoNormalized(t *testing.T) {
	s := New("")
	s.Record("acct-1", "zcode:GLM-5.3-Flash", Counters{Input: 10, Output: 1})
	s.Record("acct-1", "GLM-5.3-Flash", Counters{Input: 20, Output: 2})

	snap := s.Snapshot(0)
	am, _ := snap["accountModels"].(map[string]any)
	if am == nil {
		t.Fatal("accountModels 缺失")
	}
	entry, ok := am["acct-1"]
	if !ok {
		t.Fatal("accountModels 里没有 acct-1")
	}
	// 该维度是 {账号: [ {key,total,...} ]}（已拍平）
	perModel, _ := entry.([]map[string]any)
	if len(perModel) != 1 {
		t.Fatalf("accountModels 应只有 1 个模型条目，实际 %d 个", len(perModel))
	}
	if k, _ := perModel[0]["key"].(string); k != "GLM-5.3-Flash" {
		t.Errorf("accountModels 的键应是裸名，实际 %q", k)
	}
}

// TestBillingDimensionAlsoNormalized modelBilling（`模型 × 区域` 那张表）也不能漏。
//
// # 为什么单列一条
//
// `recordAt` 里有**两处**用到 model：`s.models` / `s.accountModels`，以及
// `s.billed`。归一化写在函数开头，理论上三处都覆盖 —— 但"理论上"正是
// 这类缺陷的温床（billed 的键是 `model+"\x00"+key`，拼接方式不同）。
// 故显式断言一次。
func TestBillingDimensionAlsoNormalized(t *testing.T) {
	s := New("")
	mult := 0.03
	b := Billing{Product: "workbuddy", Region: "cn", HasMultiplier: true, Multiplier: &mult}

	s.RecordBilled("u1", "国服:deepseek-v4.1-flash", b, Counters{Input: 5, Output: 7})
	s.RecordBilled("u1", "deepseek-v4.1-flash", b, Counters{Input: 100, Output: 200})

	snap := s.Snapshot(0)
	mb, _ := snap["modelBilling"].(map[string]any)
	if mb == nil {
		t.Fatal("modelBilling 缺失")
	}
	if len(mb) != 1 {
		t.Fatalf("modelBilling 应只有 1 个模型键，实际 %d 个：%v", len(mb), keysOf(mb))
	}
	if _, ok := mb["deepseek-v4.1-flash"]; !ok {
		t.Errorf("modelBilling 的键应是裸名，实际 %v", keysOf(mb))
	}
}

// TestEmptyModelStillBecomesDash 空模型名的既有行为不能被破坏。
func TestEmptyModelStillBecomesDash(t *testing.T) {
	s := New("")
	s.Record("u1", "", Counters{Input: 1, Output: 1})

	snap := s.Snapshot(0)
	models := groupKeys(t, snap, "models")
	if len(models) != 1 || models[0] != "-" {
		t.Errorf("空模型名应记为 '-'，实际 %v", models)
	}
}

// TestLoadMergesLegacyPrefixedKeys 磁盘上的**历史**带前缀键，装载时要归并。
//
// # 这条直接对应所有者截图里那两行
//
// 他的 usage.json 里实际存着（实测确认）：
//
//	models["zcode:GLM-5.3-Flash"]
//	models["国服:deepseek-v4.1-flash"]
//
// 这些是**旧版本写下的**。只堵住新写入只会让它们"不再增长"，
// 界面里那两行**依然在** —— 而所有者的要求是"不要单独列一个统计"。
//
// 故装载时也要归一。本用例用真实的磁盘形态验证归并 + **累加**（不能丢用量）。
func TestLoadMergesLegacyPrefixedKeys(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "usage.json")

	legacy := map[string]any{
		"version": 1,
		"days":    map[string]any{"2026-09-21": map[string]any{"input": 105, "output": 207, "records": 2}},
		"models": map[string]any{
			// 旧版本写下的两种写法
			"国服:deepseek-v4.1-flash": map[string]any{
				"2026-09-21": map[string]any{"input": 5, "output": 7, "records": 1},
			},
			"zcode:GLM-5.3-Flash": map[string]any{
				"2026-09-21": map[string]any{"input": 85622, "output": 31, "records": 2},
			},
			// 以及本来就正常的裸名
			"deepseek-v4.1-flash": map[string]any{
				"2026-09-21": map[string]any{"input": 100, "output": 200, "records": 1},
			},
		},
		"accounts": map[string]any{},
		"accountModels": map[string]any{
			"u1": map[string]any{
				"国服:deepseek-v4.1-flash": map[string]any{
					"2026-09-21": map[string]any{"input": 5, "output": 7, "records": 1},
				},
				"deepseek-v4.1-flash": map[string]any{
					"2026-09-21": map[string]any{"input": 100, "output": 200, "records": 1},
				},
			},
		},
	}
	blob, _ := json.Marshal(legacy)
	if err := os.WriteFile(path, blob, 0o644); err != nil {
		t.Fatal(err)
	}

	s := New(path)
	snap := s.Snapshot(0)

	keys := groupKeys(t, snap, "models")
	for _, k := range keys {
		if strings.Contains(k, ":") {
			t.Errorf("装载后仍有带前缀的模型键 %q —— 界面会多出一行", k)
		}
	}
	// 用量必须**累加**（5+7 归并进 deepseek 那份，不能丢）
	if got, want := modelTotal(t, snap, "deepseek-v4.1-flash"), int64(105+207); got != want {
		t.Errorf("归并后 deepseek-v4.1-flash 总量应为 %d（两份相加），实际 %d", want, got)
	}
	if got := modelTotal(t, snap, "GLM-5.3-Flash"); got != 85653 {
		t.Errorf("归并后 GLM-5.3-Flash 总量应为 85653，实际 %d", got)
	}
	// accountModels 那一维同样不能漏
	am, _ := snap["accountModels"].(map[string]any)
	if am != nil {
		if per, ok := am["u1"].([]map[string]any); ok {
			for _, m := range per {
				if k, _ := m["key"].(string); strings.Contains(k, ":") {
					t.Errorf("accountModels 装载后仍有带前缀的键 %q", k)
				}
			}
		}
	}
}

// TestNormalizeModelKeysKeepsTotals 归并必须**累加**而不是覆盖。
//
// 覆盖会让用量凭空变少 —— 比多显示一行更糟（用户会以为数据丢了）。
func TestNormalizeModelKeysKeepsTotals(t *testing.T) {
	in := map[string]map[string]Counters{
		"国服:glm-5.3": {"2026-09-21": {Input: 5, Output: 7, Records: 1}},
		"glm-5.3":     {"2026-09-21": {Input: 100, Output: 200, Records: 1}},
	}
	out := normalizeModelKeys(in)
	if len(out) != 1 {
		t.Fatalf("应归并成 1 个键，实际 %d 个", len(out))
	}
	got := out["glm-5.3"]["2026-09-21"]
	if got.Input != 105 || got.Output != 207 || got.Records != 2 {
		t.Errorf("应累加为 input=105 output=207 records=2，实际 %+v", got)
	}
}

// TestNormalizeModelKeysNoopWhenClean 全是裸名时**原样返回**（不重建 map）。
//
// 为什么要断言"同一底层 map"：这份数据结构可能很大，每次启动都全量复制
// 会拖慢启动。故实现里先探测"是否需要归一"，不需要就返回入参本身。
func TestNormalizeModelKeysNoopWhenClean(t *testing.T) {
	in := map[string]map[string]Counters{
		"glm-5.3": {"2026-09-21": {Input: 1}},
	}
	out := normalizeModelKeys(in)
	if len(out) != 1 {
		t.Fatal("不该改变内容")
	}
	// 改 out 是否影响 in —— 同一底层 map 才说明没有重建
	out["glm-5.3"]["2026-09-21"] = Counters{Input: 999}
	if in["glm-5.3"]["2026-09-21"].Input != 999 {
		t.Error("全是裸名时应原样返回（同一底层 map），不应重建")
	}
}

// ---- 测试辅助 ----

// modelsOf 取 Snapshot 的 `models` 维度（形状是 `[{"key":..,"total":..}]`）。
//
// ⚠ 我第一版把它当成 `{模型: {日期: counters}}` 的嵌套 map，
// 于是三个用例全红在"实际 0 个"——**是我读错了结构，不是代码错**。
// 快照早已把各维度拍平成数组（见 groupList），照实际形状读即可。
func modelsOf(t *testing.T, snap map[string]any) []map[string]any {
	t.Helper()
	arr, _ := snap["models"].([]map[string]any)
	return arr
}

// groupKeys 取某分组维度的全部 key（已拍平的数组形状）。
func groupKeys(t *testing.T, snap map[string]any, dim string) []string {
	t.Helper()
	arr, _ := snap[dim].([]map[string]any)
	out := make([]string, 0, len(arr))
	for _, m := range arr {
		if k, ok := m["key"].(string); ok {
			out = append(out, k)
		}
	}
	return out
}

// keysOf 取任意 map 的键（billed 那类仍是嵌套 map）。
func keysOf(m map[string]any) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	return out
}

// modelTotal 取某模型在 `models` 维度里的总用量。
func modelTotal(t *testing.T, snap map[string]any, model string) int64 {
	t.Helper()
	for _, m := range modelsOf(t, snap) {
		if k, _ := m["key"].(string); k == model {
			return intOf(m["total"])
		}
	}
	return 0
}
