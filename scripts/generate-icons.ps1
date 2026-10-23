#Requires -Version 7.0
<#
.SYNOPSIS
Regenerates Quicktext Windows icons from the editable SVG sources, without dependencies.
.EXAMPLE
pwsh -File scripts/generate-icons.ps1
.EXAMPLE
pwsh -File scripts/generate-icons.ps1 -PreviewPath work/icon-refresh/icon-preview.png
#>
[CmdletBinding()]
param([string]$PreviewPath = 'work/icon-refresh/icon-preview.png')
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This generator requires Windows System.Drawing.' }
$projectRoot = Split-Path $PSScriptRoot -Parent
$iconDirectory = Join-Path $projectRoot 'src-tauri/icons'
Add-Type -AssemblyName System.Drawing.Common
if (-not ('QuicktextIconRenderer' -as [type])) {
    $references = @('System.Drawing.Common', 'System.Drawing.Primitives', 'System.Xml.ReaderWriter', 'System.Xml.XDocument', 'System.Linq', 'System.Collections')
    # Newer bundled System.Drawing versions expose IImage through this assembly.
    $windowsCore = Join-Path $PSHOME 'System.Private.Windows.Core.dll'
    if (Test-Path -LiteralPath $windowsCore) { $references += $windowsCore }
    Add-Type -ReferencedAssemblies $references -TypeDefinition @'
using System;
using System.IO;
using System.Globalization;
using System.Linq;
using System.Xml.Linq;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;

public static class QuicktextIconRenderer {
    static float N(XElement e, string key) => float.Parse(e.Attribute(key).Value, CultureInfo.InvariantCulture);
    static Color C(string value) => ColorTranslator.FromHtml(value);
    // Deliberately supports only the SVG primitives used by our editable sources.
    // Reject unsupported elements rather than silently produce incomplete icons.
    public static Bitmap Render(string svgPath, int size) {
        var root = XDocument.Load(svgPath).Root;
        float extent = float.Parse(root.Attribute("viewBox").Value.Split(' ')[2], CultureInfo.InvariantCulture);
        const int samples = 8;
        using (var high = new Bitmap(size * samples, size * samples, PixelFormat.Format32bppArgb)) {
            using (var g = Graphics.FromImage(high)) {
                g.Clear(Color.Transparent);
                g.SmoothingMode = SmoothingMode.AntiAlias;
                g.ScaleTransform(size * samples / extent, size * samples / extent);
                foreach (var e in root.Elements()) {
                    if (e.Name.LocalName == "rect") {
                        float x=N(e,"x"), y=N(e,"y"), w=N(e,"width"), h=N(e,"height"), d=N(e,"rx")*2;
                        using (var path = new GraphicsPath()) using (var brush = new SolidBrush(C(e.Attribute("fill").Value))) {
                            path.AddArc(x,y,d,d,180,90); path.AddArc(x+w-d,y,d,d,270,90);
                            path.AddArc(x+w-d,y+h-d,d,d,0,90); path.AddArc(x,y+h-d,d,d,90,90);
                            path.CloseFigure(); g.FillPath(brush,path);
                        }
                    } else if (e.Name.LocalName == "g") {
                        using (var pen = new Pen(C(e.Attribute("stroke").Value), N(e,"stroke-width"))) {
                            pen.StartCap=LineCap.Round; pen.EndCap=LineCap.Round; pen.LineJoin=LineJoin.Round;
                            foreach (var child in e.Elements()) {
                                if (child.Name.LocalName == "line") g.DrawLine(pen,N(child,"x1"),N(child,"y1"),N(child,"x2"),N(child,"y2"));
                                else if (child.Name.LocalName == "polyline") {
                                    var points = child.Attribute("points").Value.Split(' ').Select(p => p.Split(',')).Select(p => new PointF(float.Parse(p[0],CultureInfo.InvariantCulture),float.Parse(p[1],CultureInfo.InvariantCulture))).ToArray();
                                    g.DrawLines(pen,points);
                                } else throw new InvalidDataException("Unsupported SVG primitive: " + child.Name);
                            }
                        }
                    } else throw new InvalidDataException("Unsupported SVG primitive: " + e.Name);
                }
            }
            var result = new Bitmap(size,size,PixelFormat.Format32bppArgb);
            using (var g = Graphics.FromImage(result)) {
                g.CompositingMode=CompositingMode.SourceCopy;
                g.InterpolationMode=InterpolationMode.HighQualityBicubic;
                g.PixelOffsetMode=PixelOffsetMode.HighQuality;
                g.DrawImage(high,new Rectangle(0,0,size,size),0,0,high.Width,high.Height,GraphicsUnit.Pixel);
            }
            return result;
        }
    }
    public static byte[] Png(Bitmap bitmap) {
        using(var stream = new MemoryStream()) { bitmap.Save(stream,ImageFormat.Png); return stream.ToArray(); }
    }
    public static void Preview(string path, string normalSvg, string smallSvg) {
        int[] sizes = {16,20,24,32,48,64,256};
        using(var sheet = new Bitmap(920,680,PixelFormat.Format32bppArgb))
        using(var g = Graphics.FromImage(sheet))
        using(var labelFont = new Font("Segoe UI",11))
        using(var titleFont = new Font("Segoe UI",17,FontStyle.Bold)) {
            for(int row=0;row<2;row++) {
                int top=row*340;
                Color bg=row==0?Color.FromArgb(247,248,250):Color.FromArgb(17,21,29);
                using(var brush=new SolidBrush(bg)) g.FillRectangle(brush,0,top,920,340);
                using(var ink=new SolidBrush(row==0?Color.FromArgb(26,36,49):Color.FromArgb(238,242,247))) {
                    g.DrawString("Quicktext / native-size icon checks",titleFont,ink,24,top+18);
                    int x=24;
                    foreach(int size in sizes) {
                        int cell=size==256?280:90;
                        using(var icon=Render(size<=24?smallSvg:normalSvg,size)) g.DrawImageUnscaled(icon,x+(cell-size)/2,top+64+(256-size)/2);
                        g.DrawString(size+" px",labelFont,ink,x+(cell-45)/2,top+311);
                        x+=cell;
                    }
                }
            }
            sheet.Save(path,ImageFormat.Png);
        }
    }
}
'@
}
$normalSvg = Join-Path $iconDirectory 'icon.svg'
$smallSvg = Join-Path $iconDirectory 'icon-small.svg'
$sizes = @(16,20,24,32,40,48,64,128,256)
$pngs = @{}
foreach ($size in ($sizes + 512)) {
    $source = if ($size -le 24) { $smallSvg } else { $normalSvg }
    $bitmap = [QuicktextIconRenderer]::Render($source, $size)
    try { $pngs[$size] = [QuicktextIconRenderer]::Png($bitmap) } finally { $bitmap.Dispose() }
}
foreach ($entry in @(@('icon.png',512),@('32x32.png',32),@('128x128.png',128),@('128x128@2x.png',256))) {
    [IO.File]::WriteAllBytes((Join-Path $iconDirectory $entry[0]), $pngs[$entry[1]])
    Write-Output "Generated $($entry[0]) ($($entry[1]) x $($entry[1]))"
}
# Each ICO entry contains its own custom rendered PNG; 0 encodes 256 pixels.
$icoPath = Join-Path $iconDirectory 'icon.ico'
$stream = [IO.File]::Create($icoPath)
$writer = [IO.BinaryWriter]::new($stream)
try {
    $writer.Write([uint16]0); $writer.Write([uint16]1); $writer.Write([uint16]$sizes.Count)
    $offset = 6 + 16 * $sizes.Count
    foreach ($size in $sizes) {
        $dimension = if ($size -eq 256) { 0 } else { $size }
        $writer.Write([byte]$dimension); $writer.Write([byte]$dimension)
        $writer.Write([byte]0); $writer.Write([byte]0)
        $writer.Write([uint16]1); $writer.Write([uint16]32)
        $writer.Write([uint32]$pngs[$size].Length); $writer.Write([uint32]$offset)
        $offset += $pngs[$size].Length
    }
    foreach ($size in $sizes) { $writer.Write([byte[]]$pngs[$size]) }
} finally { $writer.Dispose(); $stream.Dispose() }
# Read back directory entries and embedded PNG dimensions, rather than trusting output.
$reader = [IO.BinaryReader]::new([IO.File]::OpenRead($icoPath))
try {
    if ($reader.ReadUInt16() -ne 0 -or $reader.ReadUInt16() -ne 1 -or $reader.ReadUInt16() -ne $sizes.Count) { throw 'Invalid ICO header.' }
    foreach ($size in $sizes) {
        $width = [int]$reader.ReadByte(); if ($width -eq 0) { $width = 256 }
        $height = [int]$reader.ReadByte(); if ($height -eq 0) { $height = 256 }
        $null = $reader.ReadBytes(6)
        $length = $reader.ReadUInt32(); $imageOffset = $reader.ReadUInt32()
        $resume = $reader.BaseStream.Position
        $reader.BaseStream.Position = $imageOffset
        $bytes = $reader.ReadBytes($length)
        $pngStream = [IO.MemoryStream]::new($bytes, $false)
        $image = [Drawing.Image]::FromStream($pngStream)
        try {
            if ($width -ne $size -or $height -ne $size -or $image.Width -ne $size -or $image.Height -ne $size) { throw "ICO size mismatch: $size" }
        } finally { $image.Dispose(); $pngStream.Dispose() }
        $reader.BaseStream.Position = $resume
    }
} finally { $reader.Dispose() }
if (-not [IO.Path]::IsPathRooted($PreviewPath)) { $PreviewPath = Join-Path $projectRoot $PreviewPath }
$null = [IO.Directory]::CreateDirectory((Split-Path $PreviewPath -Parent))
[QuicktextIconRenderer]::Preview($PreviewPath,$normalSvg,$smallSvg)
Write-Output "Validated icon.ico sizes: $($sizes -join ', ')"
Write-Output "Preview: $PreviewPath"


