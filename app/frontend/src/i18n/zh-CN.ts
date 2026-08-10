// 简体中文语料。**唯一发布语种**，也是缺省回退语种。
//
// 键的命名规矩：`<视图或模块>.<语义>`，稳定不变；改文案只动值，绝不动键。
// 值里的 `{name}` 是具名占位符——一条文案永远是**一句完整的话**，变量插进去，
// 不允许调用方在外面用 + 把半句半句接起来。

export const zhCN = {
  // ---------------------------------------------------------------- 通用
  // 并列短语的**连接符**：两侧的留白是它契约的一部分，joinPhrases() 用的就是它。
  'common.phraseSep': ' · ',
  // 独立展示用的间隔点。与 phraseSep 不是一回事——那是连接符，不能 trim 出来当字形用：
  // 某个语种把 phraseSep 设成「，」时，被 trim 的结果会变成一个悬空的逗号。
  'common.bullet': '·',
  'common.listSep': '、',
  'common.dash': '—',
  'common.ok': '确定',
  'common.cancel': '取消',
  'common.save': '保存',
  'common.clear': '清除',
  'common.close': '关闭',
  'common.copy': '复制',
  // 复制失败在详情页与设置页是同一件事，一条键服务两处——分成两条早晚会各自漂移。
  'common.copyFailed': '复制失败，请手动选择文本',
  'common.retry': '重试',
  'common.connect': '连接',
  'common.connecting': '连接中…',
  'common.required': '必需',
  'common.optional': '可选',
  'common.online': '在线',
  'common.offline': '离线',

  // ---------------------------------------------------------------- 外壳
  'app.name': 'AudioHub',
  'app.tagline': '网络音频共享',
  'nav.peers': '主面板',
  'nav.pair': '配对向导',
  'nav.stats': '统计诊断',
  'nav.settings': '设置',
  'nav.detail': '对端详情',

  // 窗口拖不动时的唯一线索。静默失败正是这个 bug 之前难以定位的原因，所以宁可吵。
  'chrome.dragFailed': '窗口拖拽不可用：{message}。请重启 AudioHub。',
  // Windows 自绘标题按钮。三条都用动词，读屏念出来是「最小化 按钮」。
  'chrome.minimize': '最小化',
  'chrome.maximize': '最大化',
  'chrome.restore': '向下还原',
  'chrome.close': '关闭窗口（音频服务继续运行）',
  'chrome.captionFailed': '窗口按钮不可用：{message}。请重启 AudioHub。',

  'badge.online': '在线',
  'badge.starting': '启动中',
  'badge.connecting': '连接中',
  'badge.offline': '离线',

  // 顶栏右上（Windows 侧在左上）那一组图标按钮。三枚都没有文字标签，所以
  // aria-label 与 title 承担全部说明责任——它们不是「顺手加的无障碍属性」，
  // 是这三个按钮唯一的文字出口。
  'chrome.status.title': '服务连接',
  'chrome.status.menu': '服务连接详情',
  'chrome.status.fingerprint': '本机指纹',
  'chrome.status.port': '控制端口',
  'chrome.status.name': '本机名称',
  'chrome.status.more': '完整信息在「设置 › 本机身份」。',

  'chrome.locale.title': '界面语言',
  'chrome.locale.system': '跟随系统',
  // 「跟随系统」当前落在哪一门语言上，括号里补出来——否则用户无从知道系统被识别成了什么。
  'chrome.locale.systemAs': '跟随系统（{name}）',
  'chrome.locale.current': '界面语言：{name}',

  'chrome.theme.title': '外观',
  'chrome.theme.system': '跟随系统',
  'chrome.theme.light': '浅色',
  'chrome.theme.dark': '深色',
  // 点击即切换，所以提示要同时说「现在是什么」和「点下去会变成什么」。
  'chrome.theme.current': '外观：{now}（点击切换到{next}）',
  'chrome.theme.systemAs': '跟随系统（当前{resolved}）',

  // foot.* 七条已删（规格 §2.4）：左下角那条注脚说的四种状态与上面四条 badge.* 逐一
  // 重合，端口在设置页「网络 › IPC 端口」，「你正在用网页端查看」由 settings.web.browserOnly
  // 常驻说明。

  // ---------------------------------------------------------------- 覆盖层
  'overlay.starting.title': '正在启动 AudioHub 服务…',
  'overlay.starting.desc': '首次启动需数秒；服务就绪后自动进入主面板。',
  'overlay.connecting.title': '正在连接 AudioHub 服务…',
  'overlay.connecting.desc': '正在连接本机端口 {port} …',
  'overlay.connecting.descNoPort': '正在获取本机服务连接信息…',
  'overlay.version.title': 'AudioHub 服务版本不兼容',
  'overlay.version.desc': '{message}。本界面只能与 IPC 协议 v{version} 的服务通信，请更新到同一次构建。',
  'overlay.version.hint': '提示：确认 audiohub 与本界面来自同一次构建。',
  'overlay.noEndpoint.title': '缺少连接参数',
  'overlay.noEndpoint.desc': '请以 ?port=<端口>&token=<令牌> 打开本页面，或直接访问本机服务提供的界面地址。',
  'overlay.noEndpoint.hint': '浏览器模式无法启动服务：请在终端运行 audiohub daemon 后等待自动重连。',
  'overlay.noBinary.title': '找不到 AudioHub 服务程序',
  'overlay.noBinary.desc': '应用内缺少 audiohub 服务程序。请重新安装 AudioHub；开发环境可设 AUDIOHUB_BIN 指向已编译的 audiohub。',
  'overlay.noBinary.hint': '重装后再点「重试」。',
  'overlay.spawnFailed.title': '无法启动 AudioHub 服务',
  'overlay.spawnFailed.desc': '已定位到服务程序，但进程启动失败——多为文件权限或系统隔离属性。可重新安装，或在终端执行 audiohub daemon 查看完整错误。',
  'overlay.portBusy.title': 'AudioHub 服务端口被占用',
  'overlay.portBusy.desc': '所需端口已被其它程序占用（多为仍在运行的旧实例）。请结束该进程，或执行 audiohub ctl shutdown 后重试。',
  'overlay.timeout.title': 'AudioHub 服务启动超时',
  'overlay.timeout.desc': '服务已启动但未在预期时间内就绪。请稍候重试；持续失败可在终端运行 audiohub daemon 观察日志。',
  'overlay.startFailed.title': '无法启动 AudioHub 服务',
  'overlay.startFailed.desc': '启动服务时发生未预期的错误。请重试；持续失败可在终端运行 audiohub daemon 查看报错。',
  'overlay.internal.title': '无法启动 AudioHub 服务',
  'overlay.internal.desc': '界面与本机服务管理器之间的调用失败。请重试，或重启 AudioHub。',
  'overlay.disconnected.title': 'AudioHub 服务已断开',
  'overlay.disconnected.descTauri': '与本机服务的连接已断开（{reason}），每 5 秒自动重连。',
  'overlay.disconnected.reasonUnknown': '原因未知',
  'overlay.disconnected.descBrowser': '与本机服务的连接已断开，每 5 秒自动重试。',
  'overlay.detail': '详细信息：{detail}',

  // ---------------------------------------------------------------- 运行模式
  // plan §13：三种模式**互斥**，共享模式与两种使用端模式并列。标题因此不再是
  // 「使用端模式」——那个名字把三选一说成了二选一，而被砍掉的那一档恰恰是默认值。
  // ⚠ 块标题走 `settings.mode.title`：模式栏 2026-08-10 整块搬进设置页之后，
  // 同一个「运行模式」留两条键就是两条早晚会漂开的键。
  'mode.share.label': '共享 · 供他人使用',
  'mode.a.label': 'A · 免驱动',
  'mode.b.label': 'B · 虚拟设备',
  // 一级界面只留一句**结果句**：读完这一行就知道「现在选谁、在哪里选」，剩下的由
  // 「了解更多」带到设置页——那里的 settings.mode.rowDesc 本来就是同一段话的完整版，
  // 原先主面板上的 mode.a.desc / mode.b.desc 与它逐句重复，已随本次重整一并删除。
  'mode.downgraded': '当前选定为模式 B，但该模式不可用，已临时按模式 A 运行。',
  'mode.switched.toShare': '已切换到共享模式：本机发起的会话已全部关闭，全部 AudioHub 虚拟设备已移除。',
  'mode.switched.toB': '已切换到模式 B：已配对主机将作为音频设备出现在系统里。正在使用本机的对端已被断开。',
  'mode.switched.toA': '已切换到模式 A：全部 AudioHub 虚拟设备已移除。正在使用本机的对端已被断开。',
  // 互斥这件事必须在切换处说清楚，而不只在文档里：用户点下去之前就该知道
  // 「选了这个，另一件事就不做了」。

  'hal.unknown': '服务未连接，暂时无法判断驱动是否可用。',
  'hal.absent': '未检测到 AudioHub 驱动，模式 B 不可用；安装驱动并重启本应用后即可选择。',
  'hal.absent.why': '未检测到 AudioHub 驱动，无法使用模式 B',
  'hal.mismatch': '驱动版本与本机服务不匹配{versions}：不会有任何虚拟设备出现。请安装配套版本的驱动。',
  'hal.mismatch.versions': '（服务 v{mine} / 驱动 v{theirs}）',
  'hal.detached': '驱动已注册，但桥接通道尚未建立：已发布的设备保留在系统中，当前不处理音频。请稍候或重启服务。',
  'hal.ready': '已连接 AudioHub 驱动，模式 B 可用。',

  'halReason.capacity': '虚拟设备数量已达上限（16 台）。解除其它配对后可用。',
  'halReason.noDriver': '本机未安装 AudioHub 驱动，无法为该对端创建虚拟设备。',
  'halReason.removedWhileOffline': '已按「断开后移除虚拟设备」移除；对端重连后以相同 UID 恢复。',
  'halReason.modeA': '当前为模式 A：虚拟设备仅存在于模式 B。',
  'halReason.modeShare': '当前为共享模式：本机对外提供设备而不使用对端设备，因此不存在虚拟设备。',
  'halReason.other': '暂无虚拟设备（{reason}）。',
  'halReason.none': '暂无虚拟设备。',

  // ---------------------------------------------------------------- 设备
  'device.state.bound': '已发布',
  'device.state.pending': '等待驱动确认',
  'device.state.delisted': '正在移除',
  'device.state.free': '未发布',
  'device.inUse': '● 使用中',
  'device.idle': '○ 未使用',
  'device.awaiting': '○ 等待系统发布',
  'device.frames': '{n} 帧',
  'device.dropped': '丢 {n}',
  'device.slotGen': '槽位 {slot} · 代号 {gen}',
  'device.speaker': '扬声器',
  'device.microphone': '麦克风',

  // ---------------------------------------------------------------- 一级指标：延迟
  // 一级界面只回答三个问题：这台主机在不在 / 我在用它做什么 / 用起来好不好。
  // 「延迟」与「音质」补的是第三个（spec-telemetry-ia §2.1）。
  'metric.latency.label': '延迟',
  'metric.latency.value': '{ms} ms',
  // 「≥」不是修辞：声卡自身的缓冲读不到，Σ 各级必然是下限（plan §7.6 补充裁定）。
  'metric.latency.valueLower': '≥{ms} ms',
  'metric.latency.none': '—',
  'metric.latency.measuring': '测量中…',
  'metric.latency.unsupported': '对端版本较旧，无法测量',
  'metric.latency.grade.imperceptible': '几乎无感',
  'metric.latency.grade.conversational': '可用于对话',
  'metric.latency.grade.noticeable': '明显延迟',
  'metric.latency.grade.unusable': '不适合互动',
  'metric.latency.footnote': '系统链路延迟，不含蓝牙 / HDMI 等外部链路的附加缓冲。',
  'metric.latency.lowerBoundWhy': '未含声卡固有缓冲，实际略高于此值。',
  'metric.latency.expand': '查看分段',
  'metric.latency.collapse': '收起分段',
  // 范围标记：读数只覆盖本机这一侧时挂在等级词的位置上。
  // 为什么要单独一条而不是复用「≥」：「≥」说的是「还要再多一点」，而这里缺的是
  // **对方整整一半管线**，量级无上界。只给「≥474 ms」而不说缺了谁，用户会把它
  // 读成端到端总延迟——那正是这次要消灭的误读。文案直说缺的是什么，不说黑话。
  'metric.latency.scopeLocal': '未含对方主机',
  'metric.latency.scopeLocalWhy': '该读数仅统计本机侧。对端尚未上报其管线分段，端到端实际延迟高于此值。',

  // 无会话、只连着控制通道时的读数（PeerState.net_ms = 控制面 min-RTT / 2）。
  //
  // 它与上面那个「延迟」**不是同一个量**，所以标签、字号、措辞全部另起一套：
  // 实测网络单程 0.58 ms，而同一条链路上的感知延迟约 1000 ms——相差三个数量级。
  // 占大头的是缓冲与声卡，而那两段**要等真的有音频在流动时才量得到**。
  // 因此这里的值自带「仅网络」后缀，旁边再挂一枚 warn 色标记：任何一处单独被看到、
  // 被截图、被复制走，都不能被读成端到端总延迟。
  'metric.latency.netOnlyLabel': '网络单程',
  'metric.latency.netOnlyValue': '{ms} ms（仅此一段）',
  'metric.latency.netOnlyScope': '不是总延迟',
  'metric.latency.netOnlyNote': '不含缓冲与声卡固有延迟，而这两段占主要比重。',
  'metric.latency.netOnlyWhy': '仅为数据包在两台主机之间的单程传输时延。占大头的缓冲与声卡延迟要等音频真正流动才测得到。',
  'metric.latency.netOnlyRtt': '最近一次往返时延 {ms} ms（供交叉校验）。',
  'metric.latency.netOnlyMeasuringWhy': '正在采集最小往返时延的样本（约十余秒）；样本不足时不给出读数。',

  // 一级四段：面向用户的说法，不出现 FIFO / JitterBuffer 这类内部词。
  // 没有第五段「设备」：色带是按音频流向排的时间轴，而两个声卡固有延迟分别落在
  // 链路的两端，一个在轴上出现两次的集合占不了一个连续色块（详见 lib/metrics.ts
  // 的 LATENCY_SEGMENTS 注释）。它们并进「采集」与「播放」，在明细里各占一行。
  'latency.seg.network': '网络',
  'latency.seg.capture': '采集',
  'latency.seg.buffer': '缓冲',
  'latency.seg.playback': '播放',
  // 段名会撒谎，这一句是它的解药。一段里并列着好几级：`playback` 段同时装着
  // 真实播放环、桥接虚拟声卡环、**虚拟麦克风环**三条并行尾级。2026-08-04 现场
  // 接收方向 136 ms 全在虚拟麦克风环上，段名却写着「播放」。
  'latency.seg.dominant': '该段当前的主要构成：{name}',

  // 就地展开的逐级明细：这里才出现内部级名，并各带一句说明。
  //
  // ⚠ 这些说明一律**不写「本机 / 对方」**，主机由每行的 stage-host 标签给。
  // 原文把主机烤进了句子里（「对方声卡把声音交给…」「本机等待送进声卡的音频」），
  // 那是照着 recv 会话写的：延迟的物理定义是「从对方声卡采到、到本机声卡送出」。
  // 可 **send 会话上两边正好互换**——本机才是提供方。于是一条 send 会话会渲染成
  // 「本机」标签配「对方声卡……」的说明，两者当场打架，而排障时指错机器比不说
  // 更糟。主机只在一个地方说，就不会有第二个地方说错。
  'latency.stage.capRing.name': '声卡采集缓冲',
  'latency.stage.capRing.desc': '采样在声卡交付给 AudioHub 之前的排队时长。',
  'latency.stage.capDev.name': '声卡采集延迟',
  // ⚠ 「计入总延迟」这半句是必须的。这两级在 2026-08-04 之前**从未被上报**，
  // 接上之后总延迟的数字会往上跳一截——不写清楚，用户会把「一直存在、只是这次
  // 才算进来的那段」读成一次性能退化。
  'latency.stage.capDev.desc': '声卡自拾取声音到交付采样之间的固有延迟，计入总延迟。',
  'latency.stage.srcFifo.name': '发送队列',
  'latency.stage.srcFifo.desc': '采集侧等待封包发送的音频。',
  'latency.stage.halSpk.name': '虚拟扬声器环',
  'latency.stage.halSpk.desc': '应用已写入虚拟扬声器、尚未被 AudioHub 取走的音频。',
  'latency.stage.sendPace.name': '打包节拍',
  'latency.stage.sendPace.desc': '发送侧以 10 ms 为周期封包，单个采样平均等待半个周期。',
  'latency.stage.network.name': '网络单程',
  'latency.stage.network.desc': '数据包在两台主机之间的单程传输时延。',
  'latency.stage.jitterBuf.name': '抖动缓冲',
  'latency.stage.jitterBuf.desc': '为吸收网络抖动而预留的缓冲深度。',
  'latency.stage.postMix.name': '混音对齐缓冲',
  'latency.stage.postMix.desc': '将长度不齐的解码输出对齐为整帧的缓冲。',
  'latency.stage.playRing.name': '播放队列',
  'latency.stage.playRing.desc': '等待送入声卡的音频。',
  'latency.stage.bridgeRing.name': '虚拟声卡队列',
  'latency.stage.bridgeRing.desc': '等待写入桥接虚拟声卡的音频。与播放队列并行，不叠加。',
  'latency.stage.halMic.name': '虚拟麦克风环',
  'latency.stage.halMic.desc': '已写入虚拟麦克风、尚未被应用取走的音频。与播放队列并行，不叠加。',
  'latency.stage.playDev.name': '声卡播放缓冲',
  // 「常常是最大的一段」不是修辞：30-win 实测 41.9 毫秒（写进系统到真正出声），
  // 其中 30 毫秒是 Windows 共享音频引擎与 KS 传输，换一块声卡也一样。
  'latency.stage.playDev.desc': '音频提交给系统到声卡实际发声之间的固有延迟，计入总延迟；通常是最大的一段。',
  'latency.stage.residual.name': '未归属',
  'latency.stage.residual.desc': '实测总延迟与各分段之和的差值。持续偏大表明仍有未纳入统计的缓冲。',
  'latency.stage.ms': '{ms} ms',
  'latency.stage.unknown': '未知',
  'latency.stage.onPeer': '对方主机',
  'latency.stage.onLocal': '本机',

  // —— 逐级明细的事实标签（排障用）。每条都是**短标签 + 长 title**：
  // 一行里要并排放四条事实，长句会被省略号吃掉后面的（这正是改版前的实际形态：
  // 「满时丢弃最早的音频（听感：…）」一条就把整行占满，饱和/丢弃/漂移全被截掉）。
  //
  // 规格 §0.2：深度读数在丢头 / 丢尾两种语义下**完全简并**——两者饱和时都恰好等于
  // cap/rate，只有 drop_mode + dropped 的组合能把它们分开。所以这几条不是装饰。
  'latency.stage.dropOldestShort': '丢最早',
  'latency.stage.dropNewestShort': '丢最新',
  'latency.stage.dropNoneShort': '不丢弃',
  'latency.stage.dropOldest': '满时丢弃最早的音频（听感：恒定迟到但连续）',
  'latency.stage.dropNewest': '满时丢弃最新的音频（听感：迟到并伴随断续）',
  'latency.stage.dropNone': '该级不丢弃音频（有界但从不饱和，或不存在队列）。',
  'latency.stage.droppedN': '已丢弃 {n} 个样本',
  'latency.stage.droppedNone': '未丢弃',
  'latency.stage.droppedWhy': '本条会话的累计值。停滞表明仅曾饱和一次；持续增长表明生产与消费速率长期失配。',
  // `dropped: null` 与 `0` 是两个结论，界面必须分开讲——混为一谈就等于替驱动
  // 宣布「它一个样本都没丢」，而我们根本数不到那一侧。
  'latency.stage.droppedUnknown': '丢弃数不可见',
  'latency.stage.droppedUnknownWhy': '该级的丢弃发生在另一侧（驱动或对端进程）内，本机数不到。不等同于未发生丢弃。',
  'latency.stage.fill': '{pct}% 满',
  'latency.stage.fullAt': '已满（{pct}%）',
  'latency.stage.fillWhy': '{n} / {cap} 个样本；达到 95% 判定为已满。',
  'latency.stage.driftUp': '每分钟涨 {ms} ms',
  'latency.stage.driftDown': '每分钟降 {ms} ms',
  'latency.stage.driftFlat': '深度稳定',
  'latency.stage.driftWhy': '最近 30 秒的深度斜率（{sps} 样本/秒）。持续上升表明该级最终会饱和。',
  'latency.stage.driftUnknown': '趋势未知',
  'latency.stage.driftUnknownWhy': '样本量不足以判定趋势（少于 3 个采样点，或跨度不足 5 秒）。不等同于无漂移。',

  'latency.conf.full': '各分段完整',
  // 接线之前这里写的是「缺声卡缓冲」——那时两级设备延迟根本没查。现在查了，
  // 「仍是下限」的成因换成了另外两种，而两种都必须说得出口：
  // ① 系统给的声卡读数已知偏低（蓝牙 / HDMI，或 Windows 上那个靠开流标定、
  //    带 ±8 毫秒开流竞态的值）；② 这条链路的某一端压根没有实体声卡
  //    （虚拟扬声器 / 虚拟麦克风），那一小截还没建模。
  'latency.conf.lowerBound': '下限（声卡延迟未取得精确值）',
  'latency.conf.converging': '时钟对齐中，约 {s} 秒后可用',
  // 说人话版：原文「仅本机分段，对端未上报」是照着字段名写的，用户读不出后果。
  'latency.conf.localOnly': '以上仅为本机侧的分段，对端尚未上报另一半，因此不是端到端总延迟。',
  'latency.conf.deviceUnreliable': '输出设备（蓝牙 / HDMI）的延迟被系统低报，实际值更高',
  // 只在两台声卡都给出平台真值时才显示（confidence = full）。
  'latency.conf.fullWhy': '两端声卡固有延迟均已取得平台真值，该读数覆盖完整链路。',
  'latency.conf.peerStale': '对端分段为 {s} 秒前的读数',
  'latency.detail.e2e': '实测采样年龄 {ms} ms（与各分段之和的差值计入「未归属」）',

  // ---------------------------------------------------------------- 一级指标：音质
  'metric.quality.label': '音质',
  'metric.quality.none': '—',
  // 一级格显示的是**线上采样率**（`wire_rate_hz`），与详情页音质滑条的档位标签
  // 「PCM 48 kHz」同量纲、同数字。
  //
  // 这里曾经是 `metric.quality.bandwidth`（奈奎斯特带宽 = 采样率/2）。2026-08-04
  // 用户实测：设 `pcm48k`、卡片显示 24 kHz，判定「设置没生效」——两个数都对，
  // 但同一个界面上**设置用采样率、显示用带宽，差 2 倍且都叫 kHz**。
  // 带宽没有丢，它在展开明细里与采样率并排（quality.part.bandwidth.valueWithRate）。
  'metric.quality.rate': '{khz} kHz',
  // 位深进阶梯之后，**只写采样率的读数是有歧义的**：`48 kHz` 说不出它是 16 位
  // 还是 24 位，而这两档现在都存在、码率差 50%。所以一级格两个维度一起写。
  //
  // ⚠ 排版塞不下时**优先保住位深写全**，把等级词移进 title；
  // 绝不许为了短而只写一个维度——那正好回到本次要消灭的那个歧义。
  'metric.quality.rateDepth': '{khz} kHz · {depth}',
  // 位深的三个拼写。**`f32` 写成「32 bit 浮点」而不是「32 bit」**：
  // 裸的 32 与 32 位整数无法区分，而线上根本没有 32 位整数这一档。
  'metric.quality.depth.s16': '16 bit',
  'metric.quality.depth.s24': '24 bit',
  'metric.quality.depth.f32': '32 bit 浮点',
  'metric.quality.rateWhy': '线上采样率与位深，与详情页所设音质档同源。带宽为采样率的一半。',
  'metric.quality.grade.excellent': '优',
  'metric.quality.grade.good': '良好',
  'metric.quality.grade.fair': '一般',
  'metric.quality.grade.poor': '差',
  'metric.quality.worst.continuity': '受限于断续',
  'metric.quality.worst.level': '受限于破音',
  'metric.quality.worst.bandwidth': '受限于带宽',
  'metric.quality.expand': '查看构成',
  'metric.quality.collapse': '收起构成',
  // daemon 报 `grade: "unknown"`（某个分量还没攒够窗口）时的等级位文案。
  //
  // 没有它的时候，这个状态在界面上长成「有 kHz 数、没有等级词、四颗点全空」——
  // 而「四颗点全空」在视觉上与「一颗点 = 差」几乎分不开，用户只能读成**测出来很差**。
  // 一个还没测出结论的通路被读成质量最差，是这套遥测最不该犯的错：它把「不知道」
  // 伪装成了一个具体且悲观的结论，方向虽反，性质与用 0 填补缺失分项完全相同。
  'metric.quality.measuring': '测量中…',
  'metric.quality.measuringWhy': '仍有分量未积满统计窗口（通常十余秒）。此时取最小值只能得到上界，故暂不给出。',
  // grade 成立、但仍缺一块板：等级已经触底，缺席改不了结论，两件事都要说。
  'metric.quality.partial': '该等级在缺少一个分量的条件下判定；补齐后只会下调，不会上调。',

  // 这一格来自对端的测量（SessionStats.peer_quality）。
  //
  // 音质三分量（补偿、削顶、带宽）全是**接收侧**的量，所以一条纯发送的通路本机
  // 恒无读数——「送对方扬声器」的音质格此前**永远**空着，而链路其实好得很。
  // 现在由对端把它那侧测到的回传过来。必须标：数是真的，但量它的人在对面，
  // 不标就等于让本机宣称了一个它没有测点的结论。
  'metric.quality.fromPeer': '对端测得',
  'metric.quality.fromPeerWhy': '音质三分量仅接收端可测，而本条通路由本机发送，故读数由对端测得后回传。',

  'quality.part.continuity.name': '连续性',
  'quality.part.continuity.desc': '输出中非由对端原始采样构成的时长占比。',
  'quality.part.continuity.value': '{pct}% 被补偿',
  'quality.part.level.name': '电平',
  'quality.part.level.desc': '波形被削顶压缩的采样占比与压缩深度。',
  'quality.part.level.value': '{pct}% 削顶，超出 {db} dB',
  'quality.part.bandwidth.name': '带宽',
  // desc 必须说清它是**由采样率推出来的标称上限**，不是对实际频谱内容的测量。
  // 旧文案「还保留了多少高频成分」把它说成了一个实测量——而树里没有任何频谱
  // 分析，这个数恒等于采样率的一半。把推导值说成测量值，比单位混淆更难查。
  'quality.part.bandwidth.desc': '可传输的最高音频频率，等于线上采样率的一半；由采样率推导，并非频谱测量。',
  // 两个数并排：带宽是本分量本身，采样率是它的来源，也是用户在设置里设的那个数。
  // 只写带宽 ⇒ 复现一级界面那次误读；只写采样率 ⇒ 丢掉 Q3 本身。
  'quality.part.bandwidth.valueWithRate': '{khz} kHz（采样率 {rate} kHz）',
  // 旧 daemon 不上报 wire_rate_hz 时的退路：只给带宽。**不许用 ×2 补一个采样率**。
  'quality.part.bandwidth.value': '{khz} kHz',
  'quality.part.window': '统计窗口：最近 {s} 秒',

  // 站点级混音健康（求和后，不可归属到单条会话，所以不进 SessionStats）。
  // **S1 尚无渲染面**：这五条是按规格 §2.7 预登记的，P0q 接上 MixHealth 时直接可用。
  // 之所以现在就写进目录而不是那时再加，是为了让「两路重复流把声音削烂」这个判据的
  // 文案与阈值在同一次评审里定死——它是 duplicate_suspect 一票否决的唯一出口。
  'mix.health.title': '本机混音',
  'mix.health.clip': '{pct}% 的采样被削顶',
  'mix.health.contrib': '同时混入 {n} 路',
  'mix.health.duplicate': '检测到两路内容高度相似的音频叠加（相关度 {r}），叠加后幅度翻倍导致削顶。',
  'mix.health.ok': '正常',

  // ---------------------------------------------------------------- 主面板
  // 汇总条只在**异常**时出现：正常态下「已配对 N 台 · 在线 N 台」是一句谁都不会读的话。
  'peers.summary.offline': '离线 {n} 台',
  'peers.summary.retrying': '重连中 {n} 台',
  'peers.addManual': '添加手动对端',
  'peers.form.fingerprint': '对端指纹',
  'peers.form.fingerprintPlaceholder': '对端指纹（可输前缀）',
  'peers.form.addr': '地址',
  'peers.form.addrPlaceholder': 'IP 或 IP:端口（留空使用最近地址）',
  'peers.form.needFingerprint': '请填写对端指纹（可输前缀）',
  // M8 P6：地址一格现在同时接受 URL 形态（plan §16.2「地址即传输选择」）。
  'peers.form.addrPlaceholder2': 'IP、IP:端口，或 ws://主机[:端口]/路径（留空使用最近地址）',
  'peers.form.done': '连接请求已完成',

  'peers.card.unnamed': '未命名主机',
  'peers.card.viewDetail': '查看 {name} 详情',
  // peers.card.alias 已删（规格 §2.3 ①）：改名后原主机名走卡片标题的 title，
  // 详情页另有一张 AliasCard；徽章只是把同一条信息又印一遍。
  'peers.card.noSession': '未建立通路',
  'peers.card.reconnecting': '重连中…',
  'peers.card.reconnectingIn': '重连中…（{s}s 后重试）',
  'peers.card.inboundMic': '对方正在取用本机麦克风',
  'peers.card.inboundMicN': '对方正在取用本机麦克风（{n} 路）',
  'peers.card.takeMic': '取对方麦克风',
  'peers.card.sendSpk': '送对方扬声器',
  'peers.card.monitor': '监听接收音频',
  'peers.card.volumeLabel': '{name} 的扬声器音量',
  'peers.card.streamIn': '接收',
  'peers.card.streamOut': '发送',
  'peers.card.idle': '空闲',
  'peers.card.kbps': '{v} kbps',

  // —— 卡片指标区按方向分栏（2026-08-04 事故的界面修复）
  //
  // 病灶：`sess={micS || spkS}` 在两条真实存在的通路里选了一条，屏幕上只剩
  // 接收方向的 170 ms，而发送方向实测 105 ms 一次都没出现过；四段色带又把
  // `hal_mic`（虚拟麦克风环）的 136 ms 写成「播放 136」。用户据此得出
  // 「扬声器慢」，方向完全反了。所以这一组文案的任务只有一个：
  // **让每一个数字前面都先有方向。**
  'peers.card.dirIdle': '未开通',
  'peers.card.dirMulti': '{n} 路 · 显示最慢的一条',
  'peers.card.dirMultiWhy': '该方向有多条并发通路，此处显示最慢的一条——并发时的表现由最差的一路决定。',
  // 延迟档的**作用对象**按方向不对称，这两句是它的界面化。
  //
  // daemon 的 `servo_pass` 只遍历本机的接收流：发送方向那半条链路的抖动缓冲
  // 在对端，由对端自己的延迟档管，本机没有执行器。不说的话，一台只发不收的
  // 使用端拖了延迟滑条会看到「两栏里只有一栏在动」，唯一自然的结论是
  // 「设置只生效了一半」——而系统是对的。设置页早已为此开了一条文案
  // （settings.transport.noRecvStream），这两句是把同一条教训搬到卡片上。
  'peers.card.dirGovLocal': '本机为接收端，延迟档在该方向生效，调节的是本机的抖动缓冲。',
  // ⚠ 语料里不许出现 Markdown 记号：这两句会直接进 `title` 与 `.metric-foot` 的
  // 纯文本节点，`**…**` 会原样显示成四个星号。第一版写了，实测截图里就是那样。
  'peers.card.dirGovPeer': '本机为发送端，该半程的缓冲位于对端，由对端自己的延迟档决定。',

  // 模式 B 下，虚拟麦克风已经真的出现在系统设备列表里、但还没有任何应用打开它。
  //
  // 「接收」那一行此前只显示「空闲」，与「对端离线」「驱动没起来」长得一模一样——
  // 用户明明知道麦克风是通的，界面却什么都不肯说。这一行说的是**状态**，不是数据：
  // 没有音频在流动时不存在码率、不存在电平，任何数字都会是编的。
  // 只有 hal_device.observed 为真（设备确实在系统里）且对端在线时才敢这么说。
  'peers.card.micReadyShort': '就绪',
  'peers.card.micReady': '通路就绪 · 暂无应用占用',
  'peers.card.micReadyWhy': '虚拟麦克风已列入系统设备列表，且对端在线。任意应用选中它即开始传输。',

  // plan §13 推论 1：对端处于使用端模式时无法被本机调取。三条分开写，因为
  // 「它在模式 A」和「它在模式 B」对用户的意义不同（后者说明对面正把本机之外的
  // 某台主机当设备用），而「认不出的模式」只能含糊其辞、绝不能冒充前两者。
  'peers.unusable.modeA': '该主机正处于模式 A，在使用其它主机的设备。请在该主机上切换到共享模式。',
  'peers.unusable.modeB': '该主机正处于模式 B，在使用其它主机的设备。请在该主机上切换到共享模式。',
  'peers.unusable.unknownMode': '该主机上报了本版本无法识别的运行模式。',
  'peers.unusable.badge': '不可被使用',

  // 原来每张卡片各印一遍（同一句话在 N 张卡上重复 N 次）：现在只在卡片列表底部渲染一次。
  'peers.devices.offline': '⚠ 对端离线：设备仍列于系统中可供选择，但不处理任何音频。',
  'peers.devices.settling': '设备已下发，正在等待系统刷新设备列表。',

  'peers.empty.title': '请先在两台设备上完成配对',
  'peers.empty.step1': '在两台设备上都打开 AudioHub',
  'peers.empty.step2': '在本机打开「配对向导」生成 6 位 PIN',
  'peers.empty.step3': '另一台设备发现本机后输入同一 PIN',
  'peers.empty.openPair': '打开配对向导',

  'peers.bridgeUnavailable': '虚拟声卡「{name}」当前不可用，本次不桥接。',
  'peers.reopenFailed': '旧会话 #{id} 未能关闭，请在对端详情页手动关闭。',

  // ---------------------------------------------------------------- 音量控件
  'volume.label': '对方扬声器音量',
  'volume.mute': '静音',
  'volume.unmute': '取消静音',
  'volume.muted': '已静音',
  'volume.pct': '{n}%',
  'volume.mutedPct': '已静音 · {n}%',
  'volume.failed': '音量调节失败，请稍后重试',
  'volume.unadjustable': '对端设备不支持音量调节',
  'volume.softwareGain': '对端设备不支持音量调节，已由本机软件增益接管',
  'volume.reading': '正在读取对端音量…',
  'volume.noSync': '该会话未启用音量同步',
  'volume.notAdjustable.tag': '不可调',

  // ---------------------------------------------------------------- 桥接
  'bridge.label': '桥接到虚拟声卡',
  'bridge.none': '不桥接',
  'bridge.undetected': '未检测到虚拟声卡',
  'bridge.notReported': '服务未上报',
  'bridge.staleOption': '{name}（未检测到）',
  'bridge.stale.reselect': '「{name}」当前未检测到，本次不桥接。请另选一张可用的声卡。',
  'bridge.stale.reinstall': '「{name}」当前未检测到，本次不桥接。重新安装该声卡并重启本应用即可恢复。',
  'bridge.noField': '当前服务未上报虚拟声卡信息，无法桥接。',
  'bridge.presentUnusable': '已检测到 {names}，但它不在系统输出设备列表中，无法写入。',
  'bridge.nothing': '未检测到虚拟声卡。AudioHub 不代为安装驱动——请自行安装下列任一款并重启本应用。',

  // ---------------------------------------------------------------- 共享来源（模式 A 的 spk 方向）
  // plan §7.1：模式 A 的「送对方扬声器」= 捕获本机系统音频送对方默认输出播放。
  // 麦克风是可选来源，不是默认值。文案里绝不出现「把系统输出切到某某设备」（plan §6 红线）。
  'share.label': '共享来源',
  'share.source.sysaudio': '系统音频',
  'share.source.mic': '麦克风',
  'share.sys.none': '本机无可用的系统音频捕获后端，仅能共享麦克风。',
  'share.backend.label': '捕获后端',
  'share.backend.autoOption': '自动',
  'share.backend.selected': '「{name}」：{note}',
  'share.backend.unknown': '当前服务未上报可用后端清单。',
  'share.backend.stale': '当前服务不识别后端「{id}」，启用时将直接报错。请改回「自动」。',
  'share.backend.staleOption': '{id}（当前服务未提供）',
  'share.backend.optionUnavailable': '{name}（本机不可用）',
  'share.backend.optionDeclined': '{name}（本项目不提供）',
  'share.perm.hint': '需「系统音频录制」授权，首次启用时由系统询问。',
  'share.perm.goto': '前往授权',
  'share.fault': '⚠ 上次开启失败：{reason}',
  'share.fault.unknown': '服务未说明原因',

  // 后端目录。id 必须与 core/audiohub-core/src/sysaudio.rs 的 BACKEND_* 常量一致。
  // 服务上报了自己的 note 时优先用它（它带本机实际版本号 / 上次被拒绝的事实）。
  'sysaudio.backend.winProcExclude.label': 'Windows 进程环回（排除自身）',
  'sysaudio.backend.winProcExclude.note': '排除 AudioHub 自身的播放输出，不会把对端音频回送。需 Windows 10 2004 及以上。',
  'sysaudio.backend.winDeviceLoopback.label': 'Windows 设备环回',
  'sysaudio.backend.winDeviceLoopback.note': '兼容较旧系统的回退方案；它会一并采集本应用的输出，双向互送时可能形成回授。',
  'sysaudio.backend.macCatap.label': 'macOS 音频进程 Tap',
  'sysaudio.backend.macCatap.note': '首选方案：只需「系统音频录制」权限，且排除本应用自身的输出。需 macOS 14.2 及以上。',
  'sysaudio.backend.macSck.label': 'macOS 屏幕捕获音频流',
  // 2026-08-09 裁定不做（plan §6 / §11.2）。这句是用户唯一能看到的解释，所以必须说清
  // 「不做」而不是「还没做」——后者会让人一直等一个不会来的版本。
  'sysaudio.backend.macSck.note': '本项目不提供该路线：它需要「屏幕录制」权限，而进程 Tap 只需「系统音频录制」。',

  // ---------------------------------------------------------------- 会话
  'session.flow.micRecv': '取对方麦克风',
  'session.flow.micSend': '对方取用本机麦克风',
  'session.flow.spkSend': '送对方扬声器',
  'session.flow.spkRecv': '对方送入本机扬声器',
  'session.short.micRecv': '对方麦克风',
  'session.short.micSend': '本机麦克风',
  'session.short.spkSend': '对方扬声器',
  'session.short.spkRecv': '本机扬声器',
  'session.dir.send': '发送',
  'session.dir.recv': '接收',
  'session.tag.peerInitiated': '对端发起',
  'session.tag.virtualDevice': '虚拟设备',
  'session.managed': '由系统设备选择驱动',
  'session.closed': '会话 #{id} 已关闭',

  // ---------------------------------------------------------------- 详情
  'detail.back': '返回主面板',
  'detail.notFound.title': '未找到该对端',
  'detail.notFound.desc': '该对端可能已被移除，或服务尚未返回列表。',
  'detail.reconnecting': '重连中…',
  'detail.identity': '身份',
  'detail.defaultPort': '默认端口',
  'detail.pairedAt': '配对时间',
  'detail.publicKey': '公钥',
  'detail.fpCopied': '已复制完整指纹',
  // 失败提示改走 common.copyFailed（设置页「本机身份」也复制指纹，两处必须同一条）。

  'detail.alias.title': '别名',
  // 详情头那枚 ✎ 的 aria-label / title。按钮没有文字，这就是它唯一说得出的话。
  'detail.alias.openLabel': '编辑别名',
  // 后果句（plan §3.1 第 4 类）。改名不只影响本页——它会就地改掉系统设备列表里
  // 那两台设备的名字，而那正是用户不敢按「保存」的原因。
  'detail.alias.effect': '会一并改掉这台对端在系统设备列表里那两台设备的名字。',
  'detail.alias.field': '显示名称',
  'detail.alias.placeholder': '对端主机名',
  'detail.alias.renamed': '已改名为「{name}」',
  'detail.alias.restored': '已恢复为对端主机名',

  'detail.devices.title': '虚拟设备',
  // `detail.devices.modeA`（「当前为模式 A，没有虚拟设备。」）在 2026-08-10 的
  // 重整里删除：那一块现在只在**请求了模式 B** 时渲染，于是这句话没有任何时刻
  // 说得出口——它恰好是 plan §3.1 禁掉的那种纯描述句。
  'detail.devices.published': '两台设备已列入系统音频设备列表，可供任意应用选用。',
  'detail.devices.offline': '⚠ 对端离线：设备仍列于系统中可供选择，但不处理任何音频。',
  'detail.devices.stateListed': '驱动状态「{state}」，系统设备列表已列出这两台设备。',
  'detail.devices.stateUnlisted': '驱动状态「{state}」，系统设备列表尚未列出这两台设备。',

  'detail.addrs.title': '地址历史',
  'detail.addrs.empty': '暂无地址记录',
  'detail.addrs.seenAt': ' 最近见于 {time}',
  'detail.addrs.fromDaemon': ' daemon 记录',

  'detail.sessions.title': '活跃会话',
  'detail.sessions.empty': '与该对端当前无活跃会话。',
  'detail.sessions.colSession': '会话',
  'detail.sessions.colFlow': '用途',
  'detail.sessions.colDir': '方向',
  'detail.sessions.colBitrate': '码率',
  'detail.sessions.colRung': 'RUNG',
  'detail.sessions.colLoss': '丢包',
  'detail.sessions.colJitter': '抖动',
  'detail.sessions.colVolume': '音量',
  'detail.sessions.colVerdict': '校验',
  'detail.sessions.colAction': '操作',
  'detail.volume.localOut': '本机输出设备音量',
  'detail.volume.remoteOut': '对端输出设备音量',
  'detail.verdict.pass': '通过 {snr} dB',
  'detail.verdict.fail': '未通过',

  'detail.danger.title': '危险操作',
  // 详情头那枚断链图标的 aria-label / title。
  'detail.danger.openLabel': '危险操作',
  'detail.unpair': '解除配对',
  'detail.unpair.confirmTitle': '解除配对？',
  'detail.unpair.confirmLead': '将解除与「{name}」的配对，并撤销双向信任。',
  'detail.unpair.confirmDevices': '将立即从系统移除「{out}」与「{in}」。若其中之一为当前默认设备，系统会自动切换。',
  'detail.unpair.confirmNoDevices': '该对端当前没有虚拟设备，仅移除信任关系与已建立的会话。',
  'detail.unpair.done': '已解除配对',

  // ---------------------------------------------------------------- 配对
  // 单栏之后只剩一个板块，标题就是这一页在做的事。
  // 「让对方找到我」是二级菜单的入口按钮，也是那个面板的标题。
  'pair.left.title': '接受配对',
  // **后果句**，不是描述句：开着这扇窗户期间会发生什么。§3.1 允许留在界面上的
  // 三类之一，所以它不搬 wiki。
  'pair.left.desc': '启用后本机在局域网内可被发现，并生成一次性 PIN 供对方输入。',
  'pair.left.enable': '开启配对模式',
  'pair.left.disable': '停止配对',
  'pair.left.expired': '配对模式已到期',
  // 配对窗口开着时入口按钮的样子。一个悄悄开着的窗口必须在一级界面上看得见。
  'pair.right.scan': '开始扫描',
  'pair.right.stopScan': '停止扫描',
  // 扫描窗口会自己到点收工，剩余时间是状态。
  // 「陈旧」= 有一阵子没再答复了。地址多半还有效，所以仍可点，只是不许和刚刚
  // 答复过的长成同一个样子。
  // 进页面即自动开扫，所以这里不再指路去按那个按钮。
  'pair.right.empty': '尚未发现主机。点击「开始扫描」在局域网内查找。',
  'pair.right.unknownHost': '未知主机',
  'pair.right.paired': '已配对',
  'pair.right.unpaired': '未配对',
  'pair.right.portOnly': '端口 {port}',
  'pair.right.addrLabel': '对方地址',
  'pair.right.addrPlaceholder': 'IP 或 IP:端口',
  'pair.right.pinLabel': 'PIN',
  'pair.right.pinPlaceholder': '对方 PIN',
  'pair.right.go': '发起配对',
  'pair.right.going': '配对中…',
  'pair.right.needAddr': '请填写对端地址（IP 或 IP:端口）',
  'pair.right.needPin': '请填写对端界面上显示的 PIN',
  // 三条地址形态提示，两处输入框共用。
  'addr.badUrl.noHost': '这个地址缺少主机名：{addr}',
  'addr.badUrl.badPort': '这个地址的端口不是 1–65535 之间的数字：{addr}',
  'addr.badUrl.badIpv6': 'IPv6 字面量少了右方括号：{addr}',
  // wss:// 认得，但本 build 没有 TLS 客户端——说清楚缺口是什么，而不是把它
  // 报成「地址无法识别」。
  'addr.wssUnsupported': '本版本不支持 wss://（未内置 TLS 客户端）。请填写隧道的明文入口 ws://…。',
  // 配对不走 WebSocket：P5 有意没有在复用连接上再开一条配对路径，P6 未改。
  // ⚠ 这句话指的路（「到该对端的详情里把地址改成隧道 URL」）现在**真的存在**了，
  // 就是详情页「连通方式」下面那一格；在它落地之前这是一句做不到的指路。
  'addr.pairNotOverWs': '配对暂不支持经 ws:// 隧道。请先以 IP:端口 完成配对，再在详情页改为隧道地址。',
  // 隧道地址那一格独有的一条：`192.168.1.9:47810` 在「添加对端」里完全正常，
  // 在那一格里却等于没填（daemon 读不出 WsUrl）。所以要点明「直连 = 留空」，
  // 而不是把它报成一句泛泛的「地址无法识别」。
  'addr.endpointNeedsUrl': '隧道地址必须以 ws:// 开头。直连请留空。',
  'pair.right.done': '已与「{name}」完成配对',
  'pair.right.failed': '配对失败：{message}。请确认对端已启用配对模式、PIN 未过期、地址可达。',
  'pair.step.connect': '建立连接',
  'pair.step.verifyPin': '校验 PIN',
  'pair.step.exchangeKeys': '交换密钥',
  'pair.step.done': '完成配对',

  // ---------------------------------------------------------------- 设置
  'settings.mode.title': '运行模式',

  // 本机指纹在右上徽标里改成了**悬停才显示**（plan §7.6 补充裁定）。悬停在触摸屏上
  // 不存在、在截图排障时也拿不到，所以必须有一个常驻落点——就是这一块。
  'settings.identity.fingerprint': '本机指纹',
  'settings.identity.name': '本机名称',
  'settings.identity.copied': '已复制本机指纹',
  // 点框即复制（无障碍红线：整框是真 <button>，这条是它的 aria-label）。
  // AUTOHUB_NAME 生效时这一行代替下面那条后果句：改不动的值配一个能编辑的框，
  // 用户只会以为自己保存失败了。
  // §3.1 第 4 类「后果」，获准留在界面上：改名会改掉每台对端系统里那两台虚拟
  // 设备的名字，而对端要等下一次连接才看得到。
  // 两条对应回包里 `restart_required` 的两个分支。**当前的 daemon 恒返回 true**
  // （`LocalIdentity` 被广播、监听与每条控制通道持有，本进程换不掉它），所以线上
  // 只会看到下面那条；上面这条留着是因为那个 bool 是契约的一部分，哪天热替换做得
  // 到了，UI 不必跟着改。不许把它删掉再让界面无条件说「已生效」——那是替 daemon
  // 说了它没说过的话。

  'settings.net.title': '网络',
  'settings.net.announceTitle': '在局域网内广播本机',
  'settings.net.announceNotInForce': '⚠ 已启用但广播未建立，其它主机扫描不到本机。macOS 需在「系统设置 › 隐私与安全性 › 本地网络」中允许 AudioHub，然后把本开关关掉再打开。',
  'settings.net.controlPort': '控制端口',
  'settings.net.controlPortBadge': '只读',
  'settings.net.ipcPort': 'IPC 端口',

  // 网页访问（plan §7.5）。文案有两条硬要求：一是必须说清「仅允许本机」关掉之后
  // **实际会发生什么**（无鉴权 + 令牌明文），二是不得把它写成一句泛泛的「请注意
  // 安全」——那种话没人会当真。
  // ⚠ `settings.web.title` 已删：这一块 2026-08-10 并进了「网络」，不再有块标题。
  'settings.web.enabledTitle': '启用网页访问',
  'settings.web.portTitle': '端口',
  'settings.web.portApply': '应用',
  'settings.web.portInvalid': '端口需在 1024–65535 之间。',
  'settings.web.localOnlyTitle': '仅允许本机',
  'settings.web.localOnlyBadge': '已锁定',
  // 「为什么不可用」必须说到底：只写「暂不支持」，下一个读到的人（包括半年后的自己）
  // 只会以为是没做完的开关，而不是一个有确定前提条件的设计裁定。
  'settings.web.urlLabel': '访问地址',
  'settings.web.urlLocal': '本机：{url}',
  'settings.web.urlLan': '局域网：{url}',
  'settings.web.urlLanUnknown': '局域网：使用本机在该网段的 IP 加同一端口访问（未能自动探测出口地址）。',
  'settings.web.off': '未启用。启用后此处将显示可直接打开的网址。',
  'settings.web.starting': '正在读取当前状态…',
  'settings.web.error': '没能开始监听：{message}',
  'settings.web.errorHint': '设置已保存，但端口未能绑定——最常见的原因是该端口已被其它程序占用。请更换端口后重试。',
  'settings.web.warnTitle': '关闭此开关后，本机服务的令牌将以明文提供给任何访问者',
  'settings.web.sourceDisk': '页面文件来自磁盘目录 {root}。',
  'settings.web.sourceEmbedded': '页面文件来自应用内嵌资源，与窗口里是同一份。',
  'settings.web.browserOnly': '你正经网页端查看本页。以下三项只能在应用窗口内修改，否则一次误操作即可关掉你正在用的入口。',

  // ---- plan §15：对端详情页的传输档位 ----
  // 卡片上那一行「这个数是目标不是能力」。措辞必须让用户一眼分出两件事：
  // 「我设的」与「对方要求的」。共享模式的机器只会看到后者。
  'peers.card.targetMine': '目标 {ms} ms（由本机设定）',
  'peers.card.targetByPeer': '目标 {ms} ms（由使用方设定）',

  // 这张卡在 2026-08-10 的重整后装着**四类**东西（档位 / 连通方式 / 虚拟设备 /
  // 隧道地址），叫「传输档位」就只说中了第一类。键名不动：全仓只有一个渲染点。
  'detail.transport.title': '连接',
  // §14 裁定 4：**常驻**，不是 tooltip。用户看到 300 ms 时必须能分辨
  // 「这是我自己设的目标」而非「系统只能做到这样」——当前界面对此一个字都没说，
  // 正是本次误判的直接成因。
  // 交叉的那半边要说出来，否则「我改了发送音质，为什么没反应」在界面上无解。
  // 措辞按用户视角，不提「推给对端」——那是实现细节（plan §15 裁定 3）。
  'detail.transport.colLatency': '延迟（目标）',
  'detail.transport.colQuality': '音质（目标）',
  'detail.transport.latencyIn': '接收方向的延迟目标',
  'detail.transport.latencyOut': '发送方向的延迟目标',
  'detail.transport.qualityIn': '接收方向的音质目标',
  'detail.transport.qualityOut': '发送方向的音质目标',
  // 共享模式：显示对端推来的值 + 出处。**不隐藏、不置灰成空壳**——
  // 本机真的有执行器在跑，只是被远程指挥；隐藏会让共享侧永远看不到自己
  // 机器上正在被执行什么，而本次事故里缺的正是这个视图。
  'detail.transport.sharedBy': '本机处于共享模式：收发档位由使用方（{name}）决定，此处只显示其当前要求的值。',
  // 「未设定」≠ 0，也 ≠ auto。对端没表态时按自动跑，但那与「对端明确选了
  // AUTO」是两件事，混成一个值会让共享侧读出一个对方从未做过的决定。
  'detail.transport.unset': '未设定 · 按自动运行',
  // 存盘的档位串本 build 不认识时的说明。**必须说出原值**：只说「已重置」
  // 的话，用户没有任何线索去判断自己当初选的是什么、要不要重新选。
  //
  // 这一格的前身是一层静默翻译（旧 id `pcm32k` → `pcm32k16`），它自己制造了
  // 一个真回归：同一个存盘值在详情页和总览里显示成两种写法。静默重置只是把
  // 同一个病换个方向——用户的选择消失了而界面处处自洽。所以：重置照做，说出来。
  'detail.transport.stopReset': '{dir}的{kind}原为「{old}」，本版本已无该档，已重置为自动。请重新选择。',
  // ---- 连通性档位（plan §16.2 的手动覆盖入口）----------------------------
  //
  // **说人话，不显示内部代号**（§16.4 第 2 条）：`tier1` 对用户不解释任何事，
  // 而解释正是这一区块存在的全部理由。代号只出现在 IPC 与日志里。
  //
  // 这里只做「可切换」这一半：下面这一组是**用户的选择**。链路的**现状**是
  // 另一个量，语料在 `tier.now.*`，呈现在卡片（一级）与本节顶部那一行（二级）。
  // 两者**不得互相冒充**——选「自动」的对端此刻可能正跑在 TCP 上，而这一组
  // 仍然、并且应当显示「自动」。
  'detail.transport.tierTitle': '连通方式',
  'detail.transport.tierAuto': '自动',
  'detail.transport.tierAutoHint': '由服务判断（默认）',
  'detail.transport.tier0': '直连（UDP）',
  'detail.transport.tier0Hint': '钉住直连；UDP 不通时不会自动改走 TCP',
  'detail.transport.tier1': '经 TCP 中转',
  'detail.transport.tier1Hint': '钉住 TCP；延迟与抖动明显更差',
  // 这一档**不需要**隧道地址就能选：填了地址是「带 WebSocket 外壳的复用」，
  // 不填是「裸 TCP 上的复用」，两者都是单连接复用。所以这句提示不许写成
  // 「需要隧道地址」——那会把一个此刻就生效的选择说成一个前置条件没满足的选择。
  'detail.transport.tier2': '单连接复用',
  'detail.transport.tier2Hint': '控制与两个方向的音频复用同一条连接，延迟最差。经 HTTP 隧道时在下方填 ws:// 地址。',
  'detail.transport.tierReset': '连通方式原为「{old}」，本版本不识别，已重置为「自动」。',
  // ---- 隧道地址（plan §16.2「地址即传输选择」）-----------------------------
  //
  // 这一格是**能存住**隧道地址的唯一界面入口。「添加对端」那一格也收 ws://，
  // 但它走 peers.connect，daemon 明写那个 URL 不落盘 ⇒ 重连即失忆。
  'detail.transport.endpointTitle': '隧道地址',
  'detail.transport.endpointField': '对端地址（ws://）',
  'detail.transport.endpointPlaceholder': 'ws://隧道主机[:端口][/路径]',
  'detail.transport.endpointSaved': '已保存隧道地址：{addr}',
  'detail.transport.endpointCleared': '已清除隧道地址，恢复为按配对时记录的地址直连。',
  'detail.transport.endpointShadow': '已填写隧道地址：本机主动连接一律走单连接复用，上方的「{tier}」对出站不生效。',
  'detail.transport.endpointReset': '隧道地址原为「{old}」，本版本无法解析，已清空。',
  // ---- 链路**现状**（plan §16.4）------------------------------------------
  //
  // 与上面那一组（用户的选择）是两个量。这一组回答「此刻字节实际走在哪条路上」。
  //
  // 三条纪律，逐条对应 §16.4：
  //   2. 说人话：写传输形态，不写 `tier1`。
  //   3. Tier 0 不在卡片上挂徽标（只在二级页面那一行出现）——常驻的「一切正常」
  //      标记只会训练用户忽略那个位置。
  //   5. 「未判定」写灰色的「—」，与 Tier 0 的「直连（UDP）」不是同一个样子。
  //
  // ⚠ 后果那一句写的是「**更容易卡顿**」，不是「延迟更高」。TCP 的握手和 UDP
  // 差不多快，初始延迟不一定高；变差的是抖动下的表现——一次 RTO 就是 200–300 ms。
  // 跨机实测 150 s 增量：Tier 1 上 jb_underruns +4 / jb_dropped +13，Tier 0 上都是 0。
  // 写成「延迟更高」会让用户去盯一个可能根本没动的毫秒数，然后判定这句提示是假的。
  'tier.now.title': '这台对端的媒体此刻实际走的通路，由服务按真实传输判定。',
  'tier.now.cap': '当前连接方式',
  'tier.now.tier0': '直连（UDP）',
  'tier.now.tier0Why': '延迟与抖动最好',
  'tier.now.tier1': '经 TCP 中转',
  'tier.now.tier1Why': 'UDP 不通，功能不减，但更容易卡顿',
  'tier.now.tier2': '经隧道复用',
  'tier.now.tier2Why': '只有应用层通路，收发共用一条连接，最容易卡顿',
  // 两种「不知道」需要相反的下一步，所以不合并成一句「未知」。
  'tier.now.unknownUnsupported': '当前服务不上报连接方式',
  'tier.now.unknownOffline': '对端未连接，没有可判定的通路',
  'tier.now.linkAddr': '链路 {addr}',
  'tier.now.linkAlive': '链路存活',
  'tier.now.linkDead': '链路已断',
  'tier.now.linkAliveUnknown': '链路存活状态未上报',
  'tier.now.writeq': '发送队列积压 {ms} ms',
  'tier.now.stale': '超时丢弃 {n} 帧',
  'tier.now.muxFrames': '控制帧 发 {w} / 收 {r}',
  // §16.4 第 4 条点名要二级页面给出「原因」与「判定时间」，而 daemon 还不上报
  // 这两项。明说出来，不留白——留白会被读成「没有原因」。
  'tier.now.reasonGap': '当前服务不上报降级的原因与判定时间，以上是能拿到的全部现场数据。',
  'detail.transport.noStream': '该方向当前没有音频流，暂无实测读数。',
  'detail.transport.measuring': '正在测量，暂无读数。',
  'detail.transport.liveMs': '实测 {n} ms',
  'detail.transport.liveAtFloor': '实测 {n} ms · 已贴住物理下限',
  'detail.transport.liveAtCeiling': '实测 {n} ms · 已贴住物理上限',
  // 这一行紧贴在音质滑条**正下方**，而滑条档位标签写着「PCM 48 kHz」。
  // 它此前显示奈奎斯特带宽（24），于是相邻两行是「PCM 48 kHz」与「线上 24 kHz」
  // ——全应用里单位混淆最刺眼的一处。现在两行同量纲，且措辞点名是采样率。
  'detail.transport.liveKhz': '线上采样率 {n} kHz',
  // 位深进阶梯之后这一行要把两个维度都写出来，理由同 `metric.quality.rateDepth`。
  // 位深读不到（旧对端）时退回上面那条只写采样率的——**不许猜一个 16 bit 填上**。
  'detail.transport.liveFormat': '线上格式 {khz} kHz · {depth}',
  // 两个档位的权威解释（`settings.transport.latencyDesc` / `qualityDesc`）的
  // 展开开关。§15 把档位搬到详情页时那两条语料的渲染点留在了设置页 ⇒ 成了死键，
  // 于是「位深是什么、为什么带宽翻倍、AUTO 为什么不会自己上去」界面上没有一处
  // 说得出。收起态是因为那两段很长，而这张卡的主角是四个控件。

  'settings.transport.title': '传输',
  'settings.transport.auto': 'AUTO',

  'settings.transport.latency': '延迟档',
  // 「这是总延迟的目标，不是某一级缓冲的大小」必须写死在文案里：把它读成缓冲大小的
  // 人，会以为调到 200 ms 就是「多缓冲 200 ms」，于是永远不明白为什么读数不听话。
  'settings.transport.latencyLowest': '尽可能低',
  'settings.transport.ms': '{n} ms',

  'settings.transport.quality': '质量档',
  // ⚠ 这句话是「采样率 / 带宽」这一对的**权威解释**，措辞不能再把两者说成一回事。
  // 旧版写「可调的是采样率，也就是能传过去的音频带宽（上限为采样率的一半）」——
  // 一句里先说「就是」再说「一半」，正是界面上那次 48/24 误读的文字版。
  'settings.transport.q.auto': 'AUTO',
  // 「64k」→「64 kbps」：**同一条滑条上 Opus 档是码率、PCM 档是采样率**，两种量纲
  // 并排。这是编解码器的惯例（Opus 按码率参数化、PCM 按采样率），改不了，但
  // 「64k」与「48 kHz」摆在一起时，那个光秃秃的 k 邀请用户去比 64 和 48。
  // 写全单位就比不起来了——一处零成本的消歧，与本轮 48/24 那处同源。
  'settings.transport.q.opus64': 'Opus 64 kbps',
  'settings.transport.q.opus128': 'Opus 128 kbps',
  'settings.transport.q.opus256': 'Opus 256 kbps',
  // 六档 PCM：**两个维度都写在主标签里**（用户裁定）。
  //
  // 业界（Spotify / Apple Music / Qobuz / RAVENNA）一致把档位名写成定性词
  // （`Lossless` / `Hi-Res`），参数只出现在说明文字里。我们与它们相反，
  // 三条本项目特有的理由：① 用户明确要求两维写全；② 这条滑条本来就是数字标签
  // （Opus 档按码率、PCM 档按采样率，两种量纲并排是编解码器的惯例）；
  // ③ 本项目刚因「数字量纲不写全」栽过一次（设 `pcm48k` 却在卡片看到 24 kHz，
  // 用户判定「没生效」），当时确立的处置惯例是**写全单位**，不是删掉数字。
  //
  // 折中：主标签写参数，副标签（`qSub.*`）写定性词，两条信息都在。
  'settings.transport.q.pcm16k16': 'PCM 16 kHz · 16 bit',
  'settings.transport.q.pcm24k16': 'PCM 24 kHz · 16 bit',
  'settings.transport.q.pcm32k16': 'PCM 32 kHz · 16 bit',
  'settings.transport.q.pcm48k16': 'PCM 48 kHz · 16 bit',
  'settings.transport.q.pcm48k24': 'PCM 48 kHz · 24 bit',
  'settings.transport.q.pcm48k32f': 'PCM 48 kHz · 32 bit 浮点',
  // 副标签：**只用词，不用数**。
  //
  // ⚠ 这里绝对不许再出现第二个 kHz 数字（例如「8 kHz 带宽」）——那正好把
  // 「设置用采样率、显示用带宽」那次 48/24 误读原样搬进副标签。
  'settings.transport.qSub.auto': '随链路自动升降',
  'settings.transport.qSub.pcm16k16': '语音清晰',
  'settings.transport.qSub.pcm24k16': '语音优先',
  'settings.transport.qSub.pcm32k16': '均衡',
  'settings.transport.qSub.pcm48k16': '全带宽（推荐）',
  'settings.transport.qSub.pcm48k24': '全带宽 · 高精度',
  // 「不再量化」是可辩护的措辞：管线内部全程 f32，这一档的编解码退化成小端
  // 字节序搬运，**线路这一段**不做任何量化。它没有承诺端到端无损——
  // 两端的虚拟设备端点各有自己的位深上限，线路再深也补不回来。
  'settings.transport.qSub.pcm48k32f': '全带宽 · 不再量化',
  'settings.transport.qBlocked': '本版本暂不支持这一档。',
  'settings.transport.qBlockedOpus': '本次构建未链接 libopus，这一档不可用。',

  // plan §15：全局滑条下线，这个位置换成只读总览 + 一次性迁移说明。
  // **位置不许留空**——区块凭空消失 = 用户找不到、也没被告知搬去哪了，
  // 正是 §15 那个病根（「界面对此一个字都没说」）换个位置复发。
  'settings.transport.noPeers': '尚无已配对的对端。配对后，每台对端的四个传输档位将列于此处。',
  'settings.transport.colPeer': '对端',
  'settings.transport.colDir': '方向',
  'settings.transport.colLatency': '延迟（目标）',
  'settings.transport.colQuality': '音质（目标）',

  // 「模式选项」（用户 2026-08-10 第 2 条）：模式 A 与模式 B 各自的配置项合并成
  // 一块，按选中的模式整组出现。
  //
  // ⚠ `modeAVolume.title` / `devices.title` / `bridge.title` 三条**块标题**随这次
  // 合并一并消失：三组内容已经按模式分开，组标题只剩装饰作用。
  // `modeAVolume.noteInForce` / `.noteIdle`（「当前不生效」）同理是死键——按模式
  // 显示之后它们出现即生效，那句话恒为假。
  'settings.modeAVolume.syncTitle': '与对端音量同步',
  'settings.modeAVolume.muteTitle': '静音本机输出',
  'settings.devices.removeTitle': '断开后移除虚拟设备',
  'settings.devices.markOfflineTitle': '离线时标注设备名',
  'settings.devices.inventory': '设备清单',
  'settings.devices.count': '已用 {used} / {cap}',
  'settings.devices.countNa': '不可用',
  'settings.devices.tagPublished': '已发布',
  'settings.devices.tagMissing': '未出现在系统中',
  'settings.devices.noteNoDriver': '本机未安装 AudioHub 驱动，因此没有虚拟设备。',
  'settings.devices.noteModeB': '配对一台对端后，它将出现在系统音频设备列表中。',
  'settings.devices.noteModeA': '当前为模式 A，没有虚拟设备。',

  'settings.bridge.detected': '已检测到',
  'settings.bridge.notInOutputs': '不在输出列表',
  'settings.bridge.notDetected': '未检测到',
  'settings.bridge.noneReported': '当前服务不上报虚拟声卡信息。',
  'settings.bridge.noneOffline': '服务未连接，暂无检测结果。',
  'settings.bridge.noneFound': '未检测到任何虚拟声卡。',

  // plan M9「开机自启」。2026-08-10 起这几行住在「杂项」里，所以没有块标题。
  //
  // ⚠ `autostartDescMac` / `...Win` 一并删除：它们是当时仅存的两条挂在
  // `SettingRow.note` 上的**描述文本**（讲的是「注册在哪里、关闭即删除不留残余」），
  // 里面没有用户需要立刻执行的动作，整段进 wiki，界面上只留那枚 `?`。
  'settings.startup.autostartTitle': '开机时自动启动 AudioHub',
  'settings.startup.target': '登录时启动',
  'settings.startup.unsupported': '当前形态无法设置开机自启：{reason}',
  // `supported=false && enabled=true`：登录项是**别的形态**（装好的 App）留下的，
  // 活得比它长。这句话必须同时说清三件事：它还在生效、当前形态开不了新的、
  // 但**关得掉**——否则用户看到的是一条自己开过、界面却答不出状况的登录项。
  'settings.startup.orphaned': '开机自启仍在注册状态，由已安装的 AudioHub.app 写入。当前形态改不了它的指向（{reason}），但可在此关闭。',
  'settings.startup.unknown': '当前服务不提供开机自启接口（服务版本较旧）。',

  // ---- 快捷键（二级菜单：⌘/ 与「设置 › 杂项 › 快捷键」开的是同一个面板）------
  // ⚠ `settings.shortcuts.title` 已删：编辑器与速查表合并成一个面板之后，
  // 它的标题统一走 `shortcuts.sheet.title`——两条键说同一个词就会漂。
  'settings.shortcuts.resetAll': '全部恢复默认',
  'settings.shortcuts.resetAllDone': '快捷键已全部恢复默认。',

  'shortcuts.action.peers': '主面板',
  'shortcuts.action.pair': '配对向导',
  'shortcuts.action.stats': '统计诊断',
  'shortcuts.action.settings': '打开设置',
  'shortcuts.action.back': '返回主面板',
  'shortcuts.action.help': '快捷键速查表',

  // Windows 侧的修饰键读法是文字（macOS 是 ⌘⌥⌃⇧ 符号，写死在代码里，因为那是
  // Apple 自己的字形约定，不随语种变）。
  'shortcuts.mod.ctrl': 'Ctrl',
  'shortcuts.mod.alt': 'Alt',
  'shortcuts.mod.shift': 'Shift',
  'shortcuts.mod.win': 'Win',

  'shortcuts.unset': '未设置',
  'shortcuts.clear': '清除',
  'shortcuts.clearOne': '清除「{action}」的快捷键',
  'shortcuts.reset': '恢复默认',
  'shortcuts.resetOne': '把「{action}」恢复为默认快捷键',
  'shortcuts.alias': '{accel} 同样可用。',
  'shortcuts.edit.aria': '{action}，当前快捷键 {accel}，按回车修改',
  'shortcuts.edit.ariaUnset': '{action}，尚未设置快捷键，按回车修改',

  'shortcuts.record.prompt': '按下组合键…',
  'shortcuts.record.more': '…',
  'shortcuts.record.hint': 'Esc 取消 · Delete 清除',
  // 最糟的失败模式：⌘Q/⌘W/⌘M/⌘H 由 macOS 菜单先行接管，keydown 根本到不了页面，
  // 控件于是毫无反应——看起来就像坏了。静默一小会儿就把这件事说出来。
  'shortcuts.record.silent': '未收到按键？⌘Q、⌘W、⌘M、⌘H 由 macOS 菜单接管，无法重新指派。',
  'shortcuts.err.system': '{accel} 已被系统占用，按下时本界面无法收到，请另选一组。',
  'shortcuts.err.noModifier': '需至少一个修饰键（F1–F12 除外），否则会与页面上的输入争夺按键。',
  'shortcuts.warn.webview': '{accel} 可能被内置的网页快捷键拦截，不保证每次生效。',
  'shortcuts.conflict.desc': '{accel} 当前是「{action}」的快捷键。继续将解除该绑定。',
  'shortcuts.conflict.replace': '替换',
  'shortcuts.conflict.freed': '「{action}」的快捷键已被解除，现在是未设置。',

  'shortcuts.sheet.title': '快捷键',
  'shortcuts.sheet.unset': '未设置',
  'shortcuts.sheet.esc': '关闭浮层 / 取消',

  // ⚠ `shortcuts.sheet.customize`（「可在设置 › 快捷键中修改」）已删：速查表与
  // 编辑器合并成同一个面板之后，那句指路指的是它自己。

  // 「杂项」（用户 2026-08-10 第 3/5/7/8 条）：与音频链路无关的本机事务全收在
  // 这一块——身份、启动、路径，以及权限与快捷键两个二级菜单入口。
  // ⚠ 它**只**收这一类。任何与模式 / 网络 / 设备 / 传输相关的东西都不许进，
  // 否则「杂项」会变成每一个没想清楚归属的开关的去处。
  // ⚠ `settings.paths.title`（「路径」）已删：那一块并进杂项，只剩下面这一行。
  'settings.paths.configDir': '配置目录',

  // 「关于」。左上角的品牌区改成背景水印之后，App 的名字与版本只剩这一处常驻落点。
  // 版本号整句带进占位符，不在组件里拼 '版本 ' + v——语序在别的语种里会散架。
  'settings.about.title': '关于',
  'settings.about.version': '版本 {version}',

  'settings.perm.title': '系统权限',
  // Windows 侧 daemon 对每一项都回「本平台无需授权」（core/audiohub-core/src/
  // permissions.rs 的 not(macos) 分支），所以开头这句不能照抄 macOS 的规矩——
  // 那会让整段说明与它下面每一行自相矛盾。
  'settings.perm.recheck': '重新检查',
  'settings.perm.unsupported': '当前服务不提供权限查询接口，无法在此显示或申请权限。',
  'settings.perm.error': '权限探测失败：{message}',
  'settings.perm.probing': '正在探测系统权限…',
  'settings.perm.offline': '服务未连接，暂无法探测权限状态。',

  // ---------------------------------------------------------------- 统计
  'stats.uptime': '服务运行时长',
  'stats.rtt': 'IPC 往返延迟',
  'stats.sessionCount': '活跃会话',
  'stats.empty.title': '暂无活跃会话',
  'stats.empty.hintModeB': '在「系统设置 › 声音」或任意应用中选中某台对端的 AudioHub 设备后，此处将出现实时指标。',
  'stats.empty.hintModeA': '在主面板打开对端卡片上的通路开关后，此处将显示指标。',
  'stats.session': '会话 #{id}',
  // 诊断页从「会话导向」改为「先按对端聚合、再按会话展开」（spec §2.5）。
  'stats.groupBy.peer': '按对端',
  'stats.groupBy.session': '按会话',
  'stats.group.sessions': '{n} 条通路',
  'stats.waterfall.title': '延迟构成',
  'stats.waterfall.empty': '暂无活跃通路',
  // ---- 降级链路（design §5.2 第 4 条）--------------------------------------
  //
  // 这两个数是解释「降级链路为什么难听」的**唯一两个，别处看不到**。它们属于
  // 链路而不是会话（同一台对端的所有流共用一条），所以既进不了会话卡也进不了
  // 对端卡片，只能在这一页有自己的位置。
  'stats.degraded.title': '降级链路',
  'stats.degraded.writeq': '发送积压',
  'stats.degraded.writeqWhy': '帧在发送队列中的等待时长（毫秒，非深度）。上升表明写线程被 TCP 阻塞，是最直接的卡顿来源。',
  'stats.degraded.stale': '超时丢弃',
  'stats.degraded.staleWhy': '出队时已超过 200 ms 预算而被主动丢弃的帧数。它不是新增的丢包源——主动留下空洞，对端的抖动缓冲才隐藏得住。',
  'stats.degraded.writeqPeak': '积压峰值 {v} ms',
  'stats.degraded.writeqAuto': 'AUTO 上次依据 {v} ms',
  'stats.degraded.queued': '队列 {n} / {cap}',
  'stats.degraded.dropped': '队列丢弃 {n} 帧',
  'stats.degraded.frames': '帧 发 {w} / 收 {r}',
  'stats.degraded.unexpected': '非预期帧型 {n}',
  // 「空数组」与「键缺席」是两条不同的结论，**不合并**：前者是「确实没有对端在
  // 降级链路上」，后者是「这一版服务说不出来」。合并等于用一个缺席的字段去证明
  // 一切正常。
  'stats.degraded.empty': '当前没有对端运行在降级链路上（这是结论，不是「读不到」）。',
  'stats.degraded.unsupported': '当前服务不上报降级链路。',
  'stats.metric.loss': '丢包率',
  'stats.metric.jitter': '抖动',
  'stats.metric.bitrate': '码率',
  'stats.metric.rung': '质量阶梯',
  // 这个裸数字**需要一句解释**：位深进阶梯之后阶梯从四档变六档，含义静默换了
  // ——改动前 AUTO 稳态是 0，现在是 2，而 0 成了最高的 48 kHz/32 位浮点。
  // 同一个位置的同一个数字改了意思，界面上必须有一处说出来。
  'stats.metric.rungWhy': '质量阶梯上的格号，0 = 最高（48 kHz · 32 bit 浮点），数值越大档位越低；AUTO 上限为 2。',
  'stats.metric.latency': '延迟',
  'stats.metric.intact': '完整度',
  'stats.unit.pct': '%',
  'stats.unit.ms': 'ms',
  'stats.unit.kbps': 'kbps',
  'stats.unit.rung': 'RUNG',
  // 「线上」二字是承重的：本机管线恒为 48 kHz（收端非 48k 必然重采样），而这一格
  // 报的是**包头里的那个速率**，会随质量档变。不点名的话，一个 16000 会被读成
  // 「本机在用 16k 播放」，而一个 48000 会被读成「质量档没生效」——后者正是这个
  // 字段此前的实际形态：它是硬编码的 48000，无论阶梯掉到哪一档都写 48000。
  'stats.meta.sampleRate': '线上 {v} Hz',
  // 位深读得到时走这一条：**两个维度一起写**。只写采样率的话，`线上 48000 Hz`
  // 一句话对应阶梯上三档（48k/f32、48k/s24、48k/s16），码率从 768 到 1536 kbps——
  // 这正是本轮改动要消灭的那个歧义，只是搬到了统计页。
  // 位深读不到就退回上面那条（旧 daemon 不发 `wire_depth`），**不猜 16 bit**。
  'stats.meta.sampleRateDepth': '线上 {v} Hz · {depth}',
  // 两侧都报不出速率（不该发生，但 daemon 此时发 0）。**不显示「0 Hz」，也不兜底
  // 成 48000**：那个兜底就是被修掉的那个 bug。
  'stats.meta.sampleRateNone': '线上采样率 —',
  'stats.meta.channels': '{v} 声道',
  'stats.origin.hal': '虚拟设备',
  'stats.origin.halTitle': '由某个应用选中该对端的 AudioHub 设备而自动建立',
  'stats.origin.peer': '对端发起',
  'stats.origin.peerTitle': '由对端主动建立',
  'stats.vol.local': '本机输出音量',
  'stats.vol.remote': '对端输出音量',
  'stats.vol.title': '输出音量',
  'stats.extra.received': '收包 {n}',
  'stats.extra.lost': '丢包 {n}',
  'stats.extra.sent': '发包 {n}',
  // 帧是这一级的原生单位，ms 是**延迟档的单位**。只给帧，用户设了 300 ms 之后
  // 对不上这一格；只给 ms，就丢了「12 帧」这个与 MIN/MAX_TARGET 直接可比的量。
  'stats.extra.jbDepth': '缓冲 {n} 帧',
  'stats.extra.jbDepthMs': '缓冲 {n} 帧（{ms} ms）',
  'stats.extra.rungChanges': '档位变更 {n} 次',
  // 这一条会话的字节走哪条通路（daemon 的 `SessionStats.transport`）。
  //
  // 恒显示，**三态各写各的**：直连 / 降级两档 / 读不到。看着像是在给正常状态挂
  // 标记（§16.4 第 3 条禁止的事），其实不是——第 3 条禁的是**卡片上的徽标**，
  // 而 §16.4 第 5 条同时要求「已判定为直连」与「未判定」不得渲染成同一个样子。
  // 只在降级时显示的话，一台 tier 0 的会话与一台服务不上报的会话在这一页上长得
  // 一模一样，那正是第 5 条点名的那种冒充。所以：降级走上面那枚 warn 徽标（一级，
  // 与延迟数字同屏），三态的全文走这一行脚注（二级，第 4 条「二级页面保留完整
  // 信息」）。
  'stats.extra.transport': '连接方式 {v}',
  'stats.extra.transportUnknown': '连接方式 —',
  'stats.extra.transportWhy': '本条会话实际所走的通路，由服务按该流绑定的传输判定，未必等于你在详情页选的那一档。',
  'stats.extra.transportUnknownWhy': '当前服务不上报每条会话的通路。这不等同于处于直连——需两端升级至同一版本。',
  // 位深进阶梯带来的两个**静默降级**。两者的共同点：JB 的丢包率 / 抖动 /
  // 五个计数器全部一片正常，而声音已经坏了 —— 所以它们各有自己的一格，
  // 挂在别人身上就等于没有。非零才显示：两个恒为 0 的数占住位置只会训练
  // 用户忽略这一行。
  'stats.extra.halfConceal': '半帧补偿 {n} 次',
  'stats.extra.halfConcealWhy': '高位深档在线上拆成两个 5 ms 包发送，该计数为「只到达一半」的次数。持续增长表明有丢包，可将音质档下调一格。',
  'stats.extra.formatMismatch': '格式不符 {n} 包',
  'stats.extra.formatMismatchWhy': '对端声明的线上格式与其实际发送的字节数不符，此类包已丢弃。非零无良性解释：请将两端升级至同一版本。',
  'stats.extra.verdictPass': '校验通过 {snr} dB',
  'stats.extra.verdictFail': '校验未通过',
  'stats.extra.mixProbes': '混音探针 {n} 路',
  'stats.rttValue': '{v} ms',

  // ---------------------------------------------------------------- 授权门
  'onboarding.title': '开始之前，请先完成授权',
  'onboarding.grantAll': '全部授权',
  'onboarding.recheck': '重新检查',
  'onboarding.enter': '进入主界面',
  'onboarding.skip': '跳过（部分功能不可用）',
  'onboarding.skipToast': '已跳过授权：未授权的功能在调用时将直接失败。可在「设置 › 系统权限」中重新授权。',
  'onboarding.hint.busy': '正在等待系统授权对话框…请在弹出的窗口中选择「允许」。',
  'onboarding.hint.blocking': '尚缺 {n} 项必需权限：{names}。授权后即可进入主界面。',
  'onboarding.hint.ready': '必需权限已就绪。可选权限可稍后在「设置 › 系统权限」中补充。',
  'onboarding.skipNote.blocking': '跳过后界面仍可使用，但{names}相关功能在调用时将直接报错。此选择不会被记住。',
  'onboarding.skipNote.optional': '可选权限（{names}）未授权：对应的共享来源在选用时将报错。',
  'onboarding.noRequestable': '已无可直接弹窗申请的权限，请通过「打开系统设置」逐项开启。',
  'onboarding.stillMissing': '仍有 {n} 项必需权限未授权：{names}',
  'onboarding.allGranted': '必需权限已全部授权',

  // ---------------------------------------------------------------- 权限
  // 首次运行自动弹出这块面板时多的那一行（用户 2026-08-10 第 22 条）。
  // 用户自己点开的那次不显示——他知道自己点了。判据见 lib/permIntro.ts。
  'perm.defer': '稍后再说',
  'perm.requesting': '请求中…',
  'perm.action.request': '授权',
  'perm.action.openSettings': '打开系统设置',
  'perm.action.checkSettings': '在系统设置中检查',
  'perm.status.granted': '已授权',
  'perm.status.denied': '未授权',
  'perm.status.undetermined': '未确定',
  'perm.status.restricted': '受限',
  'perm.status.unknown': '未知',
  'perm.statusDeferred': '{status} · 稍后再说',
  'perm.note.undetermined': '点击「授权」后由 macOS 弹窗申请；系统仅询问一次。',
  'perm.note.deniedManual': '已被拒绝：macOS 不允许再次弹窗申请，只能手动开启。路径：{manual}',
  'perm.note.denied': '已被拒绝：macOS 不允许再次弹窗申请，只能手动开启。',
  'perm.note.restrictedManual': '受系统策略限制（如描述文件或屏幕使用时间），本应用无法申请。路径：{manual}',
  'perm.note.restricted': '受系统策略限制（如描述文件或屏幕使用时间），本应用无法申请。',
  'perm.note.unqueryable': '系统不提供查询接口，无法在此显示当前状态。',
  'perm.openManual': '请手动前往：{manual}',
  'perm.noSettingsUrl': '本机服务未提供系统设置入口。',
  'perm.settingsFallback': '若系统设置没有自动打开：{manual}',

  'perm.microphone.name': '麦克风',
  'perm.microphone.why': '将本机麦克风共享给已配对设备；仅在主动启用共享时采集。',
  'perm.microphone.manual': '系统设置 → 隐私与安全性 → 麦克风 → 打开 AudioHub',
  'perm.localNetwork.name': '本地网络',
  'perm.localNetwork.why': '在同一局域网内发现其它 AudioHub 主机，并与已配对设备直接传输音频。',
  'perm.localNetwork.manual': '系统设置 → 隐私与安全性 → 本地网络 → 打开 AudioHub',
  'perm.localNetwork.unknownNote': '系统不提供查询接口，首次使用时会询问；此前已拒绝的需手动开启。',
  'perm.systemAudio.name': '系统音频录制',
  'perm.systemAudio.why': '把本机正在播放的音频共享给对端；仅在共享来源选为「系统音频」时需要。',
  'perm.systemAudio.manual': '系统设置 → 隐私与安全性 → 系统音频录制 → 打开 AudioHub',
  'perm.unknown.name': '未知权限',
  'perm.unknown.why': '该权限由本机服务上报，界面暂无对应说明。',

  // ---------------------------------------------------------------- 错误
  // 这些是 Error.message。它们不只写进 console：rpc() 会把 message 直接 toast 出去，
  // 离线覆盖层也会把它插进「与本机服务的连接已断开（{reason}）」。所以它们同样是
  // 面向用户的文案，必须走语料。
  'error.versionMismatch': '服务协议版本不匹配（期望 {expected}，实际 {actual}）',
  'error.unknownVersion': '未知',
  'error.authTimeout': '服务认证握手超时',
  'error.authFailed': '认证失败',
  'error.requestFailed': '请求失败',
  'error.requestTimeout': '请求超时：{method}',
  'error.disconnected': '连接已断开',
  'error.connectionClosed': '连接已关闭',
  'error.cannotConnect': '无法连接本机服务',
  'error.ipcNotConnected': 'IPC 未连接',
  'error.notTauri': '非 Tauri 环境',
  'error.startFailed': '启动服务失败',
  'error.noEndpoint': '未提供连接参数',
  'error.connectTimeout': '连接服务超时',

  // ---- 公开文档（GitHub wiki）------------------------------------------------
  //
  // 这一组不是「附加阅读」，是**界面上唯一的解释出口**。用户 2026-08-10 裁定
  // 「为了简化而简化」（docs/plan.md §3.1）：功能描述一律不留在界面上，全部移进
  // wiki，界面上只剩标题、值/状态，以及一枚 `?`。
  //
  // 因此每一条都是那枚 `?` 的**无障碍名称**（aria-label + title），不是段落。
  // 写法固定为「这一块讲的是什么（英文）」——读屏念出来是一句完整的话，而
  // 「更多信息」那种写法在一页十几枚 `?` 的地方等于什么都没说。
  //
  // ⚠ wiki 是**英文**的，与界面语种无关。这些文案里因此明写「英文」——
  // 点开一个语言与预期不符的页面，是一次很容易避免的意外。
  // ⚠ 地址不在这里，在 lib/external.ts 的 WIKI 表：翻译一门语言不该有机会改掉
  // 一个地址，而每条地址都带章节锚点。
  'wiki.open': '打开项目文档（英文）',
  'wiki.modes': '运行模式详解（英文）',
  'wiki.transport': '连通方式与降级代价（英文）',
  'wiki.quality': '音质阶梯与 AUTO（英文）',
  'wiki.latency': '延迟的构成与测量（英文）',
  'wiki.volume': '两端音量如何取舍（英文）',
  'wiki.permissions': '各项系统权限的用途（英文）',
  'wiki.discovery': '发现、配对与端口（英文）',
  'wiki.web': '网页访问及其边界（英文）',
  'wiki.webLocalOnly': '「仅允许本机」为何锁定（英文）',
  'wiki.devices': '虚拟设备的行为（英文）',
  'wiki.deviceNaming': '虚拟设备的命名（英文）',
  'wiki.bridge': '虚拟声卡桥接（英文）',
  'wiki.capture': '系统音频捕获后端（英文）',
  'wiki.startup': '开机自启的实现（英文）',
  'wiki.shortcuts': '快捷键（英文）',
  'wiki.paths': '配置目录（英文）',
  'wiki.tunnel': '隧道地址（英文）',
  'wiki.unpair': '解除配对会发生什么（英文）',
  'wiki.degraded': '降级链路的代价（英文）',

  // ---------------------------------------------------------------- 外链 / 时间
  'vendor.blackhole': 'BlackHole（macOS）',
  'vendor.vbcable': 'VB-Cable（Windows）',
  'link.copied': '已复制链接，请在浏览器中打开',
  'link.openManually': '请在浏览器中打开：{url}',

  'time.uptime': '{hh}:{mm}:{ss}',
  'time.uptimeDays': '{d} 天 {hh}:{mm}:{ss}',
  'mode.title': '运行模式',
  'pair.left.tip': '请对方在其配对界面输入上述 PIN，以建立双向信任。',
  'pair.right.title': '发起配对',
  'pair.right.note': '配对成功后双向信任立即生效；模式 B 下对端将同时以一对音频设备出现在系统中。',
  'settings.mode.rowTitle': '当前模式',
  'settings.mode.goto': '前往主面板切换',
  'settings.mode.downgraded': '当前选定为「{mode}」，但该模式不可用，已临时按模式 A 运行。{hint}',
  'settings.identity.title': '本机身份',
  'settings.web.title': '网页访问',
  'settings.modeAVolume.title': '模式 A · 音量',
  'settings.modeAVolume.noteInForce': '当前生效（本机正运行于模式 A）。',
  'settings.modeAVolume.noteIdle': '当前不生效：这两项仅属于模式 A，设置会保留。',
  'settings.devices.title': '虚拟设备',
  'settings.bridge.title': '虚拟声卡桥接',
  'settings.startup.title': '启动',
  'settings.startup.autostartDescMac': '在「登录项」中注册 AudioHub：每次登录时自动启动，已有的授权继续有效。关闭即删除，不留残余。',
  'settings.startup.autostartDescWin': '注册一个登录时触发的计划任务（AudioHubDaemon），每次登录时自动启动 AudioHub。关闭即删除，不留残余。',
  'settings.shortcuts.title': '快捷键',
  'shortcuts.sheet.customize': '可在「设置 › 快捷键」中修改。',
  'settings.paths.title': '路径',
} as const;
