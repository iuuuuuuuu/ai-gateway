// config.go 加载 JSON 配置 + 环境变量覆盖。
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"strconv"
	"strings"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/prompt"
	"workbuddy2api/internal/records"
	"workbuddy2api/internal/upstream"
)

// Config 顶层配置。
type Config struct {
	Listen    string `json:"listen"`     // ":7863"
	APIKey    string `json:"api_key"`    // 空 = 不鉴权
	AuthDir   string `json:"auth_dir"`   // ./auths
	StateFile string `json:"state_file"` // ./data/state.json

	Cooldown struct {
		// hard_credit / err_threshold / err_cooldown 三个历史键已退役：
		// 硬冷却固定为次日 04:00（CooldownUntilTomorrow4AM），连续错误语义并入熔断器。
		// 旧 config 中的这些键因 JSON 未知字段而自然忽略，不报错。
		SoftRate string `json:"soft_rate"` // "60s"
	} `json:"cooldown"`

	Schedule struct {
		CheckinHours   []int `json:"checkin_hours"`   // [9,21]
		KeepaliveHours []int `json:"keepalive_hours"` // [22]
		// ActivityHours 活跃上报时点，默认 [10]。
		//
		// 签到只恢复余额；**连登天数**与领养资格靠对话活跃上报点亮
		//（一条 chat_request_send 同时点亮连登 + 解锁 first_buddy 任务）。
		ActivityHours []int `json:"activity_hours"`
		// NightOwlHours 夜猫子任务时点，默认 [1]。
		//
		// growth 有个时段敏感任务只在夜猫窗口（23:00–08:00 CST）内计入，
		// 故单独排一个落在窗口内的时点（01 点避开 22 点的 token 保活）。
		// 窗口外执行会被自动跳过，手工触发也不会做无用请求。
		NightOwlHours []int `json:"nightowl_hours"`
		// SchoolHours 开学季活动任务时点，默认 [12]。该活动限时，
		// 服务端下发 in_period，下线后自动跳过；只领取已达标的奖励，不伪造完成动作。
		SchoolHours []int `json:"school_hours"`
		// TrialHours 国际版 trial 加油包领取时点，默认 [9, 21]（与签到同步）。
		// 国际版没有签到/任务中心，trial 是其唯一积分增益动作；幂等可每天重试。
		TrialHours []int `json:"trial_hours"`
	// ProductTasksHours Qoder/ZCode 日常任务的自动执行时点，默认 [10]。
	//
	// # 为什么是"每天一次"而不是参考实现的"每 5 分钟"
	//
	// 参考实现（TriDefender/zcode-api）每 5 分钟探测一次，因为它要**抢**
	// 限量套餐（先到先得）。而所有者的诉求是「任务也应该自动执行」
	//（别让我每天手点）—— 那不需要抢：幂等任务每天做一次就够，
	// 高频只会扩大风控面。
	//
	// 两个端点都幂等：Qoder 已领回 `replayed:true`、ZCode 回 `1003`。
	// 故"重复执行"的最坏情况只是"今天已经领过了"。
		// CheckinEnabled/KeepaliveEnabled/ActivityEnabled 显式禁用开关（缺省 true）。
		//
		// 为什么用独立 bool 而不是空数组/哨兵值表意"禁用"：
		//   - 空数组与 null 在老语义里已被"未配置 → 回落默认"占用，改判会静默翻转
		//     所有老 config 的行为（用户只想删掉一行，结果关掉了签到）；bool 缺省 true
		//     则对老配置零影响，向后完全兼容。
		//   - 开关与取值解耦：禁用时仍保留用户显式配的小时，重新启用无需补配。
		//   - 无需猜测哨兵（[-1] 之类），非法小时一律报错并提示改用本开关。
		CheckinEnabled   bool `json:"checkin_enabled"`   // 缺省 true；false = 关签到（旅行随之停）
		KeepaliveEnabled bool `json:"keepalive_enabled"` // 缺省 true；false = 关 token 保活
		ActivityEnabled  bool `json:"activity_enabled"`  // 缺省 true；false = 关活跃上报
		NightOwlEnabled  bool `json:"nightowl_enabled"`  // 缺省 true；false = 关夜猫子任务
		SchoolEnabled    bool `json:"school_enabled"`    // 缺省 true；false = 关开学季活动
		TrialEnabled     bool `json:"trial_enabled"`     // 缺省 true；false = 关 trial 领取
		// ProductTasksEnabled Qoder/ZCode 日常任务的**自动执行**（缺省 true）。
		//
		// ⚠⚠ 已于 2026-09-22 **拆分**（所有者要求：「权益自动领取 qoder zcode
		// 拆分开,不要合成一个」）。现在请用下面两个**分产品**的开关：
		//
		//	QoderClaimEnabled  Qoder 权益活动自动领取
		//	ZcodeClaimEnabled  ZCode 套餐/权益自动领取
		//
		// 本字段**保留为兼容用的总闸**：任一新产品开关**未显式配置**时回落到它。
		// 这样老配置（只有 `product_tasks_enabled`）行为完全不变，
		// 而新界面写的是分产品开关。
		//
		// ⚠ 不要删掉它 —— 删了会让所有存量配置的"自动领取"在某次升级后
		// 静默失效（缺键 ⇒ 零值 false ⇒ 不领 ⇒ 活动过期作废）。
		//
		// # 默认开的理由（所有者 2026-09-20 要求）
		//
		// 原话：「qoder这个活动卡片…而且任务也应该自动执行」、
		//       「他那个仓库还有个自动领取那个积分包的功能，我们也要接进来」。
		//
		// 即"别让我每天手点"。默认关等于没做。
		//
		// # 与"不做自动抢"的关系
		//
		// `internal/zcode/claim.go` 里写着「不做定时自动抢 —— 会让账号表现出
		// 非人类的活动模式」，那条结论**仍然成立**：它反对的是**抢**
		//（高频探测 + 争限量名额）。而本项是**做**：
		//
		//	抢：每 5 分钟探测、失败重试   ← 非人类画像
		//	做：每天一次、零重试、端点幂等 ← 人类也会每天点一下
		//
		// 两个端点都幂等（Qoder `replayed:true` / ZCode `1003 already_claimed`），
		// 故最坏情况只是"今天已经领过了"。
		ProductTasksEnabled bool `json:"product_tasks_enabled"`
		// QoderClaimEnabled Qoder 权益活动**自动领取**（缺省继承总闸）。
		//
		// 用 `*bool` 而非 `bool`：需要区分「没配」与「配了 false」——
		// 前者要回落到总闸，后者是用户明确要关。
		// 用值类型的话缺键 = false，会把老配置全变成"关闭"。
		QoderClaimEnabled *bool `json:"qoder_claim_enabled"`
		// ZcodeClaimEnabled ZCode 套餐**自动领取**（缺省继承总闸）。
		//
		// 同上用 `*bool` 区分「没配」与「显式关」。
		ZcodeClaimEnabled *bool `json:"zcode_claim_enabled"`
		// QoderClaimHours Qoder 权益活动的**每日领取时点**（本地时间，24 小时制）。
		//
		// # 为什么要有它（2026-09-22 所有者要求）
		//
		// 原话：「qoder改为 早十点,晚九点 两次触发,防止错漏」。
		//
		// 活动每天 10:00（UTC+8）重置，只领一次的话——某一轮网络抖动、
		// 上游 5xx、或恰好在重置前跑过——当天就**领不到了**（额度作废）。
		// 两个时点互相兜底：早上那轮失败，晚上 21:00 还有一次机会。
		//
		// 缺省 `[10, 21]`（见 Default）；空数组 = 不做时点制领取
		//（那时只靠下面 `QoderClaimIntervalMinutes` 的轮询）。
		QoderClaimHours []int `json:"qoder_claim_hours"`
		// QoderClaimIntervalMinutes Qoder 自动领取的**轮询间隔**（分钟），0 = 只用上面两个时点。
		//
		// 为什么要保留轮询而不纯靠时点：时点制在两个整点之间完全不动，
		// 若用户在 10:30 打开软件、那时还没领到（比如 10:00 那轮失败），
		// 就要**干等到 21:00**。轮询让它在下一轮就补上。
		//
		// 缺省 20（与宿主侧 `PATROL_INTERVAL` 一致），此时点制是额外保障。
		QoderClaimIntervalMinutes int `json:"qoder_claim_interval_minutes"`
		// ActivityReportCount 每号每日上报条数，默认 3。
		//
		// 取 3 而非 1：单条上报偶发被服务端丢弃（缺 userId 时 200 但静默丢弃），
		// 多条提高点亮成功率；也不宜过多，避免被风控当成异常流量。
		ActivityReportCount int `json:"activity_report_count"`
		// CheckinScope 签到 + 猫猫旅行 + 活跃上报覆盖的账号区域："cn"（缺省，仅国服）/ "all"。
		//
		// 为什么默认只做国服：国际版（workbuddy.ai）的 billing 与 growth 接口目前
		// 不返回真实数据（签到状态恒 active=false，travel/status 恒为空 data），
		// 对国际版账号执行只会产生无意义的失败日志。keepalive 不在此范围内
		//（token 刷新对两个区域都有效且必要）。
		CheckinScope string `json:"checkin_scope"`
		// 猫猫旅行已退役 travel_interval_minutes：派猫合并到签到时点执行（见 scheduler.RunCheckinNow）。
		// 旧 config 里的该键因 JSON 未知字段而自然忽略，不报错。
	} `json:"schedule"`

	Upstream struct {
		// TimeoutSeconds 短 RPC（refresh/checkin/balance/FetchModels）总时长上限，默认 120。
		TimeoutSeconds int `json:"timeout_seconds"`
		// HeaderTimeoutSeconds 聊天 SSE 首字节前（响应头）上限；<=0 回落 TimeoutSeconds。
		HeaderTimeoutSeconds int `json:"header_timeout_seconds"`
		// IdleTimeoutSeconds 聊天 SSE 流中空闲上限（活跃吐数据续命不掐）；<=0 回落默认 300。
		IdleTimeoutSeconds int `json:"idle_timeout_seconds"`
	} `json:"upstream"`

	Features struct {
		// SanitizeBlacklistFingerprints 出站请求体黑名单指纹脱敏（默认 true；false 完全还原）。
		SanitizeBlacklistFingerprints bool `json:"sanitize_blacklist_fingerprints"`
	} `json:"features"`

	// Prompt 系统提示词替换（个性化提示词）。
	//
	// 与 features.sanitize_blacklist_fingerprints 是**两层叠加、互不替代**：
	// sanitize 清洗 user/assistant 消息里的指纹串，本块则把客户端的
	// system/developer 消息**整体替换**掉，从源头消灭 system 来源的指纹误报。
	Prompt struct {
		// Mode 缺省 "passthrough" = 透传客户端原始 system（**既有行为不变**）；
		// "custom" = 用网关自有提示词替换客户端的 system/developer 消息。
		//
		// 为什么缺省是 passthrough 而不是 custom：这是**新增能力**，老配置里
		// 没有这个键。若缺省 custom，所有既有用户升级后 system 会被静默换掉 ——
		// 人设、项目约定、工具说明全丢，且从请求上看不出是网关动的手。
		// 保守缺省 + 显式开启，用户改配置时才知道自己换掉了什么。
		Mode string `json:"mode"`
		// File 自定义提示词文件路径（**绝对路径**最稳妥，相对路径以网关工作目录为基准）。
		// 空 = 用内置默认提示词（internal/prompt/defaultprompt.md）。
		//
		// 路径非空但不可读 / 内容为空 → 启动即报错（fail fast）：
		// 用户明确配了文件却读不到时静默回落别的文本，现象是「配了却像没配」，
		// 排查成本极高。注意该限制只作用于 custom 模式 —— passthrough 下
		// 即使 file 填错也不该拦住启动（那段文本根本不会被使用）。
		File string `json:"file"`
	} `json:"prompt"`

	Upstash struct {
		URL   string `json:"url"`   // 空 = 纯内存模式；支持完整 rediss:// URL 或 https://xxx.upstash.io host
		Token string `json:"token"` // url 非完整连接串时用于组装 rediss://default:<token>@<host>:6379
	} `json:"upstash"`

	Pool struct {
		MaxInFlight        int     `json:"max_in_flight"`        // 单账号最大在途请求数，0 = 不限
		BreakerThreshold   int     `json:"breaker_threshold"`    // 连续失败次数触发熔断，默认 3
		BreakerCooldown    string  `json:"breaker_cooldown"`     // 基础熔断时长，默认 "30m"
		BreakerCooldownMax string  `json:"breaker_cooldown_max"` // 指数退避封顶，默认 "6h"
		IdleWeightPerHour  float64 `json:"idle_weight_per_hour"` // 闲置补偿：每小时未用 +0.5 权重
		IdleWeightMax      float64 `json:"idle_weight_max"`      // 闲置补偿封顶，默认 5.0
		// CreditRefreshInterval 积分到期巡检周期（duration 字符串，默认 "15m"）。
		// 巡检只刷新余额与到期日，不签到、不改账号状态，用于驱动到期分层选号。
		CreditRefreshInterval string `json:"credit_refresh_interval"`
		// CreditRefreshEnabled 积分到期巡检开关（缺省 true）。
		CreditRefreshEnabled bool `json:"credit_refresh_enabled"`
		// Rotation 是否启用「单一模型 + 积分轮转」模式（缺省 false = 负载均衡）。
		//
		// 语义差异：负载均衡在最早到期的那一档**内部分摊**（多号并行承接流量）；
		// 轮转则**串行烧号** —— 始终只用一个账号，把它烧到不可用才换下一个，
		// 换的仍是按到期日排序的下一个。见 pool.PickForModel / SetRotation。
		//
		// 缺省 false 保证老配置行为不变；该字段由宿主写入网关配置。
		Rotation bool `json:"rotation"`
		// AllowedModels 「限制使用的模型」白名单（多选）。
		//
		// 非空时网关**只放行名单内的模型**，其余一律 400 model_not_allowed；
		// 空（默认）= 不限制。三个工作模式（自动 / 手动 / 积分轮转）共用同一份
		// 名单 —— 它限制的是「放行哪些模型」，与「用哪些账号」是正交的两件事。
		//
		// 轮转模式尤其需要它：轮转的语义是「把这个账号的某个模型额度烧干净再
		// 换号」，模型是策略的一部分，不限制的话客户端换个模型就能绕过轮转与
		// 额度控制，也让「当前烧的是哪个模型」变得不可预期。
		//
		// 类型是 AllowedModels —— 它的 UnmarshalJSON 让同一个 JSON 键
		// `allowed_model` **既接受字符串也接受数组**：老配置写的是
		// `"deepseek-v4.1-flash"`，新宿主写的是 `["a","b"]`，两者都必须能读。
		AllowedModels AllowedModels `json:"allowed_model"`

		// MultiProduct 是否启用**多产品路由**（默认 false = 只跑 WorkBuddy）。
		//
		// 开启后：
		//   · 加载 Qoder 凭证进同一个账号池（见 QoderAuthDir）
		//   · 注入 Qoder 的请求派发实现（按账号所属产品分发到不同上游）
		//   · 成本维度参与加权（见 pool.SetMultiProduct）
		//
		// 为什么做成开关而不是直接启用：跨产品路由是最容易"悄悄改变现有行为"
		// 的地方，而现有单产品行为已在生产环境跑了很久。关闭时所有既有路径
		// **逐字不变** —— 这也是出问题时的回滚点（关掉即回到已验证行为，
		// 不需要回滚代码）。
		MultiProduct bool `json:"multi_product"`

		// QoderAuthDir Qoder 凭证目录；空 = ~/.wb-switch/qoder/auths。
		//
		// 与宿主侧的 qoder_account::auth_dir() 必须一致，否则会出现
		// "界面里登录成功了但网关看不到账号"。
		QoderAuthDir string `json:"qoder_auth_dir"`

		// ZcodeAuthDir ZCode 凭证目录；空 = ~/.wb-switch/zcode/auths。
		//
		// 同上，必须与宿主侧的 zcode_account::auth_dir() 一致。
		ZcodeAuthDir string `json:"zcode_auth_dir"`

		// ProductModels 各产品**实际可用**的模型清单（由宿主透传）。
		//
		// # 为什么由宿主透传，而不是网关自己去查
		//
		// `/v1/models` 是**请求路径**上的接口 —— 在那里发外部请求会带来
		// 延迟与失败面（每个客户端列一次模型就打一次上游）。
		//
		// 而"某产品的账号能用哪些模型"只有宿主知道：它持有账号库、
		// 在刷新账号时已经查过并按 uid 缓存了（见 zcode/qoder 的
		// `refresh_account`）。故宿主把**汇总后的清单**放进配置，
		// 网关只读 —— 与 `account_records` 的透传方式一致。
		//
		// 形状：`{"qoder":["qwen3.8-max"],"zcode":["glm-5.3"]}`
		//
		// 用途：`/v1/models` 的 `channels` 字段（"这个模型来自哪个平台"）。
		// 某个 id 出现在多个产品的清单里时，它就有多个渠道 ——
		// 这正是界面上要表达的"重叠"。
		ProductModels map[string][]string `json:"product_models"`

		// ModelPlatforms 「模型 → 允许的平台」白名单（由宿主透传）。
		//
		// # 为什么需要（所有者的需求）
		//
		//	「我希望可以加上 **平台区分使用哪个平台的模型**」
		//
		// 同名模型可能同时在多个平台上（实测 `glm-5.2` 在 WorkBuddy 与
		// ZCode 上都有）。选号默认是按权重随机的，用户无法指定
		// "这个模型走 ZCode"。
		//
		// 而他**需要**这个能力，因为各平台额度性质不同：
		//
		//	ZCode 体验套餐：每日重置，**9/23 到期后归零** → 该优先烧掉
		//	WorkBuddy：长期额度
		//
		// # 形状与语义
		//
		//	{"glm-5.2":["zcode"]}           → glm-5.2 只走 ZCode
		//	{"glm-5.2":["zcode","qoder"]}   → 两个都可以
		//	缺这个键 / 值为空                 → 不限制（保持既有行为）
		//
		// ⚠ 白名单内账号全不可用时**回退到全池**，不硬失败
		//（见 pool.platformAllowedLocked 与 pickBestFrom）。
		ModelPlatforms map[string][]string `json:"model_platforms"`

		// ZcodeCaptchaDir ZCode 验证码求解器所在目录（含 solver.js + node_modules）。
		//
		// # 为什么需要它
		//
		// ZCode 对话端点要求 `X-Aliyun-Captcha-Verify-Param`（实测回
		// `400 code=3007 captcha verify failed`，且与模型名、请求头都无关）。
		// 求解器是一个 Node 脚本（用 happy-dom 模拟浏览器跑阿里云官方 SDK），
		// 由宿主作为**发行资源**释放到磁盘并告诉网关路径。
		//
		// # 为什么不是网关自己找
		//
		// 发行包里的资源路径由 Tauri 决定（安装目录下的 resources），
		// 网关无从推断；开发期又指向仓库里的 assets/。由宿主透传最可靠。
		//
		// 空 = 不启用（网关回落到如实报 3007）。
		ZcodeCaptchaDir string `json:"zcode_captcha_dir"`

		// ZcodeCaptchaSolverURL 宿主提供的**外部求解服务**地址
		//（形如 `http://127.0.0.1:51234/solve`）。
		//
		// # 为什么需要它（2026-09-21 所有者提出的方案）
		//
		// 本地求解要起 Node 子进程 + happy-dom **模拟**浏览器，两个硬伤：
		//
		//	① 要求用户机器有 Node（为此外置了 81MB node.exe）
		//	② 模拟环境被风控盯上，实测成功率仅约 40%（靠调 stallMs 提到 88%）
		//
		// 而官方 ZCode 客户端用的是**真实浏览器环境**（已从 app.asar 核实：
		// `script.src = ".../aliyunCaptcha/AliyunCaptcha.js"` +
		// `inst.startTracelessVerification()`）。
		//
		// 宿主自带真实 WebView2（Win10/11 预装，零体积）。实测脚本化调用
		//（`uitest/probe-captcha-in-browser.cjs`，有头 Chrome，无人工点击）：
		//
		//	initAliyunCaptcha +10ms → getInstance +600ms →
		//	success（param 280 字符）**+929ms**
		//
		// 即不到 1 秒，比本地 Node（~3 秒）更快，且不占安装包体积。
		//
		// 配置后**外部优先**，本地 Node 作为回退（外部不可用时仍能工作）。
		ZcodeCaptchaSolverURL string `json:"zcode_captcha_solver_url"`
		// ZcodeCaptchaSolverToken 调外部求解服务时的共享密钥（同机 IPC 鉴权）。
		ZcodeCaptchaSolverToken string `json:"zcode_captcha_solver_token"`

		// ZcodeCaptchaEnabled 是否**主动求解**验证码。
		//
		// # 2026-09-20 反转：默认关闭 → **始终开启**（所有者决定）
		//
		// 原注释写「默认 false…由用户在界面上明确开启后才参与请求」。
		// 但那个"界面开关"**从来不存在**（前端与宿主搜 captcha 均 0 命中），
		// 而实测 ZCode 对话通道对不带验证码的请求一律回
		// `HTTP 400 {"code":3007,"msg":"captcha verify failed"}` ——
		// 即"默认关闭"等于 **ZCode 对话从未成功过**。
		//
		// 所有者原话：
		//
		//	「肯定要默认打开并且不能关闭啊，这是开源软件有什么在乎的？」
		//
		// 求解是在精简环境里跑**阿里云官方 SDK**（不是逆向破解），
		// 账号属于用户自己，开源软件也没有替他保守的立场。故改为恒开。
		//
		// ⚠ 字段保留是为了**向后兼容**：老配置里可能写着 false，
		// 但读取处已不再据此关闭（见 `ZcodeCaptchaActive`），
		// 否则升级后老用户仍然"功能永远关着"。
		ZcodeCaptchaEnabled bool `json:"zcode_captcha_enabled"`

		// WatchAuthDir 是否监听凭证目录、运行期自动热加载（缺省 true）。
		//
		// # 为什么需要它
		//
		// 账号池此前只在**进程启动时**扫一次 auths 目录，于是「在界面上新增一个
		// 账号」不会进池，必须手动重启网关才生效。这是用户实际报过的痛点
		//（宿主侧把这条限制写进了注释，见 crates/ai-gateway-core 的 switch_mode）。
		// 打开后目录内容一变就重新对齐账号池，无需重启。
		//
		// # 为什么缺省 true
		//
		// 这是**修缺陷**而不是加可选能力：不打开就等于保留原缺陷，用户仍然要
		// 手动重启。它也不改变任何既有语义 —— 热加载复用 SyncToDir/upsertLocked，
		// 对已存在账号只换凭证、保留 credits/冷却/熔断/统计（幂等，有单测钉住）。
		// 老配置没有这个键 → 键缺席保留默认 true，行为只会变得更好。
		//
		// 需要关掉的场景：把凭证目录放在网络盘/同步盘上，轮询会带来无谓 IO；
		// 或想完全锁死「运行期账号集合」以便复现问题。
		WatchAuthDir bool `json:"watch_auth_dir"`

		// WatchAuthDirInterval 目录轮询周期（duration 字符串，默认 "5s"）。
		//
		// 为什么是轮询而不是 fsnotify：见 internal/pool/watch.go 的包注释
		//（零新依赖 + 容器/网络文件系统上不丢事件）。周期可配是因为它与
		// 「目录所在介质的 IO 成本」强相关，而默认 5s 只对本地盘是最优。
		//
		// 空/非法一律回落默认（不报错）：与 credit_refresh_interval 同一口径，
		// 避免一个调优键把网关拦停。
		WatchAuthDirInterval string `json:"watch_auth_dir_interval"`
	} `json:"pool"`

	// Proxy 出站 HTTP 代理，形如 "http://127.0.0.1:7890"（缺省空 = 不用显式代理）。
	//
	// 为什么需要：国际版（workbuddy.ai）在国内直连不稳定（实测 wsarecv 超时），
	// 走代理才稳。宿主会把「设置 → 更新代理」里已填的地址复用到此处，
	// 用户无需配两遍。
	//
	// 注意 Go 的 http.ProxyFromEnvironment **只读环境变量**、不读 Windows 注册表，
	// 所以「浏览器能走系统代理」不代表网关也能 —— 必须显式配置。
	//
	// 地址本身与区域无关：**是否真的使用它**由下面的 ProxyScope 按区域决定。
	Proxy string `json:"proxy"`

	// ProxyScope 代理的适用范围（三个独立开关中的**网关侧两个**）。
	//
	// 为什么把「一个地址」拆成按区域的两个开关：同一个代理对两个区域的收益完全
	// 相反 —— 国际版（workbuddy.ai）国内直连实测 wsarecv 超时，必须走代理；
	// 国服（copilot.tencent.com / codebuddy.cn）直连即通，绕进代理只会多一跳延迟、
	// 多一个故障面（代理一挂，本来好好的国服账号跟着不可用）。
	//
	// 为什么没有 github：检查更新 / 下载安装包是**宿主**的活，网关根本不发往
	// github.com 的请求。多一个永远不会被读的键只会误导排查。
	//
	// 缺省值见 Default()：cn=false / intl=true，**不是**「全开」。
	// 理由：本次改动之前国服走的是 ProxyFromEnvironment（**不吃**显式代理），
	// 只有国际版吃。若把缺省当成「全开」，所有既有用户升级后国服会被**新绕进**
	// 显式代理 —— 那正是上一轮明确要消除的行为。缺省取「国际版开、国服关」
	// 才能保证升级前后逐字一致（详见 upstream.SetProxyScope 的注释）。
	ProxyScope struct {
		// CN 国服账号的出站请求是否使用该代理（缺省 false = 直连）。
		CN bool `json:"cn"`
		// Intl 国际版账号的出站请求是否使用该代理（缺省 true）。
		Intl bool `json:"intl"`
	} `json:"proxy_scope"`

	SessionSticky struct {
		Enabled    bool   `json:"enabled"`     // 默认 true
		TTL        string `json:"ttl"`         // 会话绑定 TTL，默认 "30m"
		GCInterval string `json:"gc_interval"` // 会话 GC 周期，默认 "5m"
	} `json:"session_sticky"`

	// AccountRecords 账号记录回写目标（由宿主透传）。
	//
	// 为什么要宿主给路径而不是网关自己算：网关的工作目录是宿主指定的
	// gateway/ 子目录，而账号记录由宿主写在数据目录根下。两边各自推算
	// （宿主看 AI_GATEWAY_HOME，网关看 cwd）迟早会算出不同的值，
	// 而那种错法表现为「任务跑了但界面上没有记录」，几乎无法从现象定位。
	//
	// 整个块缺席（老宿主没写这个键）时 File 为空 → 不记录：
	// 独立运行 gateway.exe 的场景下这是正确行为，不该报错也不该刷日志。
	AccountRecords struct {
		// File account_records.json 的**绝对路径**。
		File string `json:"file"`
		// RetentionDays 记录保留天数，与宿主设置同一口径
		//（避免「界面说保留 60 天、网关按 7 天清」这类不一致）。
		RetentionDays int `json:"retention_days"`
		// Identities 账号身份映射：网关手上只有 uid，而界面按账号库的 id 过滤记录。
		Identities []struct {
			UID  string `json:"uid"`
			ID   string `json:"id"`
			Name string `json:"name"`
		} `json:"identities"`
	} `json:"account_records"`

	// 解析后
	SoftRateDur         time.Duration `json:"-"`
	BreakerCooldownDur  time.Duration `json:"-"`
	BreakerCooldownMaxD time.Duration `json:"-"`
	SessionTTL          time.Duration `json:"-"`
	SessionGCInterval   time.Duration `json:"-"`
	// CreditRefreshIntervalD 解析后的积分到期巡检周期。
	CreditRefreshIntervalD time.Duration `json:"-"`
	// WatchAuthDirIntervalD 解析后的凭证目录轮询周期（WatchAuthDir 开启时生效）。
	WatchAuthDirIntervalD time.Duration `json:"-"`
	// PromptText custom 模式下**解析后**的系统提示词文本（passthrough 下恒空）。
	//
	// 在 normalize 阶段一次性读盘并缓存，而不是每个请求现读文件：
	// 请求路径上做文件 IO 会引入可避免的延迟与失败面，且运行期改文件
	// 本该由「改配置 + 重启」承载，语义更清晰（也避免读到写了一半的文件）。
	PromptText string `json:"-"`
}

// AllowedModels 「限制使用的模型」白名单。
//
// 为什么需要自定义类型而不是直接用 []string：这个键在配置文件里的历史形状是
// **字符串**（`"allowed_model": "deepseek-v4.1-flash"`），新宿主写的是**数组**。
// 直接把字段声明成 []string 会让所有老配置在解析阶段就失败 ——
// `json: cannot unmarshal string into Go struct field ... of type []string` ——
// 表现为「升级后网关直接起不来」，是最严重的一类向后兼容事故。
//
// 自定义 UnmarshalJSON 是唯一能同时吃下两种形状的写法。
type AllowedModels []string

// UnmarshalJSON 同时接受字符串与字符串数组；null / 空串 → 空（= 不限制）。
//
// 其余形状（数字、对象）按类型错误上报，不静默吞掉：用户把
// `"allowed_model": 123` 写进配置时，明确报错比「静默当成不限制」安全得多
// —— 后者会让用户以为限制生效了，实际网关放行一切。
func (a *AllowedModels) UnmarshalJSON(data []byte) error {
	trimmed := strings.TrimSpace(string(data))
	if trimmed == "" || trimmed == "null" {
		*a = nil
		return nil
	}
	// 数组形状：正常解码，元素里的空白与空串交给 server 侧归一化统一处理
	//（那里已经有一份 normalizeAllowedModels，两边各写一套必然分叉）。
	if strings.HasPrefix(trimmed, "[") {
		var list []string
		if err := json.Unmarshal(data, &list); err != nil {
			return fmt.Errorf("pool.allowed_model: %w", err)
		}
		*a = list
		return nil
	}
	// 字符串形状（老配置）：空串视为「不限制」而不是「一个叫空串的模型」。
	var single string
	if err := json.Unmarshal(data, &single); err != nil {
		return fmt.Errorf("pool.allowed_model: 需要字符串或字符串数组: %w", err)
	}
	if strings.TrimSpace(single) == "" {
		*a = nil
		return nil
	}
	*a = []string{single}
	return nil
}

// RecordIdentities 把配置里的账号身份映射转成 records 包需要的形状
//（uid → 宿主的 id 与展示名）。
//
// 抽成方法而不是在 main 里内联转换：identities 是切片结构体，
// 内联转换会在 main 里引入一个与 records 包重复的匿名类型。
func (c *Config) RecordIdentities() map[string]records.Identity {
	out := make(map[string]records.Identity, len(c.AccountRecords.Identities))
	for _, item := range c.AccountRecords.Identities {
		if item.UID == "" {
			continue
		}
		out[item.UID] = records.Identity{ID: item.ID, Name: item.Name}
	}
	return out
}

// Default 默认配置。
func Default() *Config {
	c := &Config{
		Listen:    ":7863",
		APIKey:    "",
		AuthDir:   "./auths",
		StateFile: "./data/state.json",
	}
	c.Cooldown.SoftRate = "60s"
	c.Schedule.CheckinHours = []int{9, 21}
	c.Schedule.KeepaliveHours = []int{22}
	c.Schedule.ActivityHours = []int{10}
	c.Schedule.NightOwlHours = []int{1}
	c.Schedule.SchoolHours = []int{12}
	c.Schedule.TrialHours = []int{9, 21}
	c.Schedule.ProductTasksEnabled = true
	// Qoder 自动领取的两个时点（2026-09-22 所有者要求）：
	//	「qoder改为 早十点,晚九点 两次触发,防止错漏」
	//
	// 活动每天 10:00（UTC+8）重置 ⇒ 10 点是重置后第一轮；
	// 21 点兜底 —— 早上那轮若因网络/上游抖动失败，晚上还能领到。
	c.Schedule.QoderClaimHours = []int{10, 21}
	// 时点之外每 20 分钟补一轮（与宿主 `PATROL_INTERVAL` 对齐）。
	// 理由见字段注释：纯时点制会让"10:00 失败"的用户干等到 21:00。
	c.Schedule.QoderClaimIntervalMinutes = 20
	// ⚠ QoderClaimEnabled / ZcodeClaimEnabled 刻意**不在这里赋默认值**：
	// 它们是 `*bool`，nil = "没配" ⇒ 由 QoderClaimOn()/ZcodeClaimOn()
	// 回落到 ProductTasksEnabled。若在这里赋 &true，就再也分不清
	// "用户显式开着" 与 "没配"，分产品开关会失去意义。
	// 开关「缺省 true」靠这几行实现：Load 先取 Default() 再 json.Unmarshal 覆盖，
	// 键缺席（或为 null）时字段原样保留 true，只有显式 false 才关。
	c.Schedule.CheckinEnabled = true
	c.Schedule.KeepaliveEnabled = true
	c.Schedule.ActivityEnabled = true
	c.Schedule.NightOwlEnabled = true
	c.Schedule.SchoolEnabled = true
	c.Schedule.TrialEnabled = true
	c.Schedule.ActivityReportCount = 3
	c.Schedule.CheckinScope = "cn"
	c.Upstream.TimeoutSeconds = 120
	// HeaderTimeoutSeconds/IdleTimeoutSeconds 默认 0（未设置态），回落见 normalize()。
	c.Upstream.HeaderTimeoutSeconds = 0
	c.Upstream.IdleTimeoutSeconds = 0
	c.Features.SanitizeBlacklistFingerprints = true
	// 缺省 passthrough：透传客户端原始 system。老配置没有 prompt 块，
	// 必须保持既有行为不变（见 Prompt.Mode 的注释）。
	c.Prompt.Mode = "passthrough"
	// 单账号最大在途请求数。**三个产品统一用 3**（2026-09-22 所有者指定）。
	//
	// # 值的演变（每次都是实测驱动，别再凭感觉改）
	//
	//	3  → 32 → 8 → 16 → **3**（本版）
	//
	//	· 最初的 3：没有排队机制时，客户端并发重试（DSH 的 pi-ai 默认 5 次）
	//	  的第 4、5 个会被直接拒 ⇒ 用户看到「每次都失败」。
	//	· 32：**恰好压在 qoder 上游的并发天花板（≈30）上** ⇒ 一重试就越界，
	//	  报「上游服务异常（HTTP 503）」。**把上限设成等于上游能力是错的**。
	//	· 8 / 16：实测 16 时并发 20 全通过。
	//
	// # ⚠ 为什么现在敢回到 3
	//
	// 关键变化：**名额满时不再直接拒绝，而是排队等待**
	//（`server.Config.InFlightWait`，默认 3000ms；见
	// `pool.AcquireWait`）。当初 3 会失败，是因为满了就 `Acquire` 失败、
	// 立刻回 503 —— 现在超额的请求会等前一个完成，通常几十毫秒就拿到名额。
	//
	// 所以「3」现在的含义是"每个账号同时最多跑 3 个"，而**不是**
	// "第 4 个请求就失败"。这两个语义差别是本值能回到 3 的前提。
	//
	// # 为什么三个产品统一
	//
	// 所有者原话：「还有单个账号并发还是改为3个,qoder workbuddy zcode都一样」。
	// 统一的好处：行为可预期、排查时不用记"哪个产品是多少"。
	// 多账号产品不受影响 —— 19 个 WorkBuddy 账号各自 3 ⇒ 总并发 57。
	//
	// ⚠ 若某天又出现"并发重试成片 503"，先确认 `InFlightWait > 0`
	//（0 表示关闭等待，等于退回旧行为），再考虑调这个值。
	//
	// 详见 `crates/ai-gateway-core/src/modules/gateway.rs` 同名键的注释。
	c.Pool.MaxInFlight = 3
	c.Pool.BreakerThreshold = 3
	c.Pool.BreakerCooldown = "30m"
	c.Pool.BreakerCooldownMax = "6h"
	c.Pool.IdleWeightPerHour = 0.5
	c.Pool.IdleWeightMax = 5.0
	// 积分到期巡检「缺省 true」同签到开关：键缺席保留默认，显式 false 才关。
	c.Pool.CreditRefreshEnabled = true
	c.Pool.CreditRefreshInterval = "15m"
	// 凭证目录热加载「缺省 true」：这是修缺陷（不打开 = 新增账号仍要手动重启），
	// 且不改动任何既有语义（见 Pool.WatchAuthDir 的注释）。
	c.Pool.WatchAuthDir = true
	c.Pool.WatchAuthDirInterval = "5s"
	c.SessionSticky.Enabled = true
	c.SessionSticky.TTL = "30m"
	c.SessionSticky.GCInterval = "5m"
	// 代理适用范围缺省值：**国际版开、国服关**（逐字保持改动前的行为）。
	//
	// 这里**刻意不是**「全 true」。Load 先取 Default() 再 json.Unmarshal 覆盖，
	// 所以老配置（没有 proxy_scope 键）保留的就是这两个值 —— 而改动前国服走的
	// 是 ProxyFromEnvironment、并不吃显式代理。若缺省成 true，升级后所有既有
	// 用户的国服流量会被新绕进代理（与「别让国内也走代理流量」的要求相反）。
	// 同理不能缺省成 false：那会让国际版代理静默失效，国内直连直接超时。
	c.ProxyScope.CN = false
	c.ProxyScope.Intl = true
	return c
}

// Load 从文件读，再用 WB2A_* env 覆盖。
func Load(path string) (*Config, error) {
	c := Default()
	if path != "" {
		raw, err := os.ReadFile(path)
		if err != nil {
			return nil, fmt.Errorf("read config: %w", err)
		}
		if err := json.Unmarshal(raw, c); err != nil {
			return nil, fmt.Errorf("parse config: %w", err)
		}
	}
	applyEnv(c)
	if err := c.normalize(); err != nil {
		return nil, err
	}
	return c, nil
}

func applyEnv(c *Config) {
	if v := os.Getenv("WB2A_LISTEN"); v != "" {
		c.Listen = v
	}
	if v := os.Getenv("WB2A_API_KEY"); v != "" {
		c.APIKey = v
	}
	if v := os.Getenv("WB2A_AUTH_DIR"); v != "" {
		c.AuthDir = v
	}
	if v := os.Getenv("WB2A_STATE_FILE"); v != "" {
		c.StateFile = v
	}
	if v := os.Getenv("WB2A_SOFT_RATE"); v != "" {
		c.Cooldown.SoftRate = v
	}
	if v := os.Getenv("WB2A_TIMEOUT_SECONDS"); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			c.Upstream.TimeoutSeconds = n
		}
	}
	if v := os.Getenv("WB2A_HEADER_TIMEOUT_SECONDS"); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			c.Upstream.HeaderTimeoutSeconds = n
		}
	}
	if v := os.Getenv("WB2A_IDLE_TIMEOUT_SECONDS"); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			c.Upstream.IdleTimeoutSeconds = n
		}
	}
	if v := os.Getenv("WB2A_SANITIZE_FINGERPRINTS"); v != "" {
		if b, err := strconv.ParseBool(v); err == nil {
			c.Features.SanitizeBlacklistFingerprints = b
		}
	}
	// 系统提示词替换：与参考实现同名（WB2A_PROMPT_*），便于两边配置互通。
	if v := os.Getenv("WB2A_PROMPT_MODE"); v != "" {
		c.Prompt.Mode = v
	}
	if v := os.Getenv("WB2A_PROMPT_FILE"); v != "" {
		c.Prompt.File = v
	}
}

func (c *Config) normalize() error {
	var err error
	if c.SoftRateDur, err = time.ParseDuration(c.Cooldown.SoftRate); err != nil {
		return fmt.Errorf("cooldown.soft_rate: %w", err)
	}
	if c.BreakerCooldownDur, err = time.ParseDuration(c.Pool.BreakerCooldown); err != nil {
		return fmt.Errorf("pool.breaker_cooldown: %w", err)
	}
	if c.BreakerCooldownMaxD, err = time.ParseDuration(c.Pool.BreakerCooldownMax); err != nil {
		return fmt.Errorf("pool.breaker_cooldown_max: %w", err)
	}
	if c.SessionTTL, err = time.ParseDuration(c.SessionSticky.TTL); err != nil {
		return fmt.Errorf("session_sticky.ttl: %w", err)
	}
	if c.SessionGCInterval, err = time.ParseDuration(c.SessionSticky.GCInterval); err != nil {
		return fmt.Errorf("session_sticky.gc_interval: %w", err)
	}
	if c.Pool.BreakerThreshold <= 0 {
		c.Pool.BreakerThreshold = 3
	}
	if c.Pool.IdleWeightPerHour <= 0 {
		c.Pool.IdleWeightPerHour = 0.5
	}
	if c.Pool.IdleWeightMax <= 0 {
		c.Pool.IdleWeightMax = 5.0
	}
	// 积分到期巡检周期：空/非法一律回落默认（不报错，避免老 config 启动失败）。
	if c.CreditRefreshIntervalD, err = time.ParseDuration(c.Pool.CreditRefreshInterval); err != nil ||
		c.CreditRefreshIntervalD <= 0 {
		c.CreditRefreshIntervalD = 15 * time.Minute
	}
	// 凭证目录轮询周期：同上一口径（空/非法回落默认，不拦启动）。
	if c.WatchAuthDirIntervalD, err = time.ParseDuration(c.Pool.WatchAuthDirInterval); err != nil ||
		c.WatchAuthDirIntervalD <= 0 {
		c.WatchAuthDirIntervalD = 5 * time.Second
	}
	if c.Upstream.TimeoutSeconds <= 0 {
		c.Upstream.TimeoutSeconds = 120
	}
	// header 缺省回落 timeout（保"首字节前换号"既有语义）；idle 缺省走内置大值。
	// 任务书约定：0 一律视为"未设置"走默认，真正的"禁用"留待后续（避免歧义）。
	if c.Upstream.HeaderTimeoutSeconds <= 0 {
		c.Upstream.HeaderTimeoutSeconds = c.Upstream.TimeoutSeconds
	}
	if c.Upstream.IdleTimeoutSeconds <= 0 {
		c.Upstream.IdleTimeoutSeconds = 300
	}
	if !strings.HasPrefix(c.Listen, ":") && !strings.Contains(c.Listen, ":") {
		c.Listen = ":" + c.Listen
	}
	// 空数组与 null 反序列化后覆盖掉 Default() 的排程值（键缺席才保留），在此补齐。
	// 空 = 未配置 → 回落默认；「禁用」一律走 *_enabled=false，两者互不混淆。
	if len(c.Schedule.CheckinHours) == 0 {
		c.Schedule.CheckinHours = []int{9, 21}
	}
	if len(c.Schedule.KeepaliveHours) == 0 {
		c.Schedule.KeepaliveHours = []int{22}
	}
	if len(c.Schedule.ActivityHours) == 0 {
		c.Schedule.ActivityHours = []int{10}
	}
	// 这三个此前被误写在 ActivityHours 的 if 里：只有活跃上报为空时才顺带赋值，
	// 于是单独把 nightowl_hours 配成 []/null 会得到空排程 —— nextFire 对空数组
	// 返回零时间，任务被静默关掉（与「未配置 → 回落默认」的约定相反）。
	if len(c.Schedule.NightOwlHours) == 0 {
		c.Schedule.NightOwlHours = []int{1}
	}
	if len(c.Schedule.SchoolHours) == 0 {
		c.Schedule.SchoolHours = []int{12}
	}
	if len(c.Schedule.TrialHours) == 0 {
		c.Schedule.TrialHours = []int{9, 21}
	}
	if c.Schedule.ActivityReportCount <= 0 {
		c.Schedule.ActivityReportCount = 3
	}
	// 签到区域范围：只接受 cn / all，其余（含缺省空串）一律回落 cn。
	c.Schedule.CheckinScope = normalizeCheckinScope(c.Schedule.CheckinScope)

	// Qoder 自动领取的时点/轮询缺省（2026-09-22 拆分时新增）。
	//
	// ⚠ 这里必须再兜一次默认值（Default() 里已有一份）：
	// `Load` 是先 `Default()` 再 `json.Unmarshal` 覆盖 —— 但用户若在配置里
	// 显式写了 `"qoder_claim_hours": []`（空数组），Unmarshal 会把默认值
	// **覆盖成空**。空数组的语义是"不做时点制领取"，那是合法配置，
	// 故这里**不能**把空数组改回 [10,21]（那会让用户关不掉时点制）。
	// 只有 nil（键完全缺席且 Default 也没给）才补默认 —— 实际不会发生，
	// 但保留判断以防将来 Default() 被改动。
	if c.Schedule.QoderClaimHours == nil {
		c.Schedule.QoderClaimHours = []int{10, 21}
	}
	// 轮询间隔：负数是非法值（写错），归一到默认；0 是合法的"只靠时点"。
	if c.Schedule.QoderClaimIntervalMinutes < 0 {
		c.Schedule.QoderClaimIntervalMinutes = 20
	}
	if err := c.validateScheduleHours(); err != nil {
		return err
	}
	if err := c.validateQoderClaimHours(); err != nil {
		return err
	}
	return c.normalizePrompt()
}

// normalizePrompt 校验 prompt.mode，并在 custom 模式下加载提示词文本。
//
// mode 只接受 custom / passthrough（大小写与首尾空白不敏感）；其余值**启动即报错**，
// 而不是静默回落到某一个分支 —— 用户把 "costom" 拼错时，若静默按 passthrough 跑，
// 表现为「按文档配了定制提示词却完全没生效」，是最难排查的一类配置错误。
//
// file 的 fail fast 范围**只限 custom 模式**：
//   - custom：file 非空但不可读 / 内容为空 → 报错（用户明确要用它，读不到就是错）；
//   - passthrough：**不读 file**，因此 file 写错也不会拦住启动 —— 该文本在
//     passthrough 下根本不会被使用，为一段不生效的配置拦住服务启动没有意义，
//     也会让「先填好文件、稍后再切模式」这种正常操作变得不可行。
func (c *Config) normalizePrompt() error {
	switch m := strings.ToLower(strings.TrimSpace(c.Prompt.Mode)); m {
	case "", "passthrough":
		c.Prompt.Mode = "passthrough"
	case "custom":
		c.Prompt.Mode = "custom"
	case "append":
		// 追加模式：客户端规则在前、网关提示词在后（见 config.go 的模式说明）
		c.Prompt.Mode = "append"
	default:
		return fmt.Errorf("prompt.mode: %q 不是合法值（passthrough / custom / append）", c.Prompt.Mode)
	}
	// ⚠ 只有 passthrough 不需要提示词文本。
	//
	// 这里原先是 `!= "custom"`，加 append 后必须改成「排除 passthrough」——
	// 否则 append 模式下 PromptText 会被清空，配置看起来生效了、
	// 实际什么都没追加（静默失效，最难查的一类）。
	if c.Prompt.Mode == "passthrough" {
		c.PromptText = ""
		return nil
	}
	text, err := prompt.Load(strings.TrimSpace(c.Prompt.File))
	if err != nil {
		return err
	}
	c.PromptText = text
	return nil
}

// normalizeCheckinScope 归一化签到区域范围；无法识别时回落 "cn"（仅国服）。
func normalizeCheckinScope(v string) string {
	if strings.EqualFold(strings.TrimSpace(v), "all") {
		return "all"
	}
	return "cn"
}

// checkinScopeAllows 该区域范围是否覆盖此账号。
func (c *Config) checkinScopeAllows(a *auth.Auth) bool {
	if normalizeCheckinScope(c.Schedule.CheckinScope) == "all" {
		return true
	}
	return !upstream.IsIntl(a)
}

// validateScheduleHours 校验排程小时落在 0-23。
//
// 为什么不用 `[-1]` 之类的哨兵值表意"禁用"：非法小时被静默吞掉时，用户以为关掉了签到，
// 实际可能被当成另一个整点照常执行；这里直接快速失败，并在错误信息里指向正确的开关
// （checkin_enabled / keepalive_enabled），避免用户靠猜哨兵值来配。
func (c *Config) validateScheduleHours() error {
	if err := checkHourRange("schedule.checkin_hours", "checkin_enabled", c.Schedule.CheckinHours); err != nil {
		return err
	}
	if err := checkHourRange("schedule.keepalive_hours", "keepalive_enabled", c.Schedule.KeepaliveHours); err != nil {
		return err
	}
	if err := checkHourRange("schedule.activity_hours", "activity_enabled", c.Schedule.ActivityHours); err != nil {
		return err
	}
	// 下面三个此前漏校验：界面上现在可自由填时点，非法值必须在启动时就报错，
	// 而不是留到 nextFire 静默算出无意义的排程。
	if err := checkHourRange("schedule.nightowl_hours", "nightowl_enabled", c.Schedule.NightOwlHours); err != nil {
		return err
	}
	if err := checkHourRange("schedule.school_hours", "school_enabled", c.Schedule.SchoolHours); err != nil {
		return err
	}
	return checkHourRange("schedule.trial_hours", "trial_enabled", c.Schedule.TrialHours)
}

func checkHourRange(field, switchKey string, hours []int) error {
	for _, h := range hours {
		if h < 0 || h > 23 {
			return fmt.Errorf("%s: %d 不是合法小时（0-23）；如要关闭该任务请设 schedule.%s=false", field, h, switchKey)
		}
	}
	return nil
}

// validateQoderClaimHours 校验 Qoder 领取时点（0-23）。
//
// 与 `validateScheduleHours` 分开，是因为它的"关闭方式"不同：
// 其它任务的开关是 `xxx_enabled=false`，而 Qoder 领取关闭有两级
//（`qoder_claim_enabled=false` 或 `product_tasks_enabled=false`），
// 报错信息里要同时给出两者，否则用户不知道该改哪个。
func (c *Config) validateQoderClaimHours() error {
	for _, h := range c.Schedule.QoderClaimHours {
		if h < 0 || h > 23 {
			return fmt.Errorf(
				"schedule.qoder_claim_hours: %d 不是合法小时（0-23）；"+
					"如要关闭 Qoder 自动领取请设 schedule.qoder_claim_enabled=false"+
					"（或总闸 schedule.product_tasks_enabled=false）", h)
		}
	}
	return nil
}

// QoderClaimOn 该不该自动领 Qoder 权益活动。
//
// # 两级开关的语义（2026-09-22 拆分）
//
//		qoder_claim_enabled  **显式**配置 ⇒ 以它为准
//		未配置（nil）        ⇒ 回落到总闸 product_tasks_enabled
//
// 这样两种用户都对：
//
//   - 老配置只有总闸：行为完全不变（分产品键缺席 ⇒ 跟总闸走）
//   - 新界面写了分产品键：各产品互不影响
//
// ⚠ 这就是字段用 `*bool` 而不是 `bool` 的全部理由 —— 值类型缺键是 false，
// 会把所有老配置变成"关闭"，而活动不领就过期作废。
func (c *Config) QoderClaimOn() bool {
	if c.Schedule.QoderClaimEnabled != nil {
		return *c.Schedule.QoderClaimEnabled
	}
	return c.Schedule.ProductTasksEnabled
}

// ZcodeClaimOn 该不该自动领 ZCode 套餐。语义同 QoderClaimOn。
func (c *Config) ZcodeClaimOn() bool {
	if c.Schedule.ZcodeClaimEnabled != nil {
		return *c.Schedule.ZcodeClaimEnabled
	}
	return c.Schedule.ProductTasksEnabled
}
