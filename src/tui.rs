use std::cell::Cell as StateCell;
use std::path::PathBuf;

use anyhow::{Context, Result};
use blit::{Atom, Constraints, Input, Key, LogicalRect, Sense, Sides, Size, Sizing, WidgetId};
use blit_tui::atom::{Border, TitlePosition};
use blit_tui::cell::{Cell, CellStyle};
use blit_tui::color::Color;
use blit_tui::layout::{flex, Justify};
use blit_tui::text::{Span, TextAttributes, TextOptions, TextOverflow, TextWrap};
use blit_tui::widget::{scroll_list, Block, Text, Title};
use blit_tui::{TuiContext, Ui};

use crate::types::{DiveData, DiveLog, DiveMode, SafetyStop, Sample, Water};

struct App {
    /// The file the dives come from, written back when one is ignored
    path: PathBuf,
    /// The dives, in the order of the file
    dives: Vec<DiveLog>,
    /// Indexes of the dives in the list, most recent first
    listed: Vec<usize>,
    /// Row of the list that is selected
    selected: usize,
    scroll: scroll_list::State,
    show_depth: bool,
    show_temp: bool,
    show_pressure: bool,
    /// Whether the list has the ignored dives too
    show_ignored: bool,
    /// What went wrong when the file was last written
    error: Option<String>,
    /// Sample picked on the chart, as (dive index, sample index). Set while
    /// the detail panel borrows the dive, hence the cell.
    cursor: StateCell<Option<(usize, usize)>>,
}

impl App {
    fn new(path: PathBuf, dives: Vec<DiveLog>) -> Self {
        let mut app = Self {
            path,
            dives,
            listed: Vec::new(),
            selected: 0,
            scroll: scroll_list::State::new(),
            show_depth: true,
            show_temp: true,
            show_pressure: true,
            show_ignored: false,
            error: None,
            cursor: StateCell::new(None),
        };
        app.list_dives(None);
        app
    }

    /// Index of the selected dive, if the list has any.
    fn current(&self) -> Option<usize> {
        self.listed.get(self.selected).copied()
    }

    /// Rebuild the list: most recent dive first, without the ignored ones
    /// unless they are asked for. `keep` is the dive to leave selected; when
    /// it is no longer listed, the selection stays on its row.
    fn list_dives(&mut self, keep: Option<usize>) {
        self.listed = (0..self.dives.len())
            .filter(|&dive| self.show_ignored || !self.dives[dive].ignored)
            .collect();
        self.listed
            .sort_by_key(|&dive| std::cmp::Reverse(self.dives[dive].number));

        let row = keep.and_then(|keep| self.listed.iter().position(|&dive| dive == keep));
        self.selected = row
            .unwrap_or(self.selected)
            .min(self.listed.len().saturating_sub(1));
        self.scroll_to_selected();
    }

    /// Ignore the selected dive, or take it back, and write the file.
    fn toggle_ignored(&mut self) {
        let Some(dive) = self.current() else {
            return;
        };
        self.dives[dive].ignored = !self.dives[dive].ignored;
        self.error = self.save().err().map(|error| format!("{error:#}"));
        if self.error.is_some() {
            // Not written: do not show what the file does not hold
            self.dives[dive].ignored = !self.dives[dive].ignored;
        }
        self.list_dives(Some(dive));
    }

    /// Write the dives back to the file they were read from.
    fn save(&mut self) -> Result<()> {
        let data = DiveData {
            dives: std::mem::take(&mut self.dives),
        };
        let json = serde_json::to_string_pretty(&data);
        self.dives = data.dives;

        // Through a temporary file: an interrupted write must not cost the log
        let mut temporary = self.path.clone().into_os_string();
        temporary.push(".tmp");
        std::fs::write(&temporary, json?)
            .and_then(|()| std::fs::rename(&temporary, &self.path))
            .with_context(|| format!("Failed to write {}", self.path.display()))
    }

    /// Apply one input event. Returns true when the user asked to quit.
    fn handle_input(&mut self, input: Input) -> bool {
        let last = self.listed.len().saturating_sub(1);
        let previous = self.selected;

        match input {
            Input::Text('q') => return true,
            Input::Text('i') => self.toggle_ignored(),
            Input::Text('a') => {
                self.show_ignored = !self.show_ignored;
                self.list_dives(self.current());
            }
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
        let ignored = self.dives.iter().filter(|dive| dive.ignored).count();
        let hint = match (self.show_ignored, ignored) {
            (true, _) => " i ignore  a hide ignored ".to_string(),
            (false, 0) => " i ignore ".to_string(),
            (false, count) => format!(" i ignore  a show {count} ignored "),
        };

        let mut panel = ui.layout(flex::column().padding(Sides::all(1.0)));
        panel.insert(
            panel_block(" Dive Log ").title(
                Title::new(&hint)
                    .color(Color::DARK_GRAY)
                    .position(TitlePosition::BottomRight),
            ),
        );

        let selected = self.selected;
        let mut clicked = None;
        panel
            .child()
            .item(flex::item().grow())
            .build(scroll_list::new(
                &mut self.scroll,
                scroll_list::Config::new(1.0),
                self.listed
                    .iter()
                    .map(|&dive| &self.dives[dive])
                    .enumerate(),
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
                    } else if dive.ignored {
                        ui.insert(Text::new(&line).color(Color::DARK_GRAY));
                    } else {
                        ui.insert(Text::new(&line));
                    }
                },
                |active| {
                    let thumb = if active { Color::CYAN } else { Color::DARK_GRAY };
                    (Some(Block::new()), Some(Block::new().background(thumb)))
                },
            ));

        // Under the list, over as many lines as it takes
        if let Some(error) = &self.error {
            panel
                .child()
                .item(flex::item().width(Sizing::grow()).height(Sizing::fit()))
                .insert(
                    Text::new(error)
                        .color(Color::RED)
                        .options(TextOptions::new().wrap(TextWrap::Word)),
                );
        }

        if let Some(index) = clicked {
            self.selected = index;
        }
    }

    fn render_detail_panel(&self, ui: Ui<'_>) {
        let Some(index) = self.current() else {
            let mut panel = ui.layout(flex::column().padding(Sides::all(1.0)));
            panel.insert(panel_block(" Dive Details "));
            panel
                .child()
                .insert(Text::new(" Every dive is ignored: press a to list them"));
            return;
        };
        let dive = &self.dives[index];

        let mut column = ui.layout(flex::column());
        column
            .child()
            .item(flex::item().width(Sizing::grow()).height(Sizing::fit()))
            .build(|ui: Ui<'_>| render_dive_info(ui, dive));
        column
            .child()
            .item(flex::item().grow())
            .build(|ui: Ui<'_>| self.render_profile(ui, index, dive));
    }

    /// `index` is that of `dive` among the dives, to tell whose sample is picked.
    fn render_profile(&self, ui: Ui<'_>, index: usize, dive: &DiveLog) {
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
                if let Some(sample) = nearest {
                    self.cursor.set(Some((index, sample)));
                }
            }
        }
        let picked = match self.cursor.get() {
            Some((dive_index, sample)) if dive_index == index => dive.samples.get(sample),
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
                    // Time into the dive, then the time of day
                    format!(
                        "{:02}:{:02} ({})  ",
                        sample.time_s / 60,
                        sample.time_s % 60,
                        (dive.datetime + chrono::Duration::seconds(sample.time_s as i64))
                            .format("%H:%M:%S"),
                    ),
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
        // Two spaces first, to stay clear of the legend
        let readout_spans: Vec<Span<'_>> = std::iter::once(Span::new("  "))
            .chain(
                readout
                    .iter()
                    .map(|(text, color)| Span::new(text).color(*color)),
            )
            .collect();
        let one_line = TextOptions::new()
            .max_lines(1)
            .overflow(TextOverflow::Ellipsis);

        {
            let mut top = panel
                .child()
                .layout(flex::row().justify(Justify::SpaceBetween));
            // The legend gives way when the two do not fit: the readout is
            // what the click asked for
            top.child()
                .item(
                    flex::item()
                        .width(Sizing::grow())
                        .height(Sizing::fixed(1.0)),
                )
                .insert(Text::rich(&spans).options(one_line));
            top.child().insert(Text::rich(&readout_spans));
        }

        // Two more rows for what the newer logs hold at each sample. Kept
        // even with no sample picked, so that the chart does not move.
        if dive.samples[0].speed_m_min.is_some() {
            let [state, signals, alarms] = picked.map(sample_readout).unwrap_or_default();
            let row = || {
                flex::item()
                    .width(Sizing::grow())
                    .height(Sizing::fixed(1.0))
            };
            panel
                .child()
                .item(row())
                .insert(Text::new(&state).options(one_line));
            panel.child().item(row()).insert(
                Text::rich(&[
                    Span::new(&signals),
                    Span::new(&alarms).color(Color::MAGENTA),
                ])
                .options(one_line),
            );
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

    let mut app = App::new(input, data.dives);

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
        .map(|g| match g.he {
            0 => format!("{}% O2", g.o2),
            he => format!("{}% O2 {he}% He", g.o2),
        })
        .collect::<Vec<_>>()
        .join(", ");

    // Temperature range from the header, else from the samples, or from the
    // dips of a freedive session
    let (temp_min, temp_max) = match (dive.min_temp_c, dive.max_temp_c) {
        (Some(min), Some(max)) => (min, max),
        _ => dive
            .samples
            .iter()
            .filter_map(|s| s.temp_c)
            .chain(dive.dips.iter().filter_map(|d| d.min_temp_c))
            .fold((f64::MAX, f64::MIN), |(min, max), t| {
                (min.min(t), max.max(t))
            }),
    };

    // Pressure: per tank from the header, else the first and last samples
    let tanks: Vec<_> = dive
        .gas_mixes
        .iter()
        .filter_map(|g| g.tank.as_ref())
        .collect();
    let pressure = if tanks.is_empty() {
        let start = dive.samples.iter().find_map(|s| s.pressure_bar);
        let end = dive.samples.iter().rev().find_map(|s| s.pressure_bar);
        start
            .zip(end)
            .map(|(start, end)| format!("{start:.0} -> {end:.0} bar"))
    } else {
        Some(
            tanks
                .iter()
                .map(|tank| format!("{:.0} -> {:.0} bar", tank.start_bar, tank.end_bar))
                .collect::<Vec<_>>()
                .join(", "),
        )
    };
    let tank_sizes = tanks
        .iter()
        .filter_map(|tank| tank.volume_l.zip(tank.working_bar))
        .map(|(volume, working)| format!("{volume} l, {working} bar"))
        .collect::<Vec<_>>()
        .join("; ");

    let mut date = format!(" Date:      {}", dive.datetime.format("%Y-%m-%d %H:%M"));
    if let Some(end) = dive.end_datetime {
        date.push_str(&format!(" - {}", end.format("%H:%M")));
    }
    let mut depth = format!(" Max depth: {:.1} m", dive.max_depth_m);
    if let Some(avg) = dive.avg_depth_m {
        depth.push_str(&format!(" (avg {avg:.1} m)"));
    }

    let mut left_col: Vec<String> = vec![
        date,
        format!(" Duration:  {:02}:{:02}", duration_min, duration_sec),
        depth,
    ];

    let mut right_col: Vec<String> = vec![if dive.dips.is_empty() {
        format!(" Gas:       {}", gas_str)
    } else {
        format!(" Dips:      {}", dive.dips.len())
    }];

    if temp_min != f64::MAX {
        right_col.push(format!(" Temp:      {:.1} - {:.1} C", temp_min, temp_max));
    }

    if let Some(pressure) = pressure {
        right_col.push(format!(" Pressure:  {pressure}"));
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

    // 0 when the watch counts the dive as the first
    if let Some(interval) = dive.surface_interval_s.filter(|&s| s > 0) {
        left_col.push(format!(
            " Surface:   {}:{:02} h since last dive",
            interval / 3600,
            interval % 3600 / 60
        ));
    }
    let water = dive.water.map(|water| match water {
        Water::Fresh => "fresh".to_string(),
        Water::Salt => "salt".to_string(),
        Water::En13319 => "EN 13319".to_string(),
    });
    let atmospheric = dive.atmospheric_mbar.map(|mbar| format!("{mbar} mbar"));
    if water.is_some() || atmospheric.is_some() {
        let parts: Vec<String> = water.into_iter().chain(atmospheric).collect();
        left_col.push(format!(" Water:     {}", parts.join(", ")));
    }
    if let (Some(start), Some(end)) = (dive.battery_start_pct, dive.battery_end_pct) {
        left_col.push(format!(" Battery:   {start} -> {end} %"));
    }

    if !tank_sizes.is_empty() {
        right_col.push(format!(" Tank:      {tank_sizes}"));
    }
    if !dive.gradient_factors.is_empty() {
        let sets: Vec<String> = dive
            .gradient_factors
            .iter()
            .map(|gf| format!("{}/{}", gf.low, gf.high))
            .collect();
        right_col.push(format!(" GF:        {}", sets.join(", ")));
    }
    if let (Some(start), Some(end)) = (dive.cns_start_pct, dive.cns_end_pct) {
        right_col.push(format!(" CNS:       {start:.1} -> {end:.1} %"));
    }
    if let (Some(start), Some(end)) = (dive.otu_start, dive.otu_end) {
        right_col.push(format!(" OTU:       {start:.1} -> {end:.1}"));
    }
    if let Some(speed) = dive.max_ascent_speed_m_min {
        right_col.push(format!(" Ascent:    {speed:.1} m/min max"));
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
        Span::new(if dive.ignored { "    ignored" } else { "" }).color(Color::MAGENTA),
    ]));

    // Two columns of fields, each taking half of the width
    let fit = || flex::item().width(Sizing::grow()).height(Sizing::fit());
    {
        let mut columns = panel.child().item(fit()).layout(flex::row());
        for lines in [&left_col, &right_col] {
            let mut column = columns.child().item(fit()).layout(flex::column());
            for line in lines {
                column.child().insert(Text::new(line));
            }
        }
    }

    // The alarms take the full width, and as many lines as they need
    if !dive.alarms.is_empty() {
        let alarms = dive.alarms.join(", ").replace('_', " ");
        let mut row = panel.child().item(fit()).layout(flex::row());
        row.child().insert(Text::new(" Alarms:    "));
        row.child().item(fit()).insert(
            Text::new(&alarms)
                .color(Color::MAGENTA)
                .options(TextOptions::new().wrap(TextWrap::Word)),
        );
    }

    let tissues = [("N2", &dive.tissue_n2_mbar), ("He", &dive.tissue_he_mbar)];
    for (i, (gas, pressures)) in tissues.into_iter().enumerate() {
        if pressures.is_empty() {
            continue;
        }
        let (min, max) = pressures
            .iter()
            .fold((f64::MAX, f64::MIN), |(min, max), &p| {
                (min.min(p), max.max(p))
            });
        let range = if max > min {
            format!("{min:.0}-{max:.0}")
        } else {
            format!("{min:.0}")
        };
        panel.child().insert(Text::new(&format!(
            " {:<11}{}  {gas} {range} mbar at the start",
            if i == 0 { "Tissues:" } else { "" },
            tissue_bars(pressures),
        )));
    }
}

/// One bar per tissue compartment, from the least to the most loaded.
fn tissue_bars(pressures: &[f64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let (min, max) = pressures
        .iter()
        .fold((f64::MAX, f64::MIN), |(min, max), &p| {
            (min.min(p), max.max(p))
        });
    pressures
        .iter()
        .map(|&p| {
            let level = if max > min {
                (p - min) / (max - min)
            } else {
                0.0
            };
            BARS[(level * 7.0).round() as usize]
        })
        .collect()
}

/// What the newer logs hold at a sample, as three pieces of text: where the
/// dive stands, what the watch works out, and the alarms it raises.
fn sample_readout(sample: &Sample) -> [String; 3] {
    let mut state = Vec::new();
    if let Some(speed) = sample.speed_m_min {
        let arrow = match speed {
            s if s > 0.0 => '↑',
            s if s < 0.0 => '↓',
            _ => '→',
        };
        state.push(format!("{arrow} {:.1} m/min", speed.abs()));
    }
    if let Some(ndl) = sample.ndl_min {
        state.push(format!("NDL {ndl} min"));
    }
    if let Some(time) = sample.deco_time_min {
        state.push(match sample.deco_stop_m {
            Some(stop) => format!("deco {time} min at {stop} m"),
            None => format!("deco {time} min"),
        });
    }
    if let (Some(now), Some(surface)) = (sample.gf_pct, sample.surface_gf_pct) {
        state.push(format!("GF {now:.0} %, surface {surface:.0} %"));
    }
    if let Some(ambient) = sample.ambient_mbar {
        state.push(format!("{ambient} mbar"));
    }

    let mut signals = Vec::new();
    if let Some(minutes) = sample.gas_time_min {
        signals.push(format!("gas for {minutes} min"));
    }
    if let Some(sac) = sample.sac_l_min {
        signals.push(format!("{sac} l/min"));
    }
    if let Some(stop) = sample.safety_stop {
        signals.push(format!(
            "safety stop {}",
            match stop {
                SafetyStop::Due => "due",
                SafetyStop::Running => "running",
                SafetyStop::Paused => "paused",
            }
        ));
    }
    if sample.gas > 0 {
        signals.push(format!("mix {}", sample.gas + 1));
    }
    if sample.gf_set > 0 {
        signals.push(format!("GF set {}", sample.gf_set + 1));
    }
    if sample.bookmark > 0 {
        signals.push(format!("bookmark {}", sample.bookmark));
    }

    let mut signals = format!(" {}", signals.join("  "));
    let alarms = sample.alarms.join(", ").replace('_', " ");
    if !alarms.is_empty() && signals.len() > 1 {
        signals.push_str("  ");
    }
    [format!(" {}", state.join("  ")), signals, alarms]
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use blit::{Frame, FrameInfo, LayoutResolution, LogicalSize};
    use blit_tui::{RendererConfig, TuiRenderer};

    use super::*;
    use crate::types::{GasMix, GradientFactors, Tank};

    /// Render the viewer off-screen and return what the terminal would show.
    fn screen(app: &mut App, columns: u16, rows: u16) -> String {
        let renderer = TuiRenderer::new(RendererConfig::new().columns(columns).rows(rows));
        let mut context = TuiContext::new(renderer);
        let mut frame: Frame<TuiContext> = Frame::default();
        let info = FrameInfo::new(LogicalSize::new(columns as f32, rows as f32)).layout_resolution(
            LayoutResolution::Discrete {
                step: LogicalSize::uniform(1.0),
            },
        );
        // Twice: the list and the chart place themselves from the frame before
        for pass in 0..2 {
            let now = Duration::from_millis(pass * 10);
            frame.build(&mut context, info, now, Input::None, |ui: Ui<'_>| {
                app.render(ui)
            });
            frame.layout(&mut context);
        }
        context.begin_paint();
        frame.paint(&mut context);
        context.finish_paint();
        context.renderer().plain_text()
    }

    fn sample(time_s: u32, depth_m: f64) -> Sample {
        Sample {
            time_s,
            depth_m,
            temp_c: Some(25.4),
            ..Default::default()
        }
    }

    /// A dive as an older log holds it: depth, temperature, tank pressure.
    fn plain_dive() -> DiveLog {
        DiveLog {
            number: 44,
            datetime: chrono::NaiveDate::from_ymd_opt(2025, 8, 1)
                .unwrap()
                .and_hms_opt(16, 3, 0)
                .unwrap(),
            duration_seconds: 2990,
            max_depth_m: 31.2,
            gas_mixes: vec![GasMix {
                o2: 21,
                ..Default::default()
            }],
            samples: vec![sample(0, 2.0), sample(5, 25.6), sample(10, 32.9)],
            site: Some("Blue Hole".to_string()),
            ..Default::default()
        }
    }

    /// The same dive with what the watch logs besides.
    fn full_dive() -> DiveLog {
        let mut dive = plain_dive();
        dive.end_datetime = Some(dive.datetime + chrono::Duration::minutes(52));
        dive.avg_depth_m = Some(16.2);
        dive.min_temp_c = Some(20.4);
        dive.max_temp_c = Some(26.1);
        dive.water = Some(Water::Salt);
        dive.atmospheric_mbar = Some(1048);
        dive.surface_interval_s = Some(16180);
        dive.cns_start_pct = Some(0.21);
        dive.cns_end_pct = Some(5.65);
        dive.otu_start = Some(0.54);
        dive.otu_end = Some(14.62);
        dive.gradient_factors = vec![
            GradientFactors { low: 85, high: 85 },
            GradientFactors { low: 95, high: 95 },
        ];
        dive.max_ascent_speed_m_min = Some(14.9);
        dive.battery_start_pct = Some(58);
        dive.battery_end_pct = Some(53);
        dive.alarms = vec!["slow_down".to_string(), "tank_lost_link".to_string()];
        dive.tissue_n2_mbar = (0..16).map(|i| 755.0 + 15.0 * i as f64).collect();
        dive.gas_mixes[0].tank = Some(Tank {
            start_bar: 209.1,
            end_bar: 99.0,
            volume_l: Some(12),
            working_bar: Some(200),
        });

        let bottom = &mut dive.samples[1];
        bottom.pressure_bar = Some(190.0);
        bottom.ambient_mbar = Some(3629);
        bottom.speed_m_min = Some(-3.2);
        bottom.ndl_min = Some(17);
        bottom.gf_pct = Some(-42.1);
        bottom.surface_gf_pct = Some(23.6);
        bottom.safety_stop = Some(SafetyStop::Due);
        bottom.gas_time_min = Some(28);
        bottom.sac_l_min = Some(18);

        let deco = &mut dive.samples[2];
        deco.speed_m_min = Some(7.3);
        deco.deco_time_min = Some(1);
        deco.deco_stop_m = Some(3);
        deco.gf_pct = Some(-25.9);
        deco.surface_gf_pct = Some(89.8);
        deco.alarms = vec!["slow_down".to_string(), "nodeco_deco".to_string()];

        dive.samples[0].speed_m_min = Some(0.0);
        dive
    }

    /// Row of the screen on which `text` shows.
    fn row_of(screen: &str, text: &str) -> usize {
        let row = screen.lines().position(|line| line.contains(text));
        row.unwrap_or_else(|| panic!("{text:?} is not on the screen:\n{screen}"))
    }

    #[test]
    fn details_show_what_the_watch_logs_about_the_dive() {
        let mut app = App::new(PathBuf::new(), vec![full_dive()]);
        let screen = screen(&mut app, 120, 36);

        for text in [
            "Date:      2025-08-01 16:03 - 16:55",
            "Max depth: 31.2 m (avg 16.2 m)",
            "Surface:   4:29 h since last dive",
            "Water:     salt, 1048 mbar",
            "Battery:   58 -> 53 %",
            "Temp:      20.4 - 26.1 C",
            "Pressure:  209 -> 99 bar",
            "Tank:      12 l, 200 bar",
            "GF:        85/85, 95/95",
            "CNS:       0.2 -> 5.7 %",
            "OTU:       0.5 -> 14.6",
            "Ascent:    14.9 m/min max",
            "Alarms:    slow down, tank lost link",
            "Tissues:   ▁▁▂▂▃▃▄▄▅▅▆▆▇▇██  N2 755-980 mbar at the start",
        ] {
            row_of(&screen, text);
        }
    }

    #[test]
    fn readout_shows_what_the_watch_logs_at_the_picked_sample() {
        let mut app = App::new(PathBuf::new(), vec![full_dive()]);

        app.cursor.set(Some((0, 1)));
        let bottom = screen(&mut app, 120, 36);
        let legend = row_of(&bottom, "00:05 (16:03:05)  25.6 m  190 bar  25.4 C");
        assert_eq!(
            row_of(
                &bottom,
                "↓ 3.2 m/min  NDL 17 min  GF -42 %, surface 24 %  3629 mbar"
            ),
            legend + 1
        );
        assert_eq!(
            row_of(&bottom, "gas for 28 min  18 l/min  safety stop due"),
            legend + 2
        );

        app.cursor.set(Some((0, 2)));
        let deco = screen(&mut app, 120, 36);
        row_of(
            &deco,
            "↑ 7.3 m/min  deco 1 min at 3 m  GF -26 %, surface 90 %",
        );
        row_of(&deco, " slow down, nodeco deco");
    }

    #[test]
    fn dive_from_an_older_log_keeps_its_short_panel() {
        let mut app = App::new(PathBuf::new(), vec![plain_dive()]);
        app.cursor.set(Some((0, 1)));
        let screen = screen(&mut app, 120, 36);

        // Title, three rows of fields, borders: the chart panel comes next
        assert_eq!(row_of(&screen, "Site:      Blue Hole"), 5);
        assert_eq!(row_of(&screen, "Depth Profile"), 7);
        // and the chart right under the legend, with no readout rows between
        let legend = row_of(&screen, "00:05 (16:03:05)  25.6 m  - bar  25.4 C");
        assert!(screen.lines().nth(legend + 1).unwrap().contains(" 0m│"));
    }

    /// Three dives numbered 1 to 3, in the order of a file.
    fn three_dives() -> Vec<DiveLog> {
        (1..=3)
            .map(|number| DiveLog {
                number,
                ..plain_dive()
            })
            .collect()
    }

    #[test]
    fn ignored_dives_are_listed_only_on_request() {
        let mut dives = three_dives();
        dives[1].ignored = true;
        let mut app = App::new(PathBuf::new(), dives);

        let hidden = screen(&mut app, 120, 36);
        assert!(row_of(&hidden, "#3 ") < row_of(&hidden, "#1 "));
        assert!(!hidden.contains("#2 "));
        row_of(&hidden, "i ignore  a show 1 ignored");

        app.handle_input(Input::Text('a'));
        let all = screen(&mut app, 120, 36);
        assert_eq!(row_of(&all, "#2 "), row_of(&all, "#3 ") + 1);
        row_of(&all, "i ignore  a hide ignored");
        // The selection stayed on the most recent dive
        row_of(&all, "Dive #3");

        // The ignored dive says so when it is the one shown
        app.handle_input(Input::Text('j'));
        row_of(
            &screen(&mut app, 120, 36),
            "Dive #2  Air    (3 samples)    ignored",
        );
    }

    #[test]
    fn ignoring_a_dive_writes_it_to_the_file() {
        let path = std::env::temp_dir().join(format!("sirius-dive-{}.json", std::process::id()));
        let read = |path: &PathBuf| -> Vec<(u32, bool)> {
            let data: DiveData =
                serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            data.dives
                .iter()
                .map(|dive| (dive.number, dive.ignored))
                .collect()
        };
        let mut app = App::new(path.clone(), three_dives());

        // The list starts on the most recent dive: ignore it
        app.handle_input(Input::Text('i'));
        // The file keeps its order and every dive, the ignored one marked
        assert_eq!(read(&path), [(1, false), (2, false), (3, true)]);
        let text = screen(&mut app, 120, 36);
        assert!(!text.contains("#3 "));
        // and the next dive takes its place
        row_of(&text, "Dive #2");

        // Listed again, the dive can be taken back
        app.handle_input(Input::Text('a'));
        app.handle_input(Input::Text('k'));
        app.handle_input(Input::Text('i'));
        assert_eq!(read(&path), [(1, false), (2, false), (3, false)]);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn dive_stays_when_the_file_cannot_be_written() {
        let path = PathBuf::from("/nonexistent/dives.json");
        let mut app = App::new(path, three_dives());

        app.handle_input(Input::Text('i'));
        assert!(!app.dives[2].ignored);
        let text = screen(&mut app, 120, 36);
        row_of(&text, "#3 ");
        // The reason shows under the list, on a few lines
        let error = row_of(&text, "Failed to write");
        assert!(error > row_of(&text, "#1 "));
        row_of(&text, "(os error 2)");
    }

    #[test]
    fn list_can_be_left_empty() {
        let mut dives = three_dives();
        dives.truncate(1);
        dives[0].ignored = true;
        let mut app = App::new(PathBuf::new(), dives);

        // Moving about an empty list does nothing
        app.handle_input(Input::Text('j'));
        app.handle_input(Input::Text('i'));
        row_of(
            &screen(&mut app, 120, 36),
            "Every dive is ignored: press a to list them",
        );
    }
}
