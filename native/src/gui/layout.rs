//! Responsive layout policy for the legacy GTK frontend.

pub const COMPACT_MAX_WIDTH: u32 = 799;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellLayout {
    pub header_switcher: bool,
    pub bottom_switcher: bool,
    pub inspector_pinned: bool,
}

pub const fn shell_layout_for_width(width: u32) -> ShellLayout {
    let compact = width <= COMPACT_MAX_WIDTH;
    ShellLayout {
        header_switcher: !compact,
        bottom_switcher: compact,
        inspector_pinned: !compact,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responsive_shell_has_exactly_one_switcher_at_supported_widths() {
        for width in [480, 720, 1_100] {
            let layout = shell_layout_for_width(width);
            assert_ne!(layout.header_switcher, layout.bottom_switcher);
            assert_eq!(layout.inspector_pinned, width > COMPACT_MAX_WIDTH);
        }
    }
}
