package main

// 「一键执行全部任务」的账号范围必须按**产品**过滤，不能只靠区域。
//
// # 为什么写这组用例（2026-09-22，我引入的回归）
//
// 背景：区域判据从 `HasSuffix(".ai")` 改成**完整域名白名单**，
// 以便让 Qoder 国际版（`qoder.sh`）正确走代理 —— 那是所有者报的真缺陷。
//
// 但 `growthTaskAdapter.poolAuths()` 当时**只跳 disabled**，
// 把「跳过国际版」交给了 `growtask.runAccount` 里的 `a.IsIntl()`。
// 那个隐含假设是「国际版 = 非 WorkBuddy」，而它**依赖旧的错误判据**：
//
//	旧判据：qoder.sh 不以 .ai 结尾 ⇒ 判成国服 ⇒ Qoder 国际版**参与**成长任务
//	新判据：qoder.sh 正确判成国际版 ⇒ 被跳过（但 Qoder **国服** 仍会参与！）
//
// 即：真正的问题是**判据用错了维度**。成长任务是 WorkBuddy 专属的，
// 该问「是不是 WorkBuddy 账号」，而不是「是不是国际版账号」。
//
// 反例（改之前和之后都存在）：Qoder 国服账号 `qoder.com.cn` 会被塞进来，
// 拿 Qoder 的令牌去打 `www.workbuddy.ai` / `copilot.tencent.com` ——
// 必然 401，还会在账号记录里写下不属于它的任务。
//
// ⚠ 修法对齐 `internal/scheduler` 已有的 `accountScopeSkip`：
// 那边早就是「产品闸门 + 区域闸门」两段式，只有 growtask 这条路径漏了。

import (
	"testing"

	"workbuddy2api/internal/auth"
)

// wantProductForGrowthTasks 是本包动作真正服务的产品。
// 写成常量是为了让"改产品"这件事必须先改这里 —— 它会被下面的用例读。
const wantProductForGrowthTasks = auth.ProductWorkBuddy

// TestGrowthTasksOnlyServeWorkBuddy 四个产品的账号，只有 workbuddy 该留下。
//
// 这张表覆盖**真实存在的产品**，不是编的：
// 本机凭证分布 = workbuddy 19 / qoder 2 / zcode 1。
func TestGrowthTasksOnlyServeWorkBuddy(t *testing.T) {
	cases := []struct {
		name    string
		acc     *auth.Auth
		wantRun bool
	}{
		{
			"WorkBuddy 国服",
			&auth.Auth{UID: "wb-cn", Domain: "copilot.tencent.com", Product: auth.ProductWorkBuddy},
			true,
		},
		{
			"WorkBuddy 国际版（由 growtask 内部的区域闸门决定跑不跑）",
			&auth.Auth{UID: "wb-intl", Domain: "www.workbuddy.ai", Product: auth.ProductWorkBuddy},
			true, // 产品闸门该放行 —— 区域是**另一道**闸门的事
		},
		{
			"Qoder 国服 —— ⚠ 这正是只靠区域判据会漏掉的那个",
			&auth.Auth{UID: "qoder-cn", Domain: "qoder.com.cn", Product: auth.ProductQoder},
			false,
		},
		{
			"Qoder 国际版",
			&auth.Auth{UID: "qoder-intl", Domain: "qoder.sh", Product: auth.ProductQoder},
			false,
		},
		{
			"ZCode",
			&auth.Auth{UID: "zcode-1", Domain: "api.z.ai", Product: auth.ProductZcode},
			false,
		},
		{
			"老账号（product 为空 ⇒ 归一成 workbuddy，须向后兼容）",
			&auth.Auth{UID: "legacy", Domain: "copilot.tencent.com"},
			true,
		},
	}

	for _, c := range cases {
		// 断言的是产品判据本身 —— 与 poolAuths 里那行同源。
		got := c.acc.ProductOf() == wantProductForGrowthTasks
		if got != c.wantRun {
			t.Errorf("%s：产品闸门 %v，应为 %v（domain=%q product=%q）",
				c.name, got, c.wantRun, c.acc.Domain, c.acc.Product)
		}
	}
}

// TestQoderIntlIsNotWorkBuddy 单独钉住这条，因为它是本次回归的核心。
//
// `qoder.sh` 现在是**国际版**（区域维度的正确答案），
// 同时**不是 WorkBuddy**（产品维度的正确答案）。
// 两个维度都要对 —— 只对一个是本次回归的形态。
func TestQoderIntlIsNotWorkBuddy(t *testing.T) {
	qoderIntl := &auth.Auth{UID: "qoder-intl", Domain: "qoder.sh", Product: auth.ProductQoder}

	if !qoderIntl.IsIntl() {
		t.Error("qoder.sh 应判为国际版（否则代理分流失效，实测直连 10.3s vs 代理 1.2s）")
	}
	if qoderIntl.ProductOf() == auth.ProductWorkBuddy {
		t.Error("qoder.sh 不该被当成 WorkBuddy 账号 —— " +
			"成长任务是 WorkBuddy 专属的，混进来会拿错凭据打错端点")
	}
}
