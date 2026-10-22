#requires -Version 7.0
param([Parameter(Mandatory=$true)][string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
$output = [IO.Path]::GetFullPath($OutputDirectory)
$active = Join-Path $output 'active.json'
if (Test-Path -LiteralPath $active) { throw 'Use a new isolated directory; existing configuration is preserved.' }
New-Item -ItemType Directory -Path $output -Force | Out-Null
function New-TextRule($id, $trigger, $text, $method = 'paste') {
    return [ordered]@{
        id=$id; enabled=$true; description="Synthetic input acceptance: $id"
        match=@{kind='literal';value=$trigger}; trigger_mode='immediate'
        action=@{type='text';method=$method;source=@{type='inline';value=$text}}
    }
}
$rules = @(
    (New-TextRule 'paste' 'qtzzx' '测试🙂 Paste OK'),
    (New-TextRule 'keys' 'qtkey' '逐字🙂 Keys OK' 'keys'),
    (New-TextRule 'digits' 'v5gcn' 'Digits OK'),
    (New-TextRule 'punctuation' ':qtp' 'Punctuation OK'),
    (New-TextRule 'space' 'qtspace' 'Space OK'),
    (New-TextRule 'space-kept' 'qtkeep' 'Keep OK'),
    (New-TextRule 'global' 'qtglobal' 'Global OK'),
    (New-TextRule 'disabled' 'qtdisabled' 'MUST NOT APPEAR'),
    (New-TextRule 'multiline' 'qtmulti' "First line`r`n第二行🙂" 'keys')
)
$rules[4].trigger_mode = 'terminator'
$rules[4].terminator = @{keys=@('space');consume=$true}
$rules[5].trigger_mode = 'terminator'
$rules[5].terminator = @{keys=@('space');consume=$false}
$rules[7].enabled = $false
$config = @{version=1;revision=1;enabled=$true;rules=$rules;globals=@{blacklist=@();ime_erasure='backspace'}}
$config | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $active -Encoding UTF8
Write-Output "Created $($rules.Count) synthetic rules: $active"
Write-Output 'This file is isolated test data, not the current user configuration.'
