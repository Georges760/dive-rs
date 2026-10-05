use std::cell::Cell as StateCell;
use std::path::PathBuf;

use anyhow::{Context, Result};
use blit::{Atom, Constraints, Input, Key, LogicalRect, Sense, Sides, Size, Sizing, WidgetId};
use blit_tui::atom::{Border, TitlePosition};
use blit_tui::cell::{Cell, CellStyle};
use blit_tui::color::Color;
use blit_tui::layout::{flex, Justify};
use blit_tui::text::{Span, TextAttributes};
use blit_tui::widget::{scroll_list, Block, Text, Title};
use blit_tui::{TuiContext, Ui};

use crate::types::{DiveData, DiveLog, DiveMode};

struct App {
    dives: Vec<DiveLog>,
    selected: usize,
    scroll: scroll_list::State,
    show_depth: bool,
    show_temp: bool,
    show_pressure: bool,
    /// Sample picked on the chart, as (dive index, sample index). Set while
    /// the detail panel borrows the dive, hence the cell.
    cursor: StateCell<Option<(usize, usize)>>,
}

impl App {
    fn new(dives: Vec<DiveLog>) -> Self {
        Self {
            dives,
            selected: 0,
            scroll: scroll_list::State::new(),
            show_depth: true,
            show_temp: true,
            show_pressure: true,
            cursor: StateCell::new(None),
        }
    }

    /// Apply one input event. Returns true when the user asked to quit.
    fn handle_input(&mut self, input: Input) -> bool {
        let last = self.dives.len() - 1;
        let previous = self.selected;

        match input {
            Input::Text('q') => return true,
            Input::Text('d') => self.show_depth = !self.show_depth,
            Input::Text('t') => self.show_temp = !self.show_temp,
            Input::Text('p') => self.show_pressure = !self.show_pressure,
            Input::Text('j') => self.selected = (self.selected + 1).min(last),
            Input::Text('k') => self.selected = self.selected.saturating_sub(1),
            Input::Key(key) if key.pressed => match key.key {
                Key::Escape => return true,
                Key::ArrowDown => self.selected = (self.selected + 1).min(last),
                Key::ArrowUp => self.selected = self.selected.saturating_sub(1),
                Key::Home => self.selected = 0,
                Key::End => self.selected = last,
                _ => {}
            },
            _ => {}
        }

        if self.selected != previous {
            self.scroll_to_selected();
        }
        false
    }

    /// Keep the selected row inside the list viewport (rows are one cell high).
    fn scroll_to_selected(&mut self) {
        let row = self.selected as f32;
        let visible = self.scroll.viewport_extent;
        if row < self.scroll.offset {
            self.scroll.scroll_to(row);
        } else if visible > 0.0 && row + 1.0 > self.scroll.offset + visible {
            self.scroll.scroll_to(row + 1.0 - visible);
        }
    }

    fn render(&mut self, mut ui: Ui<'_>) {
        let input = *ui.input();
        if self.handle_input(input) {
            ui.context().quit();
            return;
        }

        let mut root = ui.layout(flex::row());
        root.child()
            .item(flex::item().width(Sizing::Percent(0.3)).height(Sizing::grow()))
            .build(|ui: Ui<'_>| self.render_dive_list(ui));
        root.child()
            .item(flex::item().grow())
            .build(|ui: Ui<'_>| self.render_detail_panel(ui));
    }

    fn render_dive_list(&mut self, ui: Ui<'_>) {
        let mut panel = ui.layout(flex::column().padding(Sides::all(1.0)));
        panel.insert(panel_block(" Dive Log "));

        let selected = self.selected;
        let mut clicked = None;
        panel
            .child()
            .item(flex::item().grow())
            .build(scroll_list::new(
                &mut self.scroll,
                scroll_list::Config::new(1.0),
                self.dives.iter().enumerate(),
                |(_, dive)| WidgetId::new(("dive", dive.number)),
                |mut ui: Ui<'_>, (index, dive)| {
                    if ui.interact(Sense::CLICK).clicked {
                        clicked = Some(index);
                    }
                    let line = format!(
                        " #{:<3} {} {:5.1}m {:3}min {}",
                        dive.number,
                        dive.datetime.format("%Y-%m-%d"),
                        dive.max_depth_m,
                        dive.duration_seconds / 60,
                        mode_short(&dive.dive_mode),
                    );
                    if index == selected {
                        ui.insert(Block::new().background(Color::CYAN));
                        ui.insert(
                            Text::new(&line)
                                .color(Color::BLACK)
                                .attributes(TextAttributes::BOLD),
                        );
                    } else {
                        ui.insert(Text::new(&line));
                    }
                },
                |active| {
                    let thumb = if active { Color::CYAN } else { Color::DARK_GRAY };
                    (Some(Block::new()), Some(Block::new().background(thumb)))
                },
            ));

        if let Some(index) = clicked {
            self.selected = index;
        }
    }

    fn render_detail_panel(&self, ui: Ui<'_>) {
        let dive = &self.dives[self.selected];

        let mut column = ui.layout(flex::column());
        column
            .child()
            .item(flex::item().width(Sizing::grow()).height(Sizing::fixed(8.0)))
            .build(|ui: Ui<'_>| render_dive_info(ui, dive));
        column
            .child()
            .item(flex::item().grow())
            .build(|ui: Ui<'_>| self.render_profile(ui, dive));
    }

    fn render_profile(&self, ui: Ui<'_>, dive: &DiveLog) {
        if !dive.dips.is_empty() {
            render_dips_table(ui, dive);
            return;
        }

        let mut panel = ui.layout(flex::column().padding(Sides::all(1.0)));
        panel.insert(
            panel_block(" Depth Profile ").title(
                Title::new(" d depth  t temp  p pressure  q quit ")
                    .color(Color::DARK_GRAY)
                    .position(TitlePosition::BottomRight),
            ),
        );

        if dive.samples.is_empty() {
            panel.child().insert(Text::new(" No sample data"));
            return;
        }

        let max_time = dive.samples.iter().map(|s| s.time_s).max().unwrap_or(0) as f32 / 60.0;
        let max_depth = dive
            .samples
            .iter()
            .map(|s| s.depth_m)
            .fold(0.0_f64, f64::max);

        // Round up axis bounds for nice labels
        let time_bound = ((max_time / 5.0).ceil() * 5.0).max(5.0);
        let depth_bound = ((max_depth / 5.0).ceil() * 5.0).max(5.0);

        // Pressing or dragging on the chart picks the sample under the pointer.
        // Done before the legend row is built so its readout is not a frame late.
        let chart_id = WidgetId::new("depth chart");
        if panel.interact_widget(chart_id, Sense::CLICK_AND_DRAG).active {
            if let (Some(area), Some(pointer)) = (panel.geometry(chart_id), panel.pointer_position()) {
                let plot_x = area.x + LineChart::GUTTER as f32;
                let plot_width = (area.width - LineChart::GUTTER as f32).max(1.0);
                let time_s = ((pointer.x - plot_x) / plot_width).clamp(0.0, 1.0) * time_bound * 60.0;
                let nearest = dive
                    .samples
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| {
                        let distance = |s: &crate::types::Sample| (s.time_s as f32 - time_s).abs();
                        distance(a).total_cmp(&distance(b))
                    })
                    .map(|(index, _)| index);
                if let Some(index) = nearest {
                    self.cursor.set(Some((self.selected, index)));
                }
            }
        }
        let picked = match self.cursor.get() {
            Some((dive_index, sample)) if dive_index == self.selected => dive.samples.get(sample),
            _ => None,
        };

        let mut series = Vec::new();
        let mut legend = Vec::new();

        // Temperature and pressure share the depth axis: their own range is
        // stretched over it, highest value at the top. Drawn first so the
        // depth curve stays on top.
        let overlay = |value: fn(&crate::types::Sample) -> Option<f64>| {
            let (min, max) = dive
                .samples
                .iter()
                .filter_map(value)
                .fold((f64::MAX, f64::MIN), |(min, max), v| (min.min(v), max.max(v)));
            let range = (max - min).max(1.0);
            let points: Vec<(f32, f32)> = dive
                .samples
                .iter()
                .filter_map(|s| {
                    value(s).map(|v| {
                        (
                            s.time_s as f32 / 60.0,
                            ((max - v) / range * depth_bound) as f32,
                        )
                    })
                })
                .collect();
            (points, min, max)
        };

        if self.show_temp && dive.samples.iter().any(|s| s.temp_c.is_some()) {
            let (points, min, max) = overlay(|s| s.temp_c);
            series.push(Series {
                points,
                color: Color::RED,
            });
            legend.push((format!(" ━ Temp ({min:.0}-{max:.0}C)"), Color::RED));
        }

        if self.show_pressure && dive.samples.iter().any(|s| s.pressure_bar.is_some()) {
            let (points, min, max) = overlay(|s| s.pressure_bar);
            series.push(Series {
                points,
                color: Color::GREEN,
            });
            legend.push((format!(" ━ Press ({max:.0}-{min:.0}bar)"), Color::GREEN));
        }

        if self.show_depth {
            series.push(Series {
                points: dive
                    .samples
                    .iter()
                    .map(|s| (s.time_s as f32 / 60.0, s.depth_m as f32))
                    .collect(),
                color: Color::CYAN,
            });
            legend.push((" ━ Depth".to_string(), Color::CYAN));
        }

        let spans: Vec<Span<'_>> = legend
            .iter()
            .rev()
            .map(|(label, color)| Span::new(label).color(*color))
            .collect();
        let readout: Vec<(String, Color)> = match picked {
            Some(sample) => vec![
                (
                    format!("{:02}:{:02}  ", sample.time_s / 60, sample.time_s % 60),
                    Color::YELLOW,
                ),
                (format!("{:.1} m  ", sample.depth_m), Color::CYAN),
                (
                    sample
                        .pressure_bar
                        .map_or("- bar  ".to_string(), |p| format!("{p:.0} bar  ")),
                    Color::GREEN,
                ),
                (
                    sample
                        .temp_c
                        .map_or("- C ".to_string(), |t| format!("{t:.1} C ")),
                    Color::RED,
                ),
            ],
            None => vec![("click the chart to read values ".to_string(), Color::DARK_GRAY)],
        };
        let readout_spans: Vec<Span<'_>> = readout
            .iter()
            .map(|(text, color)| Span::new(text).color(*color))
            .collect();

        {
            let mut top = panel
                .child()
                .layout(flex::row().justify(Justify::SpaceBetween));
            top.child().insert(Text::rich(&spans));
            top.child().insert(Text::rich(&readout_spans));
        }

        panel
            .child()
            .item(flex::item().grow())
            .widget_id(chart_id)
            .insert(LineChart {
                series,
                x_max: time_bound,
                y_max: depth_bound as f32,
                cursor: picked.map(|sample| sample.time_s as f32 / 60.0),
            });
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

    // blit owns the terminal: raw mode, alternate screen, keyboard and mouse.
    // It needs the kitty keyboard protocol (kitty, ghostty, ...).
    blit_tui::run(|ui| app.render(ui)).context("Terminal UI failed")?;

    Ok(())
}

fn mode_short(mode: &DiveMode) -> &'static str {
    match mode {
        DiveMode::Air => "Air",
        DiveMode::Nitrox => "Nx",
        DiveMode::Gauge => "Gau",
        DiveMode::Freedive => "Free",
    }
}

fn panel_block(title: &str) -> Block<'_> {
    Block::new()
        .border(Border::new(Color::Reset))
        .title(Title::new(title))
}

fn render_dive_info(ui: Ui<'_>, dive: &DiveLog) {
    let duration_min = dive.duration_seconds / 60;
    let duration_sec = dive.duration_seconds % 60;

    let gas_str = dive
        .gas_mixes
        .iter()
        .map(|g| format!("{}% O2", g.o2))
        .collect::<Vec<_>>()
        .join(", ");

    // Temperature range from samples, or from the dips of a freedive session
    let (temp_min, temp_max) = dive
        .samples
        .iter()
        .filter_map(|s| s.temp_c)
        .chain(dive.dips.iter().filter_map(|d| d.min_temp_c))
        .fold((f64::MAX, f64::MIN), |(min, max), t| {
            (min.min(t), max.max(t))
        });

    // Pressure: first and last non-None values
    let pressure_start = dive.samples.iter().find_map(|s| s.pressure_bar);
    let pressure_end = dive.samples.iter().rev().find_map(|s| s.pressure_bar);

    let mut left_col: Vec<String> = vec![
        format!(" Date:      {}", dive.datetime.format("%Y-%m-%d %H:%M")),
        format!(" Duration:  {:02}:{:02}", duration_min, duration_sec),
        format!(" Max depth: {:.1} m", dive.max_depth_m),
    ];

    let mut right_col: Vec<String> = vec![if dive.dips.is_empty() {
        format!(" Gas:       {}", gas_str)
    } else {
        format!(" Dips:      {}", dive.dips.len())
    }];

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

    let mut panel = ui.layout(flex::column().padding(Sides::all(1.0)));
    panel.insert(panel_block(" Dive Details "));

    // Title line
    let number = format!("  Dive #{}  ", dive.number);
    let mode = format!("{:?}", dive.dive_mode);
    let count = if dive.dips.is_empty() {
        format!("    ({} samples)", dive.samples.len())
    } else {
        format!("    ({} dips)", dive.dips.len())
    };
    panel.child().insert(Text::rich(&[
        Span::new(&number)
            .color(Color::CYAN)
            .attributes(TextAttributes::BOLD),
        Span::new(&mode).color(Color::YELLOW),
        Span::new(&count),
    ]));

    // Two columns of fields, each taking half of the width
    let mut columns = panel.child().item(flex::item().grow()).layout(flex::row());
    for lines in [&left_col, &right_col] {
        let mut column = columns
            .child()
            .item(flex::item().grow())
            .layout(flex::column());
        for line in lines {
            column.child().insert(Text::new(line));
        }
    }
}

/// Freedive sessions have no depth samples: list the dips instead of a chart.
fn render_dips_table(ui: Ui<'_>, dive: &DiveLog) {
    let mut panel = ui.layout(flex::column().padding(Sides::all(1.0)));
    panel.insert(panel_block(" Dips "));

    panel.child().insert(
        Text::new("   #   Start   Depth    Time   Surface    Temp").attributes(TextAttributes::BOLD),
    );

    // Start times are rebuilt by adding up surface and dive times
    let mut start_s = 0;
    for (i, dip) in dive.dips.iter().enumerate() {
        start_s += dip.surface_s;
        let temp = dip
            .min_temp_c
            .map(|t| format!("{t:.1} C"))
            .unwrap_or_default();
        panel.child().insert(Text::new(&format!(
            "  {:>2}  {:>3}:{:02}  {:>5.1} m  {:>2}:{:02}   {:>3}:{:02}   {:>6}",
            i + 1,
            start_s / 60,
            start_s % 60,
            dip.max_depth_m,
            dip.duration_s / 60,
            dip.duration_s % 60,
            dip.surface_s / 60,
            dip.surface_s % 60,
            temp,
        )));
        start_s += dip.duration_s;
    }
}

/// One curve of a [`LineChart`]: (minutes, metres below the surface).
struct Series {
    points: Vec<(f32, f32)>,
    color: Color,
}

/// Braille line chart with the surface at the top. blit has no chart atom.
struct LineChart {
    series: Vec<Series>,
    x_max: f32,
    y_max: f32,
    /// Time of the picked sample (minutes), marked with a vertical line
    cursor: Option<f32>,
}

impl LineChart {
    /// Columns kept on the left for the depth labels and the axis.
    const GUTTER: usize = 5;
}

impl Atom<TuiContext> for LineChart {
    fn measure(&self, _: &mut TuiContext, constraints: Constraints) -> Size {
        constraints.constrain(Size::ZERO)
    }

    fn paint(&self, context: &mut TuiContext, area: LogicalRect) {
        let mut cells = context.cells(area);
        if cells.columns() <= Self::GUTTER + 2 || cells.rows() <= 3 {
            return;
        }

        // The last two rows hold the time axis and its labels
        let width = cells.columns() - Self::GUTTER;
        let height = cells.rows() - 2;
        let axis = CellStyle::new().foreground(Color::GRAY);

        for y in 0..height {
            cells.set_cell(Self::GUTTER - 1, y, Cell::new('│').style(axis));
        }
        cells.set_cell(Self::GUTTER - 1, height, Cell::new('└').style(axis));
        for x in 0..width {
            cells.set_cell(Self::GUTTER + x, height, Cell::new('─').style(axis));
        }

        cells.write(0, 0, "  0m", axis);
        cells.write(0, height / 2, &format!("{:>3.0}m", self.y_max / 2.0), axis);
        cells.write(0, height - 1, &format!("{:>3.0}m", self.y_max), axis);

        let middle = format!("{:.0}", self.x_max / 2.0);
        let end = format!("{:.0} min", self.x_max);
        cells.write(Self::GUTTER, height + 1, "0", axis);
        cells.write(Self::GUTTER + (width - middle.len()) / 2, height + 1, &middle, axis);
        cells.write(Self::GUTTER + width.saturating_sub(end.len()), height + 1, &end, axis);

        // Each cell is a 2x4 grid of braille dots; later series win the color
        let (dots_x, dots_y) = (width * 2, height * 4);
        let mut grid = vec![(0u8, Color::Reset); width * height];
        for series in &self.series {
            let to_dot = |&(x, y): &(f32, f32)| {
                let x = (x / self.x_max).clamp(0.0, 1.0) * (dots_x - 1) as f32;
                let y = (y / self.y_max).clamp(0.0, 1.0) * (dots_y - 1) as f32;
                (x.round() as i32, y.round() as i32)
            };
            let mut plot = |x: i32, y: i32| {
                const DOTS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
                let (x, y) = (x as usize, y as usize);
                let cell = &mut grid[(y / 4) * width + x / 2];
                cell.0 |= DOTS[y % 4][x % 2];
                cell.1 = series.color;
            };

            let mut dots = series.points.iter().map(to_dot);
            let Some(mut from) = dots.next() else {
                continue;
            };
            plot(from.0, from.1);
            for to in dots {
                // Bresenham between consecutive samples
                let (dx, dy) = ((to.0 - from.0).abs(), -(to.1 - from.1).abs());
                let (sx, sy) = ((to.0 - from.0).signum(), (to.1 - from.1).signum());
                let mut error = dx + dy;
                while from != to {
                    let doubled = 2 * error;
                    if doubled >= dy {
                        error += dy;
                        from.0 += sx;
                    }
                    if doubled <= dx {
                        error += dx;
                        from.1 += sy;
                    }
                    plot(from.0, from.1);
                }
            }
        }

        if let Some(cursor) = self.cursor {
            let dot = (cursor / self.x_max).clamp(0.0, 1.0) * (dots_x - 1) as f32;
            let column = dot.round() as usize / 2;
            let marker = Cell::new('│').style(CellStyle::new().foreground(Color::YELLOW));
            for y in 0..height {
                cells.set_cell(Self::GUTTER + column, y, marker);
            }
        }

        for (index, &(dots, color)) in grid.iter().enumerate() {
            if dots != 0 {
                let braille = char::from_u32(0x2800 + dots as u32).unwrap_or(' ');
                cells.set_cell(
                    Self::GUTTER + index % width,
                    index / width,
                    Cell::new(braille).style(CellStyle::new().foreground(color)),
                );
            }
        }
    }

    fn paint_bounds(&self, area: LogicalRect) -> LogicalRect {
        area
    }
}
