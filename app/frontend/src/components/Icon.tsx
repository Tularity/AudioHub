// 内联 SVG 图标。路径全是编译期常量，dangerouslySetInnerHTML 在这里没有注入面
// ——外部输入永远走不到这个函数（它只接受下面这张表里的键）。

const SVG_ATTRS = {
  xmlns: 'http://www.w3.org/2000/svg',
  viewBox: '0 0 24 24',
  fill: 'none',
  stroke: 'currentColor',
  strokeWidth: 1.8,
  strokeLinecap: 'round',
  strokeLinejoin: 'round',
  'aria-hidden': true,
} as const;

const ICONS = {
  mic: '<path d="M12 3a3 3 0 0 1 3 3v5a3 3 0 0 1-6 0V6a3 3 0 0 1 3-3z"/><path d="M5 11a7 7 0 0 0 14 0"/><path d="M12 18v3"/>',
  spk: '<path d="M4 9v6h4l5 4V5L8 9H4z"/><path d="M16.5 8.5a5 5 0 0 1 0 7"/>',
  mute: '<path d="M4 9v6h4l5 4V5L8 9H4z"/><path d="M16.5 9.5l5 5m0-5l-5 5"/>',
  monitor: '<path d="M4 13a8 8 0 0 1 16 0"/><rect x="3" y="13" width="4" height="6" rx="1.5"/><rect x="17" y="13" width="4" height="6" rx="1.5"/>',
  peers: '<rect x="3" y="3" width="8" height="8" rx="2"/><rect x="13" y="3" width="8" height="8" rx="2"/><rect x="3" y="13" width="8" height="8" rx="2"/><rect x="13" y="13" width="8" height="8" rx="2"/>',
  pair: '<path d="M9 15l6-6"/><path d="M10.5 6.5L12 5a4 4 0 0 1 5.7 5.7l-1.5 1.5"/><path d="M13.5 17.5L12 19a4 4 0 0 1-5.7-5.7l1.5-1.5"/>',
  stats: '<path d="M4 5v14h16"/><path d="M8 15l3-4 3 2 4-6"/>',
  settings: '<path d="M5 7h14"/><circle cx="9" cy="7" r="2"/><path d="M5 17h14"/><circle cx="15" cy="17" r="2"/>',
  wave: '<path d="M3 12h2l2-5 3 10 3-14 3 12 2-3h3"/>',
  back: '<path d="M15 5l-7 7 7 7"/>',
  chev: '<path d="M9 5l7 7-7 7"/>',
  copy: '<rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V5a2 2 0 0 1 2-2h10"/>',
  close: '<path d="M6 6l12 12M18 6L6 18"/>',
  scan: '<circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="4"/><path d="M12 12l6.4-6.4"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  plug: '<path d="M9 6v4m6-4v4"/><path d="M7 10h10v2a5 5 0 0 1-10 0v-2z"/><path d="M12 17v4"/>',
  cable: '<rect x="14" y="3" width="7" height="7" rx="2"/><rect x="3" y="14" width="7" height="7" rx="2"/><path d="M17.5 10v3.5a3 3 0 0 1-3 3H10"/><path d="M12 14.5l-2 2 2 2"/>',
  link: '<path d="M14 4h6v6"/><path d="M20 4l-8.5 8.5"/><path d="M18 14v4a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4"/>',
  shield: '<path d="M12 3l7 3v5.5c0 4.4-2.9 7.6-7 8.5-4.1-.9-7-4.1-7-8.5V6l7-3z"/><path d="M9 12l2 2 4-4"/>',
  device: '<rect x="5" y="3" width="14" height="18" rx="2.5"/><circle cx="12" cy="14" r="3.2"/><circle cx="12" cy="7" r="1"/>',
  tagname: '<path d="M3 12V5a2 2 0 0 1 2-2h7l9 9-9 9-9-9z"/><circle cx="8" cy="8" r="1.3"/>',
  refresh: '<path d="M20 12a8 8 0 1 1-2.6-5.9"/><path d="M20 4v4h-4"/>',
  // Appearance controls. Each preference gets its *own* glyph rather than one
  // glyph plus a state dot: these buttons carry no label, so the icon is the
  // only thing telling you which of the three states you are in.
  themeLight: '<circle cx="12" cy="12" r="4.2"/><path d="M12 2.4v2.5M12 19.1v2.5M4.4 4.4l1.8 1.8M17.8 17.8l1.8 1.8M2.4 12h2.5M19.1 12h2.5M4.4 19.6l1.8-1.8M17.8 6.2l1.8-1.8"/>',
  themeDark: '<path d="M20.2 14.4A8.6 8.6 0 0 1 9.6 3.8a8.6 8.6 0 1 0 10.6 10.6z"/>',
  // "Auto" is the half-filled disc macOS uses for the system appearance
  // setting. The filled half needs its own fill/stroke because the shared
  // SVG_ATTRS below are stroke-only.
  themeAuto: '<circle cx="12" cy="12" r="8.4"/><path d="M12 3.6a8.4 8.4 0 0 1 0 16.8z" fill="currentColor" stroke="none"/>',
  // Globe = "whichever language the system is in". Deliberately not a flag:
  // languages are not countries, and the follow-system state has no country.
  langAuto: '<circle cx="12" cy="12" r="8.6"/><path d="M3.7 9.4h16.6M3.7 14.6h16.6"/><path d="M12 3.4c2.3 2.3 3.5 5.2 3.5 8.6s-1.2 6.3-3.5 8.6c-2.3-2.3-3.5-5.2-3.5-8.6S9.7 5.7 12 3.4z"/>',
  // Translate glyph = "a language was picked". Not a CJK character and not a
  // flag, so it still reads correctly the day a second catalogue is added.
  langPinned: '<path d="M3.4 6.3h8.8M7.8 4.2v2.1"/><path d="M10.1 6.3c0 4-2.7 7.4-6.7 8.9"/><path d="M5.5 10.7c1.4 2.3 3.4 4 5.9 4.9"/><path d="M12.5 20.4l4.1-9.3 4.1 9.3"/><path d="M13.9 17.1h5.4"/>',
  // Windows caption glyphs. Deliberately squared off (`stroke-linecap: butt`
  // via `.caption-btn svg` in styles.css) and drawn on a 10x10 box centred in
  // the 24 viewBox, because that is what Segoe MDL2's chrome glyphs look like
  // -- a rounded minimise dash next to real Windows chrome reads as a toy.
  winMin: '<path d="M7 12h10"/>',
  winMax: '<rect x="7" y="7" width="10" height="10"/>',
  winRestore: '<rect x="7" y="10" width="7" height="7"/><path d="M10 10V7h7v7h-3"/>',
} as const;

export type IconName = keyof typeof ICONS;

export function Icon({ name, cls = 'ico' }: { name: IconName; cls?: string }) {
  return (
    <span className={cls}>
      <svg {...SVG_ATTRS} dangerouslySetInnerHTML={{ __html: ICONS[name] || '' }} />
    </span>
  );
}

/** 裸 SVG（侧栏导航按钮直接吃 `.nav-item svg` 的尺寸规则，外面不能包 span）。 */
export function RawIcon({ name }: { name: IconName }) {
  return <svg {...SVG_ATTRS} dangerouslySetInnerHTML={{ __html: ICONS[name] || '' }} />;
}
