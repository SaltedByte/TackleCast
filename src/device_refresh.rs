use crate::devices::AudioDevice;
use crate::{capabilities, devices, settings::Settings};
use std::sync::mpsc::{self, Receiver, TryRecvError};

pub struct DiscoveredDevices {
    pub settings: Settings,
    pub videos: Vec<String>,
    pub inputs: Vec<AudioDevice>,
    pub outputs: Vec<AudioDevice>,
    pub video: String,
    pub capabilities: capabilities::DeviceCapabilities,
}

impl DiscoveredDevices {
    pub fn scan(mut settings: Settings) -> Self {
        let videos = devices::enumerate_video_devices();
        let inputs = devices::enumerate_audio_inputs();
        let outputs = devices::enumerate_audio_outputs();
        devices::refresh_audio_choices(&mut settings, &inputs, &outputs);
        settings.video_device = devices::restore_video_device(&settings.video_device, &videos);
        let video = devices::select_capture_devices(
            &settings.video_device,
            settings.audio_input,
            &videos,
            &inputs,
        )
        .video_device;
        let capabilities = if video.is_empty() {
            Default::default()
        } else {
            capabilities::query_device_capabilities(&video)
        };
        // Preserve the requested mode if it is still available; never force the highest mode.
        capabilities::sanitize_draft_settings(&mut settings, &capabilities);
        Self {
            settings,
            videos,
            inputs,
            outputs,
            video,
            capabilities,
        }
    }
}

enum Phase {
    Stopping(Box<Settings>),
    Discovering(Receiver<DiscoveredDevices>),
    Finished,
}

/// A scan cannot begin until the old capture worker has actually retired.
/// Its result is the only path that permits capture/audio to be reopened.
pub struct DeviceRefresh {
    phase: Phase,
    pub generation: u64,
}

impl DeviceRefresh {
    #[cfg(all(test, target_os = "windows"))]
    pub(crate) fn from_receiver(generation: u64, receiver: Receiver<DiscoveredDevices>) -> Self {
        Self {
            generation,
            phase: Phase::Discovering(receiver),
        }
    }
    pub fn new(settings: Settings, generation: u64) -> Self {
        Self {
            phase: Phase::Stopping(Box::new(settings)),
            generation,
        }
    }

    pub fn poll(
        &mut self,
        capture_retired: bool,
        discover: impl FnOnce(Settings) -> DiscoveredDevices + Send + 'static,
    ) -> Option<Result<DiscoveredDevices, String>> {
        match &self.phase {
            Phase::Stopping(_) if !capture_retired => None,
            Phase::Stopping(settings) => {
                let settings = (**settings).clone();
                let (sender, receiver) = mpsc::channel();
                self.phase = Phase::Discovering(receiver);
                std::thread::spawn(move || {
                    let _ = sender.send(discover(settings));
                });
                None
            }
            Phase::Discovering(receiver) => match receiver.try_recv() {
                Ok(devices) => {
                    self.phase = Phase::Finished;
                    Some(Ok(devices))
                }
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    self.phase = Phase::Finished;
                    Some(Err(
                        "Device discovery failed. Reinitialize devices to retry.".into(),
                    ))
                }
            },
            Phase::Finished => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    #[test]
    fn rediscovery_waits_for_retirement_and_rebuilds_once_without_frames() {
        let mut settings = Settings {
            audio_input: 7,
            audio_output: 8,
            ..Default::default()
        };
        let old_inputs = vec![AudioDevice {
            index: 7,
            name: "ShadowCast 3".into(),
        }];
        let old_outputs = vec![AudioDevice {
            index: 8,
            name: "Speakers".into(),
        }];
        devices::remember_audio_choices(&mut settings, &old_inputs, &old_outputs);
        let count = Arc::new(AtomicUsize::new(0));
        let mut task = DeviceRefresh::new(settings, 1);
        for _ in 0..3 {
            assert!(task
                .poll(false, |_| panic!("driver has not released the old stream"))
                .is_none());
        }
        let calls = count.clone();
        assert!(task
            .poll(true, move |mut settings| {
                calls.fetch_add(1, Ordering::SeqCst);
                let inputs = vec![
                    AudioDevice {
                        index: 7,
                        name: "Headset".into(),
                    },
                    AudioDevice {
                        index: 2,
                        name: "ShadowCast 3".into(),
                    },
                ];
                let outputs = vec![AudioDevice {
                    index: 3,
                    name: "Speakers".into(),
                }];
                devices::refresh_audio_choices(&mut settings, &inputs, &outputs);
                DiscoveredDevices {
                    settings,
                    videos: vec!["ShadowCast 3".into()],
                    inputs,
                    outputs,
                    video: "ShadowCast 3".into(),
                    capabilities: capabilities::DeviceCapabilities {
                        modes: vec![capabilities::CaptureMode {
                            width: 3840,
                            height: 2160,
                            format: crate::settings::VideoFormat::Mjpeg,
                            rate: 60.into(),
                        }],
                        ..Default::default()
                    },
                }
            })
            .is_none());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let discovered = loop {
            if let Some(result) = task.poll(true, |_| panic!("must not scan twice")) {
                break result.unwrap();
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        };
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(
            (
                discovered.settings.audio_input,
                discovered.settings.audio_output
            ),
            (2, 3)
        );
        assert_eq!(discovered.capabilities.modes[0].width, 3840);
        assert!(task
            .poll(true, |_| panic!("completed scan cannot reopen again"))
            .is_none());
    }
    #[test]
    fn removed_or_ambiguous_explicit_audio_never_becomes_an_unrelated_index() {
        let mut settings = Settings {
            audio_input: 2,
            audio_output: 3,
            ..Default::default()
        };
        devices::remember_audio_choices(
            &mut settings,
            &[AudioDevice {
                index: 2,
                name: "Capture".into(),
            }],
            &[AudioDevice {
                index: 3,
                name: "Speakers".into(),
            }],
        );
        devices::refresh_audio_choices(
            &mut settings,
            &[AudioDevice {
                index: 2,
                name: "Headset".into(),
            }],
            &[],
        );
        assert_eq!(settings.audio_input, devices::UNAVAILABLE_AUDIO_INDEX);
        assert_eq!(settings.audio_output, devices::UNAVAILABLE_AUDIO_INDEX);
        let saved = serde_json::to_string(&settings).unwrap();
        settings = serde_json::from_str(&saved).unwrap();
        devices::refresh_audio_choices(
            &mut settings,
            &[
                AudioDevice {
                    index: 4,
                    name: "Capture".into(),
                },
                AudioDevice {
                    index: 5,
                    name: "Capture".into(),
                },
            ],
            &[],
        );
        assert_eq!(settings.audio_input, devices::UNAVAILABLE_AUDIO_INDEX);
        settings.audio_input = 4;
        devices::validate_audio_choices(
            &mut settings,
            &[
                AudioDevice {
                    index: 4,
                    name: "Capture".into(),
                },
                AudioDevice {
                    index: 5,
                    name: "Capture".into(),
                },
            ],
            &[],
        );
        assert_eq!(
            settings.audio_input, 4,
            "a fresh explicit menu choice is usable even when names are identical"
        );
        devices::refresh_audio_choices(
            &mut settings,
            &[AudioDevice {
                index: 6,
                name: "Capture".into(),
            }],
            &[AudioDevice {
                index: 9,
                name: "Speakers".into(),
            }],
        );
        assert_eq!((settings.audio_input, settings.audio_output), (6, 9));
        let mut automatic = Settings::default();
        devices::refresh_audio_choices(&mut automatic, &[], &[]);
        assert_eq!((automatic.audio_input, automatic.audio_output), (-1, -1));
    }
}
