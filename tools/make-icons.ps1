Add-Type -AssemblyName System.Drawing
$ErrorActionPreference = 'Stop'

# Draws the CURRENT Potflix mark from assets/icon.svg: rounded square #E5343F
# on transparent, white P bowl with the red play notch cut out of it. Geometry
# only (lines, arcs, triangles scaled from the 512 viewBox) -- no fonts, no
# dark background rect, so the output cannot drift with installed fonts.
# Play/pause/stop keep the same #E5343F tile with a white transport glyph.

function Add-RoundRect($gp, $x, $y, $w, $h, $r) {
  $d = $r * 2
  $gp.AddArc($x, $y, $d, $d, 180, 90)
  $gp.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
  $gp.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
  $gp.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
  $gp.CloseFigure()
}

function New-Icon($size, $path, $kind) {
  $k = $size / 512.0
  $bmp = New-Object System.Drawing.Bitmap($size, $size)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
  $g.Clear([System.Drawing.Color]::Transparent)
  $red = New-Object System.Drawing.SolidBrush([System.Drawing.ColorTranslator]::FromHtml('#E5343F'))
  $white = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::White)

  # Base tile: assets/icon.svg rect x=40 y=40 w=432 h=432 rx=96.
  $tile = New-Object System.Drawing.Drawing2D.GraphicsPath
  Add-RoundRect $tile (40*$k) (40*$k) (432*$k) (432*$k) (96*$k)
  $g.FillPath($red, $tile)
  $tile.Dispose()

  if ($kind -eq 'logo') {
    # P bowl: M159 386 V126 H288 A65 65 0 0 1 288 256 H223 V386 Z.
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $p.AddLine(159*$k, 386*$k, 159*$k, 126*$k)
    $p.AddLine(159*$k, 126*$k, 288*$k, 126*$k)
    $p.AddArc(288*$k, 126*$k, 130*$k, 130*$k, 270, 180)
    $p.AddLine(288*$k, 256*$k, 223*$k, 256*$k)
    $p.AddLine(223*$k, 256*$k, 223*$k, 386*$k)
    $p.CloseFigure()
    $g.FillPath($white, $p)
    $p.Dispose()
    # Play notch: M246.4 149.4 L246.4 232.6 L329.6 191 Z.
    $notch = [System.Drawing.PointF[]]@(
      [System.Drawing.PointF]::new([float](246.4*$k), [float](149.4*$k)),
      [System.Drawing.PointF]::new([float](246.4*$k), [float](232.6*$k)),
      [System.Drawing.PointF]::new([float](329.6*$k), [float](191*$k))
    )
    $g.FillPolygon($red, $notch)
  } elseif ($kind -eq 'play') {
    $tri = [System.Drawing.PointF[]]@(
      [System.Drawing.PointF]::new([float](216*$k), [float](176*$k)),
      [System.Drawing.PointF]::new([float](216*$k), [float](336*$k)),
      [System.Drawing.PointF]::new([float](336*$k), [float](256*$k))
    )
    $g.FillPolygon($white, $tri)
  } elseif ($kind -eq 'pause') {
    $g.FillRectangle($white, 196*$k, 176*$k, 56*$k, 160*$k)
    $g.FillRectangle($white, 260*$k, 176*$k, 56*$k, 160*$k)
  } else {
    $g.FillRectangle($white, 186*$k, 186*$k, 140*$k, 140*$k)
  }
  $g.Dispose(); $red.Dispose(); $white.Dispose()
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
  Write-Host "wrote $path"
}

$root = Split-Path $MyInvocation.MyCommand.Path -Parent | Split-Path -Parent
New-Icon 256 "$root\src\main\resources\icon.png" 'logo'
New-Icon 1024 "$root\assets\discord\logo.png" 'logo'
New-Icon 512 "$root\assets\discord\play.png" 'play'
New-Icon 512 "$root\assets\discord\pause.png" 'pause'
New-Icon 512 "$root\assets\discord\stop.png" 'stop'
