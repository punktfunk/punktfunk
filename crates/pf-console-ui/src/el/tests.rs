use super::*;
use std::cell::RefCell;

fn canvas() -> skia_safe::Surface {
    skia_safe::surfaces::raster_n32_premul((1280, 800)).expect("raster surface")
}

/// Rows as `MenuList` spaces them: `ROW_H` rows `ROW_GAP` apart, a `HEADER_H` band
/// above a sectioned row, `ROW_MAX_W` wide and centred.
#[test]
fn a_menu_column_lays_out_the_list_rhythm() {
    let (row_h, gap, header_h, max_w) = (50.0, 6.0, 34.0, 620.0);
    let headed = [false, false, true, false, true];
    let list = Id::new("list", 0);
    let root = El::scroll(list, Axis::Vertical)
        .gap(gap)
        .style(|s| s.align_items = Some(taffy::AlignItems::CENTER))
        .children(headed.iter().enumerate().map(|(i, &h)| {
            let row = El::paint(|_, _| {})
                .id(Id::new("row", i))
                .size(max_w, row_h);
            match h {
                true => El::column().child(El::column().height(header_h)).child(row),
                false => row,
            }
        }));
    let mut tree = Tree::new();
    let frame = tree.layout(root, Rect::from_xywh(0.0, 100.0, 1280.0, 600.0));
    let tops: Vec<f32> = (0..headed.len())
        .map(|i| frame.rect(Id::new("row", i)).unwrap().top - 100.0)
        .collect();
    assert_eq!(tops, [0.0, 56.0, 146.0, 202.0, 292.0]);
    let r = frame.rect(Id::new("row", 0)).unwrap();
    assert_eq!(
        (r.left, r.width(), r.height()),
        ((1280.0 - max_w) / 2.0, max_w, row_h)
    );
    // Content ends at the last row; it fits, so nothing scrolls.
    assert_eq!(frame.scroll(list).map(|(_, max)| max), Some(0.0));
}

fn tall_list<'a>(id: Id, rows: usize, drawn: &'a RefCell<Vec<usize>>) -> El<'a> {
    El::scroll(id, Axis::Vertical)
        .gap(10.0)
        .children((0..rows).map(move |i| {
            El::paint(move |_, _| drawn.borrow_mut().push(i))
                .id(Id::new("row", i))
                .height(90.0)
        }))
}

#[test]
fn a_scroll_offsets_clips_and_hit_tests_its_children() {
    let list = Id::new("list", 0);
    let drawn = RefCell::new(Vec::new());
    let mut surface = canvas();
    let mut tree = Tree::new();
    let view = Rect::from_xywh(0.0, 0.0, 400.0, 300.0);
    // 20 rows at a 100 px pitch, 300 px of viewport.
    let frame = tree.layout(tall_list(list, 20, &drawn), view);
    assert_eq!(frame.scroll(list).map(|(_, max)| max), Some(1690.0));
    tree.paint(surface.canvas(), frame);
    assert_eq!(*drawn.borrow(), [0, 1, 2]);
    assert_eq!(tree.hit(10.0, 150.0), Some(Id::new("row", 1)));
    // The gap between rows belongs to the viewport, not a row.
    assert_eq!(tree.hit(10.0, 95.0), Some(list));

    tree.set_offset(list, 250.0);
    drawn.borrow_mut().clear();
    let frame = tree.layout(tall_list(list, 20, &drawn), view);
    tree.paint(surface.canvas(), frame);
    assert_eq!(*drawn.borrow(), [2, 3, 4, 5]);
    assert_eq!(tree.rect(Id::new("row", 2)).map(|r| r.top), Some(-50.0));
    // Row 2 hangs above the viewport: the part outside is not hittable.
    assert_eq!(tree.hit(10.0, -10.0), None);
    assert_eq!(tree.hit(10.0, 10.0), Some(Id::new("row", 2)));

    // Content shrank under the offset: it clamps to the new end.
    tree.set_offset(list, 5000.0);
    let frame = tree.layout(tall_list(list, 5, &drawn), view);
    tree.paint(surface.canvas(), frame);
    assert_eq!(tree.offset(list), 190.0);
}

/// 2000 items at a 100 px pitch; `built` records which ones were made.
fn grid(list: Id, built: &RefCell<Vec<usize>>) -> El<'_> {
    El::scroll(list, Axis::Vertical).child(El::virtual_list(
        Axis::Vertical,
        2000,
        90.0,
        10.0,
        move |i| {
            built.borrow_mut().push(i);
            El::row().child(El::paint(|_, _| {}).id(Id::new("cell", i)).width(50.0))
        },
    ))
}

#[test]
fn a_virtual_list_builds_only_what_is_in_view() {
    let list = Id::new("grid", 0);
    let built = RefCell::new(Vec::new());
    let mut tree = Tree::new();
    let view = Rect::from_xywh(0.0, 0.0, 400.0, 800.0);
    let frame = tree.layout(grid(list, &built), view);
    assert_eq!(
        frame.scroll(list).map(|(_, max)| max),
        Some(199_990.0 - 800.0)
    );
    // 800 px of viewport plus 400 px of overscan below it: items 0..12.
    assert_eq!(*built.borrow(), (0..12).collect::<Vec<_>>());
    assert_eq!(
        frame.rect(Id::new("cell", 3)),
        Some(Rect::from_xywh(0.0, 300.0, 50.0, 90.0))
    );

    tree.set_offset(list, 100_000.0);
    built.borrow_mut().clear();
    let frame = tree.layout(grid(list, &built), view);
    // 400 px of overscan each side of 100 000..100 800.
    assert_eq!(*built.borrow(), (996..1012).collect::<Vec<_>>());
    let mut surface = canvas();
    tree.paint(surface.canvas(), frame);
    assert_eq!(tree.rect(Id::new("cell", 1000)).map(|r| r.top), Some(0.0));
    assert_eq!(tree.hit(10.0, 5.0), Some(Id::new("cell", 1000)));
}

/// Layout and paint 20 rows at a 100 px pitch in a 300 px viewport.
fn frame_of(tree: &mut Tree, list: Id) {
    let drawn = RefCell::new(Vec::new());
    let frame = tree.layout(
        tall_list(list, 20, &drawn),
        Rect::from_xywh(0.0, 0.0, 400.0, 300.0),
    );
    tree.paint(canvas().canvas(), frame);
}

fn run(tree: &mut Tree, list: Id, seconds: f32) {
    for _ in 0..(seconds * 120.0) as usize {
        tree.tick(1.0 / 120.0);
        frame_of(tree, list);
    }
}

#[test]
fn a_pan_follows_the_finger_and_rubber_bands_past_an_end() {
    let list = Id::new("list", 0);
    let mut tree = Tree::new();
    frame_of(&mut tree, list);
    assert_eq!(tree.scroll_at(10.0, 150.0, Axis::Vertical), Some(list));
    assert_eq!(tree.scroll_at(10.0, 150.0, Axis::Horizontal), None);

    tree.pan(list, 100.0);
    assert_eq!(tree.offset(list), 100.0);
    tree.pan(list, -150.0);
    assert_eq!(tree.offset(list), -50.0, "in range, one to one");
    tree.pan(list, -50.0);
    let stretched = tree.offset(list);
    assert!(
        (-70.0..-55.0).contains(&stretched),
        "past the top it lags: {stretched}"
    );
    // Held past the end, layout leaves it there.
    frame_of(&mut tree, list);
    assert_eq!(tree.offset(list), stretched);
    assert!(tree.moving(list));

    tree.release(list, 0.0);
    run(&mut tree, list, 1.0);
    assert_eq!(tree.offset(list), 0.0, "springs back to the top");
    assert!(!tree.moving(list));
}

#[test]
fn a_fling_decays_to_a_stop_and_bounces_off_an_end() {
    let list = Id::new("list", 0);
    let mut tree = Tree::new();
    frame_of(&mut tree, list);
    tree.release(list, 1000.0);
    run(&mut tree, list, 3.0);
    // ∫ 1000·e^(−t/0.4) until it drops under 20 px/s: 0.4 × 1000 × 0.98.
    let travel = tree.offset(list);
    assert!((370.0..400.0).contains(&travel), "{travel}");
    assert!(!tree.moving(list));

    tree.set_offset(list, 1600.0);
    frame_of(&mut tree, list);
    tree.release(list, 3000.0);
    let mut furthest = 0.0f32;
    for _ in 0..360 {
        tree.tick(1.0 / 120.0);
        frame_of(&mut tree, list);
        furthest = furthest.max(tree.offset(list));
    }
    assert!(furthest > 1690.0, "overshoots the end: {furthest}");
    assert!(furthest < 1690.0 + 150.0, "but not by a screen: {furthest}");
    assert_eq!(tree.offset(list), 1690.0);
    assert!(!tree.moving(list));
}

#[test]
fn ids_are_stable_and_distinct() {
    assert_eq!(Id::new("row", 3), Id::new("row", 3));
    assert_ne!(Id::new("row", 3), Id::new("row", 4));
    assert_ne!(Id::new("row", 3), Id::new("tab", 3));
}
