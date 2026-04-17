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
        platform_specific::shell::wayland::commands::popup::destroy_popup,
        widget::{column, row, rule, scrollable},
        window,
    },
    theme,
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
    time::get_calendar_first,
};
use cosmic::applet::token::subscription::{
    TokenRequest, TokenUpdate, activation_token_subscription,
};
use icu::{
    calendar::{Gregorian, cal::Persian},
    datetime::{
        DateTimeFormatter, DateTimeFormatterPreferences, fieldsets,
        input::{Date as IcuDate, DateTime, Time},
        options::TimePrecision,
    },
    locale::{
        Locale, locale,
        preferences::extensions::unicode::keywords::{CalendarAlgorithm, HourCycle},
    },
};

const APPLET_ID: &str = "io.github.hojjatabdollahi.day";

static AUTOSIZE_MAIN_ID: LazyLock<Id> = LazyLock::new(|| Id::new("autosize-main"));

const FIRST_DAY_OPTIONS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

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

            if let Some(lang) = cleaned_locale.split('-').next()
                && let Ok(locale) = Locale::try_from_str(lang)
            {
                return locale;
            }
        }
    }
    tracing::warn!("No valid locale found in environment, using en-US");
    locale!("en-US")
}

/// Turns "America/New_York" into "New York", "UTC" into "UTC", etc.
fn clock_display_name(tz_name: &str) -> String {
    tz_name
        .rsplit('/')
        .next()
        .unwrap_or(tz_name)
        .replace('_', " ")
}

/// Offset from local time as "+9", "+9:30" or "-7".
fn format_offset(secs: i32) -> String {
    let sign = if secs < 0 { "-" } else { "+" };
    let (hours, mins) = (secs.abs() / 3600, secs.abs() / 60 % 60);
    if mins == 0 {
        format!("{sign}{hours}")
    } else {
        format!("{sign}{hours}:{mins:02}")
    }
}

/// Pomodoro presets: one click sets the duration and starts the timer.
fn timer_presets() -> Element<'static, Message> {
    let preset = |label: &'static str, mins: u64| {
        button::standard(label).on_press(Message::TimerPreset(mins * 60))
    };
    column![
        text::caption("Pomodoro"),
        row![
            preset("Focus 25", 25),
            preset("Break 5", 5),
            preset("Rest 15", 15)
        ]
        .spacing(8),
    ]
    .align_x(Alignment::Center)
    .spacing(4)
    .apply(container)
    .center_x(Length::Fill)
    .into()
}

fn icu_date(date: Date) -> IcuDate<Gregorian> {
    IcuDate::try_new_gregorian(i32::from(date.year()), date.month() as u8, date.day() as u8)
        .expect("valid date")
}

fn icu_datetime(zoned: &Zoned) -> DateTime<Gregorian> {
    DateTime {
        date: icu_date(zoned.date()),
        time: Time::try_new(
            zoned.hour() as u8,
            zoned.minute() as u8,
            zoned.second() as u8,
            0,
        )
        .expect("valid time"),
    }
}

fn format_shamsi_date(date: Date) -> String {
    let mut prefs = DateTimeFormatterPreferences::from(locale!("fa"));
    prefs.calendar_algorithm = Some(CalendarAlgorithm::Persian);
    DateTimeFormatter::try_new(prefs, fieldsets::YMD::long())
        .unwrap()
        .format(&icu_date(date).to_calendar(Persian))
        .to_string()
}

/// Overlay menus share the popup's surface. With frosted glass on, libcosmic
/// gives them the same translucent background as the popup, so they'd draw
/// translucent-over-translucent and look see-through. Render them opaque.
fn opaque<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let theme = theme::Theme {
        transparent: false,
        ..theme::active()
    };
    cosmic::iced::widget::Themer::new(Some(theme), content).into()
}

fn toggle_row(
    label: &'static str,
    on: bool,
    msg: fn(bool) -> Message,
) -> Element<'static, Message> {
    padded_control(
        row![
            text::body(label).width(Length::Fill),
            toggler(on).on_toggle(msg),
        ]
        .align_y(Alignment::Center),
    )
    .into()
}

/// Subscription key for a `watch` receiver. Hashes only the id, so the stream
/// is not restarted when `subscription()` hands over a fresh receiver.
struct Watch<T> {
    inner: watch::Receiver<T>,
    id: &'static str,
}

impl<T> Hash for Watch<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Timer {
    Idle,
    Running { deadline: Instant },
    Paused { remaining: Duration },
    Finished { at: Instant },
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
    // Stopwatch: elapsed = accumulated + time since running_since.
    running_since: Option<Instant>,
    accumulated: Duration,
    laps: Vec<Duration>,
    timer_duration: Duration,
    timer: Timer,
    // Step being repeated while a timer arrow is held.
    hold_tx: watch::Sender<Option<i64>>,
    // Repaint period for the stopwatch/timer; None while neither is counting.
    tick_tx: watch::Sender<Option<Duration>>,
}

/// "01:23.45" or "1:02:03.45"
fn format_elapsed(d: Duration) -> String {
    format!("{}.{:02}", format_elapsed_short(d), d.subsec_millis() / 10)
}

/// "01:23" or "1:02:03"
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
    GoToToday,
    ToggleSettings,
    Token(TokenUpdate),
    ConfigChanged(TimeAppletConfig),
    TimezoneUpdate(String),
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
    TimerDismiss,
    TimerAdd(i64),
    TimerPreset(u64),
    TimerHoldStart(i64),
    TimerHoldStop,
    TimerHoldTick,
    // Shared repaint tick for the stopwatch and timer
    FastTick,
}

impl Window {
    fn save_config(&self) {
        if let Ok(helper) = CosmicConfig::new(APPLET_ID, TimeAppletConfig::VERSION)
            && let Err(err) = self.config.write_entry(&helper)
        {
            tracing::error!(?err, "Failed to save config");
        }
    }

    fn format_clock_time(&self, zoned: &Zoned) -> String {
        let dt = icu_datetime(zoned);
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
        let first_day_of_week = match self.config.first_day_of_week {
            0 => Weekday::Monday,
            1 => Weekday::Tuesday,
            2 => Weekday::Wednesday,
            3 => Weekday::Thursday,
            4 => Weekday::Friday,
            5 => Weekday::Saturday,
            _ => Weekday::Sunday,
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
            calendar = calendar.push(
                text::caption(weekday.format(&icu_date(date)).to_string())
                    .apply(container)
                    .center_x(Length::Fixed(44.0)),
            );
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
                Some(icu_date(date).to_calendar(Persian).day_of_month().0)
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

    fn stopwatch_elapsed(&self) -> Duration {
        self.accumulated + self.running_since.map_or(Duration::ZERO, |t| t.elapsed())
    }

    /// Repaint period: 100ms with a readout on screen, 1s for the panel, 500ms
    /// blink once the timer has finished, None when nothing is counting.
    fn desired_tick(&self) -> Option<Duration> {
        let on_screen = self.popup.is_some() && matches!(self.page, Page::Stopwatch | Page::Timer);
        let counting = self.running_since.is_some() || self.timer_running();
        let mut ms = counting.then_some(if on_screen { 100 } else { 1000 });
        if self.timer_finished() {
            ms = Some(ms.map_or(500, |m| m.min(500)));
        }
        ms.map(Duration::from_millis)
    }

    fn timer_running(&self) -> bool {
        matches!(self.timer, Timer::Running { .. })
    }

    fn timer_finished(&self) -> bool {
        matches!(self.timer, Timer::Finished { .. })
    }

    /// Running or finished: the timer owns the panel and the page shown on open.
    fn timer_active(&self) -> bool {
        self.timer_running() || self.timer_finished()
    }

    /// Adjust the duration while idle, clamped to 0..=99h.
    fn timer_add(&mut self, delta: i64) {
        if self.timer == Timer::Idle {
            let secs = (self.timer_duration.as_secs() as i64 + delta).clamp(0, 99 * 3600);
            self.timer_duration = Duration::from_secs(secs as u64);
        }
    }

    fn timer_remaining(&self) -> Duration {
        match self.timer {
            Timer::Idle => self.timer_duration,
            Timer::Running { deadline } => deadline.saturating_duration_since(Instant::now()),
            Timer::Paused { remaining } => remaining,
            Timer::Finished { .. } => Duration::ZERO,
        }
    }

    /// Red for the last 10 seconds; blinks red/transparent once finished.
    fn timer_text_class(&self) -> theme::Text {
        let red = Color::from(theme::active().cosmic().destructive.base);
        match self.timer {
            Timer::Finished { at } if at.elapsed().as_millis() / 500 % 2 == 1 => {
                theme::Text::Color(Color::TRANSPARENT)
            }
            Timer::Finished { .. } => theme::Text::Color(red),
            Timer::Running { .. } if self.timer_remaining() <= Duration::from_secs(10) => {
                theme::Text::Color(red)
            }
            _ => theme::Text::Default,
        }
    }

    /// Icon plus readout shown on the panel while the stopwatch or timer runs.
    fn panel_indicator(
        &self,
        horizontal: bool,
        icon_name: &'static str,
        readout: String,
        class: theme::Text,
    ) -> Element<'_, Message> {
        let (width, height) = self.core.applet.suggested_size(true);
        let padding = 2 * self.core.applet.suggested_padding(true).1;
        let label = self.core.applet.text(readout).class(class);
        let glyph = icon::from_name(icon_name).size(width);
        if horizontal {
            row!(
                glyph,
                label,
                container(space::vertical().height(Length::Fixed((height + padding) as f32)))
            )
            .spacing(4)
            .align_y(Alignment::Center)
            .into()
        } else {
            column!(
                glyph,
                label,
                space::horizontal().width(Length::Fixed((width + padding) as f32))
            )
            .spacing(4)
            .align_x(Alignment::Center)
            .into()
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
        let finished = self.timer_finished();
        let remaining = self.timer_remaining();

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

        if self.timer == Timer::Idle {
            content = content.push(self.timer_steppers());
            content = content.push(timer_presets());
        }

        let controls: Element<'_, Message> = if finished {
            row![button::suggested("Dismiss").on_press(Message::TimerDismiss),]
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
                if self.timer != Timer::Idle || self.timer_duration > Duration::ZERO {
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

        content = content
            .push(padded_control(divider::horizontal::default()).padding([space_xxs, space_s]));
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
            // on_press_down so a hold can repeat; release_listener ends it.
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
            content = content.push(scrollable(list).height(Length::Fixed(STOPWATCH_LAPS_HEIGHT)));
        }

        content.padding([8, 0]).into()
    }

    fn vertical_layout(&self) -> Element<'_, Message> {
        let mut elements: Vec<Element<'_, Message>> = Vec::new();
        let datetime = icu_datetime(&self.now);
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
        let datetime = icu_datetime(&self.now);
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

        let selected = icu_date(self.date_selected);
        let prefs = DateTimeFormatterPreferences::from(self.locale.clone());

        let date = text(
            DateTimeFormatter::try_new(prefs, fieldsets::YMD::long())
                .unwrap()
                .format(&selected)
                .to_string(),
        )
        .size(18);
        let day_of_week = text::body(
            DateTimeFormatter::try_new(prefs, fieldsets::E::long())
                .unwrap()
                .format(&selected)
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

        // Date gets its own row so long month names can't push the buttons off the popup.
        let controls = row![
            day_of_week,
            space::horizontal().width(Length::Fill),
            month_controls,
            stopwatch_btn,
            timer_btn,
            settings_btn,
        ]
        .align_y(Alignment::Center);

        let mut date_row = row![date].align_y(Alignment::Center);
        if self.date_selected != self.date_today {
            date_row = date_row.push(space::horizontal().width(Length::Fill)).push(
                button::icon(icon::from_name("x-office-calendar-symbolic"))
                    .padding(4)
                    .on_press(Message::GoToToday),
            );
        }

        let mut header = column![date_row, controls];
        if self.config.show_persian_calendar {
            header = header.push(text::caption(format_shamsi_date(self.date_selected)));
        }
        let header = header.padding([12, 20]);

        let mut content = column![header, self.calendar_grid().padding([0, 12].into())];

        if !self.config.additional_clocks.is_empty() {
            content = content
                .push(padded_control(divider::horizontal::default()).padding([space_xxs, space_s]));
            for tz_name in &self.config.additional_clocks {
                if let Ok(tz) = TimeZone::get(tz_name) {
                    let zoned = self.now.clone().with_time_zone(tz);
                    let glyph = if (6..18).contains(&zoned.hour()) {
                        "weather-clear-symbolic"
                    } else {
                        "weather-clear-night-symbolic"
                    };
                    let mut place = row![
                        icon::from_name(glyph).size(16),
                        text::body(clock_display_name(tz_name)),
                    ]
                    .spacing(space_xxs)
                    .align_y(Alignment::Center);
                    let offset = zoned.offset().seconds() - self.now.offset().seconds();
                    if offset != 0 {
                        place = place.push(text::caption(format_offset(offset)));
                    }
                    content = content.push(
                        row![
                            place.width(Length::Fill),
                            text::body(self.format_clock_time(&zoned)),
                        ]
                        .align_y(Alignment::Center)
                        .padding([4, 20]),
                    );
                }
            }
        }

        content.padding([8, 0]).into()
    }

    fn settings_view(&self) -> Element<'_, Message> {
        let Spacing {
            space_xxs,
            space_s,
            space_m,
            ..
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
            .button_height(28)
            .padding([space_xxs, space_m]);

        let active_tab = self
            .tabs
            .active_data::<SettingsTab>()
            .copied()
            .unwrap_or(SettingsTab::General);

        let tab_content: Element<'_, Message> = match active_tab {
            SettingsTab::General => scrollable(self.general_settings())
                .height(Length::Fixed(SETTINGS_SCROLL_HEIGHT))
                .into(),
            SettingsTab::Clocks => self.clocks_settings(),
            SettingsTab::Calendar => scrollable(self.calendar_settings())
                .height(Length::Fixed(SETTINGS_SCROLL_HEIGHT))
                .into(),
        };

        column![header, tabs, divider::horizontal::default(), tab_content,].into()
    }

    fn general_settings(&self) -> Element<'_, Message> {
        let Spacing {
            space_xxs,
            space_s,
            space_m,
            ..
        } = theme::active().cosmic().spacing;
        let divider = || container(divider::horizontal::default()).padding([space_xxs, space_m]);
        let c = &self.config;

        column![
            toggle_row(
                "Show date in panel",
                c.show_date_in_top_panel,
                Message::SetShowDate
            ),
            toggle_row("Show weekday", c.show_weekday, Message::SetShowWeekday),
            divider(),
            toggle_row("24-hour time", c.military_time, Message::SetMilitaryTime),
            toggle_row("Show seconds", c.show_seconds, Message::SetShowSeconds),
            divider(),
            padded_control(
                column![
                    text::body("First day of week"),
                    opaque(
                        dropdown(
                            &FIRST_DAY_OPTIONS,
                            Some(c.first_day_of_week as usize),
                            Message::SetFirstDayOfWeek,
                        )
                        .width(Length::Fill),
                    ),
                ]
                .spacing(space_s),
            ),
        ]
        .into()
    }

    fn clocks_settings(&self) -> Element<'_, Message> {
        let Spacing {
            space_s, space_m, ..
        } = theme::active().cosmic().spacing;

        // Kept outside the scrollable so the dropdown overlay is not clipped.
        let search = padded_control(
            column![
                text::body("Search for a city"),
                opaque(
                    combo_box::ComboBox::new(
                        &self.city_combo_state,
                        "e.g. Tokyo, London, New York…",
                        None::<&CityEntry>,
                        Message::SelectCity,
                    )
                    .width(Length::Fill),
                ),
            ]
            .spacing(space_s),
        );

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
        let Spacing {
            space_s, space_m, ..
        } = theme::active().cosmic().spacing;

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
                timer: Timer::Idle,
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
            Subscription::run_with(
                Watch {
                    inner: show_seconds,
                    id: "time-sub",
                },
                |Watch { inner, .. }| {
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
            Subscription::run_with(
                Watch {
                    inner: tick,
                    id: "fast-tick-sub",
                },
                |Watch { inner, .. }| {
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

        // Emits TimerHoldTick at an accelerating rate while a timer arrow is held.
        fn hold_repeat_subscription(hold: watch::Receiver<Option<i64>>) -> Subscription<Message> {
            fn hold_delay(count: u32) -> Duration {
                let ms = 360u64.saturating_sub(count as u64 * 35).max(45);
                Duration::from_millis(ms)
            }
            Subscription::run_with(
                Watch {
                    inner: hold,
                    id: "timer-hold-sub",
                },
                |Watch { inner, .. }| {
                    let mut hold = inner.clone();
                    stream::channel(1, move |mut output: mpsc::Sender<Message>| async move {
                        loop {
                            if hold.borrow_and_update().is_none() {
                                if hold.changed().await.is_err() {
                                    break;
                                }
                                continue;
                            }
                            // Any change (release or a new press) restarts the acceleration.
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

        // Ends a stepper hold on any pointer release. mouse_area only sees releases
        // inside its own bounds, so it would miss the cursor drifting off the arrow.
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
        if self.hold_tx.borrow().is_some() {
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
                    // Opening the popup dismisses a finished timer.
                    if self.timer_finished() {
                        self.timer = Timer::Idle;
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

                    // Open through the surface tracker (not raw get_popup) so
                    // libcosmic applies the theme's frosted-glass blur and
                    // corner radius to the popup when "frosted applets" is on.
                    cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                        |_| Default::default(),
                        move |app: &mut Self| {
                            let mut popup_settings = app.core.applet.get_popup_settings(
                                app.core.main_window_id().unwrap(),
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
                            } = app.rectangle;
                            popup_settings.positioner.anchor_rect = Rectangle::<i32> {
                                x: x.max(1.) as i32,
                                y: y.max(1.) as i32,
                                width: width.max(1.) as i32,
                                height: height.max(1.) as i32,
                            };
                            popup_settings.positioner.size = None;
                            popup_settings
                        },
                        None,
                    ))
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
            Message::GoToToday => {
                self.date_today = self.now.date();
                self.date_selected = self.date_today;
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
                    Some(start) => self.accumulated += start.elapsed(),
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
                let now = Instant::now();
                self.timer = match self.timer {
                    Timer::Running { deadline } => Timer::Paused {
                        remaining: deadline.saturating_duration_since(now),
                    },
                    Timer::Paused { remaining } => Timer::Running {
                        deadline: now + remaining,
                    },
                    _ if self.timer_duration > Duration::ZERO => Timer::Running {
                        deadline: now + self.timer_duration,
                    },
                    other => other,
                };
                self.refresh_tick();
                Task::none()
            }
            Message::TimerReset => {
                self.timer = Timer::Idle;
                self.timer_duration = Duration::ZERO;
                self.refresh_tick();
                Task::none()
            }
            // Clears a finished timer but keeps its duration for the next run.
            Message::TimerDismiss => {
                self.timer = Timer::Idle;
                self.refresh_tick();
                Task::none()
            }
            Message::TimerAdd(delta) => {
                self.timer_add(delta);
                Task::none()
            }
            Message::TimerPreset(secs) => {
                if self.timer == Timer::Idle {
                    self.timer_duration = Duration::from_secs(secs);
                    return self.update(Message::TimerStartPause);
                }
                Task::none()
            }
            Message::TimerHoldStart(delta) => {
                self.timer_add(delta);
                self.hold_tx.send_replace(Some(delta));
                Task::none()
            }
            Message::TimerHoldStop => {
                self.hold_tx.send_replace(None);
                Task::none()
            }
            Message::TimerHoldTick => {
                let held = *self.hold_tx.borrow();
                if let Some(delta) = held {
                    self.timer_add(delta);
                }
                Task::none()
            }
            Message::FastTick => {
                if self.timer_running() && self.timer_remaining().is_zero() {
                    self.timer = Timer::Finished { at: Instant::now() };
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
            self.panel_indicator(
                horizontal,
                "alarm-symbolic",
                format_elapsed_short(self.timer_remaining()),
                self.timer_text_class(),
            )
        } else if self.running_since.is_some() {
            self.panel_indicator(
                horizontal,
                "accessories-clock-symbolic",
                format_elapsed_short(self.stopwatch_elapsed()),
                theme::Text::Default,
            )
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
    // Persian digits are contiguous from U+06F0.
    n.to_string()
        .chars()
        .map(|c| char::from_u32(0x06F0 + c.to_digit(10).unwrap()).unwrap())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readouts() {
        let d = Duration::from_millis(3_723_450);
        assert_eq!(format_elapsed_short(d), "1:02:03");
        assert_eq!(format_elapsed(d), "1:02:03.45");
        assert_eq!(format_elapsed(Duration::from_millis(83_450)), "01:23.45");
        assert_eq!(to_farsi_digits(29), "۲۹");
        assert_eq!(clock_display_name("America/New_York"), "New York");
        assert_eq!(clock_display_name("UTC"), "UTC");
        assert_eq!(format_offset(34_200), "+9:30");
        assert_eq!(format_offset(-25_200), "-7");
    }

    #[test]
    fn icu_dates() {
        let date = Date::constant(2026, 9, 17);
        assert_eq!(icu_date(date).to_calendar(Persian).day_of_month().0, 26);
        assert!(format_shamsi_date(date).contains("۱۴۰۵"));
        let prefs = DateTimeFormatterPreferences::from(locale!("en-US"));
        let weekday = DateTimeFormatter::try_new(prefs, fieldsets::E::short()).unwrap();
        assert_eq!(weekday.format(&icu_date(date)).to_string(), "Thu");
    }
}
