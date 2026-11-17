//! Persisted, ordered performance-pill choices. No synthetic telemetry.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Hidden,
    #[default]
    Compact,
    Custom,
}

impl Mode {
    pub const ALL: [Self; 3] = [Self::Hidden, Self::Compact, Self::Custom];

    pub fn label(self) -> &'static str {
        match self {
            Self::Hidden => "Hidden",
            Self::Compact => "Compact",
            Self::Custom => "Custom",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stat {
    Resolution,
    DeliveredFps,
    NominalFps,
    PixelFormat,
    ScalingFilter,
    ColorSpace,
    ColorRange,
    FrameGeneration,
    UniqueFps,
    GeneratedFps,
    PresentedFps,
}
impl Stat {
    pub const ALL: [Self; 11] = [
        Self::Resolution,
        Self::DeliveredFps,
        Self::NominalFps,
        Self::PixelFormat,
        Self::ScalingFilter,
        Self::ColorSpace,
        Self::ColorRange,
        Self::FrameGeneration,
        Self::UniqueFps,
        Self::GeneratedFps,
        Self::PresentedFps,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Resolution => "Resolution",
            Self::DeliveredFps => "Capture FPS (Cap)",
            Self::NominalFps => "Requested FPS (Req)",
            Self::PixelFormat => "Pixel format",
            Self::ScalingFilter => "Scaling filter",
            Self::ColorSpace => "Color space",
            Self::ColorRange => "Color range",
            Self::FrameGeneration => "Frame generation",
            Self::UniqueFps => "Source FPS (Src)",
            Self::GeneratedFps => "Generated FPS (Gen)",
            Self::PresentedFps => "Presented FPS (Out)",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatChoice {
    pub stat: Stat,
    pub enabled: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Corner {
    #[default]
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayLayout {
    #[serde(default)]
    pub corner: Corner,
    pub stats: Vec<StatChoice>,
}
impl Default for OverlayLayout {
    fn default() -> Self {
        Self::legacy(false)
    }
}
impl OverlayLayout {
    pub fn move_row(&mut self, from: usize, to: usize) -> bool {
        if from >= self.stats.len() || to >= self.stats.len() || from == to {
            return false;
        }
        let row = self.stats.remove(from);
        self.stats.insert(to, row);
        true
    }

    pub fn normalize(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.stats.retain(|row| seen.insert(row.stat));
        for stat in Stat::ALL {
            if seen.insert(stat) {
                self.stats.push(StatChoice {
                    stat,
                    enabled: false,
                });
            }
        }
    }
    pub fn legacy(detailed: bool) -> Self {
        Self {
            corner: Corner::default(),
            stats: Stat::ALL
                .into_iter()
                .enumerate()
                .map(|(i, stat)| StatChoice {
                    stat,
                    enabled: (detailed && i < 7) || i < 4,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn moving_a_checked_row_keeps_its_identity_and_round_trips() {
        let mut layout = OverlayLayout::default();
        layout.stats[0].enabled = false;
        let checked = layout.stats[1].clone();
        assert!(layout.move_row(1, 5));
        assert_eq!(layout.stats[5], checked);
        assert!(!layout.stats[0].enabled);
        assert!(!layout.move_row(99, 0));
        let json = serde_json::to_string(&layout).unwrap();
        assert_eq!(
            serde_json::from_str::<OverlayLayout>(&json).unwrap(),
            layout
        );
    }
}
