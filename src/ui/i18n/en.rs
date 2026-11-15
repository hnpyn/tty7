use super::L10nKey;

pub fn translate_en(key: L10nKey) -> &'static str {
    match key {
        L10nKey::SearchFind => "Find…",
        L10nKey::SearchMatchCase => "Match case",
        L10nKey::SearchUseRegex => "Use regular expression",
        L10nKey::Close => "Close",
        L10nKey::CloseBusyCommandBody => "{what} is still running. Closing ends it.",
        L10nKey::CloseBusyAgentBody => "{agent} is still working. Closing ends its turn.",
        L10nKey::CloseIdleBody => "Its shell ends with it.",
        L10nKey::Keep => "Keep",
        L10nKey::LinkFileNotUnder => "{path} — nothing by that name under {dir}",
        L10nKey::LinkFileNoDirectory => {
            "{path} — this pane has not said which directory it is in, so a relative path has no base"
        }
        L10nKey::LinkFileMissing => "{path} — nothing at that path",
        L10nKey::ProgramNotesDropped => "More notifications from this pane weren't shown",
        L10nKey::LinkFileOpenFailed => "Could not open {path}: {error}",
        L10nKey::AppMenuServices => "Services",
        L10nKey::AppMenuHideApp => "Hide ntty7",
        L10nKey::AppMenuHideOthers => "Hide Others",
        L10nKey::AppMenuShowAll => "Show All",
        L10nKey::AppMenuQuit => "Quit ntty7",
        L10nKey::AppMenuFile => "File",
        L10nKey::AppMenuEdit => "Edit",
        L10nKey::AppMenuView => "View",
        L10nKey::AppMenuTheme => "Theme",
        L10nKey::AppMenuFollowSystemAppearance => "Follow System Appearance",
        L10nKey::AppMenuOpenConfigFile => "Open Config File",
        L10nKey::AppMenuWindow => "Window",
        L10nKey::AppMenuUndo => "Undo",
        L10nKey::AppMenuRedo => "Redo",
        L10nKey::AppMenuCut => "Cut",
        L10nKey::AppMenuCopy => "Copy",
        L10nKey::AppMenuPaste => "Paste",
        L10nKey::AppMenuSelectAll => "Select All",
        L10nKey::AppMenuFind => "Find…",
        L10nKey::AppMenuFindNext => "Find Next",
        L10nKey::AppMenuFindPrevious => "Find Previous",
        L10nKey::AppMenuIncreaseFontSize => "Increase Font Size",
        L10nKey::AppMenuDecreaseFontSize => "Decrease Font Size",
        L10nKey::AppMenuResetFontSize => "Reset Font Size",
        L10nKey::AppMenuClearScrollback => "Clear Scrollback",
        L10nKey::AppMenuOpenLink => "Open",
        L10nKey::AppMenuOpenLinkWithDefaultApp => "Open with Default App",
        L10nKey::AppMenuRevealInFinder => "Show in Finder",
        L10nKey::AppMenuRevealInFolder => "Show Containing Folder",
        L10nKey::AppMenuCopyLinkPath => "Copy Path",
        L10nKey::NotifyCommandFinished => "Command finished after {secs}s",
        L10nKey::NotifyCommandFinishedWithCommand => "{command} — finished after {secs}s",
        L10nKey::NotifyAgentFinished => "Finished after {secs}s",
        L10nKey::NotifyAgentWaiting => "Waiting for your input",
        L10nKey::NotifyTurnFinished => "Turn finished",
        L10nKey::TerminalContextClear => "Clear",
        L10nKey::CmdNewWindow => "New Window",
        L10nKey::CmdCloseWindow => "Close Window",
        L10nKey::ComposerPlaceholder => "Message {agent}   ·   @ files   / commands",
        L10nKey::ComposerPlaceholderFiles => "Message {agent}   ·   @ files",
        L10nKey::ComposerSendTip => "Send (Enter)  ·  Shift+Enter for a new line",
        L10nKey::ComposerAttach => "Attach files or images",
        L10nKey::ComposerMenuCommands => "Commands",
        L10nKey::ComposerMenuFiles => "Files",
        L10nKey::ComposerCmdProject => "Project command",
        L10nKey::ComposerCmdUser => "User command",
        L10nKey::ComposerAgentAsking => {
            "{agent} is asking something. Answer it in the terminal, then send."
        }
        L10nKey::ComposerModeDefault => "Ask before edits",
        L10nKey::ComposerModeAcceptEdits => "Accept edits",
        L10nKey::ComposerModePlan => "Plan mode",
        L10nKey::ComposerModeBypass => "Bypass permissions",
        L10nKey::ComposerModeAuto => "Auto mode",
        L10nKey::ComposerModeTip => "Permission mode  ·  Shift+Tab to cycle",
        L10nKey::ComposerModel => "Model",
        L10nKey::ComposerModelTip => "Switch model",
        L10nKey::ComposerModelDefault => "Default (recommended)",
        L10nKey::ComposerEffort => "Effort",
        L10nKey::ComposerEffortTip => "Reasoning effort  ·  click to change",
        L10nKey::AppMenuMinimize => "Minimize",
        L10nKey::AppMenuZoom => "Zoom",
        L10nKey::PaneRestoredScreenBanner => {
            "restored screen — this shell is new, nothing above it is still running"
        }
        L10nKey::IntegrationNoticeBlocked => {
            "\u{201c}{wrapper}\u{201d} is intercepting shell reports in this pane, so inline completion and the Ctrl+R menu are unavailable. The shell's own history search still works."
        }
        L10nKey::IntegrationNoticeNotEngaged => {
            "tty7 shell integration hasn't engaged in this pane, so inline completion and the Ctrl+R menu are unavailable. Usual causes: a shell you started with your own arguments, a PTY wrapper, or an unsupported shell."
        }
        L10nKey::PaneTitleDisconnected => "{title} — disconnected",
        L10nKey::PaneTitleProcessExited => "{title} — process exited",
        L10nKey::CompletionListingRemote => "listing remote…",
        L10nKey::ShellFellBack => {
            "Configured shell {shell} failed to start. Using the login shell."
        }
        L10nKey::ShellCouldNotStart => "Could not start a terminal: {error}",
        L10nKey::ShellCouldNotStartTitle => "ntty7 could not start a terminal",
    }
}
