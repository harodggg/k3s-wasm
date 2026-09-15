//! 构建脚本：保证前端 dist 目录存在。
//!
//! 前端 build 产物是**编译期**嵌进 wasm 的（见 assets.rs），所以 dist 必须在
//! `cargo build` 之前就存在。这里在缺失时放一个「显眼的占位页」而不是留空目录，
//! 免得后端编过了、部署上去却白屏，还以为是路由问题。
//!
//! 这样 `cargo check` / `cargo build` 在没有 Node 的环境里也能跑通。

use std::path::PathBuf;

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><title>前端未构建</title>
<style>body{font:16px/1.6 system-ui;margin:0;display:grid;place-items:center;height:100vh;background:#111;color:#eee}
code{background:#222;padding:2px 6px;border-radius:4px}</style></head>
<body><div>
<h1>前端资源未构建</h1>
<p>后端已经跑起来了，但 <code>ui/frontend/dist</code> 里没有内容。</p>
<p>请在 <code>ui/frontend</code> 下执行 <code>npm ci &amp;&amp; npm run build</code>，
然后重新 <code>cargo build --release --target wasm32-wasip2</code>。</p>
<p>或者直接用仓库根目录的 <code>make ui</code> 一次搞定。</p>
</div></body></html>
"#;

fn main() {
    let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("frontend")
        .join("dist");

    if !dist.join("index.html").exists() {
        if let Err(e) = std::fs::create_dir_all(&dist) {
            println!("cargo:warning=无法创建 {}: {e}", dist.display());
            return;
        }
        if let Err(e) = std::fs::write(dist.join("index.html"), PLACEHOLDER) {
            println!("cargo:warning=无法写入占位 index.html: {e}");
        } else {
            println!(
                "cargo:warning=前端 dist 不存在，已写入占位页。发布了这个构建的话，页面会提示「前端未构建」。"
            );
        }
    }

    // dist 变化时重新嵌入
    println!("cargo:rerun-if-changed=../frontend/dist");
    println!("cargo:rerun-if-changed=build.rs");
}
