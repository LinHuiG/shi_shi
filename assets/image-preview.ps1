$ErrorActionPreference='Stop'
[Console]::InputEncoding=[Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)
try {
    $request=[Console]::In.ReadToEnd() | ConvertFrom-Json
    Add-Type -AssemblyName System.Drawing
    $file=[IO.FileInfo]::new($request.path)
    if(!$file.Exists -or $file.Length -gt 10485760){throw '图片不存在或超过10 MiB'}
    $stream=[IO.File]::OpenRead($file.FullName)
    try {
        $img=[Drawing.Image]::FromStream($stream,$true,$true)
        try {
            if([long]$img.Width*$img.Height -gt 40000000){throw '原始图片像素过大'}
            $scale=[Math]::Min(1.0,1280.0/[Math]::Max($img.Width,$img.Height))
            $width=[Math]::Max(1,[int]($img.Width*$scale));$height=[Math]::Max(1,[int]($img.Height*$scale))
            $bmp=[Drawing.Bitmap]::new($width,$height)
            try {
                $g=[Drawing.Graphics]::FromImage($bmp)
                try {$g.Clear([Drawing.Color]::White);$g.InterpolationMode=[Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic;$g.DrawImage($img,0,0,$width,$height)}finally{$g.Dispose()}
                $codec=[Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() | Where-Object MimeType -eq 'image/jpeg'
                $parameters=[Drawing.Imaging.EncoderParameters]::new(1)
                try {
                    $parameters.Param[0]=[Drawing.Imaging.EncoderParameter]::new([Drawing.Imaging.Encoder]::Quality,[long]78)
                    $output=[IO.MemoryStream]::new()
                    try {
                        $bmp.Save($output,$codec,$parameters)
                        if($output.Length -gt 750000){throw '压缩后图片仍超过750 KB'}
                        [Console]::Out.Write([Convert]::ToBase64String($output.ToArray()))
                    }finally{$output.Dispose()}
                }finally{$parameters.Dispose()}
            }finally{$bmp.Dispose()}
        }finally{$img.Dispose()}
    }finally{$stream.Dispose()}
} catch {[Console]::Error.Write($_.Exception.Message);exit 1}
