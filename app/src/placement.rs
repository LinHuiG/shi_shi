//! Where the tray panel opens: against the taskbar edge next to the click
//! on the tray icon, inside the work area of that monitor. Physical pixels
//! throughout; Windows supplies the rectangles and the monitor's scale.
#![cfg_attr(not(windows), allow(dead_code))]

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Top,
    Right,
    Bottom,
}

/// The taskbar edge: the side the work area gives up on its monitor. An
/// auto-hidden taskbar gives up nothing; then the edge nearest the icon.
pub fn edge(monitor: Rect, work: Rect, anchor: Rect) -> Edge {
    let gaps = [
        (work.left - monitor.left, Edge::Left),
        (work.top - monitor.top, Edge::Top),
        (monitor.right - work.right, Edge::Right),
        (monitor.bottom - work.bottom, Edge::Bottom),
    ];
    if let Some(&(_, edge)) = gaps.iter().filter(|g| g.0 > 0).max_by_key(|g| g.0) {
        return edge;
    }
    let (x, y) = ((anchor.left + anchor.right) / 2, (anchor.top + anchor.bottom) / 2);
    let distances = [
        (x - monitor.left, Edge::Left),
        (y - monitor.top, Edge::Top),
        (monitor.right - x, Edge::Right),
        (monitor.bottom - y, Edge::Bottom),
    ];
    distances.iter().min_by_key(|d| d.0).map_or(Edge::Bottom, |d| d.1)
}

/// The panel's rectangle for a logical `size` at `scale`: against the
/// taskbar edge, centred on the icon along it and kept inside the work area
/// with a small gap, shrunk when the work area is smaller than the panel.
pub fn place(monitor: Rect, work: Rect, anchor: Rect, size: (f64, f64), scale: f64) -> Rect {
    let gap = (12. * scale).round() as i32;
    let width = ((size.0 * scale).round() as i32).min(work.right - work.left - 2 * gap).max(1);
    let height = ((size.1 * scale).round() as i32).min(work.bottom - work.top - 2 * gap).max(1);
    // Along an edge: centred on the icon, clamped into the work area.
    let along = |centre: i32, low: i32, high: i32, length: i32| {
        (centre - length / 2).clamp(low + gap, (high - gap - length).max(low + gap))
    };
    let (cx, cy) = ((anchor.left + anchor.right) / 2, (anchor.top + anchor.bottom) / 2);
    let (left, top) = match edge(monitor, work, anchor) {
        Edge::Bottom => (along(cx, work.left, work.right, width), work.bottom - gap - height),
        Edge::Top => (along(cx, work.left, work.right, width), work.top + gap),
        Edge::Left => (work.left + gap, along(cy, work.top, work.bottom, height)),
        Edge::Right => (work.right - gap - width, along(cy, work.top, work.bottom, height)),
    };
    Rect { left, top, right: left + width, bottom: top + height }
}

#[cfg(test)]
mod tests;
