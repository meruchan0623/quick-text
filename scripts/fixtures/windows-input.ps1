param(
    [string]$EvidencePath
)

# This is an input target only. All evidence comes from this form's controls.
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
[System.Windows.Forms.Application]::EnableVisualStyles()

if ($EvidencePath) {
    $EvidencePath = [System.IO.Path]::GetFullPath($EvidencePath)
    [System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($EvidencePath)) | Out-Null
}

$form = New-Object System.Windows.Forms.Form
$form.Text = 'Quicker Text Native Input Fixture'
$form.ClientSize = New-Object System.Drawing.Size(760, 640)
$form.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::FixedDialog
$form.MaximizeBox = $false
$form.StartPosition = [System.Windows.Forms.FormStartPosition]::CenterScreen
$form.AutoScaleMode = [System.Windows.Forms.AutoScaleMode]::None
$form.Font = New-Object System.Drawing.Font('Segoe UI', 10)

$specs = @(
    @{ Name = 'singleLine'; Caption = '1. Single-line TextBox'; Type = 'TextBox'; Y = 18; Height = 30 },
    @{ Name = 'multiLine'; Caption = '2. Multi-line TextBox'; Type = 'TextBox'; Y = 66; Height = 100 },
    @{ Name = 'richText'; Caption = '3. RichTextBox'; Type = 'RichTextBox'; Y = 184; Height = 100 },
    @{ Name = 'editableCombo'; Caption = '4. Editable ComboBox'; Type = 'ComboBox'; Y = 302; Height = 30 },
    @{ Name = 'password'; Caption = '5. Password TextBox'; Type = 'TextBox'; Y = 350; Height = 30 },
    @{ Name = 'readOnly'; Caption = '6. Read-only TextBox'; Type = 'TextBox'; Y = 398; Height = 30 },
    @{ Name = 'disabled'; Caption = '7. Disabled TextBox'; Type = 'TextBox'; Y = 446; Height = 30 }
)

$fields = [ordered]@{}
$index = 0
foreach ($spec in $specs) {
    $label = New-Object System.Windows.Forms.Label
    $label.Text = $spec.Caption
    $label.Location = New-Object System.Drawing.Point(18, ($spec.Y + 4))
    $label.Size = New-Object System.Drawing.Size(220, 26)
    $form.Controls.Add($label)

    $field = New-Object ("System.Windows.Forms." + $spec.Type)
    $field.Name = $spec.Name
    $field.AccessibleName = $spec.Caption
    $field.Location = New-Object System.Drawing.Point(244, $spec.Y)
    $field.Size = New-Object System.Drawing.Size(492, $spec.Height)
    $field.TabIndex = $index
    if ($spec.Name -eq 'multiLine') {
        $field.Multiline = $true
        $field.AcceptsReturn = $true
        $field.ScrollBars = [System.Windows.Forms.ScrollBars]::Vertical
    }
    if ($spec.Name -eq 'editableCombo') {
        $field.DropDownStyle = [System.Windows.Forms.ComboBoxStyle]::DropDown
    }
    if ($spec.Name -eq 'password') { $field.UseSystemPasswordChar = $true }
    if ($spec.Name -eq 'readOnly') {
        $field.ReadOnly = $true
        $field.Text = 'Synthetic read-only fixture text.'
    }
    if ($spec.Name -eq 'disabled') { $field.Enabled = $false }
    $fields[$spec.Name] = $field
    $form.Controls.Add($field)
    $index++
}

$summary = New-Object System.Windows.Forms.Label
$summary.Name = 'evidenceSummary'
$summary.Location = New-Object System.Drawing.Point(18, 502)
$summary.Size = New-Object System.Drawing.Size(718, 120)
$form.Controls.Add($summary)

$saveEvidence = {
    $values = [ordered]@{}
    $counts = @()
    foreach ($name in $fields.Keys) {
        $value = $fields[$name].Text
        if ($name -eq 'password') {
            $values[$name] = @{ length = $value.Length }
        } else {
            $values[$name] = $value
        }
        $counts += ('{0}: {1} chars' -f $name, $value.Length)
    }
    $summary.Text = ($counts -join ' | ')
    if ($EvidencePath) {
        try {
            $evidence = [ordered]@{
                fixture = 'Quicker Text Native Input Fixture'
                updatedAtUtc = [DateTime]::UtcNow.ToString('o')
                controls = $values
            }
            $json = ConvertTo-Json -InputObject $evidence -Depth 4
            [System.IO.File]::WriteAllText($EvidencePath, $json, (New-Object System.Text.UTF8Encoding($false)))
            $summary.Text += "`r`nEvidence: $EvidencePath"
        } catch {
            $summary.Text += "`r`nEvidence write failed: $($_.Exception.Message)"
        }
    }
}

foreach ($field in $fields.Values) { $field.add_TextChanged($saveEvidence) }
$form.add_Shown({ & $saveEvidence; $fields['singleLine'].Focus() | Out-Null })
try {
    [System.Windows.Forms.Application]::Run($form)
} finally {
    $form.Dispose()
}
