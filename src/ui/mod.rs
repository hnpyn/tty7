pub mod assets;
pub mod dialog;
pub mod host_ops;
pub mod i18n;
pub mod keymap;
pub mod prefill;
pub mod presets;
pub mod prompt;
pub mod rounding;
pub mod solo;
pub mod theme;

/// The two answers a confirmation dialog gets, arranged the way macOS arranges
/// them: the action on the right, where Return lands and where every other app
/// on the machine puts it, and the safe answer on the left, marked as the
/// cancel so it also answers Escape, Space and the initial keyboard focus.
///
/// gpui renders answer 0 rightmost, so the action goes first — `Ok(0)` is
/// "they meant it" and everything else, including a dropped channel, is "leave
/// it alone".
pub(crate) fn confirm_answers(action: &str, keep: &str) -> [gpui::PromptButton; 2] {
    [
        gpui::PromptButton::ok(action),
        gpui::PromptButton::cancel(keep),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_action_answers_return_and_the_safe_one_answers_escape() {
        // gpui hands answer 0 to the platform first, and both NSAlert and
        // TaskDialog draw that one on the right and give it Return. Reversing
        // these two puts Delete under the mouse where Cancel belongs.
        let [action, keep] = super::confirm_answers("Delete", "Cancel");
        assert_eq!(action.label(), "Delete");
        assert!(!action.is_cancel(), "answer 0 has to keep Return");
        assert_eq!(keep.label(), "Cancel");
        assert!(keep.is_cancel(), "only a cancel answer is given Escape");
    }
}
