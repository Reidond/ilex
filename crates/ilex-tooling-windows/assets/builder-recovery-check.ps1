param([Parameter(Mandatory=$true)][string]$Request)
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false)
$r=Get-Content -LiteralPath $Request -Raw -Encoding UTF8|ConvertFrom-Json
if($r.schema -ne 1){throw 'Unsupported recovery check schema'}
Import-Module Hyper-V
$production=[IO.File]::ReadAllText($r.helper)
$root=[IO.Path]::GetFullPath($r.root)
$checks=[IO.Path]::GetFullPath($r.checks)
if($checks -ne (Join-Path (Split-Path $root -Parent) 'builder-checks')){throw 'Unexpected recovery-check parent'}
$work=Join-Path $checks ([guid]::NewGuid().ToString())
$fixture=Join-Path $work 'builder'
[IO.Directory]::CreateDirectory($fixture)|Out-Null
$baseline=Get-Content -LiteralPath (Join-Path $root 'journal.json') -Raw -Encoding UTF8|ConvertFrom-Json
$expectedId=[guid]$baseline.vm_id
$retained=Get-VM -Id $expectedId
if($baseline.phase -ne 'ready' -or $retained.State -ne 'Off' -or $retained.Notes -ne $baseline.notes -or $baseline.checkpoint_id -or @(Get-VMSnapshot -VM $retained).Count){throw 'Recovery check requires the journal-owned ready builder off without checkpoints'}
$knownHostsHash=(Get-FileHash -LiteralPath (Join-Path $root 'known_hosts')).Hash
$sequence=0
$fixtureId=$null
$replacement=$null
$result=$null
function Check([bool]$ok,[string]$message){if(!$ok){throw $message}}
function Replace-Once([string]$code,[string]$anchor,[string]$replacementText){
    $at=$code.IndexOf($anchor,[StringComparison]::Ordinal)
    if($at -lt 0 -or $code.IndexOf($anchor,$at+$anchor.Length,[StringComparison]::Ordinal) -ge 0){throw "Recovery injection anchor is not unique: $anchor"}
    return $code.Substring(0,$at)+$replacementText+$code.Substring($at+$anchor.Length)
}
function Invoke-Helper([string]$code,[string]$operation,[string]$target,$options){
    $script:sequence++
    $scriptPath=Join-Path $work "helper-$sequence.ps1"
    $requestPath=Join-Path $work "request-$sequence.json"
    [IO.File]::WriteAllText($scriptPath,$code)
    $data=@{schema=1;operation=$operation;root=$target;revision=$r.revision;runtime=$r.runtime;image=$r.image;options=$options;validation_run=(Split-Path $work -Leaf)}
    [IO.File]::WriteAllText($requestPath,($data|ConvertTo-Json -Depth 8))
    $psi=New-Object Diagnostics.ProcessStartInfo
    $psi.FileName=(Get-Command powershell.exe).Source
    $psi.Arguments="-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$scriptPath`" -Request `"$requestPath`""
    $psi.UseShellExecute=$false;$psi.CreateNoWindow=$true
    $psi.RedirectStandardOutput=$true;$psi.RedirectStandardError=$true
    $p=New-Object Diagnostics.Process;$p.StartInfo=$psi
    $null=$p.Start();$stdout=$p.StandardOutput.ReadToEndAsync();$stderr=$p.StandardError.ReadToEndAsync()
    if(!$p.WaitForExit(([int]$r.options.wait_for_memory_seconds+1200)*1000)){$p.Kill();$p.WaitForExit();throw 'Recovery helper timed out'}
    $reply=[pscustomobject]@{Code=$p.ExitCode;Out=$stdout.Result;Err=$stderr.Result}
    $p.Dispose()
    return $reply
}
function Expected($reply,[int]$exitCode,[string]$errorPattern=''){
    Check ($reply.Code -eq $exitCode) "Unexpected recovery helper exit $($reply.Code): $($reply.Err)"
    if($errorPattern){Check ($reply.Err -match $errorPattern) "Missing expected recovery diagnostic: $($reply.Err)"}
}
function Retained-VM {
    $j=Get-Content -LiteralPath (Join-Path $root 'journal.json') -Raw -Encoding UTF8|ConvertFrom-Json
    $vm=Get-VM -Id $expectedId
    Check ($j.vm_id -eq [string]$expectedId -and $j.owner -eq $baseline.owner -and $j.run -eq $baseline.run -and $vm.Notes -eq $baseline.notes -and $vm.State -eq 'Off') 'Retained builder identity/state changed'
    return $vm
}
function Assert-RetainedProfile {
    $null=Retained-VM
    $j=Get-Content -LiteralPath (Join-Path $root 'journal.json') -Raw -Encoding UTF8|ConvertFrom-Json
    $fresh=Invoke-Helper $production 'status' $root @{}
    Expected $fresh 0
    $actual=($fresh.Out|ConvertFrom-Json).actual_resources
    $profileOK=$actual.memory_bytes -eq $baseline.memory_bytes -and !$actual.dynamic_memory -and $actual.cpus -eq $baseline.cpus -and $actual.nested
    Check $profileOK "Retained resource profile was not restored: RAM=$($actual.memory_bytes)/$($baseline.memory_bytes), CPUs=$($actual.cpus)/$($baseline.cpus)"
    Check ($j.memory_bytes -eq $baseline.memory_bytes -and $j.cpus -eq $baseline.cpus -and $j.data_uuid -eq $baseline.data_uuid -and $j.revision -eq $baseline.revision) 'Retained journal/data/provisioning identity changed'
    Check ((Get-FileHash -LiteralPath (Join-Path $root 'known_hosts')).Hash -eq $knownHostsHash) 'Retained SSH pin changed'
}
# Authorize through the full production validator before the restoration
# finally block is allowed to perform any native mutation on this UUID.
Expected (Invoke-Helper $production 'status' $root @{}) 0
Assert-RetainedProfile
try {
    # Only identity salting and abrupt-exit/start-witness instrumentation differ
    # from the embedded production helper. The admission/recovery code is intact.
    $noStart=Replace-Once $production 'Start-VM -VM $vm | Out-Null' "throw 'Unsafe start reached by recovery self-check'"
    $repair=Replace-Once $noStart '$j.memory_bytes=$requestedMemory;$j.cpus=$requestedCpus;Write-Journal' ('$j.memory_bytes=$requestedMemory;$j.cpus=$requestedCpus;Write-Journal'+"`n                [Environment]::Exit(86)")
    $otherMemory=16;if([long]$baseline.memory_bytes -eq 16GB){$otherMemory=4}
    $memoryStep='Set-VMMemory -VM (Owned-VM) -DynamicMemoryEnabled $false -StartupBytes $requestedMemory'
    $memoryCrash=Replace-Once $noStart $memoryStep ($memoryStep+"`n                [Environment]::Exit(86)")
    Expected (Invoke-Helper $memoryCrash 'up' $root @{memory_gib=$otherMemory;cpus=$baseline.cpus;wait_for_memory_seconds=0}) 86
    $fresh=Invoke-Helper $production 'status' $root @{}
    Expected $fresh 0
    $actual=($fresh.Out|ConvertFrom-Json).actual_resources
    Check ($actual.memory_bytes -eq ([long]$otherMemory*1GB)) "Memory interruption missed the post-Set-VMMemory boundary: actual=$($actual.memory_bytes), expected=$([long]$otherMemory*1GB)"
    Expected (Invoke-Helper $noStart 'up' $root @{wait_for_memory_seconds=0}) 1 'Actual VM resources differ from journal'
    Expected (Invoke-Helper $repair 'up' $root @{memory_gib=([long]$baseline.memory_bytes/1GB);cpus=$baseline.cpus;wait_for_memory_seconds=0}) 86
    Assert-RetainedProfile
    $otherCpus=2;if($baseline.cpus -eq 2){$otherCpus=4}
    $cpuStep='Set-VMProcessor -VM (Owned-VM) -Count $requestedCpus -ExposeVirtualizationExtensions $true'
    $cpuCrash=Replace-Once $noStart $cpuStep ($cpuStep+"`n                [Environment]::Exit(86)")
    Expected (Invoke-Helper $cpuCrash 'up' $root @{cpus=$otherCpus;wait_for_memory_seconds=0}) 86
    Expected (Invoke-Helper $noStart 'up' $root @{wait_for_memory_seconds=0}) 1 'Actual VM resources differ from journal'
    Expected (Invoke-Helper $repair 'up' $root @{memory_gib=([long]$baseline.memory_bytes/1GB);cpus=$baseline.cpus;wait_for_memory_seconds=0}) 86
    Assert-RetainedProfile
    [Console]::Error.WriteLine('builder recovery self-check: post-memory and post-CPU interruption rejected before admission; retained profile repaired while off')

    $salted=Replace-Once $noStart 'GetBytes("github.com/Reidond/ilex`0$sid")' 'GetBytes("github.com/Reidond/ilex`0$sid`0$($r.validation_run)")'
    foreach($file in @('provision.sh','builder-control.sh','smoke.sh')){[IO.File]::WriteAllText((Join-Path $fixture $file),[IO.File]::ReadAllText((Join-Path $root $file)))}
    $flush='    [IlexAtomicFile]::Replace($tmp,$journal)'
    $allocatedCrash=Replace-Once $salted $flush ("    if(`$script:j.phase -eq 'allocated'){[Environment]::Exit(86)}`n"+$flush)
    Expected (Invoke-Helper $allocatedCrash 'up' $fixture @{memory_gib=4;cpus=4;wait_for_memory_seconds=$r.options.wait_for_memory_seconds}) 86
    $journal=Join-Path $fixture 'journal.json';$partial="$journal.partial"
    $intentBytes=[IO.File]::ReadAllText($journal);$candidateBytes=[IO.File]::ReadAllText($partial)
    $candidate=$candidateBytes|ConvertFrom-Json;$fixtureId=[guid]$candidate.vm_id
    $vm=Get-VM -Id $fixtureId
    Check (($intentBytes|ConvertFrom-Json).phase -eq 'preparing' -and $candidate.phase -eq 'allocated' -and [string]::IsNullOrEmpty($vm.Notes) -and $vm.State -eq 'Off') 'Allocated interruption missed the flushed-candidate/empty-Notes boundary'
    $candidate.resource.run=[guid]::NewGuid().ToString()
    [IO.File]::WriteAllText($partial,($candidate|ConvertTo-Json -Depth 15))
    Expected (Invoke-Helper $salted 'cleanup-incomplete' $fixture @{}) 1 'OwnedResource journal identity mismatch'
    [IO.File]::WriteAllText($partial,$candidateBytes)
    # Empty Notes are forbidden for an ordinary ready record, even in this fixture.
    $ready=$candidateBytes|ConvertFrom-Json;$ready.phase='ready'
    [IO.File]::WriteAllText($journal,($ready|ConvertTo-Json -Depth 15))
    Expected (Invoke-Helper $salted 'status' $fixture @{}) 1 'VM ownership Notes/UUID/name/generation mismatch'
    [IO.File]::WriteAllText($journal,$intentBytes)
    $remove='Remove-VM -VM $vm -Force'
    $cleanupCrash=Replace-Once $salted $remove ($remove+';[Environment]::Exit(86)')
    Expected (Invoke-Helper $cleanupCrash 'cleanup-incomplete' $fixture @{}) 86
    $deletingBytes=[IO.File]::ReadAllText($journal);$deleting=$deletingBytes|ConvertFrom-Json
    Check ($deleting.phase -eq 'deleting' -and $deleting.vm_id -eq [string]$fixtureId -and !(Get-VM -Id $fixtureId -ErrorAction SilentlyContinue) -and (Test-Path -LiteralPath (Join-Path $fixture 'seed-keys\bootstrap')) -and (Test-Path -LiteralPath (Join-Path $fixture 'disks\os.vhdx'))) 'Cleanup interruption missed the unregistered-VM/pending-files boundary'
    [Console]::Error.WriteLine('builder recovery self-check: allocated candidate with empty Notes recovered; wrong run and ordinary ready empty Notes rejected')
    $deleting.cleanup_paths[0]=Join-Path $root 'identity'
    [IO.File]::WriteAllText($journal,($deleting|ConvertTo-Json -Depth 15))
    Expected (Invoke-Helper $salted 'cleanup-incomplete' $fixture @{}) 1 'Cleanup paths differ'
    [IO.File]::WriteAllText($journal,$deletingBytes)
    $deleting=$deletingBytes|ConvertFrom-Json
    $replacement=New-VM -Name $deleting.name -Generation 2 -MemoryStartupBytes 4GB -NoVHD -Path (Join-Path $work 'replacement')
    $replacementNotes="ilex-builder recovery replacement owner=$($deleting.owner) run=$([guid]::NewGuid())"
    Set-VM -VM $replacement -Notes $replacementNotes -AutomaticStartAction Nothing
    Expected (Invoke-Helper $salted 'cleanup-incomplete' $fixture @{}) 1 'name was reused by a different UUID'
    $replacement=Get-VM -Id $replacement.Id
    Check ($replacement.Notes -eq $replacementNotes -and $replacement.State -eq 'Off' -and (Test-Path -LiteralPath (Join-Path $fixture 'seed-keys\bootstrap'))) 'Replacement guard mutated resources'
    $replacementId=[string]$replacement.Id
    Remove-VM -VM $replacement -Force;$replacement=$null
    Expected (Invoke-Helper $salted 'cleanup-incomplete' $fixture @{}) 0
    Check (!(Test-Path -LiteralPath $journal) -and !(Test-Path -LiteralPath $partial)) 'Cleanup did not finish its journal'
    foreach($path in $deleting.cleanup_paths){Check (!(Test-Path -LiteralPath $path)) "Cleanup left an owned path: $path"}
    Assert-RetainedProfile
    $result=@{schema=1;retained_vm_id=[string]$expectedId;retained_state='Off';retained_profile_restored=$true;memory_interruption_rejected=$true;cpu_interruption_rejected=$true;allocated_vm_id=[string]$fixtureId;allocated_empty_notes_recovered=$true;invalid_run_rejected=$true;ready_empty_notes_rejected=$true;cleanup_missing_vm_resumed=$true;unsafe_cleanup_path_rejected=$true;replacement_vm_id=$replacementId;replacement_uuid_protected=$true;fixture_resources_removed=$true;helper_sha256=(Get-FileHash -LiteralPath $r.helper).Hash.ToLower()}
    [Console]::Error.WriteLine('builder recovery self-check: resumed deletion after unregister; replacement UUID and outside paths protected; fixture removed')
} finally {
    # Restore only the exact retained UUID; never start it or touch a name match.
    $vm=Retained-VM
    Set-VMMemory -VM $vm -DynamicMemoryEnabled $false -StartupBytes $baseline.memory_bytes
    Set-VMProcessor -VM $vm -Count $baseline.cpus -ExposeVirtualizationExtensions $true
    if($replacement){
        $vm=Get-VM -Id $replacement.Id
        if($vm.Notes -ne $replacementNotes -or $vm.State -ne 'Off'){throw 'Replacement fixture ownership mismatch during cleanup'}
        Remove-VM -VM $vm -Force
    }
    # Retain a failed fixture's journal for exact recovery rather than erase it.
    if(Test-Path -LiteralPath (Join-Path $fixture 'journal.json')) {
        $cleanup=Invoke-Helper $salted 'cleanup-incomplete' $fixture @{}
        if($cleanup.Code -ne 0){throw "Recovery fixture retained for exact-UUID cleanup at $fixture`: $($cleanup.Err)"}
    }
    $absolute=[IO.Path]::GetFullPath($work)
    if(!$absolute.StartsWith($checks+'\',[StringComparison]::OrdinalIgnoreCase)){throw 'Unsafe recovery fixture cleanup root'}
    foreach($entry in @((Get-Item -LiteralPath $absolute -Force)) + @(Get-ChildItem -LiteralPath $absolute -Recurse -Force)){
        if(!$entry.FullName.StartsWith($checks+'\',[StringComparison]::OrdinalIgnoreCase) -or ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint)){throw 'Unsafe recovery fixture cleanup child'}
    }
    Remove-Item -LiteralPath $absolute -Recurse -Force
}
$result|ConvertTo-Json -Depth 8
