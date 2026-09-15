//! 网络拓扑：把集群里的对象压成「节点 + 边」的一张图，交给前端 SVG 实时渲染。
//!
//! # 为什么在后端算
//!
//! 组件是「一个请求一个实例」、没有内存状态，而前端每 5 秒只拿到一份快照。
//! 把「快照 → 图」做成**纯函数**（`build_topology`）有两个好处：
//!   * 可以在原生 host 上 `cargo test`，不用起集群（见文件末的单测）；
//!   * 图的结构只在一处定义，前端只负责画。
//!
//! # 边都是真实关联，不猜
//!
//! | kind | 来源 |
//! |---|---|
//! | `routes` | Ingress 规则里写的 service 名 → Service |
//! | `exposes` | Service 的 selector → 命中它的工作负载模板标签 |
//! | `runs-on` | Pod 的 `spec.nodeName` → Node |
//! | `in` | 对象的 namespace → Namespace |
//! | `allows` | NetworkPolicy 的 podSelector → 命中的工作负载 |
//! | `belongs-to` | Pod → 它的工作负载（ownerReferences，ReplicaSet 名去掉 hash 尾巴） |
//!
//! 刻意**不**做的事：不去读 Cilium 的 BPF 状态、不声称知道真实数据流向。
//! 这是「声明的拓扑」——它回答的是「谁被允许/暴露/调度到哪里」，这也是排查
//! 连通性问题时真正要看的东西；要把实际流量画出来得接 Hubble，那是另一件事。

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use serde_json::{json, Value};

use crate::api::{client, node_gpu, runtime_category};
use crate::http_io::{Request, Response};
use crate::k8s::{ApiError, K8s};

/// 画出来的 Pod 节点上限：超了就截断并如实告诉前端（避免浏览器被大集群拖死）。
const MAX_POD_NODES: usize = 200;
/// 工作负载上限：全命名空间视图下防止把图画成毛线球。
const MAX_WORKLOADS: usize = 60;

pub struct TopologyInput {
    pub namespace: String,
    pub include_pods: bool,
    pub nodes: Vec<Value>,
    pub namespaces: Vec<Value>,
    pub deployments: Vec<Value>,
    pub spinapps: Vec<Value>,
    pub pods: Vec<Value>,
    pub services: Vec<Value>,
    pub ingresses: Vec<Value>,
    pub network_policies: Vec<Value>,
    /// 读不到的辅助资源（404/403 等）：如实带给前端，而不是让整张图失败
    pub notes: Vec<String>,
    pub now: u64,
}

/// GET /api/topology?namespace=<ns|_all>&pods=0|1
pub fn topology(req: &Request) -> Response {
    let (cfg, k8s) = client();
    let namespace = req
        .query_param("namespace")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cfg.default_namespace.clone());
    let include_pods = matches!(
        req.query_param("pods").as_deref().map(str::trim),
        Some("1") | Some("true")
    );
    match gather(&k8s, namespace, include_pods) {
        Ok(input) => Response::ok(build_topology(&input)),
        Err(e) => Response::fail(e.http_status(), format!("读取集群对象失败：{}", e.message())),
    }
}

fn items(k8s: &K8s, path: &str) -> Result<Vec<Value>, ApiError> {
    Ok(k8s.get(path)?["items"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

/// 可选资源（例如没装 SpinKube 时的 SpinApp CRD）：拿不到就当空，但把原因带出去。
/// 辅助资源（Service / Ingress / NetworkPolicy / Namespace / SpinApp）：
/// 读不到就当空表并**记录原因**，而不是让整张拓扑图失败。
///
/// 这是刻意的：受限集群里很常见「能看 Pod 但没权限看 Ingress」，
/// 那种情况下让人看到半张图（并说明缺了什么）比看到一个红叉有用得多。
fn optional_items(k8s: &K8s, path: &str, what: &str, notes: &mut Vec<String>) -> Vec<Value> {
    match k8s.get(path) {
        Ok(v) => v["items"].as_array().cloned().unwrap_or_default(),
        Err(e) if e.is_not_found() => {
            notes.push(format!("{what}：接口不存在（{path}）"));
            Vec::new()
        }
        Err(e) if e.is_forbidden() => {
            notes.push(format!("{what}：没有读取权限（{path}）"));
            Vec::new()
        }
        Err(e) => {
            notes.push(format!("{what}：读取失败（{}）", e.message()));
            Vec::new()
        }
    }
}

fn gather(k8s: &K8s, namespace: String, include_pods: bool) -> Result<TopologyInput, ApiError> {
    let mut notes: Vec<String> = Vec::new();
    // 这四个是图的主干：读不到就不能假装有拓扑，直接失败更诚实
    let nodes = items(k8s, "/api/v1/nodes")?;
    // 全命名空间视图下 limit 有用：默认不返回全部 Pod 会让计数失真
    let pods = items(k8s, "/api/v1/pods?limit=2000")?;
    let deployments = items(k8s, "/apis/apps/v1/deployments")?;
    // 辅助资源可以缺
    let namespaces = optional_items(k8s, "/api/v1/namespaces", "命名空间", &mut notes);
    let services = optional_items(k8s, "/api/v1/services", "Service", &mut notes);
    let ingresses = optional_items(k8s, "/apis/networking.k8s.io/v1/ingresses", "Ingress", &mut notes);
    let network_policies = optional_items(
        k8s,
        "/apis/networking.k8s.io/v1/networkpolicies",
        "NetworkPolicy",
        &mut notes,
    );
    let spinapps = optional_items(
        k8s,
        "/apis/core.spinkube.dev/v1alpha1/spinapps",
        "SpinApp",
        &mut notes,
    );
    Ok(TopologyInput {
        namespace,
        include_pods,
        nodes,
        namespaces,
        deployments,
        spinapps,
        pods,
        services,
        ingresses,
        network_policies,
        notes,
        now: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    })
}

// ════════════════════════════════════════════════════════════════════
// 纯函数：快照 → 图
// ════════════════════════════════════════════════════════════════════

fn meta_str<'a>(v: &'a Value, key: &str) -> &'a str {
    v["metadata"][key].as_str().unwrap_or("")
}

fn in_scope(v: &Value, ns: &str) -> bool {
    ns == "_all" || meta_str(v, "namespace") == ns
}

fn svc_id(ns: &str, name: &str) -> String {
    format!("service:{ns}/{name}")
}
fn workload_id(ns: &str, name: &str) -> String {
    format!("workload:{ns}/{name}")
}
fn pod_id(ns: &str, name: &str) -> String {
    format!("pod:{ns}/{name}")
}
fn node_id(name: &str) -> String {
    format!("node:{name}")
}
fn ns_id(name: &str) -> String {
    format!("ns:{name}")
}
fn ingress_id(ns: &str, name: &str) -> String {
    format!("ingress:{ns}/{name}")
}
fn netpol_id(ns: &str, name: &str) -> String {
    format!("networkPolicy:{ns}/{name}")
}

/// 一个工作负载在图上要画的东西（Deployment / SpinApp / 以及从 Pod owner 推出来的）。
struct Workload {
    id: String,
    kind: String,
    name: String,
    ns: String,
    labels: Value,
    runtime_class: String,
    desired: i64,
    ready: i64,
    image: Value,
    created_at: Value,
    /// 落在这个工作负载上的 Pod：用来算 node_spread 与 exposes 的权重
    pods_on_node: BTreeMap<String, i64>,
    pod_total: i64,
}

impl Workload {
    fn new(id: String, kind: &str, name: &str, ns: &str, labels: Value) -> Self {
        Self {
            id,
            kind: kind.to_string(),
            name: name.to_string(),
            ns: ns.to_string(),
            labels,
            runtime_class: String::new(),
            desired: 0,
            ready: 0,
            image: Value::Null,
            created_at: Value::Null,
            pods_on_node: BTreeMap::new(),
            pod_total: 0,
        }
    }
}

fn template_labels(workload: &Value) -> Value {
    workload["spec"]["template"]["metadata"]["labels"].clone()
}

/// Pod → 它属于哪个工作负载。
///
/// ReplicaSet 的名字形如 `<deploy>-7d9f8b6c5`，去掉最后一段就是 Deployment 名；
/// 其余 owner（StatefulSet / DaemonSet / Job）名字本身就是工作负载名。
fn owner_workload(pod: &Value) -> Option<(String, String)> {
    let owner = pod["metadata"]["ownerReferences"].as_array()?.first()?;
    let kind = owner["kind"].as_str().unwrap_or("");
    let name = owner["name"].as_str().unwrap_or("");
    if name.is_empty() {
        return None;
    }
    match kind {
        "ReplicaSet" => {
            let base = name.rsplit_once('-').map(|(b, _)| b).unwrap_or(name);
            Some(("Deployment".to_string(), base.to_string()))
        }
        "StatefulSet" | "DaemonSet" | "Job" | "CronJob" => {
            Some((kind.to_string(), name.to_string()))
        }
        _ => None,
    }
}

/// selector（Service 的 `spec.selector` / NetworkPolicy 的 `podSelector.matchLabels`）
/// 是否命中工作负载模板标签。空 selector 视为「命中本命名空间所有 Pod」（k8s 语义）。
fn selector_hits(selector: &Value, labels: &Value) -> bool {
    let Some(sel) = selector.as_object() else {
        return false;
    };
    if sel.is_empty() {
        return true;
    }
    sel.iter().all(|(k, v)| labels.get(k) == Some(v))
}

pub fn build_topology(input: &TopologyInput) -> Value {
    let mut objects: Vec<Value> = Vec::new();
    let mut edges: Vec<Value> = Vec::new();
    let mut workloads: BTreeMap<String, Workload> = BTreeMap::new();

    // ── 1. 节点 ──
    // 只画有 Pod 的节点？不：节点是拓扑的落脚点，空节点也要在（否则工作负载的
    // runs-on 边会指向不存在的目标）。
    for n in &input.nodes {
        let name = meta_str(n, "name");
        if name.is_empty() {
            continue;
        }
        let (gpu_present, gpu_count) = node_gpu(n);
        let labels = n["metadata"]["labels"].clone();
        let ready = n["status"]["conditions"]
            .as_array()
            .map(|cs| {
                cs.iter()
                    .any(|c| c["type"] == "Ready" && c["status"] == "True")
            })
            .unwrap_or(false);
        let addr = n["status"]["addresses"]
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|x| x["type"] == "InternalIP")
                    .and_then(|x| x["address"].as_str())
            })
            .unwrap_or("");
        objects.push(json!({
            "id": node_id(name),
            "kind": "node",
            "label": name,
            "sublabel": addr,
            "group": "node",
            "category": "native",
            "meta": {
                "ready": ready,
                "wasmtime": crate::api::label_bool(&labels, "wasm.sh/wasmtime"),
                "spin": crate::api::label_bool(&labels, "wasm.sh/spin"),
                "gpu": gpu_present,
                "gpuCount": gpu_count,
                "pods": 0,
                "cpu": n["status"]["capacity"]["cpu"],
                "memory": n["status"]["capacity"]["memory"],
                "kubeletVersion": n["status"]["nodeInfo"]["kubeletVersion"],
            },
        }));
    }

    // ── 2. 工作负载（Deployment + SpinApp）──
    for d in &input.deployments {
        if !in_scope(d, &input.namespace) {
            continue;
        }
        let ns = meta_str(d, "namespace");
        let name = meta_str(d, "name");
        let rc = d["spec"]["template"]["spec"]["runtimeClassName"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let mut w = Workload::new(
            workload_id(ns, name),
            "Deployment",
            name,
            ns,
            template_labels(d),
        );
        w.runtime_class = rc;
        w.desired = d["spec"]["replicas"].as_i64().unwrap_or(1);
        w.ready = d["status"]["readyReplicas"].as_i64().unwrap_or(0);
        w.image = d["spec"]["template"]["spec"]["containers"]
            .as_array()
            .and_then(|c| c.first())
            .map(|c| c["image"].clone())
            .unwrap_or(Value::Null);
        w.created_at = d["metadata"]["creationTimestamp"].clone();
        workloads.insert(w.id.clone(), w);
    }
    for a in &input.spinapps {
        if !in_scope(a, &input.namespace) {
            continue;
        }
        let ns = meta_str(a, "namespace");
        let name = meta_str(a, "name");
        let mut w = Workload::new(workload_id(ns, name), "SpinApp", name, ns, json!({}));
        // SpinApp 没有 runtimeClassName：它由 executor 决定，而 executor 一定是 spin
        // 运行时，所以这里直接定成 wasm（与 api.rs 里 shape_spinapp 的口径一致）。
        w.runtime_class = "wasmtime-spin-v2".to_string();
        w.desired = a["spec"]["replicas"].as_i64().unwrap_or(1);
        w.ready = a["status"]["readyReplicas"].as_i64().unwrap_or(0);
        w.image = a["spec"]["image"].clone();
        w.created_at = a["metadata"]["creationTimestamp"].clone();
        workloads.insert(w.id.clone(), w);
    }

    // ── 3. Pod：只用来算计数、落在哪个节点、以及（可选）画出来 ──
    // 注意 Pod 可能属于图上没有的工作负载（例如 StatefulSet）——那就现造一个，
    // 否则这些 Pod 在拓扑里会凭空消失，看起来像"集群里没有它"。
    let mut pod_nodes: Vec<Value> = Vec::new();
    let mut pods_truncated = false;
    for p in &input.pods {
        if !in_scope(p, &input.namespace) {
            continue;
        }
        let ns = meta_str(p, "namespace");
        let name = meta_str(p, "name");
        let node = p["spec"]["nodeName"].as_str().unwrap_or("").to_string();
        if let Some((kind, wname)) = owner_workload(p) {
            let id = workload_id(ns, &wname);
            let entry = workloads.entry(id.clone()).or_insert_with(|| {
                let mut w = Workload::new(id.clone(), &kind, &wname, ns, json!({}));
                // 造出来的工作负载没有模板可读：runtime 就从 Pod 自己的 runtimeClassName 推
                w.runtime_class = p["spec"]["runtimeClassName"].as_str().unwrap_or("").to_string();
                w
            });
            entry.pod_total += 1;
            if !node.is_empty() {
                *entry.pods_on_node.entry(node.clone()).or_insert(0) += 1;
            }
        }
        // 节点上的 Pod 计数（不管它是谁的）
        if let Some(n) = objects
            .iter_mut()
            .find(|o| o["id"].as_str() == Some(node_id(&node).as_str()))
        {
            let cur = n["meta"]["pods"].as_i64().unwrap_or(0);
            n["meta"]["pods"] = json!(cur + 1);
        }
        if input.include_pods {
            if pod_nodes.len() >= MAX_POD_NODES {
                pods_truncated = true;
                continue;
            }
            let owner = owner_workload(p).map(|(_, n)| workload_id(ns, &n));
            let statuses = p["status"]["containerStatuses"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let ready = statuses.iter().filter(|c| c["ready"] == true).count();
            let restarts: i64 = statuses
                .iter()
                .map(|c| c["restartCount"].as_i64().unwrap_or(0))
                .sum();
            let rc = p["spec"]["runtimeClassName"].as_str().unwrap_or("");
            let mut pn = json!({
                "id": pod_id(ns, name),
                "kind": "pod",
                "label": name,
                "sublabel": p["status"]["phase"],
                "group": "pod",
                "category": runtime_category(rc),
                "meta": {
                    "namespace": ns,
                    "node": node,
                    "phase": p["status"]["phase"],
                    "ready": format!("{ready}/{}", p["spec"]["containers"].as_array().map(|c| c.len()).unwrap_or(0)),
                    "restarts": restarts,
                    "podIP": p["status"]["podIP"],
                    "workload": owner.clone(),
                },
            });
            // pod → node / namespace 的边（有 owner 时也保留，便于看实际落点）
            if !node.is_empty() {
                edges.push(json!({ "from": pod_id(ns, name), "to": node_id(&node), "kind": "runs-on", "count": 1 }));
            }
            edges.push(json!({ "from": pod_id(ns, name), "to": ns_id(ns), "kind": "in", "count": 1 }));
            if let Some(w) = owner {
                edges.push(json!({ "from": pod_id(ns, name), "to": w, "kind": "belongs-to", "count": 1 }));
            }
            if pn["meta"]["workload"].is_null() {
                pn["meta"]["workload"] = Value::Null;
            }
            pod_nodes.push(pn);
        }
    }

    // ── 4. 命名空间节点（只画范围内真的被引用到的）──
    let mut ns_used: BTreeMap<String, i64> = BTreeMap::new();
    for w in workloads.values() {
        *ns_used.entry(w.ns.clone()).or_insert(0) += 1;
    }
    for s in &input.services {
        if in_scope(s, &input.namespace) {
            *ns_used.entry(meta_str(s, "namespace").to_string()).or_insert(0) += 0;
        }
    }
    let known_ns: Vec<String> = input
        .namespaces
        .iter()
        .map(|n| meta_str(n, "name").to_string())
        .collect();
    for ns in ns_used.keys() {
        // 名字对不上时也画（例如权限只允许看部分命名空间）——不要因为读不到就丢节点
        let _ = known_ns.contains(ns);
        objects.push(json!({
            "id": ns_id(ns),
            "kind": "namespace",
            "label": ns,
            "sublabel": "命名空间",
            "group": "namespace",
            "category": "native",
            "meta": { "workloads": ns_used.get(ns).copied().unwrap_or(0) },
        }));
        for w in workloads.values().filter(|w| &w.ns == ns) {
            edges.push(json!({ "from": w.id, "to": ns_id(ns), "kind": "in", "count": 1 }));
        }
    }

    // ── 5. Service：selector → 工作负载；同时把 endpoints 数出来 ──
    let mut service_nodes: Vec<Value> = Vec::new();
    for s in &input.services {
        if !in_scope(s, &input.namespace) {
            continue;
        }
        let ns = meta_str(s, "namespace");
        let name = meta_str(s, "name");
        let selector = s["spec"]["selector"].clone();
        let matched: Vec<&Workload> = workloads
            .values()
            .filter(|w| w.ns == ns && selector_hits(&selector, &w.labels))
            .collect();
        let endpoints: i64 = matched.iter().map(|w| w.pod_total).sum();
        let ports: Vec<Value> = s["spec"]["ports"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|p| {
                json!({
                    "name": p["name"],
                    "port": p["port"],
                    "targetPort": p["targetPort"],
                    "nodePort": p["nodePort"],
                    "protocol": p["protocol"],
                })
            })
            .collect();
        let ty = s["spec"]["type"].as_str().unwrap_or("ClusterIP");
        let cluster_ip = s["spec"]["clusterIP"].as_str().unwrap_or("");
        let external = match ty {
            "NodePort" => s["spec"]["ports"]
                .as_array()
                .and_then(|p| p.first())
                .map(|p| p["nodePort"].clone())
                .filter(|np| !np.is_null())
                .map(|np| format!("<节点IP>:{np}"))
                .unwrap_or_else(|| "<节点IP>:<nodePort>".to_string()),
            "LoadBalancer" => s["status"]["loadBalancer"]["ingress"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|i| i["ip"].as_str().or_else(|| i["hostname"].as_str()))
                .unwrap_or("<pending>")
                .to_string(),
            _ => cluster_ip.to_string(),
        };
        for w in &matched {
            edges.push(json!({
                "from": svc_id(ns, name),
                "to": w.id,
                "kind": "exposes",
                "count": w.pod_total.max(1),
            }));
        }
        service_nodes.push(json!({
            "id": svc_id(ns, name),
            "kind": "service",
            "label": name,
            "sublabel": ty,
            "group": "service",
            "category": "native",
            "meta": {
                "namespace": ns,
                "type": ty,
                "clusterIP": cluster_ip,
                "ports": ports,
                "endpoints": endpoints,
                "external": external,
                "matchedWorkloads": matched.len(),
            },
        }));
    }
    objects.extend(service_nodes);

    // ── 6. Ingress → Service ──
    for i in &input.ingresses {
        if !in_scope(i, &input.namespace) {
            continue;
        }
        let ns = meta_str(i, "namespace");
        let name = meta_str(i, "name");
        let mut rules: Vec<Value> = Vec::new();
        let mut hosts: Vec<String> = Vec::new();
        let mut targets: BTreeMap<String, i64> = BTreeMap::new();
        for rule in i["spec"]["rules"].as_array().cloned().unwrap_or_default() {
            let host = rule["host"].as_str().unwrap_or("*").to_string();
            let mut paths: Vec<String> = Vec::new();
            for p in rule["http"]["paths"].as_array().cloned().unwrap_or_default() {
                paths.push(p["path"].as_str().unwrap_or("/").to_string());
                if let Some(svc) = p["backend"]["service"]["name"].as_str() {
                    *targets.entry(svc.to_string()).or_insert(0) += 1;
                }
            }
            if !hosts.contains(&host) {
                hosts.push(host.clone());
            }
            rules.push(json!({ "host": host, "paths": paths }));
        }
        let tls = i["spec"]["tls"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        for (svc, count) in &targets {
            edges.push(json!({
                "from": ingress_id(ns, name),
                "to": svc_id(ns, svc),
                "kind": "routes",
                "count": count,
            }));
        }
        objects.push(json!({
            "id": ingress_id(ns, name),
            "kind": "ingress",
            "label": if hosts.is_empty() { name.to_string() } else { hosts.join(", ") },
            "sublabel": "Ingress",
            "group": "ingress",
            "category": "native",
            "meta": {
                "namespace": ns,
                "className": i["spec"]["ingressClassName"],
                "tls": tls,
                "rules": rules,
                "serviceCount": targets.len(),
            },
        }));
    }

    // ── 7. NetworkPolicy → 工作负载（谁被允许连进来）──
    for np in &input.network_policies {
        if !in_scope(np, &input.namespace) {
            continue;
        }
        let ns = meta_str(np, "name");
        let ns_name = meta_str(np, "namespace");
        let selector = np["spec"]["podSelector"]["matchLabels"].clone();
        let hit: Vec<&Workload> = workloads
            .values()
            .filter(|w| w.ns == ns_name && selector_hits(&selector, &w.labels))
            .collect();
        let ingress_rules = np["spec"]["ingress"].as_array().map(|a| a.len()).unwrap_or(0);
        let egress_rules = np["spec"]["egress"].as_array().map(|a| a.len()).unwrap_or(0);
        for w in &hit {
            edges.push(json!({
                "from": netpol_id(ns_name, ns),
                "to": w.id,
                "kind": "allows",
                "count": ingress_rules.max(1),
            }));
        }
        let policy_types: Vec<Value> = np["spec"]["policyTypes"].as_array().cloned().unwrap_or_default();
        objects.push(json!({
            "id": netpol_id(ns_name, ns),
            "kind": "networkPolicy",
            "label": ns,
            "sublabel": "NetworkPolicy",
            "group": "networkPolicy",
            "category": "native",
            "meta": {
                "namespace": ns_name,
                "policyTypes": policy_types,
                "ingressRules": ingress_rules,
                "egressRules": egress_rules,
                "matchedWorkloads": hit.len(),
            },
        }));
    }

    // ── 8. 工作负载节点（放在最后：需要 pod 计数与 node 分布）──
    let mut workload_nodes: Vec<Value> = Vec::new();
    let mut worklist: Vec<&Workload> = workloads.values().collect();
    // 稳定的排序键：名字 → 输出顺序稳定，前端布局才不会每次刷新都跳
    worklist.sort_by(|a, b| a.id.cmp(&b.id));
    let workloads_truncated = worklist.len() > MAX_WORKLOADS;
    for w in worklist.into_iter().take(MAX_WORKLOADS) {
        let spread: Value = Value::Object(
            w.pods_on_node
                .iter()
                .map(|(node, n)| (node.clone(), json!(n)))
                .collect(),
        );
        for (node, n) in &w.pods_on_node {
            edges.push(json!({
                "from": w.id,
                "to": node_id(node),
                "kind": "runs-on",
                "count": n,
            }));
        }
        let category = if w.kind == "SpinApp" {
            "wasm"
        } else {
            runtime_category(&w.runtime_class)
        };
        workload_nodes.push(json!({
            "id": w.id,
            "kind": "workload",
            "label": w.name,
            "sublabel": w.kind,
            "group": "workload",
            "category": category,
            "meta": {
                "namespace": w.ns,
                "workloadKind": w.kind,
                "runtimeClass": if w.runtime_class.is_empty() { Value::Null } else { json!(w.runtime_class) },
                "desired": w.desired,
                "ready": w.ready,
                "pods": w.pod_total,
                "nodeSpread": spread,
                "image": w.image,
                "createdAt": w.created_at,
            },
        }));
    }
    objects.extend(workload_nodes);
    objects.extend(pod_nodes);

    // ── 9. 计数与指纹 ──
    let count_kind = |k: &str| {
        objects
            .iter()
            .filter(|o| o["kind"].as_str() == Some(k))
            .count()
    };
    let count_cat = |c: &str| {
        objects
            .iter()
            .filter(|o| o["kind"] == "workload" && o["category"].as_str() == Some(c))
            .count()
    };
    let pods_total = input
        .pods
        .iter()
        .filter(|p| in_scope(p, &input.namespace))
        .count();
    let revision = revision_of(&objects, &edges);

    json!({
        "generatedAt": input.now,
        "revision": revision,
        "scope": {
            "namespace": input.namespace,
            "includePods": input.include_pods,
            "truncated": pods_truncated || workloads_truncated,
            "limits": { "pods": MAX_POD_NODES, "workloads": MAX_WORKLOADS },
        },
        // 缺了哪些辅助资源：前端要显示出来，否则"图里没有 Service"会被误读成"集群里没有"
        "notes": input.notes,
        "counts": {
            "nodes": count_kind("node"),
            "namespaces": count_kind("namespace"),
            "workloads": count_kind("workload"),
            "services": count_kind("service"),
            "ingresses": count_kind("ingress"),
            "networkPolicies": count_kind("networkPolicy"),
            "pods": pods_total,
            "wasm": count_cat("wasm"),
            "native": count_cat("native"),
            "gpu": count_cat("gpu"),
        },
        "nodes": objects,
        "edges": edges,
    })
}

/// 内容指纹：节点/边的**集合**或关键计数变了它就变，前端据此跳过无意义的重绘。
fn revision_of(objects: &[Value], edges: &[Value]) -> String {
    let mut hasher = DefaultHasher::new();
    let mut ids: Vec<&str> = objects.iter().filter_map(|o| o["id"].as_str()).collect();
    ids.sort_unstable();
    for id in ids {
        id.hash(&mut hasher);
    }
    // 计数也算进去：扩缩容时 id 不变，但"变了"这件事必须让前端知道
    let mut counters: Vec<(String, i64, i64)> = objects
        .iter()
        .map(|o| {
            (
                o["id"].as_str().unwrap_or("").to_string(),
                o["meta"]["pods"].as_i64().unwrap_or(-1),
                o["meta"]["ready"].as_i64().unwrap_or(-1),
            )
        })
        .collect();
    counters.sort();
    for c in counters {
        c.hash(&mut hasher);
    }
    let mut edge_keys: Vec<String> = edges
        .iter()
        .map(|e| {
            format!(
                "{}>{}:{}:{}",
                e["from"].as_str().unwrap_or(""),
                e["to"].as_str().unwrap_or(""),
                e["kind"].as_str().unwrap_or(""),
                e["count"].as_i64().unwrap_or(0)
            )
        })
        .collect();
    edge_keys.sort();
    for k in edge_keys {
        k.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, ip: &str, labels: Value, capacity: Value) -> Value {
        json!({
            "metadata": { "name": name, "labels": labels },
            "status": {
                "conditions": [{ "type": "Ready", "status": "True" }],
                "addresses": [{ "type": "InternalIP", "address": ip }],
                "capacity": capacity,
                "nodeInfo": { "kubeletVersion": "v1.36.4+k3s1" },
            }
        })
    }

    fn input() -> TopologyInput {
        TopologyInput {
            namespace: "k3s-wasm".to_string(),
            include_pods: false,
            nodes: vec![
                node(
                    "n1",
                    "2.29.44.63",
                    json!({ "wasm.sh/wasmtime": "true", "nvidia.com/gpu.present": "true" }),
                    json!({ "cpu": "8", "memory": "16Gi", "nvidia.com/gpu": "1" }),
                ),
                node("n2", "10.0.0.2", json!({}), json!({ "cpu": "4", "memory": "8Gi" })),
            ],
            namespaces: vec![json!({ "metadata": { "name": "k3s-wasm" } })],
            deployments: vec![
                json!({
                    "metadata": { "name": "ui", "namespace": "k3s-wasm", "creationTimestamp": "t1" },
                    "spec": {
                        "replicas": 1,
                        "template": {
                            "metadata": { "labels": { "app": "ui" } },
                            "spec": {
                                "runtimeClassName": "wasmtime-wasip2",
                                "containers": [{ "image": "img:1" }],
                            }
                        }
                    },
                    "status": { "readyReplicas": 1 },
                }),
                json!({
                    "metadata": { "name": "gpu-job", "namespace": "k3s-wasm" },
                    "spec": {
                        "replicas": 1,
                        "template": {
                            "metadata": { "labels": { "app": "gpu-job" } },
                            "spec": {
                                "runtimeClassName": "nvidia",
                                "containers": [{ "image": "cuda:1" }],
                            }
                        }
                    },
                    "status": { "readyReplicas": 0 },
                }),
            ],
            spinapps: vec![],
            pods: vec![
                json!({
                    "metadata": {
                        "name": "ui-abc", "namespace": "k3s-wasm",
                        "ownerReferences": [{ "kind": "ReplicaSet", "name": "ui-7d9f8b6c5" }],
                    },
                    "spec": { "nodeName": "n1", "runtimeClassName": "wasmtime-wasip2",
                              "containers": [{ "name": "ui" }] },
                    "status": { "phase": "Running", "podIP": "10.42.0.9",
                                "containerStatuses": [{ "ready": true, "restartCount": 0 }] },
                }),
                json!({
                    "metadata": {
                        "name": "gpu-job-abc", "namespace": "k3s-wasm",
                        "ownerReferences": [{ "kind": "ReplicaSet", "name": "gpu-job-5f6" }],
                    },
                    "spec": { "nodeName": "n1", "runtimeClassName": "nvidia",
                              "containers": [{ "name": "c" }] },
                    "status": { "phase": "Pending", "containerStatuses": [] },
                }),
            ],
            services: vec![json!({
                "metadata": { "name": "ui", "namespace": "k3s-wasm" },
                "spec": { "type": "ClusterIP", "clusterIP": "10.43.0.1", "selector": { "app": "ui" },
                          "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] },
            })],
            ingresses: vec![json!({
                "metadata": { "name": "ui-tls", "namespace": "k3s-wasm" },
                "spec": {
                    "ingressClassName": "traefik",
                    "tls": [{ "hosts": ["console.example.test"] }],
                    "rules": [{ "host": "console.example.test",
                                "http": { "paths": [{ "path": "/",
                                    "backend": { "service": { "name": "ui" } } }] } }],
                },
            })],
            network_policies: vec![json!({
                "metadata": { "name": "ui-np", "namespace": "k3s-wasm" },
                "spec": { "podSelector": { "matchLabels": { "app": "ui" } },
                          "policyTypes": ["Ingress"], "ingress": [{}] },
            })],
            notes: vec![],
            now: 1694763000,
        }
    }

    fn ids(v: &Value, kind: &str) -> Vec<String> {
        v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o["kind"] == kind)
            .map(|o| o["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn has_edge(v: &Value, from: &str, to: &str, kind: &str) -> bool {
        v["edges"].as_array().unwrap().iter().any(|e| {
            e["from"] == from && e["to"] == to && e["kind"] == kind
        })
    }

    #[test]
    fn graph_has_expected_nodes_and_edges() {
        let v = build_topology(&input());
        // 节点
        assert!(ids(&v, "node").contains(&"node:n1".to_string()));
        assert!(ids(&v, "workload").contains(&"workload:k3s-wasm/ui".to_string()));
        assert!(ids(&v, "workload").contains(&"workload:k3s-wasm/gpu-job".to_string()));
        assert!(ids(&v, "service").contains(&"service:k3s-wasm/ui".to_string()));
        assert!(ids(&v, "ingress").contains(&"ingress:k3s-wasm/ui-tls".to_string()));
        assert!(ids(&v, "namespace").contains(&"ns:k3s-wasm".to_string()));
        assert!(ids(&v, "networkPolicy").contains(&"networkPolicy:k3s-wasm/ui-np".to_string()));
        // 边：ingress → service → workload → node，以及 netpol → workload
        assert!(has_edge(&v, "ingress:k3s-wasm/ui-tls", "service:k3s-wasm/ui", "routes"));
        assert!(has_edge(&v, "service:k3s-wasm/ui", "workload:k3s-wasm/ui", "exposes"));
        assert!(has_edge(&v, "workload:k3s-wasm/ui", "node:n1", "runs-on"));
        assert!(has_edge(&v, "workload:k3s-wasm/ui", "ns:k3s-wasm", "in"));
        assert!(has_edge(&v, "networkPolicy:k3s-wasm/ui-np", "workload:k3s-wasm/ui", "allows"));
        // 没绑到任何 Pod 的工作负载不该有 runs-on 边（否则图上会出现假连接）
        assert!(!has_edge(&v, "workload:k3s-wasm/gpu-job", "node:n2", "runs-on"));
        // 默认不画 Pod 节点
        assert!(ids(&v, "pod").is_empty());
    }

    #[test]
    fn categories_follow_runtime_class() {
        let v = build_topology(&input());
        let wl = |name: &str| {
            v["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|o| o["id"] == format!("workload:k3s-wasm/{name}"))
                .cloned()
                .unwrap()
        };
        assert_eq!(wl("ui")["category"], "wasm");
        assert_eq!(wl("gpu-job")["category"], "gpu");
        assert_eq!(v["counts"]["wasm"], 1);
        assert_eq!(v["counts"]["gpu"], 1);
        // 节点上的 GPU 能力也要标出来（device plugin 的 capacity 优先）
        let n1 = v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["id"] == "node:n1")
            .cloned()
            .unwrap();
        assert_eq!(n1["meta"]["gpu"], true);
        assert_eq!(n1["meta"]["gpuCount"], 1);
        assert_eq!(n1["meta"]["pods"], 2);
    }

    #[test]
    fn include_pods_adds_pod_nodes_and_belongs_to_edges() {
        let mut inp = input();
        inp.include_pods = true;
        let v = build_topology(&inp);
        assert!(ids(&v, "pod").contains(&"pod:k3s-wasm/ui-abc".to_string()));
        assert!(has_edge(&v, "pod:k3s-wasm/ui-abc", "workload:k3s-wasm/ui", "belongs-to"));
        assert!(has_edge(&v, "pod:k3s-wasm/ui-abc", "node:n1", "runs-on"));
        assert_eq!(v["scope"]["includePods"], true);
    }

    #[test]
    fn namespace_filter_keeps_only_that_namespace() {
        let mut inp = input();
        inp.namespace = "other".to_string();
        let v = build_topology(&inp);
        assert!(ids(&v, "workload").is_empty());
        assert!(ids(&v, "service").is_empty());
        // 节点是集群级的，仍然要在（否则图没有落脚点）
        assert_eq!(ids(&v, "node").len(), 2);
    }

    #[test]
    fn revision_changes_when_counts_change() {
        let a = build_topology(&input());
        let mut inp = input();
        // 只改副本数：id 集合不变，但指纹必须变（否则前端会跳过重绘）
        inp.deployments[0]["status"]["readyReplicas"] = json!(0);
        let b = build_topology(&inp);
        assert_ne!(a["revision"], b["revision"]);
        // 完全相同的输入 → 完全相同的指纹（前端靠它跳过无意义的重绘）
        let c = build_topology(&input());
        assert_eq!(a["revision"], c["revision"]);
    }

    #[test]
    fn spinapp_workload_is_wasm_without_runtime_class() {
        let mut inp = input();
        inp.deployments.clear();
        inp.pods.clear();
        inp.services.clear();
        inp.ingresses.clear();
        inp.network_policies.clear();
        inp.spinapps = vec![json!({
            "metadata": { "name": "hello", "namespace": "k3s-wasm" },
            "spec": { "image": "ghcr.io/x/hello:1", "replicas": 2 },
            "status": { "readyReplicas": 2 },
        })];
        let v = build_topology(&inp);
        let w = v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["id"] == "workload:k3s-wasm/hello")
            .cloned()
            .unwrap();
        assert_eq!(w["category"], "wasm");
        assert_eq!(w["meta"]["workloadKind"], "SpinApp");
        assert_eq!(w["meta"]["ready"], 2);
    }
}
