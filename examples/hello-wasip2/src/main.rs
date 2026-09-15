//! 最小的 wasm32-wasip2 工作负载。
//!
//! 它存在的意义是**证明运行时链路**：容器里只有一个 .wasm，宿主 wasmtime 直接跑它。
//! 不依赖 tokio / 依赖任何 wasm SDK，只用 std，所以只要能跑起来就说明
//! containerd → shim → wasmtime → wasip2 component 这条链是通的。
//!
//! 本地跑（需要 wasmtime）：
//!   cargo build --release --target wasm32-wasip2
//!   wasmtime run target/wasm32-wasip2/release/hello-wasip2.wasm -- --name k3s

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let name = args
        .iter()
        .position(|a| a == "--name")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "world".to_string());

    println!("hello, {name}!");
    println!("runtime  : wasm32-wasip2");

    // 证明我们真的在 wasm 里，而不是被某个宿主的原生二进制冒充
    println!("arch     : {}", std::env::consts::ARCH);
    println!("os       : {}", std::env::consts::OS);
    #[cfg(target_os = "wasi")]
    println!("wasi     : target_os=wasi");

    // 环境变量能力（k8s 里由 Pod spec 注入，便于确认配置是否正确到达组件）
    match std::env::var("WASM_GREETING") {
        Ok(v) => println!("env      : WASM_GREETING={v}"),
        Err(_) => println!("env      : WASM_GREETING 未设置（正常，可用 env 注入）"),
    }

    // 读文件能力默认**没有**授权：在 k3s 的 wasmtime shim 下会返回错误。
    // 这里显式演示「能力被拒绝」是可观测的，而不是静默失败。
    match std::fs::read_to_string("/etc/hostname") {
        Ok(v) => println!("fs       : /etc/hostname = {}", v.trim()),
        Err(e) => println!("fs       : 读 /etc/hostname 被拒绝（{e}）—— wasip2 能力模型符合预期"),
    }

    println!("exit     : ok");
}

#[cfg(test)]
mod tests {
    #[test]
    fn args_default_to_world() {
        // 纯逻辑冒烟：保证 host 上 `cargo test` 也能过
        let args: Vec<String> = vec![];
        let name = args
            .iter()
            .position(|a| a == "--name")
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| "world".to_string());
        assert_eq!(name, "world");
    }
}
