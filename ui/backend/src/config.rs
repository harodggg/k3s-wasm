//! 运行时配置。
//!
//! 只有一层来源：环境变量 → 编译期默认值。
//! 默认值刻意指向集群内的 kube-api-proxy，所以**标准部署不需要任何配置**。

/// 集群内 kubectl proxy 的地址（见 deploy/base/kube-api-proxy.yaml，ClusterIP 被固定为 10.43.0.53）。
///
/// 支持在**构建时**用 `K3S_WASM_PROXY_URL` 烘焙另一个地址（`option_env!` 是编译期求值，
/// 不是运行时读环境变量 —— 否则组件会多出一个 wasi:cli/environment 依赖，
/// 而我们希望运行时配置尽量走容器 env）。
pub const DEFAULT_PROXY_URL: &str = match option_env!("K3S_WASM_PROXY_URL") {
    Some(v) => v,
    // ⚠️ 用 IP 而不是 DNS 名：wasmtime shim 不提供 wasi:sockets/ip-name-lookup，
    // 组件用域名访问会**永久挂住**（无报错，只是超时），IP 直连则正常。
    // 这个 IP 与 deploy/base/kube-api-proxy.yaml 里固定的 ClusterIP 对应。
    None => "http://10.43.0.53:8001",
};

/// 控制台默认操作的命名空间。
pub const DEFAULT_NAMESPACE: &str = "k3s-wasm";

/// xray 隧道工作负载的归属标签，UI 靠它在集群里认领自己创建的资源。
pub const XRAY_PART_OF: &str = "xray-wasm";
pub const MANAGED_BY: &str = "k3s-wasm-ui";

/// SpinApp 默认使用的 executor 名字。
/// 注意：SpinApp 的 runtimeClassName 来自 executor（SpinAppExecutor），
/// 不是 SpinApp 自己的字段 —— 这点很容易写错。
pub const DEFAULT_SPINAPP_EXECUTOR: &str = "containerd-shim-spin";

/// 配置值的来源，会通过 /api/health 暴露 —— 配置没生效时一眼能看出用的是哪一层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    Builtin,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::Builtin => "builtin-default",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub proxy_url: String,
    pub proxy_url_source: Source,
    pub default_namespace: String,
}

impl Config {
    fn resolve(env_key: &str, default: &str) -> (String, Source) {
        if let Ok(v) = std::env::var(env_key) {
            if !v.trim().is_empty() {
                return (v.trim().to_string(), Source::Env);
            }
        }
        (default.to_string(), Source::Builtin)
    }

    pub fn load() -> Self {
        let (proxy_url, proxy_url_source) = Self::resolve("K8S_PROXY_URL", DEFAULT_PROXY_URL);
        let (default_namespace, _) = Self::resolve("DEFAULT_NAMESPACE", DEFAULT_NAMESPACE);
        Self {
            proxy_url,
            proxy_url_source,
            default_namespace,
        }
    }
}

/// 去掉 URL 结尾的 `/`，避免拼出 `//api/v1/...`。
pub fn normalize_base(base: &str) -> String {
    base.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_base_strips_trailing_slash() {
        assert_eq!(normalize_base("http://x:8001/"), "http://x:8001");
        assert_eq!(normalize_base("http://x:8001"), "http://x:8001");
    }

    #[test]
    fn default_proxy_url_is_an_ip_literal() {
        // 这条断言守的是一个实测约束：wasmtime shim 不向 wasm guest 提供域名解析，
        // 用 DNS 名的出站请求会永久挂住（无报错、只是超时）。所以默认地址必须是 IP。
        assert!(DEFAULT_PROXY_URL.starts_with("http://"), "必须是明文 HTTP：TLS 由 kube-api-proxy 承担");
        let host = DEFAULT_PROXY_URL
            .trim_start_matches("http://")
            .split(':')
            .next()
            .unwrap_or("");
        assert!(
            host.parse::<std::net::Ipv4Addr>().is_ok(),
            "默认代理地址必须是 IPv4 字面量（不能用 DNS 名），实际：{host}"
        );
    }

    #[test]
    fn unset_env_falls_back_to_builtin() {
        let (v, s) = Config::resolve("K3S_WASM_TEST_UNSET_KEY_XYZ", "fallback");
        assert_eq!(v, "fallback");
        assert_eq!(s, Source::Builtin);
    }
}
