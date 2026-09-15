// 极简 DOM 垫片：只实现 views-topology.ts 实际用到的那部分 DOM API。
class ClassList {
  constructor(el) { this.el = el; this.set = new Set(); }
  sync() { this.el._class = [...this.set].join(' '); }
  add(...cs) { for (const c of cs) this.set.add(c); this.sync(); }
  remove(...cs) { for (const c of cs) this.set.delete(c); this.sync(); }
  toggle(c, force) {
    const has = this.set.has(c);
    const on = force === undefined ? !has : force;
    if (on) this.set.add(c); else this.set.delete(c);
    this.sync();
    return on;
  }
  contains(c) { return this.set.has(c); }
}

export class El {
  constructor(tag, ns = null) {
    this.tagName = String(tag).toUpperCase();
    this.ns = ns;
    this.children = [];
    this.parent = null;
    this.attrs = {};
    this.listeners = {};
    this._class = '';
    this._text = '';
    this.classList = new ClassList(this);
    this.style = {};
  }
  set className(v) { this._class = String(v); this.classList.set = new Set(String(v).split(/\s+/).filter(Boolean)); }
  get className() { return this._class; }
  set textContent(v) { this._text = String(v); this.children = []; }
  get textContent() { return this._text; }
  set innerHTML(v) { this._text = String(v); }
  get innerHTML() { return this._text; }
  set title(v) { this._title = v; }
  get title() { return this._title; }
  setAttribute(k, v) { this.attrs[k] = String(v); if (k === 'class') this.className = v; }
  getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; }
  append(...kids) {
    for (const k of kids) {
      if (k === null || k === undefined || k === false) continue;
      if (typeof k === 'string') this.children.push({ text: k, children: [], classList: new ClassList({ _class: '' }) });
      else { k.parent = this; this.children.push(k); }
    }
  }
  replaceChildren(...kids) { this.children = []; this.append(...kids); }
  remove() { if (this.parent) { this.parent.children = this.parent.children.filter((c) => c !== this); this.parent = null; } }
  addEventListener(ev, fn) { (this.listeners[ev] ||= []).push(fn); }
  dispatch(ev, arg) { for (const fn of this.listeners[ev] ?? []) fn(arg); }
  getBoundingClientRect() { return { x: 0, y: 0, width: 0, height: 0, top: 0, left: 0, right: 0, bottom: 0 }; }
  querySelector() { return null; }
  get firstChild() { return this.children[0] ?? null; }
}

export function installDom() {
  globalThis.document = {
    createElement: (t) => new El(t),
    createElementNS: (ns, t) => new El(t, ns),
    createTextNode: (s) => ({ text: String(s), children: [], classList: new ClassList({ _class: '' }) }),
    getElementById: () => null,
    body: new El('body'),
  };
}

/** 深度优先收集满足条件的元素 */
export function findAll(root, pred, out = []) {
  for (const c of root.children ?? []) {
    if (c && c.tagName) {
      if (pred(c)) out.push(c);
      findAll(c, pred, out);
    }
  }
  return out;
}

export function textOf(node) {
  if (node === null || node === undefined) return '';
  if (typeof node.text === 'string') return node.text;
  const own = node._text ?? '';
  return own + (node.children ?? []).map(textOf).join('');
}
