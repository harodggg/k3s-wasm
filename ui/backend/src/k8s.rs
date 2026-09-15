//! Kubernetes API 客户端。
//!
//! 设计取舍（重要）：
//!
//! 1. **只讲明文 HTTP**，目标是集群内的 kube-api-proxy（kubectl proxy）。
//!    TLS、证书、ServiceAccount token 全部留在组件之外。原因是组件的出站 TLS 由宿主
//!    完成，而宿主（wasmtime shim）的信任根我们配置不了，面对 k3s 的集群自签 CA
//!    会直接握手失败。详见 deploy/base/kube-api-proxy.yaml。
//!
//! 2. **不引入 k8s-openapi / kube-rs**：它们会带进 tokio/hyper 和一整套版本化类型，
//!    编到 wasm 里体积和编译时间都不可接受。这里只读几个列表字段、打几个 patch，
//!    所以直接用 `serde_json::Value`，形态在 api.rs 里收窄成前端需要的形状。
//!
//! 3. **阻塞式调用，不用 async**。wasmtime shim 是「一个请求一个实例」，
//!    组件内阻塞没有任何副作用，却省掉了 executor 与 Send/Sync 的一堆约束
//!    （xray-wasm 那边也是同一个结论）。实现上就是
//!    `outgoing_handler::handle(...)` → `subscribe().block()`。
//!
//! 4. **错误原样上抛**：k8s 的 Status.body 里带着 RBAC 拒绝的具体原因，
//!    丢掉它换成「请求失败」会让排障非常痛苦。

use std::io::Write as _;

use serde_json::Value;
use wasi::http::outgoing_handler;
use wasi::http::types::{
    Fields, IncomingBody, IncomingResponse, Method, OutgoingBody, OutgoingRequest, Scheme,
};

use crate::config;
use crate::http_io;

#[derive(Debug)]
pub enum ApiError {
    /// 上游返回了 >= 400
    Upstream {
        status: u16,
        reason: String,
        message: String,
    },
    /// 连不上 / 宿主不提供 outgoing-handler / 契约不符
    Transport(String),
    /// 返回的 body 不是合法 JSON
    Decode(String),
}

impl ApiError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, ApiError::Upstream { status: 404, .. })
    }

    pub fn is_forbidden(&self) -> bool {
        matches!(self, ApiError::Upstream { status: 403, .. })
    }

    /// 给运维看的一句话，尽量把上游的原因带出来。
    pub fn message(&self) -> String {
        match self {
            ApiError::Upstream {
                status,
                reason,
                message,
            } => {
                if reason.is_empty() {
                    format!("Kubernetes API 返回 {status}：{message}")
                } else {
                    format!("Kubernetes API 返回 {status}（{reason}）：{message}")
                }
            }
            ApiError::Transport(m) => format!("访问 kube-api-proxy 失败：{m}"),
            ApiError::Decode(m) => format!("解析 Kubernetes 返回失败：{m}"),
        }
    }

    /// 映射成给前端的 HTTP 状态码。上游 4xx 原样透传，其余算 502。
    pub fn http_status(&self) -> u16 {
        match self {
            ApiError::Upstream { status, .. } if (400..500).contains(status) => *status,
            ApiError::Upstream { .. } => 502,
            _ => 502,
        }
    }
}

/// 上游响应的原始形态。
pub struct Upstream {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Upstream {
    /// 上游 >=400 就变成 ApiError，否则解析 JSON。
    ///
    /// ⚠️ 这个方法名很短，但它承担了「状态码检查」这个关键职责。
    /// 重构时如果让 post/patch/delete 直接解析 body 而不检查状态码，
    /// 4xx 会被当成成功 —— 表现为「创建失败却提示成功」，非常隐蔽。
    fn json(self) -> Result<Value, ApiError> {
        let ok = self.ensure_ok()?;
        if ok.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&ok.body).map_err(|e| ApiError::Decode(e.to_string()))
    }

    fn text(self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    /// >=400 就转成 ApiError，尽量解析出 k8s 的 Status 字段。
    fn ensure_ok(self) -> Result<Upstream, ApiError> {
        if self.status < 400 {
            return Ok(self);
        }
        let parsed: Option<Value> = serde_json::from_slice(&self.body).ok();
        let (reason, message) = match &parsed {
            Some(v) => (
                v["reason"].as_str().unwrap_or_default().to_string(),
                v["message"].as_str().unwrap_or_default().to_string(),
            ),
            None => (
                String::new(),
                String::from_utf8_lossy(&self.body)
                    .chars()
                    .take(300)
                    .collect::<String>(),
            ),
        };
        Err(ApiError::Upstream {
            status: self.status,
            reason,
            message,
        })
    }
}

#[derive(Clone)]
pub struct K8s {
    base: String,
}

impl K8s {
    pub fn new(base: &str) -> Self {
        Self {
            base: config::normalize_base(base),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn get(&self, path_and_query: &str) -> Result<Value, ApiError> {
        self.send(Method::Get, path_and_query, &[], None)?.json()
    }

    /// 日志接口返回 text/plain，不能当 JSON 解。
    pub fn get_text(&self, path_and_query: &str) -> Result<String, ApiError> {
        Ok(self
            .send(Method::Get, path_and_query, &[], None)?
            .ensure_ok()?
            .text())
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        let payload = serde_json::to_string(body)
            .map_err(|e| ApiError::Decode(format!("序列化请求体失败：{e}")))?;
        self.send(
            Method::Post,
            path,
            &[("content-type".into(), "application/json".into())],
            Some(payload),
        )?
        .json()
    }

    /// 合并式 patch。用 `spec.replicas` 做扩缩容，对 Deployment 与 SpinApp 都适用，
    /// 于是不必依赖 scale 子资源。
    pub fn merge_patch(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        let payload = serde_json::to_string(body)
            .map_err(|e| ApiError::Decode(format!("序列化请求体失败：{e}")))?;
        self.send(
            Method::Patch,
            path,
            &[("content-type".into(), "application/merge-patch+json".into())],
            Some(payload),
        )?
        .json()
    }

    pub fn delete(&self, path: &str) -> Result<Value, ApiError> {
        self.send(Method::Delete, path, &[], None)?.json()
    }

    /// 真正发出请求的地方。所有方法都走这里，于是错误处理只有一处。
    fn send(
        &self,
        method: Method,
        path_and_query: &str,
        headers: &[(String, String)],
        body: Option<String>,
    ) -> Result<Upstream, ApiError> {
        let (scheme, authority) = split_base(&self.base)?;
        send_request(scheme, &authority, method, path_and_query, headers, body)
    }
}

/// 向**集群外**的绝对 URL 发一个 GET（例如 GitHub API）。
///
/// 存在的意义：镜像版本下拉需要"现在有哪些 tag"。CRI 不暴露镜像列表、
/// k8s API 里也没有，而 GitHub releases 是 tag 的权威来源 ——
/// 前提是宿主允许 wasm 组件做出站 HTTPS（本仓库在真机上验证过这一点）。
pub fn fetch_absolute(url: &str) -> Result<Upstream, ApiError> {
    let (scheme, rest) = if let Some(r) = url.strip_prefix("https://") {
        (Scheme::Https, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (Scheme::Http, r)
    } else {
        return Err(ApiError::Transport(format!("只支持 http/https：{url}")));
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a.to_string(), format!("/{p}")),
        None => (rest.to_string(), "/".to_string()),
    };
    if authority.is_empty() {
        return Err(ApiError::Transport(format!("URL 里没有主机名：{url}")));
    }
    // ⚠️ 必须带 User-Agent：GitHub API 对没有 UA 的请求直接返回 403。
    // 实测：节点上 curl（自带 UA）→ 200；组件不带 UA → 403，很容易误判成"限流"或"HTTPS 不通"。
    let headers = vec![
        ("user-agent".to_string(), "k3s-wasm-ui/0.1".to_string()),
        ("accept".to_string(), "application/vnd.github+json".to_string()),
    ];
    send_request(scheme, &authority, Method::Get, &path, &headers, None)
}

fn send_request(
    scheme: Scheme,
    authority: &str,
    method: Method,
    path_and_query: &str,
    headers: &[(String, String)],
    body: Option<String>,
) -> Result<Upstream, ApiError> {
    {

        // ⚠️ wasi-http 的坑：OutgoingRequest 的 header 必须在**构造时**通过 Fields 传入。
        // 构造之后再拿 req.headers() 去 set 是拿不到的 —— 那是一个只读视图，
        // set 会返回 HeaderError（表现为「设置请求头 content-type 失败」，
        // 而 GET 没有自定义头所以看起来一切正常，很容易误判为代理的问题）。
        let entries: Vec<(String, Vec<u8>)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.as_bytes().to_vec()))
            .collect();
        let fields = if entries.is_empty() {
            Fields::new()
        } else {
            Fields::from_list(&entries)
                .map_err(|_| ApiError::Transport("构造请求头 Fields 失败".into()))?
        };

        let req = OutgoingRequest::new(fields);
        req.set_method(&method)
            .map_err(|_| ApiError::Transport("set_method 失败".into()))?;
        req.set_scheme(Some(&scheme))
            .map_err(|_| ApiError::Transport("set_scheme 失败".into()))?;
        req.set_authority(Some(authority))
            .map_err(|_| ApiError::Transport("set_authority 失败".into()))?;
        req.set_path_with_query(Some(path_and_query))
            .map_err(|_| ApiError::Transport("set_path_with_query 失败".into()))?;

        // ⚠️ 这里有个反直觉但必须遵守的顺序（实测踩过）：
        //   1. 先把 body 资源从 request 上取下来（take）
        //   2. 调 handle() 把请求**发出去**
        //   3. 再往 body 里写数据并 finish
        //   4. 最后阻塞等响应
        // 如果按直觉写成「先写完 body、再 handle」，Spin 宿主会把 body 当空处理 ——
        // 现象是 POST 全部变成空 body（我们的 mock 会因此回 422，
        // 真实 k8s 会回 "resource name may not be empty" 之类，很难联想到顺序问题）。
        // spin-sdk 自己的 send() 也是这个顺序：先 outgoing_request_send，再用 body sink 喂数据。
        let out_body = req
            .body()
            .map_err(|_| ApiError::Transport("无法获取请求 body 资源".into()))?;

        let fut = outgoing_handler::handle(req, None).map_err(|_| {
            ApiError::Transport(
                "outgoing_handler::handle 失败：宿主没有提供 wasi:http/outgoing-handler？\
                 （wasmtime serve 需要 -S http=y；Spin 需要 allowed_outbound_hosts 放行该地址）"
                    .into(),
            )
        })?;

        if let Some(payload) = body.as_deref() {
            if let Ok(mut stream) = out_body.write() {
                let _ = stream.write_all(payload.as_bytes());
                let _ = stream.flush();
                drop(stream);
            }
        }
        let _ = OutgoingBody::finish(out_body, None);

        // 单线程 wasm 里直接阻塞等待，没有 async runtime 的开销
        fut.subscribe().block();

        let resp: IncomingResponse = match fut.get() {
            Some(Ok(Ok(resp))) => resp,
            Some(Ok(Err(code))) => {
                return Err(ApiError::Transport(format!("上游返回错误码：{code:?}")))
            }
            Some(Err(())) => return Err(ApiError::Transport("请求被宿主取消".into())),
            None => return Err(ApiError::Transport("响应尚未就绪".into())),
        };

        let status = resp.status();
        // headers 是 resp 的子资源，必须在 resp 之前 drop：先取完再丢
        let body_bytes = match resp.consume() {
            Ok(b) => drain(b),
            Err(()) => Vec::new(),
        };

        Ok(Upstream {
            status,
            body: body_bytes,
        })
    }
}

fn drain(body: IncomingBody) -> Vec<u8> {
    // 复用 http_io 里的实现，保证 drop/finish 顺序一致
    http_io::drain_body(body)
}

/// `http://127.0.0.1:8001/` → `(Scheme::Http, "127.0.0.1:8001")`
pub fn split_base(base: &str) -> Result<(Scheme, String), ApiError> {
    let (scheme, rest) = if let Some(r) = base.strip_prefix("http://") {
        (Scheme::Http, r)
    } else if let Some(r) = base.strip_prefix("https://") {
        (Scheme::Https, r)
    } else {
        return Err(ApiError::Transport(format!(
            "K8S_PROXY_URL 必须以 http:// 或 https:// 开头，收到：{base}"
        )));
    };
    let authority = rest.split('/').next().unwrap_or("").to_string();
    if authority.is_empty() {
        return Err(ApiError::Transport(format!("URL 里没有主机名：{base}")));
    }
    Ok((scheme, authority))
}

/// 查询参数值的最小百分号编码。
pub fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_query_escapes_selector_punctuation() {
        assert_eq!(
            encode_query("app.kubernetes.io/part-of=xray-wasm"),
            "app.kubernetes.io%2Fpart-of%3Dxray-wasm"
        );
    }

    #[test]
    fn base_trailing_slash_is_normalized() {
        let c = K8s::new("http://127.0.0.1:8001/");
        assert_eq!(c.base(), "http://127.0.0.1:8001");
    }

    #[test]
    fn split_base_parses_scheme_and_authority() {
        let (s, a) = split_base("http://127.0.0.1:8001").unwrap();
        // Scheme 没有实现 PartialEq（wit-bindgen 生成类型），用 matches! 判断
        assert!(matches!(s, Scheme::Http));
        assert_eq!(a, "127.0.0.1:8001");

        let (s, a) = split_base("https://kubernetes.default.svc/").unwrap();
        assert!(matches!(s, Scheme::Https));
        assert_eq!(a, "kubernetes.default.svc");

        assert!(split_base("ftp://x").is_err());
        assert!(split_base("http://").is_err());
    }

    #[test]
    fn forbidden_is_flagged() {
        let e = ApiError::Upstream {
            status: 403,
            reason: "Forbidden".into(),
            message: "pods is forbidden".into(),
        };
        assert!(e.is_forbidden());
        assert_eq!(e.http_status(), 403);
        assert!(e.message().contains("pods is forbidden"));
    }
}
