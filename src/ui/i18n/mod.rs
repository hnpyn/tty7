use std::sync::atomic::{AtomicU8, Ordering};

mod en;
mod ja;
mod ru;
mod zh;

use en::translate_en;
use ja::translate_ja;
use ru::translate_ru;
use zh::translate_zh;

pub struct LanguageInfo {
    pub code: &'static str,
    pub translate_fn: fn(L10nKey) -> Option<&'static str>,
}

pub const SUPPORTED_LANGUAGES: &[LanguageInfo] = &[
    LanguageInfo {
        code: "en",
        translate_fn: |k| Some(translate_en(k)),
    },
    LanguageInfo {
        code: "zh-CN",
        translate_fn: translate_zh,
    },
    LanguageInfo {
        code: "ja-JP",
        translate_fn: translate_ja,
    },
    LanguageInfo {
        code: "ru-RU",
        translate_fn: translate_ru,
    },
];

#[cfg(test)]
pub fn default_language_code() -> &'static str {
    SUPPORTED_LANGUAGES[0].code
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

#[cfg(test)]
thread_local! {
    static TEST_LOCALE: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
}

/// One list of every string the window still asks for. The four locale tables
/// match it exhaustively, so a key added here without a translation does not
/// compile.
macro_rules! l10n_keys {
    ($($key:ident),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum L10nKey { $($key),* }

        impl L10nKey {
            #[cfg(test)]
            pub(crate) const ALL: &'static [L10nKey] = &[$(L10nKey::$key),*];
        }
    };
}

l10n_keys! {
    SearchFind,
    SearchMatchCase,
    SearchUseRegex,
    Close,
    CloseBusyCommandBody,
    CloseBusyAgentBody,
    CloseIdleBody,
    Keep,
    LinkFileNotUnder,
    LinkFileNoDirectory,
    LinkFileMissing,
    ProgramNotesDropped,
    LinkFileOpenFailed,
    AppMenuServices,
    AppMenuHideApp,
    AppMenuHideOthers,
    AppMenuShowAll,
    AppMenuQuit,
    AppMenuFile,
    AppMenuEdit,
    AppMenuView,
    AppMenuTheme,
    AppMenuFollowSystemAppearance,
    AppMenuOpenConfigFile,
    AppMenuWindow,
    AppMenuUndo,
    AppMenuRedo,
    AppMenuCut,
    AppMenuCopy,
    AppMenuPaste,
    AppMenuSelectAll,
    AppMenuFind,
    AppMenuFindNext,
    AppMenuFindPrevious,
    AppMenuIncreaseFontSize,
    AppMenuDecreaseFontSize,
    AppMenuResetFontSize,
    AppMenuClearScrollback,
    AppMenuOpenLink,
    AppMenuOpenLinkWithDefaultApp,
    AppMenuRevealInFinder,
    AppMenuRevealInFolder,
    AppMenuCopyLinkPath,
    NotifyCommandFinished,
    NotifyCommandFinishedWithCommand,
    NotifyAgentFinished,
    NotifyAgentWaiting,
    NotifyTurnFinished,
    TerminalContextClear,
    CmdNewWindow,
    CmdCloseWindow,
    ComposerPlaceholder,
    ComposerPlaceholderFiles,
    ComposerSendTip,
    ComposerAttach,
    ComposerMenuCommands,
    ComposerMenuFiles,
    ComposerCmdProject,
    ComposerCmdUser,
    ComposerAgentAsking,
    ComposerModeDefault,
    ComposerModeAcceptEdits,
    ComposerModePlan,
    ComposerModeBypass,
    ComposerModeAuto,
    ComposerModeTip,
    ComposerModel,
    ComposerModelTip,
    ComposerModelDefault,
    ComposerEffort,
    ComposerEffortTip,
    AppMenuMinimize,
    AppMenuZoom,
    PaneRestoredScreenBanner,
    IntegrationNoticeBlocked,
    IntegrationNoticeNotEngaged,
    PaneTitleDisconnected,
    PaneTitleProcessExited,
    CompletionListingRemote,
    ShellFellBack,
    ShellCouldNotStart,
    ShellCouldNotStartTitle,
}

pub fn set_locale(gui_language: &str) {
    let index = SUPPORTED_LANGUAGES
        .iter()
        .position(|lang| lang.code == gui_language)
        .unwrap_or(0) as u8;
    #[cfg(test)]
    TEST_LOCALE.with(|slot| slot.set(Some(index)));
    #[cfg(not(test))]
    CURRENT.store(index, Ordering::Relaxed);
}

pub fn t(key: L10nKey) -> &'static str {
    translate(current_locale_index(), key)
}

pub fn t_fmt(key: L10nKey, args: &[(&str, &str)]) -> String {
    let mut text = t(key).to_string();
    for (name, value) in args {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

fn current_locale_index() -> usize {
    #[cfg(test)]
    if let Some(idx) = TEST_LOCALE.with(|slot| slot.get()) {
        return idx as usize;
    }
    CURRENT.load(Ordering::Relaxed) as usize
}

fn translate(locale_idx: usize, key: L10nKey) -> &'static str {
    if let Some(lang) = SUPPORTED_LANGUAGES.get(locale_idx)
        && let Some(text) = (lang.translate_fn)(key)
    {
        return text;
    }
    translate_en(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_is_translated_in_every_locale() {
        // Deliberately the same in every locale: English is the right answer,
        // not a string nobody translated.
        const KEPT_IN_ENGLISH: &[L10nKey] = &[];

        for &key in KEPT_IN_ENGLISH {
            let en = translate_en(key);
            let zh = translate_zh(key).unwrap();
            let ja = translate_ja(key).unwrap();
            let ru = translate_ru(key).unwrap();
            if zh != en && ja != en && ru != en {
                println!("STALE ALLOWLIST ENTRY: {key:?} en={en:?} zh={zh:?} ja={ja:?}");
            }
        }
        for &key in L10nKey::ALL {
            let en = translate_en(key);
            assert!(!en.is_empty(), "missing en translation for {key:?}");
            for (name, text) in [
                ("zh", translate_zh(key)),
                ("ja", translate_ja(key)),
                ("ru", translate_ru(key)),
            ] {
                let text = text.unwrap_or_else(|| panic!("missing {name} translation for {key:?}"));
                assert!(!text.is_empty(), "empty {name} translation for {key:?}");
                assert!(
                    text != en || KEPT_IN_ENGLISH.contains(&key),
                    "{name} still reads the English for {key:?}: {en:?}"
                );
            }
        }
    }

    #[test]
    fn explicit_languages_select_the_right_locale() {
        set_locale("zh-CN");
        assert_eq!(current_locale_index(), 1);
        assert_eq!(t(L10nKey::Close), "关闭");
        set_locale("ja-JP");
        assert_eq!(current_locale_index(), 2);
        assert_eq!(t(L10nKey::Close), "閉じる");
        set_locale("ru-RU");
        assert_eq!(current_locale_index(), 3);
        assert_eq!(t(L10nKey::Close), "Закрыть");
        set_locale("en");
        assert_eq!(current_locale_index(), 0);
        assert_eq!(t(L10nKey::Close), "Close");
        set_locale("ko");
        assert_eq!(current_locale_index(), 0);
        set_locale(default_language_code());
    }

    #[test]
    fn templates_fill_named_placeholders() {
        set_locale("en");
        let text = t_fmt(L10nKey::NotifyCommandFinished, &[("secs", "12")]);
        assert!(text.contains("12"), "{text}");
        assert!(!text.contains("{secs}"), "{text}");
        set_locale(default_language_code());
    }
}
