/**
 * `EventSource` 测试桩（M-14）：jsdom 无内置 EventSource，且测试需要
 * **脚本化重放** SSE 事件序列（record/replay 快照，对齐 dsh `DSH_SNAPSHOT`）。
 *
 * 用法：`installMockEventSource()` 替换 `globalThis.EventSource`；测试内通过
 * 返回的 `instances` 拿到桩实例，`emit({ data: JSON.stringify(dto) })` 逐条
 * 重放事件（与 server `sse.rs` 的 wire 格式一致：`onmessage` + JSON payload）。
 */
export interface MockEventSource {
  url: string;
  readyState: number;
  onmessage: ((ev: { data: string }) => void) | null;
  onerror: ((ev: Event) => void) | null;
  onopen: (() => void) | null;
  emit(data: string): void;
  emitError(): void;
  close(): void;
}

const CLOSED = 2;

/** 当前测试安装的全部桩实例（按创建顺序）。 */
let instances: MockEventSource[] = [];

/** 替换全局 EventSource，返回实例数组引用（测试结束时 `restore()`）。 */
export function installMockEventSource(): MockEventSource[] {
  class FakeEventSource implements MockEventSource {
    url: string;
    readyState = 0;
    onmessage: ((ev: { data: string }) => void) | null = null;
    onerror: ((ev: Event) => void) | null = null;
    onopen: (() => void) | null = null;
    constructor(url: string | URL) {
      this.url = String(url);
      instances.push(this);
      // 异步触发 onopen（模拟连接建立 → 前端拉取 pending 权限快照）
      queueMicrotask(() => this.onopen?.());
    }
    emit(data: string): void {
      this.onmessage?.({ data });
    }
    emitError(): void {
      this.onerror?.(new Event("error"));
    }
    close(): void {
      this.readyState = CLOSED;
    }
  }
  instances = [];
  globalThis.EventSource = FakeEventSource as unknown as typeof EventSource;
  return instances;
}

/** 还原全局 EventSource（vitest afterEach 调用）。 */
export function restoreEventSource(): void {
  delete (globalThis as { EventSource?: unknown }).EventSource;
  instances = [];
}
