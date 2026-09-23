package usage

// billing_persist_test.go —— 钉住「计费归属（平台）必须持久化，并回填历史」。
//
// # 所有者报的现场（2026-09-21）
//
// 他的截图：
//
//	按模型                    共 4 个
//	deepseek-v4.1-flash                1.7B      ← 4223 次
//	…
//
// 而同一模型的**计费明细**只有 **3 次** —— 因为计费归属此前
// **只在内存里**（`fileState` 没有 Billed/BillingMeta 字段），
// 网关一重启就清零。
//
// 所有者原话：
//
//	「这个应该持久化平台 显示啊,这要不刚开始都没办法区分」
//
// 「刚开始没办法区分」正是这个缺陷的直接后果：越早的用量越没有归属。
//
// # 三层修复，本文件逐层锁住
//
//	① 落盘      —— fileState 带上 Billed / BillingMeta
//	② 装载      —— 读回来（含带前缀的旧键归一）
//	③ 回填历史  —— 用 accountModels 的 uid 形态反推平台
//
// ③ 只能补 `product`，**不能**补 region/倍率（历史数据里没有）。
// 编一个会让计费显示错误 —— 比显示"未记录"更糟，故显式断言"不编造"。

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

// ---- ① 落盘 + ② 装载：往返 ----

func TestBillingSurvivesRestart(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "usage.json")

	mult := 0.03
	b := Billing{Product: "workbuddy", Region: "cn", HasMultiplier: true, Multiplier: &mult}

	s1 := New(path)
	s1.RecordBilled("u1", "deepseek-v4.1-flash", b, Counters{Input: 100, Output: 20, Records: 1})
	s1.Flush()

	// 重启（新实例读同一个文件）
	s2 := New(path)
	snap := s2.Snapshot(0)

	mb, ok := snap["modelBilling"].(map[string]any)
	if !ok {
		t.Fatal("重启后 modelBilling 丢失 —— 这正是所有者报的「刚开始没办法区分」")
	}
	arr, ok := mb["deepseek-v4.1-flash"].([]map[string]any)
	if !ok || len(arr) == 0 {
		t.Fatalf("重启后该模型的计费归属丢失：%v", mb)
	}

	// 平台、区域、倍率都要在
	var found bool
	for _, g := range arr {
		if g["product"] != "workbuddy" {
			continue
		}
		found = true
		if g["region"] != "cn" {
			t.Errorf("区域应保留，实际 %v", g["region"])
		}
		if g["hasMultiplier"] != true {
			t.Errorf("hasMultiplier 应保留，实际 %v", g["hasMultiplier"])
		}
		if g["creditMultiplier"] != 0.03 {
			t.Errorf("倍率应保留为 0.03，实际 %v", g["creditMultiplier"])
		}
	}
	if !found {
		t.Errorf("未找到 workbuddy 的归属条目：%v", arr)
	}
}

func TestBillingMetaSurvivesRestart(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "usage.json")

	// 未声明倍率（nil）也不能在往返中变成 0 —— 那是三态语义的落点
	b := Billing{Product: "qoder", HasMultiplier: true, Multiplier: nil}
	s1 := New(path)
	s1.RecordBilled("qoder-abc", "Qwen3.8-Flash", b, Counters{Input: 10, Records: 1})
	s1.Flush()

	s2 := New(path)
	snap := s2.Snapshot(0)
	mb, _ := snap["modelBilling"].(map[string]any)
	arr, _ := mb["Qwen3.8-Flash"].([]map[string]any)
	if len(arr) == 0 {
		t.Fatal("归属丢失")
	}
	for _, g := range arr {
		if g["product"] != "qoder" {
			continue
		}
		if g["creditMultiplier"] != nil {
			t.Errorf("未声明倍率应保持 null，实际 %v —— "+
				"把它变成 0 会让界面显示「免费」，而实际是「未声明」", g["creditMultiplier"])
		}
	}
}

// ---- ③ 回填：uid 形态 → 平台 ----

func TestProductFromUID(t *testing.T) {
	cases := []struct {
		uid  string
		want string
		why  string
	}{
		{"zcode-abcdef123456", "zcode", "带前缀"},
		{"qoder-00000000-0000-4000-8000-000000000001", "qoder", "带前缀"},
		{"00000000-0000-4000-8000-000000000002", "workbuddy", "裸 UUID（auth 层空 Product 归一为 workbuddy）"},
		// ⚠ 这里原本写的是一个**真实账号 UUID**（从线上凭证抄来的）。
		// 本仓库是公开仓库，真实标识不得入库 —— 换成一眼看出是假的占位值。
		// 该用例只验证"裸 UUID 判成 workbuddy"，与具体取值无关，故占位值等效。
		{"12345678-1234-4123-8123-123456789012", "workbuddy", "裸 UUID（占位值）"},
		// 判不出的必须返回空，**不能**默认成某个平台
		{"", "", "空 uid"},
		{"some-random-key", "", "形态未知"},
		{"workbuddy-abc", "", "历史数据里没有这个前缀形态，别猜"},
		{"00000000-0000-4000-8000-00000000000", "", "长度不足 36，不是 UUID"},
		{"00000000x0000-4000-8000-000000000002", "", "分隔符不对"},
	}
	for _, c := range cases {
		if got := productFromUID(c.uid); got != c.want {
			t.Errorf("productFromUID(%q) = %q，期望 %q（%s）", c.uid, got, c.want, c.why)
		}
	}
}

func TestIsBareUUID(t *testing.T) {
	cases := map[string]bool{
		"00000000-0000-4000-8000-000000000002": true,
		// 大写变体：IsBareUUID 必须大小写不敏感（用明显假的十六进制字母）
		"ABCDEF00-1234-4ABC-8DEF-ABCDEF012345": true,
		"00000000-0000-4000-8000-00000000000":  false, // 少一位
		"00000000-0000-4000-8000-0000000000022": false, // 多一位
		"00000000x0000-4000-8000-000000000002": false, // 第一个分隔符错
		"zcode-abcdef123456":                   false,
		"":                                     false,
	}
	for in, want := range cases {
		if got := isBareUUID(in); got != want {
			t.Errorf("isBareUUID(%q) = %v，期望 %v", in, got, want)
		}
	}
}

// TestBackfillAttributesHistoricalUsage 无归属的历史用量要按账号 uid 补上平台。
//
// 用真实的磁盘形态（只有 accountModels、没有 billed）构造，
// 模拟"旧版本跑出来的文件"。
func TestBackfillAttributesHistoricalUsage(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "usage.json")

	legacy := map[string]any{
		"version": 1,
		"days":    map[string]any{"2026-09-21": map[string]any{"input": 165, "output": 27, "records": 3}},
		// 按模型：有量但无归属
		"models": map[string]any{
			"deepseek-v4.1-flash": map[string]any{
				"2026-09-21": map[string]any{"input": 110, "output": 22, "records": 2},
			},
			"GLM-5.3-Flash": map[string]any{
				"2026-09-21": map[string]any{"input": 55, "output": 5, "records": 1},
			},
		},
		"accounts": map[string]any{},
		// 交叉维度：这里才带账号 → 可反推平台
		"accountModels": map[string]any{
			"00000000-0000-4000-8000-000000000002": map[string]any{ // 裸 UUID → workbuddy
				"deepseek-v4.1-flash": map[string]any{
					"2026-09-21": map[string]any{"input": 100, "output": 20, "records": 1},
				},
			},
			"e94c5d4f-17ac-47db-8d31-b80d139e8bfb": map[string]any{ // 裸 UUID → workbuddy
				"deepseek-v4.1-flash": map[string]any{
					"2026-09-21": map[string]any{"input": 10, "output": 2, "records": 1},
				},
			},
			"zcode-abcdef123456": map[string]any{ // → zcode
				"GLM-5.3-Flash": map[string]any{
					"2026-09-21": map[string]any{"input": 55, "output": 5, "records": 1},
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
	mb, ok := snap["modelBilling"].(map[string]any)
	if !ok {
		t.Fatal("回填后应有 modelBilling")
	}

	// deepseek：两个 WorkBuddy 账号的用量应**合并成一条** workbuddy
	ds, _ := mb["deepseek-v4.1-flash"].([]map[string]any)
	if len(ds) != 1 {
		t.Fatalf("deepseek 应回填成 1 条（两个账号都是 workbuddy，合并），实际 %d 条：%v", len(ds), ds)
	}
	if ds[0]["product"] != "workbuddy" {
		t.Errorf("deepseek 的平台应为 workbuddy，实际 %v", ds[0]["product"])
	}
	if got := intOf(ds[0]["total"]); got != 110+22 {
		t.Errorf("合并后总量应为 %d（100+20+10+2），实际 %d", 132, got)
	}

	// GLM-5.3-Flash：zcode 账号用过 → 回填 zcode
	glm, _ := mb["GLM-5.3-Flash"].([]map[string]any)
	if len(glm) != 1 {
		t.Fatalf("GLM 应回填成 1 条，实际 %d 条", len(glm))
	}
	if glm[0]["product"] != "zcode" {
		t.Errorf("GLM 的平台应为 zcode，实际 %v", glm[0]["product"])
	}
}

// TestBackfillDoesNotInventRegionOrMultiplier 回填**不得**编造区域与倍率。
//
// # 为什么单列一条
//
// 历史数据里没有区域信息，而倍率**依赖区域**（国服 0.03 / 国际版 0）。
// 若为了"让界面好看"填一个默认值，用户会看到错误的计费数字 ——
// 那是**编造事实**，比显示"未记录"严重得多。
func TestBackfillDoesNotInventRegionOrMultiplier(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "usage.json")

	legacy := map[string]any{
		"version": 1,
		"days":    map[string]any{"2026-09-21": map[string]any{"input": 10, "records": 1}},
		"models":  map[string]any{},
		"accounts": map[string]any{},
		"accountModels": map[string]any{
			"00000000-0000-4000-8000-000000000002": map[string]any{
				"deepseek-v4.1-flash": map[string]any{
					"2026-09-21": map[string]any{"input": 10, "records": 1},
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
	mb, _ := snap["modelBilling"].(map[string]any)
	arr, _ := mb["deepseek-v4.1-flash"].([]map[string]any)
	if len(arr) == 0 {
		t.Fatal("应回填出条目")
	}
	g := arr[0]
	if _, ok := g["region"]; ok {
		t.Errorf("回填**不该**编造 region（历史数据没有它），实际 %v", g["region"])
	}
	if _, ok := g["hasMultiplier"]; ok {
		t.Errorf("回填**不该**设 hasMultiplier —— 那会让界面把「未记录」显示成「免费」")
	}
	if _, ok := g["creditMultiplier"]; ok {
		t.Errorf("回填**不该**编造倍率，实际 %v", g["creditMultiplier"])
	}
}

// TestBackfillDoesNotDuplicateExistingBilling 已有归属的 (模型,平台) 不重复回填。
//
// 否则同一平台会出现两行 —— 而所有者要的恰恰是"别拆错"。
func TestBackfillDoesNotDuplicateExistingBilling(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "usage.json")

	mult := 0.03
	legacy := map[string]any{
		"version": 1,
		"days":    map[string]any{"2026-09-21": map[string]any{"input": 110, "records": 2}},
		"models": map[string]any{
			"deepseek-v4.1-flash": map[string]any{
				"2026-09-21": map[string]any{"input": 110, "records": 2},
			},
		},
		"accounts": map[string]any{},
		// 已有真实归属（带倍率）
		"billed": map[string]any{
			"deepseek-v4.1-flash": map[string]any{
				"workbuddy|cn|0.03": map[string]any{
					"2026-09-21": map[string]any{"input": 100, "records": 1},
				},
			},
		},
		"billingMeta": map[string]any{
			"deepseek-v4.1-flash\x00workbuddy|cn|0.03": map[string]any{
				"Product": "workbuddy", "Region": "cn",
				"HasMultiplier": true, "Multiplier": mult,
			},
		},
		"accountModels": map[string]any{
			"00000000-0000-4000-8000-000000000002": map[string]any{
				"deepseek-v4.1-flash": map[string]any{
					"2026-09-21": map[string]any{"input": 10, "records": 1},
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
	mb, _ := snap["modelBilling"].(map[string]any)
	arr, _ := mb["deepseek-v4.1-flash"].([]map[string]any)

	// 该模型在 workbuddy 上**已有**归属 → 不该再回填一条同平台的
	n := 0
	for _, g := range arr {
		if g["product"] == "workbuddy" {
			n++
		}
	}
	if n != 1 {
		t.Errorf("workbuddy 该只有 1 条（已有归属优先），实际 %d 条：%v", n, arr)
	}
}

// ---- 真实数据形态的冒烟检查 ----

// TestSnapshotShapeIsStableForFrontend 快照的 modelBilling 形状必须稳定。
//
// 前端按 `[{key,product?,region?,hasMultiplier?,creditMultiplier?,total,…}]`
// 读取（见 GatewayPage.tsx 的 UsageBillingRow）。本用例把形状固定下来，
// 防止回填逻辑引入前端不认识的字段或丢掉已知字段。
func TestSnapshotShapeIsStableForFrontend(t *testing.T) {
	s := New("")
	mult := 0.06
	s.RecordBilled("00000000-0000-4000-8000-000000000002", "glm-5.3",
		Billing{Product: "workbuddy", Region: "intl", HasMultiplier: true, Multiplier: &mult},
		Counters{Input: 1, Output: 2, Records: 1})

	snap := s.Snapshot(0)
	mb, _ := snap["modelBilling"].(map[string]any)
	arr, _ := mb["glm-5.3"].([]map[string]any)
	if len(arr) != 1 {
		t.Fatalf("期望 1 条，实际 %d", len(arr))
	}
	g := arr[0]

	// 前端依赖的字段（缺一个界面就少一块信息）
	for _, k := range []string{"key", "product", "hasMultiplier", "creditMultiplier", "total", "records"} {
		if _, ok := g[k]; !ok {
			t.Errorf("modelBilling 条目缺字段 %q —— 前端读取它渲染倍率/归属", k)
		}
	}
	// 区域可选（分区平台才有），但有了就必须是字符串
	if v, ok := g["region"]; ok {
		if _, isStr := v.(string); !isStr {
			t.Errorf("region 应为字符串，实际 %T", v)
		}
	}
	// 不该出现内部私有键
	for k := range g {
		if len(k) > 0 && k[0] == '_' {
			t.Errorf("快照里泄漏了内部键 %q", k)
		}
	}
}
