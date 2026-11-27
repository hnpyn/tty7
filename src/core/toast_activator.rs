//! The COM class Windows activates when a tty7 toast is clicked.
//!
//! A toast clicked from notification history (Action Center) reaches an
//! unpackaged app only this way: the shell looks up the `ToastActivatorCLSID`
//! on the shortcut that carries our AUMID and calls
//! `INotificationActivationCallback::Activate` on that class — in the running
//! tty7 if it has registered a class object, otherwise by launching the
//! `LocalServer32` command `core::aumid` registers. Without this class, such a
//! click did nothing at all (#1179).
//!
//! `Activate` returns success and does not route the click to a pane. It also
//! does not raise an already-running window. The class has to be registered
//! so a click from notification history can reach this process (#1179);
//! `activationType="foreground"` is the toast attribute the shell expects
//! alongside that class.

use windows::Win32::Foundation::{BOOL, CLASS_E_NOAGGREGATION, E_POINTER};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoInitializeEx, CoRegisterClassObject,
    IClassFactory, IClassFactory_Impl, REGCLS_MULTIPLEUSE,
};
use windows::Win32::UI::Notifications::{
    INotificationActivationCallback, INotificationActivationCallback_Impl,
    NOTIFICATION_USER_INPUT_DATA,
};
use windows::core::{GUID, IUnknown, Interface, PCWSTR, implement};

/// Register the class object for this process. Idempotent; never blocks.
///
/// The registration lives on a thread of its own that joins the MTA and then
/// parks for good: COM calls `Activate` on an RPC thread without needing a
/// message loop, and the parked thread keeps the apartment — and with it the
/// registration — alive for the life of the process.
pub(crate) fn start() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("tty7-toast-activator".into())
            .spawn(|| {
                // SAFETY: plain COM initialization of this new thread.
                let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                let factory: IClassFactory = Factory.into();
                // SAFETY: the CLSID is a live constant and `factory` a valid
                // class object; COM holds its own reference from here on.
                let registered = unsafe {
                    CoRegisterClassObject(
                        &crate::core::aumid::TOAST_ACTIVATOR_CLSID,
                        &factory,
                        CLSCTX_LOCAL_SERVER,
                        REGCLS_MULTIPLEUSE,
                    )
                };
                if let Err(e) = registered {
                    log::warn!("toast clicks from notification history will be ignored: {e}");
                    return;
                }
                loop {
                    std::thread::park();
                }
            });
        if let Err(e) = spawned {
            log::warn!("failed to start the toast activator thread: {e}");
        }
    });
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Option<&IUnknown>,
        iid: *const GUID,
        object: *mut *mut core::ffi::c_void,
    ) -> windows::core::Result<()> {
        if object.is_null() || iid.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: checked non-null above; COM's contract is that it is writable.
        unsafe { *object = std::ptr::null_mut() };
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let callback: INotificationActivationCallback = Activator.into();
        // SAFETY: `iid` and `object` come straight from COM and were checked.
        unsafe { callback.query(iid, object).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

#[implement(INotificationActivationCallback)]
struct Activator;

impl INotificationActivationCallback_Impl for Activator_Impl {
    fn Activate(
        &self,
        _app_user_model_id: &PCWSTR,
        _invoked_args: &PCWSTR,
        _data: *const NOTIFICATION_USER_INPUT_DATA,
        _count: u32,
    ) -> windows::core::Result<()> {
        // Acknowledge the click. The launch string is not routed to a pane,
        // and this callback does not raise an already-running window.
        Ok(())
    }
}
