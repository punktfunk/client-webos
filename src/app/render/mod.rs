//! The render path: per-frame bookkeeping (`prepare`), the grid's window (`prepare_grid`),
//! what the screens say (`geometry`), and the animation state (`state`).

pub(crate) mod geometry;
pub(crate) mod prepare;
pub(crate) mod prepare_grid;
pub(crate) mod state;
