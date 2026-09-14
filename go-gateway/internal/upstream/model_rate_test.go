package upstream

import (
	"testing"
	"time"
)

// 用 Issue/需求现场的真实报错文案验证解析。
//
// 上游原始响应体（429）：
//
//	{"code":"6004","msg":"您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，
//	 您也可以切换其他模型继续使用。","requestId":"62b8393f-..."}
func TestParseResetTimeRealSample(t *testing.T) {
	const real = `{"code":"6004","msg":"您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。","requestId":"62b8393f-e7ce-46e6-912e-6e610a8d1e01"}`

	// 取一个早于该时刻的 now，让它成为"未来时刻"。
	now := time.Date(2026, 9, 15, 10, 0, 0, 0, time.Local)
	got, ok := ParseResetTime(real, now)
	if !ok {
		t.Fatalf("未解析出重置时间")
	}
	// UTC+8 的 13:25:47 = 本地（若本地为 +08:00）13:25:47
	want := time.Date(2026, 9, 15, 13, 25, 47, 0, time.FixedZone("UTC+8", 8*3600))
	if !got.Equal(want) {
		t.Errorf("解析结果=%s (%s)，期望 %s (%s)", got.Format(time.RFC3339), got.Location(), want.Format(time.RFC3339), want.Location())
	}
	// 关键：与"现在"的实际间隔应约 3h25m（若本地就是 +08:00）
	t.Logf("now=%s  重置=%s  间隔=%s", now.Format(time.RFC3339), got.Format(time.RFC3339), got.Sub(now))
}

func TestParseResetTimeVariants(t *testing.T) {
	// 基准用 UTC 00:00，确保下面各时区的 13:25:47 都落在"未来"。
	base := time.Date(2026, 9, 15, 0, 0, 0, 0, time.UTC)
	cases := []struct {
		name string
		body string
		ok   bool
		want string // RFC3339 in UTC
	}{
		{
			"UTC+8",
			`{"code":6004,"msg":"将在 2026-09-15 13:25:47 UTC+8 重置"}`,
			true, "2026-09-15T05:25:47Z",
		},
		{
			"UTC+08:00",
			`{"code":6004,"msg":"将在 2026-09-15 13:25:47 UTC+08:00 重置"}`,
			true, "2026-09-15T05:25:47Z",
		},
		{
			"UTC-5",
			`{"code":6004,"msg":"将在 2026-09-15 13:25:47 UTC-5 重置"}`,
			true, "2026-09-15T18:25:47Z",
		},
		{
			"Z 后缀",
			`{"code":6004,"msg":"将在 2026-09-15 13:25:47 Z 重置"}`,
			true, "2026-09-15T13:25:47Z",
		},
		{
			"ISO T 分隔符",
			`{"code":6004,"msg":"reset at 2026-09-15T13:25:47 UTC+8"}`,
			true, "2026-09-15T05:25:47Z",
		},
		{
			"已过去的时刻 → 不采用",
			`{"code":6004,"msg":"将在 2026-09-14 05:00:00 UTC+8 重置"}`,
			false, "",
		},
		{
			"无时间字面量",
			`{"code":6004,"msg":"您的使用量已超出频率限制"}`,
			false, "",
		},
		{
			"非法月日",
			`{"code":6004,"msg":"将在 2026-13-45 99:99:99 UTC+8 重置"}`,
			false, "",
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got, ok := ParseResetTime(c.body, base)
			if ok != c.ok {
				t.Fatalf("ok=%v want %v (got=%s)", ok, c.ok, got)
			}
			if !c.ok {
				return
			}
			want, _ := time.Parse(time.RFC3339, c.want)
			if !got.Equal(want) {
				t.Errorf("got=%s want=%s", got.UTC().Format(time.RFC3339), c.want)
			}
		})
	}
}

// 无时区后缀时按本地时区解释（上游历史上返回过裸本地时间）。
func TestParseResetTimeNoZoneUsesLocal(t *testing.T) {
	body := `{"code":6004,"msg":"将在 2026-09-15 13:25:47 重置"}`
	now := time.Date(2026, 9, 15, 10, 0, 0, 0, time.Local)
	got, ok := ParseResetTime(body, now)
	if !ok {
		t.Fatal("未解析出")
	}
	want := time.Date(2026, 9, 15, 13, 25, 47, 0, time.Local)
	if !got.Equal(want) {
		t.Errorf("got=%s want=%s（应按本地时区）", got, want)
	}
}

func TestIsModelRateLimited(t *testing.T) {
	cases := []struct {
		name string
		body string
		want bool
	}{
		{"真实样本 code 6004", `{"code":"6004","msg":"您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。"}`, true},
		{"数字型 code", `{"code":6004,"msg":"x"}`, true},
		{"仅文案兜底", `{"code":"9999","msg":"您也可以切换其他模型继续使用。"}`, true},
		{"账号级限流不应误判", `{"code":"1001","msg":"请求过于频繁，请稍后重试"}`, false},
		{"空体", ``, false},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := IsModelRateLimited(c.body); got != c.want {
				t.Errorf("IsModelRateLimited=%v want %v", got, c.want)
			}
		})
	}
}

// Classify 必须把"429 + 模型限流"判为 ErrModelRate，把普通 429 仍判为 ErrSoftRate。
func TestClassifyModelRate(t *testing.T) {
	model429 := `{"code":"6004","msg":"您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。"}`
	if got := Classify(429, model429); got != ErrModelRate {
		t.Errorf("429+6004 → %v want ErrModelRate", got)
	}
	if got := Classify(429, `{"code":"1001","msg":"too many requests"}`); got != ErrSoftRate {
		t.Errorf("普通 429 → %v want ErrSoftRate", got)
	}
	if got := Classify(429, ``); got != ErrSoftRate {
		t.Errorf("空体 429 → %v want ErrSoftRate", got)
	}
	// 模型限流判定必须只在 429 下生效：同样是 6004 文案但 5xx 不该误判。
	if got := Classify(500, model429); got != ErrServer {
		t.Errorf("500+6004文案 → %v want ErrServer（模型限流只在 429 下判定）", got)
	}
}
