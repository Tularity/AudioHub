import { describe, it, expect } from 'vitest';
import { iconStateFrom } from './trayIcon';
import type { IconState } from './trayIcon';

describe('iconStateFrom', () => {
  it('离线就是离线，与会话数无关', () => {
    expect(iconStateFrom({ conn: 'offline', sessionCount: 0 })).toBe('offline');
    // 掉线时 sessions 还没来得及清空——这一条正是它存在的理由。
    expect(iconStateFrom({ conn: 'offline', sessionCount: 3 })).toBe('offline');
  });

  it('connecting 与 starting 合并成同一个图标', () => {
    expect(iconStateFrom({ conn: 'connecting', sessionCount: 0 })).toBe('connecting');
    expect(iconStateFrom({ conn: 'starting', sessionCount: 0 })).toBe('connecting');
  });

  it('在线且没有会话是 idle，有会话是 active', () => {
    expect(iconStateFrom({ conn: 'online', sessionCount: 0 })).toBe('idle');
    expect(iconStateFrom({ conn: 'online', sessionCount: 1 })).toBe('active');
    expect(iconStateFrom({ conn: 'online', sessionCount: 9 })).toBe('active');
  });

  it('只有 online 能得到 active', () => {
    // 会话列表清空晚于 conn 翻转时，图标不许抢先报「正在传输」。
    const notOnline = ['offline', 'connecting', 'starting'] as const;
    for (const conn of notOnline) {
      expect(iconStateFrom({ conn, sessionCount: 5 })).not.toBe('active');
    }
  });

  it('取值落在与 Rust/Python 对齐的那四个名字里', () => {
    // 三处按名字对齐（icons/make-icons.py 的 STATES、src-tauri 的 IconState）。
    // 这条守的是「改了这边忘了那边」——Rust 侧收到不认识的名字会回落成
    // connecting，静默且看起来正常。
    const allowed: IconState[] = ['offline', 'connecting', 'idle', 'active'];
    const conns = ['offline', 'connecting', 'starting', 'online'] as const;
    for (const conn of conns) {
      for (const sessionCount of [0, 1]) {
        expect(allowed).toContain(iconStateFrom({ conn, sessionCount }));
      }
    }
  });
});
