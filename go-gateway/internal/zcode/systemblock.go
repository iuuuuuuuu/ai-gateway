package zcode

// systemblock.go 构造**官方客户端精确形状的 system 块**。
//
// # ⚠ 实测结论（2026-09-19）：**它不是 3012 的答案**，本模块默认不启用
//
// ## 我们做过的 A/B 实验
//
// 参考实现 `TriDefender/zcode-api` 的注释提出一个假设：服务端做**内容检查**，
// 在 `system` 里看不到 ZCode 身份块就判"非官方客户端"。这本来能解释
// `3012 unusual activity` 的语义，也是当时唯一有机制解释的方向。
//
// 于是我们在**同一个账号、同一个模型、每次新解一个 captcha**（param 一次性）
// 的条件下做了对照 —— 唯一变量是 `system` 字段：
//
//	A. 无 system（我们当前行为）
//	   → HTTP 405 code=3012 unusual activity
//	B. 官方 3 块 system（逐字照抄 cliPrefix + stableSections + dynamicSections）
//	   + meta_user 首轮（<system-reminder> 包住 currentDate）
//	   → HTTP 405 code=3012 unusual activity
//
// **结论：两者都不通 → 3012 与请求形状无关，是账号/风控层。**
//
// 脚本：`uitest/diag-system-block-ab.cjs`（可复跑；两组之间需等 45 秒
// 避开 captcha 求解限流）。
//
// ## 那为什么还保留这个模块
//
// 1. 它构造的是**官方客户端真实形状**，是"让代理在指纹层与官方不可区分"
//    这条既有路线的补完 —— 将来若风控策略变化，或要给新账号建立
//    "看起来真实"的首个请求，这套形状是必需的
// 2. 它已经写完并验证过形状正确（有单测），删掉等于白费
// 3. A/B 开关还在，随时可重跑验证
//
// 故：**默认关闭**，通过 `AI_GATEWAY_ZCODE_SYSTEM_BLOCK=1` 启用。
// 在 3012 上它已被证伪，不要指望它解决问题。
//
// # 另一个更根本的事实（见 provider.go 的详注）
//
// 我们的对话走 `coding/paas` 通道，而**额度挂在 `start-plan` 上** ——
// 两条通道。`coding/paas` 回 `1113 无可用资源包` 不是"账号没额度"。
// 而 start-plan 那条在解完验证码后回 3012 —— 即**切过去也不一定能用**，
// 却要付出一整套协议翻译的代价。这是产品决策，已记录待所有者定夺。
//
// # 官方形状（逐字对照参考实现，MIT）
//
// 恰好 **3 个** system 块，每个都带 `cache_control: {type:"ephemeral"}`：
//
//	1. cliPrefix 单独一块                       （"You are ZCode, …"）
//	2. 其余**稳定段**以 "\n\n" 连接              （stableSections）
//	3. 所有**动态段**以 "\n\n" 连接，**带 "\n\n" 前缀**（dynamicSections + environment）
//
// 然后才是调用方自己的 system 内容。
//
// ⚠ 为什么恰好 3 个 + 每个都带 cache_control：Anthropic 只允许 **4 个**
// 缓存断点，官方客户端把 3 个用在 system 上、第 4 个用在最后一条消息
//（后者由 `ApplyClientShape` 负责）。多发会超限。

import (
	_ "embed"
	"encoding/json"
	"fmt"
	"os"
	"strings"
	"sync"
)

//go:embed zcode_system.json
var zcodeSystemJSON []byte

// systemData 是官方 prompt 资源的反序列化形态。
//
// 字段名与资源里的键**逐字对应**，不要"顺手改成 Go 风格" ——
// 资源文件是从官方 bundle 提取的，改名会让 diff 无法对照。
type systemData struct {
	CLIPrefix       string     `json:"cliPrefix"`
	StableSections  []string   `json:"stableSections"`
	DynamicSections struct {
		BeforeEnvironment string `json:"beforeEnvironment"`
		AfterEnvironment  string `json:"afterEnvironment"`
	} `json:"dynamicSections"`
	Environment struct {
		Heading         string `json:"heading"`
		InvokedLine     string `json:"invokedLine"`
		CwdLabel        string `json:"cwdLabel"`
		GitLabel        string `json:"gitLabel"`
		GitNo           string `json:"gitNo"`
		PlatformLabel   string `json:"platformLabel"`
		ShellLabel      string `json:"shellLabel"`
		OSVersionLabel  string `json:"osVersionLabel"`
		PoweredByLine   string `json:"poweredByLine"`
	} `json:"environment"`
	ContextPrefix struct {
		Intro              string `json:"intro"`
		Outro              string `json:"outro"`
		CurrentDateHeading string `json:"currentDateHeading"`
		CurrentDateLine    string `json:"currentDateLine"`
	} `json:"contextPrefix"`
	SystemReminder struct {
		Open  string `json:"open"`
		Close string `json:"close"`
	} `json:"systemReminder"`
}

var (
	systemDataOnce sync.Once
	systemDataVal  systemData
	systemDataErr  error
)

// loadSystemData 懒加载官方 prompt 资源。
//
// 用 `sync.Once` 而不是包级 `init()`：解析失败时 `init()` 会 panic，
// 而一块**默认不启用**的 prompt 资源不该有能力让整个网关起不来。
// 故这里把错误留到真正要用的时候再报。
func loadSystemData() (systemData, error) {
	systemDataOnce.Do(func() {
		systemDataErr = json.Unmarshal(zcodeSystemJSON, &systemDataVal)
		if systemDataErr == nil && strings.TrimSpace(systemDataVal.CLIPrefix) == "" {
			systemDataErr = fmt.Errorf("官方 prompt 资源里 cliPrefix 为空（资源可能损坏）")
		}
	})
	return systemDataVal, systemDataErr
}

// envInfo environment 段要用的实际取值。
//
// ⚠ 这些值**必须与请求头一致**（`X-Platform` / `X-Os-Version` 等）。
// 参考实现明确警告：prompt 里写一套、请求头写另一套，会拼出
// 「真实客户端不会产生的混合组合」—— 那比不写还糟。
type envInfo struct {
	Cwd        string
	IsGit      bool
	Platform   string // 形如 win32-x64（与 X-Platform 同源）
	Shell      string
	OSVersion  string
	Model      string
	Provider   string
}

// BuildOfficialSystem 构造官方形状的 system 块数组。
//
// 返回 `[]map[string]any`，可直接塞进 Anthropic 请求体的 `system` 字段。
//
// 参数 `existing` 是调用方（Claude Code / DSH 等）自己的 system 内容 ——
// 官方块**排在它前面**，不替换它。这一点很重要：整体替换会丢掉客户端的
// 项目规范与工具约定（那是 `prompt` 模块 `custom` 模式的已知副作用）。
func BuildOfficialSystem(existing any, model string, env envInfo) ([]map[string]any, error) {
	data, err := loadSystemData()
	if err != nil {
		return nil, err
	}

	stable := strings.Join(data.StableSections, "\n\n")
	dynamic := strings.Join([]string{
		data.DynamicSections.BeforeEnvironment,
		buildEnvironmentSection(data, model, env),
		data.DynamicSections.AfterEnvironment,
	}, "\n\n")

	ephemeral := map[string]any{"type": "ephemeral"}
	official := []map[string]any{
		{"type": "text", "text": data.CLIPrefix, "cache_control": ephemeral},
		{"type": "text", "text": stable, "cache_control": ephemeral},
		// ⚠ 第 3 块带 "\n\n" 前缀 —— 这是官方形状的一部分，不是笔误。
		// 参考实现逐字保留了它（`text: `\n\n${dynamic}``）。
		{"type": "text", "text": "\n\n" + dynamic, "cache_control": ephemeral},
	}

	return append(official, normalizeUserSystem(existing)...), nil
}

// buildEnvironmentSection 拼出 `# Environment` 段。
//
// 形状照官方模板：
//
//	# Environment
//	You have been invoked in the following environment:
//	 - Primary working directory: <cwd>
//	 - Is a git repository: yes/no
//	 - Platform: <win32-x64>
//	 - Shell: <bash>
//	 - OS Version: <Windows 10 Pro>
//	 - You are powered by the model named <provider>/<model>.
func buildEnvironmentSection(data systemData, model string, env envInfo) string {
	e := data.Environment
	var b strings.Builder
	b.WriteString(e.Heading)
	b.WriteString("\n")
	b.WriteString(e.InvokedLine)
	b.WriteString("\n")

	line := func(label, value string) {
		if strings.TrimSpace(value) == "" {
			// 取不到就**整行省略**，而不是写 "unknown" ——
			// 假值比缺行更容易被识别成"非真实客户端"（与身份头同一取舍）
			return
		}
		b.WriteString(" - ")
		b.WriteString(label)
		b.WriteString(": ")
		b.WriteString(value)
		b.WriteString("\n")
	}

	line(e.CwdLabel, env.Cwd)
	// git 是布尔：两个分支都要写（"no" 也是有效信息，省略反而奇怪）
	if env.IsGit {
		line(e.GitLabel, "yes")
	} else {
		line(e.GitLabel, e.GitNo)
	}
	line(e.PlatformLabel, env.Platform)
	line(e.ShellLabel, env.Shell)
	line(e.OSVersionLabel, env.OSVersion)

	if strings.TrimSpace(model) != "" {
		powered := strings.ReplaceAll(e.PoweredByLine, "{provider}", env.Provider)
		powered = strings.ReplaceAll(powered, "{model}", model)
		b.WriteString(powered)
		b.WriteString("\n")
	}
	return strings.TrimRight(b.String(), "\n")
}

// normalizeUserSystem 把调用方自己的 system 统一成块数组。
//
// 官方客户端只会发块数组；调用方可能发：
//
//	· 字符串        → 包成一个 text 块
//	· 块数组        → 原样保留（但**剥掉它们的 cache_control**，见下）
//	· nil / 其它    → 返回空
//
// ⚠ 必须剥掉调用方的 `cache_control`：官方 3 块已占用 3 个断点，
// 加上最后一条消息那个就是 4 个（Anthropic 上限）。客户端自带的
// 外来断点会让请求**超出上限**，而真实客户端自己拥有整个请求体、
// 从不发外来断点。
func normalizeUserSystem(existing any) []map[string]any {
	switch v := existing.(type) {
	case nil:
		return nil
	case string:
		if strings.TrimSpace(v) == "" {
			return nil
		}
		return []map[string]any{{"type": "text", "text": v}}
	case []any:
		out := make([]map[string]any, 0, len(v))
		for _, item := range v {
			m, ok := item.(map[string]any)
			if !ok {
				continue
			}
			clone := make(map[string]any, len(m))
			for k, val := range m {
				if k == "cache_control" {
					continue // 剥掉外来断点（理由见上）
				}
				clone[k] = val
			}
			out = append(out, clone)
		}
		return out
	case []map[string]any:
		out := make([]map[string]any, 0, len(v))
		for _, m := range v {
			clone := make(map[string]any, len(m))
			for k, val := range m {
				if k == "cache_control" {
					continue
				}
				clone[k] = val
			}
			out = append(out, clone)
		}
		return out
	default:
		return nil
	}
}

// BuildContextPrefixMessage 构造 `meta_user` 首轮消息。
//
// 官方客户端在第一轮之前插一条 user 消息，内容是 `<system-reminder>` 包住的
// 当前日期块（**没有内层填充换行**）：
//
//	<system-reminder>
//	As you answer the user's questions, you can use the following context:
//	# currentDate
//	Today's date is 2026-09-19.
//
//	IMPORTANT: this context may or may not be relevant to your tasks. …
//	</system-reminder>
//
// 这是官方请求体的又一个特征形状。
func BuildContextPrefixMessage(now string) (map[string]any, error) {
	data, err := loadSystemData()
	if err != nil {
		return nil, err
	}
	cp := data.ContextPrefix
	dateLine := strings.ReplaceAll(cp.CurrentDateLine, "{date}", now)

	body := strings.Join([]string{
		cp.Intro,
		cp.CurrentDateHeading,
		dateLine,
		"",
		cp.Outro,
	}, "\n")

	return map[string]any{
		"role": "user",
		"content": []map[string]any{{
			"type": "text",
			"text": data.SystemReminder.Open + "\n" + body + "\n" + data.SystemReminder.Close,
		}},
	}, nil
}

// SystemBlockEnabled 报告是否启用官方 system 块注入。
//
// **默认关闭**：这是一条**未经验证**的假设（见文件头说明）。
// 开启方式：环境变量 `AI_GATEWAY_ZCODE_SYSTEM_BLOCK=1`。
//
// 为什么做成环境变量而不是配置项：它的用途是**在同一账号上做 A/B 对照**
//（开/关各打一次看 3012 是否消失），那种实验不需要重启整套配置、
// 更不该成为持久化设置里一个用户看不懂的开关。
func SystemBlockEnabled() bool {
	v := strings.TrimSpace(os.Getenv("AI_GATEWAY_ZCODE_SYSTEM_BLOCK"))
	return v == "1" || strings.EqualFold(v, "true")
}
