# implement.md — ZCode 接入执行计划

## 完成状态（2026-09-19）

| 子任务 | 状态 | 提交 | 说明 |
|---|---|---|---|
| Z1 接口重命名 | ✅ 完成 | `1fad353` | `QoderUpstream` → `ProductUpstream` |
| Z2 ZCode 后端 | ✅ 完成 | `195706a` | 凭证/双服务商/额度/模型/对话 |
| Z3 ZCode 登录 | ✅ 完成 | `85793dc` | OAuth 设备流 + 凭证兑换 + 导入 |
| Z4 宿主接入 | ✅ 完成 | `6433e33` | 8 个 Tauri 命令 + 8 条 HTTP 路由 |
| Z5 账号页 | ✅ 完成 | `6433e33` | `ZcodePage.tsx` + 侧边栏 + 路由 |
| Z6 网关派发 | ✅ 完成 | `ee3e780` | 接入多产品路由 |
| Z7 全流程实测 | ✅ 完成 | `be1acb9` | 真实凭证端到端 22/22 |
| Z8 宿主配置集成 | ✅ 完成 | `c113fd1` | 补上三个配置键 |
| Z9 开关链路 | ✅ 完成 | `4ed6a56` `d785d8f` | ZCode + Qoder |
| Z10 交互性验证 | ✅ 完成 | `7b1be71` | 界面操作真的调到后端 |

### 最终验证数字（对照基线：Go 13 包 / Rust 777 / UI 782）

| 项 | 基线 | 最终 | 变化 |
|---|---|---|---|
| Go | 13 包全绿 | **14 包全绿** + vet 干净 | +1 包（`internal/zcode`） |
| Rust | 777 通过 | **789 通过 / 0 失败** | +12 |
| UI 套件 | 782 通过 / 27 脚本 | **844 通过 / 31 脚本 / 0 失败 / 0 跳过** | +62 |
| TypeScript | 干净 | 干净 | — |

### ⚠ 本任务最大的教训：**每一段单独看都对，断在接缝处**

ZCode 的实现分三段：Go 后端 → 网关派发 → 宿主界面。每段的单元测试
都通过了，但端到端实测暴露出 **5 个真实缺陷**，全部断在**接缝**上：

| # | 缺陷 | 症状 | 断在哪 |
|---|---|---|---|
| 1 | 模型名大小写（`GLM-5.3` vs `glm-5.3`） | 上游报"模型不存在" | 客户端展示名 ≠ API 参数值 |
| 2 | 跨产品饿死（到期分层误用） | 账号在池里但永不入选 | 积分维度 ≠ 其它产品 |
| 3 | `(&http.Transport{}).Clone()` 破坏 h2 | `malformed HTTP response` | 手工构造 ≠ DefaultTransport |
| 4 | 宿主漏写 `pool.multi_product` | 账号**完全不加载** | Go 支持 ≠ 宿主写出 |
| 5 | 「停止接流量」只改账号库 | 开关是**摆设** | 宿主账号库 ≠ 网关扫的目录 |

**共同特征**：都是"看起来正常但实际没生效"——界面上看不出任何异常。

**为什么单元测试没抓到**：它们都测"一段内部是否正确"，
而缺陷都在**两段的契约**上（键名、形状、语义假设）。

**因此新增的验证都是契约级的**：
- `z8-host-config.cjs`：断言宿主写出的键名与 Go 读的键名一致
- `z9-noroute.cjs`：断言"宿主写标记 → 网关读标记 → 池排除"整条链路
- `verify-zcode-page-interactive.cjs`：断言"点按钮 → 发出请求 → 形状正确"

### Z7 实测中发现并修掉的三个真实缺陷（提交 `be1acb9`）

用**所有者真实的 ZCode 凭证**做端到端验证时，暴露出三个此前所有单元测试
都没覆盖到的缺陷。它们都属同一类：**失败信息指向错误的方向**。

1. **模型名大小写** —— 我用了 `client/configs` 的模型名（`GLM-5.3`），
   但那是**客户端展示名**；API 真正接受的是**小写** `glm-5.3`。
   上游严格区分大小写 → `11102 model not found` → ZCode 完全不可用。
2. **跨产品饿死** —— 到期分层的"未知到期日排最后"被跨产品套用，
   导致 ZCode 账号（无积分概念）**永不进候选集**。
   症状是"界面显示正常、日志显示已载入、但请求永远走 WorkBuddy"。
3. **`(&http.Transport{}).Clone()` 破坏 h2 协商** —— 确定性 0/8 失败，
   报 `malformed HTTP response`（那串字节其实是 h2 SETTINGS 帧）。
   我在排查中误判了两次（先怪 `TLSNextProto`，又怪网络波动）。

### 未测通项（如实记录）

**真实对话**：所有者那个 `bigmodel-coding-plan` 账号**额度已用尽**
（上游回 `1113 余额不足`），故无法验证完整对话链路。

用 **ZCode-only 池**做了确定性替代验证：上游既未报"模型不存在"、
又报了"额度耗尽" —— 说明请求路径与模型名映射都正确，只差额度。

若要补齐这一项，需要一个**有余额**的 ZCode 账号。

### 一处测试自身的缺陷（记录以免重犯）

`TestCostDoesNotOverrideExpiryTier` 原本拿 **Qoder** 账号当"下个月才到期"
的一方来断言到期分层 —— 那正是本次豁免的产品。修复后它失败，
但**那不是回归**，是测试把两个不同维度混在了一起。已改成两个 WorkBuddy 账号。

`mkProductAuth` 补了 ZCode 分支并加了注释：**每新增产品都必须补**，
漏掉时账号会被当成 WorkBuddy，测试不报错但断言失去意义。

---

## 0. 全局验证约定（每个子任务都适用）

### 0.1 必须遵守的既有铁律（来自 `AGENTS.md`）

- Windows + PowerShell；源码文件只用 edit/write 工具改（PowerShell 会破坏中文编码）
- `.ps1` 文件必须有 UTF-8 BOM
- **绝不**运行安装包/卸载器
- **绝不**按进程名批量杀进程；清理只按 PID + 路径双条件
- 验证一律独立实例：独立名 + 独立端口（5789x）+ 独立数据目录
- 所有者的 `:7864`（网关）与 `:43120`（DSH）**绝不能碰**

### 0.2 每个子任务的收尾命令

```powershell
# Go
cd wt-port\go-gateway; go test ./...; go vet ./...

# Rust
cd wt-port; cargo test --workspace

# 前端类型与构建
cd wt-port; npx tsc --noEmit; npm run build

# 敏感信息扫描（提交前必跑）
node uitest\final-identity-scan.cjs
```

### 0.3 基线（开工前实测）

| 项 | 基线 |
|---|---|
| Go | 13 包全绿 + vet 干净 |
| Rust | 777 通过 / 0 失败 |
| UI 套件 | 782 通过 / 0 失败 / 0 跳过（27 个脚本） |
| TypeScript | `tsc --noEmit` 干净 |
| 提交起点 | `2996f3c` |

**每个子任务结束时，这些数字只能升不能降。**

---

## Z1 接口重命名：`QoderUpstream` → `ProductUpstream`

### 开工前检查

- [ ] 跑一次全量测试，记录基线数字
- [ ] `grep -rn "QoderUpstream"` 列出全部引用点

### 步骤

1. 改接口名与注释（`server/dispatch.go`）
2. 改 `server.Config.Qoder` 字段名为 `Product`（**这是行为相关字段，改前先确认引用点**）
3. 改所有引用点
4. **不改任何逻辑** —— 只改名字

### 验证

- [ ] `go test ./...` 全绿，且**没有测试期望值变化**（`git diff` 里 `_test.go` 只有标识符改名）
- [ ] `go vet` 干净
- [ ] 启动独立实例，跑一次真实 WorkBuddy 请求（确认派发没坏）

### 风险与回滚点

纯重命名，风险最低。回滚 = `git revert`。

---

## Z2 ZCode 后端（依赖 Z1）

### 开工前检查

- [ ] 重读 prd.md §3（不需要签名的实测证据）
- [ ] 重读 design.md §2（协议要点）
- [ ] 确认两个服务商的端点可达（`uitest/probe-zcode-signing-required.cjs` 已验）

### 步骤

1. `internal/zcode/cred.go` —— 凭证解析（`{apiKey}.{secret}` 单段/两段）
2. `internal/zcode/provider.go` —— 两个服务商的端点表
3. `internal/zcode/client.go` —— 对话 / 额度 / 模型清单
4. `internal/zcode/errors.go` —— 错误分类（1000 / 1001 / `VERIFY_*`）
5. 单元测试（**重点是形状守卫**：上游字段改名时必须报错而不是存下 "undefined"）

### 验证

- [ ] Go 全绿 + vet 干净
- [ ] 新增测试覆盖：凭证解析三形态、错误分类、额度字段兼容（snake/camel）
- [ ] **真实上游探测**：用伪造凭证确认错误分类正确
      （`uitest/probe-zcode-signing-control.cjs` 的方法可复用）

### 风险与回滚点

- 风险：凭证兑换流程步骤多（z/login → customer → apikey → secret），
  任一环形状变化都会失败 → **每步都要形状守卫**
- 回滚：删除 `internal/zcode/` 目录即可（尚无调用方）

---

## Z3 ZCode 登录（依赖 Z2）

### 步骤

1. `internal/zcode/login.go` —— OAuth 设备流（`cli/init` + `cli/poll/{flow_id}`）
2. 凭证兑换（`z/login` → `getCustomerInfo` → `api_keys` → `copy/{key}`）
3. `internal/zcode/logincli.go` —— 宿主子命令（`zcode-login url|poll|import`）

### 验证

- [ ] 子命令实测：发起返回授权链接、轮询返回 pending、错误可读
- [ ] 授权链接真实可达（HTTP 200）
- [ ] 凭证导入（方式 B）可独立完成

### 风险与回滚点

- 风险：OAuth 流程需要真实账号才能端到端验证 → 留到 Z7
- 回滚：删除 `login.go` / `logincli.go`（凭证导入路径独立，不受影响）

---

## Z4 宿主接入（依赖 Z3）

### 步骤

1. `crates/ai-gateway-core/src/modules/zcode_account.rs` —— 账号库
   （与 `qoder_account.rs` 同构，但字段是"服务商"而非"区域"）
2. `crates/ai-gateway-core/src/modules/zcode_login.rs` —— 登录编排
3. `src-tauri/src/commands_apps.rs` —— Tauri 命令
4. `crates/ai-gateway-server/src/api.rs` —— HTTP 路由（dev 模式需要）
5. `src/lib/api.ts` —— 前端类型与调用

### 验证

- [ ] `cargo test --workspace` 全绿（基线 777，只增不减）
- [ ] `cargo check` 三个 crate 全过
- [ ] `tsc --noEmit` 干净
- [ ] **HTTP 路由必须加** —— 否则 dev 模式下页面整页白屏（C4 踩过这个坑）

### 风险与回滚点

- 风险：漏加 HTTP 路由 → dev 模式白屏（有先例）
- 回滚：`git revert`

---

## Z5 ZCode 账号页（依赖 Z4）

### 步骤

1. `src/pages/ZcodePage.tsx`（与 `QoderPage.tsx` 同构）
2. `src/App.tsx` —— 侧边栏加「ZCode 账号」+ 路由
3. 页面差异：
   - 服务商选择（Z.AI / 智谱）而非区域
   - **粘贴凭证**放在显眼位置（ZCode 的主路径）
   - 额度未知显示"未知"而不是 0
4. 更新 `uitest/mock-host-api.cjs` 加 zcode 路由（**必须**，否则白屏）
5. 更新 `uitest/verify-multi-product-naming.cjs` 加 ZCode 断言

### 验证

- [ ] 命名校验脚本更新后全过（4 个产品命名一致）
- [ ] 新增健壮性断言：后端旧于前端时**不白屏**（复刻 Qoder 的坑）
- [ ] UI 套件全绿

### 风险与回滚点

- 风险：白屏（有先例，已建防线）
- 回滚：`git revert`

---

## Z6 网关派发（依赖 Z2，可与 Z4/Z5 并行）

### 步骤

1. `main.go` —— `MultiProduct` 开启时加载 ZCode 凭证进池
2. `server.dispatch.go` —— 加 ZCode 分支
3. `internal/zcode/dispatch.go` —— 实现 `ProductUpstream` 接口

### 验证

- [ ] 派发测试：ZCode 账号必须发给 ZCode 上游（不能发给 WorkBuddy/Qoder）
- [ ] 反向断言：WorkBuddy/Qoder 账号**不**发给 ZCode
- [ ] 三产品都在时，选号不饿死任何一方（扩展现有的 `multiproduct_test.go`）
- [ ] 独立实例实测：三产品同池启动、路由正常

### 风险与回滚点

- 风险：派发错产品（静默失败，错误信息误导）→ 用"互相可区分的假上游"测
- 回滚：关 `pool.multi_product` 开关

---

## Z7 全流程实测 + 打包（最后一关）

### 开工前检查

- [ ] 自检 `:7864` 与 `:43120` 在运行
- [ ] 建独立实例（5789x + 独立数据目录 + 独立进程名）
- [ ] 凭证只读复制

### 实测项

| # | 项目 | 判据 |
|---|---|---|
| 1 | 凭证导入（方式 B） | 粘贴 `{apiKey}.{secret}` → 落盘 → 字段完整 |
| 2 | OAuth 登录（方式 A） | 真实完成授权 → 凭证落盘 |
| 3 | 额度查询 | 剩余/总量 = 上游真值（非占位） |
| 4 | 到期时间 | `expires_at` 正确解析 |
| 5 | 模型清单 | 与直连上游一致 |
| 6 | 走网关对话（流式） | SSE 正确、内容非空 |
| 7 | 走网关对话（非流式） | JSON 正确 |
| 8 | 三产品同池路由 | 都命中，不饿死 |
| 9 | WorkBuddy 回归 | 真实账号跑通现有流程 |
| 10 | Qoder 回归 | 不因新增产品而退化 |

### 人工交接点

第 2 项需要所有者**在浏览器点一次授权**。到这一步**停下来告诉他**，
不要试图自动化或代为操作。

### 打包

- 全部实测通过后，才构建安装包
- 打包命令见 `AGENTS.md`：`pwsh scripts/build-signed.ps1`
- **绝不运行安装包**

---

## 提交与收尾

- 每个子任务独立提交，Conventional Commit + 中文正文
- 提交前跑敏感信息扫描
- **未经所有者确认不推送、不发版**（既有约定）
