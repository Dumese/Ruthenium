import { useEffect } from 'react';
import { useAppStore } from '../store/useAppStore';
import { restyleTerms } from '../components/terminal/useXterm';
import { termStack } from '../lib/termfonts';

/** light/dark/system 三态：system 时监听 prefers-color-scheme */
export function useTheme() {
  const theme = useAppStore((s) => s.theme);
  const fontSize = useAppStore((s) => s.fontSize);
  const lineHeight = useAppStore((s) => s.lineHeight);
  const font = useAppStore((s) => s.font);
  const cursor = useAppStore((s) => s.cursor);
  const sidebarWidth = useAppStore((s) => s.sidebarWidth);

  useEffect(() => {
    const apply = () => {
      let dark = theme === 'dark';
      if (theme === 'system') dark = window.matchMedia('(prefers-color-scheme: dark)').matches;
      document.body.classList.toggle('dark', dark);
    };
    apply();
    const mq = window.matchMedia('(prefers-color-scheme: dark)');
    mq.addEventListener('change', apply);
    return () => mq.removeEventListener('change', apply);
  }, [theme]);

  // CSS 变量同步 + xterm 重排
  useEffect(() => {
    document.documentElement.style.setProperty('--termfs', fontSize + 'px');
    document.documentElement.style.setProperty('--termfont', termStack(font));
    document.documentElement.style.setProperty('--sidew', sidebarWidth + 'px');
    // 语言 → html[lang]（字体栈随 --uifont 切换）
    document.documentElement.lang = useAppStore.getState().lang;
    restyleTerms();
  }, [fontSize, lineHeight, font, cursor, sidebarWidth, theme]);
}
