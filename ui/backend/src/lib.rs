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
mod auth;
mod config;
mod http_io;
mod k8s;
mod topology;

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

/// 生产入口：读一次配置，交给 `route_inner`。
fn route(req: &Request) -> Response {
    let (cfg, k8s) = api::client();
    let auth_cfg = auth::AuthConfig::load(&cfg.default_namespace);
    route_inner(req, &auth_cfg, &k8s)
}

/// 路由表 + 鉴权门禁。
///
/// 拆成「不带环境依赖」的纯函数，是为了能在笔记本上直接 `cargo test`：
/// 测试传一个固定的 `AuthConfig` 与一个不会真的发请求的 `K8s`。
///
/// 门禁策略（失败关闭）：`/api/*` 里除了 `/api/health` 与 `/api/auth/*` 全都要求
/// 有效会话；会话密钥没配好时返回 **503 并说明要配什么**，而不是放行 ——
/// 这个控制台手里是 kube-api-proxy 的权限，配置不全时放行等于把集群交出去。
fn route_inner(req: &Request, auth_cfg: &auth::AuthConfig, k8s: &k8s::K8s) -> Response {
    if auth::requires_auth(&req.path) {
        match auth::authenticated(req, auth_cfg) {
            Ok(true) => {}
            Ok(false) => {
                return Response::fail(401, "未登录：请先用 Touch ID / passkey 登录（见 /api/auth/status）")
            }
            Err(reason) => return Response::fail(503, reason),
        }
    }

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

        // ── 免密登录（WebAuthn / passkey）───────────────────────────
        // 这几条必须在门禁的放行名单里（否则没法登录）—— 见 auth::requires_auth
        ("GET", ["api", "auth", "status"]) => auth::status(req, auth_cfg, k8s),
        ("POST", ["api", "auth", "register", "begin"]) => auth::register_begin(req, auth_cfg, k8s),
        ("POST", ["api", "auth", "register", "finish"]) => auth::register_finish(req, auth_cfg, k8s),
        ("POST", ["api", "auth", "login", "begin"]) => auth::login_begin(req, auth_cfg, k8s),
        ("POST", ["api", "auth", "login", "finish"]) => auth::login_finish(req, auth_cfg, k8s),
        ("POST", ["api", "auth", "logout"]) => auth::logout(),

        // ── 集群视图 ───────────────────────────────────────────────
        ("GET", ["api", "nodes"]) => api::nodes(req),
        ("GET", ["api", "runtimes"]) => api::runtimes(req),
        ("GET", ["api", "namespaces"]) => api::namespaces(req),
        ("GET", ["api", "workloads"]) => api::workloads(req),
        ("GET", ["api", "pods"]) => api::pods(req),
        ("GET", ["api", "pods", ns, name, "logs"]) => api::pod_logs(req, ns, name),
        ("GET", ["api", "events"]) => api::events(req),
        ("GET", ["api", "images"]) => api::images(req),
        ("GET", ["api", "image-tags"]) => api::image_tags(req),
        // 网络拓扑：一次请求拉回整张图（节点 + 边），前端每 5 秒刷新
        ("GET", ["api", "topology"]) => topology::topology(req),

        // ── SpinKube ───────────────────────────────────────────────
        ("GET", ["api", "spinapps"]) => api::spinapps_list(req),
        ("GET", ["api", "spinapp-executors"]) => api::spinapp_executors(req),
        ("POST", ["api", "spinapps"]) => api::spinapp_create(req),
        ("POST", ["api", "spinapps", ns, name, "scale"]) => api::spinapp_scale(req, ns, name),
        ("DELETE", ["api", "spinapps", ns, name]) => api::spinapp_delete(req, ns, name),

        // ── xray-wasm 隧道 ─────────────────────────────────────────
        ("GET", ["api", "xray", "tunnels"]) => api::xray_list(req),
        ("POST", ["api", "xray", "tunnels"]) => api::xray_create(req),
        // 自动生成一整套隧道参数（密钥对/UUID/shortId/SOCKS 凭据 + 服务端与客户端配置）
        ("POST", ["api", "xray", "generate"]) => api::xray_generate(req),
        ("POST", ["api", "xray", "tunnels", ns, name, "scale"]) => {
            api::xray_scale(req, ns, name)
        }
        ("GET", ["api", "xray", "tunnels", ns, name, "vless"]) => api::xray_vless(req, ns, name),
        ("DELETE", ["api", "xray", "tunnels", ns, name]) => api::xray_delete(req, ns, name),

        // 其它 /api/* 一律 JSON 404（不要落到静态资源，否则前端会拿到 HTML）
        (_, ["api", ..]) => Response::fail(
            404,
            format!("没有这个接口：{} {}", req.method, req.path),
        ),

        // ── 前端静态资源 ───────────────────────────────────────────
        // 登录页本身也是静态资源，所以静态资源不能要求鉴权；数据由门禁挡。
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
            headers: vec![("host".into(), "console.example.test".into())],
            body: vec![],
        }
    }

    fn test_auth() -> auth::AuthConfig {
        auth::AuthConfig {
            rp_id: "console.example.test".into(),
            origin: "https://console.example.test".into(),
            session_secret: b"0123456789abcdef0123456789abcdef".to_vec(),
            registration_code: "code".into(),
            secret_name: "k3s-wasm-ui-auth".into(),
            namespace: "k3s-wasm".into(),
        }
    }

    /// 不会真的发请求的 K8s 客户端（指向 127.0.0.1:1），只用于路由测试。
    fn dummy_k8s() -> k8s::K8s {
        k8s::K8s::new("http://127.0.0.1:1")
    }

    #[test]
    fn unknown_api_route_is_json_404() {
        let resp = route_inner(&req("GET", "/api/does-not-exist"), &test_auth(), &dummy_k8s());
        // 未登录时先被门禁挡住 —— 这也是对的：连「有哪些接口」都不该泄露给匿名访问
        assert_eq!(resp.status, 401);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["ok"], false);
    }

    #[test]
    fn api_without_session_is_401_and_health_without_session_is_200() {
        let a = test_auth();
        let k = dummy_k8s();
        assert_eq!(route_inner(&req("GET", "/api/nodes"), &a, &k).status, 401);
        assert_eq!(route_inner(&req("GET", "/api/pods"), &a, &k).status, 401);
        assert_eq!(route_inner(&req("POST", "/api/xray/tunnels"), &a, &k).status, 401);
        // 健康检查必须匿名可用（探针/排障），且要能看出认证配置状态
        let health = route_inner(&req("GET", "/api/health"), &a, &k);
        assert_eq!(health.status, 200);
        let v: Value = serde_json::from_slice(&health.body).unwrap();
        assert_eq!(v["data"]["auth"]["mode"], "webauthn-passkey");
        assert_eq!(v["data"]["auth"]["rpId"], "console.example.test");
    }

    #[test]
    fn auth_endpoints_are_reachable_without_session() {
        // 匿名也必须能打到认证接口，否则根本没法登录。
        // 这里用 logout 来证明路由没被门禁挡住：它是纯函数，不像 status/login
        // 那样要访问 k8s（原生单测里没有 wasi:http，会 panic）。
        let resp = route_inner(&req("POST", "/api/auth/logout"), &test_auth(), &dummy_k8s());
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["data"]["authenticated"], false);
        // 会话 cookie 与挑战 cookie 都要清掉（两个 Set-Cookie 头）
        let cookies = resp
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .count();
        assert_eq!(cookies, 2);

        // 会话密钥没配好时，认证接口要明确 503（而不是静默放行）
        let mut a = test_auth();
        a.session_secret = vec![];
        assert_eq!(
            route_inner(&req("POST", "/api/auth/login/begin"), &a, &dummy_k8s()).status,
            503
        );
    }

    #[test]
    fn unconfigured_session_secret_fails_closed_with_503() {
        let mut a = test_auth();
        a.session_secret = vec![];
        let resp = route_inner(&req("GET", "/api/nodes"), &a, &dummy_k8s());
        assert_eq!(resp.status, 503);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("K3S_WASM_SESSION_SECRET"),
            "错误信息必须告诉运维该配什么：{}",
            v["error"]["message"]
        );
    }

    #[test]
    fn wrong_method_on_known_api_is_not_static() {
        // 未登录时不泄露路由细节；登录后才是 404 JSON
        let resp = route_inner(&req("POST", "/api/nodes"), &test_auth(), &dummy_k8s());
        assert_eq!(resp.status, 401);
        assert!(serde_json::from_slice::<Value>(&resp.body).is_ok());
    }

    #[test]
    fn root_serves_spa_shell() {
        let resp = route_inner(&req("GET", "/"), &test_auth(), &dummy_k8s());
        assert_eq!(resp.status, 200);
        assert!(String::from_utf8_lossy(&resp.body).contains("<html"));
    }

    #[test]
    fn deep_link_falls_back_to_spa_shell() {
        let resp = route_inner(&req("GET", "/tunnels/tokyo"), &test_auth(), &dummy_k8s());
        assert_eq!(resp.status, 200);
    }
}

