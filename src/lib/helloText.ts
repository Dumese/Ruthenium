// Windows Hello 可用性 → 用户可读的原因文案（设置页与启动门禁共用）。
//
// 分原因而不是一句笼统的「不可用」：用户要能区分「这台机器没有 Hello 硬件」
// （换设备 / 插外设）与「硬件有但没在系统设置里配 PIN」（去设置里配一下），
// 两者的下一步动作完全不同。
import type { HelloAvailability } from '../ipc/commands';
import { T } from '../i18n';

export function helloUnavailableText(a: HelloAvailability): string {
  switch (a) {
    case 'noDevice':
      return T('hello.availNoDevice');
    case 'notConfigured':
      return T('hello.availNotConfigured');
    case 'policy':
      return T('hello.availPolicy');
    case 'busy':
      return T('hello.availBusy');
    default:
      return T('hello.availUnsupported');
  }
}
