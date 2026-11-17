param([Parameter(Mandatory=$true)][string]$Request)
$ErrorActionPreference='Stop'
$path=(Get-Content -LiteralPath $Request -Raw -Encoding UTF8 | ConvertFrom-Json).path
$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User
$acl=[IO.Directory]::GetAccessControl($path,[Security.AccessControl.AccessControlSections]::Access)
$acl.SetAccessRuleProtection($true,$false)
foreach($rule in @($acl.Access)) {$acl.RemoveAccessRuleSpecific($rule)}
foreach($identity in @($sid, [Security.Principal.SecurityIdentifier]'S-1-5-18')) {
    $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($identity,'FullControl','ContainerInherit,ObjectInherit','None','Allow')))
}
[IO.Directory]::SetAccessControl($path,$acl)
