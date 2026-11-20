// 界面与数据的统一默认值。
//
// 为什么单独放一个模块：新主机/导入主机的「默认分组」若散落在 store、表单、SSH 配置解析器
// 三处（且有的用 folders[0]、有的用 folders[2]），行为就会不一致；集中到这里后只有一个事实来源。

/** 默认分组名。属于用户数据语义（与默认配置里的分组同名），不走 i18n。 */
export const DEFAULT_GROUP_NAME = '开发机';

/**
 * 为新主机挑选默认分组：
 * 1) 优先找与 {@link DEFAULT_GROUP_NAME} 同名的已有分组；
 * 2) 否则退回第一个分组（用户的第一个分组通常就是常用分组）；
 * 3) 一个分组都没有时用默认名兜底，保证字段非空。
 */
export function defaultGroup(folders: ReadonlyArray<{ name: string }>): string {
  return folders.find((f) => f.name === DEFAULT_GROUP_NAME)?.name
    ?? folders[0]?.name
    ?? DEFAULT_GROUP_NAME;
}
