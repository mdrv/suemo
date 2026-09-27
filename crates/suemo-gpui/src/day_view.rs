//! Read-only day view (M2): a 24-hour local-time grid, kind-colored event
//! blocks (Q2 palette), a now-line, and live updates — a background thread
//! watches the daemon socket and pushes fresh day ranges into the UI
//! (decisions.md Q3; the bridge pattern is gpui-ce docs §16.1: the async
//! side never blocks, only `.await`s).

use std::time::Duration;

use chrono::{DateTime, Local};
use futures::{
    StreamExt,
    channel::mpsc::{self, UnboundedSender},
};
use gpui::{
    Context, FocusHandle, Focusable, IntoElement, ParentElement, Render, ScrollHandle,
    SharedString, Styled, Window, actions, div, hsla, point, prelude::*, px,
};
use suemo::domain::{self, Event};
use suemo::ipc::{self, Request, Response};

actions!(suemo_gui, [Quit]);

/// Pixels per minute of the day timeline (2880 px for 24 h, scrollable).
const PX_PER_MIN: f32 = 2.0;
const HOUR_PX: f32 = 60.0 * PX_PER_MIN;
const GRID_H: f32 = 24.0 * HOUR_PX;
/// Hour-label gutter on the left of the grid.
const LABEL_W: f32 = 56.0;
const HEADER_H: f32 = 48.0;

// Dark theme (decisions.md Q2). Hues are turns (0..1).
const BG: fn() -> gpui::Hsla = || hsla(0.0, 0.0, 0.07, 1.0);

fn text_primary() -> gpui::Hsla {
    hsla(0.0, 0.0, 0.88, 1.0)
}

fn text_dim() -> gpui::Hsla {
    hsla(0.0, 0.0, 0.55, 1.0)
}

fn hour_line() -> gpui::Hsla {
    hsla(0.0, 0.0, 1.0, 0.06)
}

fn now_line() -> gpui::Hsla {
    hsla(0.02, 0.7, 0.55, 1.0)
}

pub struct DayView {
    focus_handle: FocusHandle,
    events: Vec<Event>,
    day_start: i64,
    day_end: i64,
    now_ms: i64,
    scroll: ScrollHandle,
    scrolled_to_now: bool,
}

impl DayView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (day_start, day_end) = domain::today_window();
        let events = fetch_today();
        let (tx, mut rx) = mpsc::unbounded::<Vec<Event>>();
        spawn_watch_bridge(tx);
        cx.spawn(async move |this, cx| {
            while let Some(events) = rx.next().await {
                let _ = this.update(cx, |view, cx| {
                    view.events = events;
                    cx.notify();
                });
            }
        })
        .detach();
        // Repaint ticker for the now-line (§26: gate notify on change).
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
                let _ = this.update(cx, |view, cx| {
                    let n = domain::now_ms();
                    if n != view.now_ms {
                        view.now_ms = n;
                        cx.notify();
                    }
                });
            }
        })
        .detach();
        Self {
            focus_handle: cx.focus_handle(),
            events,
            day_start,
            day_end,
            now_ms: domain::now_ms(),
            scroll: ScrollHandle::new(),
            scrolled_to_now: false,
        }
    }

    /// Grid-space y for a UTC-ms timestamp inside the day window.
    fn y_for_ms(&self, ms: i64) -> f32 {
        let minutes = (ms - self.day_start).clamp(0, 24 * 60 * 60_000) as f32 / 60_000.0;
        minutes * PX_PER_MIN
    }
}

fn fetch_today() -> Vec<Event> {
    let (from, to) = domain::today_window();
    match ipc::round_trip(&Request::Range { from, to }) {
        Ok(Response::Events { events }) => events,
        Ok(_) => Vec::new(),
        Err(err) => {
            log::warn!("day range fetch failed: {err:#}");
            Vec::new()
        }
    }
}

/// Blocking watch thread — owns all socket I/O; the UI side only receives
/// fresh `Vec<Event>` over the channel and repaints. Reconnects with a
/// short backoff when the daemon restarts.
fn spawn_watch_bridge(tx: UnboundedSender<Vec<Event>>) {
    std::thread::spawn(move || {
        loop {
            if tx.is_closed() {
                return;
            }
            match ipc::open_watch() {
                Ok(mut reader) => loop {
                    match ipc::read_changed(&mut reader) {
                        Ok(Some(_lsn)) => {
                            if tx.unbounded_send(fetch_today()).is_err() {
                                return; // UI is gone
                            }
                        }
                        Ok(None) => break, // daemon stopped
                        Err(err) => {
                            log::warn!("watch stream error: {err:#}");
                            break;
                        }
                    }
                },
                Err(err) => log::warn!("watch connect failed: {err:#}"),
            }
            if tx.is_closed() {
                return;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

fn fmt_hm(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.with_timezone(&Local).format("%H:%M").to_string())
        .unwrap_or_else(|| "??:??".into())
}

/// One absolutely-positioned block on the grid; midnight-crossing events
/// are clipped to the day (data unsplit, decisions.md Q8).
fn event_block(event: &Event, day_start: i64, day_end: i64) -> impl IntoElement {
    let start_min = (event.starts_utc.max(day_start) - day_start) as f32 / 60_000.0;
    let end_min = (event.ends_utc.min(day_end) - day_start) as f32 / 60_000.0;
    let top = start_min * PX_PER_MIN;
    let height = ((end_min - start_min) * PX_PER_MIN).max(18.0);
    let bg = domain::kind_hsla(&event.kind);
    let fg = domain::contrast_text(bg);
    div()
        .absolute()
        .left(px(LABEL_W))
        .right(px(8.))
        .top(px(top))
        .h(px(height))
        .rounded_sm()
        .bg(hsla(bg[0], bg[1], bg[2], bg[3]))
        .p(px(6.))
        .flex()
        .flex_col()
        .gap_0p5()
        .overflow_hidden()
        .child(
            div()
                .text_sm()
                .text_color(hsla(fg[0], fg[1], fg[2], fg[3]))
                .child(SharedString::from(event.title.clone())),
        )
        .child(
            div()
                .text_xs()
                .opacity(0.8)
                .text_color(hsla(fg[0], fg[1], fg[2], fg[3]))
                .child(SharedString::from(format!(
                    "{}–{}",
                    fmt_hm(event.starts_utc),
                    fmt_hm(event.ends_utc)
                ))),
        )
}

impl Render for DayView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Open on "now" instead of midnight — one-time, next paint consumes
        // the offset (§18: scroll offsets go negative going down).
        if !self.scrolled_to_now {
            self.scrolled_to_now = true;
            let viewport = f32::from(window.bounds().size.height) - HEADER_H;
            let target = (self.y_for_ms(self.now_ms) - viewport * 0.35).max(0.0);
            self.scroll.set_offset(point(px(0.), px(-target)));
        }

        let day_label = DateTime::from_timestamp_millis(self.now_ms)
            .map(|dt| dt.with_timezone(&Local).format("%a %Y-%m-%d").to_string())
            .unwrap_or_default();
        let count_label = if self.events.is_empty() {
            "no events today".to_string()
        } else {
            format!("{} event(s)", self.events.len())
        };

        let mut layers: Vec<gpui::AnyElement> = Vec::new();
        for h in 0..24 {
            let y = h as f32 * HOUR_PX;
            layers.push(
                div()
                    .absolute()
                    .left(px(8.))
                    .top(px(y - 7.0))
                    .w(px(LABEL_W - 16.0))
                    .text_xs()
                    .text_right()
                    .text_color(text_dim())
                    .child(SharedString::from(format!("{h:02}:00")))
                    .into_any_element(),
            );
            layers.push(
                div()
                    .absolute()
                    .left(px(LABEL_W))
                    .right(px(0.))
                    .top(px(y))
                    .h(px(1.))
                    .bg(hour_line())
                    .into_any_element(),
            );
        }
        for event in &self.events {
            if event.ends_utc > self.day_start && event.starts_utc < self.day_end {
                layers.push(event_block(event, self.day_start, self.day_end).into_any_element());
            }
        }
        layers.push(
            div()
                .absolute()
                .left(px(LABEL_W))
                .right(px(0.))
                .top(px(self.y_for_ms(self.now_ms) - 1.0))
                .h(px(2.))
                .bg(now_line())
                .into_any_element(),
        );

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(BG())
            .child(
                div()
                    .h(px(HEADER_H))
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .text_color(text_primary())
                            .child(SharedString::from(day_label)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(text_dim())
                            .child(SharedString::from(count_label)),
                    ),
            )
            .child(
                div()
                    .id("day")
                    .flex_1()
                    .track_scroll(&self.scroll)
                    .overflow_y_scroll()
                    .child(div().relative().w_full().h(px(GRID_H)).children(layers)),
            )
    }
}

impl Focusable for DayView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
