//! The overlay view (M3): day + week grids with direct-manipulation
//! editing (decisions.md Q1) and live updates (Q3).
//!
//! Interaction model: drag empty grid = create span, click empty = 60-min
//! create, click block = editor, drag block = move, drag block edges =
//! resize — past events behave identically. Drag gestures exist on the day
//! grid only; the week view is click-to-create / click-to-edit. All gesture
//! times snap to `config.ui.snap_minutes` anchored at local midnight.
//! Every socket touch happens on the watch thread or inline in handlers;
//! the UI side only ever `.await`s (gpui-ce docs §16.1).

use std::{cell::Cell, env, rc::Rc, sync::Arc, time::Duration};

use chrono::{Days, Local, NaiveDate, TimeZone};
use futures::{
    StreamExt,
    channel::mpsc::{self, UnboundedSender},
};
use gpui::{
    AnyElement, App, Bounds, Context, Div, FocusHandle, Focusable, IntoElement, MouseButton,
    ParentElement, Pixels, Point, Render, ScrollHandle, SharedString, Styled, WeakEntity, Window,
    actions, canvas, div, hsla, point, prelude::*, px,
};
use suemo::domain::{self, Event};
use suemo::ipc::{self, KindHours, Request, Response};

use crate::{editor::Editor, theme};

actions!(suemo_gui, [Quit]);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Day,
    Week,
}

/// An in-progress gesture on the day grid.
enum Drag {
    Create {
        anchor_ms: i64,
        cur_ms: i64,
        moved: bool,
    },
    Move {
        id: String,
        grab_off_ms: i64,
        down_ms: i64,
        cur_ms: i64,
        moved: bool,
    },
    Resize {
        id: String,
        top_edge: bool,
        down_ms: i64,
        cur_ms: i64,
        moved: bool,
    },
}

/// What a finished gesture asks the (window-having) handler to do.
struct Plan {
    editing: Option<Event>,
    span: (i64, i64),
    day: NaiveDate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edge {
    Top,
    Middle,
    Bottom,
}

#[derive(Clone, Copy)]
struct Hit {
    index: usize,
    edge: Edge,
}

pub struct ScheduleView {
    focus_handle: FocusHandle,
    weak: WeakEntity<Self>,
    mode: Mode,
    events: Vec<Event>,
    win_start: i64,
    win_end: i64,
    /// Local midnight→midnight per week-day column (Week mode).
    week_days: Arc<Vec<(i64, i64)>>,
    week_stats: Vec<KindHours>,
    now_ms: i64,
    snap_minutes: u32,
    scroll_day: ScrollHandle,
    scroll_week: ScrollHandle,
    day_scrolled: bool,
    week_scrolled: bool,
    /// Grid content bounds in window coords, captured at paint; scroll is
    /// already baked in because the canvas paints inside the scrolled
    /// content.
    day_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    week_col_bounds: Vec<Rc<Cell<Option<Bounds<Pixels>>>>>,
    drag: Option<Drag>,
    editor: Option<Editor>,
}

impl ScheduleView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let snap_minutes = suemo::config::load()
            .map(|c| c.ui.snap_minutes)
            .unwrap_or(5);
        let mode = if env::var("SUEMO_START_VIEW").as_deref() == Ok("week") {
            Mode::Week
        } else {
            Mode::Day
        };
        let mut view = Self {
            focus_handle: cx.focus_handle(),
            weak: cx.entity().downgrade(),
            mode: Mode::Day,
            events: Vec::new(),
            win_start: 0,
            win_end: 0,
            week_days: Arc::new(Vec::new()),
            week_stats: Vec::new(),
            now_ms: domain::now_ms(),
            snap_minutes,
            scroll_day: ScrollHandle::new(),
            scroll_week: ScrollHandle::new(),
            day_scrolled: false,
            week_scrolled: false,
            day_bounds: Rc::new(Cell::new(None)),
            week_col_bounds: (0..7).map(|_| Rc::new(Cell::new(None))).collect(),
            drag: None,
            editor: None,
        };
        view.enter(mode);
        view.refetch(cx);

        let (tx, mut rx) = mpsc::unbounded::<()>();
        spawn_watch_bridge(tx);
        cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                let _ = this.update(cx, |view, cx| view.refetch(cx));
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
        view
    }

    /// Switch windows for `mode` (fetch happens after).
    fn enter(&mut self, mode: Mode) {
        self.mode = mode;
        match mode {
            Mode::Day => {
                let today = local_date(self.now_ms);
                (self.win_start, self.win_end) = domain::day_bounds(today);
                self.day_scrolled = false;
            }
            Mode::Week => {
                (self.win_start, self.win_end) = domain::week_window_at(self.now_ms);
                self.week_days = Arc::new(week_days(self.win_start));
                self.week_scrolled = false;
            }
        }
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.drag = None;
        self.enter(mode);
        self.refetch(cx);
    }

    fn refetch(&mut self, cx: &mut Context<Self>) {
        self.events = fetch_range(self.win_start, self.win_end);
        if self.mode == Mode::Week {
            self.week_stats = fetch_stats(self.win_start, self.win_end);
        }
        cx.notify();
    }

    // ---- gestures -------------------------------------------------------

    /// Mouse-down on the day grid content. Drags start here; plans only
    /// appear on release.
    fn day_down(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) -> Option<Plan> {
        if self.editor.is_some() {
            return None;
        }
        let bounds = self.day_bounds.get()?;
        let y = f32::from(pos.y - bounds.origin.y);
        if y < 0.0 {
            return None;
        }
        let day = (self.win_start, self.win_end);
        let ms = self.snap(day.0, y, theme::PX_PER_MIN, day);
        if let Some(hit) = hit_test(&self.events, day.0, day.1, y, theme::PX_PER_MIN) {
            let ev = &self.events[hit.index];
            self.drag = match hit.edge {
                Edge::Top => Some(Drag::Resize {
                    id: ev.id.clone(),
                    top_edge: true,
                    down_ms: ms,
                    cur_ms: ms,
                    moved: false,
                }),
                Edge::Bottom => Some(Drag::Resize {
                    id: ev.id.clone(),
                    top_edge: false,
                    down_ms: ms,
                    cur_ms: ms,
                    moved: false,
                }),
                Edge::Middle => Some(Drag::Move {
                    id: ev.id.clone(),
                    grab_off_ms: ms - ev.starts_utc,
                    down_ms: ms,
                    cur_ms: ms,
                    moved: false,
                }),
            };
        } else {
            self.drag = Some(Drag::Create {
                anchor_ms: ms,
                cur_ms: ms,
                moved: false,
            });
        }
        cx.notify();
        None
    }

    /// Mouse-down on week column `col` — click-to-create / click-to-edit.
    fn week_down(
        &mut self,
        pos: Point<Pixels>,
        col: usize,
        cx: &mut Context<Self>,
    ) -> Option<Plan> {
        if self.editor.is_some() {
            return None;
        }
        let bounds = self.week_col_bounds.get(col)?.get()?;
        let y = f32::from(pos.y - bounds.origin.y);
        if y < 0.0 {
            return None;
        }
        let day = *self.week_days.get(col)?;
        let ms = self.snap(day.0, y, theme::WEEK_PX_PER_MIN, day);
        let plan = match hit_test(&self.events, day.0, day.1, y, theme::WEEK_PX_PER_MIN) {
            // Topmost block under the press (hit_test scans in paint order).
            Some(hit) => edit_plan(&self.events[hit.index]),
            None => Some(Plan {
                editing: None,
                span: (ms, ms + default_span()),
                day: local_date(day.0),
            }),
        };
        cx.notify();
        plan
    }

    fn grid_move(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        if self.editor.is_some() || self.mode != Mode::Day {
            return;
        }
        let Some(bounds) = self.day_bounds.get() else {
            return;
        };
        let y = f32::from(pos.y - bounds.origin.y);
        let ms = self.snap(
            self.win_start,
            y,
            theme::PX_PER_MIN,
            (self.win_start, self.win_end),
        );
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        match drag {
            Drag::Create {
                anchor_ms,
                cur_ms,
                moved,
            } => {
                *moved |= ms != *anchor_ms;
                *cur_ms = ms;
            }
            Drag::Move {
                down_ms,
                cur_ms,
                moved,
                ..
            }
            | Drag::Resize {
                down_ms,
                cur_ms,
                moved,
                ..
            } => {
                *moved |= ms != *down_ms;
                *cur_ms = ms;
            }
        }
        cx.notify();
    }

    /// Mouse-up: commit or turn the gesture into an editor-open plan.
    fn grid_up(&mut self, cx: &mut Context<Self>) -> Option<Plan> {
        let drag = self.drag.take()?;
        if self.editor.is_some() {
            return None;
        }
        let day = (self.win_start, self.win_end);
        let plan = match drag {
            Drag::Create {
                anchor_ms,
                cur_ms,
                moved,
            } => {
                let (a, b) = normalize_span(anchor_ms, cur_ms);
                let (a, b) = if moved {
                    (a, b)
                } else {
                    (a, a + default_span())
                };
                Some(Plan {
                    editing: None,
                    span: (a, b),
                    day: local_date(a),
                })
            }
            Drag::Move {
                id,
                grab_off_ms,
                cur_ms,
                moved,
                ..
            } => {
                let Some(ev) = self.event(&id).cloned() else {
                    return None;
                };
                if moved {
                    let (s, e) = moved_times(&ev, grab_off_ms, cur_ms, day);
                    self.push_times(ev, s, e, cx);
                    None
                } else {
                    edit_plan(&ev)
                }
            }
            Drag::Resize {
                id,
                top_edge,
                cur_ms,
                moved,
                ..
            } => {
                let Some(ev) = self.event(&id).cloned() else {
                    return None;
                };
                if moved {
                    let (s, e) = resized_times(&ev, top_edge, cur_ms, day);
                    self.push_times(ev, s, e, cx);
                    None
                } else {
                    edit_plan(&ev)
                }
            }
        };
        cx.notify();
        plan
    }

    fn event(&self, id: &str) -> Option<&Event> {
        self.events.iter().find(|ev| ev.id == id)
    }

    /// Where the dragged event would land right now (render preview).
    fn preview_event(&self) -> Option<Event> {
        let drag = self.drag.as_ref()?;
        let day = (self.win_start, self.win_end);
        match drag {
            Drag::Create { .. } => None,
            Drag::Move {
                id,
                grab_off_ms,
                cur_ms,
                ..
            } => {
                let ev = self.event(id)?;
                let (s, e) = moved_times(ev, *grab_off_ms, *cur_ms, day);
                Some(with_times(ev, s, e))
            }
            Drag::Resize {
                id,
                top_edge,
                cur_ms,
                ..
            } => {
                let ev = self.event(id)?;
                let (s, e) = resized_times(ev, *top_edge, *cur_ms, day);
                Some(with_times(ev, s, e))
            }
        }
    }

    fn snap(&self, day_start: i64, y: f32, scale: f32, day: (i64, i64)) -> i64 {
        let minutes = (y / scale).max(0.0);
        let ms = day_start + (minutes as i64) * 60_000;
        domain::snap_to_step(ms, i64::from(self.snap_minutes)).clamp(day.0, day.1 - 60_000)
    }

    /// Send a move/resize to the daemon (LWW full-row update). On error,
    /// refetch so the preview reverts to reality.
    fn push_times(&mut self, mut ev: Event, starts: i64, ends: i64, cx: &mut Context<Self>) {
        ev = with_times(&ev, starts, ends);
        ev.updated_utc = domain::now_ms();
        match ipc::round_trip(&Request::Update { event: ev }) {
            Ok(_) => {} // the change broadcast refetches
            other => {
                log::warn!("update failed: {other:?}");
                self.refetch(cx);
            }
        }
    }

    // ---- editor ---------------------------------------------------------

    fn commit_editor(&mut self, cx: &mut Context<Self>) -> bool {
        let request = match self.editor_request(cx) {
            Ok(request) => request,
            Err(message) => {
                if let Some(ed) = self.editor.as_mut() {
                    ed.error = Some(message);
                }
                return false;
            }
        };
        match ipc::round_trip(&request) {
            Ok(Response::Err { message }) => {
                if let Some(ed) = self.editor.as_mut() {
                    ed.error = Some(message);
                }
                false
            }
            Ok(_) => {
                self.editor = None; // broadcast refetches the grid
                cx.notify();
                true
            }
            Err(err) => {
                if let Some(ed) = self.editor.as_mut() {
                    ed.error = Some(format!("{err:#}"));
                }
                false
            }
        }
    }

    /// Read the editor fields (all-immutable borrows) and build the request.
    fn editor_request(&self, cx: &App) -> Result<Request, String> {
        let Some(ed) = self.editor.as_ref() else {
            return Err("no editor".into());
        };
        let title = ed.title.read(cx).as_str().trim().to_string();
        let kind = ed.kind.read(cx).as_str().trim().to_string();
        let kind = if kind.is_empty() {
            domain::DEFAULT_KIND.to_string()
        } else {
            kind
        };
        let start_spec = ed.start.read(cx).as_str().trim().to_string();
        let end_spec = ed.end.read(cx).as_str().trim().to_string();
        let start = match domain::local_time_on(ed.day, &start_spec) {
            Ok(v) => v,
            Err(err) => return Err(format!("start: {err:#}")),
        };
        let end = match domain::parse_end_on(ed.day, &end_spec, start) {
            Ok(v) => v,
            Err(err) => return Err(format!("end: {err:#}")),
        };
        Ok(match &ed.editing_id {
            Some(id) => {
                let Some(orig) = self.event(id) else {
                    return Err("event no longer exists".into());
                };
                let mut ev = orig.clone();
                ev.starts_utc = start;
                ev.ends_utc = end;
                ev.title = title;
                ev.kind = kind;
                ev.updated_utc = domain::now_ms();
                if let Err(err) = ev.validate() {
                    return Err(format!("{err:#}"));
                }
                Request::Update { event: ev }
            }
            None => {
                let ev = Event::new(title, Some(kind), start, end);
                if let Err(err) = ev.validate() {
                    return Err(format!("{err:#}"));
                }
                Request::Add {
                    title: ev.title.clone(),
                    kind: ev.kind.clone(),
                    starts_utc: ev.starts_utc,
                    ends_utc: ev.ends_utc,
                }
            }
        })
    }

    fn delete_clicked(&mut self, cx: &mut Context<Self>) {
        let Some(ed) = self.editor.as_mut() else {
            return;
        };
        if !ed.delete_armed {
            ed.delete_armed = true;
            cx.notify();
            return;
        }
        let Some(id) = ed.editing_id.clone() else {
            return;
        };
        match ipc::round_trip(&Request::Delete { id }) {
            Ok(Response::Err { message }) => {
                ed.error = Some(message);
                cx.notify();
            }
            Ok(_) => {
                self.editor = None;
                cx.notify();
            }
            Err(err) => {
                ed.error = Some(format!("{err:#}"));
                cx.notify();
            }
        }
    }

    // ---- rendering ------------------------------------------------------

    fn header(&self, weak: &WeakEntity<Self>) -> Div {
        let label = match self.mode {
            Mode::Day => format!("{}", local_date(self.now_ms).format("%a %Y-%m-%d")),
            Mode::Week => format!(
                "{} – {}",
                local_date(self.win_start).format("%Y-%m-%d"),
                (local_date(self.win_end) - chrono::Duration::days(1)).format("%Y-%m-%d")
            ),
        };
        let count = if self.events.is_empty() {
            "no events".to_string()
        } else {
            format!("{} event(s)", self.events.len())
        };
        div()
            .h(px(theme::HEADER_H))
            .px(px(16.))
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_lg()
                            .text_color(theme::text_primary())
                            .child(SharedString::from(label)),
                    )
                    .child(self.mode_tab("day", Mode::Day, weak))
                    .child(self.mode_tab("week", Mode::Week, weak)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::text_dim())
                            .child(SharedString::from(count)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_dim())
                            .child("esc hides"),
                    ),
            )
    }

    fn mode_tab(
        &self,
        label: &'static str,
        mode: Mode,
        weak: &WeakEntity<Self>,
    ) -> impl IntoElement {
        let active = self.mode == mode;
        div()
            .cursor_pointer()
            .px_2()
            .py_0p5()
            .rounded_sm()
            .text_xs()
            .when(active, |d| {
                d.bg(hsla(0.0, 0.0, 1.0, 0.10))
                    .text_color(theme::text_primary())
            })
            .when(!active, |d| d.text_color(theme::text_dim()))
            .child(label)
            .id(SharedString::from(format!("tab-{label}")))
            .on_click({
                let weak = weak.clone();
                move |_, _, cx| {
                    let _ = weak.update(cx, |view, cx| view.set_mode(mode, cx));
                }
            })
    }

    fn stats_strip(&self) -> impl IntoElement {
        let chips: Vec<AnyElement> = self
            .week_stats
            .iter()
            .map(|kh| {
                let [h, s, l, a] = domain::kind_hsla(&kh.kind);
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(
                        div()
                            .w(px(8.))
                            .h(px(8.))
                            .rounded_full()
                            .bg(hsla(h, s, l, a)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_dim())
                            .child(SharedString::from(format!("{} {:.1}h", kh.kind, kh.hours))),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_3()
            .px(px(16.))
            .pb(px(6.))
            .children(chips)
    }

    fn render_day(&self, weak: &WeakEntity<Self>) -> AnyElement {
        let mut layers: Vec<AnyElement> = Vec::new();
        for h in 0..24 {
            let y = h as f32 * theme::HOUR_PX;
            layers.push(
                div()
                    .absolute()
                    .left(px(8.))
                    .top(px(y - 7.0))
                    .w(px(theme::LABEL_W - 16.0))
                    .text_xs()
                    .text_right()
                    .text_color(theme::text_dim())
                    .child(SharedString::from(format!("{h:02}:00")))
                    .into_any_element(),
            );
            layers.push(
                div()
                    .absolute()
                    .left(px(theme::LABEL_W))
                    .right(px(0.))
                    .top(px(y))
                    .h(px(1.))
                    .bg(theme::hour_line())
                    .into_any_element(),
            );
        }
        let preview = self.preview_event();
        for event in &self.events {
            let dragged = self
                .drag
                .as_ref()
                .is_some_and(|d| drag_id(d) == Some(&event.id));
            if dragged || event.ends_utc <= self.win_start || event.starts_utc >= self.win_end {
                continue;
            }
            layers.push(
                event_block(event, self.win_start, self.win_end, theme::PX_PER_MIN)
                    .into_any_element(),
            );
        }
        match self.drag.as_ref().zip(preview) {
            Some((drag, pv)) => layers.push(
                event_block(&pv, self.win_start, self.win_end, theme::PX_PER_MIN)
                    .opacity(0.9)
                    .border_1()
                    .border_color(theme::accent())
                    .when(matches!(drag, Drag::Create { .. }), |d| {
                        d.bg(hsla(0.55, 0.5, 0.6, 0.35))
                    })
                    .into_any_element(),
            ),
            // Create-preview has no underlying event block.
            None => {
                if let Some(Drag::Create {
                    anchor_ms, cur_ms, ..
                }) = self.drag.as_ref()
                {
                    let (a, b) = normalize_span(*anchor_ms, *cur_ms);
                    let (a, b) = (a, b.max(a + 60_000));
                    let fake = with_times(&Event::new("", None, a, b), a, b);
                    layers.push(
                        event_block(&fake, self.win_start, self.win_end, theme::PX_PER_MIN)
                            .bg(hsla(0.55, 0.5, 0.6, 0.35))
                            .border_1()
                            .border_color(theme::accent())
                            .into_any_element(),
                    );
                }
            }
        }
        layers.push(
            div()
                .absolute()
                .left(px(theme::LABEL_W))
                .right(px(0.))
                .top(px(self.y_for_ms(self.now_ms, theme::PX_PER_MIN) - 1.0))
                .h(px(2.))
                .bg(theme::now_line())
                .into_any_element(),
        );

        let bounds_cell = self.day_bounds.clone();
        div()
            .id("day")
            .flex_1()
            .track_scroll(&self.scroll_day)
            .overflow_y_scroll()
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(theme::GRID_H))
                    // Bounds capture: paint-time canvas filling the content.
                    .child(div().absolute().inset_0().child(canvas(
                        {
                            let cell = bounds_cell.clone();
                            move |bounds, _, _| cell.set(Some(bounds))
                        },
                        |_, _, _, _| {},
                    )))
                    .children(layers)
                    .on_mouse_down(MouseButton::Left, {
                        let weak = weak.clone();
                        move |ev, window, cx| {
                            let plan = weak
                                .update(cx, |view, cx| view.day_down(ev.position, cx))
                                .ok()
                                .flatten();
                            if let Some(plan) = plan {
                                apply_plan(&weak, plan, window, cx);
                            }
                        }
                    }),
            )
            .into_any_element()
    }

    fn render_week(&self, weak: &WeakEntity<Self>) -> AnyElement {
        let preview = self.preview_event();
        let mut layers: Vec<AnyElement> = Vec::new();
        for h in 0..24 {
            let y = h as f32 * theme::WEEK_HOUR_PX;
            layers.push(
                div()
                    .absolute()
                    .left(px(6.))
                    .top(px(y - 7.0))
                    .w(px(theme::WEEK_LABEL_W - 10.0))
                    .text_xs()
                    .text_right()
                    .text_color(theme::text_dim())
                    .child(SharedString::from(format!("{h:02}:00")))
                    .into_any_element(),
            );
            layers.push(
                div()
                    .absolute()
                    .left(px(theme::WEEK_LABEL_W))
                    .right(px(0.))
                    .top(px(y))
                    .h(px(1.))
                    .bg(theme::hour_line())
                    .into_any_element(),
            );
        }
        if let Some(day) = self
            .week_days
            .iter()
            .find(|d| self.now_ms >= d.0 && self.now_ms < d.1)
        {
            let y = (self.now_ms - day.0) as f32 / 60_000.0 * theme::WEEK_PX_PER_MIN;
            layers.push(
                div()
                    .absolute()
                    .left(px(theme::WEEK_LABEL_W))
                    .right(px(0.))
                    .top(px(y - 1.0))
                    .h(px(2.))
                    .bg(theme::now_line())
                    .into_any_element(),
            );
        }

        let day_labels: Vec<AnyElement> = self
            .week_days
            .iter()
            .map(|d| {
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(theme::text_dim())
                    .child(SharedString::from(
                        local_date(d.0).format("%a %d").to_string(),
                    ))
                    .into_any_element()
            })
            .collect();

        let columns: Vec<AnyElement> = self
            .week_days
            .iter()
            .enumerate()
            .map(|(col, day)| {
                let mut blocks: Vec<AnyElement> = Vec::new();
                for event in &self.events {
                    if event.ends_utc <= day.0 || event.starts_utc >= day.1 {
                        continue;
                    }
                    let dragged = self
                        .drag
                        .as_ref()
                        .is_some_and(|d| drag_id(d) == Some(&event.id));
                    if dragged {
                        continue;
                    }
                    blocks.push(
                        event_block(event, day.0, day.1, theme::WEEK_PX_PER_MIN)
                            .left(px(2.))
                            .right(px(2.))
                            .into_any_element(),
                    );
                }
                if let Some(pv) = &preview {
                    if pv.ends_utc > day.0 && pv.starts_utc < day.1 {
                        blocks.push(
                            event_block(pv, day.0, day.1, theme::WEEK_PX_PER_MIN)
                                .left(px(2.))
                                .right(px(2.))
                                .opacity(0.9)
                                .border_1()
                                .border_color(theme::accent())
                                .into_any_element(),
                        );
                    }
                }
                let cell = self.week_col_bounds[col].clone();
                div()
                    .flex_1()
                    .relative()
                    .h_full()
                    .child(div().absolute().inset_0().child(canvas(
                        {
                            let cell = cell.clone();
                            move |bounds, _, _| cell.set(Some(bounds))
                        },
                        |_, _, _, _| {},
                    )))
                    .children(blocks)
                    .on_mouse_down(MouseButton::Left, {
                        let weak = weak.clone();
                        move |ev, window, cx| {
                            let plan = weak
                                .update(cx, |view, cx| view.week_down(ev.position, col, cx))
                                .ok()
                                .flatten();
                            if let Some(plan) = plan {
                                apply_plan(&weak, plan, window, cx);
                            }
                        }
                    })
                    .into_any_element()
            })
            .collect();

        // Day labels stay pinned above the scrolling hour grid so the week
        // always reads while scrolled.
        div()
            .flex()
            .flex_col()
            .flex_1()
            .child(
                div()
                    .flex()
                    .ml(px(theme::WEEK_LABEL_W))
                    .pb(px(2.))
                    .children(day_labels),
            )
            .child(
                div()
                    .id("week")
                    .flex_1()
                    .track_scroll(&self.scroll_week)
                    .overflow_y_scroll()
                    .child(
                        div()
                            .relative()
                            .h(px(theme::WEEK_GRID_H + 20.0))
                            .children(layers)
                            .child(
                                div()
                                    .flex()
                                    .ml(px(theme::WEEK_LABEL_W))
                                    .h_full()
                                    .children(columns),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn editor_panel(&self, weak: &WeakEntity<Self>) -> AnyElement {
        let Some(ed) = self.editor.as_ref() else {
            return div().into_any_element();
        };
        let title = "edit event".to_string();
        let weak = weak.clone();
        div()
            .absolute()
            .left(px((theme::COLUMN_W - 460.0) / 2.0))
            .top(px(56.))
            .w(px(460.))
            .rounded_md()
            .border_1()
            .border_color(theme::field_border())
            .bg(theme::panel())
            .p(px(12.))
            .flex()
            .flex_col()
            .gap_2()
            .on_key_down({
                let weak = weak.clone();
                move |ev, window, cx| {
                    let weak = weak.clone();
                    match ev.keystroke.key.as_str() {
                        "escape" => {
                            let _ = weak.update(cx, |view, cx| {
                                view.editor = None;
                                cx.notify();
                            });
                            if let Some(root) =
                                weak.update(cx, |view, _| view.focus_handle.clone()).ok()
                            {
                                window.focus(&root, cx);
                            }
                            cx.stop_propagation();
                        }
                        "enter" => {
                            let saved = weak
                                .update(cx, |view, cx| view.commit_editor(cx))
                                .unwrap_or(false);
                            if saved {
                                if let Some(root) =
                                    weak.update(cx, |view, _| view.focus_handle.clone()).ok()
                                {
                                    window.focus(&root, cx);
                                }
                            }
                            cx.stop_propagation();
                        }
                        "tab" => {
                            let backwards = ev.keystroke.modifiers.shift;
                            let target = weak
                                .update(cx, |view, cx| {
                                    view.editor.as_mut().map(|e| e.cycle_focus(backwards, cx))
                                })
                                .ok()
                                .flatten();
                            if let Some(target) = target {
                                window.focus(&target, cx);
                                cx.stop_propagation();
                            }
                        }
                        _ => {}
                    }
                }
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().text_color(theme::text_primary()).child(
                        if ed.editing_id.is_some() {
                            SharedString::from(title)
                        } else {
                            SharedString::from("new event")
                        },
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_dim())
                            .child(SharedString::from(ed.day.format("%Y-%m-%d").to_string())),
                    ),
            )
            .child(crate::editor::field_row("title", &ed.title))
            .child(crate::editor::field_row("kind", &ed.kind))
            .child(crate::editor::field_row("start", &ed.start))
            .child(crate::editor::field_row("end", &ed.end))
            .children(ed.error.clone().map(|msg| {
                div()
                    .text_xs()
                    .text_color(theme::danger())
                    .child(SharedString::from(msg))
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .cursor_pointer()
                            .px_3()
                            .py_1()
                            .rounded_sm()
                            .bg(hsla(0.55, 0.5, 0.6, 0.25))
                            .text_xs()
                            .text_color(theme::text_primary())
                            .child("save")
                            .id("ed-save")
                            .on_click({
                                let weak = weak.clone();
                                move |_, window, cx| {
                                    let saved = weak
                                        .update(cx, |view, cx| view.commit_editor(cx))
                                        .unwrap_or(false);
                                    if saved {
                                        if let Some(root) = weak
                                            .update(cx, |view, _| view.focus_handle.clone())
                                            .ok()
                                        {
                                            window.focus(&root, cx);
                                        }
                                    }
                                }
                            }),
                    )
                    .child(
                        div()
                            .cursor_pointer()
                            .px_3()
                            .py_1()
                            .rounded_sm()
                            .when(!ed.delete_armed, |d| d.bg(hsla(0.99, 0.6, 0.62, 0.15)))
                            .when(ed.delete_armed, |d| d.bg(hsla(0.99, 0.6, 0.62, 0.45)))
                            .text_xs()
                            .text_color(theme::danger())
                            .child(if ed.delete_armed {
                                "really delete?"
                            } else {
                                "delete"
                            })
                            .id("ed-delete")
                            .on_click({
                                let weak = weak.clone();
                                move |_, _, cx| {
                                    let _ = weak.update(cx, |view, cx| view.delete_clicked(cx));
                                }
                            }),
                    )
                    .child(
                        div()
                            .cursor_pointer()
                            .px_3()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(theme::field_border())
                            .text_xs()
                            .text_color(theme::text_dim())
                            .child("cancel")
                            .id("ed-cancel")
                            .on_click({
                                let weak = weak.clone();
                                move |_, window, cx| {
                                    let _ = weak.update(cx, |view, cx| {
                                        view.editor = None;
                                        cx.notify();
                                    });
                                    if let Some(root) =
                                        weak.update(cx, |view, _| view.focus_handle.clone()).ok()
                                    {
                                        window.focus(&root, cx);
                                    }
                                }
                            }),
                    ),
            )
            .into_any_element()
    }

    fn y_for_ms(&self, ms: i64, scale: f32) -> f32 {
        let minutes = (ms - self.win_start).clamp(0, 24 * 60 * 60_000) as f32 / 60_000.0;
        minutes * scale
    }
}

/// The window-having half of every gesture: build the editor here (field
/// states need `Window`), then hand it to the view.
fn apply_plan(weak: &WeakEntity<ScheduleView>, plan: Plan, window: &mut Window, cx: &mut App) {
    let Plan { editing, span, day } = plan;
    let values: [String; 4] = match &editing {
        Some(ev) => [
            ev.title.clone(),
            ev.kind.clone(),
            domain::local_hm(ev.starts_utc),
            domain::local_hm(ev.ends_utc),
        ],
        None => [
            String::new(),
            domain::DEFAULT_KIND.to_string(),
            domain::local_hm(span.0),
            domain::local_hm(span.1),
        ],
    };
    let editor = Editor::open(
        editing.map(|ev| ev.id),
        day,
        [
            values[0].as_str(),
            values[1].as_str(),
            values[2].as_str(),
            values[3].as_str(),
        ],
        window,
        cx,
    );
    let focus = editor.title.read(cx).focus_handle(cx);
    let _ = weak.update(cx, |view, cx| {
        view.editor = Some(editor);
        cx.notify();
    });
    window.focus(&focus, cx);
}

fn edit_plan(ev: &Event) -> Option<Plan> {
    Some(Plan {
        editing: Some(ev.clone()),
        span: (ev.starts_utc, ev.ends_utc),
        day: local_date(ev.starts_utc),
    })
}

fn drag_id(drag: &Drag) -> Option<&String> {
    match drag {
        Drag::Create { .. } => None,
        Drag::Move { id, .. } | Drag::Resize { id, .. } => Some(id),
    }
}

fn normalize_span(a: i64, b: i64) -> (i64, i64) {
    (a.min(b), a.max(b))
}

fn default_span() -> i64 {
    domain::DEFAULT_DURATION_MINUTES * 60_000
}

fn with_times(ev: &Event, starts: i64, ends: i64) -> Event {
    let mut out = ev.clone();
    out.starts_utc = starts;
    out.ends_utc = ends;
    out
}

/// Move: keep duration, clamp fully inside the visible day.
fn moved_times(ev: &Event, grab_off: i64, cur: i64, day: (i64, i64)) -> (i64, i64) {
    let dur = ev.ends_utc - ev.starts_utc;
    let s = (cur - grab_off).clamp(day.0, day.1 - dur);
    (s, s + dur)
}

/// Resize the picked edge; never shorter than one minute.
fn resized_times(ev: &Event, top_edge: bool, cur: i64, day: (i64, i64)) -> (i64, i64) {
    if top_edge {
        (cur.clamp(day.0, ev.ends_utc - 60_000), ev.ends_utc)
    } else {
        (ev.starts_utc, cur.clamp(ev.starts_utc + 60_000, day.1))
    }
}

/// Topmost block under `y_px` wins (later = painted on top); presses within
/// `EDGE_PX` of a tall block's edge resize, everything else moves.
fn hit_test(events: &[Event], day_start: i64, day_end: i64, y_px: f32, scale: f32) -> Option<Hit> {
    for (index, ev) in events.iter().enumerate().rev() {
        if ev.ends_utc <= day_start || ev.starts_utc >= day_end {
            continue;
        }
        let top = (ev.starts_utc.max(day_start) - day_start) as f32 / 60_000.0 * scale;
        let height = ((ev.ends_utc.min(day_end) - ev.starts_utc.max(day_start)) as f32 / 60_000.0
            * scale)
            .max(18.0);
        if y_px >= top && y_px <= top + height {
            // Blocks rendered at the minimum height are move-only: edge
            // zones would eat the whole block.
            let edge = if height <= 18.0 {
                Edge::Middle
            } else if y_px - top <= theme::EDGE_PX {
                Edge::Top
            } else if top + height - y_px <= theme::EDGE_PX {
                Edge::Bottom
            } else {
                Edge::Middle
            };
            return Some(Hit { index, edge });
        }
    }
    None
}

/// One absolutely-positioned block on a grid at `scale` px/min. The parent
/// decides horizontal placement via `left`/`right`; midnight-crossing
/// events are clipped to the day (data unsplit, decisions.md Q8).
fn event_block(event: &Event, day_start: i64, day_end: i64, scale: f32) -> gpui::Div {
    let start_min = (event.starts_utc.max(day_start) - day_start) as f32 / 60_000.0;
    let end_min = (event.ends_utc.min(day_end) - day_start) as f32 / 60_000.0;
    let top = start_min * scale;
    let height = ((end_min - start_min) * scale).max(18.0);
    let bg = domain::kind_hsla(&event.kind);
    let fg = domain::contrast_text(bg);
    div()
        .absolute()
        .left(px(theme::LABEL_W))
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
        .cursor_pointer()
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
                    domain::local_hm(event.starts_utc),
                    domain::local_hm(event.ends_utc)
                ))),
        )
}

fn local_date(ms: i64) -> NaiveDate {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .expect("valid local timestamp")
        .date_naive()
}

/// Local midnight→midnight for the seven days of the week containing
/// `week_start` (a Monday by construction).
fn week_days(week_start: i64) -> Vec<(i64, i64)> {
    let monday = local_date(week_start);
    (0..7)
        .map(|i| domain::day_bounds(monday + Days::new(i)))
        .collect()
}

fn fetch_range(from: i64, to: i64) -> Vec<Event> {
    match ipc::round_trip(&Request::Range { from, to }) {
        Ok(Response::Events { events }) => events,
        Ok(_) => Vec::new(),
        Err(err) => {
            log::warn!("range fetch failed: {err:#}");
            Vec::new()
        }
    }
}

fn fetch_stats(from: i64, to: i64) -> Vec<KindHours> {
    match ipc::round_trip(&Request::Stats { from, to }) {
        Ok(Response::Stats { stats }) => stats,
        Ok(_) => Vec::new(),
        Err(err) => {
            log::warn!("stats fetch failed: {err:#}");
            Vec::new()
        }
    }
}

/// Blocking watch thread — owns all socket I/O; the UI only receives
/// "changed" hints and refetches. Reconnects with a short backoff when the
/// daemon restarts.
fn spawn_watch_bridge(tx: UnboundedSender<()>) {
    std::thread::spawn(move || {
        loop {
            if tx.is_closed() {
                return;
            }
            match ipc::open_watch() {
                Ok(mut reader) => loop {
                    match ipc::read_changed(&mut reader) {
                        Ok(Some(_lsn)) => {
                            if tx.unbounded_send(()).is_err() {
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

// The week columns call `week_down(pos, col, cx)` — the view holds the
// day windows itself (`week_days`), so closures don't need to smuggle them.

impl Render for ScheduleView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = self.weak.clone();
        // One-time scroll-to-now per window (§18: offsets go negative).
        if self.mode == Mode::Day && !self.day_scrolled {
            self.day_scrolled = true;
            let viewport = f32::from(window.bounds().size.height) - theme::HEADER_H;
            let target = (self.y_for_ms(self.now_ms, theme::PX_PER_MIN) - viewport * 0.35).max(0.0);
            self.scroll_day.set_offset(point(px(0.), px(-target)));
        }
        if self.mode == Mode::Week && !self.week_scrolled {
            self.week_scrolled = true;
            let viewport = f32::from(window.bounds().size.height) - theme::HEADER_H;
            let today_start = self
                .week_days
                .iter()
                .find(|d| self.now_ms >= d.0 && self.now_ms < d.1);
            let target = match today_start {
                Some(day) => ((self.now_ms - day.0) as f32 / 60_000.0 * theme::WEEK_PX_PER_MIN
                    - viewport * 0.35)
                    .max(0.0),
                None => 0.0,
            };
            self.scroll_week.set_offset(point(px(0.), px(-target)));
        }

        let grid = match self.mode {
            Mode::Day => self.render_day(&weak),
            Mode::Week => self.render_week(&weak),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::backdrop())
            .text_color(theme::text_primary())
            // Root focus target: the Esc action reaches the view here first,
            // so an open editor swallows it before the shell quits.
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Quit, _, cx| {
                if this.editor.is_some() {
                    this.editor = None;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_mouse_move_all({
                let weak = weak.clone();
                move |ev, _, _, _, cx| {
                    let _ = weak.update(cx, |view, cx| view.grid_move(ev.position, cx));
                }
            })
            .on_mouse_up_all({
                let weak = weak.clone();
                move |ev, _, _, window, cx| {
                    if ev.button != MouseButton::Left {
                        return;
                    }
                    let plan = weak.update(cx, |view, cx| view.grid_up(cx)).ok().flatten();
                    if let Some(plan) = plan {
                        apply_plan(&weak, plan, window, cx);
                    }
                }
            })
            .child(
                div()
                    .relative()
                    .mx_auto()
                    .h_full()
                    .w_full()
                    .max_w(px(theme::COLUMN_W))
                    .flex()
                    .flex_col()
                    .child(self.header(&weak))
                    .when(self.mode == Mode::Week, |d| d.child(self.stats_strip()))
                    .child(grid)
                    .when(self.editor.is_some(), |d| d.child(self.editor_panel(&weak))),
            )
    }
}

impl Focusable for ScheduleView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: i64 = 86_400_000;

    fn ev(id: &str, start_min: i64, end_min: i64) -> Event {
        let mut e = Event::new(id.to_string(), None, start_min * 60_000, end_min * 60_000);
        e.id = id.to_string(); // Event::new mints a ULID; tests want fixed labels
        e
    }

    #[test]
    fn hit_test_prefers_topmost_and_finds_edges() {
        let events = vec![ev("a", 60, 120), ev("b", 90, 150)];
        // Overlap region 90–120: "b" painted later wins.
        let hit = hit_test(
            &events,
            0,
            DAY_MS,
            100.0 * theme::PX_PER_MIN,
            theme::PX_PER_MIN,
        )
        .unwrap();
        assert_eq!(events[hit.index].id, "b");
        // Near b's top edge (90 min * 2 px = 180): resize.
        let hit = hit_test(&events, 0, DAY_MS, 181.0, theme::PX_PER_MIN).unwrap();
        assert_eq!(events[hit.index].id, "b");
        assert_eq!(hit.edge, Edge::Top);
        // Near a's bottom edge (120 min * 2 = 240): that's inside b too
        // (b ends at 150*2=300), so b's interior wins.
        let hit = hit_test(&events, 0, DAY_MS, 239.0, theme::PX_PER_MIN).unwrap();
        assert_eq!(events[hit.index].id, "b");
        assert_eq!(hit.edge, Edge::Middle);
        // Below everything.
        assert!(hit_test(&events, 0, DAY_MS, 999.0, theme::PX_PER_MIN).is_none());
        // Short blocks (18 px < 2*EDGE) are always Middle.
        let short = vec![ev("s", 60, 65)];
        let hit = hit_test(&short, 0, DAY_MS, 122.0, theme::PX_PER_MIN).unwrap();
        assert_eq!(hit.edge, Edge::Middle);
    }

    #[test]
    fn moved_times_keep_duration_and_stay_in_day() {
        let e = ev("a", 9 * 60, 10 * 60 + 30); // 90 min
        let day = (0, DAY_MS);
        let (s, e2) = moved_times(&e, 30 * 60_000, 8 * 60 * 60_000 + 40 * 60_000, day);
        assert_eq!(e2 - s, 90 * 60_000);
        // Clamp at day start.
        let (s, e2) = moved_times(&e, 9 * 60 * 60_000, 0, day);
        assert_eq!((s, e2), (0, 90 * 60_000));
        // Clamp at day end.
        let (s, e2) = moved_times(&e, 0, DAY_MS, day);
        assert_eq!((s, e2), (DAY_MS - 90 * 60_000, DAY_MS));
    }

    #[test]
    fn resized_times_never_invert_or_leave_day() {
        let e = ev("a", 9 * 60, 10 * 60);
        let day = (0, DAY_MS);
        // Bottom edge up to the start → 1 min minimum.
        let (s, e2) = resized_times(&e, false, 9 * 60 * 60_000 + 30_000, day);
        assert_eq!((s, e2), (9 * 60 * 60_000, 9 * 60 * 60_000 + 60_000));
        // Top edge past the day start clamps at midnight.
        let (s, e2) = resized_times(&e, true, -5_000, day);
        assert_eq!((s, e2), (0, 10 * 60 * 60_000));
    }

    #[test]
    fn normalize_span_orders_drag_edges() {
        assert_eq!(normalize_span(500, 100), (100, 500));
    }
}
