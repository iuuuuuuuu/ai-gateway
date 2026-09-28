package upstream

// account_unusable_test.go 锁定「账号不可用」这个独立错误类的识别。
//
// # 守的是所有者 2026-09-27 报的缺陷 + 他澄清的调度模型
//
// 所有者原话：
//
//	「跟区域没关系，就是成本有关系」
//	「所有的请求模型的策略，都应该按照成本优先的调度去调度，
//	  然后账号不可用就冷却，然后这时候再自动降级到国服的才对」
//	「要区分是模型冷却，还是账号冷却，还是账号封禁，这是三个概念，需要区分」
//
// # 缺陷链（实测）
//
// 11140 此前落进通用 `ErrClient`，而 `applyErrorPolicy` 对 `ErrClient` 是
// **"只换号不罚"** ⇒ 坏号**永不冷却** ⇒ 成本分层永远卡在最便宜却打不通的
// 那一档 ⇒ **降级不下去**。
//
//	修复前：裸名 `deepseek-v4.1-flash` → 0/6 成功（全 502）
//	修复后：同条件 → 7/8 成功（坏号冷却后自动落到可用账号）
//
// 这些测试锁住「11140 必须被单独识别」——它是冷却得以发生的前提。

import "testing"

// TestClassifyAccountUnusable 403 + 11140 必须判成 ErrAccountUnusable。
//
// 这是整条修复链的入口：判错成 ErrClient 就永远不会冷却。
func TestClassifyAccountUnusable(t *testing.T) {
	body := `{"code":11140,"msg":"request illegal","requestId":"abc",` +
		`"displayMsg":{"en":"The content did not pass the safety review.",` +
		`"zh":"内容未通过安全审核"}}`
	if got := Classify(403, body); got != ErrAccountUnusable {
		t.Fatalf("403+11140 应判成 ErrAccountUnusable（否则坏号永不冷却、"+
			"成本分层降不下去），实际 %v", got)
	}
}

// TestAccountUnusableIsNotClient 必须与通用 ErrClient 分开。
//
// 防"图省事合并回去"：ErrClient 的处置是"只换号不罚"，
// 合并会让 2026-09-27 的 0/6 缺陷原样复现。
func TestAccountUnusableIsNotClient(t *testing.T) {
	body := `{"code":11140,"msg":"request illegal"}`
	if got := Classify(403, body); got == ErrClient {
		t.Fatal("11140 不能归进 ErrClient —— 那正是本次缺陷的根因")
	}
}

// TestAccountUnusableDistinctFromOtherTwo 三类"不可用"必须互不混淆。
//
// 所有者明确要求区分：
//
//	模型冷却（ErrModelRate）      只有这个模型不可用，换模型仍可用
//	账号冷却（ErrAccountUnusable）整号暂时打不通，到期自动恢复
//	账号封禁（ErrSessionDead）    凭证失效，必须人工重登
//
// 判据不同、恢复方式不同，混用会让"等一会儿"与"必须重登"变成同一件事。
func TestAccountUnusableDistinctFromOtherTwo(t *testing.T) {
	cases := []struct {
		name string
		body string
		want ErrKind
	}{
		{"账号冷却", `{"code":11140,"msg":"request illegal"}`, ErrAccountUnusable},
		{"模型冷却", `{"code":6004,"msg":"当前模型已达使用上限"}`, ErrModelRate},
		{"账号封禁", `{"code":12153,"msg":"offline session"}`, ErrSessionDead},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			status := 403
			if c.want == ErrModelRate {
				status = 429
			}
			got := Classify(status, c.body)
			if got != c.want {
				t.Fatalf("%s 应判成 %v，实际 %v —— 三者混淆会让恢复方式错配",
					c.name, c.want, got)
			}
			// 三者必须两两不同。
			for _, other := range cases {
				if other.want != c.want && got == other.want {
					t.Fatalf("%s 被判成了 %v（与 %s 混淆）", c.name, other.want, other.name)
				}
			}
		})
	}
}

// TestIsAccountUnusableOnlyMatchesOwnCode 判据只认自己的码，不误伤别的。
//
// 关键：11102（模型不在该区域）是**请求侧**错误，换号无用 ——
// 若被这里误收，会把所有账号挨个冷却一遍，而模型名依然错。
func TestIsAccountUnusableOnlyMatchesOwnCode(t *testing.T) {
	yes := []string{
		`{"code":11140,"msg":"request illegal"}`,
		`{"code": 11140 }`,
	}
	for _, b := range yes {
		if !IsAccountUnusable(b) {
			t.Errorf("应识别为账号不可用: %s", b)
		}
	}
	no := []string{
		`{"code":11102,"msg":"model service info not found"}`, // 请求侧，不冷却账号
		`{"code":400,"msg":"invalid json"}`,                   // 请求侧
		`{"code":11115,"msg":"context too long"}`,             // 请求侧
		`{"code":6004,"msg":"model rate limited"}`,            // 模型冷却
		`{"code":0,"msg":"ok"}`,
		``,
	}
	for _, b := range no {
		if IsAccountUnusable(b) {
			t.Errorf("不该识别为账号不可用（误伤会让健康账号被冷却，"+
				"或让请求侧错误触发无意义的轮转）: %s", b)
		}
	}
}

// TestAccountUnusableKindString 类别名可读（日志与前端直接用）。
func TestAccountUnusableKindString(t *testing.T) {
	if got := ErrAccountUnusable.String(); got != "account_unusable" {
		t.Errorf("类别名应为 account_unusable，实际 %q", got)
	}
}

// TestClassifyUnaffectedCodes 本次新增不得改变既有分类（零漂移）。
//
// 11140 的加入位置在 404 判定之后、5xx/4xx 之前，理论上不影响别的码 ——
// 这条把它钉成事实。
func TestClassifyUnaffectedCodes(t *testing.T) {
	cases := []struct {
		status int
		body   string
		want   ErrKind
	}{
		{402, `{}`, ErrHardCredit},
		{429, `{}`, ErrSoftRate},
		{404, `{}`, ErrNotFound},
		{500, `{}`, ErrServer},
		{503, `{}`, ErrServer},
		{403, `<html>403 Forbidden</html>`, ErrClient},
		{400, `{"code":11102}`, ErrModelNotInRegion},
		{200, `{"code":0}`, ErrNone},
	}
	for _, c := range cases {
		if got := Classify(c.status, c.body); got != c.want {
			t.Errorf("Classify(%d, %q) = %v，期望 %v（新增 kind 不该改动既有分类）",
				c.status, c.body, got, c.want)
		}
	}
}
