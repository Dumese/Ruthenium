// 终端等宽字体表：五项字体 pill，默认 JetBrains Mono。
// bundled=true 表示随应用内置完整字体（styles/fonts.css 的 "Rut Mono *"），bundled=false 为 Windows 系统字体。
export interface TermFont {
  id: string;
  stack: string;
  mono: string;
  bundled: boolean;
}

export const TERMFONTS: TermFont[] = [
  { id: 'JetBrains Mono', stack: '"Rut Mono JB"', mono: '"Rut Mono JB"', bundled: true },
  { id: 'IBM Plex Mono', stack: '"Rut Mono Plex"', mono: '"Rut Mono Plex"', bundled: true },
  { id: 'Source Code Pro', stack: '"Rut Mono SC"', mono: '"Rut Mono SC"', bundled: true },
  { id: 'Consolas', stack: 'Consolas', mono: 'Consolas', bundled: false },
  { id: 'Monaco', stack: 'Monaco', mono: 'Monaco', bundled: false },
];

export const DEFAULT_FONT = TERMFONTS[0].id; // 'JetBrains Mono'

/** 选中项 stack + 兜底。
 *
 *  末尾挂上内嵌的 Pan-CJK Noto Sans CJK：等宽字体本身没有 CJK 字形，不补这一层时
 *  中英混排（例如 `%APPDATA%\...\hosts.toml` 紧跟「凭据管理器」）会掉到系统字体，
 *  同一行出现两种字形风格。这里同时作用于 xterm（终端里的中文）与 `.mono`（UI 路径/命令）。 */
export function termStack(id: string): string {
  const f = TERMFONTS.find((x) => x.id === id) ?? TERMFONTS[0];
  return f.stack + ',Consolas,"Courier New","Rut UI",monospace';
}

/** 旧偏好值（'SF Mono'/'Menlo'/'monospace'/'Cascadia Mono' 等）不在表内时回退默认字体 */
export function normalizeFont(id: string | undefined | null): string {
  return id && TERMFONTS.some((f) => f.id === id) ? id : DEFAULT_FONT;
}
