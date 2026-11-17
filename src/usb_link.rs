#[cfg(target_os = "windows")]
extern "C" {
    fn tacklecast_usb_link(instance: *const u16) -> i32;
}

#[cfg(target_os = "windows")]
fn instance_id(moniker: &str) -> Option<String> {
    let start = moniker.to_ascii_lowercase().find("usb#")?;
    let value = moniker.get(start..)?.split("#{").next()?;
    let parts: Vec<_> = value.split('#').collect();
    (parts.len() == 3 && !parts[2].is_empty()).then(|| parts.join("\\"))
}

#[cfg(target_os = "windows")]
pub fn warning(moniker: &str) -> Option<String> {
    warning_for_speed(connection_speed(moniker)?)
}

#[cfg(target_os = "windows")]
fn connection_speed(moniker: &str) -> Option<i32> {
    let id = instance_id(moniker)?;
    let wide: Vec<_> = id.encode_utf16().chain([0]).collect();
    let speed = unsafe { tacklecast_usb_link(wide.as_ptr()) };
    (1..=5).contains(&speed).then_some(speed)
}

pub(crate) fn warning_for_speed(speed: i32) -> Option<String> {
    let label = match speed {
        1 => "USB Low-Speed (1.5 Mb/s)",
        2 => "USB Full-Speed (12 Mb/s)",
        3 => "USB 2.0 High-Speed (480 Mb/s)",
        _ => return None,
    };
    Some(format!("{label}: connection speed may limit available capture modes. For a USB 3 card, use a USB 3 data cable/port, then Reinitialize devices."))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires a connected ShadowCast 3; read-only USB query"]
    fn hardware_selected_usb_device_reports_its_actual_link() {
        let videos = crate::devices::enumerate_video_devices();
        let card = videos
            .iter()
            .find(|id| crate::devices::video_device_label(id) == "ShadowCast 3")
            .expect("connected ShadowCast 3");
        let speed =
            connection_speed(card).expect("exact PnP instance must resolve to a hub connection");
        if let Ok(expected) = std::env::var("TACKLECAST_EXPECT_USB_SPEED") {
            assert_eq!(speed.to_string(), expected);
        }
        assert_eq!(warning(card).is_some(), speed <= 3);
        eprintln!(
            "Selected ShadowCast USB speed class: {speed}; 3=USB2, 4=SuperSpeed, 5=SuperSpeedPlus"
        );
    }
    #[test]
    fn only_confirmed_slow_links_warn() {
        assert!(warning_for_speed(3).unwrap().contains("480 Mb/s"));
        assert!(warning_for_speed(2).is_some());
        for unknown_or_fast in [0, 4, 5, -1] {
            assert!(warning_for_speed(unknown_or_fast).is_none());
        }
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn selected_moniker_keeps_the_exact_pnp_instance() {
        let moniker = r"@device_pnp_\\?\usb#vid_32ed&pid_3701&mi_00#9&1234&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global";
        assert_eq!(
            instance_id(moniker).as_deref(),
            Some(r"usb\vid_32ed&pid_3701&mi_00\9&1234&0&0000")
        );
        assert!(instance_id("ShadowCast 3").is_none());
        assert!(instance_id(r"@device_sw_{virtual}").is_none());
    }
}
