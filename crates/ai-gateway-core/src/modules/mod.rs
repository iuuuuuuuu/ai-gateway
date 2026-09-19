pub mod account;
// 账号记录：任务 / 积分 / Token 三类事件的统一追加式流水，供单账号记录视图查询。
pub mod account_records;
pub mod agent_import;
// 多应用档案表（5 应用 × 3 快照布局）：切换流程的表驱动参数源。
pub mod app_profile;
pub mod auth_file;
pub mod checkin;
// CLI 任务模式（--task-run <key>，刻意不启动 Tauri）
pub mod cli_task;
pub mod codebuddy_cli;
pub mod codebuddy_cn_ide;
pub mod config;
pub mod credit_usage;
pub mod credits;
// MITM 设备身份代理（hyper 协议栈自建，不依赖 Tauri）：CA 签发 / 抓包改写 /
// 豆包与 Trae 凭证捕获 / 系统代理编排 / OAuth 直连豁免 / WS 桥接。
pub mod device_proxy;
// 豆包：账号池与凭证 / 会话保活 / 会员额度 / 对话备份导出
pub mod doubao_account;
pub mod doubao_chats;
pub mod doubao_quota;
pub mod doubao_session;
pub mod export_import;
pub mod gateway;
pub mod gateway_embed;
// 数据目录迁移（~/.ai-gateway → ~/.wb-switch，复制式、幂等）
pub mod migrate_store;
// 模型计费真值（哪些模型在哪个区域免费）：判定依据是上游 /v3/config 的
// credits 倍率，经 Go 网关 /v1/models/regions 按区域透出，**不硬编码清单**。
pub mod model_billing;
pub mod oauth;
pub mod official_usage;
pub mod process;
// Qoder（QoderWork）：账号库。凭证由 Go 侧 internal/qoder 管理（COSY 签名
// 需要机器指纹与 dt/drt 令牌），宿主只存界面需要的元信息。
pub mod qoder_account;
// Qoder 登录编排：调 `gateway qoder-login url|poll` 两个子命令。
// 协议实现（COSY 签名 / PKCE 设备流）只在 Go 侧有一份，经真实上游验证。
pub mod qoder_login;
// ZCode（Z.AI / 智谱 GLM 编码套餐）：账号库 + 登录编排。
// 凭证由 Go 侧 internal/zcode 管理；与 Qoder 的差异是**服务商**而非区域，
// 且凭证是用户可复制的字符串（故"导入"是主路径）。
pub mod zcode_account;
pub mod zcode_login;
// 扫描本机 ZCode 客户端的登录凭证 → 一键导入，省掉手工粘贴。
//
// ⚠ **Qoder 没有对应的模块，这是刻意的**：它的凭证在自定义加密的
// `auth.v1.dat` 里（实测熵 7.5、明文占比 7%、非标准 DPAPI），本机没有
// 明文副本 —— 拿不到就**不做**那个按钮，否则是个点了没反应的摆设。
pub mod zcode_scan;
// 读客户端 `credentials.json` 里的**账号名**（`enc:v1:` 解密）。
//
// 与 zcode_scan 分开是因为关注点不同：那个负责"找出可导入的凭证"，
// 这个负责"把账号叫什么弄对"。解密逻辑必须逐字照抄客户端源码，
// 故独立成文件并把源码出处写在里面。
pub mod zcode_credstore;
pub mod refresh;
pub mod rotate;
// Windows 计划任务（Trae 签到 / 豆包保活 / 额度巡检）
pub mod scheduler;
pub mod session;
pub mod switch;
// 登录态切换器（Trae 系 icube 布局 / 豆包 chromium 布局）
pub mod switcher;
pub mod token_stats;
// Trae：账号库 / JWT 解析 / 设备指纹 / 签到 / 本机发现
pub mod trae_account;
pub mod trae_checkin;
pub mod trae_device;
pub mod trae_discover;
pub mod travel;
pub mod update;
pub mod vscode_cn_inject;
pub mod yaml_lite;
