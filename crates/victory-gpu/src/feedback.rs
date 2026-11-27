//! (Neptune) Bindless usage feedback (`VICTORY_BINDLESS_FEEDBACK=1`; off by default).
//!
//! Shaders pick most of the title's textures by index from the view heap: nothing on the CPU
//! says whether a frame read one, so a texture only such slots show was never let go (3.3-3.5
//! GB of the card held "idle" in the open world, every one of them blocked from the slot
//! eviction). With the switch the translator (`victory_shader::emit::feedback_slot`) has every
//! texture a shader takes from the view heap store 1 into dword `index` of a raw buffer at the
//! slot below the null slot. Every `VICTORY_BINDLESS_FEEDBACK_EVERY` frames (4) a list ahead of
//! a submission copies the buffer down and clears it; when the card has run it, each slot found
//! read gets the frame of that copy as the last frame a shader read it ([`seen_since`]).
//!
//! The slot eviction (`crate::resource::slot_evict`) may then take a texture that only such
//! slots show, once none of them was read for `VICTORY_BINDLESS_IDLE_S` seconds (45) and the
//! feedback has watched that whole time ([`covers`]). Its slots are parked
//! (`crate::descriptor::host::HostHeap::park`): the texture queue does not make them again by
//! itself; a slot found read here again is unparked and due at once ([`land`]).
//!
//! The buffer rests in the common state between submissions, as the detile ring does: the
//! copy list goes COMMON -> COPY_SOURCE (copied down) -> COPY_DEST (zeros copied in) -> COMMON,
//! and the title's shaders after it in the same call take it from COMMON as a UAV.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_TYPELESS;
use windows::core::Interface;

/// Whether shaders note the textures they take from the view heap.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BINDLESS_FEEDBACK").is_some_and(|v| v == "1"))
}

/// Frames between copies of the buffer (`VICTORY_BINDLESS_FEEDBACK_EVERY`, 4; 1 to 64).
fn every() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_BINDLESS_FEEDBACK_EVERY").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(4).clamp(1, 64)
    })
}

/// How long none of a texture's slots may have been read before the eviction may take it
/// (`VICTORY_BINDLESS_IDLE_S`, 60 s; 5 to 600), in milliseconds.
pub(crate) fn idle_ms() -> u64 {
    static S: OnceLock<u64> = OnceLock::new();
    *S.get_or_init(|| std::env::var("VICTORY_BINDLESS_IDLE_S").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(60).clamp(5, 600))
        * 1000
}

/// Since the start: copies landed, slots found read in them (summed), slots unparked, and slots
/// of textures let go unread that the queue came to and parked instead of making them.
pub(crate) static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Per view heap slot: the frame (plus one) of the copy that last found it read; 0 never.
static LAST_SEEN: OnceLock<Vec<AtomicU32>> = OnceLock::new();

/// The frame of the first copy that landed: what the feedback has watched since (0: none).
static COVERED_FROM: AtomicU64 = AtomicU64::new(0);

/// Whether a shader read view heap slot `slot` after frame `cutoff`, as far as the copies
/// landed so far say.
pub(crate) fn seen_since(slot: u32, cutoff: u64) -> bool {
    LAST_SEEN
        .get()
        .and_then(|s| s.get(slot as usize))
        .is_some_and(|f| u64::from(f.load(Relaxed)) > cutoff + 1)
}

/// Whether the feedback has watched every frame since `cutoff`: a slot it never found read
/// since then was not read.
pub(crate) fn covers(cutoff: u64) -> bool {
    let from = COVERED_FROM.load(Relaxed);
    from != 0 && from <= cutoff
}

struct Back {
    buffer: ID3D12Resource,
    data: usize,
    /// The fence value of the submission that copied into it: 0 free, `u64::MAX` recorded
    /// and not sent yet.
    fence: u64,
    /// The frame the copy was recorded in.
    frame: u64,
}

struct State {
    buffer: ID3D12Resource,
    zeros: ID3D12Resource,
    slots: u32,
    backs: Vec<Back>,
    lists: Vec<(ID3D12CommandAllocator, ID3D12GraphicsCommandList, u64)>,
    last: u64,
}

// SAFETY: D3D12 objects (free-threaded) and persistent mappings, used under the lock.
unsafe impl Send for State {}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// The feedback buffer, made on the first ask (with `slots` dwords: the view heap's slots);
/// `None` without the switch or if it could not be made.
fn buffer(slots: u32) -> Option<ID3D12Resource> {
    if !on() {
        return None;
    }
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = guard.as_ref() {
        return Some(s.buffer.clone());
    }
    let host = crate::host::host()?;
    let bytes = u64::from(slots) * 4;
    let buffer = host.uav_buffer(bytes)?;
    crate::host::name(&buffer, || "bindless feedback".into());
    let (zeros, data) = host.upload_buffer(bytes)?;
    // SAFETY: the upload buffer's mapping, `bytes` long.
    unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    crate::host::name(&zeros, || "bindless feedback zeros".into());
    let mut backs = Vec::new();
    for _ in 0..3 {
        let (b, d) = host.shared_buffer(bytes)?;
        crate::host::name(&b, || "bindless feedback read-back".into());
        backs.push(Back { buffer: b, data: d as usize, fence: 0, frame: 0 });
    }
    let _ = LAST_SEEN.set((0..slots).map(|_| AtomicU32::new(0)).collect());
    tracing::info!(slots, every = every(), idle_s = idle_ms() / 1000, "VICTORY_BINDLESS_FEEDBACK=1: shaders note the textures they take from the view heap");
    *guard = Some(State { buffer: buffer.clone(), zeros, slots, backs, lists: Vec::new(), last: 0 });
    Some(buffer)
}

/// For a view heap just made null (`HostHeap::new`): writes the feedback buffer's view into
/// slot `slot` at `at` (a raw UAV over all of it). Whether it did.
pub(crate) fn install(at: D3D12_CPU_DESCRIPTOR_HANDLE, slot: u32) -> bool {
    let Some(host) = crate::host::host() else {
        return false;
    };
    // One dword per slot up to and with the null slot.
    let slots = slot + 2;
    let Some(buffer) = buffer(slots) else {
        return false;
    };
    let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
        Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_UAV { NumElements: slots, Flags: D3D12_BUFFER_UAV_FLAG_RAW, ..Default::default() },
        },
    };
    // SAFETY: a slot of a live view heap; a live buffer of `slots` dwords.
    unsafe { host.device.CreateUnorderedAccessView(&buffer, None, Some(&desc), at) };
    true
}

/// At submission, after the steps: lands the copies the card has run, and every
/// [`every`] frames the list that copies the buffer down and clears it, to go first in the
/// submission's call.
pub(crate) fn pre_list() -> Option<ID3D12CommandList> {
    let host = crate::host::host()?;
    let completed = host.completed();
    let frame = crate::frame::current();
    let mut seen_now: Vec<u32> = Vec::new();
    let list = {
        let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let state = guard.as_mut()?;
        land(state, completed, &mut seen_now);
        record(host, state, frame, completed)
    };
    if !seen_now.is_empty() {
        let n = crate::descriptor::unpark_seen(&seen_now);
        STATS[2].fetch_add(n, Relaxed);
    }
    list
}

/// The copies the card has run: each slot found read gets the copy's frame; the slots found
/// read that are parked go into `parked`.
fn land(state: &mut State, completed: u64, seen: &mut Vec<u32>) {
    let Some(last) = LAST_SEEN.get() else { return };
    for back in state.backs.iter_mut().filter(|b| b.fence != 0 && b.fence != u64::MAX && b.fence <= completed) {
        // SAFETY: the read-back buffer's mapping, `slots` dwords, written by a list that has run.
        let words = unsafe { std::slice::from_raw_parts(back.data as *const u32, state.slots as usize) };
        let stamp = (back.frame + 1).min(u64::from(u32::MAX)) as u32;
        let mut found = 0u64;
        for (slot, &word) in words.iter().enumerate() {
            if word != 0 {
                found += 1;
                if let Some(f) = last.get(slot) {
                    f.store(stamp, Relaxed);
                }
                seen.push(slot as u32);
            }
        }
        STATS[0].fetch_add(1, Relaxed);
        STATS[1].fetch_add(found, Relaxed);
        if COVERED_FROM.load(Relaxed) == 0 {
            COVERED_FROM.store(back.frame.max(1), Relaxed);
        }
        back.fence = 0;
    }
}

/// The copy list, if one is due and a read-back buffer is free.
fn record(host: &crate::host::Host, state: &mut State, frame: u64, completed: u64) -> Option<ID3D12CommandList> {
    if state.last != 0 && frame < state.last + every() {
        return None;
    }
    let back = state.backs.iter().position(|b| b.fence == 0)?;
    let at = match state.lists.iter().position(|l| l.2 <= completed) {
        Some(at) => at,
        None => {
            // SAFETY: plain object creation.
            let made: windows::core::Result<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> = unsafe {
                (|| {
                    let allocator: ID3D12CommandAllocator = host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                    let list: ID3D12GraphicsCommandList = host.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                    list.Close()?;
                    Ok((allocator, list))
                })()
            };
            match made {
                Ok((a, l)) => {
                    state.lists.push((a, l, 0));
                    state.lists.len() - 1
                }
                Err(e) => {
                    tracing::warn!(error = %e, "bindless feedback: no list for the copy");
                    return None;
                }
            }
        }
    };
    let bytes = u64::from(state.slots) * 4;
    let (allocator, list, _) = &state.lists[at];
    let (b, rb, z) = (&state.buffer, &state.backs[back].buffer, &state.zeros);
    let t = crate::host::transition;
    let (common, source, dest) = (D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
    // SAFETY: the allocator's last list has run; live buffers of `bytes` bytes.
    let recorded = unsafe {
        (|| -> windows::core::Result<()> {
            allocator.Reset()?;
            list.Reset(allocator, None)?;
            list.ResourceBarrier(&[t(b, common, source)]);
            list.CopyBufferRegion(rb, 0, b, 0, bytes);
            list.ResourceBarrier(&[t(b, source, dest)]);
            list.CopyBufferRegion(b, 0, z, 0, bytes);
            list.ResourceBarrier(&[t(b, dest, common)]);
            list.Close()
        })()
    };
    if let Err(e) = recorded {
        tracing::warn!(error = %e, "bindless feedback: the copy could not be recorded");
        return None;
    }
    let out = list.cast().ok();
    state.backs[back].fence = u64::MAX;
    state.backs[back].frame = frame;
    state.lists[at].2 = u64::MAX;
    state.last = frame;
    out
}

/// After the submission is sent with fence value `fence`.
pub(crate) fn sent(fence: u64) {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = guard.as_mut() {
        for b in state.backs.iter_mut().filter(|b| b.fence == u64::MAX) {
            b.fence = fence;
        }
        for l in state.lists.iter_mut().filter(|l| l.2 == u64::MAX) {
            l.2 = fence;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slot read at frame 100 is seen since frame 99, not since frame 100; coverage starts at
    /// the first landed copy.
    #[test]
    fn seen_since_and_covers_go_by_the_copies_frames() {
        let _ = LAST_SEEN.set((0..8).map(|_| AtomicU32::new(0)).collect());
        let last = LAST_SEEN.get().unwrap();
        last[3].store(101, Relaxed);
        assert!(seen_since(3, 99));
        assert!(!seen_since(3, 100));
        assert!(!seen_since(4, 0), "never read");
        assert!(!seen_since(1_000_000, 0), "past the heap");
        assert!(!covers(500), "nothing landed yet");
        COVERED_FROM.store(200, Relaxed);
        assert!(covers(200) && covers(900));
        assert!(!covers(150), "not watched since 150");
        COVERED_FROM.store(0, Relaxed);
    }
}
