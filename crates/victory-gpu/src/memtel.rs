//! (Neptune) Our CPU-visible D3D12 memory, alive: the buffers the host's factories make
//! (`Host::upload_buffer`, `Host::shared_buffer`, `Host::gpu_upload_buffer`), counted when made
//! and taken off when D3D12 destroys them (`ID3DDestructionNotifier`), whoever held them last.
//! Beside the system's count of the process's memory shared with the card (`Host::video_memory`)
//! this tells our buffers from what the driver allocates for itself (command memory and the
//! like) and from the few buffers made elsewhere.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use windows::Win32::Graphics::Direct3D::ID3DDestructionNotifier;
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use windows::core::Interface;

/// Upload heaps (write-combined, the PC's memory).
pub(crate) const UPLOAD: usize = 0;
/// Custom write-back heaps in the PC's memory (`Host::shared_buffer`: host copies' CPU sides,
/// fetches, results).
pub(crate) const SHARED: usize = 1;
/// GPU upload heaps (the card's memory, CPU-visible through Resizable BAR).
pub(crate) const GPU_UPLOAD: usize = 2;

/// Bytes alive by kind (rounded up to the 64 KiB a committed buffer takes), and buffers.
pub(crate) static ALIVE: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
pub(crate) static BUFFERS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Counts `resource`, a buffer of `bytes` of `kind`, until D3D12 destroys it. A resource that
/// cannot tell of its destruction is not counted at all (so nothing is counted that never
/// goes).
pub(crate) fn track(resource: &ID3D12Resource, kind: usize, bytes: u64) {
    let Ok(notifier) = resource.cast::<ID3DDestructionNotifier>() else {
        return;
    };
    let bytes = bytes.max(1).next_multiple_of(64 << 10);
    // The kind in the low two bits, the bytes above: no allocation for the callback's data.
    let data = (bytes << 2) | kind as u64;
    ALIVE[kind].fetch_add(bytes, Relaxed);
    BUFFERS[kind].fetch_add(1, Relaxed);
    // SAFETY: a live resource; the callback takes a plain number, not a pointer to anything.
    if unsafe { notifier.RegisterDestructionCallback(Some(destroyed), data as usize as *const _) }.is_err() {
        ALIVE[kind].fetch_sub(bytes, Relaxed);
        BUFFERS[kind].fetch_sub(1, Relaxed);
    }
}

/// D3D12's call as a counted buffer is destroyed (on whatever thread let it go last).
unsafe extern "system" fn destroyed(data: *mut core::ffi::c_void) {
    let data = data as usize as u64;
    let kind = (data & 3) as usize;
    if kind < ALIVE.len() {
        ALIVE[kind].fetch_sub(data >> 2, Relaxed);
        BUFFERS[kind].fetch_sub(1, Relaxed);
    }
}

/// Bytes alive by kind.
pub(crate) fn alive() -> [u64; 3] {
    [0, 1, 2].map(|k| ALIVE[k].load(Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::Win32::Graphics::Dxgi::*;

    /// A tracked buffer counts while it lives, whoever lets it go, and not after.
    #[test]
    fn warp_a_tracked_buffer_counts_until_it_is_destroyed() {
        // SAFETY: plain object creation on the software adapter.
        let device = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)) else { return };
            let Ok(adapter) = factory.EnumWarpAdapter::<IDXGIAdapter>() else { return };
            let mut device: Option<ID3D12Device> = None;
            if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).is_err() {
                return;
            }
            device.unwrap()
        };
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 100_000,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_GENERIC_READ, None, &mut buffer).unwrap() };
        let buffer = buffer.unwrap();
        // The kind no factory uses in tests running beside this one: GPU upload heaps.
        let before = ALIVE[GPU_UPLOAD].load(Relaxed);
        track(&buffer, GPU_UPLOAD, 100_000);
        assert_eq!(ALIVE[GPU_UPLOAD].load(Relaxed), before + (128 << 10), "rounded up to whole 64 KiB");
        let held = buffer.clone();
        drop(buffer);
        assert_eq!(ALIVE[GPU_UPLOAD].load(Relaxed), before + (128 << 10), "another reference holds it");
        drop(held);
        assert_eq!(ALIVE[GPU_UPLOAD].load(Relaxed), before, "taken off as D3D12 destroys it");
    }
}
