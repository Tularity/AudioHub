# AudioHubVad —— Windows 虚拟声卡驱动构建说明

本目录只负责**编译 + 测试签名**，不负责安装。

里程碑现状：

- **M6-1（已完成）**：一个编译期写死的静态设备 `AudioHub Virtual Audio` 出现在系统音频设备列表里。
- **M6-2（本轮，代码已落地、尚未装机）**：静态设备**已被删除**，改为
  **每台已配对对端各自一对系统音频设备**，由 daemon 经 IOCTL 在运行时增删。

⚠️ **M6-1 的验收判据在 M6-2 之后不再成立**：没有配对对端时，
系统里**一个 AudioHub 端点都没有**（`Get-PnpDevice -Class AudioEndpoint` 计数为 0 才是正确的）。
任何断言「驱动装好就应看到 AudioHub 端点」的脚本都是在测上一版架构
（本项目已三次栽在「判据来自旧架构」上，见 `docs/progress.md`）。

---

## 0. 这是什么

以微软官方样例 [`audio/simpleaudiosample`](https://github.com/microsoft/Windows-driver-samples/tree/main/audio/simpleaudiosample)
为基座的 PortCls / WaveRT 内核态驱动，本身就是「一个虚拟扬声器 + 一个虚拟麦克风」。
M6-1 阶段**只改名字与标识，不动任何音频逻辑**（`.cpp` / `.h` 一行未改），
目的是先把三条最不确定的链路验证掉：MSVC+WDK 编译链、测试签名链、root 枚举 PnP 链。

上游 commit：`ef7c3074748ab05726c3a9161d3256118efd76e2`，上游 README 保留为 `README.upstream.md`。

与上游相比改动的文件：

| 文件 | 改动 | 阶段 |
|---|---|---|
| `Source/Main/AudioHubVad.inx` | 硬件 ID `ROOT\SimpleAudioSample` → `ROOT\AudioHubVad`；服务名、`CatalogFile`、`.sys` 文件名同步改 | M6-1（命名） |
| `Source/Main/AudioHubVad.rc` | 版本资源里的文件名与描述 | M6-1（命名） |
| `Source/Main/Main.vcxproj` / `.Filters` | `TargetName`、`ResourceCompile Include` | M6-1（命名） |
| `AudioHubVad.sln` | 仅文件名 | M6-1（命名） |
| **`Source/Inc/AudioHubIoctl.h`** | **新增**。与 daemon 的冻结控制面契约（IOCTL 码、结构体、布局断言） | M6-2 |
| **`Source/Inc/perpeer.h` / `Source/Main/perpeer.cpp`** | **新增**。每对端一对端点的槽位表与运行时增删 | M6-2 |
| **`Source/Inc/ctldevice.h` / `Source/Main/ctldevice.cpp`** | **新增**。`\\.\AudioHubVadCtl` 控制设备、IOCTL 派发、调用方身份校验 | M6-2 |
| `Source/Main/adapter.cpp` | `MaxObjects` 4 → 64；删掉 `StartDevice` 里的静态端点安装；`DriverEntry`/`DriverUnload`/`PnpHandler` 挂钩控制设备 | M6-2 |
| `Source/Filters/minipairs.h` | 删掉静态 `ENDPOINT_MINIPAIR`；新增 `g_MaxAudioHubMiniports` 及其 `C_ASSERT` | M6-2 |
| `Source/Filters/speakertoptable.h` / `micarray1toptable.h` | bridge pin 的 `KsPinDescriptor.Name` 指向自定义 GUID | M6-2 |
| `Source/Main/AudioHubVad.inx` | 四个接口段由**实体端点**降级为**模板**；新增 `MediaCategories` pin 名 | M6-2 |
| `Source/Main/common.cpp` | `ConnectTopologies` 之后**重新武装**两个 KS 接口；`InstallEndpointFilters` / `RemoveEndpointFilters` / `DisconnectTopologies` 如实报告部分失败 | M6-2 D3 |
| `Source/Inc/AudioHubIoctl.h` | 协议 2→**3**：`AH_BIND_REPLY.reserved` 变 `flags`，新增 `AH_STAGE_PINNAME` / `AH_BINDFLAG_FAIL_PIN_NAME` / `AH_BINDREPLY_FLAG_PIN_NAME_FALLBACK` | M6-2 D2 |
| `Source/Inc/perpeer.h` / `Source/Main/perpeer.cpp` | **每对端设备名**：由指纹派生 pin 名 GUID，绑定时写 `MediaCategories\{guid}\Name`，解绑时清除；topology filter 描述符与 pin 数组改为**每槽位一份** | M6-2 D2 |

音频逻辑（`minwavert` / `minwavertstream` / `mintopo` / `basetopo`）**至今一行未改**。

新增的构建基础设施：`Directory.Build.props`、`build.ps1`、`sign-test.ps1`、`tools/`、`BUILD.md`、`.gitignore`。

对外标识（安装/验收要用的判据）：

```
硬件 ID       ROOT\AudioHubVad
服务名        AudioHubVad          (Win32_SystemDriver 里的 Name)
驱动文件      audiohubvad.sys
INF / CAT     AudioHubVad.inf / AudioHubVad.cat
设备描述      AudioHub Virtual Audio      (devnode，不是端点)
控制设备      \\.\AudioHubVadCtl          (daemon 用 CreateFileW 打开)
Class         MEDIA {4d36e96c-e325-11ce-bfc1-08002be10318}

端点          未配对时为 0 个。
              每配对一台对端 +2 个，接口引用串是
                  AhWaveOut-<16位指纹> / AhTopoOut-<16位指纹>
                  AhWaveIn -<16位指纹> / AhTopoIn -<16位指纹>
              用户看到的名字由系统合成为「<pin 名> (<devnode FriendlyName>)」，
              即「AudioHub – <对端主机名> 扬声器 (AudioHub Virtual Audio)」。
              括号前那一整段与 macOS 的「AudioHub – <主机名> 扬声器」逐字相同。
```

> **引用串用对端指纹而不是槽位号**，这是本阶段最重要的正确性决定：
> 引用串决定 endpoint ID，而 endpoint ID 是 Windows 挂载「系统默认设备选择 /
> 每应用设备指派 / 端点音量 / 用户改过的名字」的地方。用槽位号会让槽位被回收后
> 分给的**新对端静默继承旧对端的这些状态**——不报错、不蓝屏，只是「新配的机器
> 莫名其妙成了默认输出」，而系统里没有任何地方能解释原因。

> **端点名的两半各自来自哪里（M6-2 实测纠正了 spec 的判断）**：
> 括号里那半来自 **devnode 的 FriendlyName**，不是 KS 接口的
> `DEVPKEY_DeviceInterface_FriendlyName`——决定性实验是直接改
> `HKLM\...\Enum\ROOT\MEDIA\0000` 的 `FriendlyName`，两个端点的括号内容立刻跟着变；
> 而给 KS 接口写同一属性，注册表里逐字读得到、端点名毫无变化。
> devnode 只有一个、被所有对端共享，所以**括号那半永远无法表达每对端不同的名字**。
>
> 能表达的是**括号前那半 = pin 名**（per-filter）。因此每个槽位分到一个
> **由对端指纹确定性派生**的 pin 名 GUID（形如
> `{9F3C7A21-6B48-4D00-<指纹前 4 位>-<指纹后 12 位>}`，第三段末位 0=渲染 1=采集），
> 驱动在绑定时把 `AudioHub – <主机名> 扬声器` 写进
> `MediaCategories\{该 GUID}\Name`，解绑时删除。
> 「扬声器 / 麦克风」这两个词**不写死在 .cpp 里**，而是驱动启动时从 INF 装好的那两条
> 静态 `MediaCategories` 项里**读回来**——本地化字符串只留 INF 一份，且 `.cpp` 保持纯 ASCII
> （非 ASCII 源码字面量会被 MSVC 按构建机的 ANSI 代码页解码）。
>
> 写不进去时**回退**为 INF 的静态 GUID（名字变成通用的「扬声器 (AudioHub Virtual Audio)」），
> 并在 `AH_BIND_REPLY.flags` 里置 `AH_BINDREPLY_FLAG_PIN_NAME_FALLBACK`
> 一路上报到 `daemon.status` 的 `hal.pin_name_fallbacks`。
> 回退不算失败（设备可用），但绝不静默——两台对端同时配对时它意味着两个同名扬声器。

---

## 1. 前置条件

构建机需要：

- **Visual Studio 2022 Build Tools**（含 MSVC v143 与 Windows SDK），
  **不需要**安装 WDK MSI，**也不需要** VS 的 “Windows Driver Kit” 组件（见下面 §3 的说明）。
- **.NET SDK**（`dotnet` 在 PATH 上），仅用于 `dotnet restore` 拉 NuGet 包。
- 能访问 `api.nuget.org`（首次拉包时）。

WDK 走 **NuGet 包**而不是 MSI：

- `Microsoft.Windows.WDK.x64` `10.0.26100.6584`
- `Microsoft.Windows.SDK.CPP` / `Microsoft.Windows.SDK.CPP.x64` `10.0.26100.1`

NuGet 路线**不写注册表、不做机器级安装、不需要重启**——包的入口 props 只是把
`WDKContentRoot` 指向包自身目录：

```xml
<WDKContentRoot>$(NuGetPackageFolder)\c</WDKContentRoot>
```

这一点对本项目是硬约束：构建机 30-win 不允许重启。

---

## 2. 构建与签名

在**构建机**上，从本目录执行：

```powershell
# 1) 编译（自动完成 NuGet restore + VCTargets shim）
.\build.ps1                       # 默认 Release|x64
.\build.ps1 -Configuration Debug
.\build.ps1 -Clean                # Rebuild

# 2) 用自签测试证书签 .sys 与 .cat
.\sign-test.ps1
```

产物落在：

```
x64\Release\package\             # MSBuild 的原始输出
  AudioHubVad.sys / .inf / .cat
_build\testcert\
  AudioHubVad-TestCert.cer       # 拿去装到测试机
  AudioHubVad-TestCert.pfx       # 私钥，禁止提交
_build\dist\                     # ★ 下一阶段只需要这一个目录
  AudioHubVad.sys / .inf / .cat + AudioHubVad-TestCert.cer
```

`_build/`、`x64/` 等已写入本目录的 `.gitignore`——仓库会被 `regress/sync-30win.sh`
同步到 30-win，构建产物不进仓库。

> 构建**不是**逐位可复现的：`.sys` 带 PE 时间戳，每次重新编译都会变；
> `.inf` 的 `DriverVer` 由 stampinf 打成构建时刻；`.cat` 每次 Inf2Cat 都重新生成。
> 只有「同一个 `.sys` 重新签名」才会得到相同哈希。
> 需要认某一份产物时，认 `_build\dist\` 里那次跑出来的哈希，不要跨构建比对。

### 两个脚本各自的强制校验

`build.ps1` 在 MSBuild 之后会断言产物存在且非空，并断言生成出来的 INF 里确实含有
`ROOT\AudioHubVad` / `AudioHub Virtual Audio` / `AudioHubVad.cat`；任何一条不满足就让构建失败。
半成品驱动包「静默成功」正是坏 `.sys` 混上测试机的典型路径。

`sign-test.ps1` 的顺序不可调换：**先签 `.sys` → 再跑 Inf2Cat 生成 `.cat` → 最后签 `.cat`**。
catalog 记录的是包目录内每个文件的哈希，先生成 catalog 再签 `.sys` 会让 catalog 记下
未签名镜像的哈希，安装时校验必然失败。脚本因此**重新生成** `.cat`，而不是复用 MSBuild 产出的那份。

---

## 3. 为什么需要 `tools/wdk-vs-shim.ps1`

WDK NuGet 包提供了驱动构建的全部 MSBuild 逻辑（`WindowsDriver.*.props/.targets`、
`stampinf`、`Inf2Cat`、`devcon`），但**不提供 Visual Studio 侧的粘合层**。
那层粘合通常由 VS 安装器组件 `Component.Microsoft.Windows.DriverKit.BuildTools` 落到：

```
Platforms\x64\ImportAfter\WDK.x64.*.Platform.props
Platforms\x64\PlatformToolsets\WindowsKernelModeDriver10.0\Toolset.props / .targets
```

缺了它，MSBuild 直接停在：

```
error MSB8020: The build tools for WindowsKernelModeDriver10.0 cannot be found.
```

（上游 `Windows-driver-samples/BuildEnvironment.ps1` 也印证了这点：即使在 NuGet 模式下，
它依然要求存在带 DriverKit 组件的 VS 安装。）

本项目不装那个 VS 组件——装它要动构建机的 VS 安装、有触发 pending-reboot 的风险，
而 30-win 的「无挂起重启」基线必须保持。改为：

`tools/wdk-vs-shim.ps1` 把 VS 自带的 VC targets 树（v170，约 175 文件 / 2.4 MB）
复制到 `_build\vctargets\v170`，在副本里补上缺的那几个文件，构建时用
`/p:VCTargetsPath=<副本>` 指过去。**真实的 VS 安装全程不被修改。**

其中 `ImportAfter\*` 直接取自 WDK NuGet 包（就是 VS 组件本会安装的那几份，
自带 `$(IsKernelModeToolset)` 守卫）；只有 `Toolset.props` / `Toolset.targets` 是自己写的，
形状照抄 VS 自带的 v143 版本，额外做四件事：

| 补的东西 | 不补会怎样 |
|---|---|
| `IsKernelModeToolset=true` | `ImportAfter` 里的 WDK 链条全部被守卫条件跳过 |
| `WDKContentRoot` 补尾部反斜杠 | NuGet props 产出的是 `...\c`（无尾分隔符），而所有消费方都写 `$(WDKContentRoot)build\...`，拼出来是 `...\cbuild\` |
| `WDKBuildFolder = $(TargetPlatformVersion)` | 找不到 `c\build\10.0.26100.0\` 下的任何 props |
| 导入 DesignTime 的 `WDK.props` / `UAP.props` | `KM_IncludePath` / `CRT_IncludePath` / `UM_IncludePath` / `KIT_SHARED_IncludePath` 未定义，编译报 `Cannot open include file: 'portcls.h'` |
| 导入 `WindowsDriver.Default.props` | `TargetPlatformVersion_NI` 等常量未定义，`WindowsDriver.OS.props` 报 `MSB4086: A numeric comparison was attempted on ...` |

### 必须用 64 位 MSBuild，且必须 `/nr:false`

`build.ps1` 调的是 `MSBuild\Current\Bin\amd64\MSBuild.exe`，不是同级目录下的 32 位那个。
`WindowsDriver.Common.targets` 按 `$(Processor_Architecture)`（= MSBuild **进程**架构）
挑工具路径；32 位 MSBuild 下它既不匹配 `AMD64` 也不匹配 `ARM64`，`InfToolPath` 留空，构建死在：

```
TRACKER : error TRK0005: Failed to locate: "stampinf.exe".
```

（顺带：这版 NuGet 包里 `stampinf.exe` 只有 x64 一份，`Inf2Cat.exe` 只有 x86 一份，
已安装的 Windows SDK 里则根本没有 `Inf2Cat` —— 这是 WDK NuGet 包不可省的原因之一。）

另外必须传 `/nr:false`（关掉 MSBuild node reuse）。默认情况下 MSBuild 构建结束后会留下
worker 进程，那些进程持有 `_build\vctargets\v170\Microsoft.Build.CPPTasks.Common.dll`
的文件锁，**下一次**构建刷新私有 VCTargets 时就会死在
`Access to the path 'Microsoft.Build.CPPTasks.Common.dll' is denied`。
`wdk-vs-shim.ps1` 另有一层防御：默认复用已存在的副本而不是先删再拷，只有 `-Force` 才整棵重建。

### DriverVer 的日期必须按 UTC 给，不能用 stampinf 的默认值

stampinf 默认（`%(Inf.TimeStamp)` = `*`）写的是**构建机本地日期**，而 inf2cat 拿 **UTC**
判「是不是未来日期」。30-win 在 UTC+10，于是**本地时间 10:00 之前发起的每一次构建**
都会编译链接全过、卡在打包这一步：

```
22.9.7: DriverVer set to a date in the future (postdated DriverVer not allowed)
error MSB6006: "inf2cat.exe" exited with code -2.
```

即构建的可重复性取决于一天里的钟点。`build.ps1` 因此显式算一个
`[DateTime]::UtcNow.AddDays(-1)` 的日期，经 `/p:AudioHubDriverVerDate=` 传给
`Main.vcxproj` 里 `Inf` 项的 `DateStamp` 元数据（**不是** `TimeStamp`——那是 `-v`
版本号，填日期进去会得到 `Invalid version '08/01/2026.0.0.0'`）。
往回退一天对地球上任何时区都必定是过去，于是打包不再依赖构建发生在何时何地。

---

## 4. 关于签名：testsigning 不等于不用签名

这是最容易在 M6-1 卡住半天的误解。即使测试机已经
`bcdedit /set testsigning on`，内核驱动镜像**仍然必须带有效的 Authenticode 签名**——
test-signing 模式放宽的是「签名可以由谁签发」，不是「要不要签名」。
`nointegritychecks` 也**不能**替代签名。

`sign-test.ps1` 用 `New-SelfSignedCertificate` 造一张可导出的代码签名测试证书
（`CN=AudioHub Test Signing`，SHA256 / RSA2048 / 10 年），签 `.sys` 与 `.cat`，
并导出 `.cer`。测试机上需要把这张 `.cer` 导入两个存储：

```powershell
Import-Certificate -FilePath AudioHubVad-TestCert.cer -CertStoreLocation Cert:\LocalMachine\Root
Import-Certificate -FilePath AudioHubVad-TestCert.cer -CertStoreLocation Cert:\LocalMachine\TrustedPublisher
```

- `Root`：让签名链能走通（否则驱动加载被拒）。
- `TrustedPublisher`：让安装时**不弹**「是否安装此设备软件」对话框——
  无人值守场景下这条是硬需求。

### 验证签名

`signtool verify /pa` 会走真实信任链，所以在**没有导入测试根**的机器上，
它必然停在 `terminated in a root certificate which is not trusted` 并返回非零。
**这是构建机上的正确结果**，不说明签名有问题。

`sign-test.ps1` 因此只做**与信任无关**的断言（这三条在任何机器上都必须成立）：

1. `.sys` 与 `.cat` 都带签名；
2. 签名者指纹 == 刚才那张证书；
3. 文件摘要是 SHA256（从 `signtool verify /pa /v` 输出里的 `Hash of file (sha256)` 取证）。

脚本**不会**把测试根导入任何存储：

- 导入 `Cert:\CurrentUser\Root` 会弹交互确认框，SSH 下直接失败
  （`Import-Certificate : UI is not allowed in this operation.`）——
  无人值守场景下这类操作一律不能出现；
- 导入 `Cert:\LocalMachine\Root` 则会改掉**整台构建机**的代码签名信任。

真正的 `/pa` PASS 属于测试机——它本来就要导入这张证书才能装驱动：

```powershell
Import-Certificate -FilePath AudioHubVad-TestCert.cer -CertStoreLocation Cert:\LocalMachine\Root
Import-Certificate -FilePath AudioHubVad-TestCert.cer -CertStoreLocation Cert:\LocalMachine\TrustedPublisher
signtool verify /pa /v AudioHubVad.sys
```

---

## 5. 安装（下一阶段，本阶段不执行）

留在这里备查，**不要在构建机上跑**：

```powershell
# 虚拟声卡是 root 枚举设备。注意 pnputil /add-driver /install 不会替你创建 root devnode，
# 必须用 devcon（devcon.exe 就在 WDK NuGet 包里）。
devcon.exe install AudioHubVad.inf ROOT\AudioHubVad
```

`devcon.exe` 路径：

```
<PackagesRoot>\microsoft.windows.wdk.x64\10.0.26100.6584\c\tools\10.0.26100.0\x64\devcon.exe
```

装之前必须先给测试虚拟机打检查点：驱动 bug 蓝屏后无人值守的机器救不回来。

---

## 6. 许可

`Source/`、`Package/` 下的代码派生自 microsoft/Windows-driver-samples（MIT），
版权归 Microsoft Corporation。上游 README 保留在 `README.upstream.md`。
