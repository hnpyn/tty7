//! Desktop notifications for a pane.
//!
//! macOS delivers through `NSUserNotification` / `NSUserNotificationCenter`.
//! Windows keeps its toast XML. Other platforms fall through to notify-rust.

/// Show a desktop notification.
///
/// macOS always goes through `macos_notify`. Elsewhere, a Windows toast that
/// the shell can brand takes the job; every other case falls back to the
/// plain `notify-rust` path below, which is why `try_clickable_notification`
/// reports whether it took the job.
pub(crate) fn notify_desktop_for_pane(title: Option<&str>, body: &str) {
    let summary = sanitize_notification_text(title.unwrap_or("ntty7"), NOTIFY_TITLE_MAX);
    let body = sanitize_notification_text(body, NOTIFY_BODY_MAX);

    #[cfg(target_os = "macos")]
    {
        macos_notify::deliver(summary, body);
    }
    #[cfg(not(target_os = "macos"))]
    {
        if try_clickable_notification(&summary, &body) {
            return;
        }

        std::thread::spawn(move || {
            let mut notif = notify_rust::Notification::new();
            notif.summary(&summary).body(&body);
            // Without our own AUMID, the Windows backend falls back to
            // PowerShell's — icon and name included. Only set ours once the
            // shell has indexed a shortcut carrying it: for an AUMID it does
            // not know, `show()` reports success and drops the toast, so the
            // ugly fallback beats the branded one every time we are not sure.
            #[cfg(target_os = "windows")]
            if let Some(app_id) = crate::core::aumid::toast_app_id() {
                notif.app_id(app_id);
            }
            let _ = notif.show();
        });
    }
}

/// Longest title / body we hand to a notification backend.
///
/// The text comes straight off the terminal (OSC 9/777 payloads, window
/// titles, an agent's own status line), so it is attacker-shaped: arbitrarily
/// long, and free to contain control bytes. Every backend truncates in its own
/// ugly way, and on Windows a stray `ESC` makes the toast XML fail to parse and
/// the whole notification disappear — so clamp both here, once, for all paths.
const NOTIFY_TITLE_MAX: usize = 96;
const NOTIFY_BODY_MAX: usize = 512;

fn sanitize_notification_text(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut taken = 0usize;
    for ch in s.chars() {
        // XML 1.0 allows exactly tab / LF / CR out of the control range.
        if ch.is_control() && !matches!(ch, '\t' | '\n' | '\r') {
            continue;
        }
        if taken == max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
        taken += 1;
    }
    out
}

/// macOS notifications, straight to `NSUserNotificationCenter`.
///
/// This used to go through `mac-notification-sys` with `wait_for_click`, so a
/// click could reveal the pane. The way that crate notices a click is to park
/// the sending thread and, for every notification outstanding, add a repeating
/// 0.5 s timer to the *main* run loop that calls `deliveredNotifications` — a
/// synchronous XPC round trip — to see whether the banner is still there. A
/// banner nobody clicks stays in Notification Center, so its timer never goes
/// away. Sampled with nine outstanding: a fifth of the UI thread inside that
/// XPC, every window juddering, and each agent turn adding one more timer.
///
/// Here the click arrives through the center's delegate, which is what the
/// API is for: no parked thread, no timer, nothing on the main thread until
/// the user actually clicks. The click dismisses the banner.
///
/// Nothing else may touch the center on this platform. `mac-notification-sys`
/// installs a delegate of its own the first time it sends, the last
/// `setDelegate:` wins, and `notify-rust` is that crate on macOS — so its
/// `show` is never called here, only its `set_application`, which does not
/// install one (that is what names a bare `cargo run` binary to the center).
#[cfg(target_os = "macos")]
#[allow(
    deprecated,
    reason = "UNUserNotificationCenter needs a signed, entitled bundle; NSUserNotification is what a bare binary can use"
)]
mod macos_notify {
    use objc2::rc::{Retained, autoreleasepool};
    use objc2::runtime::ProtocolObject;
    use objc2::{AnyThread, define_class, msg_send};
    use objc2_foundation::{
        NSObject, NSObjectProtocol, NSString, NSUserNotification, NSUserNotificationCenter,
        NSUserNotificationCenterDelegate,
    };

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; `Delegate` has no
        // ivars and no `Drop`.
        #[unsafe(super = NSObject)]
        #[name = "Tty7NotificationDelegate"]
        struct Delegate;

        // SAFETY: `NSObjectProtocol` has no safety requirements.
        unsafe impl NSObjectProtocol for Delegate {}

        // SAFETY: `NSUserNotificationCenterDelegate` has no safety requirements.
        unsafe impl NSUserNotificationCenterDelegate for Delegate {
            /// Runs on the main thread, when the user clicks the banner or the
            /// entry in Notification Center. The clicked entry is dropped from
            /// the center — a one-way message, unlike the
            /// `deliveredNotifications` round trip this module exists to avoid.
            #[unsafe(method(userNotificationCenter:didActivateNotification:))]
            fn did_activate(
                &self,
                center: &NSUserNotificationCenter,
                notification: &NSUserNotification,
            ) {
                center.removeDeliveredNotification(notification);
            }
        }
    );

    fn install_delegate() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // Name the delivering application to the center before it is first
            // touched: the center drops requests from a process with no bundle
            // identity, which is what a bare `cargo run` binary is. This
            // swizzles `-[NSBundle bundleIdentifier]` for the main bundle to
            // `com.github.ntty7` when LaunchServices knows that id (a bundled
            // ntty7.app). It is not tty7's id: a machine that also has
            // tty7.app installed must not have this process's notifications
            // claimed by that bundle. When LaunchServices does not know the
            // id, the swizzle's own default, `com.apple.Terminal`, is what the
            // center sees. It can only be called once per process, so there is
            // no second chance to pass a different name.
            let _ = notify_rust::set_application("com.github.ntty7");
            // SAFETY: `NSObject`'s `init` takes nothing and returns the object.
            let delegate: Retained<Delegate> = unsafe { msg_send![Delegate::alloc(), init] };
            let center = NSUserNotificationCenter::defaultUserNotificationCenter();
            // SAFETY: `delegate` is a `Delegate`, which conforms to the protocol.
            unsafe { center.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
            // The center holds its delegate unretained. Ours lives as long as
            // the process, so it is simply never released.
            std::mem::forget(delegate);
        });
    }

    /// Hands the notification to the center and returns. `deliverNotification:`
    /// is an XPC message, so it goes out on a short-lived thread rather than
    /// from wherever the OSC sequence was parsed — never the UI thread.
    pub(super) fn deliver(title: String, body: String) {
        std::thread::spawn(move || {
            autoreleasepool(|_| {
                install_delegate();
                let notification = NSUserNotification::new();
                notification.setTitle(Some(&NSString::from_str(&title)));
                notification.setInformativeText(Some(&NSString::from_str(&body)));
                NSUserNotificationCenter::defaultUserNotificationCenter()
                    .deliverNotification(&notification);
            });
        });
    }
}

#[cfg(all(target_os = "windows", not(test)))]
fn try_clickable_notification(title: &str, body: &str) -> bool {
    // An AUMID the shell has not indexed makes `Show` report success and drops
    // the toast on the floor, so without one the notify-rust path — which
    // deliberately keeps PowerShell's identity rather than lose the toast — is
    // the only one that shows anything at all.
    if crate::core::aumid::toast_app_id().is_none() {
        return false;
    }
    let Some(tx) = toast_thread() else {
        return false;
    };
    tx.try_send((title.to_string(), body.to_string())).is_ok()
}

/// Title and body of one toast.
#[cfg(all(target_os = "windows", not(test)))]
type ToastRequest = (String, String);

/// Toasts are built, shown and kept on one dedicated thread.
///
/// Keeping the objects on a single thread means the WinRT objects never
/// cross a thread boundary. A click is not routed to a pane. The toast's
/// `activationType="foreground"` and the registered COM class
/// (`core::toast_activator`) are how the shell delivers that click; neither
/// raises an already-running window from here.
#[cfg(all(target_os = "windows", not(test)))]
fn toast_thread() -> Option<&'static std::sync::mpsc::SyncSender<ToastRequest>> {
    use std::sync::OnceLock;
    use std::sync::mpsc::sync_channel;

    static TX: OnceLock<Option<std::sync::mpsc::SyncSender<ToastRequest>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = sync_channel::<ToastRequest>(32);
        std::thread::Builder::new()
            .name("tty7-toasts".into())
            .spawn(move || toast_loop(rx))
            .map_err(|e| log::warn!("failed to start the toast thread: {e}"))
            .ok()
            .map(|_| tx)
    })
    .as_ref()
}

#[cfg(all(target_os = "windows", not(test)))]
fn toast_loop(rx: std::sync::mpsc::Receiver<ToastRequest>) {
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    // MTA: showing a toast does not need a message loop of our own.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    while let Ok((title, body)) = rx.recv() {
        show_windows_toast(&title, &body);
    }
}

/// The toast's XML. `launch` is empty because a click is not routed to a
/// pane. `activationType="foreground"` is the attribute the shell pairs with
/// the registered activator; this process does not raise its own window from
/// the click.
#[cfg(any(target_os = "windows", test))]
fn toast_xml(title: &str, body: &str) -> String {
    format!(
        r#"<toast launch="" activationType="foreground">
  <visual>
    <binding template="ToastText02">
      <text id="1">{}</text>
      <text id="2">{}</text>
    </binding>
  </visual>
</toast>"#,
        xml_escape(title),
        xml_escape(body)
    )
}

#[cfg(all(target_os = "windows", not(test)))]
fn show_windows_toast(title: &str, body: &str) {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

    let Some(app_id) = crate::core::aumid::toast_app_id() else {
        return;
    };

    let xml = toast_xml(title, body);

    let doc = match XmlDocument::new() {
        Ok(d) => {
            if let Err(e) = d.LoadXml(&xml.into()) {
                log::warn!("failed to load toast xml: {e}");
                return;
            }
            d
        }
        Err(e) => {
            log::warn!("failed to create toast xml document: {e}");
            return;
        }
    };

    let toast = match ToastNotification::CreateToastNotification(&doc) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("failed to create toast notification: {e}");
            return;
        }
    };

    let notifier = match ToastNotificationManager::CreateToastNotifierWithId(&app_id.into()) {
        Ok(n) => n,
        Err(e) => {
            log::warn!("failed to create toast notifier: {e}");
            return;
        }
    };

    if let Err(e) = notifier.Show(&toast) {
        log::warn!("failed to show toast: {e}");
    }
}

#[cfg(not(any(target_os = "macos", all(target_os = "windows", not(test)))))]
fn try_clickable_notification(_title: &str, _body: &str) -> bool {
    // Linux notifications go through notify-rust.
    false
}

#[cfg(any(target_os = "windows", test))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod notification_tests {
    use super::*;

    #[test]
    fn sanitizing_drops_control_bytes_but_keeps_line_breaks() {
        let raw = "build \x1b[31mfailed\x07\nsee log\ttail";
        assert_eq!(
            sanitize_notification_text(raw, NOTIFY_BODY_MAX),
            "build [31mfailed\nsee log\ttail"
        );
    }

    #[test]
    fn sanitizing_clamps_by_chars_not_bytes() {
        let out = sanitize_notification_text(&"账".repeat(200), 8);
        assert_eq!(out, format!("{}…", "账".repeat(8)));
        assert_eq!(sanitize_notification_text("short", 8), "short");
    }

    /// The real WinRT XML parser must accept the document, including escaped
    /// text and the empty `launch` attribute. Parses only; shows nothing.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_parses_the_toast_xml() {
        use windows::Data::Xml::Dom::XmlDocument;
        let doc = XmlDocument::new().expect("XmlDocument");
        doc.LoadXml(&toast_xml("a & <b>", "\"c\" 'd'").into())
            .expect("toast xml parses");
        let launch = doc
            .DocumentElement()
            .and_then(|root| root.GetAttribute(&"launch".into()))
            .expect("launch attribute");
        assert_eq!(launch.to_string(), "");
    }

    #[test]
    fn toast_xml_leaves_launch_empty_and_escapes_text() {
        let xml = toast_xml("a & <b>", "\"c\"");
        assert!(xml.contains(r#"launch="""#));
        assert!(xml.contains(r#"activationType="foreground""#));
        assert!(xml.contains("a &amp; &lt;b&gt;"));
        assert!(xml.contains("&quot;c&quot;"));
    }

    #[test]
    fn xml_escaping_covers_every_entity_once() {
        assert_eq!(
            xml_escape(r#"a & b < c > "d" 'e'"#),
            "a &amp; b &lt; c &gt; &quot;d&quot; &apos;e&apos;"
        );
    }
}
