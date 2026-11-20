// 图标集合 P：40+ 个内联 SVG path（描边色取 currentColor）
const P: Record<string, string> = {
  search:'<circle cx="11" cy="11" r="7"/><path d="M20 20l-3.6-3.6"/>',
  plus:'<path d="M12 5v14M5 12h14"/>', gear:'<path d="M3.5 8.5h8M16.5 8.5h4M3.5 15.5h3.5M11.5 15.5h9"/><circle cx="14.5" cy="8.5" r="1.9"/><circle cx="9.5" cy="15.5" r="1.9"/>',
  sidebar:'<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M9 4v16"/>',
  eye:'<path d="M2 12s3.6-6 10-6 10 6 10 6-3.6 6-10 6-10-6-10-6z"/><circle cx="12" cy="12" r="2.6"/>',
  terminal:'<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M7 9l3 3-3 3M13 15h4"/>',
  folder:'<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
  file:'<path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z"/><path d="M14 3v6h6"/>',
  upload:'<path d="M12 16V4M7 9l5-5 5 5M4 17v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-2"/>',
  download:'<path d="M12 4v12M7 11l5 5 5-5M4 17v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-2"/>',
  chev:'<path d="M9 6l6 6-6 6"/>', down:'<path d="M6 9l6 6 6-6"/>',
  x:'<path d="M18 6L6 18M6 6l12 12"/>',
  check:'<path d="M20 6L9 17l-5-5"/>',
  bolt:'<path d="M13 2L4 14h7l-1 8 9-12h-7z"/>',
  link:'<path d="M10 13a5 5 0 0 0 7.5.5l2-2A5 5 0 0 0 12.5 4l-1 1"/><path d="M14 11a5 5 0 0 0-7.5-.5l-2 2A5 5 0 0 0 11.5 20l1-1"/>',
  shield:'<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/>',
  key:'<circle cx="8" cy="15" r="4"/><path d="M11 12l9-9 2 2-2 2 1 1-2 2-1-1-2 2"/>',
  clock:'<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/>',
  more:'<circle cx="5" cy="12" r="1.4"/><circle cx="12" cy="12" r="1.4"/><circle cx="19" cy="12" r="1.4"/>',
  refresh:'<path d="M20 11a8 8 0 1 0-.6 4M20 5v6h-6"/>',
  trash:'<path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13"/>',
  edit:'<path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L8 18l-4 1 1-4z"/>',
  play:'<path d="M7 4l13 8-13 8z"/>', pause:'<path d="M8 5v14M16 5v14"/>',
  copy:'<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2"/>',
  keyboard:'<rect x="2" y="6" width="20" height="12" rx="2"/><path d="M6 10h.01M10 10h.01M14 10h.01M18 10h.01M8 14h8"/>',
  layers:'<path d="M12 2l9 5-9 5-9-5zM3 12l9 5 9-5M3 17l9 5 9-5"/>',
  monitor:'<rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4"/>',
  sun:'<circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/>',
  moon:'<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z"/>',
  wifi:'<path d="M2 8.8a15 15 0 0 1 20 0M5 12.3a10 10 0 0 1 14 0M8.5 15.8a5 5 0 0 1 7 0"/><circle cx="12" cy="19" r="1"/>',
  alert:'<path d="M12 3l10 18H2z"/><path d="M12 9v5M12 17h.01"/>',
  info:'<circle cx="12" cy="12" r="9"/><path d="M12 11v5M12 8h.01"/>',
  command:'<path d="M9 6a3 3 0 1 0-3 3h12a3 3 0 1 0-3-3v12a3 3 0 1 0 3-3H6a3 3 0 1 0 3 3z"/>',
  filter:'<path d="M3 5h18l-7 8v6l-4 2v-8z"/>',
  grid:'<rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/>',
  box:'<path d="M21 8l-9-5-9 5 9 5zM3 8v8l9 5 9-5V8M12 13v8"/>',
  network:'<circle cx="5" cy="6" r="2"/><circle cx="19" cy="6" r="2"/><circle cx="12" cy="18" r="2"/><path d="M6.8 7l4 9.4M17.2 7l-4 9.4M7 6h10"/>',
  arrow:'<path d="M5 12h14M13 6l6 6-6 6"/>',
  slash:'<path d="M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM5.6 5.6l12.8 12.8"/>',
  code:'<path d="M8 8l-4 4 4 4M16 8l4 4-4 4M14 4l-4 16"/>',
  db:'<ellipse cx="12" cy="5" rx="8" ry="3"/><path d="M4 5v14c0 1.7 3.6 3 8 3s8-1.3 8-3V5M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3"/>',
  home:'<path d="M3 10l9-7 9 7v10a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1z"/>',
};

export function Icon({ n, s = 15, className, style }: { n: string; s?: number; className?: string; style?: React.CSSProperties }) {
  return (
    <svg
      width={s} height={s} viewBox="0 0 24 24" fill="none" stroke="currentColor"
      strokeWidth={1.7} strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"
      className={className} style={style}
      dangerouslySetInnerHTML={{ __html: P[n] || P.info }}
    />
  );
}
