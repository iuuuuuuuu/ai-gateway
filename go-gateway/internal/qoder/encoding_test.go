package qoder

// 请求体编码的单元测试。
//
// 为什么必须有：编码算法有三步（base64 → 三段重排 → 字符映射），
// 任何一步写错都**不会编译报错**，只会让上游解不开请求体 ——
// 现象是"账号用不了"或"流挂起"，排查方向极易被引向凭证/网络。
//
// 参考实现自带 encoding_test.go；本文件对齐其口径并补上字母表自检。

import (
	"encoding/base64"
	"net/http"
	"strings"
	"testing"
)

// TestAlphabetShape 自定义字母表必须是 64 个**互不重复**的字符。
//
// 复制粘贴字母表时最易犯的错：漏字符、改顺序、重复字符。
// 三者都会让所有请求静默失败，故单独钉住。
func TestAlphabetShape(t *testing.T) {
	if len(customAlphabet) != 64 {
		t.Fatalf("自定义字母表应为 64 字符，实际 %d", len(customAlphabet))
	}
	if len(stdAlphabet) != 64 {
		t.Fatalf("标准字母表应为 64 字符，实际 %d", len(stdAlphabet))
	}
	seen := map[byte]bool{}
	for i := 0; i < len(customAlphabet); i++ {
		c := customAlphabet[i]
		if seen[c] {
			t.Errorf("自定义字母表有重复字符 %q（位置 %d）", c, i)
		}
		seen[c] = true
	}
	// 自定义字母表不能与标准字母表完全相同（否则"映射"这一步形同虚设）
	if customAlphabet == stdAlphabet {
		t.Error("自定义字母表与标准字母表相同 —— 映射步骤失去意义，可能是抄错了")
	}
	// 映射表必须覆盖标准字母表的每一位
	for i := 0; i < len(stdAlphabet); i++ {
		if stdToCustom[stdAlphabet[i]] == 0xFF {
			t.Errorf("映射表缺少标准字符 %q", stdAlphabet[i])
		}
	}
}

// TestEncodeDecodeRoundTrip 往返必须还原（参考实现的核心断言）。
func TestEncodeDecodeRoundTrip(t *testing.T) {
	cases := []string{
		"hi",
		"",
		"a",
		"ab",
		"abc",
		`{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"你好"}]}`,
		strings.Repeat("x", 1000),
		"中文内容也要能往返",
		"emoji 🎉 也要能往返",
	}
	for _, s := range cases {
		enc := QoderEncode([]byte(s))
		dec, err := QoderDecode(enc)
		if err != nil {
			t.Errorf("解码 %q 失败: %v", truncate(s, 30), err)
			continue
		}
		if string(dec) != s {
			t.Errorf("往返不一致：原文 %q，还原 %q", truncate(s, 30), truncate(string(dec), 30))
		}
	}
}

// TestEncodeIsNotPlainBase64 编码结果必须与明文 base64 **不同**。
//
// 这条能抓到"忘了做三段重排/字符映射"这类缺陷：
// 若实现只做了 base64，往返测试仍然通过（因为 decode 也只做 base64），
// 但上游会解不开。故必须单独断言"编码结果不是标准 base64"。
func TestEncodeIsNotPlainBase64(t *testing.T) {
	plain := []byte(`{"model":"test"}`)
	enc := QoderEncode(plain)

	// 结果里应出现自定义字母表特有、而标准 base64 没有的字符
	hasCustomOnly := false
	for i := 0; i < len(enc); i++ {
		if !strings.ContainsRune(stdAlphabet, rune(enc[i])) && enc[i] != customPad {
			hasCustomOnly = true
			break
		}
	}
	if !hasCustomOnly {
		t.Error("编码结果全是标准 base64 字符 —— 字符映射那一步可能没生效")
	}
	// 也不应与标准 base64 相同
	std := stdBase64Encode(plain)
	if enc == std {
		t.Error("编码结果与标准 base64 相同 —— 三段重排与字符映射都没生效")
	}
}

// TestEncodePaddingUsesCustomChar 填充符必须映射成 '$'。
func TestEncodePaddingUsesCustomChar(t *testing.T) {
	// 长度不是 3 的倍数时 base64 会有 '=' 填充
	enc := QoderEncode([]byte("hi")) // "aGk=" 有一个填充
	if !strings.Contains(enc, string(customPad)) {
		t.Errorf("编码结果应含自定义填充符 %q，实际 %q", string(customPad), enc)
	}
	if strings.Contains(enc, "=") {
		t.Errorf("编码结果不应含标准填充符 '='，实际 %q", enc)
	}
}

// TestDecodeRejectsInvalidChar 非法字符必须报错（而不是静默产出坏数据）。
func TestDecodeRejectsInvalidChar(t *testing.T) {
	// '!' 不在自定义字母表里（注意自定义表里有 '#' '@' 等，但确实没有 '!'）
	if _, err := QoderDecode("!!!"); err == nil {
		t.Error("非法字符应报错")
	}
	// 空串是合法的（返回空）
	out, err := QoderDecode("")
	if err != nil || out != nil {
		t.Errorf("空串应返回空且无错，实际 %v / %v", out, err)
	}
}

// TestEncodeEmpty 空输入应产出空串（而不是 panic）。
func TestEncodeEmpty(t *testing.T) {
	if got := QoderEncode(nil); got != "" {
		t.Errorf("nil 应编码为空串，实际 %q", got)
	}
	if got := QoderEncode([]byte{}); got != "" {
		t.Errorf("空切片应编码为空串，实际 %q", got)
	}
}

// TestEncodeStable 同一输入必须产出同一输出（无随机性）。
//
// 编码若有随机性，签名（覆盖编码结果）会与发送内容不一致 ——
// 表现为偶发 101，极难排查。
func TestEncodeStable(t *testing.T) {
	plain := []byte(`{"a":1}`)
	first := QoderEncode(plain)
	for i := 0; i < 20; i++ {
		if got := QoderEncode(plain); got != first {
			t.Fatalf("第 %d 次编码结果不同 —— 编码必须是确定性的（否则签名会与发送内容不符）", i)
		}
	}
}

// TestSignatureCoversEncodedBody 签名必须覆盖**编码后**的字节。
//
// 这是最容易搞反的一处：先签名再编码 → 上游算出的签名与收到的 body 不符 → 101。
// 用"同一 body 的明文与编码形式产生不同签名"来确认签名确实吃了 body，
// 并确认 ChatStream 传的是编码结果（这里通过直接调用 ApplyHeaders 复现）。
func TestSignatureCoversEncodedBody(t *testing.T) {
	c := testCred()
	sess, err := NewCosySession(c, c.DT, c.DRT)
	if err != nil {
		t.Fatal(err)
	}
	rawURL := RegionCN.Gateway() + ChatPath

	plain := []byte(`{"model":"m"}`)
	encoded := QoderEncode(plain)

	reqA := mustRequestPost(t, rawURL)
	if err := sess.ApplyHeaders(reqA, encoded, rawURL, c.UID, true, "m"); err != nil {
		t.Fatal(err)
	}
	reqB := mustRequestPost(t, rawURL)
	if err := sess.ApplyHeaders(reqB, string(plain), rawURL, c.UID, true, "m"); err != nil {
		t.Fatal(err)
	}
	// 两者签名应不同（因为 body 不同）
	if reqA.Header.Get("authorization") == reqB.Header.Get("authorization") {
		t.Error("编码与明文产生了相同签名 —— 签名没有覆盖 body")
	}
	// 且编码后的 body 不是明文
	if encoded == string(plain) {
		t.Error("编码结果与明文相同 —— 编码没生效")
	}
}

// TestModelKeyComesFromPlainBody x-model-key 必须从**明文** body 里取。
//
// 编码后的 body 不是 JSON，解析不出模型名 —— 若从编码结果取，
// x-model-key 会恒为空，上游便不知道要用哪个模型。
func TestModelKeyComesFromPlainBody(t *testing.T) {
	plain := []byte(`{"model":"deepseek-v4.1-flash","messages":[]}`)
	if got := modelKeyOf(plain); got != "deepseek-v4.1-flash" {
		t.Errorf("应从明文 body 取模型名，实际 %q", got)
	}
	// 编码后的 body 解析不出模型名（这正是"必须用明文"的原因）
	encoded := []byte(QoderEncode(plain))
	if got := modelKeyOf(encoded); got != "" {
		t.Errorf("编码后的 body 不应能解析出模型名，实际 %q", got)
	}
}

// --- 测试辅助 ---

// stdBase64Encode 标准 base64（用于对比"编码结果确实不同于标准 base64"）。
func stdBase64Encode(b []byte) string { return base64.StdEncoding.EncodeToString(b) }

// mustRequestPost 构造一个 POST 请求（ApplyHeaders 会写头，方法不影响断言）。
func mustRequestPost(t *testing.T, rawURL string) *http.Request {
	t.Helper()
	req, err := http.NewRequest(http.MethodPost, rawURL, nil)
	if err != nil {
		t.Fatalf("构造请求失败: %v", err)
	}
	return req
}
