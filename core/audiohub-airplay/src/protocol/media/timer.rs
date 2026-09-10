//! Windows timer resolution scoped to active media, never receiver idleness.
//!
//! Each successful request has one matching release. See Microsoft's
//! timeBeginPeriod contract; this does not make QPC itself more accurate.

pub(super) struct PlaybackTimer {
    #[cfg(target_os = "windows")]
    active: bool,
}

#[cfg(target_os = "windows")]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}

impl PlaybackTimer {
    pub(super) fn acquire() -> Self {
        #[cfg(target_os = "windows")]
        {
            let result = unsafe { timeBeginPeriod(1) };
            if result != 0 {
                log::warn!("AirPlay high-resolution media timer unavailable status={result}");
            }
            Self { active: result == 0 }
        }
        #[cfg(not(target_os = "windows"))]
        { Self {} }
    }
}

impl Drop for PlaybackTimer {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        if self.active {
            let result = unsafe { timeEndPeriod(1) };
            if result != 0 {
                log::warn!("AirPlay media timer release failed status={result}");
            }
        }
    }
}
