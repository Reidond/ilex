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
if(Test-Path -LiteralPath $journal) {
    $j=Get-Content -LiteralPath $journal -Raw -Encoding UTF8 | ConvertFrom-Json
    if($j.schema -ne 1 -or $j.owner -ne $owner -or $j.name -ne $name -or $j.root -ne $root) {throw 'Builder journal identity/version mismatch; no mutation performed'}
}
if($r.operation -ne 'status' -and (Test-Path -LiteralPath "$journal.partial") -and $j) {
    # A flushed rename candidate can survive a host/process interruption. Recover
    # it only when its UUID/resource and the pre-existing creation intent agree.
    $candidate=Get-Content -LiteralPath "$journal.partial" -Raw -Encoding UTF8 | ConvertFrom-Json
    if($candidate.schema -ne 1 -or $candidate.owner -ne $owner -or $candidate.root -ne $root -or $candidate.name -ne $name -or $candidate.run -ne $j.run) {throw 'Interrupted journal candidate identity mismatch'}
    if($candidate.vm_id) {
        Import-Module Hyper-V
        $candidateVm=Get-VM -Id ([guid]$candidate.vm_id)
        if($candidate.resource.opaque_id -ne $candidate.vm_id -or $candidate.resource.owner -ne $owner -or $candidateVm.Notes -ne $candidate.notes) {throw 'Interrupted journal candidate VM ownership mismatch'}
    }
    [IlexAtomicFile]::Replace("$journal.partial",$journal)
    $j=$candidate
}
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
function Owned-VM {
    if(!$script:j -or !$script:j.vm_id) {return $null}
    if($j.resource.schema -ne 1 -or $j.resource.owner -ne $owner -or $j.resource.run -ne $j.run -or $j.resource.opaque_id -ne $j.vm_id) {throw 'OwnedResource journal identity mismatch'}
    $vm=Get-VM -Id ([guid]$script:j.vm_id) -ErrorAction SilentlyContinue
    if(!$vm) {throw "Owned VM UUID $($script:j.vm_id) is missing; refusing name-based adoption"}
    $notesPending=$j.phase -eq 'allocated' -and [string]::IsNullOrEmpty($vm.Notes)
    if((!$notesPending -and $vm.Notes -ne $script:j.notes) -or $vm.Name -ne $name) {throw 'VM ownership Notes/UUID/name mismatch; refusing mutation'}
    if([IO.Path]::GetFullPath($vm.ConfigurationLocation) -notlike "$root\config*") {throw 'Owned VM configuration path mismatch'}
    return $vm
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
function Preflight([long]$memory) {
    $available=[long](Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory*1KB
    while($available -lt ($memory+14GB)) {
        if([DateTime]::UtcNow -ge $memoryDeadline) {throw "Insufficient available memory: $([math]::Round($available/1GB,1)) GiB; builder needs $($memory/1GB) GiB plus 14 GiB host reserve. No VM was started."}
        Say "Waiting for host capacity: $([math]::Round($available/1GB,1)) GiB available, $(($memory+14GB)/1GB) GiB required"
        Start-Sleep -Seconds 120
        $available=[long](Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory*1KB
    }
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
    if($vm.State -eq 'Off') {Preflight ([long]$j.memory_bytes);Start-VM -VM $vm | Out-Null}
    elseif($vm.State -ne 'Running') {throw "Unsupported builder state: $($vm.State)"}
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
    if($vm) {$reply.state=[string]$vm.State;$reply.vm_id=[string]$vm.Id;$reply.addresses=@(Addresses $vm)}
    if($script:address) {$reply.endpoint=@{schema=1;vm_id=$j.vm_id;owner=$owner;address=$script:address;user='ilex';identity_file=(Join-Path $root 'identity');known_hosts_file=(Join-Path $root 'known_hosts');host_key_alias=$name;provisioning_revision=$j.revision}}
    return ($reply | ConvertTo-Json -Depth 18)
}
try {
    Import-Module Hyper-V -ErrorAction Stop
    if($r.operation -eq 'status') {Reply;exit 0}
    if($r.operation -eq 'up') {
        if($j -and $j.update_phase -eq 'updating') {throw 'Interrupted update requires builder rollback before up'}
        if(!$j) {
            if(Get-VM -Name $name -ErrorAction SilentlyContinue) {throw 'Builder name already exists without an ownership journal; refusing adoption'}
            $memory=16GB;if($r.options.memory_gib){$memory=[long]$r.options.memory_gib*1GB}
            $cpus=8;if($r.options.cpus){$cpus=[int]$r.options.cpus}
            if($memory -lt 4GB -or $memory -gt 32GB -or $cpus -lt 2 -or $cpus -gt [Environment]::ProcessorCount) {throw 'Builder resources require 4..32 GiB fixed memory and 2..host CPU count'}
            Preflight $memory
            $run=[guid]::NewGuid().ToString()
            $j=[pscustomobject]@{schema=1;owner=$owner;name=$name;root=$root;run=$run;vm_id=$null;notes="ilex-builder schema=1 owner=$owner run=$run";phase='preparing';revision=$r.revision;memory_bytes=$memory;cpus=$cpus;checkpoint_id=$null;update_phase=$null;serial_pipe="\\.\pipe\$name";resource=$null;data_uuid=$null;data_filesystem='xfs'}
            Write-Journal
            Seed
            $os=Join-Path $root 'os.vhdx';$data=Join-Path $root 'data.vhdx'
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
            $j.resource=@{schema=1;owner=$owner;run=$run;provider='hyperv-builder';opaque_id=$j.vm_id;created_unix_ms=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();paths=@($os,$data,(Join-Path $root 'config'))}
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
            if($requestedMemory -ne $j.memory_bytes -or $requestedCpus -ne $j.cpus) {
                if((Owned-VM).State -ne 'Off' -or $j.checkpoint_id) {throw 'Resource changes require builder down and no retained recovery checkpoint'}
                if($requestedMemory -lt 4GB -or $requestedMemory -gt 32GB -or $requestedCpus -lt 2 -or $requestedCpus -gt [Environment]::ProcessorCount) {throw 'Invalid builder resources'}
                Preflight $requestedMemory
                Set-VMMemory -VM (Owned-VM) -DynamicMemoryEnabled $false -StartupBytes $requestedMemory
                Set-VMProcessor -VM (Owned-VM) -Count $requestedCpus -ExposeVirtualizationExtensions $true
                $j.memory_bytes=$requestedMemory;$j.cpus=$requestedCpus;Write-Journal
            }
        }
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
        if($vm) {if($vm.State -ne 'Off'){Stop-VM -VM $vm -TurnOff -Confirm:$false};Remove-VM -VM $vm -Force}
        elseif(Get-VM -Name $name -ErrorAction SilentlyContinue) {throw 'Unjournaled VM exists; UUID authorization unavailable, refusing cleanup'}
        foreach($p in @('seed','seed-keys','seed.iso','identity','identity.pub','known_hosts','config','os.vhdx','data.vhdx')) {Remove-Owned (Join-Path $root $p)}
        Remove-Item -LiteralPath $journal;$j=$null
    } elseif($r.operation -eq 'accept-update') {
        if(!$j -or !$j.checkpoint_id -or $j.update_phase -notin @('verified','rolled-back')) {throw 'No verified/rolled-back recovery checkpoint to accept'}
        $snapshot=Get-VMSnapshot -VM (Owned-VM) | Where-Object {$_.Id -eq [guid]$j.checkpoint_id}
        if(!$snapshot) {throw 'Exact owned checkpoint missing'}
        Remove-VMSnapshot -VMSnapshot $snapshot -Confirm:$false
        $j.checkpoint_id=$null;$j.update_phase=$null;Write-Journal
    } elseif($r.operation -eq 'self-check') {
        $vm=Owned-VM
        if(!$vm -or $vm.State -ne 'Running') {throw 'Run builder up before the active self-check'}
        $memory=Get-VMMemory -VM $vm
        $processor=Get-VMProcessor -VM $vm
        if($memory.DynamicMemoryEnabled -or $memory.Startup -ne $j.memory_bytes -or !$processor.ExposeVirtualizationExtensions -or $vm.AutomaticCheckpointsEnabled) {throw 'Fixed-memory/nested/checkpoint configuration drift'}
        $probe=Guest 'smoke'
        if($probe.revision -ne $r.revision) {throw 'Provisioning revision differs; run builder update before self-check'}
        if(!$probe.nested_kvm_boot) {throw 'Nested KVM VM did not boot userspace'}
        if([long]$probe.guest_memory_bytes -lt ([long]$j.memory_bytes-512MB) -or $probe.guest_cpus -ne $j.cpus) {throw 'Live guest memory/CPU does not match the fixed resource profile'}
        Say "Nested KVM boot passed; UUID=$($vm.Id); revision=$($probe.revision)"
    } elseif($r.operation -in @('update','rollback')) {
        if(!$j -or $j.phase -ne 'ready') {throw 'Update/rollback requires a ready builder'}
        if($r.operation -eq 'rollback') {
            if(!$j.checkpoint_id){throw 'No owned recovery checkpoint'}
            if($j.update_phase -eq 'updating') {
                # Explicit rollback is recovery of this exact interrupted maintenance
                # transaction; its checkpoint was taken while all jobs were quiescent.
                $vm=Owned-VM
                if($vm.State -ne 'Off'){Stop-VM -VM $vm -TurnOff -Confirm:$false}
            } else {Stop-Owned}
            $snapshot=Get-VMSnapshot -VM (Owned-VM) | Where-Object {$_.Id -eq [guid]$j.checkpoint_id}
            if(!$snapshot){throw 'Exact recovery checkpoint missing'}
            Restore-VMSnapshot -VMSnapshot $snapshot -Confirm:$false
            Start-Owned;$probe=Guest 'smoke'
            if(!$probe.nested_kvm_boot -or $probe.revision -ne $j.revision) {throw 'Recovery checkpoint provisioning/KVM verification failed'}
            $j.update_phase='rolled-back';Write-Journal
        } else {
            if($j.update_phase -eq 'updating') {throw 'Interrupted update: run builder rollback before another update'}
            Start-Owned
            Wait-Idle
            Stop-Owned
            Preflight ([long]$j.memory_bytes)
            if($j.checkpoint_id) {throw 'Recovery checkpoint retained; accept it with builder accept-update before updating again'}
            $vm=Owned-VM
            Set-VM -VM $vm -CheckpointType Standard
            $snap=Checkpoint-VM -VM $vm -SnapshotName "ilex-builder-before-$([guid]::NewGuid())" -Passthru
            $j.checkpoint_id=[string]$snap.Id;$j.update_phase='updating';Write-Journal
            $previousRevision=$j.revision
            try {
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
                $vm=Owned-VM
                if($vm.State -ne 'Off'){Stop-VM -VM $vm -TurnOff -Confirm:$false}
                Restore-VMSnapshot -VMSnapshot (Get-VMSnapshot -VM (Owned-VM) | Where-Object {$_.Id -eq [guid]$j.checkpoint_id}) -Confirm:$false
                Start-Owned;$probe=Guest 'smoke'
                if(!$probe.nested_kvm_boot -or $probe.revision -ne $previousRevision) {throw 'Automatic rollback provisioning/KVM verification failed'}
                $j.revision=$probe.revision;$j.update_phase='rolled-back';Write-Journal
                throw "Update failed and exact owned checkpoint was restored and smoke-tested: $failure"
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
