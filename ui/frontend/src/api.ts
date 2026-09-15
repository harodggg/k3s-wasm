// 后端 API 客户端。
//
// 后端统一返回信封 { ok: true, data } / { ok: false, error }，
// 所以这里只需要一个 fetch 包装：要么拿到 data，要么抛 ApiError。
// 上游 k8s 的错误信息（比如 RBAC 拒绝的具体原因）会被后端原样带出来，
// 这里也就原样往界面上抛 —— 排障时这比 "请求失败" 有用得多。

export interface ApiErrorBody {
  message: string;
  status: number;
}

export class ApiError extends Error {
  readonly status: number;
  constructor(message: string, status: number) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

type Envelope<T> = { ok: true; data: T } | { ok: false; error: ApiErrorBody };

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let resp: Response;
  try {
    resp = await fetch(path, {
      headers: { accept: 'application/json' },
      ...init,
    });
  } catch (e) {
    throw new ApiError(`请求 ${path} 失败：${(e as Error).message}`, 0);
  }

  let payload: Envelope<T>;
  try {
    payload = (await resp.json()) as Envelope<T>;
  } catch {
    throw new ApiError(`${path} 返回的不是 JSON（HTTP ${resp.status}）`, resp.status);
  }

  if (!payload.ok) {
    throw new ApiError(payload.error.message, payload.error.status);
  }
  return payload.data;
}

function post<T>(path: string, body: unknown): Promise<T> {
  return request<T>(path, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'application/json' },
    body: JSON.stringify(body),
  });
}

function del<T>(path: string): Promise<T> {
  return request<T>(path, { method: 'DELETE', headers: { accept: 'application/json' } });
}

// ── 类型 ────────────────────────────────────────────────────────────

export interface Health {
  status: string;
  component: string;
  target: string;
  proxyUrl: string;
  proxyUrlSource: string;
  defaultNamespace: string;
}

export interface ClusterConfig {
  proxyUrl: string;
  proxyUrlSource: string;
  defaultNamespace: string;
}

export interface Summary {
  version: { gitVersion?: string; platform?: string } | null;
  nodes: { total: number; ready: number; wasmCapable: number };
  pods: { total: number; running: number; pending: number; failed: number; succeeded: number; wasm: number };
  namespaces: number;
  runtimes: RuntimeInfo[];
  spinapps: { installed: boolean | null; count: number; error?: string };
  xrayTunnels: number;
  config: ClusterConfig;
}

export interface NodeInfo {
  name: string;
  ready: boolean;
  unschedulable: boolean;
  arch: string;
  os: string;
  kubeletVersion: string;
  runtime: string;
  capacity: { cpu: string; memory: string; pods: string };
  wasm: { spin: boolean; wasmtime: boolean };
  labels: Record<string, string>;
}

export interface RuntimeInfo {
  name: string;
  handler: string;
  isWasm: boolean;
  nodeSelector: Record<string, string>;
  capableNodes: number;
  misconfigured: boolean;
}

export interface Workload {
  kind: string;
  name: string;
  namespace: string;
  replicas: number;
  readyReplicas: number;
  availableReplicas: number;
  image: string | null;
  runtimeClass: string;
  isWasm: boolean;
  createdAt: string;
}

export interface PodInfo {
  name: string;
  namespace: string;
  node: string;
  phase: string;
  podIP: string | null;
  ready: string;
  restarts: number;
  image: string | null;
  runtimeClass: string;
  isWasm: boolean;
  startedAt: string | null;
  createdAt: string;
}

export interface SpinApp {
  kind: string;
  name: string;
  namespace: string;
  image: string;
  replicas: number;
  readyReplicas: number;
  runtimeClass: string | null;
  executor: string | null;
  variables: unknown;
  conditions: unknown;
  createdAt: string;
  isWasm: boolean;
}

export interface SpinAppList {
  installed: boolean;
  apiVersion: string;
  items: SpinApp[];
  hint?: string;
}

/** SpinAppExecutor：SpinApp 的 runtimeClassName / 镜像等来自它，不是 SpinApp 自己的字段 */
export interface SpinAppExecutorInfo {
  name: string;
  createDeployment: boolean;
  runtimeClassName: string | null;
  spinImage: string | null;
}

export interface SpinAppExecutorList {
  installed: boolean;
  items: SpinAppExecutorInfo[];
  hint?: string;
}

export interface TunnelView {
  server: string | null;
  sni: string | null;
  shortId: string | null;
  listen: string | null;
  hasUuid: boolean;
}

export interface XrayTunnel {
  kind: string;
  name: string;
  namespace: string;
  replicas: number;
  readyReplicas: number;
  image: string | null;
  runtimeClass: string;
  createdAt: string;
  tunnel: TunnelView;
  managedBy: string;
  isWasm: boolean;
}

export interface XrayList {
  items: XrayTunnel[];
  shim: { runtimeClass: string; requiredNodeLabel: string };
}

export interface NamespaceInfo {
  name: string;
  phase: string;
}

export interface ClusterEvent {
  type: string;
  reason: string;
  object: string;
  message: string;
  count: number;
  lastSeen: string;
}

export interface LogResult {
  namespace: string;
  pod: string;
  tailLines: number;
  log: string;
}

export interface CreateSpinAppBody {
  name: string;
  namespace?: string;
  image: string;
  replicas?: number;
  runtimeClassName?: string;
  executor?: string;
  apiVersion?: string;
  variables?: Record<string, string>;
  spec?: Record<string, unknown>;
}

export interface CreateTunnelBody {
  name: string;
  namespace?: string;
  server: string;
  uuid: string;
  publicKey: string;
  shortId?: string;
  sni?: string;
  listen?: string;
  replicas?: number;
  image?: string;
  runtimeClassName?: string;
}

// ── 接口 ────────────────────────────────────────────────────────────

const q = (params: Record<string, string | undefined>): string => {
  const usp = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== '') usp.set(k, v);
  }
  const s = usp.toString();
  return s ? `?${s}` : '';
};

export const api = {
  health: () => request<Health>('/api/health'),
  summary: () => request<Summary>('/api/summary'),
  nodes: () => request<NodeInfo[]>('/api/nodes'),
  runtimes: () => request<RuntimeInfo[]>('/api/runtimes'),
  namespaces: () => request<NamespaceInfo[]>('/api/namespaces'),
  workloads: (namespace?: string) => request<Workload[]>(`/api/workloads${q({ namespace })}`),
  pods: (namespace?: string, node?: string) => request<PodInfo[]>(`/api/pods${q({ namespace, node })}`),
  events: (namespace?: string) => request<ClusterEvent[]>(`/api/events${q({ namespace })}`),
  logs: (ns: string, name: string, tail = 200) =>
    request<LogResult>(`/api/pods/${encodeURIComponent(ns)}/${encodeURIComponent(name)}/logs${q({ tail: String(tail) })}`),

  spinapps: (namespace?: string, apiVersion?: string) =>
    request<SpinAppList>(`/api/spinapps${q({ namespace, apiVersion })}`),
  executors: (namespace?: string) =>
    request<SpinAppExecutorList>(`/api/spinapp-executors${q({ namespace })}`),
  createSpinApp: (body: CreateSpinAppBody) => post<unknown>('/api/spinapps', body),
  scaleSpinApp: (ns: string, name: string, replicas: number, apiVersion?: string) =>
    post<SpinApp>(
      `/api/spinapps/${encodeURIComponent(ns)}/${encodeURIComponent(name)}/scale`,
      { replicas, apiVersion },
    ),
  deleteSpinApp: (ns: string, name: string, apiVersion?: string) =>
    del<unknown>(`/api/spinapps/${encodeURIComponent(ns)}/${encodeURIComponent(name)}${q({ apiVersion })}`),

  tunnels: (namespace?: string) => request<XrayList>(`/api/xray/tunnels${q({ namespace })}`),
  createTunnel: (body: CreateTunnelBody) => post<unknown>('/api/xray/tunnels', body),
  scaleTunnel: (ns: string, name: string, replicas: number) =>
    post<unknown>(`/api/xray/tunnels/${encodeURIComponent(ns)}/${encodeURIComponent(name)}/scale`, { replicas }),
  deleteTunnel: (ns: string, name: string) =>
    del<unknown>(`/api/xray/tunnels/${encodeURIComponent(ns)}/${encodeURIComponent(name)}`),
};
