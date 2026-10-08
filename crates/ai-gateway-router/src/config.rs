//! 网关配置：JSON 文件 + `WB2A_*` 环境变量覆盖 + normalize 回落。
//!
//! 对应 Go 源文件 `cmd/server/config.go`，行为逐条对齐，包括几处非显然语义：
//!
//! 1. **bool 开关「缺省 true」**：Go 侧依赖「先 `Default()` 再 `json.Unmarshal` 覆盖，
//!    键缺席时保留 true」。serde 的 `#[serde(default = "...")]` 在**键缺席**时也生效，
//!    因此这里对每个开关都显式标注默认构造函数，做到同样的缺席/显式-false 区分。
//! 2. **空数组 = 未配置 → 回落默认**：JSON 里的 `[]` / `null` 会把 serde 默认值覆盖掉，
//!    与 Go 侧「空数组被 unmarshal 覆盖」一致，回落逻辑放在 [`Config::normalize`]。
//! 3. **`<=0` 一律视为未设置**：timeouts / breaker threshold 等沿用 Go 侧口径，
//!    「0」不是「禁用」，而是「走默认」。

use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

use crate::error::{GatewayError, Result};

/// 默认监听地址。
///
/// **只绑 127.0.0.1**，不是 `:7863`（全网卡）。
///
/// 为什么这是安全要求而非偏好：`api_key` 默认为空（= 不鉴权），若同时监听全网卡，
/// 同一局域网内任何设备都能直接调用本网关、烧掉本机账号额度，`/status` 还会
/// 暴露账号标识。两者叠加就是一个默认敞开的代理。
///
/// 需要对外暴露时**必须同时设置 `api_key`**（README 有说明）。
fn default_listen() -> String {
    "127.0.0.1:7863".into()
}
/// 默认凭证目录。
fn default_auth_dir() -> String {
    "./auths".into()
}
/// 默认状态文件。
fn default_state_file() -> String {
    "./data/state.json".into()
}
/// 开关类「缺省 true」的 serde 默认构造。
fn default_true() -> bool {
    true
}

/// 保活兜底的默认容错间隔（小时）。
///
/// 24：与「每日一次」的语义一致 —— 超过一天没成功刷新就补跑。
/// 配置里写 0 = 关闭兜底（退回「只认整点」的旧行为）。
fn default_keepalive_max_age_hours() -> i64 {
    24
}

/// 默认 Qoder 凭证目录：`~/.wb-switch/qoder/auths`。
///
/// ★ **必须**与宿主 `crates/ai-gateway-core/src/modules/qoder_account.rs::auth_dir()`
/// 一致。宿主在生产里总是显式写出 `qoder_auth_dir`（`gateway.rs:2777`），
/// 所以本回落只在手工调试（无宿主）时生效 —— 但一旦分叉，
/// 症状就是「界面里登录成功了但网关看不到账号」，从界面上完全看不出来。
fn default_qoder_auth_dir() -> String {
    home_join(&[".wb-switch", "qoder", "auths"])
}

/// 默认 ZCode 凭证目录：`~/.wb-switch/zcode/auths`。
///
/// 同 [`default_qoder_auth_dir`]，须与 `zcode_account::auth_dir()` 一致。
fn default_zcode_auth_dir() -> String {
    home_join(&[".wb-switch", "zcode", "auths"])
}

/// 默认 Trae 凭证目录：`~/.wb-switch/trae/auths`。
///
/// 同 [`default_qoder_auth_dir`]，须与宿主 `trae_account::auth_dir()` 一致。
fn default_trae_auth_dir() -> String {
    home_join(&[".wb-switch", "trae", "auths"])
}

/// 在用户主目录下拼路径；拿不到主目录时退回相对路径（不 panic）。
///
/// 用 `USERPROFILE`（Windows）/ `HOME`（Unix）而不是引入 `dirs` crate：
/// 本 crate 目前零平台相关依赖，为一个回落值不值得加。
fn home_join(parts: &[&str]) -> String {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from);
    match home {
        Some(h) => {
            let mut p = h;
            for part in parts {
                p.push(part);
            }
            p.to_string_lossy().into_owned()
        }
        None => parts.join("/"),
    }
}

/// 顶层配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 监听地址，如 `"127.0.0.1:7863"`。裸端口（无冒号）时 normalize 阶段补回环前缀。
    #[serde(default = "default_listen")]
    pub listen: String,
    /// API Key，空 = 不鉴权。
    pub api_key: String,
    /// 凭证目录。
    #[serde(default = "default_auth_dir")]
    pub auth_dir: String,
    /// 池状态文件路径。
    #[serde(default = "default_state_file")]
    pub state_file: String,

    /// 冷却相关。
    pub cooldown: CooldownConfig,
    /// 定时任务相关。
    pub schedule: ScheduleConfig,
    /// 上游请求参数。
    pub upstream: UpstreamConfig,
    /// 特性开关。
    pub features: FeaturesConfig,
    /// Upstash Redis 快照。
    pub upstash: UpstashConfig,
    /// 网关自有系统提示词（`prompt` 块）。
    pub prompt: PromptConfig,
    /// 账号池参数。
    pub pool: PoolConfig,
    /// 会话粘性。
    #[serde(rename = "session_sticky")]
    pub session_sticky: SessionStickyConfig,

    /// 出站代理（空 = 不显式设置，回落环境变量）。
    ///
    /// 宿主（ai-gateway-core）会把它从「设置 → 更新代理」透传过来，用户无需配两遍。
    /// 为什么网关需要它：国际版（workbuddy.ai）在国内直连不稳定，走代理才稳。
    #[serde(default)]
    pub proxy: String,
    /// 代理作用域：三个独立开关（cn / intl / github）。
    ///
    /// 宿主侧由 `native_config_proxy_scope()` 写出。★ 与 Go 侧同样**必须是
    /// 每次全量重写**：宿主每次启动都重写本文件，若这里靠缺省值，用户在界面上
    /// 关掉国际版代理后，下次启动又会按「缺键 → 默认 intl=true」把代理打开，
    /// 表现为「关了开关，重启后又自己开了」。
    #[serde(rename = "proxy_scope")]
    pub proxy_scope: ProxyScopeConfig,

    // ==================== 多产品路由（WorkBuddy / Qoder / ZCode）====================
    //
    // ★★ 这些字段**声明在顶层**，但配置文件里的**真实位置是嵌套的**
    // （`pool.multi_product` / `schedule.qoder_claim_hours` …）。两者不一致这件事
    // 本身是历史包袱，不能靠「把字段搬进 `PoolConfig`」来消除 —— 那样会让所有
    // 顶层写法（含既有测试与用户手写配置）**静默失效**，比现状更糟。
    //
    // 真实层级（三方一致，均已实测核对）：
    // - Go：`cmd/server/config.go:201-501` 的 `Pool struct { … } \`json:"pool"\``
    //   （`multi_product` 在 `:247`、`watch_auth_dir` 在 `:490`）；`Schedule struct`
    //   在 `:32-156`。读取点全部走 `cfg.Pool.*`（`cmd/server/main.go:442/444/479/
    //   637/652/676/154/175/182/219/291`）。
    // - 宿主：`crates/ai-gateway-core/src/modules/gateway.rs:2708-2863` 的 `"pool"` 块
    //   与 `:2571-2669` 的 `"schedule"` 块。宿主测试
    //   `native_config_writes_multi_product_keys`（`:5427`）已把「写在 pool 下」
    //   固化为契约。
    // - 实机 ground truth：`~/.wb-switch/gateway/gateway_native_config.json` 顶层
    //   只有 15 个键，`multi_product` 等**全部在 `pool.*` 之下**。
    //
    // ⇒ 兼容由 [`hoist_nested_keys`] 在**反序列化之前**完成：把 `pool`/`schedule`
    // 下的这些键提到顶层（**顶层已存在时不动**，保持「顶层优先」）。
    //
    // ⚠ 关键契约：宿主**总是显式写出**这些键（`multi_product: true` 等）。若本文件
    // 不解析它们，网关会「起来了但只有 WorkBuddy 账号」—— 属于静默失效。这正是
    // 2026-10-01 实测复现的缺陷：宿主配置下 `multi_product` 读不到 ⇒ 网关恒为
    // `false`，Qoder/ZCode 账号一个都不加载，而**日志里没有任何报错**。
    //
    // ⚠ 新增反序列化入口时**必须**先过 [`hoist_nested_keys`]（见 `load`/`from_json`）。
    /// 多产品路由总开关，缺省 **false** = 只跑 WorkBuddy。
    ///
    /// 开启后：① 加载 Qoder / ZCode 凭证进同一个账号池 ② 请求按账号所属产品
    /// 分发到不同上游 ③ 成本维度参与加权（见 `pool.SetMultiProduct`）。
    ///
    /// ★ 做成开关的理由（Go 侧注释原文）：跨产品路由是最容易「悄悄改变现有行为」
    /// 的地方，而单产品行为已在生产跑了很久。关闭时所有既有路径**逐字不变** ——
    /// 这也是出问题时的回滚点（关掉即回到已验证行为，不需要回滚代码）。
    #[serde(rename = "multi_product")]
    pub multi_product: bool,
    /// Qoder 凭证目录；空 = `~/.wb-switch/qoder/auths`。
    ///
    /// ★ 必须与宿主侧 `qoder_account::auth_dir()` 一致，否则会出现
    /// 「界面里登录成功了但网关看不到账号」。宿主**显式写出**该键正是为此。
    #[serde(rename = "qoder_auth_dir")]
    pub qoder_auth_dir: String,
    /// ZCode 凭证目录；空 = `~/.wb-switch/zcode/auths`。同 `qoder_auth_dir`。
    #[serde(rename = "zcode_auth_dir")]
    pub zcode_auth_dir: String,
    /// Trae 凭证目录；空 = `~/.wb-switch/trae/auths`。同 `qoder_auth_dir`。
    ///
    /// ★ 国服与国际版账号**同目录混放**（凭证文件里的 `realm` 字段区分），
    /// 不按区域分目录：一个用户通常只有一个区域有账号，分目录会让
    /// 「加了账号但网关看不到」再多一种成因。
    #[serde(rename = "trae_auth_dir")]
    pub trae_auth_dir: String,
    /// 是否监听凭证目录、运行期自动热加载，缺省 **true**。
    ///
    /// ★ 为什么缺省 true：这是**修缺陷**而不是加可选能力 —— 不打开就等于保留
    /// 原缺陷（新增账号必须手动重启网关才生效，用户实际报过的痛点）。
    /// 它也不改变任何既有语义：热加载复用 `SyncToDir`/`upsertLocked`，
    /// 对已存在账号只换凭证、保留 credits/冷却/熔断/统计（幂等）。
    #[serde(rename = "watch_auth_dir", default = "default_true")]
    pub watch_auth_dir: bool,
    /// 凭证目录轮询周期，默认 `"5s"`。
    ///
    /// ★ 为什么是轮询而不是 fsnotify：零新依赖 + 容器/网络文件系统上不丢事件。
    /// 空/非法一律回落默认（**不报错**）：与 `credit_refresh_interval` 同一口径，
    /// 避免一个调优键把网关拦停。
    #[serde(rename = "watch_auth_dir_interval")]
    pub watch_auth_dir_interval: String,
    /// 各产品**实际可用**的模型清单（宿主透传），形状 `{"qoder":["qwen3.8-max"]}`。
    ///
    /// ★ 为什么由宿主透传：`/v1/models` 在**请求路径**上，在那里发外部请求会带来
    /// 延迟与失败面（每个客户端列一次模型就打一次上游）；而「某产品账号能用哪些
    /// 模型」只有宿主知道（它持有账号库、刷新账号时已查过并按 uid 缓存）。
    ///
    /// 用途：`/v1/models` 的 `channels` 字段（「这个模型来自哪个平台」）；
    /// 某 id 出现在多个产品清单里就有多个渠道 = 界面要表达的「重叠」。
    #[serde(rename = "product_models")]
    pub product_models: HashMap<String, Vec<String>>,
    /// 「模型 → 允许的平台」白名单（宿主透传）。缺键或空值 = 不限制。
    ///
    /// 所有者需求原文「我希望可以加上**平台区分使用哪个平台的模型**」。
    /// 同名模型可能同时在多个平台上（实测 `glm-5.2` 在 WorkBuddy 与 ZCode 都有），
    /// 选号默认按权重随机，用户无法指定「这个模型走 ZCode」。他**需要**这个能力
    /// 因为各平台额度性质不同：ZCode 体验套餐每日重置、到期后归零 → 该优先烧掉；
    /// WorkBuddy 是长期额度。
    ///
    /// ★ 白名单内账号全不可用时**回退到全池**，不硬失败
    /// （见 `pool.platformAllowedLocked` 与 `pickBestFrom`）。
    #[serde(rename = "model_platforms")]
    pub model_platforms: HashMap<String, Vec<String>>,
    /// 「平台 × 区域 → 允许的模型」白名单（用户手动配）。
    ///
    /// 所有者 2026-09-28 原话：「qoder 的 deepseek-v4.1-flash 不应该不拦截，而是给
    /// 每个平台手动配置支持的模型，而且要区分国内外版本」。
    ///
    /// ★ **与 `product_models` 的关键区别（别混用）**：
    /// - `product_models` = 上游**查询**来的清单 → 只用于「无前缀时优先选声明提供
    ///   该模型的产品」，**未知 = 不排除**（路由提示）；
    /// - `platform_models` = 用户**手动配**的白名单 → 用于「显式指定平台时只放行他
    ///   允许的模型」，**未知 = 不拦**（准入控制）。
    ///
    /// 背景：qoder 上游**实际上能服务**很多没列在查询清单里的模型（实测
    /// `qoder:deepseek-v4.1-flash` 10/10 全 200），但那是**上游能力**不等于
    /// **用户想让它跑**：上游能力（超集）不该由网关替用户判断。
    ///
    /// 形状（二级键为区域 `cn`/`intl`）：
    /// `{"qoder":{"cn":["Qwen3.8-Flash"],"intl":["Qwen3.8-Max"]}}`；
    /// 区域键 `""` 表示「该平台不分区域」（简单用法的兼容路径）。
    ///
    /// ★ 平台不在 map 里 / 该平台某区域没配 ⇒ **不拦**（向后兼容：老配置没有这个
    /// 键，升级后行为必须逐字不变）。
    #[serde(rename = "platform_models")]
    pub platform_models: PlatformModels,
    /// 用户显式**禁用**的「平台 × 区域 → 模型」。形状与归一化规则与
    /// `platform_models` **完全相同**，但语义相反且**更强**：命中即**拦**，
    /// **先于**白名单的并集判定。
    ///
    /// ★ 为什么需要（而不是「从 `platform_models` 里删掉」）：放行判据是**并集**
    /// （`platformAllowsModelLocked`）：放行 = 手动配置 ∪ 接口返回 ∪ 网关兜底，
    /// 故删掉一个模型**删不掉**它（仍可能被「接口返回」那支放行，实测
    /// `qoder:deepseek-v4.1-flash` 正是如此）；用户要的「禁用」是**独立于并集的
    /// 否决项**：「不管哪个来源说它能用，我就是不许它跑」。
    ///
    /// ★ **刻意用 `Option`（缺省 `None`）而不是空 map**：`None` 表示「用户没禁用
    /// 任何东西」，行为与引入该键前逐字不变；若用空 map 虽结果相同，但会让
    /// 「没配」与「配了个空的」再也分不清（与 Go 侧 `QoderClaimEnabled` 那组
    /// `*bool` 同一道理）。
    ///
    /// ★ 空清单同样**不写入**（`{"qoder":{"cn":[]}}` = 未配置，不否决）：界面里
    /// 清空禁用列表的语义应是「没有禁用任何模型」，而不是「该平台所有模型都禁用」
    /// （后者会让该平台彻底不可用）。
    #[serde(rename = "platform_models_disabled", default)]
    pub platform_models_disabled: Option<PlatformModels>,
    /// 「平台 → 是否参加兼容网关路由」总开关。
    ///
    /// ★ 语义 = `no_route` 的**平台级版本** —— 所有者原话：
    ///
    /// > 「关闭平台只是不参与兼容网关,并不是不执行任务请注意」
    ///
    /// 即：只把该平台的账号从**选号**里排除；签到 / 上报 / 成长任务 / 额度巡检
    /// 照常运行。它**不是**「停用平台」，也绝不写进账号的 `disabled`。
    ///
    /// ★ 与上面那组白名单的关系：那组回答「这个平台允许跑哪些模型」，本键回答
    /// 「这个平台参不参加路由」。两者都过：平台被关掉时，白名单里放行了也没用。
    ///
    /// ★ **刻意用 `Option`（缺省 `None`）而不是空 map**：`None` 表示「用户没关任何
    /// 平台」，行为与引入该键前逐字不变（老配置没有这个键）。与
    /// `platform_models_disabled` 同一道理。
    ///
    /// ★ 归一化：平台名 `trim + 小写`、空键跳过；**只保留显式 `false` 的项** ——
    /// `true` 与「键不存在」同义，写进配置文件只会让「全量还是增量」难以分辨。
    #[serde(rename = "platform_enabled", default)]
    pub platform_enabled: Option<HashMap<String, bool>>,
    /// Qoder / ZCode 日常任务**自动执行**总闸，缺省 **true**。
    ///
    /// ★ 已于 2026-09-22 拆分为 `qoder_claim_enabled` / `zcode_claim_enabled`
    /// （所有者要求「权益自动领取 qoder zcode 拆分开，不要合成一个」）；本字段
    /// **保留为兼容总闸**（任一新产品开关**未显式配置**时回落）。
    ///
    /// ★ 警告（Go 侧注释原文）「⚠ 不要删掉它 —— 删了会让所有存量配置的『自动
    /// 领取』在某次升级后**静默失效**（缺键 ⇒ 零值 false ⇒ 不领 ⇒ 活动过期作废）」。
    ///
    /// 默认开的理由（所有者 2026-09-20 原话）「qoder这个活动卡片…而且任务也应该
    /// 自动执行」= 「别让我每天手点」，默认关等于没做。
    #[serde(rename = "product_tasks_enabled", default = "default_true")]
    pub product_tasks_enabled: bool,
    /// Qoder 自动领取开关。★ 用 `Option<bool>` 而非 `bool`：需区分「没配」
    /// （`None`，回落总闸）与「显式 false」（用户明确要关）。
    /// 值类型的话缺键 = false，会把老配置全变成「关闭」。
    #[serde(rename = "qoder_claim_enabled", default)]
    pub qoder_claim_enabled: Option<bool>,
    /// ZCode 自动领取开关，语义同 `qoder_claim_enabled`。
    #[serde(rename = "zcode_claim_enabled", default)]
    pub zcode_claim_enabled: Option<bool>,
    /// Qoder 领取时点（小时），默认 `[10, 21]`。
    ///
    /// 所有者 2026-09-22 原话「qoder改为 早十点,晚九点 两次触发,防止错漏」。
    /// 活动每天 10:00（UTC+8）重置，只领一次的话某轮网络抖动/上游 5xx/恰在重置前
    /// 跑过 ⇒ 当天**领不到**（额度作废）；两个时点互相兜底。
    /// 空数组 = 不做时点制领取（只靠下面的轮询）。
    #[serde(rename = "qoder_claim_hours")]
    pub qoder_claim_hours: Vec<i32>,
    /// Qoder 领取轮询周期（分钟），默认 20。
    ///
    /// 保留轮询的理由：时点制在两个整点之间完全不动，若用户 10:30 打开软件而
    /// 10:00 那轮失败，就要**干等到 21:00**；轮询让它下一轮补上。
    /// 缺省 20 与宿主侧 `PATROL_INTERVAL` 一致。
    #[serde(rename = "qoder_claim_interval_minutes")]
    pub qoder_claim_interval_minutes: i64,
    /// ZCode 验证码求解器目录（含 `solver.js` + `node_modules`）。
    ///
    /// 空 = 不启用本地求解。理由：ZCode 对话端点要求
    /// `X-Aliyun-Captcha-Verify-Param`（实测回 `400 code=3007 captcha verify failed`，
    /// 且与模型名、请求头都无关）。求解器是 Node 脚本（happy-dom 模拟浏览器跑阿里云
    /// 官方 SDK），由宿主作为**发行资源**释放到磁盘并告诉网关路径。
    ///
    /// ★ 为什么不是网关自己找：发行包里的资源路径由 Tauri 决定（安装目录下的
    /// resources），网关无从推断；开发期又指向仓库里的 `assets/`。
    #[serde(rename = "zcode_captcha_dir")]
    pub zcode_captcha_dir: String,
    /// ZCode 验证码求解开关。
    ///
    /// ★ 2026-09-20 **反转：默认关闭 → 始终开启**（所有者决定）。原注释写「默认
    /// false…由用户在界面上明确开启后才参与请求」，但那个「界面开关」**从来不存在**，
    /// 而实测 ZCode 对话通道对不带验证码的请求一律回 `HTTP 400 {"code":3007}` ⇒
    /// 「默认关闭」等于 **ZCode 对话从未成功过**。所有者原话「肯定要默认打开并且
    /// 不能关闭啊，这是开源软件有什么在乎的？」。
    ///
    /// ★ 本字段**保留只为向后兼容与诊断对照**：老配置里可能写着 false，但读取处
    /// **不再据此关闭**（见 `Config::zcode_captcha_active`）—— 否则升级后老用户仍然
    /// 「功能永远关着」。
    #[serde(rename = "zcode_captcha_enabled", default = "default_true")]
    pub zcode_captcha_enabled: bool,
    /// 宿主提供的**外部求解服务**地址（形如 `http://127.0.0.1:51234/solve`）。
    ///
    /// 所有者 2026-09-21 提出的方案。★ 本地求解两个硬伤：① 要求用户机器有 Node
    /// （为此外置了 81MB node.exe）② 模拟环境被风控盯上，实测成功率仅约 40%
    /// （靠调 `stallMs` 提到 88%）。而宿主自带真实 WebView2（Win10/11 预装，零体积），
    /// 实测脚本化调用（无人工点击）`startTracelessVerification()` **929ms** 拿到 param。
    ///
    /// ★ 配置后**外部优先**，本地 Node 作为回退。
    #[serde(rename = "zcode_captcha_solver_url")]
    pub zcode_captcha_solver_url: String,
    /// 调外部求解服务时的共享密钥（同机 IPC 鉴权）。
    #[serde(rename = "zcode_captcha_solver_token")]
    pub zcode_captcha_solver_token: String,
    /// 宿主提供的 **WorkBuddy 设备 token 服务**地址；空 = 不启用。
    ///
    /// 官方 WorkBuddy 客户端**每个请求**都带 `X-Device-Token`（`v3:` 形态，
    /// 约 1030 字符），网关此前完全不带。该 token 由官方自带的腾讯 TuringShield SDK
    /// 生成（`channelId=400111`、`sdkVariant="overseas"`、`productName="workbuddy-ai"`、
    /// `productVersion="5.6.2"`）。★ **每次调用都不同**（按请求签发，故不能缓存固定值）。
    ///
    /// ★ 它解决什么/不解决什么：✅ 让网关请求与官方客户端**同形态** —— 长期缺设备
    /// 凭证会被风控逐步标记，带上可避免这一类新增标记；❌ **不能**救活已被上游标记
    /// 失效的账号（那是服务端的 (设备,账号) 绑定状态，实测 403 11140 的账号带上它
    /// 仍然 403，只能去官方客户端重登一次）。
    #[serde(rename = "wb_device_token_url")]
    pub wb_device_token_url: String,
    /// 调该服务时的共享密钥（同机 IPC 鉴权）。
    #[serde(rename = "wb_device_token_token")]
    pub wb_device_token_token: String,

    /// normalize 后的解析值（不参与反序列化）。
    #[serde(skip)]
    pub parsed: ParsedConfig,
    /// 账号记录回写目标（`account_records` 块）。
    ///
    /// ★ 为什么网关需要它：养号任务（活跃上报 / 夜猫子 / 开学季 / trial / 活跃地图）
    /// 的执行痕迹此前只走 `log.Printf` 写 stdout，而宿主启动网关子进程时把
    /// stdout/stderr 丢进了 `Stdio::null` ⇒ **任务照跑，界面上一条痕迹都没有**。
    /// 写宿主已经在读的 `account_records.json`，记录就能与签到并列出现在
    /// 「账号记录 → 任务」里，不必引入第二套格式。
    ///
    /// ★ 路径由**宿主**给出而不是网关自己算：网关的 cwd 是宿主指定的 `gateway/`
    /// 子目录，两边各自推算迟早算出不同值，而那种错法表现为「任务跑了但界面上
    /// 没有记录」，几乎无法从现象定位。
    ///
    /// ★ 整块缺席（老宿主）时 `file` 为空 ⇒ 不记录。独立运行 `gateway.exe` 时
    /// 这是正确行为，**不该报错也不该刷日志** —— 旁路观测数据不该让独立运行模式
    /// 产生噪音。
    #[serde(rename = "account_records", default)]
    pub account_records: AccountRecordsConfig,
}

/// `account_records` 块：账号记录回写目标。
///
/// 对应 Go `cmd/server/config.go:543` 的 `AccountRecords` 结构体，字段名逐字一致
/// （宿主与网关读写**同一个文件**，键名不一致会让两边互相覆盖）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AccountRecordsConfig {
    /// `account_records.json` 的**绝对路径**；空 = 未启用（全部写入静默 no-op）。
    #[serde(rename = "file")]
    pub file: String,
    /// 保留天数，与宿主设置同一口径；`<= 0` 回落 60
    /// （[`crate::records::DEFAULT_RETENTION_DAYS`]）。
    ///
    /// ★ 取宿主设置而不是网关自己定：两边各算一次会出现「界面说保留 60 天、
    /// 网关按 7 天清」这类不一致，而用户只会看到记录莫名其妙消失。
    #[serde(rename = "retention_days")]
    pub retention_days: i64,
    /// 账号身份映射（uid → id/name）。
    ///
    /// ★ 为什么需要：界面按**账号库 id** 过滤记录，而记录里写的是 uid。没有这份
    /// 映射，记录在界面上会显示成一条 uid 裸串，用户在账号列表里找不到对应账号。
    #[serde(rename = "identities")]
    pub identities: Vec<RecordIdentity>,
}

/// `account_records.identities` 的一项。
#[derive(Debug, Clone, Deserialize)]
pub struct RecordIdentity {
    /// 账号 uid（记录里的 `accountId`）。
    #[serde(rename = "uid")]
    pub uid: String,
    /// 账号库 id（界面过滤用）。
    #[serde(rename = "id", default)]
    pub id: String,
    /// 展示名。
    #[serde(rename = "name", default)]
    pub name: String,
}

impl Config {
    /// `account_records.identities` → [`crate::records::Identity`] 映射。
    ///
    /// 与 Go `Config.RecordIdentities()`（`cmd/server/config.go:629`）同口径：
    /// **跳过 `uid == ""` 的项** —— 那种项映射不到任何记录，收进来只会让索引里
    /// 多一条永远命不中的空键。
    pub fn record_identities(&self) -> std::collections::HashMap<String, crate::records::Identity> {
        let mut out = std::collections::HashMap::new();
        for item in &self.account_records.identities {
            if item.uid.is_empty() {
                continue;
            }
            out.insert(
                item.uid.clone(),
                crate::records::Identity::new(item.id.clone(), item.name.clone()),
            );
        }
        out
    }
}

/// 声明在顶层、但宿主/Go 写在 **`pool.`** 之下的键。
///
/// 每一项都对应上面 [`Config`] 的一个字段。**新增顶层字段时若宿主把它写在
/// `pool` 下，必须同步加进本表** —— 否则该键会静默失效（`serde` 忽略不认识的
/// 嵌套键，字段拿默认值，日志无任何提示）。
///
/// 来源：`cmd/server/config.go:201-501`（Go 的 `Pool struct`）与
/// `crates/ai-gateway-core/src/modules/gateway.rs:2708-2863`（宿主的 `"pool"` 块）。
const HOISTED_POOL_KEYS: [&str; 17] = [
    "multi_product",
    "qoder_auth_dir",
    "zcode_auth_dir",
    "trae_auth_dir",
    "watch_auth_dir",
    "watch_auth_dir_interval",
    "product_models",
    "model_platforms",
    "platform_models",
    "platform_models_disabled",
    // ★ 平台总开关（宿主写在 "pool" 块里，见 gateway.rs 的 write_native_config）
    "platform_enabled",
    "zcode_captcha_dir",
    "zcode_captcha_enabled",
    "zcode_captcha_solver_url",
    "zcode_captcha_solver_token",
    "wb_device_token_url",
    "wb_device_token_token",
];

/// 声明在顶层、但宿主/Go 写在 **`schedule.`** 之下的键。
///
/// 来源：`cmd/server/config.go:32-156`（Go 的 `Schedule struct`）与
/// `crates/ai-gateway-core/src/modules/gateway.rs:2571-2669`（宿主的 `"schedule"` 块）。
///
/// ★ 刻意**只收本 crate 真正有字段的 5 个**：`activity_hours` / `nightowl_*` /
/// `school_*` / `trial_*` / `activity_report_count` 已在 [`ScheduleConfig`] 里
/// 有了字段（2026-10-02 接通 `crate::scheduler` 之后），因此**不需要**提升 ——
/// 它们本来就写在 `schedule.` 之下，与 [`ScheduleConfig`] 的层级一致。
/// 提升机制只为「声明在顶层、写在嵌套里」的历史包袱而存在。
const HOISTED_SCHEDULE_KEYS: [&str; 5] = [
    "product_tasks_enabled",
    "qoder_claim_enabled",
    "zcode_claim_enabled",
    "qoder_claim_hours",
    "qoder_claim_interval_minutes",
];

/// 把 `pool.` / `schedule.` 下的「顶层键」提升到顶层。
///
/// # 为什么需要
///
/// 这些键在**声明**上属于顶层 [`Config`]，在**配置文件**里却嵌在 `pool` / `schedule`
/// 之下 —— 三方（Go 结构体、宿主写出、实机配置文件）一致如此，只有 Rust 的声明
/// 层级与之不符。2026-10-01 实测：宿主配置下 `multi_product` 读不到 ⇒ 网关恒为
/// `false`（只跑 WorkBuddy），而**日志里没有任何报错**，是最难发现的一类失效。
///
/// # 为什么在反序列化前改 JSON，而不是把字段搬进 `PoolConfig`
///
/// 搬字段会连带废掉**顶层写法**（既有测试、用户手写配置、老版本宿主），
/// 让它们静默失效 —— 用一个静默失效换另一个，不划算。这里改成「两级都认」。
///
/// # 优先级：顶层优先
///
/// 仅当顶层**缺失或为 `null`** 时才提升。`null` 视为「未配置」是刻意放宽：
/// 顶层写 `null` 在今天的实现里会直接解析失败（`String`/`bool` 不接受 `null`），
/// 没有依赖这个行为的配置；而当它出现在 `Option` 字段（`qoder_claim_enabled`）上时，
/// `null` 的语义本来就是「没配」。
///
/// # 边界
///
/// `pool` / `schedule` 不是对象（缺失 / `null` / 数组）时静默跳过，不报错 ——
/// 形状错误留给 `serde` 在反序列化阶段报，错误信息更准。
fn hoist_nested_keys(v: &mut serde_json::Value) {
    let Some(obj) = v.as_object_mut() else {
        return;
    };
    for (nested, keys) in [
        ("pool", HOISTED_POOL_KEYS.as_slice()),
        ("schedule", HOISTED_SCHEDULE_KEYS.as_slice()),
    ] {
        // 克隆出嵌套对象再改（配置只在启动时解析一次，这点拷贝可以忽略）；
        // 直接 `get_mut` 会与下面 `obj.get` / `obj.insert` 的借用冲突。
        let Some(mut inner) = obj.get(nested).and_then(|x| x.as_object()).cloned() else {
            continue;
        };
        let mut changed = false;
        for k in keys {
            // 顶层已有非 null 值 ⇒ 保持不动（顶层优先）。
            if obj.get(*k).map(|x| !x.is_null()).unwrap_or(false) {
                continue;
            }
            if let Some(val) = inner.remove(*k) {
                obj.insert((*k).to_string(), val);
                changed = true;
            }
        }
        if changed {
            obj.insert(nested.to_string(), serde_json::Value::Object(inner));
        }
    }
}

/// 解析配置：先 `Value` → 提升嵌套键 → 再反序列化成 [`Config`]。
///
/// 语法错误仍带行列号（来自 `from_slice::<Value>`）；类型错误不带行列号
/// （`from_value` 不保留位置信息）—— 这是为「两级都认」付出的代价，
/// 错误文案里仍带字段名与期望类型，足够定位。
fn deserialize_config(raw: &[u8]) -> std::result::Result<Config, serde_json::Error> {
    let mut v: serde_json::Value = serde_json::from_slice(raw)?;
    hoist_nested_keys(&mut v);
    serde_json::from_value(v)
}

/// 「平台 × 区域 → 模型清单」。
///
/// 外层键 = 平台名（`workbuddy`/`qoder`/`zcode`），内层键 = 区域
/// （`cn`/`intl`，或 `""` 表示「该平台不分区域」），值 = 模型 id 列表。
pub type PlatformModels = HashMap<String, HashMap<String, Vec<String>>>;

/// 代理作用域：三个独立开关。
///
/// 宿主侧 `native_config_proxy_scope()` 写出。★ 每个都是「缺省 true」：
/// Go 侧依赖「先 `Default()` 再 `json.Unmarshal` 覆盖」，键缺席时保留 true。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProxyScopeConfig {
    /// 国服（workbuddy.cn / codebuddy.cn）流量走代理，缺省 true。
    pub cn: bool,
    /// 国际版（workbuddy.ai）流量走代理，缺省 true。
    pub intl: bool,
    /// GitHub（更新检查）流量走代理，缺省 true。
    pub github: bool,
}

impl Default for ProxyScopeConfig {
    fn default() -> Self {
        Self {
            cn: true,
            intl: true,
            github: true,
        }
    }
}

/// duration 字符串在 normalize 后的解析结果。
#[derive(Debug, Clone, Default)]
pub struct ParsedConfig {
    /// `cooldown.soft_rate` 解析结果。
    pub soft_rate: Duration,
    /// `pool.breaker_cooldown` 解析结果。
    pub breaker_cooldown: Duration,
    /// `pool.breaker_cooldown_max` 解析结果。
    pub breaker_cooldown_max: Duration,
    /// `session_sticky.ttl` 解析结果。
    pub session_ttl: Duration,
    /// `session_sticky.gc_interval` 解析结果。
    pub session_gc_interval: Duration,
    /// `pool.credit_refresh_interval` 解析结果。
    pub credit_refresh_interval: Duration,
    /// `watch_auth_dir_interval` 解析结果。
    ///
    /// 只在 `watch_auth_dir` 开启时有意义。
    pub watch_auth_dir_interval: Duration,
    /// `prompt.mode` 归一化结果（`passthrough` / `custom` / `append`）。
    ///
    /// ★ 归一化（去空白 + 小写 + 合法性校验）只在 [`Config::normalize`] 做一次：
    /// 请求路径上只做一次字符串比较，且**非法值在启动期就报错**而不是每个请求
    /// 各判一次（那会让「配错了」表现为「有时生效有时不生效」）。
    pub prompt_mode: String,
    /// `prompt` 模式下**解析后**的系统提示词文本（passthrough 下恒空）。
    ///
    /// ★ 在 normalize 阶段一次性读盘并缓存，而不是每个请求现读文件：请求路径上做文件
    /// IO 会引入可避免的延迟与失败面，且运行期改文件本该由「改配置 + 重启」承载，语义
    /// 更清晰（也避免读到写了一半的文件）。
    pub prompt_text: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            api_key: String::new(),
            auth_dir: default_auth_dir(),
            state_file: default_state_file(),
            cooldown: CooldownConfig::default(),
            schedule: ScheduleConfig::default(),
            upstream: UpstreamConfig::default(),
            features: FeaturesConfig::default(),
            upstash: UpstashConfig::default(),
            pool: PoolConfig::default(),
            session_sticky: SessionStickyConfig::default(),
            proxy: String::new(),
            proxy_scope: ProxyScopeConfig::default(),
            // ★ 缺省 false = 只跑 WorkBuddy。Go 侧同口径（`config.go:247`）：
            // 多产品是「最容易悄悄改变现有行为」的地方，默认必须是已验证的旧行为。
            multi_product: false,
            qoder_auth_dir: String::new(),
            zcode_auth_dir: String::new(),
            trae_auth_dir: String::new(),
            watch_auth_dir: true,
            watch_auth_dir_interval: default_watch_auth_dir_interval(),
            product_models: HashMap::new(),
            model_platforms: HashMap::new(),
            platform_models: HashMap::new(),
            // ★ 刻意 `None` 而不是空 map，理由见字段文档（区分「没配」与「配了个空的」）。
            platform_models_disabled: None,
            // ★ 同上：`None` = 用户没关任何平台，行为与引入该键前逐字一致。
            platform_enabled: None,
            product_tasks_enabled: true,
            qoder_claim_enabled: None,
            zcode_claim_enabled: None,
            qoder_claim_hours: vec![10, 21],
            qoder_claim_interval_minutes: default_qoder_claim_interval_minutes(),
            zcode_captcha_dir: String::new(),
            zcode_captcha_enabled: true,
            zcode_captcha_solver_url: String::new(),
            zcode_captcha_solver_token: String::new(),
            wb_device_token_url: String::new(),
            wb_device_token_token: String::new(),
            prompt: PromptConfig::default(),
            parsed: ParsedConfig::default(),
            // ★ 缺省空路径 = 不记录（独立运行 `gateway.exe` 时的正确行为）。
            account_records: AccountRecordsConfig::default(),
        }
    }
}

/// `watch_auth_dir_interval` 缺省值。
fn default_watch_auth_dir_interval() -> String {
    "5s".into()
}

/// `qoder_claim_interval_minutes` 缺省值（与宿主侧 `PATROL_INTERVAL` 一致）。
fn default_qoder_claim_interval_minutes() -> i64 {
    20
}

/// 冷却配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CooldownConfig {
    /// 429 软冷却时长字符串，默认 `"60s"`。
    #[serde(rename = "soft_rate")]
    pub soft_rate: String,
}

impl Default for CooldownConfig {
    fn default() -> Self {
        Self { soft_rate: "60s".into() }
    }
}

/// 定时任务配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ScheduleConfig {
    /// 签到时点（小时），默认 `[9, 21]`。
    #[serde(rename = "checkin_hours")]
    pub checkin_hours: Vec<i32>,
    /// token 保活时点（小时），默认 `[22]`。
    #[serde(rename = "keepalive_hours")]
    pub keepalive_hours: Vec<i32>,
    /// 保活兜底：距上次**成功**刷新超过这么多小时就在启动时补跑一次；0 = 关闭。
    ///
    /// # 为什么需要它（2026-10-08）
    ///
    /// `keepalive_hours` 只在整点触发，而网关是独立子进程 —— 22:00 那一刻没在跑
    /// （应用关了 / 刚重启 / 维护窗口），当天的保活就**静默消失**，下次机会 24 小时后。
    /// 保活要防的正是「服务端清理闲置 refresh 会话」（闲置数天 ⇒ 12153 invalid_grant
    /// ⇒ 账号被迫重新登录）。有它之后，「今天没在 22:00 跑过」会在下次启动时补上。
    ///
    /// 默认 24（宿主 `native_config` 写出；缺键也按 24 处理）。
    #[serde(rename = "keepalive_max_age_hours", default = "default_keepalive_max_age_hours")]
    pub keepalive_max_age_hours: i64,
    /// 签到开关，缺省 true。
    #[serde(rename = "checkin_enabled", default = "default_true")]
    pub checkin_enabled: bool,
    /// 保活开关，缺省 true。
    #[serde(rename = "keepalive_enabled", default = "default_true")]
    pub keepalive_enabled: bool,
    /// 签到区域范围：`"cn"`（缺省，仅国服）/ `"all"`。
    #[serde(rename = "checkin_scope")]
    pub checkin_scope: String,

    // ==================== 四个自动养号任务（2026-10-02 接通）====================
    //
    // ★★ 为什么这些字段必须在这里（而不是像 `qoder_claim_*` 那样提升到顶层）：
    // 它们在宿主与 Go 侧的**真实位置就是 `schedule.` 之下**，而本结构体正是
    // `schedule` 块。声明位置与配置文件位置一致时不需要任何兼容层 —— 提升机制
    // （[`HOISTED_SCHEDULE_KEYS`]）只是为「声明在顶层但写在嵌套里」的历史包袱
    // 准备的，不该被当成默认做法。
    //
    // ★ 为什么之前一个都没有：网关侧当时**没有任何消费者**（这四个任务的执行体
    // 在 `internal/scheduler`，Rust 侧尚未移植），收进来只会是一堆死字段。现在
    // [`crate::scheduler`] 已落地，它们成了必需项。
    //
    // ★★ 缺键时的行为必须与 Go 一致：**默认全部启用**（`*_enabled` 缺省 true）。
    // 这与「引入开关之前的行为逐字相同」—— 老配置里没有这些键时任务照跑，只有
    // 显式 `false` 才关。反过来（缺省 false）会让存量用户的养号任务**静默停摆**，
    // 而连登断一天就要重攒，代价不可逆。
    //
    /// 活跃上报时点，默认 `[10]`（对齐 Go `scheduler.New` 的兜底）。
    #[serde(rename = "activity_hours")]
    pub activity_hours: Vec<i32>,
    /// 夜猫子时点，默认 `[1]`。
    #[serde(rename = "nightowl_hours")]
    pub nightowl_hours: Vec<i32>,
    /// 开学季时点，默认 `[12]`。
    #[serde(rename = "school_hours")]
    pub school_hours: Vec<i32>,
    /// trial 加油包时点，默认 `[9, 21]`。
    #[serde(rename = "trial_hours")]
    pub trial_hours: Vec<i32>,
    /// 活跃上报开关，缺省 true。
    ///
    /// ★ 同时闸住活跃地图（`growthmap`）：后者没有独立时点，挂在活跃上报的
    /// slot 上（见 `scheduler::next_wake`），故这个开关是两者的共同闸门。
    #[serde(rename = "activity_enabled", default = "default_true")]
    pub activity_enabled: bool,
    /// 夜猫子开关，缺省 true。
    #[serde(rename = "nightowl_enabled", default = "default_true")]
    pub nightowl_enabled: bool,
    /// 开学季开关，缺省 true。
    #[serde(rename = "school_enabled", default = "default_true")]
    pub school_enabled: bool,
    /// trial 加油包开关，缺省 true。
    #[serde(rename = "trial_enabled", default = "default_true")]
    pub trial_enabled: bool,
    /// 每号每日活跃上报条数，`<= 0` 时回落 `3`（`scheduler::DEFAULT_ACTIVITY_REPORT_COUNT`）。
    ///
    /// 取 3 而非 1：单条上报偶发被服务端静默丢弃，多条提高点亮成功率；也不宜过多，
    /// 避免被风控当成异常流量。宿主侧另有 `.min(20)` 兜住误填。
    #[serde(rename = "activity_report_count")]
    pub activity_report_count: i32,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            checkin_hours: vec![9, 21],
            keepalive_hours: vec![22],
    keepalive_max_age_hours: default_keepalive_max_age_hours(),
            checkin_enabled: true,
            keepalive_enabled: true,
            checkin_scope: "cn".into(),
            activity_hours: vec![10],
            nightowl_hours: vec![1],
            school_hours: vec![12],
            trial_hours: vec![9, 21],
            activity_enabled: true,
            nightowl_enabled: true,
            school_enabled: true,
            trial_enabled: true,
            activity_report_count: 3,
        }
    }
}

/// 上游请求配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UpstreamConfig {
    /// 短 RPC 总时长上限（秒），默认 120。
    #[serde(rename = "timeout_seconds")]
    pub timeout_seconds: i64,
    /// SSE 首字节前上限（秒）；`<=0` 回落 timeout_seconds。
    #[serde(rename = "header_timeout_seconds")]
    pub header_timeout_seconds: i64,
    /// SSE 流中空闲上限（秒）；`<=0` 回落 300。
    #[serde(rename = "idle_timeout_seconds")]
    pub idle_timeout_seconds: i64,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            timeout_seconds: 120,
            header_timeout_seconds: 0,
            idle_timeout_seconds: 0,
        }
    }
}

/// 特性开关。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FeaturesConfig {
    /// 出站请求体黑名单指纹脱敏，默认 true。
    #[serde(rename = "sanitize_blacklist_fingerprints", default = "default_true")]
    pub sanitize_blacklist_fingerprints: bool,
}

impl Default for FeaturesConfig {
    fn default() -> Self {
        Self {
            sanitize_blacklist_fingerprints: true,
        }
    }
}

/// Upstash Redis 配置。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct UpstashConfig {
    /// 连接串或 host；空 = 纯内存模式。
    pub url: String,
    /// url 非完整连接串时用于组装 `rediss://default:<token>@<host>:6379`。
    pub token: String,
}

/// 网关自有系统提示词配置（`prompt` 块）。
///
/// 与 Go 侧 `cmd/server/config.go:177-194` 的 `Prompt struct` 逐字段对应。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PromptConfig {
    /// 模式：`passthrough`（缺省）/ `custom` / `append`。
    ///
    /// ★ 为什么缺省是 passthrough 而不是 custom：这是**新增能力**，老配置里没有这个
    /// 键。若缺省 custom，所有既有用户升级后 system 会被静默换掉 —— 人设、项目约定、
    /// 工具说明全丢，且从请求上看不出是网关动的手。保守缺省 + 显式开启，用户改配置时
    /// 才知道自己换掉了什么。
    pub mode: String,
    /// 提示词文件路径；空 = 用内置默认 [`crate::prompt::default_prompt`]。
    ///
    /// 路径非空但不可读 / 内容为空 → **启动即报错**（fail fast）。注意该限制**只作用
    /// 于 custom / append 模式** —— passthrough 下即使 file 填错也不该拦住启动（那段
    /// 文本根本不会被使用）。
    pub file: String,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            // 缺省 passthrough：透传客户端原始 system。老配置没有 prompt 块，
            // 必须保持既有行为不变。
            mode: crate::prompt::MODE_PASSTHROUGH.to_string(),
            file: String::new(),
        }
    }
}

/// 账号池配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PoolConfig {
    /// 单账号最大在途请求数，0 = 不限，默认 3。
    #[serde(rename = "max_in_flight")]
    pub max_in_flight: i64,
    /// 连续失败触发熔断阈值，默认 3。
    #[serde(rename = "breaker_threshold")]
    pub breaker_threshold: i64,
    /// 基础熔断时长字符串，默认 `"30m"`。
    #[serde(rename = "breaker_cooldown")]
    pub breaker_cooldown: String,
    /// 指数退避封顶字符串，默认 `"6h"`。
    #[serde(rename = "breaker_cooldown_max")]
    pub breaker_cooldown_max: String,
    /// 闲置补偿：每小时未用增加的权重，默认 0.5。
    #[serde(rename = "idle_weight_per_hour")]
    pub idle_weight_per_hour: f64,
    /// 闲置补偿封顶，默认 5.0。
    #[serde(rename = "idle_weight_max")]
    pub idle_weight_max: f64,
    /// 积分到期巡检周期字符串，默认 `"15m"`。
    #[serde(rename = "credit_refresh_interval")]
    pub credit_refresh_interval: String,
    /// 积分到期巡检开关，缺省 true。
    #[serde(rename = "credit_refresh_enabled", default = "default_true")]
    pub credit_refresh_enabled: bool,
    /// 「单一模型 + 积分轮转」模式开关；缺省 false = 负载均衡。
    ///
    /// 由宿主的**工作模式**推导（不读独立开关），避免「模式是轮转、
    /// rotation 却是 false」这类不一致状态。
    #[serde(default)]
    pub rotation: bool,
    /// 「限制使用的模型」白名单（多选）。
    ///
    /// 非空时网关**只放行名单内的模型**，其余一律 400 `model_not_allowed`；
    /// 空（默认）= 不限制。三个工作模式（自动 / 手动 / 积分轮转）共用同一份
    /// 名单 —— 它限制的是「放行哪些模型」，与「用哪些账号」是正交的两件事。
    ///
    /// 轮转模式尤其需要它：轮转的语义是「把这个账号的某个模型额度烧干净再
    /// 换号」，模型是策略的一部分，不限制的话客户端换个模型就能绕过轮转与
    /// 额度控制，也让「当前烧的是哪个模型」变得不可预期。
    ///
    /// # 为什么是 [`AllowedModels`] 而不是 `Vec<String>`
    ///
    /// 这个键在配置文件里的历史形状是**字符串**
    /// （`"allowed_model": "deepseek-v4.1-flash"`），新宿主写的是**数组**。
    /// 直接声明成 `Vec<String>` 会让所有老配置在解析阶段就失败 ——
    /// 表现为「升级后网关直接起不来」，是最严重的一类向后兼容事故。
    /// 见 [`AllowedModels`] 的 `Deserialize` 实现。
    ///
    /// ★ 实测（2026-10-01）宿主导出的 `gateway_native_config.json` 写的就是
    /// `"allowed_model": []` —— 旧实现（`String` 字段）会**直接解析失败**。
    #[serde(rename = "allowed_model", default)]
    pub allowed_model: AllowedModels,
    /// 单请求最多换号次数。
    ///
    /// 缺省 3。此前该值在网关侧从未接线（硬编码为 3），配置里写什么都不生效 ——
    /// 本次补上字段并接进 [`crate::forward::ForwardCtx`]。
    #[serde(rename = "max_rotate", default = "default_max_rotate")]
    pub max_rotate: usize,
}

/// `pool.max_rotate` 缺省值。
fn default_max_rotate() -> usize {
    3
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 3,
            breaker_threshold: 3,
            breaker_cooldown: "30m".into(),
            breaker_cooldown_max: "6h".into(),
            idle_weight_per_hour: 0.5,
            idle_weight_max: 5.0,
            credit_refresh_interval: "15m".into(),
            credit_refresh_enabled: true,
            rotation: false,
            allowed_model: AllowedModels::default(),
            max_rotate: default_max_rotate(),
        }
    }
}

/// 「限制使用的模型」白名单。
///
/// 对应 Go `cmd/server/config.go:584-627` 的 `type AllowedModels []string`。
///
/// # 为什么需要自定义类型而不是直接用 `Vec<String>`
///
/// 这个键在配置文件里的历史形状是**字符串**
/// （`"allowed_model": "deepseek-v4.1-flash"`），新宿主写的是**数组**
/// （`"allowed_model": ["a", "b"]`）。直接把字段声明成 `Vec<String>` 会让所有
/// 老配置在解析阶段就失败，表现为「升级后网关直接起不来」，是最严重的一类
/// 向后兼容事故。自定义 `Deserialize` 是唯一能同时吃下两种形状的写法。
///
/// # 归一化在别处
///
/// 这里只负责**形状**（字符串 / 数组 / null），元素里的空白、空串、前缀都交给
/// [`crate::model_allow::normalize_allowed_models`] 统一处理 ——
/// 两边各写一套必然分叉（Go 侧注释逐字记着这条）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowedModels(pub Vec<String>);

impl AllowedModels {
    /// 借用内部切片。
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
    /// 是否为空（= 不限制）。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::ops::Deref for AllowedModels {
    type Target = [String];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Vec<String>> for AllowedModels {
    fn from(v: Vec<String>) -> Self {
        Self(v)
    }
}

impl<'de> serde::Deserialize<'de> for AllowedModels {
    /// 同时接受字符串与字符串数组；`null` / 空串 → 空（= 不限制）。
    ///
    /// ⚠ 其余形状（数字、对象）**按类型错误上报，不静默吞掉**：用户把
    /// `"allowed_model": 123` 写进配置时，明确报错比「静默当成不限制」安全得多
    /// —— 后者会让用户以为限制生效了，实际网关放行一切（Go `config.go:597-599`
    /// 逐字记着这条）。
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        // 先看原始值形状：serde_json 下 `Value` 能区分数组/字符串/数字/对象。
        let v = serde_json::Value::deserialize(deserializer)?;
        match v {
            serde_json::Value::Null => Ok(Self(Vec::new())),
            serde_json::Value::String(s) => {
                // 空串视为「不限制」而不是「一个叫空串的模型」。
                if s.trim().is_empty() {
                    Ok(Self(Vec::new()))
                } else {
                    Ok(Self(vec![s]))
                }
            }
            serde_json::Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        serde_json::Value::String(s) => out.push(s),
                        // 数组里出现非字符串：与 Go `json.Unmarshal` 到 []string
                        // 的行为一致 —— 明确报错，不静默丢弃。
                        other => {
                            return Err(D::Error::custom(format!(
                                "pool.allowed_model: 需要字符串或字符串数组，数组元素不是字符串：{other}"
                            )))
                        }
                    }
                }
                Ok(Self(out))
            }
            other => Err(D::Error::custom(format!(
                "pool.allowed_model: 需要字符串或字符串数组，收到的是：{other}"
            ))),
        }
    }
}

/// 会话粘性配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SessionStickyConfig {
    /// 会话粘性开关，缺省 true。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 绑定 TTL 字符串，默认 `"30m"`。
    pub ttl: String,
    /// GC 周期字符串，默认 `"5m"`。
    #[serde(rename = "gc_interval")]
    pub gc_interval: String,
}

impl Default for SessionStickyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl: "30m".into(),
            gc_interval: "5m".into(),
        }
    }
}

impl Config {
    /// 从文件加载，再用环境变量覆盖，最后 normalize。
    pub fn load(path: &str) -> Result<Self> {
        let mut c = if path.is_empty() {
            Self::default()
        } else {
            let raw = std::fs::read(path)
                .map_err(|e| GatewayError::ConfigRead(format!("{path}: {e}")))?;
            // ★ 必须走 deserialize_config（内含 hoist_nested_keys）——
            // 直接 from_slice::<Self> 会让 pool./schedule. 下的键全部静默失效。
            deserialize_config(&raw)
                .map_err(|e| GatewayError::ConfigParse(format!("{path}: {e}")))?
        };
        c.apply_env();
        c.normalize()?;
        Ok(c)
    }

    /// 从 JSON 字节加载（测试用，不读文件系统、不读环境变量）。
    pub fn from_json(raw: &[u8]) -> Result<Self> {
        let mut c = deserialize_config(raw)
            .map_err(|e| GatewayError::ConfigParse(e.to_string()))?;
        c.normalize()?;
        Ok(c)
    }

    /// `WB2A_*` 环境变量覆盖。
    ///
    /// 与 Go 侧 `applyEnv` 一致：**只有非空且解析成功才覆盖**，
    /// 非法值静默保留原值（不报错），避免环境变量笔误导致网关起不来。
    pub fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("WB2A_LISTEN") {
            if !v.is_empty() {
                self.listen = v;
            }
        }
        if let Ok(v) = std::env::var("WB2A_API_KEY") {
            if !v.is_empty() {
                self.api_key = v;
            }
        }
        if let Ok(v) = std::env::var("WB2A_AUTH_DIR") {
            if !v.is_empty() {
                self.auth_dir = v;
            }
        }
        if let Ok(v) = std::env::var("WB2A_STATE_FILE") {
            if !v.is_empty() {
                self.state_file = v;
            }
        }
        if let Ok(v) = std::env::var("WB2A_SOFT_RATE") {
            if !v.is_empty() {
                self.cooldown.soft_rate = v;
            }
        }
        // 数值类：解析失败保持原值（与 Go 侧 strconv 失败不动字段一致）。
        Self::env_i64("WB2A_TIMEOUT_SECONDS", &mut self.upstream.timeout_seconds);
        Self::env_i64(
            "WB2A_HEADER_TIMEOUT_SECONDS",
            &mut self.upstream.header_timeout_seconds,
        );
        Self::env_i64(
            "WB2A_IDLE_TIMEOUT_SECONDS",
            &mut self.upstream.idle_timeout_seconds,
        );
        if let Ok(v) = std::env::var("WB2A_SANITIZE_FINGERPRINTS") {
            if let Ok(b) = v.parse::<bool>() {
                self.features.sanitize_blacklist_fingerprints = b;
            }
        }
        // prompt：与 Go `cmd/server/config.go:807-812` 同口径（非空才覆盖）。
        if let Ok(v) = std::env::var("WB2A_PROMPT_MODE") {
            if !v.is_empty() {
                self.prompt.mode = v;
            }
        }
        if let Ok(v) = std::env::var("WB2A_PROMPT_FILE") {
            if !v.is_empty() {
                self.prompt.file = v;
            }
        }
    }

    /// 读取整数环境变量，仅在存在且解析成功时写入。
    fn env_i64(key: &str, slot: &mut i64) {
        if let Ok(v) = std::env::var(key) {
            if let Ok(n) = v.trim().parse::<i64>() {
                *slot = n;
            }
        }
    }

    /// 解析 Go 风格的 duration 字符串（`"60s"` / `"1m30s"` / `"2h45m"` / `"100ms"` / `"0"`）。
    ///
    /// 与 Go `time.ParseDuration` 的语义对齐点：
    ///   - 必须有单位（裸 `"60"` 报错），唯一例外是 `"0"`（Go 允许零值无单位）；
    ///   - 支持复合表达式（按「数字 + 单位」分段累加）；
    ///   - 不支持小数秒以外的科学计数法，也不支持负时长（配置场景无意义）。
    ///
    /// 单位集合：`ns` / `us` / `µs` / `ms` / `s` / `m` / `h`。
    pub fn parse_go_duration(s: &str) -> Result<Duration> {
        let s = s.trim();
        if s.is_empty() {
            return Err(GatewayError::ConfigInvalid(format!("empty duration")));
        }
        // Go 允许 "0" 无单位（time.ParseDuration 的特例）。
        if s == "0" {
            return Ok(Duration::ZERO);
        }

        let bytes = s.as_bytes();
        let mut total_nanos: u128 = 0;
        let mut i = 0usize;
        let mut saw_unit = false;

        while i < bytes.len() {
            // 1) 读数字段（含可选前导 +/- 与小数点）
            let start = i;
            if matches!(bytes[i], b'+' | b'-') {
                i += 1;
            }
            let digits_start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            if i == digits_start {
                return Err(GatewayError::ConfigInvalid(format!(
                    "missing number at position {start} in {s:?}"
                )));
            }
            if i == start {
                return Err(GatewayError::ConfigInvalid(format!(
                    "missing number at position {start} in {s:?}"
                )));
            }
            let num_str = &s[start..i];

            // 2) 读单位段：优先匹配两字符单位，再退一字符
            let unit = if s[i..].starts_with("ns") {
                i += 2;
                "ns"
            } else if s[i..].starts_with("us") || s[i..].starts_with("µs") {
                // "µs" 是 2 字节 UTF-8，按字符切片处理
                if s[i..].starts_with("µs") {
                    i += "µs".len();
                    "us"
                } else {
                    i += 2;
                    "us"
                }
            } else if s[i..].starts_with("ms") {
                i += 2;
                "ms"
            } else {
                match bytes.get(i) {
                    Some(b's') => {
                        i += 1;
                        "s"
                    }
                    Some(b'm') => {
                        i += 1;
                        "m"
                    }
                    Some(b'h') => {
                        i += 1;
                        "h"
                    }
                    _ => {
                        return Err(GatewayError::ConfigInvalid(format!(
                            "missing or unknown unit at position {i} in {s:?}"
                        )));
                    }
                }
            };
            saw_unit = true;

            // 3) 累加
            let value: f64 = num_str.parse().map_err(|_| {
                GatewayError::ConfigInvalid(format!("bad number {num_str:?} in {s:?}"))
            })?;
            if value < 0.0 {
                return Err(GatewayError::ConfigInvalid(format!(
                    "negative duration {s:?}"
                )));
            }
            let nanos = unit_nanos(unit)
                .ok_or_else(|| {
                    GatewayError::ConfigInvalid(format!("unknown unit {unit:?} in {s:?}"))
                })?;
            let scaled = value * nanos as f64;
            if scaled < 0.0 || !scaled.is_finite() || scaled > u128::MAX as f64 {
                return Err(GatewayError::ConfigInvalid(format!(
                    "duration overflow {s:?}"
                )));
            }
            total_nanos += scaled.round() as u128;
        }

        if !saw_unit {
            return Err(GatewayError::ConfigInvalid(format!(
                "missing unit in duration {s:?}"
            )));
        }

        let nanos = u64::try_from(total_nanos).map_err(|_| {
            GatewayError::ConfigInvalid(format!("duration out of range {s:?}"))
        })?;
        Ok(Duration::from_nanos(nanos))
    }

    /// normalize：解析 duration、补默认值、校验合法性。
    ///
    /// 与 Go 侧 `Config.normalize` 逐条对应。
    pub fn normalize(&mut self) -> Result<()> {
        self.parsed.soft_rate =
            Self::parse_go_duration(&self.cooldown.soft_rate).map_err(|e| {
                GatewayError::ConfigInvalid(format!("cooldown.soft_rate: {e}"))
            })?;
        self.parsed.breaker_cooldown =
            Self::parse_go_duration(&self.pool.breaker_cooldown).map_err(|e| {
                GatewayError::ConfigInvalid(format!("pool.breaker_cooldown: {e}"))
            })?;
        self.parsed.breaker_cooldown_max =
            Self::parse_go_duration(&self.pool.breaker_cooldown_max).map_err(|e| {
                GatewayError::ConfigInvalid(format!("pool.breaker_cooldown_max: {e}"))
            })?;
        self.parsed.session_ttl =
            Self::parse_go_duration(&self.session_sticky.ttl).map_err(|e| {
                GatewayError::ConfigInvalid(format!("session_sticky.ttl: {e}"))
            })?;
        self.parsed.session_gc_interval =
            Self::parse_go_duration(&self.session_sticky.gc_interval).map_err(|e| {
                GatewayError::ConfigInvalid(format!("session_sticky.gc_interval: {e}"))
            })?;

        if self.pool.breaker_threshold <= 0 {
            self.pool.breaker_threshold = 3;
        }
        if self.pool.idle_weight_per_hour <= 0.0 {
            self.pool.idle_weight_per_hour = 0.5;
        }
        if self.pool.idle_weight_max <= 0.0 {
            self.pool.idle_weight_max = 5.0;
        }

        // 白名单归一化：去空白、剥产品/区域前缀、丢弃空项。
        //
        // 放在 normalize 里（而不是每次请求时算）有两个收益：请求路径上零分配；
        // 且**启动日志与拒绝文案看到的是同一份名单** —— 若两处各算一次，
        // 用户会看到「日志说放行了 a、b，被拒时文案却说 a」这类自相矛盾。
        //
        // ⚠ 归一化只在**解析层**做形状，前缀剥离复用 [`crate::model_allow`]：
        // 那里已经有一份实现，两边各写一套必然分叉（Go 侧注释逐字记着这条）。
        self.pool.allowed_model =
            crate::model_allow::normalize_allowed_models(&self.pool.allowed_model.0).into();

        // 积分巡检周期：空/非法一律回落默认（不报错，避免老 config 启动失败）。
        self.parsed.credit_refresh_interval =
            Self::parse_go_duration(&self.pool.credit_refresh_interval)
                .unwrap_or(Duration::from_secs(15 * 60));
        if self.parsed.credit_refresh_interval <= Duration::ZERO {
            self.parsed.credit_refresh_interval = Duration::from_secs(15 * 60);
        }

        if self.upstream.timeout_seconds <= 0 {
            self.upstream.timeout_seconds = 120;
        }
        if self.upstream.header_timeout_seconds <= 0 {
            self.upstream.header_timeout_seconds = self.upstream.timeout_seconds;
        }
        if self.upstream.idle_timeout_seconds <= 0 {
            self.upstream.idle_timeout_seconds = 300;
        }

        // 裸端口（无冒号）补成回环地址 —— 不能补成 `:{port}`（全网卡）。
        //
        // `:{port}` 会监听所有网卡，而 `api_key` 默认为空（= 不鉴权），
        // 两者叠加等于把本机账号额度暴露给整个局域网。
        if !self.listen.contains(':') {
            self.listen = format!("127.0.0.1:{}", self.listen);
        }

        // 空数组 = 未配置 → 回落默认。
        if self.schedule.checkin_hours.is_empty() {
            self.schedule.checkin_hours = vec![9, 21];
        }
        if self.schedule.keepalive_hours.is_empty() {
            self.schedule.keepalive_hours = vec![22];
        }
        // 保活兜底：负数无意义（会被当成巨大的超期窗口 ⇒ 每次启动全量刷新），
        // 归一为 0（= 关闭兜底）；上界不设 —— 365 天也是用户的合法选择。
        if self.schedule.keepalive_max_age_hours < 0 {
            self.schedule.keepalive_max_age_hours = 0;
        }
        // 四个养号任务的时点：空数组 = 未配置 → 回落默认（与 Go `scheduler.New`
        // 的兜底逐字一致）。
        //
        // ★ 与 `checkin_hours` 的区别：这些键的默认值在 Go 里是**调度器构造时**
        // 兜的（`scheduler.New`），不是配置层。放在这里兜是因为 Rust 侧调度器
        // 同样会兜一遍 —— 两处都兜不会冲突（幂等），而配置层兜过之后
        // `/status` 与启动日志能显示真实生效的时点，不必让运维去猜。
        if self.schedule.activity_hours.is_empty() {
            self.schedule.activity_hours = vec![10];
        }
        if self.schedule.nightowl_hours.is_empty() {
            self.schedule.nightowl_hours = vec![1];
        }
        if self.schedule.school_hours.is_empty() {
            self.schedule.school_hours = vec![12];
        }
        if self.schedule.trial_hours.is_empty() {
            self.schedule.trial_hours = vec![9, 21];
        }
        // 条数：`<= 0` 回落 3（调度器侧还有一层兜底，见 `ScheduleConfig` 的说明）。
        if self.schedule.activity_report_count <= 0 {
            self.schedule.activity_report_count = crate::scheduler::DEFAULT_ACTIVITY_REPORT_COUNT;
        }
        self.schedule.checkin_scope = normalize_checkin_scope(&self.schedule.checkin_scope);

        self.validate_schedule_hours()?;
        self.normalize_multi_product();
        self.normalize_prompt()?;
        Ok(())
    }

    /// `prompt` 块的归一化：校验模式、按需读盘并缓存提示词文本。
    ///
    /// 与 Go `cmd/server/config.go:931-958 func (c *Config) normalizePrompt() error`
    /// 逐条对应（含错误文案口径）。
    fn normalize_prompt(&mut self) -> Result<()> {
        // mode 只接受 custom / passthrough / append（大小写与首尾空白不敏感）；
        // 其余值**启动即报错**，而不是静默回落到某一个分支 —— 用户把 "costom"
        // 拼错时，若静默按 passthrough 跑，表现为「按文档配了定制提示词却完全没
        // 生效」，是最难排查的一类配置错误。
        let mode = self.prompt.mode.trim().to_lowercase();
        let mode = match mode.as_str() {
            // 空串 = 未配置 → passthrough（老配置没有 prompt 块）。
            "" | crate::prompt::MODE_PASSTHROUGH => crate::prompt::MODE_PASSTHROUGH,
            crate::prompt::MODE_CUSTOM => crate::prompt::MODE_CUSTOM,
            crate::prompt::MODE_APPEND => crate::prompt::MODE_APPEND,
            _ => {
                return Err(GatewayError::ConfigInvalid(format!(
                    "prompt.mode: {:?} 不是合法值（passthrough / custom / append）",
                    self.prompt.mode
                )))
            }
        };
        self.parsed.prompt_mode = mode.to_string();
        // 回写规范化后的值：`/status` 与启动日志看到的是同一份，避免「日志说
        // passthrough、行为却按 Custom 跑」这类自相矛盾。
        self.prompt.mode = mode.to_string();

        // ⚠ 只有 passthrough 不需要提示词文本。这里**不能**写成「只有 custom 才读」
        // —— 加 append 后那样会让 append 模式下 PromptText 被清空，配置看起来生效了、
        // 实际什么都没追加（静默失效，最难查的一类）。
        if mode == crate::prompt::MODE_PASSTHROUGH {
            self.parsed.prompt_text = String::new();
            return Ok(());
        }

        self.parsed.prompt_text = crate::prompt::load(self.prompt.file.trim())?;
        Ok(())
    }

    /// 多产品相关键的回落。
    ///
    /// 口径与 Go 侧 `cmd/server/config.go` 一致：
    /// - 目录类：空 = 回落宿主默认目录（**不是**「禁用」）；
    /// - 周期类：空/非法一律回落默认，**不报错**（与 `credit_refresh_interval`
    ///   同一口径，避免一个调优键把网关拦停）；
    /// - `<=0` 的整数：视为未设置。
    fn normalize_multi_product(&mut self) {
        // 凭证目录：空 = 回落默认目录。
        //
        // ★ 这里的默认值**必须**与宿主 `qoder_account::auth_dir()` /
        // `zcode_account::auth_dir()` / `trae_account::auth_dir()` 一致。宿主在
        // 生产里总是显式写出这三个键（`gateway.rs:2777-2779`），所以本回落只在
        // 手工调试（无宿主）时生效；一旦分叉就会出现「界面里登录成功、网关看不到账号」。
        if self.qoder_auth_dir.trim().is_empty() {
            self.qoder_auth_dir = default_qoder_auth_dir();
        }
        if self.zcode_auth_dir.trim().is_empty() {
            self.zcode_auth_dir = default_zcode_auth_dir();
        }
        if self.trae_auth_dir.trim().is_empty() {
            self.trae_auth_dir = default_trae_auth_dir();
        }

        // 轮询周期：空/非法一律回落 5s，不报错。
        let iv = Self::parse_go_duration(&self.watch_auth_dir_interval)
            .unwrap_or_else(|_| Duration::from_secs(5));
        self.parsed.watch_auth_dir_interval = if iv <= Duration::ZERO {
            Duration::from_secs(5)
        } else {
            iv
        };

        // 领取时点：空数组 = 未配置 → 回落 [10, 21]。
        if self.qoder_claim_hours.is_empty() {
            self.qoder_claim_hours = vec![10, 21];
        }
        if self.qoder_claim_interval_minutes <= 0 {
            self.qoder_claim_interval_minutes = default_qoder_claim_interval_minutes();
        }

        // 平台总开关：平台名 trim + 小写、空键跳过、**只保留显式 `false` 的项**。
        //
        // ★ 与 `platform_models_disabled` 同一口径（区分「没配」与「配了个空的」）：
        // `true` 与「键不存在」同义，留着只会让「全量还是增量」难以分辨；宿主每次保存
        // 都会整份下发，这里归一化一次，保证池里那份与配置文件里那份逐字一致。
        // 全部平台都开着 ⇒ 回落 `None`（= 用户没关任何平台），与引入该键前逐字不变。
        if let Some(raw) = self.platform_enabled.take() {
            let mut norm: HashMap<String, bool> = HashMap::new();
            for (k, v) in raw {
                let plat = k.trim().to_ascii_lowercase();
                if plat.is_empty() || v {
                    continue;
                }
                norm.insert(plat, false);
            }
            self.platform_enabled = if norm.is_empty() { None } else { Some(norm) };
        }
    }

    /// 分产品「自动领取」是否生效。
    ///
    /// ★ 两级回落，与 Go 侧 `*bool` 语义一致：分产品开关**未显式配置**
    /// （`None`）时回落总闸 `product_tasks_enabled`；显式 `false` 是用户明确要关。
    pub fn qoder_claim_active(&self) -> bool {
        self.qoder_claim_enabled.unwrap_or(self.product_tasks_enabled)
    }

    /// ZCode 自动领取是否生效，语义同 [`Config::qoder_claim_active`]。
    pub fn zcode_claim_active(&self) -> bool {
        self.zcode_claim_enabled.unwrap_or(self.product_tasks_enabled)
    }

    /// ZCode 验证码求解是否参与请求。
    ///
    /// ★ **不读 `zcode_captcha_enabled`**（恒为 true）—— 见该字段文档：
    /// 2026-09-20 起该功能始终开启，字段只为向后兼容与诊断对照保留。
    /// 是否**真的能**求解取决于有没有配置求解器（目录或外部服务），
    /// 见 [`Config::zcode_captcha_available`]。
    pub fn zcode_captcha_active(&self) -> bool {
        true
    }

    /// 是否有可用的验证码求解器（本地目录或外部服务二选一）。
    ///
    /// ★ 外部**优先**（真实 WebView2，实测 929ms，成功率远高于本地 Node 的 40~88%），
    /// 本地 Node 作为回退。
    pub fn zcode_captcha_available(&self) -> bool {
        !self.zcode_captcha_solver_url.trim().is_empty()
            || !self.zcode_captcha_dir.trim().is_empty()
    }

    /// 平台 × 区域 → 模型是否被用户**显式禁用**（否决项，先于并集判定）。
    ///
    /// 区域键 `""` 表示「该平台不分区域」（简单用法的兼容路径）。
    /// ★ 平台不在 map 里 / 该区域没配 / 清单为空 ⇒ **不否决**
    /// （空清单的语义是「没有禁用任何模型」，不是「全禁」）。
    pub fn platform_model_disabled(&self, platform: &str, region: &str, model: &str) -> bool {
        let Some(all) = self.platform_models_disabled.as_ref() else {
            return false;
        };
        let Some(by_region) = all.get(platform) else {
            return false;
        };
        // 先看精确区域，再看「不分区域」的 `""` 键。
        for key in [region, ""] {
            if let Some(models) = by_region.get(key) {
                if models.iter().any(|m| m == model) {
                    return true;
                }
            }
        }
        false
    }

    /// 平台是否参加兼容网关路由（缺省 **true**）。
    ///
    /// ★ 语义 = 账号级 `no_route` 的**平台级版本**：只把该平台排除在**选号**之外，
    /// 签到 / 上报 / 成长任务 / 额度巡检照常运行（所有者原话「关闭平台只是不参与兼容
    /// 网关,并不是不执行任务请注意」）。
    ///
    /// ★ 平台不在 map 里 / 没配这个键 ⇒ **参加**（向后兼容：老配置没有这个键，
    /// 升级后行为必须逐字不变）。
    pub fn platform_enabled_active(&self, platform: &str) -> bool {
        let plat = platform.trim().to_ascii_lowercase();
        if plat.is_empty() {
            return true;
        }
        match self.platform_enabled.as_ref() {
            Some(m) => m.get(&plat).copied().unwrap_or(true),
            None => true,
        }
    }

    /// 校验排程小时落在 0-23，错误信息指向正确的开关（与 Go 侧一致）。
    fn validate_schedule_hours(&self) -> Result<()> {
        check_hour_range("schedule.checkin_hours", "checkin_enabled", &self.schedule.checkin_hours)?;
        check_hour_range(
            "schedule.keepalive_hours",
            "keepalive_enabled",
            &self.schedule.keepalive_hours,
        )
    }

    /// 签到作用域是否覆盖全部区域。
    pub fn checkin_scope_all(&self) -> bool {
        normalize_checkin_scope(&self.schedule.checkin_scope) == "all"
    }
}

/// 归一化签到区域范围；无法识别一律回落 `"cn"`。
pub fn normalize_checkin_scope(v: &str) -> String {
    if v.trim().eq_ignore_ascii_case("all") {
        return "all".into();
    }
    "cn".into()
}

/// 校验小时范围，非法时报错并提示改用显式开关。
fn check_hour_range(field: &str, switch_key: &str, hours: &[i32]) -> Result<()> {
    for &h in hours {
        if !(0..=23).contains(&h) {
            return Err(GatewayError::ConfigInvalid(format!(
                "{field}: {h} 不是合法小时（0-23）；如要关闭该任务请设 schedule.{switch_key}=false"
            )));
        }
    }
    Ok(())
}

/// Go duration 单位对应的纳秒数。
fn unit_nanos(unit: &str) -> Option<u128> {
    Some(match unit {
        "ns" => 1,
        "us" | "µs" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60 * 1_000_000_000,
        "h" => 60 * 60 * 1_000_000_000,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_go_default() {
        let c = Config::default();
        // 默认只绑回环：api_key 缺省为空（不鉴权），全网卡监听会暴露账号额度
        assert_eq!(c.listen, "127.0.0.1:7863");
        assert_eq!(c.auth_dir, "./auths");
        assert_eq!(c.state_file, "./data/state.json");
        assert_eq!(c.cooldown.soft_rate, "60s");
        assert_eq!(c.schedule.checkin_hours, vec![9, 21]);
        assert_eq!(c.schedule.keepalive_hours, vec![22]);
        // ★ 保活兜底必须有默认值：缺了它，「22:00 网关没在跑」就永远补不回来。
        assert_eq!(c.schedule.keepalive_max_age_hours, 24);
        assert!(c.schedule.checkin_enabled);
        assert!(c.schedule.keepalive_enabled);
        assert_eq!(c.schedule.checkin_scope, "cn");
        assert_eq!(c.upstream.timeout_seconds, 120);
        assert!(c.features.sanitize_blacklist_fingerprints);
        assert_eq!(c.pool.max_in_flight, 3);
        assert_eq!(c.pool.breaker_threshold, 3);
        assert_eq!(c.pool.breaker_cooldown, "30m");
        assert_eq!(c.pool.breaker_cooldown_max, "6h");
        assert!((c.pool.idle_weight_per_hour - 0.5).abs() < f64::EPSILON);
        assert!((c.pool.idle_weight_max - 5.0).abs() < f64::EPSILON);
        assert!(c.pool.credit_refresh_enabled);
        assert!(c.session_sticky.enabled);
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(
            Config::parse_go_duration("60s").unwrap(),
            Duration::from_secs(60)
        );
        assert_eq!(
            Config::parse_go_duration("30m").unwrap(),
            Duration::from_secs(1800)
        );
        assert_eq!(
            Config::parse_go_duration("6h").unwrap(),
            Duration::from_secs(21600)
        );
        assert_eq!(
            Config::parse_go_duration("0").unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            Config::parse_go_duration("1m30s").unwrap(),
            Duration::from_secs(90)
        );
        assert_eq!(
            Config::parse_go_duration("2h45m").unwrap(),
            Duration::from_secs(9900)
        );
        assert_eq!(
            Config::parse_go_duration("100ms").unwrap(),
            Duration::from_millis(100)
        );
        // 缺少单位 → 报错（与 Go time.ParseDuration 一致）
        assert!(Config::parse_go_duration("60").is_err());
        assert!(Config::parse_go_duration("abc").is_err());
        assert!(Config::parse_go_duration("").is_err());
    }

    #[test]
    fn normalize_fills_defaults_and_falls_back() {
        let raw = br#"{"listen":"7863"}"#;
        let c = Config::from_json(raw).unwrap();
        // 裸端口补回环前缀（不是全网卡的 ":"）
        assert_eq!(c.listen, "127.0.0.1:7863");
        // 开关缺席 → true
        assert!(c.schedule.checkin_enabled);
        assert!(c.pool.credit_refresh_enabled);
        // header 回落 timeout
        assert_eq!(c.upstream.header_timeout_seconds, 120);
        // idle 回落 300
        assert_eq!(c.upstream.idle_timeout_seconds, 300);
        // duration 解析
        assert_eq!(c.parsed.soft_rate, Duration::from_secs(60));
        assert_eq!(c.parsed.breaker_cooldown, Duration::from_secs(1800));
        assert_eq!(c.parsed.breaker_cooldown_max, Duration::from_secs(21600));
        assert_eq!(c.parsed.session_ttl, Duration::from_secs(1800));
        assert_eq!(c.parsed.session_gc_interval, Duration::from_secs(300));
        assert_eq!(c.parsed.credit_refresh_interval, Duration::from_secs(900));
    }

    #[test]
    fn explicit_false_disables_switch() {
        let raw = br#"{"schedule":{"checkin_enabled":false,"keepalive_enabled":false},"features":{"sanitize_blacklist_fingerprints":false}}"#;
        let c = Config::from_json(raw).unwrap();
        assert!(!c.schedule.checkin_enabled);
        assert!(!c.schedule.keepalive_enabled);
        assert!(!c.features.sanitize_blacklist_fingerprints);
        // 显式 false 不应把小时也清掉（默认仍补齐）
        assert_eq!(c.schedule.checkin_hours, vec![9, 21]);
    }

    #[test]
    fn empty_array_falls_back_to_default_hours() {
        let raw = br#"{"schedule":{"checkin_hours":[],"keepalive_hours":[]}}"#;
        let c = Config::from_json(raw).unwrap();
        assert_eq!(c.schedule.checkin_hours, vec![9, 21]);
        assert_eq!(c.schedule.keepalive_hours, vec![22]);
    }

    #[test]
    fn checkin_scope_normalization() {
        let c = Config::from_json(br#"{"schedule":{"checkin_scope":"ALL"}}"#).unwrap();
        assert_eq!(c.schedule.checkin_scope, "all");
        assert!(c.checkin_scope_all());
        let c2 = Config::from_json(br#"{"schedule":{"checkin_scope":"weird"}}"#).unwrap();
        assert_eq!(c2.schedule.checkin_scope, "cn");
        assert!(!c2.checkin_scope_all());
    }

    #[test]
    fn invalid_hours_rejected_with_switch_hint() {
        let raw = br#"{"schedule":{"checkin_hours":[-1]}}"#;
        let err = Config::from_json(raw).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("不是合法小时"), "msg={msg}");
        assert!(msg.contains("checkin_enabled"), "应提示正确开关: {msg}");

        let raw = br#"{"schedule":{"keepalive_hours":[24]}}"#;
        let err = Config::from_json(raw).unwrap_err();
        assert!(err.to_string().contains("keepalive_enabled"));
    }

    /// ★ 2026-10-08：保活兜底的小时数只做单向归一（负数 → 0 = 关闭）。
    ///
    /// # 为什么不能在验证层拒绝负数
    ///
    /// `keepalive_max_age_hours` 与 `keepalive_hours` 是两回事：后者是**时点**，
    /// 越界只能报错（`[24]` 不是小时）；前者是一个**时长**，0 是它的合法取值
    ///（= 关闭兜底）。负数没有意义，但把它当成「非法」会让整份配置拒绝启动 ——
    /// 一个畸形的兜底窗口不该让网关起不来。
    ///
    /// 而若把负数**原样放过**，`as_secs()` 转换后是一个天文数字，判据会变成
    /// 「每次启动都对全部账号刷一遍」—— 那是对上游的真实骚扰。故归一到 0。
    #[test]
    fn keepalive_max_age_negative_clamps_to_disabled() {
        let raw = br#"{"schedule":{"keepalive_max_age_hours":-5}}"#;
        let c = Config::from_json(raw).unwrap();
        assert_eq!(c.schedule.keepalive_max_age_hours, 0, "负数应归一为 0（关闭兜底）");
        // 0 本身必须能原样读回（它与「缺键」不同：缺键是 24）。
        let raw = br#"{"schedule":{"keepalive_max_age_hours":0}}"#;
        let c = Config::from_json(raw).unwrap();
        assert_eq!(c.schedule.keepalive_max_age_hours, 0);
        // 缺键 ⇒ 默认 24（不是 0）—— 否则升级上来的用户永远不补跑。
        let c = Config::from_json(br#"{"schedule":{}}"#).unwrap();
        assert_eq!(c.schedule.keepalive_max_age_hours, 24, "缺键必须回落 24，否则兜底形同不存在");
    }

    #[test]
    fn invalid_duration_rejected_with_field_name() {
        let raw = br#"{"cooldown":{"soft_rate":"not-a-duration"}}"#;
        let err = Config::from_json(raw).unwrap_err();
        assert!(err.to_string().contains("cooldown.soft_rate"), "{}", err);
    }

    #[test]
    fn bad_json_returns_parse_error() {
        let err = Config::from_json(b"{not json").unwrap_err();
        assert!(matches!(err, GatewayError::ConfigParse(_)), "{err:?}");
    }

    #[test]
    fn less_than_zero_ints_treated_as_unset() {
        let raw = br#"{"upstream":{"timeout_seconds":-5},"pool":{"breaker_threshold":0,"idle_weight_per_hour":0}}"#;
        let c = Config::from_json(raw).unwrap();
        assert_eq!(c.upstream.timeout_seconds, 120);
        assert_eq!(c.pool.breaker_threshold, 3);
        assert!((c.pool.idle_weight_per_hour - 0.5).abs() < f64::EPSILON);
    }

    // ═══════════════ pool./schedule. 嵌套键提升（2026-10-01 实机缺陷回归）═══════════════
    //
    // 背景：宿主与 Go 把 `multi_product` 等键写在 `pool.` / `schedule.` 之下，
    // 而 Rust 把它们声明在顶层 ⇒ 宿主生成的配置下这些键**全部读不到**，
    // 网关恒为 `multi_product=false`（只跑 WorkBuddy）且日志无任何报错。
    // 实机复现：`o.log` 打出「多产品路由已关闭（pool.multi_product=false）」，
    // 而该配置文件里明明写着 `"pool": {"multi_product": true}`。

    /// 宿主写出的真实形状：`pool.multi_product=true` 必须被读到。
    #[test]
    fn hoists_pool_nested_keys_from_host_config() {
        let raw = br#"{
            "pool": {
                "multi_product": true,
                "qoder_auth_dir": "Q:/qoder",
                "zcode_auth_dir": "Q:/zcode",
                "trae_auth_dir": "Q:/trae",
                "watch_auth_dir": false,
                "watch_auth_dir_interval": "2s",
                "product_models": {"qoder": ["qwen3.8-max"]},
                "model_platforms": {"glm-5.2": ["zcode"]},
                "platform_models": {"qoder": {"cn": ["Qwen3.8-Flash"]}},
                "platform_models_disabled": {"qoder": {"intl": ["deepseek-v4.1-flash"]}},
                "zcode_captcha_dir": "Q:/captcha",
                "zcode_captcha_solver_url": "http://127.0.0.1:1/solve",
                "zcode_captcha_solver_token": "tok",
                "wb_device_token_url": "http://127.0.0.1:2/dev",
                "wb_device_token_token": "dtok"
            }
        }"#;
        let c = Config::from_json(raw).unwrap();
        // ★ 这一条就是实机缺陷的断言：读不到会退回缺省 false。
        assert!(c.multi_product, "pool.multi_product 必须被提升到顶层");
        assert_eq!(c.qoder_auth_dir, "Q:/qoder");
        assert_eq!(c.zcode_auth_dir, "Q:/zcode");
        assert_eq!(c.trae_auth_dir, "Q:/trae");
        assert!(!c.watch_auth_dir, "显式 false 必须生效（缺省是 true）");
        assert_eq!(c.watch_auth_dir_interval, "2s");
        assert_eq!(c.product_models.get("qoder").unwrap(), &vec!["qwen3.8-max".to_string()]);
        assert_eq!(c.model_platforms.get("glm-5.2").unwrap(), &vec!["zcode".to_string()]);
        assert_eq!(
            c.platform_models.get("qoder").unwrap().get("cn").unwrap(),
            &vec!["Qwen3.8-Flash".to_string()]
        );
        assert!(c.platform_model_disabled("qoder", "intl", "deepseek-v4.1-flash"));
        assert_eq!(c.zcode_captcha_dir, "Q:/captcha");
        assert_eq!(c.zcode_captcha_solver_url, "http://127.0.0.1:1/solve");
        assert_eq!(c.zcode_captcha_solver_token, "tok");
        assert_eq!(c.wb_device_token_url, "http://127.0.0.1:2/dev");
        assert_eq!(c.wb_device_token_token, "dtok");
    }

    /// `schedule.` 下的 5 个键同样要被提升。
    #[test]
    fn hoists_schedule_nested_keys_from_host_config() {
        let raw = br#"{
            "schedule": {
                "product_tasks_enabled": false,
                "qoder_claim_enabled": true,
                "zcode_claim_enabled": false,
                "qoder_claim_hours": [10, 21],
                "qoder_claim_interval_minutes": 45
            }
        }"#;
        let c = Config::from_json(raw).unwrap();
        // 总闸显式 false 必须生效 —— 否则用户「关掉自动领取」在界面上无效。
        assert!(!c.product_tasks_enabled, "schedule.product_tasks_enabled 必须被提升");
        assert_eq!(c.qoder_claim_enabled, Some(true));
        assert_eq!(c.zcode_claim_enabled, Some(false));
        assert_eq!(c.qoder_claim_hours, vec![10, 21]);
        assert_eq!(c.qoder_claim_interval_minutes, 45);
    }

    /// ★ 顶层优先：两级都写时以顶层为准（不能反向覆盖用户/老配置的显式意图）。
    #[test]
    fn top_level_wins_over_nested() {
        let raw = br#"{
            "multi_product": true,
            "watch_auth_dir_interval": "9s",
            "pool": {
                "multi_product": false,
                "watch_auth_dir_interval": "1s"
            }
        }"#;
        let c = Config::from_json(raw).unwrap();
        assert!(c.multi_product, "顶层 true 不该被 pool 下的 false 覆盖");
        assert_eq!(c.watch_auth_dir_interval, "9s");
    }

    /// 顶层显式 `null` 视为「未配置」，此时嵌套值仍然生效。
    ///
    /// 放宽的理由：顶层写 `null` 在今天的实现里会直接解析失败（`bool`/`String`
    /// 不接受 null），没有依赖该行为的配置；而 `Option` 字段（`qoder_claim_enabled`）
    /// 上 `null` 的语义本来就是「没配」。
    #[test]
    fn null_at_top_level_falls_through_to_nested() {
        let raw = br#"{
            "qoder_claim_enabled": null,
            "schedule": {"qoder_claim_enabled": true}
        }"#;
        let c = Config::from_json(raw).unwrap();
        assert_eq!(c.qoder_claim_enabled, Some(true));
    }

    /// 老配置（顶层写法）必须逐字不变 —— 这是「两级都认」而非「改层级」的理由。
    #[test]
    fn top_level_only_config_still_works() {
        let raw = br#"{
            "multi_product": true,
            "qoder_auth_dir": "T:/qoder",
            "watch_auth_dir_interval": "7s",
            "product_tasks_enabled": false
        }"#;
        let c = Config::from_json(raw).unwrap();
        assert!(c.multi_product);
        assert_eq!(c.qoder_auth_dir, "T:/qoder");
        assert_eq!(c.watch_auth_dir_interval, "7s");
        assert!(!c.product_tasks_enabled);
    }

    /// `pool` / `schedule` 不是对象（缺失 / `null` / 数组 / 字符串）时静默跳过。
    ///
    /// ★ 直接测 `hoist_nested_keys` 而不是走 `from_json`：`{"pool":null}` 在
    /// `serde` 阶段就会失败（`PoolConfig` 不是 `Option`，不接受 null），那是
    /// **既有**行为，与本函数无关。本函数只需保证自己**不 panic、不改动**。
    #[test]
    fn hoist_is_noop_for_non_object_nesting() {
        for raw in [
            r#"{"multi_product":true}"#,
            r#"{"pool":null,"multi_product":true}"#,
            r#"{"pool":[1,2,3],"multi_product":true}"#,
            r#"{"pool":"nope","multi_product":true}"#,
            r#"{"schedule":7}"#,
            r#"{"schedule":null}"#,
            r#"[1,2,3]"#,
            r#""just a string""#,
            r#"null"#,
        ] {
            let mut v: serde_json::Value = serde_json::from_str(raw).unwrap();
            let before = v.clone();
            hoist_nested_keys(&mut v);
            assert_eq!(v, before, "非对象嵌套必须原样不动：{raw}");
        }
    }

    /// 提升是**搬走**而不是复制：嵌套对象里不该留下同名键。
    ///
    /// 否则 `PoolConfig` 将来若新增同名字段，会同时收到两个来源的值，
    /// 出现「改了顶层不生效」这种极难排查的行为。
    #[test]
    fn hoist_removes_key_from_nested_object() {
        let mut v: serde_json::Value =
            serde_json::from_str(r#"{"pool":{"multi_product":true,"max_in_flight":9}}"#).unwrap();
        hoist_nested_keys(&mut v);
        assert_eq!(v["multi_product"], serde_json::json!(true));
        assert!(v["pool"].get("multi_product").is_none(), "嵌套里不该留副本");
        assert_eq!(v["pool"]["max_in_flight"], serde_json::json!(9), "无关键必须原样保留");
    }

    /// 宿主实际写出的完整 `pool` 块（键名取自实机 `gateway_native_config.json`）
    /// 必须能被解析，且其中的 15 个提升键全部生效。
    #[test]
    fn real_host_pool_block_parses() {
        let raw = br#"{
            "listen": "127.0.0.1:7863",
            "pool": {
                "allowed_model": [],
                "breaker_cooldown": "30m",
                "breaker_cooldown_max": "6h",
                "breaker_threshold": 3,
                "idle_weight_max": 5,
                "idle_weight_per_hour": 0.5,
                "max_in_flight": 3,
                "model_platforms": {},
                "multi_product": true,
                "platform_models": {},
                "platform_models_disabled": {},
                "product_models": {},
                "qoder_auth_dir": "C:/Users/x/.wb-switch/qoder/auths",
            "trae_auth_dir": "C:/Users/x/.wb-switch/trae/auths",
                "rotation": false,
                "watch_auth_dir": true,
                "watch_auth_dir_interval": "5s",
                "wb_device_token_token": "",
                "wb_device_token_url": "",
                "zcode_auth_dir": "C:/Users/x/.wb-switch/zcode/auths",
                "zcode_captcha_dir": "C:/Users/x/_up_/assets/zcode-captcha",
                "zcode_captcha_enabled": true,
                "zcode_captcha_solver_token": "",
                "zcode_captcha_solver_url": ""
            }
        }"#;
        let c = Config::from_json(raw).unwrap();
        assert!(c.multi_product, "实机 pool 块里 multi_product=true 必须生效");
        assert_eq!(c.qoder_auth_dir, "C:/Users/x/.wb-switch/qoder/auths");
        assert_eq!(c.zcode_auth_dir, "C:/Users/x/.wb-switch/zcode/auths");
        assert_eq!(c.trae_auth_dir, "C:/Users/x/.wb-switch/trae/auths");
        assert_eq!(c.zcode_captcha_dir, "C:/Users/x/_up_/assets/zcode-captcha");
        assert!(c.watch_auth_dir);
        // 未被提升的键仍由 PoolConfig 正常消费。
        assert_eq!(c.pool.max_in_flight, 3);
        assert_eq!(c.pool.breaker_threshold, 3);
    }

    // ═══════════════ prompt 块（系统提示词注入）═══════════════
    //
    // 对应 Go `cmd/server/config.go:931-958 func (c *Config) normalizePrompt() error`。
    // ★ 这些用例是 Go 侧 `prompt_rewrite_test.go` **覆盖不到**的一层：那里直接构造
    // handler（绕过配置解析），验证的是「给定 PromptText 时出站字节对不对」；而
    // 「配置能不能正确变成 PromptText」只能在配置层验。

    /// 老配置（没有 `prompt` 块）必须缺省 passthrough。
    ///
    /// 宿主侧同口径断言：`ai-gateway-core` 的 `gateway.rs:7259`「缺省必须是
    /// passthrough：缺省 custom 会在升级后静默替换用户的 system」。
    #[test]
    fn prompt_defaults_to_passthrough() {
        let c = Config::from_json(br#"{}"#).unwrap();
        assert_eq!(c.prompt.mode, crate::prompt::MODE_PASSTHROUGH);
        assert!(c.prompt.file.is_empty());
        // ★ 显式断言：缺省下 `prompt_text` 必须是空的 —— 它为空正是 `run_forward`
        // 走零成本分支（完全不调用改写函数）的判据。
        assert!(
            c.parsed.prompt_text.is_empty(),
            "passthrough 下 prompt_text 必须为空（否则 run_forward 会误改写）"
        );
        assert_eq!(c.parsed.prompt_mode, crate::prompt::MODE_PASSTHROUGH);
    }

    /// `prompt.mode` 大小写与首尾空白不敏感；非法值**启动即报错**。
    #[test]
    fn prompt_mode_is_normalized_and_invalid_rejected() {
        for raw_mode in ["custom", "CUSTOM", "  Custom  ", "cUsToM"] {
            let raw = format!(r#"{{"prompt":{{"mode":{raw_mode:?},"file":""}}}}"#);
            let c = Config::from_json(raw.as_bytes()).unwrap();
            assert_eq!(
                c.parsed.prompt_mode,
                crate::prompt::MODE_CUSTOM,
                "{raw_mode:?} 应归一化为 custom"
            );
            // 回写规范化值：`/status` 与启动日志看到的应与实际行为一致。
            assert_eq!(c.prompt.mode, crate::prompt::MODE_CUSTOM);
        }
        for raw_mode in ["append", "APPEND", " Append "] {
            let raw = format!(r#"{{"prompt":{{"mode":{raw_mode:?},"file":""}}}}"#);
            let c = Config::from_json(raw.as_bytes()).unwrap();
            assert_eq!(c.parsed.prompt_mode, crate::prompt::MODE_APPEND);
        }
        // 空串 = 未配置 → passthrough（不是报错）。
        let c = Config::from_json(br#"{"prompt":{"mode":"","file":""}}"#).unwrap();
        assert_eq!(c.parsed.prompt_mode, crate::prompt::MODE_PASSTHROUGH);

        // 拼错时必须报错，**不能**静默回落 passthrough —— 否则表现为「按文档配了
        // 定制提示词却完全没生效」，是最难排查的一类配置错误。
        let err = Config::from_json(br#"{"prompt":{"mode":"costom","file":""}}"#).unwrap_err();
        let msg = err.to_string();
        assert!(
            matches!(err, GatewayError::ConfigInvalid(_)),
            "应为 ConfigInvalid，实际 {err:?}"
        );
        assert!(msg.contains("prompt.mode"), "错误文案应点名 prompt.mode：{msg}");
        assert!(msg.contains("costom"), "错误文案应回显用户输入：{msg}");
    }

    /// `custom` / `append` 模式读文件；空 `file` 回落内置默认。
    #[test]
    fn prompt_custom_reads_file_and_empty_file_uses_builtin() {
        let d = std::env::temp_dir().join(format!("wbswg-cfgprompt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("sys.md");
        std::fs::write(&f, "你是一名工程助手。\n\n- 保持简洁。\n").unwrap();
        let fwd = f.to_string_lossy().replace('\\', "/");

        // 指定文件 ⇒ 按原文读入（不 TrimSpace）。
        let raw = format!(
            r#"{{"prompt":{{"mode":"custom","file":{fwd:?}}}}}"#
        );
        let c = Config::from_json(raw.as_bytes()).unwrap();
        assert_eq!(c.parsed.prompt_text, "你是一名工程助手。\n\n- 保持简洁。\n");

        // ★ append 模式**同样**要读文件。这里是 Go 侧踩过的坑：原先写的是
        // `if mode != "custom" { PromptText = "" }`，加 append 后 append 模式下
        // 提示词被清空 —— 配置看起来生效了、实际什么都没追加（静默失效）。
        let raw = format!(
            r#"{{"prompt":{{"mode":"append","file":{fwd:?}}}}}"#
        );
        let c = Config::from_json(raw.as_bytes()).unwrap();
        assert_eq!(
            c.parsed.prompt_text, "你是一名工程助手。\n\n- 保持简洁。\n",
            "append 模式下 prompt_text 被清空 = 静默失效"
        );

        // 空 file ⇒ 内置默认提示词。
        let c = Config::from_json(br#"{"prompt":{"mode":"custom","file":""}}"#).unwrap();
        assert_eq!(c.parsed.prompt_text, crate::prompt::default_prompt());
        assert!(!c.parsed.prompt_text.is_empty());

        std::fs::remove_dir_all(&d).ok();
    }

    /// `custom` 模式下文件读不到 / 内容为空 ⇒ 启动即报错（fail fast）。
    ///
    /// ★ 但 passthrough 下**即使 file 填错也不该拦住启动** —— 那段文本根本不会被
    /// 使用（Go `config.go:189-192` 逐字）。
    #[test]
    fn prompt_custom_fails_fast_but_passthrough_ignores_bad_file() {
        let missing = "Q:/definitely/not/here/sys.md";
        let raw = format!(r#"{{"prompt":{{"mode":"custom","file":{missing:?}}}}}"#);
        let err = Config::from_json(raw.as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains("prompt.file"),
            "读失败应点名 prompt.file：{err}"
        );

        let d = std::env::temp_dir().join(format!("wbswg-cfgprompt2-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let empty = d.join("empty.md");
        std::fs::write(&empty, "   \n\t\n").unwrap();
        let ep = empty.to_string_lossy().replace('\\', "/");
        let raw = format!(r#"{{"prompt":{{"mode":"custom","file":{ep:?}}}}}"#);
        let err = Config::from_json(raw.as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains("prompt.file"),
            "空内容应点名 prompt.file：{err}"
        );

        // passthrough + 坏路径 ⇒ 正常启动，且 prompt_text 为空。
        let raw = format!(r#"{{"prompt":{{"mode":"passthrough","file":{missing:?}}}}}"#);
        let c = Config::from_json(raw.as_bytes()).unwrap();
        assert!(c.parsed.prompt_text.is_empty());

        std::fs::remove_dir_all(&d).ok();
    }

    /// 宿主写出的真实 `prompt` 块（顶层）能解析。
    ///
    /// 实机值：`{"file":"","mode":"passthrough"}` ⇒ 今天行为等同不注入。
    /// ★ `prompt` 是宿主 native config 的**顶层**键（不在 `pool.` 下），因此**不需要**
    /// `hoist_nested_keys` 处理 —— 本用例同时守住这条层级假设：若将来宿主把它挪进
    /// `pool.`，这里会失败，提醒把两个键加进 `HOISTED_POOL_KEYS`。
    #[test]
    fn real_host_prompt_block_parses() {
        let c = Config::from_json(br#"{"prompt":{"file":"","mode":"passthrough"}}"#).unwrap();
        assert_eq!(c.prompt.mode, crate::prompt::MODE_PASSTHROUGH);
        assert!(c.prompt.file.is_empty());
        assert!(c.parsed.prompt_text.is_empty());

        // 用户把它改成 custom 时，网关必须真的读到（顶层层级）。
        let c = Config::from_json(br#"{"prompt":{"file":"","mode":"custom"}}"#).unwrap();
        assert_eq!(c.parsed.prompt_mode, crate::prompt::MODE_CUSTOM);
        assert_eq!(c.parsed.prompt_text, crate::prompt::default_prompt());
    }
}
