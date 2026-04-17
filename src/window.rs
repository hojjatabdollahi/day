// SPDX-License-Identifier: GPL-3.0-only

use cosmic::{
    Apply, Element, Task, app,
    applet::{cosmic_panel_config::PanelAnchor, padded_control},
    cctk::sctk::reexports::calloop,
    cosmic_theme::Spacing,
    iced::stream,
    iced::widget::Column,
    iced::{
        Alignment, Length, Rectangle, Subscription,
        futures::{SinkExt, StreamExt, channel::mpsc},
        platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup},
        widget::{column, row, rule, scrollable},
        window,
    },
    surface, theme,
    widget::{
        Button, Grid, Id, autosize, button, combo_box, container, divider, dropdown, grid, icon,
        rectangle_tracker::*, segmented_button, space, tab_bar, text, toggler,
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
    locale::{Locale, preferences::extensions::unicode::keywords::HourCycle},
};

const APPLET_ID: &str = "io.github.hojjatabdollahi.day";

static AUTOSIZE_MAIN_ID: LazyLock<Id> = LazyLock::new(|| Id::new("autosize-main"));

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    Calendar,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Clocks,
    Calendar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarSystem {
    Gregorian,
    Persian,
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
    active_calendar: CalendarSystem,
    city_combo_state: combo_box::State<CityEntry>,
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
    SetActiveCalendar(CalendarSystem),
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
            let datetime = self.create_datetime(&date);
            calendar = calendar.push(
                text::caption(weekday.format(&datetime).to_string())
                    .apply(container)
                    .center_x(Length::Fixed(44.0)),
            );
        }
        calendar = calendar.insert_row();

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

            calendar = calendar.push(date_button(date.day(), is_month, is_day, is_today));
        }

        calendar
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

        let date = text(self.format_header_date(&self.date_selected)).size(18);
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

        let settings_btn = button::icon(icon::from_name("preferences-system-symbolic"))
            .padding(8)
            .on_press(Message::ToggleSettings);

        let header = row![
            column![date, day_of_week],
            space::horizontal().width(Length::Fill),
            month_controls,
            settings_btn,
        ]
        .align_y(Alignment::Center)
        .padding([12, 20]);

        let mut content = column![header];

        if self.config.show_persian_calendar {
            let cal_switcher = row![
                button::custom(
                    text::caption("Gregorian")
                        .apply(container)
                        .center(Length::Fill),
                )
                .width(Length::FillPortion(1))
                .class(if self.active_calendar == CalendarSystem::Gregorian {
                    button::ButtonClass::Suggested
                } else {
                    button::ButtonClass::Standard
                })
                .on_press(Message::SetActiveCalendar(CalendarSystem::Gregorian)),
                button::custom(
                    text::caption("Shamsi")
                        .apply(container)
                        .center(Length::Fill),
                )
                .width(Length::FillPortion(1))
                .class(if self.active_calendar == CalendarSystem::Persian {
                    button::ButtonClass::Suggested
                } else {
                    button::ButtonClass::Standard
                })
                .on_press(Message::SetActiveCalendar(CalendarSystem::Persian)),
            ]
            .spacing(space_s)
            .padding([0, 20, space_s, 20]);

            content = content.push(cal_switcher);
        }

        content = content.push(self.calendar_grid().padding([0, 12].into()));

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

    fn format_header_date(&self, date: &jiff::civil::Date) -> String {
        let prefs = DateTimeFormatterPreferences::from(self.locale.clone());
        let gregorian = IcuDate::try_new_gregorian(
            date.year() as i32,
            date.month() as u8,
            date.day() as u8,
        )
        .unwrap();

        match self.active_calendar {
            CalendarSystem::Gregorian => {
                let dt = DateTime {
                    date: gregorian,
                    time: Time::try_new(0, 0, 0, 0).unwrap(),
                };
                DateTimeFormatter::try_new(prefs, fieldsets::YMD::long())
                    .unwrap()
                    .format(&dt)
                    .to_string()
            }
            CalendarSystem::Persian => {
                let persian = gregorian.to_calendar(Persian);
                let dt = DateTime {
                    date: persian,
                    time: Time::try_new(0, 0, 0, 0).unwrap(),
                };
                DateTimeFormatter::try_new(prefs, fieldsets::YMD::long())
                    .unwrap()
                    .format(&dt)
                    .to_string()
            }
        }
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
                active_calendar: CalendarSystem::Gregorian,
                city_combo_state: combo_box::State::new(CITIES.clone()),
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

        let show_seconds_rx = self.show_seconds_tx.subscribe();
        Subscription::batch([
            rectangle_tracker_subscription(0).map(|e| Message::Rectangle(e.1)),
            time_subscription(show_seconds_rx),
            activation_token_subscription(0).map(Message::Token),
            timezone_subscription(),
            wake_from_sleep_subscription(),
            self.core.watch_config(Self::APP_ID).map(|u| {
                for err in u.errors {
                    tracing::error!(?err, "Error watching config");
                }
                Message::ConfigChanged(u.config)
            }),
        ])
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::TogglePopup => {
                if let Some(p) = self.popup.take() {
                    destroy_popup(p)
                } else {
                    self.date_today = self.now.date();
                    self.date_selected = self.date_today;
                    self.page = Page::Calendar;

                    let new_id = window::Id::unique();
                    self.popup = Some(new_id);

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
                } else {
                    tracing::error!("invalid date");
                }
                Task::none()
            }
            Message::NextMonth => {
                if let Ok(date) = self.date_selected.checked_add(1.month()) {
                    self.date_selected = date;
                } else {
                    tracing::error!("invalid date");
                }
                Task::none()
            }
            Message::ToggleSettings => {
                self.page = if self.page == Page::Settings {
                    Page::Calendar
                } else {
                    Page::Settings
                };
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
                if !v && self.active_calendar == CalendarSystem::Persian {
                    self.active_calendar = CalendarSystem::Gregorian;
                }
                self.save_config();
                Task::none()
            }
            Message::SetActiveCalendar(cal) => {
                self.active_calendar = cal;
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let horizontal = matches!(
            self.core.applet.anchor,
            PanelAnchor::Top | PanelAnchor::Bottom
        );

        let button = button::custom(if horizontal {
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
        };
        self.core.applet.popup_container(container(content)).into()
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
    }
}

fn date_button(day: i8, is_month: bool, is_day: bool, is_today: bool) -> Button<'static, Message> {
    let style = if is_day {
        button::ButtonClass::Suggested
    } else if is_today {
        button::ButtonClass::Standard
    } else {
        button::ButtonClass::Text
    };

    let button = button::custom(
        text::body(format!("{day}"))
            .apply(container)
            .center(Length::Fill),
    )
    .class(style)
    .height(Length::Fixed(44.0))
    .width(Length::Fixed(44.0));

    if is_month {
        button.on_press(Message::SelectDay(day))
    } else {
        button
    }
}
