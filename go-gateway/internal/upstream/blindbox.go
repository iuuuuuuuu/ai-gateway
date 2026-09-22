// blindbox.go growth 域「盲盒」接口：查可开次数 / 开盒。
//
// # 为什么补它（2026-09-20，对照参考脚本 workbuddyv3）
//
// 所有者的参考脚本里有一项我们**完全没实现**的互动玩法：
//
//	t_blindbox —— 盲盒：能量足够就开（每次 10 能量，最多开 5 次）
//	  GET  /v2/activity/growth/buddy/quota  → {"balance":N,"affordable":M}
//	  POST /v2/activity/growth/buddy/open   {"count":1}
//	       → {"results":[{"instance":{...},"template":{"name":…,"rarity":…}}]}
//
// 逐项对照后，参考脚本的 23 项任务里我们**只缺这一个**（另一项
// `workstation_expert` 需要**杀掉并重启 WorkBuddy.exe** 做桌面换血对话，
// 不适合自动化，已在 growtask 里如实标为需人工）。
//
// # 为什么限次
//
// 参考实现写死 `min(affordable, 5)`。我们沿用同一上限：开盒消耗能量
//（每号每日有限），一次跑满会把后续玩法（抽奖、兑换）要用的能量吃光。
// 上限做成常量便于对照与调整。
package upstream

import (
	"encoding/json"
	"errors"
	"net/http"
	"strconv"

	"workbuddy2api/internal/auth"
)

const (
	// blindboxQuotaPath 查盲盒余额与「还能开几次」。
	blindboxQuotaPath = "/activity/growth/buddy/quota"
	// blindboxOpenPath 开盒（每次消耗固定能量）。
	blindboxOpenPath = "/activity/growth/buddy/open"
)

// MaxBlindboxOpens 单轮最多开几个盒子。
//
// 与参考实现一致（`min(affordable, 5)`）。理由见文件头：
// 开盒吃能量，一次开满会挤掉抽奖/兑换要用的额度。
const MaxBlindboxOpens = 5

// BlindboxQuota 盲盒配额。
type BlindboxQuota struct {
	// Balance 当前能量余额。
	Balance int64
	// Affordable 按单次消耗算，还能开几个（上游直接给，不由我们推算 ——
	// 单次消耗可能随活动调整，自己算会算错）。
	Affordable int64
}

// GrowthBlindboxQuota 查盲盒配额。
func (c *Client) GrowthBlindboxQuota(a *auth.Auth) (*BlindboxQuota, error) {
	data, err := c.growthJSON(a, http.MethodGet, blindboxQuotaPath, nil)
	if err != nil {
		return nil, err
	}
	var resp struct {
		Balance    int64 `json:"balance"`
		Affordable int64 `json:"affordable"`
	}
	if err := json.Unmarshal(data, &resp); err != nil {
		return nil, err
	}
	return &BlindboxQuota{Balance: resp.Balance, Affordable: resp.Affordable}, nil
}

// BlindboxItem 开出来的一个盒子。
type BlindboxItem struct {
	// Name 物品名（优先 instance.name，回退 template.name）。
	Name string
	// Rarity 稀有度（同上取法）。
	Rarity string
}

// GrowthBlindboxOpen 开 count 个盒子（count 会被夹到 [1, MaxBlindboxOpens]）。
//
// 返回**实际开出来的**物品列表 —— 上游可能只回一部分（能量在中途耗尽），
// 故调用方应以返回值的长度为准，而不是请求的 count。
func (c *Client) GrowthBlindboxOpen(a *auth.Auth, count int) ([]BlindboxItem, error) {
	if count <= 0 {
		return nil, errors.New("开盒数量必须为正")
	}
	if count > MaxBlindboxOpens {
		count = MaxBlindboxOpens
	}
	body := map[string]any{"count": count}
	data, err := c.growthJSON(a, http.MethodPost, blindboxOpenPath, body)
	if err != nil {
		return nil, err
	}
	var resp struct {
		Results []struct {
			Instance struct {
				Name   string `json:"name"`
				Rarity any    `json:"rarity"`
			} `json:"instance"`
			Template struct {
				Name   string `json:"name"`
				Rarity any    `json:"rarity"`
			} `json:"template"`
		} `json:"results"`
	}
	if err := json.Unmarshal(data, &resp); err != nil {
		return nil, err
	}
	out := make([]BlindboxItem, 0, len(resp.Results))
	for _, r := range resp.Results {
		name := r.Instance.Name
		if name == "" {
			name = r.Template.Name
		}
		rarity := rarityText(r.Instance.Rarity)
		if rarity == "" {
			rarity = rarityText(r.Template.Rarity)
		}
		out = append(out, BlindboxItem{Name: name, Rarity: rarity})
	}
	return out, nil
}

// rarityText 把稀有度字段转成字符串。
//
// ⚠ 上游的稀有度**可能是数字也可能是字符串**（参考实现直接用 `%s` 打印，
// 两种都能出）。Go 的 `json.Unmarshal` 到 string 会在遇到数字时报错，
// 故这里用 any 接住再转 —— 否则一个数字稀有度会让整个开盒结果解析失败。
func rarityText(v any) string {
	switch t := v.(type) {
	case nil:
		return ""
	case string:
		return t
	case float64:
		// JSON 数字统一解成 float64；整数值不要显示成 "3.000000"
		if t == float64(int64(t)) {
			return strconv.FormatInt(int64(t), 10)
		}
		return strconv.FormatFloat(t, 'f', -1, 64)
	case bool:
		if t {
			return "true"
		}
		return "false"
	default:
		return ""
	}
}
