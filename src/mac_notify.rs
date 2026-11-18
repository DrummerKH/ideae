//! Notifications on macOS as the app's own, from the system's notification
//! center: they have the app's name and icon, and a click on one comes
//! back here. Sent by AppleScript they were Script Editor's, and a click
//! opened that.
//!
//! Found by trying, on macOS 27: the system gives leave only to a bundle
//! in a folder of applications (one in a temporary folder is refused
//! without asking), and asks the user the first time.
//!
//! Only an app whose binary is in its bundle has a center. A process that
//! is not one (the daemon, a binary started from a build folder or linked
//! into a bundle from one, a test) says so, and the caller sends the
//! notification the old way.

use std::{
    collections::HashMap,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use block2::RcBlock;
use objc2::{
    AnyThread, define_class, msg_send,
    rc::Retained,
    runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject},
};
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

type Click = Box<dyn FnOnce() + Send>;

/// What to do when each notification still shown is clicked, by its id.
/// Bounded: one never clicked is forgotten when many came after it.
static CLICKS: Mutex<Option<HashMap<String, (u64, Click)>>> = Mutex::new(None);
const CLICKS_KEPT: usize = 64;
static SENT: AtomicU64 = AtomicU64::new(0);

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and this has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "IdeaeNotifications"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        /// Shown while the app is in front too: the task it is about may be another than the one looked at.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(&self, _center: &UNUserNotificationCenter, _notification: &UNNotification, done: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>) {
            done.call((UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List | UNNotificationPresentationOptions::Sound,));
        }

        /// Clicked: the system brings the app to the front by itself; what the notification was for is done here.
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(&self, _center: &UNUserNotificationCenter, response: &UNNotificationResponse, done: &block2::DynBlock<dyn Fn()>) {
            let id = response.notification().request().identifier().to_string();
            let click = CLICKS.lock().ok().and_then(|mut clicks| clicks.as_mut()?.remove(&id));
            if let Some((_, click)) = click {
                click();
            }
            done.call(());
        }
    }
);

/// What the system said went wrong, for a debug run.
fn told(error: *mut NSError) -> String {
    // SAFETY: the system gives a valid error or none, for the length of the call.
    match unsafe { error.as_ref() } {
        Some(error) => format!("refused: {} ({} {})", error.localizedDescription(), error.domain(), error.code()),
        None => "ok".into(),
    }
}

/// The center, with the delegate set and leave asked for, once. None for a
/// process that is no app in a bundle: asked for one there, AppKit raises.
fn center() -> Option<&'static Retained<UNUserNotificationCenter>> {
    struct Ready(Option<(Retained<UNUserNotificationCenter>, Retained<Delegate>)>);
    // SAFETY: the center is documented as usable from any thread; the delegate has no state.
    unsafe impl Send for Ready {}
    unsafe impl Sync for Ready {}
    static READY: OnceLock<Ready> = OnceLock::new();
    READY
        .get_or_init(|| {
            // Only the window's process: the daemon is the same binary of the
            // same bundle, and has no app's loop for a click to come back in.
            if !WINDOW.load(Ordering::Relaxed) {
                return Ready(None);
            }
            // Only by its own bundle. One started from a bundle whose binary is
            // a link out of it (scripts/macos-app.sh --link) is known to the
            // system as that app, and still has no center: asked for it, the
            // system ends the process, and that cannot be caught. Tried.
            if bundle().is_none() {
                return Ready(None);
            }
            let center = UNUserNotificationCenter::currentNotificationCenter();
            let delegate: Retained<Delegate> = unsafe { msg_send![super(Delegate::alloc().set_ivars(())), init] };
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            // Asked once by the system; refused, sending fails and the caller falls back.
            let asked = RcBlock::new(|granted: Bool, error: *mut NSError| {
                if std::env::var_os("IDEAE_NOTIFY").is_some() {
                    eprintln!("notifications: leave {} {}", granted.as_bool(), told(error));
                }
            });
            center.requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound, &asked);
            Ready(Some((center, delegate)))
        })
        .0
        .as_ref()
        .map(|(center, _)| center)
}

/// Set by the process that has the window, before it sends anything.
static WINDOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn of_the_window() {
    WINDOW.store(true, Ordering::Relaxed);
}

/// The bundle this process is the app of, by its identifier: with none it
/// cannot send notifications as an app. Told in a debug run (`IDEAE_DEBUG`).
pub fn bundle() -> Option<String> {
    NSBundle::mainBundle().bundleIdentifier().map(|id| id.to_string())
}

/// Whether this process sends its notifications as the app's own. Asks
/// the system for leave to, the first time.
pub fn as_the_app() -> bool {
    center().is_some()
}

/// Sends one as the app's own; `on_click` runs when it is clicked, on a
/// thread of the system's. False when this process has no center: nothing
/// was sent. One the system then refuses (notifications turned off for the
/// app) is given to `refused`, to send it some other way.
pub fn send(title: &str, body: &str, on_click: Option<Click>, refused: impl Fn() + Send + 'static) -> bool {
    let Some(center) = center() else { return false };
    let n = SENT.fetch_add(1, Ordering::Relaxed);
    let id = format!("ideae-{}-{n}", std::process::id());
    if let Some(click) = on_click {
        if let Ok(mut clicks) = CLICKS.lock() {
            let clicks = clicks.get_or_insert_with(HashMap::new);
            clicks.insert(id.clone(), (n, click));
            while clicks.len() > CLICKS_KEPT {
                let Some(oldest) = clicks.iter().min_by_key(|(_, (sent, _))| *sent).map(|(id, _)| id.clone()) else { break };
                clicks.remove(&oldest);
            }
        }
    }
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&NSString::from_str(&id), &content, None);
    let done = RcBlock::new(move |error: *mut NSError| {
        if std::env::var_os("IDEAE_NOTIFY").is_some() {
            eprintln!("notifications: sent {}", told(error));
        }
        if !error.is_null() {
            if let Ok(mut clicks) = CLICKS.lock() {
                if let Some(clicks) = clicks.as_mut() {
                    clicks.remove(&id);
                }
            }
            refused();
        }
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&done));
    true
}
