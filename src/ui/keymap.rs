use gpui::{App, Global, KeyBinding, Keystroke};

use crate::core::actions::*;
use crate::core::config::{Config, KeybindingOverride};
use crate::terminal::view::{
    AlternatePaste, ClearScrollback, CopyRaw, CopyText, FindInTerminal, FindNext, FindPrevious,
    InsertNewline, InsertNewlineFallback, PasteText, ToggleComposer,
};
use crate::ui::theme::set_menus;

/// Bindings gpui-component installed before ntty7. A rebuild replaces the map,
/// so these have to be laid down again or text fields lose backspace (#548).
struct BaseBindings(Vec<KeyBinding>);
impl Global for BaseBindings {}

pub fn init(cx: &mut App) {
    if cx.try_global::<BaseBindings>().is_none() {
        let base: Vec<KeyBinding> = cx.key_bindings().borrow().bindings().cloned().collect();
        cx.set_global(BaseBindings(base));
    }
    rebuild_keymap(cx);
    // A focused confirmation, or a minimized window, never sees this chord.
    // The live window asks; only a process with no window left quits here.
    cx.on_action(|_: &Quit, cx: &mut App| crate::ui::solo::route_quit(cx));
    cx.on_action(|_: &NewWindow, cx: &mut App| crate::ui::solo::route_new_window(cx));
    cx.on_action(crate::ui::theme::toggle_theme_follow_system);
    cx.on_action(crate::ui::theme::select_theme);
    cx.on_action(crate::ui::theme::open_config_file);
    set_menus(cx);
}

/// Terminal keys the config table does not carry: Tab is input. Cmd+Enter
/// while a completion or history-search menu is open falls through to the
/// editor, which accepts the row and submits the line.
fn fixed_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("secondary-+", IncreaseFontSize, None),
        KeyBinding::new("tab", SendTab, Some("Terminal")),
        KeyBinding::new("shift-tab", SendBackTab, Some("Terminal")),
    ]
}

/// Replaces the keymap: inherited bindings, then the live config, then the fixed ones.
/// gpui only appends, so this clears first; otherwise every reload leaks a full table.
fn rebuild_keymap(cx: &mut App) {
    let effective = effective_bindings(cx);
    let mut bindings: Vec<KeyBinding> = cx
        .try_global::<BaseBindings>()
        .map(|base| base.0.clone())
        .unwrap_or_default();
    bindings.extend(action_bindings(&effective));
    bindings.extend(fixed_bindings());
    cx.clear_key_bindings();
    cx.bind_keys(bindings);
}

pub fn rebind(cx: &mut App) {
    rebuild_keymap(cx);
    set_menus(cx);
}

/// The three config fields `effective_bindings` reads. The watcher rebinds only when one of them moved.
pub(crate) fn keybinding_config(cx: &App) -> (Vec<(String, KeybindingOverride)>, String, String) {
    let cfg = cx.global::<Config>();
    let mut overrides: Vec<(String, KeybindingOverride)> = cfg
        .keybindings
        .iter()
        .map(|(a, k)| (a.clone(), k.clone()))
        .collect();
    // A HashMap's order is not stable across reloads, and a reordered map is
    // not a changed binding set.
    overrides.sort();
    (overrides, cfg.keybinding_preset.clone(), cfg.prefix.clone())
}

const INSERT_NEWLINE_DEFAULT: &str = "shift-enter";

const INSERT_NEWLINE_ALT_DEFAULT: &str = "alt-enter";

const PASTE_ALT_DEFAULT: &str = "shift-insert";

fn paste_text_default() -> &'static str {
    per_platform("", "ctrl-shift-v")
}

/// Windows and Linux paste with Ctrl+V everywhere else in the desktop, so the
/// terminal answers it too — but only off the alternate screen, where the key
/// is a control code a full-screen program is waiting for (#677). It is a
/// binding of its own rather than a second keystroke on `PasteText` so that it
/// can carry that narrower context, and so that a user who wants the Windows
/// Terminal behaviour back can say so: `"AlternatePaste": ""` hands Ctrl+V to
/// the shell at the prompt as well, and `"PasteText": "ctrl-v"` pastes with it
/// on every screen.
fn alternate_paste_default() -> &'static str {
    per_platform("", "ctrl-v")
}

fn extra_defaults() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "InsertNewline",
            INSERT_NEWLINE_DEFAULT,
            INSERT_NEWLINE_ALT_DEFAULT,
        ),
        ("PasteText", paste_text_default(), PASTE_ALT_DEFAULT),
    ]
}

fn extra_keystrokes(effective: &[(String, String)]) -> Vec<(&'static str, &'static str)> {
    extra_defaults()
        .into_iter()
        .filter(|(action, primary, _)| {
            !primary.is_empty() && effective.iter().any(|(a, k)| a == action && k == primary)
        })
        .map(|(action, _, extra)| (action, extra))
        .collect()
}

fn is_default_insert_newline_binding(action: &str, key: &str) -> bool {
    action == "InsertNewline" && key == INSERT_NEWLINE_DEFAULT
}

/// Installs `key` for `action`, and alongside it the chord the platform will
/// actually deliver when the two are spelled differently — `secondary-shift-]`
/// is pressed as `secondary-}`, and only the second ever reaches the
/// dispatcher. Both go in rather than one replacing the other, so a spec that
/// was already live stays live whatever the backend does with Shift (#750).
///
/// Answers whether the action was known at all, which is the caller's cue to
/// warn about a name it cannot bind.
fn push_binding(bindings: &mut Vec<KeyBinding>, action: &str, key: &str) -> bool {
    let Some(binding) = make_binding(action, key) else {
        return false;
    };
    bindings.push(binding);
    if let Some(folded) = folded_spec(key)
        && let Some(alias) = make_binding(action, &folded)
    {
        bindings.push(alias);
    }
    true
}

fn action_bindings(effective: &[(String, String)]) -> Vec<KeyBinding> {
    let mut bindings = Vec::new();
    for (action, key) in extra_keystrokes(effective) {
        push_binding(&mut bindings, action, key);
    }
    for (action, key) in effective {
        if key.is_empty() {
            continue;
        }
        if !keystroke_is_valid(key) {
            log::warn!("ignoring keybinding for '{action}': invalid keystroke '{key}'");
            continue;
        }
        // Warn when a user chord takes a C0 byte the shell is owed, including the folded spelling.
        let folded = folded_spec(key);
        for chord in [Some(key.as_str()), folded.as_deref()]
            .into_iter()
            .flatten()
        {
            if !chord.contains(' ')
                && steals_a_control_code(chord)
                && !control_code_binding_allowed(action, chord)
            {
                log::warn!(
                    "keybinding '{chord}' for '{action}' takes a control code away from the shell"
                );
            }
        }
        if !push_binding(&mut bindings, action, key) {
            log::warn!("ignoring keybinding: unknown action '{action}'");
        } else if is_default_insert_newline_binding(action, key) {
            bindings.push(KeyBinding::new(
                INSERT_NEWLINE_DEFAULT,
                InsertNewlineFallback,
                Some("Terminal"),
            ));
        }
    }
    bindings
}

/// Whether a chord is one the terminal owes the PTY as a control code.
///
/// Ctrl and nothing else, over the keys that carry a C0 byte: the alphabet,
/// `@ [ \ ] ^ _ / ?`, the digits 2..8 and Space — `ctrl_c0` in
/// `terminal::input` is the table this mirrors. A binding sitting on one of
/// these does not merely shadow the shell, it deletes a byte the program on
/// the far end is waiting for — Ctrl+D is EOF, Ctrl+W deletes a word, Ctrl+^
/// is vim's alternate file.
///
/// The backtick is in the set without being in that table — it is held over
/// from when this rule lived inside the defaults test. It errs the safe way:
/// a chord that encodes nothing gets a warning it did not strictly earn,
/// which is cheaper than a default quietly eating one that does.
fn steals_a_control_code(chord: &str) -> bool {
    let Ok(ks) = Keystroke::parse(chord) else {
        return false;
    };
    let m = &ks.modifiers;
    if !m.control || m.alt || m.shift || m.platform || m.function {
        return false;
    }
    ks.key == "space"
        || ks.key.len() == 1
            && ks.key.chars().next().is_some_and(|c| {
                c.is_ascii_alphabetic() || "[]\\`^_/?@".contains(c) || ('2'..='8').contains(&c)
            })
}

/// Ctrl+V is how Windows and Linux paste. Everything else on a bare control
/// code belongs to the shell.
fn control_code_binding_allowed(_action: &str, chord: &str) -> bool {
    cfg!(not(target_os = "macos")) && chord == "ctrl-v"
}

fn per_platform(mac: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        mac
    } else {
        other
    }
}

pub(crate) fn default_bindings() -> Vec<(&'static str, &'static str)> {
    shipped_bindings()
}

fn shipped_bindings() -> Vec<(&'static str, &'static str)> {
    vec![
        ("NewTab", per_platform("secondary-t", "secondary-shift-t")),
        (
            "NewWindow",
            per_platform("secondary-n", "secondary-shift-n"),
        ),
        // Listed before CloseWindow. They share one chord, and gpui keeps the
        // binding added last, so CloseWindow is the one a keypress runs.
        (
            "CloseActiveTab",
            per_platform("secondary-w", "secondary-shift-w"),
        ),
        (
            "CloseWindow",
            per_platform("secondary-w", "secondary-shift-w"),
        ),
        ("IncreaseFontSize", "secondary-="),
        ("DecreaseFontSize", "secondary--"),
        ("ResetFontSize", "secondary-0"),
        (
            "FindInTerminal",
            if cfg!(target_os = "macos") {
                "secondary-f"
            } else {
                "ctrl-shift-f"
            },
        ),
        (
            "FindNext",
            if cfg!(target_os = "macos") {
                "secondary-g"
            } else {
                "f3"
            },
        ),
        (
            "FindPrevious",
            if cfg!(target_os = "macos") {
                "secondary-shift-g"
            } else {
                "shift-f3"
            },
        ),
        (
            "ClearScrollback",
            per_platform("secondary-k", "secondary-shift-k"),
        ),
        ("InsertNewline", INSERT_NEWLINE_DEFAULT),
        (
            "ToggleComposer",
            per_platform("secondary-i", "secondary-shift-i"),
        ),
        ("CopyText", per_platform("", "ctrl-shift-c")),
        ("CopyRaw", "cmd-alt-c"),
        ("PasteText", paste_text_default()),
        ("AlternatePaste", alternate_paste_default()),
        (
            "HideApp",
            if cfg!(target_os = "macos") {
                "secondary-h"
            } else {
                ""
            },
        ),
        (
            "HideOthers",
            if cfg!(target_os = "macos") {
                "secondary-alt-h"
            } else {
                ""
            },
        ),
        ("ShowAll", ""),
        (
            "MinimizeWindow",
            if cfg!(target_os = "macos") {
                "secondary-m"
            } else {
                ""
            },
        ),
        ("ZoomWindow", ""),
        ("ToggleFullscreen", per_platform("secondary-ctrl-f", "f11")),
        ("OpenConfigFile", "secondary-,"),
        ("Quit", per_platform("secondary-q", "secondary-shift-q")),
    ]
}

/// One action and the chords it answers to, split by where they came from:
/// the ones it ships with — its default, or the preset's in its place — and
/// the ones `config.json` names.
///
/// Kept apart because they install apart. gpui resolves two bindings on one
/// chord to the one added last, and a chord the user asked for by name has to
/// be that one, wherever its action sits in the table.
struct ActionChords {
    action: String,
    shipped: Vec<String>,
    configured: Vec<String>,
}

fn resolve_chords(
    preset: &str,
    prefix: &str,
    overrides: &std::collections::HashMap<String, KeybindingOverride>,
) -> Vec<ActionChords> {
    let mut resolved: Vec<ActionChords> = default_bindings()
        .into_iter()
        .map(|(action, key)| ActionChords {
            action: action.to_string(),
            shipped: [key]
                .into_iter()
                .filter(|k| !k.is_empty())
                .map(str::to_string)
                .collect(),
            configured: Vec::new(),
        })
        .collect();
    // A preset is a scheme rather than an addition: its chord takes the
    // default's place.
    for (action, key) in preset_bindings(preset, prefix) {
        if let Some(slot) = resolved.iter_mut().find(|s| s.action == action) {
            slot.shipped = vec![key];
        }
    }
    for (action, value) in overrides {
        let Some(slot) = resolved.iter_mut().find(|s| s.action == *action) else {
            // A hand-edited config.json with a typo used to vanish into this
            // branch. The Keybindings page lists every name that works.
            log::warn!("keybinding for unknown action {action:?} ignored");
            continue;
        };
        match value {
            // `""` has always been how a config retires a chord — the docs
            // spell `"AlternatePaste": ""`, and Settings wrote it for an action
            // whose chord was taken — so it goes on unbinding the action.
            KeybindingOverride::Add(key) if key.is_empty() => {
                slot.shipped.clear();
            }
            // A chord beside the ones the action has, not in place of them
            // (#868): `"NextTab": "cmd-shift-]"` is a second way to switch tabs,
            // and Ctrl+Tab going dead because of it was the bug.
            KeybindingOverride::Add(key) => {
                if !slot.shipped.iter().any(|k| same_chord(k, key)) {
                    slot.configured.push(key.clone());
                }
            }
            KeybindingOverride::Exact(keys) => {
                slot.shipped.clear();
                for key in keys {
                    if !key.is_empty() && !slot.configured.iter().any(|k| same_chord(k, key)) {
                        slot.configured.push(key.clone());
                    }
                }
            }
        }
    }
    resolved
}

/// Every chord as an `(action, chord)` pair, in the order the keymap is built
/// from: all the shipped chords, then all the configured ones, so that a
/// configured chord landing on another action's default is the binding that
/// wins it. An action with no chord at all has no pair.
fn flatten_chords(resolved: &[ActionChords]) -> Vec<(String, String)> {
    let pairs = |pick: fn(&ActionChords) -> &Vec<String>| {
        resolved
            .iter()
            .flat_map(move |s| pick(s).iter().map(|k| (s.action.clone(), k.clone())))
    };
    pairs(|s| &s.shipped)
        .chain(pairs(|s| &s.configured))
        .collect()
}

pub(crate) fn effective_bindings(cx: &App) -> Vec<(String, String)> {
    let cfg = cx.global::<Config>();
    flatten_chords(&resolve_chords(
        &cfg.keybinding_preset,
        &cfg.prefix,
        &cfg.keybindings,
    ))
}

fn preset_bindings(preset: &str, prefix: &str) -> Vec<(String, String)> {
    match preset {
        "tmux" => tmux_preset(prefix),
        _ => Vec::new(),
    }
}

fn tmux_preset(prefix: &str) -> Vec<(String, String)> {
    let p = |key: &str| format!("{prefix} {key}");
    [("NewTab", p("c")), ("CloseActiveTab", p("x"))]
        .into_iter()
        .map(|(action, key)| (action.to_string(), key))
        .collect()
}

#[cfg(test)]
pub(crate) fn effective_key(action: &str, cx: &App) -> Option<String> {
    effective_bindings(cx)
        .into_iter()
        .find(|(a, _)| a == action)
        .map(|(_, k)| k)
        .filter(|k| !k.is_empty())
}

pub(crate) fn spec_from_keystroke(ks: &Keystroke) -> Option<String> {
    if matches!(
        ks.key.as_str(),
        "shift" | "control" | "alt" | "platform" | "function" | "cmd" | "ctrl"
    ) {
        return None;
    }
    let m = &ks.modifiers;
    let mut parts: Vec<&str> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        if m.platform {
            parts.push("secondary");
        }
        if m.control {
            parts.push("ctrl");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if m.control {
            parts.push("secondary");
        }
        if m.platform {
            parts.push("cmd");
        }
    }
    if m.alt {
        parts.push("alt");
    }
    if m.shift {
        parts.push("shift");
    }
    if m.function {
        parts.push("fn");
    }
    let mut spec = String::new();
    for part in parts {
        spec.push_str(part);
        spec.push('-');
    }
    spec.push_str(&ks.key);
    Some(spec)
}

/// The glyph a US keyboard prints on the shifted half of every key that is not
/// a letter — `shift-]` types `}`, `shift-2` types `@`.
const SHIFTED_GLYPHS: [(char, char); 21] = [
    ('`', '~'),
    ('1', '!'),
    ('2', '@'),
    ('3', '#'),
    ('4', '$'),
    ('5', '%'),
    ('6', '^'),
    ('7', '&'),
    ('8', '*'),
    ('9', '('),
    ('0', ')'),
    ('-', '_'),
    ('=', '+'),
    ('[', '{'),
    (']', '}'),
    ('\\', '|'),
    (';', ':'),
    ('\'', '"'),
    (',', '<'),
    ('.', '>'),
    ('/', '?'),
];

/// The glyph the platform delivers for shifted punctuation, or `None` when nothing changes.
/// Digits and punctuation arrive with Shift cleared (`shift-]` → `}`), so the folded form is bound too.
/// The table is US-layout and only ever adds — substituting would drop Windows numpad chords that still carry Shift.
fn fold_shift_into_glyph(chord: &str) -> Option<String> {
    let mut ks = Keystroke::parse(chord).ok()?;
    if !ks.modifiers.shift {
        return None;
    }
    let mut chars = ks.key.chars();
    let (Some(key), None) = (chars.next(), chars.next()) else {
        return None;
    };
    if let Some((_, shifted)) = SHIFTED_GLYPHS.iter().find(|(plain, _)| *plain == key) {
        ks.key = shifted.to_string();
    } else if !SHIFTED_GLYPHS.iter().any(|(_, shifted)| *shifted == key) {
        return None;
    }
    ks.modifiers.shift = false;
    spec_from_keystroke(&ks)
}

/// `spec` with every chord folded, or `None` when it is already the shape the
/// platform delivers and there is nothing to add.
pub(crate) fn folded_spec(spec: &str) -> Option<String> {
    let mut folded = String::with_capacity(spec.len());
    let mut changed = false;
    for chord in spec.split_whitespace() {
        if !folded.is_empty() {
            folded.push(' ');
        }
        match fold_shift_into_glyph(chord) {
            Some(chord) => {
                folded.push_str(&chord);
                changed = true;
            }
            None => folded.push_str(chord),
        }
    }
    changed.then_some(folded)
}

/// Whether two specs claim the same keystroke — spelled the same way, or one
/// the fold of the other. `secondary-shift-]` and `secondary-}` are one chord
/// written twice, and a rebind that cannot see that leaves both installed on
/// it, where which one fires is arbitrary (#750).
pub(crate) fn same_chord(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (folded_a, folded_b) = (folded_spec(a), folded_spec(b));
    folded_a.as_deref() == Some(b)
        || folded_b.as_deref() == Some(a)
        || (folded_a.is_some() && folded_a == folded_b)
}

#[cfg(test)]
pub(crate) fn key_chords(spec: &str) -> Vec<Vec<String>> {
    spec.split_whitespace().map(key_tokens).collect()
}

#[cfg(test)]
pub(crate) fn key_tokens(spec: &str) -> Vec<String> {
    #[cfg(target_os = "macos")]
    const MODS: [(&str, &str); 6] = [
        ("secondary", "⌘"),
        ("cmd", "⌘"),
        ("ctrl", "⌃"),
        ("alt", "⌥"),
        ("shift", "⇧"),
        ("fn", "fn"),
    ];
    #[cfg(not(target_os = "macos"))]
    const MODS: [(&str, &str); 6] = [
        ("secondary", "Ctrl"),
        ("cmd", "Win"),
        ("ctrl", "Ctrl"),
        ("alt", "Alt"),
        ("shift", "Shift"),
        ("fn", "Fn"),
    ];
    let mut rest = spec;
    let mut tokens = Vec::new();
    'outer: loop {
        for (name, glyph) in MODS {
            let prefix = format!("{name}-");
            if let Some(stripped) = rest.strip_prefix(&prefix)
                && !stripped.is_empty()
            {
                tokens.push(glyph.to_string());
                rest = stripped;
                continue 'outer;
            }
        }
        break;
    }
    // macOS names modifiers in one fixed order — ⌃ ⌥ ⇧ ⌘ — whatever order a
    // binding was written in; `secondary-shift-o` read `⌘ ⇧ O` here while the
    // menu bar said `⇧⌘O` for the same key.
    #[cfg(target_os = "macos")]
    tokens.sort_by_key(|glyph| {
        ["fn", "⌃", "⌥", "⇧", "⌘"]
            .iter()
            .position(|g| g == glyph)
            .unwrap_or(usize::MAX)
    });
    tokens.push(key_glyph(rest));
    tokens
}

#[cfg(test)]
fn key_glyph(key: &str) -> String {
    match key {
        // The glyph every hint in the app already draws for Return; this one
        // alone said `⏎`, so the same key looked different in a row's
        // shortcut and in the footer under it.
        "enter" | "return" => "↵".into(),
        "tab" => "⇥".into(),
        "space" => "Space".into(),
        "escape" | "esc" => "⎋".into(),
        "backspace" => "⌫".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "-" => "−".into(),
        other => other.to_uppercase(),
    }
}

fn keystroke_is_valid(s: &str) -> bool {
    let mut any = false;
    for token in s.split_whitespace() {
        any = true;
        if gpui::Keystroke::parse(token).is_err() {
            return false;
        }
    }
    any
}

fn action_context(action: &str) -> Option<&'static str> {
    match action {
        "FindInTerminal" | "FindNext" | "FindPrevious" | "ClearScrollback" | "InsertNewline"
        | "CopyText" | "CopyRaw" | "PasteText" | "ToggleComposer" => Some("Terminal"),
        "AlternatePaste" => Some("Terminal && !alt_screen"),
        _ => None,
    }
}

fn make_binding(action: &str, keystroke: &str) -> Option<KeyBinding> {
    let context = action_context(action);
    Some(match action {
        "NewTab" => KeyBinding::new(keystroke, NewTab, context),
        "NewWindow" => KeyBinding::new(keystroke, NewWindow, context),
        "CloseWindow" => KeyBinding::new(keystroke, CloseWindow, context),
        "CloseActiveTab" => KeyBinding::new(keystroke, CloseActiveTab, context),
        "IncreaseFontSize" => KeyBinding::new(keystroke, IncreaseFontSize, context),
        "DecreaseFontSize" => KeyBinding::new(keystroke, DecreaseFontSize, context),
        "ResetFontSize" => KeyBinding::new(keystroke, ResetFontSize, context),
        "FindInTerminal" => KeyBinding::new(keystroke, FindInTerminal, context),
        "FindNext" => KeyBinding::new(keystroke, FindNext, context),
        "FindPrevious" => KeyBinding::new(keystroke, FindPrevious, context),
        "ClearScrollback" => KeyBinding::new(keystroke, ClearScrollback, context),
        "InsertNewline" => KeyBinding::new(keystroke, InsertNewline, context),
        "ToggleComposer" => KeyBinding::new(keystroke, ToggleComposer, context),
        "CopyText" => KeyBinding::new(keystroke, CopyText, context),
        "CopyRaw" => KeyBinding::new(keystroke, CopyRaw, context),
        "PasteText" => KeyBinding::new(keystroke, PasteText, context),
        "AlternatePaste" => KeyBinding::new(keystroke, AlternatePaste, context),
        "HideApp" => KeyBinding::new(keystroke, HideApp, context),
        "HideOthers" => KeyBinding::new(keystroke, HideOthers, context),
        "ShowAll" => KeyBinding::new(keystroke, ShowAll, context),
        "MinimizeWindow" => KeyBinding::new(keystroke, MinimizeWindow, context),
        "ZoomWindow" => KeyBinding::new(keystroke, ZoomWindow, context),
        "ToggleFullscreen" => KeyBinding::new(keystroke, ToggleFullscreen, context),
        "OpenConfigFile" => KeyBinding::new(keystroke, OpenConfigFile, context),
        "Quit" => KeyBinding::new(keystroke, Quit, context),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Action as _;

    #[cfg(target_os = "macos")]
    const SECONDARY: &str = "⌘";
    #[cfg(not(target_os = "macos"))]
    const SECONDARY: &str = "Ctrl";
    #[cfg(target_os = "macos")]
    const SHIFT: &str = "⇧";
    #[cfg(not(target_os = "macos"))]
    const SHIFT: &str = "Shift";
    #[cfg(target_os = "macos")]
    const CTRL: &str = "⌃";
    #[cfg(not(target_os = "macos"))]
    const CTRL: &str = "Ctrl";

    /// The actions a keymap built from `action_bindings` dispatches for `keys`
    /// typed in `context`, in precedence order — the same lookup gpui performs
    /// on a real keypress.
    ///
    /// Bindings are asserted through this rather than through a mirror of the
    /// table, so a chord the app would not really install, or would install in
    /// another context, cannot pass.
    fn dispatched(effective: &[(String, String)], keys: &str, context: &str) -> Vec<&'static str> {
        let input: Vec<Keystroke> = keys
            .split(' ')
            .map(|k| Keystroke::parse(k).expect("the typed keystroke parses"))
            .collect();
        dispatched_keystrokes(effective, &input, context)
    }

    /// The same lookup for input a spec cannot spell — the shifted-glyph shape
    /// a backend hands over for `secondary-shift-]` has no written form that
    /// `Keystroke::parse` turns back into it.
    fn dispatched_keystrokes(
        effective: &[(String, String)],
        input: &[Keystroke],
        context: &str,
    ) -> Vec<&'static str> {
        let mut keymap = gpui::Keymap::default();
        keymap.add_bindings(action_bindings(effective));
        let context = [gpui::KeyContext::parse(context).expect("the context parses")];
        keymap
            .bindings_for_input(input, &context)
            .0
            .iter()
            .map(|b| b.action().name())
            .collect()
    }

    /// What every gpui backend delivers when the secondary modifier is held
    /// over a key whose shifted half is `glyph`: the glyph, and no shift flag —
    /// the modifier is already spent on the character.
    fn delivered_with_secondary(glyph: &str) -> Keystroke {
        Keystroke {
            modifiers: gpui::Modifiers {
                control: cfg!(not(target_os = "macos")),
                platform: cfg!(target_os = "macos"),
                shift: false,
                alt: false,
                function: false,
            },
            key: glyph.to_string(),
            key_char: None,
        }
    }

    #[test]
    fn key_tokens_maps_modifiers_to_glyphs() {
        assert_eq!(key_tokens("secondary-t"), vec![SECONDARY, "T"]);
        #[cfg(target_os = "macos")]
        assert_eq!(key_tokens("secondary-shift-d"), vec![SHIFT, SECONDARY, "D"]);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(key_tokens("secondary-shift-d"), vec![SECONDARY, SHIFT, "D"]);
        assert_eq!(key_tokens("secondary-enter"), vec![SECONDARY, "↵"]);
    }

    #[test]
    fn key_tokens_keeps_the_minus_key_as_the_final_token() {
        assert_eq!(key_tokens("secondary--"), vec![SECONDARY, "−"]);
        assert_eq!(key_tokens("secondary-="), vec![SECONDARY, "="]);
        assert_eq!(key_tokens("secondary-,"), vec![SECONDARY, ","]);
    }

    #[test]
    fn key_chords_splits_a_sequence_into_keycap_groups() {
        assert_eq!(
            key_chords("ctrl-b n"),
            vec![
                vec![CTRL.to_string(), "B".to_string()],
                vec!["N".to_string()]
            ]
        );
        assert_eq!(key_chords("secondary-t"), vec![vec![SECONDARY, "T"]]);
    }

    #[test]
    fn every_default_action_has_a_binding_builder_or_is_unbound() {
        for (action, key) in default_bindings() {
            if !key.is_empty() {
                assert!(
                    keystroke_is_valid(key),
                    "default keystroke for {action} is invalid: {key:?}"
                );
                assert!(
                    make_binding(action, key).is_some(),
                    "no make_binding arm for action {action}"
                );
            }
        }
    }

    #[test]
    fn every_action_has_a_binding_arm() {
        // The sibling test above only reaches actions that ship with a default
        // keystroke, which leaves the unbound ones — the majority — free to be
        // listed in `default_bindings` with no `make_binding` arm behind them.
        // Nothing surfaces that: the action shows up in Settings, the user
        // assigns a key, and the key silently does nothing.
        for (action, _) in default_bindings() {
            assert!(
                make_binding(action, "ctrl-f12").is_some(),
                "no make_binding arm for action {action}; \
                 anyone who binds a key to it in Settings gets nothing"
            );
        }
    }

    #[test]
    fn tmux_preset_keystrokes_all_parse_and_map_to_actions() {
        for (action, key) in tmux_preset("ctrl-b") {
            assert!(
                keystroke_is_valid(&key),
                "tmux preset keystroke for {action} does not parse: {key:?}"
            );
            assert!(
                make_binding(&action, &key).is_some(),
                "tmux preset action {action} has no make_binding arm"
            );
        }
    }

    #[test]
    fn insert_newline_ships_both_default_chords() {
        let effective: Vec<(String, String)> = default_bindings()
            .into_iter()
            .map(|(a, k)| (a.to_string(), k.to_string()))
            .collect();
        assert_eq!(
            effective
                .iter()
                .find(|(a, _)| a == "InsertNewline")
                .map(|(_, k)| k.as_str()),
            Some("shift-enter")
        );
        assert!(extra_keystrokes(&effective).contains(&("InsertNewline", "alt-enter")));
        for key in ["shift-enter", "alt-enter"] {
            assert!(keystroke_is_valid(key), "{key} does not parse");
            assert!(
                dispatched(&effective, key, "Terminal").contains(&InsertNewline::name_for_type()),
                "{key} does not reach InsertNewline in a terminal"
            );
        }
        assert_eq!(key_tokens("shift-enter"), vec![SHIFT, "↵"]);
    }

    #[test]
    fn shift_enter_fallback_retires_with_an_insert_newline_rebind() {
        assert!(is_default_insert_newline_binding(
            "InsertNewline",
            "shift-enter"
        ));
        assert!(!is_default_insert_newline_binding(
            "InsertNewline",
            "ctrl-o"
        ));
        assert!(!is_default_insert_newline_binding("InsertNewline", ""));
    }

    #[test]
    fn shift_enter_prefix_binding_still_waits_for_its_second_key() {
        let mut effective = default_bindings()
            .into_iter()
            .map(|(action, key)| (action.to_string(), key.to_string()))
            .collect::<Vec<_>>();
        // Quit is the last shipped binding, so this prefix is installed after
        // InsertNewline's complete `shift-enter`. gpui keeps the later one,
        // and a later prefix has to wait instead of letting the newline fire.
        effective
            .iter_mut()
            .find(|(action, _)| action == "Quit")
            .unwrap()
            .1 = "shift-enter x".to_string();

        let mut keymap = gpui::Keymap::default();
        keymap.add_bindings(action_bindings(&effective));
        let input = [gpui::Keystroke::parse("shift-enter").unwrap()];
        let context = [gpui::KeyContext::parse("Terminal").unwrap()];
        let (_, pending) = keymap.bindings_for_input(&input, &context);

        assert!(pending, "the keymap must wait for the second key");

        let input = [
            gpui::Keystroke::parse("shift-enter").unwrap(),
            gpui::Keystroke::parse("x").unwrap(),
        ];
        let (matched, pending) = keymap.bindings_for_input(&input, &context);
        assert!(!pending);
        assert!(
            matched
                .first()
                .is_some_and(|binding| binding.action().partial_eq(&Quit))
        );
    }

    #[test]
    fn paste_ships_both_terminal_chords_off_macos_and_retires_together() {
        let effective: Vec<(String, String)> = default_bindings()
            .into_iter()
            .map(|(a, k)| (a.to_string(), k.to_string()))
            .collect();
        if cfg!(target_os = "macos") {
            assert!(
                !extra_keystrokes(&effective)
                    .iter()
                    .any(|(a, _)| *a == "PasteText"),
                "macOS pastes with Cmd+V; no extra chord should install there"
            );
            return;
        }
        assert_eq!(
            effective
                .iter()
                .find(|(a, _)| a == "PasteText")
                .map(|(_, k)| k.as_str()),
            Some("ctrl-shift-v")
        );
        assert_eq!(
            effective
                .iter()
                .find(|(a, _)| a == "CopyText")
                .map(|(_, k)| k.as_str()),
            Some("ctrl-shift-c")
        );
        assert!(extra_keystrokes(&effective).contains(&("PasteText", "shift-insert")));
        for key in ["ctrl-shift-v", "shift-insert"] {
            assert!(
                dispatched(&effective, key, "Terminal").contains(&PasteText::name_for_type()),
                "{key} must paste in a terminal"
            );
            assert!(
                !dispatched(&effective, key, "Workspace").contains(&PasteText::name_for_type()),
                "{key} is a terminal chord and must not paste outside one"
            );
        }
        // Plain Ctrl+V is `AlternatePaste`, and only where no full-screen
        // program is running: on the alternate screen the chord belongs to
        // that program and reaches it as SYN.
        assert!(
            dispatched(&effective, "ctrl-v", "Terminal").contains(&AlternatePaste::name_for_type()),
            "ctrl-v pastes at a prompt off macOS"
        );
        assert!(
            dispatched(&effective, "ctrl-v", "Terminal alt_screen").is_empty(),
            "a full-screen program owns ctrl-v"
        );
        assert!(
            dispatched(&effective, "ctrl-shift-v", "Terminal alt_screen")
                .contains(&PasteText::name_for_type()),
            "Ctrl+Shift+V is the paste that works on every screen"
        );
        // The two ways out, both of which the control-code validator has to
        // let through: retire the chord, or hand it the whole screen. Both are
        // one line in a `config.json`, so both are asserted against the whole
        // default table with that line applied — a bare one-entry table would
        // pass either assertion without the escape hatch working at all.
        let with = |action: &str, value: KeybindingOverride| {
            let overrides = std::collections::HashMap::from([(action.to_string(), value)]);
            flatten_chords(&resolve_chords("default", "ctrl-b", &overrides))
        };
        let retired = with("AlternatePaste", KeybindingOverride::Add(String::new()));
        assert!(
            dispatched(&retired, "ctrl-v", "Terminal").is_empty(),
            "an emptied AlternatePaste gives Ctrl+V back to the shell"
        );
        let everywhere = with("PasteText", KeybindingOverride::Add("ctrl-v".to_string()));
        for context in ["Terminal", "Terminal alt_screen"] {
            assert!(
                dispatched(&everywhere, "ctrl-v", context).contains(&PasteText::name_for_type()),
                "a user may put Paste itself on Ctrl+V and have it on every screen"
            );
        }
        let rebound = vec![("PasteText".to_string(), "ctrl-alt-v".to_string())];
        assert!(
            !extra_keystrokes(&rebound)
                .iter()
                .any(|(a, _)| *a == "PasteText"),
            "moving Paste off its default must retire Shift+Insert with it"
        );
    }

    #[test]
    fn rebinding_insert_newline_retires_both_default_chords() {
        let effective = vec![("InsertNewline".to_string(), "ctrl-o".to_string())];
        assert!(extra_keystrokes(&effective).is_empty());
        assert_eq!(
            dispatched(&effective, "ctrl-o", "Terminal"),
            vec![InsertNewline::name_for_type()],
            "the chord the config asks for is the one the keymap dispatches"
        );
        for retired in ["shift-enter", "alt-enter"] {
            assert!(
                dispatched(&effective, retired, "Terminal").is_empty(),
                "{retired} is off the action now and must dispatch nothing"
            );
        }

        let unbound = vec![("InsertNewline".to_string(), String::new())];
        assert!(extra_keystrokes(&unbound).is_empty());
        assert!(action_bindings(&unbound).is_empty());
    }

    #[test]
    fn each_binding_lands_in_the_context_its_action_is_scoped_to() {
        let effective = vec![
            ("InsertNewline".to_string(), "shift-enter".to_string()),
            ("NewTab".to_string(), "secondary-t".to_string()),
        ];
        // The newline is a terminal chord, and its default ships the fallback
        // beside it; both are scoped, so neither reaches the rest of the app.
        let mut newline = dispatched(&effective, "shift-enter", "Terminal");
        newline.sort_unstable();
        assert_eq!(
            newline,
            vec![
                InsertNewline::name_for_type(),
                InsertNewlineFallback::name_for_type(),
            ]
        );
        assert!(dispatched(&effective, "shift-enter", "Workspace").is_empty());
        assert_eq!(
            dispatched(&effective, "alt-enter", "Terminal"),
            vec![InsertNewline::name_for_type()],
            "the extra chord follows its action's scope"
        );
        // A window action has no context: it has to work from a terminal too.
        for context in ["Terminal", "Workspace"] {
            assert_eq!(
                dispatched(&effective, "secondary-t", context),
                vec![NewTab::name_for_type()],
                "NewTab must not be scoped to {context}"
            );
        }
    }

    #[test]
    fn action_context_matches_the_scope_make_binding_installs() {
        let extra_actions = extra_keystrokes(
            &default_bindings()
                .into_iter()
                .map(|(a, k)| (a.to_string(), k.to_string()))
                .collect::<Vec<_>>(),
        );
        let actions = default_bindings()
            .into_iter()
            .map(|(a, _)| a)
            .chain(extra_actions.into_iter().map(|(a, _)| a));
        for action in actions {
            let binding =
                make_binding(action, "f13").unwrap_or_else(|| panic!("no arm for {action}"));
            let installed = binding.predicate().map(|p| p.to_string());
            assert_eq!(
                installed.as_deref(),
                action_context(action),
                "{action} is installed in a different context than `action_context` reports"
            );
        }
    }

    #[test]
    fn no_default_binding_sits_on_a_terminal_control_code() {
        // The invariant is "no default may *swallow* a terminal control code".
        // The exceptions are named and justified in
        // `control_code_binding_allowed`; anything new needs a fall-through of
        // its own to join them.
        for (action, spec) in default_bindings() {
            // Both spellings, because `push_binding` installs both. "Window
            // actions belong on ctrl-shift-*" stops being an escape over the
            // digits and the punctuation the moment the fold clears the Shift
            // again: `ctrl-shift-2` goes into the keymap as `ctrl-@` (#750).
            let folded = folded_spec(spec);
            for spelling in [Some(spec), folded.as_deref()].into_iter().flatten() {
                for chord in spelling.split_whitespace() {
                    Keystroke::parse(chord).expect("default chords parse");
                    assert!(
                        !steals_a_control_code(chord)
                            || control_code_binding_allowed(action, chord),
                        "{action} is bound to {spec}, installed as {chord}, which the shell \
                         needs as a control code \
                         (Ctrl+[ is ESC, Ctrl+D is EOF, Ctrl+W deletes a word, \
                         Ctrl+2..8 are NUL/ESC/FS/GS/RS/US/DEL). \
                         Window actions belong on ctrl-shift-* off macOS — over a letter, \
                         where the platform keeps the Shift."
                    );
                }
            }
        }
    }

    /// F1..F12 now encode (#834), so a default sitting on one takes it away
    /// from the shell the same way a Ctrl binding takes a control code: gpui
    /// matches bindings before the pane's key handler runs, so a bound
    /// function key never reaches the PTY at all.
    ///
    /// The three that do are named here rather than left to be discovered,
    /// and each answers for the shell key it stands on:
    ///
    /// * F11 keeps fullscreen outright. It is the chord Windows Terminal,
    ///   GNOME Terminal and konsole all use, and no shell binds it —
    ///   PSReadLine's F keys are F3, F7 and F8.
    /// * F3 and Shift+F3 are Find Next / Previous, and they fall through:
    ///   with no find bar open the listener in `terminal::view` calls
    ///   `cx.propagate()`, so PSReadLine's CharacterSearch still gets the key.
    ///
    /// A fourth needs a fall-through of its own to join them.
    #[test]
    fn f11_is_the_only_default_on_a_bare_function_key() {
        let mut bound = Vec::new();
        for (action, spec) in default_bindings() {
            for chord in spec.split_whitespace() {
                let ks = Keystroke::parse(chord).expect("default chords parse");
                if crate::terminal::input::is_function_key(&ks.key) {
                    bound.push((action, chord));
                }
            }
        }
        let expected: &[(&str, &str)] = if cfg!(target_os = "macos") {
            &[]
        } else {
            &[
                ("FindNext", "f3"),
                ("FindPrevious", "shift-f3"),
                ("ToggleFullscreen", "f11"),
            ]
        };
        bound.sort();
        let mut expected = expected.to_vec();
        expected.sort();
        assert_eq!(
            bound, expected,
            "a default on a function key hides it from the shell; \
             PSReadLine wants F3, F7 and F8, readline's `bind -x` any of them"
        );
    }

    #[test]
    fn the_control_code_rule_knows_what_the_shell_needs() {
        // The keys with a C0 byte behind them, and the modifier shape that
        // reaches it: Ctrl alone. This is the predicate the defaults are held
        // to above and the one `action_bindings` warns on.
        for chord in [
            "ctrl-d",
            "ctrl-c",
            "ctrl-[",
            "ctrl-2",
            "ctrl-6",
            "ctrl-8",
            "ctrl-/",
            "ctrl-space",
        ] {
            assert!(steals_a_control_code(chord), "{chord} is a control code");
        }
        for chord in [
            "ctrl-shift-v",
            "ctrl-alt-v",
            "secondary-shift-t",
            "ctrl-1",
            "ctrl-9",
            "ctrl--",
            "ctrl-f3",
            "alt-enter",
        ] {
            assert!(
                !steals_a_control_code(chord),
                "{chord} carries no control code"
            );
        }
        // Ctrl+V is allowed to anyone off macOS — it is how the default paste
        // reaches the chord, and how a user moves the full-screen paste onto
        // it — while Ctrl+D stays refused whoever asks.
        assert_eq!(
            control_code_binding_allowed("PasteText", "ctrl-v"),
            cfg!(not(target_os = "macos"))
        );
        assert!(!control_code_binding_allowed("PasteText", "ctrl-d"));
    }

    #[test]
    fn every_default_chord_is_claimed_by_exactly_one_action() {
        // Per context, not globally: gpui resolves a keystroke by walking the
        // focus chain outwards, so a chord bound inside a narrow context and
        // again with no context is not a clash — the narrow one wins while
        // that element has focus and the global one applies everywhere else.
        // `ScmCommit` and `ToggleFullscreen` both take secondary-enter on that
        // basis. Two bindings sharing a chord *and* a context is still a bug,
        // because then which one fires is arbitrary.
        // `same_chord`, not string equality: `secondary-shift-]` and
        // `secondary-}` are two spellings of one keystroke and both install it.
        let mut seen: Vec<(&str, &str, Option<&'static str>)> = Vec::new();
        for (action, spec) in default_bindings() {
            if spec.is_empty() {
                continue;
            }
            let context = action_context(action);
            if let Some((other, _, _)) = seen
                .iter()
                .find(|(_, s, c)| same_chord(s, spec) && *c == context)
            {
                let shared_close = matches!(
                    (action, *other),
                    ("CloseWindow", "CloseActiveTab") | ("CloseActiveTab", "CloseWindow")
                );
                if !shared_close {
                    panic!("{action} and {other} both claim {spec} in context {context:?}");
                }
            }
            seen.push((action, spec, context));
        }
    }

    #[test]
    fn spec_from_keystroke_round_trips_through_parse() {
        for spec in [
            "secondary-t",
            "secondary-shift-t",
            "secondary-alt-left",
            "ctrl-shift-tab",
            "secondary--",
        ] {
            let ks = Keystroke::parse(spec).unwrap();
            let round = spec_from_keystroke(&ks).expect("real key produces a spec");
            let reparsed = Keystroke::parse(&round).unwrap();
            assert_eq!(
                (reparsed.modifiers, reparsed.key),
                (ks.modifiers, ks.key),
                "round trip diverged for {spec}"
            );
        }
    }

    #[test]
    fn a_shifted_punctuation_chord_dispatches_however_it_is_spelled() {
        // Recording ⌘⇧] wrote `secondary-}` — correctly, since that is the
        // keystroke it was handed — while the default table and every config
        // in the wild spell the same chord `secondary-shift-]`. Only one of
        // the two used to reach the dispatcher (#750); all three do now.
        let typed = delivered_with_secondary("}");
        let recorded = spec_from_keystroke(&typed).expect("the recorder spells the chord");
        assert_eq!(recorded, "secondary-}");
        for spec in [recorded.as_str(), "secondary-shift-]", "secondary-shift-}"] {
            let effective = [("NewWindow".to_string(), spec.to_string())];
            assert_eq!(
                dispatched_keystrokes(&effective, std::slice::from_ref(&typed), ""),
                vec![NewWindow::name_for_type()],
                "{spec} never reaches the dispatcher"
            );
        }
    }

    #[test]
    fn folding_leaves_every_other_spelling_alone() {
        // Shift over a letter is a real modifier, and the named keys have no
        // shifted half at all — folding either would break far more than it
        // fixed.
        for spec in [
            "secondary-shift-t",
            "shift-enter",
            "shift-insert",
            "shift-tab",
            "ctrl-shift-up",
            "secondary-t",
            "secondary--",
            "ctrl-b n",
        ] {
            assert_eq!(folded_spec(spec), None, "{spec} was rewritten");
        }
        // A prefix chord folds per chord, not as a whole.
        assert_eq!(folded_spec("ctrl-b shift-5").as_deref(), Some("ctrl-b %"));
    }

    #[test]
    fn a_chord_the_platform_delivers_with_shift_intact_stays_bound() {
        // Windows leaves Shift on the numpad's `/ * + -` — they are absent
        // from gpui's `need_to_convert_to_shifted_key`, so Ctrl+Shift+numpad-/
        // arrives as `ctrl-shift-/` and the recorder writes exactly that.
        // Folding is an addition, never a substitution, so that spec has to
        // survive the treatment `secondary-shift-]` gets (#750).
        let numpad = Keystroke {
            modifiers: gpui::Modifiers {
                control: cfg!(not(target_os = "macos")),
                platform: cfg!(target_os = "macos"),
                shift: true,
                alt: false,
                function: false,
            },
            key: "/".to_string(),
            key_char: None,
        };
        assert_eq!(
            spec_from_keystroke(&numpad).as_deref(),
            Some("secondary-shift-/")
        );
        let effective = [("NewWindow".to_string(), "secondary-shift-/".to_string())];
        assert_eq!(
            dispatched_keystrokes(&effective, std::slice::from_ref(&numpad), ""),
            vec![NewWindow::name_for_type()],
            "the numpad chord lost the binding written for it"
        );
        // And the main row's `?`, which the same spec also stands for, still
        // reaches the action — the fold added it rather than taking over.
        assert_eq!(
            dispatched_keystrokes(&effective, &[delivered_with_secondary("?")], ""),
            vec![NewWindow::name_for_type()],
        );
    }

    #[test]
    fn two_spellings_of_one_chord_are_recognised_as_one() {
        // What `assign_keybinding` leans on to displace the action already
        // holding a chord, whichever way either of them is written down.
        assert!(same_chord("secondary-shift-]", "secondary-}"));
        assert!(same_chord("secondary-}", "secondary-shift-]"));
        assert!(same_chord("secondary-shift-}", "secondary-shift-]"));
        assert!(same_chord("secondary-t", "secondary-t"));
        assert!(!same_chord("secondary-shift-]", "secondary-shift-["));
        assert!(!same_chord("secondary-}", "secondary-{"));
    }

    #[test]
    fn the_control_code_guard_follows_the_folded_chord() {
        // The half of the control-code rule that only warns. A chord carrying
        // Shift reads as safe on its own — `steals_a_control_code` recognises a
        // bare Ctrl and nothing else — and it *was* safe while nothing could
        // press it. The fold makes it live, and Ctrl+Shift+2 is delivered as
        // Ctrl+@, which is the NUL the program on the far end is waiting for.
        // So the guard is asked about the spelling that goes into the keymap
        // (#750); `action_bindings` runs exactly this pair.
        for (written, folded) in [
            ("ctrl-shift-2", per_platform("ctrl-@", "secondary-@")),
            ("ctrl-shift-6", per_platform("ctrl-^", "secondary-^")),
            ("ctrl-shift--", per_platform("ctrl-_", "secondary-_")),
            ("ctrl-shift-/", per_platform("ctrl-?", "secondary-?")),
        ] {
            assert!(
                !steals_a_control_code(written),
                "{written} reads as safe as written, which is why the fold has to be checked"
            );
            assert_eq!(folded_spec(written).as_deref(), Some(folded));
            assert!(
                steals_a_control_code(folded),
                "{written} is installed as {folded} and takes a control code"
            );
        }
    }

    #[test]
    fn a_key_the_us_table_does_not_know_is_left_alone() {
        // `SHIFTED_GLYPHS` is a US keyboard, and every other layout prints
        // something else on the shifted half: `?` is Shift+ß on a German
        // keyboard and Shift+, on a French one. The fold cannot know that, so
        // it declines rather than guesses — a key it has never seen keeps its
        // Shift, and the binding written for it stays exactly as written. That
        // is also what keeps the fold additive: it can widen a spec's reach but
        // never move it onto a chord the user did not ask for.
        for spec in [
            "secondary-shift-ß",
            "secondary-shift-é",
            "secondary-shift-ä",
            "secondary-shift-ç",
            "secondary-shift-ñ",
        ] {
            assert_eq!(folded_spec(spec), None, "{spec} was guessed at");
        }
        // And the recorded spelling, which is what a non-US layout actually
        // produces, needs no fold to reach the dispatcher in the first place.
        let effective = [("NewWindow".to_string(), "secondary-?".to_string())];
        assert_eq!(
            dispatched_keystrokes(&effective, &[delivered_with_secondary("?")], ""),
            vec![NewWindow::name_for_type()],
        );
    }

    #[test]
    fn spec_from_keystroke_ignores_a_lone_modifier() {
        let ks = Keystroke::parse("secondary").unwrap();
        assert_eq!(spec_from_keystroke(&ks), None);
    }

    #[test]
    fn open_config_file_is_secondary_comma() {
        let effective: Vec<(String, String)> = default_bindings()
            .into_iter()
            .map(|(a, k)| (a.to_string(), k.to_string()))
            .collect();
        assert_eq!(
            dispatched(&effective, "secondary-,", "Terminal"),
            vec![OpenConfigFile::name_for_type()]
        );
        assert_eq!(
            dispatched(&effective, "secondary-,", "Workspace"),
            vec![OpenConfigFile::name_for_type()]
        );
    }

    #[test]
    fn new_window_ships_a_chord_only_where_one_is_free() {
        let effective: Vec<(String, String)> = default_bindings()
            .into_iter()
            .map(|(a, k)| (a.to_string(), k.to_string()))
            .collect();
        let default = effective
            .iter()
            .find(|(action, _)| action == "NewWindow")
            .map(|(_, key)| key.clone())
            .expect("NewWindow is listed");
        let expected = per_platform("secondary-n", "secondary-shift-n");
        assert_eq!(default, expected);
        assert_eq!(
            dispatched(&effective, &default, "Terminal"),
            vec![NewWindow::name_for_type()]
        );
    }

    #[test]
    fn fullscreen_is_distinct_from_insert_newline() {
        let defaults = default_bindings();
        let key_of = |action: &str| {
            defaults
                .iter()
                .find(|(a, _)| *a == action)
                .map(|(_, k)| *k)
                .unwrap()
        };
        assert_eq!(
            key_of("ToggleFullscreen"),
            per_platform("secondary-ctrl-f", "f11")
        );
        assert_ne!(key_of("ToggleFullscreen"), INSERT_NEWLINE_DEFAULT);
        assert_ne!(key_of("InsertNewline"), "secondary-ctrl-f");
    }
}

#[cfg(test)]
mod gpui_tests {
    use super::*;
    use crate::core::config::Config;
    use gpui::TestAppContext;

    #[gpui::test]
    fn the_keybinding_triple_only_moves_when_a_binding_does(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
            init(cx);

            let before = keybinding_config(cx);
            assert_eq!(
                keybinding_config(cx),
                before,
                "reading the config twice changes nothing"
            );

            // An unrelated edit leaves the triple alone — this is the gate the
            // watcher compares, and a preference save must not rebind.
            cx.global_mut::<Config>().copy_on_select = true;
            assert_eq!(keybinding_config(cx), before);

            // A real binding edit moves it.
            cx.global_mut::<Config>().keybindings.insert(
                "NewWindow".to_string(),
                KeybindingOverride::Add("ctrl-shift-r".to_string()),
            );
            assert_ne!(keybinding_config(cx), before);

            // So does the preset, and the prefix.
            let with_override = keybinding_config(cx);
            cx.global_mut::<Config>().keybinding_preset = "tmux".to_string();
            assert_ne!(keybinding_config(cx), with_override);
            let with_preset = keybinding_config(cx);
            cx.global_mut::<Config>().prefix = "ctrl-a".to_string();
            assert_ne!(keybinding_config(cx), with_preset);
        });
    }

    /// An app whose `config.json` reads `json`, keymap and all. Parsed from
    /// text rather than built in Rust, because the file is the surface #868 is
    /// about: what a hand-written line means is the thing under test.
    fn running_on_json(cx: &mut gpui::App, json: &str) {
        gpui_component::init(cx);
        cx.set_global(Config(
            serde_json::from_str(json).expect("the config parses"),
        ));
        init(cx);
    }

    /// What the live keymap dispatches for `keys` typed in a terminal, best
    /// match first — the first entry is the action a real keypress runs.
    fn fired(cx: &gpui::App, keys: &str) -> Vec<&'static str> {
        let input: Vec<Keystroke> = keys
            .split(' ')
            .map(|k| Keystroke::parse(k).expect("the typed keystroke parses"))
            .collect();
        let context = [gpui::KeyContext::parse("Terminal").expect("the context parses")];
        cx.key_bindings()
            .borrow()
            .bindings_for_input(&input, &context)
            .0
            .iter()
            .map(|b| b.action().name())
            .collect()
    }

    #[gpui::test]
    fn a_chord_the_user_adds_wins_over_a_default_already_on_it(cx: &mut TestAppContext) {
        use gpui::Action as _;
        cx.update(|cx| {
            // `ctrl-tab` is `NextTab`'s default, and `NewTab` sits above it in the
            // table. The keymap resolves a tie to the binding added last, so a
            // user chord laid down in table order lost to the default whenever
            // its action happened to come first — the line in config.json was
            // read and did nothing.
            running_on_json(cx, r#"{"keybindings": {"NewTab": "ctrl-tab"}}"#);
            assert_eq!(
                fired(cx, "ctrl-tab").first(),
                Some(&NewTab::name_for_type()),
                "the chord the user asked for is the one that runs"
            );
            assert_eq!(
                fired(cx, per_platform("secondary-t", "secondary-shift-t")).first(),
                Some(&NewTab::name_for_type()),
                "and NewTab keeps its own default"
            );
        });
    }

    #[gpui::test]
    fn repeated_rebinds_do_not_grow_the_keymap(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
            init(cx);
            let size = cx.key_bindings().borrow().bindings().len();
            // A rebind that clears first stays the same size however many
            // times it runs; the append-only one this replaced grew by a full
            // table each call, and every keystroke walks the whole map (#548).
            for _ in 0..3 {
                rebind(cx);
            }
            assert_eq!(
                cx.key_bindings().borrow().bindings().len(),
                size,
                "rebind clears before it binds, so the map does not grow"
            );
        });
    }

    #[gpui::test]
    fn a_rebuild_keeps_the_bindings_tty7_did_not_install(cx: &mut TestAppContext) {
        cx.update(|cx| {
            // gpui-component binds the whole `Input` editing table — backspace
            // among it — from its own `init`, which runs once, before ours. A
            // rebuild replaces the map, so it has to carry them; without this
            // the first rebind left every text field in the app unable to
            // delete a character (#548).
            gpui_component::init(cx);
            cx.set_global(Config::default());

            let backspace = |cx: &App| {
                let typed = [Keystroke::parse("backspace").unwrap()];
                cx.key_bindings()
                    .borrow()
                    .all_bindings_for_input(&typed)
                    .iter()
                    .map(|b| b.action().name().to_string())
                    .collect::<Vec<_>>()
            };

            let inherited = backspace(cx);
            assert!(
                !inherited.is_empty(),
                "gpui-component binds backspace before tty7 touches the keymap"
            );

            init(cx);
            assert_eq!(backspace(cx), inherited, "init must not drop them");
            rebind(cx);
            assert_eq!(backspace(cx), inherited, "nor may a rebind");
        });
    }

    #[gpui::test]
    fn init_then_rebind_installs_the_merged_table(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
            init(cx);

            {
                let cfg = cx.global_mut::<Config>();
                cfg.keybinding_preset = "tmux".to_string();
                cfg.keybindings.insert(
                    "NewTab".to_string(),
                    KeybindingOverride::Add("secondary-shift-n".to_string()),
                );
            }
            rebind(cx);

            let eff = effective_bindings(cx);
            let keys_of = |action: &str| {
                eff.iter()
                    .filter(|(a, _)| a == action)
                    .map(|(_, k)| k.clone())
                    .collect::<Vec<_>>()
            };
            assert_eq!(keys_of("NewTab"), ["ctrl-b c", "secondary-shift-n"]);
            assert_eq!(keys_of("CloseActiveTab"), ["ctrl-b x"]);

            cx.global_mut::<Config>().keybinding_preset = "default".to_string();
            rebind(cx);
            let eff = effective_bindings(cx);
            let keys_of = |action: &str| {
                eff.iter()
                    .filter(|(a, _)| a == action)
                    .map(|(_, k)| k.clone())
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                keys_of("CloseActiveTab"),
                [per_platform("secondary-w", "secondary-shift-w")]
            );
        });
    }

    #[gpui::test]
    fn a_chord_added_in_config_keeps_the_default_one(cx: &mut TestAppContext) {
        use gpui::Action as _;
        cx.update(|cx| {
            running_on_json(cx, r#"{"keybindings": {"NewTab": "ctrl-alt-t"}}"#);
            let shipped = per_platform("secondary-t", "secondary-shift-t");
            assert_eq!(
                fired(cx, shipped).first(),
                Some(&NewTab::name_for_type()),
                "the default chord survives a chord added beside it"
            );
            assert_eq!(
                fired(cx, "ctrl-alt-t").first(),
                Some(&NewTab::name_for_type()),
                "and the added chord works too"
            );
            assert_eq!(effective_key("NewTab", cx).as_deref(), Some(shipped));
        });
    }

    #[gpui::test]
    fn an_empty_chord_still_unbinds_the_action(cx: &mut TestAppContext) {
        cx.update(|cx| {
            running_on_json(cx, r#"{"keybindings": {"NewTab": ""}}"#);
            let shipped = per_platform("secondary-t", "secondary-shift-t");
            assert!(fired(cx, shipped).is_empty());
            assert_eq!(effective_key("NewTab", cx), None);
        });
    }

    #[gpui::test]
    fn a_list_is_the_whole_set_of_chords_for_an_action(cx: &mut TestAppContext) {
        use gpui::Action as _;
        cx.update(|cx| {
            running_on_json(
                cx,
                r#"{"keybindings": {"NewTab": ["ctrl-alt-t", "ctrl-alt-n"]}}"#,
            );
            let shipped = per_platform("secondary-t", "secondary-shift-t");
            assert!(
                fired(cx, shipped).is_empty(),
                "a list replaces the default rather than joining it"
            );
            for chord in ["ctrl-alt-t", "ctrl-alt-n"] {
                assert_eq!(
                    fired(cx, chord).first(),
                    Some(&NewTab::name_for_type()),
                    "{chord} is in the list"
                );
            }
        });
    }

    #[gpui::test]
    fn an_empty_list_unbinds_the_action(cx: &mut TestAppContext) {
        cx.update(|cx| {
            running_on_json(cx, r#"{"keybindings": {"NewTab": []}}"#);
            let shipped = per_platform("secondary-t", "secondary-shift-t");
            assert!(fired(cx, shipped).is_empty());
            assert_eq!(effective_key("NewTab", cx), None);
        });
    }

    #[gpui::test]
    fn a_chord_added_under_the_tmux_preset_joins_the_preset_chord(cx: &mut TestAppContext) {
        use gpui::Action as _;
        cx.update(|cx| {
            running_on_json(
                cx,
                r#"{"keybinding_preset": "tmux",
                    "keybindings": {"NewTab": "ctrl-alt-t", "CloseActiveTab": ["ctrl-alt-x"]}}"#,
            );
            assert!(fired(cx, per_platform("secondary-t", "secondary-shift-t")).is_empty());
            assert_eq!(
                fired(cx, "ctrl-b c").first(),
                Some(&NewTab::name_for_type())
            );
            assert_eq!(
                fired(cx, "ctrl-alt-t").first(),
                Some(&NewTab::name_for_type())
            );
            assert!(fired(cx, "ctrl-b x").is_empty());
            assert_eq!(
                fired(cx, "ctrl-alt-x").first(),
                Some(&CloseActiveTab::name_for_type())
            );
        });
    }

    #[gpui::test]
    fn the_close_chord_closes_the_window(cx: &mut TestAppContext) {
        use gpui::Action as _;
        cx.update(|cx| {
            running_on_json(cx, "{}");
            let chord = per_platform("secondary-w", "secondary-shift-w");
            assert_eq!(
                fired(cx, chord).first().copied(),
                Some(CloseWindow::name_for_type())
            );
        });
    }
}
