//! The Curvature tool (Shift+~).
//!
//! Each click adds a point and the path curves smoothly through all points. Alt-click or
//! double-click a point toggles it between smooth and corner; drag a point to move it; click the
//! first point to close; Backspace/Delete removes the last touched point; Esc/Enter ends the path.
//! Pressing a point of an existing curvature path (or the end of an open one) picks it up again, so it
//! can be edited or continued after Esc or a tool switch.
//! A rubber band shows the curve to the cursor (Enable Rubber Band for Curvature Tool). Each point
//! placed or dragged snaps to Smart Guides ([`DrawSnap`]), as the Pen's anchors do.

use serde_json::{Value, json};
use vectorcraft_doc::{NodeId, NodeKind, Selection};
use vectorcraft_geom::{AnchorKind, Point, SubPath};

use super::{FEEDBACK, catmull_rom};
use crate::guides::{DrawSnap, Leave, Targets};
use crate::{Action, Cursor, Mods, Overlay, PointerEvent, PointerKind, Tool, ToolContext, ToolKey};

/// The points and closed flag a path would be re-fitted from, if it is one the tool made (or any
/// path it would fit identically): the Catmull-Rom of the result must reproduce the path, so a path
/// with hand-set handles is never re-fitted and its handles lost. Shapes that are all corners and
/// closed (rectangles, polygons) are left alone, as are live shapes.
fn points_of(kind: &NodeKind) -> Option<(Vec<(Point, bool)>, bool)> {
    let NodeKind::Path { path, live: None, clipping: false, guide: false, .. } = kind else { return None };
    let [sp] = path.subpaths.as_slice() else { return None };
    let n = sp.anchors.len();
    let pts: Vec<(Point, bool)> = sp
        .anchors
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let end = !sp.closed && (i == 0 || i + 1 == n);
            (a.p, !end && !(a.kind == AnchorKind::Smooth && a.has_in() && a.has_out()))
        })
        .collect();
    if sp.closed && pts.iter().all(|(_, corner)| *corner) {
        return None;
    }
    same_curve(&catmull_rom(&pts, sp.closed), sp).then_some((pts, sp.closed))
}

fn same_curve(a: &SubPath, b: &SubPath) -> bool {
    if a.closed != b.closed || a.anchors.len() != b.anchors.len() {
        return false;
    }
    let extent = b.anchors.iter().flat_map(|k| [k.p.x.abs(), k.p.y.abs()]).fold(1.0, f64::max);
    let tol = 1e-6 * extent;
    a.anchors.iter().zip(&b.anchors).all(|(x, y)| x.p.distance(y.p) <= tol && x.h_in.distance(y.h_in) <= tol && x.h_out.distance(y.h_out) <= tol)
}

#[derive(Default)]
pub struct CurvatureTool {
    pts: Vec<(Point, bool)>,
    closed: bool,
    id: Option<NodeId>,
    /// Point being dragged: (index, began).
    drag: Option<(usize, Point, bool)>,
    /// Last touched point (Backspace removes it).
    current: Option<usize>,
    hover: Option<Point>,
    snap: DrawSnap,
}

impl CurvatureTool {
    /// The path being drawn, if it is still the single selected object and matches our points.
    fn active(&self, cx: &ToolContext) -> Option<NodeId> {
        let id = self.id?;
        if cx.selection.objects != [id] {
            return None;
        }
        let pd = cx.doc.node(id)?.path_data()?;
        (pd.subpaths.len() == 1 && pd.anchor_count() == self.pts.len()).then_some(id)
    }

    fn params(&self) -> Value {
        let pts: Vec<Value> = self.pts.iter().map(|(p, c)| json!({"x": p.x, "y": p.y, "corner": c})).collect();
        let mut v = json!({"points": pts, "closed": self.closed});
        if let Some(id) = self.id {
            v["id"] = json!(id.0);
        }
        v
    }

    fn hit_point(&self, cx: &ToolContext, p: Point) -> Option<usize> {
        let tol = cx.tol(5.0);
        self.pts.iter().position(|(q, _)| q.distance(p) <= tol)
    }

    /// Press on point `i` of the path being drawn (`at` is its index in the document when the points
    /// were reversed to continue from the first end).
    fn press_point(&mut self, cx: &ToolContext, id: NodeId, (i, at): (usize, usize), p: Point, alt: bool, may_close: bool) -> Vec<Action> {
        self.current = Some(i);
        if alt {
            if let Some(pt) = self.pts.get_mut(i) {
                pt.1 = !pt.1;
            }
            return vec![Action::Exec("path.curvature".into(), self.params())];
        }
        if may_close && i == 0 && !self.closed && self.pts.len() >= 3 {
            self.closed = true;
            return vec![Action::Exec("path.curvature".into(), self.params())];
        }
        // The point dragged and the curve through it move: the rest pull.
        let sel = Selection { objects: vec![id], anchors: [(id, [(0, at)].into())].into(), ..Selection::default() };
        self.snap.hold(cx, || Targets::for_anchor_drag(cx.doc, &sel));
        self.drag = Some((i, p, false));
        vec![]
    }

    /// Pick up the curvature path with a point under `p`: its points become ours, and the path the
    /// selection. Returns the actions to run and the point pressed (as `press_point` takes it).
    fn adopt(&mut self, cx: &ToolContext, p: Point) -> Option<(NodeId, (usize, usize), Vec<Action>)> {
        let (id, si, at) = super::anchor_in(cx, super::candidate_paths(cx), p, cx.tol(5.0))?;
        let (mut pts, closed) = points_of(&cx.doc.node(id)?.kind)?;
        if si != 0 {
            return None;
        }
        let mut i = at;
        // Pressing the first end of an open path continues from it: the points run the other way.
        if !closed && at == 0 {
            pts.reverse();
            i = pts.len().checked_sub(1)?;
        }
        (self.pts, self.closed, self.id) = (pts, closed, Some(id));
        let mut out = vec![];
        if cx.selection.objects != [id] {
            out.push(Action::Exec("select.set".into(), json!({"ids": [id.0]})));
        }
        Some((id, (i, at), out))
    }

    fn reset(&mut self) {
        self.pts.clear();
        self.closed = false;
        self.id = None;
        self.drag = None;
        self.current = None;
        self.snap.clear();
    }

    /// The segment to the next point leaves the last one.
    fn leave(&self, cx: &ToolContext) -> Option<Leave> {
        self.pts.last().map(|(p, _)| Leave::segment(cx, *p, false))
    }
}

impl Tool for CurvatureTool {
    fn id(&self) -> &'static str {
        "curvature"
    }
    fn busy(&self) -> bool {
        self.drag.is_some()
    }
    fn pointer(&mut self, cx: &ToolContext, ev: &PointerEvent) -> Vec<Action> {
        let p = ev.pos;
        let active = self.active(cx);
        if active.is_none() && self.id.is_some() && matches!(ev.kind, PointerKind::Down) {
            self.reset();
        }
        match ev.kind {
            PointerKind::Move => {
                let from = active.and_then(|_| self.leave(cx));
                self.hover = Some(self.snap.hover(cx, p, active.as_slice(), from.as_ref()));
                vec![]
            }
            PointerKind::Down => {
                if let Some(id) = active {
                    if let Some(i) = self.hit_point(cx, p) {
                        return self.press_point(cx, id, (i, i), p, ev.mods.alt, true);
                    }
                    if !self.closed {
                        let from = self.leave(cx);
                        let p = self.snap.press(cx, p, &[id], from.as_ref());
                        self.pts.push((p, ev.mods.alt));
                        self.current = Some(self.pts.len() - 1);
                        return vec![Action::Exec("path.curvature".into(), self.params())];
                    }
                }
                // Pick up an existing curvature path, else start a new path.
                if let Some((id, i, mut out)) = self.adopt(cx, p) {
                    out.extend(self.press_point(cx, id, i, p, ev.mods.alt, false));
                    return out;
                }
                self.reset();
                let p = self.snap.press(cx, p, &[], None);
                self.pts.push((p, false));
                self.current = Some(0);
                vec![Action::Exec("path.curvature".into(), self.params()), Action::Notify("created".into())]
            }
            PointerKind::Drag => {
                let Some((i, start, began)) = self.drag else { return vec![] };
                let mut out = vec![];
                if !began {
                    if p.distance(start) < cx.tol(3.0) {
                        return out;
                    }
                    out.push(Action::Begin("Curvature".into()));
                    self.drag = Some((i, start, true));
                }
                let p = self.snap.drag(cx, p, None);
                if let Some(pt) = self.pts.get_mut(i) {
                    pt.0 = p;
                }
                out.push(Action::Preview("path.curvature".into(), self.params()));
                out
            }
            PointerKind::Up => {
                self.snap.clear();
                match self.drag.take() {
                    Some((_, _, true)) => vec![Action::Commit],
                    _ => vec![],
                }
            }
            PointerKind::DoubleClick => {
                if active.is_none() {
                    return vec![];
                }
                let Some(i) = self.hit_point(cx, p) else { return vec![] };
                self.pts[i].1 = !self.pts[i].1;
                vec![Action::Exec("path.curvature".into(), self.params())]
            }
        }
    }
    fn notify(&mut self, cx: &ToolContext, what: &str) {
        if what == "created" && cx.selection.objects.len() == 1 {
            self.id = Some(cx.selection.objects[0]);
        }
    }
    fn key(&mut self, cx: &ToolContext, key: ToolKey, _m: Mods) -> Vec<Action> {
        match key {
            ToolKey::Escape | ToolKey::Enter => {
                let dragging = self.drag.is_some_and(|d| d.2);
                self.reset();
                if dragging { vec![Action::Cancel] } else { vec![] }
            }
            ToolKey::Backspace | ToolKey::Delete if self.active(cx).is_some() && self.pts.len() > 1 && self.drag.is_none() => {
                let i = self.current.unwrap_or(self.pts.len() - 1).min(self.pts.len() - 1);
                self.pts.remove(i);
                if self.pts.len() < 3 {
                    self.closed = false;
                }
                self.current = None;
                vec![Action::Exec("path.curvature".into(), self.params())]
            }
            _ => vec![],
        }
    }
    fn deactivate(&mut self, _cx: &ToolContext) -> Vec<Action> {
        let dragging = self.drag.is_some_and(|d| d.2);
        self.reset();
        if dragging { vec![Action::Commit] } else { vec![] }
    }
    fn overlays(&self, cx: &ToolContext) -> Vec<Overlay> {
        let mut o = self.snap.guides().to_vec();
        if self.active(cx).is_none() {
            // A finished curvature path shows its points, which a press picks up.
            if let [id] = cx.selection.objects[..]
                && let Some((pts, _)) = cx.doc.node(id).and_then(|n| points_of(&n.kind))
            {
                o.extend(pts.iter().map(|(p, _)| Overlay::Anchor { p: *p, color: FEEDBACK, filled: false, size: 6.0 }));
            }
            return o;
        }
        if cx.curvature_rubber_band
            && !self.closed
            && self.drag.is_none()
            && let Some(h) = self.hover
        {
            let mut pts = self.pts.clone();
            pts.push((h, false));
            let sp = catmull_rom(&pts, false);
            let mut bp = vectorcraft_geom::BezPath::new();
            sp.to_bezpath_into(&mut bp);
            o.push(Overlay::Path { path: bp, color: FEEDBACK, width: 1.0, dashed: false });
        }
        for (i, (p, _)) in self.pts.iter().enumerate() {
            o.push(Overlay::Anchor { p: *p, color: FEEDBACK, filled: Some(i) == self.current, size: 6.0 });
        }
        o
    }
    fn cursor(&self, _cx: &ToolContext, _p: Point, _m: Mods) -> Cursor {
        Cursor::Pen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use vectorcraft_doc::Selection;

    #[test]
    fn first_click_creates_then_notify_tracks_path() {
        let (d, id) = doc_with_rect();
        let s = Selection::default();
        let p = paint();
        let cx = cx(&d, &s, &p);
        let mut t = CurvatureTool::default();
        let a = t.pointer(&cx, &PointerEvent::new(PointerKind::Down, 10.0, 10.0));
        assert!(matches!(&a[0], Action::Exec(c, v) if c == "path.curvature" && v.get("id").is_none() && v["points"].as_array().unwrap().len() == 1));
        assert_eq!(a[1], Action::Notify("created".into()));
        // Pretend the engine created `id` with one anchor: it is a rect (4 anchors) so it doesn't match.
        let mut s2 = Selection::default();
        s2.set([id]);
        let cx2 = crate::testutil::cx(&d, &s2, &p);
        t.notify(&cx2, "created");
        assert_eq!(t.id, Some(id));
        assert!(t.active(&cx2).is_none());
        // With four points it matches, so a click appends a fifth.
        t.pts = vec![(Point::new(100.0, 100.0), false); 4];
        let a = t.pointer(&cx2, &PointerEvent::new(PointerKind::Down, 400.0, 400.0));
        assert!(matches!(&a[0], Action::Exec(_, v) if v["id"] == id.0 && v["points"].as_array().unwrap().len() == 5));
    }

    /// Points snap to Smart Guides (#506): the first lands on another object's anchor and says so.
    #[test]
    fn points_snap_to_smart_guides() {
        let (d, _) = doc_with_rect();
        let s = Selection::default();
        let p = paint();
        let cx = cx(&d, &s, &p);
        let mut t = CurvatureTool::default();
        let a = t.pointer(&cx, &PointerEvent::new(PointerKind::Down, 202.0, 99.0));
        assert!(matches!(&a[0], Action::Exec(_, v) if v["points"][0]["x"] == 200.0 && v["points"][0]["y"] == 100.0), "{a:?}");
        assert!(t.overlays(&cx).iter().any(|o| matches!(o, Overlay::Label { text, .. } if text == "anchor")));
        t.pointer(&cx, &PointerEvent::new(PointerKind::Up, 202.0, 99.0));
        assert!(t.overlays(&cx).is_empty());
    }

    #[test]
    fn points_of_round_trips_catmull_rom() {
        use vectorcraft_geom::{Affine, FillRule, PathData, Vec2};
        let pts = [(Point::new(0.0, 0.0), false), (Point::new(50.0, 50.0), false), (Point::new(80.0, 10.0), true), (Point::new(120.0, 40.0), false)];
        let mut pd = PathData::single(catmull_rom(&pts, false));
        let kind = |pd: &PathData| NodeKind::Path {
            path: pd.clone(),
            rule: FillRule::NonZero,
            live: None,
            clipping: false,
            guide: false,
        };
        let (got, closed) = points_of(&kind(&pd)).unwrap();
        assert!(!closed);
        assert_eq!(got, pts.to_vec());
        // Catmull-Rom is affine-invariant: a moved and scaled path still round-trips.
        pd.transform(Affine::translate((30.0, -5.0)) * Affine::scale(2.0));
        assert_eq!(points_of(&kind(&pd)).unwrap().0.len(), 4);
        // Hand-set handles are not ours to re-fit.
        pd.subpaths[0].anchors[1].h_out += Vec2::new(5.0, 0.0);
        assert!(points_of(&kind(&pd)).is_none());
    }
}
