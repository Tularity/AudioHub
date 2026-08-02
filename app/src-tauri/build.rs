// Windows 高 DPI：用自定义应用清单替换 tauri-build 的默认清单。
//
// 问题现场（30-win，2560x1440 @ 150%）：界面右侧三分之一被切到窗口外，导航胶囊第四项
// 「设置」看不见、模式栏文字断在半句。前端侧的测量自洽地指向同一件事——CSS 坐标到物理
// 像素的换算是 1.5 倍（内容左边界 720 CSS -> 1082 物理、胶囊中心 1280 CSS -> 1920 物理），
// 而 WebView2 拿到的 CSS 视口宽度等于窗口的**物理**宽度。两者相乘，渲染面就比窗口宽
// 1.5 倍，右边三分之一自然落在窗口之外（2560 / 3840 = 66.7%）。
//
// 根因：本进程没有在应用清单里声明 DPI 感知。
//
//   * tauri-build 2.6.3 的默认清单（其 src/windows-app-manifest.xml，334 字节）里**只有**
//     Common-Controls v6 依赖，既没有 dpiAware / dpiAwareness，也没有 supportedOS。
//     已在 30-win 上从部署的 audiohub-app.exe 里把 RT_MANIFEST(id=1) 抠出来核对过，
//     内容与该默认清单逐字一致。
//   * 于是进程的 DPI 感知只剩 tao 在创建事件循环时的运行期调用
//     SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)
//     （tao-0.35.3 src/platform_impl/windows/dpi.rs::become_dpi_aware）。那条路径没有
//     任何保证：一旦感知已被别处锁定，调用就失败，而 tao 用 `let _ =` 把失败静默吞掉，
//     既不重试也不报错——进程会带着「未感知」继续跑，且不留下任何痕迹。
//   * 宿主进程一旦是 DPI 未感知，WebView2 就按约定把宿主坐标系当作 DIP，再乘显示器
//     缩放去光栅化；而窗口本身是按物理像素摆放的，两边对不上就是上面那个 1.5 倍。
//     微软对 WebView2 宿主的要求本来就是「必须声明 DPI 感知」。
//
// 为什么改清单而不是去修运行期调用：清单在进程启动时、任何 DLL 初始化和窗口创建之前
// 就生效，是微软推荐的权威做法；运行期 API 是「没法用清单时」的退路，天然存在时序竞争。
// 换成清单声明后，无论 tao 那次调用成功与否，进程从第一条指令起就是 PerMonitorV2。
//
// 两个必须注意的点：
//
//   1. WindowsAttributes::app_manifest 是**替换**语义——tauri-build 内部只保存一个
//      Option<String>，最后交给 tauri_winres::set_manifest 写成唯一的 RT_MANIFEST 资源。
//      所以不会出现重复清单/链接冲突；但也正因为是替换，自定义清单必须自己带上
//      Common-Controls v6 依赖，否则会丢视觉样式并影响 tauri 的对话框 API。
//   2. tauri-build 自己会发 cargo:rerun-if-changed（至少对 tauri.conf.json），这已经关掉了
//      cargo「包内任意文件变动就重跑」的默认行为。所以清单文件必须自己声明一条，
//      否则改了清单不会触发重新构建。
fn main() {
    println!("cargo:rerun-if-changed=windows-app-manifest.xml");

    let attributes = tauri_build::Attributes::new().windows_attributes(
        tauri_build::WindowsAttributes::new()
            .app_manifest(include_str!("windows-app-manifest.xml")),
    );

    // 下面这段是 tauri_build::build() 的原样复刻：它本身就只是
    // try_build(Attributes::default()) 加上这段错误提示，改用 try_build 后得自己带上，
    // 否则构建失败时会退化成一句光秃秃的 panic。
    if let Err(error) = tauri_build::try_build(attributes) {
        let error = format!("{error:#}");
        println!("{error}");
        if error.starts_with("unknown field") {
            print!(
                "found an unknown configuration field. This usually means that you are using a CLI version that is newer than `tauri-build` and is incompatible. "
            );
            println!(
                "Please try updating the Rust crates by running `cargo update` in the Tauri app folder."
            );
        }
        std::process::exit(1);
    }
}
