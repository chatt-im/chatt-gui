use gpui::{AnyElement, Div, SharedString, Stateful, div, prelude::*};

use crate::{
    icons::{IconName, icon},
    theme::{ThemePalette, ThemeRole},
    ui_scale::rems_from_px,
};

pub(crate) fn room_button(
    id: impl Into<gpui::ElementId>,
    sigil: &'static str,
    label: String,
    active: bool,
    unread: u32,
    palette: &ThemePalette,
) -> Stateful<Div> {
    let background = palette.color(if active {
        ThemeRole::StateSelected
    } else {
        ThemeRole::Sidebar
    });
    let text = palette.color(if active {
        ThemeRole::TextPrimary
    } else {
        ThemeRole::TextSecondary
    });
    let hover = palette.color(ThemeRole::StateHover);
    let pressed = palette.color(ThemeRole::StatePressed);
    div()
        .id(id)
        .mx_2()
        .min_h(rems_from_px(34.))
        .px_2()
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .bg(background)
        .when(!active, |button| {
            button.hover(move |button| button.bg(hover))
        })
        .active(move |button| button.bg(pressed))
        .text_color(text)
        .child(
            div()
                .w(rems_from_px(16.))
                .flex_none()
                .text_center()
                .child(sigil),
        )
        .child(div().min_w_0().flex_1().truncate().child(label))
        .when(unread > 0, |button| {
            button.child(
                div()
                    .text_xs()
                    .px_2()
                    .bg(palette.color(ThemeRole::ControlActive))
                    .child(unread.to_string()),
            )
        })
}

pub(crate) fn toolbar_button(
    id: &'static str,
    icon_name: Option<IconName>,
    label: &'static str,
    palette: &ThemePalette,
) -> Stateful<Div> {
    let hover = palette.color(ThemeRole::ControlButtonHover);
    div()
        .id(id)
        .min_h(rems_from_px(30.))
        .px_2()
        .flex()
        .items_center()
        .justify_center()
        .gap_1()
        .cursor_pointer()
        .bg(palette.color(ThemeRole::ControlButton))
        .hover(move |button| button.bg(hover))
        .text_xs()
        .when_some(icon_name, |button, icon_name| {
            button.child(icon(
                icon_name,
                15.0,
                palette.color(ThemeRole::TextSecondary),
            ))
        })
        .child(label)
}

pub(crate) fn mini_button(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    palette: &ThemePalette,
) -> Stateful<Div> {
    let hover = palette.color(ThemeRole::ControlSurfaceHover);
    div()
        .id(id)
        .min_h(rems_from_px(28.))
        .px_2()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .bg(palette.color(ThemeRole::ControlSurface))
        .hover(move |button| button.bg(hover))
        .text_xs()
        .child(label)
}

pub(crate) fn icon_button(
    id: impl Into<gpui::ElementId>,
    icon_name: IconName,
    palette: &ThemePalette,
) -> Stateful<Div> {
    let hover = palette.color(ThemeRole::ControlActive);
    let active_text = palette.color(ThemeRole::ControlActiveText);
    div()
        .id(id)
        .size(rems_from_px(28.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .bg(palette.color(ThemeRole::ControlSurface))
        .text_color(palette.color(ThemeRole::TextSecondary))
        .hover(move |button| button.bg(hover).text_color(active_text))
        .child(icon(icon_name, 17.0, palette.color(ThemeRole::TextPrimary)))
}

pub(crate) fn message_action_button(
    id: impl Into<gpui::ElementId>,
    icon_name: IconName,
    destructive: bool,
    palette: &ThemePalette,
) -> Stateful<Div> {
    let hover = palette.color(ThemeRole::ControlSurfaceHover);
    let hover_text = palette.color(if destructive {
        ThemeRole::StateDanger
    } else {
        ThemeRole::TextPrimary
    });
    div()
        .id(id)
        .size(rems_from_px(28.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .bg(palette.color(ThemeRole::ControlSurface))
        .text_color(palette.color(ThemeRole::TextMuted))
        .hover(move |button| button.bg(hover).text_color(hover_text))
        .child(icon(
            icon_name,
            16.0,
            palette.color(if destructive {
                ThemeRole::StateDanger
            } else {
                ThemeRole::TextSecondary
            }),
        ))
}

pub(crate) fn composer_add_button(ready: bool, palette: &ThemePalette) -> Stateful<Div> {
    let color = palette.color(if ready {
        ThemeRole::TextMuted
    } else {
        ThemeRole::StateDisabled
    });
    let hover = palette.color(ThemeRole::TextPrimary);
    div()
        .id("add-media")
        .size(rems_from_px(36.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_color(color)
        .hover(move |button| button.text_color(hover))
        .child(icon(IconName::Plus, 24.0, color))
}

pub(crate) fn preview_action_button(
    id: &'static str,
    icon_name: IconName,
    palette: &ThemePalette,
) -> Stateful<Div> {
    let hover = palette.color(ThemeRole::Window);
    let hover_text = palette.color(ThemeRole::TextPrimary);
    div()
        .id(id)
        .size(rems_from_px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_color(palette.color(ThemeRole::TextMuted))
        .hover(move |button| button.bg(hover).text_color(hover_text))
        .child(icon(
            icon_name,
            17.0,
            palette.color(ThemeRole::TextSecondary),
        ))
}

pub(crate) fn preview_status(
    message: impl Into<SharedString>,
    palette: &ThemePalette,
) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(palette.color(ThemeRole::MediaViewport))
        .text_sm()
        .text_color(palette.color(ThemeRole::TextMuted))
        .child(message.into())
        .into_any_element()
}

pub(crate) fn preview_control_button(
    id: &'static str,
    label: &'static str,
    palette: &ThemePalette,
) -> Stateful<Div> {
    let hover = palette.color(ThemeRole::ControlActive);
    div()
        .id(id)
        .min_w(rems_from_px(32.0))
        .min_h(rems_from_px(28.0))
        .px_2()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .bg(palette.color(ThemeRole::ControlSurface))
        .hover(move |button| button.bg(hover))
        .text_xs()
        .child(label)
}
