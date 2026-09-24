//! Per-thread COM initialization.

use anyhow::Result;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// Joins the calling thread to the multithreaded apartment for its lifetime.
///
/// Every thread that creates WIC or shell COM objects should hold one. A
/// thread without it only works through the process's implicit MTA, which
/// disappears as soon as the last explicitly initialized thread leaves; its
/// COM objects then crash.
pub struct ComApartment {
    /// False when the thread was already in a single-threaded apartment; COM
    /// still works there and this guard must not uninitialize it.
    owned: bool,
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl ComApartment {
    pub fn initialize() -> Result<Self> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let owned = if result == RPC_E_CHANGED_MODE {
            false
        } else if result.is_err() {
            anyhow::bail!("CoInitializeEx failed: {result:?}");
        } else {
            true
        };
        Ok(Self {
            owned,
            _not_send: std::marker::PhantomData,
        })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owned {
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_initialization_balances() {
        std::thread::spawn(|| {
            let outer = ComApartment::initialize().unwrap();
            let inner = ComApartment::initialize().unwrap();
            assert!(outer.owned && inner.owned);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn single_threaded_apartment_is_left_alone() {
        use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;
        std::thread::spawn(|| {
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
                .ok()
                .unwrap();
            let guard = ComApartment::initialize().unwrap();
            assert!(!guard.owned);
            drop(guard);
            unsafe { CoUninitialize() };
        })
        .join()
        .unwrap();
    }
}
