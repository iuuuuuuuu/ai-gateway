// sse.go 处理上游 SSE 流：聚合成单个 OpenAI 响应，或透传给客户端。
package upstream

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Aggregate 读取完整 SSE 流，聚合 delta.content 为单个 OpenAI chat.completion 响应。
// 分片/半行由 bufio.Reader.ReadString 处理；遇到 "data: [DONE]" 结束。
// tool_calls 以流式 delta 到达（按 index 合并：首片带 id/type/name，后续只带 arguments 片段）。
func Aggregate(r io.Reader) (map[string]any, error) {
	br := bufio.NewReaderSize(r, 64*1024)
	var (
		id, model     string
		created       float64
		content       strings.Builder
		reasoning     strings.Builder
		role          = "assistant"
		finishReason  = "stop"
		usage         map[string]any
		gotAnyContent bool
		validEvents   int
		toolCalls     = map[int]map[string]any{}
		toolOrder     []int
	)
	for {
		line, err := br.ReadString('\n')
		if err != nil && err != io.EOF {
			return nil, err
		}
		line = strings.TrimRight(line, "\r\n")
		if strings.HasPrefix(line, "data: ") {
			payload := strings.TrimPrefix(line, "data: ")
			if payload == "[DONE]" {
				// 上游显式结束：停止读取，DONE 之后的任何数据一律忽略。
				break
			} else {
				var chunk map[string]any
				if json.Unmarshal([]byte(payload), &chunk) == nil {
					// 有效事件计数：仅 JSON 解析成功的数据帧计入（解析失败沿用静默 continue）。
					validEvents++
					if v, ok := chunk["id"].(string); ok && id == "" {
						id = v
					}
					if v, ok := chunk["model"].(string); ok && model == "" {
						model = v
					}
					if v, ok := chunk["created"].(float64); ok && created == 0 {
						created = v
					}
					if u, ok := chunk["usage"].(map[string]any); ok {
						usage = u
					}
					if ch, ok := chunk["choices"].([]any); ok {
						for _, ci := range ch {
							c, _ := ci.(map[string]any)
							if c == nil {
								continue
							}
							if fr, ok := c["finish_reason"].(string); ok && fr != "" {
								finishReason = fr
							}
							if delta, ok := c["delta"].(map[string]any); ok {
								if r2, ok := delta["role"].(string); ok && r2 != "" {
									role = r2
								}
								if txt, ok := delta["content"].(string); ok {
									content.WriteString(txt)
									// 只有**非空**正文才锁死 message 回退路径：
									// 首帧常带 "content":""（仅含 role 的保活/开场帧），
									// 若空串也置位，后续"完整消息放在 message 里"的上游
									// 形态就再也读不到内容，客户端只会收到空回复。
									if txt != "" {
										gotAnyContent = true
									}
								}
								if rc, ok := delta["reasoning_content"].(string); ok {
									reasoning.WriteString(rc)
								}
								if tcs, ok := delta["tool_calls"].([]any); ok {
									for _, tc := range tcs {
										call, ok := tc.(map[string]any)
										if !ok {
											continue
										}
										// index 必须是数字：上游偶发把它发成 JSON 字符串
										//（"0"/"1"）。只认 float64 会让所有调用落到槽位 0，
										// 多个并行工具调用被合并成一条（名字取最后一个、
										// 参数被拼接），模型拿到的工具调用直接损坏。
										idx := indexOfToolCall(call)
										merged, seen := toolCalls[idx]
										if !seen {
											merged = map[string]any{"index": idx}
											toolCalls[idx] = merged
											toolOrder = append(toolOrder, idx)
										}
										mergeToolCallDelta(merged, call)
									}
								}
							}
							// 有的上游把完整消息放在 message 里（非 delta）
							if msg, ok := c["message"].(map[string]any); ok && !gotAnyContent {
								if txt, ok := msg["content"].(string); ok {
									content.WriteString(txt)
								}
							}
						}
					}
				}
			}
		}
		if err == io.EOF {
			break
		}
	}
	if validEvents == 0 {
		// 上游返回 200 但没有任何有效数据事件（空流/只有 [DONE]/只有注释行）：
		// 不再合成空 content 的假成功响应，直接报错，由 handler 映射为 502 upstream_parse。
		return nil, fmt.Errorf("upstream stream contained no valid data events")
	}
	if id == "" {
		id = fmt.Sprintf("chatcmpl-%d", time.Now().UnixNano())
	}
	if created == 0 {
		created = float64(time.Now().Unix())
	}
	message := map[string]any{
		"role":    role,
		"content": content.String(),
	}
	if reasoning.Len() > 0 {
		message["reasoning_content"] = reasoning.String()
	}
	if len(toolOrder) > 0 {
		sortInts(toolOrder)
		calls := make([]map[string]any, 0, len(toolOrder))
		for _, idx := range toolOrder {
			calls = append(calls, toolCalls[idx])
		}
		message["tool_calls"] = calls
	}
	resp := map[string]any{
		"id":      id,
		"object":  "chat.completion",
		"created": int64(created),
		"model":   model,
		"choices": []any{
			map[string]any{
				"index":         0,
				"message":       message,
				"finish_reason": finishReason,
			},
		},
	}
	if usage != nil {
		resp["usage"] = usage
	}
	return resp, nil
}

// indexOfToolCall 取 tool_call 分片的 index。
//
// 兼容三种上游形态（实测均出现过）：
//   - 数字：{"index":0}          （标准）
//   - 字符串数字：{"index":"0"}  （部分上游把 int 序列化成字符串）
//   - 缺省：无 index 字段        （视为单调用，回落 0）
//
// 为什么必须兼容字符串：只认 float64 时字符串 index 会静默变成 0，
// 使多个并行工具调用全部合并进槽位 0 —— 名字被后者覆盖、参数被拼接，
// 客户端拿到一个损坏的工具调用（见回归测试）。
func indexOfToolCall(call map[string]any) int {
	switch v := call["index"].(type) {
	case float64:
		return int(v)
	case int:
		return v
	case int64:
		return int(v)
	case json.Number:
		if n, err := v.Int64(); err == nil {
			return int(n)
		}
	case string:
		if n, err := strconv.Atoi(strings.TrimSpace(v)); err == nil && n >= 0 {
			return n
		}
	}
	return 0
}

// mergeToolCallDelta 把流式 tool_call 片段合并到累计对象：
// id/type/function.name 直覆盖（后续分片通常缺省），function.arguments 拼接。
func mergeToolCallDelta(merged, delta map[string]any) {
	if v, ok := delta["id"].(string); ok && v != "" {
		merged["id"] = v
	}
	if v, ok := delta["type"].(string); ok && v != "" {
		merged["type"] = v
	}
	df, _ := delta["function"].(map[string]any)
	if df == nil {
		return
	}
	mf, _ := merged["function"].(map[string]any)
	if mf == nil {
		mf = map[string]any{}
		merged["function"] = mf
	}
	if v, ok := df["name"].(string); ok && v != "" {
		mf["name"] = v
	}
	if v, ok := df["arguments"].(string); ok && v != "" {
		if prev, _ := mf["arguments"].(string); prev != "" {
			mf["arguments"] = prev + v
		} else {
			mf["arguments"] = v
		}
	}
}

// sortInts 升序排序（避免引 sort 包只为三行）。
func sortInts(a []int) {
	for i := 0; i < len(a)-1; i++ {
		for j := i + 1; j < len(a); j++ {
			if a[j] < a[i] {
				a[i], a[j] = a[j], a[i]
			}
		}
	}
}

// normalizeFrame 以 OpenAI 流式规范白名单重建帧：仅保留标准字段，
// 剔除上游噪声（finish_reason:"" → null、空 content/refusal、空 tool_calls 列表、
// 空占位 function_call、顶层未知字段），空 delta 键一律省略，
// usage 缺失 → null，保证任意标准客户端按规范解析。
func normalizeFrame(obj map[string]any) map[string]any {
	out := map[string]any{}
	for _, k := range []string{"id", "object", "created", "model", "system_fingerprint", "service_tier"} {
		if v, ok := obj[k]; ok && v != nil {
			out[k] = v
		}
	}
	if _, ok := out["object"]; !ok {
		out["object"] = "chat.completion.chunk"
	}
	if _, ok := out["id"]; !ok {
		out["id"] = "chatcmpl-wb2api"
	}
	if chs, ok := obj["choices"].([]any); ok {
		nchs := make([]any, 0, len(chs))
		for _, ci := range chs {
			c, ok := ci.(map[string]any)
			if !ok {
				continue
			}
			nc := map[string]any{}
			if idx, ok := c["index"]; ok {
				nc["index"] = idx
			}
			delta := map[string]any{}
			if d, ok := c["delta"].(map[string]any); ok {
				if v, ok := d["role"].(string); ok && v != "" {
					delta["role"] = v
				}
				if v, ok := d["content"].(string); ok && v != "" {
					delta["content"] = v
				}
				if v, ok := d["reasoning_content"].(string); ok && v != "" {
					delta["reasoning_content"] = v
				}
				if v, ok := d["refusal"].(string); ok && v != "" {
					delta["refusal"] = v
				}
				if tcs, ok := d["tool_calls"].([]any); ok && len(tcs) > 0 {
					delta["tool_calls"] = tcs
				}
				if fc, ok := d["function_call"]; ok && fc != nil {
					// 空占位 function_call（name/arguments 全空）视为噪声剔除
					keep := false
					if fcm, ok2 := fc.(map[string]any); ok2 {
						n, _ := fcm["name"].(string)
						a, _ := fcm["arguments"].(string)
						keep = n != "" || a != ""
					} else {
						keep = true
					}
					if keep {
						delta["function_call"] = fc
					}
				}
			}
			nc["delta"] = delta
			if fr, ok := c["finish_reason"].(string); ok && fr != "" {
				nc["finish_reason"] = fr
			} else {
				nc["finish_reason"] = nil
			}
			nchs = append(nchs, nc)
		}
		out["choices"] = nchs
	}
	if u, ok := obj["usage"]; ok {
		out["usage"] = u
	} else {
		out["usage"] = nil
	}
	// error 帧必须原样保留：上游常在 HTTP 200 的流中途发
	// {"error":{...}}（如 code=11128 渠道未批准、账号被封）来表示失败。
	// 白名单重建会把它降级成一个普通的 "chat.completion.chunk"，客户端于是
	// 把「截断的回答 + 正常 [DONE]」当成一次成功，永远不知道请求失败了。
	// 保留 error 字段让客户端/上层能识别终止性错误。
	if e, ok := obj["error"]; ok && e != nil {
		out["error"] = e
	}
	return out
}

// probeReader 把"已预读的内容 + 原流"接回去，同时保留原流的 Close。
//
// # 为什么必须保留 Close
//
// 调用方拿到的是 `io.ReadCloser`（HTTP 响应体），**关掉它才会释放连接**。
// 若探针返回一个纯 `io.Reader`，调用方就无法关闭 —— 连接泄漏，
// 高并发下会把上游连接池耗光（表现为「用一会儿就连不上上游」）。
//
// 故这里组合而非替换：读走预读缓冲，关闭转交原流。
type probeReader struct {
	r     io.Reader
	close func() error
}

func (p *probeReader) Read(b []byte) (int, error) { return p.r.Read(b) }
func (p *probeReader) Close() error               { return p.close() }

// ProbeFirstFrame 预读上游 SSE 的**第一帧**，判断这条流是否值得交给客户端。
//
// # 为什么需要它（真实缺陷，2026-09-19 现场）
//
// 流式请求此前在 `status < 400` 时**直接交给调用方转写**，从不检查流里
// 有没有内容。而上游有一种失败形态是 **HTTP 200 + 空流**：
//
//	ZCode 对话通道对受限账号返回 200，但流里一帧有效数据都没有
//	（实测 code=3012 "unusual activity" 就是这个表现）
//
// 于是 `Stream` 走到 `validFrames == 0` 分支，回给用户
// `empty upstream stream` —— 而这条流**已经把状态码 200 写出去了**，
// 上层**再也无法改状态码、也无法换账号重试**。
//
// 更糟的是调用方在拿到这条流**之前**已调用 `Pool.NoteSuccess`：
// 坏账号被记成"成功"，**永远不会冷却**，于是每次请求都选中它。
//
// # 设计：把"是否可重试"的判断提前到写响应头之前
//
//	probe 成功 → 返回已读到的首帧 + 可继续读的流，调用方安全开始转写
//	probe 失败 → 调用方**还没写任何响应头**，可以标记账号冷却并换号重试
//
// ⚠ 只预读**一帧**（不是整个流）：那足够区分"空流"与"正常流"，
// 又不破坏流式体验（首帧延迟只增加一个上游 TTFB）。
//
// ⚠ 返回的 reader 必须被调用方**继续读完并关闭** —— 它已从 r 消费掉
// 一部分，丢掉就会**丢首帧**（表现为回答少开头几个字）。
func ProbeFirstFrame(r io.ReadCloser, timeout time.Duration) (io.ReadCloser, string, error) {
	type outcome struct {
		line string
		err  error
	}
	ch := make(chan outcome, 1)
	br := bufio.NewReaderSize(r, 64*1024)

	// consumed 累积读走的所有内容（**逐字节**保留，见下面 wrap 的说明）。
	//
	// ⚠ 必须加锁：超时返回时读协程**可能仍在运行**并继续往这里写，
	// 而主协程同时读它 —— 那是真实的数据竞争（`go test -race` 会报）。
	// 第一版用裸 `strings.Builder` 就有这个问题。
	var mu sync.Mutex
	var consumed strings.Builder

	snapshot := func() string {
		mu.Lock()
		defer mu.Unlock()
		return consumed.String()
	}

	go func() {
		for {
			line, rerr := br.ReadString('\n')
			if line != "" {
				mu.Lock()
				consumed.WriteString(line)
				mu.Unlock()
				t := strings.TrimRight(line, "\r\n")
				// 跳过注释/心跳/空行，找第一个真实 data: 帧
				if strings.HasPrefix(t, "data:") {
					ch <- outcome{line: line}
					return
				}
			}
			if rerr != nil {
				ch <- outcome{err: rerr}
				return
			}
		}
	}()

	// ⚠ 重建方式必须**逐字节等价**于原流。
	//
	// 我第一版用 `io.MultiReader(br, r)` —— 看起来对，实测却让下游
	// 多出一个前导 `\n`：`br.ReadString('\n')` 会把**行尾的 \n 一起读走**，
	// 于是拼接回来的流比原来**少一个 \n**；而 SSE 用空行分帧，
	// 少一个换行会让 `Stream` 把相邻帧粘连（实测表现为「工具调用参数
	// 不完整」「工具块数=0」这类看似无关的解析错误）。
	//
	// 修法：把读走的每一行**原样**（含行尾）拼回去，再接着读剩余缓冲。
	wrap := func() io.ReadCloser {
		return &probeReader{
			r:     io.MultiReader(strings.NewReader(snapshot()), br, r),
			close: r.Close,
		}
	}

	select {
	case o := <-ch:
		if o.err != nil {
			return wrap(), "", o.err
		}
		return wrap(), o.line, nil
	case <-time.After(timeout):
		// 首帧超时：把已缓冲的内容接回去，交由上层决定
		return wrap(), "", fmt.Errorf("上游首帧超时")
	}
}

// ValidSSEFrame 判断一个 SSE data 行是否是**有效数据帧**。
//
// 与 `Stream` 内部 `writeFrame` 的计数口径保持一致：JSON 能解析成对象
// 即算有效（`[DONE]` 不算 —— 它只表示结束，不代表有内容）。
//
// 为什么口径必须一致：`ProbeFirstFrame` 用它判断"这条流要不要重试"，
// 而 `Stream` 用它判断"要不要报空流"。两者不一致会出现
// 「probe 说有效、Stream 说空」的自相矛盾，用户看到的现象就是
// 明明探测通过却收到 empty upstream stream。
func ValidSSEFrame(line string) bool {
	t := strings.TrimSpace(line)
	if !strings.HasPrefix(t, "data:") {
		return false
	}
	d := strings.TrimSpace(t[len("data:"):])
	if d == "" || d == "[DONE]" {
		return false
	}
	var obj map[string]any
	return json.Unmarshal([]byte(d), &obj) == nil
}

// IsUpstreamErrorFrame 判断一帧是否是上游的错误帧（可据此判"该账号坏了"）。
//
// # 为什么要区分「错误帧」与「正常帧」
//
// 上游常用 `HTTP 200 + {"error":{...}}` 表达失败（如 ZCode 的
// code=3012/3007、WorkBuddy 的 11128 渠道未批准）。
// 这类帧说明**这个账号当前不可用**，应当冷却换号；
// 而正常的首帧（哪怕只有 role）说明账号是好的，必须原样透传。
//
// ⚠ 不能只看"有没有 error 字段"就判坏：某些上游在正常流里也会带
// 非致命 error 字段。故额外要求它**不含任何 choices 内容** ——
// 有内容就说明这轮对话是能用的。
func IsUpstreamErrorFrame(line string) (bool, string) {
	t := strings.TrimSpace(line)
	if !strings.HasPrefix(t, "data:") {
		return false, ""
	}
	d := strings.TrimSpace(t[len("data:"):])
	if d == "" || d == "[DONE]" {
		return false, ""
	}
	var obj map[string]any
	if json.Unmarshal([]byte(d), &obj) != nil {
		return false, ""
	}
	e, hasErr := obj["error"]
	if !hasErr || e == nil {
		return false, ""
	}
	// 有实际产出就不算失败
	if chs, ok := obj["choices"].([]any); ok && len(chs) > 0 {
		for _, c := range chs {
			if cm, ok := c.(map[string]any); ok {
				if delta, ok := cm["delta"].(map[string]any); ok {
					if s, _ := delta["content"].(string); s != "" {
						return false, ""
					}
				}
				if fr, _ := cm["finish_reason"].(string); fr != "" {
					return false, ""
				}
			}
		}
	}
	// 提取可读原因（上游形状不一，尽量挖）
	msg := ""
	switch v := e.(type) {
	case string:
		msg = v
	case map[string]any:
		if s, ok := v["message"].(string); ok {
			msg = s
		}
		if msg == "" {
			if c, ok := v["code"]; ok {
				msg = fmt.Sprintf("code=%v", c)
			}
		}
	}
	return true, msg
}

// Stream 透传上游 SSE 到 w（逐帧规范化后 flush），保证至少写一个 [DONE]。
// 调用方必须先设置过 status 200；本函数自设 SSE headers。
// 流式策略：逐帧透传（规范化已剥空 content 噪声），恢复与上游一致的平滑流式。
func Stream(w http.ResponseWriter, r io.Reader) error {
	h := w.Header()
	h.Set("Content-Type", "text/event-stream")
	h.Set("Cache-Control", "no-cache")
	h.Set("Connection", "keep-alive")
	h.Set("X-Accel-Buffering", "no")
	fl, _ := w.(http.Flusher)

	// writeFrame 把 payload 按规范白名单重建后以 data: 帧写出并 flush。
	// 仅 JSON 解析成功时计数记为一次有效转发（JSON 解析失败照常降级原样写出，但不计数）。
	writeFrame := func(payload string) (int, error) {
		var obj map[string]any
		valid := 0
		if json.Unmarshal([]byte(payload), &obj) == nil {
			if raw, err := json.Marshal(normalizeFrame(obj)); err == nil {
				payload = string(raw)
			}
			valid = 1
		}
		if _, werr := io.WriteString(w, "data: "+payload+"\n\n"); werr != nil {
			return 0, werr
		}
		if fl != nil {
			fl.Flush()
		}
		return valid, nil
	}

	// writeRaw 原样写出一帧（绕过 normalizeFrame）并 flush。空流错误帧需保留 error 字段，
	// 不能被白名单剥掉，故不经 writeFrame 规范化。
	writeRaw := func(payload string) error {
		if _, werr := io.WriteString(w, "data: "+payload+"\n\n"); werr != nil {
			return werr
		}
		if fl != nil {
			fl.Flush()
		}
		return nil
	}

	br := bufio.NewReaderSize(r, 64*1024)
	validFrames := 0
readLoop:
	for {
		line, err := br.ReadString('\n')
		trimmed := strings.TrimRight(line, "\r\n")
		switch {
		case strings.HasPrefix(trimmed, "data: [DONE]"):
			// 上游显式结束：停止读取，DONE 之后的任何数据（含垃圾帧）一律不再透传。
			// [DONE] 统一在循环结束后写出，保证恰好一个。
			break readLoop
		case strings.HasPrefix(trimmed, "data: "):
			n, werr := writeFrame(strings.TrimPrefix(trimmed, "data: "))
			validFrames += n
			if werr != nil {
				return werr
			}
		case trimmed != "":
			// 注释/其他行：原样透传
			if _, werr := io.WriteString(w, line); werr != nil {
				return werr
			}
			if fl != nil {
				fl.Flush()
			}
		}
		// 空行（帧分隔）吞掉：本函数自产 "\n\n"
		if err != nil {
			if err == io.EOF {
				break
			}
			return err
		}
	}
	// 空流（0 有效帧）：先写一帧 error（绕过 normalizeFrame 原样保留 error 字段），
	// 再补 [DONE] 保证客户端能正常收尾，并返回非 nil error 供调用方记录。
	if validFrames == 0 {
		_ = writeRaw(`{"error":{"message":"empty upstream stream","type":"upstream_error"}}`)
	}
	// 保证恰好写一个 [DONE]（上游漏发时兜底补上）。
	if _, err := io.WriteString(w, "data: [DONE]\n\n"); err != nil {
		return err
	}
	if fl != nil {
		fl.Flush()
	}
	if validFrames == 0 {
		return fmt.Errorf("upstream stream contained no valid data events")
	}
	return nil
}
