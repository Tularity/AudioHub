use super::*;
use crate::spatial_output::{
    SpatialOutput, SpatialOutputConfig, SpatialOutputState, SpeakerLayout, SpeakerPosition,
    SPATIAL_QUEUE_FRAMES, SPATIAL_SAMPLE_RATE,
};
use ringbuf::traits::{Consumer, Observer, Split};
use ringbuf::{HeapCons, HeapRb};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const MAX_RENDERERS: usize = 16;
const MAX_QUANTUM_FRAMES: usize = 4096;
static ACTIVE_RENDERERS: AtomicUsize = AtomicUsize::new(0);
const IID_SPATIAL_STREAM: Guid = Guid::new(
    0xBAB5F473,
    0xB423,
    0x477B,
    [0x85, 0xF5, 0xB5, 0xA3, 0x32, 0xA0, 0x41, 0x53],
);

#[repr(C, packed(1))]
struct Activation {
    format: *const WaveFormatEx,
    static_mask: u32,
    min_dynamic_objects: u32,
    max_dynamic_objects: u32,
    category: u32,
    event: *mut c_void,
    notify: *mut c_void,
}

#[repr(C)]
struct BlobVariant {
    vt: u16,
    reserved: [u16; 3],
    bytes: u32,
    #[cfg(target_pointer_width = "64")]
    padding: u32,
    data: *const c_void,
}

#[repr(C)]
struct StreamVtbl {
    base: IUnknownVtbl,
    get_available_dynamic_object_count: usize,
    get_service: usize,
    start: unsafe extern "system" fn(*mut c_void) -> Hresult,
    stop: unsafe extern "system" fn(*mut c_void) -> Hresult,
    reset: unsafe extern "system" fn(*mut c_void) -> Hresult,
    begin: unsafe extern "system" fn(*mut c_void, *mut u32, *mut u32) -> Hresult,
    end: unsafe extern "system" fn(*mut c_void) -> Hresult,
    activate_object: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> Hresult,
}

#[repr(C)]
struct ObjectVtbl {
    base: IUnknownVtbl,
    get_buffer: unsafe extern "system" fn(*mut c_void, *mut *mut u8, *mut u32) -> Hresult,
    set_end_of_stream: usize,
    is_active: usize,
    get_audio_object_type: usize,
    set_position: usize,
    set_volume: usize,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateEventW(
        attributes: *const c_void,
        manual: i32,
        initial: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
}

struct Event(*mut c_void);
impl Drop for Event {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

struct Reservation;
impl Drop for Reservation {
    fn drop(&mut self) {
        ACTIVE_RENDERERS.fetch_sub(1, Ordering::Release);
    }
}

fn object_type(position: SpeakerPosition) -> u32 {
    use SpeakerPosition::*;
    1 << match position {
        FrontLeft => 1,
        FrontRight => 2,
        FrontCenter => 3,
        LowFrequency => 4,
        SideLeft => 5,
        SideRight => 6,
        BackLeft => 7,
        BackRight => 8,
        TopFrontLeft => 9,
        TopFrontRight => 10,
        TopBackLeft => 11,
        TopBackRight => 12,
    }
}

fn static_mask(layout: SpeakerLayout) -> u32 {
    layout
        .positions()
        .iter()
        .fold(0, |mask, position| mask | object_type(*position))
}

struct NativeStream {
    objects: Vec<ComPtr>,
    stream: ComPtr,
    event: Event,
    started: bool,
    updating: bool,
    reset_done: bool,
}

impl NativeStream {
    fn finish_stop(&mut self) -> Result<(), ScanError> {
        if self.started {
            let hr = unsafe { ((*self.stream.vtbl::<StreamVtbl>()).stop)(self.stream.0) };
            check(hr, "ISpatialAudioObjectRenderStream::Stop")?;
            self.started = false;
        }
        check(
            unsafe { ((*self.stream.vtbl::<StreamVtbl>()).reset)(self.stream.0) },
            "ISpatialAudioObjectRenderStream::Reset",
        )?;
        self.reset_done = true;
        Ok(())
    }

    fn finish_update(&mut self) -> Result<(), ScanError> {
        let hr = unsafe { ((*self.stream.vtbl::<StreamVtbl>()).end)(self.stream.0) };
        self.updating = false;
        check(
            hr,
            "ISpatialAudioObjectRenderStream::EndUpdatingAudioObjects",
        )
    }
}

impl Drop for NativeStream {
    fn drop(&mut self) {
        unsafe {
            if self.updating {
                ((*self.stream.vtbl::<StreamVtbl>()).end)(self.stream.0);
            }
            if self.started {
                ((*self.stream.vtbl::<StreamVtbl>()).stop)(self.stream.0);
            }
            if !self.reset_done {
                ((*self.stream.vtbl::<StreamVtbl>()).reset)(self.stream.0);
            }
        }
    }
}

fn open_native(config: &SpatialOutputConfig) -> Result<NativeStream, ScanError> {
    let enumerator = create_enumerator()?;
    let device = default_render_endpoint(&enumerator)?;
    let id = endpoint_id(&device)?;
    if !id.eq_ignore_ascii_case(&config.endpoint_id) {
        return Err(ScanError::Message(
            "default output differs from accepted spatial endpoint",
        ));
    }
    let Some((Some(active), _)) = winrt_formats(&id)? else {
        return Err(ScanError::Message(
            "default output has no confirmed active spatial format",
        ));
    };
    if !active.eq_ignore_ascii_case(&config.active_format) {
        return Err(ScanError::Message(
            "active spatial format differs from accepted contract",
        ));
    }
    let client = activate(
        &device,
        &IID_ISPATIAL_AUDIO_CLIENT,
        "IMMDevice::Activate(ISpatialAudioClient)",
    )?;
    let v = unsafe { client.vtbl::<ISpatialAudioClientVtbl>() };
    let mut supported_mask = 0;
    check(
        unsafe { ((*v).get_native_static_object_type_mask)(client.0, &mut supported_mask) },
        "ISpatialAudioClient::GetNativeStaticObjectTypeMask",
    )?;
    let mask = static_mask(config.layout);
    if mask & supported_mask != mask {
        return Err(ScanError::Message(
            "native spatial renderer does not support requested speaker positions",
        ));
    }
    let format = WaveFormatEx {
        format_tag: WAVE_FORMAT_IEEE_FLOAT,
        channels: 1,
        samples_per_sec: SPATIAL_SAMPLE_RATE,
        avg_bytes_per_sec: SPATIAL_SAMPLE_RATE * 4,
        block_align: 4,
        bits_per_sample: 32,
        extra_size: 0,
    };
    let hr = unsafe { ((*v).is_audio_object_format_supported)(client.0, &format) };
    check(hr, "ISpatialAudioClient::IsAudioObjectFormatSupported")?;
    if hr != 0 {
        return Err(ScanError::Message(
            "native spatial renderer did not accept exact PCM format",
        ));
    }
    let event = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
    if event.is_null() {
        return Err(ScanError::Message("could not create spatial render event"));
    }
    let event = Event(event);
    let params = Activation {
        format: &format,
        static_mask: mask,
        min_dynamic_objects: 0,
        max_dynamic_objects: 0,
        category: 0,
        event: event.0,
        notify: ptr::null_mut(),
    };
    let variant = BlobVariant {
        vt: 65,
        reserved: [0; 3],
        bytes: mem::size_of::<Activation>() as u32,
        #[cfg(target_pointer_width = "64")]
        padding: 0,
        data: (&params as *const Activation).cast(),
    };
    let mut stream = ComPtr::null();
    check(
        unsafe {
            ((*v).activate_spatial_audio_stream)(
                client.0,
                (&variant as *const BlobVariant).cast(),
                &IID_SPATIAL_STREAM,
                &mut stream.0,
            )
        },
        "ISpatialAudioClient::ActivateSpatialAudioStream",
    )?;
    if stream.0.is_null() {
        return Err(ScanError::Message(
            "native spatial activation returned no stream",
        ));
    }
    let mut native = NativeStream {
        objects: Vec::with_capacity(config.layout.channels()),
        stream,
        event,
        started: false,
        updating: false,
        reset_done: false,
    };
    if !endpoint_id(&default_render_endpoint(&enumerator)?)?.eq_ignore_ascii_case(&id) {
        return Err(ScanError::Message(
            "default output changed during spatial stream activation",
        ));
    }
    if !matches!(winrt_formats(&id)?, Some((Some(ref current), _)) if current.eq_ignore_ascii_case(&config.active_format))
    {
        return Err(ScanError::Message(
            "spatial format changed during stream activation",
        ));
    }
    check(
        unsafe { ((*native.stream.vtbl::<StreamVtbl>()).start)(native.stream.0) },
        "ISpatialAudioObjectRenderStream::Start",
    )?;
    native.started = true;
    Ok(native)
}

fn render(
    config: SpatialOutputConfig,
    mut consumer: HeapCons<f32>,
    state: &SpatialOutputState,
    ready: &mpsc::SyncSender<Result<(), String>>,
) -> Result<(), ScanError> {
    let _apartment = Apartment::enter()?;
    let mut native = open_native(&config)?;
    let channels = config.layout.channels();
    let mut scratch = vec![0.0f32; MAX_QUANTUM_FRAMES * channels];
    let mut announced = false;
    let mut last_update = Instant::now();
    while !state.stop.load(Ordering::Acquire) {
        match unsafe { WaitForSingleObject(native.event.0, 100) } {
            0 => {}
            258 if last_update.elapsed() < Duration::from_secs(2) => continue,
            _ => {
                return Err(ScanError::Message(
                    "native spatial render event stalled or failed",
                ))
            }
        }
        let mut available = 0;
        let mut frame_count = 0;
        check(
            unsafe {
                ((*native.stream.vtbl::<StreamVtbl>()).begin)(
                    native.stream.0,
                    &mut available,
                    &mut frame_count,
                )
            },
            "ISpatialAudioObjectRenderStream::BeginUpdatingAudioObjects",
        )?;
        native.updating = true;
        let frames = frame_count as usize;
        if frames == 0 || frames > MAX_QUANTUM_FRAMES {
            return Err(ScanError::Message(
                "native spatial render quantum is out of bounds",
            ));
        }
        if native.objects.is_empty() {
            for position in config.layout.positions() {
                let mut object = ComPtr::null();
                check(
                    unsafe {
                        ((*native.stream.vtbl::<StreamVtbl>()).activate_object)(
                            native.stream.0,
                            object_type(*position),
                            &mut object.0,
                        )
                    },
                    "ISpatialAudioObjectRenderStream::ActivateSpatialAudioObject",
                )?;
                if object.0.is_null() {
                    return Err(ScanError::Message(
                        "native spatial object activation returned null",
                    ));
                }
                native.objects.push(object);
            }
        }
        let samples = &mut scratch[..frames * channels];
        samples.fill(0.0);
        let available_samples = (consumer.occupied_len() / channels * channels).min(samples.len());
        let read = consumer.pop_slice(&mut samples[..available_samples]);
        if read % channels != 0 {
            return Err(ScanError::Message(
                "spatial queue lost channel-frame alignment",
            ));
        }
        for (channel, object) in native.objects.iter().enumerate() {
            let mut buffer = ptr::null_mut();
            let mut bytes = 0;
            check(
                unsafe {
                    ((*object.vtbl::<ObjectVtbl>()).get_buffer)(object.0, &mut buffer, &mut bytes)
                },
                "ISpatialAudioObject::GetBuffer",
            )?;
            if buffer.is_null()
                || buffer as usize % mem::align_of::<f32>() != 0
                || bytes as usize != frames * 4
            {
                return Err(ScanError::Message(
                    "native spatial object buffer has unexpected size",
                ));
            }
            let output = unsafe { slice::from_raw_parts_mut(buffer.cast::<f32>(), frames) };
            for (frame, value) in output.iter_mut().enumerate() {
                *value = samples[frame * channels + channel];
            }
        }
        native.finish_update()?;
        state
            .rendered_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
        state
            .consumed_frames
            .fetch_add((read / channels) as u64, Ordering::Relaxed);
        state
            .underrun_frames
            .fetch_add((frames - read / channels) as u64, Ordering::Relaxed);
        if !announced {
            if ready.send(Ok(())).is_err() {
                break;
            }
            announced = true;
        }
        last_update = Instant::now();
    }
    native.finish_stop()
}

pub(crate) fn start_spatial_output(config: SpatialOutputConfig) -> anyhow::Result<SpatialOutput> {
    if config.endpoint_id.is_empty()
        || config.endpoint_id.len() > 1024
        || config.active_format.is_empty()
        || config.active_format.len() > 128
    {
        anyhow::bail!("invalid native spatial output contract");
    }
    ACTIVE_RENDERERS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < MAX_RENDERERS).then_some(count + 1)
        })
        .map_err(|_| anyhow::anyhow!("native spatial renderer limit reached"))?;
    let reservation = Reservation;
    let layout = config.layout;
    let (producer, consumer) = HeapRb::new(SPATIAL_QUEUE_FRAMES * layout.channels()).split();
    let state = Arc::new(SpatialOutputState::new());
    let worker_state = Arc::clone(&state);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("ahb-spatial-output".into())
        .spawn(move || {
            let reservation = reservation;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                render(config, consumer, &worker_state, &ready_tx)
            }))
            .unwrap_or(Err(ScanError::Message("native spatial renderer panicked")));
            if let Err(error) = result {
                let message = error.message();
                worker_state.fail(message.clone());
                let _ = ready_tx.try_send(Err(message));
            }
            drop(reservation);
            worker_state.closed.store(true, Ordering::Release);
        })?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(SpatialOutput {
            producer,
            state,
            layout,
        }),
        result => {
            state.stop.store(true, Ordering::Release);
            anyhow::bail!(
                "native spatial output did not start: {}",
                match result {
                    Ok(Err(message)) => message,
                    _ => "startup deadline or worker failure".into(),
                }
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "opens the native spatial renderer only in the dedicated Windows test VM"]
    fn native_static_layouts_in_test_vm() {
        assert_eq!(std::env::var("COMPUTERNAME").unwrap(), "WINAUDIO-ADM2");
        let config = SpatialOutputConfig {
            endpoint_id: "{0.0.0.00000000}.{87da73b7-0226-4ccc-909f-9d09cf5c5262}".into(),
            active_format: "{1459AC38-3875-49BF-BB59-0FE80F4D395D}".into(),
            layout: SpeakerLayout::Immersive714,
        };
        let mut wrong = config.clone();
        wrong.active_format = "{00000000-0000-0000-0000-000000000001}".into();
        assert!(start_spatial_output(wrong).is_err());
        for layout in [SpeakerLayout::Surround51, SpeakerLayout::Surround71] {
            let mut output = start_spatial_output(SpatialOutputConfig {
                layout,
                ..config.clone()
            })
            .unwrap();
            assert_eq!(
                output
                    .write_interleaved(&vec![0.0; layout.channels() * 480])
                    .unwrap(),
                480
            );
            output.shutdown().unwrap();
        }
        let mut output = start_spatial_output(config).unwrap();
        let deadline = Instant::now() + Duration::from_secs(25);
        let total_frames = 14 * SPATIAL_SAMPLE_RATE as usize;
        let mut queued = 0;
        let mut block = [0.0f32; 480 * 12];
        while queued < total_frames {
            assert!(
                Instant::now() < deadline,
                "spatial source exceeded deadline"
            );
            block.fill(0.0);
            let requested = (total_frames - queued).min(480);
            for frame in 0..requested {
                let source_frame = queued + frame;
                if source_frame >= 96_000 {
                    let body_frame = source_frame - 96_000;
                    let channel = body_frame / 48_000;
                    let phase = body_frame % 48_000;
                    let fade = (phase.min(47_999 - phase) as f32 / 480.0).min(1.0);
                    let t = phase as f64 / 48_000.0;
                    let value = [80.0, 701.0, 1901.0]
                        .iter()
                        .map(|frequency| (std::f64::consts::TAU * frequency * t).sin())
                        .sum::<f64>()
                        / 3.0;
                    block[frame * 12 + channel] = value as f32 * 0.15 * fade;
                }
            }
            let written = output.write_interleaved(&block[..requested * 12]).unwrap();
            queued += written;
            if written == 0 {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        while output.queued_frames() != 0 {
            assert!(Instant::now() < deadline, "spatial queue did not drain");
            assert!(output.failure().is_none());
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(output.consumed_frames(), total_frames as u64);
        println!("layout=immersive_7_1_4 channels=12 submitted_frames={queued} consumed_frames={} rendered_frames={} underrun_frames={}",
            output.consumed_frames(), output.rendered_frames(), output.underrun_frames());
        output.shutdown().unwrap();
        assert_eq!(ACTIVE_RENDERERS.load(Ordering::Acquire), 0);
    }

    #[test]
    fn static_layout_maps_side_and_back_positions_independently() {
        assert_eq!(static_mask(SpeakerLayout::Surround51), 0x7E);
        assert_eq!(static_mask(SpeakerLayout::Surround71), 0x1FE);
        assert_eq!(static_mask(SpeakerLayout::Immersive714), 0x1FFE);
        assert_eq!(
            object_type(SpeakerLayout::Surround71.positions()[4]),
            1 << 7
        );
    }

    #[test]
    fn native_spatial_activation_uses_sdk_packing() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(mem::size_of::<Activation>(), 40);
            assert_eq!(mem::align_of::<Activation>(), 1);
            assert_eq!(mem::size_of::<BlobVariant>(), 24);
        }
    }
}
