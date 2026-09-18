package qoder

// encoding.go 实现 Qoder 的请求体编码（QoderEncoding）。
//
// 算法三步（缺一不可，来自参考实现 docs/api-reference.md §5）：
//
//	① 标准 base64
//	② 三段重排：把末尾 1/3 移到最前、开头 1/3 移到最后
//	③ 字符映射：标准字母表 → 自定义字母表，'=' → '$'
//
// ## 为什么必须编码
//
// 上游的对话端点在 URL 里带 `Encode=1`。实测与参考实现均表明：
// 请求体需要按此格式编码后发送，否则上游无法解析（表现为流挂起或业务错误）。
//
// ## 为什么两个方向都要实现
//
// `QoderEncode` 用于请求体；`QoderDecode` 用于**验证往返正确性**
// （单测里 encode→decode 必须还原原文，否则算法有笔误）。
// 另外若上游哪天改为编码响应，Decode 也已就绪。
//
// ⚠ 自定义字母表里有 `,` `@` `#` `&` `*` `%` `^` `.` `(` `)` `!` 等字符 ——
// 它们不是 base64 合法字符，故这份编码结果**不能**当 base64 用。
// 复制粘贴字母表时极易漏字符或改顺序，改错会让所有请求静默失败，
// 故字母表本身也有单测钉住（见 encoding_test.go）。

import (
	"encoding/base64"
	"fmt"
	"strings"
)

// customAlphabet 上游的自定义字母表（64 字符，与标准 base64 逐位对应）。
const customAlphabet = "_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!"

// stdAlphabet 标准 base64 字母表。
const stdAlphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

// customPad 自定义填充字符（替代 '='）。
const customPad = '$'

var stdToCustom [128]byte
var customToStd [256]byte

func init() {
	if len(customAlphabet) != 64 {
		// 编译期常量写错属于程序缺陷：继续跑会让**每个**请求都失败，
		// 而现象是"账号用不了"，排查方向会被引向凭证。直接 panic 更诚实。
		panic(fmt.Sprintf("qoder: 自定义字母表长度应为 64，实际 %d", len(customAlphabet)))
	}
	for i := range stdToCustom {
		stdToCustom[i] = 0xFF
	}
	for i := range customToStd {
		customToStd[i] = 0xFF
	}
	for i := 0; i < len(stdAlphabet); i++ {
		stdToCustom[stdAlphabet[i]] = customAlphabet[i]
		customToStd[customAlphabet[i]] = stdAlphabet[i]
	}
	customToStd[customPad] = '='
}

// QoderEncode 编码请求体。
func QoderEncode(plain []byte) string {
	std := base64.StdEncoding.EncodeToString(plain)
	n := len(std)
	if n == 0 {
		return ""
	}
	// ② 三段重排
	a := n / 3
	rearranged := std[n-a:] + std[a:n-a] + std[:a]

	// ③ 字符映射
	var sb strings.Builder
	sb.Grow(n)
	for i := 0; i < n; i++ {
		c := rearranged[i]
		if c == '=' {
			sb.WriteByte(customPad)
			continue
		}
		mapped := stdToCustom[c]
		if mapped == 0xFF {
			// 理论上不可达（base64 输出只含标准字母表），
			// 但保留分支以免将来改字母表时静默产出坏数据。
			panic(fmt.Sprintf("qoder: base64 输出含意外字符 %q", c))
		}
		sb.WriteByte(mapped)
	}
	return sb.String()
}

// QoderDecode 逆编码。主要用于往返自检与响应解码。
func QoderDecode(enc string) ([]byte, error) {
	n := len(enc)
	if n == 0 {
		return nil, nil
	}
	// ③ 逆映射
	mapped := make([]byte, n)
	for i := 0; i < n; i++ {
		c := enc[i]
		s := customToStd[c]
		if s == 0xFF {
			return nil, fmt.Errorf("非法字符 %q（位置 %d）", c, i)
		}
		mapped[i] = s
	}
	// ② 逆重排
	a := n / 3
	std := string(mapped[n-a:]) + string(mapped[a:n-a]) + string(mapped[:a])
	// ① base64 解码
	out, err := base64.StdEncoding.DecodeString(std)
	if err != nil {
		return nil, fmt.Errorf("base64 解码失败: %w", err)
	}
	return out, nil
}
