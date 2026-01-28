use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use super::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoTab {
    System,
    Disk,
}

#[derive(Debug, Clone)]
pub struct DiskInfo {
    pub filesystem: String,
    pub size: String,
    pub used: String,
    pub available: String,
    pub use_percent: u8,
    pub mountpoint: String,
}

pub struct SystemInfoState {
    pub current_tab: InfoTab,
    pub disks: Vec<DiskInfo>,
    pub disk_selected: usize,
    #[allow(dead_code)]
    pub last_update: std::time::Instant,
}

impl Default for SystemInfoState {
    fn default() -> Self {
        Self {
            current_tab: InfoTab::System,
            disks: load_disk_info(),
            disk_selected: 0,
            last_update: std::time::Instant::now(),
        }
    }
}

impl SystemInfoState {
    pub fn refresh_disks(&mut self) {
        self.disks = load_disk_info();
        if self.disk_selected >= self.disks.len() {
            self.disk_selected = self.disks.len().saturating_sub(1);
        }
    }
}

#[derive(Default)]
struct SystemData {
    hostname: String,
    platform: String,
    arch: String,
    kernel: String,
    uptime_secs: u64,
    total_mem: u64,
    free_mem: u64,
    cpu_count: usize,
    cpu_model: String,
    load_avg: [f64; 3],
    username: String,
}

impl SystemData {
    fn load() -> Self {
        let mut data = Self::default();

        // Hostname
        #[cfg(unix)]
        {
            if let Ok(output) = std::process::Command::new("hostname").output() {
                data.hostname = String::from_utf8_lossy(&output.stdout).trim().to_string();
            }
        }

        // Platform and arch
        data.platform = std::env::consts::OS.to_string();
        data.arch = std::env::consts::ARCH.to_string();

        // Username
        data.username = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown".to_string());

        #[cfg(unix)]
        {
            // Kernel version
            if let Ok(output) = std::process::Command::new("uname").arg("-r").output() {
                data.kernel = String::from_utf8_lossy(&output.stdout).trim().to_string();
            }

            // Uptime
            if let Ok(content) = std::fs::read_to_string("/proc/uptime") {
                if let Some(uptime_str) = content.split_whitespace().next() {
                    data.uptime_secs = uptime_str.parse::<f64>().unwrap_or(0.0) as u64;
                }
            }

            // Memory info
            if let Ok(content) = std::fs::read_to_string("/proc/meminfo") {
                for line in content.lines() {
                    if line.starts_with("MemTotal:") {
                        if let Some(val) = line.split_whitespace().nth(1) {
                            data.total_mem = val.parse::<u64>().unwrap_or(0) * 1024;
                        }
                    } else if line.starts_with("MemAvailable:") {
                        if let Some(val) = line.split_whitespace().nth(1) {
                            data.free_mem = val.parse::<u64>().unwrap_or(0) * 1024;
                        }
                    }
                }
            }

            // CPU info
            if let Ok(content) = std::fs::read_to_string("/proc/cpuinfo") {
                let mut count = 0;
                for line in content.lines() {
                    if line.starts_with("processor") {
                        count += 1;
                    }
                    if line.starts_with("model name") && data.cpu_model.is_empty() {
                        if let Some(model) = line.split(':').nth(1) {
                            data.cpu_model = model.trim().to_string();
                        }
                    }
                }
                data.cpu_count = count;
            }

            // Load average
            if let Ok(content) = std::fs::read_to_string("/proc/loadavg") {
                let parts: Vec<&str> = content.split_whitespace().collect();
                if parts.len() >= 3 {
                    data.load_avg[0] = parts[0].parse().unwrap_or(0.0);
                    data.load_avg[1] = parts[1].parse().unwrap_or(0.0);
                    data.load_avg[2] = parts[2].parse().unwrap_or(0.0);
                }
            }
        }

        #[cfg(target_os = "macos")]
        {
            // macOS specific implementations
            if let Ok(output) = std::process::Command::new("sysctl")
                .args(["-n", "kern.version"])
                .output()
            {
                data.kernel = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
            }
        }

        data
    }
}

fn load_disk_info() -> Vec<DiskInfo> {
    let mut disks = Vec::new();

    #[cfg(unix)]
    {
        if let Ok(output) = std::process::Command::new("df")
            .arg("-h")
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines().skip(1) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 6 {
                        let filesystem = parts[0].to_string();

                        // Skip tmpfs and devtmpfs
                        if filesystem.starts_with("tmpfs") || filesystem.starts_with("devtmpfs") {
                            continue;
                        }

                        let use_percent = parts[4]
                            .trim_end_matches('%')
                            .parse::<u8>()
                            .unwrap_or(0);

                        disks.push(DiskInfo {
                            filesystem,
                            size: parts[1].to_string(),
                            used: parts[2].to_string(),
                            available: parts[3].to_string(),
                            use_percent,
                            mountpoint: parts[5].to_string(),
                        });
                    }
                }
            }
        }
    }

    disks
}

fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    let mins = (seconds % 3600) / 60;
    format!("{}d {}h {}m", days, hours, mins)
}

fn format_bytes(bytes: u64) -> String {
    let gb = bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    format!("{:.2} GB", gb)
}

fn get_usage_color(percent: u8, theme: &Theme) -> Color {
    if percent >= 90 {
        theme.error
    } else if percent >= 70 {
        theme.warning
    } else {
        theme.success
    }
}

pub fn draw(frame: &mut Frame, state: &SystemInfoState, area: Rect, theme: &Theme) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(2), // Tab bar
            Constraint::Min(5),    // Content
            Constraint::Length(1), // Footer
        ])
        .split(area);

    // Tab bar
    draw_tab_bar(frame, state, chunks[0], theme);

    // Content based on current tab
    match state.current_tab {
        InfoTab::System => draw_system_tab(frame, chunks[1], theme),
        InfoTab::Disk => draw_disk_tab(frame, state, chunks[1], theme),
    }

    // Footer
    let footer_text = match state.current_tab {
        InfoTab::System => "Tab/←→: Switch tab | ESC: Back",
        InfoTab::Disk => "Tab/←→: Switch tab | ↑↓: Select | ESC: Back",
    };
    let footer = Paragraph::new(Span::styled(footer_text, theme.dim_style()))
        .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer, chunks[2]);
}

fn draw_tab_bar(frame: &mut Frame, state: &SystemInfoState, area: Rect, theme: &Theme) {
    let tabs = [
        (" System ", InfoTab::System),
        (" Disk ", InfoTab::Disk),
    ];

    let mut spans = Vec::new();
    spans.push(Span::styled("  ", theme.normal_style()));

    for (label, tab) in tabs {
        let style = if state.current_tab == tab {
            Style::default()
                .fg(theme.border_active)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            theme.dim_style()
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::styled("  ", theme.normal_style()));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_system_tab(frame: &mut Frame, area: Rect, theme: &Theme) {
    let data = SystemData::load();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),  // System info
            Constraint::Length(6),  // Memory
            Constraint::Min(4),     // CPU
        ])
        .split(area);

    // System info block
    let sys_block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim_style())
        .title(" System ");

    let sys_inner = sys_block.inner(chunks[0]);
    frame.render_widget(sys_block, chunks[0]);

    let platform_str = format!("{} ({})", data.platform, data.arch);
    let uptime_str = format_uptime(data.uptime_secs);
    let sys_lines = vec![
        create_info_line("Hostname:", &data.hostname, theme),
        create_info_line("User:", &data.username, theme),
        create_info_line("Platform:", &platform_str, theme),
        create_info_line("Kernel:", &data.kernel, theme),
        create_info_line("Uptime:", &uptime_str, theme),
    ];
    frame.render_widget(Paragraph::new(sys_lines), sys_inner);

    // Memory block
    let mem_block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim_style())
        .title(Span::styled(" Memory ", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)));

    let mem_inner = mem_block.inner(chunks[1]);
    frame.render_widget(mem_block, chunks[1]);

    let mem_used = data.total_mem.saturating_sub(data.free_mem);
    let mem_percent = if data.total_mem > 0 {
        (mem_used as f64 / data.total_mem as f64 * 100.0) as u8
    } else {
        0
    };

    let bar_width = 20;
    let filled = (mem_percent as usize * bar_width / 100).min(bar_width);
    let bar = format!(
        "[{}{}]",
        "█".repeat(filled),
        "░".repeat(bar_width - filled)
    );

    let total_str = format_bytes(data.total_mem);
    let used_str = format!("{} ({}%)", format_bytes(mem_used), mem_percent);
    let free_str = format_bytes(data.free_mem);
    let mem_lines = vec![
        create_info_line("Total:", &total_str, theme),
        create_info_line("Used:", &used_str, theme),
        create_info_line("Free:", &free_str, theme),
        Line::from(vec![
            Span::styled("            ", theme.normal_style()),
            Span::styled(bar, Style::default().fg(theme.success)),
        ]),
    ];
    frame.render_widget(Paragraph::new(mem_lines), mem_inner);

    // CPU block
    let cpu_title = format!(" CPU ({} cores) ", data.cpu_count);
    let cpu_block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim_style())
        .title(Span::styled(cpu_title, Style::default().fg(theme.success).add_modifier(Modifier::BOLD)));

    let cpu_inner = cpu_block.inner(chunks[2]);
    frame.render_widget(cpu_block, chunks[2]);

    let cpu_model_display = if data.cpu_model.chars().count() > 50 {
        let truncated: String = data.cpu_model.chars().take(47).collect();
        format!("{}...", truncated)
    } else {
        data.cpu_model.clone()
    };
    let load_str = format!("{:.2} / {:.2} / {:.2}", data.load_avg[0], data.load_avg[1], data.load_avg[2]);

    let cpu_lines = vec![
        create_info_line("Model:", &cpu_model_display, theme),
        create_info_line("Load (1/5/15m):", &load_str, theme),
    ];
    frame.render_widget(Paragraph::new(cpu_lines), cpu_inner);
}

fn draw_disk_tab(frame: &mut Frame, state: &SystemInfoState, area: Rect, theme: &Theme) {
    let disk_block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim_style())
        .title(" Disks ");

    let disk_inner = disk_block.inner(area);
    frame.render_widget(disk_block, area);

    if disk_inner.width < 60 {
        // Narrow view
        draw_disk_list_narrow(frame, state, disk_inner, theme);
    } else {
        // Wide view
        draw_disk_list_wide(frame, state, disk_inner, theme);
    }
}

fn draw_disk_list_wide(frame: &mut Frame, state: &SystemInfoState, area: Rect, theme: &Theme) {
    // Header
    let header = Line::from(vec![
        Span::styled(format!("{:20}", "Filesystem"), Style::default().fg(theme.info).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{:>8}", "Size"), Style::default().fg(theme.info).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{:>8}", "Used"), Style::default().fg(theme.info).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{:>8}", "Avail"), Style::default().fg(theme.info).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{:>6}", "Use%"), Style::default().fg(theme.info).add_modifier(Modifier::BOLD)),
        Span::styled("  Mount", Style::default().fg(theme.info).add_modifier(Modifier::BOLD)),
    ]);

    let mut lines = vec![header];

    for (i, disk) in state.disks.iter().enumerate() {
        let is_selected = i == state.disk_selected;
        let usage_color = get_usage_color(disk.use_percent, theme);

        let bar_width = 10;
        let filled = (disk.use_percent as usize * bar_width / 100).min(bar_width);
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_width - filled));

        let fs_display = if disk.filesystem.chars().count() > 18 {
            let truncated: String = disk.filesystem.chars().take(15).collect();
            format!("{}...", truncated)
        } else {
            disk.filesystem.clone()
        };

        let line_style = if is_selected {
            theme.selected_style()
        } else {
            theme.normal_style()
        };

        let line = Line::from(vec![
            Span::styled(format!("{:20}", fs_display), line_style),
            Span::styled(format!("{:>8}", disk.size), line_style),
            Span::styled(format!("{:>8}", disk.used), line_style),
            Span::styled(format!("{:>8}", disk.available), line_style),
            Span::styled(format!("{:>5}%", disk.use_percent), Style::default().fg(usage_color)),
            Span::styled(format!(" {}", bar), Style::default().fg(usage_color)),
            Span::styled(format!(" {}", disk.mountpoint), line_style),
        ]);
        lines.push(line);
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_disk_list_narrow(frame: &mut Frame, state: &SystemInfoState, area: Rect, theme: &Theme) {
    let mut lines = Vec::new();

    for (i, disk) in state.disks.iter().enumerate() {
        let is_selected = i == state.disk_selected;
        let usage_color = get_usage_color(disk.use_percent, theme);

        let line_style = if is_selected {
            theme.selected_style()
        } else {
            theme.normal_style()
        };

        let bar_width = 10;
        let filled = (disk.use_percent as usize * bar_width / 100).min(bar_width);
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_width - filled));

        // Line 1: Mount point
        lines.push(Line::from(Span::styled(
            format!(" {} ", disk.mountpoint),
            line_style.add_modifier(Modifier::BOLD),
        )));

        // Line 2: Usage bar and percent
        lines.push(Line::from(vec![
            Span::styled("   ", theme.normal_style()),
            Span::styled(bar, Style::default().fg(usage_color)),
            Span::styled(format!(" {}%", disk.use_percent), Style::default().fg(usage_color)),
            Span::styled(format!("  {}/{}", disk.used, disk.size), theme.dim_style()),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn create_info_line<'a>(label: &'a str, value: &'a str, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{:15}", label), Style::default().fg(theme.info)),
        Span::styled(value.to_string(), theme.normal_style()),
    ])
}

pub fn handle_input(state: &mut SystemInfoState, code: KeyCode) -> bool {
    match code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
            return true;
        }
        KeyCode::Tab | KeyCode::Left | KeyCode::Right => {
            state.current_tab = match state.current_tab {
                InfoTab::System => InfoTab::Disk,
                InfoTab::Disk => InfoTab::System,
            };
            if state.current_tab == InfoTab::Disk {
                state.refresh_disks();
            }
        }
        KeyCode::Up if state.current_tab == InfoTab::Disk => {
            state.disk_selected = state.disk_selected.saturating_sub(1);
        }
        KeyCode::Down if state.current_tab == InfoTab::Disk => {
            if state.disk_selected < state.disks.len().saturating_sub(1) {
                state.disk_selected += 1;
            }
        }
        _ => {}
    }
    false
}
