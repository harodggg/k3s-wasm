//! 最小的 `wasi:http/proxy` 组件示例。
//!
//! 它的价值在于**两种宿主都能跑**：
//!   · wasmtime shim（RuntimeClass wasmtime-wasip2）：按导出名自动识别，容器内监听 8080
//!   · Spin（本地 `spin up`，或 SpinKube 的 SpinApp）：由 Spin 的 HTTP 触发器承载，监听 80
//!
//! 两者都不需要改代码 —— 因为这里没有任何 `spin:*` 导入，就是标准 WASI 接口。
//!
//! 本地跑：
//!   cargo build --release --target wasm32-wasip2
//!   wasmtime serve -C cache=n -S http=y -S inherit-network=y \
//!       target/wasm32-wasip2/release/hello_http.wasm
//!   curl -i localhost:8080/hello

use std::io::Write as _;

use wasi::http::types::{Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam};

struct HelloHttp;

impl wasi::exports::http::incoming_handler::Guest for HelloHttp {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_else(|| "/".to_string());
        let (body, status) = if path.starts_with("/hello") {
            (format!("hello from wasm32-wasip2（路径 {path}）\n"), 200u16)
        } else {
            (format!("试试 /hello，当前是 {path}\n"), 404u16)
        };

        let mut headers = Fields::new();
        let _ = headers.set("content-type", &[b"text/plain; charset=utf-8".to_vec()]);
        let resp = OutgoingResponse::new(headers);
        let _ = resp.set_status_code(status);

        let out = match resp.body() {
            Ok(b) => b,
            Err(()) => {
                ResponseOutparam::set(
                    response_out,
                    Err(wasi::http::types::ErrorCode::InternalError(None)),
                );
                return;
            }
        };

        // 顺序要求：先 set ResponseOutparam，再写 body，最后 finish
        ResponseOutparam::set(response_out, Ok(resp));
        if let Ok(mut stream) = out.write() {
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
            drop(stream);
        }
        let _ = OutgoingBody::finish(out, None);
    }
}

wasi::http::proxy::export!(HelloHttp);
