//! Semantic UI colors; Classic deliberately retains the original palette.
use crate::settings::Theme;
use egui::Color32;

#[derive(Clone, Copy)]
pub struct Palette {
    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub card_bg: Color32,
    pub panel_bg: Color32,
    pub panel_hover: Color32,
    pub border: Color32,
    pub border_subtle: Color32,
    pub menu_border: Color32,
    pub tab_bar_bg: Color32,
    pub tab_active_bg: Color32,
    pub dim_overlay: Color32,
    pub pill_bg: Color32,
    pub exit_bg: Color32,
    pub exit_border: Color32,
    pub exit_text: Color32,
    pub warning: Color32,
    pub menu_bg: Color32,
}
impl Palette {
    pub fn for_theme(theme: Theme) -> Self {
        let classic = Self {
            text_primary: Color32::from_rgb(0xE0, 0xE0, 0xE0),
            text_secondary: Color32::from_rgb(0x88, 0x99, 0xAA),
            text_muted: Color32::from_rgb(0x88, 0x99, 0xAA),
            accent: Color32::from_rgb(0xE9, 0x45, 0x60),
            accent_hover: Color32::from_rgb(0xF4, 0x6A, 0x81),
            card_bg: Color32::from_rgb(0x10, 0x19, 0x2F),
            panel_bg: Color32::from_rgb(0x16, 0x21, 0x3E),
            panel_hover: Color32::from_rgb(0x1A, 0x2A, 0x50),
            border: Color32::from_rgb(0x0F, 0x34, 0x60),
            border_subtle: Color32::from_rgb(0x1A, 0x2A, 0x50),
            menu_border: Color32::from_rgb(0x1A, 0x2A, 0x50),
            tab_bar_bg: Color32::from_rgb(0x0C, 0x10, 0x22),
            tab_active_bg: Color32::from_rgb(0x16, 0x21, 0x3E),
            dim_overlay: Color32::from_black_alpha(120),
            pill_bg: Color32::from_black_alpha(180),
            exit_bg: Color32::from_rgb(0x3A, 0x10, 0x20),
            exit_border: Color32::from_rgb(0xE9, 0x45, 0x60),
            exit_text: Color32::from_rgb(0xE0, 0xE0, 0xE0),
            warning: Color32::from_rgb(0xF5, 0x9E, 0x0B),
            menu_bg: Color32::from_rgba_unmultiplied(12, 12, 28, 240),
        };
        match theme {
            Theme::Classic => classic,
            Theme::Vice => Self {
                text_primary: Color32::from_rgb(255, 244, 232),
                text_secondary: Color32::from_rgb(221, 189, 206),
                text_muted: Color32::from_rgb(188, 151, 177),
                accent: Color32::from_rgb(255, 164, 182),
                accent_hover: Color32::from_rgb(255, 195, 167),
                card_bg: Color32::from_rgb(36, 21, 43),
                panel_bg: Color32::from_rgb(52, 30, 57),
                panel_hover: Color32::from_rgb(73, 41, 73),
                border: Color32::from_rgb(105, 61, 95),
                border_subtle: Color32::from_rgb(67, 40, 64),
                menu_border: Color32::from_rgb(112, 65, 103),
                tab_bar_bg: Color32::from_rgb(24, 15, 32),
                tab_active_bg: Color32::from_rgb(65, 34, 66),
                exit_bg: Color32::from_rgb(65, 24, 48),
                exit_border: Color32::from_rgb(255, 144, 172),
                exit_text: Color32::from_rgb(255, 244, 232),
                warning: Color32::from_rgb(255, 195, 130),
                menu_bg: Color32::from_rgba_unmultiplied(27, 16, 35, 248),
                ..classic
            },
        }
    }
    pub fn current(ctx: &egui::Context) -> Self {
        Self::for_theme(
            ctx.data(|data| data.get_temp::<Theme>(egui::Id::new("edition_theme")))
                .unwrap_or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Theme;
    use egui::Color32;
    #[test]
    fn classic_preserves_colors_and_vice_has_distinct_warm_palette() {
        let classic = Palette::for_theme(Theme::Classic);
        assert_eq!(classic.text_primary, Color32::from_rgb(0xE0, 0xE0, 0xE0));
        assert_eq!(classic.accent, Color32::from_rgb(0xE9, 0x45, 0x60));
        assert_eq!(classic.card_bg, Color32::from_rgb(0x10, 0x19, 0x2F));
        assert_eq!(
            classic.menu_bg,
            Color32::from_rgba_unmultiplied(12, 12, 28, 240)
        );
        let vice = Palette::for_theme(Theme::Vice);
        assert_ne!(vice.menu_bg, classic.menu_bg);
        assert_eq!(vice.text_primary, Color32::from_rgb(255, 244, 232));
        assert!(vice.panel_bg.r() > vice.panel_bg.g());
    }
}
