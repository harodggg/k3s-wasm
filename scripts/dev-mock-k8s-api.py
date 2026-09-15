#!/usr/bin/env python3
"""dev-mock-k8s-api.py —— 一个假的 Kubernetes API，用来在没有集群时跑控制台。

存在的意义：k3s 的 wasm 运行时链路（containerd → shim → wasm）只在 Linux 节点上成立，
但**组件本身的逻辑**（路由、JSON 形态、错误处理）可以在笔记本上验证。
这个脚本提供 kubectl proxy 那一侧的最小实现，于是：

    ./scripts/dev-mock-k8s-api.py --port 8001 &
    wasmtime serve -S http=y -S inherit-network=y -S inherit-env=y \
        --addr 127.0.0.1:8080 ui/backend/target/wasm32-wasip2/release/k3s_wasm_ui.wasm
    # 浏览器打开 http://127.0.0.1:8080

只依赖标准库。写操作放在内存里，重启即恢复。

用 `--no-spinapp-crd` 可以让 SpinApp 接口返回 404，
用来验证前端「SpinKube 未安装」的提示路径。
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

# ── 假数据 ──────────────────────────────────────────────────────────

NODES = [
    {
        "metadata": {
            "name": "k3s-node-1",
            "creationTimestamp": "2026-09-01T02:00:00Z",
            "labels": {
                "node-role.kubernetes.io/control-plane": "true",
                "wasm.sh/spin": "true",
                "wasm.sh/wasmtime": "true",
                "kubernetes.io/arch": "arm64",
            },
        },
        "spec": {"unschedulable": False},
        "status": {
            "conditions": [{"type": "Ready", "status": "True"}],
            "nodeInfo": {
                "architecture": "arm64",
                "operatingSystem": "linux",
                "kubeletVersion": "v1.33.1+k3s1",
                "containerRuntimeVersion": "containerd://2.0.0-k3s1",
            },
            "capacity": {"cpu": "8", "memory": "16384000Ki", "pods": "110"},
            "addresses": [{"type": "InternalIP", "address": "10.0.0.11"}],
        },
    },
    {
        # 故意造一个「没装 shim」的节点：前端应当显示出能力不齐
        "metadata": {
            "name": "k3s-node-2",
            "creationTimestamp": "2026-09-02T02:00:00Z",
            "labels": {"kubernetes.io/arch": "arm64"},
        },
        "spec": {"unschedulable": False},
        "status": {
            "conditions": [{"type": "Ready", "status": "True"}],
            "nodeInfo": {
                "architecture": "arm64",
                "operatingSystem": "linux",
                "kubeletVersion": "v1.33.1+k3s1",
                "containerRuntimeVersion": "containerd://2.0.0-k3s1",
            },
            "capacity": {"cpu": "4", "memory": "8192000Ki", "pods": "110"},
            "addresses": [{"type": "InternalIP", "address": "10.0.0.12"}],
        },
    },
]

NS = ["k3s-wasm", "default", "kube-system"]

PODS = [
    {
        "metadata": {"name": "k3s-wasm-ui-7d9f8b6c5-abcde", "namespace": "k3s-wasm",
                     "creationTimestamp": "2026-09-15T01:00:00Z", "labels": {"app.kubernetes.io/name": "k3s-wasm-ui"}},
        "spec": {"nodeName": "k3s-node-1", "runtimeClassName": "wasmtime-spin-v2",
                 "containers": [{"name": "ui", "image": "k3s-wasm/k3s-wasm-ui:dev"}]},
        "status": {"phase": "Running", "podIP": "10.42.0.7", "startTime": "2026-09-15T01:00:05Z",
                   "containerStatuses": [{"name": "ui", "ready": True, "restartCount": 0}]},
    },
    {
        "metadata": {"name": "spin-hello-6b7c8d9e0-fghij", "namespace": "k3s-wasm",
                     "creationTimestamp": "2026-09-15T02:00:00Z"},
        "spec": {"nodeName": "k3s-node-1", "runtimeClassName": "wasmtime-spin-v2",
                 "containers": [{"name": "spin", "image": "ghcr.io/example/spin-hello:0.1.0"}]},
        "status": {"phase": "Running", "podIP": "10.42.0.9", "startTime": "2026-09-15T02:00:04Z",
                   "containerStatuses": [{"name": "spin", "ready": True, "restartCount": 1}]},
    },
    {
        "metadata": {"name": "kube-api-proxy-5f6a7b8c9-klmno", "namespace": "k3s-wasm",
                     "creationTimestamp": "2026-09-15T01:00:00Z",
                     "labels": {"app.kubernetes.io/name": "kube-api-proxy"}},
        "spec": {"nodeName": "k3s-node-2",
                 "containers": [{"name": "proxy", "image": "registry.k8s.io/kubectl:v1.33.0"}]},
        "status": {"phase": "Running", "podIP": "10.42.1.3", "startTime": "2026-09-15T01:00:03Z",
                   "containerStatuses": [{"name": "proxy", "ready": True, "restartCount": 0}]},
    },
]

RUNTIMECLASSES = [
    {"metadata": {"name": "wasmtime-spin-v2"}, "handler": "spin",
     "scheduling": {"nodeSelector": {"wasm.sh/spin": "true"}}},
    {"metadata": {"name": "wasmtime"}, "handler": "wasmtime",
     "scheduling": {"nodeSelector": {"wasm.sh/wasmtime": "true"}}},
    {"metadata": {"name": "runc"}, "handler": "runc"},
]

DEPLOYMENTS = [
    {
        "metadata": {"name": "k3s-wasm-ui", "namespace": "k3s-wasm",
                     "creationTimestamp": "2026-09-15T01:00:00Z",
                     "labels": {"app.kubernetes.io/name": "k3s-wasm-ui",
                                "app.kubernetes.io/part-of": "k3s-wasm"}},
        "spec": {"replicas": 1, "template": {"spec": {
            "runtimeClassName": "wasmtime-spin-v2",
            "containers": [{"name": "ui", "image": "k3s-wasm/k3s-wasm-ui:dev"}]}}},
        "status": {"readyReplicas": 1, "availableReplicas": 1},
    },
    {
        "metadata": {"name": "coredns", "namespace": "kube-system",
                     "creationTimestamp": "2026-09-01T02:00:00Z"},
        "spec": {"replicas": 2, "template": {"spec": {
            "containers": [{"name": "coredns", "image": "rancher/mirrored-coredns-coredns:1.11.1"}]}}},
        "status": {"readyReplicas": 2, "availableReplicas": 2},
    },
]

SPINAPPS = [
    {
        "metadata": {"name": "spin-hello", "namespace": "k3s-wasm",
                     "creationTimestamp": "2026-09-15T02:00:00Z"},
        "spec": {"image": "ghcr.io/example/spin-hello:0.1.0", "replicas": 1,
                 "runtimeClassName": "wasmtime-spin-v2",
                 "variables": [{"name": "greeting", "value": "hi"}]},
        "status": {"readyReplicas": 1, "conditions": [
            {"type": "Ready", "status": "True", "reason": "DeploymentReady"}]},
    },
]

CONFIGMAPS = [
    {
        "metadata": {"name": "tokyo-config", "namespace": "k3s-wasm",
                     "labels": {"app.kubernetes.io/part-of": "xray-wasm",
                                "app.kubernetes.io/managed-by": "k3s-wasm-ui"}},
        "data": {"tunnel.json": json.dumps({
            "server": "203.0.113.10:443", "uuid": "11111111-2222-3333-4444-555555555555",
            "publicKey": "PUBKEYPUBKEYPUBKEYPUBKEYPUBKEYPUBKEYPUBKEY0=",
            "shortId": "9f1c2a3b", "sni": "www.amazon.com", "listen": "0.0.0.0:1080",
        }, ensure_ascii=False)},
    },
]

# xray 隧道在工作负载列表里以 Deployment 的形式存在
XRAY_DEPLOYMENTS = [
    {
        "metadata": {"name": "tokyo", "namespace": "k3s-wasm",
                     "creationTimestamp": "2026-09-15T03:00:00Z",
                     "labels": {"app.kubernetes.io/name": "xray-tokyo",
                                "app.kubernetes.io/instance": "tokyo",
                                "app.kubernetes.io/part-of": "xray-wasm",
                                "app.kubernetes.io/managed-by": "k3s-wasm-ui"}},
        "spec": {"replicas": 1, "template": {"spec": {
            "runtimeClassName": "wasmtime",
            "containers": [{"name": "tunnel", "image": "k3s-wasm/xray-wasm-cli:dev"}]}}},
        "status": {"readyReplicas": 0},
    }
]

EVENTS = [
    {"type": "Normal", "reason": "Scheduled", "involvedObject": {"kind": "Pod", "name": "spin-hello-6b7c8d9e0-fghij"},
     "message": "Successfully assigned k3s-wasm/spin-hello to k3s-node-1", "count": 1,
     "lastTimestamp": "2026-09-15T02:00:01Z"},
    {"type": "Warning", "reason": "FailedCreate", "involvedObject": {"kind": "Pod", "name": "tokyo-abc123"},
     "message": "failed to create shim task: no runtime for io.containerd.wasmtime.v1 is configured on node k3s-node-2",
     "count": 4, "lastTimestamp": "2026-09-15T03:00:20Z"},
]

LOG_SAMPLE = """xt-wasm-cli: 隧道层尚未接入（等待 xt-wasm-tls / xt-wasm-vless 移植完成）
exit code 2
"""


class Handler(BaseHTTPRequestHandler):
    server_version = "mock-k8s/1.0"
    protocol_version = "HTTP/1.1"

    # 写操作（POST 出来的对象）放内存，便于验证创建流程
    created: dict[str, dict] = {}

    def log_message(self, fmt: str, *args) -> None:  # 静音，避免刷屏
        if self.server.verbose:  # type: ignore[attr-defined]
            sys.stderr.write("mock-k8s: " + fmt % args + "\n")

    # ── 工具 ──
    def send_json(self, obj, status=200) -> None:
        body = json.dumps(obj, ensure_ascii=False).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def send_text(self, text: str, status=200) -> None:
        body = text.encode()
        self.send_response(status)
        self.send_header("content-type", "text/plain; charset=utf-8")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def failure(self, status: int, reason: str, message: str) -> None:
        self.send_json({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                        "message": message, "reason": reason, "code": status}, status)

    def items(self, lst) -> dict:
        return {"kind": "List", "apiVersion": "v1", "items": lst}

    def all_deployments(self):
        """静态样例 + POST 出来的。PATCH/DELETE 也必须看到新建的对像，
        否则「创建成功但扩容 404」这种 mock 自身的缺陷会伪装成组件问题。"""
        return DEPLOYMENTS + XRAY_DEPLOYMENTS + list(self.created.values())

    def read_body(self):
        """读请求体。

        ⚠️ 这里必须同时支持 content-length 与 **chunked**：
        wasi:http 的出站请求经常用 chunked（不带 content-length）。
        只按 content-length 读的话，POST/PATCH 的 body 会静默变成空，
        于是 mock 报「缺字段」——而真正的问题在测试脚手架，
        现象却像组件发了个空 body，特别容易查错方向。
        """
        te = (self.headers.get("transfer-encoding") or "").lower()
        if "chunked" in te:
            raw = self._read_chunked()
            if self.server.verbose:  # type: ignore[attr-defined]
                sys.stderr.write(f"mock-k8s: <- {self.command} {self.path} chunked body={len(raw)}B\n")
        else:
            length = int(self.headers.get("content-length") or 0)
            if self.server.verbose:  # type: ignore[attr-defined]
                sys.stderr.write(f"mock-k8s: <- {self.command} {self.path} body={length}B\n")
            raw = self.rfile.read(length) if length else b""
        if not raw:
            return None
        try:
            return json.loads(raw)
        except json.JSONDecodeError:
            return None

    def _read_chunked(self) -> bytes:
        out = b""
        while True:
            line = self.rfile.readline().strip()
            if not line:
                break
            size = int(line.split(b";")[0], 16)
            if size == 0:
                # 吃掉 trailer 与结尾空行
                while True:
                    trailer = self.rfile.readline()
                    if trailer in (b"\r\n", b"\n", b""):
                        break
                break
            out += self.rfile.read(size)
            self.rfile.read(2)  # CRLF
        return out

    def selector(self):
        qs = parse_qs(urlparse(self.path).query)
        raw = (qs.get("labelSelector") or [""])[0]
        out = {}
        for part in filter(None, raw.split(",")):
            if "=" in part:
                k, v = part.split("=", 1)
                out[k] = v
        return out

    def matches(self, obj, sel) -> bool:
        labels = (obj.get("metadata", {}).get("labels") or {})
        return all(labels.get(k) == v for k, v in sel.items())

    # ── 路由 ──
    def do_GET(self) -> None:  # noqa: N802
        parsed = urlparse(self.path)
        path = parsed.path
        qs = parse_qs(parsed.query)
        sel = self.selector()
        ns = (qs.get("namespace") or [""])[0]

        if path == "/version":
            return self.send_json({"major": "1", "minor": "33",
                                   "gitVersion": "v1.33.1+k3s1", "platform": "linux/arm64"})
        if path == "/api/v1/nodes":
            return self.send_json(self.items(NODES))
        if path == "/api/v1/namespaces":
            return self.send_json(self.items(
                [{"metadata": {"name": n}, "status": {"phase": "Active"}} for n in NS]))
        if path == "/api/v1/pods":
            pods = [p for p in PODS if self.matches(p, sel)]
            if ns:
                pods = [p for p in pods if p["metadata"]["namespace"] == ns]
            return self.send_json(self.items(pods))
        if re.fullmatch(r"/api/v1/namespaces/[^/]+/pods", path):
            real = path.split("/")[4]
            return self.send_json(self.items(
                [p for p in PODS if p["metadata"]["namespace"] == real and self.matches(p, sel)]))
        if re.fullmatch(r"/api/v1/namespaces/[^/]+/pods/[^/]+/log", path):
            return self.send_text(LOG_SAMPLE)
        if re.fullmatch(r"/api/v1/namespaces/[^/]+/events", path):
            return self.send_json(self.items(EVENTS))
        if path == "/api/v1/configmaps":
            return self.send_json(self.items([c for c in CONFIGMAPS if self.matches(c, sel)]))
        if re.fullmatch(r"/api/v1/namespaces/[^/]+/configmaps", path):
            real = path.split("/")[4]
            return self.send_json(self.items(
                [c for c in CONFIGMAPS
                 if obj_ns(c) == real and self.matches(c, sel)]))
        if path == "/apis/node.k8s.io/v1/runtimeclasses":
            return self.send_json(self.items(RUNTIMECLASSES))
        if path == "/apis/apps/v1/deployments":
            return self.send_json(self.items([d for d in self.all_deployments() if self.matches(d, sel)]))
        if re.fullmatch(r"/apis/apps/v1/namespaces/[^/]+/deployments", path):
            real = path.split("/")[5]
            return self.send_json(self.items(
                [d for d in self.all_deployments()
                 if obj_ns(d) == real and self.matches(d, sel)]))
        if path == "/apis/core.spinkube.dev/v1alpha1/spinapps":
            if self.server.no_spinapp_crd:  # type: ignore[attr-defined]
                return self.failure(
                    404, "NotFound",
                    'the server could not find the requested resource (get spinapps.core.spinkube.dev)')
            return self.send_json(self.items(SPINAPPS))
        if re.fullmatch(r"/apis/core.spinkube.dev/[^/]+/namespaces/[^/]+/spinapps", path):
            if self.server.no_spinapp_crd:  # type: ignore[attr-defined]
                return self.failure(
                    404, "NotFound",
                    'the server could not find the requested resource (get spinapps.core.spinkube.dev)')
            real = path.split("/")[5]
            return self.send_json(self.items(
                [a for a in SPINAPPS if obj_ns(a) == real]))
        if path == "/apis/core.spinkube.dev/v1alpha1/spinappexecutors":
            return self.send_json(self.items([]))

        return self.failure(404, "NotFound", f"the server could not find the requested resource: {path}")

    def do_POST(self) -> None:  # noqa: N802
        parsed = urlparse(self.path)
        path = parsed.path
        body = self.read_body() or {}
        if path.endswith("/spinapps"):
            if self.server.no_spinapp_crd:  # type: ignore[attr-defined]
                return self.failure(
                    404, "NotFound",
                    'the server could not find the requested resource (post spinapps.core.spinkube.dev)')
            body.setdefault("metadata", {}).setdefault("creationTimestamp", "2026-09-15T04:00:00Z")
            body.setdefault("status", {"readyReplicas": 0})
            if not obj_name(body):
                return self.failure(422, "Invalid", "mock: SpinApp 必须带 metadata.name")
            SPINAPPS.append(body)
            return self.send_json(body, 201)
        if "/deployments" in path:
            body.setdefault("metadata", {}).setdefault("creationTimestamp", "2026-09-15T04:00:00Z")
            body.setdefault("status", {"readyReplicas": 0})
            key = f"{body['metadata'].get('namespace')}/{body['metadata'].get('name')}"
            self.created[key] = body
            return self.send_json(body, 201)
        if "/configmaps" in path or "/services" in path or "/secrets" in path:
            body.setdefault("metadata", {}).setdefault("creationTimestamp", "2026-09-15T04:00:00Z")
            return self.send_json(body, 201)

        return self.failure(404, "NotFound", f"mock 未实现该 POST：{path}")

    def do_PATCH(self) -> None:  # noqa: N802
        parsed = urlparse(self.path)
        path = parsed.path
        body = self.read_body() or {}
        if re.search(r"/spinapps/[^/]+$", path):
            name = path.rsplit("/", 1)[1]
            for app in SPINAPPS:
                if obj_name(app) == name:
                    app["spec"]["replicas"] = body.get("spec", {}).get("replicas", app["spec"].get("replicas"))
                    return self.send_json(app)
            return self.failure(404, "NotFound", f"spinapp {name} not found")
        if re.search(r"/deployments/[^/]+$", path):
            name = path.rsplit("/", 1)[1]
            for dep in self.all_deployments():
                if obj_name(dep) == name:
                    dep["spec"]["replicas"] = body.get("spec", {}).get("replicas", dep["spec"]["replicas"])
                    return self.send_json(dep)
            return self.failure(404, "NotFound", f"deployment {name} not found")
        return self.failure(404, "NotFound", f"mock 未实现该 PATCH：{path}")

    def do_DELETE(self) -> None:  # noqa: N802
        parsed = urlparse(self.path)
        path = parsed.path
        if re.search(r"/spinapps/[^/]+$", path):
            name = path.rsplit("/", 1)[1]
            for i, app in enumerate(SPINAPPS):
                if obj_name(app) == name:
                    SPINAPPS.pop(i)
                    return self.send_json({"kind": "Status", "status": "Success"})
            return self.failure(404, "NotFound", f"spinapp {name} not found")
        if re.search(r"/deployments/[^/]+$", path):
            name = path.rsplit("/", 1)[1]
            matched = [d for d in self.all_deployments() if obj_name(d) == name]
            if not matched:
                return self.failure(404, "NotFound", f"deployment {name} not found")
            # 运行时创建的要从 created 里摘掉，否则删完还在列表里
            for key in [k for k, v in self.created.items() if obj_name(v) == name]:
                del self.created[key]
            return self.send_json({"kind": "Status", "status": "Success"})
        if "/configmaps/" in path or "/services/" in path:
            return self.send_json({"kind": "Status", "status": "Success"})
        return self.failure(404, "NotFound", f"mock 未实现该 DELETE：{path}")


def obj_name(obj) -> str:
    return ((obj or {}).get("metadata") or {}).get("name", "")


def obj_ns(obj) -> str:
    return ((obj or {}).get("metadata") or {}).get("namespace", "")


def main() -> int:
    ap = argparse.ArgumentParser(description="假的 Kubernetes API（给控制台本地开发用）")
    ap.add_argument("--port", type=int, default=8001)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--no-spinapp-crd", action="store_true",
                    help="让 SpinApp 接口返回 404，验证「SpinKube 未安装」提示")
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()

    server = ThreadingHTTPServer((args.host, args.port), Handler)
    server.no_spinapp_crd = args.no_spinapp_crd  # type: ignore[attr-defined]
    server.verbose = args.verbose  # type: ignore[attr-defined]
    print(f"mock-k8s: listening on http://{args.host}:{args.port} "
          f"(spinapp-crd={'no' if args.no_spinapp_crd else 'yes'})", file=sys.stderr)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("mock-k8s: bye", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
