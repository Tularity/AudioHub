//! Non-RT, same-endpoint engine observations. Never initialize a probe stream.
//! One worker, at most 32 registrations; idle wait. A stuck COM call can retain
//! one worker and one observer client, but cannot delay stream close or spawn
//! replacement workers indefinitely. All streams then expire to fallback.
use std::sync::{Arc, Weak, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Media::Audio;
use windows::Win32::System::Com;

pub(crate) struct Observation {
    endpoint: Vec<u16>,
    rate: u32,
    base: Instant,
    // Single writer; a bounded reader rejects an overlapping publication.
    version: AtomicU64,
    seconds: AtomicU64,
    nanos_frames: AtomicU64,
    closed: AtomicBool,
}
impl Observation {
    pub(crate) fn close(&self) { self.closed.store(true, Ordering::Release); }
    pub(crate) fn read(&self) -> Option<(Instant, u32)> {
        if self.closed.load(Ordering::Acquire) { return None; }
        let version = self.version.load(Ordering::SeqCst);
        if version & 1 != 0 { return None; }
        let seconds = self.seconds.load(Ordering::SeqCst);
        let p = self.nanos_frames.load(Ordering::SeqCst);
        if version != self.version.load(Ordering::SeqCst)
            || self.closed.load(Ordering::Acquire) { return None; }
        let frames = p as u32;
        let at = self.base.checked_add(Duration::new(seconds, (p >> 32) as u32))?;
        let age = Instant::now().checked_duration_since(at)?;
        (frames > 0 && age <= Duration::from_millis(250)).then_some((at, frames))
    }
    fn publish(&self, at: Duration, frames: u32) {
        // SeqCst gives all fields and both version checks one total order.
        // No callback spins, locks, or waits for this single worker.
        self.version.fetch_add(1, Ordering::SeqCst);
        self.seconds.store(at.as_secs(), Ordering::SeqCst);
        self.nanos_frames.store(((at.subsec_nanos() as u64) << 32) | frames as u64, Ordering::SeqCst);
        self.version.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Default)]
struct Registry {
    running: bool,
    thread: Option<std::thread::JoinHandle<()>>,
    slots: Vec<Weak<Observation>>,
}
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

pub(crate) fn register(device: &Audio::IMMDevice, rate: u32) -> Option<Arc<Observation>> {
    // Called only by the non-RT stream construction path. Identity comes from
    // the very IMMDevice used to initialize the real render client.
    let endpoint = unsafe {
        let id = device.GetId().ok()?;
        let result = id.to_string();
        Com::CoTaskMemFree(Some(id.0.cast()));
        result.ok()?.encode_utf16().chain(Some(0)).collect()
    };
    let state = Arc::new(Observation { endpoint, rate, base: Instant::now(),
        version: AtomicU64::new(0), seconds: AtomicU64::new(0), nanos_frames: AtomicU64::new(0), closed: AtomicBool::new(false) });
    let mut registry = REGISTRY.get_or_init(Default::default).lock().ok()?;
    registry.slots.retain(|w| w.upgrade().is_some_and(|s| !s.closed.load(Ordering::Acquire)));
    if registry.slots.len() >= 32 { return None; }
    registry.slots.push(Arc::downgrade(&state));
    if !registry.running {
        registry.running = true;
        match std::thread::Builder::new().name("cpal_engine_period".into()).spawn(observe) {
            Ok(thread) => registry.thread = Some(thread),
            Err(_) => {
                registry.running = false;
                registry.slots.clear();
                return None;
            }
        }
    }
    Some(state)
}

fn observe() {
    loop {
        let slots = {
            let mut registry = REGISTRY.get().unwrap().lock().unwrap();
            registry.slots.retain(|w| w.upgrade().is_some_and(|s| !s.closed.load(Ordering::Acquire)));
            // Keep the sole worker alive across idle and recoverable COM failures.
            registry.slots.clone()
        };
        for weak in slots {
            let Some(state) = weak.upgrade() else { continue };
            if state.closed.load(Ordering::Acquire) { continue; }
            // Timestamp before all COM work; a slow call cannot rejuvenate data.
            let at = state.base.elapsed();
            let frames = std::panic::catch_unwind(|| {
                super::com::com_initialized();
                unsafe { query(&state.endpoint, state.rate) }
            }).ok().flatten().unwrap_or(0);
            if !state.closed.load(Ordering::Acquire) { state.publish(at, frames); }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

unsafe fn query(endpoint: &[u16], rate: u32) -> Option<u32> {
    let enumerator: Audio::IMMDeviceEnumerator =
        Com::CoCreateInstance(&Audio::MMDeviceEnumerator, None, Com::CLSCTX_ALL).ok()?;
    let device = enumerator.GetDevice(PCWSTR(endpoint.as_ptr())).ok()?;
    let client: Audio::IAudioClient = device.Activate(Com::CLSCTX_ALL, None).ok()?;
    let client: Audio::IAudioClient3 = client.cast().ok()?;
    let mut format = std::ptr::null_mut();
    let mut frames = 0;
    let result = client.GetCurrentSharedModeEnginePeriod(&mut format, &mut frames);
    // Free on both success and failure, including any partial result.
    let engine_rate = if format.is_null() { 0 } else { (*format).nSamplesPerSec };
    Com::CoTaskMemFree(Some(format.cast()));
    if result.is_err() { return None; }
    convert_frames(frames, engine_rate, rate)
}

fn convert_frames(frames: u32, engine_rate: u32, stream_rate: u32) -> Option<u32> {
    if frames == 0 || engine_rate == 0 || stream_rate == 0 { return None; }
    // Ceiling avoids understating a quantum after a sample-rate conversion.
    let converted = (frames as u64 * stream_rate as u64).div_ceil(engine_rate as u64);
    u32::try_from(converted).ok().filter(|f| *f > 0)
}
