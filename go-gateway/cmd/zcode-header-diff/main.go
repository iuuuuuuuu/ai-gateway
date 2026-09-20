// 用生产代码发一个请求，把**我们实际发出的头**逐字打印出来，
// 与官方抓包的头做**逐条差异对照**。
//
// # 为什么要这个
//
// 我已经确认：
//   · captcha 接线正确（3007 → 3012，说明 param 被接受了）
//   · 追踪头稳定性已修（自检通过）
//   · 但仍是 3012，而官方同一分钟能成功
//
// 那差异必然在**某个具体的头/字段**上。逐条对照是最直接的找法 ——
// 比继续猜"可能是风控"有意义。
//
// # 做法
//
// 用 httptest 起一个本地"假上游"，让生产代码打它，
// 然后在假上游里 dump 出**全部请求头**。这样看到的头就是
// 生产代码真实发出的，一个不漏。
package main

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"workbuddy2api/internal/zcode"
)

// 官方客户端抓包实测的头（逐字，来自 Reqable）
var official = map[string]string{
	"anthropic-beta":                    "mid-conversation-system-2026-04-07",
	"anthropic-version":                 "2023-06-01",
	"authorization":                     "Bearer <jwt>",
	"content-type":                      "application/json",
	"http-referer":                      "https://zcode.z.ai",
	"user-agent":                        "ZCode/3.14.0 ai-sdk/provider-utils/4.0.27 runtime/node.js/24",
	"x-aliyun-captcha-verify-param":     "<base64>",
	"x-aliyun-captcha-verify-region":    "cn",
	"x-api-key":                         "<jwt>",
	"x-client-language":                 "zh-CN",
	"x-client-timezone":                 "Asia/Shanghai",
	"x-os-category":                     "windows",
	"x-os-version":                      "10.0.19045",
	"x-platform":                        "win32-x64",
	"x-query-id":                        "<uuid>",
	"x-release-channel":                 "production",
	"x-request-id":                      "<uuid>",
	"x-session-id":                      "<uuid>",
	"x-title":                           "Z Code@electron",
	"x-zcode-agent":                     "glm",
	"x-zcode-app-version":               "3.14.0",
	"x-zcode-session-type":              "main",
	"x-zcode-trace-id":                  "<uuid>",
	"accept":                            "text/event-stream",
}

func main() {
	// ---- 载入真实凭证 ----
	creds, _, err := zcode.LoadDir(zcode.DefaultAuthDir())
	if err != nil || len(creds) == 0 {
		fmt.Printf("载入凭证失败: %v\n", err)
		os.Exit(1)
	}
	var cred *zcode.Cred
	for _, c := range creds {
		if c.UID == "zcode-1b2941c020ef" {
			cred = c
		}
	}
	if cred == nil {
		fmt.Println("找不到目标账号")
		os.Exit(1)
	}

	// ---- 假上游：dump 请求头 ----
	var got http.Header
	var gotPath string
	var gotBody string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got = r.Header.Clone()
		gotPath = r.URL.Path
		b, _ := io.ReadAll(r.Body)
		gotBody = string(b)
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(200)
		io.WriteString(w, "event: message_start\ndata: {\"type\":\"message_start\"}\n\n")
		io.WriteString(w, "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
	}))
	defer srv.Close()

	c := zcode.New()
	c.SetAnthropicBaseForTest(srv.URL + "/api/anthropic")
	// 不求解 captcha（本地假上游不校验），这样也顺便验证"不带 param"的形状
	solver := zcode.SharedCaptchaSolver()
	if solver != nil {
		solver.SetDir("")
	}

	body := `{"model":"GLM-5.3-Flash","max_tokens":32,"stream":true,` +
		`"messages":[{"role":"user","content":"hi"}]}`
	rc, _, _, err := c.StreamChat(context.Background(), cred, []byte(body))
	if err != nil {
		fmt.Printf("请求失败: %v\n", err)
		os.Exit(1)
	}
	if rc != nil {
		io.Copy(io.Discard, rc)
		rc.Close()
	}

	fmt.Println(strings.Repeat("=", 92))
	fmt.Println("  我们实际发出的请求头 vs 官方抓包（逐条对照）")
	fmt.Println(strings.Repeat("=", 92))
	fmt.Printf("\n  路径: %s\n", gotPath)

	// ---- 逐条对照 ----
	ours := map[string]string{}
	for k, v := range got {
		lk := strings.ToLower(k)
		if lk == "host" || lk == "content-length" || lk == "accept-encoding" {
			continue
		}
		ours[lk] = strings.Join(v, ", ")
	}

	allKeys := map[string]bool{}
	for k := range official {
		allKeys[k] = true
	}
	for k := range ours {
		allKeys[k] = true
	}
	keys := make([]string, 0, len(allKeys))
	for k := range allKeys {
		keys = append(keys, k)
	}
	sort.Strings(keys)

	var missing, extra []string
	fmt.Printf("\n  %-34s %-8s %s\n", "头", "状态", "值")
	fmt.Println("  " + strings.Repeat("─", 88))
	for _, k := range keys {
		o, hasO := official[k]
		u, hasU := ours[k]
		status := ""
		switch {
		case hasO && hasU:
			status = "✅"
		case hasO && !hasU:
			status = "❌缺"
			missing = append(missing, k)
		case !hasO && hasU:
			status = "➕多"
			extra = append(extra, k)
		}
		show := u
		if !hasU {
			show = "(未发)"
		}
		if len(show) > 62 {
			show = show[:62] + "…"
		}
		// 官方值太长时也给个提示
		if hasO && len(o) > 62 {
			o = o[:62] + "…"
		}
		fmt.Printf("  %s %-32s %s\n", status, k, show)
	}

	fmt.Printf("\n%s\n", strings.Repeat("═", 92))
	fmt.Println("  差异汇总")
	fmt.Println(strings.Repeat("═", 92))
	fmt.Printf("\n  ❌ 我们**缺** %d 个头:\n", len(missing))
	for _, k := range missing {
		fmt.Printf("       %-34s 官方值 = %s\n", k, official[k])
	}
	fmt.Printf("\n  ➕ 我们**多发** %d 个头:\n", len(extra))
	for _, k := range extra {
		fmt.Printf("       %-34s 我们值 = %.70s\n", k, ours[k])
	}

	// ---- 请求体顶部字段对照 ----
	fmt.Printf("\n  请求体顶层字段（我们发的）:\n")
	for _, f := range []string{"model", "max_tokens", "stream", "thinking", "output_config", "metadata", "system", "tools", "tool_choice"} {
		if strings.Contains(gotBody, `"`+f+`"`) {
			fmt.Printf("       ✅ %s\n", f)
		} else {
			fmt.Printf("       ❌ %s（我们没发）\n", f)
		}
	}
	fmt.Printf("\n  官方抓包的顶层字段: model, max_tokens, metadata, system, messages, tools, tool_choice, stream, thinking, output_config\n")
	fmt.Printf("  ⚠ **metadata** 与 **cache_control** 是官方的特征字段，检查我们是否漏了。\n")

	// 写一份对照结果供复核
	_ = os.MkdirAll(filepath.Join("..", "..", "uitest"), 0o755)
}
