use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use editor::{Editor, EditorMode, MultiBuffer, display_map::DisplayRow};
use gpui::{
    AnyWindowHandle, AppContext as _, BenchAppContext, Context, Entity, Focusable as _,
    IntoElement, ParentElement as _, Render, Styled as _, Window, div,
};
use language::{Buffer, LineEnding, Rope};

#[gpui::bench(
    inputs = pending_long_row_characters(),
    group = "Scroll a long row while it is measured",
    input_name = "character",
    sample_size = 10
)]
fn editor_scroll_pending_long_row(character: &char, cx: &mut BenchAppContext) {
    init_context(cx);

    let text = Rope::from(
        character
            .to_string()
            .repeat(PENDING_LONG_ROW_CHARACTERS)
            .as_str(),
    );
    let mut session_window = None::<AnyWindowHandle>;
    cx.bench_renderer_session(
        PENDING_LONG_ROW_SESSION_TIMEOUT,
        move |cx| {
            let window =
                *session_window.get_or_insert_with(|| cx.add_empty_window().window_handle());
            let text = text.clone();
            let editor_slot = Rc::new(RefCell::new(None));
            cx.update_window(window, |_, window, cx| {
                let editor = cx.new(|cx| {
                    let buffer = cx.new(|cx| Buffer::local_normalized(text, LineEnding::Unix, cx));
                    let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
                    let mut editor = Editor::new(EditorMode::full(), buffer, None, window, cx);
                    editor.set_style(editor::EditorStyle::default(), window, cx);
                    editor
                });
                editor_slot.replace(Some(editor.clone()));
                window.replace_root(cx, |_, _| PendingLongRowRoot {
                    editor: editor_slot.clone(),
                });
                window.focus(&editor.focus_handle(cx), cx);
            })
            .expect("the benchmark window must remain open");
            let stopped = Arc::new(AtomicBool::new(false));
            let session = PendingLongRowSession {
                editor: editor_slot,
                stopped: stopped.clone(),
                scroll_x: PENDING_LONG_ROW_SCROLL_START,
            };
            (session, window, stopped)
        },
        |session, frame, window, cx| {
            if frame == PENDING_LONG_ROW_FRAMES {
                session.stopped.store(true, Ordering::Release);
                return;
            }
            let Some(editor) = session.editor.borrow().clone() else {
                return;
            };
            editor.update(cx, |editor, cx| {
                let is_pending = editor
                    .display_snapshot(cx)
                    .long_row(DisplayRow(0))
                    .is_some_and(|long_row| !long_row.is_measured());
                assert!(
                    is_pending,
                    "the long row finished measuring before scroll frame {frame}, \
                     increase PENDING_LONG_ROW_CHARACTERS"
                );
                session.scroll_x += PENDING_LONG_ROW_SCROLL_STEP;
                editor.set_scroll_position(gpui::point(session.scroll_x, 0.), window, cx);
            });
        },
    );
}

struct PendingLongRowSession {
    editor: Rc<RefCell<Option<Entity<Editor>>>>,
    stopped: Arc<AtomicBool>,
    scroll_x: f64,
}

impl Drop for PendingLongRowSession {
    fn drop(&mut self) {
        self.editor.borrow_mut().take();
    }
}

struct PendingLongRowRoot {
    editor: Rc<RefCell<Option<Entity<Editor>>>>,
}

impl Render for PendingLongRowRoot {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().children(self.editor.borrow().clone())
    }
}

const PENDING_LONG_ROW_CHARACTERS: usize = 20_000_000;
const PENDING_LONG_ROW_FRAMES: u64 = 8;
const PENDING_LONG_ROW_SCROLL_START: f64 = 1_000_000.;
const PENDING_LONG_ROW_SCROLL_STEP: f64 = 200.;
const PENDING_LONG_ROW_SESSION_TIMEOUT: Duration = Duration::from_secs(30);

fn pending_long_row_characters() -> Vec<char> {
    vec!['x', 'é', '漢']
}

fn init_context(cx: &mut BenchAppContext) {
    cx.update(|cx| {
        settings::init(cx);
        assets::Assets.load_test_fonts(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    });
}

gpui::bench_group!(benches, editor_scroll_pending_long_row);
gpui::bench_main!(benches);
