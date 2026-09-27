//! The event editor: a floating panel with four single-line fields (title,
//! kind, start, end) built on the fork's editable_text inputs, plus
//! Save / Delete / Cancel wiring lives in the schedule view. Field state
//! persists across opens via `use_keyed` ids and is re-emplaced on open
//! (`emplace` resets content and caret).

use chrono::NaiveDate;
use gpui::{
    App, ElementId, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Styled, Window,
    div, px,
};
use gpui_elements::editable_text::{EditableTextState, text_input};

use crate::theme;

const FIELD_IDS: [&str; 4] = ["ed-title", "ed-kind", "ed-start", "ed-end"];

pub struct Editor {
    /// `None` while creating a new event.
    pub editing_id: Option<String>,
    /// Local date the `HH:MM` fields are interpreted on (the event's day).
    pub day: NaiveDate,
    pub title: Entity<EditableTextState>,
    pub kind: Entity<EditableTextState>,
    pub start: Entity<EditableTextState>,
    pub end: Entity<EditableTextState>,
    /// Which field Tab cycles from (approximate: mouse focus changes in a
    /// field are not tracked).
    pub focus_idx: usize,
    /// Delete needs a second click within the panel's lifetime to fire.
    pub delete_armed: bool,
    pub error: Option<String>,
}

impl Editor {
    /// `values` is `[title, kind, start_hm, end_hm]`.
    pub fn open(
        editing_id: Option<String>,
        day: NaiveDate,
        values: [&str; 4],
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let fields: [Entity<EditableTextState>; 4] = std::array::from_fn(|i| {
            let state = EditableTextState::use_keyed(ElementId::from(FIELD_IDS[i]), window, cx);
            state.update(cx, |s, cx| s.emplace(values[i], cx));
            state
        });
        Self {
            editing_id,
            day,
            title: fields[0].clone(),
            kind: fields[1].clone(),
            start: fields[2].clone(),
            end: fields[3].clone(),
            focus_idx: 0,
            delete_armed: false,
            error: None,
        }
    }

    fn handles(&self, cx: &App) -> [FocusHandle; 4] {
        [
            self.title.read(cx).focus_handle(cx),
            self.kind.read(cx).focus_handle(cx),
            self.start.read(cx).focus_handle(cx),
            self.end.read(cx).focus_handle(cx),
        ]
    }

    /// Next (or previous) field handle for Tab cycling.
    pub fn cycle_focus(&mut self, backwards: bool, cx: &App) -> FocusHandle {
        const N: usize = 4;
        self.focus_idx = if backwards {
            (self.focus_idx + N - 1) % N
        } else {
            (self.focus_idx + 1) % N
        };
        self.handles(cx)[self.focus_idx].clone()
    }
}

/// One labeled `text_input` row; the element owns its FocusHandle, so it
/// participates in the panel's Tab cycle naturally.
pub fn field_row(label: &'static str, state: &Entity<EditableTextState>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(48.))
                .text_xs()
                .text_color(theme::text_dim())
                .child(label),
        )
        .child(
            text_input(ElementId::from(label))
                .state(state.downgrade())
                .placeholder(match label {
                    "title" => "what",
                    "kind" => domain_default_kind(),
                    _ => "HH:MM",
                })
                .flex_1()
                .min_h_auto()
                .whitespace_nowrap()
                .overflow_hidden()
                .px_2()
                .py_1()
                .rounded_sm()
                .border_1()
                .border_color(theme::field_border())
                .bg(theme::field_bg())
                .text_sm(),
        )
}

fn domain_default_kind() -> &'static str {
    suemo::domain::DEFAULT_KIND
}
