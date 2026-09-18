// payload.go 改写发往上游的 chat 请求体：
//  1. 强制 stream:true（上游拒绝非流式）
//  2. tool_choice 归一化（上游该字段是 string，对象形式会 400 code=11101）
package upstream

import (
	"encoding/json"
	"log"
	"sort"
	"strings"
)

// PrepareBodyOpt 单 pass 改写；sanitize=false 时行为完全还原（仅强制 stream + 归一化 tool_choice）。
func PrepareBodyOpt(src []byte, sanitize bool) []byte {
	return PrepareBodyOptWithEfforts(src, sanitize, nil)
}

// PrepareBodyOptWithEfforts 在 PrepareBodyOpt 基础上按模型 supportedEfforts 降级 reasoning_effort：
// 仅当请求显式携带且模型不支持该档位时，改为 ≤请求档位的最高支持档；支持档全部高于请求档时取最低档；
// 未知模型/未知档位/未携带该字段一律透传。efforts 为 nil 表示未知（不降级）。
func PrepareBodyOptWithEfforts(src []byte, sanitize bool, efforts map[string][]string) []byte {
	return PrepareBodyForRegion(src, sanitize, efforts, false)
}

// PrepareBodyForRegion 在 PrepareBodyOptWithEfforts 基础上按账号区域做协议适配。
//
// intl=true（国际版 workbuddy.ai）时额外保证 messages 首条是 system ——
// 实测国际版对首条非 system 的请求返回 HTTP 400 code=11128
// "first message is not system prompt"（同一账号补上 system 首条即 200）。
//
// 只对国际版做这件事：国服的 11128 是另一种含义（渠道指纹未批准，见 sanitize.go），
// 国服并无「首条必须 system」的要求，不能混为一谈。
func PrepareBodyForRegion(src []byte, sanitize bool, efforts map[string][]string, intl bool) []byte {
	if len(src) == 0 {
		return src
	}
	var obj map[string]any
	if err := json.Unmarshal(src, &obj); err != nil {
		return src
	}
	obj["stream"] = true
	normalizeToolChoice(obj)
	normalizeRoles(obj)
	// 归一化之后再做国际版适配：developer 已被改写成 system，
	// 此时首条若已是 system 就不必补（否则会给 Codex 之类客户端多插一条）。
	if intl {
		ensureSystemFirst(obj)
	}
	normalizeReasoningEffort(obj, efforts)
	if sanitize {
		if msgs, ok := obj["messages"].([]any); ok {
			sanitizeMessages(msgs)
		}
	}
	out, err := json.Marshal(obj)
	if err != nil {
		return src
	}
	return out
}

// ensureSystemFirst 保证 messages[0] 是 system（国际版协议要求）。
//
// 仅在首条不是 system 时补一条**最小**的 system，不改动任何既有消息，也不合并 ——
// 合并会改变模型看到的对话结构，风险大于收益。空 messages（或缺失）时不动：
// 那种请求本就缺少上下文，交给上游报错更诚实。
//
// 补的内容刻意保持中性（"You are a helpful assistant."），因为这里无法得知
// 调用方想要的系统提示；它只为满足协议前提，不承载业务语义。
func ensureSystemFirst(obj map[string]any) {
	msgs, ok := obj["messages"].([]any)
	if !ok || len(msgs) == 0 {
		return
	}
	first, ok := msgs[0].(map[string]any)
	if !ok {
		return
	}
	if role, _ := first["role"].(string); strings.EqualFold(strings.TrimSpace(role), "system") {
		return
	}
	sys := map[string]any{"role": "system", "content": "You are a helpful assistant."}
	obj["messages"] = append([]any{sys}, msgs...)
	log.Printf("intl payload: messages 首条非 system（原 role=%v），已补一条 system", first["role"])
}

// effortRank 档位从低到高。
var effortRank = map[string]int{"off": 0, "minimal": 1, "low": 2, "medium": 3, "high": 4, "xhigh": 5, "max": 6}

// StandardEfforts 上游的**标准思考档阶梯**（由低到高，不含 off）。
//
// 用于「上游没声明 supportedEfforts，但有默认档」的模型：实测这类模型
// 接受标准阶梯里的全部档，只拒绝 off 与无法识别的值 —— 也就是说上游有
// 一套全局阶梯在校验，只是没在 /v3/config 里逐模型列出可选范围。
//
// 实测依据（2026-09-18，真实流式调用 deepseek-v4.1-flash）：
//
//	off / bogus_value → HTTP 400 "the reasoning effort value is not supported"
//	minimal/low/medium/high/max/xhigh → 全部接受
//	低到高各档的推理长度：low 397 → medium 420 → high 646 → max 776（单调递增）
//
// 不含 off 是刻意的：该模型明确拒绝 off，把它列进可选项会让用户选到一个
// 必然 400 的值。**注意**这不代表所有此类模型都拒绝 off —— 但既然上游
// 没声明范围，少列一个「已知可能被拒」的档，比多列一个更安全。
//
// 返回**副本**：调用方会把它塞进响应体，共享切片会被下游的 in-place 修改污染。
func StandardEfforts() []string {
	names := make([]string, 0, len(effortRank)-1)
	for name := range effortRank {
		if name == "off" {
			continue
		}
		names = append(names, name)
	}
	// 按档位从低到高排序：map 遍历顺序随机，不排序会让同一模型每次请求
	// 下发不同顺序的列表，客户端的下拉框顺序会跳变。
	sort.Slice(names, func(i, j int) bool { return effortRank[names[i]] < effortRank[names[j]] })
	return names
}

// KnownEffort 该档位名是否是网关认识的思考档（大小写与空白不敏感）。
//
// 导出它而不是让调用方各存一份档位表：档位表一旦分叉，就会出现
// 「校验用一个、改写用另一个」——同一请求在两道关里被判成不同结果。
// 服务端用它在选号前拒绝无法识别的档位名（那些名字此前被原样透传给上游，
// 上游多半静默忽略，用户同样看不到原因）。
func KnownEffort(name string) bool {
	_, ok := effortRank[strings.TrimSpace(strings.ToLower(name))]
	return ok
}

// normalizeReasoningEffort 已**废弃**（2026-09-18），保留空实现仅为守住调用点。
//
// 原行为：请求档位不在该模型 supportedEfforts 里时**静默改写**为最接近的支持档。
// 移除理由与 forward.go 里移除 400 拦截同源 —— 实测证明 supportedEfforts
// 不是硬范围：
//
//	hy3     声明 [low, high]     → medium / max / minimal / **off** 全部接受
//	glm-5.2 声明 [high, xhigh]  → low / max / medium / **off** 全部接受
//
// 既然上游对这些「范围外」的档照常接受，静默改写就做了两件坏事：
//   · 用户明确调了 max，实际被降成 high —— 他看到的是「调了没效果」，
//     而日志里只有一行 downgrade，完全无从知道自己被改了参数；
//   · 改写本身基于一个错误前提，等于网关凭空替上游「猜」它能接受什么。
//
// 现在网关对 reasoning_effort 一律**原样透传**：
//   · 上游接受的档 → 正常生效（实测 deepseek-v4.1-flash 的 low→max 单调递增）
//   · 上游不接受的档 → 上游自己报 400，错误原样返回（那才是权威判据）
//
// 保留函数签名而不是删掉：调用点在 client.go 的 prepareBody 链路上，
// 删签名要动一串接口。空实现 + 本注释能让「为什么这里是空的」可查。
func normalizeReasoningEffort(obj map[string]any, efforts map[string][]string) {
	_ = obj
	_ = efforts
}

// normalizeRoles 把 messages 里的 developer 角色归一为 system。
//
// 背景：上游对 messages 的 role 字段做白名单校验，developer 不在白名单内，
// 命中即 HTTP 400 code=11128。developer 是 OpenAI 新规范里 system 的别名
// （Codex / Cursor 等新客户端用它承载 system 级指令），改写为 system 不丢语义。
//
// 此归一化是「协议兼容」（补上游 role 白名单），不是「内容脱敏」，
// 因此有意与 SanitizeFingerprints / sanitize 参数解耦：即使 sanitize=false 也照常归一。
//
// 只认 developer 这一个值：其余 role（system/user/assistant/tool/任意未知值）一律原样保留，
// 不合并、不重排、不删除任何消息（上游对多 system 的行为尚未实测，合并会引入新变量）。
func normalizeRoles(obj map[string]any) {
	msgs, ok := obj["messages"].([]any)
	if !ok {
		return
	}
	for i, m := range msgs {
		msg, ok := m.(map[string]any)
		if !ok {
			continue
		}
		role, ok := msg["role"].(string)
		if !ok {
			continue
		}
		if strings.EqualFold(strings.TrimSpace(role), "developer") {
			msg["role"] = "system"
			log.Printf("role normalized developer->system idx=%d", i)
		}
	}
}

// normalizeToolChoice 按上游 Go struct（string 类型）改写 OpenAI tool_choice。
//   - "none"            → 删 tool_choice + 删 tools/functions
//   - {"type":"none"}   → 同上
//   - {"type":"auto"/"required"} → 字符串 "auto"/"required"
//   - {"type":"function","function":{"name":"x"}} → 字符串 "x"
//   - 其他对象/非标量 → 删 tool_choice
func normalizeToolChoice(obj map[string]any) {
	suppress := func() {
		delete(obj, "tools")
		delete(obj, "functions")
	}
	tc, present := obj["tool_choice"]
	if !present {
		return
	}
	switch v := tc.(type) {
	case string:
		if strings.EqualFold(strings.TrimSpace(v), "none") {
			delete(obj, "tool_choice")
			suppress()
		}
	case map[string]any:
		typ, _ := v["type"].(string)
		typ = strings.ToLower(strings.TrimSpace(typ))
		switch typ {
		case "none":
			delete(obj, "tool_choice")
			suppress()
		case "auto", "required":
			obj["tool_choice"] = typ
		case "function":
			name := ""
			if fn, ok := v["function"].(map[string]any); ok {
				name, _ = fn["name"].(string)
			}
			if name == "" {
				name, _ = v["name"].(string)
			}
			if name = strings.TrimSpace(name); name != "" {
				obj["tool_choice"] = name
			} else {
				obj["tool_choice"] = "auto"
			}
		default:
			delete(obj, "tool_choice")
		}
	default:
		delete(obj, "tool_choice")
	}
}
