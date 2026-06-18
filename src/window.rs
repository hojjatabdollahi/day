// SPDX-License-Identifier: GPL-3.0-only

use cosmic::{
    Apply, Element, Task, app,
    applet::{cosmic_panel_config::PanelAnchor, padded_control},
    cctk::sctk::reexports::calloop,
    cosmic_theme::Spacing,
    iced::stream,
    iced::widget::Column,
    iced::{
        Alignment, Color, Length, Rectangle, Subscription,
        futures::{SinkExt, StreamExt, channel::mpsc},
        mouse::ScrollDelta,
        platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup},
        widget::{column, row, rule, scrollable},
        window,
    },
    surface, theme,
    widget::{
        Button, Grid, Id, autosize, button, combo_box, container, divider, dropdown, grid, icon,
        mouse_area, rectangle_tracker::*, segmented_button, space, tab_bar, text, toggler,
    },
};
use cosmic_config::{Config as CosmicConfig, CosmicConfigEntry};
use jiff::{
    Timestamp, ToSpan, Zoned,
    civil::{Date, Weekday},
    tz::TimeZone,
};
use logind_zbus::manager::ManagerProxy;
use std::hash::Hash;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use timedate_zbus::TimeDateProxy;
use tokio::{sync::watch, time};

use crate::{
    cities::{CITIES, CityEntry},
    config::TimeAppletConfig,
    fl,
    time::get_calendar_first,
};
use cosmic::applet::token::subscription::{
    TokenRequest, TokenUpdate, activation_token_subscription,
};
use icu::{
    calendar::cal::Persian,
    datetime::{
        DateTimeFormatter, DateTimeFormatterPreferences, fieldsets,
        input::{Date as IcuDate, DateTime, Time},
        options::TimePrecision,
    },
    locale::{
        Locale,
        preferences::extensions::unicode::keywords::{CalendarAlgorithm, HourCycle},
    },
};

const APPLET_ID: &str = "io.github.hojjatabdollahi.day";

static AUTOSIZE_MAIN_ID: LazyLock<Id> = LazyLock::new(|| Id::new("autosize-main"));

static COSMIC_LOGO: LazyLock<cosmic::widget::icon::Handle> = LazyLock::new(|| {
    cosmic::widget::icon::from_svg_bytes(
        include_bytes!("../res/icons/bundled/cosmic.svg").as_slice(),
    )
});

static FIRST_DAY_OPTIONS: LazyLock<Vec<String>> = LazyLock::new(|| {
    vec![
        "Monday".into(),
        "Tuesday".into(),
        "Wednesday".into(),
        "Thursday".into(),
        "Friday".into(),
        "Saturday".into(),
        "Sunday".into(),
    ]
});

const SETTINGS_SCROLL_HEIGHT: f32 = 380.0;

const STOPWATCH_LAPS_HEIGHT: f32 = 180.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    Calendar,
    Settings,
    Stopwatch,
    Timer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Clocks,
    Calendar,
}

fn get_system_locale() -> Locale {
    for var in ["LC_TIME", "LC_ALL", "LANG"] {
        if let Ok(locale_str) = std::env::var(var) {
            let cleaned_locale = locale_str
                .split('.')
                .next()
                .unwrap_or(&locale_str)
                .replace('_', "-");

            if let Ok(locale) = Locale::try_from_str(&cleaned_locale) {
                return locale;
            }

            if let Some(lang) = cleaned_locale.split('-').next() {
                if let Ok(locale) = Locale::try_from_str(lang) {
                    return locale;
                }
            }
        }
    }
    tracing::warn!("No valid locale found in environment, using fallback");
    Locale::try_from_str("en-US").expect("Failed to parse fallback locale 'en-US'")
}

/// Turns "America/New_York" into "New York", "UTC" into "UTC", etc.
fn clock_display_name(tz_name: &str) -> String {
    tz_name
        .split('/')
        .last()
        .unwrap_or(tz_name)
        .replace('_', " ")
}

pub struct Window {
    core: cosmic::app::Core,
    popup: Option<window::Id>,
    now: Zoned,
    timezone: Option<TimeZone>,
    date_today: Date,
    date_selected: Date,
    rectangle_tracker: Option<RectangleTracker<u32>>,
    rectangle: Rectangle,
    token_tx: Option<calloop::channel::Sender<TokenRequest>>,
    config: TimeAppletConfig,
    show_seconds_tx: watch::Sender<bool>,
    locale: Locale,
    page: Page,
    tabs: segmented_button::SingleSelectModel,
    city_combo_state: combo_box::State<CityEntry>,
    // Stopwatch. Elapsed time is computed from a monotonic anchor, never
    // accumulated from ticks, so it can't drift or jump on a clock change.
    running_since: Option<Instant>,
    accumulated: Duration,
    laps: Vec<Duration>,
    // Countdown timer. Like the stopwatch, the remaining time is derived from a
    // monotonic deadline rather than decremented per tick.
    timer_duration: Duration,            // configured length, editable while idle
    timer_deadline: Option<Instant>,     // Some while running
    timer_paused: Option<Duration>,      // remaining, Some while paused
    timer_finished_at: Option<Instant>,  // when it reached zero; drives the blink
    // Press-and-hold on a stepper: the signed step being repeated, None when no
    // arrow is held. Pushed to hold_tx to drive the accelerating repeat.
    timer_hold: Option<i64>,
    hold_tx: watch::Sender<Option<i64>>,
    // Carries the desired repaint cadence to the fast-tick subscription that
    // drives both the stopwatch and the timer: None parks it, Some(period) ticks.
    tick_tx: watch::Sender<Option<Duration>>,
}

/// Stopwatch readout with centiseconds, e.g. "01:23.45" or "1:02:03.45".
fn format_elapsed(d: Duration) -> String {
    let total_cs = d.as_millis() / 10;
    let cs = total_cs % 100;
    let total_secs = total_cs / 100;
    let secs = total_secs % 60;
    let mins = (total_secs / 60) % 60;
    let hours = total_secs / 3600;
    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}.{cs:02}")
    } else {
        format!("{mins:02}:{secs:02}.{cs:02}")
    }
}

/// Compact stopwatch/timer readout for the panel, no centiseconds: "01:23" or "1:02:03".
fn format_elapsed_short(d: Duration) -> String {
    let total_secs = d.as_secs();
    let secs = total_secs % 60;
    let mins = (total_secs / 60) % 60;
    let hours = total_secs / 3600;
    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}")
    } else {
        format!("{mins:02}:{secs:02}")
    }
}

/// Post a desktop notification on the session bus. Best-effort: any failure is
/// logged by the caller and otherwise ignored.
async fn send_notification(summary: String, body: String) -> zbus::Result<()> {
    let conn = zbus::Connection::session().await?;
    let actions: Vec<&str> = Vec::new();
    let hints: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> =
        std::collections::HashMap::new();
    conn.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &(
            "Day",            // app_name
            0u32,             // replaces_id
            "alarm-symbolic", // app_icon
            summary,          // summary
            body,             // body
            actions,          // actions
            hints,            // hints
            -1i32,            // expire_timeout (default)
        ),
    )
    .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    CloseRequested(window::Id),
    Tick,
    Rectangle(RectangleUpdate<u32>),
    SelectDay(i8),
    PreviousMonth,
    NextMonth,
    ToggleSettings,
    Token(TokenUpdate),
    ConfigChanged(TimeAppletConfig),
    TimezoneUpdate(String),
    Surface(surface::Action),
    // General settings
    SetMilitaryTime(bool),
    SetShowSeconds(bool),
    SetShowDate(bool),
    SetShowWeekday(bool),
    SetFirstDayOfWeek(usize),
    // Tab navigation
    TabActivated(segmented_button::Entity),
    // Clocks settings
    SelectCity(CityEntry),
    RemoveClock(usize),
    // Calendar settings
    SetShowPersianCalendar(bool),
    // Stopwatch
    ToggleStopwatch,
    StopwatchStartPause,
    StopwatchReset,
    StopwatchLap,
    // Timer
    ToggleTimer,
    TimerStartPause,
    TimerReset,
    TimerAdd(i64),
    TimerHoldStart(i64),
    TimerHoldStop,
    TimerHoldTick,
    // Shared repaint tick for the stopwatch and timer
    FastTick,
}

impl Window {
    fn save_config(&self) {
        if let Ok(helper) = CosmicConfig::new(APPLET_ID, TimeAppletConfig::VERSION) {
            if let Err(err) = self.config.write_entry(&helper) {
                tracing::error!(?err, "Failed to save config");
            }
        }
    }

    fn create_datetime(&self, date: &Date) -> DateTime<icu::calendar::Gregorian> {
        DateTime {
            date: IcuDate::try_new_gregorian(
                date.year() as i32,
                date.month() as u8,
                date.day() as u8,
            )
            .unwrap(),
            time: Time::try_new(
                self.now.hour() as u8,
                self.now.minute() as u8,
                self.now.second() as u8,
                0,
            )
            .unwrap(),
        }
    }

    fn create_datetime_for_zoned(&self, zoned: &Zoned) -> DateTime<icu::calendar::Gregorian> {
        let date = zoned.date();
        DateTime {
            date: IcuDate::try_new_gregorian(
                date.year() as i32,
                date.month() as u8,
                date.day() as u8,
            )
            .unwrap(),
            time: Time::try_new(
                zoned.hour() as u8,
                zoned.minute() as u8,
                zoned.second() as u8,
                0,
            )
            .unwrap(),
        }
    }

    fn format_clock_time(&self, zoned: &Zoned) -> String {
        let dt = self.create_datetime_for_zoned(zoned);
        let mut prefs = DateTimeFormatterPreferences::from(self.locale.clone());
        prefs.hour_cycle = Some(if self.config.military_time {
            HourCycle::H23
        } else {
            HourCycle::H12
        });
        let fs = fieldsets::MDET::short().with_time_precision(TimePrecision::Minute);
        DateTimeFormatter::try_new(prefs, fs)
            .unwrap()
            .format(&dt)
            .to_string()
    }

    fn calendar_grid(&self) -> Grid<'_, Message> {
        let mut calendar = grid().width(Length::Fill);
        let first_day_of_week = if self.config.show_persian_calendar {
            Weekday::Friday
        } else {
            match self.config.first_day_of_week {
                0 => Weekday::Monday,
                1 => Weekday::Tuesday,
                2 => Weekday::Wednesday,
                3 => Weekday::Thursday,
                4 => Weekday::Friday,
                5 => Weekday::Saturday,
                _ => Weekday::Sunday,
            }
        };

        let first_day = get_calendar_first(
            self.date_selected.year(),
            self.date_selected.month(),
            first_day_of_week,
        );

        let prefs = DateTimeFormatterPreferences::from(self.locale.clone());
        let weekday = DateTimeFormatter::try_new(prefs, fieldsets::E::short()).unwrap();

        for i in 0..7 {
            let date = first_day.checked_add(i.days()).unwrap();
            let cell: Element<'_, Message> = if date.weekday() == Weekday::Tuesday {
                icon::icon(COSMIC_LOGO.clone())
                    .size(24)
                    .apply(container)
                    .center_x(Length::Fixed(44.0))
                    .into()
            } else {
                let datetime = self.create_datetime(&date);
                text::caption(weekday.format(&datetime).to_string())
                    .apply(container)
                    .center_x(Length::Fixed(44.0))
                    .into()
            };
            calendar = calendar.push(cell);
        }
        calendar = calendar.insert_row();

        let show_persian = self.config.show_persian_calendar;

        for i in 0..42 {
            if i > 0 && i % 7 == 0 {
                calendar = calendar.insert_row();
            }

            let date = first_day
                .checked_add(i.days())
                .expect("valid date in calendar range");
            let is_month = date.first_of_month() == self.date_selected.first_of_month();
            let is_day = date == self.date_selected;
            let is_today = date == self.date_today;

            let persian_day = if show_persian {
                Some(self.date_to_persian_icu(&date).day_of_month().0)
            } else {
                None
            };

            calendar = calendar.push(date_button(
                date.day(),
                is_month,
                is_day,
                is_today,
                persian_day,
            ));
        }

        calendar
    }

    fn date_to_persian_icu(&self, date: &Date) -> IcuDate<Persian> {
        IcuDate::try_new_gregorian(date.year() as i32, date.month() as u8, date.day() as u8)
            .unwrap()
            .to_calendar(Persian)
    }

    fn format_shamsi_date(&self, date: &Date) -> String {
        let persian = self.date_to_persian_icu(date);
        let dt = DateTime {
            date: persian,
            time: Time::try_new(0, 0, 0, 0).unwrap(),
        };
        let fa_locale = Locale::try_from_str("fa").unwrap_or_else(|_| self.locale.clone());
        let mut fa_prefs = DateTimeFormatterPreferences::from(fa_locale);
        fa_prefs.calendar_algorithm = Some(CalendarAlgorithm::Persian);
        DateTimeFormatter::try_new(fa_prefs, fieldsets::YMD::long())
            .unwrap()
            .format(&dt)
            .to_string()
    }

    fn stopwatch_elapsed(&self) -> Duration {
        self.accumulated + self.running_since.map_or(Duration::ZERO, |t| t.elapsed())
    }

    /// Repaint cadence for the fast-tick subscription, serving the stopwatch and
    /// timer together. Fast while a readout is on screen, slow while only the
    /// panel needs it, off when neither is counting.
    fn desired_tick(&self) -> Option<Duration> {
        let counting = self.running_since.is_some() || self.timer_running();
        let base = if !counting {
            None
        } else if self.popup.is_some()
            && (self.page == Page::Stopwatch || self.page == Page::Timer)
        {
            Some(Duration::from_millis(100))
        } else {
            Some(Duration::from_secs(1))
        };
        // A finished timer blinks, so it needs a steady ~500ms repaint even
        // when nothing is counting.
        if self.timer_finished() {
            let blink = Duration::from_millis(500);
            Some(base.map_or(blink, |b| b.min(blink)))
        } else {
            base
        }
    }

    fn timer_running(&self) -> bool {
        self.timer_deadline.is_some()
    }

    /// Adjust the configured duration, clamped to [0, 99h]. Only meaningful
    /// while idle, so it is a no-op once the timer is running, paused or done.
    fn timer_add(&mut self, delta: i64) {
        if self.timer_deadline.is_none()
            && self.timer_paused.is_none()
            && !self.timer_finished()
        {
            let secs = (self.timer_duration.as_secs() as i64 + delta).clamp(0, 99 * 3600);
            self.timer_duration = Duration::from_secs(secs as u64);
        }
    }

    fn timer_finished(&self) -> bool {
        self.timer_finished_at.is_some()
    }

    /// Red while the last 10 seconds count down. When finished, alternates
    /// between red and transparent every 500ms to flash the zeroed readout.
    fn timer_text_color(&self) -> Option<Color> {
        let red = || Color::from(theme::active().cosmic().destructive.base);
        if let Some(finished_at) = self.timer_finished_at {
            let on = (finished_at.elapsed().as_millis() / 500) % 2 == 0;
            Some(if on { red() } else { Color::TRANSPARENT })
        } else if self.timer_running() && self.timer_remaining() <= Duration::from_secs(10) {
            Some(red())
        } else {
            None
        }
    }

    /// Text style for the timer readout, red/flashing per `timer_text_color`.
    fn timer_text_class(&self) -> theme::Text {
        self.timer_text_color()
            .map_or(theme::Text::Default, theme::Text::Color)
    }

    /// Time left on the timer, whichever state it is in.
    fn timer_remaining(&self) -> Duration {
        if let Some(deadline) = self.timer_deadline {
            deadline.saturating_duration_since(Instant::now())
        } else if let Some(paused) = self.timer_paused {
            paused
        } else {
            self.timer_duration
        }
    }

    /// True when the timer should claim the panel and the click-to-open page.
    fn timer_active(&self) -> bool {
        self.timer_running() || self.timer_finished()
    }

    /// Compact countdown / alarm indicator shown on the panel.
    fn timer_panel(&self, horizontal: bool) -> Element<'_, Message> {
        // A finished timer reads zero and flashes; otherwise show what's left.
        let remaining = if self.timer_finished() {
            Duration::ZERO
        } else {
            self.timer_remaining()
        };
        let label = self
            .core
            .applet
            .text(format_elapsed_short(remaining))
            .class(self.timer_text_class());
        let glyph =
            icon::from_name("alarm-symbolic").size(self.core.applet.suggested_size(true).0);
        if horizontal {
            Element::from(
                row!(
                    glyph,
                    label,
                    container(space::vertical().height(Length::Fixed(
                        (self.core.applet.suggested_size(true).1
                            + 2 * self.core.applet.suggested_padding(true).1)
                            as f32
                    )))
                )
                .spacing(4)
                .align_y(Alignment::Center),
            )
        } else {
            Element::from(
                column!(
                    glyph,
                    label,
                    space::horizontal().width(Length::Fixed(
                        (self.core.applet.suggested_size(true).0
                            + 2 * self.core.applet.suggested_padding(true).1)
                            as f32
                    ))
                )
                .spacing(4)
                .align_x(Alignment::Center),
            )
        }
    }

    fn timer_view(&self) -> Element<'_, Message> {
        let Spacing {
            space_xxs,
            space_s,
            space_m,
            ..
        } = theme::active().cosmic().spacing;

        let header = row![
            button::icon(icon::from_name("go-previous-symbolic"))
                .padding(8)
                .on_press(Message::ToggleTimer),
            text::heading("Timer"),
        ]
        .align_y(Alignment::Center)
        .spacing(space_s)
        .padding([4, 8]);

        let running = self.timer_running();
        let paused = self.timer_paused.is_some();
        let finished = self.timer_finished();
        let idle = !running && !paused && !finished;
        // A finished timer reads zero and flashes.
        let remaining = if finished {
            Duration::ZERO
        } else {
            self.timer_remaining()
        };

        let readout = container(
            text(format_elapsed_short(remaining))
                .size(48)
                .class(self.timer_text_class()),
        )
        .center_x(Length::Fill)
        .padding([space_m, 0]);

        let mut content = column![header].spacing(space_s);

        if finished {
            content = content.push(
                container(text::heading("Time's up"))
                    .center_x(Length::Fill)
                    .padding([0, 0, space_s, 0]),
            );
        }

        content = content.push(readout);

        // Steppers to set the duration, shown only while idle.
        if idle {
            content = content.push(self.timer_steppers());
        }

        let controls: Element<'_, Message> = if finished {
            row![
                button::suggested("Dismiss").on_press(Message::TimerReset),
            ]
            .spacing(space_s)
            .padding([0, space_m])
            .into()
        } else {
            let primary = {
                let label = if running { "Pause" } else { "Start" };
                let b = button::suggested(label);
                if running || remaining > Duration::ZERO {
                    b.on_press(Message::TimerStartPause)
                } else {
                    b
                }
            };
            let reset = {
                let b = button::standard("Reset");
                if running || paused {
                    b.on_press(Message::TimerReset)
                } else {
                    b
                }
            };
            row![reset, space::horizontal().width(Length::Fill), primary]
                .spacing(space_s)
                .padding([0, space_m])
                .into()
        };

        content = content.push(padded_control(divider::horizontal::default()).padding([space_xxs, space_s]));
        content = content.push(controls);

        content.padding([8, 0]).into()
    }

    /// Up/down steppers for hours, minutes and seconds.
    fn timer_steppers(&self) -> Element<'_, Message> {
        let total = self.timer_duration.as_secs();
        let hours = total / 3600;
        let mins = (total / 60) % 60;
        let secs = total % 60;

        let unit = |label: &'static str, value: u64, step: i64| -> Element<'_, Message> {
            // Press-and-hold starts an accelerating repeat (stopped by a global
            // release listener); a single click yields exactly one step.
            let col = column![
                button::custom(icon::from_name("go-up-symbolic").size(16))
                    .class(theme::Button::Icon)
                    .padding(4)
                    .on_press_down(Message::TimerHoldStart(step)),
                text(format!("{value:02}")).size(28),
                text::caption(label),
                button::custom(icon::from_name("go-down-symbolic").size(16))
                    .class(theme::Button::Icon)
                    .padding(4)
                    .on_press_down(Message::TimerHoldStart(-step)),
            ]
            .align_x(Alignment::Center)
            .spacing(4);
            // Scrolling over the column nudges the value one step per notch.
            mouse_area(col)
                .on_scroll(move |delta| {
                    let y = match delta {
                        ScrollDelta::Lines { y, .. } | ScrollDelta::Pixels { y, .. } => y,
                    };
                    Message::TimerAdd(if y > 0.0 {
                        step
                    } else if y < 0.0 {
                        -step
                    } else {
                        0
                    })
                })
                .into()
        };

        row![
            unit("hr", hours, 3600),
            unit("min", mins, 60),
            unit("sec", secs, 1),
        ]
        .spacing(24)
        .align_y(Alignment::Center)
        .apply(container)
        .center_x(Length::Fill)
        .into()
    }

    fn refresh_tick(&self) {
        let _ = self.tick_tx.send(self.desired_tick());
    }

    /// Compact running indicator shown on the panel in place of the clock.
    fn stopwatch_panel(&self, horizontal: bool) -> Element<'_, Message> {
        let label = self.core.applet.text(format_elapsed_short(self.stopwatch_elapsed()));
        let glyph = icon::from_name("accessories-clock-symbolic")
            .size(self.core.applet.suggested_size(true).0);
        if horizontal {
            Element::from(
                row!(
                    glyph,
                    label,
                    container(space::vertical().height(Length::Fixed(
                        (self.core.applet.suggested_size(true).1
                            + 2 * self.core.applet.suggested_padding(true).1)
                            as f32
                    )))
                )
                .spacing(4)
                .align_y(Alignment::Center),
            )
        } else {
            Element::from(
                column!(
                    glyph,
                    label,
                    space::horizontal().width(Length::Fixed(
                        (self.core.applet.suggested_size(true).0
                            + 2 * self.core.applet.suggested_padding(true).1)
                            as f32
                    ))
                )
                .spacing(4)
                .align_x(Alignment::Center),
            )
        }
    }

    fn stopwatch_view(&self) -> Element<'_, Message> {
        let Spacing {
            space_xxs,
            space_s,
            space_m,
            ..
        } = theme::active().cosmic().spacing;

        let running = self.running_since.is_some();
        let elapsed = self.stopwatch_elapsed();

        let header = row![
            button::icon(icon::from_name("go-previous-symbolic"))
                .padding(8)
                .on_press(Message::ToggleStopwatch),
            text::heading("Stopwatch"),
        ]
        .align_y(Alignment::Center)
        .spacing(space_s)
        .padding([4, 8]);

        let readout = container(text(format_elapsed(elapsed)).size(48))
            .center_x(Length::Fill)
            .padding([space_m, 0]);

        let primary = button::suggested(if running { "Pause" } else { "Start" })
            .on_press(Message::StopwatchStartPause);

        let secondary = if running {
            button::standard("Lap").on_press(Message::StopwatchLap)
        } else {
            let b = button::standard("Reset");
            if elapsed > Duration::ZERO {
                b.on_press(Message::StopwatchReset)
            } else {
                b
            }
        };

        let controls = row![secondary, space::horizontal().width(Length::Fill), primary]
            .spacing(space_s)
            .padding([0, space_m]);

        let mut content = column![header, readout, controls].spacing(space_s);

        if !self.laps.is_empty() {
            content = content
                .push(padded_control(divider::horizontal::default()).padding([space_xxs, space_s]));
            let mut list = column![].spacing(space_xxs);
            for (i, lap) in self.laps.iter().enumerate() {
                list = list.push(
                    row![
                        text::body(format!("Lap {}", i + 1)).width(Length::Fill),
                        text::body(format_elapsed(*lap)),
                    ]
                    .padding([4, space_m]),
                );
            }
            content = content.push(
                scrollable(list).height(Length::Fixed(STOPWATCH_LAPS_HEIGHT)),
            );
        }

        content.padding([8, 0]).into()
    }

    fn vertical_layout(&self) -> Element<'_, Message> {
        let mut elements: Vec<Element<'_, Message>> = Vec::new();
        let date = self.now.date();
        let datetime = self.create_datetime(&date);
        let mut prefs = DateTimeFormatterPreferences::from(self.locale.clone());
        prefs.hour_cycle = Some(if self.config.military_time {
            HourCycle::H23
        } else {
            HourCycle::H12
        });

        if self.config.show_date_in_top_panel {
            let formatted_date = DateTimeFormatter::try_new(prefs, fieldsets::MD::medium())
                .unwrap()
                .format(&datetime)
                .to_string();

            for p in formatted_date.split_whitespace() {
                elements.push(self.core.applet.text(p.to_owned()).into());
            }
            elements.push(
                rule::horizontal(2)
                    .width(self.core.applet.suggested_size(true).0)
                    .into(),
            );
        }
        let mut fs = fieldsets::T::medium();
        if !self.config.show_seconds {
            fs = fs.with_time_precision(TimePrecision::Minute);
        }
        let formatted_time = DateTimeFormatter::try_new(prefs, fs)
            .unwrap()
            .format(&datetime)
            .to_string();

        for p in formatted_time.split_whitespace().flat_map(|s| s.split(':')) {
            elements.push(self.core.applet.text(p.to_owned()).into());
        }

        let date_time_col = Column::with_children(elements)
            .align_x(Alignment::Center)
            .spacing(4);

        Element::from(
            column!(
                date_time_col,
                space::horizontal().width(Length::Fixed(
                    (self.core.applet.suggested_size(true).0
                        + 2 * self.core.applet.suggested_padding(true).1)
                        as f32
                ))
            )
            .align_x(Alignment::Center),
        )
    }

    fn horizontal_layout(&self) -> Element<'_, Message> {
        let datetime = self.create_datetime(&self.now.date());
        let mut prefs = DateTimeFormatterPreferences::from(self.locale.clone());
        prefs.hour_cycle = Some(if self.config.military_time {
            HourCycle::H23
        } else {
            HourCycle::H12
        });

        let formatted_date = if self.config.show_date_in_top_panel {
            if self.config.show_weekday {
                let mut fs = fieldsets::MDET::medium();
                if !self.config.show_seconds {
                    fs = fs.with_time_precision(TimePrecision::Minute);
                }
                DateTimeFormatter::try_new(prefs, fs)
                    .unwrap()
                    .format(&datetime)
                    .to_string()
            } else {
                let mut fs = fieldsets::MDT::medium();
                if !self.config.show_seconds {
                    fs = fs.with_time_precision(TimePrecision::Minute);
                }
                DateTimeFormatter::try_new(prefs, fs)
                    .unwrap()
                    .format(&datetime)
                    .to_string()
            }
        } else {
            let mut fs = fieldsets::T::medium();
            if !self.config.show_seconds {
                fs = fs.with_time_precision(TimePrecision::Minute);
            }
            DateTimeFormatter::try_new(prefs, fs)
                .unwrap()
                .format(&datetime)
                .to_string()
        };

        Element::from(
            row!(
                self.core.applet.text(formatted_date),
                container(space::vertical().height(Length::Fixed(
                    (self.core.applet.suggested_size(true).1
                        + 2 * self.core.applet.suggested_padding(true).1)
                        as f32
                )))
            )
            .align_y(Alignment::Center),
        )
    }

    fn calendar_view(&self) -> Element<'_, Message> {
        let Spacing {
            space_xxs, space_s, ..
        } = theme::active().cosmic().spacing;

        let datetime = self.create_datetime(&self.date_selected);
        let prefs = DateTimeFormatterPreferences::from(self.locale.clone());

        let date = text(
            DateTimeFormatter::try_new(prefs, fieldsets::YMD::long())
                .unwrap()
                .format(&datetime)
                .to_string(),
        )
        .size(18);
        let day_of_week = text::body(
            DateTimeFormatter::try_new(prefs, fieldsets::E::long())
                .unwrap()
                .format(&datetime)
                .to_string(),
        );

        let month_controls = row![
            button::icon(icon::from_name("go-previous-symbolic"))
                .padding(8)
                .on_press(Message::PreviousMonth),
            button::icon(icon::from_name("go-next-symbolic"))
                .padding(8)
                .on_press(Message::NextMonth),
        ]
        .spacing(8);

        let stopwatch_btn = button::icon(icon::from_name("accessories-clock-symbolic"))
            .padding(8)
            .on_press(Message::ToggleStopwatch);

        let timer_btn = button::icon(icon::from_name("alarm-symbolic"))
            .padding(8)
            .on_press(Message::ToggleTimer);

        let settings_btn = button::icon(icon::from_name("preferences-system-symbolic"))
            .padding(8)
            .on_press(Message::ToggleSettings);

        let mut date_col = column![date, day_of_week];
        if self.config.show_persian_calendar {
            date_col =
                date_col.push(text::caption(self.format_shamsi_date(&self.date_selected)));
        }

        let header = row![
            date_col,
            space::horizontal().width(Length::Fill),
            month_controls,
            stopwatch_btn,
            timer_btn,
            settings_btn,
        ]
        .align_y(Alignment::Center)
        .padding([12, 20]);

        let mut content = column![header, self.calendar_grid().padding([0, 12].into())];

        if !self.config.additional_clocks.is_empty() {
            content = content
                .push(padded_control(divider::horizontal::default()).padding([space_xxs, space_s]));
            for tz_name in &self.config.additional_clocks {
                if let Ok(tz) = TimeZone::get(tz_name) {
                    let zoned = self.now.clone().with_time_zone(tz);
                    let location = clock_display_name(tz_name);
                    let time_str = self.format_clock_time(&zoned);
                    content = content.push(
                        row![
                            text::body(format!("{location}:")).width(Length::Fill),
                            text::body(time_str),
                        ]
                        .padding([4, 20]),
                    );
                }
            }
        }

        content.padding([8, 0]).into()
    }

    fn settings_view(&self) -> Element<'_, Message> {
        let Spacing {
            space_s, space_m, ..
        } = theme::active().cosmic().spacing;

        let header = row![
            button::icon(icon::from_name("go-previous-symbolic"))
                .padding(8)
                .on_press(Message::ToggleSettings),
            text::heading("Settings"),
        ]
        .align_y(Alignment::Center)
        .spacing(space_s)
        .padding([4, 8]);

        let tabs = tab_bar::horizontal(&self.tabs)
            .on_activate(Message::TabActivated)
            .button_height(32)
            .padding([space_s, space_m]);

        let active_tab = self
            .tabs
            .active_data::<SettingsTab>()
            .copied()
            .unwrap_or(SettingsTab::General);

        let tab_content: Element<'_, Message> = match active_tab {
            SettingsTab::General => scrollable(self.general_settings())
                .height(Length::Fixed(SETTINGS_SCROLL_HEIGHT))
                .into(),
            // Clocks manages its own layout: combo box sits above a smaller scrollable
            // list so the dropdown overlay is never clipped by a scrollable.
            SettingsTab::Clocks => self.clocks_settings(),
            SettingsTab::Calendar => scrollable(self.calendar_settings())
                .height(Length::Fixed(SETTINGS_SCROLL_HEIGHT))
                .into(),
        };

        column![header, tabs, divider::horizontal::default(), tab_content,].into()
    }

    fn general_settings(&self) -> Element<'_, Message> {
        let Spacing {
            space_s, space_m, ..
        } = theme::active().cosmic().spacing;

        let show_date_row = padded_control(
            row![
                text::body("Show date in panel").width(Length::Fill),
                toggler(self.config.show_date_in_top_panel).on_toggle(Message::SetShowDate),
            ]
            .align_y(Alignment::Center),
        );

        let show_weekday_row = padded_control(
            row![
                text::body("Show weekday").width(Length::Fill),
                toggler(self.config.show_weekday).on_toggle(Message::SetShowWeekday),
            ]
            .align_y(Alignment::Center),
        );

        let military_row = padded_control(
            row![
                text::body("24-hour time").width(Length::Fill),
                toggler(self.config.military_time).on_toggle(Message::SetMilitaryTime),
            ]
            .align_y(Alignment::Center),
        );

        let seconds_row = padded_control(
            row![
                text::body("Show seconds").width(Length::Fill),
                toggler(self.config.show_seconds).on_toggle(Message::SetShowSeconds),
            ]
            .align_y(Alignment::Center),
        );

        let first_day_row = padded_control(
            column![
                text::body("First day of week"),
                dropdown(
                    FIRST_DAY_OPTIONS.as_slice(),
                    Some(self.config.first_day_of_week as usize),
                    Message::SetFirstDayOfWeek,
                )
                .width(Length::Fill),
            ]
            .spacing(space_s),
        );

        column![
            container(text::caption("PANEL")).padding([space_s, space_m]),
            show_date_row,
            show_weekday_row,
            container(divider::horizontal::default()).padding([0, space_m]),
            container(text::caption("TIME")).padding([space_s, space_m]),
            military_row,
            seconds_row,
            container(divider::horizontal::default()).padding([0, space_m]),
            container(text::caption("CALENDAR")).padding([space_s, space_m]),
            first_day_row,
        ]
        .into()
    }

    fn clocks_settings(&self) -> Element<'_, Message> {
        let Spacing {
            space_s, space_m, ..
        } = theme::active().cosmic().spacing;

        // Combo box for city search — kept outside the scrollable list so its
        // dropdown overlay is never clipped.
        let search = padded_control(
            column![
                text::body("Search for a city"),
                combo_box::ComboBox::new(
                    &self.city_combo_state,
                    "e.g. Tokyo, London, New York…",
                    None::<&CityEntry>,
                    Message::SelectCity,
                )
                .width(Length::Fill),
            ]
            .spacing(space_s),
        );

        // Scrollable list of already-added clocks.
        let mut clocks_list = column![];
        if self.config.additional_clocks.is_empty() {
            clocks_list = clocks_list
                .push(container(text::caption("No clocks added yet")).padding([space_s, space_m]));
        }
        for (i, tz_name) in self.config.additional_clocks.iter().enumerate() {
            clocks_list = clocks_list.push(padded_control(
                row![
                    column![
                        text::body(clock_display_name(tz_name)),
                        text::caption(tz_name),
                    ]
                    .width(Length::Fill),
                    button::icon(icon::from_name("list-remove-symbolic"))
                        .padding(4)
                        .on_press(Message::RemoveClock(i)),
                ]
                .align_y(Alignment::Center),
            ));
        }

        const LIST_HEIGHT: f32 = SETTINGS_SCROLL_HEIGHT - 90.0;

        column![
            search,
            divider::horizontal::default(),
            scrollable(clocks_list).height(Length::Fixed(LIST_HEIGHT)),
        ]
        .into()
    }

    fn calendar_settings(&self) -> Element<'_, Message> {
        let Spacing { space_s, space_m, .. } = theme::active().cosmic().spacing;

        let persian_row = padded_control(
            row![
                column![
                    text::body("Persian (Shamsi)"),
                    text::caption("Solar Hijri / Jalali calendar"),
                ]
                .width(Length::Fill),
                toggler(self.config.show_persian_calendar)
                    .on_toggle(Message::SetShowPersianCalendar),
            ]
            .align_y(Alignment::Center),
        );

        column![
            container(text::caption("ADDITIONAL CALENDARS")).padding([space_s, space_m]),
            persian_row,
        ]
        .into()
    }

}

impl cosmic::Application for Window {
    type Message = Message;
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    const APP_ID: &str = APPLET_ID;

    fn init(core: app::Core, _flags: Self::Flags) -> (Self, app::Task<Self::Message>) {
        let locale = get_system_locale();
        let now = Zoned::now();
        let today = now.date();

        let (show_seconds_tx, _) = watch::channel(false);
        let (tick_tx, _) = watch::channel(None);
        let (hold_tx, _) = watch::channel(None);

        (
            Self {
                core,
                popup: None,
                now,
                timezone: None,
                date_today: today,
                date_selected: today,
                rectangle_tracker: None,
                rectangle: Rectangle::default(),
                token_tx: None,
                config: TimeAppletConfig::default(),
                show_seconds_tx,
                locale,
                page: Page::Calendar,
                tabs: segmented_button::Model::builder()
                    .insert(|b| b.text("General").data(SettingsTab::General).activate())
                    .insert(|b| b.text("Clocks").data(SettingsTab::Clocks))
                    .insert(|b| b.text("Calendar").data(SettingsTab::Calendar))
                    .build(),
                city_combo_state: combo_box::State::new(CITIES.clone()),
                running_since: None,
                accumulated: Duration::ZERO,
                laps: Vec::new(),
                timer_duration: Duration::from_secs(5 * 60),
                timer_deadline: None,
                timer_paused: None,
                timer_finished_at: None,
                timer_hold: None,
                hold_tx,
                tick_tx,
            },
            Task::none(),
        )
    }

    fn core(&self) -> &cosmic::app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::app::Core {
        &mut self.core
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }

    fn subscription(&self) -> Subscription<Message> {
        fn time_subscription(show_seconds: watch::Receiver<bool>) -> Subscription<Message> {
            struct Wrapper {
                inner: watch::Receiver<bool>,
                id: &'static str,
            }
            impl Hash for Wrapper {
                fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                    self.id.hash(state);
                }
            }
            Subscription::run_with(
                Wrapper {
                    inner: show_seconds,
                    id: "time-sub",
                },
                |Wrapper { inner, id: _ }| {
                    let mut show_seconds = inner.clone();
                    stream::channel(1, move |mut output: mpsc::Sender<Message>| async move {
                        show_seconds.mark_changed();
                        let mut period = 1u64;
                        let mut timer = time::interval(time::Duration::from_secs(period));
                        timer.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

                        loop {
                            tokio::select! {
                                _ = timer.tick() => {
                                    #[cfg(debug_assertions)]
                                    if let Err(err) = output.send(Message::Tick).await {
                                        tracing::error!(?err, "Failed sending tick request to applet");
                                    }
                                    #[cfg(not(debug_assertions))]
                                    let _ = output.send(Message::Tick).await;

                                    let current = Timestamp::now().as_second() as u64 % period;
                                    if current != 0 {
                                        timer.reset_after(time::Duration::from_secs(period - current));
                                    }
                                },
                                Ok(()) = show_seconds.changed() => {
                                    let seconds = *show_seconds.borrow_and_update();
                                    if seconds {
                                        period = 1;
                                        let dur = time::Duration::from_secs(period);
                                        let start = time::Instant::now() + dur;
                                        timer = time::interval_at(start, dur);
                                    } else {
                                        period = 60;
                                        let delta = time::Duration::from_secs(
                                            period - Timestamp::now().as_second() as u64 % period,
                                        );
                                        let start = time::Instant::now() + delta;
                                        let dur = time::Duration::from_secs(period);
                                        timer = time::interval_at(start, dur);
                                        timer.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
                                    }
                                }
                            }
                        }
                    })
                },
            )
        }

        async fn timezone_update(output: &mut mpsc::Sender<Message>) -> zbus::Result<()> {
            let conn = zbus::Connection::system().await?;
            let proxy = TimeDateProxy::new(&conn).await?;
            let mut stream_tz = proxy.receive_timezone_changed().await;
            while let Some(property) = stream_tz.next().await {
                let tz = property.get().await?;
                output
                    .send(Message::TimezoneUpdate(tz))
                    .await
                    .map_err(|e| {
                        zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::other(e)))
                    })?;
            }
            Ok(())
        }

        fn timezone_subscription() -> Subscription<Message> {
            Subscription::run_with("timezone-sub", |_| {
                stream::channel(1, |mut output| async move {
                    'retry: loop {
                        match timezone_update(&mut output).await {
                            Ok(()) => break 'retry,
                            Err(err) => {
                                tracing::error!(
                                    ?err,
                                    "Automatic timezone updater failed; retrying in one minute"
                                );
                                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                            }
                        }
                    }
                    std::future::pending().await
                })
            })
        }

        async fn wake_from_sleep(output: &mut mpsc::Sender<Message>) -> zbus::Result<()> {
            let connection = zbus::Connection::system().await?;
            let proxy = ManagerProxy::new(&connection).await?;
            while let Some(property) = proxy.receive_prepare_for_sleep().await?.next().await {
                let waking = !property.args()?.start();
                if waking {
                    let _ = output.send(Message::Tick).await;
                }
            }
            Ok(())
        }

        fn wake_from_sleep_subscription() -> Subscription<Message> {
            Subscription::run_with("wake-from-suspend-sub", |_| {
                stream::channel(1, |mut output| async move {
                    if let Err(err) = wake_from_sleep(&mut output).await {
                        tracing::error!(?err, "Failed to subscribe to wake-from-sleep signal");
                    }
                })
            })
        }

        fn fast_tick_subscription(
            tick: watch::Receiver<Option<Duration>>,
        ) -> Subscription<Message> {
            struct Wrapper {
                inner: watch::Receiver<Option<Duration>>,
                id: &'static str,
            }
            impl Hash for Wrapper {
                fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                    self.id.hash(state);
                }
            }
            Subscription::run_with(
                Wrapper {
                    inner: tick,
                    id: "fast-tick-sub",
                },
                |Wrapper { inner, id: _ }| {
                    let mut tick = inner.clone();
                    stream::channel(1, move |mut output: mpsc::Sender<Message>| async move {
                        let build = |period: Option<Duration>| {
                            period.map(|p| {
                                let mut t = time::interval(p);
                                t.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
                                t
                            })
                        };
                        let mut timer = build(*tick.borrow_and_update());
                        loop {
                            match timer.as_mut() {
                                // Running: tick at the carried cadence; rebuild on change.
                                Some(t) => {
                                    tokio::select! {
                                        _ = t.tick() => {
                                            let _ = output.send(Message::FastTick).await;
                                        }
                                        Ok(()) = tick.changed() => {
                                            timer = build(*tick.borrow_and_update());
                                        }
                                    }
                                }
                                // Stopped: park until a cadence arrives.
                                None => {
                                    if tick.changed().await.is_ok() {
                                        timer = build(*tick.borrow_and_update());
                                    } else {
                                        break;
                                    }
                                }
                            }
                        }
                    })
                },
            )
        }

        // Repeats a held stepper at an accelerating rate while hold_tx carries a
        // delta. The held delta lives on the model; this only paces TimerHoldTick.
        fn hold_repeat_subscription(
            hold: watch::Receiver<Option<i64>>,
        ) -> Subscription<Message> {
            struct Wrapper {
                inner: watch::Receiver<Option<i64>>,
                id: &'static str,
            }
            impl Hash for Wrapper {
                fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                    self.id.hash(state);
                }
            }
            // Initial pause, then accelerate to a 45ms floor.
            fn hold_delay(count: u32) -> Duration {
                let ms = 360u64.saturating_sub(count as u64 * 35).max(45);
                Duration::from_millis(ms)
            }
            Subscription::run_with(
                Wrapper {
                    inner: hold,
                    id: "timer-hold-sub",
                },
                |Wrapper { inner, id: _ }| {
                    let mut hold = inner.clone();
                    stream::channel(1, move |mut output: mpsc::Sender<Message>| async move {
                        loop {
                            // Park until an arrow is held.
                            if hold.borrow_and_update().is_none() {
                                if hold.changed().await.is_err() {
                                    break;
                                }
                                continue;
                            }
                            // Held: repeat faster the longer it lasts. Any change
                            // (release or a new press) restarts the acceleration.
                            let mut count: u32 = 1;
                            loop {
                                tokio::select! {
                                    _ = time::sleep(hold_delay(count)) => {
                                        if output.send(Message::TimerHoldTick).await.is_err() {
                                            return;
                                        }
                                        count += 1;
                                    }
                                    res = hold.changed() => {
                                        if res.is_err() {
                                            return;
                                        }
                                        break;
                                    }
                                }
                            }
                        }
                    })
                },
            )
        }

        // Stops a held stepper on the next left-button (or touch) release anywhere
        // — mouse_area only reports releases over its own bounds, which would miss
        // a release after the cursor drifts off the arrow.
        fn release_listener(
            event: cosmic::iced::Event,
            _status: cosmic::iced::event::Status,
            _id: window::Id,
        ) -> Option<Message> {
            use cosmic::iced::{Event, mouse, touch};
            match event {
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                | Event::Touch(touch::Event::FingerLifted { .. })
                | Event::Touch(touch::Event::FingerLost { .. }) => Some(Message::TimerHoldStop),
                _ => None,
            }
        }

        let show_seconds_rx = self.show_seconds_tx.subscribe();
        let tick_rx = self.tick_tx.subscribe();
        let hold_rx = self.hold_tx.subscribe();
        let mut subscriptions = vec![
            rectangle_tracker_subscription(0).map(|e| Message::Rectangle(e.1)),
            time_subscription(show_seconds_rx),
            fast_tick_subscription(tick_rx),
            hold_repeat_subscription(hold_rx),
            activation_token_subscription(0).map(Message::Token),
            timezone_subscription(),
            wake_from_sleep_subscription(),
            self.core.watch_config(Self::APP_ID).map(|u| {
                for err in u.errors {
                    tracing::error!(?err, "Error watching config");
                }
                Message::ConfigChanged(u.config)
            }),
        ];
        if self.timer_hold.is_some() {
            subscriptions.push(cosmic::iced::event::listen_with(release_listener));
        }
        Subscription::batch(subscriptions)
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::TogglePopup => {
                if let Some(p) = self.popup.take() {
                    self.refresh_tick();
                    destroy_popup(p)
                } else {
                    self.date_today = self.now.date();
                    self.date_selected = self.date_today;
                    // Opening the applet dismisses a finished timer: it resets
                    // and the flashing panel indicator goes away.
                    if self.timer_finished() {
                        self.timer_deadline = None;
                        self.timer_paused = None;
                        self.timer_finished_at = None;
                    }
                    self.page = if self.timer_active() {
                        Page::Timer
                    } else if self.running_since.is_some() {
                        Page::Stopwatch
                    } else {
                        Page::Calendar
                    };

                    let new_id = window::Id::unique();
                    self.popup = Some(new_id);
                    self.refresh_tick();

                    let mut popup_settings = self.core.applet.get_popup_settings(
                        self.core.main_window_id().unwrap(),
                        new_id,
                        None,
                        None,
                        None,
                    );
                    let Rectangle {
                        x,
                        y,
                        width,
                        height,
                    } = self.rectangle;
                    popup_settings.positioner.anchor_rect = Rectangle::<i32> {
                        x: x.max(1.) as i32,
                        y: y.max(1.) as i32,
                        width: width.max(1.) as i32,
                        height: height.max(1.) as i32,
                    };
                    popup_settings.positioner.size = None;
                    get_popup(popup_settings)
                }
            }
            Message::Tick => {
                self.now = self
                    .timezone
                    .as_ref()
                    .map_or_else(Zoned::now, |tz| Zoned::now().with_time_zone(tz.clone()));
                Task::none()
            }
            Message::Rectangle(u) => {
                match u {
                    RectangleUpdate::Rectangle(r) => self.rectangle = r.1,
                    RectangleUpdate::Init(tracker) => self.rectangle_tracker = Some(tracker),
                }
                Task::none()
            }
            Message::CloseRequested(id) => {
                if Some(id) == self.popup {
                    self.popup = None;
                }
                Task::none()
            }
            Message::SelectDay(day) => {
                if let Ok(date) = self.date_selected.with().day(day).build() {
                    self.date_selected = date;
                } else {
                    tracing::error!("invalid date");
                }
                Task::none()
            }
            Message::PreviousMonth => {
                if let Ok(date) = self.date_selected.checked_sub(1.month()) {
                    self.date_selected = date;
                }
                Task::none()
            }
            Message::NextMonth => {
                if let Ok(date) = self.date_selected.checked_add(1.month()) {
                    self.date_selected = date;
                }
                Task::none()
            }
            Message::ToggleSettings => {
                self.page = if self.page == Page::Settings {
                    Page::Calendar
                } else {
                    Page::Settings
                };
                self.refresh_tick();
                Task::none()
            }
            Message::Token(u) => {
                match u {
                    TokenUpdate::Init(tx) => self.token_tx = Some(tx),
                    TokenUpdate::Finished => self.token_tx = None,
                    TokenUpdate::ActivationToken { .. } => {}
                }
                Task::none()
            }
            Message::ConfigChanged(c) => {
                self.show_seconds_tx.send_if_modified(|show_seconds| {
                    if *show_seconds == c.show_seconds {
                        false
                    } else {
                        *show_seconds = c.show_seconds;
                        true
                    }
                });
                self.config = c;
                Task::none()
            }
            Message::TimezoneUpdate(timezone) => {
                if let Ok(tz) = TimeZone::get(&timezone) {
                    self.now = Zoned::now().with_time_zone(tz.clone());
                    self.date_today = self.now.date();
                    self.date_selected = self.date_today;
                    self.timezone = Some(tz);
                }
                self.update(Message::Tick)
            }
            Message::Surface(a) => {
                return cosmic::task::message(cosmic::Action::Cosmic(
                    cosmic::app::Action::Surface(a),
                ));
            }
            Message::SetMilitaryTime(v) => {
                self.config.military_time = v;
                self.save_config();
                Task::none()
            }
            Message::SetShowSeconds(v) => {
                self.config.show_seconds = v;
                self.save_config();
                Task::none()
            }
            Message::SetShowDate(v) => {
                self.config.show_date_in_top_panel = v;
                self.save_config();
                Task::none()
            }
            Message::SetShowWeekday(v) => {
                self.config.show_weekday = v;
                self.save_config();
                Task::none()
            }
            Message::SetFirstDayOfWeek(i) => {
                self.config.first_day_of_week = i as u8;
                self.save_config();
                Task::none()
            }
            Message::TabActivated(entity) => {
                self.tabs.activate(entity);
                Task::none()
            }
            Message::SelectCity(entry) => {
                if !self.config.additional_clocks.contains(&entry.timezone) {
                    self.config.additional_clocks.push(entry.timezone);
                    self.save_config();
                }
                // Recreate state to clear the search text
                self.city_combo_state = combo_box::State::new(CITIES.clone());
                Task::none()
            }
            Message::RemoveClock(i) => {
                if i < self.config.additional_clocks.len() {
                    self.config.additional_clocks.remove(i);
                    self.save_config();
                }
                Task::none()
            }
            Message::SetShowPersianCalendar(v) => {
                self.config.show_persian_calendar = v;
                self.save_config();
                Task::none()
            }
            Message::ToggleStopwatch => {
                self.page = if self.page == Page::Stopwatch {
                    Page::Calendar
                } else {
                    Page::Stopwatch
                };
                self.refresh_tick();
                Task::none()
            }
            Message::StopwatchStartPause => {
                match self.running_since.take() {
                    // Was running: fold the current segment into the total.
                    Some(start) => self.accumulated += start.elapsed(),
                    // Was stopped: open a new segment.
                    None => self.running_since = Some(Instant::now()),
                }
                self.refresh_tick();
                Task::none()
            }
            Message::StopwatchReset => {
                self.running_since = None;
                self.accumulated = Duration::ZERO;
                self.laps.clear();
                self.refresh_tick();
                Task::none()
            }
            Message::StopwatchLap => {
                self.laps.push(self.stopwatch_elapsed());
                Task::none()
            }
            Message::ToggleTimer => {
                self.page = if self.page == Page::Timer {
                    Page::Calendar
                } else {
                    Page::Timer
                };
                self.refresh_tick();
                Task::none()
            }
            Message::TimerStartPause => {
                match self.timer_deadline.take() {
                    // Was running: freeze the remaining time.
                    Some(deadline) => {
                        self.timer_paused = Some(deadline.saturating_duration_since(Instant::now()));
                    }
                    // Was paused or idle: count down from whatever is left.
                    None => {
                        let remaining = self.timer_paused.take().unwrap_or(self.timer_duration);
                        if remaining > Duration::ZERO {
                            self.timer_deadline = Some(Instant::now() + remaining);
                        }
                    }
                }
                self.refresh_tick();
                Task::none()
            }
            Message::TimerReset => {
                self.timer_deadline = None;
                self.timer_paused = None;
                self.timer_finished_at = None;
                self.refresh_tick();
                Task::none()
            }
            Message::TimerAdd(delta) => {
                self.timer_add(delta);
                Task::none()
            }
            Message::TimerHoldStart(delta) => {
                // One immediate step, then the repeat subscription accelerates.
                self.timer_add(delta);
                self.timer_hold = Some(delta);
                let _ = self.hold_tx.send(Some(delta));
                Task::none()
            }
            Message::TimerHoldStop => {
                self.timer_hold = None;
                let _ = self.hold_tx.send(None);
                Task::none()
            }
            Message::TimerHoldTick => {
                if let Some(delta) = self.timer_hold {
                    self.timer_add(delta);
                }
                Task::none()
            }
            Message::FastTick => {
                // Detect the countdown reaching zero.
                if self.timer_deadline.is_some() && self.timer_remaining().is_zero() {
                    self.timer_deadline = None;
                    self.timer_paused = None;
                    self.timer_finished_at = Some(Instant::now());
                    self.refresh_tick();
                    let label = format_elapsed_short(self.timer_duration);
                    return cosmic::task::future(async move {
                        if let Err(err) = send_notification(
                            "Timer finished".to_string(),
                            format!("Your {label} timer is done."),
                        )
                        .await
                        {
                            tracing::error!(?err, "Failed to send timer notification");
                        }
                        cosmic::Action::None
                    });
                }
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let horizontal = matches!(
            self.core.applet.anchor,
            PanelAnchor::Top | PanelAnchor::Bottom
        );

        let button = button::custom(if self.timer_active() {
            self.timer_panel(horizontal)
        } else if self.running_since.is_some() {
            self.stopwatch_panel(horizontal)
        } else if horizontal {
            self.horizontal_layout()
        } else {
            self.vertical_layout()
        })
        .padding(if horizontal {
            [0, self.core.applet.suggested_padding(true).0]
        } else {
            [self.core.applet.suggested_padding(true).0, 0]
        })
        .on_press_down(Message::TogglePopup)
        .class(cosmic::theme::Button::AppletIcon);

        autosize::autosize(
            if let Some(tracker) = self.rectangle_tracker.as_ref() {
                Element::from(tracker.container(0, button).ignore_bounds(true))
            } else {
                button.into()
            },
            AUTOSIZE_MAIN_ID.clone(),
        )
        .into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Message> {
        let content = match self.page {
            Page::Calendar => self.calendar_view(),
            Page::Settings => self.settings_view(),
            Page::Stopwatch => self.stopwatch_view(),
            Page::Timer => self.timer_view(),
        };
        self.core.applet.popup_container(container(content)).into()
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
    }
}

fn to_farsi_digits(n: u8) -> String {
    n.to_string()
        .chars()
        .map(|c| match c {
            '0' => '۰',
            '1' => '۱',
            '2' => '۲',
            '3' => '۳',
            '4' => '۴',
            '5' => '۵',
            '6' => '۶',
            '7' => '۷',
            '8' => '۸',
            '9' => '۹',
            other => other,
        })
        .collect()
}

fn date_button(
    day: i8,
    is_month: bool,
    is_day: bool,
    is_today: bool,
    persian_day: Option<u8>,
) -> Button<'static, Message> {
    let style = if is_day {
        button::ButtonClass::Suggested
    } else if is_today {
        button::ButtonClass::Standard
    } else {
        button::ButtonClass::Text
    };

    let content: Element<'static, Message> = if let Some(pd) = persian_day {
        let gregorian_center = text(format!("{day}"))
            .size(16)
            .apply(container)
            .center(Length::Fill);
        let farsi_bottom = text(to_farsi_digits(pd))
            .size(10)
            .apply(container)
            .align_x(Alignment::Center)
            .width(Length::Fill)
            .padding([0, 0, 2, 0]);
        column![gregorian_center, farsi_bottom].into()
    } else {
        text::body(format!("{day}"))
            .apply(container)
            .center(Length::Fill)
            .into()
    };

    let button = button::custom(content)
        .class(style)
        .height(Length::Fixed(44.0))
        .width(Length::Fixed(44.0));

    if is_month {
        button.on_press(Message::SelectDay(day))
    } else {
        button
    }
}
