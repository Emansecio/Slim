//! Lock helpers that recover the guard when a panicking holder poisoned the
//! lock: the protected state stays usable instead of cascading the panic.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub(crate) fn lock_mutex<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn read_lock<T>(value: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    value
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn write_lock<T>(value: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    value
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
