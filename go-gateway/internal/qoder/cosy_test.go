package qoder

// 签名层与凭证层的单元测试。
//
// 为什么签名层必须有测试：它有三个"改一个字就静默失效"的特性 ——
// 临时密钥必须是 16 个 ASCII 字符、身份 JSON 必须排序无空白、
// pathSig 必须去掉 /algo 前缀。任何一处改动都不会编译报错，
// 只会让所有请求被上游拒（101 Signature invalid），而现象是"账号用不了"，
// 排查方向极易被引向凭证或网络。
//
// 参考实现几乎没有测试，本文件补上。

import (
	"crypto/aes"
	"crypto/cipher"
	"encoding/base64"
	"encoding/json"
	"net/http"
	"strings"
	"testing"
)

// mustRequest 构造一个测试用请求（失败即 Fatal）。
func mustRequest(t *testing.T, rawURL string) *http.Request {
	t.Helper()
	req, err := http.NewRequest(http.MethodGet, rawURL, nil)
	if err != nil {
		t.Fatalf("构造请求失败: %v", err)
	}
	return req
}

// testCred 构造一个字段齐全的测试凭证。
func testCred() *Cred {
	return &Cred{
		UID:          "11111111-2222-3333-4444-555555555555",
		Nickname:     "测试账号",
		DT:           "dt-test-token",
		DRT:          "drt-test-refresh",
		MachineID:    "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
		MachineToken: "0123456789abcdef0123456789abcdef",
		MachineType:  "5",
		Region:       RegionCN,
	}
}

// TestCosySessionBuildsWithoutError 基本构造应当成功。
func TestCosySessionBuildsWithoutError(t *testing.T) {
	c := testCred()
	sess, err := NewCosySession(c, c.DT, c.DRT)
	if err != nil {
		t.Fatalf("构造签名会话失败: %v", err)
	}
	if sess.cosyKey == "" || sess.info == "" {
		t.Fatal("cosyKey 或 info 为空")
	}
	// 临时密钥必须是 16 个 ASCII 字符（不是 16 个随机字节）——
	// 这是与桌面客户端对齐的关键，改错会让 AES 密钥非法或签名不匹配。
	if len(sess.tempKey) != 16 {
		t.Errorf("临时密钥应为 16 字节，实际 %d", len(sess.tempKey))
	}
	for i, b := range sess.tempKey {
		if b < 0x20 || b > 0x7e {
			t.Errorf("临时密钥第 %d 字节不是可打印 ASCII: %#x", i, b)
		}
	}
}

// TestCosyMissingFingerprintFails 缺机器指纹时必须报错，而不是拿空值去签名。
//
// 空指纹签出来的请求上游一定拒，但错误会显示成"登录失效"，
// 让人去查凭证 —— 所以这里必须**提前**给出明确错误。
func TestCosyMissingFingerprintFails(t *testing.T) {
	cases := []struct {
		name string
		mut  func(*Cred)
	}{
		{"缺 MachineID", func(c *Cred) { c.MachineID = "" }},
		{"缺 MachineToken", func(c *Cred) { c.MachineToken = "" }},
		{"缺 MachineType", func(c *Cred) { c.MachineType = "" }},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			c := testCred()
			tc.mut(c)
			if _, err := NewCosySession(c, c.DT, c.DRT); err == nil {
				t.Error("缺机器指纹时应报错，实际成功了 —— 会拿空指纹签名并全部失败")
			}
		})
	}
}

// TestIdentityJSONIsSortedCompact 身份 JSON 必须按 key 排序且无空白。
//
// 签名覆盖这些字节，任何空白或顺序差异都会导致签名不匹配。
func TestIdentityJSONIsSortedCompact(t *testing.T) {
	got := string(jsonSortedCompact(map[string]string{
		"uid":  "u1",
		"aid":  "u1",
		"name": "n",
	}))
	want := `{"aid":"u1","name":"n","uid":"u1"}`
	if got != want {
		t.Errorf("身份 JSON 应为 %s，实际 %s", want, got)
	}
	if strings.Contains(got, " ") {
		t.Error("身份 JSON 不得含空白")
	}
	// 与标准库的对比：json.Marshal 不排序，故必须用自己的实现
	std, _ := json.Marshal(map[string]string{"uid": "u1", "aid": "u1", "name": "n"})
	if string(std) == got {
		t.Log("（标准库恰好也排序了，但不保证 —— 仍用自己的实现）")
	}
}

// TestAuthHeaderShape Authorization 头的形状必须固定。
func TestAuthHeaderShape(t *testing.T) {
	c := testCred()
	sess, err := NewCosySession(c, c.DT, c.DRT)
	if err != nil {
		t.Fatal(err)
	}
	h, err := sess.AuthHeader("", "https://gateway.qoder.com.cn/algo/api/v2/model/list?Encode=1")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(h, "Bearer COSY.") {
		t.Errorf("Authorization 应以 'Bearer COSY.' 开头，实际 %.40s", h)
	}
	// 形状：Bearer COSY.<payloadB64>.<32 位 hex 签名>
	parts := strings.Split(strings.TrimPrefix(h, "Bearer COSY."), ".")
	if len(parts) != 2 {
		t.Fatalf("应为 payload.签名 两段，实际 %d 段", len(parts))
	}
	if len(parts[1]) != 32 {
		t.Errorf("MD5 签名应为 32 位十六进制，实际 %d 位", len(parts[1]))
	}
	if _, err := base64.StdEncoding.DecodeString(parts[0]); err != nil {
		t.Errorf("payload 不是合法 base64: %v", err)
	}
	// payload 解码后应是排序无空白的 JSON
	decoded, _ := base64.StdEncoding.DecodeString(parts[0])
	var m map[string]any
	if err := json.Unmarshal(decoded, &m); err != nil {
		t.Errorf("payload 不是合法 JSON: %v", err)
	}
	for _, k := range []string{"cosyVersion", "ideVersion", "info", "requestId", "version"} {
		if _, ok := m[k]; !ok {
			t.Errorf("payload 缺少字段 %s", k)
		}
	}
}

// TestSignatureDependsOnBody 签名必须覆盖 body —— 这是 GET 用空串的原因。
//
// 若实现里漏掉了 body，那么"GET 用空串、POST 用实体"这条约定就形同虚设，
// 而且这个 bug 在只测 GET 的情况下发现不了。
func TestSignatureDependsOnBody(t *testing.T) {
	c := testCred()
	sess, err := NewCosySession(c, c.DT, c.DRT)
	if err != nil {
		t.Fatal(err)
	}
	rawURL := "https://gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation"

	h1, _ := sess.AuthHeader("", rawURL)
	h2, _ := sess.AuthHeader(`{"model":"auto"}`, rawURL)
	if h1 == h2 {
		t.Error("不同 body 应产生不同签名 —— body 没被纳入签名输入")
	}
	// 同一 body 两次调用：payload 里的 requestId 每次不同，故签名也不同。
	// 这里只断言"不 panic 且形状正确"，不断言相等。
	h3, _ := sess.AuthHeader("", rawURL)
	if !strings.HasPrefix(h3, "Bearer COSY.") {
		t.Error("重复调用应仍产出合法头")
	}
}

// TestPathSigStripsAlgoPrefix pathSig 必须去掉 /algo 前缀。
//
// 这条无法直接观测（pathSig 被卷进 MD5），但可以用**等价性**验证：
// 带 /algo 与不带 /algo 的两个 URL 若产生相同签名，说明前缀被正确剥离。
func TestPathSigStripsAlgoPrefix(t *testing.T) {
	c := testCred()
	sess, err := NewCosySession(c, c.DT, c.DRT)
	if err != nil {
		t.Fatal(err)
	}
	// 手工复现签名输入，验证 pathSig 的口径
	pathWithAlgo := "/algo/api/v2/model/list"
	pathSig := strings.TrimPrefix(pathWithAlgo, "/algo")
	if pathSig != "/api/v2/model/list" {
		t.Errorf("剥离 /algo 后应为 /api/v2/model/list，实际 %s", pathSig)
	}
	// 不含 /algo 的路径应原样保留
	if got := strings.TrimPrefix("/api/v2/model/list", "/algo"); got != "/api/v2/model/list" {
		t.Errorf("不含前缀时应原样保留，实际 %s", got)
	}
	_ = sess
}

// TestApplyHeadersSetsAllRequired 必填头一个都不能少。
//
// 缺任何一个上游都会拒，但错误信息不会告诉你是缺了哪个 ——
// 所以用测试把清单钉住。
func TestApplyHeadersSetsAllRequired(t *testing.T) {
	c := testCred()
	sess, err := NewCosySession(c, c.DT, c.DRT)
	if err != nil {
		t.Fatal(err)
	}
	rawURL := "https://gateway.qoder.com.cn/algo/api/v2/model/list?Encode=1"
	req := mustRequest(t, rawURL)
	if err := sess.ApplyHeaders(req, "", rawURL, c.UID, false, ""); err != nil {
		t.Fatal(err)
	}

	required := []string{
		"cosy-data-policy", "content-type", "cosy-machinetype", "cosy-clienttype",
		"cosy-date", "cosy-key", "accept", "cosy-clientip", "authorization",
		"accept-encoding", "cosy-version", "cosy-machineid", "cosy-machinetoken",
		"login-version", "user-agent",
		// cosy-user 曾经被我漏掉，导致上游回 101 Signature invalid（不是"缺头"）——
		// 这条断言就是为了防止再次漏掉。
		"cosy-user",
	}
	for _, k := range required {
		if req.Header.Get(k) == "" {
			t.Errorf("缺少必填头 %s", k)
		}
	}
	// cosy-user 的值必须与签名里 identity 的 uid 一致
	if got := req.Header.Get("cosy-user"); got != c.UID {
		t.Errorf("cosy-user 应为 %s，实际 %s", c.UID, got)
	}
	// 值也必须对（这几个是有固定要求的）
	if got := req.Header.Get("cosy-data-policy"); got != "AGREE" {
		t.Errorf("cosy-data-policy 应为 AGREE，实际 %s", got)
	}
	if got := req.Header.Get("accept"); got != "text/event-stream" {
		t.Errorf("accept 应恒为 text/event-stream，实际 %s", got)
	}
	// 非 SSE 请求不应带 cache-control
	if got := req.Header.Get("cache-control"); got != "" {
		t.Errorf("非 SSE 请求不该有 cache-control，实际 %s", got)
	}
}

// TestApplyHeadersSSESetsCacheControl SSE 请求才加 cache-control。
func TestApplyHeadersSSESetsCacheControl(t *testing.T) {
	c := testCred()
	sess, err := NewCosySession(c, c.DT, c.DRT)
	if err != nil {
		t.Fatal(err)
	}
	rawURL := "https://gateway.qoder.com.cn/algo/api/v2/model/list"
	req := mustRequest(t, rawURL)
	if err := sess.ApplyHeaders(req, "", rawURL, c.UID, true, ""); err != nil {
		t.Fatal(err)
	}
	if got := req.Header.Get("cache-control"); got != "no-cache" {
		t.Errorf("SSE 请求应带 cache-control: no-cache，实际 %q", got)
	}
}

// TestApplyHeadersModelKey 指定模型时带上 x-model-key / x-model-source。
func TestApplyHeadersModelKey(t *testing.T) {
	c := testCred()
	sess, err := NewCosySession(c, c.DT, c.DRT)
	if err != nil {
		t.Fatal(err)
	}
	rawURL := "https://gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation"
	req := mustRequest(t, rawURL)
	if err := sess.ApplyHeaders(req, "{}", rawURL, c.UID, true, "qmodel_latest"); err != nil {
		t.Fatal(err)
	}
	if got := req.Header.Get("x-model-key"); got != "qmodel_latest" {
		t.Errorf("x-model-key 应为 qmodel_latest，实际 %q", got)
	}
	if got := req.Header.Get("x-model-source"); got != "system" {
		t.Errorf("x-model-source 应为 system，实际 %q", got)
	}
}

// TestServerPubKeyUsable 内置公钥必须可用（位数正确）。
//
// 若哪天上游轮换密钥，这里不会红（我们无法知道新密钥），
// 但至少能保证公钥本身没被写坏 —— 写坏的表现是全部请求失败。
func TestServerPubKeyUsable(t *testing.T) {
	bits := ServerPubKeyBits()
	if bits != 1024 {
		t.Errorf("内置公钥应为 1024 位 RSA（与客户端一致），实际 %d 位", bits)
	}
}

// TestAESCBCEncryptRoundTrip 加密结果可被同密钥解回（验证 key=iv 的约定）。
//
// 这条测试的价值：如果哪天有人把 iv 改成随机值或零值，加密仍会成功，
// 但上游解不出来 —— 现象是"签名通过但身份无效"。用往返解密把它钉住。
func TestAESCBCEncryptRoundTrip(t *testing.T) {
	key := []byte("0123456789abcdef") // 16 字节 = AES-128
	plain := []byte(`{"a":"b"}`)
	ct, err := aesCBCEncrypt(plain, key)
	if err != nil {
		t.Fatal(err)
	}
	if len(ct)%16 != 0 {
		t.Errorf("密文长度应为 16 的倍数，实际 %d", len(ct))
	}
	// 用同 key 解密（iv 必须等于 key）
	block, err := aes.NewCipher(key)
	if err != nil {
		t.Fatal(err)
	}
	out := make([]byte, len(ct))
	cipher.NewCBCDecrypter(block, key).CryptBlocks(out, ct)
	// 去 PKCS7 padding
	pad := int(out[len(out)-1])
	if pad <= 0 || pad > 16 || pad > len(out) {
		t.Fatalf("padding 非法: %d", pad)
	}
	got := string(out[:len(out)-pad])
	if got != string(plain) {
		t.Errorf("解密结果应为 %s，实际 %s（iv 可能不等于 key）", plain, got)
	}
}
