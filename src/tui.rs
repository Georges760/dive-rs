use std::path::PathBuf;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Chart, Dataset, GraphType, List, ListItem, ListState, Paragraph,
};
use ratatui::DefaultTerminal;

use crate::types::{DiveData, DiveLog, DiveMode};

struct App {
    dives: Vec<DiveLog>,
    list_state: ListState,
    should_quit: bool,
    show_depth: bool,
    show_temp: bool,
    show_pressure: bool,
}

impl App {
    fn new(dives: Vec<DiveLog>) -> Self {
        let mut list_state = ListState::default();
        if !dives.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            dives,
            list_state,
            should_quit: false,
            show_depth: true,
            show_temp: true,
            show_pressure: true,
        }
    }

    fn selected_dive(&self) -> Option<&DiveLog> {
        self.list_state.selected().and_then(|i| self.dives.get(i))
    }

    fn handle_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('d') => self.show_depth = !self.show_depth,
            KeyCode::Char('t') => self.show_temp = !self.show_temp,
            KeyCode::Char('p') => self.show_pressure = !self.show_pressure,
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(i) = self.list_state.selected() {
                    if i + 1 < self.dives.len() {
                        self.list_state.select(Some(i + 1));
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(i) = self.list_state.selected() {
                    if i > 0 {
                        self.list_state.select(Some(i - 1));
                    }
                }
            }
            KeyCode::Home => {
                if !self.dives.is_empty() {
                    self.list_state.select(Some(0));
                }
            }
            KeyCode::End => {
                if !self.dives.is_empty() {
                    self.list_state.select(Some(self.dives.len() - 1));
                }
            }
            _ => {}
        }
    }
}

pub fn run(input: PathBuf) -> Result<()> {
    let contents =
        std::fs::read_to_string(&input).with_context(|| format!("Failed to read {}", input.display()))?;
    let data: DiveData =
        serde_json::from_str(&contents).with_context(|| format!("Failed to parse {}", input.display()))?;

    if data.dives.is_empty() {
        eprintln!("No dives found in {}", input.display());
        return Ok(());
    }

    // Sort dives by number descending (most recent first)
    let mut dives = data.dives;
    dives.sort_by(|a, b| b.number.cmp(&a.number));

    let mut app = App::new(dives);

    // Setup terminal
    terminal::enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
    let mut terminal = ratatui::init();

    let result = main_loop(&mut terminal, &mut app);

    // Restore terminal
    ratatui::restore();
    crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;

    result
}

fn main_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|frame| ui(frame, app))?;

        if event::poll(std::time::Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key.code);
                }
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

fn ui(frame: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(frame.area());

    render_dive_list(frame, app, chunks[0]);

    if app.selected_dive().is_some() {
        render_detail_panel(frame, app, chunks[1]);
    }
}

fn mode_short(mode: &DiveMode) -> &'static str {
    match mode {
        DiveMode::Air => "Air",
        DiveMode::Nitrox => "Nx",
        DiveMode::Gauge => "Gau",
        DiveMode::Freedive => "Free",
    }
}

fn render_dive_list(frame: &mut ratatui::Frame, app: &mut App, area: ratatui::layout::Rect) {
    let items: Vec<ListItem> = app
        .dives
        .iter()
        .map(|dive| {
            let duration_min = dive.duration_seconds / 60;
            let line = format!(
                "#{:<3} {} {:5.1}m {:3}min {}",
                dive.number,
                dive.datetime.format("%Y-%m-%d"),
                dive.max_depth_m,
                duration_min,
                mode_short(&dive.dive_mode),
            );
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Dive Log "),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_detail_panel(frame: &mut ratatui::Frame, app: &mut App, area: ratatui::layout::Rect) {
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(10)])
        .split(area);

    render_dive_info(frame, app, right_chunks[0]);
    render_depth_chart(frame, app, right_chunks[1]);
}

fn render_dive_info(frame: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    let dive = match app.selected_dive() {
        Some(d) => d,
        None => return,
    };

    let duration_min = dive.duration_seconds / 60;
    let duration_sec = dive.duration_seconds % 60;

    let gas_str = dive
        .gas_mixes
        .iter()
        .map(|g| format!("{}% O2", g.o2))
        .collect::<Vec<_>>()
        .join(", ");

    // Temperature range from samples
    let (temp_min, temp_max) = dive
        .samples
        .iter()
        .filter_map(|s| s.temp_c)
        .fold((f64::MAX, f64::MIN), |(min, max), t| {
            (min.min(t), max.max(t))
        });

    // Pressure: first and last non-None values
    let pressure_start = dive.samples.iter().find_map(|s| s.pressure_bar);
    let pressure_end = dive.samples.iter().rev().find_map(|s| s.pressure_bar);

    // Two-column layout: left and right fields paired per row
    // col_w is the width of one column (half the inner area minus borders)
    let inner_w = area.width.saturating_sub(2) as usize; // subtract borders
    let col_w = inner_w / 2;

    let mut left_col: Vec<String> = vec![
        format!(
            " Date:      {}",
            dive.datetime.format("%Y-%m-%d %H:%M")
        ),
        format!(" Duration:  {:02}:{:02}", duration_min, duration_sec),
        format!(" Max depth: {:.1} m", dive.max_depth_m),
    ];

    let mut right_col: Vec<String> = vec![
        format!(" Gas:       {}", gas_str),
    ];

    if temp_min != f64::MAX {
        right_col.push(format!(" Temp:      {:.1} - {:.1} C", temp_min, temp_max));
    }

    if let (Some(start), Some(end)) = (pressure_start, pressure_end) {
        right_col.push(format!(" Pressure:  {:.0} -> {:.0} bar", start, end));
    }

    if let Some(ref site) = dive.site {
        left_col.push(format!(" Site:      {}", site));
    }
    if let Some(ref country) = dive.country {
        right_col.push(format!(" Country:   {}", country));
    }
    if let Some(ref buddy) = dive.buddy {
        left_col.push(format!(" Buddy/Ctr: {}", buddy));
    }

    // Pad columns to same length
    let max_rows = left_col.len().max(right_col.len());
    left_col.resize(max_rows, String::new());
    right_col.resize(max_rows, String::new());

    // Title line
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("  Dive #{}  ", dive.number),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:?}", dive.dive_mode),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(format!(
                "    ({} samples)",
                dive.samples.len()
            )),
        ]),
    ];

    // Data rows: left field padded to col_w, then right field
    for (l, r) in left_col.iter().zip(right_col.iter()) {
        let padded_left = format!("{:<width$}", l, width = col_w);
        lines.push(Line::from(format!("{}{}", padded_left, r)));
    }

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Dive Details "),
    );

    frame.render_widget(paragraph, area);
}

fn render_depth_chart(frame: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    let dive = match app.selected_dive() {
        Some(d) => d,
        None => return,
    };

    if dive.samples.is_empty() {
        let msg = Paragraph::new("  No sample data").block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Depth Profile "),
        );
        frame.render_widget(msg, area);
        return;
    }

    // Build depth data points: (time_minutes, -depth_m) — negate depth so surface (0) is at top
    let depth_data: Vec<(f64, f64)> = dive
        .samples
        .iter()
        .map(|s| (s.time_s as f64 / 60.0, -s.depth_m))
        .collect();

    let max_time = depth_data
        .iter()
        .map(|(t, _)| *t)
        .fold(0.0_f64, f64::max);
    let max_depth = dive
        .samples
        .iter()
        .map(|s| s.depth_m)
        .fold(0.0_f64, f64::max);

    // Round up axis bounds for nice labels
    let time_bound = ((max_time / 5.0).ceil() * 5.0).max(5.0);
    let depth_bound = ((max_depth / 5.0).ceil() * 5.0).max(5.0);

    // Y-axis labels: surface at top, max depth at bottom
    let y_labels = vec![
        Span::raw(format!("{:.0}m", depth_bound)),
        Span::raw(format!("{:.0}m", depth_bound / 2.0)),
        Span::raw("0m"),
    ];

    // X-axis labels
    let x_labels = vec![
        Span::raw("0"),
        Span::raw(format!("{:.0}", time_bound / 2.0)),
        Span::raw(format!("{:.0}", time_bound)),
    ];

    let mut datasets = Vec::new();

    // Optional temperature overlay (drawn first = bottom layer)
    let has_temp = app.show_temp && dive.samples.iter().any(|s| s.temp_c.is_some());
    let temp_data: Vec<(f64, f64)>;
    let temp_label: String;

    if has_temp {
        let (tmin, tmax) = dive
            .samples
            .iter()
            .filter_map(|s| s.temp_c)
            .fold((f64::MAX, f64::MIN), |(min, max), t| {
                (min.min(t), max.max(t))
            });

        let temp_range = (tmax - tmin).max(1.0);

        // Normalize temperature to depth axis: map temp range to [-depth_bound, 0]
        // Higher temp → closer to 0 (top), lower temp → closer to -depth_bound (bottom)
        temp_data = dive
            .samples
            .iter()
            .filter_map(|s| {
                s.temp_c
                    .map(|t| (s.time_s as f64 / 60.0, -((tmax - t) / temp_range) * depth_bound))
            })
            .collect();

        temp_label = format!("Temp ({:.0}-{:.0}C)", tmin, tmax);

        datasets.push(
            Dataset::default()
                .name(temp_label.as_str())
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Red))
                .data(&temp_data),
        );
    }

    // Optional pressure overlay (drawn second = middle layer)
    let has_pressure = app.show_pressure && dive.samples.iter().any(|s| s.pressure_bar.is_some());
    let pressure_data: Vec<(f64, f64)>;
    let pressure_label: String;

    if has_pressure {
        let (pmin, pmax) = dive
            .samples
            .iter()
            .filter_map(|s| s.pressure_bar)
            .fold((f64::MAX, f64::MIN), |(min, max), p| {
                (min.min(p), max.max(p))
            });

        let pressure_range = (pmax - pmin).max(1.0);

        // Normalize pressure to depth axis: map pressure range to [-depth_bound, 0]
        // Higher pressure → closer to 0 (top), lower pressure → closer to -depth_bound (bottom)
        pressure_data = dive
            .samples
            .iter()
            .filter_map(|s| {
                s.pressure_bar
                    .map(|p| (s.time_s as f64 / 60.0, -((pmax - p) / pressure_range) * depth_bound))
            })
            .collect();

        pressure_label = format!("Press ({:.0}-{:.0}bar)", pmax, pmin);

        datasets.push(
            Dataset::default()
                .name(pressure_label.as_str())
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Green))
                .data(&pressure_data),
        );
    }

    // Depth (drawn last = top layer)
    if app.show_depth {
        datasets.push(
            Dataset::default()
                .name("Depth")
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Cyan))
                .data(&depth_data),
        );
    }

    let chart = Chart::new(datasets)
        .hidden_legend_constraints((Constraint::Min(0), Constraint::Min(0)))
        .legend_position(Some(ratatui::widgets::LegendPosition::Right))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Depth Profile "),
        )
        .x_axis(
            Axis::default()
                .title("Time (min)")
                .style(Style::default().fg(Color::Gray))
                .labels(x_labels)
                .bounds([0.0, time_bound]),
        )
        .y_axis(
            Axis::default()
                .title("Depth")
                .style(Style::default().fg(Color::Gray))
                .labels(y_labels)
                .bounds([-depth_bound, 0.0]),
        );

    frame.render_widget(chart, area);
}
