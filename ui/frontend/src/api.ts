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

/** Pod/工作负载的运行时类别计数（/api/summary 的 runtimeCategories） */
export interface RuntimeCategoryCounts {
  wasm: number;
  native: number;
  gpu: number;
  total: number;
}

export interface Summary {
  version: { gitVersion?: string; platform?: string } | null;
  nodes: { total: number; ready: number; wasmCapable: number };
  pods: { total: number; running: number; pending: number; failed: number; succeeded: number; wasm: number };
  /** 对 summary 已经检查过的 Pod 按运行时类别计数 */
  runtimeCategories?: RuntimeCategoryCounts;
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
  /** k8s 的 NodeStatus.addresses；翻墙模式要从中取 InternalIP 展示给用户 */
  addresses?: { type: string; address: string }[];
  capacity: { cpu: string; memory: string; pods: string };
  wasm: { spin: boolean; wasmtime: boolean };
  /** GPU 存在性：由节点标签/容量推导（present=false 时 count 恒为 0） */
  gpu?: { present: boolean; count: number };
  labels: Record<string, string>;
}

export interface RuntimeInfo {
  name: string;
  handler: string;
  /** 运行时类别：wasm | native | gpu */
  category?: string;
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
  /** 运行时类别：wasm | native | gpu */
  runtimeCategory?: string;
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
  /** 运行时类别：wasm | native | gpu */
  runtimeCategory?: string;
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

/** 两种模式用同一个 wasm 组件：翻墙跑服务端（REALITY 入站），隧道跑客户端（SOCKS5 入站） */
export type TunnelMode = 'walljump' | 'tunnel';

/** GET /api/xray/tunnels 的 modes[]：模式语义由后端定义，面板直接渲染，避免前后端各写一套 */
export interface XrayModeInfo {
  id: string;
  label: string;
  impl?: string;
  runtimeClass?: string;
  entry?: string;
  who?: string;
  nodeRequirement?: string;
  flowNote?: string;
}

export interface XrayTunnel {
  kind: string;
  /** walljump = 入站 REALITY → 直连出；tunnel = SOCKS5 入站 → 经 REALITY 出 */
  mode?: TunnelMode;
  name: string;
  namespace: string;
  replicas: number;
  readyReplicas: number;
  image: string | null;
  runtimeClass: string;
  createdAt: string;
  impl?: string;
  /** 仅翻墙：入口（NodePort）所在的节点 */
  node?: string | null;
  tunnel: TunnelView;
  managedBy: string;
  isWasm: boolean;
  /** ingress = 翻墙（公网 REALITY 入口）；egress = 隧道（集群内流量经 REALITY 出网） */
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
  mode?: TunnelMode;
  impl?: string;
  /** 客户端 config.json；翻墙模式下不含 flow（服务端未实现 XTLS-Vision 流控） */
  clientConfig?: Record<string, unknown>;
  vlessLink: string;
  server: string;
  /** 客户端实际连的地址，可能不同于链接里的对外地址 */
  clientServer?: string;
  sni: string;
  shortId: string;
  publicKey: string;
  socksUser: string;
  /** 翻墙模式没有 SOCKS5 入口，后端给空串 —— 前端据此隐藏 SOCKS5 相关按钮 */
  socksEndpoint: string;
  note: string;
}

export interface XrayList {
  items: XrayTunnel[];
  shim: { runtimeClass: string; requiredNodeLabel: string };
  /** 两种模式的语义说明，面板直接渲染 */
  modes?: XrayModeInfo[];
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

// ── 网络拓扑（GET /api/topology） ────────────────────────────────────
//
// 一次请求返回整张图。`revision` 是内容指纹：节点/边集合没变时它就不变，
// 前端据此跳过重渲染（拓扑重排一次代价不低，而且会让正在看的人眼花）。

/** 拓扑节点的 kind（后端 group 字段与之一致，个别地方沿用后端命名） */
export type TopologyKind =
  | 'node'
  | 'namespace'
  | 'workload'
  | 'service'
  | 'ingress'
  | 'networkPolicy'
  | 'pod';

export interface TopologyScope {
  namespace: string;
  includePods: boolean;
  /** true = 后端对 Pod 数量做了上限截断，前端要提示「部分 Pod 未显示」 */
  truncated: boolean;
}

export interface TopologyCounts {
  nodes: number;
  namespaces: number;
  workloads: number;
  services: number;
  ingresses: number;
  networkPolicies: number;
  pods: number;
  wasm: number;
  native: number;
  gpu: number;
}

/** meta 的键随 kind 不同而不同，一律按未知值处理，取值要防御性 */
export interface TopologyNode {
  id: string;
  kind: string;
  label: string;
  sublabel: string;
  group: string;
  /** wasm | native | gpu */
  category: string;
  meta: Record<string, unknown>;
}

export interface TopologyEdge {
  from: string;
  to: string;
  kind: string;
  count: number;
}

export interface TopologyGraph {
  /** 后端时间（秒） */
  generatedAt: number;
  revision: string;
  scope: TopologyScope;
  /** 可选资源（Service/Ingress/NetworkPolicy/Namespace）读不到时的原因，要显示出来 */
  notes?: string[];
  counts: TopologyCounts;
  nodes: TopologyNode[];
  edges: TopologyEdge[];
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
  /** 缺省即 tunnel；显式传避免依赖后端默认值 */
  mode?: TunnelMode;
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

/** 翻墙模式：入站 REALITY（NodePort），出站直连，跑的是同一个 wasm 组件的服务端形态 */
export interface CreateWalljumpBody {
  mode: 'walljump';
  name: string;
  namespace?: string;
  /** 节点名；缺省由后端选第一个 Ready 节点 */
  node?: string;
  /** 30000-32767，默认 30543（hostPort 被本集群 PodSecurity baseline 禁止） */
  nodePort?: number;
  /** 伪装站点，默认 www.cloudflare.com */
  sni?: string;
  /** 回落目标，默认 <sni>:443 */
  dest?: string;
  /** 对外地址；缺省用面板访问地址 / 节点 IP */
  publicHost?: string;
  /** 复用已有服务端的 REALITY 私钥（base64url），不传就现生成 */
  privateKey?: string;
  uuid?: string;
  shortId?: string;
  image?: string;
  runtimeClassName?: string;
  replicas?: number;
  allowFrom?: string;
  secretName?: string;
}

/** 创建隧道后的连接方式（后端从请求 Host 推出节点地址） */
export interface CreatedTunnel {
  created: boolean;
  mode?: TunnelMode;
  namespace: string;
  name: string;
  secret?: string;
  socksEndpoint: string;
  portForward?: string;
  expose?: 'cluster' | 'nodeport';
  externalEndpoint?: string | null;
  allowFrom?: string | null;
  usage?: string;
  note?: string;
  /** NetworkPolicy 等附属资源创建失败时的可读原因（主体已建好） */
  warning?: string;
}

/** 创建翻墙入口后的结果：vless 链接 + 无 flow 的客户端配置 */
export interface CreatedWalljump {
  created: boolean;
  mode: 'walljump';
  namespace: string;
  name: string;
  node?: string;
  entry: string;
  nodePort: number;
  publicHost?: string;
  sni?: string;
  dest?: string;
  shortId?: string;
  uuid?: string;
  publicKey?: string;
  /** 故意不带 flow：xray-wasm 服务端未实现 XTLS-Vision 流控 */
  vlessLink: string;
  clientConfig: Record<string, unknown>;
  impl?: string;
  direction?: string;
  usage?: string;
  note?: string;
  warning?: string;
}

// ── 认证（WebAuthn / Touch ID） ──────────────────────────────────────
//
// 后端只认 WebAuthn（ui/backend/src/auth.rs）。二进制字段一律是
// **无 padding 的 base64url 字符串**，到了视图里才转成 ArrayBuffer。
// 这里直接沿用 DOM 的字面量联合类型：它们就是 WebAuthn 线上的取值，
// 于是视图构造 PublicKeyCredential*Options 时不需要额外断言。

/** GET /api/auth/status */
export interface AuthStatus {
  mode: string;
  configured: boolean;
  configuredError: string | null;
  rpId: string;
  origin: string;
  secretName: string;
  registrationCodeSet: boolean;
  registered: boolean;
  authenticated: boolean;
  sessionTtlSeconds: number;
}

/** allowCredentials / excludeCredentials 的元素；id 是 base64url */
export interface AuthCredentialDescriptor {
  type: 'public-key';
  id: string;
  transports?: AuthenticatorTransport[];
}

/** POST /api/auth/register/begin 的响应 = navigator.credentials.create 的参数 */
export interface RegisterBeginOptions {
  /** base64url */
  challenge: string;
  rp: { id: string; name: string };
  user: { id: string; name: string; displayName: string };
  pubKeyCredParams: { type: 'public-key'; alg: number }[];
  timeout: number;
  attestation: AttestationConveyancePreference;
  authenticatorSelection: {
    authenticatorAttachment?: AuthenticatorAttachment;
    residentKey?: ResidentKeyRequirement;
    userVerification?: UserVerificationRequirement;
  };
  excludeCredentials: AuthCredentialDescriptor[];
}

/** POST /api/auth/login/begin 的响应 = navigator.credentials.get 的参数 */
export interface LoginBeginOptions {
  /** base64url */
  challenge: string;
  rpId: string;
  timeout: number;
  userVerification: UserVerificationRequirement;
  allowCredentials: AuthCredentialDescriptor[];
}

export interface RegisterBeginBody {
  code: string;
}

/** POST /api/auth/register/finish 的请求体（全部 base64url） */
export interface RegisterFinishBody {
  id: string;
  rawId: string;
  type: 'public-key';
  response: { clientDataJSON: string; attestationObject: string };
}

export interface RegisterFinishResult {
  registered: boolean;
  authenticated: boolean;
  credentialId: string;
  note: string;
}

/** POST /api/auth/login/finish 的请求体（全部 base64url） */
export interface LoginFinishBody {
  id: string;
  rawId: string;
  type: 'public-key';
  response: {
    clientDataJSON: string;
    authenticatorData: string;
    signature: string;
    userHandle?: string | null;
  };
}

export interface LoginFinishResult {
  authenticated: boolean;
  signCount?: number;
  warning?: string;
}

export interface LogoutResult {
  authenticated: boolean;
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

  // 免密登录。除 /api/health 与 /api/auth/* 外，未登录时所有接口都返回 401。
  authStatus: () => request<AuthStatus>('/api/auth/status'),
  authRegisterBegin: (code: string) =>
    post<RegisterBeginOptions>('/api/auth/register/begin', { code } satisfies RegisterBeginBody),
  authRegisterFinish: (body: RegisterFinishBody) =>
    post<RegisterFinishResult>('/api/auth/register/finish', body),
  authLoginBegin: () => post<LoginBeginOptions>('/api/auth/login/begin', {}),
  authLoginFinish: (body: LoginFinishBody) =>
    post<LoginFinishResult>('/api/auth/login/finish', body),
  authLogout: () => post<LogoutResult>('/api/auth/logout', {}),

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
  /** 整张网络拓扑图；pods=true 时后端额外下发 Pod 节点（可能被截断，看 scope.truncated） */
  topology: (namespace?: string, pods = false) =>
    request<TopologyGraph>(`/api/topology${q({ namespace, pods: pods ? '1' : '0' })}`),
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
  /** 同一个 POST /api/xray/tunnels，mode=walljump；响应字段与隧道完全不同，所以单独一个方法 */
  createWalljumpTunnel: (body: CreateWalljumpBody) =>
    post<CreatedWalljump>('/api/xray/tunnels', body),
  scaleTunnel: (ns: string, name: string, replicas: number) =>
    post<unknown>(`/api/xray/tunnels/${encodeURIComponent(ns)}/${encodeURIComponent(name)}/scale`, { replicas }),
  tunnelVless: (ns: string, name: string) =>
    request<TunnelVless>(
      `/api/xray/tunnels/${encodeURIComponent(ns)}/${encodeURIComponent(name)}/vless`,
    ),
  deleteTunnel: (ns: string, name: string) =>
    del<unknown>(`/api/xray/tunnels/${encodeURIComponent(ns)}/${encodeURIComponent(name)}`),
};
