/**
 * SSE 事件流 record/replay 快照（M-14/R-10，对齐 dsh `DSH_SNAPSHOT` 三态）。
 *
 * 由 `SNAPSHOT_MODE` 环境变量控制：
 * - `replay`（默认）：与已录快照深度比对，不一致即失败（回归门禁）；
 * - `record`：重新生成快照文件并放行（事件序列/归约逻辑有意变更时使用，
 *   录制产物必须随代码一起 review 提交）；
 * - `off`：跳过断言（调试用，CI 中禁用——防止静默漏检）。
 *
 * 快照存于 `src/test/snapshots/{name}.json`。
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { expect } from "vitest";

const SNAPSHOT_DIR = join(import.meta.dirname ?? ".", "snapshots");

type SnapshotMode = "replay" | "record" | "off";

function mode(): SnapshotMode {
  const raw = process.env.SNAPSHOT_MODE;
  if (raw === "record" || raw === "off") return raw;
  return "replay";
}

/**
 * 断言 `actual` 与具名快照一致（三态语义见模块注释）。
 *
 * # Panics
 * - `replay` 模式下与快照不一致时 panic（vitest expect 失败）；
 * - `record` 模式写盘后直接通过；
 * - `off` 模式跳过。
 */
export function expectMatchesSnapshot(name: string, actual: unknown): void {
  const m = mode();
  if (m === "off") return;

  const file = join(SNAPSHOT_DIR, `${name}.json`);
  if (m === "record") {
    mkdirSync(dirname(file), { recursive: true });
    writeFileSync(file, `${JSON.stringify(actual, null, 2)}\n`);
    return;
  }
  // replay
  if (!existsSync(file)) {
    // 无快照：先录制一次并失败提示（避免首跑静默通过空比对）
    mkdirSync(dirname(file), { recursive: true });
    writeFileSync(file, `${JSON.stringify(actual, null, 2)}\n`);
    throw new Error(
      `快照 ${name} 不存在，已录制基线。请检查 src/test/snapshots/${name}.json 内容后重跑。`,
    );
  }
  const expected = JSON.parse(readFileSync(file, "utf-8"));
  expect(actual).toEqual(expected);
}
