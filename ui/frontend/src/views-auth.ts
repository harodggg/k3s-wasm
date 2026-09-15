// 免密登录 / 首次绑定界面。
//
// 后端只认 WebAuthn（见 ui/backend/src/auth.rs）：没有口令，也就没有口令库、
// 没有撞库、没有明文口令过网。代价是「第一个凭据怎么来」—— 用一次性注册码
// （K3S_WASM_REGISTRATION_CODE）绑定，绑定成功后注册接口会直接拒绝，
// 之后就只用 Touch ID / passkey 登录。
//
// WebAuthn 对上下文很挑剔：
//   * 必须是安全上下文（HTTPS + 受信任证书），否则 navigator.credentials 不工作；
//   * clientDataJSON.origin 必须与后端允许的 origin **精确**相等；
//   * authenticatorData.rpIdHash 必须等于 SHA256(rpId)。
// 所以这个页面把 rpId / origin / secretName 原样列出来 —— 配错了能一眼看出来，
// 而不是只给一句「操作失败」。

import { ApiError, api } from './api';
import type { AuthStatus, LoginBeginOptions, RegisterBeginOptions } from './api';
import { button, clear, el, errorText, field, input } from './dom';

export interface AuthViewOptions {
  /** 绑定/登录成功后的回调：main.ts 用它继续正常启动流程。 */
  onSuccess: () => void | Promise<void>;
  /** 由外壳带进来的提示（比如「会话已过期」）。 */
  notice?: string | null;
}

// ── base64url ↔ ArrayBuffer ─────────────────────────────────────────
//
// 后端约定：所有二进制字段都是**无 padding 的 base64url**，而 atob/btoa 只认
// 标准 base64，所以补 '='、换回 +/ 这两步必须自己做，别指望浏览器。

export function b64urlToBuf(s: string): ArrayBuffer {
  const std = s.replace(/-/g, '+').replace(/_/g, '/');
  const pad = std.length % 4 === 0 ? '' : '='.repeat(4 - (std.length % 4));
  let bin: string;
  try {
    bin = atob(std + pad);
  } catch {
    throw new Error(`不是合法的 base64url：${s.slice(0, 24)}…`);
  }
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes.buffer;
}

export function bufToB64url(buf: ArrayBuffer): string {
  const bytes = new Uint8Array(buf);
  let bin = '';
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]!);
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

/** WebAuthn 是否可用：安全上下文 + 浏览器提供 credentials 容器。 */
export function webauthnUsable(): boolean {
  return window.isSecureContext && !!navigator.credentials && typeof navigator.credentials.create === 'function';
}

/** 退出登录：让外壳调用（清 cookie 由后端做）。 */
export async function logout(): Promise<void> {
  await api.authLogout();
}

// ── 参数转换（后端 JSON → DOM 选项） ────────────────────────────────

function creationOptions(o: RegisterBeginOptions): PublicKeyCredentialCreationOptions {
  return {
    challenge: b64urlToBuf(o.challenge),
    rp: { id: o.rp.id, name: o.rp.name },
    user: { id: b64urlToBuf(o.user.id), name: o.user.name, displayName: o.user.displayName },
    pubKeyCredParams: o.pubKeyCredParams.map((p) => ({ type: p.type, alg: p.alg })),
    timeout: o.timeout,
    attestation: o.attestation,
    authenticatorSelection: {
      authenticatorAttachment: o.authenticatorSelection.authenticatorAttachment,
      residentKey: o.authenticatorSelection.residentKey,
      userVerification: o.authenticatorSelection.userVerification,
    },
    excludeCredentials: o.excludeCredentials.map((c) => ({
      type: c.type,
      id: b64urlToBuf(c.id),
      transports: c.transports ?? [],
    })),
  };
}

function requestOptions(o: LoginBeginOptions): PublicKeyCredentialRequestOptions {
  return {
    challenge: b64urlToBuf(o.challenge),
    rpId: o.rpId,
    timeout: o.timeout,
    userVerification: o.userVerification,
    allowCredentials: o.allowCredentials.map((c) => ({
      type: c.type,
      id: b64urlToBuf(c.id),
      transports: c.transports ?? [],
    })),
  };
}

/** WebAuthn / 后端错误的可读文本。
 *
 * 注意：这个页面里的 401 是**本地**语义（注册码不对、登录被拒），
 * 不是「会话过期」—— 会话过期由 main.ts 的 handleUnauthorized() 处理，
 * 这里照后端原文显示即可。
 */
function webauthnErrorText(e: unknown): string {
  if (e instanceof ApiError) return e.message;
  if (e instanceof DOMException) {
    switch (e.name) {
      case 'NotAllowedError':
        return '认证被取消或超时（Touch ID 弹窗被关闭 / 未通过验证），也可能是 origin、RP ID 与后端不一致。';
      case 'SecurityError':
        return '安全上下文不满足：WebAuthn 需要 HTTPS + 受信任证书。';
      case 'NotSupportedError':
        return '当前浏览器或认证器不支持所需的 WebAuthn 参数。';
      case 'InvalidStateError':
        return '该认证器上已经存在这个凭据（可能已经绑定过了）。';
      case 'AbortError':
        return '操作被中断，请重试。';
      default:
        return `${e.name}：${e.message}`;
    }
  }
  return errorText(e);
}

// ── 小部件 ──────────────────────────────────────────────────────────

function head(): HTMLElement {
  return el(
    'div',
    { class: 'card-head auth-head' },
    el('span', { class: 'brand-mark', text: 'W' }),
    el(
      'div',
      {},
      el('div', { class: 'brand-title', text: 'k3s WASM 控制台' }),
      el('div', { class: 'brand-sub', text: '免密登录 · WebAuthn（Touch ID / passkey）' }),
    ),
  );
}

function hintBox(kind: 'info' | 'warn' | 'err', title: string, detail: string): HTMLElement {
  return el(
    'div',
    { class: `hint hint-${kind}` },
    title ? el('div', { class: 'hint-title', text: title }) : null,
    el('div', { class: 'hint-detail', text: detail }),
  );
}

/** 把 rpId / origin / secretName 原样列出来，origin 不匹配时明确点破。 */
function diagnostics(status: AuthStatus, secure: boolean): HTMLElement {
  const rows: [string, string][] = [
    ['当前页面 origin', location.origin],
    ['后端允许的 origin', status.origin],
    ['RP ID', status.rpId],
    ['凭据 Secret', status.secretName],
    ['安全上下文', secure ? '是' : '否'],
    ['已绑定凭据', status.registered ? '是' : '否'],
  ];
  const dl = el('dl', { class: 'kv auth-diag' });
  for (const [k, v] of rows) {
    dl.append(el('div', { class: 'kv-row' }, el('dt', { text: k }), el('dd', { text: v })));
  }
  const box = el(
    'div',
    { class: 'hint hint-info' },
    el('div', { class: 'hint-title', text: '诊断信息' }),
    dl,
  );
  if (status.origin !== location.origin) {
    box.append(
      el('div', {
        class: 'hint-detail',
        text:
          '当前页面 origin 与后端允许的 origin 不一致：WebAuthn 的 clientDataJSON.origin 是精确匹配的，' +
          '后端只接受上面列出的那个 origin，请用它重新打开控制台。',
      }),
    );
  }
  return box;
}

// ── 登录 / 绑定视图 ─────────────────────────────────────────────────

export function renderAuthView(host: HTMLElement, opts: AuthViewOptions): void {
  clear(host);
  const wrap = el('div', { class: 'auth-wrap' });
  host.append(wrap);

  // 当前这一屏里所有「动作按钮」；paint() 时重建，动作进行中统一禁用。
  let locked: HTMLButtonElement[] = [];
  let codeInput: HTMLInputElement | null = null;
  let msgHost: HTMLElement | null = null;

  const setBusy = (on: boolean): void => {
    for (const b of locked) b.disabled = on;
  };

  const showMsg = (text: string | null): void => {
    if (!msgHost) return;
    clear(msgHost);
    if (text) msgHost.append(hintBox('err', '操作失败', text));
  };

  const paint = (status: AuthStatus | null, loadError: string | null): void => {
    clear(wrap);
    const body = el('div', { class: 'card-body stack' });
    wrap.append(el('div', { class: 'card auth-card' }, head(), body));
    locked = [];
    codeInput = null;
    msgHost = el('div', { class: 'stack' });
    body.append(msgHost);

    if (opts.notice) body.append(hintBox('warn', '需要重新登录', opts.notice));

    if (!status) {
      body.append(
        hintBox('err', '无法获取登录状态', loadError ?? '未知错误'),
        el('div', { class: 'hint-detail', text: '后端可能还没起来；起来后点下面按钮重试。' }),
      );
      const retry = button('重新检查状态', load);
      locked = [retry];
      body.append(el('div', { class: 'auth-actions' }, retry));
      return;
    }

    const secure = webauthnUsable();
    const ready = status.configured && secure;
    body.append(diagnostics(status, secure));

    if (!status.configured) {
      body.append(
        hintBox(
          'err',
          '后端认证未配置，所有 /api/* 都会被拒绝',
          status.configuredError ?? '（后端没有给出原因）',
        ),
      );
    } else if (!secure) {
      body.append(
        hintBox(
          'err',
          '当前页面不是安全上下文，WebAuthn 不可用',
          `WebAuthn 只在 HTTPS + 受信任证书下可用，请用 https://<域名> 打开（当前 location.origin = ${location.origin}）。`,
        ),
      );
    }

    const actions: HTMLButtonElement[] = [];

    if (status.registered) {
      body.append(hintBox('info', '已绑定凭据', '这台控制台已经绑定过凭据，直接用 Touch ID / passkey 登录。'));
      const loginBtn = button('用 Touch ID 登录', () => void doLogin(), 'btn-primary');
      loginBtn.disabled = !ready;
      actions.push(loginBtn);
    } else {
      if (!status.registrationCodeSet) {
        body.append(
          hintBox(
            'warn',
            '后端没有设置 K3S_WASM_REGISTRATION_CODE',
            '首次绑定必须要有这个一次性注册码；设置它需要重启后端。',
          ),
        );
      } else if (ready) {
        codeInput = input('一次性注册码', '', 'password');
        body.append(
          field(
            '注册码（K3S_WASM_REGISTRATION_CODE）',
            codeInput,
            '只在首次绑定时需要；绑定成功后注册接口会直接拒绝。',
          ),
        );
      }
      const bindBtn = button('用 Touch ID 绑定', () => void doRegister(), 'btn-primary');
      bindBtn.disabled = !ready || !status.registrationCodeSet;
      actions.push(bindBtn);
    }

    const recheck = button('重新检查状态', load);
    recheck.disabled = !ready;
    actions.push(recheck);
    locked = actions;
    body.append(el('div', { class: 'auth-actions' }, ...actions));

    if (!ready) {
      body.append(el('div', { class: 'hint-detail', text: '修好后刷新页面（⌘R / Ctrl+R）再试。' }));
    }
  };

  const load = (): void => {
    clear(wrap);
    wrap.append(
      el(
        'div',
        { class: 'card auth-card' },
        head(),
        el('div', { class: 'card-body' }, el('div', { class: 'muted', text: '正在检查登录状态…' })),
      ),
    );
    locked = [];
    void (async () => {
      try {
        paint(await api.authStatus(), null);
      } catch (e) {
        paint(null, errorText(e));
      }
    })();
  };

  async function doRegister(): Promise<void> {
    const code = codeInput?.value.trim() ?? '';
    if (!code) {
      showMsg('请先填入后端设置的一次性注册码（K3S_WASM_REGISTRATION_CODE）。');
      return;
    }
    setBusy(true);
    showMsg(null);
    try {
      const begin = await api.authRegisterBegin(code);
      const created = await navigator.credentials.create({ publicKey: creationOptions(begin) });
      if (!created || created.type !== 'public-key') {
        throw new Error('认证器没有返回 public-key 凭据');
      }
      const cred = created as PublicKeyCredential;
      const resp = cred.response as AuthenticatorAttestationResponse;
      await api.authRegisterFinish({
        id: cred.id,
        rawId: bufToB64url(cred.rawId),
        type: 'public-key',
        response: {
          clientDataJSON: bufToB64url(resp.clientDataJSON),
          attestationObject: bufToB64url(resp.attestationObject),
        },
      });
      await opts.onSuccess();
    } catch (e) {
      showMsg(webauthnErrorText(e));
    } finally {
      setBusy(false);
    }
  }

  async function doLogin(): Promise<void> {
    setBusy(true);
    showMsg(null);
    try {
      const begin = await api.authLoginBegin();
      const got = await navigator.credentials.get({ publicKey: requestOptions(begin) });
      if (!got || got.type !== 'public-key') {
        throw new Error('认证器没有返回 public-key 凭据');
      }
      const cred = got as PublicKeyCredential;
      const resp = cred.response as AuthenticatorAssertionResponse;
      await api.authLoginFinish({
        id: cred.id,
        rawId: bufToB64url(cred.rawId),
        type: 'public-key',
        response: {
          clientDataJSON: bufToB64url(resp.clientDataJSON),
          authenticatorData: bufToB64url(resp.authenticatorData),
          signature: bufToB64url(resp.signature),
          userHandle: resp.userHandle ? bufToB64url(resp.userHandle) : null,
        },
      });
      await opts.onSuccess();
    } catch (e) {
      showMsg(webauthnErrorText(e));
    } finally {
      setBusy(false);
    }
  }

  load();
}
