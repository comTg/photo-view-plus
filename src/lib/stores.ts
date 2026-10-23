import { listen } from "@tauri-apps/api/event";
import { useSyncExternalStore } from "react";
import { aiPipelineStatus, aiWorkerStatus, queueStatus } from "./tauri";
import type { AiPipelineStatus, AiWorkerStatus, QueueStatus, ScanProgress } from "./tauri-types";

/**
 * 高频状态事件的共享订阅（docs/09 P0-3、docs/11 R5）。
 *
 * 每个事件全局只挂一个监听，用到它的组件各自通过 `useEventStore` 订阅：事件到来时只重渲染这些
 * 小组件，不再让整个 App 跟着重渲染（`queue:status` 每秒一次、`ai:progress` 每 2 秒一次，空闲时也照发）。
 */
export interface EventStore<T> {
  get: () => T;
  /** 命令直接返回了新状态时（如启动 / 停止 worker）用它写回。 */
  set: (value: T) => void;
  subscribe: (listener: () => void) => () => void;
}

function createEventStore<P, T>(
  eventName: string,
  initial: T,
  reduce: (state: T, payload: P) => T,
  load?: () => Promise<T>,
): EventStore<T> {
  let state = initial;
  let started = false;
  // 收到过事件或被 set 过之后，初始查询的结果已经过时，不再采用。
  let fresh = false;
  const listeners = new Set<() => void>();

  const update = (next: T) => {
    if (Object.is(next, state)) return;
    state = next;
    for (const listener of listeners) listener();
  };

  const start = () => {
    if (started) return;
    started = true;
    listen<P>(eventName, (event) => {
      fresh = true;
      update(reduce(state, event.payload));
    }).catch((error) => console.warn(`订阅事件失败：${eventName}`, error));
    load?.()
      .then((value) => {
        if (!fresh) update(value);
      })
      .catch(() => undefined);
  };

  return {
    get: () => state,
    set: (value) => {
      fresh = true;
      update(value);
    },
    subscribe: (listener) => {
      start();
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
  };
}

/** 取最新一条；内容和上一条一样时沿用旧对象（`queue:status` 空闲时每秒都发一模一样的）。 */
const latest = <T extends object>(state: T | null, payload: T): T | null =>
  state && shallowEqual(state, payload) ? state : payload;

function shallowEqual<T extends object>(a: T, b: T): boolean {
  const keys = Object.keys(a) as (keyof T)[];
  return keys.length === Object.keys(b).length && keys.every((key) => Object.is(a[key], b[key]));
}

export const queueStatusStore = createEventStore<QueueStatus, QueueStatus | null>(
  "queue:status",
  null,
  latest,
  queueStatus,
);

export const aiWorkerStatusStore = createEventStore<AiWorkerStatus, AiWorkerStatus | null>(
  "ai:worker_status",
  null,
  latest,
  aiWorkerStatus,
);

export const aiPipelineStore = createEventStore<AiPipelineStatus, AiPipelineStatus | null>(
  "ai:progress",
  null,
  latest,
  aiPipelineStatus,
);

/** 各目录最近一次的扫描进度，键为 rootId。 */
export const scanProgressStore = createEventStore<ScanProgress, Record<number, ScanProgress>>(
  "scan:progress",
  {},
  (state, payload) => ({ ...state, [payload.rootId]: payload }),
);

export function useEventStore<T>(store: EventStore<T>): T {
  return useSyncExternalStore(store.subscribe, store.get);
}
