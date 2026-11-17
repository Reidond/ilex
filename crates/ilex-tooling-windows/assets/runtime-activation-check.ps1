param([Parameter(Mandatory=$true)][string]$Request)
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false)
$r=Get-Content -LiteralPath $Request -Raw -Encoding UTF8|ConvertFrom-Json
if($r.schema -ne 1){throw 'Unsupported activation check schema'}
$homeRoot=Join-Path $r.fixture 'host'
$store=Join-Path $homeRoot 'ilex\runtimes'
$archives=Join-Path $store 'archives'
[IO.Directory]::CreateDirectory($archives)|Out-Null
# Copy the pinned compressed bytes; acquisition still verifies every package
# and extracts a fresh runtime. The normal runtime/pointers are never changed.
foreach($file in (Get-ChildItem -LiteralPath $r.archives -Filter '*.tar.zst' -File)) {
    if($file.Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Archive cache has a reparse point'}
    Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $archives $file.Name)
}
function Invoke-Update([bool]$failSdl){
    $psi=New-Object Diagnostics.ProcessStartInfo
    $psi.FileName=$r.xtask;$psi.Arguments='runtime update'
    $psi.UseShellExecute=$false;$psi.CreateNoWindow=$true
    $psi.RedirectStandardOutput=$true;$psi.RedirectStandardError=$true
    $psi.EnvironmentVariables['LOCALAPPDATA']=$homeRoot
    if($failSdl){$psi.EnvironmentVariables['SDL_VIDEODRIVER']='ilex-self-check-unavailable'}
    else{$psi.EnvironmentVariables.Remove('SDL_VIDEODRIVER')}
    $p=New-Object Diagnostics.Process;$p.StartInfo=$psi
    $null=$p.Start();$stdout=$p.StandardOutput.ReadToEndAsync();$stderr=$p.StandardError.ReadToEndAsync()
    if(!$p.WaitForExit(1200000)){$p.Kill();$p.WaitForExit();throw 'Activation check update timed out'}
    $reply=@{Code=$p.ExitCode;Out=$stdout.Result;Err=$stderr.Result};$p.Dispose();return $reply
}
function Wait-Capacity {
    $deadline=[DateTime]::UtcNow.AddHours(3)
    while(([long](Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory*1KB) -lt (14GB+256MB)) {
        if([DateTime]::UtcNow -ge $deadline){throw 'Runtime check memory wait exhausted; reserve unchanged'}
        [Console]::Error.WriteLine('runtime activation self-check: waiting for 256 MiB plus 14 GiB host reserve')
        Start-Sleep -Seconds 30
    }
}
$active=Join-Path $store 'active.json';$previous=Join-Path $store 'previous.json'
if((Test-Path -LiteralPath $active) -or (Test-Path -LiteralPath $previous)){throw 'Activation fixture is not a fresh installation'}
Wait-Capacity
$bad=Invoke-Update $true
if($bad.Code -eq 0 -or $bad.Err -notmatch 'Could not initialize SDL|ilex-self-check-unavailable.*not available' -or (Test-Path -LiteralPath $active) -or (Test-Path -LiteralPath $previous)){throw "Failed first update activated a runtime, or missed SDL failure: $($bad.Err.Substring([Math]::Max(0,$bad.Err.Length-3000)))"}
$candidate=Join-Path $store $r.candidate_name
if(!(Test-Path -LiteralPath (Join-Path $candidate 'ready.json'))){throw 'Failed GPU smoke did not leave a verified acquired runtime'}
[Console]::Error.WriteLine('runtime activation self-check: fresh acquisition followed by real SDL failure left active/previous absent')
Wait-Capacity
$good=Invoke-Update $false
if($good.Code -ne 0 -or !(Test-Path -LiteralPath $active) -or ((Get-Content -LiteralPath $active -Raw -Encoding UTF8|ConvertFrom-Json) -ne $candidate) -or (Test-Path -LiteralPath $previous)){throw "Successful first GPU smoke did not activate exactly its candidate: $($good.Err)"}
[Console]::Error.WriteLine('runtime activation self-check: successful WHPX/SDL/GL firmware smoke then activated the first pointer')
@{schema=1;failed_first_update_left_active_absent=$true;failed_first_update_left_previous_absent=$true;verified_acquisition_retained=$true;failure='real SDL initialization with unavailable video driver';successful_gpu_smoke_activated=$true}|ConvertTo-Json
