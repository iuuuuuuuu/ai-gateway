// logging.go 请求级表格日志：每个 /v1/chat/completions 请求结束后打印一行到 stdout。
package server

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"strings"
	"sync/atomic"
	"time"

	"workbuddy2api/internal/usage"
)

// chatSeq 进程级请求序号。
var chatSeq atomic.Int64

// chatLogEnabled 聊天表格日志总开关。生产恒 true；
// 测试包经 TestMain 置 false 关闭 stdout 噪音，需要断言行输出的测试用 withChatLog 临时开启（R5）。
var chatLogEnabled = true

// chatStat 单个 chat 请求的日志统计；handler 挂 defer，请求出口后落一行。
type chatStat struct {
	start time.Time
	// model **客户端原始请求体里的**模型名（可能带 `国服:` / `zcode:` 这类路由前缀）。
	//
	// 用途仅限**日志**：把用户实际写的那个名字记下来，排查时能一眼看出
	// "他写的是带前缀的形式"。
	model string
	// usageModel 用于 **Token 用量统计**的模型名：已剥掉路由前缀的**裸名**。
	//
	// # 为什么必须与 model 分开（2026-09-21 所有者报的缺陷）
	//
	// 所有者截图里同一份用量被拆成了三行：
	//
	//	deepseek-v4.1-flash · 国际版      732.9M
	//	zcode:GLM-5.3-Flash               1.9K
	//	国服:deepseek-v4.1-flash · 国服   12
	//
	// 第三行是**同一个模型**，只因用户写的是 `国服:deepseek-v4.1-flash`
	// 就被统计成了另一个模型名 —— 用量被割裂，倍率也就无从按模型汇总。
	//
	// 根因：统计直接用了 `parseModelFromBody(body)`（原始体），
	// 而前缀是给**网关的选号指令**，不属于模型标识（`resolveModel` 早就
	// 把它剥掉了，只是那之后没回填到统计上）。
	//
	// 空 = 未回填（如产品路径未走到 resolveModel），此时 `usageName()`
	// 回落到 model，行为与修复前一致。
	usageModel string
	mode       string // "stream" | "sync"
	uid        string // 完整 uid，展示时只取前 8 位
	ttfb       time.Duration
	toks       int // <0 表示 usage 缺失 → 显示 "-"
	status     int

	// counters/hasCounters 为网关 Token 用量统计的采集结果（与日志字段解耦：
	// 日志只关心 output，统计需要完整的输入/输出/缓存计量）。
	counters    usage.Counters
	hasCounters bool

	// billing 本次请求的**计费归属**（平台/区域/倍率），供 /usage 按归属拆开。
	//
	// 为什么放在 chatStat 而不是在 recordUsage 里现查：归属要在**选号完成时**
	// 才知道（选中的是哪个产品的哪个区域账号），而 recordUsage 是在流读完后
	// 才调用的 —— 那时选号上下文已经出栈。故在选号处填一次、这里带着走。
	billing usage.Billing

	logged bool
}

// usageName 返回**统计用**的模型名：优先裸名，未回填时回落原始名。
func (s *chatStat) usageName() string {
	if s.usageModel != "" {
		return s.usageModel
	}
	return s.model
}

// setModel 设置本次请求的模型名（原始写法），**同时**算好统计用的裸名。
//
// # 为什么要有这个方法而不是直接赋 `stat.model = req.Model`
//
// 三个协议入口都会在 `newChatStat` 之后用自己的解析结果覆盖 model
//（messages / responses 从各自的结构体取 `req.Model`，而不是从 JSON 体）。
// 直接赋值会让 `usageModel` 停在 `newChatStat` 时的值 —— 而那时若
// 请求体结构与入口期望的不一致（如 Anthropic 体走 chat 入口），
// `parseModelFromBody` 可能取不到 model，裸名就丢了。
//
// 收敛到一处赋值，保证 model 与 usageModel **永远同步更新**。
func (s *chatStat) setModel(raw string) {
	s.model = raw
	usage := raw
	if raw != "" && raw != "-" {
		if _, _, bare := resolveModel(raw); bare != "" {
			usage = bare
		}
	}
	s.usageModel = usage
}

// setBilling 记录本次请求的计费归属（由选号处调用）。
func (s *chatStat) setBilling(b usage.Billing) {
	s.billing = b
}

// setCounters 记录一次可用的完整计量。
func (s *chatStat) setCounters(c usage.Counters) {
	s.counters = c
	s.hasCounters = true
}

// setUsageMap 从上游 usage 对象提取统计计量；字段不可识别时忽略。
func (s *chatStat) setUsageMap(u map[string]any) {
	if c, ok := usage.ParseOpenAIUsage(u); ok {
		s.setCounters(c)
	}
}

// newChatStat 以请求进入 handler 的时刻为起点构造统计对象；toks 默认 -1（usage 缺失）。
//
// # 为什么在这里就剥掉路由前缀（2026-09-21 修正）
//
// `model` 保留客户端原始写法（日志要如实显示"他写的是 `国服:xxx`"），
// 而 `usageModel` 存**裸名**供 Token 用量统计。
//
// 为什么放在这里而不是各协议入口：三个入口（chat / messages / responses）
// 都调本函数，在此处剥一次就全都有了。若在入口各写一次，将来新增协议
// 必然漏 —— 而漏的表现是"用量又被拆成两个模型名"，从界面上看只是
// 多了一行，很难联想到是统计口径问题。
func newChatStat(now time.Time, body []byte, stream bool) *chatStat {
	mode := "sync"
	if stream {
		mode = "stream"
	}
	raw := parseModelFromBody(body)
	// `resolveModel` 返回 (product, realm, bare)。这里只取 bare：
	// 前缀是给**选号**用的指令，不属于模型标识。
	//
	// ⚠ 不能对 `-`（解析失败的占位）调它：那不是模型名，剥了也没意义。
	usage := raw
	if raw != "" && raw != "-" {
		if _, _, bare := resolveModel(raw); bare != "" {
			usage = bare
		}
	}
	return &chatStat{start: now, model: raw, usageModel: usage, mode: mode, toks: -1}
}

// done 幂等落一行表格日志。
func (s *chatStat) done() {
	if s.logged {
		return
	}
	s.logged = true
	logChatRow(s.ttfb, time.Since(s.start), s.model, s.mode, s.uid, s.status, s.toks)
}

// chatStatsReader 在流式透传时抓取 SSE 末帧的 usage（completion_tokens 用于日志，
// 完整计量用于 Token 用量统计），并记录首个 data 帧的 TTFB；原始字节原样返回给下游透传。
// 注意：不做 rune 估算，token 数一律采信上游 usage。
type chatStatsReader struct {
	br       *bufio.Reader
	start    time.Time
	ttfb     time.Duration
	seen     bool // 已见过首个 data 帧（TTFB 只记一次）
	hasUsage bool // 末帧是否带 usage
	tokens   int
	pend     []byte // 已读未返回的行缓存

	counters    usage.Counters
	hasCounters bool
}

// newChatStatsReaderSince 以 since 为 TTFB 计时起点（通常是请求进入 handler 的时刻）。
func newChatStatsReaderSince(r io.Reader, since time.Time) *chatStatsReader {
	return &chatStatsReader{br: bufio.NewReaderSize(r, 64*1024), start: since}
}

// TTFB 返回首个 data 帧到达耗时；无帧时为 0。
func (s *chatStatsReader) TTFB() time.Duration { return s.ttfb }

// Tokens 返回末帧 usage.completion_tokens 与是否缺失；无 usage 时 ok=false。
func (s *chatStatsReader) Tokens() (int, bool) { return s.tokens, s.hasUsage }

// Usage 返回末帧 usage 的完整计量（输入/输出/缓存）与是否可用。
func (s *chatStatsReader) Usage() (usage.Counters, bool) { return s.counters, s.hasCounters }

// parseSSELine 解析一行 "data: {...}"：首帧记 TTFB，含 usage 时采信精确计量。
func (s *chatStatsReader) parseSSELine(line string) {
	line = strings.TrimRight(line, "\r\n")
	if !strings.HasPrefix(line, "data: ") {
		return
	}
	payload := strings.TrimPrefix(line, "data: ")
	if payload == "[DONE]" {
		return
	}
	if !s.seen {
		s.seen = true
		s.ttfb = time.Since(s.start)
		// 保证不变式：见到 data 帧 ⇒ TTFB > 0。
		//
		// 必要性（实测）：Windows 时钟粒度约 511µs（30 万次 time.Now() 采样仅
		// 3 个不同值）。透传本地内存流时首帧可在同一次时钟滴答内到达，
		// time.Since 返回精确 0。而下游 logChatRow 以 ttfb > 0 为据打印耗时、
		// 否则打印 "-"，0 会被误报成「未收到任何数据帧」。
		// 这里补齐 1ns：不伪造真实耗时（仍是纳秒级真值），只消除哨兵值歧义。
		if s.ttfb <= 0 {
			s.ttfb = time.Nanosecond
		}
	}
	var chunk struct {
		Usage map[string]any `json:"usage"`
	}
	if json.Unmarshal([]byte(payload), &chunk) != nil || chunk.Usage == nil {
		return
	}
	s.hasUsage = true
	s.tokens = numOf(chunk.Usage["completion_tokens"])
	if c, ok := usage.ParseOpenAIUsage(chunk.Usage); ok {
		s.counters = c
		s.hasCounters = true
	}
}

// Read 返回原始数据，同时解析统计 TTFB/token。
func (s *chatStatsReader) Read(p []byte) (int, error) {
	if len(s.pend) > 0 {
		n := copy(p, s.pend)
		s.pend = s.pend[n:]
		return n, nil
	}
	line, err := s.br.ReadString('\n')
	if line != "" {
		s.parseSSELine(line)
		s.pend = []byte(line)
		n := copy(p, s.pend)
		s.pend = s.pend[n:]
		return n, nil
	}
	return 0, err
}

// parseModelFromBody 从请求 JSON 取 model 字段，缺省标 "-"。
func parseModelFromBody(body []byte) string {
	var obj struct {
		Model string `json:"model"`
	}
	if err := json.Unmarshal(body, &obj); err != nil || obj.Model == "" {
		return "-"
	}
	return obj.Model
}

// completionTokens 从 Aggregate 返回的响应中提取 usage.completion_tokens；缺失返回 -1。
func completionTokens(resp map[string]any) int {
	u, ok := resp["usage"].(map[string]any)
	if !ok {
		return -1
	}
	v, ok := u["completion_tokens"].(float64)
	if !ok {
		return -1
	}
	return int(v)
}

// uidPrefix 只显示 uid 前 8 位；空 uid 显示 "-"。
func uidPrefix(uid string) string {
	if uid == "" {
		return "-"
	}
	if len(uid) > 8 {
		return uid[:8]
	}
	return uid
}

// logChatRow 打印一行请求级表格日志（直接输出 stdout，无 log 时间戳前缀）。
// toks<0 表示 usage 缺失，显示 "-"。
func logChatRow(ttfb, total time.Duration, model, mode, uid string, status int, toks int) {
	if !chatLogEnabled {
		return
	}
	seq := chatSeq.Add(1)
	if len(model) > 11 {
		model = model[:11]
	}
	tokField := "-"
	tokpsField := "-"
	if toks >= 0 {
		tokField = fmt.Sprintf("%d", toks)
		if total > 0 {
			tokpsField = fmt.Sprintf("%.1f", float64(toks)/total.Seconds())
		} else {
			tokpsField = "0.0"
		}
	}
	ttfbMS := "-"
	if ttfb > 0 {
		ttfbMS = fmt.Sprintf("%dms", ttfb.Milliseconds())
	}
	fmt.Fprintf(os.Stdout, "| #%03d | %s | %s | %s | %d | uid=%s | TTFB=%s | tok=%s | %stok/s | total=%.1fs |\n",
		seq,
		time.Now().Format("15:04:05"),
		model,
		mode,
		status,
		uidPrefix(uid),
		ttfbMS,
		tokField,
		tokpsField,
		total.Seconds(),
	)
}
