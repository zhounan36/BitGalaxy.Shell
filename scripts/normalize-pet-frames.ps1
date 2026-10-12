<#
.SYNOPSIS
  桌宠帧归一化管线：测量 alpha 包围盒 → 按 manifest v4 规范缩放 → 锚点对齐 → 输出新画布。
  解决 AI 逐帧生成导致的"同状态角色忽大忽小 / 跨状态比例不一致"。

.DESCRIPTION
  - 规范来源：.trae/documents/pet-art-plan.md §3.2（每状态 norm.mode/target + anchor）。
  - 默认 ReportOnly（只出报告不改图）；-Apply 时输出到 -OutDir（默认 assets/pet-normalized/），
    确认后整目录替换 assets/pet/。raw/、refs/ 原样保留在源目录，不在本管线内。
  - @2x 与 1x 分别测量、按各自目标尺寸（target*2）归一，保证 retina 清晰度不损失。
  - -Apply 时同步产出 manifest v4（states.* 增注 anchor/norm，version 升 4）。

.EXAMPLE
  powershell -File scripts\normalize-pet-frames.ps1                    # 仅报告
  powershell -File scripts\normalize-pet-frames.ps1 -Apply             # 写入 assets/pet-normalized/
  powershell -File scripts\normalize-pet-frames.ps1 -States idle_breath,drag -Apply -OutDir "$env:TEMP\pet-norm-preview"
#>
param(
  [string]$AssetsDir = (Join-Path $PSScriptRoot '..\assets\pet'),
  [string]$OutDir = (Join-Path $PSScriptRoot '..\assets\pet-normalized'),
  [switch]$Apply,
  [string[]]$States,
  [string]$ReportPath = ''
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

# ── 归一化规范（pet-art-plan.md §3.2；mode: height|width|align，anchor: bottom|center）──
$Spec = @{
  idle_breath = [pscustomobject]@{ mode = 'height'; target = 474; anchor = 'bottom' }
  idle_blink  = [pscustomobject]@{ mode = 'height'; target = 474; anchor = 'bottom' }
  wave        = [pscustomobject]@{ mode = 'height'; target = 474; anchor = 'bottom' }
  ready       = [pscustomobject]@{ mode = 'height'; target = 474; anchor = 'bottom' }
  angry       = [pscustomobject]@{ mode = 'height'; target = 474; anchor = 'bottom' }
  need_input  = [pscustomobject]@{ mode = 'align';  target = 0;   anchor = 'bottom' } # 气泡出帧前仅对齐
  working     = [pscustomobject]@{ mode = 'height'; target = 400; anchor = 'bottom' } # 盘坐
  eating      = [pscustomobject]@{ mode = 'height'; target = 400; anchor = 'bottom' } # 坐姿
  sleep       = [pscustomobject]@{ mode = 'width';  target = 470; anchor = 'bottom' } # 横躺
  drag        = [pscustomobject]@{ mode = 'height'; target = 460; anchor = 'center' } # 悬空
}
$Canvas1x = 512
$GroundLineY = 492   # 站立地面线（1x）
$CenterY = 256
$AlphaMin = 16

function Get-AlphaBBox([System.Drawing.Bitmap]$bmp) {
  $stride = [Math]::Max(2, [int]($bmp.Width / 256))
  $minX = $bmp.Width; $minY = $bmp.Height; $maxX = -1; $maxY = -1
  for ($y = 0; $y -lt $bmp.Height; $y += $stride) {
    for ($x = 0; $x -lt $bmp.Width; $x += $stride) {
      if ($bmp.GetPixel($x, $y).A -gt $AlphaMin) {
        if ($x -lt $minX) { $minX = $x }
        if ($x -gt $maxX) { $maxX = $x }
        if ($y -lt $minY) { $minY = $y }
        if ($y -gt $maxY) { $maxY = $y }
      }
    }
  }
  if ($maxX -lt 0) { return $null }
  [pscustomobject]@{ X = $minX; Y = $minY; W = $maxX - $minX + $stride; H = $maxY - $minY + $stride }
}

function New-NormalizedFrame([string]$src, [string]$dst, [pscustomobject]$spec, [int]$canvas, [pscustomobject]$report) {
  $bmp = New-Object System.Drawing.Bitmap($src)
  try {
    $box = Get-AlphaBBox $bmp
    if ($null -eq $box) { $report.Flags = 'EMPTY'; return $false }

    $anchorX = $canvas / 2.0
    $anchorY = if ($spec.anchor -eq 'bottom') { $GroundLineY * ($canvas / $Canvas1x) } else { $CenterY * ($canvas / $Canvas1x) }
    $target = $spec.target * ($canvas / $Canvas1x)

    switch ($spec.mode) {
      'height' { $s = $target / $box.H }
      'width'  { $s = $target / $box.W }
      'align'  { $s = 1.0 }
    }
    if ($s -lt 0.4 -or $s -gt 2.5) { $report.Flags = "SCALE_CHECK(s=$([Math]::Round($s,3)))" }
    elseif ($report.Flags -eq '') { $report.Flags = 'ok' }

    # 整图缩放 s，偏移使包围盒按锚点落位
    $pasteX = $anchorX - ($box.X + $box.W / 2.0) * $s
    $pasteY = if ($spec.anchor -eq 'bottom') { $anchorY - ($box.Y + $box.H) * $s }
              else                           { $anchorY - ($box.Y + $box.H / 2.0) * $s }

    if (-not $Apply) { return $true }

    $out = New-Object System.Drawing.Bitmap($canvas, $canvas, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($out)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $dstRect = New-Object System.Drawing.RectangleF([float]$pasteX, [float]$pasteY, [float]($bmp.Width * $s), [float]($bmp.Height * $s))
    $g.DrawImage($bmp, $dstRect)
    $g.Dispose()
    $dir = Split-Path $dst -Parent
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
    $out.Save($dst, [System.Drawing.Imaging.ImageFormat]::Png)
    $out.Dispose()
    return $true
  } finally { $bmp.Dispose() }
}

# ── 主流程 ──
$rows = @()
$stateDirs = Get-ChildItem -Path $AssetsDir -Directory | Where-Object { $Spec.ContainsKey($_.Name) }
if ($States) { $stateDirs = $stateDirs | Where-Object { $States -contains $_.Name } }

foreach ($dir in $stateDirs) {
  $state = $dir.Name
  $stateSpec = $Spec[$state]
  foreach ($f in (Get-ChildItem -Path $dir.FullName -Filter '*.png' | Sort-Object Name)) {
    $is2x = $f.Name -match '@2x'
    $canvas = if ($is2x) { $Canvas1x * 2 } else { $Canvas1x }
    $rel = Join-Path $state $f.Name
    $dst = if ($Apply) { Join-Path $OutDir $rel } else { '' }
    $rep = [pscustomobject]@{ Flags = '' }
    $ok = New-NormalizedFrame $f.FullName $dst $stateSpec $canvas $rep
    $rows += [pscustomobject]@{
      State = $state; Frame = $f.Name; Mode = $stateSpec.mode; Canvas = $canvas
      Done = $ok; Flags = $rep.Flags
    }
  }
}

# 报告：同状态缩放系数极差（连贯性验收指标：极差越小越稳，目标 <2%）
$summary = $rows | Where-Object Done | Group-Object State | ForEach-Object {
  $scales = @()
  foreach ($r in $_.Group) { if ($r.Flags -match 's=([\d.]+)') { $scales += [double]$Matches[1] } }
  [pscustomobject]@{
    State = $_.Name; Frames = $_.Count; Flagged = @($_.Group | Where-Object { $_.Flags -ne 'ok' }).Count
    ScaleSpread = if ($scales.Count -ge 2) { [Math]::Round(($scales | Measure-Object -Maximum -Minimum).Maximum - ($scales | Measure-Object -Maximum -Minimum).Minimum, 4) } else { 0 }
  }
}

$mode = if ($Apply) { "APPLY → $OutDir" } else { 'REPORT ONLY（加 -Apply 写出）' }
Write-Host "=== 帧归一化 $mode ==="
$rows | Format-Table -AutoSize | Out-String -Width 160
Write-Host '=== 同状态缩放系数极差（<0.02 为达标）==='
$summary | Format-Table -AutoSize | Out-String -Width 120

if ($ReportPath) {
  $rows | Export-Csv -Path $ReportPath -NoTypeInformation -Encoding UTF8
  Write-Host "明细报告: $ReportPath"
}

if ($Apply) {
  # 产出 manifest v4：states.* 注入 anchor/norm
  $manifestPath = Join-Path $AssetsDir 'manifest.json'
  $m = Get-Content $manifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
  foreach ($p in ($m.states.PSObject.Properties)) {
    if ($Spec.ContainsKey($p.Name)) {
      $sp = $Spec[$p.Name]
      $ax = 0.5; $ay = if ($sp.anchor -eq 'bottom') { [Math]::Round($GroundLineY / $Canvas1x, 3) } else { 0.5 }
      $norm = [pscustomobject]@{ mode = $sp.mode; target = $sp.target; anchor = $sp.anchor }
      $p.Value | Add-Member -NotePropertyName anchor -NotePropertyValue ([pscustomobject]@{ x = $ax; y = $ay }) -Force
      $p.Value | Add-Member -NotePropertyName norm -NotePropertyValue $norm -Force
    }
  }
  $m.version = 4
  $outManifest = Join-Path $OutDir 'manifest.json'
  $outDirRoot = Split-Path $outManifest -Parent
  if (-not (Test-Path $outDirRoot)) { New-Item -ItemType Directory -Path $outDirRoot -Force | Out-Null }
  $m | ConvertTo-Json -Depth 10 | Set-Content $outManifest -Encoding UTF8
  Write-Host "manifest v4 已产出: $outManifest（含 anchor/norm 注入）"
  Write-Host '验收通过后：整目录替换 assets/pet/（保留 raw/ refs/ 与 manifest 中 raw 相关说明）'
}
