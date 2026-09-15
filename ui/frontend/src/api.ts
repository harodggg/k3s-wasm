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
  version: string;
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
  /** 永远是对象（后端保证不发 null）；空对象表示没有 selector */
  nodeSelector: Record<string, string>;
  /** null = 无法判定（RuntimeClass 没有 nodeSelector，装没装看标签看不出来） */
  capableNodes: number | null;
  selectorless: boolean;
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

/** 这条隧道的「入站面」：谁能连进它的监听端口（由 Service 类型决定，后端读实际的 Service） */
export interface TunnelExposure {
  serviceType: string | null;
  nodePort: number | null;
  port: number;
  reach: string;
  /** true=公网可达，false=仅集群内，null=取不到 Service */
  public: boolean | null;
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
  /** 目前恒为 "egress"：xray-wasm 是客户端，只能把集群内流量送出去 */
  direction: string;
  directionLabel: string;
  egress: { via: string | null; protocol: string; note: string };
  ingress: { endpoint: string; exposure: TunnelExposure };
  /** 谁可以用：outbound=集群内 Pod；inbound=外部经节点IP（要求已对外暴露） */
  usage: { outbound: boolean; inbound: boolean; allowFrom: string | null };
}

/** 自动生成的一整套隧道参数（含只出现一次的私钥） */
export interface GeneratedTunnel {
  name: string;
  server: string;
  uuid: string;
  publicKey: string;
  shortId: string;
  sni: string;
  socksUser: string;
  socksPass: string;
  privateKey: string;
  vlessLink: string;
  serverConfig: Record<string, unknown>;
  clientConfig: Record<string, unknown>;
  usage: string;
  /** 出站：集群内 Pod 怎么用它 */
  outbound: Record<string, unknown>;
  /** 入站：外部/本机怎么用它（未开放外部时是一段说明） */
  inbound: Record<string, unknown>;
  notes: string[];
}

export interface GenerateTunnelBody {
  server?: string;
  sni?: string;
  name?: string;
  socksUser?: string;
  /** cluster = 只给集群内 Pod 用（出站）；nodeport = 也要给外部/本机用（入站） */
  usage?: 'cluster' | 'nodeport';
}

/** 按需重建出来的 vless 链接（不含 REALITY 私钥） */
export interface TunnelVless {
  name: string;
  namespace: string;
  vlessLink: string;
  server: string;
  sni: string;
  shortId: string;
  publicKey: string;
  socksUser: string;
  socksEndpoint: string;
  note: string;
}

export interface XrayList {
  items: XrayTunnel[];
  shim: { runtimeClass: string; requiredNodeLabel: string };
}

/** 集群里正在运行的 Pod 所用镜像（下拉建议的来源） */
export interface ImageInfo {
  image: string;
  count: number;
  wasmCount: number;
  runtimeClasses: string[];
  isWasm: boolean;
}

export interface ImageList {
  wasmOnly: boolean;
  items: ImageInfo[];
  defaults: string[];
  hint: string;
}

/** 镜像版本 tag（来自 GitHub releases，用于"按版本下拉"） */
export interface ImageTag {
  tag: string;
  name: string;
  prerelease: boolean;
  publishedAt: string | null;
}

export interface ImageTagList {
  repo: string;
  tags: ImageTag[];
  error?: string;
  hint?: string;
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
  /** 可直接粘贴 xray-deploy 输出的 vless:// 链接，server/uuid/pbk/sid/sni 会自动补全 */
  vlessLink?: string;
  server: string;
  uuid: string;
  publicKey: string;
  shortId?: string;
  sni?: string;
  /** SOCKS5 认证：绑 0.0.0.0 时**必填**（否则就是开放代理） */
  socksUser: string;
  socksPass: string;
  /** 引用一个你预先建好的 Secret（给了它就不用控制台创建 Secret） */
  secretName?: string;
  clientVer?: string;
  listen?: string;
  replicas?: number;
  image?: string;
  runtimeClassName?: string;
  /** cluster = 仅集群内（出站）；nodeport = 允许外部经节点IP使用（入站） */
  expose?: 'cluster' | 'nodeport';
  nodePort?: number;
  /** 外部用途时的放行来源 CIDR，默认 0.0.0.0/0（=公开，强烈建议收窄） */
  allowFrom?: string;
}

/** 创建隧道后的连接方式（后端从请求 Host 推出节点地址） */
export interface CreatedTunnel {
  created: boolean;
  namespace: string;
  name: string;
  secret: string;
  socksEndpoint: string;
  portForward: string;
  expose: 'cluster' | 'nodeport';
  externalEndpoint: string | null;
  allowFrom: string | null;
  usage: string;
  note: string;
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
  /** wasmOnly=false 时把系统组件镜像也列出来 */
  images: (wasmOnly = true) => request<ImageList>(`/api/images${q({ wasmOnly: wasmOnly ? '1' : '0' })}`),
  imageTags: (repo = 'harodggg/xray-wasm') =>
    request<ImageTagList>(`/api/image-tags${q({ repo })}`),
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
  generateTunnel: (body: GenerateTunnelBody) => post<GeneratedTunnel>('/api/xray/generate', body),
  createTunnel: (body: CreateTunnelBody) => post<CreatedTunnel>('/api/xray/tunnels', body),
  scaleTunnel: (ns: string, name: string, replicas: number) =>
    post<unknown>(`/api/xray/tunnels/${encodeURIComponent(ns)}/${encodeURIComponent(name)}/scale`, { replicas }),
  tunnelVless: (ns: string, name: string) =>
    request<TunnelVless>(
      `/api/xray/tunnels/${encodeURIComponent(ns)}/${encodeURIComponent(name)}/vless`,
    ),
  deleteTunnel: (ns: string, name: string) =>
    del<unknown>(`/api/xray/tunnels/${encodeURIComponent(ns)}/${encodeURIComponent(name)}`),
};
