## AI Gateway 1.0.25 · 正式版

> 本版只做一件事：**把「页面切换保持状态缓存」真正修好**。
> 你报的是「切到别的页面再切回来，不要每次都重新加载、重新拉数据」。1.0.24 的启动卡顿修复、1.0.23 的 ⑪⑫⑬ 全部保留，本版没有回退任何东西。

---

## 一、你报的问题，结果

| 你的原话 | 结论 |
| --- | --- |
| 「页面切换保持状态缓存，不要重置」 | **上一版是坏的，本版已修好**。根因：`App.tsx:239` 的 `<ErrorBoundary key={location.pathname}>` 让**每次点平台标签都换一次 key**，React 于是把整棵内容子树卸载重建 —— 保活的面板 DOM 全没了，`visited` 集合也被重置 |

实测对比（同一台机器、同一套断言、隔离实例）：

| 指标 | 1.0.24 | **1.0.25** |
| --- | --- | --- |
| 切到 Trae 后 WorkBuddy 面板仍在 DOM | **否**（`marks:[]`、`panels:7`） | **是**（`marks:["KA-1"]`、`panels:10`） |
| 切回来后原 DOM 节点还在（expando 未丢） | **否** | **是** |
| 切回来后三级视图仍停在「积分统计」 | **否** | **是**（`?view=credits` + 选中项都还在） |
| 真机断言通过数 | 14/18（2 条真缺陷） | **21/21** |

> 「expando」是我给面板 DOM 打的私有标记 `__kaMark='KA-1'`。切走再切回来，标记还在 ⇒ **同一个 DOM 节点没被重建**，这才是「状态缓存」的硬证据。

---

## 二、两处修法（3 个文件 / +71 / −7，全在前端）

### 修法 1：`key` 换成只在出错时才复位的 `resetKey`（`src/App.tsx` + `src/components/error-boundary.tsx`）

```tsx
// App.tsx
- <ErrorBoundary key={location.pathname}>
+ <ErrorBoundary resetKey={location.pathname}>

// error-boundary.tsx
interface Props { children: ReactNode; resetKey?: unknown; }

componentDidUpdate(prevProps: Props) {
  if (prevProps.resetKey !== this.props.resetKey && this.state.error) {
    this.reset();
  }
}
```

- 正常路径：没有错误 ⇒ 什么都不做 ⇒ **子树不再被卸载**；
- 出错路径：切路由时确实处于错误态 ⇒ 复位 ⇒ **原作者「别让上一个页面的错误粘住新页面」的意图原样保留**。

### 修法 2：三级视图（账号 / 积分统计 / Token 统计）写进 URL（`src/pages/AccountHubPage.tsx`）

只保活 DOM 还不够 —— `view` 原本是组件 state，路由一变视图还是会回到「账号」。改成从 URL 派生：

```tsx
const [searchParams, setSearchParams] = useSearchParams();
const raw = searchParams.get("view");
const view: WorkbuddyView = isWorkbuddyView(raw) ? raw : "accounts";
// onValueChange 用 replace，不往历史栈塞记录
setSearchParams((prev) => { ... }, { replace: true });
// 平台标签互切时带上 ?view=
navigate("/accounts/" + value + (qs ? "?" + qs : ""));
```

URL 参数是不可信输入，所以配了类型守卫 `isWorkbuddyView`。

---

## 三、回归与测试基线

```
cargo test --workspace   EXIT=0
  15 suites / 2985 passed / 0 failed / 5 ignored     （uitest\ws-test-1025.log）
```

与 1.0.24 持平 —— 本版只动前端，没有新增 Rust 测试。

前端：`tsc --noEmit` EXIT=0 → `vite build` EXIT=0 → 新 bundle `index-QnJFNcf6.js`；包内可搜到 `resetKey` / `componentDidUpdate`。

★ 本仓库前端没有单测基础设施（无 vitest、`src` 下 0 个测试文件），所以前端改动的验证链是 `tsc` → `vite build` → **真机 CDP 断言（21 条）**。

---

## 四、交付物

| 文件 | 大小 | SHA256 |
| --- | --- | --- |
| `dist\AI Gateway_1.0.25_x64-setup.exe` | 14,460,297 B | `09EF17DE160AD3851F96BF6594DA2E3CDA342C4ED7D4BC0C8D9E004F0A453084` |
| `dist\AI Gateway_1.0.25_x64-setup.exe.sig` | 424 B | `5D60A23C5CF9D47B3187BD7AF4CEF77939B5180AB9A5D8FEF8EA80A66A0D563C` |

签名 keyid `7e7ce64b6fc3a36f`。安装包用 7-Zip 独立解包核验过（不运行安装器）：包内主程序与 `wt-port\target\release\ai-gateway.exe` **逐字节只差 3 个字节**（偏移 32877328-32877330，tauri 写的 bundle 类型标记 `NSS` vs `UNK`）⇒ 证明包由当前工作区构建；版本串 UTF-16LE `1.0.25` 出现 2 次、`1.0.24` / `1.0.23` 各 0 次；内嵌网关反解出来 SHA256 `B93CF45C…`，与 `crates\ai-gateway-core\embedded\gateway.exe` 逐字节相同。

---

## 五、安装

1. 从托盘**退出**正在运行的 AI Gateway（⚠ 见下）
2. 运行 `dist\AI Gateway_1.0.25_x64-setup.exe`（配套 `.sig` 一起留着，更新器要校验）

### ⚠ 安装前请先退出正在运行的 AI Gateway（重要）

NSIS 模板的 `CheckIfAppIsRunning` 是**按可执行文件名**结束进程的（`installer.nsi:641` / `:768`）—— 它会直接杀掉 `ai-gateway.exe`。若你当时正在跑任务，任务会被硬中断。**先托盘退出，再装。**

---

## 六、仍未做到的（如实说）

1. **第一次点「积分统计」仍要 1.8–5 s**（页面内精确计时：URL 434 ms 就变了，面板 5061 ms 才激活）。原因是 `CreditStatsPage.tsx` 1,607 行 + `recharts` 图表库首挂载 + 首屏拉数据，**跟本次改动无关，1.0.24 也一样**。本版只把「切回来」的代价从「重新挂载」降到 **11–512 ms**。如果你觉得第一次那几秒也难忍，下一版我专门治它（懒挂载 + 骨架屏）。
2. **keep-alive 的边界**：本版保证「平台标签互切」与「三级 tab 互切」。整页刷新（F5 / 重启应用）当然会重建，这是预期行为。
3. `crates/ai-gateway-router/Cargo.toml` 与 `crates/ai-gateway-core/Cargo.toml` 的 `version` 仍是 `1.0.23`（只有根包三处提到 1.0.25）。网关二进制不带版本串，无功能影响。
4. 本版**没有**在你真机上安装过 —— 所有实测都在隔离实例（`AI_GATEWAY_HOME=test-bin\pkg1025\home`，独立端口 `:7874`/`:9418`，独立 WebView2 配置目录 `%LOCALAPPDATA%\pkg10250000001igateway\EBWebView`）里做，**你正在用的实例（`:7864`）全程未动**。
5. 断言装置用的是合成鼠标事件（CDP `Input.dispatchMouseEvent`），验证的是「点击后的 DOM / URL 正确性」，**没有**验证手感。

