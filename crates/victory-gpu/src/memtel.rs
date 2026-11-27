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
pub(crate) fn track(resource: &ID3D12Resource, kind: usize, bytes: u64, site: &'static std::panic::Location<'static>) {
    let Ok(notifier) = resource.cast::<ID3DDestructionNotifier>() else {
        return;
    };
    let bytes = bytes.max(1).next_multiple_of(64 << 10);
    let at = site_index(site);
    // The kind in the low two bits, the site in the next eight, the bytes above: no
    // allocation for the callback's data.
    let data = (bytes << 10) | ((at as u64) << 2) | kind as u64;
    ALIVE[kind].fetch_add(bytes, Relaxed);
    BUFFERS[kind].fetch_add(1, Relaxed);
    SITE_ALIVE[at].fetch_add(bytes, Relaxed);
    SITE_BUFFERS[at].fetch_add(1, Relaxed);
    // SAFETY: a live resource; the callback takes a plain number, not a pointer to anything.
    if unsafe { notifier.RegisterDestructionCallback(Some(destroyed), data as usize as *const _) }.is_err() {
        ALIVE[kind].fetch_sub(bytes, Relaxed);
        BUFFERS[kind].fetch_sub(1, Relaxed);
        SITE_ALIVE[at].fetch_sub(bytes, Relaxed);
        SITE_BUFFERS[at].fetch_sub(1, Relaxed);
    }
}

/// (Neptune) The most call sites told apart; later ones share the last.
const SITES: usize = 256;
/// Bytes and buffers alive by the call site that asked a factory for them ([`by_site`]).
static SITE_ALIVE: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static SITE_BUFFERS: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
/// The call sites, in the order first seen.
static SITE_NAMES: std::sync::Mutex<Vec<&'static std::panic::Location<'static>>> = std::sync::Mutex::new(Vec::new());

fn site_index(site: &'static std::panic::Location<'static>) -> usize {
    let mut names = SITE_NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(at) = names.iter().position(|s| s.file() == site.file() && s.line() == site.line()) {
        return at;
    }
    if names.len() < SITES {
        names.push(site);
    }
    names.len() - 1
}

/// (Neptune) For the memory minute: bytes and buffers alive by the call site that made them
/// (`file:line`), largest first.
pub(crate) fn by_site() -> Vec<(String, u64, u64)> {
    let names = SITE_NAMES.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut out: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(at, s)| {
            let file = s.file().rsplit(['/', '\\']).next().unwrap_or(s.file());
            (format!("{file}:{}", s.line()), SITE_ALIVE[at].load(Relaxed), SITE_BUFFERS[at].load(Relaxed))
        })
        .filter(|s| s.1 > 0)
        .collect();
    out.sort_unstable_by_key(|s| std::cmp::Reverse(s.1));
    out
}

/// D3D12's call as a counted buffer is destroyed (on whatever thread let it go last).
unsafe extern "system" fn destroyed(data: *mut core::ffi::c_void) {
    let data = data as usize as u64;
    let kind = (data & 3) as usize;
    let at = ((data >> 2) & 0xff) as usize;
    if kind < ALIVE.len() {
        ALIVE[kind].fetch_sub(data >> 10, Relaxed);
        BUFFERS[kind].fetch_sub(1, Relaxed);
        SITE_ALIVE[at].fetch_sub(data >> 10, Relaxed);
        SITE_BUFFERS[at].fetch_sub(1, Relaxed);
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

    /// The process's private bytes.
    fn private() -> u64 {
        use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
        // SAFETY: our own process and a sized structure.
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast::<PROCESS_MEMORY_COUNTERS>(), c.cb) };
        c.PrivateUsage as u64
    }

    /// What the card's memory and command memory cost in the process's private bytes, on the
    /// card itself (`VICTORY_MEMTEL_CARD=1 cargo test -p victory-gpu memtel -- --ignored
    /// --nocapture`): 2 GB of buffers in the card's memory, then 300 command allocators with a
    /// list each that recorded a few thousand commands. Prints what each step added.
    #[test]
    #[ignore]
    fn card_what_card_memory_and_command_memory_cost_in_private_bytes() {
        if std::env::var_os("VICTORY_MEMTEL_CARD").is_none_or(|v| v != "1") {
            return;
        }
        // SAFETY: plain object creation on the default adapter.
        let device = unsafe {
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).unwrap();
            device.unwrap()
        };
        let mb = |v: u64| v as i64 >> 20;
        let start = private();
        let buffer = |heap: D3D12_HEAP_TYPE, bytes: u64| {
            let heap = D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: bytes,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut b: Option<ID3D12Resource> = None;
            let state = if heap.Type == D3D12_HEAP_TYPE_UPLOAD { D3D12_RESOURCE_STATE_GENERIC_READ } else { D3D12_RESOURCE_STATE_COMMON };
            // SAFETY: plain creation.
            unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut b).unwrap() };
            b.unwrap()
        };
        let card: Vec<_> = (0..4).map(|_| buffer(D3D12_HEAP_TYPE_DEFAULT, 512 << 20)).collect();
        let after_card = private();
        let upload = buffer(D3D12_HEAP_TYPE_UPLOAD, 512 << 20);
        let after_upload = private();
        let mut lists = Vec::new();
        for _ in 0..300 {
            // SAFETY: plain creation and recording of a list that is never executed.
            unsafe {
                let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
                for _ in 0..4000 {
                    list.CopyBufferRegion(&card[0], 0, &card[1], 0, 256);
                }
                list.Close().unwrap();
                lists.push((allocator, list));
            }
        }
        let after_lists = private();
        println!(
            "private bytes: start {} MB; +2048 MB of card buffers: +{} MB; +512 MB upload buffer: +{} MB; \
             +300 allocators with a 4,000-command list each: +{} MB",
            mb(start),
            mb(after_card) - mb(start),
            mb(after_upload) - mb(after_card),
            mb(after_lists) - mb(after_upload)
        );
        drop((card, upload, lists));
    }

    /// (Neptune) What small command lists cost in private bytes, on the card itself
    /// (`VICTORY_MEMTEL_CARD=1 cargo test -p victory-gpu memtel -- --ignored --nocapture`):
    /// 1,000 allocators with a list each, open and empty (as a host list waits after its
    /// reset); 1,000 with a few barriers recorded and closed (as a prelude stands); and the
    /// same allocators reset with their lists recording again.
    #[test]
    #[ignore]
    fn card_what_small_command_lists_cost_in_private_bytes() {
        if std::env::var_os("VICTORY_MEMTEL_CARD").is_none_or(|v| v != "1") {
            return;
        }
        // SAFETY: plain object creation on the default adapter.
        let device = unsafe {
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).unwrap();
            device.unwrap()
        };
        let kb = |a: u64, b: u64| (b as i64 - a as i64) / 1024 / 1000;
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 1 << 20,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut b: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut b).unwrap() };
        let buffer = b.unwrap();
        let make = || -> (ID3D12CommandAllocator, ID3D12GraphicsCommandList) {
            // SAFETY: plain creation.
            unsafe {
                let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                let list = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
                (allocator, list)
            }
        };
        let barriers = |list: &ID3D12GraphicsCommandList| {
            let uav = D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                        // SAFETY: a borrowed pointer the barrier never releases.
                        pResource: unsafe { std::mem::transmute_copy(&buffer) },
                    }),
                },
                ..Default::default()
            };
            // SAFETY: an open list; a live buffer.
            unsafe {
                for _ in 0..8 {
                    list.ResourceBarrier(&[uav.clone()]);
                }
                list.Close().unwrap();
            }
        };
        // The driver's first lists set up what every later one shares: not counted.
        let warm: Vec<_> = (0..200).map(|_| make()).collect();
        // SAFETY: an open list of ours: closed so its allocator can make others.
        unsafe { warm[0].1.Close().unwrap() };
        // Allocators alone, and lists alone (closed, all made on one allocator).
        let before = private();
        // SAFETY: plain creation.
        let allocators: Vec<ID3D12CommandAllocator> =
            (0..1000).map(|_| unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap() }).collect();
        let after_allocators = private();
        let lists: Vec<ID3D12GraphicsCommandList> = (0..1000)
            .map(|_| {
                // SAFETY: plain creation; each list is closed before the allocator makes the next.
                unsafe {
                    let list: ID3D12GraphicsCommandList =
                        device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &warm[0].0, None).unwrap();
                    list.Close().unwrap();
                    list
                }
            })
            .collect();
        let after_lists = private();
        println!(
            "private bytes: an allocator alone {} KB, a closed list alone {} KB",
            kb(before, after_allocators),
            kb(after_allocators, after_lists)
        );
        drop((allocators, lists));
        let start = private();
        let open: Vec<_> = (0..1000).map(|_| make()).collect();
        let after_open = private();
        let closed: Vec<_> = (0..1000)
            .map(|_| {
                let pair = make();
                barriers(&pair.1);
                pair
            })
            .collect();
        let after_closed = private();
        for (allocator, list) in &closed {
            // SAFETY: never executed: nothing runs from the allocator.
            unsafe {
                allocator.Reset().unwrap();
                list.Reset(allocator, None).unwrap();
            }
        }
        let after_reset = private();
        println!(
            "private bytes per allocator with its list: open and empty {} KB; 8 barriers recorded and closed {} KB; \
             those reset and recording again: {} KB less each",
            kb(start, after_open),
            kb(after_open, after_closed),
            kb(after_reset, after_closed)
        );
        drop((open, closed, buffer));
    }

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
        track(&buffer, GPU_UPLOAD, 100_000, std::panic::Location::caller());
        assert_eq!(ALIVE[GPU_UPLOAD].load(Relaxed), before + (128 << 10), "rounded up to whole 64 KiB");
        let held = buffer.clone();
        drop(buffer);
        assert_eq!(ALIVE[GPU_UPLOAD].load(Relaxed), before + (128 << 10), "another reference holds it");
        drop(held);
        assert_eq!(ALIVE[GPU_UPLOAD].load(Relaxed), before, "taken off as D3D12 destroys it");
    }
}
