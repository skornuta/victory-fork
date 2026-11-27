# Neptune (VIctory Edit.) launcher.
#
#   .\play.ps1 -Game 'D:\path\to\game_scarlett_master.exe' -OptiScaler 'D:\OptiScaler' -Warm
#       (first time: translates the shaders, about 7 minutes, then starts the game)
#   .\play.ps1 -Game 'D:\path\to\game_scarlett_master.exe' -OptiScaler 'D:\OptiScaler'
#       (every time after that)
#   add -FrameGen for frame generation, -FrametimeDump for a CSV of frame times to post.
#
# Two ways to run:
#   Series S mode (default when a PC FidelityFX DLL is at hand): the scene is rendered at
#       1280x720 and upscaled to 1920x1080 by AMD's upscaler. Fastest: 16-17 fps in the first
#       room on an RTX 5070 Ti / i7-14700 (2026-11-25). The DLL is not ours to ship: it comes
#       from -OptiScaler <folder> (an extracted OptiScaler release has one), or name one with
#       -FsrDll (amd_fidelityfx_upscaler_dx12.dll; many PC games ship it), or put one beside
#       this script.
#   Series X path (-SeriesX, or when no such DLL is found): 4K, about 14 fps there.
#
# It does not take the window's focus. In the game: click the picture (or F9) for mouse look,
# F9 or Alt+Tab lets the pointer go. WASD move, right/left mouse = left/right trigger, Shift /
# Ctrl = stick clicks, Enter/Backspace/X/Y = A/B/X/Y, Q/E bumpers, Esc menu, Tab view. F10
# hides the figures in the corner, F12 saves a picture.
#
# On a card with less than 16 GB, lower VICTORY_CARD_WINDOWS_MB and VICTORY_CARD_BUFFERS_MB
# first. If a launch misbehaves, remove switches from the last blocks one at a time.
param(
  [string]$Game = '',                            # your own copy's game_scarlett_master.exe (default: VICTORY_GAME_DIR)
  [switch]$Warm,                                  # translate the shaders into the cache first
  [switch]$NoAutoA,                               # do not tap A through the start screens
  [switch]$SeriesX,                               # the 4K path, the title's upscaler not used
  [string]$FsrDll = '',                           # a PC amd_fidelityfx_upscaler_dx12.dll
  [string]$OptiScaler = '',                       # the folder of an extracted OptiScaler release
  [switch]$FrameGen,                              # with -OptiScaler: its upscaler and frame generation
  [string]$Upscaler = 'xess',                     # with -FrameGen: xess, fsr31 or dlss
  [switch]$FrametimeDump                          # writes <cache>\frametime.csv (a row a second)
)
if (-not $Game) {
  if ($env:VICTORY_GAME_DIR -and (Test-Path (Join-Path $env:VICTORY_GAME_DIR 'game_scarlett_master.exe'))) {
    $Game = Join-Path $env:VICTORY_GAME_DIR 'game_scarlett_master.exe'
  } else {
    'usage: play.ps1 -Game <path to game_scarlett_master.exe>, or set VICTORY_GAME_DIR'; exit 1
  }
}
if (-not (Test-Path -LiteralPath $Game)) { "Not found: $Game"; exit 1 }
$set = [ordered]@{
  # settled
  VICTORY_CARD_WINDOWS_MB     = '4096'      # draw data on the card (16 GB card)
  VICTORY_BAKE_TEXTURES_MS    = '10000'     # textures uploaded before the character bake
  VICTORY_EARLY_TEXTURES_MS   = '40'
  VICTORY_WAVE_OWN_LDS_GROUPS = '64,128'    # wave rule for particles, reflection tiles, decal defrag
  VICTORY_WIDE_MERGE          = '1'         # shader results merged back on several threads
  VICTORY_RELAY_OWN_QUEUE     = '1'         # copy-back fetches on a GPU queue of their own
  VICTORY_UNDER_WRITES_WORDS  = '1'         # small changes under shader writes do not force a wait
  VICTORY_UNDER_WRITES_RUNS   = '100000'
  VICTORY_UNDER_WRITES_MASKS  = '1'
  VICTORY_RELAY               = '2'
  VICTORY_WIDE_REFRESH        = '1'
  VICTORY_ASK_ONCE            = '1'
  VICTORY_TEXTURE_NO_WAIT     = '1'
  VICTORY_CARD_REHASH_RUNS    = '1'
  VICTORY_JOIN_COPIES         = '1'
  VICTORY_D3D12_BACKGROUND    = '0'
  VICTORY_LATE_WAIT_LISTS     = '1024'
  VICTORY_WINDOW_TAILS        = '1'
  VICTORY_TEXTURE_STAGE_MB    = '512'
  VICTORY_CARD_BUFFERS_MIN_KB = '64'
  VICTORY_ASK_QUIET           = '8'         # quiet memory asked about once a frame
  VICTORY_COPY_CHANGED        = '1'         # only bytes that differ go up to the card
  VICTORY_QUIET_BACK          = '1'         # copied-back shader results are not read again
  VICTORY_LIST_POOL_MOST      = '2048'      # command lists kept, not made anew each frame
  VICTORY_LIST_POOL_MB        = '2048'
  VICTORY_WIDE_THREADS        = '12'
  VICTORY_UNDER_WRITES_MASKS_LEAST = '1'   # small changes go up in one pass per buffer
  VICTORY_INDIRECT_RAW        = '2'         # indirect draws use the game's own arguments
  VICTORY_TEXTURE_HASH_EVERY  = '32'
  # added 2026-11-25 noon
  VICTORY_LEVELS_ON_DEMAND    = '1'         # a texture takes the card's memory only for the mip levels the game holds
  VICTORY_WINDOW_KB           = '1024'
  VICTORY_CARD_BUFFERS_MB     = '1536'
  VICTORY_CARD_WINDOW_UPLOAD  = '1'
  VICTORY_DROP_DXIL           = '1'
  VICTORY_DESC_DATA           = '1'         # shaders that read a descriptor as data translate (5 more in the first scenes)
  VICTORY_VRAM_IDLE_MS        = '15000,2500,1000'   # an unused texture is kept 15 s while the card has room (less black-then-back)
  VICTORY_PIPELINE_WAIT_MS    = '2500'      # a frame waits at most 2.5 s for a shader being compiled (12 s by default: freezes, and the game's own watchdog ends a 30 s frame)
  # Memory (RTX 5080 runs of 2026-11-26: VRAM 12.6 -> 9.5 GB, private 26.0 -> 22.6 GB in the first area, no pop-in seen)
  VICTORY_GPU_DETILE          = '1'         # textures are detiled on the card
  VICTORY_GPU_DETILE_VERIFY   = '0'
  VICTORY_GPU_DETILE_OUT_MB   = '512'       # detile rings for loading bursts...
  VICTORY_GPU_DETILE_RAW_MB   = '512'
  VICTORY_GPU_DETILE_TRIM     = '1'         # ...shrunk to 64 + 64 MB once loading is over, grown back for a burst
  VICTORY_SLOT_EVICT          = '1'         # idle textures' card copies let go above 75 % of the card's budget, down to 65 %
  VICTORY_SLOT_EVICT_PERCENT  = '75'
  VICTORY_SLOT_EVICT_LOW_PERCENT = '65'
  VICTORY_BINDLESS_FEEDBACK   = '1'         # shaders note the textures they take from the heap, so unseen ones can go too
  VICTORY_ALLOCATOR_TRIM      = '1'         # command allocators taken anew every 30 s (they keep their largest recording)
}
if (-not $NoAutoA) { $set['VICTORY_AUTO_A'] = '150' }   # taps A through the start screens for 150 s
if ($FrametimeDump) { $set['VICTORY_FRAMETIME_DUMP'] = '1' }

function Set-IniValue([string]$Text, [string]$Section, [string]$Key, [string]$Value) {
  $lines = $Text -split "`r?`n"
  $in = $false; $done = $false
  for ($i = 0; $i -lt $lines.Count; $i++) {
    if ($lines[$i] -match '^\[(.+)\]\s*$') { $in = ($Matches[1] -eq $Section); continue }
    if ($in -and -not $done -and $lines[$i] -match ('^' + [regex]::Escape($Key) + '\s*=')) { $lines[$i] = "$Key=$Value"; $done = $true }
  }
  if (-not $done) { Write-Warning "OptiScaler.ini: no $Key in [$Section]" }
  $lines -join "`r`n"
}

# A copy of victory.exe in a folder of its own ("opti") with OptiScaler loaded as its dxgi.dll:
# OptiScaler's upscaler takes the title's upscaler calls and its frame generation runs on the
# finished frames. A launch without -FrameGen stays without OptiScaler in the process.
function Initialize-Opti([string]$Exe, [string]$OptiDir, [string]$Upscaler) {
  $to = Join-Path (Split-Path $Exe -Parent) 'opti'
  New-Item -ItemType Directory -Force $to | Out-Null
  $stamp = Join-Path $to 'optiscaler.from'
  $from = (Get-Item (Join-Path $OptiDir 'OptiScaler.dll')).LastWriteTimeUtc.Ticks.ToString() + ' ' + $OptiDir
  if (-not (Test-Path $stamp) -or (Get-Content $stamp -Raw).Trim() -ne $from) {
    Get-ChildItem $OptiDir -File | Where-Object { $_.Extension -in '.dll', '.ini' } | Copy-Item -Destination $to -Force
    Get-ChildItem $OptiDir -Directory | Where-Object { $_.Name -ne 'Licenses' } | Copy-Item -Destination $to -Recurse -Force
    Copy-Item (Join-Path $OptiDir 'OptiScaler.dll') (Join-Path $to 'dxgi.dll') -Force
    Remove-Item (Join-Path $to 'OptiScaler.dll') -Force -ErrorAction SilentlyContinue
    Set-Content $stamp $from
  }
  $ini = Get-Content (Join-Path $OptiDir 'OptiScaler.ini') -Raw
  $ini = Set-IniValue $ini 'Upscalers' 'Dx12Upscaler' $Upscaler
  $ini = Set-IniValue $ini 'FrameGen' 'Enabled' 'true'
  $ini = Set-IniValue $ini 'FrameGen' 'FGInput' 'upscaler'
  $ini = Set-IniValue $ini 'FrameGen' 'FGOutput' 'fsrfg'
  $ini = Set-IniValue $ini 'FSRFG' 'AllowAsync' 'true'       # the present call does not wait for the generated frame's turn
  $ini = Set-IniValue $ini 'OptiFG' 'DisableHUDFix' 'true'   # its tracking of every draw costs 5 fps here
  Set-Content (Join-Path $to 'OptiScaler.ini') $ini -NoNewline
  $copy = Join-Path $to (Split-Path $Exe -Leaf)
  if (-not (Test-Path $copy) -or (Get-Item $copy).LastWriteTimeUtc -ne (Get-Item $Exe).LastWriteTimeUtc -or (Get-Item $copy).Length -ne (Get-Item $Exe).Length) {
    Copy-Item $Exe $copy -Force
  }
  $copy
}

$built = Join-Path $PSScriptRoot 'target\release\victory.exe'
$exe = if (Test-Path $built) { $built } else { Join-Path $PSScriptRoot 'victory.exe' }
$console = @()
if ($OptiScaler -and -not (Test-Path (Join-Path $OptiScaler 'OptiScaler.dll'))) { "No OptiScaler.dll in $OptiScaler"; exit 1 }
if ($FrameGen -and -not $OptiScaler) { '-FrameGen needs -OptiScaler <folder of an extracted OptiScaler release>'; exit 1 }
if (-not $SeriesX) {
  if ($FrameGen) {
    $exe = Initialize-Opti $exe (Resolve-Path $OptiScaler).Path $Upscaler
    $FsrDll = Join-Path (Split-Path $exe -Parent) 'amd_fidelityfx_dx12.dll'
  } elseif (-not $FsrDll) {
    $near = @((Join-Path $PSScriptRoot 'amd_fidelityfx_upscaler_dx12.dll'))
    if ($OptiScaler) { $near = @((Join-Path $OptiScaler 'amd_fidelityfx_upscaler_dx12.dll')) + $near }
    $FsrDll = $near | Where-Object { Test-Path $_ } | Select-Object -First 1
  }
  if ($FsrDll -and (Test-Path $FsrDll)) {
    $set['VICTORY_FSR_DLL'] = (Resolve-Path $FsrDll).Path   # the game's upscaler calls go to this DLL
    $console = @('--console', 'series-s')
  } else {
    'No PC FidelityFX DLL (-OptiScaler <folder>, -FsrDll <path>, or one beside this script): starting the Series X path at 4K.'
  }
}
foreach ($k in $set.Keys) { Set-Item -Path ("Env:" + $k) -Value $set[$k] }
# The game's folder, for the shader translation (no config file needed).
$env:VICTORY_GAME_DIR = Split-Path -Parent (Resolve-Path -LiteralPath $Game)
if (Get-Process victory -ErrorAction SilentlyContinue) { 'The game is already running.'; exit 1 }
Set-Location $PSScriptRoot
if ($Warm) { & $exe warm }
& $exe run $Game --execute @console
