// 图形加速可用性探测：前端只看 WebGL2（兜底 WebGL）。
// 探测结果缓存：设置面板与 xterm 建实例共用同一判断，避免反复创建 canvas。
// 注意：这只决定「本机 WebGL 渲染器能不能用」，真正的 webview 硬件加速开关在 Rust 侧。
//
// 低配策略：
//   WebGL 渲染器会额外拉起 GPU 进程并给每个终端一个 WebGL 上下文，单实例基线
//   376MB、每个标签 +28MB；DOM 渲染器同样场景只有 141MB 基线、每标签 +2.7MB，
//   而且在 CPU 限速（≈低端机）下 DOM 的输出/输入延迟还更好（128KB 输出：主线程
//   961ms vs 1315ms，输出中按键 69ms vs 112ms）。
//   所以「装得动 Windows 10 的最低配」这类机器（≤4GB 内存或 ≤2 核）自动用 DOM 渲染器，
//   把内存让给系统；性能机仍走 WebGL。

let cached: boolean | null = null;
/** 后端（Rust）判定的低配设备。设备分档以它为准，前端 heuristic 只作兜底。 */
let backendLowEnd: boolean | null = null;

/** 由 store 初始化时写入（get_app_info 的 lowEnd 字段）。 */
export function setLowEndDevice(v: boolean | undefined): void {
  if (typeof v === 'boolean') backendLowEnd = v;
}

/** 低配设备判定：≤4GB 内存或 ≤2 核。用于自动关闭图形加速、缩小回滚缓冲。 */
export function isLowEndDevice(): boolean {
  if (backendLowEnd !== null) return backendLowEnd;
  if (typeof navigator === 'undefined') return false;
  const nav = navigator as Navigator & { deviceMemory?: number };
  const cores = nav.hardwareConcurrency || 8;
  const mem = nav.deviceMemory;
  return (mem !== undefined && mem <= 4) || cores <= 2;
}

/** 本机 WebGL 渲染器是否可用（只探测能力，不看低配策略）。 */
export function gpuAvailable(): boolean {
  if (cached !== null) return cached;
  try {
    const canvas = document.createElement('canvas');
    cached = !!(canvas.getContext('webgl2') || canvas.getContext('webgl'));
  } catch {
    cached = false;
  }
  return cached;
}

/** 是否应当启用 WebGL 渲染器：用户没关 + 本机支持 + 不是低配设备。 */
export function gpuAccelerationEnabled(pref: 'system' | 'off'): boolean {
  if (pref === 'off') return false;
  if (isLowEndDevice()) return false;
  return gpuAvailable();
}
