//! 把 `wasi:http` 的资源类型「拍平」成普通 Rust 结构。
//!
//! 这么做的目的很实际：
//!   1. wasi 的 IncomingRequest / OutgoingResponse 是**资源**，借用规则严格
//!      （headers 是 body 的子资源，必须按顺序 drop）。业务代码直接操作它们会
//!      变成一片 unsafe 风格的生命周期纠缠。
//!   2. 拍平之后，路由与 JSON 处理都是纯函数，可以在笔记本上 `cargo test`
//!      直接跑，不需要 wasm 宿主 —— 这也是本文件里的解析函数都带单测的原因。

use std::io::{Read as _, Write as _};

use wasi::http::types::{
    ErrorCode, Fields, IncomingBody, IncomingRequest, Method, OutgoingBody, OutgoingResponse,
    ResponseOutparam,
};

/// 已读进内存的请求。
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// 读 query 里的一个参数，做了百分号解码。
    pub fn query_param(&self, key: &str) -> Option<String> {
        self.query.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            if k == key {
                Some(percent_decode(v))
            } else {
                None
            }
        })
    }

    /// body 当 JSON 解析。
    pub fn json_body(&self) -> Result<serde_json::Value, String> {
        if self.body.is_empty() {
            return Err("请求体为空，需要 JSON".to_string());
        }
        serde_json::from_slice(&self.body).map_err(|e| format!("请求体不是合法 JSON：{e}"))
    }
}

/// 待写出的响应。
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// 统一信封的成功响应。
    pub fn ok(data: serde_json::Value) -> Self {
        Self::json(200, &serde_json::json!({ "ok": true, "data": data }))
    }

    /// 统一信封的失败响应。
    pub fn fail(status: u16, message: impl Into<String>) -> Self {
        Self::json(
            status,
            &serde_json::json!({
                "ok": false,
                "error": { "message": message.into(), "status": status }
            }),
        )
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Self::new(status, value.to_string())
            .with_header("content-type", "application/json; charset=utf-8")
            .with_header("cache-control", "no-store")
    }
}

// ════════════════════════════════════════════════════════════════════
// wasi → 普通结构
// ════════════════════════════════════════════════════════════════════

fn method_to_string(m: &Method) -> String {
    match m {
        Method::Get => "GET".into(),
        Method::Head => "HEAD".into(),
        Method::Post => "POST".into(),
        Method::Put => "PUT".into(),
        Method::Delete => "DELETE".into(),
        Method::Connect => "CONNECT".into(),
        Method::Options => "OPTIONS".into(),
        Method::Trace => "TRACE".into(),
        Method::Patch => "PATCH".into(),
        Method::Other(s) => s.clone(),
    }
}

fn read_fields(fields: &Fields) -> Vec<(String, String)> {
    fields
        .entries()
        .into_iter()
        .map(|(k, v)| (k, String::from_utf8_lossy(&v).to_string()))
        .collect()
}

/// 读干一个 wasi body 资源，并按规范 finish 掉它。
pub fn drain_body(body: IncomingBody) -> Vec<u8> {
    let mut buf = Vec::new();
    // stream 是 body 的子资源，必须在 finish 之前 drop —— 用块作用域保证顺序
    {
        if let Ok(stream) = body.stream() {
            let mut stream = stream;
            let _ = stream.read_to_end(&mut buf);
        }
    }
    let _ = IncomingBody::finish(body);
    buf
}

pub fn read_request(req: &IncomingRequest) -> Request {
    let method = method_to_string(&req.method());
    let raw = req.path_with_query().unwrap_or_else(|| "/".to_string());
    let (path, query) = split_path_query(&raw);
    // ⚠️ wasi:http 把 Host 放在 request 的 **authority** 上，不保证出现在 headers 里
    //    （wasmtime 就不放）。而组件里好几处要靠 Host 推导 RP ID / origin / 对外地址，
    //    少了它就会莫名其妙失败 —— 实测症状是 WebAuthn 注册接口 400「请求没有 Host 头」。
    //    这里补一条 host 头，上层就不用关心它原本来自哪里。
    let headers = ensure_host_header(read_fields(&req.headers()), req.authority());
    let body = match req.consume() {
        Ok(b) => drain_body(b),
        Err(()) => Vec::new(),
    };
    Request {
        method,
        path,
        query,
        headers,
        body,
    }
}

/// 把 `authority`（wasi:http 里承载 Host 的地方）合并进头列表：
/// 已经有 `host`（大小写不敏感）就不动，否则补一条。
pub fn ensure_host_header(
    mut headers: Vec<(String, String)>,
    authority: Option<String>,
) -> Vec<(String, String)> {
    let has_host = headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("host"));
    if !has_host {
        if let Some(a) = authority.filter(|a| !a.trim().is_empty()) {
            headers.push(("host".to_string(), a));
        }
    }
    headers
}

/// 把响应写回宿主。顺序必须严格按 wasi-http 的要求：
/// 先 set ResponseOutparam，再写 body，最后 finish。
pub fn write_response(response_out: ResponseOutparam, resp: Response) {
    let entries: Vec<(String, Vec<u8>)> = resp
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.as_bytes().to_vec()))
        .collect();
    let fields = Fields::from_list(&entries).unwrap_or_else(|_| Fields::new());
    let out = OutgoingResponse::new(fields);
    let _ = out.set_status_code(resp.status);

    let body = match out.body() {
        Ok(b) => b,
        Err(()) => {
            ResponseOutparam::set(
                response_out,
                Err(ErrorCode::InternalError(Some(
                    "无法获取响应 body 资源".to_string(),
                ))),
            );
            return;
        }
    };

    ResponseOutparam::set(response_out, Ok(out));

    if let Ok(mut stream) = body.write() {
        let _ = stream.write_all(&resp.body);
        let _ = stream.flush();
        drop(stream);
    }
    let _ = OutgoingBody::finish(body, None);
}

// ════════════════════════════════════════════════════════════════════
// 纯解析工具（可原生单测）
// ════════════════════════════════════════════════════════════════════

/// `"/api/pods?namespace=x"` → `("/api/pods", "namespace=x")`
pub fn split_path_query(raw: &str) -> (String, String) {
    match raw.split_once('?') {
        Some((p, q)) => {
            let p = if p.is_empty() { "/" } else { p };
            (p.to_string(), q.to_string())
        }
        None => {
            let p = if raw.is_empty() { "/" } else { raw };
            (p.to_string(), String::new())
        }
    }
}

/// 查询串里的百分号解码。
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_path_query_basics() {
        assert_eq!(
            split_path_query("/api/pods?namespace=x"),
            ("/api/pods".into(), "namespace=x".into())
        );
        assert_eq!(split_path_query("/"), ("/".into(), "".into()));
        assert_eq!(split_path_query(""), ("/".into(), "".into()));
        assert_eq!(split_path_query("/a?"), ("/a".into(), "".into()));
    }

    #[test]
    fn percent_decode_handles_common_cases() {
        assert_eq!(percent_decode("a%3Db"), "a=b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%2F"), "/");
        // 残缺的转义不能 panic，原样保留
        assert_eq!(percent_decode("bad%2"), "bad%2");
    }

    #[test]
    fn query_param_reads_and_decodes() {
        let req = Request {
            method: "GET".into(),
            path: "/api/pods".into(),
            query: "namespace=k3s-wasm&node=k3s-node-1&empty=".into(),
            headers: vec![],
            body: vec![],
        };
        assert_eq!(req.query_param("namespace"), Some("k3s-wasm".into()));
        assert_eq!(req.query_param("node"), Some("k3s-node-1".into()));
        assert_eq!(req.query_param("empty"), Some("".into()));
        assert_eq!(req.query_param("missing"), None);
    }

    #[test]
    fn host_header_is_synthesized_from_authority() {
        // wasi:http 把 Host 放在 authority 上，headers 里可能没有 —— 实测 wasmtime 就是
        // 这样，症状是 WebAuthn 注册接口回 400「请求没有 Host 头」。
        let h = ensure_host_header(vec![], Some("console.example.test".into()));
        assert_eq!(h, vec![("host".to_string(), "console.example.test".to_string())]);
        // 已有 Host（大小写不敏感）就不要重复补
        let h2 = ensure_host_header(vec![("Host".into(), "a.test".into())], Some("b.test".into()));
        assert_eq!(h2, vec![("Host".to_string(), "a.test".to_string())]);
        // 没有 authority 就保持原样
        assert!(ensure_host_header(vec![], None).is_empty());
        assert!(ensure_host_header(vec![], Some("   ".into())).is_empty());
    }

    #[test]
    fn envelope_shapes() {
        let ok = Response::ok(serde_json::json!({"a": 1}));
        assert_eq!(ok.status, 200);
        assert!(String::from_utf8_lossy(&ok.body).contains("\"ok\":true"));

        let err = Response::fail(403, "forbidden");
        assert_eq!(err.status, 403);
        let v: serde_json::Value = serde_json::from_slice(&err.body).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["status"], 403);
    }
}
