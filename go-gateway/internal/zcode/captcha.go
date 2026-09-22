package zcode

// 阿里云无痕验证的**求解**（ZCode 对话通道的 3007「captcha verify failed」）。
//
// # 为什么需要它
//
// 实测（2026-09-19）：ZCode 对话端点对只带 JWT 的请求一律回
//
//	HTTP 400 {"code":3007,"msg":"captcha verify failed"}
//
// 且与**模型名大小写**、**请求头完整度**都无关（已用多组对照排除）。
// 服务端要求带 `X-Aliyun-Captcha-Verify-Param`。
//
// # 求解原理（以及它的性质）
//
// `assets/zcode-captcha/solver.js` 用 **happy-dom** 在 Node 里模拟一个
// 浏览器环境，在其中加载并运行**阿里云官方的验证 SDK**，走完它的正常
// 流程拿到 verifyParam。
//
// ⚠ 说清楚它是什么、不是什么：
//
//	是：在精简环境里正常执行官方 SDK
//	不是：逆向破解验证算法
//
// 但它在**没有真人操作**的情况下产出通过凭证，这一点必须让用户知道 ——
// 故本能力：默认关闭、界面上说明用途、**只在用户主动开启后**才参与请求。
//
// # 三条实测出来的硬约束（都踩过）
//
//  1. **verifyParam 是一次性的**。同一个值用第二次必然回 3007。
//     故**不能做预解池复用** —— 每个请求要现解（或用完即弃）。
//
//  2. ⚠⚠ **`[pe-stall]` 不是上游限流**（2026-09-21 实测修正，此前判断错了）。
//
//     旧注释写的是「连续请求几次后上游回 `[pe-stall]`，等约 90 秒恢复，
//     故必须有冷却」。**两半都错**：
//
//     ① 它的真实含义是**求解器自己的失速超时** —— 见 solver.js 的 stallTimer：
//     距上一次 XHR 超过 `CAPTCHA_STALL_MS`（旧默认 **6 秒**）就判失速并失败。
//     与上游无关，纯本地等待。
//
//     ② 实测对照（同机器、同求解器，各连跑 5 次）：
//
//     CAPTCHA_STALL_MS=6000   成功 2 / 失败 3（失败**全是** STALL）
//     CAPTCHA_STALL_MS=20000  成功 4 / 失败 1
//
//     即：提高阈值就把成功率从 40% 提到 80% —— 若是上游限流，改本地阈值
//     不会有任何效果。**故"等 90 秒恢复"是巧合**（那 90 秒里 stall 判定
//     恰好没被触发），不是服务端冷却。
//
//     现在 solver.js 的默认值已提到 20 秒。**冷却仍保留**（见下），但它现在
//     只用于兜住"真的反复失败"的场景，不再是主要路径。
//
//  3. **调试信息在 stderr、结果在 stdout**。
//     合并两者（`2>&1`）会被 `[pe-stall]` 抢先，看起来像失败 ——
//     我因此误判过一次，以为求解器不可用。
//
// # 它解决不了什么（同样重要）
//
// 实测：带上有效 verifyParam 后 **3007 消失**，但会变成
//
//	HTTP 405 {"code":3012,"msg":"request has been blocked due to unusual activity."}
//
// 3012 是**账号/风控层**，与 deviceMid 取值、param 复用都无关
//（三种 deviceMid 取值实测都是 3012）。故本模块**只解决验证码这一层**，
// 不承诺对话一定能通 —— 界面要如实说明。

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
	"sync"
	"time"
)

// CaptchaSolver 求解器（带限流冷却与 node 探测）。
type CaptchaSolver struct {
	mu sync.Mutex
	// dir 求解器所在目录（含 solver.js 与 node_modules）。
	dir string
	// nodePath 已解析到的 node 可执行文件；空 = 尚未探测。
	nodePath string
	// nodeErr 探测失败的原因（供界面如实展示）。
	nodeErr error
	// cooldownUntil 限流冷却截止；零值 = 不在冷却中。
	cooldownUntil time.Time
	// cooldownReason 进入冷却的原因（供诊断）。
	cooldownReason string
	// config 上游下发的 captcha 配置（scene/region/prefix）。
	scene  string
	region string
	prefix string
	// nodePathOverride 非 nil 时**替代**自动探测出的候选路径（测试专用）。
	//
	// # 为什么需要这个钩子（2026-09-21）
	//
	// 探测扩展后，`nodeCandidatePaths` 会去扫 `~/.nvmd`、`/usr/local/bin` 等
	// **真实存在**的位置。于是"想制造探测失败"的测试（清空 PATH）在装了
	// node 的开发机上**必然失败** —— 测出来的不是代码行为，而是"这台机器
	// 装了什么"。
	//
	// 用钩子把候选集替换成空列表，就能让"找不到 node"成为确定事实。
	// nil（生产默认）= 走真实探测。
	nodePathOverride []string

	// externalURL 宿主提供的**外部求解服务**地址（形如 `http://127.0.0.1:PORT/solve`）。
	//
	// # 为什么要它（2026-09-21 所有者提出的方案）
	//
	// 原做法：起 Node 子进程 + happy-dom **模拟**浏览器跑阿里云 SDK。两个问题：
	//
	//	① 要用户机器上有 Node（否则功能不可用）—— 我们甚至为此内置了 81MB node
	//	② 模拟环境被风控盯上，实测成功率仅 ~40%（改 stallMs 后 88%）
	//
	// 而官方 ZCode 客户端用的是**真实浏览器环境**（已从 app.asar 核实）：
	//
	//	script.src = ".../aliyunCaptcha/AliyunCaptcha.js"
	//	inst.startTracelessVerification()      // 无痕验证，约 0.9 秒拿到 param
	//
	// 我们的 Tauri 宿主**就有**真实 WebView2（Windows 自带，零体积）。
	// 实测（`uitest/probe-captcha-in-browser.cjs`，有头 Chrome）：
	//
	//	startTracelessVerification()  →  929ms 拿到 param（280 字符）
	//
	// 故宿主在 WebView2 里跑官方 SDK，网关通过 HTTP 问它要 param。
	//
	// 空 = 未配置（回退到本地 Node 求解，行为与改动前一致）。
	externalURL string
	// externalToken 调外部求解服务时带的共享密钥（同机 IPC，防其它进程误用）。
	externalToken string
	// lastExternalErr 最近一次外部求解失败的原因（供错误信息如实透出）。
	//
	// 为什么要记：外部求解与本地 Node 是**两条独立路径**，两边都可能失败。
	// 只报本地那条的原因会让用户以为"是 Node 的问题"，而实际外部服务
	// 才是首选路径（它的失败原因往往更关键，如"宿主窗口还没起"）。
	lastExternalErr string
	// pool 预取的 param 池（见 captchaPool）。
	//
	// # 为什么必须池化（所有者明确要求）
	//
	// 所有者原话：
	//
	//	「而且要有一个池子,备用,不然多并发一下就不够用,还会拉长延迟」
	//
	// 他说的是实情：单次求解约 0.9 秒。若不池化，**每个**需要补码的请求
	// 都要现场等 0.9 秒 —— 并发 N 个请求就串成 N × 0.9 秒。
	// 而 param 有约 2 分钟寿命（参考实现 `CAPTCHA_TOKEN_TTL = 95_000`），
	// 完全可以预先解好放着，取用时是**零等待**。
	pool *captchaPool
}

// captchaPool 预取的 verifyParam 池。
//
// # 设计要点（都来自实测，不是拍脑袋）
//
//   - **寿命**：上游给 param 约 2 分钟有效期（参考实现取 95s 留余量，
//     本实现取 90s）。过期即丢弃，**绝不把过期值发出去** —— 那会回 3007，
//     比"池空、现解一个"更糟（现解至少有机会成功）。
//
//   - **上限**：池不是越大越好。每个 param 都会在 2 分钟后失效，
//     无上限地补货会让后台不断求解却没人用（浪费 CPU 与上游请求）。
//
//   - **补货是后台的**：取用时若发现池低于水位，**异步**触发补充，
//     不阻塞当前请求 —— 阻塞就回到"每个请求等 0.9 秒"的老问题。
//
//   - **单飞**：同时只能有一个补货在跑。并发触发会拉起 N 个求解，
//     那正是我们要避免的。
type captchaPool struct {
	mu     sync.Mutex
	items  []pooledParam
	refill bool // 补货中（单飞标志）
	// solve 实际的求解函数（由 CaptchaSolver 注入）。
	//
	// 为什么用函数而不是直接调 solver 方法：池是**纯数据结构**，
	// 不持有求解器的锁与配置。注入让它可独立测试（测试传入假求解函数，
	// 不必起 node 或 HTTP 服务）。
	solve func() (string, error)
}

// pooledParam 池里的一枚 param 及其出生时刻。
type pooledParam struct {
	param string
	born  time.Time
}

const (
	// captchaParamTTL param 的有效期。
	//
	// 90 秒：参考实现取 95s（`CAPTCHA_TOKEN_TTL = 95_000`，注释说"上游实际 ~2min"）。
	// 我们取更保守的 90s —— 宁可早弃重解，也不要在边界上发出一个刚好过期的值。
	captchaParamTTL = 90 * time.Second
	// captchaPoolTarget 池的目标水位（低于它就触发后台补货）。
	//
	// 3：足以覆盖"用户连续发几个请求"与轻量并发。取更大只会让后台
	// 多解几个大概率用不上的 param（每个都要起一次求解）。
	captchaPoolTarget = 3
	// captchaPoolMax 池的上限（硬顶，防止无限增长）。
	captchaPoolMax = 8
	// captchaWaitForRefill 池空时，等"已在进行的补货"的最长时间。
	//
	// # 取值依据（2026-09-21 实测）
	//
	// 冷启动时第一个求解要付：建 WebView2 窗口 + 下载官方 SDK（225KB）
	// + pe 字节码（约 450KB）。所有者实测首次对话 **29 秒**。
	//
	// 预热就绪后，单次求解约 1 秒（宿主侧实测 929ms）。
	//
	// 故等待上限取 **25 秒**：足以覆盖冷启动那一次（避免用户另起一次、
	// 重复付窗口与下载成本），又不至于让请求长挂 —— 超过它就放弃等待，
	// 走原有的"现场解"路径（宁可多试，也不能让请求无限等）。
	captchaWaitForRefill = 25 * time.Second
)

// take 从池里取一枚未过期的 param。池空/全过期时返回空串。
//
// 取用的同时**异步**触发补货（若低于水位），故调用方拿到的要么是现成值
//（零等待），要么是空串（由调用方决定现场解）。
func (p *captchaPool) take(now time.Time) string {
	if p == nil {
		return ""
	}
	p.mu.Lock()
	// 先清掉过期的（绝不发过期值）
	alive := p.items[:0]
	for _, it := range p.items {
		if now.Sub(it.born) < captchaParamTTL {
			alive = append(alive, it)
		}
	}
	p.items = alive
	var got string
	if len(p.items) > 0 {
		got = p.items[0].param
		p.items = p.items[1:]
	}
	low := len(p.items) < captchaPoolTarget
	p.mu.Unlock()
	if low {
		go p.refillAsync()
	}
	return got
}

// takeOrWait 池空时**等待正在进行的补货**，而不是让调用方另起一次求解。
//
// # 为什么必须等（2026-09-21 所有者反馈「首次对话 29 秒」）
//
// 所有者原话：「我首次对话就耗费了29秒，你需要优化这个」。
//
// 那 29 秒的主要构成是**串行的重复劳动**：
//
//	网关启动 → 预热开始（第 1 次求解：建窗口 + 下 SDK，最慢）
//	用户立刻发请求 → 池还空 → Solve **又起一次求解**（第 2 次，与预热并行）
//	                两次都在建 WebView 窗口、都在等 SDK ⇒ 互相争抢且都没快多少
//
// 旧实现的问题：池空就无脑走 `solveOnce`，**不知道后台已经在解了**。
// 而"等一个已在进行的求解"通常比"另起一个"更快 —— 尤其是冷启动阶段
//（第一个求解要付窗口创建 + SDK 下载的全部成本，第二个并不能省掉它）。
//
// # 等待上限
//
// 那 29 秒里包含首次窗口创建与 SDK 下载，无法靠等待消除 —— 但可以
// **不重复付**。等待上限取 captchaWaitForRefill（见其注释）：超过它就
// 放弃等待、走原有路径（宁可多试一次，也不能让请求无限挂着）。
func (p *captchaPool) takeOrWait(now time.Time, maxWait time.Duration) string {
	if p == nil {
		return ""
	}
	if got := p.take(now); got != "" {
		return got
	}
	// 池空：若已有补货在跑，等它出一枚（轮询窗口很短，避免复杂条件变量）。
	if !p.refilling() {
		return ""
	}
	deadline := time.Now().Add(maxWait)
	for time.Now().Before(deadline) {
		time.Sleep(100 * time.Millisecond)
		if got := p.take(time.Now()); got != "" {
			return got
		}
		if !p.refilling() {
			break // 补货结束了（成功或失败），没必要再等
		}
	}
	return ""
}

// refilling 报告是否已有补货在跑。
func (p *captchaPool) refilling() bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.refill
}

// refillAsync 后台补货到目标水位（单飞）。
func (p *captchaPool) refillAsync() {
	if p == nil || p.solve == nil {
		return
	}
	p.mu.Lock()
	if p.refill {
		p.mu.Unlock()
		return // 已有补货在跑
	}
	p.refill = true
	p.mu.Unlock()
	defer func() {
		p.mu.Lock()
		p.refill = false
		p.mu.Unlock()
	}()
	p.refillTo(captchaPoolTarget)
}

// refillTo 补充到目标数量。由 refillAsync 在后台调用。
func (p *captchaPool) refillTo(target int) {
	for {
		p.mu.Lock()
		n := len(p.items)
		fn := p.solve
		p.mu.Unlock()
		if n >= target || n >= captchaPoolMax || fn == nil {
			return
		}
		param, err := fn()
		if err != nil || param == "" {
			// 求解失败就停：继续循环只会连续撞失败（而失败往往有冷却），
			// 徒增日志与 CPU。
			return
		}
		p.mu.Lock()
		if len(p.items) < captchaPoolMax {
			p.items = append(p.items, pooledParam{param: param, born: time.Now()})
		}
		p.mu.Unlock()
	}
}

// DefaultCaptchaCooldown 求解**连续失败**后的冷却时长。
//
// # ⚠ 2026-09-21 修正：这不是"上游限流冷却"
//
// 旧注释写「实测：连续求解约 3-5 次后开始 `pe-stall`，等 90 秒恢复」，
// 并据此把它当成**上游施加的限流**。实测推翻了这个结论：
//
//	`pe-stall` 是求解器**自己的失速超时**（solver.js 的 stallTimer：
//	距上次 XHR 超过 CAPTCHA_STALL_MS 就判失速）。
//
// 对照实验（同机器，各连跑 5 次）：
//
//	stallMs=6000   成功 2 / 失败 3（失败全是 STALL）
//	stallMs=20000  成功 4 / 失败 1
//
// 改**本地**阈值就能把成功率从 40% 提到 80% —— 若真是上游限流，这不成立。
//
// 故现在的语义变为：**兜底熔断**。求解偶尔失败属正常（stall 是概率性的），
// 但若连续失败，说明求解器环境有问题（Node 版本、SDK 变更、网络异常），
// 此时继续密集重试只会烧 CPU 并刷日志，故冷却一段时间再试。
//
// 时长仍取 2 分钟：真实故障（如 SDK 被上游改坏）不会在更短时间内自愈，
// 而偶发 stall 本就不该走到这里（见 Solve 里的重试）。
const DefaultCaptchaCooldown = 2 * time.Minute

// captchaEntryFile 求解器入口文件名。
//
// # 为什么优先用打包好的单文件
//
// 原始做法是把整个 `node_modules/` 塞进安装包 ——
// **11.36MB / 3353 个文件**。而 Tauri 的 NSIS 模板对每个资源文件生成
// 一条 `File /a "/oname=..."`，即**逐文件解压**；3353 个零散小文件的
// 写入远慢于单个大文件（尤其被杀软逐个扫描时）。
//
// 实测：装了这种包的机器上，安装过程慢到用户专门反馈
//（「安装的时候那个 node_modules 解压速度超级慢」）。
//
// 现在用 esbuild 打成**单个文件**（`solver.bundle.cjs`，928KB），
// 安装时的文件写入从 3353 次降到 1 次。
// 打包方法与坑见 `assets/zcode-captcha/README-PACKAGING.md`。
//
// ⚠ 保留 `solver.js` 作为**回退**：万一某台机器上 bundle 出问题
//（如 node 版本过旧不支持某个语法），源码目录还在就能原地诊断。
// 这也是"打包产物不可读"的补偿 —— 排查时有源码可看。
const (
	captchaEntryBundle = "solver.bundle.cjs"
	captchaEntrySource = "solver.js"
)

// captchaConfig 上游下发的 captcha 默认值（取自 client/configs 实测）。
const (
	defaultCaptchaScene  = "11xygtvd"
	defaultCaptchaRegion = "cn"
	defaultCaptchaPrefix = "no8xfe"
)

// NewCaptchaSolver 造一个求解器（带实测默认的运营参数）。
//
// ⚠ 千万不要直接 `&CaptchaSolver{}` —— 那样 scene/region/prefix 都是空串，
// 求解必然失败（上游要求这三个非空）。本测试
// `TestSetCaptchaConfigFromUpstream` 就是抓住这个才写的。
func NewCaptchaSolver() *CaptchaSolver {
	return &CaptchaSolver{
		scene:  defaultCaptchaScene,
		region: defaultCaptchaRegion,
		prefix: defaultCaptchaPrefix,
	}
}

var (
	captchaOnce   sync.Once
	captchaShared *CaptchaSolver
)

// SharedCaptchaSolver 进程级共享的求解器（含冷却状态）。
//
// 为什么共享：冷却状态必须是**进程级**的 —— 每个账号各造一个求解器的话，
// 限流会变成"每个账号各自撞墙"，而限流是**上游按来源**施加的。
func SharedCaptchaSolver() *CaptchaSolver {
	captchaOnce.Do(func() {
		captchaShared = NewCaptchaSolver()
	})
	return captchaShared
}

// SetCaptchaConfig 用上游配置覆盖默认值（scene/region/prefix 都是运营参数）。
func (s *CaptchaSolver) SetCaptchaConfig(scene, region, prefix string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if v := strings.TrimSpace(scene); v != "" {
		s.scene = v
	}
	if v := strings.TrimSpace(region); v != "" {
		s.region = v
	}
	if v := strings.TrimSpace(prefix); v != "" {
		s.prefix = v
	}
}

// SetExternalSolver 配置**外部求解服务**（宿主的 WebView2 求解器）。
//
// # 为什么有它（2026-09-21 所有者提出的方案，实测确认可行）
//
// 原做法是本地起 Node 子进程 + happy-dom **模拟**浏览器。两个硬伤：
//
//	① 要求用户机器有 Node（否则 ZCode 不可用）—— 为此外置了 81MB node.exe
//	② 模拟环境被风控盯上，实测成功率仅约 40%（把 stallMs 从 6s 提到 20s 后 88%）
//
// 而官方 ZCode 客户端用的是**真实浏览器环境**（已从 `resources/app.asar` 核实）：
//
//	script.src = ".../aliyunCaptcha/AliyunCaptcha.js"
//	inst.startTracelessVerification()   // 无痕验证
//
// 我们的 Tauri 宿主**自带**真实 WebView2（Windows 10/11 预装，零体积）。
// 实测（`uitest/probe-captcha-in-browser.cjs`，有头 Chrome，脚本触发无人工点击）：
//
//	initAliyunCaptcha   → +10ms
//	getInstance         → +600ms
//	startTracelessVerification()
//	success             → **+929ms** 拿到 param（280 字符）
//
// 即：真实环境里不到 1 秒，比本地 Node 方案（~3 秒）更快，且不占安装包体积。
//
// 参数：
//
//	url   宿主求解服务的完整地址（如 `http://127.0.0.1:51234/solve`）。空 = 禁用。
//	token 共享密钥，随请求头发给宿主（同机 IPC，防止其它进程误用这个端口）。
//
// ⚠ 配置后**外部优先于本地**：外部是真实浏览器环境，更接近官方客户端，
// 成功率与速度都更好。本地 Node 求解保留为回退（外部不可用时仍能工作）。
func (s *CaptchaSolver) SetExternalSolver(url, token string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.externalURL = strings.TrimSpace(url)
	s.externalToken = strings.TrimSpace(token)
	if s.externalURL != "" && s.pool == nil {
		// 池的求解函数指向"外部求解"（而非本地 Node）——
		// 池的目标就是**预先备好**外部环境的产物，避免请求路径上等 0.9 秒。
		p := &captchaPool{}
		p.solve = func() (string, error) {
			return s.solveViaExternal()
		}
		s.pool = p
		// 立即预热一批：否则前几个请求仍要现场等。
		go p.refillAsync()
	}
}

// externalSolveEnabled 报告外部求解是否已配置。
func (s *CaptchaSolver) externalSolveEnabled() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.externalURL != ""
}

// externalErrText 返回最近一次外部求解失败的原因（空 = 没失败过/未配置）。
func (s *CaptchaSolver) externalErrText() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.lastExternalErr == "" {
		return "未配置"
	}
	return s.lastExternalErr
}

// solveViaExternal 调宿主的 WebView2 求解服务要一个 param。
//
// 协议（与宿主侧 `captcha_webview` 模块对齐）：
//
//	POST <url>
//	X-Captcha-Token: <token>
//	→ 200 {"param": "...", "region": "cn"}
//	→ 非 200 / 体里无 param ⇒ 失败（如实返回原因）
func (s *CaptchaSolver) solveViaExternal() (string, error) {
	s.mu.Lock()
	url, token := s.externalURL, s.externalToken
	s.mu.Unlock()
	if url == "" {
		return "", fmt.Errorf("未配置外部求解服务")
	}

	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, url, nil)
	if err != nil {
		return "", err
	}
	if token != "" {
		req.Header.Set("X-Captcha-Token", token)
	}
	// 与宿主约定：要哪个 scene/region/prefix。虽然宿主侧有默认值，
	// 但显式传避免两边默认值漂移（上游改运营参数时只改一处）。
	q := req.URL.Query()
	s.mu.Lock()
	q.Set("scene", s.scene)
	q.Set("region", s.region)
	q.Set("prefix", s.prefix)
	s.mu.Unlock()
	req.URL.RawQuery = q.Encode()

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", fmt.Errorf("请求宿主求解服务失败: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		return "", fmt.Errorf("宿主求解失败（HTTP %d）：%s",
			resp.StatusCode, truncateStr(string(raw), 200))
	}
	var doc struct {
		Param string `json:"param"`
		Error string `json:"error"`
	}
	if err := json.Unmarshal(raw, &doc); err != nil {
		return "", fmt.Errorf("宿主求解响应不是合法 JSON: %w", err)
	}
	if doc.Param == "" {
		if doc.Error != "" {
			return "", fmt.Errorf("宿主求解失败：%s", doc.Error)
		}
		return "", fmt.Errorf("宿主未返回 param（原始：%s）", truncateStr(string(raw), 160))
	}
	return doc.Param, nil
}

// truncateStr 截断字符串用于错误信息（避免把整页 HTML 塞进响应）。
func truncateStr(s string, n int) string {
	s = strings.TrimSpace(s)
	if len(s) <= n {
		return s
	}
	// 按 rune 截断，避免切坏 UTF-8 中文
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	return string(r[:n]) + "…"
}
// SetDir 指定求解器目录（宿主把它作为资源释放到某处后告诉网关）。
//
// 目录里需要有 solver.bundle.cjs（优先）或 solver.js。
// 配置了外部求解（SetExternalSolver）时本目录**仍保留** —— 它是回退路径。
func (s *CaptchaSolver) SetDir(dir string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.dir = strings.TrimSpace(dir)
	s.nodePath = "" // 目录变了要重新探测
	s.nodeErr = nil
}

// Dir 返回当前求解器目录。
func (s *CaptchaSolver) Dir() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.dir
}

// Region 返回验证码所属区域（`X-Aliyun-Captcha-Verify-Region` 的值）。
//
// # 为什么需要这个访问器
//
// 它与 param **成对**：实测缺 region 就是 3007（见 cred.go 的注释）。
// 而 region 是求解器的运营参数（可能被上游配置覆盖，见 SetCaptchaConfig），
// 故**不能**在调用方硬编码 —— 那样一旦上游换区域，我们会带着
// 旧 region 去配新 param，结果是"求解成功但仍 3007"，极难查。
//
// 空串表示未配置，调用方应回落默认值。
func (s *CaptchaSolver) Region() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.region
}

// resolveNode 找到可用的 node 可执行文件。
//
// # 探测顺序（2026-09-21 扩展）
//
//	① 环境变量 ZCODE_NODE_PATH（显式指定，最高优先级）
//	② PATH 里的 node
//	③ **常见安装位置**（见 nodeCandidatePaths）
//	④ 与本程序同目录的 node.exe（发行包将来内置时的位置）
//
// # 为什么必须扩展 ③④（所有者反馈的真实缺陷）
//
// 所有者原话：
//
//	「不是每个用户电脑上都有node，你那个求解器，不能期盼所有用户都能
//	  满足运行环境，你需要修复这个问题」
//
// 他说的是可用性问题，但这里先修**同一问题的另一半**：即使装了 Node，
// 只要它不在**网关进程继承到的 PATH** 里，`exec.LookPath` 就找不到。
//
// 这不是假想：网关由宿主 GUI 以子进程方式启动，继承的是**启动 GUI 时**
// 的环境 —— 用户后来用 nvm/nvmd 装的 Node 往往只写进了**新开的终端**的
// PATH，GUI 那份环境是旧的。实测本机 `node` 在
// `%USERPROFILE%\.nvmd\versions\<ver>\node.exe`，而这类路径**默认不在**
// 系统 PATH 里（nvmd 靠 shell 钩子注入）。
//
// 后果与"完全没装 Node"完全一样（求解器报找不到 node），但用户会认为
// "我明明装了" —— 这是最难解释的一类故障。多列几个候选路径就能覆盖
// 绝大多数真实安装方式，成本只是几次 os.Stat。
//
// # 为什么仍不内置 node.exe
//
// 实测真实二进制 **93MB**（本机 nvmd v26.0.0），打进安装包会让 11.8MB
// 变成约 100MB。那是**产品决策**（体积 vs 开箱可用），不在这里替所有者定；
// 本函数把"内置后能直接找到"的位置（④）预留好，届时只需把文件放进去。
func (s *CaptchaSolver) resolveNode() (string, error) {
	if s.nodePath != "" {
		return s.nodePath, nil
	}
	if s.nodeErr != nil {
		return "", s.nodeErr
	}
	// ① 显式环境变量
	if env := strings.TrimSpace(os.Getenv("ZCODE_NODE_PATH")); env != "" {
		if _, err := os.Stat(env); err == nil {
			s.nodePath = env
			return env, nil
		}
	}
	// ② PATH
	name := "node"
	if runtime.GOOS == "windows" {
		name = "node.exe"
	}
	if p, err := exec.LookPath(name); err == nil {
		s.nodePath = p
		return p, nil
	}
	// ③④ 常见安装位置
	for _, p := range s.candidatePaths() {
		if _, err := os.Stat(p); err == nil {
			s.nodePath = p
			return p, nil
		}
	}
	s.nodeErr = fmt.Errorf("找不到 node 可执行文件。ZCode 的验证码求解需要 Node.js；"+
		"请安装 Node.js（https://nodejs.org）后重启本软件，"+
		"或用环境变量 ZCODE_NODE_PATH 指定 node.exe 的完整路径。"+
		"（已查找：ZCODE_NODE_PATH、PATH、%s）",
		strings.Join(s.candidatePaths(), "、"))
	return "", s.nodeErr
}

// candidatePaths 返回要尝试的 node 路径候选：生产走真实探测，测试可覆盖。
func (s *CaptchaSolver) candidatePaths() []string {
	if s.nodePathOverride != nil {
		return s.nodePathOverride
	}
	return s.nodeCandidatePaths()
}

// nodeCandidatePaths 列出**常见安装位置**的 node 可执行文件候选。
//
// 顺序按"最可能命中"排：版本管理器（nvm/nvmd/fnm/volta）→ 官方安装包 →
// 包管理器（scoop/choco/winget）→ 与本程序同目录（内置位置）。
//
// # 为什么要覆盖这么多
//
// 每个工具装的路径都不同，而**漏掉一个就等于"那个用户仍然不能用"**。
// 这份列表是穷举常见方式，不是猜 —— 每一项都对应一个真实存在的安装器。
//
// Windows 与类 Unix 分列：路径分隔与文件名不同，混在一起会列出
// 一堆在对方平台上永远不存在的路径（无害但会让错误信息变长）。
func (s *CaptchaSolver) nodeCandidatePaths() []string {
	var out []string
	add := func(p string) {
		if p == "" {
			return
		}
		out = append(out, p)
	}

	// ④ 与本程序同目录（发行包内置 node 时的位置）。
	//
	// 放在**最前面**：内置的那份是我们完全控制的版本，比用户环境里的
	// 任何版本都更可能与求解器兼容（求解器依赖 happy-dom 的 DOM 行为）。
	if exe, err := os.Executable(); err == nil {
		dir := filepath.Dir(exe)
		add(filepath.Join(dir, nodeExeName()))
		add(filepath.Join(dir, "node", nodeExeName()))
	}

	home, _ := os.UserHomeDir()
	if runtime.GOOS == "windows" {
		appdata := os.Getenv("APPDATA")
		local := os.Getenv("LOCALAPPDATA")
		// nvmd（本机实测用的就是这个）
		if home != "" {
			add(globNewest(filepath.Join(home, ".nvmd", "versions", "*", "node.exe")))
		}
		// nvm-windows
		if appdata != "" {
			add(globNewest(filepath.Join(appdata, "nvm", "v*", "node.exe")))
		}
		// fnm
		if local != "" {
			add(globNewest(filepath.Join(local, "fnm_multishells", "*", "node.exe")))
			add(globNewest(filepath.Join(local, "fnm", "node-versions", "*", "installation", "node.exe")))
		}
		// volta
		if local != "" {
			add(filepath.Join(local, "Volta", "tools", "image", "node", "node.exe"))
		}
		// 官方安装包 / winget / scoop / chocolatey
		add(`C:\Program Files\nodejs\node.exe`)
		add(`C:\Program Files (x86)\nodejs\node.exe`)
		if local != "" {
			add(filepath.Join(local, "Programs", "nodejs", "node.exe"))
			add(filepath.Join(local, "Microsoft", "WinGet", "Links", "node.exe"))
			add(globNewest(filepath.Join(local, "scoop", "apps", "nodejs", "*", "node.exe")))
		}
		add(`C:\ProgramData\chocolatey\bin\node.exe`)
		if programData := os.Getenv("ProgramData"); programData != "" {
			add(globNewest(filepath.Join(programData, "chocolatey", "lib", "nodejs*", "tools", "node.exe")))
		}
	} else {
		if home != "" {
			add(globNewest(filepath.Join(home, ".nvm", "versions", "node", "*", "bin", "node")))
			add(filepath.Join(home, ".volta", "bin", "node"))
			add(filepath.Join(home, ".local", "bin", "node"))
			add(globNewest(filepath.Join(home, ".fnm", "node-versions", "*", "installation", "bin", "node")))
		}
		add("/usr/local/bin/node")
		add("/usr/bin/node")
		add("/opt/homebrew/bin/node")
	}
	return out
}

// nodeExeName 平台对应的 node 可执行文件名。
func nodeExeName() string {
	if runtime.GOOS == "windows" {
		return "node.exe"
	}
	return "node"
}

// globNewest 在通配路径里挑**版本号最大**的那个。
//
// 为什么不用 Glob 的第一个：`filepath.Glob` 返回**字典序**，于是
// `v9` 会排在 `v26` 之后 —— 选出来的是最旧的那个。而版本管理器里
// 同时存在多个版本是常态，装到旧版本上可能因语法/DOM 行为差异
// 导致求解失败，且现象是"求解不出来"而非"找不到 node"，更难查。
//
// 字典序下 `v26.0.0` > `v9.0.0`（"2" < "9"），所以必须显式排序。
// 这里按**路径字符串倒序**取第一个：对 `versions/26.0.0/`、
// `versions/9.0.0/` 这类同前缀路径，倒序即"版本较大者优先"。
// 不追求严格的语义化版本比较 —— 那只在少数版本号长度不同时才有差异
//（如 100 vs 99），而真实 Node 版本号位数一致。
//
// 匹配不到时返回空串（调用方会跳过）。
func globNewest(pattern string) string {
	matches, err := filepath.Glob(pattern)
	if err != nil || len(matches) == 0 {
		return ""
	}
	sort.Sort(sort.Reverse(sort.StringSlice(matches)))
	return matches[0]
}

// entryFile 选出要执行的求解器入口（优先打包好的单文件）。
//
// 返回 (入口文件名, 错误)。两个都不在时报错 —— 空目录会被当成
// "有效组件"，于是求解时才发现缺文件（fail late）。
//
// 调用方需持锁（读 s.dir 之外无共享状态，但保持一致）。
func (s *CaptchaSolver) entryFile() (string, error) {
	if s.dir == "" {
		return "", fmt.Errorf("求解器组件未安装")
	}
	// 优先单文件 bundle：它就是为"安装快"而打出来的
	if _, err := os.Stat(filepath.Join(s.dir, captchaEntryBundle)); err == nil {
		return captchaEntryBundle, nil
	}
	if _, err := os.Stat(filepath.Join(s.dir, captchaEntrySource)); err == nil {
		return captchaEntrySource, nil
	}
	return "", fmt.Errorf("求解器目录里既没有 %s 也没有 %s：%s",
		captchaEntryBundle, captchaEntrySource, s.dir)
}

// Available 报告求解器当前是否可用（供界面判断要不要显示开关）。
//
// ⚠ 与 UnavailableReason 同一口径：**外部求解算可用**（见其注释）。
// 两者必须一致 —— 否则会出现"Available 说不可用、UnavailableReason 说可用"
// 这类自相矛盾的状态，而调用方各取一个就会行为分叉。
func (s *CaptchaSolver) Available() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.externalURL != "" {
		return true
	}
	if _, err := s.entryFile(); err != nil {
		return false
	}
	_, err := s.resolveNode()
	return err == nil
}

// UnavailableReason 不可用的原因（为空 = 可用）。
//
// # ⚠ 必须把**外部求解**算进来（2026-09-21 实测踩到）
//
// 旧实现只看本地组件（目录/入口文件/node），于是配置了外部求解
//（宿主的 WebView2 服务）时，`dir == ""` 仍然被判成"组件未安装" ——
// 而调用方 `client.go::solveCaptcha` 会**先查本函数、非空就直接失败**，
// 压根走不到 `Solve`（那才是有外部回退的地方）。
//
// 实测症状：配置了 `zcode_captcha_solver_url`，请求仍回
//
//	求解器不可用：求解器组件未安装（发行包应包含 assets/zcode-captcha）
//
// 而外部服务其实一切正常（已从服务端日志确认它**根本没被调用到**）。
// 这是一类「新路径接好了、旧的前置检查把它挡在门外」的缺陷。
func (s *CaptchaSolver) UnavailableReason() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	// 外部求解可用 ⇒ 整体可用（它是首选路径，本地只是回退）
	if s.externalURL != "" {
		return ""
	}
	if s.dir == "" {
		return "求解器组件未安装（发行包应包含 assets/zcode-captcha）"
	}
	if _, err := s.entryFile(); err != nil {
		return err.Error()
	}
	if _, err := s.resolveNode(); err != nil {
		return err.Error()
	}
	return ""
}

// isCaptchaStall 报告 stderr 是否是**求解器自身的失速**（而非上游错误）。
//
// `pe-stall` 与 `captcha solve stall` 都来自 solver.js 的 stallTimer
//（距上次 XHR 超过 CAPTCHA_STALL_MS 即判失速），与上游无关。
// `timeout` 是整体超时（30s）兜底，同一性质。
//
// ⚠ 2026-09-21：此前这个判据被用来认定"**上游限流**"并直接冷却 2 分钟。
// 那是误判 —— 见 DefaultCaptchaCooldown 的注释（改本地阈值即可提升成功率，
// 证明与上游无关）。现在它只用来决定"要不要重试"。
func isCaptchaStall(stderrText string) bool {
	return strings.Contains(stderrText, "pe-stall") ||
		strings.Contains(stderrText, "captcha solve stall") ||
		strings.Contains(stderrText, "timeout")
}

// captchaSolveAttempts 单次 Solve 内的求解尝试次数。
//
// 为什么 >1：失速是**概率性**的 —— 实测同一台机器连跑 5 次会有 1-3 次
// `pe-stall`（SDK 启动快慢有随机性，还与 CDN 冷热有关），而紧接着重试
// 往往就成功。旧实现第一次失败就冷却 2 分钟，把偶发抖动放大成了
// 「ZCode 两分钟不可用」。
//
// 取 3：三次都失速基本可判定是环境问题（Node 版本/SDK 变更/网络），
// 那时进冷却才合理。每次约 3-20 秒，最坏约 1 分钟，仍在单次请求可容忍范围内。
const captchaSolveAttempts = 3

// Solve 求解一个 verifyParam（池优先，带内部重试）。
//
// ⚠ 返回值是**一次性**的：用一次就作废，不要缓存复用（实测第二次必回 3007）。
//
// 重试只针对**失速**（求解器自身超时），不针对别的错误 ——
// 组件缺失、Node 找不到、非零退出码这类都是确定性失败，重试无意义且拖慢响应。
//
// # 取用顺序（2026-09-21 加池）
//
//	① 池里有未过期的现成 param → **零等待**返回，并异步触发补货
//	② 池空 → 现场求解（外部服务优先，失败回退本地 Node）
//
// ① 是关键：单次求解约 0.9 秒（外部）/ 3 秒（本地），若不池化，
// **每个**补码请求都要等那么久 —— 并发时更是串行叠加。
// 而 param 有约 90 秒寿命，预先备好几个即可让绝大多数请求零等待。
func (s *CaptchaSolver) Solve(ctx context.Context) (string, error) {
	// ① 池优先：命中就零等待返回。
	s.mu.Lock()
	pool := s.pool
	s.mu.Unlock()
	if pool != nil {
		if p := pool.takeOrWait(time.Now(), captchaWaitForRefill); p != "" {
			return p, nil
		}
	}

	// ② 池空（且没有正在进行的补货，或等待超时）：现场解一个。
	//
	// ⚠ 这里**不**把结果放进池 —— 它是给当前这个请求用的，放进去会与
	// "池 = 预取备用"的语义混淆，也会让池里混入"即将被这个请求消费"的值。
	// 补货由 take() 异步触发（见 captchaPool.take）。
	var lastErr error
	for attempt := 1; attempt <= captchaSolveAttempts; attempt++ {
		param, err := s.solveOnce(ctx)
		if err == nil {
			return param, nil
		}
		lastErr = err
		// 冷却中/组件缺失/Node 缺失：确定性失败，立刻返回（重试只会重复同一条错误）
		if !isCaptchaStall(err.Error()) {
			return "", err
		}
		// 最后一次不再等，直接返回那个已经写好冷却信息的错误
		if attempt == captchaSolveAttempts {
			// 所有尝试都用尽了 —— 这时才熔断。
			//
			// 为什么放在这里而不是 solveOnce 里：失速是概率性的，
			// 第一次失败就冷却会把偶发抖动放大成 2 分钟不可用（旧行为）。
			s.mu.Lock()
			s.cooldownUntil = time.Now().Add(DefaultCaptchaCooldown)
			s.cooldownReason = fmt.Sprintf("连续 %d 次失速（SDK 启动超时，非上游限流）",
				captchaSolveAttempts)
			s.mu.Unlock()
			return "", fmt.Errorf("验证码求解连续 %d 次失速，已暂停 %s 后自动重试；"+
				"若持续如此请确认已装 Node（≥20）且能访问 o.alicdn.com。最后一次原因：%s",
				captchaSolveAttempts, DefaultCaptchaCooldown, err)
		}
		// 短暂退避后重试：给 SDK/网络一个恢复窗口，又不至于让请求久等。
		select {
		case <-ctx.Done():
			return "", ctx.Err()
		case <-time.After(time.Duration(attempt) * time.Second):
		}
	}
	return "", lastErr
}

// solveOnce 单次求解（不含重试）。
func (s *CaptchaSolver) solveOnce(ctx context.Context) (string, error) {
	s.mu.Lock()
	dir := s.dir
	scene, region, prefix := s.scene, s.region, s.prefix
	extURL := s.externalURL
	// 冷却检查
	if !s.cooldownUntil.IsZero() && time.Now().Before(s.cooldownUntil) {
		left := time.Until(s.cooldownUntil).Round(time.Second)
		reason := s.cooldownReason
		s.mu.Unlock()
		return "", fmt.Errorf("验证码求解处于冷却中（还需 %s%s）", left,
			func() string {
				if reason == "" {
					return ""
				}
				return "：" + reason
			}())
	}
	s.mu.Unlock()

	// ① 外部求解（宿主 WebView2）优先 —— 真实浏览器环境，比模拟环境更快更稳。
	//
	// 失败时**不直接返回错误**，而是继续往下走本地 Node 路径：
	// 外部服务可能因为宿主还没起、端口被占、用户关了主窗口等原因不可用，
	// 那种情况下本地求解仍有可能成功，没有理由让整个功能失效。
	if extURL != "" {
		if param, err := s.solveViaExternal(); err == nil && param != "" {
			return param, nil
		} else if err != nil {
			// 记下原因供最终错误信息使用（不打断流程）
			s.mu.Lock()
			s.lastExternalErr = err.Error()
			s.mu.Unlock()
		}
	}

	if dir == "" {
		return "", fmt.Errorf("求解器组件未安装（且外部求解不可用：%s）", s.externalErrText())
	}
	node, err := func() (string, error) {
		s.mu.Lock()
		defer s.mu.Unlock()
		return s.resolveNode()
	}()
	if err != nil {
		return "", err
	}
	// 入口文件在锁内解析（它读 s.dir，与 SetDir 竞争）
	entry, err := func() (string, error) {
		s.mu.Lock()
		defer s.mu.Unlock()
		return s.entryFile()
	}()
	if err != nil {
		return "", err
	}

	cctx, cancel := context.WithTimeout(ctx, 90*time.Second)
	defer cancel()

	cmd := exec.CommandContext(cctx, node, entry, scene, region, prefix)
	cmd.Dir = dir

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return "", err
	}
	var stderr strings.Builder
	cmd.Stderr = &stderr

	if err := cmd.Start(); err != nil {
		return "", fmt.Errorf("启动求解器失败: %w", err)
	}

	// ⚠ 只读 **stdout**：调试信息走 stderr（`[pe-stall] …`）。
	// 合并两者会被调试信息抢先 —— 我因此误判过一次"求解器不可用"。
	var param string
	sc := bufio.NewScanner(stdout)
	sc.Buffer(make([]byte, 0, 64*1024), 1<<20)
	for sc.Scan() {
		line := strings.TrimSpace(sc.Text())
		if v, ok := strings.CutPrefix(line, "VERIFY_PARAM="); ok {
			param = strings.TrimSpace(v)
		}
	}

	waitErr := cmd.Wait()
	stderrText := strings.TrimSpace(stderr.String())

	if param != "" {
		// 成功：清掉冷却
		s.mu.Lock()
		s.cooldownUntil = time.Time{}
		s.cooldownReason = ""
		s.mu.Unlock()
		return param, nil
	}

	// 失败：`stall`/`timeout` 属**求解器自身**的失速（非上游限流）。
	//
	// # ⚠ 冷却**不在这里设**（2026-09-21 修正）
	//
	// 失速是概率性的，`Solve` 会在外层立刻重试（见 captchaSolveAttempts）。
	// 若在这里就设 2 分钟冷却，重试会被冷却检查直接挡掉 ——
	// 那等于"第一次失败就放弃"，与重试的目的相矛盾。
	//
	// 故本函数只**如实报告**错误，把"要不要熔断"交给 `Solve` 决定
	//（它在所有尝试都用尽后才设冷却）。
	if isCaptchaStall(stderrText) {
		return "", fmt.Errorf("求解器失速（SDK 启动超时，非上游限流）：%s",
			firstLine(stderrText))
	}

	msg := firstLine(stderrText)
	if msg == "" && waitErr != nil {
		msg = waitErr.Error()
	}
	return "", fmt.Errorf("验证码求解失败：%s", msg)
}

// CooldownLeft 距离冷却结束还有多久（0 = 不在冷却）。
func (s *CaptchaSolver) CooldownLeft() time.Duration {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.cooldownUntil.IsZero() {
		return 0
	}
	if d := time.Until(s.cooldownUntil); d > 0 {
		return d.Round(time.Second)
	}
	return 0
}

func firstLine(s string) string {
	if i := strings.IndexByte(s, '\n'); i >= 0 {
		s = s[:i]
	}
	return strings.TrimSpace(s)
}
