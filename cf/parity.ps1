param(
    [string]$Ours = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist\cf-0.1.0-src\strobe.exe'),
    [string]$Official = 'C:\cf\strobe_lab\strobe-windows-x64\strobe.exe',
    [string]$Out = 'C:\cf\tmp\sb\parity',
    [string]$Frames = '3000-3899',
    [string]$Mode = 'fast'
)
$ErrorActionPreference = 'Continue'
New-Item -ItemType Directory -Force $Out | Out-Null
$env:VRF_NO_DAEMON = '1'
$lock = 'D:\cf\lab\gpu.lock'
$owner = "strobe-src parity $(Get-Date -Format s) pid $PID"
$log = Join-Path $Out 'parity.log'
function Say($m) { $m | Tee-Object -FilePath $log -Append | Out-Host }

function Render($exe, $tag, $cache) {
    $env:VRF_CACHE_DIR = $cache
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $mp4 = Join-Path $Out "$tag.mp4"
    cmd /c "`"$exe`" render C:\cf\motion --composition EditV2 --props C:\cf\motion\work\iy002_v32_noaudio.json --frames $Frames --mode $Mode --no-audio --json -o `"$mp4`" > `"$Out\$tag.json`" 2> `"$Out\$tag.err`""
    $code = $LASTEXITCODE
    $j = Get-Content "$Out\$tag.json" -Raw -ErrorAction SilentlyContinue | ConvertFrom-Json
    Say ("{0}: exit={1} wall={2:N1}s fps={3:N2} render_s={4:N2} bundle_s={5:N1} frames={6}" -f $tag, $code, $sw.Elapsed.TotalSeconds, $j.fps, $j.stages.render_s, $j.stages.bundle_s, $j.frames)
}

while ($true) {
    try {
        $h = [System.IO.File]::Open($lock, 'CreateNew', 'Write', 'None')
        $b = [Text.Encoding]::UTF8.GetBytes($owner); $h.Write($b, 0, $b.Length); $h.Close(); break
    } catch { Say ("waiting for GPU lock: " + (Get-Content $lock -Raw -ErrorAction SilentlyContinue)); Start-Sleep 15 }
}
try {
    Say "lock held by: $owner"
    Say "ours=$Ours official=$Official frames=$Frames mode=$Mode"
    $cacheA = Join-Path $Out 'cache_official'
    Render $Official 'official_cold' $cacheA
    Render $Official 'official_warm' $cacheA
    Remove-Item -Recurse -Force $cacheA -ErrorAction SilentlyContinue
    $cacheB = Join-Path $Out 'cache_ours'
    Render $Ours 'ours_cold' $cacheB
    Render $Ours 'ours_warm' $cacheB
    Remove-Item -Recurse -Force $cacheB -ErrorAction SilentlyContinue
} finally {
    if ((Get-Content $lock -Raw -ErrorAction SilentlyContinue) -eq $owner) { Remove-Item $lock; Say 'lock released' }
}

Set-Location $Out
foreach ($t in 'official_cold', 'official_warm', 'ours_cold', 'ours_warm') {
    $p = cmd /c "ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames,width,height,r_frame_rate,codec_name,pix_fmt -of compact=p=0 $t.mp4 2>&1"
    $md5 = cmd /c "ffmpeg -v error -i $t.mp4 -map 0:v -f md5 - 2>&1"
    Say "$t | $p | decoded $md5"
}
foreach ($pair in @('ours_warm', 'official_warm'), @('ours_cold', 'official_cold'), @('official_cold', 'official_warm'), @('ours_cold', 'ours_warm')) {
    $a, $r = $pair
    $vlog = "vmaf_${a}__$r.json"
    cmd /c "ffmpeg -hide_banner -i $a.mp4 -i $r.mp4 -lavfi `"[0:v]setpts=PTS-STARTPTS,format=yuv420p[a];[1:v]setpts=PTS-STARTPTS,format=yuv420p[b];[a][b]libvmaf=n_threads=16:feature=name=psnr:log_fmt=json:log_path=$vlog`" -f null - > NUL 2>&1"
    $j = Get-Content $vlog -Raw | ConvertFrom-Json
    $v = $j.frames | ForEach-Object { $_.metrics.vmaf }
    $min = ($v | Measure-Object -Minimum).Minimum
    $worst = $j.frames | Sort-Object { $_.metrics.vmaf } | Select-Object -First 3 | ForEach-Object { '{0}:{1:N2}' -f $_.frameNum, $_.metrics.vmaf }
    Say ("VMAF {0} vs {1}: mean={2:N3} min={3:N3} psnrY={4:N2} frames={5} worst={6}" -f $a, $r, $j.pooled_metrics.vmaf.mean, $min, $j.pooled_metrics.psnr_y.mean, $j.frames.Count, ($worst -join ','))
}
