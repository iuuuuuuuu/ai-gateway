//! 构建脚本：把网关可执行文件压缩后内嵌，实现单文件分发。
//!
//! 查找顺序（任一命中即内嵌）：
//!   1. 环境变量 AI_GATEWAY_ROUTER_BIN
//!   2. crates/ai-gateway-core/embedded/gateway[.exe]（约定目录）
//!   3. 仓库根 dist/gateway[.exe]
//!
//! 都找不到时生成 `None`，程序仍可编译，只是不内嵌网关
//!（此时回退到用户自备 gateway 可执行文件的旧方式）。

use std::io::Write;
use std::path::{Path, PathBuf};

fn candidate_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(p) = std::env::var("AI_GATEWAY_ROUTER_BIN") {
        if !p.trim().is_empty() {
            out.push(PathBuf::from(p));
        }
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    // Windows 产物带 .exe 后缀；macOS/Linux 不带。与 scripts/build-gateway.sh 一致。
    let name = if cfg!(windows) { "gateway.exe" } else { "gateway" };
    out.push(manifest.join("embedded").join(name));
    // 仓库根 dist/（crates/ai-gateway-core → ../..）
    if let Some(root) = manifest.parent().and_then(Path::parent) {
        out.push(root.join("dist").join(name));
    }
    out
}

fn main() {
    println!("cargo:rerun-if-env-changed=AI_GATEWAY_ROUTER_BIN");
    // 无条件注册所有候选路径的监听：这样「先构建时没有网关、后来补上」
    // 也能触发 build.rs 重跑。只在命中路径上注册会导致永远内嵌不进去。
    for c in candidate_paths() {
        println!("cargo:rerun-if-changed={}", c.display());
    }
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let gen_file = out_dir.join("gateway_embed.rs");

    let mut chosen: Option<PathBuf> = None;
    for c in candidate_paths() {
        if c.is_file() {
            if let Ok(meta) = std::fs::metadata(&c) {
                if meta.len() > 1024 * 1024 {
                    chosen = Some(c);
                    break;
                }
            }
        }
    }

    let Some(path) = chosen else {
        std::fs::write(
            &gen_file,
            "// 未内嵌网关二进制（构建时未找到 gateway 可执行文件）\nNone::<&[u8]>",
        )
        .expect("写入 gateway_embed.rs 失败");
        println!("cargo:warning=未找到网关二进制，本次构建不内嵌（可设 AI_GATEWAY_ROUTER_BIN 指定）");
        return;
    };

    println!("cargo:rerun-if-changed={}", path.display());
    let raw = std::fs::read(&path).expect("读取网关二进制失败");

    // gzip 压缩：体积可降约 60%
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(&raw).expect("压缩网关失败");
    let gz = enc.finish().expect("压缩网关失败");

    // 关键：把压缩数据写成**独立的二进制文件**，再用 include_bytes! 引进来，
    // 而不是 format!("Some(&{:?})") 展开成一个 15.5 MB 的 Rust 数组字面量。
    //
    // 原因：`{:?}` 会把每个字节渲染成 "31, 139, 8, " 这样的十进制文本，产出
    // 一个 ~15.5 MB 的 .rs 文件，rustc 每次都得**逐字节解析这个字面量**。
    // 实测这一项占了 ai-gateway-core 重编时间的绝大部分：
    //   改 build.rs 触发重编 54.1s  vs  只改 src 重编 20.5s
    // 改成 include_bytes! 后同一路径实测降到 5.09s。
    // 而 CI 的 windows 关键路径上，核心单测恰好就是 68s，可见其占比。
    //
    // 生成文件仍是表达式形式（include! 需要表达式，不能是 const 项）。
    let bin_file = out_dir.join("gateway_embed.gz");
    std::fs::write(&bin_file, &gz).expect("写入 gateway_embed.gz 失败");

    std::fs::write(
        &gen_file,
        format!(
            "// 由 build.rs 生成：内嵌网关（原始 {} 字节 → 压缩 {} 字节）\nSome(include_bytes!({:?}))",
            raw.len(),
            gz.len(),
            bin_file
        ),
    )
    .expect("写入 gateway_embed.rs 失败");

    println!(
        "cargo:warning=已内嵌网关: {} ({} MB → {} MB)，仅需分发单个可执行文件",
        path.display(),
        raw.len() / 1048576,
        gz.len() / 1048576
    );
}

