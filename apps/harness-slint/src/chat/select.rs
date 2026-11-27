//! Selecting text across blocks of the chat.
//!
//! Every paragraph, heading, list item and code block is a text field of its own, and a text field's
//! selection ends at its edge: dragging from one paragraph into the next used to stop there. This module
//! follows the pointer through the window's own events (a field that is being dragged in keeps the mouse to
//! itself, so nothing else in the page hears where the pointer is) and tells the page the span between the
//! field the drag began in and the pointer. Fields in that span select themselves whole and report their
//! text; Ctrl+C in the first field then copies its part and theirs, top to bottom.

use std::cell::RefCell;
use std::collections::BTreeMap;

use slint::ComponentHandle;

use crate::ui::{AppWindow, ChatUi};

#[derive(Default)]
struct Drag {
    /// The left button is down, and where the pointer is (logical pixels from the window's top).
    down: bool,
    y: f32,
    /// The field the selection began in: its top and bottom. `None` until it has a selection.
    origin: Option<(f32, f32)>,
    /// The message the first field belongs to: its top and bottom. The span never leaves it.
    bounds: (f32, f32),
    /// The span told to the page.
    span: (f32, f32),
    /// The fields that joined, by where they sit (message id, block, list row), and their text.
    joined: BTreeMap<Key, String>,
}

/// Where a text field sits: the message (`ChatItem.id`), the block of its Markdown, the row of a list.
type Key = (i32, i32, i32);

thread_local! {
    static DRAG: RefCell<Drag> = RefCell::default();
}

/// How far (logical pixels) the pointer must leave the first field before other blocks join the selection.
const LEAVE: f32 = 6.0;

/// The part of the page between the first field and the pointer: below the field when the pointer is under
/// it, above when over it, nothing while the pointer is still inside (or just past its edge).
fn span(origin: (f32, f32), bounds: (f32, f32), y: f32) -> (f32, f32) {
    // 修复：列表项、单行段落只有一行高，横向拖选时指针上下抖一两个像素就算“离开”了起点块，
    // 上面（或下面）的段落被整段加入选区，复制时混进没想选的文字。现在要明显越过边缘才算离开。
    // 修复：选区只在起点所在的那条消息内扩展（看不见的框），指针拖出消息也只选到消息的首/尾，
    // 别的消息永远不会混进复制内容。至少保留 1px，起点块仍能识别“已离开”并选到自身首/尾。
    if y > origin.1 + LEAVE {
        (origin.1, y.min(bounds.1).max(origin.1 + 1.0))
    } else if y < origin.0 - LEAVE {
        (y.max(bounds.0).min(origin.0 - 1.0), origin.0)
    } else {
        (0.0, 0.0)
    }
}

fn show(ui: &ChatUi<'_>, drag: &mut Drag) {
    let Some(origin) = drag.origin else { return };
    let next = span(origin, drag.bounds, drag.y);
    if next != drag.span {
        drag.span = next;
        ui.set_drag_lo(next.0);
        ui.set_drag_hi(next.1);
    }
}

/// The pointer moved; `y` in logical pixels.
pub fn moved(window: &AppWindow, y: f32) {
    DRAG.with(|cell| {
        let mut drag = cell.borrow_mut();
        drag.y = y;
        if drag.down && drag.origin.is_some() {
            show(&window.global::<ChatUi>(), &mut drag);
        }
    });
}

/// The left button went down or came up. A new press ends the selection that stood.
pub fn button(window: &AppWindow, down: bool) {
    DRAG.with(|cell| {
        let mut drag = cell.borrow_mut();
        drag.down = down;
        window.global::<ChatUi>().set_drag_held(down);
        if down {
            // 修复：新的按下总是丢掉上一轮登记的段落，不再只在有起点时才清，避免残留文字混进下一次复制。
            drag.joined.clear();
        }
        if down && drag.origin.take().is_some() {
            drag.span = (0.0, 0.0);
            let ui = window.global::<ChatUi>();
            ui.set_drag_on(false);
            ui.set_drag_lo(0.0);
            ui.set_drag_hi(0.0);
        }
    });
}

/// The text of everything selected, top to bottom: `origin` is the first field's place and selected part.
/// Only blocks of the first field's message count.
fn gather(joined: &BTreeMap<Key, String>, origin: (Key, &str)) -> String {
    // 修复：只取起点那条消息里的块，按块/行顺序拼接；别的消息（含列表复用后残留的登记）一律不复制。
    let mut parts: Vec<(Key, &str)> = joined
        .iter()
        .filter(|(at, _)| at.0 == origin.0 .0 && **at != origin.0)
        .map(|(at, text)| (*at, text.as_str()))
        .collect();
    parts.push(origin);
    parts.sort_by_key(|(at, _)| *at);
    parts
        .iter()
        .map(|(_, text)| text.trim_end())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn install(window: &AppWindow, copy: impl Fn(&str) + 'static) {
    let ui = window.global::<ChatUi>();
    let weak = window.as_weak();
    ui.on_drag_begin(move |top, bottom, box_top, box_bottom| {
        let Some(window) = weak.upgrade() else { return };
        DRAG.with(|cell| {
            let mut drag = cell.borrow_mut();
            if !drag.down || drag.origin.is_some() {
                return;
            }
            drag.origin = Some((top, bottom));
            drag.bounds = (box_top.min(top), box_bottom.max(bottom));
            let ui = window.global::<ChatUi>();
            show(&ui, &mut drag);
            ui.set_drag_on(true);
        });
    });
    ui.on_drag_join(|msg, block, row, text| {
        DRAG.with(|cell| {
            cell.borrow_mut()
                .joined
                .insert((msg, block, row), text.into());
        });
    });
    ui.on_drag_part(|msg, block, row| {
        DRAG.with(|cell| {
            cell.borrow_mut().joined.remove(&(msg, block, row));
        });
    });
    ui.on_drag_copy(move |text, from, to, msg, block, row| {
        let (from, to) = (from.min(to), from.max(to));
        let part = usize::try_from(from)
            .ok()
            .zip(usize::try_from(to).ok())
            .and_then(|(from, to)| text.as_str().get(from..to))
            .unwrap_or_default();
        let all = DRAG.with(|cell| gather(&cell.borrow().joined, ((msg, block, row), part)));
        copy(&all);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_span_runs_from_the_first_field_to_the_pointer() {
        let origin = (100.0, 140.0);
        let open = (0.0, 1000.0);
        assert_eq!(span(origin, open, 120.0), (0.0, 0.0));
        // A pixel or two past the edge is a wobble, not a move into the next block.
        assert_eq!(span(origin, open, 142.0), (0.0, 0.0));
        assert_eq!(span(origin, open, 97.0), (0.0, 0.0));
        assert_eq!(span(origin, open, 300.0), (140.0, 300.0));
        assert_eq!(span(origin, open, 40.0), (40.0, 100.0));
    }

    #[test]
    fn the_span_stays_inside_the_message() {
        let origin = (100.0, 140.0);
        // The message runs from 80 to 220: dragging past it stops at its edge.
        assert_eq!(span(origin, (80.0, 220.0), 600.0), (140.0, 220.0));
        assert_eq!(span(origin, (80.0, 220.0), 10.0), (80.0, 100.0));
        // The first field is the message's last block: the span still leaves it, by one pixel only.
        assert_eq!(span(origin, (80.0, 140.0), 600.0), (140.0, 141.0));
        assert_eq!(span(origin, (100.0, 220.0), 10.0), (99.0, 100.0));
    }

    #[test]
    fn what_is_copied_reads_top_to_bottom() {
        let mut joined = BTreeMap::new();
        joined.insert((7, 3, 0), "third paragraph\n".to_owned());
        joined.insert((7, 2, 0), "first item".to_owned());
        joined.insert((7, 2, 1), "second item".to_owned());
        joined.insert((7, 4, 0), "  ".to_owned());
        assert_eq!(
            gather(&joined, ((7, 1, 0), "end of the first")),
            "end of the first\nfirst item\nsecond item\nthird paragraph"
        );
        // Dragging upwards: the first field is the last to read.
        assert_eq!(
            gather(&joined, ((7, 5, 0), "start of the last"))
                .lines()
                .last(),
            Some("start of the last")
        );
    }

    #[test]
    fn other_messages_are_never_copied() {
        let mut joined = BTreeMap::new();
        // A block of an earlier message left behind (a recycled list row that never parted).
        joined.insert((3, 9, 0), "from another answer".to_owned());
        joined.insert((7, 0, 0), "the paragraph above".to_owned());
        // A stale entry for the first field itself is not read twice.
        joined.insert((7, 1, 0), "the whole first field".to_owned());
        assert_eq!(
            gather(&joined, ((7, 1, 0), "part of it")),
            "the paragraph above\npart of it"
        );
    }
}
