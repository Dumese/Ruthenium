// @vitest-environment jsdom
// 面包屑「根」按钮的跳转目标必须区分本地栏（'' = 此电脑盘符列表）与远端栏（'/'）。
// 固定传 '/' 会让本地栏进入一个不在路径约定内的状态（Windows 上表现为显示 C:\ 内容
// 但面包屑写着「此电脑」）。
import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render } from '@testing-library/react';
import { Breadcrumb } from './Breadcrumb';

describe('Breadcrumb 根按钮', () => {
  it('本地栏：点根按钮回调 ""（盘符列表）', () => {
    const onGo = vi.fn();
    const { container } = render(
      <Breadcrumb parts={['C:', 'Users']} sep="\\" rootPath="" rootLabel="此电脑" onGo={onGo} />,
    );
    fireEvent.click(container.querySelector('.crumb button') as HTMLButtonElement);
    expect(onGo).toHaveBeenCalledWith('');
  });

  it('远端栏：点根按钮回调 "/"', () => {
    const onGo = vi.fn();
    const { container } = render(<Breadcrumb parts={['home', 'deploy']} rootPath="/" onGo={onGo} />);
    fireEvent.click(container.querySelector('.crumb button') as HTMLButtonElement);
    expect(onGo).toHaveBeenCalledWith('/');
  });

  it('层级按钮按累积路径回调', () => {
    const onGo = vi.fn();
    const { container } = render(<Breadcrumb parts={['home', 'deploy']} rootPath="/" onGo={onGo} />);
    const buttons = container.querySelectorAll('.crumb button');
    fireEvent.click(buttons[2]); // 根、home、deploy → 点 deploy
    expect(onGo).toHaveBeenCalledWith('home/deploy');
  });
});
