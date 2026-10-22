use super::*;
use gpui::{Modifiers, MouseButton, TestAppContext, point};
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Default)]
struct GestureCounts {
    checkbox: Rc<Cell<usize>>,
    row_down: Rc<Cell<usize>>,
    row_up: Rc<Cell<usize>>,
    row_click: Rc<Cell<usize>>,
}

struct CheckboxTestView {
    counts: GestureCounts,
    disabled: bool,
    labelled: bool,
}

impl Render for CheckboxTestView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let counts = self.counts.clone();
        let down = counts.row_down.clone();
        let up = counts.row_up.clone();
        let click = counts.row_click.clone();
        div().size_full().p_4().child(
            h_flex()
                .id("row")
                .debug_selector(|| "CHECKBOX_TEST_ROW".into())
                .w(px(400.))
                .h(px(40.))
                .justify_end()
                .on_mouse_down(MouseButton::Left, move |_, _, _| down.set(down.get() + 1))
                .on_mouse_up(MouseButton::Left, move |_, _, _| up.set(up.get() + 1))
                .on_click(move |_, _, _| click.set(click.get() + 1))
                .child(review_checkbox(
                    "review-checkbox-test-target",
                    Checkbox::new("review-checkbox-test", false.into())
                        .when(self.labelled, |checkbox| checkbox.label("Viewed")),
                    self.disabled,
                    move |_, _| counts.checkbox.set(counts.checkbox.get() + 1),
                )),
        )
    }
}

#[gpui::test]
fn checkbox_pointer_target_does_not_select_or_open_enclosing_row(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
    });
    let counts = GestureCounts::default();
    let (_view, cx) = cx.add_window_view(|_, _| CheckboxTestView {
        counts: counts.clone(),
        disabled: false,
        labelled: false,
    });
    let target = cx
        .debug_bounds("REVIEW_CHECKBOX_TARGET")
        .expect("checkbox target");
    assert!(target.size.width >= px(24.) && target.size.height >= px(24.));
    for position in [
        target.center(),
        point(target.left() + px(1.), target.center().y),
        point(target.right() - px(1.), target.center().y),
        point(target.center().x, target.top() + px(1.)),
        point(target.center().x, target.bottom() - px(1.)),
    ] {
        cx.simulate_mouse_move(position, None, Modifiers::default());
        cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::default());
    }
    assert_eq!(
        counts.checkbox.get(),
        5,
        "center and padded edges activate once"
    );
    assert_eq!(
        counts.row_down.get(),
        0,
        "no row/editor mouse-down selection"
    );
    assert_eq!(counts.row_up.get(), 0);
    assert_eq!(
        counts.row_click.get(),
        0,
        "no file opening or folder collapse"
    );

    let row = cx.debug_bounds("CHECKBOX_TEST_ROW").expect("row");
    let outside = point(row.left() + px(2.), row.center().y);
    cx.simulate_mouse_move(outside, None, Modifiers::default());
    cx.simulate_mouse_down(outside, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_up(outside, MouseButton::Left, Modifiers::default());
    assert_eq!(
        counts.row_down.get(),
        1,
        "ordinary row behavior is preserved"
    );
    assert_eq!(counts.row_up.get(), 1);
    assert_eq!(counts.row_click.get(), 1);
}

#[gpui::test]
fn disabled_labelled_checkbox_absorbs_gesture_without_mutation(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
    });
    let counts = GestureCounts::default();
    let (view, cx) = cx.add_window_view(|_, _| CheckboxTestView {
        counts: counts.clone(),
        disabled: true,
        labelled: true,
    });
    let target = cx
        .debug_bounds("REVIEW_CHECKBOX_TARGET")
        .expect("checkbox target");
    for position in [
        target.center(),
        point(target.right() - px(1.), target.center().y),
    ] {
        cx.simulate_mouse_move(position, None, Modifiers::default());
        cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::default());
    }
    assert_eq!(counts.checkbox.get(), 0);
    assert_eq!(counts.row_down.get(), 0);
    assert_eq!(counts.row_up.get(), 0);
    assert_eq!(counts.row_click.get(), 0);

    view.update(cx, |view, cx| {
        view.disabled = false;
        cx.notify();
    });
    cx.run_until_parked();
    let target = cx
        .debug_bounds("REVIEW_CHECKBOX_TARGET")
        .expect("enabled labelled target");
    let label = point(target.right() - px(1.), target.center().y);
    cx.simulate_mouse_move(label, None, Modifiers::default());
    cx.simulate_mouse_down(label, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_up(label, MouseButton::Left, Modifiers::default());
    assert_eq!(
        counts.checkbox.get(),
        1,
        "the label is part of the enabled action target"
    );
    assert_eq!(counts.row_down.get(), 0);
    assert_eq!(counts.row_up.get(), 0);
    assert_eq!(counts.row_click.get(), 0);
}
