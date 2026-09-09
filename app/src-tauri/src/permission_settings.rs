/// Fixed macOS destinations. Never accept a daemon-provided custom URL here.
pub fn destination(id: &str) -> Result<(&'static str, bool), String> {
    match id {
        "microphone" => Ok((
            "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Microphone",
            true,
        )),
        "system_audio" => Ok((
            "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AudioCapture",
            true,
        )),
        // Tahoe does not resolve the LocalNetwork anchor to its detail page.
        // Open the containing pane and explicitly retain the navigation hint.
        "local_network" => Ok((
            "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension",
            false,
        )),
        _ => Err("Unsupported permission settings destination".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::destination;

    #[test]
    fn known_permissions_have_fixed_destinations() {
        let (mic, direct) = destination("microphone").unwrap();
        assert!(direct && mic.ends_with("?Privacy_Microphone"));
        let (audio, direct) = destination("system_audio").unwrap();
        assert!(direct && audio.ends_with("?Privacy_AudioCapture"));
        let (network, direct) = destination("local_network").unwrap();
        assert!(!direct && !network.contains('?'));
    }

    #[test]
    fn arbitrary_schemes_paths_and_permission_names_are_rejected() {
        for id in ["", "camera", "file:///etc/passwd", "https://example.test", "microphone; echo bad", "--args", "MICROPHONE"] {
            assert!(destination(id).is_err(), "accepted {id}");
        }
    }
}
