//! The geometry the app lays out in: an integer rect in display units, its float twin for
//! animated positions, a size.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub fn x(&self) -> i32 {
        self.x
    }

    pub fn y(&self) -> i32 {
        self.y
    }

    pub fn width(&self) -> u32 {
        self.w
    }

    pub fn height(&self) -> u32 {
        self.h
    }

    pub fn right(&self) -> i32 {
        self.x + self.w as i32
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h as i32
    }

    pub fn offset(self, dx: i32, dy: i32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    /// Inset by `pad` on the left and right, full height. The content column inside a
    /// card: one pad governs both edges, so there is nothing here for a [`Layout`] split
    /// to keep in agreement.
    ///
    /// [`Layout`]: crate::ui::layout::Layout
    pub fn inset_x(self, pad: u32) -> Self {
        Self::new(self.x + pad as i32, self.y, self.w.saturating_sub(2 * pad), self.h)
    }

    pub fn contains_point(&self, p: (i32, i32)) -> bool {
        let (px, py) = p;
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }
}

/// Float rectangle, for the one case where whole-pixel placement is too coarse: a pan
/// slow enough that an integer destination would advance in visible jumps rather than
/// drift (see `DrawCmd::TexF`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// extent it is laying out into.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Size {
    pub w: u32,
    pub h: u32,
}

impl Size {
    pub fn new(w: u32, h: u32) -> Self {
        Self { w, h }
    }
}
