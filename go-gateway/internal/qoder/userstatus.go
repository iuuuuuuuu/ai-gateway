package qoder

// userstatus.go —— Qoder 计费周期（NextResetMs）的端点调用与解析。
//
// # 这个文件解决什么
//
// 界面要能说清「本周期已用多少 / 还有多久重置」，而这两件事都需要
// **下次重置时刻**。我方现有的三个端点都拿不到它：
//
//	/algo/api/v2/model/list     模型清单
//	/algo/api/v2/service/...    对话（SSE）
//	/api/v2/quota/usage         额度余量（`expiresAt` 是**套餐到期**，
//	                            不是周期重置；无到期套餐报 9999-12-31 哨兵）
//
// 真正的来源是 `/algo/api/v3/user/status?Encode=1`，它**只认机器身份签名**
//（不认 Bearer 会话），响应里顶层字段 `nextResetAt` 就是下次重置时刻
//（**epoch 毫秒**）。
//
// # 认证方式与其它端点完全不同（照抄参考实现，不要"顺手统一"）
//
// 参考实现 test-bin\recon\qoder-echoping07\auth\auth.go：
//
//	:43-48  CN.AuthBase = "https://gateway.qoder.com.cn"（与我方 Region.Gateway() 一致）
//	:137-151 appcode = "cosy"；secret 默认 "d2FyLCB3YXIgbmV2ZXIgY2hhbmdlcw=="
//	          （= 明文 "war, war never changes"，可用 $env:QODER_SIGNATURE_SECRET 覆盖）
//	:157-163 Sign(date) = md5(appCode + "&" + secret + "&" + date)
//	:614-630 commonHeaders：appcode / date / signature 三个头是这条通道的钥匙
//	:632-672 postEncoded：请求体 = QoderEncode(json)，响应 = 直接解成 map
//	:725-736 userStatusInnerStruct：**字段顺序必须与官方客户端一致**，
//	          且 token 字段**发空**（只有 userId 有值）
//	:810-829 FetchUserStatus：URL = AuthBase + "/algo/api/v3/user/status?Encode=1"
//	:879-900 ParseAccountStatus：响应是**扁平**的，`nextResetAt` 在顶层
//	:752-753 注释原文：「NextResetAtMs is the subscription refresh instant in
//	          epoch millis. **0 means the gateway did not report one.**」
//
// ⚠ 机器指纹**不伪造**：参考实现 bridge.go:78-100 是本地 uuid 随机编一套
//（machineID/machineToken/machineType 全是 uuid.New()）。我方已实测国际版
// host 对残缺/伪造的指纹会回 **HTTP 200 + 结构合法的 JSON**，只是少一条
// 活动 —— 不报错、不告警（见 internal\qoderclient\machine_identity.go 的
// 文件头注释）。故这里一律走 `qoderclient.MachineIdentityOf`，拿不到就
// **不发**那几个头，绝不发空串或编造值。

import (
	"bytes"
	"context"
	"crypto/md5"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"time"

	"workbuddy2api/internal/qoderclient"
)

// UserStatusPath 计费周期查询端点（拼在 Region.Gateway() 上）。
//
// `?Encode=1` 表示请求体经 QoderEncode 编码 —— 与 ModelsPath / ChatPath 同一约定。
const UserStatusPath = "/algo/api/v3/user/status?Encode=1"

const (
	// userStatusAppCode 这条通道的固定应用码（参考实现 auth.go:137）。
	userStatusAppCode = "cosy"
	// userStatusClientType 桌面客户端类型（参考实现 auth.go:625 固定 5）。
	userStatusClientType = "5"
	// userStatusDateLayout HTTP Date 头格式，**必须是 GMT**。
	//
	// 签名把 date 串原样纳入摘要，格式错一个字符签名就不对。
	userStatusDateLayout = "Mon, 02 Jan 2006 15:04:05 GMT"
	// userStatusSecretEnv 覆盖签名密钥的环境变量（参考实现 auth.go:143-151）。
	userStatusSecretEnv = "QODER_SIGNATURE_SECRET"
	// userStatusDefaultSecret 参考实现里的默认密钥（auth.go:138）。
	//
	// 它是 base64 的 "war, war never changes"，在公开仓库里是**明文常量**，
	// 不属于凭据泄漏 —— 但允许用环境变量覆盖，便于上游换密钥时不动代码。
	userStatusDefaultSecret = "d2FyLCB3YXIgbmV2ZXIgY2hhbmdlcw=="

	// MaxPlausibleResetMs 上限：2100-01-01T00:00:00Z。
	//
	// 上游对**没有到期时间**的套餐会报 9999-12-31（253402214400000）这种
	// 哨兵值 —— 那不是"下次重置"，是"永不过期"。参考实现
	// bridge\session.go:40-49 同值同理由，并明确要求
	// 「must never be mistaken for the instant the allowance actually refreshes」。
	MaxPlausibleResetMs = int64(4102444800000)
)

// PlausibleResetMs 判断一个下次重置时刻是否**可能是真的**。
//
// 0 / 负数 = 上游没报；≥ 2100 年 = 哨兵值。两者都不是一个周期。
//
// ⚠ internal\usage 里有一份同名同值的实现：那个包是**零外部依赖**的底层
// 统计包（server 依赖它），让它反向 import 本包会成环，故各留一份。
// 两处都有测试钉住取值，改一处必须改另一处。
func PlausibleResetMs(ms int64) bool {
	return ms > 0 && ms < MaxPlausibleResetMs
}

// AccountStatus `/user/status` 里我方关心的字段（参考实现 auth.go:738-764）。
type AccountStatus struct {
	// UserType 账号类型（实测形如 "teams" / "personal"）。
	UserType string
	// Plan 套餐标识（实测形如 "PLAN_TIER_TEAM"）。
	Plan string
	// UserTag 套餐展示名。
	UserTag string
	// OrgName 组织名（个人账号为空）。
	OrgName string
	// NextResetAtMs 订阅额度下次重置时刻（**epoch 毫秒**）。
	//
	// ⚠ **0 表示上游没有报**，不是"立刻重置"。调用方必须据此保留上一次的值，
	// 而不是把周期清零（参考实现 auth.go:752-753 的原话）。
	NextResetAtMs int64
	// IsQuotaExceeded 额度是否已用尽。
	IsQuotaExceeded bool
	// TotalUsagePercentage 用量百分比（0..1）。
	TotalUsagePercentage float64
}

// userStatusInner 请求体内层。
//
// ⚠ **字段顺序必须与官方客户端的 dict 插入顺序一致**（参考实现
// auth.go:725-736 的注释原文：「Field order MUST match the official client's
// dict insertion order for signature compatibility」）—— 签名覆盖的是
// 序列化后的字节，换个顺序摘要就变了。**不要**给这几个字段加 omitempty：
// 参考实现是把空串**发出去**的。
type userStatusInner struct {
	UserID             string          `json:"userId"`
	PersonalToken      string          `json:"personalToken"`
	SecurityOauthToken string          `json:"securityOauthToken"`
	RefreshToken       string          `json:"refreshToken"`
	NeedRefresh        bool            `json:"needRefresh"`
	AuthInfo           json.RawMessage `json:"authInfo"`
}

// userStatusOuter 请求体外层（与 jobToken 通道同一个外壳形状）。
type userStatusOuter struct {
	Payload       string `json:"payload"`
	EncodeVersion string `json:"encodeVersion"`
}

// userStatusSecret 取签名密钥（环境变量优先）。
func userStatusSecret() string {
	if v := os.Getenv(userStatusSecretEnv); v != "" {
		return v
	}
	return userStatusDefaultSecret
}

// userStatusDate 生成 HTTP Date 头（**UTC**，GMT 后缀）。
func userStatusDate(t time.Time) string {
	return t.UTC().Format(userStatusDateLayout)
}

// userStatusSign 计算签名：md5(appCode + "&" + secret + "&" + date)。
//
// 注意签名**只覆盖 date**，不含请求体 —— 请求体已由 `?Encode=1` 那套
// 编码保护（参考实现 auth.go:157-163 逐字如此）。
func userStatusSign(date string) string {
	sum := md5.Sum([]byte(userStatusAppCode + "&" + userStatusSecret() + "&" + date))
	return hex.EncodeToString(sum[:])
}

// marshalNoEscape 序列化且**不转义 HTML**。
//
// json.Marshal 默认把 `<` `>` `&` 转成 \u003c 这类转义序列，字节一变
// 签名就对不上（参考实现 auth.go:271-279 同样用 SetEscapeHTML(false)）。
func marshalNoEscape(v any) ([]byte, error) {
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(v); err != nil {
		return nil, err
	}
	// Encoder.Encode 会补一个换行；参考实现同样把它剪掉。
	return bytes.TrimRight(buf.Bytes(), "\n"), nil
}

// buildUserStatusRequest 构造 `/user/status` 请求。
//
// base 为空时用账号所属区域的 Gateway 域；测试传 httptest 的地址。
func buildUserStatusRequest(ctx context.Context, cr *Cred, base string) (*http.Request, error) {
	if cr == nil {
		return nil, fmt.Errorf("账号为空")
	}
	if cr.UID == "" {
		// userId 是这个端点的**唯一**有效载荷，空 uid 必然查不到东西 ——
		// 早失败，别让上游用一个语义不明的 200 把它盖过去。
		return nil, fmt.Errorf("账号缺少 uid，无法查询计费周期")
	}
	if base == "" {
		base = cr.Region.Gateway()
	}

	inner, err := marshalNoEscape(userStatusInner{
		UserID: cr.UID,
		// token 字段**发空**（参考实现 auth.go:725-736：只有 userId 有值）。
		// 这个端点靠机器身份签名认证，不需要会话令牌。
		AuthInfo: json.RawMessage("{}"),
	})
	if err != nil {
		return nil, err
	}
	outer, err := marshalNoEscape(userStatusOuter{Payload: string(inner), EncodeVersion: "1"})
	if err != nil {
		return nil, err
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, base+UserStatusPath, bytes.NewReader([]byte(QoderEncode(outer))))
	if err != nil {
		return nil, err
	}
	setUserStatusHeaders(req, ctx, cr, userStatusDate(time.Now()))
	return req, nil
}

// setUserStatusHeaders 设置这条通道的全部请求头（参考实现 auth.go:614-630）。
//
// # 为什么不复用 cosy.go 的 ApplyHeaders
//
// ApplyHeaders 是**会话**通道（Bearer COSY 签名 + cosy-key + cosy-user）；
// 本端点走的是**机器身份**通道（appcode + date + signature）。两套头的
// 集合几乎不重叠，混在一起发会让上游按错误的通道校验。
func setUserStatusHeaders(req *http.Request, ctx context.Context, cr *Cred, date string) {
	h := req.Header
	h.Set("appcode", userStatusAppCode)
	h.Set("login-version", "v2")
	h.Set("accept", "application/json")
	h.Set("accept-encoding", "identity")
	h.Set("cosy-version", "0.1.43")
	h.Set("cosy-clienttype", userStatusClientType)
	h.Set("content-type", "application/json")
	h.Set("date", date)
	h.Set("signature", userStatusSign(date))
	h.Set("user-agent", userAgent())

	// ⚠ 机器指纹**不伪造**：拿不到就一个都不发（见文件头注释）。
	//
	// 这里刻意**不读** `cr.MachineToken` —— 那个值由 cred.go 的
	// EnsureFingerprint 本地编造（hexShort(32) + 固定 "5"），国服 host
	// 不校验所以没暴露，国际版会静默降级成"少一条数据"。campaign.go 的
	// setMachineHeaders 出于同一理由也只认 MachineIdentityOf。
	if id := qoderclient.MachineIdentityOf(ctx); id.Usable() {
		h.Set("cosy-machinetoken", id.MachineToken)
		h.Set("cosy-machinetype", id.MachineType)
		h.Set("cosy-machinecode", id.MachineCode)
	}
	// cosy-machineid：参考实现发的是本地编造的 uuid（bridge.go:82）。
	// 我方用凭证里**持久化**的 machine.id（与 cosy.go 的会话通道同源）——
	// 它是"同一账号 + 同一设备"的稳定标识，而这条通道正是机器身份通道。
	if cr != nil && cr.MachineID != "" {
		h.Set("cosy-machineid", cr.MachineID)
	}
}

// FetchUserStatus 查询账号状态，取回计费周期相关的字段。
//
// 与其它端点一样：超时走 c.timeout()，响应体限读 1 MiB，>=400 直接报错
// 并把响应片段带上（不静默降级成零值 —— 那会让"查不到"看起来像"没有周期"）。
func (c *Client) FetchUserStatus(ctx context.Context, cr *Cred) (*AccountStatus, error) {
	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	req, err := buildUserStatusRequest(ctx, cr, c.userStatusBase)
	if err != nil {
		return nil, err
	}
	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("查询计费周期失败: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("查询计费周期失败（HTTP %d）：%s", resp.StatusCode, truncate(string(raw), 200))
	}

	var top map[string]any
	if err := json.Unmarshal(raw, &top); err != nil {
		return nil, fmt.Errorf("计费周期响应不是合法 JSON：%s", truncate(string(raw), 160))
	}
	st, ok := ParseAccountStatus(top)
	if !ok {
		return nil, fmt.Errorf("计费周期响应里没有可识别字段：%s", truncate(string(raw), 160))
	}
	return &st, nil
}

// ParseAccountStatus 从 `/user/status` 的响应里取出账号状态。
//
// 返回 ok=false 表示"这个响应不是我认识的形状"（三个特征键全空）。
// 调用方应据此**保留上一次的缓存**，而不是把周期清零 —— 参考实现
// auth.go:879-900 就是这么做的（它的返回签名是 `(AccountStatus, bool)`）。
func ParseAccountStatus(raw map[string]any) (AccountStatus, bool) {
	if st, ok := parseAccountStatusFlat(raw); ok {
		return st, true
	}
	// 兜底：若上游把结果包在 `{"code":0,"data":{…}}` 里。
	//
	// ⚠ 这一层**未在真上游验证过**（参考实现只读顶层）。加它是因为
	// 包一层信封是这个 API 家族的常见形态（`/sash/...` 那几个就是这样），
	// 而漏读的代价是"永远拿不到周期"却看不出原因。真上游实测见
	// userstatus_test.go 的 TestLiveUserStatusReachable。
	if data, ok := raw["data"].(map[string]any); ok {
		if st, ok := parseAccountStatusFlat(data); ok {
			return st, true
		}
	}
	return AccountStatus{}, false
}

func parseAccountStatusFlat(raw map[string]any) (AccountStatus, bool) {
	if raw == nil {
		return AccountStatus{}, false
	}
	userType, _ := raw["userType"].(string)
	plan, _ := raw["plan"].(string)
	// 三键全空 ⇒ 形状不认识（参考实现 auth.go:888 的判据）。
	if userType == "" && plan == "" && raw["nextResetAt"] == nil {
		return AccountStatus{}, false
	}
	st := AccountStatus{UserType: userType, Plan: plan}
	st.UserTag, _ = raw["userTag"].(string)
	st.OrgName, _ = raw["orgName"].(string)
	st.NextResetAtMs = toInt64Ms(raw["nextResetAt"])
	st.IsQuotaExceeded, _ = raw["isQuotaExceeded"].(bool)
	st.TotalUsagePercentage, _ = raw["totalUsagePercentage"].(float64)
	return st, true
}

// toInt64Ms 把 JSON 数字（float64）取成 int64 毫秒。
//
// ⚠ 只认 float64：响应里 nextResetAt 是**数字**。字符串形态**不猜** ——
// 猜错会把"未知"变成一个看起来很确定的错误时刻。
func toInt64Ms(v any) int64 {
	if f, ok := v.(float64); ok {
		return int64(f)
	}
	return 0
}

// ResolveNextResetMs 决定最终采用哪个"下次重置时刻"。
//
// # 优先级（照抄参考实现 bridge\session.go:193-202）
//
//	① `/user/status` 的 nextResetAt —— **权威**；
//	② 它缺失时退回 `/api/v2/quota/usage` 的 expiresAt。
//
// 参考实现的原话：「the gateway's nextResetAt is authoritative, and the
// OpenAPI expiresAt is only a fallback for when it is missing. Plans without
// an expiry report the far-future 9999-12-31 sentinel in expiresAt, which must
// never be mistaken for the instant the allowance actually refreshes.」
//
// 返回 0 = 两边都没给出可信值 ⇒ 调用方**保留**上一次的周期，不要清零。
func ResolveNextResetMs(st *AccountStatus, q *Quota) int64 {
	var fromStatus int64
	if st != nil {
		fromStatus = st.NextResetAtMs
	}
	if !PlausibleResetMs(fromStatus) {
		fromStatus = 0
	}
	if fromStatus != 0 {
		return fromStatus
	}
	if q != nil && PlausibleResetMs(q.ExpiresAt) {
		return q.ExpiresAt
	}
	return 0
}
