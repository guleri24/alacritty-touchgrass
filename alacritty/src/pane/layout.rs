use std::fmt;

/// Unique identifier for a pane.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(u64);

impl PaneId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

/// Direction of a pane split.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// Binary tree layout for pane splits.
#[derive(Clone, Debug)]
pub enum PaneLayout {
    Leaf(PaneId),
    Split { direction: SplitDirection, ratio: f32, first: Box<PaneLayout>, second: Box<PaneLayout> },
}

/// Temporary placeholder used during tree mutations; never exposed outside internals.
impl Default for PaneLayout {
    fn default() -> Self {
        Self::Leaf(PaneId::new(u64::MAX))
    }
}

/// Bounds of a pane within the window, in pixels.
#[derive(Copy, Clone, Debug, Default)]
pub struct PaneBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl PaneBounds {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }

    pub fn pad(&self, padding_x: f32, padding_y: f32) -> Self {
        Self {
            x: self.x + padding_x,
            y: self.y + padding_y,
            width: (self.width - 2. * padding_x).max(0.),
            height: (self.height - 2. * padding_y).max(0.),
        }
    }
}

impl PaneLayout {
    pub fn new(pane_id: PaneId) -> Self {
        Self::Leaf(pane_id)
    }

    /// Split an existing leaf pane, creating a new leaf alongside it.
    pub fn split(&mut self, target: PaneId, new_pane: PaneId, direction: SplitDirection) -> bool {
        match self {
            PaneLayout::Leaf(id) if *id == target => {
                let old = PaneLayout::Leaf(*id);
                let new = PaneLayout::Leaf(new_pane);
                *self = PaneLayout::Split {
                    direction,
                    ratio: 0.5,
                    first: Box::new(old),
                    second: Box::new(new),
                };
                true
            },
            PaneLayout::Split { first, second, .. } => {
                first.split(target, new_pane, direction)
                    || second.split(target, new_pane, direction)
            },
            _ => false,
        }
    }

    /// Close a pane by its id, collapsing the tree.
    pub fn close(&mut self, target: PaneId) -> bool {
        match self {
            PaneLayout::Leaf(id) if *id == target => true,
            PaneLayout::Split { first, second, .. } => {
                // Direct child leaf — remove and collapse this split without cloning.
                if matches!(&**first, PaneLayout::Leaf(id) if *id == target) {
                    let taken = std::mem::take(second);
                    *self = *taken;
                    return true;
                }
                if matches!(&**second, PaneLayout::Leaf(id) if *id == target) {
                    let taken = std::mem::take(first);
                    *self = *taken;
                    return true;
                }
                // Target is deeper in the tree — recurse without collapsing parent.
                first.close(target) || second.close(target)
            },
            _ => false,
        }
    }

    /// Navigate from the current pane in a direction.
    pub fn navigate(&self, current: PaneId, direction: SplitDirection) -> Option<PaneId> {
        match self {
            PaneLayout::Leaf(id) => Some(*id),
            PaneLayout::Split { direction: dir, first, second, .. } => {
                let in_first = contains_pane(first, current);
                let in_second = contains_pane(second, current);

                match (in_first, in_second, dir == &direction) {
                    // The split direction matches the navigation direction, so we cross between sides.
                    (true, false, true) => {
                        // Navigate from first to second: find leftmost/topmost in second.
                        first_leaf(second)
                    },
                    (false, true, true) => {
                        // Navigate from second to first: find rightmost/bottommost in first.
                        last_leaf(first)
                    },
                    // The split direction doesn't match, or we need to go deeper.
                    _ => {
                        if in_first {
                            first.navigate(current, direction)
                        } else if in_second {
                            second.navigate(current, direction)
                        } else {
                            None
                        }
                    },
                }
            },
        }
    }

    /// Resize a split by adjusting the ratio.
    ///
    /// Recurses into child splits to find the innermost matching split;
    /// only adjusts the current split if no deeper match exists.
    /// `min_ratio`/`max_ratio` clamp the ratio for the split that is actually adjusted.
    pub fn resize(
        &mut self,
        target: PaneId,
        direction: SplitDirection,
        amount: f32,
        min_ratio: f32,
        max_ratio: f32,
    ) -> bool {
        match self {
            PaneLayout::Split { direction: dir, ratio, first, second, .. } => {
                let in_first = contains_pane(first, target);
                let in_second = contains_pane(second, target);

                if *dir == direction {
                    if in_first {
                        // Try a more specific (inner) split first.
                        if first.resize(target, direction, amount, min_ratio, max_ratio) {
                            return true;
                        }
                        *ratio = (*ratio + amount).clamp(min_ratio, max_ratio);
                        return true;
                    }
                    if in_second {
                        if second.resize(target, direction, amount, min_ratio, max_ratio) {
                            return true;
                        }
                        *ratio = (*ratio - amount).clamp(min_ratio, max_ratio);
                        return true;
                    }
                }

                // Direction doesn't match — recurse into the child containing the target.
                in_first && first.resize(target, direction, amount, min_ratio, max_ratio)
                    || in_second && second.resize(target, direction, amount, min_ratio, max_ratio)
            },
            _ => false,
        }
    }

    /// Resize by dragging a split border to a new pixel position.
    ///
    /// `subtree_bounds` are the bounds of the current subtree (the split node itself),
    /// computed from its parent via `split_bounds`. The root call passes `window_bounds`.
    /// `min_pane_dim` is the pixel width (for vertical) or height (for horizontal) that each
    /// side must have at minimum.
    pub fn resize_drag(
        &mut self,
        target: PaneId,
        direction: SplitDirection,
        new_pixel_pos: f32,
        subtree_bounds: &PaneBounds,
        border_width: f32,
        min_pane_dim: f32,
    ) -> bool {
        match self {
            PaneLayout::Split { direction: dir, ratio, first, second, .. } => {
                let (first_bounds, second_bounds) =
                    split_bounds(*subtree_bounds, *dir, *ratio, border_width);

                let in_first = contains_pane(first, target);
                let in_second = contains_pane(second, target);

                // Try deeper splits first (innermost match wins).
                if in_first
                    && first.resize_drag(
                        target,
                        direction,
                        new_pixel_pos,
                        &first_bounds,
                        border_width,
                        min_pane_dim,
                    )
                {
                    return true;
                }
                if in_second
                    && second.resize_drag(
                        target,
                        direction,
                        new_pixel_pos,
                        &second_bounds,
                        border_width,
                        min_pane_dim,
                    )
                {
                    return true;
                }

                // Only adjust this split if the direction matches and target is a child.
                if *dir == direction && (in_first || in_second) {
                    let total = match direction {
                        SplitDirection::Horizontal => subtree_bounds.height - border_width,
                        SplitDirection::Vertical => subtree_bounds.width - border_width,
                    };

                    if total <= 0. {
                        return false;
                    }

                    let offset = match direction {
                        SplitDirection::Horizontal => new_pixel_pos - subtree_bounds.y,
                        SplitDirection::Vertical => new_pixel_pos - subtree_bounds.x,
                    };

                    let min_ratio = (min_pane_dim / total).clamp(0.05, 0.5);
                    let max_ratio = 1.0 - min_ratio;
                    let new_ratio = (offset / total).clamp(min_ratio, max_ratio);
                    *ratio = new_ratio;
                    return true;
                }

                false
            },
            _ => false,
        }
    }

    /// Collect all pane IDs in depth-first order.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        self.collect_ids(&mut ids);
        ids
    }

    fn collect_ids(&self, ids: &mut Vec<PaneId>) {
        match self {
            PaneLayout::Leaf(id) => ids.push(*id),
            PaneLayout::Split { first, second, .. } => {
                first.collect_ids(ids);
                second.collect_ids(ids);
            },
        }
    }

    /// Count the total panes.
    pub fn count(&self) -> usize {
        match self {
            PaneLayout::Leaf(_) => 1,
            PaneLayout::Split { first, second, .. } => first.count() + second.count(),
        }
    }

    /// Get the pane ID at a specific pixel position (for hit-testing).
    pub fn pane_at(
        &self,
        px: f32,
        py: f32,
        bounds: &PaneBounds,
        border_width: f32,
    ) -> Option<PaneId> {
        match self {
            PaneLayout::Leaf(id) => {
                if bounds.contains(px, py) {
                    Some(*id)
                } else {
                    None
                }
            },
            PaneLayout::Split { direction, ratio, first, second, .. } => {
                let (first_bounds, second_bounds) =
                    split_bounds(*bounds, *direction, *ratio, border_width);
                first
                    .pane_at(px, py, &first_bounds, border_width)
                    .or_else(|| second.pane_at(px, py, &second_bounds, border_width))
            },
        }
    }

    /// Find all draggable borders at a pixel position (for corner detection).
    pub fn corners_at(
        &self,
        px: f32,
        py: f32,
        bounds: &PaneBounds,
        border_width: f32,
    ) -> Vec<(PaneId, SplitDirection)> {
        match self {
            PaneLayout::Leaf(_) => Vec::new(),
            PaneLayout::Split { direction, ratio, first, second, .. } => {
                let (first_bounds, second_bounds) =
                    split_bounds(*bounds, *direction, *ratio, border_width);
                let mut result = Vec::new();

                // Segment-based separator check with border_width tolerance on the
                // perpendicular axis to account for the overdraw zone at corners.
                let separator = match direction {
                    SplitDirection::Vertical => {
                        let sep_x = first_bounds.x + first_bounds.width;
                        (px - sep_x).abs() < border_width
                            && py >= bounds.y - border_width
                            && py < bounds.y + bounds.height + border_width
                    },
                    SplitDirection::Horizontal => {
                        let sep_y = first_bounds.y + first_bounds.height;
                        (py - sep_y).abs() < border_width
                            && px >= bounds.x - border_width
                            && px < bounds.x + bounds.width + border_width
                    },
                };

                if separator {
                    if let Some(id) = first_leaf(first) {
                        result.push((id, *direction));
                    }
                }

                // When the point is on this node's separator it sits between both children,
                // so we must recurse into both to pick up perpendicular borders at the
                // corner. Otherwise only recurse into the child that contains the point.
                if first_bounds.contains(px, py) || separator {
                    result.extend(first.corners_at(px, py, &first_bounds, border_width));
                }
                if second_bounds.contains(px, py) || separator {
                    result.extend(second.corners_at(px, py, &second_bounds, border_width));
                }

                result
            },
        }
    }

    /// Find a draggable border at a pixel position.
    pub fn border_at(
        &self,
        px: f32,
        py: f32,
        bounds: &PaneBounds,
        border_width: f32,
    ) -> Option<(PaneId, SplitDirection)> {
        match self {
            PaneLayout::Leaf(_) => None,
            PaneLayout::Split { direction, ratio, first, second, .. } => {
                let (first_bounds, second_bounds) =
                    split_bounds(*bounds, *direction, *ratio, border_width);
                let separator = match direction {
                    SplitDirection::Vertical => {
                        let sep_x = first_bounds.x + first_bounds.width;
                        (px - sep_x).abs() < border_width
                    },
                    SplitDirection::Horizontal => {
                        let sep_y = first_bounds.y + first_bounds.height;
                        (py - sep_y).abs() < border_width
                    },
                };

                if separator {
                    // Return first pane ID in the focused side as the "anchor" for resize.
                    first_leaf(first).map(|id| (id, *direction))
                } else {
                    first
                        .border_at(px, py, &first_bounds, border_width)
                        .or_else(|| second.border_at(px, py, &second_bounds, border_width))
                }
            },
        }
    }

    /// Compute pixel bounds for all panes.
    pub fn compute_bounds(
        &self,
        window_bounds: PaneBounds,
        border_width: f32,
    ) -> Vec<(PaneId, PaneBounds)> {
        let mut result = Vec::new();
        self.compute_bounds_recursive(window_bounds, border_width, &mut result);
        result
    }

    fn compute_bounds_recursive(
        &self,
        bounds: PaneBounds,
        border_width: f32,
        result: &mut Vec<(PaneId, PaneBounds)>,
    ) {
        match self {
            PaneLayout::Leaf(id) => {
                result.push((*id, bounds));
            },
            PaneLayout::Split { direction, ratio, first, second, .. } => {
                let (first_bounds, second_bounds) =
                    split_bounds(bounds, *direction, *ratio, border_width);
                first.compute_bounds_recursive(first_bounds, border_width, result);
                second.compute_bounds_recursive(second_bounds, border_width, result);
            },
        }
    }
}

fn split_bounds(
    bounds: PaneBounds,
    direction: SplitDirection,
    ratio: f32,
    border_width: f32,
) -> (PaneBounds, PaneBounds) {
    match direction {
        SplitDirection::Vertical => {
            let first_w = (bounds.width - border_width) * ratio;
            let first =
                PaneBounds { x: bounds.x, y: bounds.y, width: first_w, height: bounds.height };
            let second = PaneBounds {
                x: bounds.x + first_w + border_width,
                y: bounds.y,
                width: bounds.width - first_w - border_width,
                height: bounds.height,
            };
            (first, second)
        },
        SplitDirection::Horizontal => {
            let first_h = (bounds.height - border_width) * ratio;
            let first =
                PaneBounds { x: bounds.x, y: bounds.y, width: bounds.width, height: first_h };
            let second = PaneBounds {
                x: bounds.x,
                y: bounds.y + first_h + border_width,
                width: bounds.width,
                height: bounds.height - first_h - border_width,
            };
            (first, second)
        },
    }
}

fn contains_pane(layout: &PaneLayout, target: PaneId) -> bool {
    match layout {
        PaneLayout::Leaf(id) => *id == target,
        PaneLayout::Split { first, second, .. } => {
            contains_pane(first, target) || contains_pane(second, target)
        },
    }
}

fn first_leaf(layout: &PaneLayout) -> Option<PaneId> {
    match layout {
        PaneLayout::Leaf(id) => Some(*id),
        PaneLayout::Split { first, .. } => first_leaf(first),
    }
}

fn last_leaf(layout: &PaneLayout) -> Option<PaneId> {
    match layout {
        PaneLayout::Leaf(id) => Some(*id),
        PaneLayout::Split { second, .. } => last_leaf(second),
    }
}

impl fmt::Display for SplitDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SplitDirection::Vertical => write!(f, "vertical"),
            SplitDirection::Horizontal => write!(f, "horizontal"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_2x2_grid() -> PaneLayout {
        // Build: Split(V, 0.5,
        //            Split(H, 0.5, Leaf(TL), Leaf(BL)),
        //            Split(H, 0.5, Leaf(TR), Leaf(BR)))
        let tl = PaneId::new(1);
        let bl = PaneId::new(2);
        let tr = PaneId::new(3);
        let br = PaneId::new(4);

        let left = PaneLayout::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::Leaf(tl)),
            second: Box::new(PaneLayout::Leaf(bl)),
        };
        let right = PaneLayout::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::Leaf(tr)),
            second: Box::new(PaneLayout::Leaf(br)),
        };
        PaneLayout::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(left),
            second: Box::new(right),
        }
    }

    fn make_2x2_grid_h_root() -> PaneLayout {
        // Build: Split(H, 0.5,
        //            Split(V, 0.5, Leaf(TL), Leaf(TR)),
        //            Split(V, 0.5, Leaf(BL), Leaf(BR)))
        let tl = PaneId::new(1);
        let tr = PaneId::new(2);
        let bl = PaneId::new(3);
        let br = PaneId::new(4);

        let top = PaneLayout::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(PaneLayout::Leaf(tl)),
            second: Box::new(PaneLayout::Leaf(tr)),
        };
        let bottom = PaneLayout::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(PaneLayout::Leaf(bl)),
            second: Box::new(PaneLayout::Leaf(br)),
        };
        PaneLayout::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(top),
            second: Box::new(bottom),
        }
    }

    #[test]
    fn test_corners_at_2x2_grid() {
        let layout = make_2x2_grid();
        let bounds = PaneBounds { x: 0., y: 0., width: 800., height: 600. };
        let border = 3.;

        // Center intersection: vertical border at x≈398.5, horizontal at y≈298.5
        let corners = layout.corners_at(400., 300., &bounds, border);
        assert_eq!(corners.len(), 3, "should find 3 borders at center intersection");
    }

    #[test]
    fn test_corners_at_2x2_grid_h_root() {
        let layout = make_2x2_grid_h_root();
        let bounds = PaneBounds { x: 0., y: 0., width: 800., height: 600. };
        let border = 3.;

        // Center intersection: horizontal border at y≈298.5, vertical at x≈398.5
        let corners = layout.corners_at(400., 300., &bounds, border);
        assert_eq!(corners.len(), 3, "should find 3 borders at center intersection");
    }

    #[test]
    fn test_corners_at_h_root_single_border() {
        let layout = make_2x2_grid_h_root();
        let bounds = PaneBounds { x: 0., y: 0., width: 800., height: 600. };
        let border = 3.;

        // Point on horizontal border only (not on either vertical)
        let corners = layout.corners_at(200., 300., &bounds, border);
        assert_eq!(corners.len(), 1, "should find only horizontal border");
        assert_eq!(corners[0].1, SplitDirection::Horizontal);
    }

    #[test]
    fn test_corners_at_single_border() {
        let layout = make_2x2_grid();
        let bounds = PaneBounds { x: 0., y: 0., width: 800., height: 600. };
        let border = 3.;

        // Point on vertical border only (not on horizontal)
        let corners = layout.corners_at(400., 200., &bounds, border);
        assert_eq!(corners.len(), 1, "should find only vertical border");
        assert_eq!(corners[0].1, SplitDirection::Vertical);
    }

    #[test]
    fn test_corners_at_single_horizontal_border() {
        let layout = make_2x2_grid();
        let bounds = PaneBounds { x: 0., y: 0., width: 800., height: 600. };
        let border = 3.;

        // Point on left horizontal border only
        let corners = layout.corners_at(200., 300., &bounds, border);
        assert_eq!(corners.len(), 1, "should find only left horizontal border");
        assert_eq!(corners[0].1, SplitDirection::Horizontal);
    }

    #[test]
    fn test_corner_drag_resize() {
        let mut layout = make_2x2_grid();
        let bounds = PaneBounds { x: 0., y: 0., width: 800., height: 600. };
        let border = 3.;
        let min_dim = 50.;

        let corners = layout.corners_at(400., 300., &bounds, border);
        assert_eq!(corners.len(), 3);

        // Simulate dragging the corner to (420, 320)
        for &(target, dir) in &corners {
            layout.resize_drag(target, dir, 420., &bounds, border, min_dim);
        }

        // After drag, the root vertical ratio should have changed
        match &layout {
            PaneLayout::Split { ratio, .. } => {
                assert!(*ratio > 0.5, "vertical ratio should increase when dragging right");
            },
            _ => panic!("expected root split"),
        }
    }
}
