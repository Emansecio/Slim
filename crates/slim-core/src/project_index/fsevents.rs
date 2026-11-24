//! Native filesystem watcher for [`super::ProjectIndexDaemon`].
//!
//! On macOS, binds directly to `CoreServices.framework` (`FSEventStream*`) and
//! `CoreFoundation.framework` (`CFRunLoop`) with zero external crate
//! dependencies. Other platforms gracefully fall back to synchronous
//! `notify_local_write` + on-demand mtime validation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Callback invoked whenever one or more paths under the watched root change.
pub(crate) type FsChangeCallback = Arc<dyn Fn(&[PathBuf]) + Send + Sync>;

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    };
    use std::thread::{self, JoinHandle};

    use super::FsChangeCallback;

    type CFAllocatorRef = *const c_void;
    type CFArrayRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFRunLoopRef = *mut c_void;
    type CFTimeInterval = f64;
    type FSEventStreamRef = *mut c_void;
    type ConstFSEventStreamRef = *const c_void;
    type FSEventStreamEventId = u64;
    type FSEventStreamCreateFlags = u32;
    type FSEventStreamEventFlags = u32;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW: FSEventStreamEventId = 0xFFFF_FFFF_FFFF_FFFF;
    const K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER: FSEventStreamCreateFlags = 0x0000_0002;
    const K_FS_EVENT_STREAM_CREATE_FLAG_FILE_EVENTS: FSEventStreamCreateFlags = 0x0000_0010;
    const STREAM_LATENCY_SECS: CFTimeInterval = 0.05;

    #[repr(C)]
    struct FSEventStreamContext {
        version: isize,
        info: *mut c_void,
        retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
        release: Option<extern "C" fn(*const c_void)>,
        copy_description: Option<extern "C" fn(*const c_void) -> CFStringRef>,
    }

    type FSEventStreamCallback = extern "C" fn(
        stream_ref: ConstFSEventStreamRef,
        client_call_back_info: *mut c_void,
        num_events: usize,
        event_paths: *mut c_void,
        event_flags: *const FSEventStreamEventFlags,
        event_ids: *const FSEventStreamEventId,
    );

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        fn FSEventStreamCreate(
            allocator: CFAllocatorRef,
            callback: FSEventStreamCallback,
            context: *mut FSEventStreamContext,
            paths_to_watch: CFArrayRef,
            since_when: FSEventStreamEventId,
            latency: CFTimeInterval,
            flags: FSEventStreamCreateFlags,
        ) -> FSEventStreamRef;
        fn FSEventStreamScheduleWithRunLoop(
            stream_ref: FSEventStreamRef,
            run_loop: CFRunLoopRef,
            run_loop_mode: CFStringRef,
        );
        fn FSEventStreamStart(stream_ref: FSEventStreamRef) -> bool;
        fn FSEventStreamStop(stream_ref: FSEventStreamRef);
        fn FSEventStreamInvalidate(stream_ref: FSEventStreamRef);
        fn FSEventStreamRelease(stream_ref: FSEventStreamRef);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFRunLoopDefaultMode: CFStringRef;
        fn CFStringCreateWithCString(
            alloc: CFAllocatorRef,
            c_str: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFArrayCreate(
            allocator: CFAllocatorRef,
            values: *const *const c_void,
            num_values: isize,
            callbacks: *const c_void,
        ) -> CFArrayRef;
        fn CFRelease(cf: *const c_void);
        fn CFRunLoopGetCurrent() -> CFRunLoopRef;
        fn CFRunLoopRun();
        fn CFRunLoopStop(rl: CFRunLoopRef);
        fn CFRetain(cf: *const c_void) -> *const c_void;
    }

    struct CallbackBox {
        callback: FsChangeCallback,
        stopped: Arc<AtomicBool>,
    }

    extern "C" fn on_fs_events(
        _stream_ref: ConstFSEventStreamRef,
        client_info: *mut c_void,
        num_events: usize,
        event_paths: *mut c_void,
        _event_flags: *const FSEventStreamEventFlags,
        _event_ids: *const FSEventStreamEventId,
    ) {
        if client_info.is_null() || event_paths.is_null() || num_events == 0 {
            return;
        }
        // SAFETY: `client_info` was allocated with `Box::into_raw` and lives
        // until after `FSEventStreamInvalidate` + `FSEventStreamRelease`.
        let state = unsafe { &*(client_info as *const CallbackBox) };
        if state.stopped.load(Ordering::Acquire) {
            return;
        }
        let paths_ptr = event_paths as *const *const c_char;
        let mut changed = Vec::with_capacity(num_events.min(512));
        for i in 0..num_events {
            // SAFETY: CoreServices guarantees `event_paths` holds `num_events`
            // NUL-terminated UTF-8 C strings when `UseCFTypes` is not set.
            let cstr_ptr = unsafe { *paths_ptr.add(i) };
            if cstr_ptr.is_null() {
                continue;
            }
            let bytes = unsafe { CStr::from_ptr(cstr_ptr).to_bytes() };
            if let Ok(path_str) = std::str::from_utf8(bytes) {
                changed.push(PathBuf::from(path_str));
            }
        }
        if !changed.is_empty() {
            (state.callback)(&changed);
        }
    }

    /// Wrapper so `*mut c_void` can cross the startup channel safely.
    struct SendRunLoop(CFRunLoopRef);
    // SAFETY: `CFRunLoopStop` and `CFRelease` are thread-safe CoreFoundation calls.
    unsafe impl Send for SendRunLoop {}

    pub(crate) struct WatcherGuard {
        run_loop: Option<SendRunLoop>,
        stopped: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl WatcherGuard {
        pub(crate) fn start(root: &Path, callback: FsChangeCallback) -> Option<Self> {
            let root_str = root.to_str()?;
            let c_root = CString::new(root_str).ok()?;
            let stopped = Arc::new(AtomicBool::new(false));
            let stopped_thread = Arc::clone(&stopped);
            let (ready_tx, ready_rx) = mpsc::channel::<Option<SendRunLoop>>();

            let handle = thread::Builder::new()
                .name("slim-fsevents".to_owned())
                .spawn(move || {
                    // SAFETY: All CF/FSEvent handles are checked for null and released
                    // symmetrically before the thread exits.
                    unsafe {
                        let cf_path = CFStringCreateWithCString(
                            ptr::null(),
                            c_root.as_ptr(),
                            K_CF_STRING_ENCODING_UTF8,
                        );
                        if cf_path.is_null() {
                            let _ = ready_tx.send(None);
                            return;
                        }
                        let values = [cf_path as *const c_void];
                        let cf_paths = CFArrayCreate(ptr::null(), values.as_ptr(), 1, ptr::null());
                        if cf_paths.is_null() {
                            CFRelease(cf_path);
                            let _ = ready_tx.send(None);
                            return;
                        }

                        let cb_box = Box::into_raw(Box::new(CallbackBox {
                            callback,
                            stopped: Arc::clone(&stopped_thread),
                        }));
                        let mut ctx = FSEventStreamContext {
                            version: 0,
                            info: cb_box as *mut c_void,
                            retain: None,
                            release: None,
                            copy_description: None,
                        };
                        let flags = K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER
                            | K_FS_EVENT_STREAM_CREATE_FLAG_FILE_EVENTS;
                        let stream = FSEventStreamCreate(
                            ptr::null(),
                            on_fs_events,
                            &mut ctx,
                            cf_paths,
                            K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW,
                            STREAM_LATENCY_SECS,
                            flags,
                        );
                        CFRelease(cf_paths);
                        CFRelease(cf_path);

                        if stream.is_null() {
                            drop(Box::from_raw(cb_box));
                            let _ = ready_tx.send(None);
                            return;
                        }

                        let run_loop = CFRunLoopGetCurrent();
                        CFRetain(run_loop as *const c_void);
                        FSEventStreamScheduleWithRunLoop(stream, run_loop, kCFRunLoopDefaultMode);
                        let started = FSEventStreamStart(stream);
                        if !started {
                            FSEventStreamInvalidate(stream);
                            FSEventStreamRelease(stream);
                            CFRelease(run_loop as *const c_void);
                            drop(Box::from_raw(cb_box));
                            let _ = ready_tx.send(None);
                            return;
                        }

                        let _ = ready_tx.send(Some(SendRunLoop(run_loop)));
                        CFRunLoopRun();

                        FSEventStreamStop(stream);
                        FSEventStreamInvalidate(stream);
                        FSEventStreamRelease(stream);
                        drop(Box::from_raw(cb_box));
                    }
                })
                .ok()?;

            match ready_rx.recv().ok().flatten() {
                Some(run_loop) => Some(Self {
                    run_loop: Some(run_loop),
                    stopped,
                    handle: Some(handle),
                }),
                None => {
                    let _ = handle.join();
                    None
                }
            }
        }
    }

    impl Drop for WatcherGuard {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            if let Some(rl) = self.run_loop.take() {
                // SAFETY: `rl.0` was retained in the watcher thread and is released here.
                unsafe {
                    CFRunLoopStop(rl.0);
                    CFRelease(rl.0 as *const c_void);
                }
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod fallback {
    use std::path::Path;

    use super::FsChangeCallback;

    pub(crate) struct WatcherGuard;

    impl WatcherGuard {
        pub(crate) fn start(_root: &Path, _callback: FsChangeCallback) -> Option<Self> {
            None
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) use macos::WatcherGuard;

#[cfg(not(target_os = "macos"))]
pub(crate) use fallback::WatcherGuard;

pub(crate) fn start_watcher(root: &Path, callback: FsChangeCallback) -> Option<WatcherGuard> {
    WatcherGuard::start(root, callback)
}
