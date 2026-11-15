//! Light/dark hint the pane spawner reads when it fills `COLORFGBG`.
//!
//! The window and the shell live in the same process, so the hint is a flag
//! the theme setter stores and the next spawn reads. Nothing is written to
//! disk.

use std::sync::atomic::{AtomicBool, Ordering};

static DARK: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Appearance {
    pub dark: bool,
}

pub fn appearance() -> Appearance {
    Appearance {
        dark: DARK.load(Ordering::Relaxed),
    }
}

pub fn note_appearance(dark: bool) {
    DARK.store(dark, Ordering::Relaxed);
}
