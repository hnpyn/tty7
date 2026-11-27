mod boxdraw;
mod color_scheme;
mod command_cursor;
mod composer;
pub mod element;
pub(crate) mod files;
pub mod fps;
pub(crate) mod images;
pub mod input;
mod notify;
pub mod palette;
pub(crate) mod parked_cursor;
pub(crate) mod prompt_reflow;
mod remote;
pub(crate) mod scrollbar;
pub mod search;
mod size;
mod smart_select;
pub mod view;

#[cfg(target_os = "windows")]
pub(crate) use notify::notify_desktop_for_pane;
pub use remote::shutdown_panes;
pub use size::TermSize;
