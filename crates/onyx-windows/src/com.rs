//! RAII COM apartment initialization guard.

use onyx_core::{OnyxError, OnyxResult};
use std::marker::PhantomData;
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};

/// RAII guard for COM STA (`COINIT_APARTMENTTHREADED`) initialization on the current thread.
///
/// Calls `CoUninitialize` on drop **only** if this guard succeeded in initializing COM
/// (`S_OK` or `S_FALSE`). When the current thread was already initialized with a different
/// concurrency model (`RPC_E_CHANGED_MODE`), initialization is tolerated without calling
/// `CoUninitialize` on drop.
///
/// This type is intentionally `!Send` and `!Sync` because COM initialization and
/// uninitialization must occur on the same OS thread.
#[derive(Debug)]
pub struct ComGuard {
    should_uninit: bool,
    _not_send_sync: PhantomData<*mut ()>,
}

impl ComGuard {
    /// Initializes COM in STA mode (`COINIT_APARTMENTTHREADED`) for the calling thread.
    pub fn init_sta() -> OnyxResult<Self> {
        // SAFETY: `CoInitializeEx` is safe to invoke on any thread with a null reserved pointer
        // and valid `COINIT_APARTMENTTHREADED` flag. Matching `CoUninitialize` is paired in `Drop`
        // strictly when `S_OK` or `S_FALSE` is returned.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

        if hr == S_OK || hr == S_FALSE {
            Ok(Self {
                should_uninit: true,
                _not_send_sync: PhantomData,
            })
        } else if hr == RPC_E_CHANGED_MODE {
            tracing::debug!("COM already initialized with a different apartment mode on thread");
            Ok(Self {
                should_uninit: false,
                _not_send_sync: PhantomData,
            })
        } else {
            Err(OnyxError::Platform {
                message: format!("CoInitializeEx failed with HRESULT 0x{:08X}", hr.0 as u32),
            })
        }
    }

    /// Returns `true` if this guard owns an STA initialization count and will call `CoUninitialize` on drop.
    #[must_use]
    pub const fn owns_initialization(&self) -> bool {
        self.should_uninit
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            // SAFETY: `should_uninit` is true only when `CoInitializeEx` returned `S_OK` or `S_FALSE`
            // on this exact thread (`ComGuard` is `!Send`), so pairing with `CoUninitialize` is sound.
            unsafe {
                CoUninitialize();
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn com_guard_init_and_reentrant_sta() {
        let handle = std::thread::spawn(|| {
            let guard1 = ComGuard::init_sta().expect("first STA init should succeed");
            assert!(guard1.owns_initialization());

            let guard2 = ComGuard::init_sta().expect("reentrant STA init should succeed");
            assert!(guard2.owns_initialization());

            drop(guard2);
            drop(guard1);
        });
        handle.join().expect("STA test thread should not panic");
    }

    #[test]
    fn com_guard_tolerates_rpc_e_changed_mode() {
        use windows::Win32::System::Com::COINIT_MULTITHREADED;

        let handle = std::thread::spawn(|| {
            // SAFETY: Test-only MTA init paired with CoUninitialize below on this dedicated thread.
            let mta_hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            assert!(mta_hr == S_OK || mta_hr == S_FALSE);

            let sta_guard = ComGuard::init_sta().expect("RPC_E_CHANGED_MODE must be tolerated");
            assert!(!sta_guard.owns_initialization());
            drop(sta_guard);

            // SAFETY: Pairs with the test-only MTA CoInitializeEx above.
            unsafe {
                CoUninitialize();
            }
        });
        handle
            .join()
            .expect("MTA compatibility test thread should not panic");
    }
}
