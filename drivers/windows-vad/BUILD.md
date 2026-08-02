# AudioHubVad —— Windows 虚拟声卡驱动构建说明

对应里程碑 **M6-1**：让一个名为 AudioHub 的音频设备出现在 Windows 系统音频设备列表里。
本目录只负责**编译 + 测试签名**，不负责安装（安装是下一阶段的事）。

---

## 0. 这是什么

以微软官方样例 [`audio/simpleaudiosample`](https://github.com/microsoft/Windows-driver-samples/tree/main/audio/simpleaudiosample)
为基座的 PortCls / WaveRT 内核态驱动，本身就是「一个虚拟扬声器 + 一个虚拟麦克风」。
M6-1 阶段**只改名字与标识，不动任何音频逻辑**（`.cpp` / `.h` 一行未改），
目的是先把三条最不确定的链路验证掉：MSVC+WDK 编译链、测试签名链、root 枚举 PnP 链。

上游 commit：`ef7c3074748ab05726c3a9161d3256118efd76e2`，上游 README 保留为 `README.upstream.md`。

与上游相比改动的文件（**全部只是命名**）：

| 文件 | 改动 |
|---|---|
| `Source/Main/AudioHubVad.inx` | 原 `SimpleAudioSample.inx`。硬件 ID `ROOT\SimpleAudioSample` → `ROOT\AudioHubVad`；服务名、`CatalogFile`、`.sys` 文件名同步改；`[Strings]` 里的设备描述/端点友好名改成 AudioHub 命名 |
| `Source/Main/AudioHubVad.rc` | 原 `SimpleAudioSample.rc`。版本资源里的文件名与描述 |
| `Source/Main/Main.vcxproj` | `TargetName` → `AudioHubVad`，`ResourceCompile Include` 指向新 rc |
| `Source/Main/Main.vcxproj.Filters` | 同上，纯 IDE 显示 |
| `AudioHubVad.sln` | 原 `SimpleAudioSample.sln`，仅文件名 |

新增的构建基础设施：`Directory.Build.props`、`build.ps1`、`sign-test.ps1`、`tools/`、`BUILD.md`、`.gitignore`。

对外标识（下一阶段安装/验收要用的判据）：

```
硬件 ID       ROOT\AudioHubVad
服务名        AudioHubVad          (Win32_SystemDriver 里的 Name)
驱动文件      audiohubvad.sys
INF / CAT     AudioHubVad.inf / AudioHubVad.cat
设备描述      AudioHub Virtual Audio
渲染端点      AudioHub Speaker
采集端点      AudioHub Microphone
Class         MEDIA {4d36e96c-e325-11ce-bfc1-08002be10318}
```

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
