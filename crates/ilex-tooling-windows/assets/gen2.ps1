param()
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$inputPipe = [IO.StreamReader]::new([Console]::OpenStandardInput(), [Text.Encoding]::UTF8)
$r = $inputPipe.ReadToEnd() | ConvertFrom-Json
$inputPipe.Dispose()
if ($r.schema -ne 1) { throw 'Unsupported Gen2 request schema' }
$ns = 'root/virtualization/v2'

function Test-Allocation($vm) {
    $vm.Name -ceq $r.name -and $vm.Generation -eq 2 -and
        [IO.Path]::GetFullPath($vm.Path) -eq (Join-Path $r.config $r.name)
}

function Get-OwnedVM {
    [guid]$id = $r.resource.opaque_id
    if ($id -eq [guid]::Empty) { throw 'Gen2 allocation has no recorded VM UUID' }
    $vm = Get-VM -Id $id -ErrorAction Stop
    if (!(Test-Allocation $vm) -or $vm.Notes -cne $r.notes) {
        throw 'Gen2 VM receipt does not match the registered allocation'
    }
    $vm
}

function Assert-LocalGuardian {
    $store = 'Cert:\LocalMachine\Shielded VM Local Certificates'
    if (!(Test-Path -LiteralPath $store)) {
        throw 'Hyper-V local guardian certificate store is missing. Preparing a guardian changes host state and is outside this provider.'
    }
    $now = Get-Date
    $usable = @(Get-ChildItem -LiteralPath $store | Where-Object { $_.HasPrivateKey -and $_.NotBefore -le $now -and $_.NotAfter -gt $now })
    foreach ($kind in @('Signing', 'Encryption')) {
        if (!@($usable | Where-Object Subject -like "CN=Shielded VM $kind Certificate (UntrustedGuardian)*").Count) {
            throw "Hyper-V local guardian is missing a usable $kind certificate. This provider requires an already-prepared host."
        }
    }
}

function Device($system, [string]$class) {
    $devices = @(Get-CimAssociatedInstance -InputObject $system -ResultClassName $class -OperationTimeoutSec 10)
    if ($devices.Count -ne 1) { throw "Expected exactly one $class for this VM" }
    $devices[0]
}

function Invoke-Device($device, [string]$method, [hashtable]$arguments) {
    $reply = Invoke-CimMethod -InputObject $device -MethodName $method -Arguments $arguments -OperationTimeoutSec 10
    if ($reply.ReturnValue -ne 0) { throw "$($device.CimClass.CimClassName).$method failed ($($reply.ReturnValue))" }
    $reply
}

function Status($vm) {
    $fw = Get-VMFirmware -VM $vm
    $security = Get-VMSecurity -VM $vm
    $protector = $null
    if ($security.TpmEnabled) {
        $sha = [Security.Cryptography.SHA256]::Create()
        try { $protector = [BitConverter]::ToString($sha.ComputeHash([byte[]](Get-VMKeyProtector -VM $vm))).Replace('-', '').ToLowerInvariant() }
        finally { $sha.Dispose() }
    }
    @{
        id=[string]$vm.Id; state=[string]$vm.State; version=[string]$vm.Version
        secure_boot=([string]$fw.SecureBoot -eq 'On'); template=[string]$fw.SecureBootTemplate
        tpm=$security.TpmEnabled; bind_to_host_tpm=$security.BindToHostTpm; protector_sha256=$protector
        memory_bytes=$vm.MemoryStartup; dynamic_memory=$vm.DynamicMemoryEnabled
        vcpus=$vm.ProcessorCount; automatic_checkpoints=$vm.AutomaticCheckpointsEnabled
    }
}

function Write-Seed {
    # Same inbox IMAPI stream route as the managed builder. Joliet preserves the leaf names.
    Add-Type @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
public static class IlexGen2Iso {
 public static void Save(object source, string path) {
  var stream = (IStream)source;
  var bytes = new byte[65536];
  IntPtr count = Marshal.AllocHGlobal(4);
  try {
   using (var file = new FileStream(path, FileMode.CreateNew)) {
    while (true) {
     stream.Read(bytes, bytes.Length, count);
     int n = Marshal.ReadInt32(count);
     if (n == 0) break;
     file.Write(bytes, 0, n);
    }
    file.Flush(true);
   }
  } finally { Marshal.FreeHGlobal(count); }
 }
}
'@
    $iso = New-Object -ComObject IMAPI2FS.MsftFileSystemImage
    $iso.FileSystemsToCreate = 3
    $iso.VolumeName = if ($r.label) { $r.label } else { 'ILEX_GEN2' }
    $iso.Root.AddTree($r.seed_directory, $false)
    [IlexGen2Iso]::Save($iso.CreateResultImage().ImageStream, $r.seed)
}

switch ($r.operation) {
    'probe' {
        $hostInfo = Get-VMHost
        $os = Get-CimInstance Win32_OperatingSystem
        $cpu = Get-CimInstance Win32_Processor | ForEach-Object { "$($_.Manufacturer) $($_.Name) $($_.ProcessorId)" }
        $switch = Get-VMSwitch -Name 'Default Switch'
        $version = Get-VMHostSupportedVersion | Where-Object IsDefault | Select-Object -First 1 -ExpandProperty Version
        if (!$version -or $switch.SwitchType -ne 'Internal') { throw 'Hyper-V default configuration version or Default Switch unavailable' }
        @{
            machine=[string](Get-CimInstance Win32_ComputerSystemProduct).UUID
            windows="$($os.Version).$((Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').UBR)"
            boot=$os.LastBootUpTime.ToUniversalTime().ToString('o'); cpu=($cpu -join ';')
            configuration=[string]$version; hyperv=[string]$hostInfo.Name
        } | ConvertTo-Json -Compress
        break
    }
    'create' {
        $vm = $null
        try {
            if ($r.tpm) { Assert-LocalGuardian }
            if (@(Get-VM | Where-Object Name -eq $r.name).Count) { throw 'Gen2 name already registered' }
            if ($r.parent) { New-VHD -Path $r.disk -ParentPath $r.parent -Differencing | Out-Null }
            else { New-VHD -Path $r.disk -SizeBytes ([uint64]$r.blank_bytes) -Dynamic | Out-Null }
            if ($r.seed) { Write-Seed }
            $options = @{
                Name=$r.name; Generation=2; Path=$r.config
                MemoryStartupBytes=[uint64]$r.memory_bytes; VHDPath=$r.disk
            }
            if ($r.network) { $options.SwitchName = 'Default Switch' }
            $vm = New-VM @options
            $r.resource.opaque_id = [string]$vm.Id
            # Publish the UUID atomically before labeling/configuring the VM.
            $partial = $r.receipt + '.partial'
            $bytes = [Text.Encoding]::UTF8.GetBytes(($r.resource | ConvertTo-Json -Depth 4 -Compress))
            $file = [IO.File]::Open($partial, 'CreateNew', 'Write', 'None')
            try { $file.Write($bytes, 0, $bytes.Length); $file.Flush($true) }
            finally { $file.Dispose() }
            [IO.File]::Move($partial, $r.receipt)
            Set-VM -VM $vm -Notes $r.notes -AutomaticCheckpointsEnabled $false -CheckpointType Disabled -AutomaticStartAction Nothing
            Set-VMMemory -VM $vm -DynamicMemoryEnabled $false -StartupBytes ([uint64]$r.memory_bytes)
            Set-VMProcessor -VM $vm -Count ([int]$r.vcpus)
            if (!$r.network) { Get-VMNetworkAdapter -VM $vm | Remove-VMNetworkAdapter }
            $boot = Get-VMHardDiskDrive -VM $vm
            if ($r.iso) { $boot = Add-VMDvdDrive -VM $vm -Path $r.iso -Passthru }
            if ($r.seed) { Add-VMDvdDrive -VM $vm -Path $r.seed }
            if ($r.answers -like '*.iso') { Add-VMDvdDrive -VM $vm -Path $r.answers }
            if ($r.answers -like '*.vhdx') {
                $answerDisk = Join-Path ([IO.Path]::GetDirectoryName($r.disk)) 'answers.vhdx'
                $source = Get-VHD -Path $r.answers
                if ($source.VhdFormat -ne 'VHDX' -or $source.VhdType -notin @('Fixed','Dynamic') -or $source.ParentPath -or $source.Attached) {
                    throw 'Gen2 OEMDRV requires an offline standalone VHDX'
                }
                [IO.File]::Copy($r.answers, $answerDisk, $false)
                [IO.File]::SetAttributes($answerDisk, [IO.FileAttributes]::Normal)
                Add-VMHardDiskDrive -VM $vm -Path $answerDisk
            }
            $sb = if ($r.secure_boot) { 'On' } else { 'Off' }
            Set-VMFirmware -VM $vm -EnableSecureBoot $sb -SecureBootTemplate MicrosoftUEFICertificateAuthority -FirstBootDevice $boot
            if ($r.tpm) {
                try {
                    Set-VMKeyProtector -VM $vm -NewLocalKeyProtector
                    Enable-VMTPM -VM $vm
                } catch { throw "Guest vTPM requires Hyper-V permission to create a local VM key protector and enable TPM. $($_.Exception.Message)" }
            }
            Status (Get-OwnedVM) | ConvertTo-Json -Compress
        } catch {
            if ($vm) {
                $allocated = Get-VM -Id $vm.Id
                if ($allocated.Name -cne $r.name -or $allocated.Path -ne (Join-Path $r.config $r.name) -or $allocated.State -ne 'Off') {
                    throw 'Gen2 creation failed; retained allocation because its identity or state changed'
                }
                Remove-VM -VM $allocated -Force
            }
            throw
        }
        break
    }
    'seed' { Write-Seed; '{}' ; break }
    'installed-boot' {
        $vm = Get-OwnedVM
        if ($vm.State -ne 'Off') { throw 'Gen2 installation media may only change when powered off' }
        $disks = @(Get-VMHardDiskDrive -VM $vm)
        $target = @($disks | Where-Object Path -eq $r.disk)
        if ($target.Count -ne 1 -or @($disks | Where-Object { $_.Path -ne $r.disk -and $_.Path -ne $r.answer_disk }).Count) {
            throw 'Gen2 installed target or answer disk differs from the owned media'
        }
        $dvds = @(Get-VMDvdDrive -VM $vm)
        if (@($dvds | Where-Object { $_.Path -and $_.Path -notin @($r.iso, $r.answers, $r.original_seed) }).Count) {
            throw 'Gen2 installer has unexpected DVD media'
        }
        foreach ($dvd in $dvds) { if (!$r.seed -or $dvd.Path -ne $r.seed) { Remove-VMDvdDrive -VMDvdDrive $dvd } }
        foreach ($disk in $disks) { if ($disk.Path -eq $r.answer_disk) { Remove-VMHardDiskDrive -VMHardDiskDrive $disk } }
        Set-VMFirmware -VM $vm -BootOrder @($target[0])
        $order = @((Get-VMFirmware -VM $vm).BootOrder)
        if ($order.Count -ne 1 -or $order[0].Device.Path -ne $r.disk) { throw 'Gen2 installed disk boot order verification failed' }
        Status (Get-OwnedVM) | ConvertTo-Json -Compress
        break
    }
    'installer-evidence' {
        $vm = Get-OwnedVM
        if ($vm.State -ne 'Off') { throw 'Gen2 installer evidence requires a powered-off VM' }
        if (@(Get-VMHardDiskDrive -VM $vm | Where-Object Path -eq $r.answer_disk).Count) {
            throw 'Detach the Gen2 OEMDRV with the installed boot transition before collecting evidence'
        }
        $disk = Get-VHD -Path $r.answer_disk
        if ($disk.VhdFormat -ne 'VHDX' -or $disk.VhdType -notin @('Fixed','Dynamic') -or $disk.ParentPath -or $disk.Attached) {
            throw 'Gen2 installer evidence disk must be detached and standalone'
        }
        '{}'
        break
    }
    'reconcile' {
        $expected = Join-Path $r.config $r.name
        $matches = @(Get-VM | Where-Object {
            $_.Name -eq $r.name -or $_.Path -eq $expected -or
                ($r.resource.opaque_id -and [string]$_.Id -eq $r.resource.opaque_id)
        })
        if ($matches.Count -eq 0) { '{}'; break }
        if ($matches.Count -ne 1) { throw 'Ambiguous Gen2 creation intent' }
        $vm = $matches[0]
        if (!(Test-Allocation $vm) -or ($r.resource.opaque_id -and [string]$vm.Id -ne $r.resource.opaque_id)) {
            throw 'Gen2 creation intent conflicts with an existing VM'
        }
        if (!$r.resource.opaque_id -or $vm.Notes -cne $r.notes) {
            $disks = @(Get-VMHardDiskDrive -VM $vm)
            if (($vm.Notes -and $vm.Notes -cne $r.notes) -or $vm.State -ne 'Off' -or
                $disks.Count -ne 1 -or $disks[0].Path -ne $r.disk) {
                throw 'Incomplete Gen2 allocation cannot be attributed safely'
            }
        }
        if ($vm.State -ne 'Off') { Stop-VM -VM $vm -TurnOff -Confirm:$false }
        Remove-VM -VM $vm -Force
        if (@(Get-VM | Where-Object Id -eq $vm.Id).Count) { throw 'Interrupted Gen2 VM still registered' }
        '{}'
        break
    }
    'status' { Status (Get-OwnedVM) | ConvertTo-Json -Compress; break }
    'start' { Start-VM -VM (Get-OwnedVM); '{}' ; break }
    'stop' {
        $vm = Get-OwnedVM
        if ($vm.State -ne 'Off') { Stop-VM -VM $vm -TurnOff -Confirm:$false }
        if ((Get-OwnedVM).State -ne 'Off') { throw 'Gen2 guest did not stop' }
        '{}'
        break
    }
    'destroy' {
        if (!@(Get-VM | Where-Object Id -eq ([guid]$r.resource.opaque_id)).Count) { '{}'; break }
        $vm = Get-OwnedVM
        if ($vm.State -ne 'Off') { Stop-VM -VM $vm -TurnOff -Confirm:$false }
        Remove-VM -VM (Get-OwnedVM) -Force
        if (@(Get-VM | Where-Object Id -eq ([guid]$r.resource.opaque_id)).Count) { throw 'Gen2 VM still registered' }
        '{}'
        break
    }
    'address' {
        $nic = Get-VMNetworkAdapter -VM (Get-OwnedVM)
        if (@($nic).Count -ne 1 -or $nic.SwitchName -cne 'Default Switch') { throw 'Unexpected Gen2 NIC or switch' }
        $addresses = @($nic.IPAddresses)
        $adapter = Get-NetAdapter -Name 'vEthernet (Default Switch)'
        if ($nic.MacAddress) {
            $neighbors = Get-NetNeighbor -InterfaceIndex $adapter.ifIndex -AddressFamily IPv4
            $addresses += @($neighbors | Where-Object { ($_.LinkLayerAddress -replace '-', '') -eq $nic.MacAddress } | Select-Object -ExpandProperty IPAddress)
        }
        @{mac=$nic.MacAddress; addresses=@($addresses | Where-Object { $_ } | Select-Object -Unique)} | ConvertTo-Json -Compress
        break
    }
    default {
        $vm = Get-OwnedVM
        if ($vm.State -ne 'Running') { throw "Gen2 console unavailable in state $($vm.State)" }
        $system = Get-CimInstance -Namespace $ns -ClassName Msvm_ComputerSystem -Filter "Name='$($vm.Id)'" -OperationTimeoutSec 10
        if (!$system) { throw 'VM UUID has no CIM ComputerSystem' }
        switch ($r.operation) {
            'capture' {
                $video = Device $system Msvm_VideoHead
                [uint32]$w = $video.CurrentHorizontalResolution
                [uint32]$h = $video.CurrentVerticalResolution
                if (!$w -or !$h -or $w -gt 4096 -or $h -gt 2160) { throw "Unsupported Hyper-V display dimensions ${w}x${h}" }
                $service = Get-CimInstance -Namespace $ns -ClassName Msvm_VirtualSystemManagementService -OperationTimeoutSec 10
                $frame = Invoke-Device $service GetVirtualSystemThumbnailImage @{TargetSystem=$system; WidthPixels=$w; HeightPixels=$h}
                $expected = 4 + 2 * [long]$w * $h
                if ($frame.ImageData.Length -ne $expected) { throw "Unexpected thumbnail length for ${w}x${h}: $($frame.ImageData.Length), expected $expected" }
                @{width=$w; height=$h; captured_unix_ms=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds(); rgb565=[Convert]::ToBase64String([byte[]]$frame.ImageData)} | ConvertTo-Json -Compress
                break
            }
            'keys' {
                $kbd = Device $system Msvm_Keyboard
                if ($kbd.EnabledState -ne 2) { throw 'Gen2 keyboard device is disabled' }
                $held = New-Object 'Collections.Generic.List[uint32]'
                try {
                    foreach ($event in $r.events) {
                        $method = if ($event.down) { 'PressKey' } else { 'ReleaseKey' }
                        [uint32]$code = $event.code
                        if ($event.down) { $held.Add($code) }
                        Invoke-Device $kbd $method @{KeyCode=$code} | Out-Null
                        if (!$event.down) { $held.Remove($code) | Out-Null }
                        if ($event.delay) { Start-Sleep -Milliseconds $event.delay }
                    }
                    if ($r.leave_held) { $held.Clear() }
                } finally {
                    foreach ($code in $held) {
                        try { Invoke-Device $kbd ReleaseKey @{KeyCode=$code} | Out-Null }
                        catch { Write-Warning 'Gen2 could not release a pressed key' }
                    }
                }
                '{}'
                break
            }
            'pointer' {
                $mouse = Device $system Msvm_SyntheticMouse
                $video = Device $system Msvm_VideoHead
                if ($video.CurrentHorizontalResolution -ne $r.width -or $video.CurrentVerticalResolution -ne $r.height) { throw 'Gen2 framebuffer changed; capture again' }
                if ($mouse.EnabledState -ne 2) { throw 'Hyper-V synthetic mouse is disabled in this boot phase' }
                Invoke-Device $mouse SetAbsolutePosition @{HorizontalPosition=[uint32]$r.x; VerticalPosition=[uint32]$r.y} | Out-Null
                try { Invoke-Device $mouse SetButtonState @{ButtonIndex=[uint32]1; IsDown=$true} | Out-Null }
                finally { Invoke-Device $mouse SetButtonState @{ButtonIndex=[uint32]1; IsDown=$false} | Out-Null }
                '{}'
                break
            }
            default { throw 'Unsupported Gen2 operation' }
        }
    }
}
