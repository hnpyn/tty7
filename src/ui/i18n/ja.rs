use super::L10nKey;

pub fn translate_ja(key: L10nKey) -> Option<&'static str> {
    Some(match key {
        L10nKey::SearchFind => "検索…",
        L10nKey::SearchMatchCase => "大文字と小文字を区別",
        L10nKey::SearchUseRegex => "正規表現を使用",
        L10nKey::Close => "閉じる",
        L10nKey::CloseBusyCommandBody => "{what} はまだ実行中です。閉じると終了します。",
        L10nKey::CloseBusyAgentBody => {
            "{agent} はまだ作業中です。閉じるとこのターンは中断されます。"
        }
        L10nKey::CloseIdleBody => "中のシェルも終了します。",
        L10nKey::Keep => "保持",
        L10nKey::LinkFileNotUnder => "{path} — {dir} にそのファイルはありません",
        L10nKey::LinkFileNoDirectory => {
            "{path} — このペインはどのディレクトリにいるかを報告していないため、相対パスの起点がありません"
        }
        L10nKey::LinkFileMissing => "{path} — そのパスには何もありません",
        L10nKey::ProgramNotesDropped => "このペインのほかの通知は表示されませんでした",
        L10nKey::LinkFileOpenFailed => "{path} を開けませんでした: {error}",
        L10nKey::AppMenuServices => "サービス",
        L10nKey::AppMenuHideApp => "ntty7 を非表示",
        L10nKey::AppMenuHideOthers => "ほかを非表示",
        L10nKey::AppMenuShowAll => "すべて表示",
        L10nKey::AppMenuQuit => "ntty7 を終了",
        L10nKey::AppMenuFile => "ファイル",
        L10nKey::AppMenuEdit => "編集",
        L10nKey::AppMenuView => "表示",
        L10nKey::AppMenuTheme => "テーマ",
        L10nKey::AppMenuFollowSystemAppearance => "システムの外観に合わせる",
        L10nKey::AppMenuOpenConfigFile => "設定ファイルを開く",
        L10nKey::AppMenuWindow => "ウィンドウ",
        L10nKey::AppMenuUndo => "元に戻す",
        L10nKey::AppMenuRedo => "やり直す",
        L10nKey::AppMenuCut => "切り取り",
        L10nKey::AppMenuCopy => "コピー",
        L10nKey::AppMenuPaste => "貼り付け",
        L10nKey::AppMenuSelectAll => "すべて選択",
        L10nKey::AppMenuFind => "検索…",
        L10nKey::AppMenuFindNext => "次を検索",
        L10nKey::AppMenuFindPrevious => "前を検索",
        L10nKey::AppMenuIncreaseFontSize => "フォントサイズを拡大",
        L10nKey::AppMenuDecreaseFontSize => "フォントサイズを縮小",
        L10nKey::AppMenuResetFontSize => "フォントサイズをリセット",
        L10nKey::AppMenuClearScrollback => "スクロールバックをクリア",
        L10nKey::AppMenuOpenLink => "開く",
        L10nKey::AppMenuOpenLinkWithDefaultApp => "既定のアプリで開く",
        L10nKey::AppMenuRevealInFinder => "Finder に表示",
        L10nKey::AppMenuRevealInFolder => "含まれるフォルダーを開く",
        L10nKey::AppMenuCopyLinkPath => "パスをコピー",
        L10nKey::NotifyCommandFinished => "コマンドが {secs} 秒で完了しました",
        L10nKey::NotifyCommandFinishedWithCommand => "{command} — {secs} 秒で完了しました",
        L10nKey::NotifyAgentFinished => "{secs} 秒で完了しました",
        L10nKey::NotifyAgentWaiting => "入力を待っています",
        L10nKey::NotifyTurnFinished => "ターンが完了しました",
        L10nKey::TerminalContextClear => "クリア",
        L10nKey::CmdNewWindow => "新しいウィンドウ",
        L10nKey::CmdCloseWindow => "ウィンドウを閉じる",
        L10nKey::ComposerPlaceholder => "{agent} にメッセージ   ·   @ ファイル   / コマンド",
        L10nKey::ComposerPlaceholderFiles => "{agent} にメッセージ   ·   @ ファイル",
        L10nKey::ComposerSendTip => "送信（Enter）  ·  Shift+Enter で改行",
        L10nKey::ComposerAttach => "ファイルや画像を添付",
        L10nKey::ComposerMenuCommands => "コマンド",
        L10nKey::ComposerMenuFiles => "ファイル",
        L10nKey::ComposerCmdProject => "プロジェクトのコマンド",
        L10nKey::ComposerCmdUser => "ユーザーのコマンド",
        L10nKey::ComposerAgentAsking => {
            "{agent} が質問しています。ターミナルで答えてから送信してください。"
        }
        L10nKey::ComposerModeDefault => "編集前に確認",
        L10nKey::ComposerModeAcceptEdits => "編集を自動承認",
        L10nKey::ComposerModePlan => "プランモード",
        L10nKey::ComposerModeBypass => "権限確認をスキップ",
        L10nKey::ComposerModeAuto => "自動モード",
        L10nKey::ComposerModeTip => "権限モード  ·  Shift+Tab で切り替え",
        L10nKey::ComposerModel => "モデル",
        L10nKey::ComposerModelTip => "モデルを切り替え",
        L10nKey::ComposerModelDefault => "デフォルト（推奨）",
        L10nKey::ComposerEffort => "推論の強さ",
        L10nKey::ComposerEffortTip => "推論の強さ  ·  クリックして変更",
        L10nKey::AppMenuMinimize => "最小化",
        L10nKey::AppMenuZoom => "ズーム",
        L10nKey::IntegrationNoticeBlocked => {
            "“{wrapper}”がこのペインのシェルレポートを横取りしているため、インライン補完と Ctrl+R メニューは使えません。シェル独自の履歴検索は引き続き使えます。"
        }
        L10nKey::IntegrationNoticeNotEngaged => {
            "このペインでは tty7 シェル統合が有効になっておらず、インライン補完と Ctrl+R メニューは使えません。よくある原因は、独自の引数で起動したシェル、PTY ラッパー、未対応のシェルです。"
        }
        L10nKey::PaneTitleDisconnected => "{title} — 切断されました",
        L10nKey::PaneTitleProcessExited => "{title} — プロセスが終了しました",
        L10nKey::CompletionListingRemote => "リモートを一覧しています…",
        L10nKey::ShellFellBack => {
            "設定したシェル {shell} の起動に失敗したため、ログインシェルを使います。"
        }
        L10nKey::ShellCouldNotStart => "ターミナルを起動できませんでした: {error}",
        L10nKey::ShellCouldNotStartTitle => "ntty7 はターミナルを起動できませんでした",
    })
}
