use super::{NativeOutputCapabilities, OutputMixFormat, SpatialAudioCapabilities};
#[allow(deprecated)]
use objc2_core_audio::{
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyStreamFormat,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsSignedInteger, kAudioFormatLinearPCM,
    AudioStreamBasicDescription,
};
use std::ffi::{c_char, c_void, CStr};
use std::mem::{size_of, MaybeUninit};
use std::ptr::{self, NonNull};

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringGetLength(value: *const c_void) -> isize;
    fn CFStringGetCString(
        value: *const c_void,
        buffer: *mut c_char,
        size: isize,
        encoding: u32,
    ) -> bool;
    fn CFRelease(value: *const c_void);
}

struct OwnedString(*const c_void);
impl Drop for OwnedString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

fn property<T: Copy>(device: AudioObjectID, selector: u32, scope: u32) -> Result<T, String> {
    let mut address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size = size_of::<T>() as u32;
    let mut value = MaybeUninit::<T>::uninit();
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&mut address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    if status != 0 || size as usize != size_of::<T>() {
        return Err(format!(
            "CoreAudio output property 0x{selector:08X} failed: {status}"
        ));
    }
    Ok(unsafe { value.assume_init() })
}

fn default_id() -> Result<AudioObjectID, String> {
    property(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
    )
}

fn query_inner() -> Result<NativeOutputCapabilities, String> {
    let device = default_id()?;
    if device == 0 {
        return Ok(NativeOutputCapabilities::Unavailable);
    }
    let uid = OwnedString(property(
        device,
        kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal,
    )?);
    if uid.0.is_null() {
        return Err("CoreAudio returned no default output UID".into());
    }
    let length = unsafe { CFStringGetLength(uid.0) };
    if !(1..=1024).contains(&length) {
        return Err("CoreAudio returned an invalid default output UID length".into());
    }
    let mut bytes = [0i8; 4097];
    if !unsafe { CFStringGetCString(uid.0, bytes.as_mut_ptr(), bytes.len() as isize, 0x08000100) } {
        return Err("CoreAudio default output UID is not valid UTF-8".into());
    }
    let endpoint_id = unsafe { CStr::from_ptr(bytes.as_ptr()) }
        .to_str()
        .map_err(|_| "CoreAudio default output UID is not UTF-8")?
        .to_owned();
    #[allow(deprecated)]
    let format: AudioStreamBasicDescription = property(
        device,
        kAudioDevicePropertyStreamFormat,
        kAudioObjectPropertyScopeOutput,
    )?;
    if format.mFormatID != kAudioFormatLinearPCM
        || !format.mSampleRate.is_finite()
        || format.mSampleRate.fract() != 0.0
        || format.mChannelsPerFrame > u16::MAX as u32
    {
        return Err("CoreAudio default output format is not a supported PCM observation".into());
    }
    let kind = if format.mFormatFlags & kAudioFormatFlagIsFloat != 0 {
        "f"
    } else if format.mFormatFlags & kAudioFormatFlagIsSignedInteger != 0 {
        "i"
    } else {
        "u"
    };
    let facts = NativeOutputCapabilities::Available {
        endpoint_id,
        mix_format: OutputMixFormat {
            sample_rate: format.mSampleRate as u32,
            channels: format.mChannelsPerFrame as u16,
            sample_format: format!("{kind}{}", format.mBitsPerChannel),
            channel_mask: None,
        },
        // Decoder availability and ordinary channel layouts do not establish
        // that this output currently runs Apple's Dolby spatial renderer.
        spatial_audio: SpatialAudioCapabilities::Unknown,
    };
    if default_id()? != device {
        return Err("CoreAudio default output changed during capability query".into());
    }
    if !facts.is_valid() {
        return Err("CoreAudio returned out-of-range default output capabilities".into());
    }
    Ok(facts)
}

pub(super) fn query() -> NativeOutputCapabilities {
    query_inner().unwrap_or_else(|message| NativeOutputCapabilities::Error { message })
}
