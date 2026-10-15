use crate::settings::{ColorRange, ColorSpace};
use ffmpeg_next::{color, format};

/// Metadata describes the YUV samples handed to the renderer, independently
/// of their plane layout or the format originally requested from the card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorInfo {
    pub space: Option<ColorSpace>,
    pub range: Option<ColorRange>,
    pub default_range: ColorRange,
}

impl Default for ColorInfo {
    fn default() -> Self {
        Self {
            space: None,
            range: None,
            default_range: ColorRange::Limited,
        }
    }
}

impl ColorInfo {
    pub fn from_ffmpeg(space: color::Space, range: color::Range, pixel: format::Pixel) -> Self {
        let space = match space {
            color::Space::BT709 => Some(ColorSpace::Rec709),
            color::Space::BT470BG | color::Space::SMPTE170M => Some(ColorSpace::Bt601),
            color::Space::BT2020NCL => Some(ColorSpace::Bt2020),
            _ => None,
        };
        let range = match range {
            color::Range::MPEG => Some(ColorRange::Limited),
            color::Range::JPEG => Some(ColorRange::Full),
            _ if matches!(
                pixel,
                format::Pixel::YUVJ420P | format::Pixel::YUVJ422P | format::Pixel::YUVJ444P
            ) =>
            {
                Some(ColorRange::Full)
            }
            _ => None,
        };
        Self {
            space,
            range,
            ..Self::default()
        }
    }

    pub fn resolve_space(self, choice: ColorSpace, height: u32) -> ColorSpace {
        if choice != ColorSpace::Auto {
            choice
        } else {
            self.space.unwrap_or(if height >= 720 {
                ColorSpace::Rec709
            } else {
                ColorSpace::Bt601
            })
        }
    }

    pub fn resolve_range(self, choice: ColorRange) -> ColorRange {
        if choice != ColorRange::Auto {
            choice
        } else {
            self.range.unwrap_or(self.default_range)
        }
    }

    pub fn space_label(self, choice: ColorSpace, height: u32) -> String {
        let active = self.resolve_space(choice, height);
        if choice != ColorSpace::Auto {
            return active.to_string();
        }
        format!(
            "Auto ({}{})",
            active,
            if self.space.is_none() { " default" } else { "" }
        )
    }

    pub fn range_label(self, choice: ColorRange) -> String {
        let active = match self.resolve_range(choice) {
            ColorRange::Full => "Full",
            _ => "Limited",
        };
        if choice != ColorRange::Auto {
            return active.to_string();
        }
        format!(
            "Auto ({}{})",
            active,
            if self.range.is_none() { " default" } else { "" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_overrides_defaults_and_plane_layout_does_not_claim_detection() {
        let full_nv12 = ColorInfo::from_ffmpeg(
            color::Space::SMPTE170M,
            color::Range::JPEG,
            format::Pixel::NV12,
        );
        assert_eq!(
            full_nv12.resolve_space(ColorSpace::Auto, 2160),
            ColorSpace::Bt601
        );
        assert_eq!(full_nv12.range_label(ColorRange::Auto), "Auto (Full)");
        let unknown = ColorInfo::default();
        assert_eq!(
            unknown.space_label(ColorSpace::Auto, 1080),
            "Auto (Rec. 709 default)"
        );
        assert_eq!(
            unknown.range_label(ColorRange::Auto),
            "Auto (Limited default)"
        );
        assert_eq!(
            full_nv12.resolve_range(ColorRange::Limited),
            ColorRange::Limited
        );
    }
}
