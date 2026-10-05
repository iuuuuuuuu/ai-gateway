## AI Gateway 1.0.12 · 正式版

这是一个**根因修复版**。1.0.11 已经修了「对外状态码」，但**真正让客户端显示「API 密钥无效」的不是状态码，而是文案里的三个字符**。1.0.12 修的是文案。

> **如果你装的是 1.0.11，这个问题在 1.0.11 上依然存在** —— 请升级到 1.0.12。

---

## 一、根因：客户端是按**文案文本**判错的，不是按状态码

DSH 走的是 `pi-ai` 适配层，它的错误分类器只看**错误消息的文本**：

```js
// @deepseek-ai/dsh-llm-pi-ai/lib/index.js:1374
function classifyPiAiError(message) {
	if (/\b(?:401|403)\b/.test(message)) return "AUTH";
	if (isQuotaExceededError(message)) return QUOTA_EXCEEDED_CODE;
	if (/\b429\b|rate.?limit/i.test(message)) return "RATE_LIMIT";
	...
}
```

命中 `AUTH` 之后，UI 就渲染成「**API 密钥无效**」：

```js
// dsh-client-ui-trajectory/lib/client.js
if (request.errorCode === "AUTH") return t("details.failure.auth");  // → "API 密钥无效"
```

**关键结论**：只要发出去的文案里出现独立的 `401` 或 `403`，客户端就判 AUTH —— **哪怕 HTTP 状态码是 502**。

1.0.11 只做了状态码归一（`403 → 502`），文案里那句「上游 **403** 业务码 11140」原封不动 ⇒ 状态码对了，**客户端照样读成 AUTH**。这就是「装了最新版还是提示密钥无效」的原因。

---

## 二、修法：给所有对外文案加一道闸门

新增 `internal/server/clienttext.go`，在**所有**面向客户端的文案出口上做一次就地改写：独立的 `401`/`403` → `4xx`。

覆盖的出口（四个协议 + 流式）：

| 出口 | 位置 |
|---|---|
| OpenAI `/v1/chat/completions` | `writeOpenAIError` |
| Anthropic `/v1/messages` | `anthropicError` |
| `/v1/responses`（含流式 `response.failed`） | `responsesError` / `failedEvent` |

另外堵掉两处**生产者级**泄漏：

1. `upstream.FriendlyMessage` 的 `ErrServer` 分支原本直接拼 `strconv.Itoa(status)` —— 它是导出纯函数，一旦被传入 401/403 就会写出裸状态码。改为 `serverStatusLabel(status)`：只有 5xx 才写具体数字，其余一律写 `5xx`。
2. 轮转耗尽的兜底分支原本 `msg += ": " + lastErr.Error()`，而 `upstream.Error.Error()` 的格式是 `upstream %s (http %d): %s` —— 会带出 `(http 403)` 和整段上游 JSON。改为只写失败类别，不再拼原始报文。

**闸门是必要但不充分的第二道防线**：状态码归一继续保留（`403 → 502`），文案闸门兜住文本。两道都在。

---

## 三、实测：同一个真实 DSH 客户端，只换二进制

对照组设计：**同一份 DSH 配置、同一批账号、同一个真实上游**，只有网关二进制不同。用隔离的 `DSH_HOME`（独立目录 + junction 复用插件），通过 `--patch` 叠加层把 `baseURL` 指向两个隔离实例，跑真实的 `dsh --profile headless --json`。

| 端口 | 二进制 | DSH 给出的 failure code | 文案 |
|---|---|---|---|
| 7866 | **修复后** | **`SERVER`** ✅ | `上游业务码 11140`（无裸 403） |
| 7867 | 1.0.11（旧） | **`AUTH`** ❌ | `上游 403 业务码 11140` |

两条腿各跑 3 轮，**3/3 稳定复现**。对照组确实判 AUTH ⇒ 实验有区分力，不是「恰好都不报错」。

### 用户真实场景：跳过已触发的账号

池里 1 个已触发 11140 的号 + 4 个正常号，用真实 DSH 跑 3 轮：

| 轮次 | 结果 |
|---|---|
| 1 / 2 / 3 | **全部成功**，模型正常回复 `pong` |

网关日志可见每轮的动作：打到坏号 → `upstream 403 account_unusable body={"code":11140,...}` → `chat rotate: 第 1 次换号前退避` → 换到好号 → 成功。**坏号被跳过，对话不中断，客户端全程无感。**

---

## 四、回归测试

新增 `clienttext_test.go`（7 个测试）与 `client_status_test.go` 的判据更正，全部通过；`go build` / `go vet` / `go test ./...` 全绿（15 个包）。

测试的关键设计：**刻意不复用生产代码的正则**，而是自己写 `\b(?:401|403)\b` —— 否则有人把闸门正则收窄时，测试会跟着变宽，测不出「客户端仍会读成 AUTH」。判据必须来自客户端本体。

其中一条专门钉住**业务码不能被误伤**：`11140`、`14018`、`11133`、`11102`、`11115`、`10605`、`6004`、`3007` 必须逐字不变。`14018` 的中间三位正好是 `401`，靠词边界保护 —— 这条测试就是为它写的。

---

## 安装

- **Windows**：`AI Gateway_1.0.12_x64-setup.exe`（NSIS）
- **macOS / Linux**：见本次 Release 的其它产物

## 已知问题

- 11140 的根因是**该账号在本设备上尚未完成登录**，网关只能冷却它并自动切到别的号；要让这个号恢复，需要在「账号管理」页**重新登录该账号**
- 人机验证**不会被绕过**（刻意如此）：遇到 `captcha_required` 请按提示在 ZCode 官方客户端里完成一次验证后重试
