//! k3s WASM 控制台 —— 后端。
//!
//! 这是一个 **wasm32-wasip2** 组件，导出 `wasi:http/incoming-handler@0.2.12`，
//! 在 k3s 上由 runwasi 的 `containerd-shim-wasmtime` 承载
//! （RuntimeClass `wasmtime-wasip2`，handler `wasmtime`，监听 8080）。
//!
//! 之所以不是 Spin 应用：本组件没有任何 `spin:*` 导入，是标准的
//! `wasi:http/proxy` 组件。而 spin shim 加载的是 Spin 应用（LockedApp）并驱动
//! Spin 自己的触发器，官方并未文档化它能承载「非 Spin 的裸 proxy 组件」。
//! 两条路的关系见 docs/01-runtime.md。
//!
//! 整体刻意保持**同步**：wasmtime shim 是「一个请求一个实例」，
//! 组件内阻塞没有副作用，却省掉整个 async 运行时与 Send/Sync 约束。

mod api;
mod assets;
mod config;
mod http_io;
mod k8s;

use http_io::{Request, Response};
use wasi::exports::http::incoming_handler::Guest;
use wasi::http::types::{IncomingRequest, ResponseOutparam};

struct Component;

impl Guest for Component {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let req = http_io::read_request(&request);
        let resp = route(&req);
        http_io::write_response(response_out, resp);
    }
}

// 生成导出 wasi:http/incoming-handler 所需的 #[no_mangle] 符号
wasi::http::proxy::export!(Component);

/// 路由表。
///
/// 手写而不是用库：路由总共十几条、模式固定，而且这样它是**纯函数**，
/// 可以在 host 上直接 `cargo test`（见文件末的测试）。
fn route(req: &Request) -> Response {
    let segs: Vec<&str> = req
        .path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    match (req.method.as_str(), segs.as_slice()) {
        // ── 自身状态 ───────────────────────────────────────────────
        ("GET", ["api", "health"]) => api::health(req),
        ("GET", ["api", "summary"]) => api::summary(req),

        // ── 集群视图 ───────────────────────────────────────────────
        ("GET", ["api", "nodes"]) => api::nodes(req),
        ("GET", ["api", "runtimes"]) => api::runtimes(req),
        ("GET", ["api", "namespaces"]) => api::namespaces(req),
        ("GET", ["api", "workloads"]) => api::workloads(req),
        ("GET", ["api", "pods"]) => api::pods(req),
        ("GET", ["api", "pods", ns, name, "logs"]) => api::pod_logs(req, ns, name),
        ("GET", ["api", "events"]) => api::events(req),

        // ── SpinKube ───────────────────────────────────────────────
        ("GET", ["api", "spinapps"]) => api::spinapps_list(req),
        ("GET", ["api", "spinapp-executors"]) => api::spinapp_executors(req),
        ("POST", ["api", "spinapps"]) => api::spinapp_create(req),
        ("POST", ["api", "spinapps", ns, name, "scale"]) => api::spinapp_scale(req, ns, name),
        ("DELETE", ["api", "spinapps", ns, name]) => api::spinapp_delete(req, ns, name),

        // ── xray-wasm 隧道 ─────────────────────────────────────────
        ("GET", ["api", "xray", "tunnels"]) => api::xray_list(req),
        ("POST", ["api", "xray", "tunnels"]) => api::xray_create(req),
        ("POST", ["api", "xray", "tunnels", ns, name, "scale"]) => {
            api::xray_scale(req, ns, name)
        }
        ("DELETE", ["api", "xray", "tunnels", ns, name]) => api::xray_delete(req, ns, name),

        // 其它 /api/* 一律 JSON 404（不要落到静态资源，否则前端会拿到 HTML）
        (_, ["api", ..]) => Response::fail(
            404,
            format!("没有这个接口：{} {}", req.method, req.path),
        ),

        // ── 前端静态资源 ───────────────────────────────────────────
        _ => assets::serve(req),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn req(method: &str, path: &str) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            query: String::new(),
            headers: vec![],
            body: vec![],
        }
    }

    #[test]
    fn unknown_api_route_is_json_404() {
        let resp = route(&req("GET", "/api/does-not-exist"));
        assert_eq!(resp.status, 404);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["ok"], false);
    }

    #[test]
    fn wrong_method_on_known_api_is_404_not_static() {
        // POST 到只读接口：必须是 JSON，而不是 index.html
        let resp = route(&req("POST", "/api/nodes"));
        assert_eq!(resp.status, 404);
        assert!(serde_json::from_slice::<Value>(&resp.body).is_ok());
    }

    #[test]
    fn root_serves_spa_shell() {
        let resp = route(&req("GET", "/"));
        assert_eq!(resp.status, 200);
        assert!(String::from_utf8_lossy(&resp.body).contains("<html"));
    }

    #[test]
    fn deep_link_falls_back_to_spa_shell() {
        let resp = route(&req("GET", "/tunnels/tokyo"));
        assert_eq!(resp.status, 200);
    }
}
