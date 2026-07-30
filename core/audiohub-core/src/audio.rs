use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SupportedStreamConfig};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

#[derive(Debug, serde::Serialize)]
pub struct DevicesReport {
    pub default_output: Option<String>,
    pub default_input: Option<String>,
    pub output_config: Option<String>,
    pub input_config: Option<String>,
}

fn describe(cfg: &SupportedStreamConfig) -> String {
    format!(
        "{}Hz {}ch {}",
        cfg.sample_rate().0,
        cfg.channels(),
        format!("{:?}", cfg.sample_format()).to_lowercase()
    )
}

pub fn default_devices_report() -> Result<DevicesReport> {
    let host = cpal::default_host();
    let out_dev = host.default_output_device();
    let in_dev = host.default_input_device();
    let default_output = out_dev.as_ref().and_then(|d| d.name().ok());
    let default_input = in_dev.as_ref().and_then(|d| d.name().ok());
    let output_config = out_dev
        .as_ref()
        .and_then(|d| d.default_output_config().ok())
        .map(|c| describe(&c));
    // On coreaudio BOTH default_input_config() and supported_input_configs()
    // build an input AudioUnit (AudioDeviceCreateIOProcID), which blocks behind
    // the mic-permission (TCC) machinery when consent is absent. A listing
    // probe must never touch the input unit: report the name only.
    let input_config = None;
    Ok(DevicesReport {
        default_output,
        default_input,
        output_config,
        input_config,
    })
}

// ----------------------------------------------------------- named devices

/// Which side of a device a listing, a lookup or a watcher is about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceKind {
    Input,
    Output,
}

impl DeviceKind {
    fn word(self) -> &'static str {
        match self {
            DeviceKind::Input => "input",
            DeviceKind::Output => "output",
        }
    }
}

/// Names of every device that can play audio, deduplicated, enumeration order.
pub fn list_output_devices() -> Vec<String> {
    list_names(DeviceKind::Output)
}

/// Names of every device that can capture audio, deduplicated, enumeration
/// order. Listing never opens a device, so it never trips a permission prompt.
pub fn list_input_devices() -> Vec<String> {
    list_names(DeviceKind::Input)
}

/// THE DUPLICATE-NAME RULE: presentation deduplicates, resolution does not.
/// Callers (UI dropdowns, `daemon.status`) get this collapsed list, while
/// `resolve_name` always runs over the RAW enumeration. Two distinct cards
/// sharing one name therefore stay visible to the ambiguity check and are
/// rejected, instead of collapsing into a single entry that would silently
/// resolve to whichever of them cpal happened to enumerate first.
fn list_names(kind: DeviceKind) -> Vec<String> {
    dedup_in_order(devices::list_all(kind))
}

fn dedup_in_order(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        if !n.is_empty() && !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// Case-insensitive exact name first, then a case-insensitive PREFIX match, so
/// `BlackHole` resolves `BlackHole 2ch` and `blackhole 2ch` still takes the
/// fast path instead of being punished as an ambiguous prefix of a longer
/// sibling. An exact hit wins over every prefix hit. Anything else — several
/// prefix hits, or several devices carrying the one name — is an error listing
/// the candidates: picking one would be a coin flip on which card audio lands.
/// `names` must be the raw list (duplicates included); see `list_names`.
fn resolve_name(names: &[String], query: &str, kind: DeviceKind) -> Result<String> {
    let q = query.trim();
    if q.is_empty() {
        bail!("empty {} device name", kind.word());
    }
    let ql = q.to_lowercase();
    let exact: Vec<&String> = names.iter().filter(|n| n.to_lowercase() == ql).collect();
    let hits = if exact.is_empty() {
        names
            .iter()
            .filter(|n| n.to_lowercase().starts_with(&ql))
            .collect::<Vec<&String>>()
    } else {
        exact
    };
    match hits.len() {
        1 => Ok(hits[0].clone()),
        0 => bail!(
            "no {} device matches {q:?}; available: [{}]",
            kind.word(),
            dedup_in_order(names.iter().cloned()).join(", ")
        ),
        n if hits.iter().all(|h| h.as_str() == hits[0].as_str()) => bail!(
            "{} device name {q:?} is ambiguous: {n} devices are named {:?}; \
             rename one of them in the system settings",
            kind.word(),
            hits[0]
        ),
        _ => bail!(
            "{} device name {q:?} is ambiguous; candidates: [{}]",
            kind.word(),
            hits.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Resolves `query` against the devices of `kind` and hands back the cpal
/// handle. There is deliberately no fallback to the default device: a bridge
/// that quietly played into the speakers instead of the virtual card would be
/// worse than a failed session.
fn find_device(kind: DeviceKind, query: &str) -> Result<(cpal::Device, String)> {
    let names = devices::list_all(kind);
    let resolved = resolve_name(&names, query, kind)?;
    let dev = devices::find(kind, &resolved)
        .ok_or_else(|| anyhow!("audio device {resolved:?} vanished between listing and open"))?;
    Ok((dev, resolved))
}

/// macOS listing goes straight to CoreAudio properties. cpal's own
/// `input_devices()` filter builds an *input* AudioUnit per device, which is
/// what blocks behind the microphone TCC machinery — the same reason
/// `default_devices_report` refuses to fill `input_config`. A listing must stay
/// permission-free.
#[cfg(target_os = "macos")]
mod devices {
    use super::DeviceKind;
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::ptr::{addr_of, null, null_mut};

    type OSStatus = i32;
    type AudioObjectID = u32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PropAddr {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[repr(C)]
    struct AudioBuffer {
        num_channels: u32,
        byte_size: u32,
        data: *mut c_void,
    }

    #[repr(C)]
    struct AudioBufferList {
        num_buffers: u32,
        first: AudioBuffer,
    }

    const SYSTEM_OBJECT: AudioObjectID = 1;
    const ELEM_MAIN: u32 = 0;

    const fn fourcc(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }
    const SEL_DEVICES: u32 = fourcc(b"dev#"); // kAudioHardwarePropertyDevices
    const SEL_STREAM_CONFIG: u32 = fourcc(b"slay"); // kAudioDevicePropertyStreamConfiguration
    const SEL_NAME: u32 = fourcc(b"lnam"); // kAudioDevicePropertyDeviceNameCFString
    const SCOPE_GLOBAL: u32 = fourcc(b"glob");
    const SCOPE_INPUT: u32 = fourcc(b"inpt");
    const SCOPE_OUTPUT: u32 = fourcc(b"outp");

    const CF_UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyDataSize(
            id: AudioObjectID,
            addr: *const PropAddr,
            qual_size: u32,
            qual: *const c_void,
            out_size: *mut u32,
        ) -> OSStatus;
        fn AudioObjectGetPropertyData(
            id: AudioObjectID,
            addr: *const PropAddr,
            qual_size: u32,
            qual: *const c_void,
            io_size: *mut u32,
            out: *mut c_void,
        ) -> OSStatus;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
        fn CFStringGetLength(s: *const c_void) -> isize;
        fn CFStringGetMaximumSizeForEncoding(len: isize, enc: u32) -> isize;
        fn CFStringGetCString(s: *const c_void, buf: *mut u8, size: isize, enc: u32) -> u8;
    }

    fn at(selector: u32, scope: u32) -> PropAddr {
        PropAddr { selector, scope, element: ELEM_MAIN }
    }

    fn prop_size(dev: AudioObjectID, a: &PropAddr) -> Option<u32> {
        let mut sz: u32 = 0;
        let st = unsafe { AudioObjectGetPropertyDataSize(dev, a, 0, null(), &mut sz) };
        (st == 0).then_some(sz)
    }

    fn device_ids() -> Vec<AudioObjectID> {
        let a = at(SEL_DEVICES, SCOPE_GLOBAL);
        let Some(sz) = prop_size(SYSTEM_OBJECT, &a) else {
            return Vec::new();
        };
        let n = sz as usize / size_of::<AudioObjectID>();
        if n == 0 {
            return Vec::new();
        }
        let mut ids = vec![0u32; n];
        let mut io = sz;
        let st = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &a,
                0,
                null(),
                &mut io,
                ids.as_mut_ptr() as *mut c_void,
            )
        };
        if st != 0 {
            return Vec::new();
        }
        ids.truncate(io as usize / size_of::<AudioObjectID>());
        ids
    }

    /// Total channel count the device exposes in `scope`; 0 means it does not
    /// do that direction at all (this is exactly how cpal decides the same).
    fn scope_channels(dev: AudioObjectID, scope: u32) -> u32 {
        let a = at(SEL_STREAM_CONFIG, scope);
        let Some(sz) = prop_size(dev, &a) else {
            return 0;
        };
        if (sz as usize) < size_of::<u32>() {
            return 0;
        }
        // u64 backing: AudioBufferList is 8-aligned because AudioBuffer holds a
        // pointer, so `mBuffers` starts at offset 8, not 4.
        let mut backing = vec![0u64; (sz as usize + 7) / 8];
        let mut io = sz;
        let st = unsafe {
            AudioObjectGetPropertyData(
                dev,
                &a,
                0,
                null(),
                &mut io,
                backing.as_mut_ptr() as *mut c_void,
            )
        };
        if st != 0 {
            return 0;
        }
        let list = backing.as_ptr() as *const AudioBufferList;
        let n = unsafe { (*list).num_buffers } as usize;
        let first = unsafe { addr_of!((*list).first) };
        let offset = first as usize - list as usize;
        if offset + n * size_of::<AudioBuffer>() > io as usize {
            return 0;
        }
        (0..n)
            .map(|i| unsafe { (*first.add(i)).num_channels })
            .sum()
    }

    /// The CFStringRef comes back owned (AudioObject "get" of a CF object hands
    /// the caller a +1 reference), so it must be released here.
    fn device_name(dev: AudioObjectID) -> Option<String> {
        // Scope Output first: that is what cpal's Device::name() asks for, and
        // the two lists have to agree or find_device() could not match.
        for scope in [SCOPE_OUTPUT, SCOPE_GLOBAL] {
            let a = at(SEL_NAME, scope);
            let mut cf: *const c_void = null_mut();
            let mut io = size_of::<*const c_void>() as u32;
            let st = unsafe {
                AudioObjectGetPropertyData(
                    dev,
                    &a,
                    0,
                    null(),
                    &mut io,
                    &mut cf as *mut *const c_void as *mut c_void,
                )
            };
            if st != 0 || cf.is_null() {
                continue;
            }
            let s = unsafe { cf_to_string(cf) };
            unsafe { CFRelease(cf) };
            if let Some(s) = s {
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
        None
    }

    unsafe fn cf_to_string(cf: *const c_void) -> Option<String> {
        let len = CFStringGetLength(cf);
        let max = CFStringGetMaximumSizeForEncoding(len, CF_UTF8);
        if max <= 0 {
            return Some(String::new());
        }
        let mut buf = vec![0u8; max as usize + 1];
        if CFStringGetCString(cf, buf.as_mut_ptr(), buf.len() as isize, CF_UTF8) == 0 {
            return None;
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(end);
        String::from_utf8(buf).ok()
    }

    /// Raw enumeration order, duplicates kept: an unnamed device can never be
    /// typed so it is dropped, but two cards with the same name must both stay
    /// visible to the ambiguity check.
    pub fn list_all(kind: DeviceKind) -> Vec<String> {
        let scope = match kind {
            DeviceKind::Input => SCOPE_INPUT,
            DeviceKind::Output => SCOPE_OUTPUT,
        };
        device_ids()
            .into_iter()
            .filter(|&d| scope_channels(d, scope) > 0)
            .filter_map(device_name)
            .filter(|n| !n.is_empty())
            .collect()
    }

    /// A coreaudio device carries both directions, so the direction was already
    /// settled by `list`; only the name has to match here.
    pub fn find(_kind: DeviceKind, name: &str) -> Option<cpal::Device> {
        use cpal::traits::{DeviceTrait, HostTrait};
        cpal::default_host()
            .devices()
            .ok()?
            .find(|d| d.name().map_or(false, |n| n == name))
    }
}

/// WASAPI answers the direction question from the endpoint's data flow alone
/// (cpal overrides `supports_input`/`supports_output` to do exactly that), so
/// no device is opened and no permission is involved.
#[cfg(not(target_os = "macos"))]
mod devices {
    use super::DeviceKind;
    use cpal::traits::{DeviceTrait, HostTrait};

    /// Raw enumeration order, duplicates kept: two WASAPI endpoints can carry
    /// the same friendly name and both must stay visible to the ambiguity
    /// check. Unnamed endpoints can never be typed, so they are dropped.
    pub fn list_all(kind: DeviceKind) -> Vec<String> {
        let host = cpal::default_host();
        let names: Vec<String> = match kind {
            DeviceKind::Input => host
                .input_devices()
                .map(|it| it.filter_map(|d| d.name().ok()).collect())
                .unwrap_or_default(),
            DeviceKind::Output => host
                .output_devices()
                .map(|it| it.filter_map(|d| d.name().ok()).collect())
                .unwrap_or_default(),
        };
        names.into_iter().filter(|n| !n.is_empty()).collect()
    }

    /// Searched inside the direction the caller asked for: a WASAPI render and
    /// capture endpoint can carry the same friendly name, and picking the wrong
    /// flow would hand back a device that cannot do the job.
    pub fn find(kind: DeviceKind, name: &str) -> Option<cpal::Device> {
        let host = cpal::default_host();
        let hit = |d: &cpal::Device| d.name().map_or(false, |n| n == name);
        match kind {
            DeviceKind::Input => host.input_devices().ok()?.find(hit),
            DeviceKind::Output => host.output_devices().ok()?.find(hit),
        }
    }
}

/// Stateful naive linear resampler (carries phase + last sample across chunks).
struct Resampler {
    step: f64, // input samples per output sample
    phase: f64,
    last: f32,
}

impl Resampler {
    fn new(src_rate: u32, dst_rate: u32) -> Self {
        Resampler {
            step: src_rate as f64 / dst_rate as f64,
            phase: 0.0,
            last: 0.0,
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let len = input.len() as f64;
        let mut p = self.phase;
        // position p: 0.0 == previous chunk's last sample, 1.0 == input[0]
        while p < len {
            let i = p.floor() as usize;
            let frac = (p - i as f64) as f32;
            let s0 = if i == 0 { self.last } else { input[i - 1] };
            let s1 = input[i];
            out.push(s0 + (s1 - s0) * frac);
            p += self.step;
        }
        self.phase = p - len;
        self.last = *input.last().unwrap();
    }
}

fn resample_all(samples: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if src_rate == dst_rate {
        return samples.to_vec();
    }
    let mut rs = Resampler::new(src_rate, dst_rate);
    let mut out = Vec::with_capacity(samples.len() * dst_rate as usize / src_rate as usize + 8);
    rs.process(samples, &mut out);
    out
}

// ------------------------------------------------------------ stream health

/// What the cpal error callback writes and the owner of the stream reads. cpal
/// reports a fatal stream error and then stops calling the data callback for
/// good, so `dead` is one-way: a dead stream is never revived, only replaced.
/// Without this the death of e.g. an unplugged bridge card was a mere log line
/// and the writer kept pushing into a stream nobody drains — silent forever.
#[derive(Default)]
struct StreamHealth {
    dead: AtomicBool,
    err: Mutex<Option<String>>,
}

impl StreamHealth {
    fn new() -> Arc<StreamHealth> {
        Arc::new(StreamHealth::default())
    }

    /// Called from the platform's stream-error path. Allocating and locking
    /// here is safe precisely because it happens once, when the stream is
    /// already finished. The first error is the diagnosis; later ones are
    /// fallout, so they only get logged.
    fn fail(&self, what: &str, e: &cpal::StreamError) {
        let msg = format!("{what} stream error: {e}");
        eprintln!("[audiohub] {msg}");
        {
            let mut slot = self.err.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_none() {
                *slot = Some(msg);
            }
        }
        // Published last: a reader that observes the death can then take the
        // message that explains it.
        self.dead.store(true, Ordering::Release);
    }

    fn is_alive(&self) -> bool {
        !self.dead.load(Ordering::Acquire)
    }

    fn take_error(&self) -> Option<String> {
        self.err.lock().unwrap_or_else(|p| p.into_inner()).take()
    }
}

fn err_sink(
    health: Arc<StreamHealth>,
    what: &'static str,
) -> impl FnMut(cpal::StreamError) + Send + 'static {
    move |e| health.fail(what, &e)
}

fn build_output_stream_f32(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    supported_format: SampleFormat,
    health: &Arc<StreamHealth>,
    mut fill_mono: impl FnMut(&mut [f32]) + Send + 'static,
) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    let mut mono: Vec<f32> = Vec::new();
    match supported_format {
        SampleFormat::I16 => {
            let stream = device.build_output_stream(
                config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let frames = data.len() / channels;
                    mono.resize(frames, 0.0);
                    fill_mono(&mut mono);
                    for (frame, &s) in data.chunks_mut(channels).zip(mono.iter()) {
                        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        frame.fill(v);
                    }
                },
                err_sink(Arc::clone(health), "output"),
                None,
            )?;
            Ok(stream)
        }
        // F32 native, or ask for f32 anyway and let the host convert.
        _ => {
            let stream = device.build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let frames = data.len() / channels;
                    mono.resize(frames, 0.0);
                    fill_mono(&mut mono);
                    for (frame, &s) in data.chunks_mut(channels).zip(mono.iter()) {
                        frame.fill(s);
                    }
                },
                err_sink(Arc::clone(health), "output"),
                None,
            )?;
            Ok(stream)
        }
    }
}

pub fn play_samples_blocking(samples: &[f32], src_rate: u32) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device"))?;
    let supported = device
        .default_output_config()
        .context("default output config")?;
    let config: cpal::StreamConfig = supported.config();
    let dev_rate = config.sample_rate.0;

    let resampled = Arc::new(resample_all(samples, src_rate, dev_rate));
    let pos = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(samples.is_empty()));

    let data = Arc::clone(&resampled);
    let pos_cb = Arc::clone(&pos);
    let done_cb = Arc::clone(&done);
    let health = StreamHealth::new();
    let stream = build_output_stream_f32(
        &device,
        &config,
        supported.sample_format(),
        &health,
        move |mono| {
            let mut p = pos_cb.load(Ordering::Relaxed);
            for m in mono.iter_mut() {
                *m = if p < data.len() {
                    let s = data[p];
                    p += 1;
                    s
                } else {
                    0.0
                };
            }
            pos_cb.store(p, Ordering::Relaxed);
            if p >= data.len() {
                done_cb.store(true, Ordering::Relaxed);
            }
        },
    )?;
    stream.play()?;
    // bounded wait: a stalled/removed device must not hang the caller forever
    let expected = Duration::from_secs_f64(resampled.len() as f64 / dev_rate.max(1) as f64);
    let drain_deadline = Instant::now() + expected + Duration::from_secs(2);
    while !done.load(Ordering::Relaxed) {
        // A dead stream never reaches `done`; report why instead of waiting out
        // the deadline and blaming a stall.
        if let Some(e) = health.take_error() {
            return Err(anyhow!(e));
        }
        if Instant::now() >= drain_deadline {
            return Err(anyhow!("output stream stalled (no progress before deadline)"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // let the device buffer drain before tearing the stream down
    std::thread::sleep(Duration::from_millis(150));
    drop(stream);
    Ok(())
}

pub struct LivePlayback {
    _stream: cpal::Stream,
    health: Arc<StreamHealth>,
}

pub struct AudioTx {
    prod: HeapProd<f32>,
    resampler: Option<Resampler>,
    staging: Vec<f32>,
}

impl AudioTx {
    pub fn push(&mut self, mono_samples: &[f32]) {
        match self.resampler.as_mut() {
            None => {
                let _ = self.prod.push_slice(mono_samples);
            }
            Some(rs) => {
                self.staging.clear();
                rs.process(mono_samples, &mut self.staging);
                let _ = self.prod.push_slice(&self.staging);
            }
        }
    }
}

impl LivePlayback {
    /// `false` once the device reported a fatal stream error — the card is gone
    /// or the host killed the stream, and nothing pushed into the paired
    /// `AudioTx` can reach it any more. One-way: a dead playback must be
    /// dropped and reopened, never waited on.
    pub fn is_alive(&self) -> bool {
        self.health.is_alive()
    }

    /// The recorded cause, handed to the first caller that asks so a supervisor
    /// reports it exactly once. `is_alive` stays `false` afterwards.
    pub fn take_error(&self) -> Option<String> {
        self.health.take_error()
    }

    pub fn start(src_rate: u32) -> Result<(LivePlayback, AudioTx)> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device"))?;
        LivePlayback::on_device(&device, src_rate)
    }

    /// Plays to the named output device. Errors when the name does not resolve
    /// or the device will not open — never falls back to the default device.
    pub fn start_on(device_name: &str, src_rate: u32) -> Result<(LivePlayback, AudioTx)> {
        let (device, resolved) = find_device(DeviceKind::Output, device_name)?;
        LivePlayback::on_device(&device, src_rate)
            .with_context(|| format!("open output device {resolved:?}"))
    }

    fn on_device(device: &cpal::Device, src_rate: u32) -> Result<(LivePlayback, AudioTx)> {
        let supported = device
            .default_output_config()
            .context("default output config")?;
        let config: cpal::StreamConfig = supported.config();
        let dev_rate = config.sample_rate.0;

        // >= 500ms required; use 1s of device-rate samples
        let rb = HeapRb::<f32>::new(dev_rate.max(8000) as usize);
        let (prod, mut cons) = rb.split();

        let health = StreamHealth::new();
        let stream = build_output_stream_f32(
            device,
            &config,
            supported.sample_format(),
            &health,
            move |mono| {
                let got = cons.pop_slice(mono);
                for m in &mut mono[got..] {
                    *m = 0.0; // underrun -> silence
                }
            },
        )?;
        stream.play()?;

        let resampler = if src_rate == dev_rate {
            None
        } else {
            Some(Resampler::new(src_rate, dev_rate))
        };
        Ok((
            LivePlayback { _stream: stream, health },
            AudioTx {
                prod,
                resampler,
                staging: Vec::new(),
            },
        ))
    }
}

pub struct LiveCapture {
    _stream: cpal::Stream,
    health: Arc<StreamHealth>,
}

pub struct AudioRx {
    cons: HeapCons<f32>,
}

impl AudioRx {
    pub fn pop(&mut self, out: &mut Vec<f32>) -> usize {
        let avail = self.cons.occupied_len();
        if avail == 0 {
            return 0;
        }
        let start = out.len();
        out.resize(start + avail, 0.0);
        let got = self.cons.pop_slice(&mut out[start..]);
        out.truncate(start + got);
        got
    }
}

impl LiveCapture {
    /// `false` once the device reported a fatal stream error — the paired
    /// `AudioRx` will never produce another sample. One-way, like playback:
    /// silence from a dead capture is indistinguishable from a quiet room, so
    /// the owner has to ask rather than infer.
    pub fn is_alive(&self) -> bool {
        self.health.is_alive()
    }

    /// The recorded cause, handed to the first caller that asks.
    pub fn take_error(&self) -> Option<String> {
        self.health.take_error()
    }

    pub fn start() -> Result<(LiveCapture, AudioRx, u32)> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?;
        LiveCapture::on_device(&device)
    }

    /// Captures from the named input device (e.g. the input side of a virtual
    /// card). Errors rather than falling back to the default microphone.
    pub fn start_on(device_name: &str) -> Result<(LiveCapture, AudioRx, u32)> {
        let (device, resolved) = find_device(DeviceKind::Input, device_name)?;
        LiveCapture::on_device(&device).with_context(|| format!("open input device {resolved:?}"))
    }

    fn on_device(device: &cpal::Device) -> Result<(LiveCapture, AudioRx, u32)> {
        let supported = device
            .default_input_config()
            .context("default input config")?;
        let config: cpal::StreamConfig = supported.config();
        let rate = config.sample_rate.0;
        let channels = config.channels as usize;

        let rb = HeapRb::<f32>::new((rate as usize) * 2); // 2s
        let (mut prod, cons) = rb.split();

        let health = StreamHealth::new();
        let mut mono: Vec<f32> = Vec::new();
        let stream = match supported.sample_format() {
            SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    mono.clear();
                    for frame in data.chunks(channels) {
                        let sum: f32 = frame.iter().map(|&v| v as f32 / 32768.0).sum();
                        mono.push(sum / channels as f32);
                    }
                    let _ = prod.push_slice(&mono);
                },
                err_sink(Arc::clone(&health), "input"),
                None,
            )?,
            _ => device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    mono.clear();
                    for frame in data.chunks(channels) {
                        let sum: f32 = frame.iter().sum();
                        mono.push(sum / channels as f32);
                    }
                    let _ = prod.push_slice(&mono);
                },
                err_sink(Arc::clone(&health), "input"),
                None,
            )?,
        };
        stream.play()?;
        Ok((LiveCapture { _stream: stream, health }, AudioRx { cons }, rate))
    }
}

// ------------------------------------------------- default device hot-swap

/// Signal the OS notification thread raises and our worker thread consumes.
/// Repeated notifications collapse into one wakeup: the callback carries no
/// payload, it only means "re-query".
struct Fanout {
    state: Mutex<FanoutState>,
    cv: Condvar,
}

#[derive(Default)]
struct FanoutState {
    pending: bool,
    stop: bool,
}

impl Fanout {
    fn new() -> Fanout {
        Fanout { state: Mutex::new(FanoutState::default()), cv: Condvar::new() }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FanoutState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Called ON the platform notification thread. The only thing it may touch
    /// is this uncontended mutex — the user callback runs on our worker.
    fn signal(&self) {
        self.lock().pending = true;
        self.cv.notify_all();
    }

    fn stop(&self) {
        self.lock().stop = true;
        self.cv.notify_all();
    }

    /// `true` = one or more notifications arrived, `false` = shut down.
    fn wait(&self) -> bool {
        let mut s = self.lock();
        loop {
            if s.stop {
                return false;
            }
            if s.pending {
                s.pending = false;
                return true;
            }
            s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Lets `start` report a registration failure that happens on the worker.
struct Handshake {
    slot: Mutex<Option<std::result::Result<(), String>>>,
    cv: Condvar,
}

impl Handshake {
    fn new() -> Handshake {
        Handshake { slot: Mutex::new(None), cv: Condvar::new() }
    }

    fn publish(&self, r: std::result::Result<(), String>) {
        let mut g = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(r);
        self.cv.notify_all();
    }

    /// `None` = the worker is still inside registration. It is then NOT joinable
    /// without reintroducing the unbounded wait this deadline exists to avoid.
    fn take(&self) -> Option<std::result::Result<(), String>> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut g = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(r) = g.take() {
                return Some(r);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            g = self.cv.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

/// Fires the callback whenever the system default device of `kind` changes.
/// Dropping it deregisters the platform listener and joins the worker, so the
/// callback provably cannot run once `drop` has returned.
pub struct DeviceChangeWatcher {
    fanout: Arc<Fanout>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl DeviceChangeWatcher {
    pub fn start(kind: DeviceKind, cb: Box<dyn Fn() + Send + 'static>) -> Result<DeviceChangeWatcher> {
        let fanout = Arc::new(Fanout::new());
        let ready = Arc::new(Handshake::new());
        let f = Arc::clone(&fanout);
        let r = Arc::clone(&ready);
        // Registration, service and deregistration all live on this one thread:
        // the Windows COM apartment is thread-affine, and joining the thread is
        // what makes "no callback after drop" a fact rather than a hope.
        let worker = std::thread::Builder::new()
            .name("audiohub-devwatch".to_string())
            .spawn(move || {
                let reg = match watch_imp::register(kind, Arc::clone(&f)) {
                    Ok(reg) => {
                        r.publish(Ok(()));
                        reg
                    }
                    Err(e) => {
                        r.publish(Err(format!("{e:#}")));
                        return;
                    }
                };
                while f.wait() {
                    cb();
                }
                watch_imp::unregister(reg);
            })
            .context("spawn device watcher thread")?;

        match ready.take() {
            Some(Ok(())) => Ok(DeviceChangeWatcher { fanout, worker: Some(worker) }),
            Some(Err(msg)) => {
                // The worker already returned, so this join is immediate.
                fanout.stop();
                let _ = worker.join();
                Err(anyhow!(msg))
            }
            None => {
                // Still stuck in registration: ask it to unwind and let it go
                // rather than block the caller for however long that takes.
                fanout.stop();
                drop(worker);
                Err(anyhow!("device watcher did not register before the deadline"))
            }
        }
    }
}

impl Drop for DeviceChangeWatcher {
    fn drop(&mut self) {
        self.fanout.stop();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

#[cfg(target_os = "macos")]
mod watch_imp {
    //! AudioObjectAddPropertyListener on the system object's
    //! kAudioHardwarePropertyDefaultInput/OutputDevice.

    use super::{DeviceKind, Fanout};
    use anyhow::{bail, Result};
    use std::ffi::c_void;
    use std::sync::Arc;

    type OSStatus = i32;
    type AudioObjectID = u32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PropAddr {
        selector: u32,
        scope: u32,
        element: u32,
    }

    const SYSTEM_OBJECT: AudioObjectID = 1;
    const ELEM_MAIN: u32 = 0;

    const fn fourcc(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }
    const SEL_DEFAULT_INPUT: u32 = fourcc(b"dIn "); // kAudioHardwarePropertyDefaultInputDevice
    const SEL_DEFAULT_OUTPUT: u32 = fourcc(b"dOut"); // kAudioHardwarePropertyDefaultOutputDevice
    const SCOPE_GLOBAL: u32 = fourcc(b"glob");

    type ListenerProc =
        unsafe extern "C" fn(AudioObjectID, u32, *const PropAddr, *mut c_void) -> OSStatus;

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectAddPropertyListener(
            id: AudioObjectID,
            addr: *const PropAddr,
            listener: ListenerProc,
            data: *mut c_void,
        ) -> OSStatus;
        fn AudioObjectRemovePropertyListener(
            id: AudioObjectID,
            addr: *const PropAddr,
            listener: ListenerProc,
            data: *mut c_void,
        ) -> OSStatus;
    }

    /// Raw pointer inside: created and consumed on the watcher thread only.
    pub struct Registration {
        addr: PropAddr,
        ctx: *const Fanout,
    }

    unsafe extern "C" fn on_change(
        _id: AudioObjectID,
        _n: u32,
        _addrs: *const PropAddr,
        data: *mut c_void,
    ) -> OSStatus {
        if !data.is_null() {
            (*(data as *const Fanout)).signal();
        }
        0
    }

    pub fn register(kind: DeviceKind, fanout: Arc<Fanout>) -> Result<Registration> {
        let selector = match kind {
            DeviceKind::Input => SEL_DEFAULT_INPUT,
            DeviceKind::Output => SEL_DEFAULT_OUTPUT,
        };
        let addr = PropAddr { selector, scope: SCOPE_GLOBAL, element: ELEM_MAIN };
        // The HAL keeps this pointer until the listener is removed, so the Arc
        // strong count has to stay raised for exactly that long.
        let ctx = Arc::into_raw(fanout);
        let st = unsafe {
            AudioObjectAddPropertyListener(SYSTEM_OBJECT, &addr, on_change, ctx as *mut c_void)
        };
        if st != 0 {
            unsafe { drop(Arc::from_raw(ctx)) };
            bail!("AudioObjectAddPropertyListener failed: OSStatus {st}");
        }
        Ok(Registration { addr, ctx })
    }

    pub fn unregister(reg: Registration) {
        let st = unsafe {
            AudioObjectRemovePropertyListener(
                SYSTEM_OBJECT,
                &reg.addr,
                on_change,
                reg.ctx as *mut c_void,
            )
        };
        if st == 0 {
            unsafe { drop(Arc::from_raw(reg.ctx)) };
        } else {
            // Removal failed, so the HAL may still hold the pointer: leaking one
            // Fanout beats handing it a dangling one.
            eprintln!("[audiohub] AudioObjectRemovePropertyListener failed: OSStatus {st}");
        }
    }
}

#[cfg(windows)]
mod watch_imp {
    //! A hand-rolled IMMNotificationClient registered on an MMDeviceEnumerator.
    //! Vtable layout is the frozen ABI of mmdeviceapi.h; the object is a normal
    //! refcounted COM object, so MMDevAPI's own reference keeps it alive for as
    //! long as it needs and the last Release frees it.

    use super::{DeviceKind, Fanout};
    use anyhow::{bail, Result};
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    type HRESULT = i32;
    const S_OK: HRESULT = 0;
    const E_POINTER: HRESULT = -2147467261; // 0x80004003
    const E_NOINTERFACE: HRESULT = -2147467262; // 0x80004002

    #[repr(C)]
    #[derive(Clone, Copy, PartialEq)]
    struct GUID {
        d1: u32,
        d2: u16,
        d3: u16,
        d4: [u8; 8],
    }

    const CLSID_MM_DEVICE_ENUMERATOR: GUID = GUID {
        d1: 0xBCDE0395,
        d2: 0xE52F,
        d3: 0x467C,
        d4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
    };
    const IID_IMM_DEVICE_ENUMERATOR: GUID = GUID {
        d1: 0xA95664D2,
        d2: 0x9614,
        d3: 0x4F35,
        d4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
    };
    const IID_IMM_NOTIFICATION_CLIENT: GUID = GUID {
        d1: 0x7991EEC9,
        d2: 0x7E89,
        d3: 0x4D85,
        d4: [0x83, 0x90, 0x6C, 0x70, 0x3C, 0xEC, 0x60, 0xC0],
    };
    const IID_IUNKNOWN: GUID =
        GUID { d1: 0, d2: 0, d3: 0, d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };

    const CLSCTX_INPROC_SERVER: u32 = 0x1;
    const COINIT_MULTITHREADED: u32 = 0x0;
    const E_RENDER: i32 = 0; // EDataFlow::eRender
    const E_CAPTURE: i32 = 1; // EDataFlow::eCapture
    const ROLE_CONSOLE: i32 = 0; // ERole::eConsole -- the role cpal and the
                                 // volume backend both resolve the default with

    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> HRESULT;
        fn CoUninitialize();
        fn CoCreateInstance(
            clsid: *const GUID,
            outer: *mut c_void,
            ctx: u32,
            iid: *const GUID,
            out: *mut *mut c_void,
        ) -> HRESULT;
    }

    #[repr(C)]
    struct IUnknownVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }

    #[repr(C)]
    struct IMMDeviceEnumeratorVtbl {
        base: IUnknownVtbl,
        enum_audio_endpoints: usize,
        get_default_audio_endpoint: usize,
        get_device: usize,
        register_endpoint_notification_callback:
            unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
        unregister_endpoint_notification_callback:
            unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PropertyKey {
        fmtid: GUID,
        pid: u32,
    }

    #[repr(C)]
    struct IMMNotificationClientVtbl {
        base: IUnknownVtbl,
        on_device_state_changed:
            unsafe extern "system" fn(*mut c_void, *const u16, u32) -> HRESULT,
        on_device_added: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        on_device_removed: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        on_default_device_changed:
            unsafe extern "system" fn(*mut c_void, i32, i32, *const u16) -> HRESULT,
        on_property_value_changed:
            unsafe extern "system" fn(*mut c_void, *const u16, PropertyKey) -> HRESULT,
    }

    #[repr(C)]
    struct NotifyClient {
        vtbl: *const IMMNotificationClientVtbl,
        refs: AtomicU32,
        fanout: Arc<Fanout>,
        flow: i32,
    }

    unsafe extern "system" fn nc_qi(
        this: *mut c_void,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        if out.is_null() {
            return E_POINTER;
        }
        if iid.is_null() {
            *out = ptr::null_mut();
            return E_POINTER;
        }
        let want = *iid;
        if want == IID_IUNKNOWN || want == IID_IMM_NOTIFICATION_CLIENT {
            nc_add_ref(this);
            *out = this;
            S_OK
        } else {
            *out = ptr::null_mut();
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn nc_add_ref(this: *mut c_void) -> u32 {
        (*(this as *mut NotifyClient)).refs.fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe extern "system" fn nc_release(this: *mut c_void) -> u32 {
        let c = this as *mut NotifyClient;
        let prev = (*c).refs.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            drop(Box::from_raw(c));
            0
        } else {
            prev - 1
        }
    }

    unsafe extern "system" fn nc_state(_t: *mut c_void, _id: *const u16, _s: u32) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn nc_added(_t: *mut c_void, _id: *const u16) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn nc_removed(_t: *mut c_void, _id: *const u16) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn nc_prop(
        _t: *mut c_void,
        _id: *const u16,
        _k: PropertyKey,
    ) -> HRESULT {
        S_OK
    }

    /// Runs on an MMDevAPI thread: only ever hands the news to the Fanout.
    unsafe extern "system" fn nc_default_changed(
        this: *mut c_void,
        flow: i32,
        role: i32,
        _id: *const u16,
    ) -> HRESULT {
        let c = &*(this as *const NotifyClient);
        if flow == c.flow && role == ROLE_CONSOLE {
            c.fanout.signal();
        }
        S_OK
    }

    static NC_VTBL: IMMNotificationClientVtbl = IMMNotificationClientVtbl {
        base: IUnknownVtbl { query_interface: nc_qi, add_ref: nc_add_ref, release: nc_release },
        on_device_state_changed: nc_state,
        on_device_added: nc_added,
        on_device_removed: nc_removed,
        on_default_device_changed: nc_default_changed,
        on_property_value_changed: nc_prop,
    };

    /// Balances CoInitializeEx on the watcher thread. RPC_E_CHANGED_MODE means
    /// somebody else already picked the apartment; that one is fine to borrow
    /// and is not ours to tear down.
    struct Apartment {
        owned: bool,
    }

    impl Apartment {
        fn enter() -> Apartment {
            let hr = unsafe { CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED) };
            Apartment { owned: hr >= 0 }
        }
    }

    impl Drop for Apartment {
        fn drop(&mut self) {
            if self.owned {
                unsafe { CoUninitialize() };
            }
        }
    }

    unsafe fn release(p: *mut c_void) {
        if !p.is_null() {
            let v = *(p as *const *const IUnknownVtbl);
            ((*v).release)(p);
        }
    }

    /// Field order is drop order, and COM demands it: the enumerator has to go
    /// before the apartment it was created in.
    pub struct Registration {
        client: *mut NotifyClient,
        enumerator: *mut c_void,
        _apt: Apartment,
    }

    pub fn register(kind: DeviceKind, fanout: Arc<Fanout>) -> Result<Registration> {
        let apt = Apartment::enter();
        let mut enumerator: *mut c_void = ptr::null_mut();
        let hr = unsafe {
            CoCreateInstance(
                &CLSID_MM_DEVICE_ENUMERATOR,
                ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_IMM_DEVICE_ENUMERATOR,
                &mut enumerator,
            )
        };
        if hr < 0 {
            bail!("CoCreateInstance(MMDeviceEnumerator) failed: HRESULT 0x{:08X}", hr as u32);
        }
        let flow = match kind {
            DeviceKind::Input => E_CAPTURE,
            DeviceKind::Output => E_RENDER,
        };
        let client = Box::into_raw(Box::new(NotifyClient {
            vtbl: &NC_VTBL,
            refs: AtomicU32::new(1),
            fanout,
            flow,
        }));
        let hr = unsafe {
            let v = *(enumerator as *const *const IMMDeviceEnumeratorVtbl);
            ((*v).register_endpoint_notification_callback)(enumerator, client as *mut c_void)
        };
        if hr < 0 {
            unsafe {
                nc_release(client as *mut c_void);
                release(enumerator);
            }
            bail!("RegisterEndpointNotificationCallback failed: HRESULT 0x{:08X}", hr as u32);
        }
        Ok(Registration { client, enumerator, _apt: apt })
    }

    pub fn unregister(reg: Registration) {
        unsafe {
            let v = *(reg.enumerator as *const *const IMMDeviceEnumeratorVtbl);
            let hr = ((*v).unregister_endpoint_notification_callback)(
                reg.enumerator,
                reg.client as *mut c_void,
            );
            if hr < 0 {
                eprintln!(
                    "[audiohub] UnregisterEndpointNotificationCallback failed: HRESULT 0x{:08X}",
                    hr as u32
                );
            }
            // Our own reference goes last: MMDevAPI may hold one of its own and
            // whichever Release lands second is the one that frees the object.
            nc_release(reg.client as *mut c_void);
            release(reg.enumerator);
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod watch_imp {
    use super::{DeviceKind, Fanout};
    use anyhow::{bail, Result};
    use std::sync::Arc;

    pub struct Registration;

    pub fn register(_kind: DeviceKind, _fanout: Arc<Fanout>) -> Result<Registration> {
        bail!("default device change notifications are not implemented on this platform");
    }

    pub fn unregister(_reg: Registration) {}
}

#[cfg(test)]
impl DeviceChangeWatcher {
    /// Does exactly what the platform listener does, so a test can prove the
    /// hand-off without moving the machine's real default device.
    fn test_signal(&self) {
        self.fanout.signal();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// `unwrap_err` would demand Debug on the stream handles.
    fn err_of<T>(r: Result<T>) -> String {
        match r {
            Ok(_) => panic!("expected an error, got a live stream"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn dedup_keeps_first_occurrence_order() {
        let got = dedup_in_order(names(&["b", "a", "b", "", "c"]));
        assert_eq!(got, names(&["b", "a", "c"]));
    }

    #[test]
    fn prefix_match_is_case_insensitive() {
        let devs = names(&["BlackHole 2ch", "MacBook Pro Speakers"]);
        for q in ["BlackHole", "blackhole", "BLACKHOLE 2", "BlackHole 2ch"] {
            assert_eq!(
                resolve_name(&devs, q, DeviceKind::Output).unwrap(),
                "BlackHole 2ch",
                "query {q:?}"
            );
        }
    }

    #[test]
    fn exact_match_wins_over_a_longer_device_it_prefixes() {
        let devs = names(&["Loopback Audio", "Loopback Audio 2"]);
        assert_eq!(
            resolve_name(&devs, "Loopback Audio", DeviceKind::Output).unwrap(),
            "Loopback Audio"
        );
    }

    #[test]
    fn exact_match_is_case_insensitive_and_still_beats_prefix_siblings() {
        let devs = names(&["Loopback Audio", "Loopback Audio 2", "MMAudio Device", "MMAudio Device (UI Sounds)"]);
        for (q, want) in [
            ("loopback audio", "Loopback Audio"),
            ("LOOPBACK AUDIO", "Loopback Audio"),
            ("lOoPbAcK aUdIo", "Loopback Audio"),
            ("mmaudio device", "MMAudio Device"),
            ("  Loopback Audio  ", "Loopback Audio"), // trimmed, then exact
        ] {
            assert_eq!(
                resolve_name(&devs, q, DeviceKind::Output).unwrap(),
                want,
                "query {q:?}"
            );
        }
    }

    #[test]
    fn two_devices_with_one_name_are_ambiguous_not_a_coin_flip() {
        let raw = names(&["USB Audio", "MacBook Pro Speakers", "USB Audio"]);
        // presentation collapses them, resolution must not
        assert_eq!(dedup_in_order(raw.clone()), names(&["USB Audio", "MacBook Pro Speakers"]));
        for q in ["USB Audio", "usb audio", "USB"] {
            let e = err_of(resolve_name(&raw, q, DeviceKind::Output));
            assert!(e.contains("ambiguous"), "query {q:?}: {e}");
            assert!(e.contains("2 devices are named"), "query {q:?}: {e}");
        }
        // the unaffected sibling still resolves
        assert_eq!(
            resolve_name(&raw, "macbook", DeviceKind::Output).unwrap(),
            "MacBook Pro Speakers"
        );
    }

    #[test]
    fn case_only_duplicates_are_ambiguous_too() {
        let raw = names(&["Focusrite", "FOCUSRITE"]);
        let e = err_of(resolve_name(&raw, "focusrite", DeviceKind::Input));
        assert!(e.contains("ambiguous"), "{e}");
        assert!(e.contains("Focusrite") && e.contains("FOCUSRITE"), "{e}");
    }

    #[test]
    fn ambiguous_prefix_errors_and_lists_the_candidates() {
        let devs = names(&["BlackHole 2ch", "BlackHole 16ch", "ADAM Audio D3V"]);
        let e = err_of(resolve_name(&devs, "black", DeviceKind::Output));
        assert!(e.contains("ambiguous"), "{e}");
        assert!(e.contains("BlackHole 2ch") && e.contains("BlackHole 16ch"), "{e}");
        assert!(!e.contains("ADAM"), "{e}");
    }

    #[test]
    fn no_match_errors_and_never_resolves_to_something_else() {
        let devs = names(&["BlackHole 2ch", "MacBook Pro Speakers"]);
        let e = err_of(resolve_name(&devs, "VB-Cable", DeviceKind::Input));
        assert!(e.contains("no input device matches"), "{e}");
        assert!(e.contains("BlackHole 2ch"), "{e}");
        assert!(resolve_name(&devs, "  ", DeviceKind::Output).is_err());
        // a suffix is not a prefix
        assert!(resolve_name(&devs, "2ch", DeviceKind::Output).is_err());
    }

    // ---- reality checks against the machine running the tests

    #[test]
    fn every_listed_device_resolves_to_itself_and_opens_by_name() {
        for kind in [DeviceKind::Output, DeviceKind::Input] {
            let raw = devices::list_all(kind);
            for n in list_names(kind) {
                let twins = raw.iter().filter(|x| x.to_lowercase() == n.to_lowercase()).count();
                if twins > 1 {
                    // This machine really has two cards under one name: the only
                    // honest answer is a refusal, not one of them at random.
                    let e = err_of(resolve_name(&raw, &n, kind));
                    assert!(e.contains("ambiguous"), "{kind:?} {n:?}: {e}");
                    continue;
                }
                assert_eq!(resolve_name(&raw, &n, kind).unwrap(), n);
                // the same name in the wrong case must take the same fast path
                assert_eq!(resolve_name(&raw, &n.to_lowercase(), kind).unwrap(), n);
                let (_, resolved) = find_device(kind, &n)
                    .unwrap_or_else(|e| panic!("{kind:?} {n:?} not findable: {e:#}"));
                assert_eq!(resolved, n);
            }
        }
    }

    #[test]
    fn output_list_is_not_empty_and_holds_the_default() {
        let outs = list_output_devices();
        assert!(!outs.is_empty(), "no output devices at all");
        let rep = default_devices_report().unwrap();
        if let Some(d) = rep.default_output {
            assert!(outs.contains(&d), "default output {d:?} missing from {outs:?}");
        }
        if let Some(d) = rep.default_input {
            let ins = list_input_devices();
            assert!(ins.contains(&d), "default input {d:?} missing from {ins:?}");
        }
    }

    fn virtual_output() -> Option<String> {
        list_output_devices()
            .into_iter()
            .find(|n| n.to_lowercase().starts_with("blackhole"))
    }

    #[test]
    fn start_on_opens_the_named_virtual_card() {
        let Some(card) = virtual_output() else {
            eprintln!("[audiohub] skip: no BlackHole output on this machine");
            return;
        };
        // Prefix form is the interesting one: "BlackHole" must land on the real
        // "BlackHole 2ch". Nothing is pushed, so the card only sees silence.
        let (pb, mut tx) = LivePlayback::start_on("BlackHole", 48000).unwrap();
        tx.push(&[0.0f32; 480]);
        std::thread::sleep(Duration::from_millis(50));
        drop(pb);
        assert!(card.to_lowercase().starts_with("blackhole"));
    }

    #[test]
    fn start_on_refuses_an_unknown_name_instead_of_using_the_default() {
        let e = err_of(LivePlayback::start_on("No Such Device 9x", 48000));
        assert!(e.contains("no output device matches"), "{e}");
        let e = err_of(LiveCapture::start_on("No Such Device 9x"));
        assert!(e.contains("no input device matches"), "{e}");
    }

    #[test]
    fn start_on_refuses_a_device_that_cannot_do_that_direction() {
        let outs = list_output_devices();
        let Some(input_only) = list_input_devices().into_iter().find(|n| !outs.contains(n)) else {
            eprintln!("[audiohub] skip: every input device is also an output");
            return;
        };
        let e = err_of(LivePlayback::start_on(&input_only, 48000));
        assert!(e.contains("no output device matches"), "{input_only:?}: {e}");
    }

    // ---- stream health (the seam the daemon watches)

    #[test]
    fn health_starts_alive_and_death_is_one_way() {
        let h = StreamHealth::new();
        assert!(h.is_alive());
        assert!(h.take_error().is_none());

        h.fail("output", &cpal::StreamError::DeviceNotAvailable);
        assert!(!h.is_alive(), "a reported stream error must kill the stream");
        let first = h.take_error().expect("the cause is kept, not just printed");
        assert!(first.contains("output stream error"), "{first}");

        // taking the message must not resurrect the stream, and the cause is
        // handed out exactly once
        assert!(!h.is_alive());
        assert!(h.take_error().is_none());
    }

    #[test]
    fn health_keeps_the_first_cause_not_the_fallout() {
        let h = StreamHealth::new();
        h.fail("input", &cpal::StreamError::DeviceNotAvailable);
        h.fail(
            "input",
            &cpal::StreamError::BackendSpecific {
                err: cpal::BackendSpecificError { description: "later fallout".into() },
            },
        );
        let msg = h.take_error().unwrap();
        assert!(!msg.contains("later fallout"), "{msg}");
        assert!(!h.is_alive());
    }

    #[test]
    fn health_is_visible_across_threads() {
        let h = StreamHealth::new();
        let w = Arc::clone(&h);
        std::thread::spawn(move || w.fail("output", &cpal::StreamError::DeviceNotAvailable))
            .join()
            .unwrap();
        assert!(!h.is_alive());
        assert!(h.take_error().unwrap().contains("stream error"));
    }

    #[test]
    fn a_freshly_opened_stream_reports_itself_alive() {
        let Some(card) = virtual_output() else {
            eprintln!("[audiohub] skip: no BlackHole output on this machine");
            return;
        };
        // Silence into a virtual card: nothing audible, no device state touched.
        let (pb, mut tx) = LivePlayback::start_on(&card, 48000).unwrap();
        tx.push(&[0.0f32; 480]);
        std::thread::sleep(Duration::from_millis(50));
        assert!(pb.is_alive(), "a healthy stream must not look dead");
        assert!(pb.take_error().is_none());
        drop(pb);
    }

    // ---- watcher

    #[test]
    fn fanout_coalesces_and_stops() {
        let f = Fanout::new();
        f.signal();
        f.signal();
        assert!(f.wait());
        f.stop();
        assert!(!f.wait(), "stop must win once set");
    }

    #[test]
    fn watcher_registers_and_delivers_off_the_platform_thread() {
        for kind in [DeviceKind::Output, DeviceKind::Input] {
            let (tx, rx) = mpsc::channel::<std::thread::ThreadId>();
            let w = DeviceChangeWatcher::start(
                kind,
                Box::new(move || {
                    let _ = tx.send(std::thread::current().id());
                }),
            )
            .unwrap_or_else(|e| panic!("{kind:?} watcher: {e:#}"));
            w.test_signal();
            let cb_thread = rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|e| panic!("{kind:?} callback never ran: {e}"));
            assert_ne!(cb_thread, std::thread::current().id());
            drop(w); // deregisters + joins; a second drop path must not exist
        }
    }

    #[test]
    fn watcher_callback_cannot_run_after_drop() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        let w = DeviceChangeWatcher::start(
            DeviceKind::Output,
            Box::new(move || {
                h.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();
        w.test_signal();
        std::thread::sleep(Duration::from_millis(100));
        drop(w);
        let after = hits.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(hits.load(Ordering::SeqCst), after);
        assert!(after >= 1);
    }
}
