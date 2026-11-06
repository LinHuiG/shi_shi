use super::*;

const MONITOR: Rect = Rect { left: 0, top: 0, right: 1920, bottom: 1080 };

fn at(x: i32, y: i32) -> Rect {
    Rect { left: x, top: y, right: x + 1, bottom: y + 1 }
}

#[test]
fn a_bottom_taskbar_opens_the_panel_above_it_next_to_the_click() {
    let work = Rect { bottom: 1032, ..MONITOR };
    let r = place(MONITOR, work, at(1700, 1050), (360., 520.), 1.);
    assert_eq!(r, Rect { left: 1520, top: 500, right: 1880, bottom: 1020 });
    // Near the corner it stays inside the work area.
    let r = place(MONITOR, work, at(1910, 1050), (360., 520.), 1.);
    assert_eq!((r.right, r.bottom), (1908, 1020));
}

#[test]
fn other_edges_and_scales_follow_the_taskbar() {
    let left = Rect { left: 48, ..MONITOR };
    let r = place(MONITOR, left, at(20, 900), (360., 520.), 1.5);
    assert_eq!((r.left, r.right - r.left, r.bottom), (66, 540, 1062));
    let top = Rect { top: 48, ..MONITOR };
    assert_eq!(place(MONITOR, top, at(960, 20), (360., 520.), 1.).top, 60);
    let right = Rect { right: 1872, ..MONITOR };
    assert_eq!(place(MONITOR, right, at(1900, 500), (360., 520.), 1.).right, 1860);
}

#[test]
fn an_auto_hidden_taskbar_uses_the_edge_nearest_the_click_and_a_small_screen_shrinks() {
    assert_eq!(edge(MONITOR, MONITOR, at(1900, 600)), Edge::Right);
    assert_eq!(edge(MONITOR, MONITOR, at(900, 1075)), Edge::Bottom);
    let small = Rect { left: 0, top: 0, right: 800, bottom: 500 };
    let r = place(small, Rect { bottom: 460, ..small }, at(700, 480), (360., 520.), 1.);
    assert_eq!((r.top, r.bottom - r.top), (12, 436));
}
