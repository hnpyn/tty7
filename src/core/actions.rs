use gpui::Action;
use gpui::actions;
use schemars::JsonSchema;
use serde::Deserialize;

actions!(
    tty7,
    [
        NewTab,
        NewWindow,
        CloseWindow,
        CloseActiveTab,
        OpenLinkUnderPointer,
        OpenLinkWithDefaultApp,
        RevealLinkUnderPointer,
        CopyLinkPathUnderPointer,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
        ToggleFullscreen,
        HideApp,
        HideOthers,
        ShowAll,
        MinimizeWindow,
        ZoomWindow,
        SendTab,
        SendBackTab,
        Quit,
        ToggleThemeFollowSystem,
        OpenConfigFile,
    ]
);

/// Switch the live theme to the preset `id`.
///
/// A menu row carries the id it paints. The keymap does not: a chord cannot
/// name which theme it means, so this stays off the default binding table.
#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Action)]
#[action(namespace = tty7)]
#[serde(deny_unknown_fields)]
pub struct SelectTheme {
    pub id: String,
}
