//! 真机探针：验证 `restart_gateway_serialized` 会**先等在途请求归零**再停网关。
//!
//! 用法：`drain_probe scenario <upstream_base>`
//!
//! # 为什么要一个探针而不是单测
//!
//! 这条修复的全部价值在**时序**上：停之前必须先把在途流等完。单测钉不住它
//!（要真的绑端口、起子进程、造一条持续数秒的 SSE）。所以这里跑真链路：
//!
//! 1. `start_gateway()`（宿主自己的启动路径 ⇒ `is_running()==true`）；
//! 2. 并发发起一条**慢**流式对话（假上游每 700ms 一帧）；
//! 3. 等 2s，读 `/status.in_flight`（应 ≥1）；
//! 4. 调 `restart_gateway_serialized()` 并计时 —— 若排空生效，耗时必然
//!    ≥ 该流剩余时长；若没生效，会立刻返回而客户端拿到截断；
//! 5. 观察那条流是否**完整结束**（`[DONE]`）还是被拦腰截断。
//!
//! 数据目录由 `AI_GATEWAY_HOME` 隔离（调用方设置）。探针不写账号数据。
use ai_gateway_core::modules::gateway;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "restart".into());
    match mode.as_str() {
        "scenario" => scenario().await,
        "start" => {
            let cfg = gateway::load_gateway_config();
            match gateway::start_gateway(&cfg).await {
                Ok(v) => println!("start ok {}", v.get("port").and_then(|p| p.as_u64()).unwrap_or(0)),
                Err(e) => println!("start failed: {e}"),
            }
            // 保持进程活着，否则 Job Object 关闭会连带杀掉子进程。
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
        "stop" => println!("stop -> {}", gateway::stop_gateway()),
        _ => {
            let t = Instant::now();
            let r = gateway::restart_gateway_serialized().await;
            println!("restart elapsed_ms={} ok={}", t.elapsed().as_millis(), r.is_ok());
        }
    }
}

async fn scenario() {
    let cfg = gateway::load_gateway_config();
    println!("[1] start_gateway ...");
    let v = gateway::start_gateway(&cfg).await.expect("网关应能启动");
    let port = v.get("port").and_then(|p| p.as_u64()).unwrap_or(7863) as u16;
    let key = cfg.get("api_key").and_then(|k| k.as_str()).unwrap_or("").to_string();
    println!("    就绪: port={port} is_running={}", gateway::is_running());

    // 2) 并发发起慢流
    let base = format!("http://127.0.0.1:{port}");
    let cli = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .expect("client");
    let streamer = {
        let base = base.clone();
        let key = key.clone();
        tokio::spawn(async move {
            let t0 = Instant::now();
            let body = serde_json::json!({
                "model": "deepseek-v4.1-flash",
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            });
            let mut req = cli
                .post(format!("{base}/v1/chat/completions"))
                .header("content-type", "application/json")
                .json(&body);
            if !key.is_empty() {
                req = req.header("authorization", format!("Bearer {key}"));
            }
            let mut resp = match req.send().await {
                Ok(r) => r,
                Err(e) => return (t0.elapsed().as_millis(), false, format!("send err {e}")),
            };
            let mut bytes = 0usize;
            let mut saw_done = false;
            let mut err: Option<String> = None;
            loop {
                match resp.chunk().await {
                    Ok(Some(c)) => {
                        bytes += c.len();
                        if c.windows(6).any(|w| w == b"[DONE]") {
                            saw_done = true;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        err = Some(format!("{e}"));
                        break;
                    }
                }
            }
            // 判据：拿到 [DONE] 才算这条流是**完整**的。
            (
                t0.elapsed().as_millis(),
                saw_done,
                err.unwrap_or_else(|| {
                    if saw_done {
                        "clean EOF after [DONE]".into()
                    } else {
                        "EOF WITHOUT [DONE]".into()
                    }
                }),
            )
        })
    };

    tokio::time::sleep(Duration::from_millis(2000)).await;
    let in_flight = probe_in_flight(port, &key).await;
    println!("[2] 流已发起 2s，/status.in_flight = {in_flight:?}");

    println!("[3] restart_gateway_serialized ...");
    let t = Instant::now();
    let r = gateway::restart_gateway_serialized().await;
    let restart_ms = t.elapsed().as_millis();
    println!("    restart elapsed_ms={restart_ms} ok={}", r.is_ok());

    let (stream_ms, saw_done, note) = streamer.await.expect("streamer task");
    println!("[4] 流：用时 {stream_ms} ms  saw_done={saw_done}  note={note}");
    println!(
        "[5] 判定：排空生效 = {}（restart 应在流结束后才返回）",
        restart_ms >= stream_ms.saturating_sub(3000)
    );
    gateway::stop_gateway();
}

async fn probe_in_flight(port: u16, key: &str) -> Option<usize> {
    let c = reqwest::Client::new();
    let mut req = c.get(format!("http://127.0.0.1:{port}/status"));
    if !key.is_empty() {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    let v: serde_json::Value = req.send().await.ok()?.json().await.ok()?;
    v.get("in_flight").and_then(|n| n.as_u64()).map(|n| n as usize)
}
