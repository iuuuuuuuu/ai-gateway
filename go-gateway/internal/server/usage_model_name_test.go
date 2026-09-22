package server

// usage_model_name_test.go 钉住「路由前缀不得进入 Token 用量统计」。
//
// # 为什么需要它（2026-09-21 所有者报的缺陷）
//
// 所有者截图里，**同一个模型**被统计成了两个不同的条目：
//
//	deepseek-v4.1-flash · 国际版      732.9M   ← 用裸名请求产生的
//	国服:deepseek-v4.1-flash · 国服   12       ← 用带前缀的写法请求产生的
//
// 第三行明明是同一个模型（只是他写成了 `国服:deepseek-v4.1-flash`），
// 却因为统计直接用了**客户端原始请求体**里的名字而被割裂。
//
// 根因：`newChatStat` 用 `parseModelFromBody(body)` 填 `model`，
// 而 `recordUsage` 又直接把它当模型名写进用量表。前缀是给**选号**的指令，
// 不属于模型标识（`resolveModel` 早就剥掉了，只是没回填到统计）。
//
// # 修法
//
// `chatStat` 拆成两个字段：
//
//	model       原始写法（日志用 —— 要如实显示"他写的是 `国服:xxx`"）
//	usageModel  裸名（统计用）
//
// 本文件钉住"两者确实分开"，且**三种前缀写法**都被剥掉。

import (
	"testing"
	"time"
)

// TestUsageModelStripsRoutePrefix 三种路由前缀都不得进入统计用的模型名。
//
// 四种写法是所有者明确要求的协议（`平台:区域:模型名` 等），
// 故三种带前缀的形式都要覆盖 —— 只测一种会让另外两种的回归漏网。
func TestUsageModelStripsRoutePrefix(t *testing.T) {
	cases := []struct {
		raw      string
		wantUsed string
		why      string
	}{
		{"deepseek-v4.1-flash", "deepseek-v4.1-flash", "裸名：原样"},
		{"国服:deepseek-v4.1-flash", "deepseek-v4.1-flash", "区域前缀"},
		{"国际版:deepseek-v4.1-flash", "deepseek-v4.1-flash", "区域前缀（国际版）"},
		{"workbuddy:deepseek-v4.1-flash", "deepseek-v4.1-flash", "平台前缀"},
		{"workbuddy:国服:deepseek-v4.1-flash", "deepseek-v4.1-flash", "平台+区域（三段）"},
		{"zcode:GLM-5.3-Flash", "GLM-5.3-Flash", "ZCode 平台前缀"},
		{"qoder:国际版:Qwen3.8-Max", "Qwen3.8-Max", "Qoder 三段"},
		// 前缀不是枚举内的词 ⇒ 不剥离（`deepseek:v3` 里的 deepseek 不是平台）。
		{"deepseek:v3", "deepseek:v3", "非前缀段不剥离（否则会吃掉模型名）"},
	}

	for _, c := range cases {
		body := []byte(`{"model":"` + c.raw + `","messages":[]}`)
		st := newChatStat(time.Now(), body, false)

		if got := st.usageName(); got != c.wantUsed {
			t.Errorf("%s：用量统计名应为 %q，实际 %q（%s）", c.raw, c.wantUsed, got, c.why)
		}
		// 日志用的 model 必须保留**原始写法** —— 否则排查时看不出用户写了前缀。
		if st.model != c.raw {
			t.Errorf("%s：日志用的 model 应保留原始写法 %q，实际 %q", c.raw, c.raw, st.model)
		}
	}
}

// TestSetModelAlsoStripsPrefix messages / responses 两个入口走 setModel。
//
// 它们不用 `newChatStat` 解析出的 model，而是从各自的结构体取 `req.Model`
// 覆盖 —— 若 setModel 忘了同步 usageModel，那两个协议的用量仍会被割裂。
func TestSetModelAlsoStripsPrefix(t *testing.T) {
	st := newChatStat(time.Now(), []byte(`{"model":"x"}`), true)
	st.setModel("workbuddy:国际版:glm-5.3")

	if st.model != "workbuddy:国际版:glm-5.3" {
		t.Errorf("日志用 model 应保留原始写法，实际 %q", st.model)
	}
	if st.usageName() != "glm-5.3" {
		t.Errorf("统计名应为 glm-5.3，实际 %q", st.usageName())
	}
}

// TestUsageNameFallsBackToRaw 未回填时回落原始名（行为与修复前一致）。
//
// 兜底路径：万一某条新协议忘了调 setModel，统计应仍能工作（只是带前缀），
// 而不是记成空串 —— 空模型名会让用量表出现一个 "": 的怪条目。
func TestUsageNameFallsBackToRaw(t *testing.T) {
	st := &chatStat{model: "国服:some-model"}
	if got := st.usageName(); got != "国服:some-model" {
		t.Errorf("未回填 usageModel 时应回落 model，实际 %q", got)
	}
}

// TestUsageModelHandlesPlaceholder 解析失败（"-"）不得被当成模型名去剥。
//
// `parseModelFromBody` 在取不到 model 时返回 "-"。若拿它去 resolveModel，
// 得到的还是 "-"（无害），但这条钉住"我们不在这条路径上出错"。
func TestUsageModelHandlesPlaceholder(t *testing.T) {
	st := newChatStat(time.Now(), []byte(`{}`), false)
	if st.usageName() != "-" {
		t.Errorf("无 model 字段时应为 %q，实际 %q", "-", st.usageName())
	}
}
