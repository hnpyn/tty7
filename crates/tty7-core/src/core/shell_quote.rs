//! One set of rules for putting a real path onto a command line, shared by
//! everything that inserts one.
//!
//! Three places need this: the file tree's `cd` and paste, the terminal's
//! drop / clipboard / staged-image insertion, and completion accepting a
//! candidate. They used to carry three separate implementations, and on
//! Windows two of them disagreed — `file_tree::shell_quote_for` wrapped the
//! path in quotes and worked, while `view::shell_escape_path` escaped with
//! backslashes and ate the path separators of `C:\Users\me` (#593 fixed the
//! first one and never reached the other two).
//!
//! Three shells, three rules:
//!
//! - cmd.exe treats only double quotes as quoting. A single quote is an
//!   ordinary character there, so the POSIX form splits the path at its first
//!   space. Windows paths cannot contain `"`, so there is nothing to escape
//!   inside the quotes.
//! - PowerShell takes `'...'`, and writes an embedded `'` twice. The POSIX
//!   `'\''` seam is not a seam there — PowerShell does not join a quoted
//!   string to the bare word beside it — so `C:\Users\O'Brien` came out as
//!   something PowerShell reads as three tokens.
//! - Every POSIX shell takes `'...'` too, and breaks out for an embedded `'`
//!   via `'\''`.
//!
//! Backslash escaping is not used to quote anywhere. Only POSIX shells
//! understand it, and on Windows it collides head-on with the path separator.
//!
//! A leading `~/` stays outside the quotes: quoting it would make it a literal
//! and lose the home expansion the user is asking for.

/// Characters that need no quoting in any shell we target.
fn is_bare(c: char) -> bool {
    c.is_alphanumeric() || "/.-_~+".contains(c)
}

/// How a shell wants a literal string written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quoting {
    /// cmd.exe: `"..."`, and a Windows path cannot hold a `"` to escape.
    Cmd,
    /// PowerShell and pwsh: `'...'`, with an embedded `'` written `''`.
    PowerShell,
    /// Everything else: `'...'`, with an embedded `'` written `'\''`.
    Posix,
}

/// The shell binary's name, without a directory and without a `.exe` suffix.
fn base_name(program: &str) -> &str {
    let base = program.rsplit(['\\', '/']).next().unwrap_or(program);
    let cut = base.len().saturating_sub(4);
    match base.get(cut..) {
        Some(tail) if tail.eq_ignore_ascii_case(".exe") => &base[..cut],
        _ => base,
    }
}

/// Which dialect the pane's shell speaks.
///
/// `shell_program` is the pane's shell binary as `ShellSpec::program` reports
/// it. `None` means the pane has not resolved one yet, and the platform is the
/// only evidence there is: a Windows pane is overwhelmingly PowerShell, and
/// everywhere else it is something POSIX. (WSL panes get their paths rewritten
/// to `/mnt/...` before they reach here.)
pub fn quoting_for(shell_program: Option<&str>) -> Quoting {
    let Some(base) = shell_program.map(base_name) else {
        return if cfg!(windows) {
            Quoting::PowerShell
        } else {
            Quoting::Posix
        };
    };
    if base.eq_ignore_ascii_case("cmd") {
        Quoting::Cmd
    } else if base.eq_ignore_ascii_case("powershell") || base.eq_ignore_ascii_case("pwsh") {
        Quoting::PowerShell
    } else {
        Quoting::Posix
    }
}

/// Whether `a || b` runs `b` only when `a` fails, in the shell named by
/// `shell_program` (a binary as `ShellSpec::program` reports it).
///
/// An allowlist, because the shells that lack it do not skip the operator —
/// they refuse the whole line, `a` included. Windows PowerShell 5.1 calls it an
/// invalid statement separator (pwsh 7 added it), nu and elvish have no such
/// operator, and fish only gained it in 3.0 — anything older is long past its
/// end of life, so a bare `fish` counts. `wsl` is out: which shell it lands in
/// is the distro's business.
pub fn runs_or_list(shell_program: &str) -> bool {
    const OR_LIST: &[&str] = &[
        "sh", "bash", "zsh", "dash", "ash", "ksh", "mksh", "oksh", "yash", "fish", "pwsh", "cmd",
        "csh", "tcsh", "xonsh",
    ];
    let base = base_name(shell_program);
    OR_LIST.iter().any(|s| base.eq_ignore_ascii_case(s))
}

/// Quote `path` as a single argument for the shell the pane is running.
pub fn quote_for_shell(path: &str, shell_program: Option<&str>) -> String {
    quote_as(path, quoting_for(shell_program))
}

/// [`quote_for_shell`] with the dialect already decided.
fn quote_as(path: &str, quoting: Quoting) -> String {
    if path.is_empty() {
        return match quoting {
            Quoting::Cmd => "\"\"".to_string(),
            _ => "''".to_string(),
        };
    }
    // `~/` has to stay unquoted for the shell to expand it, so quote only the
    // rest. A bare `~` is already covered by `is_bare`.
    if let Some(rest) = path.strip_prefix("~/") {
        if rest.is_empty() {
            return "~/".to_string();
        }
        return format!("~/{}", quote_as(rest, quoting));
    }
    if path.chars().all(is_bare) {
        return path.to_string();
    }
    match quoting {
        Quoting::Cmd => format!("\"{path}\""),
        Quoting::PowerShell => format!("'{}'", path.replace('\'', "''")),
        Quoting::Posix => format!("'{}'", path.replace('\'', r"'\''")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_shells_with_an_or_list_are_handed_one() {
        for yes in [
            "bash",
            "/bin/zsh",
            "/opt/homebrew/bin/fish",
            r"C:\Program Files\PowerShell\7\pwsh.exe",
            r"C:\Program Files\Git\bin\bash.exe",
            "CMD.EXE",
        ] {
            assert!(runs_or_list(yes), "{yes}");
        }
        for no in [
            "powershell.exe",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            "nu",
            "/usr/local/bin/elvish",
            "wsl.exe",
            "",
        ] {
            assert!(!runs_or_list(no), "{no}");
        }
    }

    #[test]
    fn a_plain_path_is_left_alone() {
        assert_eq!(
            quote_for_shell("/Users/me/notes.txt", None),
            "/Users/me/notes.txt"
        );
        assert_eq!(quote_for_shell("notes.txt", None), "notes.txt");
        assert_eq!(quote_for_shell("--message", None), "--message");
    }

    #[test]
    fn posix_shells_get_single_quotes() {
        assert_eq!(
            quote_for_shell("/Users/me/My File (1).txt", Some("zsh")),
            "'/Users/me/My File (1).txt'"
        );
        assert_eq!(
            quote_for_shell("/a/$HOME & more", None),
            "'/a/$HOME & more'"
        );
        assert_eq!(quote_for_shell("it's here", Some("zsh")), r"'it'\''s here'");
        assert_eq!(quote_for_shell("", None), "''");
    }

    #[test]
    fn a_newline_survives_inside_the_quotes() {
        assert_eq!(quote_for_shell("a\nb", None), "'a\nb'");
    }

    /// The bug this module exists for: a Windows path used to come out as
    /// `C:\\Users\\me\\My\ Docs`, which the shell then un-escaped back into a
    /// path with no separators at all.
    #[test]
    fn a_windows_path_keeps_its_separators() {
        assert_eq!(
            quote_for_shell(r"C:\Users\me\My Docs", Some("powershell.exe")),
            r"'C:\Users\me\My Docs'"
        );
        assert_eq!(
            quote_for_shell(r"C:\Users\me\My Docs", Some(r"C:\Windows\System32\cmd.exe")),
            "\"C:\\Users\\me\\My Docs\""
        );
    }

    #[test]
    fn cmd_exe_is_recognised_by_basename_on_either_separator() {
        for p in [
            "cmd",
            "cmd.exe",
            "CMD.EXE",
            r"C:\Windows\System32\cmd.exe",
            "/c/Windows/System32/cmd.exe",
        ] {
            assert_eq!(
                quote_for_shell("a b", Some(p)),
                "\"a b\"",
                "{p} should be recognised as cmd.exe"
            );
        }
        assert_eq!(quote_for_shell("a b", Some("pwsh")), "'a b'");
        assert_eq!(quote_for_shell("a b", Some("powershell.exe")), "'a b'");
    }

    #[test]
    fn a_tilde_stays_outside_the_quotes_so_the_shell_expands_it() {
        assert_eq!(quote_for_shell("~/My Documents", None), "~/'My Documents'");
        assert_eq!(quote_for_shell("~/notes.txt", None), "~/notes.txt");
        assert_eq!(quote_for_shell("~", None), "~");
        // Not a home reference — a file whose name starts with a tilde.
        assert_eq!(quote_for_shell("~weird name", None), "'~weird name'");
    }

    /// PowerShell does not join a quoted string to the bare word beside it, so
    /// the POSIX `'\''` seam is not a seam there — it is three tokens.
    #[test]
    fn powershell_doubles_an_embedded_quote_where_posix_breaks_out() {
        assert_eq!(
            quote_for_shell(r"C:\Users\O'Brien\a.txt", Some("powershell.exe")),
            r"'C:\Users\O''Brien\a.txt'"
        );
        assert_eq!(
            quote_for_shell("it's here", Some("pwsh")),
            "'it''s here'",
            "pwsh speaks the same dialect"
        );
        assert_eq!(
            quote_for_shell(r"C:\Users\O'Brien\a.txt", Some("CMD.EXE")),
            "\"C:\\Users\\O'Brien\\a.txt\"",
            "cmd.exe quotes with \", so an apostrophe needs nothing"
        );
    }

    #[test]
    fn the_shell_decides_the_dialect() {
        assert_eq!(quoting_for(Some("zsh")), Quoting::Posix);
        assert_eq!(quoting_for(Some("/bin/bash")), Quoting::Posix);
        assert_eq!(quoting_for(Some("cmd.exe")), Quoting::Cmd);
        assert_eq!(quoting_for(Some("powershell.exe")), Quoting::PowerShell);
        assert_eq!(quoting_for(Some("POWERSHELL.EXE")), Quoting::PowerShell);
        assert_eq!(quoting_for(Some("pwsh")), Quoting::PowerShell);
        let posix_escapes_for = |s| quoting_for(s) == Quoting::Posix;
        assert!(posix_escapes_for(Some("zsh")));
        assert!(posix_escapes_for(Some("/bin/bash")));
        assert!(!posix_escapes_for(Some("cmd.exe")));
        assert!(!posix_escapes_for(Some("powershell.exe")));
        assert!(!posix_escapes_for(Some("pwsh")));
        assert_eq!(posix_escapes_for(None), !cfg!(windows));
    }
}
