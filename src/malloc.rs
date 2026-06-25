/*
Copyright 2025 The Hyperlight Authors.
Modifications Copyright 2026 The picolibc-rs Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

//! `malloc`/`free`/`realloc`/`calloc`/`aligned_alloc`/`memalign`/`posix_memalign`
//! backed by the Rust global allocator.
//!
//! Ported from [`hyperlight_guest_bin::memory`](https://github.com/hyperlight-dev/hyperlight/blob/main/src/hyperlight_guest_bin/src/memory.rs)
//! (Apache-2.0). Adaptations for `picolibc-rs`:
//! - Removed hyperlight-specific `abort_with_code`; OOM/invalid-layout → returns null.
//! - `calloc` overflow → returns null instead of panicking.
//! - Added `memalign` and `posix_memalign` wrappers.
//! - `#[unsafe(no_mangle)]` → `#[no_mangle]` (MSRV 1.74 compatibility).
//!
//! # Memory layout
//!
//! Every allocation stores a [`Layout`] header immediately before the pointer
//! returned to C, so `free`/`realloc` can recover the original size and
//! alignment without a separate metadata map.
//!
//! ```text
//! ┌─────────────────────────────┬──────────────────────────┐
//! │  Header (Layout)            │  user data (size bytes)  │ …
//! └─────────────────────────────┴──────────────────────────┘
//!                               ^
//!                               ptr returned to C
//! ```
//!
//! `data_offset = HEADER_LEN.next_multiple_of(alignment)`, so the user pointer
//! satisfies the requested alignment even when `alignment > align_of::<Header>()`.
//!
//! # Requirements
//!
//! The final binary must register a `#[global_allocator]`. Baremetal / UEFI
//! targets have no default allocator.

extern crate alloc;

use core::alloc::Layout;
use core::ffi::{c_int, c_void};
use core::mem::{align_of, size_of};
use core::ptr;

// Maximum natural alignment on all supported platforms (same as Hyperlight).
const DEFAULT_ALIGN: usize = align_of::<u128>();
const HEADER_LEN: usize = size_of::<Header>();

// POSIX errno values (standard across all picolibc targets).
const EINVAL: c_int = 22;
const ENOMEM: c_int = 12;

#[repr(transparent)]
struct Header(Layout);

/// Core allocator. Stores a `Header` before the returned pointer so that
/// `free`/`realloc` can reconstruct the original `Layout`.
///
/// Returns null on size == 0, overflow, invalid layout, or OOM.
unsafe fn alloc_helper(size: usize, alignment: usize, zero: bool) -> *mut c_void {
    if size == 0 {
        return ptr::null_mut();
    }

    let actual_align = alignment.max(align_of::<Header>());
    let data_offset = HEADER_LEN.next_multiple_of(actual_align);

    let total_size = match data_offset.checked_add(size) {
        Some(n) => n,
        None => return ptr::null_mut(),
    };

    let layout = match Layout::from_size_align(total_size, actual_align) {
        Ok(l) => l,
        Err(_) => return ptr::null_mut(),
    };

    unsafe {
        let raw_ptr = if zero {
            alloc::alloc::alloc_zeroed(layout)
        } else {
            alloc::alloc::alloc(layout)
        };

        if raw_ptr.is_null() {
            return ptr::null_mut();
        }

        // Write header just before the user region.
        let header_ptr = raw_ptr.add(data_offset - HEADER_LEN).cast::<Header>();
        header_ptr.write(Header(layout));
        raw_ptr.add(data_offset) as *mut c_void
    }
}

/// Recover the `Header` written by [`alloc_helper`] for a non-null user pointer.
///
/// # Safety
/// `ptr` must have been returned by `alloc_helper`.
#[inline]
unsafe fn read_header(ptr: *const u8) -> Layout {
    unsafe { ptr.sub(HEADER_LEN).cast::<Header>().read().0 }
}

// ---------------------------------------------------------------------------
// C ABI exports
// ---------------------------------------------------------------------------

/// C `malloc`.
#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    unsafe { alloc_helper(size, DEFAULT_ALIGN, false) }
}

/// C `calloc`: `nmemb * size` zeroed bytes. Returns null on overflow.
#[no_mangle]
pub unsafe extern "C" fn calloc(nmemb: usize, size: usize) -> *mut c_void {
    let total = match nmemb.checked_mul(size) {
        Some(n) => n,
        None => return ptr::null_mut(),
    };
    unsafe { alloc_helper(total, DEFAULT_ALIGN, true) }
}

/// C11 `aligned_alloc`. Returns null if `alignment` is zero or not a power of two.
#[no_mangle]
pub unsafe extern "C" fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void {
    if alignment == 0 || !alignment.is_power_of_two() {
        return ptr::null_mut();
    }
    unsafe { alloc_helper(size, alignment, false) }
}

/// POSIX `memalign` — identical to `aligned_alloc` with the same argument order.
#[no_mangle]
pub unsafe extern "C" fn memalign(alignment: usize, size: usize) -> *mut c_void {
    unsafe { aligned_alloc(alignment, size) }
}

/// POSIX `posix_memalign`. Writes the allocated pointer to `*memptr`.
/// Returns 0 on success, `EINVAL` for bad alignment, `ENOMEM` on OOM.
#[no_mangle]
pub unsafe extern "C" fn posix_memalign(
    memptr: *mut *mut c_void,
    alignment: usize,
    size: usize,
) -> c_int {
    // POSIX: alignment must be a multiple of sizeof(void*) and a power of two.
    if alignment < size_of::<*mut c_void>() || !alignment.is_power_of_two() {
        return EINVAL;
    }
    let ptr = unsafe { alloc_helper(size, alignment, false) };
    if ptr.is_null() && size != 0 {
        return ENOMEM;
    }
    unsafe { *memptr = ptr };
    0
}

/// C `free`. No-op on null.
#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let user_ptr = ptr as *const u8;
    unsafe {
        let layout = read_header(user_ptr);
        let offset = HEADER_LEN.next_multiple_of(layout.align());
        let raw_ptr = user_ptr.sub(offset) as *mut u8;
        alloc::alloc::dealloc(raw_ptr, layout);
    }
}

/// C `realloc`. Null `ptr` behaves like `malloc`; zero `size` behaves like `free`.
#[no_mangle]
pub unsafe extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    if ptr.is_null() {
        return unsafe { malloc(size) };
    }
    if size == 0 {
        unsafe { free(ptr) };
        return ptr::null_mut();
    }

    let user_ptr = ptr as *const u8;
    unsafe {
        let old_layout = read_header(user_ptr);
        let old_offset = HEADER_LEN.next_multiple_of(old_layout.align());
        let old_user_size = old_layout.size() - old_offset;

        let new_ptr = alloc_helper(size, old_layout.align(), false);
        if new_ptr.is_null() {
            return ptr::null_mut();
        }

        ptr::copy_nonoverlapping(user_ptr, new_ptr as *mut u8, old_user_size.min(size));
        free(ptr);
        new_ptr
    }
}
