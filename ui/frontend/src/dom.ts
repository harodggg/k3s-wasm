// 极小的 DOM 构建工具。
//
// 刻意不引框架：这个 SPA 的产物会被编译期嵌进 wasm 二进制，
// 每 KB 都直接加在 wasm 体积上；而界面复杂度用几十行 helper 就够了。

type Props = {
  class?: string;
  text?: string;
  html?: string;
  title?: string;
  attrs?: Record<string, string>;
  on?: Partial<Record<keyof HTMLElementEventMap, (ev: Event) => void>>;
};

export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Props = {},
  ...children: (Node | string | null | undefined | false)[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (props.class) node.className = props.class;
  if (props.text !== undefined) node.textContent = props.text;
  if (props.html !== undefined) node.innerHTML = props.html;
  if (props.title) node.title = props.title;
  if (props.attrs) {
    for (const [k, v] of Object.entries(props.attrs)) node.setAttribute(k, v);
  }
  if (props.on) {
    for (const [ev, handler] of Object.entries(props.on)) {
      if (handler) node.addEventListener(ev, handler as EventListener);
    }
  }
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    node.append(typeof child === 'string' ? document.createTextNode(child) : child);
  }
  return node;
}

export function clear(node: HTMLElement): void {
  node.replaceChildren();
}

export type BadgeKind = 'ok' | 'warn' | 'err' | 'info' | 'muted';

export function badge(text: string, kind: BadgeKind = 'muted'): HTMLElement {
  return el('span', { class: `badge badge-${kind}`, text });
}

export function card(title: string, body: Node | string, extra?: Node): HTMLElement {
  return el(
    'div',
    { class: 'card' },
    el('div', { class: 'card-head' }, el('h3', { text: title }), extra ?? el('span', {})),
    el('div', { class: 'card-body' }, body),
  );
}

/** 数值卡片：概览页用 */
export function stat(label: string, value: string | number, hint?: string, kind: BadgeKind = 'info'): HTMLElement {
  return el(
    'div',
    { class: `stat stat-${kind}` },
    el('div', { class: 'stat-value', text: String(value) }),
    el('div', { class: 'stat-label', text: label }),
    hint ? el('div', { class: 'stat-hint', text: hint }) : null,
  );
}

export function table(headers: string[], rows: (Node | string)[][]): HTMLElement {
  return el(
    'table',
    { class: 'table' },
    el('thead', {}, el('tr', {}, ...headers.map((h) => el('th', { text: h })))),
    el(
      'tbody',
      {},
      ...rows.map((cells) =>
        el(
          'tr',
          {},
          ...cells.map((c) =>
            el('td', {}, typeof c === 'string' ? document.createTextNode(c) : c),
          ),
        ),
      ),
    ),
  );
}

export function button(label: string, onClick: () => void | Promise<void>, kind = ''): HTMLButtonElement {
  const b = el('button', {
    class: `btn ${kind}`.trim(),
    text: label,
    on: {
      click: () => {
        void onClick();
      },
    },
  });
  return b;
}

export function field(label: string, input: HTMLElement, hint?: string): HTMLElement {
  return el(
    'label',
    { class: 'field' },
    el('span', { class: 'field-label', text: label }),
    input,
    hint ? el('span', { class: 'field-hint', text: hint }) : null,
  );
}

export function input(placeholder = '', value = '', type = 'text'): HTMLInputElement {
  const i = el('input', { class: 'input', attrs: { placeholder, type } });
  i.value = value;
  return i;
}

export function empty(message: string, hint?: string): HTMLElement {
  return el(
    'div',
    { class: 'empty' },
    el('div', { class: 'empty-msg', text: message }),
    hint ? el('div', { class: 'empty-hint', text: hint }) : null,
  );
}

/** 相对时间：k8s 的时间戳都是 RFC3339 UTC */
export function age(iso?: string | null): string {
  if (!iso) return '-';
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return '-';
  const secs = Math.max(0, Math.floor((Date.now() - t) / 1000));
  if (secs < 60) return `${secs}s`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.floor(hours / 24)}d`;
}

export function phaseKind(phase: string): BadgeKind {
  switch (phase) {
    case 'Running':
    case 'Active':
    case 'Ready':
      return 'ok';
    case 'Pending':
      return 'warn';
    case 'Failed':
    case 'Unknown':
      return 'err';
    case 'Succeeded':
      return 'info';
    default:
      return 'muted';
  }
}

let toastTimer: number | undefined;

export function toast(message: string, kind: 'ok' | 'err' = 'ok'): void {
  const node = document.getElementById('toast');
  if (!node) return;
  node.textContent = message;
  node.className = `toast toast-${kind}`;
  if (toastTimer !== undefined) window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => {
    node.className = 'toast hidden';
  }, kind === 'err' ? 8000 : 3000);
}

/** 复制到剪贴板。
 *
 * 面板是通过明文 HTTP + IP 访问的（不是安全上下文），此时 navigator.clipboard
 * 根本不存在，只能退回 execCommand。所以这里两条路都走一遍，并把结果如实返回。
 */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    /* 落到下面的兜底 */
  }
  try {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.setAttribute('readonly', '');
    ta.style.position = 'fixed';
    ta.style.top = '-1000px';
    document.body.append(ta);
    ta.select();
    const ok = document.execCommand('copy');
    ta.remove();
    return ok;
  } catch {
    return false;
  }
}

export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
