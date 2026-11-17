param([Parameter(Mandatory=$true)][string]$Request)
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false)
$r=Get-Content -LiteralPath $Request -Raw -Encoding UTF8 | ConvertFrom-Json
if ($r.schema -ne 1) {throw 'Unsupported builder request schema; no mutation performed'}
$root=[IO.Path]::GetFullPath($r.root)
$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$sha=[Security.Cryptography.SHA256]::Create()
$hash=([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes("github.com/Reidond/ilex`0$sid")))).Replace('-','').ToLower()
$name="ilex-builder-$($hash.Substring(0,12))"
$owner="ilex-builder:$hash"
$journal=Join-Path $root 'journal.json'
$memoryDeadline=[DateTime]::UtcNow.AddSeconds([double]$r.options.wait_for_memory_seconds)
$j=$null
Add-Type @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.IO;
using System.IO.Pipes;
using System.Threading;
public static class IlexAtomicFile {
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)]
 private static extern bool MoveFileEx(string source,string destination,int flags);
 public static void Replace(string source,string destination) {
  if(!MoveFileEx(source,destination,9)) throw new Win32Exception(Marshal.GetLastWin32Error());
 }
}
public sealed class IlexSerialCapture : IDisposable {
 private volatile bool stopping;
 private NamedPipeClientStream active;
 private readonly Thread thread;
 public IlexSerialCapture(string pipeName,string output) {
  thread=new Thread(delegate() {
   bool reported=false;
   while(!stopping) {
    try {
     using(var pipe=new NamedPipeClientStream(".",pipeName,PipeDirection.In)) {
      active=pipe; pipe.Connect(1000);
      using(var file=new FileStream(output,FileMode.Append,FileAccess.Write,FileShare.Read)) {
       byte[] buffer=new byte[16384];
       while(!stopping) {int n=pipe.Read(buffer,0,buffer.Length);if(n==0)break;file.Write(buffer,0,n);file.Flush();}
      }
     }
    } catch(IOException) {} catch(TimeoutException) {} catch(ObjectDisposedException) {}
    catch(Exception error) {if(!reported) {Console.Error.WriteLine("builder serial diagnostics: "+error.Message);reported=true;}Thread.Sleep(500);}
    finally {active=null;}
   }
  });
  thread.IsBackground=true;thread.Start();
 }
 public void Dispose() {stopping=true;var pipe=active;if(pipe!=null)pipe.Dispose();thread.Join(2000);}
}
'@
function Write-Journal {
    $bytes=[Text.Encoding]::UTF8.GetBytes(($script:j | ConvertTo-Json -Depth 15))
    $tmp="$journal.partial"
    $file=[IO.File]::Open($tmp,'Create','Write','None')
    try {$file.Write($bytes,0,$bytes.Length);$file.Flush($true)} finally {$file.Dispose()}
    [IlexAtomicFile]::Replace($tmp,$journal)
}
function Say([string]$message) {[Console]::Error.WriteLine("builder: $message")}
function Native([string]$exe,[string[]]$arguments) {
    & $exe @arguments | ForEach-Object {[Console]::Error.WriteLine($_)}
    if($LASTEXITCODE -ne 0) {throw "$exe failed ($LASTEXITCODE)"}
}
function Assert-JournalIdentity($record) {
    if($record.schema -ne 1 -or $record.owner -ne $owner -or $record.name -ne $name -or $record.root -ne $root -or $record.notes -ne "ilex-builder schema=1 owner=$owner run=$($record.run)") {throw 'Builder journal identity/version mismatch; no mutation performed'}
    $null=[guid]::Parse($record.run)
    if($record.phase -notin @('preparing','allocated','creating','provisioning','ready','deleting')) {throw 'Unknown builder journal phase'}
    if($record.vm_id) {
        $null=[guid]::Parse($record.vm_id)
        $resource=$record.resource
        if($resource.schema -ne 1 -or $resource.owner -ne $owner -or $resource.run -ne $record.run -or $resource.provider -ne 'hyperv-builder' -or $resource.opaque_id -ne $record.vm_id -or $resource.created_unix_ms -le 0) {throw 'OwnedResource journal identity mismatch'}
        $paths=@($resource.paths | ForEach-Object {Assert-Path $_})
        $modern=@((Join-Path $root 'disks'),(Join-Path $root 'config'))
        $legacy=@((Join-Path $root 'os.vhdx'),(Join-Path $root 'data.vhdx'),(Join-Path $root 'config'))
        if(!(($paths.Count -eq 2 -and !(Compare-Object $paths $modern)) -or ($paths.Count -eq 3 -and !(Compare-Object $paths $legacy)))) {throw 'OwnedResource paths differ from the builder creation layout'}
    } elseif($record.resource -or $record.phase -notin @('preparing','deleting')) {throw 'Journal phase requires an exact VM UUID'}
    if($record.phase -eq 'deleting') {Assert-CleanupManifest $record}
}
function VM-Inventory {
    # A missing UUID is established only by a successful complete query. Never
    # turn provider/access errors into an empty inventory, even nonterminating ones.
    $lookupErrors=@()
    $inventory=@(Get-VM -ErrorAction Stop -ErrorVariable lookupErrors)
    if(!$? -or $lookupErrors.Count) {throw 'Hyper-V inventory lookup failed; absence is unconfirmed'}
    foreach($entry in $inventory) {if(!$entry.Id -or !$entry.Name){throw 'Malformed Hyper-V inventory; absence is unconfirmed'}}
    return $inventory
}
function Name-Conflict {
    return @(VM-Inventory | Where-Object {$_.Name -eq $name}).Count -gt 0
}
function Assert-VMAbsent($record) {
    $inventory=@(VM-Inventory)
    if($record.vm_id -and @($inventory | Where-Object {$_.Id -eq [guid]$record.vm_id}).Count) {throw 'Owned VM is still registered; no files were deleted'}
    if(@($inventory | Where-Object {$_.Name -eq $name}).Count) {throw 'Builder name was reused by a different UUID; refusing cleanup of replacement resources'}
}
function Resolve-OwnedVM($record,[bool]$allowDeleted=$false) {
    Assert-JournalIdentity $record
    if(!$record.vm_id) {return $null}
    $inventory=@(VM-Inventory)
    $vmMatches=@($inventory | Where-Object {$_.Id -eq [guid]$record.vm_id})
    if($vmMatches.Count -gt 1) {throw 'Ambiguous exact UUID in Hyper-V inventory'}
    $vm=$vmMatches | Select-Object -First 1
    if(!$vm) {
        if($allowDeleted -and $record.phase -eq 'deleting') {
            if(@($inventory | Where-Object {$_.Name -eq $name}).Count) {throw 'Builder name was reused by a different UUID; refusing cleanup of replacement resources'}
            return $null
        }
        throw "Owned VM UUID $($record.vm_id) is missing; refusing name-based adoption"
    }
    $allocated=$record.phase -eq 'allocated' -or ($record.phase -eq 'deleting' -and $record.cleanup_from_phase -eq 'allocated')
    $notesPending=$allocated -and [string]::IsNullOrEmpty($vm.Notes)
    if((!$notesPending -and $vm.Notes -ne $record.notes) -or $vm.Name -ne $name -or $vm.Generation -ne 2) {throw 'VM ownership Notes/UUID/name/generation mismatch; refusing mutation'}
    $config=Assert-Path $vm.ConfigurationLocation
    $configRoot=Join-Path $root 'config'
    if($config -ne $configRoot -and !$config.StartsWith($configRoot+'\',[StringComparison]::OrdinalIgnoreCase)) {throw 'Owned VM configuration path mismatch'}
    foreach($drive in (Get-VMHardDiskDrive -VM $vm)) {
        $path=Assert-Path $drive.Path
        if([IO.Path]::GetDirectoryName($path) -ne (Join-Path $root 'disks') -and $path -notin @((Join-Path $root 'os.vhdx'),(Join-Path $root 'data.vhdx'))) {throw 'Owned VM disk path mismatch'}
    }
    return $vm
}
function Owned-VM {
    if(!$script:j) {return $null}
    return Resolve-OwnedVM $script:j ($r.operation -in @('status','cleanup-incomplete'))
}
function Assert-Path([string]$path) {
    $absolute=[IO.Path]::GetFullPath($path)
    if(!$absolute.StartsWith($root+'\',[StringComparison]::OrdinalIgnoreCase)) {throw "Unsafe owned path: $absolute"}
    $part=$absolute
    while($part -and $part.Length -ge $root.Length) {
        if(Test-Path -LiteralPath $part) {
            if((Get-Item -LiteralPath $part -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {throw "Reparse point in owned path: $part"}
        }
        $part=[IO.Path]::GetDirectoryName($part)
    }
    return $absolute
}
function Remove-Owned([string]$path) {
    $absolute=Assert-Path $path
    if(Test-Path -LiteralPath $absolute) {
        # Validate every resolved child before recursive removal, within this one shell.
        Get-ChildItem -LiteralPath $absolute -Recurse -Force | ForEach-Object {$null=Assert-Path $_.FullName}
        Remove-Item -LiteralPath $absolute -Recurse -Force
    }
}
function Cleanup-Paths {
    return @('seed','seed-keys','seed.iso','identity','identity.pub','known_hosts','config','disks','os.vhdx','data.vhdx') | ForEach-Object {Assert-Path (Join-Path $root $_)}
}
function Assert-CleanupManifest($record) {
    if($record.cleanup_from_phase -notin @('preparing','allocated','creating','provisioning')) {throw 'Invalid cleanup origin phase'}
    $paths=@($record.cleanup_paths)
    if($paths.Count -ne 10 -or (Compare-Object $paths @(Cleanup-Paths))) {throw 'Cleanup paths differ from the previously authorized creation layout'}
    foreach($path in $paths) {
        $null=Assert-Path $path
        if(Test-Path -LiteralPath $path) {Get-ChildItem -LiteralPath $path -Recurse -Force | ForEach-Object {$null=Assert-Path $_.FullName}}
    }
}
function Read-Journal {
    if(Test-Path -LiteralPath $journal) {
        $script:j=Get-Content -LiteralPath $journal -Raw -Encoding UTF8 | ConvertFrom-Json
        Assert-JournalIdentity $j
    }
    if($r.operation -eq 'status' -or !(Test-Path -LiteralPath "$journal.partial") -or !$j) {return}
    $candidate=Get-Content -LiteralPath "$journal.partial" -Raw -Encoding UTF8 | ConvertFrom-Json
    Assert-JournalIdentity $candidate
    if($candidate.run -ne $j.run -or $candidate.notes -ne $j.notes -or ($j.vm_id -and $candidate.vm_id -ne $j.vm_id)) {throw 'Interrupted journal candidate identity mismatch'}
    if(!$j.vm_id -and $candidate.vm_id) {
        if($j.phase -ne 'preparing' -or $candidate.phase -ne 'allocated' -or $candidate.revision -ne $j.revision -or $candidate.memory_bytes -ne $j.memory_bytes -or $candidate.cpus -ne $j.cpus) {throw 'Allocated journal candidate differs from its creation intent'}
    }
    if($candidate.vm_id) {$null=Resolve-OwnedVM $candidate ($r.operation -eq 'cleanup-incomplete')}
    [IlexAtomicFile]::Replace("$journal.partial",$journal)
    $script:j=$candidate
}
function Ensure-DiskStorage {
    $vm=Owned-VM
    $storage=Assert-Path (Join-Path $root 'disks')
    [IO.Directory]::CreateDirectory($storage)|Out-Null
    # vmwp must create differencing VHDX files, but gets no access to the
    # sibling seed/client identities. Do not grant the Virtual Machines group.
    $account=New-Object Security.Principal.NTAccount('NT VIRTUAL MACHINE',([string]$vm.Id))
    $sid=$account.Translate([Security.Principal.SecurityIdentifier])
    $acl=[IO.Directory]::GetAccessControl($storage,[Security.AccessControl.AccessControlSections]::Access)
    $acl.SetAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow')))
    [IO.Directory]::SetAccessControl($storage,$acl)
    $moves=@()
    foreach($drive in (Get-VMHardDiskDrive -VM $vm)) {
        foreach($file in @('os.vhdx','data.vhdx')) {
            $legacy=Assert-Path (Join-Path $root $file)
            if($drive.Path -eq $legacy) {
                $destination=Assert-Path (Join-Path $storage $file)
                $moves+=@{SourceFilePath=$legacy;DestinationFilePath=$destination}
            }
        }
    }
    if($moves.Count) {
        if($vm.State -ne 'Off' -or @(Get-VMSnapshot -VM $vm).Count){throw 'Legacy disk migration requires the owned VM off with no checkpoints'}
        Move-VMStorage -VM $vm -Vhds $moves
    }
    foreach($drive in (Get-VMHardDiskDrive -VM $vm)) {
        $path=Assert-Path $drive.Path
        if(![IO.Path]::GetDirectoryName($path).Equals($storage,[StringComparison]::OrdinalIgnoreCase)){throw 'Unexpected builder disk outside its owned storage directory'}
    }
    $j.resource.paths=@($storage,(Join-Path $root 'config'));Write-Journal
}
function Preflight([long]$memory) {
    $available=[long](Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory*1KB
    while($available -lt ($memory+14GB)) {
        if([DateTime]::UtcNow -ge $memoryDeadline) {throw "Insufficient available memory: $([math]::Round($available/1GB,1)) GiB; builder needs $($memory/1GB) GiB plus 14 GiB host reserve. No VM was started."}
        Say "Waiting for host capacity: $([math]::Round($available/1GB,1)) GiB available, $(($memory+14GB)/1GB) GiB required"
        Start-Sleep -Seconds 30
        $available=[long](Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory*1KB
    }
    Check-DiskSpace
}
function Check-DiskSpace {
    $drive=Get-PSDrive -Name ([IO.Path]::GetPathRoot($root).Substring(0,1))
    if($drive.Free -lt 45GB) {throw 'Builder needs 45 GiB free (30 GiB reserve plus bootstrap/update peak)'}
}
function Addresses($vm) {
    $nic=@(Get-VMNetworkAdapter -VM $vm)
    if($nic.Count -ne 1 -or $nic[0].SwitchName -ne 'Default Switch') {throw 'Expected exactly one owned Default Switch NIC'}
    $mac=$nic[0].MacAddress.Replace('-','').Replace(':','').ToUpper()
    $switch=Get-VMSwitch -Name 'Default Switch'
    $hostNic=Get-VMNetworkAdapter -ManagementOS | Where-Object {$_.SwitchId -eq $switch.Id} | Select-Object -First 1
    $adapter=Get-NetAdapter -IncludeHidden | Where-Object {$hostNic -and $_.MacAddress -and $_.MacAddress.Replace('-','').Replace(':','') -eq $hostNic.MacAddress} | Select-Object -First 1
    $candidates=@($nic[0].IPAddresses)
    if($adapter) {
        $candidates+=@(Get-NetNeighbor -InterfaceIndex $adapter.ifIndex -ErrorAction SilentlyContinue | Where-Object {$_.LinkLayerAddress -and $_.LinkLayerAddress.Replace('-','').Replace(':','').ToUpper() -eq $mac -and $_.State -notin @('Unreachable','Incomplete')} | ForEach-Object {$_.IPAddress})
    }
    return @($candidates | Where-Object {
        $ip=$null
        [Net.IPAddress]::TryParse($_,[ref]$ip) -and $ip.AddressFamily -eq 'InterNetwork' -and ![Net.IPAddress]::IsLoopback($ip) -and !$_.StartsWith('169.254.') -and $_ -ne '0.0.0.0'
    } | Select-Object -Unique)
}
function Ssh-Args([string]$address) {
    return @('-F','NUL','-i',(Join-Path $root 'identity'),'-o','IdentitiesOnly=yes','-o','BatchMode=yes','-o','ConnectTimeout=3','-o','ConnectionAttempts=1','-o','StrictHostKeyChecking=yes','-o',"UserKnownHostsFile=$(Join-Path $root 'known_hosts')",'-o','GlobalKnownHostsFile=NUL','-o',"HostKeyAlias=$name",'-o','LogLevel=ERROR',"ilex@$address")
}
function Invoke-GuestSsh([string[]]$sshArguments,[string]$op,[int]$timeout) {
    $psi=New-Object Diagnostics.ProcessStartInfo
    $psi.FileName=(Get-Command ssh.exe).Source
    $psi.Arguments=($sshArguments | ForEach-Object {'"'+$_.Replace('"','\"')+'"'}) -join ' '
    $psi.UseShellExecute=$false;$psi.CreateNoWindow=$true
    $psi.RedirectStandardInput=$true;$psi.RedirectStandardOutput=$true;$psi.RedirectStandardError=$true
    $p=New-Object Diagnostics.Process;$p.StartInfo=$psi
    $null=$p.Start();$out=$p.StandardOutput.ReadToEndAsync();$err=$p.StandardError.ReadToEndAsync()
    # The finite guest protocol is UTF-8 with LF framing, including on Windows.
    $p.StandardInput.NewLine="`n"
    $p.StandardInput.WriteLine($op)
    if($op -eq 'update') {
        $payload=@{schema=1;revision=$r.revision;provision=[IO.File]::ReadAllText((Join-Path $root 'provision.sh'));control=[IO.File]::ReadAllText((Join-Path $root 'builder-control.sh'));smoke=[IO.File]::ReadAllText((Join-Path $root 'smoke.sh'))}
        $p.StandardInput.WriteLine(($payload | ConvertTo-Json -Depth 4 -Compress))
    }
    $p.StandardInput.Close()
    if(!$p.WaitForExit($timeout*1000)) {$p.Kill();$p.WaitForExit();throw "Guest $op timed out; provisioning logs retained"}
    return [pscustomobject]@{Code=$p.ExitCode;Out=$out.Result;Err=$err.Result}
}
function Guest([string]$op,[int]$timeout=1200) {
    $vm=Owned-VM
    if($vm.State -ne 'Running') {throw "Builder is $($vm.State), not running"}
    $deadline=[DateTime]::UtcNow.AddSeconds(900)
    while([DateTime]::UtcNow -lt $deadline) {
        foreach($address in (Addresses $vm)) {
            $sshArguments=Ssh-Args $address
            $errFile=Join-Path $root 'ssh-last-error.txt'
            $reply=Invoke-GuestSsh $sshArguments 'probe' 12
            [IO.File]::WriteAllText($errFile,$reply.Err)
            if($reply.Code -ne 0) {continue}
            try {$identity=$reply.Out | ConvertFrom-Json} catch {continue}
            if($identity.schema -ne 1 -or $identity.owner -ne $owner) {throw 'Pinned SSH endpoint provisioning identity mismatch'}
            if($j.data_uuid -and $identity.data_uuid -ne $j.data_uuid) {throw 'Builder data filesystem identity mismatch'}
            $script:address=$address
            if($op -eq 'probe') {return $identity}
            $reply=Invoke-GuestSsh $sshArguments $op $timeout
            $stdout=$reply.Out;$stderr=$reply.Err
            [IO.File]::WriteAllText((Join-Path $root "guest-$op.log"),$stdout+"`n"+$stderr)
            if($reply.Code -ne 0) {throw "Guest $op failed ($($reply.Code)); $stderr"}
            if($op -eq 'smoke') {return ($stdout | ConvertFrom-Json)}
            return $stdout
        }
        Start-Sleep -Seconds 2
    }
    $nic=Get-VMNetworkAdapter -VM $vm
    throw "SSH discovery/authentication timed out: UUID=$($vm.Id), NIC=$($nic.Name), MAC=$($nic.MacAddress), switch=$($nic.SwitchName). Check $(Join-Path $root 'ssh-last-error.txt'), serial pipe $($j.serial_pipe), and cloud-init console. No subnet scan or unpinned SSH fallback performed."
}
function Stop-Owned {
    $vm=Owned-VM
    if($vm.State -eq 'Off') {return}
    $idle=(Guest 'idle') | ConvertFrom-Json
    if(!$idle.idle) {throw 'Builder has active jobs; refusing shutdown'}
    Stop-VM -VM $vm -Confirm:$false -ErrorAction Stop
    if((Owned-VM).State -ne 'Off') {throw 'Graceful builder shutdown did not complete'}
}
function Wait-Idle {
    $deadline=[DateTime]::UtcNow.AddMinutes(30)
    do {
        $idle=(Guest 'idle') | ConvertFrom-Json
        if($idle.idle) {return}
        if([DateTime]::UtcNow -ge $deadline) {throw 'Timed out waiting for builder jobs to become idle; no update performed'}
        Say 'Waiting for active builder jobs before update'
        Start-Sleep -Seconds 10
    } while($true)
}
function Start-Owned {
    $vm=Owned-VM
    $memory=Assert-Resources $vm
    if($vm.State -eq 'Off') {
        Ensure-DiskStorage
        Preflight $memory
        # Recheck after a potentially long wait, before allocating any host RAM.
        $null=Assert-Resources (Owned-VM)
        Start-VM -VM $vm | Out-Null
    }
    elseif($vm.State -ne 'Running') {throw "Unsupported builder state: $($vm.State)"}
}
function Assert-Resources($vm) {
    if(!$vm) {throw 'No owned VM to admit'}
    $memory=Get-VMMemory -VM $vm
    $processor=Get-VMProcessor -VM $vm
    if($memory.DynamicMemoryEnabled -or $memory.Startup -ne $j.memory_bytes -or $processor.Count -ne $j.cpus -or !$processor.ExposeVirtualizationExtensions) {throw "Actual VM resources differ from journal (possibly interrupted resize); no VM was started. While off, repair explicitly with builder up --memory-gib $([long]$j.memory_bytes/1GB) --cpus $($j.cpus)."}
    return [long]$memory.Startup
}
function Recovery-Snapshot {
    $lookupErrors=@()
    $snapshots=@(Get-VMSnapshot -VM (Owned-VM) -ErrorAction Stop -ErrorVariable lookupErrors)
    if(!$? -or $lookupErrors.Count) {throw 'Hyper-V checkpoint lookup failed; absence is unconfirmed'}
    if($j.checkpoint_id) {$found=@($snapshots | Where-Object {$_.Id -eq [guid]$j.checkpoint_id})}
    elseif($j.checkpoint_name) {$found=@($snapshots | Where-Object {$_.Name -eq $j.checkpoint_name})}
    else {throw 'No owned recovery checkpoint or creation intent'}
    if($found.Count -gt 1) {throw 'Ambiguous owned checkpoint creation intent'}
    if($found.Count -eq 1) {
        if($j.checkpoint_name -and $found[0].Name -ne $j.checkpoint_name) {throw 'Recovery checkpoint name/UUID mismatch'}
        return $found[0]
    }
    return $null
}
function Clear-Recovery {
    foreach($field in @('checkpoint_id','checkpoint_name','checkpoint_revision','update_phase')) {$j | Add-Member -NotePropertyName $field -NotePropertyValue $null -Force}
    Write-Journal
}
function Finish-Rollback {
    $recoveryRevision=$j.checkpoint_revision
    if(!$recoveryRevision) {$recoveryRevision=$j.revision}
    Start-Owned
    $script:probe=Guest 'smoke'
    if(!$probe.nested_kvm_boot -or $probe.revision -ne $recoveryRevision) {throw 'Recovery checkpoint provisioning/KVM verification failed'}
    $j.revision=$probe.revision;$j.update_phase='rolled-back';Write-Journal
}
function Recover-Update {
    if($j.update_phase -eq 'accepting') {throw 'Checkpoint acceptance was interrupted; resume builder accept-update'}
    if($j.update_phase -eq 'restored') {
        # Do not restore again: an explicit repair may have reconciled a legacy
        # checkpoint's mismatched resources while this phase was pending.
        Finish-Rollback
        return
    }
    $snapshot=Recovery-Snapshot
    if(!$snapshot) {
        if($j.update_phase -ne 'checkpointing') {throw 'Exact recovery checkpoint missing; refusing recovery'}
        $vm=Owned-VM
        if($vm.State -ne 'Off' -or [int]$vm.PrimaryOperationalStatus -ne 2 -or $vm.SecondaryOperationalStatus) {throw 'Checkpoint operation is still pending; retry builder rollback'}
        # Re-query after the idle observation: creation may have completed
        # between the first snapshot query and the VM status query.
        $snapshot=Recovery-Snapshot
        if(!$snapshot) {
            # No guest update was sent; abandon only the unfulfilled intent.
            Clear-Recovery
            return
        }
    }
    if(!$j.checkpoint_revision) {
        if($j.update_phase -eq 'verified') {throw 'Recovery checkpoint revision missing; refusing an unverifiable rollback'}
        $j | Add-Member -NotePropertyName checkpoint_revision -NotePropertyValue $j.revision -Force
    }
    $vm=Owned-VM
    if($j.update_phase -in @('checkpointing','updating','restoring')) {
        if($vm.State -ne 'Off') {Stop-VM -VM $vm -TurnOff -Confirm:$false}
    } else {Stop-Owned}
    $j.checkpoint_id=[string]$snapshot.Id;$j.update_phase='restoring';Write-Journal
    Restore-VMSnapshot -VMSnapshot $snapshot -Confirm:$false -ErrorAction Stop
    $j.update_phase='restored';Write-Journal
    # Keep this durable phase if admission/probing fails. Rollback can retry it,
    # and explicit up options can repair the journal profile without re-restoring.
    Finish-Rollback
}
function Key([string]$file) {
    Native (Get-Command ssh-keygen.exe).Source @('-q','-t','ed25519','-N','""','-f',$file)
}
function Seed {
    $seed=Join-Path $root 'seed';New-Item -ItemType Directory -Path $seed | Out-Null
    $keys=Join-Path $root 'seed-keys';New-Item -ItemType Directory -Path $keys | Out-Null
    Key (Join-Path $keys 'bootstrap')
    Key (Join-Path $keys 'host')
    Key (Join-Path $root 'identity')
    $hostPublic=(Get-Content -LiteralPath (Join-Path $keys 'host.pub') -Raw -Encoding UTF8).Trim()
    [IO.File]::WriteAllText((Join-Path $root 'known_hosts'),"$name $hostPublic`n")
    $files=@(
        @{path='/var/lib/ilex-builder/owner';permissions='0600';content=$owner},
        @{path='/var/lib/ilex-builder/revision';permissions='0600';content=$r.revision}
    )
    foreach($filename in @('provision.sh','builder-control.sh','smoke.sh')) {
        $files+=@{path="/var/lib/ilex-builder/$filename";permissions='0700';content=[IO.File]::ReadAllText((Join-Path $root $filename))}
    }
    # Maintenance key is forced to the finite controller; bootstrap key is revoked by provisioning.
    $maintenance=(Get-Content -LiteralPath (Join-Path $root 'identity.pub') -Raw).Trim()
    $bootstrap=(Get-Content -LiteralPath (Join-Path $keys 'bootstrap.pub') -Raw -Encoding UTF8).Trim()
    $cloud=@{
        hostname=$name;ssh_pwauth=$false;disable_root=$true
        users=@(
            @{name='ilex-bootstrap';lock_passwd=$true;shell='/bin/bash';ssh_authorized_keys=@($bootstrap)},
            @{name='ilex';lock_passwd=$true;shell='/bin/bash';ssh_authorized_keys=@("restrict,command=`"/usr/local/libexec/ilex-builder-control`" $maintenance")}
        )
        ssh_keys=@{ed25519_private=[IO.File]::ReadAllText((Join-Path $keys 'host'));ed25519_public=$hostPublic}
        write_files=$files
        runcmd=@(,@('/bin/bash','/var/lib/ilex-builder/provision.sh',$r.revision))
    }
    [IO.File]::WriteAllText((Join-Path $seed 'user-data'),"#cloud-config`n"+($cloud | ConvertTo-Json -Depth 15))
    [IO.File]::WriteAllText((Join-Path $seed 'meta-data'),"instance-id: $($j.run)`nlocal-hostname: $name`n")
    Add-Type @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
public class IlexIsoWriter {
 public static void Save(object obj,string path) {
  IStream stream=(IStream)obj; byte[] b=new byte[65536]; IntPtr p=Marshal.AllocHGlobal(4);
  try { using(var file=File.Create(path)) { while(true) {stream.Read(b,b.Length,p);int n=Marshal.ReadInt32(p);if(n==0)break;file.Write(b,0,n);}file.Flush(true);} }
  finally {Marshal.FreeHGlobal(p);}
 }
}
'@
    $iso=New-Object -ComObject IMAPI2FS.MsftFileSystemImage
    $iso.FileSystemsToCreate=3;$iso.VolumeName='cidata';$iso.Root.AddTree($seed,$false)
    [IlexIsoWriter]::Save($iso.CreateResultImage().ImageStream,(Join-Path $root 'seed.iso'))
}
function Cleanup-Seed {
    $vm=Owned-VM
    Get-VMDvdDrive -VM $vm | Where-Object {$_.Path -eq (Join-Path $root 'seed.iso')} | Set-VMDvdDrive -Path $null
    foreach($p in @('seed','seed-keys','seed.iso')) {Remove-Owned (Join-Path $root $p)}
}
function Reply {
    $vm=Owned-VM
    $reply=[ordered]@{schema=1;name=$name;owner=$owner;state='absent';provisioning_revision=$r.revision;root=$root}
    if($j) {$reply.journal=$j}
    if($script:probe) {$reply.probe=$script:probe}
    if($vm) {
        $reply.state=[string]$vm.State;$reply.vm_id=[string]$vm.Id;$reply.addresses=@(Addresses $vm)
        $memory=Get-VMMemory -VM $vm;$processor=Get-VMProcessor -VM $vm
        $reply.actual_resources=@{memory_bytes=[long]$memory.Startup;cpus=[int]$processor.Count;dynamic_memory=[bool]$memory.DynamicMemoryEnabled;nested=[bool]$processor.ExposeVirtualizationExtensions}
    }
    if($script:address) {$reply.endpoint=@{schema=1;vm_id=$j.vm_id;owner=$owner;address=$script:address;user='ilex';identity_file=(Join-Path $root 'identity');known_hosts_file=(Join-Path $root 'known_hosts');host_key_alias=$name;provisioning_revision=$j.revision}}
    return ($reply | ConvertTo-Json -Depth 18)
}
try {
    Import-Module Hyper-V -ErrorAction Stop
    Read-Journal
    if($r.operation -eq 'status') {Reply;exit 0}
    if($r.operation -eq 'up') {
        if($j -and $j.update_phase -in @('checkpointing','updating','restoring','accepting')) {throw 'Interrupted maintenance requires builder rollback (or accept-update for accepting) before up'}
        if(!$j) {
            if(Name-Conflict) {throw 'Builder name already exists without an ownership journal; refusing adoption'}
            $memory=16GB;if($r.options.memory_gib){$memory=[long]$r.options.memory_gib*1GB}
            $cpus=8;if($r.options.cpus){$cpus=[int]$r.options.cpus}
            if($memory -lt 4GB -or $memory -gt 32GB -or $cpus -lt 2 -or $cpus -gt [Environment]::ProcessorCount) {throw 'Builder resources require 4..32 GiB fixed memory and 2..host CPU count'}
            Preflight $memory
            $run=[guid]::NewGuid().ToString()
            $j=[pscustomobject]@{schema=1;owner=$owner;name=$name;root=$root;run=$run;vm_id=$null;notes="ilex-builder schema=1 owner=$owner run=$run";phase='preparing';revision=$r.revision;memory_bytes=$memory;cpus=$cpus;checkpoint_id=$null;update_phase=$null;serial_pipe="\\.\pipe\$name";resource=$null;data_uuid=$null;data_filesystem='xfs'}
            Write-Journal
            Seed
            $storage=Assert-Path (Join-Path $root 'disks');New-Item -ItemType Directory -Path $storage|Out-Null
            $os=Join-Path $storage 'os.vhdx';$data=Join-Path $storage 'data.vhdx'
            $savedChildPath=$env:PATH
            try {
                $env:PATH=(Join-Path $r.runtime 'ucrt64\bin')+';'+(Join-Path $env:SystemRoot 'System32')
                Native (Join-Path $r.runtime 'ucrt64\bin\qemu-img.exe') @('convert','-S','0','-f','qcow2','-O','vhdx',$r.image,$os)
            } finally {$env:PATH=$savedChildPath}
            Native (Get-Command fsutil.exe).Source @('sparse','setflag',$os,'0')
            $attr=(Get-Item -LiteralPath $os).Attributes
            if($attr -band ([IO.FileAttributes]::SparseFile -bor [IO.FileAttributes]::Compressed -bor [IO.FileAttributes]::Encrypted)) {throw 'OS VHDX remains sparse/compressed/encrypted'}
            Resize-VHD -Path $os -SizeBytes 40GB
            New-VHD -Path $data -Dynamic -SizeBytes 240GB | Out-Null
            $vm=New-VM -Name $name -Generation 2 -MemoryStartupBytes $memory -VHDPath $os -Path (Join-Path $root 'config') -SwitchName 'Default Switch'
            $j.vm_id=[string]$vm.Id
            $j.resource=@{schema=1;owner=$owner;run=$run;provider='hyperv-builder';opaque_id=$j.vm_id;created_unix_ms=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();paths=@($storage,(Join-Path $root 'config'))}
            $j.phase='allocated';Write-Journal
            Set-VM -VM $vm -Notes $j.notes -AutomaticCheckpointsEnabled $false -AutomaticStartAction Nothing -AutomaticStopAction ShutDown
            $j.phase='creating';Write-Journal
            Set-VMMemory -VM $vm -DynamicMemoryEnabled $false
            Set-VMProcessor -VM $vm -Count $cpus -ExposeVirtualizationExtensions $true
            Set-VMFirmware -VM $vm -EnableSecureBoot On -SecureBootTemplate MicrosoftUEFICertificateAuthority
            Set-VMKeyProtector -VM $vm -NewLocalKeyProtector;Enable-VMTPM -VM $vm
            Add-VMHardDiskDrive -VM $vm -Path $data
            Add-VMDvdDrive -VM $vm -Path (Join-Path $root 'seed.iso')
            Set-VMComPort -VM $vm -Number 1 -Path $j.serial_pipe
            $j.phase='provisioning';Write-Journal
        } elseif($j.phase -notin @('ready','provisioning')) {throw "Interrupted bootstrap at $($j.phase); run builder cleanup-incomplete before retrying"}
        if($j.phase -eq 'ready' -and ($r.options.memory_gib -or $r.options.cpus)) {
            $requestedMemory=[long]$j.memory_bytes
            $requestedCpus=[int]$j.cpus
            if($r.options.memory_gib){$requestedMemory=[long]$r.options.memory_gib*1GB}
            if($r.options.cpus){$requestedCpus=[int]$r.options.cpus}
            $actualMemory=Get-VMMemory -VM (Owned-VM)
            $actualProcessor=Get-VMProcessor -VM (Owned-VM)
            if($requestedMemory -ne $j.memory_bytes -or $requestedCpus -ne $j.cpus -or $actualMemory.Startup -ne $requestedMemory -or $actualMemory.DynamicMemoryEnabled -or $actualProcessor.Count -ne $requestedCpus -or !$actualProcessor.ExposeVirtualizationExtensions) {
                $restoredRepair=$j.update_phase -eq 'restored' -and $requestedMemory -eq $j.memory_bytes -and $requestedCpus -eq $j.cpus
                if((Owned-VM).State -ne 'Off' -or ($j.checkpoint_id -and !$restoredRepair)) {throw 'Resource changes require builder down and no retained checkpoint, except original-profile repair after restoration'}
                if($requestedMemory -lt 4GB -or $requestedMemory -gt 32GB -or $requestedCpus -lt 2 -or $requestedCpus -gt [Environment]::ProcessorCount) {throw 'Invalid builder resources'}
                # Changing an off VM's configuration allocates no RAM. Actual
                # resources are reconciled and admitted by Start-Owned below.
                Set-VMMemory -VM (Owned-VM) -DynamicMemoryEnabled $false -StartupBytes $requestedMemory
                Set-VMProcessor -VM (Owned-VM) -Count $requestedCpus -ExposeVirtualizationExtensions $true
                $j.memory_bytes=$requestedMemory;$j.cpus=$requestedCpus;Write-Journal
            }
        }
        if($j.update_phase -eq 'restored') {Finish-Rollback;Reply;exit 0}
        if($j.phase -eq 'provisioning') {$serialCapture=New-Object IlexSerialCapture($name,(Join-Path $root 'serial.log'))}
        Start-Owned
        Say 'Waiting for pinned SSH/cloud-init provisioning identity'
        $probe=Guest 'probe'
        if($probe.revision -ne $r.revision) {
            if($j.phase -ne 'provisioning') {throw 'Provisioning revision differs; builder update is required'}
            Say 'Resuming the unfinished provisioning recipe through the pinned controller'
            $null=Guest 'update'
            $probe=Guest 'probe'
            if($probe.revision -ne $r.revision) {throw 'Resumed provisioning revision mismatch'}
            $j.revision=$r.revision;Write-Journal
        }
        if($j.phase -ne 'ready') {$null=Guest 'smoke';Cleanup-Seed;$j.data_uuid=$probe.data_uuid;$j.phase='ready';Write-Journal}
    } elseif($r.operation -eq 'down') {Stop-Owned}
    elseif($r.operation -eq 'cleanup-incomplete') {
        if(!$j) {Reply;exit 0}
        if($j.phase -eq 'ready') {throw 'Ready builder is persistent infrastructure; cleanup-incomplete cannot destroy it'}
        $vm=Owned-VM
        if($j.phase -ne 'deleting') {
            if(!$vm -and (Name-Conflict)) {throw 'Unjournaled VM exists; UUID authorization unavailable, refusing cleanup'}
            $j | Add-Member -NotePropertyName cleanup_from_phase -NotePropertyValue $j.phase -Force
            $j | Add-Member -NotePropertyName cleanup_paths -NotePropertyValue @(Cleanup-Paths) -Force
            Assert-CleanupManifest $j
            $j.phase='deleting';Write-Journal
        }
        Assert-CleanupManifest $j
        if($vm) {if($vm.State -ne 'Off'){Stop-VM -VM $vm -TurnOff -Confirm:$false};Remove-VM -VM $vm -Force}
        Assert-VMAbsent $j
        foreach($path in $j.cleanup_paths) {Remove-Owned $path}
        Remove-Item -LiteralPath $journal;$j=$null
    } elseif($r.operation -eq 'accept-update') {
        if(!$j -or !$j.checkpoint_id -or $j.update_phase -notin @('verified','rolled-back','accepting')) {throw 'No verified/rolled-back recovery checkpoint to accept'}
        $snapshot=Recovery-Snapshot
        # Explicit acceptance may finish an older interrupted removal too.
        # Recovery-Snapshot must positively confirm absence; errors never clear it.
        $j.update_phase='accepting';Write-Journal
        if($snapshot) {Remove-VMSnapshot -VMSnapshot $snapshot -Confirm:$false -ErrorAction Stop}
        if(Recovery-Snapshot) {throw 'Checkpoint removal is still pending; retry accept-update'}
        Clear-Recovery
    } elseif($r.operation -eq 'self-check') {
        $vm=Owned-VM
        if(!$vm -or $vm.State -ne 'Running') {throw 'Run builder up before the active self-check'}
        $null=Assert-Resources $vm
        if($vm.AutomaticCheckpointsEnabled) {throw 'Automatic checkpoint configuration drift'}
        $probe=Guest 'smoke'
        if($probe.revision -ne $r.revision) {throw 'Provisioning revision differs; run builder update before self-check'}
        if(!$probe.nested_kvm_boot) {throw 'Nested KVM VM did not boot userspace'}
        if([long]$probe.guest_memory_bytes -lt ([long]$j.memory_bytes-512MB) -or $probe.guest_cpus -ne $j.cpus) {throw 'Live guest memory/CPU does not match the fixed resource profile'}
        Say "Nested KVM boot passed; UUID=$($vm.Id); revision=$($probe.revision)"
    } elseif($r.operation -in @('update','rollback')) {
        if(!$j -or $j.phase -ne 'ready') {throw 'Update/rollback requires a ready builder'}
        if($r.operation -eq 'rollback') {
            if(!$j.checkpoint_id -and !$j.checkpoint_name){throw 'No owned recovery checkpoint or creation intent'}
            Recover-Update
        } else {
            if($j.update_phase -in @('checkpointing','updating','restoring','restored','accepting')) {throw 'Interrupted maintenance: run builder rollback (or accept-update for accepting) before another update'}
            if($j.checkpoint_id -or $j.checkpoint_name) {throw 'Recovery checkpoint retained; accept it with builder accept-update before updating again'}
            $vm=Owned-VM
            # Reject interrupted resize before shutdown, journal mutation or any
            # recovery checkpoint. Ordinary explicit off-state repair stays usable.
            $admittedMemory=Assert-Resources $vm
            if($vm.State -eq 'Running'){Wait-Idle;Stop-Owned}
            elseif($vm.State -ne 'Off'){throw "Unsupported builder state for update: $($vm.State)"}
            Ensure-DiskStorage
            Preflight $admittedMemory
            $vm=Owned-VM
            $null=Assert-Resources $vm
            Set-VM -VM $vm -CheckpointType Standard
            $j | Add-Member -NotePropertyName checkpoint_name -NotePropertyValue "ilex-builder-before-$([guid]::NewGuid())" -Force
            $j | Add-Member -NotePropertyName checkpoint_revision -NotePropertyValue $j.revision -Force
            $j.update_phase='checkpointing';Write-Journal
            try {
                $snap=Checkpoint-VM -VM $vm -SnapshotName $j.checkpoint_name -Passthru -ErrorAction Stop
                $j.checkpoint_id=[string]$snap.Id;$j.update_phase='updating';Write-Journal
                Start-Owned
                $null=Guest 'update'
                $updatedProbe=Guest 'probe'
                if($updatedProbe.revision -ne $r.revision) {throw 'Updated guest provisioning revision mismatch'}
                Say "Applied provisioning revision $($updatedProbe.revision)"
                if($r.options.rollback_probe){throw 'Requested rollback acceptance probe after real package update'}
                Stop-Owned;Start-Owned;$probe=Guest 'smoke'
                if(!$probe.nested_kvm_boot -or $probe.revision -ne $r.revision) {throw 'Updated guest reboot/KVM verification failed'}
                $j.revision=$r.revision;$j.update_phase='verified';Write-Journal
            } catch {
                $failure=$_
                try {Recover-Update} catch {throw "Update failed: $failure; recovery remains $($j.update_phase). Resume builder rollback (explicit original-profile up options can repair restored resources): $_"}
                throw "Update failed and owned recovery completed ($($j.update_phase)): $failure"
            }
        }
    } else {throw "Unsupported builder operation $($r.operation)"}
    Reply
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    [Console]::Error.WriteLine($_.ScriptStackTrace)
    exit 1
} finally {
    if($serialCapture) {$serialCapture.Dispose()}
}
