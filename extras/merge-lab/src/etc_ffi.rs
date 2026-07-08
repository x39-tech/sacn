//! FFI wrapper around the reference ETC sACN DMX merger (C).
//!
//! Only the DMX-merger feature is initialized via `sacn_init_features`.

use std::os::raw::{c_int, c_void};
use std::ptr;
use std::sync::Once;

use crate::{Merger, Output, SourceIndex, MAX_SLOTS, NO_OWNER};

/// `sacn_dmx_merger_t` (an `int` handle).
type MergerHandle = c_int;

/// `SACN_FEATURE_DMX_MERGER` (bit 0).
const FEATURE_DMX_MERGER: u32 = 1 << 0;
/// `kSacnReceiverInfiniteSources`.
const INFINITE_SOURCES: c_int = 0;
/// `kSacnDmxMergerInvalid`.
const INVALID_MERGER: MergerHandle = -1;
/// `kEtcPalErrOk`.
const OK: c_int = 0;

/// Mirror of `SacnDmxMergerConfig`. Field order and types must match exactly.
#[repr(C)]
struct SacnDmxMergerConfig {
    levels: *mut u8,
    per_address_priorities: *mut u8,
    per_address_priorities_active: *mut bool,
    universe_priority: *mut u8,
    owners: *mut SourceIndex,
    source_count_max: c_int,
}

extern "C" {
    fn sacn_init_features(
        log_params: *const c_void,
        netint_config: *const c_void,
        features: u32,
    ) -> c_int;
    fn sacn_dmx_merger_create(
        config: *const SacnDmxMergerConfig,
        handle: *mut MergerHandle,
    ) -> c_int;
    fn sacn_dmx_merger_destroy(handle: MergerHandle) -> c_int;
    fn sacn_dmx_merger_add_source(merger: MergerHandle, source_id: *mut SourceIndex) -> c_int;
    fn sacn_dmx_merger_remove_source(merger: MergerHandle, source: SourceIndex) -> c_int;
    fn sacn_dmx_merger_update_levels(
        merger: MergerHandle,
        source: SourceIndex,
        new_levels: *const u8,
        new_levels_count: usize,
    ) -> c_int;
    fn sacn_dmx_merger_update_pap(
        merger: MergerHandle,
        source: SourceIndex,
        pap: *const u8,
        pap_count: usize,
    ) -> c_int;
    fn sacn_dmx_merger_update_universe_priority(
        merger: MergerHandle,
        source: SourceIndex,
        universe_priority: u8,
    ) -> c_int;
    fn sacn_dmx_merger_remove_pap(merger: MergerHandle, source: SourceIndex) -> c_int;
}

fn ensure_init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // SAFETY: passing null log/netint config is supported; only the merger
        // feature is requested, so no network init occurs.
        let res = unsafe { sacn_init_features(ptr::null(), ptr::null(), FEATURE_DMX_MERGER) };
        assert_eq!(res, OK, "sacn_init_features(DMX_MERGER) failed: {res}");
    });
}

/// The reference ETC DMX merger, driven through its C API.
///
/// The output buffers are boxed so their addresses stay stable for the merger's
/// lifetime - ETC stores raw pointers to them and writes the merge result there.
pub struct EtcMerger {
    handle: MergerHandle,
    levels: Box<[u8; MAX_SLOTS]>,
    paps: Box<[u8; MAX_SLOTS]>,
    owners: Box<[SourceIndex; MAX_SLOTS]>,
    // Required by the C API but unused by the lab; kept alive for the merger.
    _pap_active: Box<bool>,
    _universe_priority: Box<u8>,
}

impl std::fmt::Debug for EtcMerger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EtcMerger")
            .field("handle", &self.handle)
            .finish()
    }
}

impl Merger for EtcMerger {
    type Handle = SourceIndex;

    fn create() -> Self {
        ensure_init();

        let mut levels = Box::new([0u8; MAX_SLOTS]);
        let mut paps = Box::new([0u8; MAX_SLOTS]);
        let mut owners = Box::new([NO_OWNER; MAX_SLOTS]);
        let mut pap_active = Box::new(false);
        let mut universe_priority = Box::new(0u8);

        let config = SacnDmxMergerConfig {
            levels: levels.as_mut_ptr(),
            per_address_priorities: paps.as_mut_ptr(),
            per_address_priorities_active: &mut *pap_active,
            universe_priority: &mut *universe_priority,
            owners: owners.as_mut_ptr(),
            source_count_max: INFINITE_SOURCES,
        };

        let mut handle: MergerHandle = INVALID_MERGER;
        // SAFETY: config points to live boxed buffers that outlive the merger
        // (dropped in Drop, after the merger is destroyed).
        let res = unsafe { sacn_dmx_merger_create(&config, &mut handle) };
        assert_eq!(res, OK, "sacn_dmx_merger_create failed: {res}");

        Self {
            handle,
            levels,
            paps,
            owners,
            _pap_active: pap_active,
            _universe_priority: universe_priority,
        }
    }

    fn add_source(&mut self) -> SourceIndex {
        let mut id: SourceIndex = NO_OWNER;
        let res = unsafe { sacn_dmx_merger_add_source(self.handle, &mut id) };
        assert_eq!(res, OK, "sacn_dmx_merger_add_source failed: {res}");
        id
    }

    fn remove_source(&mut self, id: SourceIndex) {
        unsafe { sacn_dmx_merger_remove_source(self.handle, id) };
    }

    fn update_levels(&mut self, id: SourceIndex, levels: &[u8]) {
        unsafe {
            sacn_dmx_merger_update_levels(self.handle, id, levels.as_ptr(), levels.len());
        }
    }

    fn update_pap(&mut self, id: SourceIndex, pap: &[u8]) {
        unsafe {
            sacn_dmx_merger_update_pap(self.handle, id, pap.as_ptr(), pap.len());
        }
    }

    fn update_universe_priority(&mut self, id: SourceIndex, priority: u8) {
        unsafe {
            sacn_dmx_merger_update_universe_priority(self.handle, id, priority);
        }
    }

    fn remove_pap(&mut self, id: SourceIndex) {
        unsafe {
            sacn_dmx_merger_remove_pap(self.handle, id);
        }
    }

    fn output(&self) -> Output<'_> {
        Output {
            levels: &self.levels[..],
            paps: &self.paps[..],
            owners: &self.owners[..],
        }
    }
}

impl Drop for EtcMerger {
    fn drop(&mut self) {
        unsafe { sacn_dmx_merger_destroy(self.handle) };
    }
}
