//! The card and buttons a confirmation is drawn with.

use gpui::{
    App, ClickEvent, Div, ElementId, Hsla, SharedString, Stateful, Window, prelude::*, px, rems,
};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::ui::presets::Surface;

const STEP: f32 = 1. / 16.;
pub(crate) const TEXT: f32 = 14. * STEP;
pub(crate) const TAB_TEXT: f32 = 12.5 * STEP;

/// The card's corner.
pub(crate) const CARD_RADIUS: f32 = 12.;

/// A button: the field's height and the field's corner.
pub(crate) const BUTTON_H: f32 = 28.;
const BUTTON_RADIUS: f32 = 7.;
const BUTTON_PAD: f32 = 14.;

/// The ladder a dialog's hovers are taken from. Cards sit on the popover surface.
pub(crate) fn popover_rungs(cx: &App) -> Surface {
    cx.global::<crate::ui::presets::Surfaces>().popover
}

/// The card itself, opaque and occluding.
///
/// `title` is what a screen reader announces the card as — a dialog, named
/// by the question it asks.
pub(crate) fn card(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    width: f32,
    cx: &App,
) -> Stateful<Div> {
    v_flex()
        .id(id)
        .role(gpui::Role::Dialog)
        .aria_label(title)
        .occlude()
        .w(px(width))
        .map(|panel| crate::ui::theme::floating_surface(panel, cx))
        .rounded(px(CARD_RADIUS))
        .overflow_hidden()
}

/// What a button is asking for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    /// The one answer a card is for: an inverted neutral, the surface's ink as
    /// the fill.
    Primary,
    /// Everything else: the well's faint fill at rest, one rung up under the
    /// pointer.
    Secondary,
    /// The action a card warns about.
    Danger,
}

/// A dialog button. The click handler is attached only while `enabled`: a
/// disabled button has to be inert, not merely grey.
pub(crate) fn button(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    tone: Tone,
    enabled: bool,
    rungs: Surface,
    cx: &App,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let theme = cx.theme();
    let surface: Hsla = gpui::rgb(rungs.base).into();
    let (fill, hover, ink): (Option<Hsla>, Option<Hsla>, Hsla) = match (tone, enabled) {
        (_, false) => (
            Some(gpui::rgb(rungs.hover).into()),
            None,
            theme.muted_foreground,
        ),
        (Tone::Primary, true) => {
            let fg = theme.foreground;
            (Some(fg), Some(fg.blend(surface.opacity(0.14))), surface)
        }
        (Tone::Danger, true) => {
            let red = theme.danger;
            (
                Some(red),
                Some(red.blend(surface.opacity(0.14))),
                theme.danger_foreground,
            )
        }
        (Tone::Secondary, true) => (
            Some(gpui::rgb(rungs.hover).into()),
            Some(gpui::rgb(rungs.selected).into()),
            theme.foreground,
        ),
    };
    h_flex()
        .id(id)
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(BUTTON_H))
        .px(px(BUTTON_PAD))
        .rounded(px(BUTTON_RADIUS))
        .text_size(rems(TAB_TEXT))
        .when(tone != Tone::Secondary, |b| {
            b.font_weight(gpui::FontWeight::MEDIUM)
        })
        .text_color(ink)
        .when_some(fill, |b, fill| b.bg(fill))
        .when_some(hover, |b, hover| b.hover(move |s| s.bg(hover)))
        .when(enabled, |b| b.cursor_pointer().on_click(on_click))
        .child(text.into())
}
