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

/// Focus moving between two trees glides from where the plate left, whichever tree paints
/// first, and the plate that left hides as the other takes over.
#[test]
fn the_plate_glides_between_trees() {
    for upper_first in [true, false] {
        let mut surface = canvas();
        let (mut upper, mut lower) = (Tree::new(), Tree::new());
        let lower_rect = Rect::from_xywh(400.0, 500.0, 100.0, 60.0);
        let frame = |upper: &mut Tree, lower: &mut Tree, s: &mut skia_safe::Surface| {
            super::begin_frame();
            let paint = |t: &mut Tree, el: El<'_>, s: &mut skia_safe::Surface| {
                let root = El::column().child(el);
                let f = t.layout(root, Rect::from_xywh(0.0, 0.0, 1280.0, 800.0));
                t.paint_focus(s.canvas(), f, 1.0, 1.0 / 120.0, false);
            };
            if upper_first {
                paint(upper, target("up", 0, card(0)), s);
                paint(lower, target("low", 0, lower_rect), s);
            } else {
                paint(lower, target("low", 0, lower_rect), s);
                paint(upper, target("up", 0, card(0)), s);
            }
        };
        upper.set_focus(Some(Id::new("up", 0)));
        lower.set_focus(None);
        for _ in 0..40 {
            frame(&mut upper, &mut lower, &mut surface);
        }
        assert_eq!(upper.plate_rect().map(|p| p.0.left), Some(0.0));
        upper.set_focus(None);
        lower.set_focus(Some(Id::new("low", 0)));
        frame(&mut upper, &mut lower, &mut surface);
        frame(&mut upper, &mut lower, &mut surface);
        let left = lower.plate_rect().map(|p| p.0.left);
        assert!(
            left.is_some_and(|l| l < 100.0),
            "starts where the other plate left, not on its target: {left:?} ({upper_first})"
        );
        // Painting first, the plate that left learns of it a frame later.
        frame(&mut upper, &mut lower, &mut surface);
        assert_eq!(upper.plate_rect(), None, "the plate that left hides");
        for _ in 0..240 {
            frame(&mut upper, &mut lower, &mut surface);
        }
        assert_eq!(
            lower.plate_rect().map(|p| p.0.left),
            Some(400.0),
            "and lands"
        );
    }
}

fn target<'a>(name: &str, i: usize, r: Rect) -> El<'a> {
    El::paint(|_, _| {})
        .id(Id::new(name, i))
        .focusable(8.0)
        .place(r)
}

fn card(i: usize) -> Rect {
    Rect::from_xywh(i as f32 * 120.0, 0.0, 100.0, 60.0)
}

/// A row of three over a lone target under the third, painted once so focus has rects.
fn shelf(tree: &mut Tree, surface: &mut skia_safe::Surface) {
    let row = El::column()
        .id(Id::new("row", 0))
        .group(Group::Row)
        .children((0..3).map(|i| target("card", i, card(i))));
    let root = El::column()
        .child(row.place(Rect::from_xywh(0.0, 0.0, 340.0, 60.0)))
        .child(target(
            "below",
            0,
            Rect::from_xywh(240.0, 120.0, 100.0, 60.0),
        ))
        .child(target("far", 0, Rect::from_xywh(0.0, 400.0, 100.0, 60.0)));
    let frame = tree.layout(root, Rect::from_xywh(0.0, 0.0, 600.0, 600.0));
    tree.paint(surface.canvas(), frame);
}

#[test]
fn focus_moves_by_geometry_overlap_first() {
    use pf_client_core::menu_nav::MenuDir::*;
    let mut surface = canvas();
    let mut tree = Tree::new();
    shelf(&mut tree, &mut surface);
    tree.set_focus(Some(Id::new("card", 0)));
    assert_eq!(tree.move_focus(Right), Some(Id::new("card", 1)));
    assert_eq!(tree.move_focus(Left), Some(Id::new("card", 0)));
    assert_eq!(tree.move_focus(Left), None, "nothing is left of the first");
    assert_eq!(tree.focus(), Some(Id::new("card", 0)));
    // "below" is nearer, but only "far" shares the first card's columns.
    assert_eq!(tree.move_focus(Down), Some(Id::new("far", 0)));
}

#[test]
fn a_group_hands_focus_back_to_the_child_it_left() {
    use pf_client_core::menu_nav::MenuDir::*;
    let mut surface = canvas();
    let mut tree = Tree::new();
    shelf(&mut tree, &mut surface);
    tree.set_focus(Some(Id::new("card", 0)));
    tree.set_focus(Some(Id::new("below", 0)));
    // Straight up is card 2; the row remembers card 0.
    assert_eq!(tree.move_focus(Up), Some(Id::new("card", 0)));
    tree.set_focus(Some(Id::new("card", 2)));
    assert_eq!(tree.move_focus(Down), Some(Id::new("below", 0)));
}

#[test]
fn the_plate_travels_lands_and_settles() {
    use pf_client_core::menu_nav::MenuDir::*;
    let mut surface = canvas();
    let mut tree = Tree::new();
    let draw = |tree: &mut Tree, surface: &mut skia_safe::Surface| {
        let root = El::column().children((0..3).map(|i| target("card", i, card(i))));
        let frame = tree.layout(root, Rect::from_xywh(0.0, 0.0, 600.0, 600.0));
        tree.paint_focus(surface.canvas(), frame, 1.0, 1.0 / 120.0, false);
    };
    tree.set_focus(Some(Id::new("card", 0)));
    draw(&mut tree, &mut surface);
    assert_eq!(
        tree.plate_rect(),
        None,
        "waits a frame for a plate leaving another tree"
    );
    draw(&mut tree, &mut surface);
    assert_eq!(
        tree.plate_rect().map(|p| p.0.left),
        Some(0.0),
        "none left, so it starts on its target"
    );
    tree.move_focus(Right);
    draw(&mut tree, &mut surface);
    let left = tree.plate_rect().unwrap().0.left;
    assert!(
        left > 0.0 && left < 120.0,
        "moves the frame focus does: {left}"
    );
    let mut peak = 0.0f32;
    for _ in 0..240 {
        draw(&mut tree, &mut surface);
        peak = peak.max(tree.plate_rect().unwrap().0.left);
    }
    assert!(peak > 120.0, "overshoots a little: {peak}");
    assert_eq!(tree.plate_rect().unwrap().0.left, 120.0);
    assert!(!tree.plate_busy(), "the landing wobble has settled");

    crate::theme::set_reduce_motion(true);
    tree.move_focus(Right);
    draw(&mut tree, &mut surface);
    crate::theme::set_reduce_motion(false);
    assert_eq!(
        tree.plate_rect().unwrap().0.left,
        240.0,
        "jumps under Reduce Motion"
    );
    assert!(tree.plate_busy(), "and fades in");
}

/// Paint the cards in `shown` of a row of three, the plate stepping a 60 Hz frame.
fn cards(tree: &mut Tree, shown: &[usize]) {
    let root = El::column().children(shown.iter().map(|&i| target("card", i, card(i))));
    let frame = tree.layout(root, Rect::from_xywh(0.0, 0.0, 600.0, 600.0));
    tree.paint_focus(canvas().canvas(), frame, 1.0, 1.0 / 60.0, false);
}

#[test]
fn a_vanished_target_hands_focus_to_the_nearest_and_the_plate_glides() {
    let mut tree = Tree::new();
    tree.set_focus(Some(Id::new("card", 2)));
    // The first frame waits for a plate leaving another tree; none does.
    cards(&mut tree, &[0, 1, 2]);
    cards(&mut tree, &[0, 1, 2]);
    assert_eq!(tree.plate_rect().map(|p| p.0.left), Some(240.0));

    // Card 2 goes and nobody names it again: the tree reseats on its nearest neighbour.
    cards(&mut tree, &[0, 1]);
    assert_eq!(tree.focus(), Some(Id::new("card", 1)));
    let left = tree.plate_rect().unwrap().0.left;
    assert!(
        left > 120.0 && left < 240.0,
        "glides from where it was: {left}"
    );
    for _ in 0..120 {
        cards(&mut tree, &[0, 1]);
    }
    assert_eq!(tree.plate_rect().map(|p| p.0.left), Some(120.0));
    assert!(!tree.plate_busy());
}

#[test]
fn a_held_focus_the_frame_lacks_fades_the_plate_out() {
    let mut tree = Tree::new();
    tree.set_focus(Some(Id::new("card", 2)));
    cards(&mut tree, &[0, 1, 2]);
    for _ in 0..60 {
        // The caller insists on card 2: the tree keeps it and the plate fades.
        tree.set_focus(Some(Id::new("card", 2)));
        cards(&mut tree, &[0, 1]);
    }
    assert_eq!(tree.focus(), Some(Id::new("card", 2)));
    assert!(tree.plate_rect().is_none(), "faded out");
    assert!(!tree.plate_busy(), "and the frames stop");

    tree.set_focus(Some(Id::new("card", 2)));
    cards(&mut tree, &[0, 1, 2]);
    assert!(
        tree.plate_rect().is_none(),
        "a frame's wait for another tree's plate"
    );
    cards(&mut tree, &[0, 1, 2]);
    assert_eq!(
        tree.plate_rect().map(|p| p.0.left),
        Some(240.0),
        "back on its target, not gliding in from where it vanished"
    );
    assert!(tree.plate_busy(), "fading in");
}

#[test]
fn a_tree_with_no_targets_counts_none_and_focus_waits() {
    let mut tree = Tree::new();
    tree.set_focus(Some(Id::new("card", 2)));
    assert_eq!(census(|| cards(&mut tree, &[0, 1, 2])), 3);

    // Nothing focusable: the census says so, focus waits and the plate fades.
    let empty = |tree: &mut Tree| {
        let frame = tree.layout(El::column(), Rect::from_xywh(0.0, 0.0, 600.0, 600.0));
        tree.paint_focus(canvas().canvas(), frame, 1.0, 1.0 / 60.0, false);
    };
    assert_eq!(census(|| empty(&mut tree)), 0);
    for _ in 0..60 {
        empty(&mut tree);
    }
    assert!(tree.plate_rect().is_none() && !tree.plate_busy());
    assert_eq!(tree.focus(), Some(Id::new("card", 2)));

    // Targets back: focus lands on the one nearest where it stood.
    cards(&mut tree, &[0, 1]);
    assert_eq!(tree.focus(), Some(Id::new("card", 1)));

    // Nested counts add up; a claim outside a census goes nowhere.
    assert_eq!(census(|| claim(2)), 2);
    let outer = census(|| {
        census(|| cards(&mut tree, &[0, 1]));
        claim(1);
    });
    assert_eq!(outer, 3);
    claim(5);
}

/// Up from the top row answers nothing and leaves focus put: a screen reads that as its
/// boundary, and the shell hands Up past a root's boundary to the tab strip.
#[test]
fn up_from_the_top_row_leaves_focus_put() {
    use pf_client_core::menu_nav::MenuDir::*;
    let mut surface = canvas();
    let mut tree = Tree::new();
    shelf(&mut tree, &mut surface);
    tree.set_focus(Some(Id::new("card", 1)));
    assert_eq!(tree.move_focus(Up), None);
    assert_eq!(tree.focus(), Some(Id::new("card", 1)));
    shelf(&mut tree, &mut surface);
    assert_eq!(tree.move_focus(Up), None, "still, after a paint");
}
