// i18n。**中文是唯一发布语种，也是默认语种**；这一层存在的目的不是「现在就多语」，
// 而是让将来加一门语言是一次翻译工作，而不是一次重构。
//
// 为什么手写而不是 i18next：这里需要的全部能力是「按稳定键查一条整句 + 具名占位符
// 插值」。i18next 为此要带进 ~40KB 运行时、一套异步加载/命名空间/后端插件的生命周期，
// 而它真正解决的问题（懒加载语料、复数规则库、ICU 语法）我们一条都没有——复数在
// 中文里不变形，真需要时 Intl.PluralRules 是浏览器自带的。更关键的是**类型安全**：
// 下面的 MsgKey 由 zh-CN 目录推导，写错一个键是编译错误；i18next 的字符串键做不到
// 这一点，而「标识符随被指对象漂移」正是这个项目已经吃过亏的地方。
//
// 三条硬规矩：
//   1. 组件里**不出现任何面向用户的字面量**，一律 t('some.key')。
//   2. 键是稳定标识符，**不是中文原文**——改文案不该动代码。
//   3. **绝不用片段拼句子**。语序在不同语言里不一样，`'共 ' + n + ' 台'` 这种写法
//      在任何非 SVO/量词语言上都会散架。要变量就用具名占位符插进**整句**里；
//      真的是「若干短语并列」（比如摘要行）就显式走 joinPhrases()，由语料决定分隔符。

import { zhCN } from './zh-CN';

export type Locale = 'zh-CN';
export type MsgKey = keyof typeof zhCN;

const CATALOGUES: Record<Locale, Record<string, string>> = {
  'zh-CN': zhCN,
};

const DEFAULT_LOCALE: Locale = 'zh-CN';

let current: Locale = DEFAULT_LOCALE;

export function getLocale(): Locale {
  return current;
}

/**
 * 切换语种。目前只有一门语言，所以它总是回到 zh-CN——留着这个入口是为了让
 * `<html lang>` 与 Intl 的调用点从一开始就读同一个来源，而不是散落的硬编码。
 */
export function setLocale(loc: string): Locale {
  current = (Object.prototype.hasOwnProperty.call(CATALOGUES, loc) ? loc : DEFAULT_LOCALE) as Locale;
  document.documentElement.lang = current;
  return current;
}

export type Vars = Record<string, string | number>;

const PLACEHOLDER = /\{(\w+)\}/g;

/**
 * 查一条文案。占位符写作 `{name}`。
 *
 * 查不到时**返回键本身**而不是空串：界面上冒出一个 `peers.summary.paired` 很难看，
 * 但它至少指着自己的出处；空白只会让人以为那里本来就没内容。
 */
export function t(key: MsgKey, vars?: Vars): string {
  const table = CATALOGUES[current] || CATALOGUES[DEFAULT_LOCALE];
  const raw = table[key] ?? CATALOGUES[DEFAULT_LOCALE][key];
  if (raw == null) {
    if (import.meta.env.DEV) console.warn(`[i18n] 缺少文案：${key}`);
    return key;
  }
  if (!vars) return raw;
  return raw.replace(PLACEHOLDER, (m, name: string) =>
    (Object.prototype.hasOwnProperty.call(vars, name) ? String(vars[name]) : m));
}

/**
 * 并列短语（摘要行、帧数 · 丢包这类）。**不是**在拼句子：每一段本身就是一条完整
 * 短语，这里只负责用语料指定的分隔符把它们串起来，分隔符本身也是可翻译的。
 */
export function joinPhrases(parts: (string | null | undefined | false)[]): string {
  return parts.filter(Boolean).join(t('common.phraseSep'));
}

/** 人名/权限名之类的顿号列表，交给 Intl 而不是硬编码「、」。 */
export function listFormat(items: string[]): string {
  try {
    return new Intl.ListFormat(current, { style: 'long', type: 'conjunction' }).format(items);
  } catch {
    return items.join(t('common.listSep'));
  }
}
