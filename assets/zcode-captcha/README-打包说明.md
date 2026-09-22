# ZCode 验证码求解器的**单文件打包**说明

## ⚠ 2026-09-21：主路径已改为宿主的 WebView2，本地 Node 求解降级为回退

**现在正常情况下这份文档里的 Node 求解器不会被用到。**

新的主路径：宿主自带的 **WebView2**（Windows 10/11 预装，零体积）在真实浏览器
环境里跑阿里云官方 SDK —— 与官方 ZCode 客户端同一做法（已从
`D:\APP\ZCode\resources\app.asar` 核实）。

数据对比（都是实测）：

| | 本地 Node + happy-dom（旧主路径） | 宿主 WebView2（新主路径） |
|---|---|---|
| 安装包体积 | +81MB（要内置 node.exe） | **0**（系统自带） |
| 单次求解 | 约 3 秒 | **929 毫秒** |
| 成功率 | 约 40%（调 stallMs 后 88%） | 官方路径 |
| 环境 | 模拟，被风控盯上 | 真实浏览器 |

实测记录（`uitest/probe-captcha-in-browser.cjs`，有头窗口，脚本触发无人工点击）：

```text
[+10ms]   initAliyunCaptcha 已调用
[+600ms]  getInstance 触发
[+601ms]  调用 startTracelessVerification()
[+929ms]  success，param 长度 280
```

### 那这份文档还有什么用

**回退路径仍然保留**：宿主服务不可用时（还没启动完、端口异常、用户环境特殊），
网关会自动回退到这里描述的本地 Node 求解 —— 有总比完全没有好。

故 `solver.bundle.cjs` 继续随包分发（不到 1MB，单个文件，不拖慢安装）。

### 为什么**不**再内置 node.exe

曾一度把 81MB 的 `node.exe` 放进本目录以解决"用户没装 Node"的问题。
WebView2 方案落地后已移除 —— 那是本方案要消除的主要代价。

用户若恰好装了 Node，回退路径会自动找到（探测 14 处常见安装位置，
含 nvm/nvmd/fnm/volta/scoop/choco/winget/官方安装包/程序同目录）。

---

## 以下是**回退路径**的说明（本地 Node 求解）

## 为什么有这个文件

ZCode 对话通道要求 `X-Aliyun-Captcha-Verify-Param`（实测回
`400 code=3007 captcha verify failed`）。求解器用 **happy-dom** 在 Node 里
模拟浏览器环境跑阿里云官方 SDK 拿到它。

## 为什么要打包成单文件（所有者的反馈）

> 「安装的时候那个 node_modules 解压速度超级慢,不能这样子,太慢了 需要优化」

原始做法是把 `node_modules/` 整个塞进安装包：

	11.36MB / **3353 个文件**

而 Tauri 的 NSIS 模板对每个资源文件生成一条
`File /a "/oname=..."` —— **逐文件解压**。3353 个零散小文件的写入
必然远慢于几个大文件（尤其被杀软逐个扫描时）。

对比：这次的另一半资源（`WebView2Loader.dll`）只有 1 个文件，
所以以前的包"可快了"。

## 现在的做法

用 esbuild 把 `solver.js` + `happy-dom` 打成**单个 CommonJS 文件**：

	solver.bundle.cjs   928KB   **1 个文件**

安装时的文件写入从 3353 次降到 1 次。

## 怎么重建（happy-dom 升级或求解器更新时）

```powershell
# 1) 准备一个临时目录
$t = "$env:TEMP\zcode-captcha-build"
Remove-Item $t -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $t | Out-Null
Copy-Item "assets\zcode-captcha\solver.js","assets\zcode-captcha\package.json" $t -Force

# 2) 装依赖（happy-dom 是 solver.js 的唯一必需依赖；undici 可选）
Set-Location $t
npm install --no-audit --no-fund happy-dom

# 3) 打单文件（--minify 可把 2.4MB 压到 0.91MB，已验证可用）
npm install --no-save --no-audit --no-fund esbuild
& node_modules\.bin\esbuild.cmd solver.js `
    --bundle --platform=node --format=cjs --target=node20 `
    --minify --external:undici `
    --outfile=solver.bundle.cjs

# 4) 验证（**必须做**：打包后跑一次真实求解）
node solver.bundle.cjs 11xygtvd cn no8xfe
#   期望 stdout 出现：VERIFY_PARAM=<280 字符左右>

# 5) 拷回仓库
Copy-Item solver.bundle.cjs "assets\zcode-captcha\solver.bundle.cjs" -Force
```

## ⚠ 三个必须知道的坑

**1. 必须 `--external:undici`**

`undici` 是**可选**的代理支持（`solver.js` 里 try/catch 包着）。
打进去会显著增大体积且毫无必要 —— 我们不用它的代理能力。

**2. 验证时必须分离 stdout / stderr**

求解器把**调试信息写 stderr**（`[pe-stall] …`）、**结果写 stdout**
（`VERIFY_PARAM=…`）。用 `2>&1` 合并会被调试信息抢先，看起来像失败。
我因此误判过一次"求解器不可用"。

**3. 求解会被上游限流**

连续跑几次后回 `[pe-stall] pe.xxx.js x1` 且退出码 2；等约 90 秒恢复。
这是**预期行为**（`captcha.go` 里设了 2 分钟冷却就是为此），
不是打包坏了。

## 为什么仍保留 solver.js 源码

打包产物是**不可读**的（minify 后 928KB 一行）。若哪天要排查
"为什么求解失败了"，源码是唯一的线索。60KB 的代价换可维护性，值得。

## 运行方式

Go 侧 `captcha.go` 按这个顺序找可执行文件：

	1. 环境变量 ZCODE_NODE_PATH
	2. PATH 里的 node

⚠ **不内置 node** —— 真实 `node.exe` 有 **87MB**，打进发行包代价过大。
找不到就如实报"求解验证码需要 Node.js"，而不是静默失效。
