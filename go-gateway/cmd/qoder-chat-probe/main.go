package main

// qoder-chat-probe 用**免费模型**跑一次真实对话，验证整条链路。
//
// # 为什么选 qwen3.8-flash
//
// `cmd/qoder-models-probe` 实测（只读接口）：
//
//	qfmodel  Qwen3.8-Flash   price_factor = **0.0000**   ← 14 个模型里唯一免费的
//
// 故用它做端到端验证**不消耗积分** —— 这是"能反复验证"的前提。
//
// # 走生产代码
//
// 调 `qoder.Dispatch.ChatStream`（与网关同一份），故验证的是**真实路径**：
// 请求翻译 → COSY 签名 → 上游 → 响应流翻译回 OpenAI。
//
// 用法：
//
//	go run ./cmd/qoder-chat-probe -model qwen3.8-flash -stream
import (
	"context"
	"flag"
	"fmt"
	"io"
	"os"
	"strings"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/qoder"
)

func main() {
	model := flag.String("model", "qwen3.8-flash", "客户端模型名（默认用免费的那个）")
	stream := flag.Bool("stream", false, "是否流式")
	prompt := flag.String("prompt", "只回答两个字：收到", "用户消息")
	tools := flag.Bool("tools", false, "是否带一个工具定义（验证工具调用翻译）")
	maxTok := flag.Int("max-tokens", 64, "max_tokens")
	flag.Parse()

	dir := qoder.DefaultAuthDir()
	creds, _, err := qoder.LoadDir(dir)
	if err != nil || len(creds) == 0 {
		fmt.Printf("载入凭证失败: %v（目录 %s）\n", err, dir)
		os.Exit(1)
	}

	fmt.Println(strings.Repeat("=", 92))
	fmt.Println("  Qoder 真实对话验证（用免费模型，不消耗积分）")
	fmt.Println(strings.Repeat("=", 92))
	fmt.Printf("\n  模型  : %s\n  流式  : %v\n  带工具: %v\n  凭证  : %s\n",
		*model, *stream, *tools, dir)

	// 构造 OpenAI 请求体（网关收到的那种）
	toolsJSON := ""
	if *tools {
		toolsJSON = `,"tools":[{"type":"function","function":{` +
			`"name":"get_weather","description":"查询指定城市的天气",` +
			`"parameters":{"type":"object","properties":{"city":{"type":"string","description":"城市名"}},"required":["city"]}}}]`
	}
	body := fmt.Sprintf(
		`{"model":%q,"max_tokens":%d,"stream":%v,"messages":[{"role":"user","content":%q}]%s}`,
		*model, *maxTok, *stream, *prompt, toolsJSON)

	d := qoder.NewDispatch(qoder.New())

	// 用第一个账号
	cr := creds[0]
	// Dispatch.ChatStream 需要 *auth.Auth，故从 Cred 造一个
	//
	// ⚠ auth.Auth 没有 Region 字段（区域在 Cred 上，由 qoder 包自己处理）。
	// 这里只填 Dispatch 实际会读的字段：UID（定位凭证）+ Product（选上游实现）。
	a := &auth.Auth{
		UID:          cr.UID,
		Product:      auth.ProductQoder,
		AccessToken:  cr.DT,
		RefreshToken: cr.DRT,
		ExpiresAt:    cr.DTExpiresAt,
		Nickname:     cr.Nickname,
	}

	fmt.Printf("\n%s\n", strings.Repeat("─", 88))
	fmt.Println("  发起请求")
	fmt.Println(strings.Repeat("─", 88))

	ctx, cancel := context.WithTimeout(context.Background(), 180*time.Second)
	defer cancel()

	start := time.Now()
	rc, status, respBody, err := d.ChatStream(ctx, a, []byte(body))
	elapsed := time.Since(start)

	fmt.Printf("\n  耗时 : %s\n  HTTP : %d\n", elapsed.Round(time.Millisecond), status)

	if err != nil {
		fmt.Printf("\n  ✗ 传输错误: %v\n", err)
		os.Exit(1)
	}
	if respBody != nil {
		fmt.Printf("\n  ── 原始响应体（%d 字节）──\n%s\n", len(respBody), truncate(string(respBody), 1500))
		os.Exit(1)
	}
	if rc == nil {
		fmt.Println("\n  ✗ rc 与 respBody 都是空 —— 状态异常")
		os.Exit(1)
	}
	defer rc.Close()

	raw, rerr := io.ReadAll(io.LimitReader(rc, 256*1024))
	if rerr != nil {
		fmt.Printf("  读流失败: %v\n", rerr)
	}
	s := string(raw)

	// ---- 判读 ----
	fmt.Printf("\n  ── 响应（前 25 帧）──\n")
	n := 0
	var text strings.Builder
	var reasoning strings.Builder
	var toolNames []string
	var toolArgs strings.Builder
	finish := ""
	isAnthropicLeak := false
	for _, line := range strings.Split(s, "\n") {
		t := strings.TrimSpace(line)
		if t == "" {
			continue
		}
		n++
		if n <= 25 {
			fmt.Printf("  %s\n", truncate(t, 175))
		}
		// 收集关键内容
		if strings.Contains(t, `"content":"`) || strings.Contains(t, `"content": "`) {
			if idx := strings.Index(t, `"content"`); idx >= 0 {
				rest := t[idx:]
				if q := strings.Index(rest[10:], `"`); q >= 0 {
					rest2 := rest[10+q+1:]
					if e := strings.Index(rest2, `"`); e > 0 {
						text.WriteString(rest2[:e])
					}
				}
			}
		}
		if strings.Contains(t, "reasoning_content") {
			reasoning.WriteString("(有)")
		}
		if strings.Contains(t, `"name":"`) {
			if i := strings.Index(t, `"name":"`); i >= 0 {
				r := t[i+8:]
				if e := strings.Index(r, `"`); e > 0 {
					toolNames = append(toolNames, r[:e])
				}
			}
		}
		if strings.Contains(t, `"arguments":"`) {
			if i := strings.Index(t, `"arguments":"`); i >= 0 {
				r := t[i+13:]
				if e := strings.LastIndex(r, `"`); e > 0 {
					toolArgs.WriteString(r[:e])
				}
			}
		}
		if strings.Contains(t, `"finish_reason":"`) {
			if i := strings.Index(t, `"finish_reason":"`); i >= 0 {
				r := t[i+17:]
				if e := strings.Index(r, `"`); e >= 0 {
					finish = r[:e]
				}
			}
		}
		// Anthropic 事件名泄漏检测（Qoder 是嵌套 SSE，不该出现这些）
		for _, leak := range []string{"content_block_delta", "message_start", "message_stop"} {
			if strings.Contains(t, leak) {
				isAnthropicLeak = true
			}
		}
	}

	fmt.Printf("\n%s\n", strings.Repeat("═", 92))
	fmt.Println("  判读")
	fmt.Println(strings.Repeat("═", 92))
	if n == 0 {
		fmt.Println("  ✗ 没有任何帧 —— 上游返回空流")
		os.Exit(1)
	}
	if !strings.Contains(s, "data: [DONE]") && !*stream {
		// 非流式的聚合产物不含 [DONE]，这是正常的
	}
	fmt.Printf("  帧数          : %d\n", n)
	fmt.Printf("  含 [DONE]     : %v\n", strings.Contains(s, "data: [DONE]"))
	fmt.Printf("  文本长度      : %d\n", text.Len())
	if text.Len() > 0 {
		fmt.Printf("  文本内容      : %s\n", truncate(text.String(), 200))
	}
	fmt.Printf("  有 reasoning  : %v\n", reasoning.Len() > 0)
	if len(toolNames) > 0 {
		fmt.Printf("  工具名        : %v\n", toolNames)
		fmt.Printf("  工具参数      : %s\n", truncate(toolArgs.String(), 200))
	}
	fmt.Printf("  finish_reason : %s\n", finish)
	fmt.Printf("  Anthropic 泄漏: %v（应为 false —— Qoder 是嵌套 SSE）\n", isAnthropicLeak)

	fmt.Printf("\n  模型倍率提示：`qwen3.8-flash` → `qfmodel` **倍率 0 = 免费**\n")
}

func truncate(s string, n int) string {
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	return string(r[:n]) + "…"
}
