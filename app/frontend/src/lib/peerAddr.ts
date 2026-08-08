// 对端地址的形态判定（M8 P6）。
//
// **地址即传输选择**（plan §16.2）：填 `ws://…` 就是要求走 WebSocket 外壳的
// Tier 2，填 `IP:端口` 就是要求直连。Tier 2 之所以是手动的，正是因为它的前提
// ——「这条通路只做应用层转发」——我们观测不到：拨不通的对端与关机的对端长得
// 一模一样。所以决定权交给唯一知道的人，而形式就用他本来就要填的那一格。
//
// 这个模块只做**形态判定**，不做可达性判断。真正的解析与拒绝在 daemon
// （`core/audiohubd/src/wsshell.rs` 的 `WsUrl`），此处只是让用户在按下按钮
// **之前**就知道这串字符会被当成什么，以及哪里写错了。两处规则必须一致，
// 不一致时以 daemon 为准——它才是执行的那一方。

export type BadUrlReason = 'noHost' | 'badPort' | 'badIpv6';

/**
 * 判定结果。**判别式联合而不是「kind + 可选 reason」**：后者会让
 * `` `addr.badUrl.${shape.reason}` `` 在 `reason` 缺席时算出
 * `addr.badUrl.undefined` 这个语料里根本没有的键，而那是一条运行期才会
 * 露面的坏文案。写成联合之后，`reason` 只在 `badUrl` 这一支存在，
 * 类型检查直接把那条路堵死（第一版就是这么被 tsc 抓住的）。
 */
export type PeerAddrInfo =
  /** `IP[:端口]` / `主机名[:端口]`，Tier 0/1 直连。 */
  | { kind: 'direct' }
  /** `ws://host[:port][/path]`，Tier 2 over WebSocket。 */
  | { kind: 'ws' }
  /** `wss://…`：形态认得，本 build 没有 TLS 客户端。 */
  | { kind: 'wss' }
  /** 看起来是 URL，但解析不出来。 */
  | { kind: 'badUrl'; reason: BadUrlReason };

/**
 * 是否长得像一个 URL。
 *
 * 与 daemon 的 `WsUrl::looks_like_url` 同一条判据，理由也相同：**「这是个写坏的
 * URL」与「这是个主机名」必须可区分**。没有这一步，`wsx://host/p` 会被当成一台
 * 名叫 `wsx` 的机器去解析，用户拿到的是一条域名解析失败——对一个拼写错误来说，
 * 这是最没用的报错。
 */
export function looksLikeUrl(s: string): boolean {
  const low = s.trim().toLowerCase();
  return low.startsWith('ws://') || low.startsWith('wss://');
}

/** 判定一段地址文本的形态。空串按 `direct` 处理（调用方各自决定空串是否允许）。 */
export function classifyPeerAddr(raw: string): PeerAddrInfo {
  const s = raw.trim();
  if (!looksLikeUrl(s)) return { kind: 'direct' };

  const low = s.toLowerCase();
  const tls = low.startsWith('wss://');
  const rest = s.slice(tls ? 6 : 5);
  if (!rest) return { kind: 'badUrl', reason: 'noHost' };

  const cut = rest.search(/[/?#]/);
  const authority = cut < 0 ? rest : rest.slice(0, cut);
  if (!authority) return { kind: 'badUrl', reason: 'noHost' };

  if (authority.startsWith('[')) {
    const close = authority.indexOf(']');
    if (close < 0) return { kind: 'badUrl', reason: 'badIpv6' };
    if (!authority.slice(1, close)) return { kind: 'badUrl', reason: 'noHost' };
    const tail = authority.slice(close + 1);
    if (tail && !isPort(tail.startsWith(':') ? tail.slice(1) : '')) {
      return { kind: 'badUrl', reason: 'badPort' };
    }
  } else {
    const i = authority.lastIndexOf(':');
    if (i >= 0) {
      if (!authority.slice(0, i)) return { kind: 'badUrl', reason: 'noHost' };
      if (!isPort(authority.slice(i + 1))) return { kind: 'badUrl', reason: 'badPort' };
    }
  }
  return { kind: tls ? 'wss' : 'ws' };
}

function isPort(s: string): boolean {
  if (!/^\d{1,5}$/.test(s)) return false;
  const n = Number(s);
  return n > 0 && n <= 65535;
}

// -------------------------------------------------------------- 隧道地址那一格
//
// 上面的 `classifyPeerAddr` 服务的是「添加对端 / 配对」那两格：那里 `IP:端口`
// 是**主流**用法。下面这一格不同——它存的就是「隧道地址」，直连在这里的表达
// 方式是**留空**，不是填一个 IP。同一个判别函数，两条不同的收活规则。

/**
 * 隧道地址那一格能不能存。
 *
 * `ok` 的两种：`''`（清除，改回直连）与一个 `ws://` URL。其余全拒，且**拒在
 * 按下按钮之前**——daemon 的 `peers.set_tier` 也会拒（`WsUrl::parse` +
 * `require_plaintext`），但那条报错是英文的、带 Rust 上下文的，而这一格的四种
 * 写错方式各自有一句能直接照做的中文。
 *
 * `notUrl` 是这一格**独有**的一条：`192.168.1.9:47810` 在「添加对端」那格是
 * 完全正常的输入，在这里却是个错误——存进去 daemon 也读不出 `WsUrl`，于是这一
 * 格看起来填了、实际等于没填。这正是「存得下、拨不动」那类静默失效。
 */
export type EndpointCheck =
  | { ok: true; value: string }
  | { ok: false; why: 'notUrl' | 'wss' | BadUrlReason };

export function checkEndpoint(raw: string): EndpointCheck {
  const s = raw.trim();
  if (!s) return { ok: true, value: '' };
  const shape = classifyPeerAddr(s);
  switch (shape.kind) {
    case 'ws': return { ok: true, value: s };
    case 'wss': return { ok: false, why: 'wss' };
    case 'badUrl': return { ok: false, why: shape.reason };
    // 不是 URL 形态。**不替用户脑补一个 `ws://` 前缀**：他想要的可能是直连
    // （那该留空），补前缀会把一个笔误变成一次没人要求过的传输切换。
    case 'direct': return { ok: false, why: 'notUrl' };
  }
}
