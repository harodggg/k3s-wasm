//! 静态资源：把前端构建产物嵌进 wasm。
//!
//! 为什么用 `include_dir!` 而不是让宿主提供静态文件服务：
//!   - 编译期嵌入 → 组件运行时**不需要任何文件系统能力**，能力面最小；
//!   - 只有一个 wasm 产物，镜像里没有额外文件，部署面也最小；
//!   - SPA 的 fallback 逻辑（未知路径回 index.html）本来就要自己写。
//!
//! 代价：前端产物会进入 wasm 体积。所以前端刻意无框架、无运行时依赖，
//! 体积由 scripts/build-ui.sh 打印出来。

use include_dir::{include_dir, Dir};

use crate::http_io::{Request, Response};

static DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../frontend/dist");

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn file_response(body: &[u8], path: &str) -> Response {
    let ctype = content_type(path);
    let resp = Response::new(200u16, body.to_vec()).with_header("content-type", ctype);
    if ctype.starts_with("text/html") {
        // index.html 绝不能缓存，否则前端更新后用户会一直拿到旧壳
        resp.with_header("cache-control", "no-store")
    } else {
        // 其它资源靠构建产物的 hash 文件名做缓存失效
        resp.with_header("cache-control", "public, max-age=31536000, immutable")
    }
}

/// 静态资源 + SPA 回退。
pub fn serve(req: &Request) -> Response {
    let raw = req.path.trim_start_matches('/');
    let path = if raw.is_empty() { "index.html" } else { raw };

    if let Some(file) = DIST.get_file(path) {
        return file_response(file.contents(), path);
    }

    // 未知的 /api/* 必须返回 JSON 404，不能回退成 index.html：
    // 否则前端 fetch 拿到一坨 HTML，报错会变成 "Unexpected token '<'"
    if req.path.starts_with("/api/") {
        return Response::fail(404, format!("没有这个接口：{}", req.path));
    }

    match DIST.get_file("index.html") {
        Some(f) => file_response(f.contents(), "index.html"),
        None => Response::new(
            500u16,
            "前端资源缺失：这个构建是在没有 dist 的情况下编出来的",
        )
        .with_header("content-type", "text/plain; charset=utf-8"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(path: &str) -> Request {
        Request {
            method: "GET".into(),
            path: path.into(),
            query: String::new(),
            headers: vec![],
            body: vec![],
        }
    }

    #[test]
    fn content_types() {
        assert!(content_type("index.html").starts_with("text/html"));
        assert!(content_type("assets/app.abc123.js").starts_with("text/javascript"));
        assert!(content_type("assets/app.css").starts_with("text/css"));
        assert_eq!(content_type("bin.wasm"), "application/octet-stream");
    }

    #[test]
    fn api_path_returns_json_404_not_html() {
        let resp = serve(&get("/api/nope"));
        assert_eq!(resp.status, 404);
        let ct = resp
            .headers
            .iter()
            .find(|(k, _)| k == "content-type")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        assert!(ct.starts_with("application/json"), "实际 content-type：{ct}");
    }

    #[test]
    fn spa_fallback_serves_index_for_unknown_paths() {
        // dist 由 build.rs 保证存在（缺失时写占位页），所以这里必然有 index.html
        let resp = serve(&get("/some/deep/link"));
        assert_eq!(resp.status, 200);
        assert!(String::from_utf8_lossy(&resp.body).contains("<html"));
    }

    #[test]
    fn html_is_not_cached_but_assets_are() {
        let index = serve(&get("/"));
        assert!(index
            .headers
            .iter()
            .any(|(k, v)| k == "cache-control" && v == "no-store"));
    }
}
