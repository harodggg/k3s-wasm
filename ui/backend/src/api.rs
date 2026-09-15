//! HTTP 处理函数与 JSON 形态收窄。
//!
//! 约定：所有 /api/* 返回统一信封，前端只认这两种形状。
//! ```json
//! { "ok": true,  "data": ... }
//! { "ok": false, "error": { "message": "...", "status": 403 } }
//! ```
//! 因为是纯函数 + 无 async，这里的每个函数都能在 host 上直接单测（见文件末尾）。

use serde_json::{json, Map, Value};

use crate::config::{Config, DEFAULT_SPINAPP_EXECUTOR, MANAGED_BY, XRAY_PART_OF};
use crate::http_io::{Request, Response};
use crate::k8s::{encode_query, ApiError, K8s};

/// SpinApp CRD 的 group/version。允许从请求覆盖：
/// 上游换版本时前端改一个字段即可，不必重新构建 wasm。
pub const SPINAPP_DEFAULT_APIVERSION: &str = "core.spinkube.dev/v1alpha1";

fn client() -> (Config, K8s) {
    let cfg = Config::load();
    let k8s = K8s::new(&cfg.proxy_url);
    (cfg, k8s)
}

/// k8s 的错误原样暴露给前端，方便直接看到 RBAC 拒绝的原因。
fn from_err(e: ApiError) -> Response {
    Response::fail(e.http_status(), e.message())
}

fn from_result(r: Result<Value, ApiError>) -> Response {
    match r {
        Ok(v) => Response::ok(v),
        Err(e) => from_err(e),
    }
}

// ════════════════════════════════════════════════════════════════════
// 请求参数校验
// ════════════════════════════════════════════════════════════════════

/// DNS-1123 label：k8s 对象名的合法字符集，手写避免为了一个校验引入 regex。
fn is_dns1123_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

fn required_str(body: &Value, key: &str) -> Result<String, Response> {
    body[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Response::fail(400, format!("缺少必填字段 {key}")))
}

fn ns_or_default(cfg: &Config, req: &Request) -> String {
    req.query_param("namespace")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cfg.default_namespace.clone())
}

// ════════════════════════════════════════════════════════════════════
// 标签 / 运行时判定
// ════════════════════════════════════════════════════════════════════

fn label_bool(labels: &Value, key: &str) -> bool {
    labels
        .get(key)
        .and_then(Value::as_str)
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// 判断一个 Pod 模板是不是 wasm 工作负载。
/// 依据只有 runtimeClassName —— 靠镜像名猜不可靠。
fn runtime_is_wasm(runtime_class: &str) -> bool {
    runtime_class.contains("wasm") || runtime_class.contains("spin")
}

fn shape_node(node: &Value) -> Value {
    let meta = &node["metadata"];
    let status = &node["status"];
    let labels = meta["labels"].clone();
    let ready = status["conditions"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .any(|c| c["type"] == "Ready" && c["status"] == "True")
        })
        .unwrap_or(false);
    json!({
        "name": meta["name"],
        "ready": ready,
        "unschedulable": node["spec"]["unschedulable"].as_bool().unwrap_or(false),
        "arch": status["nodeInfo"]["architecture"],
        "os": status["nodeInfo"]["operatingSystem"],
        "kubeletVersion": status["nodeInfo"]["kubeletVersion"],
        "runtime": status["nodeInfo"]["containerRuntimeVersion"],
        "addresses": status["addresses"],
        "capacity": {
            "cpu": status["capacity"]["cpu"],
            "memory": status["capacity"]["memory"],
            "pods": status["capacity"]["pods"],
        },
        // wasm 能力来自安装脚本打的节点标签
        "wasm": {
            "spin": label_bool(&labels, "wasm.sh/spin"),
            "wasmtime": label_bool(&labels, "wasm.sh/wasmtime"),
        },
        "labels": labels,
    })
}

fn shape_pod(pod: &Value) -> Value {
    let spec = &pod["spec"];
    let status = &pod["status"];
    let runtime_class = spec["runtimeClassName"].as_str().unwrap_or("");
    let statuses = status["containerStatuses"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let ready_count = statuses.iter().filter(|c| c["ready"] == true).count();
    let restarts: i64 = statuses
        .iter()
        .map(|c| c["restartCount"].as_i64().unwrap_or(0))
        .sum();
    let image = spec["containers"]
        .as_array()
        .and_then(|c| c.first())
        .map(|c| c["image"].clone())
        .unwrap_or(Value::Null);

    json!({
        "name": pod["metadata"]["name"],
        "namespace": pod["metadata"]["namespace"],
        "node": spec["nodeName"],
        "phase": status["phase"],
        "podIP": status["podIP"],
        "ready": format!("{ready_count}/{}", spec["containers"].as_array().map(|c| c.len()).unwrap_or(0)),
        "restarts": restarts,
        "image": image,
        "runtimeClass": runtime_class,
        "isWasm": runtime_is_wasm(runtime_class),
        "startedAt": status["startTime"],
        "createdAt": pod["metadata"]["creationTimestamp"],
        "labels": pod["metadata"]["labels"],
    })
}

fn shape_deployment(dep: &Value) -> Value {
    let spec = &dep["spec"];
    let status = &dep["status"];
    let template = &spec["template"]["spec"];
    let runtime_class = template["runtimeClassName"].as_str().unwrap_or("");
    json!({
        "kind": "Deployment",
        "name": dep["metadata"]["name"],
        "namespace": dep["metadata"]["namespace"],
        "replicas": spec["replicas"].as_i64().unwrap_or(1),
        "readyReplicas": status["readyReplicas"].as_i64().unwrap_or(0),
        "availableReplicas": status["availableReplicas"].as_i64().unwrap_or(0),
        "image": template["containers"].as_array().and_then(|c| c.first()).map(|c| c["image"].clone()).unwrap_or(Value::Null),
        "runtimeClass": runtime_class,
        "isWasm": runtime_is_wasm(runtime_class),
        "labels": dep["metadata"]["labels"],
        "createdAt": dep["metadata"]["creationTimestamp"],
    })
}

fn shape_spinapp(app: &Value) -> Value {
    let spec = &app["spec"];
    json!({
        "kind": "SpinApp",
        "name": app["metadata"]["name"],
        "namespace": app["metadata"]["namespace"],
        "image": spec["image"],
        "replicas": spec["replicas"].as_i64().unwrap_or(1),
        "readyReplicas": app["status"]["readyReplicas"].as_i64().unwrap_or(0),
        // SpinApp 没有 runtimeClassName 字段：它由 executor 决定
        "executor": spec["executor"],
        "variables": spec["variables"],
        "conditions": app["status"]["conditions"],
        "createdAt": app["metadata"]["creationTimestamp"],
        "isWasm": true,
    })
}

fn shape_list(items: &Value, f: impl Fn(&Value) -> Value) -> Value {
    let arr = items["items"].as_array().cloned().unwrap_or_default();
    Value::Array(arr.iter().map(f).collect())
}

fn filter_items_by_ns(mut list: Value, ns: &str) -> Value {
    if ns.is_empty() || ns == "_all" {
        return list;
    }
    if let Some(items) = list["items"].as_array_mut() {
        items.retain(|i| i["metadata"]["namespace"] == ns);
    }
    list
}

// ════════════════════════════════════════════════════════════════════
// 自身状态
// ════════════════════════════════════════════════════════════════════

pub fn health(_req: &Request) -> Response {
    let (cfg, _k8s) = client();
    Response::ok(json!({
        "status": "ok",
        "component": "k3s-wasm-ui",
        "target": "wasm32-wasip2",
        "interface": "wasi:http/incoming-handler@0.2.12",
        "proxyUrl": cfg.proxy_url,
        "proxyUrlSource": cfg.proxy_url_source.as_str(),
        "defaultNamespace": cfg.default_namespace,
    }))
}

pub fn summary(_req: &Request) -> Response {
    let (cfg, k8s) = client();

    // 先取最基础的三个列表；任一失败就直接返回（否则后面的数字会误导人）
    let nodes = match k8s.get("/api/v1/nodes") {
        Ok(v) => v,
        Err(e) => return from_err(e),
    };
    let pods = match k8s.get("/api/v1/pods?limit=2000") {
        Ok(v) => v,
        Err(e) => return from_err(e),
    };
    let namespaces = k8s
        .get("/api/v1/namespaces")
        .map(|v| v["items"].as_array().map(|a| a.len()).unwrap_or(0))
        .unwrap_or(0);

    let node_items = nodes["items"].as_array().cloned().unwrap_or_default();
    let node_total = node_items.len();
    let node_ready = node_items
        .iter()
        .filter(|n| shape_node(n)["ready"] == true)
        .count();
    let node_wasm = node_items
        .iter()
        .filter(|n| {
            let l = &n["metadata"]["labels"];
            label_bool(l, "wasm.sh/spin") || label_bool(l, "wasm.sh/wasmtime")
        })
        .count();

    let pod_items = pods["items"].as_array().cloned().unwrap_or_default();
    let pod_count = |phase: &str| {
        pod_items
            .iter()
            .filter(|p| p["status"]["phase"] == phase)
            .count()
    };
    let wasm_pods = pod_items
        .iter()
        .filter(|p| runtime_is_wasm(p["spec"]["runtimeClassName"].as_str().unwrap_or("")))
        .count();

    let runtimes = match runtime_list(&k8s, &node_items) {
        Ok(v) => v,
        Err(e) => return from_err(e),
    };

    // SpinKube 不一定装了，所以这里失败不算错误
    let spinapps = match k8s.get(&spinapp_path(SPINAPP_DEFAULT_APIVERSION, None)) {
        Ok(v) => json!({ "installed": true, "count": v["items"].as_array().map(|a| a.len()).unwrap_or(0) }),
        Err(e) if e.is_not_found() => json!({ "installed": false, "count": 0 }),
        Err(e) => json!({ "installed": null, "count": 0, "error": e.message() }),
    };

    let xray = xray_deployments(&k8s, "").map(|v| v.len()).unwrap_or(0);

    let version = k8s
        .get("/version")
        .map(|v| json!({ "gitVersion": v["gitVersion"], "platform": v["platform"] }))
        .unwrap_or(Value::Null);

    Response::ok(json!({
        "version": version,
        "nodes": { "total": node_total, "ready": node_ready, "wasmCapable": node_wasm },
        "pods": {
            "total": pod_items.len(),
            "running": pod_count("Running"),
            "pending": pod_count("Pending"),
            "failed": pod_count("Failed"),
            "succeeded": pod_count("Succeeded"),
            "wasm": wasm_pods,
        },
        "namespaces": namespaces,
        "runtimes": runtimes,
        "spinapps": spinapps,
        "xrayTunnels": xray,
        "config": {
            "proxyUrl": cfg.proxy_url,
            "proxyUrlSource": cfg.proxy_url_source.as_str(),
            "defaultNamespace": cfg.default_namespace,
        },
    }))
}

// ════════════════════════════════════════════════════════════════════
// 集群视图
// ════════════════════════════════════════════════════════════════════

pub fn nodes(_req: &Request) -> Response {
    let (_cfg, k8s) = client();
    from_result(k8s.get("/api/v1/nodes").map(|v| shape_list(&v, shape_node)))
}

/// 把 RuntimeClass 与「有多少节点具备该能力」合在一起。
///
/// 顺带把「RuntimeClass 存在但没有任何节点装了 shim」标出来 ——
/// 这是最常见的坑：Pod 一直 Pending，或者 shim 报 no runtime configured。
fn runtime_list(k8s: &K8s, nodes: &[Value]) -> Result<Value, ApiError> {
    let rcs = k8s.get("/apis/node.k8s.io/v1/runtimeclasses")?;
    let items = rcs["items"].as_array().cloned().unwrap_or_default();

    let out: Vec<Value> = items
        .iter()
        .map(|rc| {
            let handler = rc["handler"].as_str().unwrap_or("");
            let is_wasm = runtime_is_wasm(handler)
                || runtime_is_wasm(rc["metadata"]["name"].as_str().unwrap_or(""));
            // 契约上 nodeSelector 是个 map：没有 scheduler 时**发 {} 而不是 null**。
            // 事故复盘：早先发 null，前端 Object.entries(null) 抛
            // "Cannot convert undefined or null to object"，而自动刷新每 5 秒把它刷成横幅。
            let node_selector = if rc["scheduling"]["nodeSelector"].is_null() {
                json!({})
            } else {
                rc["scheduling"]["nodeSelector"].clone()
            };
            let selectorless = node_selector
                .as_object()
                .map(|o| o.is_empty())
                .unwrap_or(true);
            let capable = if selectorless {
                nodes.len()
            } else {
                nodes
                    .iter()
                    .filter(|n| {
                        node_selector
                            .as_object()
                            .map(|sel| {
                                let labels = &n["metadata"]["labels"];
                                sel.iter().all(|(k, v)| labels.get(k) == Some(v))
                            })
                            .unwrap_or(false)
                    })
                    .count()
            };
            json!({
                "name": rc["metadata"]["name"],
                "handler": handler,
                "isWasm": is_wasm,
                "nodeSelector": node_selector,
                // wasm 运行时如果**没有** nodeSelector，就无法从节点标签判断它到底装没装
                // （k3s 会给 wasmedge/wasmer 这些也建 RuntimeClass，但节点上未必有二进制）。
                // 这种情况返回 null，让前端显示「无法判定」，而不是编一个数字出来。
                "capableNodes": if selectorless && is_wasm { Value::Null } else { json!(capable) },
                "selectorless": selectorless,
                "misconfigured": !selectorless && capable == 0,
            })
        })
        .collect();

    Ok(Value::Array(out))
}

pub fn runtimes(_req: &Request) -> Response {
    let (_cfg, k8s) = client();
    let nodes = match k8s.get("/api/v1/nodes") {
        Ok(v) => v["items"].as_array().cloned().unwrap_or_default(),
        Err(e) => return from_err(e),
    };
    match runtime_list(&k8s, &nodes) {
        Ok(v) => Response::ok(v),
        Err(e) => from_err(e),
    }
}

pub fn namespaces(_req: &Request) -> Response {
    let (_cfg, k8s) = client();
    from_result(k8s.get("/api/v1/namespaces").map(|v| {
        shape_list(&v, |n| {
            json!({
                "name": n["metadata"]["name"],
                "phase": n["status"]["phase"],
                "createdAt": n["metadata"]["creationTimestamp"],
            })
        })
    }))
}

/// Deployment 列表（可按命名空间过滤）。SpinApp 走单独的接口。
pub fn workloads(req: &Request) -> Response {
    let (cfg, k8s) = client();
    let _ = cfg;
    let ns = req.query_param("namespace").unwrap_or_default();
    let path = if ns.is_empty() || ns == "_all" {
        "/apis/apps/v1/deployments?limit=1000".to_string()
    } else if is_dns1123_label(&ns) {
        format!("/apis/apps/v1/namespaces/{ns}/deployments")
    } else {
        return Response::fail(400, format!("命名空间不合法：{ns}"));
    };
    from_result(k8s.get(&path).map(|v| {
        let v = filter_items_by_ns(v, &ns);
        shape_list(&v, shape_deployment)
    }))
}

pub fn pods(req: &Request) -> Response {
    let (cfg, k8s) = client();
    let ns = req.query_param("namespace").unwrap_or_default();
    let node = req.query_param("node").unwrap_or_default();

    let mut path = if ns.is_empty() || ns == "_all" || ns == cfg.default_namespace {
        "/api/v1/pods?limit=2000".to_string()
    } else if is_dns1123_label(&ns) {
        format!("/api/v1/namespaces/{ns}/pods")
    } else {
        return Response::fail(400, format!("命名空间不合法：{ns}"));
    };
    if !node.is_empty() {
        path.push_str(&format!(
            "{}fieldSelector=spec.nodeName%3D{}",
            if path.contains('?') { "&" } else { "?" },
            encode_query(&node)
        ));
    }

    from_result(k8s.get(&path).map(|v| {
        let v = if !ns.is_empty() && ns != "_all" {
            filter_items_by_ns(v, &ns)
        } else {
            v
        };
        shape_list(&v, shape_pod)
    }))
}

pub fn pod_logs(req: &Request, ns: &str, name: &str) -> Response {
    let (_cfg, k8s) = client();
    if !is_dns1123_label(ns) || !is_dns1123_label(name) {
        return Response::fail(400, "命名空间或 Pod 名不合法");
    }
    let tail = req
        .query_param("tail")
        .and_then(|t| t.parse::<u32>().ok())
        .unwrap_or(200)
        .min(2000);
    let container = req.query_param("container").unwrap_or_default();

    let mut path = format!("/api/v1/namespaces/{ns}/pods/{name}/log?tailLines={tail}");
    if !container.is_empty() {
        path.push_str(&format!("&container={}", encode_query(&container)));
    }

    match k8s.get_text(&path) {
        Ok(text) => Response::ok(json!({ "namespace": ns, "pod": name, "tailLines": tail, "log": text })),
        Err(e) => from_err(e),
    }
}

pub fn events(req: &Request) -> Response {
    let (cfg, k8s) = client();
    let ns = ns_or_default(&cfg, req);
    if !is_dns1123_label(&ns) {
        return Response::fail(400, format!("命名空间不合法：{ns}"));
    }
    from_result(
        k8s.get(&format!("/api/v1/namespaces/{ns}/events?limit=100"))
            .map(|v| {
                shape_list(&v, |e| {
                    json!({
                        "type": e["type"],
                        "reason": e["reason"],
                        "object": format!(
                            "{}/{}",
                            e["involvedObject"]["kind"].as_str().unwrap_or(""),
                            e["involvedObject"]["name"].as_str().unwrap_or("")
                        ),
                        "message": e["message"],
                        "count": e["count"],
                        "lastSeen": e["lastTimestamp"],
                    })
                })
            }),
    )
}

// ════════════════════════════════════════════════════════════════════
// SpinApp（SpinKube CRD）
// ════════════════════════════════════════════════════════════════════

fn spinapp_path(api_version: &str, ns: Option<&str>) -> String {
    match ns {
        Some(ns) => format!("/apis/{api_version}/namespaces/{ns}/spinapps"),
        None => format!("/apis/{api_version}/spinapps"),
    }
}

pub fn spinapps_list(req: &Request) -> Response {
    let (_cfg, k8s) = client();
    let api_version = req
        .query_param("apiVersion")
        .unwrap_or_else(|| SPINAPP_DEFAULT_APIVERSION.to_string());
    let ns = req.query_param("namespace").unwrap_or_default();

    let path = if ns.is_empty() || ns == "_all" {
        spinapp_path(&api_version, None)
    } else if is_dns1123_label(&ns) {
        spinapp_path(&api_version, Some(&ns))
    } else {
        return Response::fail(400, format!("命名空间不合法：{ns}"));
    };

    match k8s.get(&path) {
        Ok(v) => Response::ok(json!({
            "installed": true,
            "apiVersion": api_version,
            "items": shape_list(&v, shape_spinapp),
        })),
        // CRD 没装：不是错误，是一个要明确告诉用户的状态
        Err(e) if e.is_not_found() => Response::ok(json!({
            "installed": false,
            "apiVersion": api_version,
            "items": [],
            "hint": "集群里没有 SpinApp CRD。用 scripts/install-spinkube.sh 装 SpinKube；\
                     或者不走 SpinKube，直接用 runtimeClassName 跑 Deployment（本仓库的默认路径）。",
        })),
        Err(e) => from_err(e),
    }
}

/// 列出 SpinAppExecutor —— SpinApp 的 runtimeClassName / 镜像等来自它，
/// 所以创建界面上应当让用户从实际存在的 executor 里选，而不是手写 runtimeClassName。
pub fn spinapp_executors(req: &Request) -> Response {
    let (cfg, k8s) = client();
    let ns = ns_or_default(&cfg, req);
    if !is_dns1123_label(&ns) {
        return Response::fail(400, format!("命名空间不合法：{ns}"));
    }
    let path = format!("/apis/{SPINAPP_DEFAULT_APIVERSION}/namespaces/{ns}/spinappexecutors");
    match k8s.get(&path) {
        Ok(v) => Response::ok(json!({
            "installed": true,
            "items": shape_list(&v, |e| json!({
                "name": e["metadata"]["name"],
                "createDeployment": e["spec"]["createDeployment"],
                "runtimeClassName": e["spec"]["deploymentConfig"]["runtimeClassName"],
                "spinImage": e["spec"]["deploymentConfig"]["spinImage"],
            })),
        })),
        Err(e) if e.is_not_found() => Response::ok(json!({
            "installed": false,
            "items": [],
            "hint": "没有 SpinAppExecutor。apply spin-operator 的 spin-operator.shim-executor.yaml 会创建一个名为 containerd-shim-spin 的 executor。",
        })),
        Err(e) => from_err(e),
    }
}

pub fn spinapp_create(req: &Request) -> Response {
    let body = match req.json_body() {
        Ok(b) => b,
        Err(m) => return Response::fail(400, m),
    };
    let (cfg, k8s) = client();

    let name = match required_str(&body, "name") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let namespace = body["namespace"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(&cfg.default_namespace)
        .to_string();
    let image = match required_str(&body, "image") {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !is_dns1123_label(&name) || !is_dns1123_label(&namespace) {
        return Response::fail(400, "name / namespace 必须是合法 DNS-1123 名称（小写字母数字和 -）");
    }
    let replicas = body["replicas"].as_i64().unwrap_or(1).clamp(0, 100);
    let api_version = body["apiVersion"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(SPINAPP_DEFAULT_APIVERSION);

    // 只发最小的、跨版本都成立的字段；其余交给 operator 默认值。
    let mut spec = Map::new();
    spec.insert("image".into(), json!(image));
    spec.insert("replicas".into(), json!(replicas));
    // executor 是必填项（CRD 层面），runtimeClassName 由 executor 决定
    spec.insert(
        "executor".into(),
        json!(body["executor"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_SPINAPP_EXECUTOR)),
    );
    if let Some(ips) = body["imagePullSecrets"].as_array() {
        spec.insert("imagePullSecrets".into(), Value::Array(ips.clone()));
    }
    if let Some(vars) = body["variables"].as_object() {
        // CRD 里 variables 是 [{name, value}] 列表；这里接受对象写法并转换
        let list: Vec<Value> = vars
            .iter()
            .map(|(k, v)| json!({ "name": k, "value": v.as_str().unwrap_or_default() }))
            .collect();
        spec.insert("variables".into(), Value::Array(list));
    }
    // 高级：允许直接覆盖/追加 spec 字段，避免为了个别字段重新构建 wasm
    if let Some(extra) = body["spec"].as_object() {
        for (k, v) in extra {
            spec.insert(k.clone(), v.clone());
        }
    }

    let manifest = json!({
        "apiVersion": api_version,
        "kind": "SpinApp",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": {
                "app.kubernetes.io/managed-by": MANAGED_BY,
                "app.kubernetes.io/part-of": "spin-apps",
            },
        },
        "spec": Value::Object(spec),
    });

    match k8s.post(&spinapp_path(api_version, Some(&namespace)), &manifest) {
        Ok(v) => Response::ok(json!({
            "created": true,
            "apiVersion": api_version,
            "namespace": namespace,
            "name": name,
            "resource": shape_spinapp(&v),
        })),
        Err(e) => from_err(e),
    }
}

pub fn spinapp_scale(req: &Request, ns: &str, name: &str) -> Response {
    let body = match req.json_body() {
        Ok(b) => b,
        Err(m) => return Response::fail(400, m),
    };
    let (_cfg, k8s) = client();
    if !is_dns1123_label(ns) || !is_dns1123_label(name) {
        return Response::fail(400, "命名空间或名称不合法");
    }
    let Some(replicas) = body["replicas"].as_i64() else {
        return Response::fail(400, "缺少 replicas（整数）");
    };
    let replicas = replicas.clamp(0, 100);
    let api_version = body["apiVersion"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(SPINAPP_DEFAULT_APIVERSION);

    let path = format!("{}/{name}", spinapp_path(api_version, Some(ns)));
    from_result(
        k8s.merge_patch(&path, &json!({ "spec": { "replicas": replicas } }))
            .map(|v| shape_spinapp(&v)),
    )
}

pub fn spinapp_delete(req: &Request, ns: &str, name: &str) -> Response {
    let (_cfg, k8s) = client();
    if !is_dns1123_label(ns) || !is_dns1123_label(name) {
        return Response::fail(400, "命名空间或名称不合法");
    }
    let api_version = req
        .query_param("apiVersion")
        .unwrap_or_else(|| SPINAPP_DEFAULT_APIVERSION.to_string());
    let path = format!("{}/{name}", spinapp_path(&api_version, Some(ns)));
    match k8s.delete(&path) {
        Ok(_) => Response::ok(json!({ "deleted": true, "namespace": ns, "name": name })),
        Err(e) if e.is_not_found() => Response::ok(json!({ "deleted": false, "reason": "不存在" })),
        Err(e) => from_err(e),
    }
}

// ════════════════════════════════════════════════════════════════════
// xray-wasm 隧道
// ════════════════════════════════════════════════════════════════════

/// UI 认领的隧道 = 带这两个标签的 Deployment。
fn xray_selector() -> String {
    format!("app.kubernetes.io/part-of={XRAY_PART_OF},app.kubernetes.io/managed-by={MANAGED_BY}")
}

fn xray_deployments(k8s: &K8s, ns: &str) -> Result<Vec<Value>, ApiError> {
    let sel = encode_query(&xray_selector());
    let path = if ns.is_empty() || ns == "_all" {
        format!("/apis/apps/v1/deployments?labelSelector={sel}")
    } else {
        format!("/apis/apps/v1/namespaces/{ns}/deployments?labelSelector={sel}")
    };
    let v = k8s.get(&path)?;
    Ok(v["items"].as_array().cloned().unwrap_or_default())
}

/// 隧道配置放在 ConfigMap 里（键 tunnel.json）。
///
/// 为什么不是 Secret：ClusterRole 里**故意没有** secrets 权限，这样即使
/// kube-api-proxy 被同命名空间其它 Pod 摸到，也读不到集群里的任何密钥。
/// 代价是隧道配置（含 UUID）以明文存在 ConfigMap 里 —— 不接受的话见
/// docs/03-xray-wasm.md 的「改用 Secret」。
/// 这条隧道的「入站面」：谁能连进它的监听端口。
///
/// 说明：xray-wasm 是**客户端**，架构上只能做出站（把集群内的流量送出去）。
/// 但「入站」这半件事是真实且可核对的 —— 就是这个 Service 的暴露方式：
///   ClusterIP   → 仅集群内可达
///   NodePort    → 公网可达（nodePort 写在下面，UI 会当风险提示）
///   LoadBalancer→ 公网可达
fn shape_exposure(svc: Option<&Value>, fallback_port: u16) -> Value {
    let Some(svc) = svc else {
        return json!({
            "serviceType": null,
            "nodePort": null,
            "port": fallback_port,
            "reach": "未找到 Service（入口可能被手工删过）",
            "public": null,
        });
    };
    let ty = svc["spec"]["type"].as_str().unwrap_or("ClusterIP");
    let port = svc["spec"]["ports"]
        .as_array()
        .and_then(|p| p.first())
        .and_then(|p| p["port"].as_u64())
        .map(|v| v as u16)
        .unwrap_or(fallback_port);
    let node_port = svc["spec"]["ports"]
        .as_array()
        .and_then(|p| p.first())
        .and_then(|p| p["nodePort"].as_u64());
    let (reach, public) = match ty {
        "ClusterIP" => ("仅集群内可达".to_string(), false),
        "NodePort" => (
            match node_port {
                Some(np) => format!("公网可达（NodePort {np}）"),
                None => "公网可达（NodePort）".to_string(),
            },
            true,
        ),
        other => (format!("公网可达（{other}）"), true),
    };
    json!({ "serviceType": ty, "nodePort": node_port, "port": port, "reach": reach, "public": public })
}

fn xray_shape(dep: &Value, svc: Option<&Value>) -> Value {
    let spec = &dep["spec"];
    let status = &dep["status"];
    let ann = &dep["metadata"]["annotations"];
    let listen = ann["k3s-wasm/listen"].as_str().unwrap_or("0.0.0.0:1080");
    let port: u16 = listen.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(1080);
    let ns = dep["metadata"]["namespace"].as_str().unwrap_or_default();
    let name = dep["metadata"]["name"].as_str().unwrap_or_default();

    json!({
        "kind": "xray-tunnel",
        "name": dep["metadata"]["name"],
        "namespace": dep["metadata"]["namespace"],
        "replicas": spec["replicas"].as_i64().unwrap_or(2),
        "readyReplicas": status["readyReplicas"].as_i64().unwrap_or(0),
        "image": spec["template"]["spec"]["containers"].as_array()
            .and_then(|c| c.first()).map(|c| c["image"].clone()).unwrap_or(Value::Null),
        "runtimeClass": spec["template"]["spec"]["runtimeClassName"],
        "createdAt": dep["metadata"]["creationTimestamp"],
        // 只回显非敏感字段；凭据在 Secret 里，控制台不读也不回显
        "tunnel": {
            "server": ann["k3s-wasm/server"],
            "sni": ann["k3s-wasm/sni"],
            "shortId": ann["k3s-wasm/short-id"],
            "listen": listen,
            "hasUuid": true,
        },
        "managedBy": MANAGED_BY,
        "isWasm": true,
        // ── 方向 ───────────────────────────────────────────────────
        // 出站：流量从集群内 → 经 REALITY 服务端 → 目标（本面板创建的就是这个方向）
        // 入站：这里指「谁能连进它的监听端口」，由 Service 类型决定
        "direction": "egress",
        "directionLabel": "出站（集群内 → 经 REALITY 出网）",
        "egress": {
            "via": ann["k3s-wasm/server"],
            "protocol": "SOCKS5 → VLESS+XTLS-Vision+REALITY",
            "note": "客户端主动连它才生效；不接管集群内其它流量",
        },
        "ingress": {
            "endpoint": format!("{name}.{ns}.svc.cluster.local:{port}"),
            "exposure": shape_exposure(svc, port),
        },
    })
}

pub fn xray_list(req: &Request) -> Response {
    let (cfg, k8s) = client();
    let ns = req
        .query_param("namespace")
        .unwrap_or_else(|| cfg.default_namespace.clone());

    let deps = match xray_deployments(&k8s, &ns) {
        Ok(d) => d,
        Err(e) => return from_err(e),
    };

    // 入站面要看 Service 的真实类型（ClusterIP / NodePort / LoadBalancer），
    // 不能靠猜：暴露与否决定了这条隧道是否等于把出口代理公开出去。
    let sel = encode_query(&xray_selector());
    let svc_path = if ns.is_empty() || ns == "_all" {
        format!("/api/v1/services?labelSelector={sel}")
    } else {
        format!("/api/v1/namespaces/{ns}/services?labelSelector={sel}")
    };
    let services = k8s
        .get(&svc_path)
        .map(|v| v["items"].as_array().cloned().unwrap_or_default())
        .unwrap_or_default();

    let items: Vec<Value> = deps
        .iter()
        .map(|d| {
            let name = d["metadata"]["name"].as_str().unwrap_or_default();
            let svc = services
                .iter()
                .find(|s| s["metadata"]["name"].as_str() == Some(name));
            xray_shape(d, svc)
        })
        .collect();

    Response::ok(json!({
        "items": items,
        "shim": {
            // xray-wasm 是命令式 wasm（自己监听 SOCKS5），必须跑在 wasmtime 运行时上
            "runtimeClass": "wasmtime-wasip2",
            "requiredNodeLabel": "wasm.sh/wasmtime=true",
        },
    }))
}

/// 从 `vless://` 分享链接里解析出隧道参数。
///
/// 这样用户可以直接把 xray-deploy 输出的链接粘进来，而不必手工拆 5 个字段
/// （字段拆错的表现是握手失败，且错误信息很不直观）。
pub fn parse_vless_link(link: &str) -> Option<Value> {
    let rest = link.trim().strip_prefix("vless://")?;
    // 先剥掉 #fragment（节点名）：否则最后一个 query 参数会被它污染，
    // 表现为 flow 之类解析成 "xtls-rprx-vision#Xray" —— 握手就会失败。
    let rest = rest.split('#').next().unwrap_or(rest);
    let (uuid, rest) = rest.split_once('@')?;
    let (authority, query) = rest.split_once('?')?;
    let mut out = json!({ "uuid": uuid, "server": authority });
    for kv in query.split('&') {
        let Some((k, v)) = kv.split_once('=') else { continue };
        let v = crate::http_io::percent_decode(v);
        match k {
            "pbk" => out["publicKey"] = json!(v),
            "sid" => out["shortId"] = json!(v),
            "sni" => out["sni"] = json!(v),
            "flow" => out["flow"] = json!(v),
            _ => {}
        }
    }
    Some(out)
}

pub fn xray_create(req: &Request) -> Response {
    let body = match req.json_body() {
        Ok(b) => b,
        Err(m) => return Response::fail(400, m),
    };
    let (cfg, k8s) = client();

    // 支持直接粘贴 vless:// 链接（`server`/`uuid` 等字段也就自动补全）
    let body = match body["vlessLink"].as_str().filter(|s| !s.is_empty()) {
        Some(link) => {
            let Some(parsed) = parse_vless_link(link) else {
                return Response::fail(400, "vlessLink 解析失败，期望形如 vless://<uuid>@<ip:port>?pbk=...&sid=...&sni=...");
            };
            let mut merged = body.clone();
            for (k, v) in parsed.as_object().cloned().unwrap_or_default() {
                if merged.get(&k).map(|x| x.is_null()).unwrap_or(true) {
                    merged[k] = v;
                }
            }
            merged
        }
        None => body,
    };

    let name = match required_str(&body, "name") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let namespace = body["namespace"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(&cfg.default_namespace)
        .to_string();
    let server = match required_str(&body, "server") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let uuid = match required_str(&body, "uuid") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let public_key = match required_str(&body, "publicKey") {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !is_dns1123_label(&name) || !is_dns1123_label(&namespace) {
        return Response::fail(400, "name / namespace 必须是合法 DNS-1123 名称");
    }

    // ⚠️ 认证不是可选项：绑非回环地址 + 无认证 = 开放代理（上游文档明确要求）
    let socks_user = match required_str(&body, "socksUser") {
        Ok(v) => v,
        Err(_) => {
            return Response::fail(
                400,
                "必须提供 socksUser/socksPass：绑 0.0.0.0 而无认证的 SOCKS5 就是开放代理",
            )
        }
    };
    let socks_pass = match required_str(&body, "socksPass") {
        Ok(v) => v,
        Err(_) => return Response::fail(400, "必须提供 socksPass"),
    };

    let short_id = body["shortId"].as_str().unwrap_or("").to_string();
    let sni = body["sni"].as_str().unwrap_or("").to_string();
    let client_ver = body["clientVer"].as_str().unwrap_or("").to_string();
    let listen = body["listen"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("0.0.0.0:1080")
        .to_string();
    let port = match listen.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()) {
        Some(p) if p > 0 => p,
        _ => return Response::fail(400, format!("listen 必须形如 0.0.0.0:1080，收到：{listen}")),
    };
    let replicas = body["replicas"].as_i64().unwrap_or(2).clamp(1, 100);
    let image = body["image"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("docker.io/k3s-wasm/xray-wasm-cli:v0.1.0")
        .to_string();
    let runtime_class = body["runtimeClassName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("wasmtime-wasip2")
        .to_string();
    let secret_name = body["secretName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(&name)
        .to_string();

    let labels = json!({
        "app.kubernetes.io/name": name,
        "app.kubernetes.io/part-of": XRAY_PART_OF,
        "app.kubernetes.io/managed-by": MANAGED_BY,
    });

    // 1) 凭据放 Secret。也可以传 secretName 引用一个你**预先建好**的 Secret，
    //    这样控制台就不需要 secrets 的写权限（见 deploy/base/kube-api-proxy.yaml 的说明）。
    if body["secretName"].as_str().filter(|s| !s.is_empty()).is_none() {
        let mut string_data = serde_json::Map::new();
        string_data.insert("XT_SERVER".into(), json!(server));
        string_data.insert("XT_UUID".into(), json!(uuid));
        string_data.insert("XT_PBK".into(), json!(public_key));
        string_data.insert("XT_SID".into(), json!(short_id));
        string_data.insert("XT_SNI".into(), json!(sni));
        string_data.insert("XT_SOCKS_USER".into(), json!(socks_user));
        string_data.insert("XT_SOCKS_PASS".into(), json!(socks_pass));
        if !client_ver.is_empty() {
            string_data.insert("XT_CLIENT_VER".into(), json!(client_ver));
        }
        let secret = json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": secret_name, "namespace": namespace, "labels": labels },
            "type": "Opaque",
            "stringData": Value::Object(string_data),
        });
        if let Err(e) = k8s.post(&format!("/api/v1/namespaces/{namespace}/secrets"), &secret) {
            return Response::fail(
                e.http_status(),
                format!(
                    "创建 Secret 失败：{}。两条出路：① 给控制台加上 secrets 写权限；\
                     ② 自己先建好 Secret，然后在表单里填「已有 Secret 名称」（这样控制台只引用、不创建）。",
                    e.message()
                ),
            );
        }
    }

    // 2) 工作负载：纯 wasm，跑在 wasmtime shim 上
    let deployment = json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": labels,
            // 非敏感的展示字段放 annotation：凭据只在 Secret 里，
            // 这样列表页不需要 secrets 的读权限就能显示「连的是哪个服务端」。
            "annotations": {
                "k3s-wasm/server": server,
                "k3s-wasm/sni": sni,
                "k3s-wasm/short-id": short_id,
                "k3s-wasm/listen": listen,
            },
        },
        "spec": {
            "replicas": replicas,
            "selector": { "matchLabels": { "app.kubernetes.io/name": name } },
            "template": {
                "metadata": { "labels": labels },
                "spec": {
                    "runtimeClassName": runtime_class,
                    "containers": [{
                        "name": "proxy",
                        "image": image,
                        "imagePullPolicy": "IfNotPresent",
                        // 全部配置走环境变量（来自 Secret），args 里不放凭据：
                        // kubectl describe pod 会把 args 原样打印出来。
                        "env": [
                            { "name": "XT_LISTEN", "value": listen },
                            // 上游已知限制：一次只处理一条连接，探针会占用一次机会
                            { "name": "XT_HANDSHAKE_TIMEOUT", "value": "15" }
                        ],
                        "envFrom": [{ "secretRef": { "name": secret_name } }],
                        "ports": [{ "name": "socks", "containerPort": port, "protocol": "TCP" }],
                        // 刻意不配 livenessProbe：单连接模型下它更容易误杀
                        "startupProbe": {
                            "tcpSocket": { "port": port },
                            "periodSeconds": 2, "failureThreshold": 30
                        },
                        "readinessProbe": {
                            "tcpSocket": { "port": port },
                            "periodSeconds": 15, "failureThreshold": 6
                        },
                        "resources": {
                            "requests": { "cpu": "10m", "memory": "32Mi" },
                            "limits": { "memory": "128Mi" }
                        }
                    }]
                }
            }
        }
    });
    if let Err(e) = k8s.post(&format!("/apis/apps/v1/namespaces/{namespace}/deployments"), &deployment) {
        return from_err(e);
    }

    // 3) ClusterIP Service（刻意不用 NodePort/LoadBalancer）
    let service = json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": { "name": name, "namespace": namespace, "labels": labels },
        "spec": {
            "type": "ClusterIP",
            "selector": { "app.kubernetes.io/name": name },
            "ports": [{ "name": "socks", "port": port, "targetPort": port, "protocol": "TCP" }]
        }
    });
    if let Err(e) = k8s.post(&format!("/api/v1/namespaces/{namespace}/services"), &service) {
        return from_err(e);
    }

    // 4) NetworkPolicy：认证管「谁能用」，它管「谁能连」。
    //
    // ⚠️ 只做 ingress 限制，**默认不做 egress 收紧** —— 这是实测结论，不是偷懒：
    //   把 egress 收紧成「只允许到服务端 IP:端口」之后，隧道会卡在 SOCKS5 协商阶段
    //   （curl 超时、客户端日志停在「新连接」没有下文）；删掉该策略后立刻恢复正常。
    //   在 Cilium + kube-proxy 替换 + 服务端就在同一节点 IP 上这个组合下复现稳定。
    //   想要 egress 收紧的话：自行加上并**务必复测隧道**（deploy/xray-wasm/networkpolicy.yaml
    //   里留了注释掉的模板）。
    let netpol = json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": { "name": name, "namespace": namespace, "labels": labels },
        "spec": {
            "podSelector": { "matchLabels": { "app.kubernetes.io/name": name } },
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{ "podSelector": {} }],
                "ports": [{ "protocol": "TCP", "port": port }]
            }]
        }
    });
    if let Err(e) = k8s.post(&format!("/apis/networking.k8s.io/v1/namespaces/{namespace}/networkpolicies"), &netpol) {
        // NetworkPolicy 失败不算致命，但要如实告诉用户
        return Response::ok(json!({
            "created": true, "namespace": namespace, "name": name,
            "socksEndpoint": format!("{name}.{namespace}.svc.cluster.local:{port}"),
            "warning": format!("Deployment/Service 已建，但 NetworkPolicy 创建失败：{}", e.message()),
        }));
    }

    Response::ok(json!({
        "created": true,
        "namespace": namespace,
        "name": name,
        "secret": secret_name,
        "socksEndpoint": format!("{name}.{namespace}.svc.cluster.local:{port}"),
        "portForward": format!("kubectl -n {namespace} port-forward svc/{name} 1080:{port}"),
        "usage": format!("curl --proxy-user '<user>:<pass>' --proxy socks5h://{name}.{namespace}.svc.cluster.local:{port} https://example.com"),
        "note": "SOCKS5 必须带认证（表单已强制）；Service 刻意是 ClusterIP，不要改成 NodePort。",
    }))
}

pub fn xray_scale(req: &Request, ns: &str, name: &str) -> Response {
    let body = match req.json_body() {
        Ok(b) => b,
        Err(m) => return Response::fail(400, m),
    };
    let (_cfg, k8s) = client();
    if !is_dns1123_label(ns) || !is_dns1123_label(name) {
        return Response::fail(400, "命名空间或名称不合法");
    }
    let Some(replicas) = body["replicas"].as_i64() else {
        return Response::fail(400, "缺少 replicas（整数）");
    };
    let replicas = replicas.clamp(0, 100);
    from_result(
        k8s.merge_patch(
            &format!("/apis/apps/v1/namespaces/{ns}/deployments/{name}"),
            &json!({ "spec": { "replicas": replicas } }),
        )
        .map(|d| shape_deployment(&d)),
    )
}

pub fn xray_delete(_req: &Request, ns: &str, name: &str) -> Response {
    let (_cfg, k8s) = client();
    if !is_dns1123_label(ns) || !is_dns1123_label(name) {
        return Response::fail(400, "命名空间或名称不合法");
    }

    // 面板创建过的对象都要清掉：Deployment / Service / NetworkPolicy / Secret
    // （删不存在的对象是幂等的，403 之类的真实错误则如实上抛）
    let mut deleted = Vec::new();
    for (path, what) in [
        (
            format!("/apis/apps/v1/namespaces/{ns}/deployments/{name}"),
            "deployment",
        ),
        (format!("/api/v1/namespaces/{ns}/services/{name}"), "service"),
        (
            format!("/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/{name}"),
            "networkpolicy",
        ),
        (
            format!("/api/v1/namespaces/{ns}/secrets/{name}"),
            "secret",
        ),
    ] {
        match k8s.delete(&path) {
            Ok(_) => deleted.push(what),
            Err(e) if e.is_not_found() => {}
            // 没有 secrets 权限时不该整体失败：其余对象已经删掉了，把情况说清楚
            Err(e) if e.is_forbidden() && what == "secret" => {
                deleted.push("secret(跳过：无权限)");
            }
            Err(e) => return from_err(e),
        }
    }
    Response::ok(json!({ "deleted": deleted, "namespace": ns, "name": name }))
}


// ════════════════════════════════════════════════════════════════════
// 自动生成隧道参数
// ════════════════════════════════════════════════════════════════════
//
// 生成一整套「客户端 + 服务端」配套参数：
//   · REALITY 的 X25519 密钥对（私钥给服务端，公钥 pbk 给客户端）
//   · UUID、shortId、SOCKS5 用户名/密码
//   · 可直接粘贴的服务端 config.json、客户端 config.json、vless:// 链接
//
// 随机数用宿主提供的 wasi:random（不引 getrandom/rand，组件体积与依赖都更小）。
// 私钥只在响应里出现一次，控制台不落盘、不记录。

fn random_bytes(n: usize) -> Vec<u8> {
    wasi::random::random::get_random_bytes(n as u64)
}

/// base64url（无 padding）—— Xray 的 x25519 密钥就是这个编码
fn b64url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// 16 字节 → UUIDv4 字符串（设置版本位与变体位，符合 RFC 4122）
fn uuid_from_bytes(b: &[u8]) -> String {
    let mut x = [0u8; 16];
    for (i, v) in b.iter().take(16).enumerate() {
        x[i] = *v;
    }
    x[6] = (x[6] & 0x0f) | 0x40; // version 4
    x[8] = (x[8] & 0x3f) | 0x80; // variant 10xx
    let h: String = x.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]
    )
}

fn bytes_to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// REALITY 密钥对 → (私钥 base64url, 公钥 base64url)
fn reality_keypair() -> (String, String) {
    let raw = random_bytes(32);
    let mut secret_bytes = [0u8; 32];
    secret_bytes.copy_from_slice(&raw[..32]);
    let secret = x25519_dalek::StaticSecret::from(secret_bytes);
    let public = x25519_dalek::PublicKey::from(&secret);
    (
        b64url(&secret.to_bytes()),
        b64url(public.as_bytes()),
    )
}

/// 把 `host:port` 拆开；没有端口时给默认值
fn split_host_port(s: &str, default_port: u16) -> (String, u16) {
    match s.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => match p.parse::<u16>() {
            Ok(port) if port > 0 => (h.to_string(), port),
            _ => (s.to_string(), default_port),
        },
        _ => (s.to_string(), default_port),
    }
}

fn build_vless_link(uuid: &str, server: &str, pbk: &str, sid: &str, sni: &str, name: &str) -> String {
    // 参数顺序尽量贴近官方客户端导出的样子；spx 固定 /（REALITY 的回落到路径）
    format!(
        "vless://{uuid}@{server}?encryption=none&type=tcp&security=reality&pbk={pbk}&fp=chrome&sni={sni}&sid={sid}&spx=%2F&flow=xtls-rprx-vision#{name}"
    )
}

fn build_server_config(uuid: &str, private_key: &str, sid: &str, sni: &str, port: u16) -> Value {
    json!({
        "log": { "loglevel": "warning" },
        "inbounds": [{
            "listen": "0.0.0.0",
            "port": port,
            "protocol": "vless",
            "settings": {
                "clients": [{ "id": uuid, "flow": "xtls-rprx-vision" }],
                "decryption": "none"
            },
            "streamSettings": {
                "network": "tcp",
                "security": "reality",
                "realitySettings": {
                    // dest 就是「回落站点」：未通过 REALITY 认证的探测流量会被转发到这里
                    "dest": format!("{sni}:443"),
                    "serverNames": [sni],
                    "privateKey": private_key,
                    "shortIds": [sid]
                }
            }
        }],
        "outbounds": [{ "protocol": "freedom" }]
    })
}

fn build_client_config(uuid: &str, server: &str, pbk: &str, sid: &str, sni: &str) -> Value {
    let (host, port) = split_host_port(server, 443);
    json!({
        "log": { "loglevel": "warning" },
        "inbounds": [{
            "listen": "127.0.0.1", "port": 1080, "protocol": "socks",
            "settings": { "auth": "noauth", "udp": false }
        }],
        "outbounds": [{
            "protocol": "vless",
            "settings": { "vnext": [{
                "address": host, "port": port,
                "users": [{ "id": uuid, "encryption": "none", "flow": "xtls-rprx-vision" }]
            }] },
            "streamSettings": {
                "network": "tcp",
                "security": "reality",
                "realitySettings": {
                    "serverName": sni, "fingerprint": "chrome",
                    "publicKey": pbk, "shortId": sid, "spiderX": "/"
                }
            }
        }]
    })
}

/// POST /api/xray/generate
/// body（都可省）：{ "server": "1.2.3.4:443", "sni": "www.cloudflare.com", "name": "tokyo" }
pub fn xray_generate(req: &Request) -> Response {
    let body = req.json_body().unwrap_or_else(|_| json!({}));
    let server = body["server"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("203.0.113.10:443")
        .to_string();
    let sni = body["sni"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("www.cloudflare.com")
        .to_string();
    let name = body["name"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("k3s-wasm")
        .to_string();

    let (private_key, public_key) = reality_keypair();
    let uuid = uuid_from_bytes(&random_bytes(16));
    let short_id = bytes_to_hex(&random_bytes(8));
    // SOCKS5 认证：默认用户名 k3s，密码 18 字节随机（base64url，无易混字符）
    let socks_user = body["socksUser"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("k3s")
        .to_string();
    let socks_pass = b64url(&random_bytes(18));

    let (_, port) = split_host_port(&server, 443);
    let link = build_vless_link(&uuid, &server, &public_key, &short_id, &sni, &name);
    let server_config = build_server_config(&uuid, &private_key, &short_id, &sni, port);
    let client_config = build_client_config(&uuid, &server, &public_key, &short_id, &sni);

    Response::ok(json!({
        // 直接填进表单的字段
        "name": name,
        "server": server,
        "uuid": uuid,
        "publicKey": public_key,
        "shortId": short_id,
        "sni": sni,
        "socksUser": socks_user,
        "socksPass": socks_pass,
        // 私钥只在这里出现一次：它属于**服务端**，控制台不保存
        "privateKey": private_key,
        "vlessLink": link,
        "serverConfig": server_config,
        "clientConfig": client_config,
        "notes": [
            "私钥只在本响应里出现一次，控制台不保存、不写入集群；请立刻粘到服务端配置里并妥善保管。",
            "服务端：把 serverConfig 覆盖到服务器的 config.json（或只取其 inbound），重启 xray。",
            "客户端：vlessLink 可直接导入官方客户端；clientConfig 是官方 Xray 的等价配置，便于先验证服务端。",
            "SOCKS5 用户名/密码只用于**集群内**这条隧道的入口认证，与上面服务端配置无关。",
            "若服务端设了 minClientVer/maxClientVer，客户端上报版本需落在区间内（默认 26.3.27，可用 clientVer 覆盖）。"
        ]
    }))
}

// ════════════════════════════════════════════════════════════════════
// 单测（纯函数，原生可跑）
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn req(path: &str, query: &str) -> Request {
        Request {
            method: "GET".into(),
            path: path.into(),
            query: query.into(),
            headers: vec![],
            body: vec![],
        }
    }

    #[test]
    fn dns1123_validation() {
        assert!(is_dns1123_label("k3s-wasm"));
        assert!(is_dns1123_label("a1"));
        assert!(!is_dns1123_label("Bad_Name"));
        assert!(!is_dns1123_label("-leading"));
        assert!(!is_dns1123_label("trailing-"));
        assert!(!is_dns1123_label(""));
        assert!(!is_dns1123_label(&"x".repeat(64)));
    }

    #[test]
    fn wasm_runtime_detection() {
        assert!(runtime_is_wasm("wasmtime-wasip2"));
        assert!(runtime_is_wasm("wasmtime-spin-v2"));
        assert!(runtime_is_wasm("wasmtime"));
        assert!(!runtime_is_wasm("runc"));
        assert!(!runtime_is_wasm(""));
    }

    #[test]
    fn spinapp_paths() {
        assert_eq!(
            spinapp_path(SPINAPP_DEFAULT_APIVERSION, None),
            "/apis/core.spinkube.dev/v1alpha1/spinapps"
        );
        assert_eq!(
            spinapp_path(SPINAPP_DEFAULT_APIVERSION, Some("k3s-wasm")),
            "/apis/core.spinkube.dev/v1alpha1/namespaces/k3s-wasm/spinapps"
        );
    }

    #[test]
    fn xray_selector_has_both_labels() {
        let s = xray_selector();
        assert!(s.contains("app.kubernetes.io/part-of=xray-wasm"));
        assert!(s.contains("app.kubernetes.io/managed-by=k3s-wasm-ui"));
    }

    #[test]
    fn shape_node_reads_wasm_labels_and_readiness() {
        let node = json!({
            "metadata": {"name": "n1", "labels": {"wasm.sh/spin": "true"}},
            "status": {
                "conditions": [{"type": "Ready", "status": "True"}],
                "nodeInfo": {"architecture": "arm64", "operatingSystem": "linux",
                             "kubeletVersion": "v1.33.1+k3s1", "containerRuntimeVersion": "containerd://2.0"},
                "capacity": {"cpu": "8", "memory": "16Gi", "pods": "110"}
            }
        });
        let out = shape_node(&node);
        assert_eq!(out["name"], "n1");
        assert_eq!(out["ready"], true);
        assert_eq!(out["wasm"]["spin"], true);
        assert_eq!(out["wasm"]["wasmtime"], false);
    }

    #[test]
    fn shape_pod_computes_ready_and_restarts() {
        let pod = json!({
            "metadata": {"name": "p1", "namespace": "k3s-wasm"},
            "spec": {"nodeName": "n1", "runtimeClassName": "wasmtime-wasip2",
                     "containers": [{"name": "ui", "image": "img:1"}]},
            "status": {"phase": "Running",
                       "containerStatuses": [{"name": "ui", "ready": true, "restartCount": 3}]}
        });
        let out = shape_pod(&pod);
        assert_eq!(out["ready"], "1/1");
        assert_eq!(out["restarts"], 3);
        assert_eq!(out["isWasm"], true);
    }

    #[test]
    fn filter_items_by_ns_keeps_only_requested() {
        let list = json!({"items": [
            {"metadata": {"namespace": "a"}},
            {"metadata": {"namespace": "b"}}
        ]});
        let filtered = filter_items_by_ns(list, "a");
        assert_eq!(filtered["items"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn b64url_matches_known_vector_and_strips_padding() {
        // RFC 4648 的 base64url 向量 + 「无 padding」这一点（Xray 的密钥就是这个格式）
        assert_eq!(b64url(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64url(&[0xff, 0xef]), "_-8");
        assert!(!b64url(&[1, 2, 3]).contains('='));
    }

    #[test]
    fn uuid_from_bytes_sets_version_and_variant() {
        let u = uuid_from_bytes(&[0u8; 16]);
        assert_eq!(u.len(), 36);
        assert_eq!(u.chars().filter(|c| *c == '-').count(), 4);
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(
            [parts[0].len(), parts[1].len(), parts[2].len(), parts[3].len(), parts[4].len()],
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "版本位应为 4，实际 {}", parts[2]);
        assert!(
            matches!(parts[3].chars().next().unwrap(), '8' | '9' | 'a' | 'b'),
            "变体位应为 10xx，实际 {}",
            parts[3]
        );
    }

    #[test]
    fn generated_link_round_trips_through_parser() {
        // 生成 → 解析 必须自洽：面板正是靠这个链路把链接拆回字段
        let link = build_vless_link(
            "11111111-2222-3333-4444-555555555555",
            "203.0.113.10:443",
            "PUBKEY",
            "9f1c2a3b",
            "www.cloudflare.com",
            "tokyo",
        );
        let v = parse_vless_link(&link).expect("自己生成的链接必须能被自己解析");
        assert_eq!(v["uuid"], "11111111-2222-3333-4444-555555555555");
        assert_eq!(v["server"], "203.0.113.10:443");
        assert_eq!(v["publicKey"], "PUBKEY");
        assert_eq!(v["shortId"], "9f1c2a3b");
        assert_eq!(v["sni"], "www.cloudflare.com");
        assert_eq!(v["flow"], "xtls-rprx-vision");
    }

    #[test]
    fn server_config_carries_private_key_and_dest() {
        let cfg = build_server_config("uuid-1", "PRIVKEY", "sid1", "www.cloudflare.com", 443);
        let ib = &cfg["inbounds"][0];
        assert_eq!(ib["settings"]["clients"][0]["id"], "uuid-1");
        assert_eq!(ib["settings"]["clients"][0]["flow"], "xtls-rprx-vision");
        assert_eq!(ib["streamSettings"]["realitySettings"]["privateKey"], "PRIVKEY");
        assert_eq!(ib["streamSettings"]["realitySettings"]["dest"], "www.cloudflare.com:443");
        assert_eq!(ib["port"], 443);
    }

    #[test]
    fn split_host_port_handles_edge_cases() {
        assert_eq!(split_host_port("1.2.3.4:8443", 443), ("1.2.3.4".into(), 8443));
        assert_eq!(split_host_port("1.2.3.4", 443), ("1.2.3.4".into(), 443));
        assert_eq!(split_host_port("1.2.3.4:abc", 443), ("1.2.3.4:abc".into(), 443));
    }

    #[test]
    fn parse_vless_link_extracts_fields() {
        let link = "vless://11111111-2222-3333-4444-555555555555@203.0.113.10:443?type=tcp&security=reality&pbk=PUBKEY&sid=9f1c2a3b&sni=www.amazon.com&fp=chrome&flow=xtls-rprx-vision#Xray";
        let v = parse_vless_link(link).expect("应能解析");
        assert_eq!(v["uuid"], "11111111-2222-3333-4444-555555555555");
        assert_eq!(v["server"], "203.0.113.10:443");
        assert_eq!(v["publicKey"], "PUBKEY");
        assert_eq!(v["shortId"], "9f1c2a3b");
        assert_eq!(v["sni"], "www.amazon.com");
        assert!(parse_vless_link("http://x").is_none());
    }

    #[test]
    fn required_str_rejects_empty() {
        assert!(required_str(&json!({"x": "  "}), "x").is_err());
        assert!(required_str(&json!({}), "x").is_err());
        assert_eq!(required_str(&json!({"x": " y "}), "x").unwrap(), "y");
    }

    #[test]
    fn ns_or_default_falls_back() {
        let cfg = Config {
            proxy_url: "http://x".into(),
            proxy_url_source: crate::config::Source::Builtin,
            default_namespace: "k3s-wasm".into(),
        };
        assert_eq!(ns_or_default(&cfg, &req("/api/pods", "")), "k3s-wasm");
        assert_eq!(ns_or_default(&cfg, &req("/api/pods", "namespace=other")), "other");
    }
}
