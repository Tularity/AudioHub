//! 设备固有延迟探针：把 `devlat::query` 在本机每一台设备上跑一遍并打表。
//!
//! 存在的理由是第二轮验收（规格 §6.5）会问「这台机器上 `play_dev` 到底是多少」，
//! 而那个数只有真机答得出。**纯属性查询**——与 `devlat` 模块本身受同一条纪律
//! 约束：不开流、不改默认设备、不触发权限，因此可以在 daemon 正在服务音频时
//! 安全运行。
//!
//! `cargo run -p audiohub-core --example devlat-probe`

use audiohub_core::audio::{self, DeviceKind};
use audiohub_core::devlat::{self, DevLatencyParts, DevTarget};
use audiohub_core::latency::LatSource;

fn main() {
    let report = audio::default_devices_report().ok();
    let default_out = report.as_ref().and_then(|r| r.default_output.clone());
    let default_in = report.as_ref().and_then(|r| r.default_input.clone());
    println!("默认输出: {}", default_out.as_deref().unwrap_or("(无)"));
    println!("默认输入: {}", default_in.as_deref().unwrap_or("(无)"));
    println!();

    show("默认输出 (play_dev)", &devlat::default_output());
    show("默认输入 (cap_dev)", &devlat::default_input());

    println!("\n---- 全部设备 ----");
    for e in audio::list_devices_detailed() {
        for (kind, on) in [(DeviceKind::Output, e.is_output), (DeviceKind::Input, e.is_input)] {
            if !on {
                continue;
            }
            let target = match e.uid.as_deref() {
                Some(uid) => DevTarget::Uid(uid),
                None => DevTarget::Name(&e.name),
            };
            let dir = if matches!(kind, DeviceKind::Output) { "out" } else { "in " };
            show(&format!("[{dir}] {}", e.name), &devlat::query(kind, target));
        }
    }
}

/// 传输方式属性是四字符码，打成可读形式；0 = 没读到。
fn fourcc(v: u32) -> String {
    if v == 0 {
        return "-".to_string();
    }
    let b = v.to_be_bytes();
    match std::str::from_utf8(&b) {
        Ok(s) if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') => format!("'{s}'"),
        _ => format!("0x{v:08X}"),
    }
}

fn show(label: &str, p: &DevLatencyParts) {
    let total = p.total();
    let ms = match total.ms() {
        Some(ms) => format!("{ms:.3} ms ({} 帧 @ {} Hz)", total.frames, total.rate),
        None => "不可用".to_string(),
    };
    let prefix = match total.source {
        LatSource::Api => "",
        LatSource::Assumed => "≥",
        LatSource::Unreliable => "≥",
        LatSource::Unavailable => "",
    };
    println!("{label}");
    println!(
        "  总量      : {prefix}{ms}  [{:?}]  transport={:?}({})",
        total.source,
        p.transport,
        fourcc(p.transport_code)
    );
    if !p.parts.is_empty() {
        let detail: Vec<String> = p
            .parts
            .iter()
            .map(|(n, f)| {
                let ms = if p.rate > 0 {
                    format!("{:.3}ms", *f as f64 * 1000.0 / p.rate as f64)
                } else {
                    "?".into()
                };
                format!("{n}={f}帧/{ms}")
            })
            .collect();
        println!("  分项      : {}", detail.join("  "));
    }
    if !p.missing.is_empty() {
        println!("  缺项      : {:?}", p.missing);
    }
    if let Some(e) = &p.error {
        println!("  错误      : {e}");
    }
}
