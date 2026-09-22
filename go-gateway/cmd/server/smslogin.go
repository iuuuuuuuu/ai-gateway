package main

// smslogin.go —— 短信登录**登记账号**的宿主侧实现（2026-09-22 新增）。
//
// # 分工
//
//	网关（internal/upstream/smslogin.go）  给手机号发短信 / 用验证码换 token
//	宿主（本文件）                          落凭证文件 + 登记进账号池
//
// 这个分工是刻意的：账号库（编号、备注、禁用标记、区域）是宿主的
// 数据结构，网关只持有凭证目录的只读视图（见 internal/server/smslogin.go）。
// 网关直接写账号库会绕过宿主的校验与缓存，表现为「加了账号但界面看不到」。
//
// # ⚠ 为什么 UID 要自己算
//
// WorkBuddy 的 accessToken 是 JWT，`sub`/`uid` 声明里带账号标识。
// 短信登录的响应**只给 token，不给 uid**（与 OAuth 不同）——
// 不解析 JWT 就没法给账号起文件名，也就无法落库。
//
// 解析失败时**明确报错**而不是编一个随机 uid：
// 随机 uid 会让同一个手机号每次登录都产生一个新账号，用户看到一堆重复账号，
// 而每个都指向同一份真实凭证。

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/server"
)

// newSMSLoginFunc 构造登记回调。
//
// `authDir` 是凭证目录（与其它 WorkBuddy 凭证同处），
// 新账号写成 `<authDir>/workbuddy-<uid>.json` —— 文件名前缀必须是
// `workbuddy`，否则 `auth.LoadDir` 的 glob `workbuddy*.json` 扫不到它。
func newSMSLoginFunc(p *pool.Pool, authDir string) server.SMSLoginFunc {
	return func(in server.SMSLoginInput) (any, error) {
		uid, err := uidFromAccessToken(in.AccessToken)
		if err != nil {
			return nil, fmt.Errorf("无法从登录令牌解析账号标识: %w", err)
		}

		domain := "www.workbuddy.cn"
		if in.Region == "intl" {
			domain = "www.workbuddy.ai"
		}

		a := &auth.Auth{
			AccessToken:  in.AccessToken,
			RefreshToken: in.RefreshToken,
			UID:          uid,
			Domain:       domain,
			Product:      auth.ProductWorkBuddy,
			FilePath: filepath.Join(authDir,
				fmt.Sprintf("workbuddy-%s.json", sanitizeFilePart(uid))),
		}

		if err := os.MkdirAll(authDir, 0o700); err != nil {
			return nil, fmt.Errorf("创建凭证目录失败: %w", err)
		}
		// 先落盘再进池：反过来的话进池成功、落盘失败 ⇒ 账号能用但重启即丢，
		// 用户会觉得"昨天加的号今天没了"。
		if err := a.SaveAtomic(); err != nil {
			return nil, fmt.Errorf("写入凭证失败: %w", err)
		}
		// 进池（upsert 语义：同 uid 重复登录只更新凭证，不产生第二个账号）
		p.Add(a)

		return map[string]any{
			"uid":      uid,
			"phone":    in.Phone,
			"region":   in.Region,
			"domain":   domain,
			"nickname": "",
			"file":     a.FilePath,
		}, nil
	}
}

// uidFromAccessToken 从 JWT 的 payload 里取账号标识。
//
// 依次尝试几个已知的声明名：上游换过字段名，只认一个会在改版后失效。
// 全部取不到时报错（见文件头：不编随机 uid）。
func uidFromAccessToken(tok string) (string, error) {
	parts := strings.Split(tok, ".")
	if len(parts) < 2 {
		return "", fmt.Errorf("令牌不是 JWT 格式（%d 段）", len(parts))
	}
	// JWT 用 base64url 且**不带 padding**；补 _ 与长度修正都要做，
	// 否则偶发号码会解出 "illegal base64 data"。
	payload := parts[1]
	if pad := len(payload) % 4; pad != 0 {
		payload += strings.Repeat("=", 4-pad)
	}
	raw, err := base64.URLEncoding.DecodeString(payload)
	if err != nil {
		raw, err = base64.RawURLEncoding.DecodeString(parts[1])
		if err != nil {
			return "", fmt.Errorf("解析令牌负载失败: %w", err)
		}
	}

	var claims map[string]any
	if err := json.Unmarshal(raw, &claims); err != nil {
		return "", fmt.Errorf("解析令牌声明失败: %w", err)
	}
	// 已知的候选声明名（上游换过字段）
	for _, key := range []string{"uid", "userId", "user_id", "sub"} {
		if v, ok := claims[key].(string); ok && strings.TrimSpace(v) != "" {
			return strings.TrimSpace(v), nil
		}
	}
	return "", fmt.Errorf("令牌里没有 uid/userId/user_id/sub 任一声明")
}

// sanitizeFilePart 把 uid 里不能进文件名的字符换掉。
//
// uid 通常是个 UUID（安全），但**不能假设**：它来自上游返回的 JWT。
// 含 `/` 或 `..` 的 uid 会把凭证写到目录之外 —— 那是真实的路径穿越风险。
func sanitizeFilePart(s string) string {
	var b strings.Builder
	for _, r := range s {
		switch {
		case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r >= '0' && r <= '9',
			r == '-', r == '_':
			b.WriteRune(r)
		default:
			b.WriteRune('_')
		}
	}
	out := b.String()
	if out == "" {
		out = "unknown"
	}
	return out
}
