// 控制台入口：路由、导航、轮询、错误横幅。
//
// 刻意保持简单：hash 路由 + 一个 5 秒轮询。没有依赖、没有构建期魔法，
// 因为产物会被嵌进 wasm 二进制，体积直接等于用户要下载的字节数。

import { api } from './api';
import { clear, el, errorText, toast } from './dom';
import type { Ctx, ViewInstance } from './view-types';
import { eventsView, logsView, nodesView, overviewView, runtimesView, workloadsView } from './views-cluster';
import { spinAppsView, xrayView } from './views-wasm';
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
  { hash: '#/spinapps', label: 'Spin 应用', factory: () => spinAppsView() },
  { hash: '#/xray', label: 'Xray 隧道', factory: () => xrayView() },
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
};

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
  } catch {
    // 命名空间列表拿不到时不影响主流程，保持原样
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

async function mountRoute(): Promise<void> {
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
    const msg = errorText(e);
    banner(`加载失败：${msg}`);
    setConn('连接异常', 'err');
    host.append(el('div', { class: 'empty' }, el('div', { class: 'empty-msg', text: '这个页面加载失败' }), el('div', { class: 'empty-hint', text: msg })));
  }
  $('last-updated').textContent = `更新于 ${new Date().toLocaleTimeString()}`;
}

async function pollOnce(): Promise<void> {
  const instance = state.instance;
  if (!instance || !instance.autoRefresh || !instance.reload) return;
  if (!state.autofollow) return;
  try {
    await instance.reload();
    banner(null);
    $('last-updated').textContent = `更新于 ${new Date().toLocaleTimeString()}`;
  } catch (e) {
    // 轮询失败不清空页面（否则会把已看到的数据也弄没），只挂横幅
    banner(`自动刷新失败：${errorText(e)}`);
    setConn('连接异常', 'err');
  }
}

async function boot(): Promise<void> {
  try {
    const health = await api.health();
    state.defaultNamespace = health.defaultNamespace;
    if (!localStorage.getItem(NS_KEY)) state.currentNamespace = health.defaultNamespace;
    setConn(`已连接 · ${health.target}`, 'ok');
  } catch (e) {
    setConn('无法连接后端', 'err');
    banner(`无法连接后端：${errorText(e)}`);
  }

  await refreshNamespaces();
  await mountRoute();

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
        toast(`刷新失败：${errorText(e)}`, 'err');
      }
    })();
  });

  window.addEventListener('hashchange', () => {
    void mountRoute();
  });

  window.setInterval(() => {
    void pollOnce();
  }, POLL_MS);
}

void boot();
