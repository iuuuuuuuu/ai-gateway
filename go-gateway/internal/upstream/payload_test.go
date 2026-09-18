package upstream

import (
	"encoding/json"
	"testing"
)

// TestNormalizeRoles 验证出站请求体把 developer 角色归一为 system。
// 上游 role 白名单不含 developer（OpenAI 新规范的 system 别名），
// 命中即 HTTP 400 code=11128；此处走 PrepareBodyOptWithEfforts 全链路断言。
func TestNormalizeRoles(t *testing.T) {
	cases := []struct {
		name      string
		body      string
		wantRoles []string // 与输出 messages 逐条对应的期望 role；len 即消息数
	}{
		{"developer 改写为 system",
			`{"messages":[{"role":"developer","content":"x"}]}`, []string{"system"}},
		{"Developer 首字母大写改写",
			`{"messages":[{"role":"Developer","content":"x"}]}`, []string{"system"}},
		{"DEVELOPER 全大写改写",
			`{"messages":[{"role":"DEVELOPER","content":"x"}]}`, []string{"system"}},
		{"前后空白 TrimSpace 后改写",
			`{"messages":[{"role":" developer ","content":"x"}]}`, []string{"system"}},
		{"system 原样保留",
			`{"messages":[{"role":"system","content":"x"}]}`, []string{"system"}},
		{"user 原样保留",
			`{"messages":[{"role":"user","content":"x"}]}`, []string{"user"}},
		{"assistant 原样保留",
			`{"messages":[{"role":"assistant","content":"x"}]}`, []string{"assistant"}},
		{"tool 原样保留（不因未知而改写）",
			`{"messages":[{"role":"tool","content":"x"}]}`, []string{"tool"}},
		{"messages 缺失不 panic 且其余字段不变",
			`{"model":"glm-5.2"}`, []string{}},
		{"messages 为空数组不 panic",
			`{"messages":[]}`, []string{}},
		{"混合消息仅 developer 被改写",
			`{"messages":[{"role":"developer","content":"a"},{"role":"user","content":"b"},{"role":"developer","content":"c"}]}`,
			[]string{"system", "user", "system"}},
		{"sanitize=false 时仍归一（与脱敏解耦）",
			`{"messages":[{"role":"developer","content":"x"}]}`, []string{"system"}},
		{"非对象消息元素跳过、其余正常处理",
			`{"messages":["str",{"role":"developer","content":"x"},42]}`, []string{"system"}},
	}

	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			// 全程 sanitize=false：验证 role 归一与内容脱敏开关无关（D4）。
			out := PrepareBodyOptWithEfforts([]byte(c.body), false, nil)
			var obj map[string]any
			if err := json.Unmarshal(out, &obj); err != nil {
				t.Fatalf("unmarshal: %v (out=%s)", err, out)
			}

			// 提取输出 messages 里的 role（非对象元素跳过，不 panic）。
			var got []string
			if msgs, ok := obj["messages"].([]any); ok {
				for _, m := range msgs {
					msg, ok := m.(map[string]any)
					if !ok {
						continue
					}
					if role, ok := msg["role"].(string); ok {
						got = append(got, role)
					}
				}
			}

			if len(got) != len(c.wantRoles) {
				t.Fatalf("role 数量不符: got %v (%d) want %v (%d)", got, len(got), c.wantRoles, len(c.wantRoles))
			}
			for i := range got {
				if got[i] != c.wantRoles[i] {
					t.Errorf("role[%d] = %q want %q", i, got[i], c.wantRoles[i])
				}
			}
		})
	}

	// messages 缺失时，其余字段必须原样保留（除强制 stream）。
	t.Run("messages 缺失时其余字段不变", func(t *testing.T) {
		out := PrepareBodyOptWithEfforts([]byte(`{"model":"glm-5.2","temperature":0.7}`), false, nil)
		var obj map[string]any
		if err := json.Unmarshal(out, &obj); err != nil {
			t.Fatalf("unmarshal: %v", err)
		}
		if obj["model"] != "glm-5.2" || obj["temperature"] != 0.7 {
			t.Errorf("其余字段被改动: %v", obj)
		}
	})
}

// TestPrepareBodyOptWithEfforts 档位**一律原样透传**，网关不改写。
//
// ⚠ 本用例在 2026-09-18 被**整体反转**：原内容是「降级 / 取最低档」，
// 即把请求档位改写成 supportedEfforts 里最接近的档。实测推翻了那个前提：
//
//	hy3     声明 [low, high]     → medium / max / minimal / **off** 全部接受
//	glm-5.2 声明 [high, xhigh]  → low / max / medium / **off** 全部接受
//
// 上游对范围外的档照常接受 ⇒ supportedEfforts 只是「界面建议列出哪些」，
// 不是可用范围。既然原档位本来就有效，改写就纯粹是**改坏**：
// 用户调 max 被悄悄降成 high，他只看到「调了没效果」。
//
// 现在唯一的权威判据是上游自己。本用例锁住「网关不碰 reasoning_effort」。
func TestPrepareBodyOptWithEfforts(t *testing.T) {
	efforts := map[string][]string{
		"glm-5.2":      {"off", "low", "high"},
		"glm-5.2-mini": {"low", "medium"},
		"glm-5.2-max":  {"high", "xhigh"},
	}
	// 每一条的期望值都是**输入原值** —— 这就是本用例的全部含义。
	cases := []struct {
		name    string
		body    string
		efforts map[string][]string
		wantKey string // 输出应带有的 effort 字段名；空表示该字段应不存在
		wantVal string // 期望值（= 输入值）
	}{
		{"范围外的档原样保留（曾被降级成 medium）",
			`{"model":"glm-5.2-mini","reasoning_effort":"high"}`, efforts, "reasoning_effort", "high"},
		{"低于全部支持档时也原样保留（曾被抬成 high）",
			`{"model":"glm-5.2-max","reasoning_effort":"low"}`, efforts, "reasoning_effort", "low"},
		{"范围内的档原样保留",
			`{"model":"glm-5.2","reasoning_effort":"low"}`, efforts, "reasoning_effort", "low"},
		{"off 原样保留（上游可能接受，也由它自己拒绝）",
			`{"model":"glm-5.2","reasoning_effort":"off"}`, efforts, "reasoning_effort", "off"},
		{"camelCase 拼写同样原样保留",
			`{"model":"glm-5.2-mini","reasoningEffort":"high"}`, efforts, "reasoningEffort", "high"},
		{"未知模型原样透传",
			`{"model":"unknown","reasoning_effort":"max"}`, efforts, "reasoning_effort", "max"},
		{"无法识别的档位名原样透传（让上游去报错）",
			`{"model":"glm-5.2","reasoning_effort":"ultra"}`, efforts, "reasoning_effort", "ultra"},
		{"缓存为空时原样透传",
			`{"model":"glm-5.2","reasoning_effort":"max"}`, map[string][]string{}, "reasoning_effort", "max"},
		{"没带该字段时不新增",
			`{"model":"glm-5.2-mini","messages":[]}`, efforts, "", ""},
		{"efforts 为 nil 时原样透传",
			`{"model":"glm-5.2","reasoning_effort":"max"}`, nil, "reasoning_effort", "max"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			out := PrepareBodyOptWithEfforts([]byte(c.body), false, c.efforts)
			var m map[string]any
			if err := json.Unmarshal(out, &m); err != nil {
				t.Fatalf("unmarshal: %v (body=%s)", err, out)
			}
			if c.wantKey == "" {
				if _, ok := m["reasoning_effort"]; ok {
					t.Errorf("reasoning_effort 不该被新增，实际 %v", m["reasoning_effort"])
				}
				if _, ok := m["reasoningEffort"]; ok {
					t.Errorf("reasoningEffort 不该被新增，实际 %v", m["reasoningEffort"])
				}
				return
			}
			got, ok := m[c.wantKey].(string)
			if !ok || got != c.wantVal {
				t.Errorf("%s: got %v (%T) want %q —— 档位必须原样透传，网关不得改写",
					c.wantKey, m[c.wantKey], m[c.wantKey], c.wantVal)
			}
		})
	}
}
