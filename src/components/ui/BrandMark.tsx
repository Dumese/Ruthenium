// 品牌标志：内联 SVG 渲染 ruthenium-mark-mono.svg 的字组（currentColor）。
// 不使用 <use> 外部引用；可选 plate 模式带深底 squircle + 光标强调色（供关于页复用）。
// 品牌色固定值，勿改；（SVG 内部渐变，不随主题切换）。
const BRAND_PLATE_TOP = '#26272d';
const BRAND_PLATE_BOTTOM = '#141519';
const BRAND_SHEEN = '#ffffff';
const BRAND_GLYPH = '#f7f7f9';
const BRAND_CURSOR = '#0a84ff';

export function BrandMark({ size = 34, plate = false }: { size?: number; plate?: boolean }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 512 512"
      aria-hidden="true"
      style={{ flex: 'none' }}
    >
      {plate && (
        <>
          {/* 底板：ink 纵向渐变 + sheen 高光 + .10/2 内描边。
              id 加 brand- 前缀避免同页多实例（关于页 + 侧栏）渐变互相覆盖。 */}
          <defs>
            <linearGradient id="brand-ink" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0" stopColor={BRAND_PLATE_TOP} />
              <stop offset="1" stopColor={BRAND_PLATE_BOTTOM} />
            </linearGradient>
            <linearGradient id="brand-sheen" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0" stopColor={BRAND_SHEEN} stopOpacity=".10" />
              <stop offset="1" stopColor={BRAND_SHEEN} stopOpacity="0" />
            </linearGradient>
          </defs>
          <rect x="16" y="16" width="480" height="480" rx="112" fill="url(#brand-ink)" stroke="rgba(255,255,255,.10)" strokeWidth="2" />
          <rect x="16" y="16" width="480" height="480" rx="112" fill="url(#brand-sheen)" />
        </>
      )}
      <g
        fill="none"
        stroke={plate ? BRAND_GLYPH : 'currentColor'}
        strokeWidth="44"
        strokeLinecap="butt"
        strokeLinejoin="miter"
      >
        <path d="M82 156V356" />
        <path d="M82 156H162A44 44 0 0 1 206 200V228 A44 44 0 0 1 162 272H82" />
        <path d="M178 264L224 356" />
        <path d="M282 228V304A30 30 0 0 0 312 334H328A30 30 0 0 0 358 304V228" />
      </g>
      <rect
        x="398"
        y="228"
        width="52"
        height="128"
        rx="3"
        fill={plate ? BRAND_CURSOR : 'currentColor'}
      />
    </svg>
  );
}
