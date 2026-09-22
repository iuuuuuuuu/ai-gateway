package upstream

// smslogin.go —— WorkBuddy **手机号 + 短信验证码**登录（2026-09-22 新增）。
//
// # 为什么需要它
//
// 所有者要求接入参考脚本的短信登录。此前只有两条路加账号：
//  1. 从桌面客户端导入（要求装客户端且已登录）
//  2. OAuth（redirect_uri 是自定义协议，只有官方客户端能回调）
//
// 短信登录是**唯一不依赖桌面端**的路子：给手机号 → 收验证码 → 换 token。
// 青龙/服务器场景尤其需要它（没有 GUI）。
//
// # 协议（逐字对照参考脚本 `workbuddyv3`，这两个端点是它实测出来的）
//
//	POST {base}/v2/plugin/login/send-sms   {"phone":"..."}
//	     → {"code":0,"data":{"expires_in":300}}
//	POST {base}/v2/plugin/login/token
//	     {"login_method":"phone","phone":"...","sms_code":"..."}
//	     → {"code":0,"data":{"accessToken":"...","refreshToken":"..."}}
//
// ⚠ 实测（2026-09-22）send-sms 对**无效手机号**回
// `HTTP 500 {"code":10000,"msg":"10000:keycloak SPI returned status=400"}` ——
// 说明端点存在且走到了 keycloak，只是号码不过校验。
// 这条记在这里是为了让后人知道"500 不一定是网络问题"。
//
// ⚠ 复用 `chatBase` 做区域分流**不适用**：登录发生在**还没有账号对象**的时候，
// 区域是调用方给的参数（与 qoder 的 `--region` 同款语义）。
// 故这里显式接收 baseURL。

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"
)

// smsUA 登录请求的 User-Agent。
//
// ⚠ **必须伪装成官方桌面客户端**：参考脚本用的是
// `WorkBuddy/5.5.4 Chrome/138.0.7204.251 Electron/37.10.3`。
// 用 Go 默认 UA（`Go-http-client/2.0`）容易被风控拦下，
// 而那种失败只表现为一个泛泛的错误码，排查方向会被引向"手机号/验证码错了"。
const smsUA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 " +
	"(KHTML, like Gecko) WorkBuddy/5.5.4 Chrome/138.0.7204.251 Electron/37.10.3 Safari/537.36"

// smsTimeout 单次登录请求的超时。
//
// 登录是**交互式**的：用户在界面上等，不能无限挂着。
const smsTimeout = 30 * time.Second

// SMSSendResult 发送验证码的结果。
type SMSSendResult struct {
	// ExpiresIn 验证码有效期（秒）。上游会下发，缺省按 300 处理。
	ExpiresIn int `json:"expires_in"`
}

// SendSMS 向手机号发送登录验证码。
//
// baseURL 形如 `https://www.workbuddy.cn`（国服）或 `https://www.workbuddy.ai`（国际版）。
func (c *Client) SendSMS(baseURL, phone string) (*SMSSendResult, error) {
	phone = strings.TrimSpace(phone)
	if phone == "" {
		return nil, fmt.Errorf("手机号不能为空")
	}

	body, err := json.Marshal(map[string]any{"phone": phone})
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequest(http.MethodPost,
		strings.TrimRight(baseURL, "/")+"/v2/plugin/login/send-sms",
		bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("User-Agent", smsUA)

	data, err := c.doLoginJSON(req)
	if err != nil {
		return nil, err
	}

	out := &SMSSendResult{ExpiresIn: 300}
	// data 可能是 null（上游对某些成功也回 null），解不出来不算错。
	if len(data) > 0 {
		_ = json.Unmarshal(data, out)
	}
	if out.ExpiresIn <= 0 {
		out.ExpiresIn = 300
	}
	return out, nil
}

// SMSLoginResult 登录成功后拿到的凭证。
type SMSLoginResult struct {
	AccessToken  string
	RefreshToken string
}

// LoginWithSMS 用手机号 + 验证码换 token。
func (c *Client) LoginWithSMS(baseURL, phone, smsCode string) (*SMSLoginResult, error) {
	phone = strings.TrimSpace(phone)
	smsCode = strings.TrimSpace(smsCode)
	if phone == "" {
		return nil, fmt.Errorf("手机号不能为空")
	}
	if smsCode == "" {
		return nil, fmt.Errorf("验证码不能为空")
	}

	body, err := json.Marshal(map[string]any{
		"login_method": "phone",
		"phone":        phone,
		"sms_code":     smsCode,
	})
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequest(http.MethodPost,
		strings.TrimRight(baseURL, "/")+"/v2/plugin/login/token",
		bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("User-Agent", smsUA)

	data, err := c.doLoginJSON(req)
	if err != nil {
		return nil, err
	}

	var tok struct {
		AccessToken  string `json:"accessToken"`
		RefreshToken string `json:"refreshToken"`
	}
	if err := json.Unmarshal(data, &tok); err != nil {
		return nil, fmt.Errorf("登录响应解析失败: %w（原始：%s）", err, truncate(string(data), 160))
	}
	// ⚠ 两个都要。只有 AT 时账号能用但**几小时后就会失效且无法自动续期** ——
	// 表现为"今天好好的，明天全部 401"，而那时早已想不起是登录环节的问题。
	if tok.AccessToken == "" || tok.RefreshToken == "" {
		return nil, fmt.Errorf("登录成功但响应缺少 accessToken/refreshToken（AT=%d 字节，RT=%d 字节）",
			len(tok.AccessToken), len(tok.RefreshToken))
	}
	return &SMSLoginResult{AccessToken: tok.AccessToken, RefreshToken: tok.RefreshToken}, nil
}

// doLoginJSON 发一个登录请求并解出 `data`。
//
// ⚠ 与 `doJSON` 分开而不是复用它：`doJSON` 需要 `*auth.Auth` 来选
// HTTP client 与区域 base —— 而**登录时还没有账号对象**，
// 那正是本流程的输入。硬套会把"先有鸡还是先有蛋"塞进签名里。
//
// 错误分类仍走同一套 `Classify`，保证错误信息口径一致。
func (c *Client) doLoginJSON(req *http.Request) (json.RawMessage, error) {
	// 登录走**默认 client**（带代理的那份），不按账号选：
	// 国际版登录必须走代理（见 gateway.go 的 proxy 说明），
	// 而 c.httpFor 需要一个账号才能判区域。
	hc := c.HTTP
	if hc == nil {
		hc = &http.Client{Timeout: smsTimeout}
	}

	resp, err := hc.Do(req)
	if err != nil {
		return nil, fmt.Errorf("请求失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))

	var env apiEnvelope
	if err := json.Unmarshal(raw, &env); err != nil {
		// 连信封都解不出来（如 HTML 错误页）时，把原始内容带上 ——
		// 否则用户只看到"解析失败"，无从判断是网络、风控还是上游改版。
		return nil, fmt.Errorf("响应解析失败（HTTP %d）：%s",
			resp.StatusCode, truncate(string(raw), 200))
	}
	// ⚠ 先看业务码：上游把"手机号格式错""验证码错"都放在 code 里，
	// 而 HTTP 状态可能是 200 也可能是 500（实测 send-sms 对无效号码回 500）。
	// 只看 HTTP 状态会把"验证码错了"报成"服务器异常"。
	if env.Code != 0 {
		return nil, fmt.Errorf("%s", bizMessage(env.Code, env.Msg))
	}
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("上游返回 HTTP %d：%s", resp.StatusCode, truncate(string(raw), 200))
	}
	return env.Data, nil
}

// bizMessage 把业务码翻成人话。
//
// 为什么需要：上游的 `msg` 形如 `10000:keycloak SPI returned status=400:`，
// 直接给用户看既看不懂、也指向不了正确动作。
// 已知码给人话，未知码**原样带上**（不能吞掉，否则无从排查）。
func bizMessage(code int, msg string) string {
	switch code {
	case 10000:
		// 实测常见于手机号格式不对 / 号码未注册 / 风控拦下
		return fmt.Sprintf("手机号无效或发送过于频繁（上游返回「%s」）；"+
			"请确认号码无误，若刚发过请稍等一分钟再试", truncate(msg, 120))
	default:
		return fmt.Sprintf("上游返回错误码 %d：%s", code, truncate(msg, 160))
	}
}
