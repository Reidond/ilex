param([Parameter(Mandatory=$true)][string]$Request)
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false)
$r=Get-Content -LiteralPath $Request -Raw -Encoding UTF8|ConvertFrom-Json
if($r.schema -ne 1){throw 'Unsupported maintenance-check schema'}
Import-Module Hyper-V
$work=[IO.Path]::GetFullPath($r.work)
$fixture=Join-Path $work 'builder'
[IO.Directory]::CreateDirectory($fixture)|Out-Null
$production=[IO.File]::ReadAllText($r.helper)
$validationRun=[guid]::NewGuid().ToString()
$capacityDeadline=[DateTime]::UtcNow.AddSeconds([double]$r.options.wait_for_memory_seconds)
$sequence=0;$fixtureId=$null;$result=@{schema=1;host_only=$true;guest_update_sent=$false}
function Check([bool]$ok,[string]$message){if(!$ok){throw $message}}
function Replace-Once([string]$code,[string]$anchor,[string]$replacement){
    $at=$code.IndexOf($anchor,[StringComparison]::Ordinal)
    if($at -lt 0 -or $code.IndexOf($anchor,$at+$anchor.Length,[StringComparison]::Ordinal) -ge 0){throw "Maintenance injection anchor is not unique: $anchor"}
    return $code.Substring(0,$at)+$replacement+$code.Substring($at+$anchor.Length)
}
function Invoke-Helper([string]$code,[string]$operation,$options=@{}){
    if($options.ContainsKey('wait_for_memory_seconds') -and $options.wait_for_memory_seconds -gt 0) {
        $options.wait_for_memory_seconds=[Math]::Min([int]$options.wait_for_memory_seconds,[Math]::Max(0,[int]($capacityDeadline-[DateTime]::UtcNow).TotalSeconds))
    }
    $script:sequence++
    $scriptPath=Join-Path $work "helper-$sequence.ps1";$requestPath=Join-Path $work "request-$sequence.json"
    [IO.File]::WriteAllText($scriptPath,$code)
    [IO.File]::WriteAllText($requestPath,(@{schema=1;operation=$operation;root=$fixture;revision=$r.revision;runtime=$r.runtime;image=$r.image;options=$options;validation_run=$validationRun}|ConvertTo-Json -Depth 8))
    $psi=New-Object Diagnostics.ProcessStartInfo
    $psi.FileName=(Get-Command powershell.exe).Source
    $psi.Arguments="-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$scriptPath`" -Request `"$requestPath`""
    $psi.UseShellExecute=$false;$psi.CreateNoWindow=$true;$psi.RedirectStandardOutput=$true;$psi.RedirectStandardError=$true
    $p=New-Object Diagnostics.Process;$p.StartInfo=$psi
    $null=$p.Start();$stdout=$p.StandardOutput.ReadToEndAsync();$stderr=$p.StandardError.ReadToEndAsync()
    if(!$p.WaitForExit(([int]$r.options.wait_for_memory_seconds+1200)*1000)){$p.Kill();$p.WaitForExit();throw 'Maintenance helper timed out'}
    $reply=[pscustomobject]@{Code=$p.ExitCode;Out=$stdout.Result;Err=$stderr.Result};$p.Dispose();return $reply
}
function Expected($reply,[int]$code,[string]$pattern=''){
    Check ($reply.Code -eq $code) "Unexpected maintenance helper exit $($reply.Code): $($reply.Err)"
    if($pattern){Check ($reply.Err -match $pattern) "Missing expected maintenance diagnostic: $($reply.Err)"}
}
function Journal {return Get-Content -LiteralPath (Join-Path $fixture 'journal.json') -Raw -Encoding UTF8|ConvertFrom-Json}
function Save-Journal($record){[IO.File]::WriteAllText((Join-Path $fixture 'journal.json'),($record|ConvertTo-Json -Depth 15))}
function Fixture-VM {
    $record=Journal
    $vm=Get-VM -Id $fixtureId -ErrorAction Stop
    Check ($record.vm_id -eq [string]$fixtureId -and $vm.Notes -eq $record.notes -and $vm.State -eq 'Off') 'Maintenance fixture identity/off-state mismatch'
    return $vm
}
function Fresh-Status {
    $reply=Invoke-Helper $script:salted 'status';Expected $reply 0
    return $reply.Out|ConvertFrom-Json
}
function File-Inventory {
    # Compare every existing relative file/directory, size and small-file digest.
    # Private-key contents never leave the process or appear in the result JSON.
    return @(Get-ChildItem -LiteralPath $fixture -Recurse -Force | Sort-Object FullName | ForEach-Object {
        $managed=$_.FullName.StartsWith((Join-Path $fixture 'config')+'\',[StringComparison]::OrdinalIgnoreCase) -or $_.FullName.StartsWith((Join-Path $fixture 'disks')+'\',[StringComparison]::OrdinalIgnoreCase)
        $digest='';if(!$_.PSIsContainer -and !$managed -and $_.Length -lt 1MB){$digest=(Get-FileHash -LiteralPath $_.FullName).Hash}
        "$($_.FullName.Substring($fixture.Length))|$($_.PSIsContainer)|$($_.Length)|$digest"
    }) -join "`n"
}
function Fault-After([string]$code,[string]$anchor){return Replace-Once $code $anchor ($anchor+';[Environment]::Exit(86)')}
function Mark-RolledBack {
    # This is a host-only fixture, not a claim that guest smoke ran. Supply the
    # acceptance precondition solely to exercise durable checkpoint removal.
    $record=Journal;$record.update_phase='rolled-back';Save-Journal $record
}
function Accept-Checkpoint {
    Mark-RolledBack
    Expected (Invoke-Helper $script:salted 'accept-update') 0
    Check (!(Journal).checkpoint_id -and !(Journal).update_phase) 'Checkpoint acceptance did not clear recovery metadata'
}
$noStart=Replace-Once $production 'Start-VM -VM $vm | Out-Null' "throw 'Unsafe start reached by maintenance self-check'"
$salted=Replace-Once $noStart 'GetBytes("github.com/Reidond/ilex`0$sid")' 'GetBytes("github.com/Reidond/ilex`0$sid`0$($r.validation_run)")'
$finishPause=Replace-Once $salted "function Finish-Rollback {" "function Finish-Rollback {`n    [Environment]::Exit(86)"
$intentStep="`$j.update_phase='checkpointing';Write-Journal"
$createStep='$snap=Checkpoint-VM -VM $vm -SnapshotName $j.checkpoint_name -Passthru -ErrorAction Stop'
$updatingStep="`$j.checkpoint_id=[string]`$snap.Id;`$j.update_phase='updating';Write-Journal"
$restoringStep="`$j.checkpoint_id=[string]`$snapshot.Id;`$j.update_phase='restoring';Write-Journal"
$restoreStep='Restore-VMSnapshot -VMSnapshot $snapshot -Confirm:$false -ErrorAction Stop'
try {
    foreach($file in @('provision.sh','builder-control.sh','smoke.sh')){[IO.File]::WriteAllText((Join-Path $fixture $file),[IO.File]::ReadAllText((Join-Path $work $file)))}
    $flush='    [IlexAtomicFile]::Replace($tmp,$journal)'
    $allocated=Replace-Once $salted $flush ("    if(`$script:j.phase -eq 'allocated'){[Environment]::Exit(86)}`n"+$flush)
    Expected (Invoke-Helper $allocated 'up' @{memory_gib=4;cpus=4;wait_for_memory_seconds=$r.options.wait_for_memory_seconds}) 86
    $candidate=Get-Content -LiteralPath (Join-Path $fixture 'journal.json.partial') -Raw -Encoding UTF8|ConvertFrom-Json
    $fixtureId=[guid]$candidate.vm_id
    # Complete only host configuration. No fixture VM is ever started.
    $vm=Get-VM -Id $fixtureId -ErrorAction Stop
    Check ($vm.Name -eq $candidate.name -and [string]::IsNullOrEmpty($vm.Notes) -and $vm.ConfigurationLocation.StartsWith((Join-Path $fixture 'config')+'\',[StringComparison]::OrdinalIgnoreCase)) 'Allocated fixture authorization failed'
    Set-VM -VM $vm -Notes $candidate.notes -AutomaticCheckpointsEnabled $false -AutomaticStartAction Nothing -CheckpointType Standard
    Set-VMMemory -VM $vm -DynamicMemoryEnabled $false -StartupBytes 4GB
    Set-VMProcessor -VM $vm -Count 4 -ExposeVirtualizationExtensions $true
    Add-VMHardDiskDrive -VM $vm -Path (Join-Path $fixture 'disks\data.vhdx')
    $candidate.phase='ready';$candidate|Add-Member -NotePropertyName maintenance_fixture -NotePropertyValue $true
    Save-Journal $candidate
    Remove-Item -LiteralPath (Join-Path $fixture 'journal.json.partial')
    $null=Fresh-Status

    $memoryStep='Set-VMMemory -VM (Owned-VM) -DynamicMemoryEnabled $false -StartupBytes $requestedMemory'
    Expected (Invoke-Helper (Fault-After $salted $memoryStep) 'up' @{memory_gib=16;wait_for_memory_seconds=0}) 86
    $before=File-Inventory
    Expected (Invoke-Helper $salted 'update' @{wait_for_memory_seconds=0}) 1 'Actual VM resources differ from journal'
    Check ((File-Inventory) -eq $before -and !(Journal).update_phase -and @(Get-VMSnapshot -VM (Fixture-VM) -ErrorAction Stop).Count -eq 0) 'Drifted update changed state or created a checkpoint'
    $repair=Fault-After $salted '$j.memory_bytes=$requestedMemory;$j.cpus=$requestedCpus;Write-Journal'
    Expected (Invoke-Helper $repair 'up' @{memory_gib=4;cpus=4;wait_for_memory_seconds=0}) 86
    Check ((Fresh-Status).actual_resources.memory_bytes -eq 4GB) 'Ordinary off-state repair was blocked after update refusal'
    $result.update_drift_rejected_before_checkpoint=$true

    $afterWait=Replace-Once $salted 'Preflight $admittedMemory' ('Preflight $admittedMemory'+"`n            Set-VMMemory -VM (Owned-VM) -StartupBytes 16GB")
    Expected (Invoke-Helper $afterWait 'update' @{wait_for_memory_seconds=$r.options.wait_for_memory_seconds}) 1 'Actual VM resources differ from journal'
    Check (!(Journal).update_phase -and @(Get-VMSnapshot -VM (Fixture-VM) -ErrorAction Stop).Count -eq 0) 'Post-wait drift created a recovery transaction'
    Expected (Invoke-Helper $repair 'up' @{memory_gib=4;cpus=4;wait_for_memory_seconds=0}) 86
    $result.post_wait_drift_rejected=$true
    [Console]::Error.WriteLine('maintenance self-check: update rejected actual/journal drift before checkpointing, including drift after capacity wait; repair remained usable')

    Expected (Invoke-Helper (Fault-After $salted $intentStep) 'update' @{wait_for_memory_seconds=$r.options.wait_for_memory_seconds}) 86
    Check ((Journal).update_phase -eq 'checkpointing' -and !(Journal).checkpoint_id) 'Checkpoint intent interruption missed its boundary'
    Expected (Invoke-Helper $salted 'rollback') 0
    Check (!(Journal).update_phase) 'Checkpoint intent without a snapshot could not be abandoned'
    $result.checkpoint_intent_recovered=$true

    foreach($point in @($createStep,$updatingStep)) {
        Expected (Invoke-Helper (Fault-After $salted $point) 'update' @{wait_for_memory_seconds=$r.options.wait_for_memory_seconds}) 86
        Expected (Invoke-Helper $finishPause 'rollback') 86
        Check ((Journal).update_phase -eq 'restored' -and (Fresh-Status).actual_resources.memory_bytes -eq 4GB) 'Checkpoint creation/updating interruption did not restore'
        Accept-Checkpoint
    }
    $result.checkpoint_return_and_journal_recovered=$true

    foreach($point in @($restoringStep,$restoreStep)) {
        Expected (Invoke-Helper (Fault-After $salted $updatingStep) 'update' @{wait_for_memory_seconds=$r.options.wait_for_memory_seconds}) 86
        Expected (Invoke-Helper (Fault-After $finishPause $point) 'rollback') 86
        Check ((Journal).update_phase -eq 'restoring') 'Restore interruption missed its durable phase'
        Expected (Invoke-Helper $finishPause 'rollback') 86
        Check ((Journal).update_phase -eq 'restored') 'Interrupted restoration was not resumable'
        Accept-Checkpoint
    }
    $result.restore_before_and_after_api_recovered=$true

    # Reproduce the legacy bad checkpoint the review found: 16 GiB in the
    # snapshot, 4 GiB in its owner journal. Only this private fixture is changed.
    $vm=Fixture-VM;Set-VMMemory -VM $vm -StartupBytes 16GB
    $bad=Checkpoint-VM -VM $vm -SnapshotName "ilex-builder-before-$([guid]::NewGuid())" -Passthru -ErrorAction Stop
    $record=Journal;$record.checkpoint_id=[string]$bad.Id
    $record|Add-Member -NotePropertyName checkpoint_name -NotePropertyValue $bad.Name -Force
    $record|Add-Member -NotePropertyName checkpoint_revision -NotePropertyValue $r.revision -Force
    $record.update_phase='updating';Save-Journal $record
    Expected (Invoke-Helper $salted 'rollback') 1 'Actual VM resources differ from journal'
    Check ((Journal).update_phase -eq 'restored') 'Legacy bad snapshot did not leave a repairable restored phase'
    Expected (Invoke-Helper $finishPause 'up' @{memory_gib=4;cpus=4;wait_for_memory_seconds=0}) 86
    Check ((Fresh-Status).actual_resources.memory_bytes -eq 4GB) 'Restored legacy checkpoint blocked explicit original-profile repair'
    $noRepeat=Replace-Once $finishPause $restoreStep "throw 'Unexpected repeated restore after repair'"
    Expected (Invoke-Helper $noRepeat 'rollback') 86
    Accept-Checkpoint
    $result.legacy_drift_checkpoint_repair_resumable=$true

    foreach($point in @("`$j.update_phase='accepting';Write-Journal",'Remove-VMSnapshot -VMSnapshot $snapshot -Confirm:$false -ErrorAction Stop')) {
        Expected (Invoke-Helper (Fault-After $salted $updatingStep) 'update' @{wait_for_memory_seconds=$r.options.wait_for_memory_seconds}) 86
        Mark-RolledBack
        Expected (Invoke-Helper (Fault-After $salted $point) 'accept-update') 86
        $interruptedAcceptance=Journal
        Expected (Invoke-Helper $salted 'accept-update') 0
        Check (!(Journal).update_phase -and @(Get-VMSnapshot -VM (Fixture-VM) -ErrorAction Stop).Count -eq 0) 'Interrupted checkpoint acceptance was not resumable'
        if($point -like 'Remove-VMSnapshot*') {
            # Older clients did not record accepting before removal. Explicit
            # acceptance must also clear that positively confirmed missing ID.
            $interruptedAcceptance.update_phase='rolled-back';Save-Journal $interruptedAcceptance
            Expected (Invoke-Helper $salted 'accept-update') 0
            Check (!(Journal).checkpoint_id) 'Legacy interrupted acceptance stayed stuck'
            $result.legacy_acceptance_recovered=$true
        }
    }
    $result.accept_before_and_after_api_recovered=$true
    [Console]::Error.WriteLine('maintenance self-check: checkpoint intent/return, restoration, legacy resource repair, and acceptance interruptions recovered while the fixture stayed off')

    $record=Journal;$record.phase='provisioning';Save-Journal $record
    $deleteStep="`$j.phase='deleting';Write-Journal"
    Expected (Invoke-Helper (Fault-After $salted $deleteStep) 'cleanup-incomplete') 86
    Check ((Journal).phase -eq 'deleting') 'Pre-unregister cleanup interruption missed deleting phase'
    $inventoryAnchor='$inventory=@(Get-VM -ErrorAction Stop -ErrorVariable lookupErrors)'
    $nonterminating=Replace-Once $salted $inventoryAnchor @'
$inventory=@(& { [CmdletBinding()]param(); Write-Error 'Injected Hyper-V provider lookup failure' -ErrorAction Continue } -ErrorAction Stop -ErrorVariable lookupErrors)
'@
    $terminating=Replace-Once $salted $inventoryAnchor "throw [UnauthorizedAccessException]::new('Injected Hyper-V access lookup failure')"
    $before=File-Inventory
    foreach($fault in @($nonterminating,$terminating)) {
        Expected (Invoke-Helper $fault 'cleanup-incomplete') 1 'lookup failure|lookup failed'
        Check ((File-Inventory) -eq $before) 'Provider failure deleted or changed cleanup files/journal'
        $null=Fixture-VM
    }
    $result.registered_vm_lookup_failure_deleted_nothing=$true
    $removeStep='Remove-VM -VM $vm -Force'
    Expected (Invoke-Helper (Fault-After $salted $removeStep) 'cleanup-incomplete') 86
    $inventory=@(Get-VM -ErrorAction Stop)
    Check (!@($inventory|Where-Object {$_.Id -eq $fixtureId}).Count) 'Cleanup did not unregister exact fixture UUID'
    $before=File-Inventory
    foreach($fault in @($nonterminating,$terminating)) {
        Expected (Invoke-Helper $fault 'cleanup-incomplete') 1 'lookup failure|lookup failed'
        Check ((File-Inventory) -eq $before) 'Failed absence lookup deleted or changed files/journal'
    }
    $result.absent_vm_lookup_failure_deleted_nothing=$true
    Expected (Invoke-Helper $salted 'cleanup-incomplete') 0
    Check (!(Test-Path -LiteralPath (Join-Path $fixture 'journal.json'))) 'Successful absence retry did not finish cleanup'
    $result.fixture_id=[string]$fixtureId;$result.fixture_removed=$true
    $result.helper_sha256=(Get-FileHash -LiteralPath $r.helper).Hash.ToLower()
    [Console]::Error.WriteLine('maintenance self-check: terminating and nonterminating lookup faults preserved every file and journal before/after unregister; successful inventory then allowed cleanup')
} finally {
    $journalPath=Join-Path $fixture 'journal.json'
    if(Test-Path -LiteralPath $journalPath) {
        $record=Journal
        if($record.phase -eq 'ready') {
            Check ($record.maintenance_fixture -and $record.vm_id -eq [string]$fixtureId) 'Refusing cleanup of a non-fixture ready VM'
            $record.phase='provisioning';Save-Journal $record
        }
        $cleanup=Invoke-Helper $salted 'cleanup-incomplete'
        if($cleanup.Code -ne 0){throw "Exact maintenance fixture recovery records retained at $fixture`: $($cleanup.Err)"}
    }
}
$result|ConvertTo-Json -Depth 8
