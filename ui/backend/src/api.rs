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

pub(crate) fn client() -> (Config, K8s) {
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

pub(crate) fn label_bool(labels: &Value, key: &str) -> bool {
    labels
        .get(key)
        .and_then(Value::as_str)
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// 运行时**统一三分类**：`wasm` / `gpu` / `native`。
///
/// 判据只有 runtimeClassName（或 RuntimeClass 的 handler）—— 靠镜像名猜不可靠：
/// `containerd-shim-*` 这类镜像名什么都看不出来，而 vm 里跑的普通 Go 二进制
/// 也可能出现在 wasm 命名空间里。
///
/// 整个控制台（概览统计、节点、运行时列表、工作负载、Pod、拓扑）都用这一个函数，
/// 避免各视图各写一套「什么算 wasm」的判断而慢慢漂移。
pub fn runtime_category(name_or_handler: &str) -> &'static str {
    // ⚠️ 这里的名字清单是**实测**出来的：k3s 会自动为它探测到的每个 shim 建一个
    // RuntimeClass，本集群上真实存在 12 个。只匹配 "wasm"/"spin" 会把
    // lunatic / slight / wws 这三个货真价实的 WASM 运行时误判成原生 ——
    // 它们都是 runwasi 系 shim（WASM 运行时），名字里偏偏没有 "wasm"。
    const WASM_HINTS: [&str; 9] = [
        "wasm", "spin", "wasi", "lunatic", "wasmedge", "wasmer", "slight", "wws", "wamr",
    ];
    let s = name_or_handler.to_ascii_lowercase();
    if WASM_HINTS.iter().any(|h| s.contains(h)) {
        "wasm"
    } else if s.contains("nvidia") || s.contains("gpu") || s.contains("cuda") || s.contains("mig-")
    {
        "gpu"
    } else {
        // 没写 runtimeClassName 的普通 Pod（空串）也归到这里 —— 那就是 runc 默认路径；
        // crun / kata / runc 这些容器运行时同理（kata 是 VM 隔离，但不是 wasm 也不是 GPU）。
        "native"
    }
}

/// 判断一个 Pod 模板是不是 wasm 工作负载（三分类的薄封装，保留旧调用点）。
fn runtime_is_wasm(runtime_class: &str) -> bool {
    runtime_category(runtime_class) == "wasm"
}

/// 节点是否有 GPU、有几块。
///
/// 三个来源都看：device plugin 报的 capacity/allocatable（最可信）、
/// NVIDIA 的节点标签、以及 PCI 设备标签（没装 device plugin 时也能看出来）。
pub(crate) fn node_gpu(node: &Value) -> (bool, i64) {
    let labels = &node["metadata"]["labels"];
    let cap = &node["status"]["capacity"];
    let mut count = 0i64;
    for key in ["nvidia.com/gpu", "amd.com/gpu", "gpu.intel.com/i915"] {
        if let Some(v) = cap[key].as_str().and_then(|s| s.parse::<i64>().ok()) {
            count += v;
        }
    }
    let present = count > 0
        || label_bool(labels, "nvidia.com/gpu.present")
        || !labels["nvidia.com/gpu.product"].is_null()
        || label_bool(labels, "feature.node.kubernetes.io/pci-10de.present");
    (present, count)
}

fn shape_node(node: &Value) -> Value {
    let meta = &node["metadata"];
    let status = &node["status"];
    let labels = meta["labels"].clone();
    let (gpu_present, gpu_count) = node_gpu(node);
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
        // GPU 也放在这里，和 wasm 一样属于「这个节点能跑什么」的运行时能力
        "gpu": { "present": gpu_present, "count": gpu_count },
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
        // 三分类的统一字段（wasm / gpu / native）——前端所有视图都读它
        "runtimeCategory": runtime_category(runtime_class),
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
        "runtimeCategory": runtime_category(runtime_class),
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
        "runtimeCategory": "wasm",
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

pub fn health(req: &Request) -> Response {
    let (cfg, _k8s) = client();
    // 认证配置也在这里暴露（不含任何秘密）：登录页/排障时一眼看出 RP ID 与
    // 「会话密钥/注册码是否配好」，省得靠猜。
    let auth_info = crate::auth::describe(req, &crate::auth::AuthConfig::load(&cfg.default_namespace));
    Response::ok(json!({
        "status": "ok",
        "component": "k3s-wasm-ui",
        "auth": auth_info,
        // 版本号来自 Cargo.toml（编译期常量），前端把它显示在侧栏
        "version": env!("CARGO_PKG_VERSION"),
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
    // 三类 Pod 数：统一走 runtime_category，避免和别的视图口径不一致
    let category_pods = |cat: &str| {
        pod_items
            .iter()
            .filter(|p| runtime_category(p["spec"]["runtimeClassName"].as_str().unwrap_or("")) == cat)
            .count()
    };
    let wasm_pods = category_pods("wasm");

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
        // 运行时三分类（wasm / gpu / native）的 Pod 口径统计
        "runtimeCategories": {
            "wasm": wasm_pods,
            "gpu": category_pods("gpu"),
            "native": category_pods("native"),
            "total": pod_items.len(),
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
            let name = rc["metadata"]["name"].as_str().unwrap_or("");
            // 分类把 handler 和名字一起看：k3s 自动生成的 RuntimeClass 里两者通常一致，
            // 但自建的（例如 nvidia）可能只在其中一个上体现。
            let category = if runtime_category(handler) != "native" {
                runtime_category(handler)
            } else {
                runtime_category(name)
            };
            let is_wasm = category == "wasm";
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
                // 三分类：wasm / gpu / native（前端用同一套徽章与过滤器）
                "category": category,
                "nodeSelector": node_selector,
                // wasm 运行时如果**没有** nodeSelector，就无法从节点标签判断它到底装没装
                // （k3s 会给 wasmedge/wasmer 这些也建 RuntimeClass，但节点上未必有二进制）。
                // 这种情况返回 null，让前端显示「无法判定」，而不是编一个数字出来。
                "capableNodes": if selectorless && is_wasm { Value::Null } else { json!(capable) },
                "selectorless": selectorless,
                "mismatch": !selectorless && capable == 0,
                // 旧字段名保留一版，避免前端/脚本还在读它
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

/// GET /api/images?wasmOnly=1
///
/// 列出集群里**实际在用**的镜像（从 Pod 规格聚合），用作表单下拉的建议值。
/// 为什么不列 containerd 的完整镜像列表：CRI 没有把这个暴露成 k8s API，
/// 组件又只能通过 kube-api-proxy 说话；而"已经在跑的镜像"恰好是最有用的那批
/// —— 它们一定已经被节点拉取/导入过，选它不会遇到 pull 失败。
pub fn images(req: &Request) -> Response {
    let (_cfg, k8s) = client();
    let wasm_only = req.query_param("wasmOnly").as_deref() != Some("0");
    let pods = match k8s.get("/api/v1/pods?limit=2000") {
        Ok(v) => v,
        Err(e) => return from_err(e),
    };

    // image -> (总次数, wasm 次数, 运行时集合)
    let mut agg: std::collections::BTreeMap<String, (i64, i64, std::collections::BTreeSet<String>)> =
        std::collections::BTreeMap::new();
    for pod in pods["items"].as_array().cloned().unwrap_or_default() {
        let rc = pod["spec"]["runtimeClassName"].as_str().unwrap_or("");
        let is_wasm = runtime_is_wasm(rc);
        // 只看运行中的 Pod：已删除/失败的 Pod 里的镜像可能并不在节点上
        let phase = pod["status"]["phase"].as_str().unwrap_or("");
        if phase != "Running" {
            continue;
        }
        for c in pod["spec"]["containers"].as_array().cloned().unwrap_or_default() {
            if let Some(img) = c["image"].as_str() {
                if img.is_empty() {
                    continue;
                }
                let e = agg.entry(img.to_string()).or_insert((0, 0, Default::default()));
                e.0 += 1;
                if is_wasm {
                    e.1 += 1;
                    if !rc.is_empty() {
                        e.2.insert(rc.to_string());
                    }
                }
            }
        }
    }

    let mut items: Vec<Value> = agg
        .into_iter()
        .filter(|(_, (_, wasm, _))| !wasm_only || *wasm > 0)
        .map(|(image, (count, wasm, rcs))| {
            json!({
                "image": image,
                "count": count,
                "wasmCount": wasm,
                "runtimeClasses": rcs.into_iter().collect::<Vec<_>>(),
                "isWasm": wasm > 0,
            })
        })
        .collect();
    // wasm 工作负载用过的排前面，其次按出现次数
    items.sort_by(|a, b| {
        let key = |v: &Value| (v["isWasm"].as_bool().unwrap_or(false), v["count"].as_i64().unwrap_or(0));
        key(b).cmp(&key(a))
    });

    Response::ok(json!({
        "wasmOnly": wasm_only,
        "items": items,
        // 兜底建议：即使集群里暂时没有这些镜像，也让下拉有东西可选
        "defaults": [
            "docker.io/k3s-wasm/xray-wasm-cli:v0.1.0",
            "ghcr.io/harodggg/xray-wasm:v0.1.0",
        ],
        "hint": "列表来自集群里正在运行的 Pod 所用镜像（这些镜像一定已在节点上，选它不会遇到拉取失败）；也可以直接手输别的。",
    }))
}

/// GET /api/image-tags?repo=harodggg/xray-wasm
///
/// 从 GitHub releases 取 tag 列表，供镜像**版本**下拉使用（v0.1.0 / v0.2.0 / …）。
/// 取不到不算错误：返回空列表 + 原因，前端会退回到"集群里在跑的镜像 + 兜底建议"。
pub fn image_tags(req: &Request) -> Response {
    let repo = req
        .query_param("repo")
        .filter(|r| {
            !r.is_empty()
                && r.len() <= 120
                && r.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
                && r.matches('/').count() == 1
        })
        .unwrap_or_else(|| "harodggg/xray-wasm".to_string());

    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=30");
    let resp = match crate::k8s::fetch_absolute(&url) {
        Ok(r) => r,
        Err(e) => {
            return Response::ok(json!({
                "repo": repo,
                "tags": [],
                "error": e.message(),
                "hint": "拉不到 GitHub releases（可能宿主不允许出站 HTTPS，或网络受限）。下拉会自动退回到集群里在跑的镜像与兜底建议。",
            }))
        }
    };
    if resp.status != 200 {
        return Response::ok(json!({
            "repo": repo,
            "tags": [],
            "error": format!("GitHub API 返回 {}", resp.status),
            "hint": "私有仓库需要带 token（当前组件不带凭据）；公开仓库通常是限流。",
        }));
    }
    let releases: Value = match serde_json::from_slice(&resp.body) {
        Ok(v) => v,
        Err(e) => {
            return Response::ok(json!({
                "repo": repo, "tags": [], "error": format!("解析 GitHub 返回失败：{e}"),
            }))
        }
    };
    let tags: Vec<Value> = releases
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            let tag = r["tag_name"].as_str()?;
            Some(json!({
                "tag": tag,
                "name": r["name"].as_str().unwrap_or(tag),
                "prerelease": r["prerelease"].as_bool().unwrap_or(false),
                "publishedAt": r["published_at"],
            }))
        })
        .collect();

    Response::ok(json!({
        "repo": repo,
        "tags": tags,
        "hint": "tag 来自 GitHub releases；新发一个 release，下拉里就会多一项。",
    }))
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

    // 翻墙模式：方向与隧道相反 —— 入站 REALITY（公网入口），出站直连（走该节点自己的网络）。
    // 实现同样是 wasm（xray-wasm 的服务端模式），所以 isWasm 仍为 true。
    if ann["k3s-wasm/mode"].as_str() == Some("walljump") {
        let entry = ann["k3s-wasm/public-server"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ann["k3s-wasm/server"].as_str().unwrap_or(""));
        return json!({
            "kind": "xray-tunnel",
            "mode": "walljump",
            "name": dep["metadata"]["name"],
            "namespace": dep["metadata"]["namespace"],
            "replicas": spec["replicas"].as_i64().unwrap_or(1),
            "readyReplicas": status["readyReplicas"].as_i64().unwrap_or(0),
            "image": spec["template"]["spec"]["containers"].as_array()
                .and_then(|c| c.first()).map(|c| c["image"].clone()).unwrap_or(Value::Null),
            "runtimeClass": spec["template"]["spec"]["runtimeClassName"],
            "impl": "xray-wasm（wasm32-wasip2，REALITY 服务端）",
            "node": ann["k3s-wasm/node"],
            "createdAt": dep["metadata"]["creationTimestamp"],
            "tunnel": {
                "server": ann["k3s-wasm/server"],
                "sni": ann["k3s-wasm/sni"],
                "shortId": ann["k3s-wasm/short-id"],
                "listen": listen,
                "hasUuid": true,
            },
            "managedBy": MANAGED_BY,
            "isWasm": true,
            "direction": "ingress",
            "directionLabel": "入站（reality 入 → socket 出）",
            "egress": {
                "via": "socket（直连，走该节点自己的网络）",
                "protocol": "VLESS+REALITY → socket 直连目标（无第二跳）",
                "note": "客户端从公网连进来，出口就是这台节点的网络；未认证流量回落到 dest 站点",
            },
            "ingress": {
                "endpoint": entry,
                "exposure": shape_exposure(svc, port),
            },
            "usage": {
                "outbound": false,
                "inbound": true,
                "allowFrom": ann["k3s-wasm/allow-from"],
            },
        });
    }

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
        "mode": "tunnel",
        "impl": "xray-wasm（wasm32-wasip2，REALITY 客户端）",
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
        // 谁可以用：出站=集群内 Pod；入站=外部经节点IP（要求 Service 已对外暴露）
        "usage": {
            "outbound": true,
            "inbound": shape_exposure(svc, port)["public"].as_bool().unwrap_or(false),
            "allowFrom": ann["k3s-wasm/allow-from"],
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
            // xray-wasm 是命令式 wasm（自己监听端口），必须跑在 wasmtime 运行时上
            "runtimeClass": "wasmtime-wasip2",
            "requiredNodeLabel": "wasm.sh/wasmtime=true",
        },
        // 两种模式的语义（面板用的就是这份定义，避免前后端各写一套）
        "modes": [
            {
                "id": "walljump",
                "label": "翻墙（reality 入站 → socket 出站）",
                "impl": "xray-wasm（服务端模式 XT_MODE=server）",
                "runtimeClass": "wasmtime-wasip2",
                "entry": "NodePort（节点公网 IP:端口）",
                "who": "你自己：在国内直连这个公网入口，出口走该节点的网络（socket 直连，无第二跳）",
                "nodeRequirement": "需要装了 wasm 运行时（wasm.sh/wasmtime=true）且建议有公网 IP 的节点",
                "flowNote": "v0.6.0 起服务端实现 Vision，链接带 flow（抗 TLS-in-TLS）；入口若跑 ≤v0.5.x 旧镜像则需空 flow",
            },
            {
                "id": "tunnel",
                "label": "隧道（socket 入站 → reality 出站）",
                "impl": "xray-wasm（客户端模式）",
                "runtimeClass": "wasmtime-wasip2",
                "entry": "ClusterIP / NodePort（socket / SOCKS5 代理端口）",
                "who": "集群内的 Pod（或外部客户端）：从 socket（SOCKS5）进来，经远端 reality 出网",
                "nodeRequirement": "任意带 wasm 运行时的节点",
                "flowNote": "上游 stock Xray 与 xray-wasm ≥v0.6.0 都支持 flow=xtls-rprx-vision；仅 ≤v0.5.x 的服务端要求留空",
            },
        ],
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

    // 两种模式（语义由使用者定义）：
    //   walljump 翻墙：入站 REALITY → 出站 **直连**（国内直连公网入口，出口走该节点网络）
    //   tunnel   隧道：入站 SOCKS5  → 出站 REALITY（集群内流量经远端 REALITY 出网）
    //
    // 两者用的是**同一个 wasm 组件**（xray-wasm v0.4.0 起一个二进制两个方向）：
    //   翻墙 = `XT_MODE=server`（服务端），隧道 = 默认（客户端）。
    // 这也是为什么翻墙模式的 vless 链接**不带 flow**：该服务端未实现 XTLS-Vision 流控。
    let mode = body["mode"].as_str().unwrap_or("tunnel").to_lowercase();
    if mode == "walljump" || mode == "reality" || mode == "server" || mode == "ingress" {
        return xray_create_walljump(req, &body, &cfg, &k8s);
    }

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
    // 用途：cluster = 仅集群内（出站）；nodeport = 同时允许外部经「节点IP:nodePort」使用（入站）
    let expose = body["expose"].as_str().unwrap_or("cluster").to_lowercase();
    let external = expose == "nodeport" || expose == "external" || body["external"].as_bool() == Some(true);
    let node_port = body["nodePort"].as_u64().filter(|p| (30000..=32767).contains(p));
    // 外部可用时，NetworkPolicy 必须放行来源 —— 否则 Cilium 会把 NodePort 进来的流量丢掉。
    // 默认 0.0.0.0/0（等于公开），强烈建议填自己的出口 IP/CIDR。
    // 分享链接里用的对外地址（如 2.29.44.63:443，经 Traefik）；不填则链接用 XT_SERVER
    let public_server = body["publicServer"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();
    let allow_from = body["allowFrom"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("0.0.0.0/0")
        .to_string();
    // 节点地址从请求的 Host 头推出来（面板通常就是经节点IP访问的），用于拼对外连接串
    let req_host = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.split(':').next().unwrap_or("").to_string())
        .filter(|h| !h.is_empty());

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
                "k3s-wasm/expose": if external { "nodeport" } else { "cluster" },
                "k3s-wasm/public-server": public_server.as_str(),
                "k3s-wasm/allow-from": if external { allow_from.as_str() } else { "" },
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

    // 3) Service：默认 ClusterIP（只有集群内能用）；勾了"允许外部"才给 NodePort
    let mut svc_port = json!({ "name": "socks", "port": port, "targetPort": port, "protocol": "TCP" });
    if external {
        if let Some(np) = node_port {
            svc_port["nodePort"] = json!(np);
        }
    }
    let service = json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": { "name": name, "namespace": namespace, "labels": labels },
        "spec": {
            "type": if external { "NodePort" } else { "ClusterIP" },
            "selector": { "app.kubernetes.io/name": name },
            "ports": [svc_port]
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
    // 允许来源：集群内 Pod 一直允许；勾了外部用途再额外放行一个 CIDR。
    // ⚠️ 少了这条，外部经 NodePort 的连接会被 Cilium 按默认拒绝丢掉 ——
    //    现象是「Service 是 NodePort、端口也通，但 SOCKS5 就是没响应」。
    let mut ingress_from = vec![json!({ "podSelector": {} })];
    if external {
        ingress_from.push(json!({ "ipBlock": { "cidr": allow_from } }));
    }
    let netpol = json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": { "name": name, "namespace": namespace, "labels": labels },
        "spec": {
            "podSelector": { "matchLabels": { "app.kubernetes.io/name": name } },
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": ingress_from,
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

    let external_endpoint = if external {
        let np = node_port
            .map(|p| p.to_string())
            .unwrap_or_else(|| "<自动分配的 nodePort，见 kubectl get svc>".to_string());
        Some(match &req_host {
            Some(h) => format!("socks5h://{h}:{np}"),
            None => format!("socks5h://<节点IP>:{np}"),
        })
    } else {
        None
    };

    Response::ok(json!({
        "created": true,
        "mode": "tunnel",
        "impl": "xray-wasm（wasm32-wasip2，REALITY 客户端）",
        "direction": "socket 入站 → reality 出站",
        "namespace": namespace,
        "name": name,
        "secret": secret_name,
        // 出站用途（集群内 Pod）
        "socksEndpoint": format!("{name}.{namespace}.svc.cluster.local:{port}"),
        "portForward": format!("kubectl -n {namespace} port-forward svc/{name} 1080:{port}"),
        // 入站用途（外部经节点 IP）
        "expose": if external { "nodeport" } else { "cluster" },
        "externalEndpoint": external_endpoint,
        "allowFrom": if external { Value::String(allow_from.clone()) } else { Value::Null },
        "usage": format!("curl --proxy-user '<user>:<pass>' --proxy socks5h://{name}.{namespace}.svc.cluster.local:{port} https://example.com"),
        "note": if external {
            "已允许外部经节点 IP 使用：务必用强密码，并尽量把 allowFrom 收窄到你的出口 IP —— 否则这就是一个对全网开放的代理。"
        } else {
            "仅集群内可用（ClusterIP）。要用它翻墙/给本机用：改用途为 nodeport，或用 port-forward / SSH 隧道。"
        },
    }))
}

/// 翻墙模式：把 xray-wasm 以 **REALITY 服务端**形态部署（`XT_MODE=server`），
/// 入站 REALITY、出站直连 —— 也就是「国内直连这台公网入口，出去走这个节点的网络」。
///
/// 与隧道模式共用同一个 wasm 组件（v0.4.0 起一个二进制两个方向），差别只在：
///   ① `XT_MODE=server` + 服务端凭据（私钥/shortIds/serverNames/dest/users）
///   ② 入口是 **NodePort**（REALITY 监听在 pod 内 8443），不是 SOCKS5
///   ③ 生成的 vless 链接**不带 flow**（该服务端未实现 XTLS-Vision 流控，带了会被明确拒绝）
///
/// 入口为什么用 NodePort 而不是 hostPort：本集群命名空间带 PodSecurity `baseline`
/// 强制，`hostPort` 会被准入直接拒掉（实测 `violates PodSecurity "baseline:latest": hostPort`）。
fn xray_create_walljump(req: &Request, body: &Value, cfg: &Config, k8s: &K8s) -> Response {
    let name = match required_str(body, "name") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let namespace = body["namespace"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(&cfg.default_namespace)
        .to_string();
    if !is_dns1123_label(&name) || !is_dns1123_label(&namespace) {
        return Response::fail(400, "name / namespace 必须是合法 DNS-1123 名称");
    }

    // ── 节点：翻墙要跑在有公网 IP 的节点上，而且必须装了 wasm 运行时 ──
    let nodes = match k8s.get("/api/v1/nodes") {
        Ok(v) => v,
        Err(e) => return from_err(e),
    };
    let items = nodes["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return Response::fail(502, "读不到任何节点（集群 API 返回空）");
    }
    let want_node = body["node"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let node_obj = match want_node {
        Some(n) => match items.iter().find(|x| x["metadata"]["name"].as_str() == Some(n)) {
            Some(o) => o.clone(),
            None => {
                let avail: Vec<&str> = items
                    .iter()
                    .filter_map(|x| x["metadata"]["name"].as_str())
                    .collect();
                return Response::fail(400, format!("节点 {n} 不存在；可用节点：{}", avail.join(", ")));
            }
        },
        None => items
            .iter()
            .find(|x| node_ready(x))
            .cloned()
            .unwrap_or_else(|| items[0].clone()),
    };
    let node = node_obj["metadata"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    // 这个检查能省掉一轮「Pod 一直 Pending」的排查：翻墙模式跑的是 wasm 组件，
    // 而 RuntimeClass wasmtime-wasip2 自带 nodeSelector=wasm.sh/wasmtime=true。
    if !label_bool(&node_obj["metadata"]["labels"], "wasm.sh/wasmtime") {
        return Response::fail(
            400,
            format!(
                "节点 {node} 没有 wasm 运行时标签（wasm.sh/wasmtime=true），翻墙模式跑的是 wasm 组件，                 调度上去会一直 Pending。先在节点上跑 scripts/install-wasm-runtime.sh，或换一个节点。"
            ),
        );
    }

    // 对外地址：显式 publicHost > 请求 Host > 节点地址；跳过 localhost 这类只对本机有意义的
    let host_header = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.split(':').next().unwrap_or("").trim().to_string())
        .filter(|h| !h.is_empty() && h != "localhost" && h != "127.0.0.1" && h != "[::1]");
    let node_ip = node_addr(&node_obj, "InternalIP")
        .or_else(|| node_addr(&node_obj, "ExternalIP"))
        .unwrap_or_default();
    let public_host = body["publicHost"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or(host_header)
        .unwrap_or(node_ip);
    if public_host.is_empty() {
        return Response::fail(400, "无法确定对外地址：请显式传 publicHost（节点公网 IP 或域名）");
    }

    // ── 端口：容器内固定 8443；对外是 NodePort（30000-32767）──
    let inner_port: u16 = 8443;
    let node_port = body["nodePort"]
        .as_u64()
        .or_else(|| body["entryPort"].as_u64())
        .unwrap_or(30543);
    if !(30000..=32767).contains(&node_port) {
        return Response::fail(
            400,
            format!("NodePort 必须落在 30000-32767，收到 {node_port}（hostPort 被本集群 PodSecurity baseline 禁止）"),
        );
    }
    let node_port = node_port as u16;

    // 伪装站点（SNI）与回落到此站点的真实 TLS 站点 —— 未通过认证的探测者看到它的证书
    let sni = body["sni"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("www.cloudflare.com")
        .to_string();
    let dest = body["dest"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{sni}:443"));

    // 私钥可复用（接管已有服务端）；不传就现生成一对
    let (private_key, public_key) = match body["privateKey"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(pk) => match pubkey_from_private_key(pk) {
            Some(pubk) => (pk.to_string(), pubk),
            None => {
                return Response::fail(
                    400,
                    "privateKey 必须是 base64url（无 padding）的 32 字节 X25519 私钥",
                )
            }
        },
        None => reality_keypair(),
    };
    let uuid = body["uuid"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid_from_bytes(&random_bytes(16)));
    let short_id = body["shortId"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| bytes_to_hex(&random_bytes(8)));

    let image = body["image"]
        .as_str()
        .filter(|s| !s.is_empty())
        // v0.4.0 起同一个 wasm 模块同时支持客户端与服务端；v0.5.0 起客户端可 --no-flow
        .unwrap_or("docker.io/k3s-wasm/xray-wasm-cli:v0.6.0")
        .to_string();
    let runtime_class = body["runtimeClassName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("wasmtime-wasip2")
        .to_string();
    let replicas = body["replicas"].as_i64().unwrap_or(1).clamp(1, 5);
    // REALITY 入站按定义要对公网开放；放行来源仍可收窄（例如只放行你自己的出口 IP）
    let allow_from = body["allowFrom"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("0.0.0.0/0")
        .to_string();
    let secret_name = body["secretName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(&name)
        .to_string();
    let entry = format!("{public_host}:{node_port}");

    let labels = json!({
        "app.kubernetes.io/name": name,
        "app.kubernetes.io/part-of": XRAY_PART_OF,
        "app.kubernetes.io/managed-by": MANAGED_BY,
    });

    // 1) Secret：**只有凭据**，全部走环境变量（args 里不放凭据：kubectl describe pod 会打印 args）
    if body["secretName"].as_str().filter(|s| !s.is_empty()).is_none() {
        let secret = json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": secret_name, "namespace": namespace, "labels": labels },
            "type": "Opaque",
            "stringData": walljump_env(&private_key, &short_id, &sni, &dest, &uuid, inner_port),
        });
        if let Err(e) = k8s.post(&format!("/api/v1/namespaces/{namespace}/secrets"), &secret) {
            return Response::fail(
                e.http_status(),
                format!(
                    "创建 Secret 失败：{}。可给控制台加 secrets 写权限，或先自建 Secret 再填「已有 Secret 名称」。",
                    e.message()
                ),
            );
        }
    }

    // 2) Deployment：wasm 组件（REALITY 服务端跑在 wasmtime shim 上）
    let deployment = json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": labels,
            "annotations": {
                "k3s-wasm/mode": "walljump",
                "k3s-wasm/impl": "xray-wasm-server",
                "k3s-wasm/node": node,
                "k3s-wasm/server": entry,
                "k3s-wasm/sni": sni,
                "k3s-wasm/dest": dest,
                "k3s-wasm/short-id": short_id,
                "k3s-wasm/listen": format!("0.0.0.0:{inner_port}"),
                "k3s-wasm/entry-port": node_port.to_string(),
                "k3s-wasm/expose": "nodeport",
                "k3s-wasm/public-server": entry,
                "k3s-wasm/allow-from": allow_from,
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
                        "name": "reality-server",
                        "image": image,
                        "imagePullPolicy": "IfNotPresent",
                        "envFrom": [{ "secretRef": { "name": secret_name } }],
                        "ports": [{ "name": "reality", "containerPort": inner_port, "protocol": "TCP" }],
                        // 探针是裸 TCP 连接：未通过 REALITY 认证的流量会被回落到 dest 站点，
                        // 所以这个探测对外看起来就是一次普通 TLS 访问，不泄露服务端身份。
                        "readinessProbe": {
                            "tcpSocket": { "port": inner_port },
                            "periodSeconds": 10, "failureThreshold": 6
                        },
                        "resources": {
                            "requests": { "cpu": "10m", "memory": "32Mi" },
                            "limits": { "memory": "192Mi" }
                        }
                    }]
                }
            }
        }
    });
    if let Err(e) = k8s.post(&format!("/apis/apps/v1/namespaces/{namespace}/deployments"), &deployment) {
        return from_err(e);
    }

    // 3) Service：NodePort 才是公网入口
    let service = walljump_service(&name, &namespace, &labels, inner_port, node_port);
    if let Err(e) = k8s.post(&format!("/api/v1/namespaces/{namespace}/services"), &service) {
        return from_err(e);
    }

    // 4) NetworkPolicy：翻墙入口本来就要对公网开放，所以放行 allowFrom；
    //    集群内 Pod 也放行（便于用集群内客户端自测这条入口）。
    let netpol = json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": { "name": name, "namespace": namespace, "labels": labels },
        "spec": {
            "podSelector": { "matchLabels": { "app.kubernetes.io/name": name } },
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{ "podSelector": {} }, { "ipBlock": { "cidr": allow_from } }],
                "ports": [{ "protocol": "TCP", "port": inner_port }]
            }]
        }
    });
    let netpol_warning = k8s
        .post(
            &format!("/apis/networking.k8s.io/v1/namespaces/{namespace}/networkpolicies"),
            &netpol,
        )
        .err()
        .map(|e| format!("NetworkPolicy 创建失败：{}", e.message()));

    let link = build_vless_link_flow(&uuid, &entry, &public_key, &short_id, &sni, &name, Some("xtls-rprx-vision"));
    let client_config = build_client_config_flow(&uuid, &entry, &public_key, &short_id, &sni, true);
    let mut out = json!({
        "created": true,
        "mode": "walljump",
        "namespace": namespace,
        "name": name,
        "node": node,
        "entry": entry,
        "nodePort": node_port,
        "publicHost": public_host,
        "sni": sni,
        "dest": dest,
        "shortId": short_id,
        "uuid": uuid,
        "publicKey": public_key,
        "vlessLink": link,
        "clientConfig": client_config,
        "impl": "xray-wasm（wasm32-wasip2，reality 服务端模式 XT_MODE=server）",
        "direction": "reality 入站 → socket 出站",
        "usage": format!("国内客户端导入上面的 vless 链接即可；入口就是 {entry}（节点 {node} 的公网地址）"),
        "note": format!(
            "翻墙入口已就绪：{entry}（reality 入 → socket 出，出网走节点 {node} 自己的网络）。\
             链接**带 flow=xtls-rprx-vision**：xray-wasm v0.6.0 起服务端实现了 Vision 流控，\
             这能抗 TLS-in-TLS 指纹；若入口跑的是 ≤v0.5.x 的旧镜像，则需把 flow 置空（否则会被拒）。未认证的探测者会看到 {dest} 的真实证书（回落行为）。"
        ),
    });
    if let Some(w) = netpol_warning {
        out["warning"] = json!(w);
    }
    Response::ok(out)
}

/// 节点是否 Ready。
fn node_ready(node: &Value) -> bool {
    node["status"]["conditions"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .any(|c| c["type"] == "Ready" && c["status"] == "True")
        })
        .unwrap_or(false)
}

/// 取节点的某种地址（InternalIP / ExternalIP / Hostname）。
fn node_addr(node: &Value, kind: &str) -> Option<String> {
    node["status"]["addresses"]
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|x| x["type"].as_str() == Some(kind))
                .and_then(|x| x["address"].as_str())
        })
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// 翻墙入口的 Service（NodePort）。
///
/// ⚠️ Service 的 `spec.selector` 是**扁平的 label map**，不是 Deployment 那种
/// `{matchLabels: {...}}`。写错的症状是创建时报：
///   `Service in version "v1" cannot be handled as a Service:
///    json: cannot unmarshal object into Go struct field ServiceSpec.spec.selector of type string`
/// 单测把形状钉住 —— 这类"形状写错"的错误 mock 完全测不出来（mock 只回 200），
/// 只有真集群的 API server 会拒绝。
fn walljump_service(
    name: &str,
    namespace: &str,
    labels: &Value,
    port: u16,
    node_port: u16,
) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": { "name": name, "namespace": namespace, "labels": labels },
        "spec": {
            "type": "NodePort",
            "selector": { "app.kubernetes.io/name": name },
            "ports": [{
                "name": "reality",
                "port": port,
                "targetPort": port,
                "nodePort": node_port,
                "protocol": "TCP"
            }]
        }
    })
}

/// 翻墙模式要写进 Secret 的环境变量。
///
/// ⚠️ 键名必须与 xray-wasm CLI 的 `server --help` 里列出的**环境变量**完全一致
/// （`XT_MODE=server` / `XT_PRIVATE_KEY` / `XT_SHORT_IDS` / `XT_SERVER_NAMES` /
/// `XT_DEST` / `XT_USERS` / `XT_SERVER_LISTEN`）。单测把这份契约钉住：写错一个字母的
/// 症状是容器起来后按客户端模式跑（或参数校验失败），而 args/pod spec 里看不出异常。
fn walljump_env(
    private_key: &str,
    short_id: &str,
    sni: &str,
    dest: &str,
    uuid: &str,
    port: u16,
) -> Value {
    json!({
        // 同一个 wasm 模块以服务端形态启动（免参数，凭据只从环境变量进来）
        "XT_MODE": "server",
        "XT_SERVER_LISTEN": format!("0.0.0.0:{port}"),
        "XT_PRIVATE_KEY": private_key,
        "XT_SHORT_IDS": short_id,
        "XT_SERVER_NAMES": sni,
        "XT_DEST": dest,
        "XT_USERS": uuid,
    })
}

/// 由 base64url 私钥推出对应公钥 —— 复用别人给的私钥时必须重算，
/// 否则客户端拿到的 pbk 与服务端私钥不匹配，症状是握手「回落」到真实站点。
fn pubkey_from_private_key(private_b64: &str) -> Option<String> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(private_b64.trim())
        .ok()?;
    if raw.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&raw);
    let secret = x25519_dalek::StaticSecret::from(bytes);
    Some(b64url(x25519_dalek::PublicKey::from(&secret).as_bytes()))
}

/// GET /api/xray/tunnels/:ns/:name/vless
///
/// 从该隧道自己的 Secret 里读出参数并重建 `vless://` 链接。
/// 链接不含 REALITY 私钥（私钥只在服务端），所以这是「客户端凭据」级别的敏感信息：
/// 因此**只在被显式请求时返回**，不塞进列表响应（避免进日志/缓存）。
pub fn xray_vless(_req: &Request, ns: &str, name: &str) -> Response {
    let (_cfg, k8s) = client();
    if !is_dns1123_label(ns) || !is_dns1123_label(name) {
        return Response::fail(400, "命名空间或名称不合法");
    }
    let secret = match k8s.get(&format!("/api/v1/namespaces/{ns}/secrets/{name}")) {
        Ok(v) => v,
        Err(e) if e.is_not_found() => {
            return Response::fail(404, "找不到该隧道的 Secret（可能不是本控制台创建的，或已被删除）")
        }
        Err(e) if e.is_forbidden() => {
            return Response::fail(
                403,
                "没有读取 Secret 的权限。需要在该命名空间给 kube-api-proxy 这个 SA 加 secrets get \
                 （见 deploy/base/kube-api-proxy.yaml 里那个命名空间级 Role）。",
            )
        }
        Err(e) => return from_err(e),
    };

    use base64::Engine as _;
    let dec = |key: &str| -> String {
        secret["data"][key]
            .as_str()
            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
            .map(|raw| String::from_utf8_lossy(&raw).to_string())
            .unwrap_or_default()
    };

    let socks_user = dec("XT_SOCKS_USER");

    // 分享链接要给**外部**用：优先取部署上的 k3s-wasm/public-server（隧道可能经 Traefik/NodePort）；
    // 而隧道客户端自己连的是 XT_SERVER（可能是集群内 ClusterIP，对外无意义）。
    // 这些都要先拿到 Deployment —— 顺便用它上面的 k3s-wasm/mode 注解判断模式。
    let dep = k8s
        .get(&format!("/apis/apps/v1/namespaces/{ns}/deployments/{name}"))
        .ok();
    let ann = dep
        .as_ref()
        .map(|d| d["metadata"]["annotations"].clone())
        .unwrap_or_else(|| json!({}));
    let mode = ann["k3s-wasm/mode"].as_str().unwrap_or("tunnel").to_string();
    let is_walljump = mode == "walljump";

    // 两种模式的 Secret **形状不同**，不能共用一套字段读取：
    //   隧道（客户端）  ：XT_SERVER / XT_UUID / XT_PBK / XT_SID / XT_SNI / XT_SOCKS_*
    //   翻墙（服务端）  ：XT_PRIVATE_KEY / XT_USERS / XT_SHORT_IDS / XT_SERVER_NAMES / XT_DEST
    // 所以翻墙模式下：uuid 取 XT_USERS 的第一个，公钥**从私钥推导**（不重复存一份公钥），
    // SNI/shortId/入口一律以 Deployment 上的注解为准（那才是分享给外部用的值）。
    let (uuid, server, pbk, sid, sni) = if is_walljump {
        let uuid = dec("XT_USERS")
            .split(',')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let pbk = pubkey_from_private_key(&dec("XT_PRIVATE_KEY")).unwrap_or_default();
        let server = ann["k3s-wasm/public-server"]
            .as_str()
            .filter(|v| !v.is_empty())
            .or_else(|| ann["k3s-wasm/server"].as_str())
            .unwrap_or("")
            .to_string();
        (
            uuid,
            server,
            pbk,
            ann["k3s-wasm/short-id"].as_str().unwrap_or("").to_string(),
            ann["k3s-wasm/sni"].as_str().unwrap_or("").to_string(),
        )
    } else {
        (
            dec("XT_UUID"),
            dec("XT_SERVER"),
            dec("XT_PBK"),
            dec("XT_SID"),
            dec("XT_SNI"),
        )
    };
    if uuid.is_empty() || server.is_empty() || pbk.is_empty() {
        return Response::fail(
            422,
            if is_walljump {
                "翻墙模式下 Secret 里缺少 XT_USERS / XT_PRIVATE_KEY（无法推导公钥与用户名）"
            } else {
                "Secret 里缺少 XT_UUID / XT_SERVER / XT_PBK，无法重建链接"
            },
        );
    }

    let public_server = dep
        .as_ref()
        .and_then(|d| d["metadata"]["annotations"]["k3s-wasm/public-server"].as_str())
        .map(str::to_string)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| server.clone());

    // 翻墙模式**不带 flow**：xray-wasm 服务端未实现 Vision 流控，带了会被明确拒绝
    let link = build_vless_link_flow(&uuid, &public_server, &pbk, &sid, &sni, name, Some("xtls-rprx-vision"));
    let client_config = build_client_config_flow(&uuid, &public_server, &pbk, &sid, &sni, true);
    Response::ok(json!({
        "name": name,
        "namespace": ns,
        "mode": if is_walljump { "walljump" } else { "tunnel" },
        "impl": if is_walljump { "xray-wasm（REALITY 服务端）" } else { "xray-wasm（REALITY 客户端）" },
        "clientConfig": client_config,
        "vlessLink": link,
        // 说明两件事：链接里的地址（对外）与客户端实际连的地址（可能不同）
        "server": public_server,
        "clientServer": server,
        "sni": sni,
        "shortId": sid,
        "publicKey": pbk,
        // SOCKS5 用户名可以给（用于拼连接命令）；密码**不回显**。
        // 翻墙模式没有 SOCKS5 入口（入口是 REALITY），这两个字段留空，前端据此隐藏相关按钮。
        "socksUser": if is_walljump { String::new() } else { socks_user },
        "socksEndpoint": if is_walljump {
            String::new()
        } else {
            format!("{name}.{ns}.svc.cluster.local:1080")
        },
        "note": if is_walljump {
            "翻墙入口：把 vlessLink 导入客户端即可（**链接不带 flow** —— xray-wasm 服务端尚未实现 \
             XTLS-Vision 流控，带非空 flow 会被拒绝）。链接含客户端凭据（UUID/pbk），请勿公开；\
             REALITY 私钥在服务端，这里没有也不需要。"
        } else {
            "链接含客户端凭据（UUID/pbk），请勿公开；REALITY 私钥在服务端，这里没有也不需要。"
        },
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
    // 隧道模式：上游是 stock Xray，支持 Vision，所以带 flow
    build_vless_link_flow(uuid, server, pbk, sid, sni, name, Some("xtls-rprx-vision"))
}

/// 带/不带 `flow` 的 vless 链接。
///
/// ⚠️ `flow` 不是可选的"优化"，而是**互操作开关**：
///   * 隧道模式（我们当客户端、上游是 stock Xray）→ `flow=xtls-rprx-vision` 可用；
///   * 翻墙模式（xray-wasm 当**服务端**）→ **必须不带 flow**：xray-wasm 服务端
///     尚未实现 XTLS-Vision 流控，带非空 flow 的请求会被明确拒绝
///     （实测报 `Not supported: 服务端尚未实现 Vision 流控…`），不是静默降级。
fn build_vless_link_flow(
    uuid: &str,
    server: &str,
    pbk: &str,
    sid: &str,
    sni: &str,
    name: &str,
    flow: Option<&str>,
) -> String {
    // 参数顺序尽量贴近官方客户端导出的样子；spx 固定 /（REALITY 的回落到路径）
    let flow_part = flow.map(|f| format!("&flow={f}")).unwrap_or_default();
    format!(
        "vless://{uuid}@{server}?encryption=none&type=tcp&security=reality&pbk={pbk}&fp=chrome&sni={sni}&sid={sid}&spx=%2F{flow_part}#{name}"
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
    // 隧道模式：上游 stock Xray，带 Vision
    build_client_config_flow(uuid, server, pbk, sid, sni, true)
}

/// `with_flow=false` 用于**翻墙**模式（xray-wasm 服务端不实现 Vision 流控）。
fn build_client_config_flow(
    uuid: &str,
    server: &str,
    pbk: &str,
    sid: &str,
    sni: &str,
    with_flow: bool,
) -> Value {
    let (host, port) = split_host_port(server, 443);
    let mut user = json!({ "id": uuid, "encryption": "none" });
    if with_flow {
        user["flow"] = json!("xtls-rprx-vision");
    }
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
                "users": [user]
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
    // usage：cluster = 只给集群内 Pod 用（出站）；nodeport = 还要给外部/本机用（入站）
    let usage = body["usage"].as_str().unwrap_or("cluster").to_lowercase();
    let external = usage == "nodeport" || usage == "external";

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

    // 出站侧：集群里的 Pod 怎么用它（环境变量形式，直接对应面板要填的字段）
    let outbound = json!({
        "usage": "cluster",
        "who": "集群内的 Pod",
        "endpoint": "<隧道名>.<命名空间>.svc.cluster.local:1080",
        "envForPod": {
            "HTTP_PROXY": format!("socks5h://{socks_user}:{socks_pass}@<隧道名>.<命名空间>.svc.cluster.local:1080"),
            "ALL_PROXY": format!("socks5h://{socks_user}:{socks_pass}@<隧道名>.<命名空间>.svc.cluster.local:1080"),
        },
        "curlExample": format!("curl --proxy-user '{socks_user}:{socks_pass}' --proxy socks5h://<隧道名>.<命名空间>.svc.cluster.local:1080 https://api.ipify.org"),
        "note": "集群内客户端必须显式配代理；隧道不会自动接管集群流量。",
    });

    // 入站侧：外部/本机怎么用它（经节点 IP:NodePort）
    let inbound = if external {
        json!({
            "usage": "nodeport",
            "who": "你自己（外部经节点 IP）",
            "service": "NodePort",
            "endpoint": format!("socks5h://<节点IP>:<nodePort>"),
            "curlExample": format!("curl --proxy-user '{socks_user}:{socks_pass}' --proxy socks5h://<节点IP>:<nodePort> https://api.ipify.org"),
            // 浏览器/系统代理多数不支持 SOCKS5 账密，需要本地再串一跳
            "localRelayExample": format!("gost -L socks5://127.0.0.1:1080 -F socks5://{socks_user}:{socks_pass}@<节点IP>:<nodePort>"),
            "vlessForLocalClient": link.clone(),
            "note": "建隧道时把「用途」选成 nodeport 才会生成 NodePort；并把放行来源 allowFrom 收窄到你的出口 IP。",
        })
    } else {
        json!({
            "usage": "cluster",
            "who": "集群内（未开放外部）",
            "note": "想要外部/本机也能用：建隧道时把「用途」选成「允许外部经节点 IP」，或先用 port-forward。",
        })
    };

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
        "usage": if external { "nodeport" } else { "cluster" },
        "vlessLink": link,
        "serverConfig": server_config,
        "clientConfig": client_config,
        // 两种方向的配置分别给出，避免"生成了但不知道给谁用"
        "outbound": outbound,
        "inbound": inbound,
        "notes": [
            "私钥只在本响应里出现一次，控制台不保存、不写入集群；请立刻粘到服务端配置里并妥善保管。",
            "服务端：把 serverConfig 覆盖到服务器的 config.json（或只取其 inbound），重启 xray。",
            "客户端：vlessLink 可直接导入官方客户端；clientConfig 是官方 Xray 的等价配置，便于先验证服务端。",
            "SOCKS5 用户名/密码用于这条隧道入口的认证（集群内与外部共用同一组），与上面服务端配置无关。",
            "入站（外部）还需要两步：建隧道时选「用途=nodeport」，并把放行来源 allowFrom 收窄到你的出口 IP。",
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
    fn walljump_link_has_no_flow_but_tunnel_does() {
        // 翻墙模式的链接**不能**带 flow：xray-wasm 服务端未实现 XTLS-Vision 流控，
        // 带非空 flow 会被明确拒绝（实测报 "Not supported: 服务端尚未实现 Vision 流控"）。
        let wj = build_vless_link_flow("u1", "1.2.3.4:30543", "pbk", "sid", "www.cloudflare.com", "home", None);
        assert!(!wj.contains("flow="), "翻墙链接不该含 flow：{wj}");
        assert!(wj.contains("security=reality") && wj.contains("pbk=pbk") && wj.contains("sid=sid"));
        assert!(wj.starts_with("vless://u1@1.2.3.4:30543?"));
        assert!(wj.ends_with("#home"));

        let tn = build_vless_link("u1", "1.2.3.4:443", "pbk", "sid", "www.cloudflare.com", "tokyo");
        assert!(tn.contains("flow=xtls-rprx-vision"));
    }

    #[test]
    fn walljump_client_config_omits_flow_field() {
        let wj = build_client_config_flow("u1", "1.2.3.4:30543", "pbk", "sid", "sni.example", false);
        let user = &wj["outbounds"][0]["settings"]["vnext"][0]["users"][0];
        assert_eq!(user["id"], "u1");
        assert!(user.get("flow").is_none(), "翻墙客户端配置里不该有 flow 字段：{user}");
        let tn = build_client_config("u1", "1.2.3.4:443", "pbk", "sid", "sni.example");
        assert_eq!(
            tn["outbounds"][0]["settings"]["vnext"][0]["users"][0]["flow"],
            "xtls-rprx-vision"
        );
    }

    #[test]
    fn walljump_service_has_flat_selector() {
        // 真集群实测过：Service 的 selector 写成 Deployment 的 {matchLabels:{...}} 会被
        // API server 拒（400 cannot unmarshal object into ... spec.selector of type string）。
        let labels = json!({"app.kubernetes.io/name": "home"});
        let svc = walljump_service("home", "k3s-wasm", &labels, 8443, 30543);
        let sel = &svc["spec"]["selector"];
        assert!(sel.is_object(), "selector 必须是对象");
        assert_eq!(sel["app.kubernetes.io/name"], "home");
        assert!(
            sel.get("matchLabels").is_none(),
            "Service 的 selector 不能嵌套 matchLabels：{sel}"
        );
        assert_eq!(svc["spec"]["type"], "NodePort");
        let p = &svc["spec"]["ports"][0];
        assert_eq!(p["port"], 8443);
        assert_eq!(p["targetPort"], 8443);
        assert_eq!(p["nodePort"], 30543);
    }

    #[test]
    fn walljump_env_matches_cli_contract() {
        let env = walljump_env("PRIV", "aabbccdd", "www.cloudflare.com", "www.cloudflare.com:443", "uuid-1", 8443);
        // 键名取自 xray-wasm `server --help` 的环境变量清单；改错一个字母会被当成客户端启动
        assert_eq!(env["XT_MODE"], "server");
        assert_eq!(env["XT_PRIVATE_KEY"], "PRIV");
        assert_eq!(env["XT_SHORT_IDS"], "aabbccdd");
        assert_eq!(env["XT_SERVER_NAMES"], "www.cloudflare.com");
        assert_eq!(env["XT_DEST"], "www.cloudflare.com:443");
        assert_eq!(env["XT_USERS"], "uuid-1");
        assert_eq!(env["XT_SERVER_LISTEN"], "0.0.0.0:8443");
    }

    #[test]
    fn pubkey_from_private_matches_official_xray_vector() {
        // 向量来自官方 `xray x25519 -i`（32 字节全零私钥 clamp 后）。
        // 复用别人给的私钥时必须重算公钥：不匹配的症状是服务端认证失败、回落真实站点。
        // 刻意不用 reality_keypair()：它走 wasi:random，原生单测里拿不到随机源。
        let zero_priv = "A".repeat(43);
        assert_eq!(
            pubkey_from_private_key(&zero_priv).as_deref(),
            Some("L-V9o0fNYkMVKNqsX7spBzD_9oSvxM_C7ZCZX1jLO3Q")
        );
        assert!(pubkey_from_private_key("not base64!!").is_none());
        assert!(pubkey_from_private_key(&b64url(&[0u8; 16])).is_none(), "长度不对必须拒绝");
    }

    #[test]
    fn shape_reports_walljump_as_wasm_ingress() {
        // 翻墙模式：入站 REALITY、出站直连；实现仍是 wasm（xray-wasm 服务端模式）
        let dep = json!({
            "metadata": {
                "name": "home", "namespace": "k3s-wasm",
                "annotations": {
                    "k3s-wasm/mode": "walljump",
                    "k3s-wasm/server": "2.29.44.63:30543",
                    "k3s-wasm/public-server": "2.29.44.63:30543",
                    "k3s-wasm/sni": "www.cloudflare.com",
                    "k3s-wasm/short-id": "aabbccdd",
                    "k3s-wasm/listen": "0.0.0.0:8443",
                    "k3s-wasm/node": "n1",
                    "k3s-wasm/allow-from": "0.0.0.0/0"
                }
            },
            "spec": {
                "replicas": 1,
                "template": {"spec": {
                    "runtimeClassName": "wasmtime-wasip2",
                    "containers": [{"image": "docker.io/k3s-wasm/xray-wasm-cli:v0.6.0"}]
                }}
            },
            "status": {"readyReplicas": 1}
        });
        let svc = json!({"spec": {"type": "NodePort", "ports": [{"port": 8443, "nodePort": 30543}]}});
        let out = xray_shape(&dep, Some(&svc));
        assert_eq!(out["mode"], "walljump");
        assert_eq!(out["direction"], "ingress");
        assert_eq!(out["isWasm"], true);
        assert_eq!(out["runtimeClass"], "wasmtime-wasip2");
        assert_eq!(out["ingress"]["endpoint"], "2.29.44.63:30543");
        assert_eq!(out["usage"]["inbound"], true);
        assert_eq!(out["usage"]["outbound"], false);
        assert_eq!(out["node"], "n1");
    }

    #[test]
    fn runtime_category_covers_real_cluster_classes() {
        // 这 12 个名字取自真机 `kubectl get runtimeclasses`（k3s 自动探测出来的
        // 每个 shim 一个）。特别钉住 lunatic / slight / wws：它们名字里没有 "wasm"，
        // 但都是 runwasi 系的 WASM 运行时 —— 早先被误判成 native。
        for wasm in [
            "spin", "wasmtime", "wasmtime-spin-v2", "wasmtime-wasip2", "wasmedge", "wasmer",
            "lunatic", "slight", "wws",
        ] {
            assert_eq!(runtime_category(wasm), "wasm", "{wasm} 应归 wasm");
        }
        for gpu in ["nvidia", "nvidia-experimental", "amd.com/gpu", "nvidia-container-runtime"] {
            assert_eq!(runtime_category(gpu), "gpu", "{gpu} 应归 gpu");
        }
        for native in ["crun", "runc", "kata", ""] {
            assert_eq!(runtime_category(native), "native", "{native:?} 应归 native");
        }
        // handler 与名字都要能被识别（k3s 的 handler 就是 shim 名）
        assert_eq!(runtime_category("containerd-shim-spin-v2"), "wasm");
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
