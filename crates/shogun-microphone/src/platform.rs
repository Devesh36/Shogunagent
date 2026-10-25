//! Live input-device discovery (feature `desktop`, E25): CPAL's CoreAudio list on macOS, and the
//! same "unavailable" answer as the default build everywhere else.

#[cfg(target_os = "macos")]
pub(crate) fn platform_input_device_names() -> Result<Vec<String>, String> {
    use cpal::traits::{DeviceTrait, HostTrait};

    cpal::default_host()
        .input_devices()
        .map_err(|error| format!("could not list microphone devices: {error}"))
        .map(|devices| {
            devices
                .filter_map(|device| device.description().ok())
                .map(|description| description.name().to_owned())
                .collect()
        })
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn platform_input_device_names() -> Result<Vec<String>, String> {
    Err(crate::UNAVAILABLE_OFF_MACOS.to_string())
}
