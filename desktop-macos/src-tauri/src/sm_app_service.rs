//! Thin Rust wrapper over Apple's `SMAppService` (macOS 13+).
//!
//! `SMAppService` is the blessed replacement for the deprecated
//! `SMJobBless` / `launchctl load` dance for app-bundled
//! LaunchAgents and LaunchDaemons. Two properties matter for the
//! drag-to-Trash UX:
//!
//! 1. The plist lives inside the .app bundle at
//!    `Contents/Library/LaunchAgents/<name>.plist` — no file ever
//!    lands in `~/Library/LaunchAgents/`, so there's no orphan
//!    state to clean up.
//! 2. When the user moves the .app to the Trash, macOS detects
//!    the bundle is gone and automatically disables every service
//!    that was registered through `SMAppService` from that bundle.
//!    No "uninstall" step is required for the happy path.
//!
//! ## Why raw `msg_send!` instead of a framework binding crate
//!
//! The `objc2-service-management` crate exists but moves fast and
//! tends to lag stable releases of newer ServiceManagement APIs.
//! Calling `+[SMAppService …]` via `objc2::msg_send!` against the
//! runtime is stable across objc2 minor versions and only requires
//! `#[link(framework = "ServiceManagement")]` (set up in
//! `build.rs`). The surface we need — register, unregister, status,
//! open settings — is six lines per call.

#![cfg(target_os = "macos")]

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, NSObject};
use objc2::{class, msg_send, msg_send_id};
use objc2_foundation::{NSError, NSString};

/// Mirrors `SMAppServiceStatus` from `<ServiceManagement/SMAppService.h>`.
/// Values are stable Apple-defined integers — do NOT reorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// Service has never been registered (or was unregistered).
    NotRegistered,
    /// Service is registered and enabled.
    Enabled,
    /// User must approve the service in *System Settings → General
    /// → Login Items & Extensions → Allow in Background*. macOS
    /// surfaces this automatically the first time the user
    /// registers a service that requires approval; we surface it
    /// in the tray menu as an actionable row.
    RequiresApproval,
    /// macOS couldn't find the plist inside the bundle. Typically
    /// means our post-bundle plist injection didn't run (the
    /// bundle was built without `scripts/build-mac.sh`).
    NotFound,
    /// `SMAppServiceStatus` introduced a new variant we don't
    /// recognise. Surfaced as a soft-error rather than a panic.
    Unknown(i64),
}

impl AgentStatus {
    fn from_raw(raw: i64) -> Self {
        match raw {
            0 => Self::NotRegistered,
            1 => Self::Enabled,
            2 => Self::RequiresApproval,
            3 => Self::NotFound,
            n => Self::Unknown(n),
        }
    }
}

/// Errors bubbling up from `+[SMAppService …]`. The user-facing
/// strings are taken verbatim from `NSError.localizedDescription`
/// because Apple's wording is usually clearer than anything we'd
/// invent (and changes per macOS version — we don't want to drift).
#[derive(Debug, thiserror::Error)]
pub enum SmError {
    #[error("ServiceManagement class not available — is this macOS 13 or newer?")]
    ClassMissing,
    #[error("SMAppService returned a null service handle for plist {0}")]
    NilService(String),
    #[error("{message} (NSError code {code})")]
    NsError { code: i64, message: String },
    #[error("call returned `false` without setting an NSError")]
    UnknownFailure,
}

/// Locate the `SMAppService` class. Returns `None` on macOS < 13
/// (the class doesn't exist there) so callers can fall back to a
/// legacy code path.
fn sm_class() -> Option<&'static AnyClass> {
    // `class!` panics if the class is missing. We need a non-panic
    // lookup so an older macOS doesn't crash the tray. Use the raw
    // runtime function via `objc2::runtime::AnyClass::get`.
    AnyClass::get(c"SMAppService")
}

/// Build an `SMAppService` for the bundled LaunchAgent at
/// `Contents/Library/LaunchAgents/<plist_name>`.
///
/// `plist_name` is the filename only (e.g.
/// `"com.execlaw.agent.plist"`) — SMAppService resolves the path
/// relative to the calling bundle.
fn make_agent_service(plist_name: &str) -> Result<Retained<AnyObject>, SmError> {
    let cls = sm_class().ok_or(SmError::ClassMissing)?;
    let plist_ns = NSString::from_str(plist_name);
    // SAFETY: `agentServiceWithPlistName:` returns a +0 autoreleased
    // SMAppService instance. `msg_send_id!` retains it for us into a
    // `Retained<AnyObject>`. The class pointer + selector are
    // statically known. The argument is a valid NSString reference.
    let service: Option<Retained<AnyObject>> = unsafe {
        msg_send_id![cls, agentServiceWithPlistName: &*plist_ns]
    };
    service.ok_or_else(|| SmError::NilService(plist_name.to_string()))
}

/// Register (enable) the bundled LaunchAgent. Returns `Ok` on
/// success or if the service is already registered — macOS treats
/// re-registration as a no-op + status refresh, which is the
/// behavior we want for a tray that runs on every app launch.
pub fn register_agent(plist_name: &str) -> Result<(), SmError> {
    let service = make_agent_service(plist_name)?;
    let mut err: *mut NSError = std::ptr::null_mut();
    // SAFETY: `registerAndReturnError:` takes `NSError **` and
    // returns BOOL. We pass a valid out-pointer. The receiver is
    // a non-null retained pointer obtained above.
    let ok: bool = unsafe { msg_send![&*service, registerAndReturnError: &mut err] };
    if ok {
        return Ok(());
    }
    Err(nserror_to_sm(err))
}

/// Unregister (disable) the bundled LaunchAgent. The user reaches
/// this via the *Uninstall execlaw…* menu item; the common-case
/// drag-to-Trash path doesn't need it (macOS auto-disables on
/// bundle removal).
pub fn unregister_agent(plist_name: &str) -> Result<(), SmError> {
    let service = make_agent_service(plist_name)?;
    let mut err: *mut NSError = std::ptr::null_mut();
    // SAFETY: same as `register_agent` — well-typed out-param +
    // valid receiver pointer.
    let ok: bool = unsafe { msg_send![&*service, unregisterAndReturnError: &mut err] };
    if ok {
        return Ok(());
    }
    Err(nserror_to_sm(err))
}

/// Query the current status. Doesn't touch launchd state — pure
/// read against the SMAppService registry.
pub fn agent_status(plist_name: &str) -> Result<AgentStatus, SmError> {
    let service = make_agent_service(plist_name)?;
    // SAFETY: `status` is a 0-arg method returning NSInteger
    // (mapped to i64 on aarch64 and x86_64). No out-params, no
    // failure mode at the ObjC level.
    let raw: i64 = unsafe { msg_send![&*service, status] };
    Ok(AgentStatus::from_raw(raw))
}

/// Open *System Settings → General → Login Items & Extensions* so
/// the user can approve a service stuck in `RequiresApproval`.
/// Apple's blessed deep-link for this surface.
pub fn open_login_items_settings() {
    let Some(cls) = sm_class() else {
        return;
    };
    // SAFETY: `openSystemSettingsLoginItems` is a 0-arg class
    // method with void return. The class pointer is valid (we
    // just resolved it).
    unsafe {
        let _: () = msg_send![cls, openSystemSettingsLoginItems];
    }
}

/// Convert an `NSError*` returned through an `(NSError **)`
/// out-parameter into our typed error. Handles the "BOOL false
/// but no error set" edge case.
fn nserror_to_sm(err: *mut NSError) -> SmError {
    if err.is_null() {
        return SmError::UnknownFailure;
    }
    // SAFETY: `err` was filled by ObjC with a valid autoreleased
    // NSError pointer. We don't take ownership (no Retained
    // wrapper) — we read it immediately and let the autorelease
    // pool drain it. The reads are well-defined Objective-C
    // accessors that don't mutate.
    unsafe {
        let code: i64 = msg_send![&*err, code];
        let desc: Option<Retained<NSString>> = msg_send_id![&*err, localizedDescription];
        let message = desc
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<no description>".to_string());
        SmError::NsError { code, message }
    }
}

// `NSObject` is referenced indirectly via the class hierarchy of
// the values we receive; the import keeps the `objc2::msg_send!`
// expansions happy on some toolchain versions.
#[allow(dead_code)]
fn _force_nsobject_link(_: &NSObject) {}
