//! macOS privacy permissions the engine needs: Accessibility and Screen Recording.
//!
//! The engine runs as a child of this app, so macOS attributes its requests to
//! the app (the "responsible" process) and the owner grants them to
//! Lazurio Agent Desktop once. Windows needs nothing similar; the app runs in
//! the signed-in user's session, where UI Automation works.

use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub needed: bool,
    pub accessibility: bool,
    pub screen_recording: bool,
    pub all_granted: bool,
}

#[cfg(target_os = "macos")]
mod mac {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::{CFString, CFStringRef};

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> u8;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
        static kAXTrustedCheckOptionPrompt: CFStringRef;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> u8;
        fn CGRequestScreenCaptureAccess() -> u8;
    }

    pub fn accessibility() -> bool {
        unsafe { AXIsProcessTrusted() != 0 }
    }

    pub fn screen_recording() -> bool {
        unsafe { CGPreflightScreenCaptureAccess() != 0 }
    }

    pub fn request_accessibility() {
        unsafe {
            let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
            let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
            AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef());
        }
    }

    pub fn request_screen_recording() {
        unsafe {
            CGRequestScreenCaptureAccess();
        }
    }
}

pub fn status() -> Status {
    #[cfg(target_os = "macos")]
    {
        let accessibility = mac::accessibility();
        let screen_recording = mac::screen_recording();
        Status {
            needed: true,
            accessibility,
            screen_recording,
            all_granted: accessibility && screen_recording,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Status {
            needed: false,
            accessibility: true,
            screen_recording: true,
            all_granted: true,
        }
    }
}

/// Shows the system prompt for `accessibility` or `screen-recording`.
pub fn request(kind: &str) {
    #[cfg(target_os = "macos")]
    match kind {
        "accessibility" => mac::request_accessibility(),
        "screen-recording" => mac::request_screen_recording(),
        _ => {}
    }
    #[cfg(not(target_os = "macos"))]
    let _ = kind;
}

/// The System Settings pane where the owner turns the permission on.
pub fn settings_url(kind: &str) -> Option<&'static str> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    match kind {
        "accessibility" => {
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        }
        "screen-recording" => {
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        }
        _ => None,
    }
}
