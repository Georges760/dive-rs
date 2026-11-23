//! `gui`: the dives in a window, as `view` shows them in the terminal.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use blit::{
    AnimationOptions, Atom, AtomCx, Constraints, Id, Input, Key, LogicalRect, Sides, Size, Tween,
};
use blit_desktop::{cpu, Application, Config, DesktopContext, EventLoopProxy, Root, Session, Ui};
use blit_gui::atom::Rectangle;
use blit_gui::color::Color;
use blit_gui::display_list::{Mesh, MeshVertex, Rectangle as DrawRectangle};
use blit_gui::style::{Border, BorderRadius};
use blit_gui::text::{
    FontId, HorizontalAlign, Span, SpanStyle, TextOptions, TextOverflow, TextRequest, TextRun,
    TextStyle, TextWrap, VerticalAlign,
};
use blit_gui::{BoundsClip, Family, TextConfig};
use blit_gui_widgets::layout::{flex, Align, Justify, Sizing};
use blit_gui_widgets::{interact, RichText, ScrollArea, ScrollState, Text, UniformList};

use crate::stats::{stats, Period, Stats};
use crate::types::{DiveLog, DiveMode};
use crate::viewer::{
    alarm_text, computer_numbers, dip_rows, dive_fields, hours, maps_url, mode_short,
    nearest_sample, range, sample_readout, sample_values, stats_summary, tissue_range, Logbook,
    Overlay, Profile,
};

/// Deep water, bright curves.
mod colors {
    use blit_gui::color::Color;

    pub const BACKGROUND: Color = Color::rgb(11, 17, 24);
    pub const PANEL: Color = Color::rgb(18, 27, 37);
    pub const RAISED: Color = Color::rgb(28, 41, 55);
    pub const SELECTED: Color = Color::rgb(24, 70, 96);
    pub const BORDER: Color = Color::rgb(38, 54, 71);
    pub const GRID: Color = Color::rgb(30, 44, 58);
    pub const TEXT: Color = Color::rgb(228, 236, 243);
    pub const MUTED: Color = Color::rgb(128, 147, 166);
    pub const DEPTH: Color = Color::rgb(76, 201, 240);
    pub const TEMP: Color = Color::rgb(255, 107, 107);
    pub const PRESSURE: Color = Color::rgb(105, 219, 124);
    /// Each tank of a sidemount pair, PRESSURE being their total
    pub const PRESSURE1: Color = Color::rgb(192, 235, 117);
    pub const PRESSURE2: Color = Color::rgb(255, 236, 153);
    pub const TIME: Color = Color::rgb(255, 209, 102);
    pub const ALARM: Color = Color::rgb(247, 131, 172);
    pub const LINK: Color = Color::rgb(116, 192, 252);
    pub const SCUBA: Color = Color::rgb(255, 209, 102);
    pub const FREEDIVE: Color = Color::rgb(76, 201, 240);
    /// Water under the surface of the chart, then down at the curve
    pub const SURFACE_WATER: Color = Color::rgba(76, 201, 240, 18);
    pub const DEEP_WATER: Color = Color::rgba(40, 120, 200, 90);
    /// The countries of the statistics page, the most dived first
    pub const COUNTRIES: [Color; 8] = [
        Color::rgb(255, 209, 102),
        Color::rgb(76, 201, 240),
        Color::rgb(105, 219, 124),
        Color::rgb(247, 131, 172),
        Color::rgb(255, 146, 92),
        Color::rgb(116, 143, 252),
        Color::rgb(169, 227, 75),
        Color::rgb(102, 217, 232),
    ];
}

/// The first of these installed shows the text.
const FONTS: [&str; 6] = [
    "Inter",
    "Noto Sans",
    "DejaVu Sans",
    "Liberation Sans",
    "Helvetica Neue",
    "Segoe UI",
];

/// Height of a row of the dive list.
const ROW: f32 = 30.0;

thread_local! {
    /// The logbook of the window about to open: the desktop runner makes
    /// the application itself, with nothing of ours.
    static OPENING: RefCell<Option<Logbook>> = const { RefCell::new(None) };
    /// Set when the user asks to quit; the runner has no way to stop.
    static QUIT: Cell<bool> = const { Cell::new(false) };
}

pub fn run(input: PathBuf) -> Result<()> {
    let Some(logbook) = Logbook::open(input.clone())? else {
        eprintln!("No dives found in {}", input.display());
        return Ok(());
    };
    OPENING.set(Some(logbook));

    let mut session = Session::<App>::new(Config {
        title: format!("dive-rs  {}", input.display()),
        width: 1400,
        height: 900,
        text_config: TextConfig::new().font(FontId::default(), FONTS.map(Family::named)),
        text: blit_text_parley::Backend::new(),
        graphics: Box::new(cpu::Backend::new(cpu::Config::default())),
    })
    .map_err(|error| anyhow!("Could not open a window: {error}"))?;

    while !QUIT.get() {
        let open = session
            .pump(None)
            .map_err(|error| anyhow!("The window failed: {error}"))?;
        if !open {
            break;
        }
    }
    Ok(())
}

/// What a click in the window asks for.
enum Choice {
    /// What the key does
    Key(char),
    /// Select this row of the list
    Row(usize),
    /// Open this page in the browser
    Open(String),
}

struct App {
    logbook: Logbook,
    scroll: ScrollState,
    stats_scroll: ScrollState,
    show_depth: bool,
    show_temp: bool,
    show_pressure: bool,
    /// The figures of the statistics page, while it is the one shown
    stats: Option<Stats>,
    /// Sample picked on the chart, as (dive index, sample index). Set while
    /// the detail panel borrows the dive, hence the cell.
    cursor: Cell<Option<(usize, usize)>>,
    /// How a page opens, in the browser
    open_url: fn(&str),
}

impl Application for App {
    type Input = ();

    fn new(_: EventLoopProxy<()>, _: Root<Self>, _: &mut DesktopContext) -> Self {
        let logbook = OPENING
            .take()
            .expect("the logbook is read before the window opens");
        Self::new(logbook)
    }

    fn input(&mut self, _: ()) {}

    fn render(&mut self, ui: Ui<'_>) {
        App::render(self, ui);
    }
}

impl App {
    fn new(logbook: Logbook) -> Self {
        Self {
            logbook,
            scroll: ScrollState::new(),
            stats_scroll: ScrollState::new(),
            show_depth: true,
            show_temp: true,
            show_pressure: true,
            stats: None,
            cursor: Cell::new(None),
            open_url: open_in_browser,
        }
    }

    /// Apply one input event, the same keys as in the terminal. Returns
    /// true when the user asked to quit.
    fn handle_input(&mut self, input: Input<f32>) -> bool {
        if self.stats.is_some() {
            match input {
                Input::Text('q') => return true,
                Input::Text('s') => self.stats = None,
                Input::Key(key) if key.pressed && key.key == Key::Escape => self.stats = None,
                _ => {}
            }
            return false;
        }

        let logbook = &mut self.logbook;
        let last = logbook.last_row();
        let previous = (logbook.selected, logbook.listed.len());

        match input {
            Input::Text('q') => return true,
            Input::Text('s') => self.stats = Some(stats(&logbook.dives)),
            Input::Text('i') => logbook.toggle_ignored(),
            Input::Text('a') => logbook.toggle_show_ignored(),
            Input::Text('d') => self.show_depth = !self.show_depth,
            Input::Text('t') => self.show_temp = !self.show_temp,
            Input::Text('p') => self.show_pressure = !self.show_pressure,
            Input::Text('j') => logbook.selected = (logbook.selected + 1).min(last),
            Input::Text('k') => logbook.selected = logbook.selected.saturating_sub(1),
            Input::Key(key) if key.pressed => match key.key {
                Key::Escape => return true,
                Key::ArrowDown => logbook.selected = (logbook.selected + 1).min(last),
                Key::ArrowUp => logbook.selected = logbook.selected.saturating_sub(1),
                Key::Home => logbook.selected = 0,
                Key::End => logbook.selected = last,
                _ => {}
            },
            _ => {}
        }

        if (self.logbook.selected, self.logbook.listed.len()) != previous {
            self.scroll_to_selected();
        }
        false
    }

    /// Keep the selected row inside the list viewport.
    fn scroll_to_selected(&mut self) {
        let top = self.logbook.selected as f32 * ROW;
        let visible = self.scroll.viewport_extent;
        let offset = if top < self.scroll.offset {
            top
        } else if visible > 0.0 && top + ROW > self.scroll.offset + visible {
            top + ROW - visible
        } else {
            return;
        };
        self.scroll
            .scroll_to(offset, Tween::default(), AnimationOptions::new());
    }

    fn render(&mut self, ui: Ui<'_>) {
        if self.handle_input(ui.input()) {
            QUIT.set(true);
            return;
        }

        let mut root = ui.layout(flex::row().padding(Sides::all(12.0)).gap(12.0));
        root.insert(Rectangle::new().background(colors::BACKGROUND));
        let list = root
            .child()
            .item(
                flex::item()
                    .width(Sizing::fixed(420.0))
                    .height(Sizing::grow()),
            )
            .build(|ui: Ui<'_>| self.render_dive_list(ui));
        let main = root
            .child()
            .item(flex::item().grow())
            .build(|ui: Ui<'_>| match &self.stats {
                Some(stats) => {
                    render_stats(ui, stats, &mut self.stats_scroll);
                    None
                }
                None => self.render_dive(ui),
            });

        // Applied after the build, which borrows the dives: the window shows
        // it on the frame asked for here
        match list.or(main) {
            Some(Choice::Key(key)) => {
                self.handle_input(Input::Text(key));
            }
            Some(Choice::Row(row)) => self.logbook.selected = row,
            Some(Choice::Open(url)) => (self.open_url)(&url),
            None => return,
        }
        root.request_frame();
    }

    fn render_dive_list(&mut self, ui: Ui<'_>) -> Option<Choice> {
        let logbook = &mut self.logbook;
        let mut choice = None;

        let mut panel = ui.layout(flex::column().padding(Sides::all(14.0)).gap(10.0));
        panel.insert(card());
        {
            let mut header = panel
                .child()
                .item(flex::item().width(Sizing::grow()))
                .layout(
                    flex::row()
                        .align(Align::Center)
                        .justify(Justify::SpaceBetween),
                );
            header
                .child()
                .insert(Text::new("Dive log").style(style(20.0, colors::TEXT).weight(700)));
            header.child().insert(
                Text::new(&format!("{} dives", logbook.listed.len()))
                    .style(style(13.0, colors::MUTED)),
            );
        }
        {
            let mut buttons = panel.child().layout(flex::row().gap(8.0));
            let ignored = logbook.ignored();
            if ignored > 0 {
                let label = match logbook.show_ignored {
                    true => "Hide ignored   a".to_string(),
                    false => format!("Show {ignored} ignored   a"),
                };
                let on = logbook.show_ignored;
                if key_button(&mut buttons, &label, 'a', on) {
                    choice = Some(Choice::Key('a'));
                }
            }
            let on = self.stats.is_some();
            if key_button(&mut buttons, "Statistics   s", 's', on) {
                choice = Some(Choice::Key('s'));
            }
        }

        let selected = logbook.selected;
        let numbers = &logbook.numbers;
        let mut clicked = None;
        panel
            .child()
            .item(flex::item().width(Sizing::grow()).height(Sizing::grow()))
            .build(
                UniformList::new(
                    &mut self.scroll,
                    logbook
                        .listed
                        .iter()
                        .map(|&dive| (dive, &logbook.dives[dive]))
                        .enumerate(),
                )
                .item_extent(ROW)
                .id(|_, _, (_, (dive, _)): &(usize, (usize, &DiveLog))| Id::new(("dive", *dive)))
                .clip(BoundsClip)
                .scrollbar_thickness(6.0)
                .minimum_thumb_extent(24.0)
                .build_scrollbar(
                    |ui: Ui<'_>, (index, (dive_index, dive))| {
                        let mut row = ui.layout(
                            flex::row()
                                .padding(Sides::xy(10.0, 0.0))
                                .gap(10.0)
                                .align(Align::Center),
                        );
                        let press = row.interact(interact::press());
                        if press.clicked {
                            clicked = Some(index);
                        }
                        let background = match (index == selected, press.hovered) {
                            (true, _) => colors::SELECTED,
                            (false, true) => colors::RAISED,
                            (false, false) => Color::TRANSPARENT,
                        };
                        row.insert(
                            Rectangle::new()
                                .background(background)
                                .radius(BorderRadius::uniform(6.0)),
                        );

                        // An ignored dive is not counted, and fades
                        let (number_color, text_color) = match dive.ignored {
                            true => (colors::MUTED, colors::MUTED),
                            false => (number_color(&dive.dive_mode), colors::TEXT),
                        };
                        let number = numbers[dive_index].map_or(String::new(), |n| format!("#{n}"));
                        let cells = [
                            (number, 50.0, number_color, HorizontalAlign::Left),
                            (
                                dive.datetime.format("%Y-%m-%d").to_string(),
                                90.0,
                                text_color,
                                HorizontalAlign::Left,
                            ),
                            (
                                format!("{:.1} m", dive.max_depth_m),
                                64.0,
                                text_color,
                                HorizontalAlign::Right,
                            ),
                            (
                                format!("{} min", dive.duration_seconds / 60),
                                64.0,
                                text_color,
                                HorizontalAlign::Right,
                            ),
                            (mode_short(dive), 52.0, colors::MUTED, HorizontalAlign::Left),
                        ];
                        for (text, width, color, align) in cells {
                            row.child()
                                .item(flex::item().width(Sizing::fixed(width)))
                                .insert(
                                    Text::new(&text)
                                        .style(style(14.0, color))
                                        .options(TextOptions::new().horizontal_align(align)),
                                );
                        }
                    },
                    |_: Ui<'_>| {},
                    |mut ui: Ui<'_>, active| {
                        let thumb = if active {
                            colors::DEPTH
                        } else {
                            colors::BORDER
                        };
                        ui.insert(
                            Rectangle::new()
                                .background(thumb)
                                .radius(BorderRadius::uniform(3.0)),
                        );
                    },
                ),
            );

        // Under the list, over as many lines as it takes
        if let Some(error) = &logbook.error {
            panel
                .child()
                .item(flex::item().width(Sizing::grow()))
                .insert(
                    Text::new(error)
                        .style(style(13.0, colors::TEMP))
                        .options(TextOptions::new().wrap(TextWrap::Word)),
                );
        }

        clicked.map(Choice::Row).or(choice)
    }

    fn render_dive(&self, ui: Ui<'_>) -> Option<Choice> {
        let Some(index) = self.logbook.current() else {
            let mut panel = ui.layout(flex::column().padding(Sides::all(18.0)));
            panel.insert(card());
            panel.child().insert(
                Text::new("Every dive is ignored: press a to list them")
                    .style(style(15.0, colors::MUTED)),
            );
            return None;
        };
        let dive = &self.logbook.dives[index];

        let mut column = ui.layout(flex::column().gap(12.0));
        let info = column
            .child()
            .item(flex::item().width(Sizing::grow()))
            .build(|ui: Ui<'_>| render_dive_info(ui, dive, self.logbook.numbers[index]));
        let profile = column
            .child()
            .item(flex::item().grow())
            .build(|ui: Ui<'_>| self.render_profile(ui, index, dive));
        info.or(profile)
    }

    /// `index` is that of `dive` among the dives, to tell whose sample is picked.
    fn render_profile(&self, ui: Ui<'_>, index: usize, dive: &DiveLog) -> Option<Choice> {
        if !dive.dips.is_empty() {
            render_dips(ui, dive);
            return None;
        }

        let mut panel = ui.layout(flex::column().padding(Sides::all(18.0)).gap(10.0));
        panel.insert(card());

        if dive.samples.is_empty() {
            panel.child().insert(heading("Depth profile"));
            panel
                .child()
                .insert(Text::new("No sample data").style(style(14.0, colors::MUTED)));
            return None;
        }
        let profile = Profile::new(dive);

        // Pointing at the chart picks the sample under the pointer, as does a
        // press or a drag. Done before the readout is built so that it is not
        // a frame late. The area is only known if asked for on every frame.
        let chart_id = Id::new("depth chart");
        let area = panel.area(chart_id);
        let press = panel.interact_widget(chart_id, interact::press());
        let drag = panel.interact_widget(chart_id, interact::drag());
        if press.hovered || press.active || drag.active {
            if let (Some(area), Some(pointer)) = (area, panel.mouse_position()) {
                let plot = ProfileChart::plot(area);
                let time_s =
                    ((pointer.x - plot.x) / plot.width).clamp(0.0, 1.0) * profile.time_bound * 60.0;
                if let Some(sample) = nearest_sample(dive, time_s) {
                    self.cursor.set(Some((index, sample)));
                }
            }
        }
        let picked = match self.cursor.get() {
            Some((dive_index, sample)) if dive_index == index => dive.samples.get(sample),
            _ => None,
        };

        // The title, then a switch per curve, the key in it
        let mut choice = None;
        {
            let mut header = panel
                .child()
                .item(flex::item().width(Sizing::grow()))
                .layout(flex::row().gap(8.0).align(Align::Center));
            header
                .child()
                .item(flex::item().width(Sizing::grow()))
                .insert(heading("Depth profile"));
            let mut curves = vec![(
                vec![("Depth".to_string(), colors::DEPTH)],
                'd',
                self.show_depth,
            )];
            if let Some(temp) = &profile.temp {
                let label = format!("Temp {:.0}-{:.0} C", temp.min, temp.max);
                curves.push((vec![(label, colors::TEMP)], 't', self.show_temp));
            }
            let span = |curve: &Overlay| {
                let about = if curve.estimated { "~" } else { "" };
                format!("{about}{:.0}-{:.0}", curve.max, curve.min)
            };
            let pressures = match (&profile.total_pressure, &profile.pressure) {
                // A sidemount pair: their total, then each tank
                (Some(total), first) => {
                    let mut labels =
                        vec![(format!("Pressure {} bar", span(total)), colors::PRESSURE)];
                    if let Some(first) = first {
                        labels.push((format!("1: {}", span(first)), colors::PRESSURE1));
                    }
                    if let Some(second) = &profile.pressure2 {
                        labels.push((format!("2: {} bar", span(second)), colors::PRESSURE2));
                    }
                    Some(labels)
                }
                (None, Some(first)) => Some(vec![(
                    format!("Pressure {} bar", span(first)),
                    colors::PRESSURE,
                )]),
                (None, None) => None,
            };
            if let Some(labels) = pressures {
                curves.push((labels, 'p', self.show_pressure));
            }
            for (labels, key, on) in curves {
                let clicked = header
                    .child()
                    .id(Id::new(("button", key)))
                    .build(|ui: Ui<'_>| curve_button(ui, &labels, key, on));
                if clicked {
                    choice = Some(Choice::Key(key));
                }
            }
        }

        // What the chart reads at the picked sample
        let readout: Vec<(String, Color)> = match picked {
            Some(sample) => sample_values(dive, sample)
                .into_iter()
                .zip([colors::TIME, colors::DEPTH, colors::PRESSURE, colors::TEMP])
                .collect(),
            None => vec![(
                "Point at the chart to read the values".to_string(),
                colors::MUTED,
            )],
        };
        let parts: Vec<(&str, Color)> = readout
            .iter()
            .flat_map(|(text, color)| [(text.as_str(), *color), ("     ", colors::MUTED)])
            .collect();
        panel
            .child()
            .insert(rich(&parts, style(15.0, colors::TEXT)));

        // Two more lines for what the newer logs hold at each sample. Kept
        // even with no sample picked, so that the chart does not move.
        if dive.samples[0].speed_m_min.is_some() {
            let [state, signals, alarms] = picked.map(sample_readout).unwrap_or_default();
            let one_line = TextOptions::new()
                .max_lines(1)
                .overflow(TextOverflow::Ellipsis);
            for parts in [
                vec![(state.as_str(), colors::TEXT)],
                vec![
                    (signals.as_str(), colors::TEXT),
                    ("     ", colors::MUTED),
                    (alarms.as_str(), colors::ALARM),
                ],
            ] {
                panel
                    .child()
                    .item(
                        flex::item()
                            .width(Sizing::grow())
                            .height(Sizing::fixed(18.0)),
                    )
                    .insert(rich(&parts, style(13.0, colors::TEXT)).options(one_line));
            }
        }

        let mut curves = Vec::new();
        if let Some(temp) = profile.temp.filter(|_| self.show_temp) {
            curves.push((temp.points, colors::TEMP));
        }
        if self.show_pressure {
            // A sidemount pair: each tank, then their total on top
            let first = match profile.pressure2.is_some() {
                true => colors::PRESSURE1,
                false => colors::PRESSURE,
            };
            let pressures = [
                (profile.pressure2, colors::PRESSURE2),
                (profile.pressure, first),
                (profile.total_pressure, colors::PRESSURE),
            ];
            for (curve, color) in pressures {
                if let Some(curve) = curve {
                    curves.push((curve.points, color));
                }
            }
        }
        let depth = self.show_depth.then_some(profile.depth);
        let cursor = picked.map(|sample| (sample.time_s as f32 / 60.0, sample.depth_m as f32));
        let (time_bound, depth_bound) = (profile.time_bound, profile.depth_bound);
        panel
            .child()
            .item(flex::item().grow())
            .id(chart_id)
            .build(|mut ui: Ui<'_>| {
                let gui = ui.context_mut().as_mut();
                let mut label = |text: String| gui.text_run(&text, style(12.0, colors::MUTED));
                let depth_step = grid_step(depth_bound, 8.0);
                let depth_labels = steps(depth_bound, depth_step)
                    .map(|metres| (metres, label(format!("{metres:.0} m"))))
                    .collect();
                let time_step = grid_step(time_bound, 10.0);
                let time_labels = steps(time_bound, time_step)
                    .map(|minutes| {
                        let text = match minutes == time_bound {
                            true => format!("{minutes:.0} min"),
                            false => format!("{minutes:.0}"),
                        };
                        (minutes, label(text))
                    })
                    .collect();
                ui.insert(ProfileChart {
                    curves,
                    depth,
                    time_bound,
                    depth_bound,
                    cursor,
                    depth_labels,
                    time_labels,
                });
            });

        choice
    }
}

fn style(size: f32, color: Color) -> TextStyle<'static> {
    TextStyle::new().size(size).color(color)
}

fn heading(text: &str) -> Text<'_> {
    Text::new(text).style(style(17.0, colors::TEXT).weight(700))
}

/// The panel behind a group of things.
fn card() -> Rectangle {
    Rectangle::new()
        .background(colors::PANEL)
        .border(Border::solid(1.0, colors::BORDER))
        .radius(BorderRadius::uniform(10.0))
}

/// Text in coloured pieces.
fn rich<'a>(parts: &[(&str, Color)], base: TextStyle<'static>) -> OwnedRichText<'a> {
    let mut text = String::new();
    let mut spans = Vec::new();
    for &(part, color) in parts.iter().filter(|(part, _)| !part.is_empty()) {
        let start = text.len();
        text.push_str(part);
        spans.push(Span::new(start..text.len()).style(SpanStyle::new().color(color)));
    }
    OwnedRichText {
        text,
        spans,
        style: base,
        options: TextOptions::new(),
    }
}

/// [`RichText`] holding its text, to be built from pieces.
struct OwnedRichText<'a> {
    text: String,
    spans: Vec<Span<'a>>,
    style: TextStyle<'static>,
    options: TextOptions,
}

impl OwnedRichText<'_> {
    fn options(mut self, options: TextOptions) -> Self {
        self.options = options;
        self
    }
}

impl blit::Widget<DesktopContext, blit::state::Node> for OwnedRichText<'_> {
    type Response = ();

    fn build(self, mut ui: Ui<'_, blit::state::Node>) {
        ui.insert(
            RichText::new(&self.text, &self.spans)
                .style(self.style)
                .options(self.options),
        );
    }
}

/// A button showing the key that does the same, given its own id so
/// that the tests find it. Returns whether it was clicked.
fn key_button<L: blit::Layout<DesktopContext>>(
    parent: &mut Ui<'_, blit::state::Open<L>>,
    label: &str,
    key: char,
    on: bool,
) -> bool {
    parent
        .child()
        .id(Id::new(("button", key)))
        .build(|ui: Ui<'_>| button(ui, &[(label, colors::TEXT)], on))
}

/// A switch for curves of the chart, each label after a stroke of its colour.
fn curve_button(ui: Ui<'_>, labels: &[(String, Color)], key: char, on: bool) -> bool {
    let key = format!("   {key}");
    let text = if on { colors::TEXT } else { colors::MUTED };
    let mut parts = Vec::new();
    for (i, (label, color)) in labels.iter().enumerate() {
        if i > 0 {
            parts.push(("   ", colors::MUTED));
        }
        parts.push(("━  ", if on { *color } else { colors::MUTED }));
        parts.push((label.as_str(), text));
    }
    parts.push((&key, colors::MUTED));
    button(ui, &parts, on)
}

fn button(ui: Ui<'_>, label: &[(&str, Color)], on: bool) -> bool {
    let mut button = ui.layout(
        flex::row()
            .padding(Sides::xy(12.0, 6.0))
            .align(Align::Center),
    );
    let press = button.interact(interact::press());
    let background = match (on, press.hovered) {
        (true, _) => colors::SELECTED,
        (false, true) => colors::RAISED,
        (false, false) => colors::PANEL,
    };
    button.insert(
        Rectangle::new()
            .background(background)
            .border(Border::solid(1.0, colors::BORDER))
            .radius(BorderRadius::uniform(7.0)),
    );
    button
        .child()
        .insert(rich(label, style(13.0, colors::TEXT)));
    press.clicked
}

/// Colour of the number of a dive in the list: one for the freedive
/// sessions, one for all the other dives.
fn number_color(mode: &DiveMode) -> Color {
    match mode {
        DiveMode::Freedive => colors::FREEDIVE,
        _ => colors::SCUBA,
    }
}

/// `number` is that of the dive in the logbook.
fn render_dive_info(ui: Ui<'_>, dive: &DiveLog, number: Option<u32>) -> Option<Choice> {
    let mut panel = ui.layout(flex::column().padding(Sides::all(18.0)).gap(12.0));
    panel.insert(card());

    // The title, what kind of dive, and the button to ignore it
    let mut ignore = false;
    {
        let mut title = panel
            .child()
            .item(flex::item().width(Sizing::grow()))
            .layout(flex::row().gap(14.0).align(Align::Center));
        let number = match number {
            Some(number) => format!("Dive #{number}"),
            None => "Dive".to_string(),
        };
        title
            .child()
            .insert(Text::new(&number).style(style(24.0, colors::TEXT).weight(700)));
        let count = match dive.dips.len() {
            0 => format!("{} samples", dive.samples.len()),
            dips => format!("{dips} dips"),
        };
        let numbers = computer_numbers(dive).join("   ");
        let ignored = if dive.ignored { "ignored" } else { "" };
        title
            .child()
            .item(flex::item().width(Sizing::grow()))
            .insert(rich(
                &[
                    (&format!("{:?}", dive.dive_mode), colors::SCUBA),
                    ("     ", colors::MUTED),
                    (&count, colors::MUTED),
                    ("     ", colors::MUTED),
                    (&numbers, colors::MUTED),
                    ("     ", colors::MUTED),
                    (ignored, colors::ALARM),
                ],
                style(14.0, colors::TEXT),
            ));
        let label = if dive.ignored {
            "Take back   i"
        } else {
            "Ignore   i"
        };
        if key_button(&mut title, label, 'i', dive.ignored) {
            ignore = true;
        }
    }

    // Two columns of fields, each taking half of the width
    let mut open = None;
    {
        let mut columns = panel
            .child()
            .item(flex::item().width(Sizing::grow()))
            .layout(flex::row().gap(24.0));
        for fields in dive_fields(dive) {
            let mut column = columns
                .child()
                .item(flex::item().width(Sizing::grow()))
                .layout(flex::column().gap(5.0));
            for (label, value) in fields {
                // A position opens on Google Maps
                let position = match label {
                    "GPS start" => dive.start_position,
                    "GPS end" => dive.end_position,
                    _ => None,
                };
                field(column.child(), label, |mut ui| match position {
                    Some(at) => {
                        if link(ui, Id::new(("link", label)), &value) {
                            open = Some(maps_url(at));
                        }
                    }
                    None => {
                        ui.insert(Text::new(&value).style(style(14.0, colors::TEXT)));
                    }
                });
            }
        }
    }

    // The alarms take the full width, and as many lines as they need
    if !dive.alarms.is_empty() {
        field(panel.child(), "Alarms", |mut ui| {
            ui.insert(
                Text::new(&alarm_text(&dive.alarms))
                    .style(style(14.0, colors::ALARM))
                    .options(TextOptions::new().wrap(TextWrap::Word)),
            );
        });
    }

    let tissues = [("N2", &dive.tissue_n2_mbar), ("He", &dive.tissue_he_mbar)];
    for (i, (gas, pressures)) in tissues.into_iter().enumerate() {
        if pressures.is_empty() {
            continue;
        }
        let label = if i == 0 { "Tissues" } else { "" };
        field(panel.child(), label, |ui| {
            let mut row = ui.layout(flex::row().gap(12.0).align(Align::End));
            row.child().build(|ui: Ui<'_>| tissue_bars(ui, pressures));
            row.child().insert(
                Text::new(&format!(
                    "{gas} {} mbar at the start",
                    tissue_range(pressures)
                ))
                .style(style(13.0, colors::MUTED)),
            );
        });
    }

    match ignore {
        true => Some(Choice::Key('i')),
        false => open.map(Choice::Open),
    }
}

/// A map pin, then `text`, that light up under the pointer. Returns
/// whether it was clicked.
fn link(ui: Ui<'_>, id: Id, text: &str) -> bool {
    let mut row = ui.layout(flex::row());
    row.child().id(id).build(|ui: Ui<'_>| {
        let mut link = ui.layout(flex::row().gap(6.0).align(Align::Center));
        let press = link.interact(interact::press());
        let color = if press.hovered {
            colors::TEXT
        } else {
            colors::LINK
        };
        link.child().insert(Pin(color));
        link.child()
            .insert(Text::new(text).style(style(14.0, color)));
        press.clicked
    })
}

/// A map pin, as tall as a line of text.
struct Pin(Color);

impl<C: blit_gui::Context> Atom<C> for Pin {
    fn measure(&self, _: &mut AtomCx<'_, C>, constraints: Constraints<f32>) -> Size<f32> {
        constraints.constrain(Size::new(10.0, 14.0))
    }

    fn paint(&self, cx: &mut AtomCx<'_, C>, area: LogicalRect) {
        let gui = cx.context_mut().as_mut();
        let (x, y, radius) = (area.x + area.width / 2.0, area.y + 5.0, 5.0);
        // The point below the head, then the head with a hole
        let point = [
            MeshVertex::new(x - radius * 0.8, y + radius * 0.6, self.0),
            MeshVertex::new(x + radius * 0.8, y + radius * 0.6, self.0),
            MeshVertex::new(x, area.y + 14.0, self.0),
        ];
        gui.paint_mesh(Mesh {
            vertices: &point,
            indices: &[0, 1, 2],
        });
        let disc = |radius: f32, color: Color| {
            DrawRectangle::new(LogicalRect::new(
                x - radius,
                y - radius,
                radius * 2.0,
                radius * 2.0,
            ))
            .background(color)
            .radius(BorderRadius::uniform(radius))
        };
        gui.paint_rectangle(disc(radius, self.0));
        gui.paint_rectangle(disc(1.8, colors::PANEL));
    }
}

/// Open a page in the browser of the desktop, rather than in the window.
fn open_in_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let child = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match child {
        // Waited for aside: the window goes on meanwhile
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(error) => eprintln!("Could not open {url} with {opener}: {error}"),
    }
}

/// A label, then what `value` builds beside it.
fn field(ui: Ui<'_, blit::state::Child<flex::Item>>, label: &str, value: impl FnOnce(Ui<'_>)) {
    let mut row = ui
        .item(flex::item().width(Sizing::grow()))
        .layout(flex::row().align(Align::Start));
    row.child()
        .item(flex::item().width(Sizing::fixed(96.0)))
        .insert(Text::new(label).style(style(14.0, colors::MUTED)));
    row.child()
        .item(flex::item().width(Sizing::grow()))
        .build(value);
}

/// One bar per tissue compartment, from the least to the most loaded.
fn tissue_bars(ui: Ui<'_>, pressures: &[f64]) {
    let (min, max) = range(pressures.iter().copied());
    let mut bars = ui.layout(flex::row().gap(2.0).align(Align::End));
    for &pressure in pressures {
        let level = if max > min {
            (pressure - min) / (max - min)
        } else {
            0.0
        };
        bars.child()
            .item(flex::item().fixed(7.0, 4.0 + 16.0 * level as f32))
            .insert(
                Rectangle::new()
                    .background(colors::DEPTH)
                    .radius(BorderRadius::uniform(2.0)),
            );
    }
}

/// Freedive sessions have no depth samples: list the dips instead of a chart.
fn render_dips(ui: Ui<'_>, dive: &DiveLog) {
    let mut panel = ui.layout(flex::column().padding(Sides::all(18.0)).gap(6.0));
    panel.insert(card());
    panel.child().insert(heading("Dips"));

    let header = ["#", "Start", "Depth", "Time", "Surface", "Temp"].map(String::from);
    for (row, cells) in std::iter::once(header).chain(dip_rows(dive)).enumerate() {
        let color = if row == 0 {
            colors::MUTED
        } else {
            colors::TEXT
        };
        let mut line = panel.child().layout(flex::row());
        for cell in cells {
            line.child()
                .item(flex::item().width(Sizing::fixed(90.0)))
                .insert(
                    Text::new(&cell)
                        .style(style(14.0, color))
                        .options(TextOptions::new().horizontal_align(HorizontalAlign::Right)),
                );
        }
    }
}

/// Colour of the dives of a country, by its rank in `Stats::countries`;
/// the dives with no country, under the empty name, are grey.
fn country_color(stats: &Stats, country: usize) -> Color {
    match stats.countries[country].0.as_str() {
        "" => colors::MUTED,
        _ => colors::COUNTRIES[country % colors::COUNTRIES.len()],
    }
}

/// One row of a bar chart: a label, a bar made of coloured stretches laid
/// end to end, and figures after it.
struct BarRow {
    label: String,
    parts: Vec<(f32, Color)>,
    text: String,
}

/// The whole logbook in figures: totals, dives per year and per month of
/// the year with a colour per country, air consumption month after month.
fn render_stats(ui: Ui<'_>, stats: &Stats, scroll: &mut ScrollState) {
    let mut page = ui.layout(flex::column());
    page.child().item(flex::item().grow()).build(
        ScrollArea::new(scroll)
            .clip(BoundsClip)
            .scrollbar_thickness(6.0)
            .minimum_thumb_extent(24.0)
            .build_scrollbar(
                |ui: Ui<'_>| {
                    let mut column = ui.layout(flex::column().gap(12.0));
                    column
                        .child()
                        .item(flex::item().width(Sizing::grow()))
                        .build(|ui: Ui<'_>| render_totals(ui, stats));

                    // A bar cut in stretches, one per country
                    let by_country = |period: &Period| -> Vec<(f32, Color)> {
                        period
                            .by_country
                            .iter()
                            .enumerate()
                            .map(|(country, &dives)| (dives as f32, country_color(stats, country)))
                            .collect()
                    };
                    let years = stats
                        .years
                        .iter()
                        .map(|year| BarRow {
                            label: year.label.clone(),
                            parts: by_country(year),
                            text: format!(
                                "{} dives  {}  depth avg {:.1} m, max {:.1} m",
                                year.dives,
                                hours(year.time_s),
                                year.avg_depth_m(),
                                year.max_depth_m,
                            ),
                        })
                        .collect();
                    column
                        .child()
                        .item(flex::item().width(Sizing::grow()))
                        .build(|ui: Ui<'_>| bar_chart(ui, "Per year", [44.0, 340.0], years));

                    let months = stats
                        .months
                        .iter()
                        .map(|month| BarRow {
                            label: month.label.clone(),
                            parts: by_country(month),
                            text: match month.dives {
                                0 => String::new(),
                                dives => format!("{dives} dives  {}", hours(month.time_s)),
                            },
                        })
                        .collect();
                    // Mean of the dives of each month, a dive counting once
                    // whatever its length
                    let consumption = stats
                        .sac_months
                        .iter()
                        .map(|month| {
                            let sac = month.avg_sac_l_min().unwrap_or(0.0);
                            BarRow {
                                label: month.label.clone(),
                                parts: vec![(sac as f32, colors::PRESSURE)],
                                text: format!("{sac:.1} l/min  {} dives", month.sac_dives),
                            }
                        })
                        .collect();
                    let mut columns = column
                        .child()
                        .item(flex::item().width(Sizing::grow()))
                        .layout(flex::row().gap(12.0));
                    columns
                        .child()
                        .item(flex::item().width(Sizing::grow()))
                        .build(|ui: Ui<'_>| {
                            bar_chart(ui, "Per month of the year", [44.0, 130.0], months)
                        });
                    columns
                        .child()
                        .item(flex::item().width(Sizing::grow()))
                        .build(|ui: Ui<'_>| {
                            bar_chart(
                                ui,
                                "Air used at the surface, per month",
                                [64.0, 150.0],
                                consumption,
                            )
                        });
                },
                |_: Ui<'_>| {},
                |mut ui: Ui<'_>, active| {
                    let thumb = if active {
                        colors::DEPTH
                    } else {
                        colors::BORDER
                    };
                    ui.insert(
                        Rectangle::new()
                            .background(thumb)
                            .radius(BorderRadius::uniform(3.0)),
                    );
                },
            ),
    );
}

/// Totals, records and the colours of the countries.
fn render_totals(ui: Ui<'_>, stats: &Stats) {
    let mut panel = ui.layout(flex::column().padding(Sides::all(18.0)).gap(8.0));
    panel.insert(card());
    panel
        .child()
        .insert(Text::new("Statistics").style(style(24.0, colors::TEXT).weight(700)));

    for line in stats_summary(stats) {
        panel
            .child()
            .insert(Text::new(&line).style(style(14.0, colors::TEXT)));
    }

    let mut legend = panel
        .child()
        .layout(flex::row().gap(18.0).align(Align::Center));
    for (index, (name, dives)) in stats.countries.iter().enumerate() {
        let name = if name.is_empty() { "no country" } else { name };
        let mut entry = legend
            .child()
            .layout(flex::row().gap(6.0).align(Align::Center));
        entry.child().item(flex::item().fixed(12.0, 12.0)).insert(
            Rectangle::new()
                .background(country_color(stats, index))
                .radius(BorderRadius::uniform(3.0)),
        );
        entry
            .child()
            .insert(Text::new(&format!("{name} {dives}")).style(style(14.0, colors::TEXT)));
    }
}

/// Horizontal bars on one scale, a row each, between `widths`: that of the
/// labels and that of the figures.
fn bar_chart(ui: Ui<'_>, title: &str, widths: [f32; 2], rows: Vec<BarRow>) {
    let mut panel = ui.layout(flex::column().padding(Sides::all(18.0)).gap(6.0));
    panel.insert(card());
    panel.child().insert(heading(title));

    let total = |row: &BarRow| row.parts.iter().map(|part| part.0).sum::<f32>();
    let longest = rows.iter().map(total).fold(0.0, f32::max);
    for row in &rows {
        let mut line = panel
            .child()
            .item(flex::item().width(Sizing::grow()))
            .layout(flex::row().gap(10.0).align(Align::Center));
        line.child()
            .item(flex::item().width(Sizing::fixed(widths[0])))
            .insert(Text::new(&row.label).style(style(13.0, colors::MUTED)));
        line.child()
            .item(
                flex::item()
                    .width(Sizing::grow())
                    .height(Sizing::fixed(14.0)),
            )
            .build(|ui: Ui<'_>| {
                let mut bar = ui.layout(flex::row());
                for &(value, color) in &row.parts {
                    let share = if longest > 0.0 { value / longest } else { 0.0 };
                    bar.child()
                        .item(
                            flex::item()
                                .width(Sizing::percent(share))
                                .height(Sizing::grow()),
                        )
                        .insert(Rectangle::new().background(color));
                }
            });
        line.child()
            .item(flex::item().width(Sizing::fixed(widths[1])))
            .insert(Text::new(&row.text).style(style(13.0, colors::TEXT)));
    }
}

/// A step between grid lines that gives at most `lines` of them up to `bound`.
fn grid_step(bound: f32, lines: f32) -> f32 {
    [1.0, 2.0, 5.0, 10.0, 15.0, 20.0, 30.0, 60.0]
        .into_iter()
        .find(|step| bound / step <= lines)
        .unwrap_or(120.0)
}

/// 0, `step`, 2 `step`... up to `bound`.
fn steps(bound: f32, step: f32) -> impl Iterator<Item = f32> {
    (0..)
        .map(move |i| i as f32 * step)
        .take_while(move |&value| value <= bound + 0.001)
}

/// Depth profile with the surface at the top, the water above the curve
/// shaded, and the picked sample marked.
struct ProfileChart {
    /// Curves stretched over the depth axis, drawn under the depth
    curves: Vec<(Vec<(f32, f32)>, Color)>,
    /// (minutes, metres) when the depth curve is shown
    depth: Option<Vec<(f32, f32)>>,
    time_bound: f32,
    depth_bound: f32,
    /// The picked sample, (minutes, metres)
    cursor: Option<(f32, f32)>,
    depth_labels: Vec<(f32, TextRun)>,
    time_labels: Vec<(f32, TextRun)>,
}

impl ProfileChart {
    /// Room for the depth labels on the left and the time labels below.
    const LEFT: f32 = 52.0;
    const RIGHT: f32 = 14.0;
    const TOP: f32 = 10.0;
    const BOTTOM: f32 = 26.0;

    /// Where the curves go within the area of the chart.
    fn plot(area: LogicalRect) -> LogicalRect {
        LogicalRect::new(
            area.x + Self::LEFT,
            area.y + Self::TOP,
            (area.width - Self::LEFT - Self::RIGHT).max(1.0),
            (area.height - Self::TOP - Self::BOTTOM).max(1.0),
        )
    }
}

impl<C: blit_gui::Context> Atom<C> for ProfileChart {
    fn measure(&self, _: &mut AtomCx<'_, C>, constraints: Constraints<f32>) -> Size<f32> {
        constraints.constrain(Size::ZERO)
    }

    fn paint(&self, cx: &mut AtomCx<'_, C>, area: LogicalRect) {
        let gui = cx.context_mut().as_mut();
        let plot = Self::plot(area);
        let at = |&(minutes, metres): &(f32, f32)| {
            (
                plot.x + minutes / self.time_bound * plot.width,
                plot.y + metres / self.depth_bound * plot.height,
            )
        };
        let line = |x: f32, y: f32, width: f32, height: f32, color: Color| {
            DrawRectangle::new(LogicalRect::new(x, y, width, height)).background(color)
        };

        let label_options = TextOptions::new().vertical_align(VerticalAlign::Center);
        for &(metres, text) in &self.depth_labels {
            let y = plot.y + metres / self.depth_bound * plot.height;
            gui.paint_rectangle(line(plot.x, y, plot.width, 1.0, colors::GRID));
            gui.paint_text(TextRequest {
                text,
                colors: &[colors::MUTED],
                area: LogicalRect::new(area.x, y - 10.0, Self::LEFT - 10.0, 20.0),
                offset_x: 0.0,
                options: label_options.horizontal_align(HorizontalAlign::Right),
            });
        }
        for &(minutes, text) in &self.time_labels {
            let x = plot.x + minutes / self.time_bound * plot.width;
            gui.paint_rectangle(line(x, plot.y, 1.0, plot.height, colors::GRID));
            gui.paint_text(TextRequest {
                text,
                colors: &[colors::MUTED],
                area: LogicalRect::new(x - 40.0, plot.y + plot.height + 4.0, 80.0, 20.0),
                offset_x: 0.0,
                options: label_options.horizontal_align(HorizontalAlign::Center),
            });
        }

        // The water down to the depth curve, lighter at the surface
        if let Some(depth) = &self.depth {
            let mut vertices = Vec::with_capacity(depth.len() * 2);
            for point in depth {
                let (x, y) = at(point);
                vertices.push(MeshVertex::new(x, plot.y, colors::SURFACE_WATER));
                vertices.push(MeshVertex::new(x, y, colors::DEEP_WATER));
            }
            let indices: Vec<u32> = (0..depth.len().saturating_sub(1) as u32)
                .flat_map(|i| {
                    let (top, bottom) = (2 * i, 2 * i + 1);
                    [top, bottom, top + 2, bottom, bottom + 2, top + 2]
                })
                .collect();
            gui.paint_mesh(Mesh {
                vertices: &vertices,
                indices: &indices,
            });
        }

        let curves = self
            .curves
            .iter()
            .map(|(points, color)| (points, *color))
            .chain(self.depth.iter().map(|points| (points, colors::DEPTH)));
        for (points, color) in curves {
            let points: Vec<(f32, f32)> = points.iter().map(at).collect();
            let (vertices, indices) = stroke(&points, 2.0, color);
            gui.paint_mesh(Mesh {
                vertices: &vertices,
                indices: &indices,
            });
        }

        if let Some(cursor) = self.cursor {
            let (x, y) = at(&cursor);
            gui.paint_rectangle(line(x, plot.y, 1.0, plot.height, colors::TIME));
            gui.paint_rectangle(
                line(x - 4.0, y - 4.0, 8.0, 8.0, colors::TIME).radius(BorderRadius::uniform(4.0)),
            );
        }
    }
}

/// Triangles drawing a line through `points`, `width` wide.
fn stroke(points: &[(f32, f32)], width: f32, color: Color) -> (Vec<MeshVertex>, Vec<u32>) {
    let mut vertices = Vec::with_capacity(points.len() * 4);
    let mut indices = Vec::with_capacity(points.len() * 6);
    for pair in points.windows(2) {
        let [(x0, y0), (x1, y1)] = [pair[0], pair[1]];
        let length = (x1 - x0).hypot(y1 - y0);
        if length == 0.0 {
            continue;
        }
        // Half the width across the segment
        let (nx, ny) = (
            -(y1 - y0) / length * width / 2.0,
            (x1 - x0) / length * width / 2.0,
        );
        let first = vertices.len() as u32;
        vertices.extend([
            MeshVertex::new(x0 + nx, y0 + ny, color),
            MeshVertex::new(x0 - nx, y0 - ny, color),
            MeshVertex::new(x1 + nx, y1 + ny, color),
            MeshVertex::new(x1 - nx, y1 - ny, color),
        ]);
        indices.extend([first, first + 1, first + 2, first + 1, first + 3, first + 2]);
    }
    (vertices, indices)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use blit::{Frame, FrameInfo, Modifiers, MouseButton, Point};
    use blit_cpu::{Renderer, RendererConfig, VecBuffer, Xrgb8888};
    use blit_gui::{GuiContext, TextSystem};

    use super::*;
    use crate::types::Sample;

    const WIDTH: f32 = 1400.0;
    const HEIGHT: f32 = 900.0;

    /// The window's frames built off-screen, once it has taken `inputs`,
    /// a frame each.
    fn render(app: &mut App, inputs: &[Input<f32>]) -> (Frame<DesktopContext>, DesktopContext) {
        let text = TextSystem::new(
            TextConfig::new().font(FontId::default(), FONTS.map(Family::named)),
            blit_text_parley::Backend::new(),
        )
        .unwrap();
        let graphics = Box::new(cpu::Backend::new(cpu::Config::default()));
        let mut context = DesktopContext::new(GuiContext::new(text), graphics);
        let mut frame = Frame::default();
        let info = FrameInfo::new(Size::new(WIDTH, HEIGHT));
        // Twice first: the list and the chart place themselves from the
        // frame before. Once more after, for what a click changed.
        let inputs = [Input::None; 2].iter().chain(inputs).chain(&[Input::None]);
        for (pass, &input) in inputs.enumerate() {
            let now = Duration::from_millis(pass as u64 * 10);
            frame.build(&mut context, info, now, input, |ui: Ui<'_>| app.render(ui));
            frame.layout(&mut context);
        }
        (frame, context)
    }

    /// The pixels of the window, painted off-screen.
    fn paint(mut frame: Frame<DesktopContext>, mut context: DesktopContext) -> Vec<u32> {
        frame.paint(&mut context);
        let buffer = VecBuffer::<Xrgb8888>::new(WIDTH as usize, HEIGHT as usize);
        let mut renderer = Renderer::new(buffer, RendererConfig::default());
        renderer.render(context.gui.render_input(), 0);
        renderer
            .buffer()
            .pixels()
            .iter()
            .map(|pixel| pixel.raw())
            .collect()
    }

    fn rgb(color: Color) -> u32 {
        (color.red as u32) << 16 | (color.green as u32) << 8 | color.blue as u32
    }

    /// Where the widget `id` lies once the window is built.
    fn area(app: &mut App, id: Id) -> LogicalRect {
        render(app, &[]).0.area(id).unwrap()
    }

    fn click(area: LogicalRect) -> [Input<f32>; 2] {
        let position = Point::new(area.x + area.width / 2.0, area.y + area.height / 2.0);
        [
            Input::MouseDown {
                position,
                button: MouseButton::Primary,
                modifiers: Modifiers::NONE,
            },
            Input::MouseUp {
                position,
                button: MouseButton::Primary,
                modifiers: Modifiers::NONE,
                leave: false,
            },
        ]
    }

    /// Three dives numbered 1 to 3, a day apart, with a short profile.
    fn dives() -> Vec<DiveLog> {
        (1..=3)
            .map(|number| DiveLog {
                number,
                datetime: chrono::NaiveDate::from_ymd_opt(2025, 8, number)
                    .unwrap()
                    .and_hms_opt(10, 0, 0)
                    .unwrap(),
                duration_seconds: 600,
                max_depth_m: 30.0,
                samples: (0..=60)
                    .map(|i| Sample {
                        time_s: i * 10,
                        depth_m: 30.0 * (i as f64 * std::f64::consts::PI / 60.0).sin(),
                        temp_c: Some(25.0 - i as f64 / 20.0),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .collect()
    }

    fn app() -> App {
        App::new(Logbook::new(PathBuf::new(), dives()))
    }

    #[test]
    fn a_click_on_a_position_opens_google_maps() {
        thread_local! {
            static OPENED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        }
        fn record(url: &str) {
            OPENED.with_borrow_mut(|opened| opened.push(url.to_string()));
        }

        let mut dives = dives();
        for dive in &mut dives {
            dive.end_position = Some(crate::types::Position {
                lat: 35.86868,
                lon: 14.35122,
            });
        }
        let mut app = App::new(Logbook::new(PathBuf::new(), dives));
        app.open_url = record;

        let link = area(&mut app, Id::new(("link", "GPS end")));
        render(&mut app, &click(link));
        assert_eq!(
            OPENED.take(),
            ["https://www.google.com/maps/search/?api=1&query=35.86868,14.35122"]
        );
        // No position at the start of these dives
        assert!(render(&mut app, &[])
            .0
            .area(Id::new(("link", "GPS start")))
            .is_none());
    }

    #[test]
    fn a_click_on_a_row_selects_its_dive() {
        let mut app = app();
        // The oldest dive is the last row
        let row = area(&mut app, Id::new(("dive", 0usize)));
        render(&mut app, &click(row));
        assert_eq!(app.logbook.selected, 2);
        assert_eq!(app.logbook.current(), Some(0));
    }

    #[test]
    fn buttons_do_what_their_keys_do() {
        let mut app = app();
        let temp = area(&mut app, Id::new(("button", 't')));
        render(&mut app, &click(temp));
        assert!(!app.show_temp);

        let stats = area(&mut app, Id::new(("button", 's')));
        render(&mut app, &click(stats));
        assert!(app.stats.is_some());
        // The same button leads back
        let back = area(&mut app, Id::new(("button", 's')));
        render(&mut app, &click(back));
        assert!(app.stats.is_none());
    }

    #[test]
    fn pointing_at_the_chart_picks_the_nearest_sample() {
        let mut app = app();
        let chart = ProfileChart::plot(area(&mut app, Id::new("depth chart")));
        // Half way along the time axis, which ends at 10 minutes
        let position = Point::new(chart.x + chart.width / 2.0, chart.y + 10.0);
        render(
            &mut app,
            &[Input::MouseMove {
                position,
                modifiers: Modifiers::NONE,
            }],
        );
        assert_eq!(app.cursor.get(), Some((2, 30)));
    }

    #[test]
    fn the_window_draws_the_profile() {
        let mut app = app();
        let chart = ProfileChart::plot(area(&mut app, Id::new("depth chart")));
        let (frame, context) = render(&mut app, &[]);
        let pixels = paint(frame, context);
        let at = |x: f32, y: f32| pixels[y as usize * WIDTH as usize + x as usize];

        assert_eq!(at(2.0, 2.0), rgb(colors::BACKGROUND));
        // The depth curve reaches the bottom of the axis half way through
        let column = chart.x + chart.width / 2.0;
        let curve = (chart.y as usize..(chart.y + chart.height) as usize)
            .any(|y| at(column, y as f32) == rgb(colors::DEPTH));
        assert!(curve);
    }
}
