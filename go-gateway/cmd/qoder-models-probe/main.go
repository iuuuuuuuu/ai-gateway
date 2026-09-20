package main

// qoder-models-probe 查 Qoder **真实**可用模型清单。
//
// # 为什么走生产代码（而不是手搓 HTTP）
//
// Qoder 的接口要 COSY 签名，而那是**有状态**的（见 cosy.go）。
// 手搓签名的细节极易做错，且做错时的报错（403 Signature invalid）
// 不提示是哪一步错。故直接调 `qoder.Client.FetchModels` —— 与网关同一份代码。
//
// # 这是**只读**调用
//
// 模型清单是官方客户端启动时就会调的接口，不产生对话、**不消耗积分**。
//
// 用法：
//
//	go run ./cmd/qoder-models-probe
import (
	"context"
	"fmt"
	"os"
	"sort"
	"strings"
	"time"

	"workbuddy2api/internal/qoder"
)

func main() {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	// 载入真实凭证（与网关同一个目录解析逻辑）
	dir := qoder.DefaultAuthDir()
	creds, failed, err := qoder.LoadDir(dir)
	if err != nil {
		fmt.Printf("读凭证目录失败（%s）：%v\n", dir, err)
		os.Exit(1)
	}
	for _, f := range failed {
		fmt.Printf("凭证解析失败，已跳过：%s\n", f)
	}
	if len(creds) == 0 {
		fmt.Printf("目录 %s 里没有可用凭证\n", dir)
		os.Exit(1)
	}

	fmt.Println(strings.Repeat("=", 92))
	fmt.Println("  Qoder 模型清单（只读接口，不消耗积分）")
	fmt.Println(strings.Repeat("=", 92))
	fmt.Printf("\n  凭证目录: %s\n  账号数  : %d\n", dir, len(creds))

	c := qoder.New()

	for _, cr := range creds {
		fmt.Printf("\n%s\n", strings.Repeat("─", 88))
		fmt.Printf("  账号 %s\n", cr.UID)
		fmt.Printf("    region=%v  DT 长度=%d  昵称=%s\n", cr.Region, len(cr.DT), cr.Nickname)
		fmt.Println(strings.Repeat("─", 88))

		models, err := c.FetchModels(ctx, cr)
		if err != nil {
			fmt.Printf("    ✗ 拉取失败: %v\n", err)
			continue
		}
		fmt.Printf("    共 %d 个模型\n\n", len(models))

		// 表头
		fmt.Printf("    %-38s %-8s %-6s %-5s %-5s %-12s %s\n",
			"Key", "展示名", "启用", "推理", "视觉", "倍率", "最大上下文")
		fmt.Println("    " + strings.Repeat("─", 100))

		var enabled, free []qoder.DynamicModel
		for _, m := range models {
			mark := ""
			if m.Enable {
				enabled = append(enabled, m)
			}
			if m.PriceFactor == 0 && m.Enable {
				free = append(free, m)
				mark = "  ← 免费"
			}
			nm := m.DisplayName
			if len([]rune(nm)) > 7 {
				nm = string([]rune(nm)[:7])
			}
			fmt.Printf("    %-38s %-8s %-6v %-5v %-5v %-12.4f %d%s\n",
				trim(m.Key, 38), nm, m.Enable, m.IsReasoning, m.IsVL,
				m.PriceFactor, m.MaxContextTokens(), mark)
		}

		fmt.Printf("\n    汇总：总计 %d / 启用 %d / **免费 %d**\n",
			len(models), len(enabled), len(free))
		if len(free) > 0 {
			fmt.Printf("\n    免费模型（倍率 0）：\n")
			for _, m := range free {
				fmt.Printf("      · %-40s %s\n", m.Key, m.DisplayName)
			}
		}

		// 与网关的映射表对照 —— 客户端能用的名字
		mapping := qoder.ResolveModelMap(models)
		fmt.Printf("\n    客户端可用的模型名（ResolveModelMap，%d 个）:\n", len(mapping))
		keys := make([]string, 0, len(mapping))
		for k := range mapping {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		for _, k := range keys {
			fmt.Printf("      %-40s → %s\n", k, mapping[k])
		}
	}

	fmt.Printf("\n%s\n", strings.Repeat("═", 92))
	fmt.Println("  说明")
	fmt.Println(strings.Repeat("═", 92))
	fmt.Print("" +
		"  · 这只读了模型清单 —— **没有发对话**，故不消耗积分\n" +
		"  · 「倍率」(price_factor) 为 0 即上游标记为免费\n" +
		"  · 客户端用的模型名 = ResolveModelMap 的键；带 `qoder:` 前缀可强制走 Qoder 通道\n")
}

func trim(s string, n int) string {
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	return string(r[:n-1]) + "…"
}
