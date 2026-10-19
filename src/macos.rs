//! macOS: files opened from Finder (double-click, "Open With") or dropped on the Dock icon.
//!
//! macOS delivers these to the application delegate's `application:openURLs:`. winit's
//! delegate doesn't implement it, so we add the method to its class at startup.

use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{ffi, sel};
use objc2_foundation::{NSArray, NSURL};
use std::path::PathBuf;
use std::sync::OnceLock;

type Handler = Box<dyn Fn(Vec<PathBuf>) + Send + Sync>;
static HANDLER: OnceLock<Handler> = OnceLock::new();

unsafe extern "C-unwind" fn open_urls(_this: &AnyObject, _cmd: Sel, _app: &AnyObject, urls: &NSArray<NSURL>) {
    let paths = (0..urls.count())
        .filter_map(|i| urls.objectAtIndex(i).path())
        .map(|p| PathBuf::from(p.to_string()))
        .collect();
    if let Some(handler) = HANDLER.get() {
        handler(paths);
    }
}

/// Routes Finder and Dock "open" requests to `handler`. Call after the event loop is
/// built (which registers winit's delegate class) and before it runs, so files that
/// launched the app are caught too.
pub fn on_open_files(handler: impl Fn(Vec<PathBuf>) + Send + Sync + 'static) {
    if HANDLER.set(Box::new(handler)).is_err() {
        return;
    }
    let Some(class) = AnyClass::get(c"WinitApplicationDelegate") else {
        eprintln!("oxide: app delegate not found; opening files from Finder is unavailable");
        return;
    };
    type OpenUrls = unsafe extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, &NSArray<NSURL>);
    // SAFETY: the method type encoding ("v@:@@": void return, self, _cmd, two objects)
    // matches `open_urls`, and the class pointer comes from the runtime.
    unsafe {
        let imp = std::mem::transmute::<OpenUrls, Imp>(open_urls);
        let class = class as *const AnyClass as *mut AnyClass;
        ffi::class_addMethod(class, sel!(application:openURLs:), imp, c"v@:@@".as_ptr());
    }
}
