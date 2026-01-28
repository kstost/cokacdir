use ratatui::style::{Color, Modifier, Style};

/// Theme characters for file/folder icons
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct ThemeChars {
    pub folder: char,
    pub file: char,
    pub folder_open: char,
    pub parent: char,
}

impl Default for ThemeChars {
    fn default() -> Self {
        Self {
            folder: ' ',
            file: ' ',
            folder_open: ' ',
            parent: ' ',
        }
    }
}

#[allow(dead_code)]
pub struct Theme {
    pub bg: Color,
    pub bg_panel: Color,
    pub bg_selected: Color,
    pub bg_header: Color,
    pub bg_status_bar: Color,

    pub text: Color,
    pub text_dim: Color,
    pub text_bold: Color,
    pub text_selected: Color,
    pub text_header: Color,
    pub text_directory: Color,

    pub border: Color,
    pub border_active: Color,

    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub info: Color,

    pub chars: ThemeChars,
}

impl Default for Theme {
    fn default() -> Self {
        Self::dracula()
    }
}

impl Theme {
    /// Classic Norton Commander blue theme
    #[allow(dead_code)]
    pub fn classic_blue() -> Self {
        Self {
            bg: Color::Blue,
            bg_panel: Color::Blue,
            bg_selected: Color::Cyan,
            bg_header: Color::Cyan,
            bg_status_bar: Color::Cyan,

            text: Color::White,
            text_dim: Color::Gray,
            text_bold: Color::White,
            text_selected: Color::Black,
            text_header: Color::Black,
            text_directory: Color::White,

            border: Color::Cyan,
            border_active: Color::Yellow,

            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,
            info: Color::Cyan,

            chars: ThemeChars::default(),
        }
    }

    /// Dracula theme (default) - uses 256 color palette for compatibility
    pub fn dracula() -> Self {
        // Check if terminal supports true color
        let truecolor = std::env::var("COLORTERM")
            .map(|v| v == "truecolor" || v == "24bit")
            .unwrap_or(false);

        if truecolor {
            Self::dracula_rgb()
        } else {
            Self::dracula_256()
        }
    }

    /// Dracula theme with RGB colors (for terminals supporting true color)
    fn dracula_rgb() -> Self {
        Self {
            bg: Color::Rgb(40, 42, 54),
            bg_panel: Color::Rgb(40, 42, 54),
            bg_selected: Color::Rgb(68, 71, 90),
            bg_header: Color::Rgb(32, 32, 46),
            bg_status_bar: Color::Rgb(68, 71, 90),

            text: Color::Rgb(248, 248, 242),
            text_dim: Color::Rgb(98, 114, 164),
            text_bold: Color::Rgb(248, 248, 242),
            text_selected: Color::Rgb(248, 248, 242),
            text_header: Color::Rgb(189, 147, 249),
            text_directory: Color::Rgb(139, 233, 253),

            border: Color::Rgb(42, 45, 62),
            border_active: Color::Rgb(189, 147, 249),

            success: Color::Rgb(80, 250, 123),
            warning: Color::Rgb(241, 250, 140),
            error: Color::Rgb(255, 85, 85),
            info: Color::Rgb(139, 233, 253),

            chars: ThemeChars::default(),
        }
    }

    /// Dracula theme with 256 color palette (for basic terminals)
    fn dracula_256() -> Self {
        Self {
            bg: Color::Indexed(236),           // dark gray
            bg_panel: Color::Indexed(236),
            bg_selected: Color::Indexed(238),  // lighter gray
            bg_header: Color::Indexed(235),
            bg_status_bar: Color::Indexed(238),

            text: Color::Indexed(255),         // white
            text_dim: Color::Indexed(103),     // gray-blue
            text_bold: Color::Indexed(255),
            text_selected: Color::Indexed(255),
            text_header: Color::Indexed(141),  // purple
            text_directory: Color::Indexed(87), // cyan

            border: Color::Indexed(237),
            border_active: Color::Indexed(141), // purple

            success: Color::Indexed(84),       // green
            warning: Color::Indexed(228),      // yellow
            error: Color::Indexed(203),        // red
            info: Color::Indexed(87),          // cyan

            chars: ThemeChars::default(),
        }
    }

    pub fn normal_style(&self) -> Style {
        Style::default().fg(self.text)
    }

    pub fn dim_style(&self) -> Style {
        Style::default().fg(self.text_dim)
    }

    pub fn selected_style(&self) -> Style {
        Style::default()
            .fg(self.text_selected)
            .bg(self.bg_selected)
    }

    pub fn directory_style(&self) -> Style {
        Style::default()
            .fg(self.text_directory)
            .add_modifier(Modifier::BOLD)
    }

    pub fn header_style(&self) -> Style {
        Style::default()
            .fg(self.text_header)
            .add_modifier(Modifier::BOLD)
    }

    pub fn border_style(&self, active: bool) -> Style {
        if active {
            Style::default().fg(self.border_active)
        } else {
            Style::default().fg(self.border)
        }
    }

    pub fn warning_style(&self) -> Style {
        Style::default()
            .fg(self.warning)
            .add_modifier(Modifier::BOLD)
    }

    pub fn error_style(&self) -> Style {
        Style::default().fg(self.error)
    }

    pub fn success_style(&self) -> Style {
        Style::default().fg(self.success)
    }

    pub fn marked_style(&self) -> Style {
        Style::default()
            .fg(self.warning)
    }

    pub fn status_bar_style(&self) -> Style {
        Style::default()
            .fg(self.text_header)
            .bg(self.bg_status_bar)
    }

    pub fn info_style(&self) -> Style {
        Style::default().fg(self.info)
    }
}
