package server

// measured.go 记录**逐账号实测**得到的图片能力结论，用于覆盖上游元数据。
//
// ## 为什么需要「覆盖上游」
//
// 上游 /v3/config 对每个模型都给了 supportsImages（bool），但实测发现它对
// **两区同名、后端不同**的模型给出的是**同一份**（错误）答案：
//
//	glm-5.3 / glm-5.2   两区都报 supportsImages=true
//	  国服后端   真的能读图（64x64 红图 → prompt_tokens +22，答「红色」）
//	  国际版后端 读不到图（同样的图 → 增量恒 +33，答「我无法查看图片」）
//
// 直接信上游的后果：客户端在本地就认定「支持图片」，用户拖图进去，
// 请求被路由到国际版后端，图片被**静默**替换成占位符，模型回一句
// 「抱歉，我无法查看图片」—— 用户以为模型不行，实际是元数据撒了谎。
//
// 因此这里把实测结论显式记下来，作为比上游声明**更高优先级**的真值来源。
// 实测优先于声明，是因为声明可能过期或本身有误，而实测是端到端验证过的。
//
// ## 判据（怎么算「实测确认不支持」）
//
// 发一张**纯色小图**，问「这是什么颜色」，同时对比纯文本请求的 prompt_tokens：
//
//	图片被真正解码  → token 增量随图片体积变化，且能答对颜色
//	图片被换占位符  → 增量是**常数**（与图片体积无关），且答「无法查看图片」
//
// 常数增量是关键：把 64x64 红图换成 512x512 蓝图，增量**完全相同**，
// 这不可能是解码结果，只能是固定长度的占位符。
//
// ## 实测记录（2026-09-16）
//
// 本机 2 个国服（*.workbuddy.cn）+ 5 个国际版（*.workbuddy.ai）账号，
// 模型 × {64x64 纯红, 512x512 纯蓝}：
//
//	模型          区域    文本pt  小图pt  大图pt  小图Δ  大图Δ  回答
//	glm-5.3       国服     26      48      393     +22   +367   红色 / 蓝色   ✅
//	glm-5.3       国际版   26      59       59     +33    +33   无法查看图片   ❌
//	glm-5.2       国服     20      48      393     +28   +373   红色 / 蓝色   ✅
//	glm-5.2       国际版   20      53       53     +33    +33   无法查看图片   ❌
//	hy3           国服     25      47      205     +22   +180   红色 / 蓝色   ✅
//	hy3           国际版   25     184      184    +159   +159   红色          ⚠️
//	kimi-k2.6     国服     23      40       40     +17    +17   红色          ⚠️
//	kimi-k2.6     国际版   23      40       40     +17    +17   颜色          ⚠️
//
// 结论分三类，**不能一概而论**：
//
//	glm-5.2 / glm-5.3  国服能读，国际版不能 → 需要区域偏好 + 覆盖国际版声明
//	hy3                两区都能读（编码不同）→ 不偏好，也不覆盖
//	kimi-k2.6          两区都能读（增量同为常数 17，但答对了颜色）
//
// kimi 那行是刻意留着的反例：它的增量**也是常数**，但回答正确。
// 只看「增量是否常数」会把 kimi 误判成不支持 —— 所以判据必须是
// 「增量常数 **且** 回答否认看到图片」，两者同时成立才算不支持。

import (
	"strings"

	"workbuddy2api/internal/auth"
)

// imageCapability 实测得到的图片能力。
type imageCapability int

const (
	// measUnmeasured 未实测：以模型为单位，语义是「没有 conflicting 的实测结论」。
	//
	// 与「实测支持」不同：实测支持是「验过，确实能读」；未实测只是
	// 「没验出问题」——对上游声明不做任何覆盖，保持原样透传。
	measUnmeasured imageCapability = iota
	// measSupported 实测确认能读图。
	measSupported
	// measUnsupported 实测确认读不到图（图片被替换成固定占位符）。
	measUnsupported
)

// measuredImageCapability 返回某模型在指定区域的**实测**图片能力。
//
// region 为 RegionAny 时返回「各区域实测结论的交集」：
//
//	两区都实测支持     → measSupported
//	两区都实测不支持   → measUnsupported
//	一支持一不支持     → measUnsupported（**保守**：网关无法保证用户会被路由到
//	                    能读图的那一侧，谎报 true 会让客户端发出必然静默降级的请求）
//	任一区未实测       → measUnmeasured（不覆盖）
//
// 保守优先（conflicting 取 false）是刻意的：宁可让客户端在混合池下少用一点
// 图片能力，也不要让它发出一个「看起来成功、实际图片被丢掉」的请求 ——
// 后者用户完全无感知，只表现为模型「莫名其妙说看不见图片」。
func measuredImageCapability(model string, region auth.Region) imageCapability {
	m := strings.ToLower(strings.TrimSpace(model))
	if m == "" {
		return measUnmeasured
	}
	cn := measuredForRegion(m, auth.RegionCN)
	intl := measuredForRegion(m, auth.RegionIntl)

	if region == auth.RegionAny {
		switch {
		case cn == measUnmeasured || intl == measUnmeasured:
			return measUnmeasured
		case cn == measSupported && intl == measSupported:
			return measSupported
		default:
			return measUnsupported
		}
	}
	return measuredForRegion(m, region)
}

// measuredForRegion 单个模型的区域实测结论。
//
// 用 switch 而不是 map：条目很少（目前只有 glm-5.2/5.3 两区各不相同），
// switch 让每条结论与它的证据注释写在**一起**，读代码时不用跳来跳去查表。
// 表大了再换 map。
func measuredForRegion(model string, region auth.Region) imageCapability {
	switch model {
	case "glm-5.3", "glm-5.2":
		// 国服能读、国际版读不到（增量恒 +33，与图片体积无关，答「无法查看图片」）。
		if region == auth.RegionCN {
			return measSupported
		}
		return measUnsupported
	}
	return measUnmeasured
}

// measuredOverride 按实测结论覆盖上游声明的图片能力。
//
// 三种情形：
//
//	实测不支持 → 覆盖成 false（**不管上游说什么**）。这是本函数存在的全部意义：
//	             上游对国际版 glm-5.x 报 true，实测是 false，必须以实测为准。
//	实测支持   → 覆盖成 true（上游漏标时也能纠正）。
//	未实测     → 原样返回上游声明（三态：nil 仍是 nil）。
func measuredOverride(model string, region auth.Region, upstream *bool) *bool {
	switch measuredImageCapability(model, region) {
	case measUnsupported:
		f := false
		return &f
	case measSupported:
		t := true
		return &t
	default:
		return upstream
	}
}
