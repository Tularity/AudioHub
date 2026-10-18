//! Regression test for the cached WASAPI device enumerator outliving the thread
//! that created it (vendor/cpal-0.16.0, `get_enumerator`).
//!
//! CPAL caches one `IMMDeviceEnumerator` for the whole process. Before the fix it
//! was created in the calling thread's apartment, which that thread's COM guard
//! uninitializes when the thread exits. With no other COM apartment left in the
//! process, COM then unloaded MMDevApi.dll and the next device listing read the
//! cached object's vtable from unmapped memory (STATUS_ACCESS_VIOLATION).
//!
//! This binary holds exactly this one test, so nothing else keeps COM alive
//! between the two listings: each runs on its own short-lived thread, the second
//! one after the first thread (the creator) has fully exited.

#[cfg(windows)]
#[test]
fn device_listing_survives_the_creating_thread_exiting() {
    for round in 0..3 {
        let listed = std::thread::spawn(audiohub_core::audio::list_output_devices)
            .join()
            .unwrap_or_else(|_| panic!("device listing panicked in round {round}"));
        // The count is machine dependent (a test VM may have no endpoint at all);
        // reaching this line without an access violation is the assertion.
        let _ = listed.len();
    }
}
