//! The "ram line": where the process's memory is, in release builds too.
//! (`copystat::log_memory` has the long form, in diagnostics builds only.)

use std::sync::atomic::Ordering::Relaxed;

/// Resident and shared bytes of each of `ranges`: every 16th 4 KiB page of the address space
/// (the pages at multiples of 64 KiB, so many small ranges do not each count one page as sixteen)
/// asked about (`K32QueryWorkingSetEx`, 4096 at a time, at most `CAP` pages a run), scaled up.
fn sample(ranges: &[(usize, usize)]) -> Vec<(u64, u64)> {
    use windows_sys::Win32::System::ProcessStatus::{K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const STEP: usize = 16;
    const CAP: usize = 2_000_000;
    let mut out = vec![(0u64, 0u64); ranges.len()];
    let mut batch: Vec<PSAPI_WORKING_SET_EX_INFORMATION> = Vec::with_capacity(4096);
    let mut owner: Vec<usize> = Vec::with_capacity(4096);
    let mut asked = 0usize;
    let flush = |batch: &mut Vec<PSAPI_WORKING_SET_EX_INFORMATION>, owner: &mut Vec<usize>, out: &mut Vec<(u64, u64)>| {
        // SAFETY: our own process and a sized array the call fills in.
        let ok = unsafe { K32QueryWorkingSetEx(GetCurrentProcess(), batch.as_mut_ptr().cast(), (batch.len() * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32) };
        if ok != 0 {
            for (e, &i) in batch.iter().zip(owner.iter()) {
                // SAFETY: a union of the flags word, filled in.
                let f = unsafe { e.VirtualAttributes.Flags };
                out[i].0 += ((f & 1) as u64) * (STEP as u64) * 4096;
                out[i].1 += (((f >> 15) & (f & 1)) as u64) * (STEP as u64) * 4096;
            }
        }
        batch.clear();
        owner.clear();
    };
    'all: for (i, &(start, len)) in ranges.iter().enumerate() {
        let mut at = start.next_multiple_of(STEP * 4096);
        while at < start + len {
            if asked >= CAP {
                break 'all;
            }
            // SAFETY: plain data; the address is the input.
            let mut e: PSAPI_WORKING_SET_EX_INFORMATION = unsafe { std::mem::zeroed() };
            e.VirtualAddress = at as *mut _;
            batch.push(e);
            owner.push(i);
            asked += 1;
            if batch.len() == 4096 {
                flush(&mut batch, &mut owner, &mut out);
            }
            at += STEP * 4096;
        }
    }
    if !batch.is_empty() {
        flush(&mut batch, &mut owner, &mut out);
    }
    out
}

/// Estimated (resident, shared) bytes of `ranges` together.
fn resident(ranges: &[(usize, usize)]) -> (u64, u64) {
    sample(ranges).iter().fold((0, 0), |a, r| (a.0 + r.0, a.1 + r.1))
}

/// A committed private or mapped region of the address space.
struct Region {
    base: usize,
    len: usize,
    alloc: usize,
    mapped: bool,
    protect: u32,
}

/// Committed private and mapped regions of the whole address space, by address.
fn committed() -> Vec<Region> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_MAPPED, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut out = Vec::new();
    let mut at = 0usize;
    loop {
        // SAFETY: plain data the call fills in.
        let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a query of any address is allowed.
        if unsafe { VirtualQuery(at as *const _, &mut m, size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
            break;
        }
        if m.State == MEM_COMMIT && m.Type & (MEM_PRIVATE | MEM_MAPPED) != 0 {
            out.push(Region { base: m.BaseAddress as usize, len: m.RegionSize, alloc: m.AllocationBase as usize, mapped: m.Type & MEM_MAPPED != 0, protect: m.Protect });
        }
        match (m.BaseAddress as usize).checked_add(m.RegionSize) {
            Some(next) if next > at => at = next,
            _ => break,
        }
    }
    out
}

/// The address ranges already given to a group: later groups get only what is left.
#[derive(Default)]
struct Taken(std::collections::BTreeMap<usize, usize>);

impl Taken {
    /// The parts of `[s, e)` nobody has, which are now taken.
    fn take(&mut self, s: usize, e: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut at = s;
        let from = self.0.range(..=s).next_back().filter(|(_, en)| **en > s).map_or(s, |(b, _)| *b);
        for (&b, &en) in self.0.range(from..e) {
            if b > at {
                out.push((at, b));
            }
            at = at.max(en);
            if at >= e {
                break;
            }
        }
        if at < e {
            out.push((at, e));
        }
        for &(a, b) in &out {
            self.0.insert(a, b);
        }
        out
    }
}

/// The parts of `[s, e)` that lie in committed `regions`.
fn clip(regions: &[Region], s: usize, e: usize) -> Vec<(usize, usize)> {
    let first = regions.partition_point(|r| r.base + r.len <= s);
    regions[first..].iter().take_while(|r| r.base < e).map(|r| (r.base.max(s), (r.base + r.len).min(e))).collect()
}

/// Title ranges by kind, as (base, end), with stale reservations dropped: the title frees its
/// `XMemVirtualAlloc` memory itself, so a listed base that is no allocation base is gone.
fn title_ranges() -> [Vec<(usize, usize)>; 5] {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut out: [Vec<(usize, usize)>; 5] = Default::default();
    for (base, size, kind) in crate::titlemem::snapshot() {
        if kind == crate::titlemem::VIRTUAL {
            // SAFETY: plain data the call fills in.
            let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a query of any address is allowed.
            let ok = unsafe { VirtualQuery(base as *const _, &mut m, size_of::<MEMORY_BASIC_INFORMATION>()) } != 0;
            if !ok || m.AllocationBase as usize != base {
                crate::titlemem::forget_if_stale(base);
                continue;
            }
        }
        let list = &mut out[(kind as usize).min(4)];
        match list.last_mut() {
            Some(last) if last.1 == base => last.1 = base + size,
            _ => list.push((base, base + size)),
        }
    }
    out
}

/// Every 30 s, on its own thread (started by the first call of [`log`]): how much of title
/// memory, the host copies' CPU sides and the texture staging buffers is in the working set.
/// The title's own memory is what the runtime listed as it allocated it (`crate::titlemem`);
/// the rest of the committed private and mapped regions is split by range, not by totals:
/// the host copies and staging first, then the title's images, page views, virtual allocations
/// and large heap blocks, then whatever is left, private and mapped.
fn sampler() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = std::thread::Builder::new().name("resident-line".into()).spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            let started = std::time::Instant::now();
            let mb = |v: u64| v >> 20;
            let total = |r: &[(usize, usize)]| r.iter().map(|r| r.1 as u64).sum::<u64>();
            let [buffers, windows] = crate::shadow::cpu_ranges();
            let staging = crate::texture::cpu_ranges();
            let regions = committed();
            let titles = title_ranges();
            let (b, w, s) = (resident(&buffers), resident(&windows), resident(&staging));
            let mut taken = Taken::default();
            for g in [&buffers, &windows, &staging] {
                for &(at, len) in g {
                    taken.take(at, at + len);
                }
            }
            // Title kinds, in the order they claim overlapping ranges: (committed, resident) each.
            let names = ["images", "page views", "virtual allocs", "heap blocks"];
            let kinds = [crate::titlemem::IMAGE, crate::titlemem::VIEW, crate::titlemem::VIRTUAL, crate::titlemem::HEAP];
            let by_kind: Vec<(u64, u64)> = kinds
                .iter()
                .map(|&k| {
                    let pieces: Vec<(usize, usize)> = titles[k as usize]
                        .iter()
                        .flat_map(|&(a, e)| taken.take(a, e))
                        .flat_map(|(a, e)| clip(&regions, a, e))
                        .map(|(a, e)| (a, e - a))
                        .collect();
                    (total(&pieces), resident(&pieces).0)
                })
                .collect();
            // What is left of the committed regions, with the region it came from.
            let mut left: Vec<(usize, usize)> = Vec::new();
            let mut from: Vec<&Region> = Vec::new();
            for r in &regions {
                for (a, e) in taken.take(r.base, r.base + r.len) {
                    left.push((a, e - a));
                    from.push(r);
                }
            }
            let res = sample(&left);
            let (mut private, mut mapped) = ((0u64, 0u64), (0u64, 0u64));
            // By allocation base: (resident, committed, mapped, protect of the biggest piece, its size).
            let mut owners: std::collections::HashMap<usize, (u64, u64, bool, u32, usize)> = Default::default();
            for ((piece, r), (rs, _)) in left.iter().zip(&from).zip(&res) {
                let side = if r.mapped { &mut mapped } else { &mut private };
                side.0 += rs;
                side.1 += piece.1 as u64;
                let o = owners.entry(r.alloc).or_insert((0, 0, r.mapped, r.protect, 0));
                o.0 += rs;
                o.1 += piece.1 as u64;
                if piece.1 > o.4 {
                    (o.2, o.3, o.4) = (r.mapped, r.protect, piece.1);
                }
            }
            let mut top: Vec<_> = owners.into_iter().collect();
            top.sort_unstable_by_key(|(_, o)| std::cmp::Reverse(o.0));
            let top: Vec<String> = top
                .iter()
                .take(5)
                .map(|(base, o)| format!("{base:#x} {} protect {:#x} {} MB of {} MB", if o.2 { "mapped" } else { "private" }, o.3, mb(o.0), mb(o.1)))
                .collect();
            let mut title = String::new();
            for (n, (c, r)) in names.iter().zip(&by_kind) {
                title += &format!("title {n} {} MB of {} MB; ", mb(*r), mb(*c));
            }
            let mut c = windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS { cb: size_of::<windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
            // SAFETY: our own process and a sized structure.
            unsafe { windows_sys::Win32::System::ProcessStatus::K32GetProcessMemoryInfo(windows_sys::Win32::System::Threading::GetCurrentProcess(), &mut c, c.cb) };
            tracing::info!(
                "resident line: {title}other private {} MB of {} MB; other mapped {} MB of {} MB (largest: {}); \
                 host copies of buffers {} MB of {} MB ({} MB shared); windows {} MB of {} MB ({} MB shared); \
                 texture staging {} MB of {} MB ({} MB shared); process working set {} MB; sampled in {} ms; \
                 rust heap {} MB live (peak {} MB)",
                mb(private.0),
                mb(private.1),
                mb(mapped.0),
                mb(mapped.1),
                top.join(", "),
                mb(b.0),
                mb(total(&buffers)),
                mb(b.1),
                mb(w.0),
                mb(total(&windows)),
                mb(w.1),
                mb(s.0),
                mb(total(&staging)),
                mb(s.1),
                mb(c.WorkingSetSize as u64),
                started.elapsed().as_millis(),
                mb(victory_core::heapcount::sample()),
                mb(victory_core::heapcount::RUST_HEAP[1].load(Relaxed)),
            );
            // (Neptune) Every other pass (once a minute): the memory minute, from this pass.
            static PASSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            if PASSES.fetch_add(1, Relaxed) % 2 == 1 {
                let k = |i: usize| (by_kind[i].0, by_kind[i].1);
                minute(&Resident {
                    title: [k(0), k(1), k(2), k(3)],
                    other_private: (private.1, private.0),
                    other_mapped: (mapped.1, mapped.0),
                    buffers: (total(&buffers), b.0),
                    windows: (total(&windows), w.0),
                    staging: (total(&staging), s.0),
                });
            }
        });
    });
}

/// (Neptune) What one pass of the resident sampler found, each as (committed, resident) bytes:
/// the title's images, page views, virtual allocations and heap blocks, what is left of the
/// private and the mapped regions, and the host copies' CPU sides and the staging buffers.
struct Resident {
    title: [(u64, u64); 4],
    other_private: (u64, u64),
    other_mapped: (u64, u64),
    buffers: (u64, u64),
    windows: (u64, u64),
    staging: (u64, u64),
}

/// Where the memory minute's rows go (`apps/victory` passes `<cache>\memory.csv`;
/// `%TEMP%\victory-memory.csv` without). `VICTORY_MEMORY_CSV=<path>` names another,
/// `VICTORY_MEMORY_CSV=0` writes none (the log line stays).
static MEMORY_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Points the memory minute's rows at `path`. Call before the title starts.
pub fn use_memory_path(path: std::path::PathBuf) {
    let _ = MEMORY_PATH.set(path);
}

/// The memory minute's columns, in megabytes unless named otherwise.
const COLUMNS: &str = "unix_s,frame,\
proc_private,proc_peak_private,proc_working_set,sys_avail_phys,sys_commit,sys_commit_limit,\
vram_budget,vram_used,sysmem_gpu_budget,sysmem_gpu_used,\
title_images,title_page_views,title_virtual,title_heap,title_page_views_res,title_virtual_res,\
host_buffer_cpu,host_window_cpu,texture_staging,snapshot_pool,\
rust_heap,d3d12_upload,d3d12_shared,d3d12_gpu_upload,d3d12_ours_unranged,sysmem_gpu_untracked,\
other_private,other_private_res,other_mapped,unattributed_private,\
host_lists,preludes_made,idle_lists,list_scratch,\
card_copies,tex_filled,tex_filled_placed,tex_targets,tex_depth,tex_uav,tex_twins,tex_idle_held,\
detile_raw_used,detile_card_used,detile_raw_size,detile_card_size,detile_to_cpu";

/// (Neptune) Once a minute, from the resident sampler's thread: one row of the memory
/// breakdown into the memory CSV, and the same derived in one log line. What we track (title
/// memory, host copies, staging, the Rust heap, our CPU-visible D3D12 buffers) is told from
/// what is left: `unattributed_private` is the private memory no category names, the driver's
/// own (command memory, the system's backing for it) among it.
fn minute(r: &Resident) {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mb = |v: u64| v >> 20;
    let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    // SAFETY: our own process and a sized structure (the extended form, as `cb` says).
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast::<PROCESS_MEMORY_COUNTERS>(), c.cb) };
    let mut p = PERFORMANCE_INFORMATION { cb: size_of::<PERFORMANCE_INFORMATION>() as u32, ..Default::default() };
    // SAFETY: a sized structure.
    unsafe { K32GetPerformanceInfo(&mut p, p.cb) };
    let page = p.PageSize as u64;
    let card = crate::host::host().and_then(|h| h.video_memory()).unwrap_or_default();
    let heap = victory_core::heapcount::sample();
    let d3d = crate::memtel::alive();
    let copies = crate::shadow::memory();
    let alive: Vec<u64> = crate::host::ALIVE.iter().map(|a| a.load(Relaxed)).collect();
    let vk: Vec<u64> = crate::resource::VRAM_KINDS.iter().map(|k| k.load(Relaxed)).collect();
    let det = crate::detile::memory();
    // Our buffers in the PC's memory that no range above names: their CPU sides are the host
    // copies and the staging buffers, which the sampler took first.
    let ranged = r.buffers.0 + r.windows.0 + r.staging.0;
    let ours_unranged = (d3d[0] + d3d[1]).saturating_sub(ranged);
    // What the system counts of the process's memory shared with the card beyond our buffers.
    let untracked = card[3].saturating_sub(d3d[0] + d3d[1]);
    let unattributed = r.other_private.0.saturating_sub(heap + ours_unranged);
    let row: Vec<u64> = vec![
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        crate::frame::current(),
        mb(c.PrivateUsage as u64),
        mb(c.PeakPagefileUsage as u64),
        mb(c.WorkingSetSize as u64),
        mb(p.PhysicalAvailable as u64 * page),
        mb(p.CommitTotal as u64 * page),
        mb(p.CommitLimit as u64 * page),
        mb(card[0]),
        mb(card[1]),
        mb(card[2]),
        mb(card[3]),
        mb(r.title[0].0),
        mb(r.title[1].0),
        mb(r.title[2].0),
        mb(r.title[3].0),
        mb(r.title[1].1),
        mb(r.title[2].1),
        mb(r.buffers.0),
        mb(r.windows.0),
        mb(r.staging.0),
        mb(copies[5]),
        mb(heap),
        mb(d3d[0]),
        mb(d3d[1]),
        mb(d3d[2]),
        mb(ours_unranged),
        mb(untracked),
        mb(r.other_private.0),
        mb(r.other_private.1),
        mb(r.other_mapped.0),
        mb(unattributed),
        alive[0],
        crate::host::LISTS[1].load(Relaxed),
        crate::list::idle_pool().0,
        mb(alive[2]),
        mb(crate::shadow::card_copy_bytes()[0]),
        mb(vk[0]),
        mb(vk[1]),
        mb(vk[2]),
        mb(vk[3]),
        mb(vk[4]),
        mb(vk[5]),
        mb(vk[6]),
        mb(det[0]),
        mb(det[1]),
        mb(det[2]),
        mb(det[3]),
        det[4],
    ];
    tracing::info!(
        "memory minute: private {} MB (peak {} MB), working set {} MB, PC free {} MB, system commit {} of {} MB; \
         card {} of {} MB, shared with the card {} MB; tracked: title {} MB (page views {} MB mapped, not private), \
         host copies {} MB, staging {} MB, rust heap {} MB, our D3D12 buffers {} MB upload + {} MB shared + {} MB GPU upload \
         ({} MB of them not in a range above); not ours in the shared count {} MB; left: private {} MB, mapped {} MB, \
         unattributed private {} MB; host lists {}, preludes {}",
        row[2],
        row[3],
        row[4],
        row[5],
        row[6],
        row[7],
        row[9],
        row[8],
        row[11],
        row[12] + row[14] + row[15],
        row[13],
        row[18] + row[19],
        row[20],
        row[22],
        row[23],
        row[24],
        row[25],
        row[26],
        row[27],
        row[28],
        row[30],
        row[31],
        row[32],
        row[33],
    );
    write_row(&row);
}

/// Appends `row` to the memory CSV, made with a header (the switches the run was started with,
/// then [`COLUMNS`]) on the first row.
fn write_row(row: &[u64]) {
    use std::io::Write;
    static FILE: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *OFF.get_or_init(|| std::env::var_os("VICTORY_MEMORY_CSV").is_some_and(|v| v == "0")) {
        return;
    }
    let mut file = FILE.lock().unwrap_or_else(|e| e.into_inner());
    if file.is_none() {
        let path = std::env::var_os("VICTORY_MEMORY_CSV")
            .map(std::path::PathBuf::from)
            .or_else(|| MEMORY_PATH.get().cloned())
            .unwrap_or_else(|| std::env::temp_dir().join("victory-memory.csv"));
        let Ok(mut made) = std::fs::File::create(&path) else {
            tracing::warn!(path = %path.display(), "memory minute: the CSV could not be made");
            let _ = OFF.set(true);
            return;
        };
        let mut switches: Vec<String> =
            std::env::vars().filter(|(k, _)| k.starts_with("VICTORY_")).map(|(k, v)| format!("{k}={v}")).collect();
        switches.sort();
        let _ = writeln!(made, "# switches: {}", switches.join(" "));
        let _ = writeln!(made, "{COLUMNS}");
        tracing::info!(path = %path.display(), "memory minute: a row a minute");
        *file = Some(made);
    }
    if let Some(f) = file.as_mut() {
        let text: Vec<String> = row.iter().map(u64::to_string).collect();
        let _ = writeln!(f, "{}", text.join(","));
        let _ = f.flush();
    }
}

/// Logs one line, called every five seconds from the host's frame stats.
pub(crate) fn log() {
    sampler();
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mb = |v: u64| v >> 20;
    let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    unsafe { GlobalMemoryStatusEx(&mut m) };
    let copies = crate::shadow::memory();
    let staged = crate::texture::memory();
    let alive: Vec<u64> = crate::host::ALIVE.iter().map(|a| a.load(Relaxed)).collect();
    let made: Vec<u64> = crate::host::MADE.iter().map(|a| a.load(Relaxed)).collect();
    let kinds: Vec<u64> = crate::resource::KINDS.iter().map(|k| k.load(Relaxed)).collect();
    let htex = [0, 1].map(|i| crate::resource::HOST_TEXTURES[i].load(Relaxed));
    let vk: Vec<u64> = crate::resource::VRAM_KINDS.iter().map(|k| k.load(Relaxed)).collect();
    let ev = [0, 1].map(|i| crate::resource::EVICTED[i].load(Relaxed));
    let sev: Vec<u64> = crate::resource::SLOT_EVICT.iter().map(|k| k.load(Relaxed)).collect();
    let card_copies = crate::shadow::card_copy_bytes();
    let card = crate::host::host().and_then(|h| h.video_memory()).unwrap_or_default();
    let k = crate::pipeline::kept();
    let (idle, idle_bytes) = crate::list::idle_pool();
    let drops = [0, 1, 2].map(|i| crate::pipeline::DXIL_DROPS[i].load(Relaxed));
    tracing::info!(
        "ram line (shaders): containers {} {} MB, translations {} with host code {} MB kept (let go so far {} MB, \
         brought back {}x {} MB; VICTORY_DROP_DXIL), pipeline library file {} MB; idle host lists {} (upload {} MB, \
         destroyed by VICTORY_LIST_POOL_IDLE_S {}); rust heap {} MB; title bytes told written after view writes (VICTORY_DS_NOTIFY) {} MB",
        k[0],
        mb(k[1]),
        k[2],
        mb(k[3]),
        mb(drops[0]),
        drops[1],
        mb(drops[2]),
        mb(k[4]),
        idle,
        mb(idle_bytes),
        crate::list::TRIMMED_LISTS.load(Relaxed),
        mb(victory_core::heapcount::sample()),
        mb(crate::watch::NOTIFIED.swap(0, Relaxed)),
    );
    tracing::info!(
        "ram line: process working set {} MB, private {} MB; PC free {} MB, free commit {} MB; \
         title memory host copies {} alive {} MB (windows outside buffers {} {} MB; snapshot buffers kept {} reserving {} MB); \
         texture staging {} alive {} MB ({} kept by their texture); \
         host textures {} {} MB; title texture host copies alive {} {} MB, let go {} {} MB; \
         list scratch chunks {} {} MB (spare {} {} MB), other list uploads {} {} MB, host lists {}; \
         made so far: upload buffers {} {} MB, shared {} {} MB, card buffers {} {} MB; \
         card copies {} MB (before twins {} MB); card (VRAM) {} MB in use of {} MB budget, shared {} MB in use; \
         host texture VRAM by kind: filled {} MB, filled placed {} MB, targets {} MB, depth {} MB, uav {} MB, twins {} MB, \
         filled idle but held {} MB, filled duplicates at one address {} MB; evicted under pressure {} {} MB; \
         evicted with slots (VICTORY_SLOT_EVICT) {} textures {} MB, {} slots reset, {} came back ({} within 2 s, away {} ms in all), {} given up{}{}",
        mb(c.WorkingSetSize as u64),
        mb(c.PagefileUsage as u64),
        mb(m.ullAvailPhys),
        mb(m.ullAvailPageFile),
        copies[0],
        mb(copies[1]),
        copies[2],
        mb(copies[3]),
        copies[4],
        mb(copies[5]),
        staged[0],
        mb(staged[1]),
        staged[2],
        htex[0],
        mb(htex[1]),
        kinds[0],
        mb(kinds[1]),
        kinds[2],
        mb(kinds[3]),
        alive[1],
        mb(alive[2]),
        alive[3],
        mb(alive[4]),
        alive[6],
        mb(alive[7]),
        alive[0],
        made[0],
        mb(made[1]),
        made[2],
        mb(made[3]),
        made[4],
        mb(made[5]),
        mb(card_copies[0]),
        mb(card_copies[1]),
        mb(card[1]),
        mb(card[0]),
        mb(card[3]),
        mb(vk[0]),
        mb(vk[1]),
        mb(vk[2]),
        mb(vk[3]),
        mb(vk[4]),
        mb(vk[5]),
        mb(vk[6]),
        mb(vk[7]),
        ev[0],
        mb(ev[1]),
        sev[0],
        mb(sev[1]),
        sev[2],
        sev[3],
        sev[4],
        sev[5] / 10,
        sev[6],
        upload_line(),
        rebar_line(),
    );
}

/// (Neptune) The rebar window part of the ram line (`VICTORY_WINDOW_REBAR=1`), else nothing.
fn rebar_line() -> String {
    if !crate::shadow::window_rebar() {
        return String::new();
    }
    use crate::shadow::REBAR;
    format!(
        "; window rebar: windows {} ({} MB), written since the last line {} MB, refused: budget {} / folding {} / other {} / unordered access {}, shader writes to one (bug) {}",
        REBAR[0].load(Relaxed),
        REBAR[1].load(Relaxed) >> 20,
        REBAR[2].swap(0, Relaxed) >> 20,
        REBAR[3].load(Relaxed),
        REBAR[4].load(Relaxed),
        REBAR[5].load(Relaxed),
        REBAR[7].load(Relaxed),
        REBAR[6].load(Relaxed),
    )
}

/// (Neptune) The window upload part of the ram line (`VICTORY_CARD_WINDOW_UPLOAD=1`), else nothing.
fn upload_line() -> String {
    if !crate::shadow::window_upload() {
        return String::new();
    }
    use crate::shadow::TRANSIENT;
    let mb = |v: u64| v >> 20;
    let [pool, peak, overflow, overflows] = crate::windowup::stats();
    format!(
        "; window upload: transient windows {}, pool {} MB (peak {} MB, overflow slabs alive {} MB, overflow allocations {}), \
         sent from chunks since the last line {} MB, buffers not folded into one {}, shader writes to one (bug) {}",
        TRANSIENT[0].load(Relaxed),
        mb(pool),
        mb(peak),
        mb(overflow),
        overflows,
        mb(crate::windowup::SENT.swap(0, Relaxed)),
        TRANSIENT[1].load(Relaxed),
        TRANSIENT[2].load(Relaxed),
    )
}
