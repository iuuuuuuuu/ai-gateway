package main

// zcode_live_probe 用**真实的生产代码路径**打一次 ZCode 上游，打印全部细节。
//
// # 为什么要有这个（而不是继续用 HTTP + 抓包）
//
// 前几轮我一直在"客户端 → 网关 → 抓包"这条链路上绕，链条上每一层都可能
// 改写信息（网关把错误归类、代理可能不生效、Reqable 可能丢帧）。
// 这个探针直接调 `zcode.Client.StreamChat` —— 与网关内部**同一份代码**，
// 中间没有任何 HTTP 层，所以看到的必然是真因。
//
// # 用法
//
//	go run ./cmd/zcode-live-probe -uid zcode-1b2941c020ef
//
// 只发 **1 个**请求。不做循环、不重试 —— 上游风控是有记忆的。
import (
	"context"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"

	"workbuddy2api/internal/zcode"
)

func main() {
	uid := flag.String("uid", "", "凭证 uid（如 zcode-1b2941c020ef）")
	model := flag.String("model", "GLM-5.3-Flash", "模型名")
	stream := flag.Bool("stream", true, "是否流式")
	prompt := flag.String("prompt", "只回答两个字：收到", "用户消息")
	noCaptcha := flag.Bool("no-captcha", false, "跳过验证码求解（只看 3007）")
	// deviceMid 覆盖 —— 用于验证"设备指纹是否影响风控"。
	//
	// # 为什么需要它
	//
	// 我们的凭证里**没有 `device_mid`** 字段（`cred.go` 见缺失就随机补一个），
	// 于是每次启动都是一个**新的设备指纹**。而官方客户端的 deviceMid 是
	// **固定**的（持久化在 `~/.zcode/v2/telemetry-state.json`）。
	//
	// 参考实现明确警告过这条：
	//   「device fingerprint 稳定：X-Device-Mid 生成一次、永久复用、落盘、
	//     **绝不每请求随机**」
	//
	// 这个开关让探针能**指定**一个 deviceMid（如官方那个），
	// 从而做 A/B —— 而它**不改任何文件**，纯运行时覆盖。
	deviceMid := flag.String("device-mid", "", "覆盖 X-Device-Mid（空 = 用凭证里的随机值）")
	flag.Parse()

	if *uid == "" {
		fmt.Println("必须指定 -uid")
		os.Exit(2)
	}

	// ---- 载入真实凭证 ----
	dir := zcode.DefaultAuthDir()
	creds, failed, err := zcode.LoadDir(dir)
	if err != nil {
		fmt.Printf("读凭证目录失败（%s）：%v\n", dir, err)
		os.Exit(1)
	}
	for _, f := range failed {
		fmt.Printf("凭证解析失败，已跳过：%s\n", f)
	}
	var cred *zcode.Cred
	for _, c := range creds {
		if c.UID == *uid {
			cred = c
		}
	}
	if cred == nil {
		fmt.Printf("找不到 uid=%s（目录 %s 里有 %d 条）\n", *uid, dir, len(creds))
		for _, c := range creds {
			fmt.Printf("  · %s  provider=%s\n", c.UID, c.Provider)
		}
		os.Exit(1)
	}

	fmt.Println(strings.Repeat("=", 88))
	fmt.Println("  ZCode 真机探针（走生产代码路径 zcode.Client.StreamChat）")
	fmt.Println(strings.Repeat("=", 88))
	// deviceMid 覆盖（见 -device-mid 的说明）。纯内存，不写文件。
	if *deviceMid != "" {
		cred.DeviceMid = *deviceMid
	}
	fmt.Printf("\n  凭证目录 : %s\n", dir)
	fmt.Printf("  账号     : %s  provider=%s\n", cred.UID, cred.Provider)
	fmt.Printf("  jwt      : %d 字符  iat=%d\n", len(cred.JWT), cred.JWTIssuedAt)
	fmt.Printf("  deviceMid: %s%s\n", cred.DeviceMid,
		map[bool]string{true: "  （-device-mid 覆盖）", false: "  （凭证缺 device_mid ⇒ 随机）"}[*deviceMid != ""])
	fmt.Printf("  模型     : %s   流式=%v\n", *model, *stream)

	// ---- 验证码求解器（与网关同一份） ----
	solver := zcode.SharedCaptchaSolver()
	if solver != nil {
		// 让求解器能找到组件（与 main.go 的探测顺序一致）
		for _, cand := range []string{
			filepath.Join("assets", "zcode-captcha"),
			filepath.Join("..", "assets", "zcode-captcha"),
			filepath.Join("..", "..", "wt-port", "assets", "zcode-captcha"),
			`D:\WishProject\WorkbuddySwitchAPi\wt-port\assets\zcode-captcha`,
		} {
			if st, serr := os.Stat(filepath.Join(cand, "solver.bundle.cjs")); serr == nil && !st.IsDir() {
				solver.SetDir(cand)
				break
			}
		}
		fmt.Printf("  求解器   : dir=%q\n", solver.Dir())
		if r := solver.UnavailableReason(); r != "" {
			fmt.Printf("             ⚠ 不可用：%s\n", r)
		} else {
			fmt.Printf("             ✓ 可用\n")
		}
	} else {
		fmt.Println("  求解器   : ⊘ 未初始化")
	}

	// ---- 构造 OpenAI 请求体（网关收到的那种） ----
	body := fmt.Sprintf(
		`{"model":%q,"max_tokens":32,"stream":%v,"messages":[{"role":"user","content":%q}]}`,
		*model, *stream, *prompt)

	client := zcode.New()

	fmt.Printf("\n%s\n", strings.Repeat("─", 86))
	fmt.Println("  发起请求")
	fmt.Println(strings.Repeat("─", 86))

	// ---- 先验证追踪头的**稳定性**（这是本轮的关键修复） ----
	{
		fmt.Printf("\n%s\n", strings.Repeat("─", 86))
		fmt.Println("  追踪头稳定性自检（同一账号连取两次，看哪些该变、哪些该定）")
		fmt.Println(strings.Repeat("─", 86))
		id := client.Identity
		id.AccountID = firstNonEmpty(cred.AccountID, cred.UID)
		h1 := id.TraceHeaders()
		h2 := id.TraceHeaders()
		for _, k := range []string{"x-request-id", "x-query-id", "x-session-id", "x-zcode-trace-id"} {
			a, b := h1[k], h2[k]
			same := a == b
			want := "应稳定"
			if k == "x-request-id" || k == "x-query-id" {
				want = "应变化"
			}
			mark := "✓"
			if (want == "应稳定" && !same) || (want == "应变化" && same) {
				mark = "✗"
			}
			fmt.Printf("  %s %-18s %-6s  第1次=%.8s…  第2次=%.8s…\n", mark, k, want, a, b)
		}
	}

	ctx, cancel := context.WithTimeout(context.Background(), 150*time.Second)
	defer cancel()

	start := time.Now()

	// 若 -no-captcha，用一个不含 jwt 的副本走 coding-plan 分支？
	// 不 —— 那会换端点。正确做法：把 CaptchaParam 显式留空，
	// 但生产代码会自己去解。故这里用 solver 临时"关掉"的方式不可行，
	// 改为直接调 StreamChat 并在失败时打印全部信息。
	//
	// 需要"只看到 3007"时用 -no-captcha：它绕过 StreamChat，
	// 直接调未导出的 streamChatOnce 不可行（跨包），
	// 故用"求解器不可用"来模拟（把 dir 清空）。
	if *noCaptcha && solver != nil {
		solver.SetDir("")
		fmt.Println("  ⚠ -no-captcha：已把求解器目录清空（模拟未安装），预期回 3007")
	}

	rc, status, respBody, err := client.StreamChat(ctx, cred, []byte(body))
	elapsed := time.Since(start)

	fmt.Printf("\n  耗时     : %s\n", elapsed.Round(time.Millisecond))
	if err != nil {
		fmt.Printf("  传输错误 : %v\n", err)
		fmt.Println("\n  ⇒ 请求根本没发出去（DNS/连接/代理层）。")
		os.Exit(1)
	}
	fmt.Printf("  HTTP     : %d\n", status)

	if respBody != nil {
		fmt.Printf("\n  ── 错误响应体（原文，%d 字节）──\n", len(respBody))
		s := string(respBody)
		if len(s) > 2000 {
			s = s[:2000] + "…"
		}
		fmt.Printf("  %s\n", s)

		// 用生产代码的分类函数解读
		kind := zcode.Classify(status, string(respBody))
		fmt.Printf("\n  ── 生产代码的分类（Classify）──\n")
		fmt.Printf("  kind = %q\n", kind)

		fmt.Printf("\n%s\n", strings.Repeat("═", 86))
		fmt.Println("  判读")
		fmt.Println(strings.Repeat("═", 86))
		switch {
		case strings.Contains(s, "3007"):
			fmt.Println("  → **3007 captcha verify failed**：验证码没被上游接受。")
			fmt.Println("     若求解器显示可用，则说明 param 被拒（非格式问题，已实测同构）。")
		case strings.Contains(s, "3012"):
			fmt.Println("  → **3012 unusual activity**：风控。")
		case strings.Contains(s, "429") || status == 429:
			fmt.Println("  → **429**：看 msg 区分「限流」（等）与「余额不足」（等也没用）。")
			fmt.Println("     注意：额度查询返回 200 ⇒ 不是账号级问题。")
		case status == 200:
			fmt.Println("  → HTTP 200 但收到错误体？少见，需细看。")
		default:
			fmt.Printf("  → 见上文原文。\n")
		}
		os.Exit(0)
	}

	if rc == nil {
		fmt.Println("\n  rc == nil 且无错误体 —— 状态异常。")
		os.Exit(1)
	}
	defer rc.Close()

	fmt.Printf("\n  ── 响应流（前 40 帧）──\n")
	raw, rerr := io.ReadAll(io.LimitReader(rc, 64*1024))
	if rerr != nil {
		fmt.Printf("  读流失败: %v\n", rerr)
	}
	lines := strings.Split(string(raw), "\n")
	n := 0
	for _, l := range lines {
		if strings.TrimSpace(l) == "" {
			continue
		}
		n++
		if n > 40 {
			fmt.Println("  …（截断）")
			break
		}
		show := l
		if len(show) > 190 {
			show = show[:190] + "…"
		}
		fmt.Printf("  %s\n", show)
	}

	fmt.Printf("\n%s\n", strings.Repeat("═", 86))
	fmt.Println("  ✓ 成功：HTTP 200 且收到 SSE 流")
	fmt.Println(strings.Repeat("═", 86))
}

// firstNonEmpty 取第一个非空值。
func firstNonEmpty(a, b string) string {
	if strings.TrimSpace(a) != "" {
		return a
	}
	return b
}