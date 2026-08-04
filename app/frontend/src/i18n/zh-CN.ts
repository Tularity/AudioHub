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
  'common.gotIt': '知道了',
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
  'chrome.dragFailed': '窗口拖拽不可用：{message}。请重启 AudioHub；若持续如此，说明界面与应用外壳版本不一致。',

  'badge.online': '在线',
  'badge.starting': '启动中',
  'badge.connecting': '连接中',
  'badge.offline': '离线',

  // foot.* 七条已删（规格 §2.4）：左下角那条注脚说的四种状态与上面四条 badge.* 逐一
  // 重合，端口在设置页「网络 › IPC 端口」，「你正在用网页端查看」由 settings.web.browserOnly
  // 常驻说明。

  // ---------------------------------------------------------------- 覆盖层
  'overlay.starting.title': '正在启动 AudioHub 服务…',
  'overlay.starting.desc': '首次启动需要几秒，完成后会自动进入主面板。',
  'overlay.connecting.title': '正在连接 AudioHub 服务…',
  'overlay.connecting.desc': '正在连接本机端口 {port} …',
  'overlay.connecting.descNoPort': '正在获取本机服务连接信息…',
  'overlay.version.title': 'AudioHub 服务版本不兼容',
  'overlay.version.desc': '{message}。本界面只能与 IPC 协议 v{version} 的服务通信，请把服务与界面更新到同一次构建。',
  'overlay.version.hint': '提示：确认 audiohub 与本界面来自同一次构建。',
  'overlay.noEndpoint.title': '缺少连接参数',
  'overlay.noEndpoint.desc': '请以 ?port=<端口>&token=<令牌> 打开本页面，或直接访问本机服务自身提供的界面地址。',
  'overlay.noEndpoint.hint': '浏览器模式无法启动服务：请在终端运行 audiohub daemon 后等待自动重连。',
  'overlay.noBinary.title': '找不到 AudioHub 服务程序',
  'overlay.noBinary.desc': '应用内缺少 audiohub 服务程序，无法启动音频服务。请重新安装 AudioHub；若在开发环境运行，可设置环境变量 AUDIOHUB_BIN 指向已编译的 audiohub。',
  'overlay.noBinary.hint': '重装后再点「重试」。',
  'overlay.spawnFailed.title': '无法启动 AudioHub 服务',
  'overlay.spawnFailed.desc': '服务程序找到了，但拉起失败——通常是文件权限或系统隔离属性所致。可尝试重新安装，或在终端手动运行一次 audiohub daemon 查看具体报错。',
  'overlay.portBusy.title': 'AudioHub 服务端口被占用',
  'overlay.portBusy.desc': '所需端口已被其他程序（很可能是仍在运行的旧实例）占用。请先结束它，或在终端执行 audiohub ctl shutdown，然后重试。',
  'overlay.timeout.title': 'AudioHub 服务启动超时',
  'overlay.timeout.desc': '服务进程已拉起，但未在预期时间内就绪。请稍候重试；若持续失败，在终端运行 audiohub daemon 观察启动日志。',
  'overlay.startFailed.title': '无法启动 AudioHub 服务',
  'overlay.startFailed.desc': '启动服务时发生未预期的错误。请重试；若持续失败，在终端运行 audiohub daemon 查看报错。',
  'overlay.internal.title': '无法启动 AudioHub 服务',
  'overlay.internal.desc': '界面与本机服务管理器之间的调用失败。请重试，或重启 AudioHub。',
  'overlay.disconnected.title': 'AudioHub 服务已断开',
  'overlay.disconnected.descTauri': '与本机服务的连接已断开（{reason}），每 5 秒自动重连。',
  'overlay.disconnected.reasonUnknown': '原因未知',
  'overlay.disconnected.descBrowser': '与 daemon 的连接已断开，每 5 秒自动重试。',
  'overlay.detail': '详细信息：{detail}',

  // ---------------------------------------------------------------- 运行模式
  // plan §13：三种模式**互斥**，共享模式与两种使用端模式并列。标题因此不再是
  // 「使用端模式」——那个名字把三选一说成了二选一，而被砍掉的那一档恰恰是默认值。
  'mode.title': '运行模式',
  'mode.sub': '全局设置，三选一：本机要么把自己的音频设备共享出去，要么使用别人的，不能同时做两件事。',
  'mode.share.label': '共享 · 供他人使用',
  'mode.a.label': 'A · 免驱动',
  'mode.b.label': 'B · 虚拟设备',
  // 一级界面只留一句**结果句**：读完这一行就知道「现在选谁、在哪里选」，剩下的由
  // 「了解更多」带到设置页——那里的 settings.mode.rowDesc 本来就是同一段话的完整版，
  // 原先主面板上的 mode.a.desc / mode.b.desc 与它逐句重复，已随本次重整一并删除。
  'mode.share.result': '已配对的主机可以取用本机的麦克风、把声音送到本机的输出设备；本机不使用它们的设备。',
  'mode.a.result': '在下方卡片上选对端；本机与对端会同时发声。',
  'mode.b.result': '在「系统设置 › 声音」或任意应用里选对端的设备即可使用。',
  'mode.learnMore': '了解更多',
  'mode.downgraded': '你选择的是模式 B，但当前不可用，已临时按模式 A 运行。',
  'mode.switched.toShare': '已切换到共享模式：本机改为供其它主机使用，本机发起的会话已全部关闭，全部 AudioHub 虚拟设备已从系统移除。',
  'mode.switched.toB': '已切换到模式 B：已配对主机将作为音频设备出现在系统里。正在使用本机的对端已被断开并收到通知。',
  'mode.switched.toA': '已切换到模式 A：全部 AudioHub 虚拟设备已从系统移除。正在使用本机的对端已被断开并收到通知。',
  // 互斥这件事必须在切换处说清楚，而不只在文档里：用户点下去之前就该知道
  // 「选了这个，另一件事就不做了」。
  'mode.exclusive.share': '共享模式下本机不使用其它主机的设备——下方卡片上的通路开关会隐藏。',
  'mode.exclusive.consumer': '使用端模式下其它主机无法调取本机的麦克风或输出设备。',

  'hal.unknown': '服务未连接，暂时无法判断驱动是否可用。',
  'hal.absent': '未检测到 AudioHub 驱动，模式 B 不可用；安装驱动后重启本应用即可选择。',
  'hal.absent.why': '未检测到 AudioHub 驱动，无法使用模式 B',
  'hal.mismatch': '已安装的 AudioHub 驱动版本与本机服务不匹配{versions}：不会有任何虚拟设备出现在系统里。请重新安装与当前版本配套的驱动——重启应用或等待都不会修好它。',
  'hal.mismatch.versions': '（服务 v{mine} / 驱动 v{theirs}）',
  'hal.detached': '驱动已注册但桥接尚未连上（通常是 coreaudiod 正在重启）：已发布的虚拟设备保留在系统中，此刻不处理声音。请稍候，或重启 AudioHub 服务后重试。',
  'hal.ready': '已连接 AudioHub 驱动，模式 B 可用。',

  'halReason.capacity': '虚拟设备已达上限（16 台），该对端暂无对应设备。解除其它配对后可用。',
  'halReason.noDriver': '本机未安装 AudioHub 驱动，无法为该对端创建虚拟设备。',
  'halReason.removedWhileOffline': '已按「断开后移除虚拟设备」把该对端的设备从系统中移除；对端重新连上后会以相同 UID 恢复。',
  'halReason.modeA': '当前是模式 A：虚拟设备只在模式 B 下存在。',
  'halReason.modeShare': '当前是共享模式：本机供其它主机使用，不使用它们的设备，因此不会有虚拟设备。',
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
  'metric.latency.footnote': '系统链路延迟，不含蓝牙 / HDMI 等外部音频链路的额外缓冲。',
  'metric.latency.lowerBoundWhy': '未含声卡固有缓冲，实际略高于此值。',
  'metric.latency.expand': '查看分段',
  'metric.latency.collapse': '收起分段',
  // 范围标记：读数只覆盖本机这一侧时挂在等级词的位置上。
  // 为什么要单独一条而不是复用「≥」：「≥」说的是「还要再多一点」，而这里缺的是
  // **对方整整一半管线**，量级无上界。只给「≥474 ms」而不说缺了谁，用户会把它
  // 读成端到端总延迟——那正是这次要消灭的误读。文案直说缺的是什么，不说黑话。
  'metric.latency.scopeLocal': '未含对方主机',
  'metric.latency.scopeLocalWhy': '这个数只算了本机这一侧的排队时长。对方主机上的分段还没上报，端到端的真实延迟比它高，高多少现在说不了。',

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
  'metric.latency.netOnlyNote': '不含缓冲与声卡，而那两段占大头——要等真的有音频在流动时才量得到。',
  'metric.latency.netOnlyWhy': '这只是数据包在两台主机之间跑一趟的时间。占大头的缓冲与声卡还没有数——它们要等真的有音频在流动时才量得到。建立通路后，这里会换成端到端的总延迟。',
  'metric.latency.netOnlyRtt': '最近一次往返 {ms} ms（交叉校验用）。',
  'metric.latency.netOnlyMeasuringWhy': '还在攒最小往返时间的样本（约十几秒）。宁可先不给，也不拿一个没滤过的往返值顶上。',

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
  'latency.seg.dominant': '这一段此刻的大头是：{name}',

  // 就地展开的逐级明细：这里才出现内部级名，并各带一句说明。
  //
  // ⚠ 这些说明一律**不写「本机 / 对方」**，主机由每行的 stage-host 标签给。
  // 原文把主机烤进了句子里（「对方声卡把声音交给…」「本机等待送进声卡的音频」），
  // 那是照着 recv 会话写的：延迟的物理定义是「从对方声卡采到、到本机声卡送出」。
  // 可 **send 会话上两边正好互换**——本机才是提供方。于是一条 send 会话会渲染成
  // 「本机」标签配「对方声卡……」的说明，两者当场打架，而排障时指错机器比不说
  // 更糟。主机只在一个地方说，就不会有第二个地方说错。
  'latency.stage.capRing.name': '声卡采集缓冲',
  'latency.stage.capRing.desc': '声卡把声音交给 AudioHub 之前的排队。',
  'latency.stage.capDev.name': '声卡采集延迟',
  // ⚠ 「计入总延迟」这半句是必须的。这两级在 2026-08-04 之前**从未被上报**，
  // 接上之后总延迟的数字会往上跳一截——不写清楚，用户会把「一直存在、只是这次
  // 才算进来的那段」读成一次性能退化。
  'latency.stage.capDev.desc': '声卡从收到声音到交出采样之间的固有延迟，计入总延迟。',
  'latency.stage.srcFifo.name': '发送队列',
  'latency.stage.srcFifo.desc': '采集侧等待打包发出的音频。',
  'latency.stage.halSpk.name': '虚拟扬声器环',
  'latency.stage.halSpk.desc': '应用写进虚拟扬声器、尚未被 AudioHub 取走的音频。',
  'latency.stage.sendPace.name': '打包节拍',
  'latency.stage.sendPace.desc': '发送侧每 10 毫秒打包一次，一个采样平均要等半个节拍。',
  'latency.stage.network.name': '网络单程',
  'latency.stage.network.desc': '数据包在两台主机之间传输的时间。',
  'latency.stage.jitterBuf.name': '抖动缓冲',
  'latency.stage.jitterBuf.desc': '为抵消网络抖动而刻意保留的余量。',
  'latency.stage.postMix.name': '混音对齐缓冲',
  'latency.stage.postMix.desc': '把不等长的解码结果对齐成整帧的小缓冲。',
  'latency.stage.playRing.name': '播放队列',
  'latency.stage.playRing.desc': '等待送进声卡的音频。',
  'latency.stage.bridgeRing.name': '虚拟声卡队列',
  'latency.stage.bridgeRing.desc': '等待送进桥接虚拟声卡的音频。与播放队列并行，不叠加。',
  'latency.stage.halMic.name': '虚拟麦克风环',
  'latency.stage.halMic.desc': '写进虚拟麦克风、尚未被应用取走的音频。与播放队列并行，不叠加。',
  'latency.stage.playDev.name': '声卡播放缓冲',
  // 「常常是最大的一段」不是修辞：30-win 实测 41.9 毫秒（写进系统到真正出声），
  // 其中 30 毫秒是 Windows 共享音频引擎与 KS 传输，换一块声卡也一样。
  'latency.stage.playDev.desc': '音频交给系统到声卡真正发声之间的固有延迟，计入总延迟。Windows 上实测约 42 毫秒，常常是整条链路上最大的一段。',
  'latency.stage.residual.name': '未归属',
  'latency.stage.residual.desc': '实测总延迟减去各分段之和。持续偏大说明链路上还有未被统计的缓冲。',
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
  'latency.stage.dropNone': '这一级不会丢弃音频（有界但从不饱和，或根本没有队列）。',
  'latency.stage.droppedN': '已丢弃 {n} 个样本',
  'latency.stage.droppedNone': '未丢弃',
  'latency.stage.droppedWhy': '本条会话的累计值。数字冻结说明只是曾被灌满一次；持续增长说明产销速率长期失配。',
  // `dropped: null` 与 `0` 是两个结论，界面必须分开讲——混为一谈就等于替驱动
  // 宣布「它一个样本都没丢」，而我们根本数不到那一侧。
  'latency.stage.droppedUnknown': '丢弃数不可见',
  'latency.stage.droppedUnknownWhy': '这一级的丢弃发生在另一侧（音频驱动或对方进程）里，本机数不到。**不等于没丢过。**',
  'latency.stage.fill': '{pct}% 满',
  'latency.stage.fullAt': '已满（{pct}%）',
  'latency.stage.fillWhy': '{n} / {cap} 个样本。到 95% 才算「已满」。',
  'latency.stage.driftUp': '每分钟涨 {ms} ms',
  'latency.stage.driftDown': '每分钟降 {ms} ms',
  'latency.stage.driftFlat': '深度稳定',
  'latency.stage.driftWhy': '最近 30 秒的深度斜率（{sps} 样本/秒）。持续上涨说明这一级迟早会被灌满。',
  'latency.stage.driftUnknown': '趋势未知',
  'latency.stage.driftUnknownWhy': '样本点还不够判趋势（不足 3 点或跨度不到 5 秒）。**不等于不漂移。**',

  'latency.conf.full': '各分段完整',
  // 接线之前这里写的是「缺声卡缓冲」——那时两级设备延迟根本没查。现在查了，
  // 「仍是下限」的成因换成了另外两种，而两种都必须说得出口：
  // ① 系统给的声卡读数已知偏低（蓝牙 / HDMI，或 Windows 上那个靠开流标定、
  //    带 ±8 毫秒开流竞态的值）；② 这条链路的某一端压根没有实体声卡
  //    （虚拟扬声器 / 虚拟麦克风），那一小截还没建模。
  'latency.conf.lowerBound': '下限（声卡延迟未取到精确值）',
  'latency.conf.converging': '时钟对齐中，约 {s} 秒后可用',
  // 说人话版：原文「仅本机分段，对端未上报」是照着字段名写的，用户读不出后果。
  'latency.conf.localOnly': '以上只有本机这一侧的分段。对方主机还没上报它那一半，所以这不是端到端的总延迟。',
  'latency.conf.deviceUnreliable': '输出设备（蓝牙 / HDMI）的延迟系统少报，实际更高',
  // 只在两台声卡都给出平台真值时才显示（confidence = full）。
  'latency.conf.fullWhy': '两端的声卡固有延迟都取到了平台真值，这个数字覆盖从对方采集到本机发声的整条链路。',
  'latency.conf.peerStale': '对方主机的分段是 {s} 秒前的读数',
  'latency.detail.e2e': '实测采样年龄 {ms} ms（与各分段之和的差记入「未归属」）',

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
  'metric.quality.rateWhy': '线上采样率，与你在详情页设的音质档同一个数。可用带宽是它的一半（展开「音质构成」可以看到）。',
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
  'metric.quality.measuringWhy': '还有分量没攒够统计窗口（通常是通路刚建立后的十几秒）。此时把在场分量取最小只是个上界，不是等级，所以先不给。',
  // grade 成立、但仍缺一块板：等级已经触底，缺席改不了结论，两件事都要说。
  'metric.quality.partial': '这一档是在还缺一个分量的情况下定的，补齐后只会更低、不会更高。',

  // 这一格来自对端的测量（SessionStats.peer_quality）。
  //
  // 音质三分量（补偿、削顶、带宽）全是**接收侧**的量，所以一条纯发送的通路本机
  // 恒无读数——「送对方扬声器」的音质格此前**永远**空着，而链路其实好得很。
  // 现在由对端把它那侧测到的回传过来。必须标：数是真的，但量它的人在对面，
  // 不标就等于让本机宣称了一个它没有测点的结论。
  'metric.quality.fromPeer': '对端测得',
  'metric.quality.fromPeerWhy': '这条通路是本机在发送，而音质（补偿、削顶、带宽）只有收端量得到，所以这一格是对端在它那侧测好后回传的。',

  'quality.part.continuity.name': '连续性',
  'quality.part.continuity.desc': '输出中不是由对方原始采样构成的时长占比（补偿帧与静音）。',
  'quality.part.continuity.value': '{pct}% 被补偿',
  'quality.part.level.name': '电平',
  'quality.part.level.desc': '波形被削顶压缩的采样占比与压缩深度。',
  'quality.part.level.value': '{pct}% 削顶，超出 {db} dB',
  'quality.part.bandwidth.name': '带宽',
  // desc 必须说清它是**由采样率推出来的标称上限**，不是对实际频谱内容的测量。
  // 旧文案「还保留了多少高频成分」把它说成了一个实测量——而树里没有任何频谱
  // 分析，这个数恒等于采样率的一半。把推导值说成测量值，比单位混淆更难查。
  'quality.part.bandwidth.desc': '能传过去的最高音频频率，等于线上采样率的一半（奈奎斯特上限）。它由采样率推出，不是对实际频谱内容的测量。网络变差时采样率会自动降档，这个数跟着降。',
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
  'mix.health.duplicate': '检测到两路内容几乎相同的音频叠加（相关度 {r}），这会使音量翻倍并削顶。',
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
  'peers.form.note': '通过 peers.connect 主动连接已配对对端：daemon 会按指纹校验对端身份，跨网段亦可用。',
  'peers.form.needFingerprint': '请填写对端指纹（可输前缀）',
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
  'peers.card.dirMultiWhy':
    '这个方向同时有多条通路在跑。一级界面显示其中最慢的一条——多路并行时体感由最差的一路决定。逐条明细在详情页。',
  // 延迟档的**作用对象**按方向不对称，这两句是它的界面化。
  //
  // daemon 的 `servo_pass` 只遍历本机的接收流：发送方向那半条链路的抖动缓冲
  // 在对端，由对端自己的延迟档管，本机没有执行器。不说的话，一台只发不收的
  // 使用端拖了延迟滑条会看到「两栏里只有一栏在动」，唯一自然的结论是
  // 「设置只生效了一半」——而系统是对的。设置页早已为此开了一条文案
  // （settings.transport.noRecvStream），这两句是把同一条教训搬到卡片上。
  'peers.card.dirGovLocal': '本机在收。延迟档作用在这个方向：它调的是本机的抖动缓冲。',
  // ⚠ 语料里不许出现 Markdown 记号：这两句会直接进 `title` 与 `.metric-foot` 的
  // 纯文本节点，`**…**` 会原样显示成四个星号。第一版写了，实测截图里就是那样。
  'peers.card.dirGovPeer': '本机在发。这半条链路的缓冲在对端，由对端自己的延迟档决定，本机的滑条对它没有作用对象。',

  // 模式 B 下，虚拟麦克风已经真的出现在系统设备列表里、但还没有任何应用打开它。
  //
  // 「接收」那一行此前只显示「空闲」，与「对端离线」「驱动没起来」长得一模一样——
  // 用户明明知道麦克风是通的，界面却什么都不肯说。这一行说的是**状态**，不是数据：
  // 没有音频在流动时不存在码率、不存在电平，任何数字都会是编的。
  // 只有 hal_device.observed 为真（设备确实在系统里）且对端在线时才敢这么说。
  'peers.card.micReadyShort': '就绪',
  'peers.card.micReady': '通路就绪 · 暂无应用在录音',
  'peers.card.micReadyWhy': '虚拟麦克风已经出现在系统设备列表里，对端也在线。任意应用（会议、录音、浏览器）选中它的那一刻，音频就开始流动，这里随即显示实时码率。',

  // plan §13 推论 1：对端处于使用端模式时无法被本机调取。三条分开写，因为
  // 「它在模式 A」和「它在模式 B」对用户的意义不同（后者说明对面正把本机之外的
  // 某台主机当设备用），而「认不出的模式」只能含糊其辞、绝不能冒充前两者。
  'peers.unusable.modeA': '该主机当前不可被使用：它处于模式 A（免驱动使用端），正在使用其它主机的音频设备。请在那台主机上切换到共享模式。',
  'peers.unusable.modeB': '该主机当前不可被使用：它处于模式 B（虚拟设备使用端），正在使用其它主机的音频设备。请在那台主机上切换到共享模式。',
  'peers.unusable.unknownMode': '该主机当前不可被使用：它上报了本版本无法识别的运行模式。',
  'peers.unusable.badge': '不可被使用',

  // 原来每张卡片各印一遍（同一句话在 N 张卡上重复 N 次）：现在只在卡片列表底部渲染一次。
  'peers.devices.footOnce': '在「系统设置 › 声音」或任意应用的音频设备菜单里选中它即可使用；音量在系统里调节，会同步到这台主机的真实设备。',
  'peers.devices.offline': '⚠ 对端离线：设备仍在系统中可选，但不处理任何声音。',
  'peers.devices.settling': '设备已下发，正在等待系统的设备列表刷新（最多 1 秒）。',

  'peers.empty.title': '先在两台设备上完成配对',
  'peers.empty.desc': 'AudioHub 通过配对建立两台设备之间的互信，之后才能共享麦克风与扬声器。',
  'peers.empty.step1': '在两台设备上都打开 AudioHub',
  'peers.empty.step2': '本机打开「配对向导」生成 6 位 PIN',
  'peers.empty.step3': '另一台设备发现本机后输入同一个 PIN',
  'peers.empty.modeB': '配对完成后，对方主机会立刻作为一对音频设备出现在「系统设置 › 声音」里；把输出切到它，或在任意应用里选它，就是在使用那台主机。',
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
  'volume.reading': '正在读取对端音量…',
  'volume.noSync': '该会话未启用音量同步',
  'volume.notAdjustable.tag': '不可调',

  // ---------------------------------------------------------------- 桥接
  'bridge.label': '桥接到虚拟声卡',
  'bridge.none': '不桥接',
  'bridge.undetected': '未检测到虚拟声卡',
  'bridge.notReported': '服务未上报',
  'bridge.staleOption': '{name}（未检测到）',
  'bridge.stale.reselect': '「{name}」当前未检测到：开启「取对方麦克风」时不会桥接。请重新选择一张可用的声卡。',
  'bridge.stale.reinstall': '「{name}」当前未检测到：开启「取对方麦克风」时不会桥接。装回该声卡后重开本应用即可恢复。',
  'bridge.noField': '当前服务未上报虚拟声卡信息，无法桥接。',
  'bridge.presentUnusable': '检测到 {names}，但它不在系统输出设备列表里，无法写入。',
  'bridge.nothing': '未检测到虚拟声卡。AudioHub 不会替你安装任何驱动——如需此功能，请自行安装下列任一款后重开本应用。',
  'bridge.selected': '对端麦克风将写入「{name}」的播放端；任意应用选择它的输入端即可当作对端麦克风使用。',
  'bridge.pick': '选择一张虚拟声卡后，对端麦克风会写入它的播放端，供其他应用当作输入设备使用。',

  // ---------------------------------------------------------------- 共享来源（模式 A 的 spk 方向）
  // plan §7.1：模式 A 的「送对方扬声器」= 捕获本机系统音频送对方默认输出播放。
  // 麦克风是可选来源，不是默认值。文案里绝不出现「把系统输出切到某某设备」（plan §6 红线）。
  'share.label': '共享来源',
  'share.source.sysaudio': '系统音频',
  'share.source.mic': '麦克风',
  'share.mic.note': '送出的是本机默认麦克风。若想让对端听到本机正在播放的声音，请选「系统音频」。',
  'share.sys.none': '本机没有可用的系统音频捕获后端，只能共享麦克风。AudioHub 不会要求你改动系统的输出设备。',
  'share.backend.label': '捕获后端',
  'share.backend.autoOption': '自动',
  'share.backend.auto': '由服务按优先级自动挑选可用的捕获后端。捕获是旁路读取，本机的输出设备与音量保持原样。',
  'share.backend.selected': '已指定「{name}」：{note}',
  'share.backend.unknown': '当前服务未上报可用后端清单，是否支持要到真正开启时才知道；开不起来会明确说明原因，不会静默失败。',
  'share.backend.stale': '当前服务不认识后端「{id}」：开启时会直接报错。请改回「自动」或另选一个。',
  'share.backend.staleOption': '{id}（当前服务未提供）',
  'share.backend.optionUnavailable': '{name}（本机不可用）',
  'share.perm.hint': '共享系统音频需要「系统音频录制」授权：macOS 无法预先查询，首次开启时系统会询问；若此前被拒绝过，需到系统设置里手动打开。',
  'share.perm.goto': '前往授权',
  'share.fault': '⚠ 上次开启失败：{reason}',
  'share.fault.unknown': '服务未说明原因',

  // 后端目录。id 必须与 core/audiohub-core/src/sysaudio.rs 的 BACKEND_* 常量一致。
  // 服务上报了自己的 note 时优先用它（它带本机实际版本号 / 上次被拒绝的事实）。
  'sysaudio.backend.winProcExclude.label': 'Windows 进程环回（排除自身）',
  'sysaudio.backend.winProcExclude.note': '天然排除 AudioHub 自己播放的声音，不会把对端音频再送回去。需要 Windows 10 2004 及以上。',
  'sysaudio.backend.winDeviceLoopback.label': 'Windows 设备环回',
  'sysaudio.backend.winDeviceLoopback.note': '兜底方案，兼容更老的系统；它会一并录到 AudioHub 自己播放的声音，与对端互送时可能形成回授。',
  'sysaudio.backend.macCatap.label': 'macOS 音频进程 Tap',
  'sysaudio.backend.macCatap.note': '首选：纯音频接口，权限归「系统音频录制」而非屏幕录制，且排除本 App 自身的播放。需要 macOS 14.2 及以上。',
  'sysaudio.backend.macSck.label': 'macOS 屏幕捕获音频流',
  'sysaudio.backend.macSck.note': '备选路线，权限归「屏幕录制」类别。',

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
  'detail.notFound.desc': '对端可能已被移除，或 daemon 尚未返回列表。',
  'detail.reconnecting': '重连中…',
  'detail.identity': '身份',
  'detail.defaultPort': '默认端口',
  'detail.pairedAt': '配对时间',
  'detail.publicKey': '公钥',
  'detail.fpCopied': '已复制完整指纹',
  // 失败提示改走 common.copyFailed（设置页「本机身份」也复制指纹，两处必须同一条）。

  'detail.alias.title': '别名',
  'detail.alias.field': '显示名称',
  'detail.alias.placeholder': '对端主机名',
  'detail.alias.renamed': '已改名为「{name}」',
  'detail.alias.restored': '已恢复为对端主机名',
  'detail.alias.noteSet': '虚拟设备名称使用别名「{alias}」；清除后恢复为对端上报的主机名「{name}」。',
  'detail.alias.noteEmpty': '设置别名会改写这台对端在系统设备列表中的名字。改名是同 UID 就地进行的：设备身份不变，任何应用已记住的选择都不受影响。',

  'detail.devices.title': '虚拟设备',
  'detail.devices.modeA': '当前是模式 A：虚拟设备只在模式 B 下存在。在主面板顶部切换模式后，这台对端会作为一对设备出现在系统音频设备列表里。',
  'detail.devices.published': '两台设备已在系统音频设备列表中，可被任意应用直接选用。',
  'detail.devices.offline': '⚠ 对端离线：设备仍在系统中可选，但不处理任何声音。',
  'detail.devices.stateListed': '驱动状态「{state}」，系统设备列表已列出它们。',
  'detail.devices.stateUnlisted': '驱动状态「{state}」，系统设备列表尚未列出它们。',

  'detail.addrs.title': '地址历史',
  'detail.addrs.empty': '暂无地址记录',
  'detail.addrs.seenAt': ' 最近见于 {time}',
  'detail.addrs.fromDaemon': ' daemon 记录',
  'detail.addrs.note': '来自 daemon 记录的最近地址与本次 UI 会话内观察到的变化。',

  'detail.sessions.title': '活跃会话',
  'detail.sessions.empty': '与该对端暂无活跃会话。',
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
  'detail.danger.desc': '解除配对会撤销双向信任、立即关闭全部会话，并无条件从系统移除这台对端的虚拟设备。对端也会收到通知并移除本机的设备——它的系统列表里不会留下一对永远离线的幽灵设备。',
  'detail.danger.foot': '若之后想再用这台主机，需要重新走一次配对流程。',
  'detail.unpair': '解除配对',
  'detail.unpair.confirmTitle': '解除配对？',
  'detail.unpair.confirmLead': '将解除与「{name}」的配对，并撤销双向信任。',
  'detail.unpair.confirmDevices': '解除配对会立即从系统移除「{out}」与「{in}」。若其中之一正是当前默认设备，系统会自动切换到其它设备。',
  'detail.unpair.confirmNoDevices': '该对端当前没有虚拟设备，只会移除信任与已建立的会话。',
  'detail.unpair.done': '已解除配对',

  // ---------------------------------------------------------------- 配对
  'pair.left.title': '我要被发现',
  'pair.left.desc': '开启后，本机将在局域网内可被发现（pairing.enable），并生成一次性 PIN 供对方输入。',
  'pair.left.enable': '开启配对模式',
  'pair.left.disable': '停止配对',
  'pair.left.tip': '请对方在其配对界面输入以上 PIN 完成双向信任。',
  'pair.left.expired': '配对模式已到期',
  'pair.right.title': '我要连别人',
  'pair.right.scan': '开始扫描',
  'pair.right.stopScan': '停止扫描',
  'pair.right.empty': '尚未发现主机。点击「开始扫描」在局域网内查找（discover.run）。',
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
  'pair.right.needAddr': '请填写对方地址（IP 或 IP:端口）',
  'pair.right.needPin': '请填写对方界面上显示的 PIN',
  'pair.right.done': '已与「{name}」完成配对',
  'pair.right.failed': '配对失败：{message}。确认对方已开启配对模式、PIN 未过期、地址可达；也可用 CLI 复现：audiohub pair --to {addr} --pin {pin}',
  'pair.right.note': '经 peers.pair 由本机服务发起：配对成功后双向信任立即生效，模式 B 下对方主机会同时作为一对音频设备出现在「系统设置 › 声音」里。',
  'pair.step.connect': '建立连接',
  'pair.step.verifyPin': '校验 PIN',
  'pair.step.exchangeKeys': '交换密钥',
  'pair.step.done': '完成配对',

  // ---------------------------------------------------------------- 设置
  'settings.mode.title': '运行模式',
  'settings.mode.rowTitle': '当前模式',
  'settings.mode.rowDesc': '三种模式互斥，同一时刻只能是其中一种。共享：本机把自己的默认麦克风与默认输出提供给已配对主机使用，可同时服务多台；本机自己不使用任何对端的设备。A：不装驱动，默认捕获本机系统音频送到对端播放——本机与对端同时发声，捕获是旁路读取，本机的输出设备不需要做任何改动；每张对端卡片上的「共享来源」可改送本机麦克风，也可指定捕获后端。取用对端麦克风需借助已安装的第三方虚拟声卡（见下方「虚拟声卡桥接」）。B：每台已配对主机作为一对设备出现在系统音频设备列表中，任意应用直接选用，调节该设备音量即调节对端真实设备。为什么必须互斥：一台既共享又使用的主机，共享出去的「默认麦克风」可能正是另一台主机的虚拟麦克风，于是它在毫不知情的情况下成了中继；若那台主机反过来又在用它，就构成闭环，延迟会一直涨到某一级缓冲塞满为止。模式是全局设置，由本机服务持有；切换入口在主面板顶部，切换不需要确认。',
  'settings.mode.goto': '前往主面板切换',
  'settings.mode.downgraded': '你选择的是「{mode}」，但当前不可用，已临时按模式 A 运行。{hint}',

  // 本机指纹在右上徽标里改成了**悬停才显示**（plan §7.6 补充裁定）。悬停在触摸屏上
  // 不存在、在截图排障时也拿不到，所以必须有一个常驻落点——就是这一块。
  'settings.identity.title': '本机身份',
  'settings.identity.fingerprint': '本机指纹',
  'settings.identity.name': '本机名称',
  'settings.identity.copied': '已复制本机指纹',
  'settings.identity.note': '对方在配对时核对的就是这串指纹。',

  'settings.net.title': '网络',
  'settings.net.controlPort': '控制端口',
  'settings.net.controlPortDesc': 'daemon 对外的 TCP 控制端口（TLS + 指纹校验）。M4a 为只读展示，暂不支持修改。',
  'settings.net.controlPortBadge': '只读 · M4a',
  'settings.net.ipcPort': 'IPC 端口',
  'settings.net.ipcPortDesc': '本机回环 WebSocket 端口，随 daemon 启动随机分配，写入 ipc.json。',

  // 网页访问（plan §7.5）。文案有两条硬要求：一是必须说清「仅允许本机」关掉之后
  // **实际会发生什么**（无鉴权 + 令牌明文），二是不得把它写成一句泛泛的「请注意
  // 安全」——那种话没人会当真。
  'settings.web.title': '网页访问',
  'settings.web.desc': '由本应用在一个独立端口上提供这套界面，用浏览器打开即可操作——手机、平板、另一台电脑都行，不必安装任何东西。它与对外控制端口无关，也不影响音频。',
  'settings.web.enabledTitle': '启用网页访问',
  'settings.web.enabledDesc': '默认关闭：没开启时这个端口根本不会被监听。开启后本应用开始服这套界面，页面自己向本机服务取连接参数（同源 GET /ipc-endpoint），网址里不带任何令牌。',
  'settings.web.portTitle': '端口',
  'settings.web.portDesc': '本应用自己的网页端口（默认 47800），与 daemon 的对外控制端口、IPC 端口都不是一回事。范围 1024–65535，改完按回车或点「应用」立即重新监听。',
  'settings.web.portApply': '应用',
  'settings.web.portInvalid': '端口需在 1024–65535 之间。',
  'settings.web.localOnlyTitle': '仅允许本机',
  'settings.web.localOnlyDesc': '开启时只监听 127.0.0.1——不是「监听所有网卡再按来源过滤」，而是根本不在对外地址上监听，局域网里连不上这个端口。',
  'settings.web.localOnlyBadge': '尚不可用',
  // 「为什么不可用」必须说到底：只写「暂不支持」，下一个读到的人（包括半年后的自己）
  // 只会以为是没做完的开关，而不是一个有确定前提条件的设计裁定。
  'settings.web.localOnlyLocked': '这个开关暂时不能关：关掉它并不会换来一个能用的远程界面。实测（本机 ↔ 另一台主机）对方能收到页面，也能从 /ipc-endpoint 拿到本机服务的 IPC 令牌，但连不上服务——本机服务的 IPC 只监听回环，远端够不到；即使在本机改用局域网地址打开，浏览器也会按「私有网络访问」规则拦掉从局域网页面指向回环的连接。也就是说，关掉它的净效果只剩「把令牌发出去」。要真正可用，需要本应用再提供一条把 IPC 转发出去的通路；而那条通路一旦存在，「暂不做鉴权」就不能同时成立——远程可操作与无鉴权只能二选一。在此之前，配置文件里即使写成 false，也一律按仅本机处理（启动日志会记一行）。',
  'settings.web.urlLabel': '访问地址',
  'settings.web.urlLocal': '本机：{url}',
  'settings.web.urlLan': '局域网：{url}',
  'settings.web.urlLanUnknown': '局域网：用本机在该网段的 IP 加同一端口访问（未能自动探测到出口地址）。',
  'settings.web.off': '未启用。开启后这里会显示可直接打开的网址。',
  'settings.web.starting': '正在读取当前状态…',
  'settings.web.error': '没能开始监听：{message}',
  'settings.web.errorHint': '设置已保存，但端口没能绑定——最常见的原因是这个端口被别的程序占着。换一个端口再试。',
  'settings.web.warnTitle': '这个开关关掉之后，本机服务的令牌会明文发给任何来访者',
  'settings.web.warnBody': '「仅允许本机」已关闭：同一局域网内任何人只要知道这台机器的 IP 和端口，就能打开这套界面，而且页面取连接参数的那个接口（/ipc-endpoint）会把本机服务的 IPC 令牌**明文**交给他——本应用目前没有任何鉴权。这个令牌等同于本机音频服务的完全控制凭据：谁拿着它又能够到本机回环（例如这台机器上的另一个登录会话、或本机上任何一个能发请求的程序），谁就能配对、开关音频通路、解除配对。只在你信得过的网络里临时开启，用完请开回来。',
  'settings.web.lanIpcNote': '实测：用局域网地址打开时页面能显示，但连不上本机服务——服务的 IPC 只监听回环，别的机器根本够不到；即使在本机用局域网地址打开，浏览器也会按「私有网络访问」规则拦掉从局域网页面指向回环的连接。所以这个开关目前只是把页面和连接参数放了出去，界面在远端还不能真正操作；要让它可用，需要再加一条把 IPC 转发出去的通路（尚未实现）。',
  'settings.web.sourceDisk': '页面文件来自磁盘目录 {root}。',
  'settings.web.sourceEmbedded': '页面文件来自应用内嵌资源，与窗口里显示的是同一份。',
  'settings.web.quitNote': '网页入口由本应用提供：从托盘选「退出界面（音频服务继续运行）」后它随之消失，音频不受影响；重新打开本应用即可恢复。',
  'settings.web.browserOnly': '你正在用网页端查看本页面。这三个选项只能在应用窗口里修改——否则一次误触就能把你自己正在用的这个入口关掉。此处显示的是按当前访问地址推断出的状态。',

  // ---- plan §15：对端详情页的传输档位 ----
  // 卡片上那一行「这个数是目标不是能力」。措辞必须让用户一眼分出两件事：
  // 「我设的」与「对方要求的」。共享模式的机器只会看到后者。
  'peers.card.targetMine': '目标 {ms} ms（你设定的，服务会主动填到这个值）',
  'peers.card.targetByPeer': '目标 {ms} ms（由使用方要求）',

  'detail.transport.title': '传输档位',
  // §14 裁定 4：**常驻**，不是 tooltip。用户看到 300 ms 时必须能分辨
  // 「这是我自己设的目标」而非「系统只能做到这样」——当前界面对此一个字都没说，
  // 正是本次误判的直接成因。
  'detail.transport.note': '这里设的是**目标值**，不是实测值。延迟档是端到端总延迟的目标：设成 300 ms 时服务会主动把缓冲填到 300 ms，而不是「这条链路只能做到 300 ms」。每格下方那一行才是实测读数。',
  // 交叉的那半边要说出来，否则「我改了发送音质，为什么没反应」在界面上无解。
  // 措辞按用户视角，不提「推给对端」——那是实现细节（plan §15 裁定 3）。
  'detail.transport.where': '两个方向由本机单方决定，对端照办。延迟由**接收**的那一端执行、音质由**发送**的那一端执行，所以同一行里的两个档位分别落在两台机器上——这一点不影响你怎么设，只影响读数从哪一侧先动。',
  'detail.transport.colLatency': '延迟（目标）',
  'detail.transport.colQuality': '音质（目标）',
  'detail.transport.latencyIn': '接收方向的延迟目标',
  'detail.transport.latencyOut': '发送方向的延迟目标',
  'detail.transport.qualityIn': '接收方向的音质目标',
  'detail.transport.qualityOut': '发送方向的音质目标',
  // 共享模式：显示对端推来的值 + 出处。**不隐藏、不置灰成空壳**——
  // 本机真的有执行器在跑，只是被远程指挥；隐藏会让共享侧永远看不到自己
  // 机器上正在被执行什么，而本次事故里缺的正是这个视图。
  'detail.transport.sharedBy': '本机处于共享模式：收发档位由使用方（{name}）决定，这里只显示它此刻要求的值。',
  // 「未设定」≠ 0，也 ≠ auto。对端没表态时按自动跑，但那与「对端明确选了
  // AUTO」是两件事，混成一个值会让共享侧读出一个对方从未做过的决定。
  'detail.transport.unset': '未设定 · 按自动运行',
  'detail.transport.noStream': '这个方向当前没有音频流，暂无实测读数。',
  'detail.transport.measuring': '正在测量，暂无读数。',
  'detail.transport.liveMs': '实测 {n} ms',
  'detail.transport.liveAtFloor': '实测 {n} ms · 已贴住物理下限',
  'detail.transport.liveAtCeiling': '实测 {n} ms · 已贴住物理上限',
  // 这一行紧贴在音质滑条**正下方**，而滑条档位标签写着「PCM 48 kHz」。
  // 它此前显示奈奎斯特带宽（24），于是相邻两行是「PCM 48 kHz」与「线上 24 kHz」
  // ——全应用里单位混淆最刺眼的一处。现在两行同量纲，且措辞点名是采样率。
  'detail.transport.liveKhz': '线上采样率 {n} kHz',

  'settings.transport.title': '传输',
  'settings.transport.auto': 'AUTO',

  'settings.transport.latency': '延迟档',
  // 「这是总延迟的目标，不是某一级缓冲的大小」必须写死在文案里：把它读成缓冲大小的
  // 人，会以为调到 200 ms 就是「多缓冲 200 ms」，于是永远不明白为什么读数不听话。
  'settings.transport.latencyDesc': '这里设的是端到端总延迟的目标值——从对方采集到本机放出声音的全程，与对方之间的网络延迟也算在内，不是某一级缓冲的大小。服务会在链路允许的范围内朝这个目标调节缓冲深度：目标低于物理下限就贴着下限跑，高于上限就贴着上限。「尽可能低」= 不设目标、一路压到最低；AUTO = 按实测网络质量自适应。',
  'settings.transport.latencyLowest': '尽可能低',
  'settings.transport.ms': '{n} ms',

  'settings.transport.quality': '质量档',
  // ⚠ 这句话是「采样率 / 带宽」这一对的**权威解释**，措辞不能再把两者说成一回事。
  // 旧版写「可调的是采样率，也就是能传过去的音频带宽（上限为采样率的一半）」——
  // 一句里先说「就是」再说「一半」，正是界面上那次 48/24 误读的文字版。
  'settings.transport.qualityDesc': '这里调的是**线上采样率**：16 kHz 够清晰说话，48 kHz 是全带宽。能传过去的最高音频频率是采样率的**一半**（48 kHz 采样率 ⇒ 24 kHz 带宽），所以卡片上的「音质」显示采样率、展开明细里才是带宽，两个数差一倍是正常的。三档 Opus 尚未实现——照样画在滑条上但选不中，好让「本机为什么没有它」看得见。AUTO 按丢包与抖动在质量阶梯（rung）上自动升降。',
  'settings.transport.q.auto': 'AUTO',
  // 「64k」→「64 kbps」：**同一条滑条上 Opus 档是码率、PCM 档是采样率**，两种量纲
  // 并排。这是编解码器的惯例（Opus 按码率参数化、PCM 按采样率），改不了，但
  // 「64k」与「48 kHz」摆在一起时，那个光秃秃的 k 邀请用户去比 64 和 48。
  // 写全单位就比不起来了——一处零成本的消歧，与本轮 48/24 那处同源。
  'settings.transport.q.opus64': 'Opus 64 kbps',
  'settings.transport.q.opus128': 'Opus 128 kbps',
  'settings.transport.q.opus256': 'Opus 256 kbps',
  'settings.transport.q.pcm16k': 'PCM 16 kHz',
  'settings.transport.q.pcm24k': 'PCM 24 kHz',
  'settings.transport.q.pcm32k': 'PCM 32 kHz',
  'settings.transport.q.pcm48k': 'PCM 48 kHz',
  'settings.transport.qBlocked': '本版本暂不支持这一档。',
  'settings.transport.qBlockedOpus': '本次构建未链接 libopus，这一档不可用。',

  // plan §15：全局滑条下线，这个位置换成只读总览 + 一次性迁移说明。
  // **位置不许留空**——区块凭空消失 = 用户找不到、也没被告知搬去哪了，
  // 正是 §15 那个病根（「界面对此一个字都没说」）换个位置复发。
  'settings.transport.migrated': '延迟与音质已改为**按对端**设置：收、发两个方向各有自己的一档延迟与一档音质。原来的全局档位不再生效，请到各对端的详情页重新设置。',
  'settings.transport.noPeers': '还没有配对的对端。配对之后，每台对端的四个传输档位会列在这里。',
  'settings.transport.colPeer': '对端',
  'settings.transport.colDir': '方向',
  'settings.transport.colLatency': '延迟（目标）',
  'settings.transport.colQuality': '音质（目标）',
  'settings.transport.noteLive': '这张表是**只读总览**，也是唯一能一眼看全所有对端档位的地方——「哪一台还停在 AUTO」在别处看不见。点对端名字进详情页去改；改动松手即生效，不需要重启服务，也不需要与对端重新连接。',

  'settings.devices.title': '虚拟设备',
  'settings.devices.removeTitle': '断开后移除虚拟设备',
  'settings.devices.removeDesc': '关闭时：断开仅显示离线，虚拟设备保留在系统设备列表；开启时：断开即移除，重连后以相同 UID 恢复。解除配对总是无条件移除。',
  'settings.devices.markOfflineTitle': '离线时标注设备名',
  'settings.devices.markOfflineDesc': '开启时，对端断开期间设备名后追加「（离线）」——同一 UID 就地改名，不影响任何应用已记住的设备选择。关闭则名字恒定，代价是「没声音」在系统里无从分辨。',
  'settings.devices.inventory': '设备清单',
  'settings.devices.count': '已用 {used} / {cap}',
  'settings.devices.countNa': '不可用',
  'settings.devices.tagPublished': '已发布',
  'settings.devices.tagMissing': '未出现在系统中',
  'settings.devices.noteHas': '「已发布」= 驱动确认绑定且系统的设备列表里确实能查到这两个 UID。',
  'settings.devices.noteNoDriver': '本机未安装 AudioHub 驱动（或服务未加载桥接），没有虚拟设备。',
  'settings.devices.noteModeB': '当前没有任何虚拟设备：配对一台对端后，它会立刻出现在系统音频设备列表里。',
  'settings.devices.noteModeA': '当前是模式 A：虚拟设备只在模式 B 下存在。',

  'settings.bridge.title': '虚拟声卡桥接',
  'settings.bridge.desc': '取用对端麦克风时，可把音频写入本机已安装的第三方虚拟声卡的播放端；任意应用选择该声卡的输入端，就等于选中了对端的麦克风。',
  'settings.bridge.foot': 'AudioHub 不会替你安装任何驱动：这些虚拟声卡由第三方签名与维护，安装后重新打开本应用即可被检测到。选择哪一张卡在主面板的对端卡片上单独设置。',
  'settings.bridge.detected': '已检测到',
  'settings.bridge.notInOutputs': '不在输出列表',
  'settings.bridge.notDetected': '未检测到',
  'settings.bridge.noneReported': '当前服务未上报虚拟声卡信息（daemon.status 无 virtual_cards）。',
  'settings.bridge.noneOffline': '服务未连接，暂无检测结果。',
  'settings.bridge.noneFound': '未检测到任何虚拟声卡。',

  'settings.paths.title': '路径',
  'settings.paths.configDir': '配置目录',
  'settings.paths.configDirDesc': 'daemon 身份、配对表与 ipc.json 所在目录；可用环境变量 AUDIOHUB_CONFIG_DIR 覆盖。',

  'settings.perm.title': '系统权限',
  'settings.perm.desc': 'macOS 的规则是：一项权限被拒绝后，应用无法再次弹窗询问，只能到系统设置里手动打开。这里显示的是本机服务实时探测到的状态，不是记住的旧结果。',
  'settings.perm.recheck': '重新检查',
  'settings.perm.unsupported': '当前服务不提供权限查询接口（daemon 版本较旧），无法在此显示或申请权限。',
  'settings.perm.error': '权限探测失败：{message}',
  'settings.perm.probing': '正在探测系统权限…',
  'settings.perm.offline': '服务未连接，暂无法探测权限状态。',

  // ---------------------------------------------------------------- 统计
  'stats.uptime': 'daemon 运行时长',
  'stats.rtt': 'IPC 往返延迟',
  'stats.sessionCount': '活跃会话',
  'stats.empty.title': '暂无活跃会话',
  'stats.empty.hintModeB': '在「系统设置 › 声音」或任意应用里选择某台对端的 AudioHub 设备后，这里会出现对应的实时指标。',
  'stats.empty.hintModeA': '在主面板打开对端卡片上的通路开关，或用 CLI 发起会话后，这里会实时出现指标。',
  'stats.session': '会话 #{id}',
  // 诊断页从「会话导向」改为「先按对端聚合、再按会话展开」（spec §2.5）。
  'stats.groupBy.peer': '按对端',
  'stats.groupBy.session': '按会话',
  'stats.group.sessions': '{n} 条通路',
  'stats.waterfall.title': '延迟构成',
  'stats.waterfall.empty': '暂无活跃通路',
  'stats.metric.loss': '丢包率',
  'stats.metric.jitter': '抖动',
  'stats.metric.bitrate': '码率',
  'stats.metric.rung': '质量阶梯',
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
  // 两侧都报不出速率（不该发生，但 daemon 此时发 0）。**不显示「0 Hz」，也不兜底
  // 成 48000**：那个兜底就是被修掉的那个 bug。
  'stats.meta.sampleRateNone': '线上采样率 —',
  'stats.meta.channels': '{v} 声道',
  'stats.origin.hal': '虚拟设备',
  'stats.origin.halTitle': '由某个应用选中这台对端的 AudioHub 设备而自动建立',
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
  'stats.extra.verdictPass': '校验通过 {snr} dB',
  'stats.extra.verdictFail': '校验未通过',
  'stats.extra.mixProbes': '混音探针 {n} 路',
  'stats.rttValue': '{v} ms',

  // ---------------------------------------------------------------- 授权门
  'onboarding.title': '开始之前，先完成授权',
  'onboarding.sub': 'AudioHub 要把声音在两台设备之间搬运，因此需要下面这些系统权限。macOS 的规则是：一项权限被拒绝后，应用就无法再弹窗询问，只能到系统设置里手动打开——所以请在这里一次给齐。',
  'onboarding.grantAll': '全部授权',
  'onboarding.recheck': '重新检查',
  'onboarding.enter': '进入主界面',
  'onboarding.skip': '跳过（部分功能不可用）',
  'onboarding.skipToast': '已跳过授权：未授权的功能会在使用时直接失败。可在「设置 → 系统权限」重新授权。',
  'onboarding.hint.busy': '正在等待系统授权对话框…请在弹出的窗口中选择「允许」。',
  'onboarding.hint.blocking': '还差 {n} 项必需权限：{names}。授权后可直接进入主界面；若你在系统设置里改过，回到本窗口会自动重新检查。',
  'onboarding.hint.ready': '必需权限已就绪，可以进入主界面。可选权限稍后也能在「设置 → 系统权限」里补上。',
  'onboarding.skipNote.blocking': '跳过后仍可使用界面，但{names}相关的功能会在使用时直接报错而不是静默失败。本设置不会记住——下次启动仍会先来这一页。',
  'onboarding.skipNote.optional': '可选权限（{names}）未授权：对应的共享来源会在选用时报错，其余功能不受影响。',
  'onboarding.noRequestable': '没有可以直接弹窗请求的权限了，请用「打开系统设置」逐项开启。',
  'onboarding.stillMissing': '仍有 {n} 项必需权限未授权：{names}',
  'onboarding.allGranted': '必需权限已全部授权',

  // ---------------------------------------------------------------- 权限
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
  'perm.note.undetermined': '点击「授权」后由 macOS 弹窗询问；系统只会问这一次。',
  'perm.note.deniedManual': '已被拒绝：macOS 不允许再次弹窗，只能手动打开。路径：{manual}',
  'perm.note.denied': '已被拒绝：macOS 不允许再次弹窗，只能手动打开。',
  'perm.note.restrictedManual': '受系统策略（如描述文件或屏幕使用时间）限制，本应用无法请求。路径：{manual}',
  'perm.note.restricted': '受系统策略（如描述文件或屏幕使用时间）限制，本应用无法请求。',
  'perm.note.unqueryable': '系统不提供查询接口，无法在此显示当前状态。',
  'perm.openManual': '请手动前往：{manual}',
  'perm.noSettingsUrl': '本机服务未提供系统设置入口。',
  'perm.settingsFallback': '若系统设置没有自动打开：{manual}',

  'perm.microphone.name': '麦克风',
  'perm.microphone.why': '把本机麦克风的声音共享给已配对的设备（例如让 Windows 电脑使用这台 Mac 的麦克风）。仅在你主动开启共享时采集。',
  'perm.microphone.manual': '系统设置 → 隐私与安全性 → 麦克风 → 打开 AudioHub',
  'perm.localNetwork.name': '本地网络',
  'perm.localNetwork.why': '在同一局域网内发现其他 AudioHub 设备，并与已配对的设备直接传输音频。音频不会上传到互联网。',
  'perm.localNetwork.manual': '系统设置 → 隐私与安全性 → 本地网络 → 打开 AudioHub',
  'perm.localNetwork.unknownNote': '系统不提供查询接口，首次使用时会询问。若此前拒绝过，需要到系统设置里重新允许。',
  'perm.systemAudio.name': '系统音频录制',
  'perm.systemAudio.why': '把这台 Mac 正在播放的声音共享给对方；只有把共享来源选为「系统音频」时才需要。',
  'perm.systemAudio.manual': '系统设置 → 隐私与安全性 → 系统音频录制 → 打开 AudioHub',
  'perm.unknown.name': '未知权限',
  'perm.unknown.why': '该权限由本机服务上报，界面暂无对应说明。',

  // ---------------------------------------------------------------- 错误
  // 这些是 Error.message。它们不只写进 console：rpc() 会把 message 直接 toast 出去，
  // 离线覆盖层也会把它插进「与本机服务的连接已断开（{reason}）」。所以它们同样是
  // 面向用户的文案，必须走语料。
  'error.versionMismatch': 'daemon 协议版本不匹配（期望 {expected}，实际 {actual}）',
  'error.unknownVersion': '未知',
  'error.authTimeout': 'daemon 认证握手超时',
  'error.authFailed': '认证失败',
  'error.requestFailed': '请求失败',
  'error.requestTimeout': '请求超时：{method}',
  'error.disconnected': '连接已断开',
  'error.connectionClosed': '连接已关闭',
  'error.cannotConnect': '无法连接 daemon',
  'error.ipcNotConnected': 'IPC 未连接',
  'error.notTauri': '非 Tauri 环境',
  'error.startFailed': '启动服务失败',
  'error.noEndpoint': '未提供连接参数',
  'error.connectTimeout': '连接服务超时',

  // ---------------------------------------------------------------- 外链 / 时间
  'vendor.blackhole': 'BlackHole（macOS）',
  'vendor.vbcable': 'VB-Cable（Windows）',
  'link.copied': '已复制链接，请在浏览器中打开',
  'link.openManually': '请在浏览器中打开：{url}',

  'time.uptime': '{hh}:{mm}:{ss}',
  'time.uptimeDays': '{d} 天 {hh}:{mm}:{ss}',
} as const;
