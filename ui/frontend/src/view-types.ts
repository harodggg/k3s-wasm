// 视图契约。
//
// 每个视图是一个「工厂」：每次导航创建一次实例，于是视图内部状态
// （选中的命名空间、日志页里选中的 Pod、表单里正在输入的内容）都是自然的闭包变量，
// 不需要任何全局状态管理。
//
// autoRefresh 决定全局轮询是否调用 reload()：
//   只读视图为 true（整体重渲染是安全的）
//   带表单的视图为 false（否则轮询会把用户正在输入的框子清掉），
//   它们只在动作后或点「刷新」时更新列表部分。

import type { api } from './api';

export type Api = typeof api;

export interface Ctx {
  api: Api;
  /** 集群里已知的命名空间名（供下拉选择） */
  namespaces: string[];
  defaultNamespace: string;
  /** 当前全局选中的命名空间，_all 表示全部 */
  currentNamespace: string;
  setNamespace(ns: string): void;
  navigate(hash: string): void;
  refreshNamespaces(): Promise<void>;
}

export interface ViewInstance {
  title: string;
  autoRefresh: boolean;
  mount(host: HTMLElement, ctx: Ctx): void | Promise<void>;
  /** 只重取数据、不重建 DOM 的部分（可选） */
  reload?(): Promise<void>;
}

export type ViewFactory = () => ViewInstance;
