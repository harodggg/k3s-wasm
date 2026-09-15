// 控制台入口：认证闸门、路由、导航、轮询、错误横幅。
//
// 刻意保持简单：hash 路由 + 一个 5 秒轮询。没有依赖、没有构建期魔法，
// 因为产物会被嵌进 wasm 二进制，体积直接等于用户要下载的字节数。
//
// 认证顺序很重要：后端在未登录时对**所有** /api/* 返回 401（只有 /api/health
// 与 /api/auth/* 例外），所以启动时先把 /api/auth/status 问一遍，没登录就只渲染
// 登录页，连 namespaces 都不去请求 —— 否则页面上会先闪一堆 401。

import { ApiError, api } from './api';
import type { AuthStatus } from './api';
import { clear, el, errorText, toast } from './dom';
import type { Ctx, ViewInstance } from './view-types';
import { eventsView, logsView, nodesView, overviewView, runtimesView, workloadsView } from './views-cluster';
import { topologyView } from './views-topology';
import { spinAppsView, xrayView } from './views-wasm';
import { logout, renderAuthView } from './views-auth';
import './styles.css';

interface Route {
  hash: string;
  label: string;
  factory: (params: URLSearchParams) => ViewInstance;
}

const ROUTES: Route[] = [
  { hash: '#/overview', label: '概览', factory: () => overviewView() },
  { hash: '#/nodes', label: '节点', factory: () => nodesView() },
  { hash: '#/runtimes', label: 'WASM 运行时', factory: () => runtimesView() },
  { hash: '#/workloads', label: '工作负载', factory: () => workloadsView() },
  { hash: '#/topology', label: '网络拓扑', factory: () => topologyView() },
  { hash: '#/spinapps', label: 'Spin 应用', factory: () => spinAppsView() },
  { hash: '#/xray', label: 'Xray 翻墙/隧道', factory: () => xrayView() },
  { hash: '#/logs', label: '日志', factory: (p) => logsView(p) },
  { hash: '#/events', label: '事件', factory: () => eventsView() },
];

const POLL_MS = 5000;
const NS_KEY = 'k3s-wasm.namespace';

const state = {
  namespaces: [] as string[],
  defaultNamespace: 'k3s-wasm',
  currentNamespace: localStorage.getItem(NS_KEY) ?? 'k3s-wasm',
  autofollow: true,
  instance: null as ViewInstance | null,
  /** 「已登录」的唯一表示：由 boot 的 authStatus() 与登录成功回调置位，
   *  401（会话过期）时置回 false。所有需要认证的动作都以它为准。 */
  authed: false,
};

/** 全局控件只绑定一次：会话过期 → 重新登录后会再次进入 startApp()。 */
let controlsWired = false;

function $(id: string): HTMLElement {
  const node = document.getElementById(id);
  if (!node) throw new Error(`缺少 DOM 节点 #${id}`);
  return node;
}

function setConn(text: string, kind: 'ok' | 'err' | 'warn' = 'ok'): void {
  const node = $('conn');
  node.textContent = text;
  node.className = `conn conn-${kind}`;
}

function banner(message: string | null): void {
  const node = $('banner');
  if (!message) {
    node.className = 'banner hidden';
    node.textContent = '';
    return;
  }
  node.className = 'banner';
  node.textContent = message;
}

function setAuthed(on: boolean): void {
  state.authed = on;
  const indicator = document.getElementById('auth-indicator');
  if (indicator) indicator.classList.toggle('hidden', !on);
}

function renderNav(): void {
  const nav = $('nav');
  clear(nav);
  for (const r of ROUTES) {
    const active = location.hash.startsWith(r.hash) || (location.hash === '' && r.hash === '#/overview');
    nav.append(
      el('a', {
        class: `nav-item${active ? ' active' : ''}`,
        text: r.label,
        attrs: { href: r.hash },
      }),
    );
  }
}

function parseHash(): { route: Route; params: URLSearchParams } {
  const raw = location.hash || '#/overview';
  const [path, query] = raw.split('?');
  const route = ROUTES.find((r) => r.hash === path) ?? ROUTES[0]!;
  return { route, params: new URLSearchParams(query ?? '') };
}

async function refreshNamespaces(): Promise<void> {
  try {
    const list = await api.namespaces();
    state.namespaces = list.map((n) => n.name).sort();
  } catch (e) {
    // 命名空间列表拿不到时不影响主流程；但会话过期要立刻回登录页
    handleUnauthorized(e);
  }
}

const ctx: Ctx = {
  api,
  get namespaces() {
    return state.namespaces;
  },
  get defaultNamespace() {
    return state.defaultNamespace;
  },
  get currentNamespace() {
    return state.currentNamespace;
  },
  setNamespace(ns: string) {
    state.currentNamespace = ns;
    localStorage.setItem(NS_KEY, ns);
  },
  navigate(hash: string) {
    location.hash = hash;
  },
  refreshNamespaces,
};

/** 只显示登录页：把整个控制台外壳（导航 + 数据区）藏起来。 */
function showAuthView(notice: string | null): void {
  state.instance = null;
  setAuthed(false);
  banner(null);
  setConn('未登录', 'warn');
  const app = $('app');
  const auth = $('auth');
  app.classList.add('hidden');
  auth.classList.remove('hidden');
  renderAuthView(auth, {
    notice,
    onSuccess: () => {
      auth.classList.add('hidden');
      clear(auth);
      app.classList.remove('hidden');
      setAuthed(true);
      return startApp();
    },
  });
}

/** 任何视图/API 抛 401 都走这里：切回登录页并说明原因。
 *
 *  返回 true 表示已经处理，调用方应直接 return —— 不要再挂通用错误横幅，
 *  否则「会话过期」会被显示成「加载失败」，看着像后端坏了。
 */
function handleUnauthorized(e: unknown): boolean {
  if (!(e instanceof ApiError) || e.status !== 401) return false;
  showAuthView('会话已过期，请重新用 Touch ID 登录');
  return true;
}

async function mountRoute(): Promise<void> {
  if (!state.authed) return;
  const { route, params } = parseHash();
  $('view-title').textContent = route.label;
  document.title = `k3s WASM 控制台 · ${route.label}`;
  renderNav();

  const host = $('view');
  clear(host);
  const instance = route.factory(params);
  state.instance = instance;

  try {
    await instance.mount(host, ctx);
    banner(null);
    setConn(`已连接 · ${state.defaultNamespace}`, 'ok');
  } catch (e) {
    if (handleUnauthorized(e)) return;
    const msg = errorText(e);
    banner(`加载失败：${msg}`);
    setConn('连接异常', 'err');
    host.append(el('div', { class: 'empty' }, el('div', { class: 'empty-msg', text: '这个页面加载失败' }), el('div', { class: 'empty-hint', text: msg })));
  }
  $('last-updated').textContent = `更新于 ${new Date().toLocaleTimeString()}`;
}

async function pollOnce(): Promise<void> {
  if (!state.authed) return;
  const instance = state.instance;
  if (!instance || !instance.autoRefresh || !instance.reload) return;
  if (!state.autofollow) return;
  try {
    await instance.reload();
    banner(null);
    $('last-updated').textContent = `更新于 ${new Date().toLocaleTimeString()}`;
  } catch (e) {
    if (handleUnauthorized(e)) return;
    // 轮询失败不清空页面（否则会把已看到的数据也弄没），只挂横幅
    banner(`自动刷新失败：${errorText(e)}`);
    setConn('连接异常', 'err');
  }
}

function wireControls(): void {
  if (controlsWired) return;
  controlsWired = true;

  const toggle = $('autorefresh') as HTMLInputElement;
  toggle.checked = state.autofollow;
  toggle.addEventListener('change', () => {
    state.autofollow = toggle.checked;
    toast(state.autofollow ? '已开启自动刷新' : '已暂停自动刷新');
  });

  $('refresh').addEventListener('click', () => {
    void (async () => {
      try {
        if (state.instance?.reload) {
          await state.instance.reload();
        } else {
          await mountRoute();
        }
        $('last-updated').textContent = `更新于 ${new Date().toLocaleTimeString()}`;
        banner(null);
      } catch (e) {
        if (handleUnauthorized(e)) return;
        toast(`刷新失败：${errorText(e)}`, 'err');
      }
    })();
  });

  $('logout').addEventListener('click', () => {
    void (async () => {
      try {
        await logout();
      } catch (e) {
        if (!handleUnauthorized(e)) toast(`退出登录失败：${errorText(e)}`, 'err');
      }
      // 无论后端有没有应答，本地都按「已退出」处理：回登录页最安全。
      showAuthView('已退出登录，请重新用 Touch ID 登录');
    })();
  });

  window.addEventListener('hashchange', () => {
    if (!state.authed) return;
    void mountRoute();
  });

  window.setInterval(() => {
    void pollOnce();
  }, POLL_MS);
}

/** 已登录之后的正常启动：健康检查 → 命名空间 → 首个路由。 */
async function startApp(): Promise<void> {
  wireControls();

  try {
    const health = await api.health();
    const v = document.getElementById('ui-version');
    if (v) v.textContent = `v${health.version}`;
    state.defaultNamespace = health.defaultNamespace;
    if (!localStorage.getItem(NS_KEY)) state.currentNamespace = health.defaultNamespace;
    setConn(`已连接 · ${health.target}`, 'ok');
  } catch (e) {
    if (handleUnauthorized(e)) return;
    setConn('无法连接后端', 'err');
    banner(`无法连接后端：${errorText(e)}`);
  }

  await refreshNamespaces();
  await mountRoute();
}

async function boot(): Promise<void> {
  wireControls();

  // 先过认证闸门：没登录就只渲染登录页，不碰任何需要认证的接口。
  let status: AuthStatus | null = null;
  try {
    status = await api.authStatus();
  } catch {
    // 连状态都拿不到（后端还没起来）：照样进登录页，由它显示错误并允许重试
  }

  if (!status || !status.authenticated) {
    showAuthView(null);
    return;
  }

  setAuthed(true);
  await startApp();
}

void boot();
