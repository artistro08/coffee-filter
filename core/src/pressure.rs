//! Keeping the proxy fast when the PC is busy, and small when memory runs low.
//!
//! Every web request goes through this process, so it must never be the slow part:
//! Windows' efficiency mode (which slows processes it thinks are in the background, as a
//! windowless process looks) is switched off. Priority stays normal: a raised priority made
//! no measurable difference under full CPU load and could only take time from other apps.
//! Heavy work that can wait, like parsing filter lists, runs at below-normal CPU priority so
//! it gives way to everything else (CPU only: its memory and disk priority stay normal, so
//! the engine it builds isn't the first thing Windows pages out). When Windows reports low
//! memory, caches that are cheap to rebuild are dropped.

use crate::filter::Shared;
use std::{sync::Arc, time::Duration};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
    System::{
        Memory::{CreateMemoryResourceNotification, LowMemoryResourceNotification},
        Threading::{
            GetCurrentProcess, GetCurrentThread, INFINITE, PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
            THREAD_PRIORITY_NORMAL, WaitForSingleObject,
        },
    },
};

/// Out of efficiency mode. Best effort: if Windows refuses, the proxy still works.
pub fn keep_fast() {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version:     PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        // Taking control of execution speed and leaving it off means "never throttle"
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask:   0,
    };
    // SAFETY: the current-process pseudo handle and a correctly sized, initialized struct.
    unsafe {
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            std::ptr::from_ref(&state).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

/// Puts the calling thread back to normal priority when dropped, so a pooled thread never
/// stays lowered, even if the work panics.
struct NormalAgain;

impl Drop for NormalAgain {
    fn drop(&mut self) {
        // SAFETY: the calling thread's pseudo handle.
        let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_NORMAL) };
    }
}

/// Run `work` at below-normal CPU priority on this thread, then back to normal.
pub fn below_normal<T>(work: impl FnOnce() -> T) -> T {
    // SAFETY: the calling thread's pseudo handle; reset by the guard below.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    let _normal_again = NormalAgain;
    work()
}

/// Watch for Windows' low-memory signal on a thread of its own. When it comes, drop caches
/// that are cheap to rebuild, at most once a minute. (Handing back the whole working set
/// would only make the next requests page the proxy back in while memory is still short.)
pub fn watch_memory(shared: Arc<Shared>) {
    // SAFETY: creates a notification handle owned by the watcher thread below.
    let Ok(low) = (unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification) }) else { return };
    let low = low.0 as usize;
    std::thread::spawn(move || {
        let low = HANDLE(low as *mut _);
        loop {
            // SAFETY: a valid handle, kept open for the life of this thread.
            if unsafe { WaitForSingleObject(low, INFINITE) } != WAIT_OBJECT_0 {
                break;
            }
            shared.app_cache.clear();
            shared.log.trim();
            // The signal stays on while memory is low; don't spin on it
            std::thread::sleep(Duration::from_secs(60));
        }
        // SAFETY: the handle from CreateMemoryResourceNotification above.
        let _ = unsafe { CloseHandle(low) };
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_normal_work_returns_its_result_and_resets_priority() {
        use windows::Win32::System::Threading::GetThreadPriority;

        assert_eq!(below_normal(|| 2 + 2), 4);
        // SAFETY: the calling thread's pseudo handle.
        assert_eq!(unsafe { GetThreadPriority(GetCurrentThread()) }, THREAD_PRIORITY_NORMAL.0, "back to normal afterward");

        // Even when the work panics
        let _ = std::panic::catch_unwind(|| below_normal(|| panic!("boom")));
        // SAFETY: as above.
        assert_eq!(unsafe { GetThreadPriority(GetCurrentThread()) }, THREAD_PRIORITY_NORMAL.0, "back to normal after a panic");
    }
}
