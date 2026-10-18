# The archive pick, run each morning by hand (archive/daily.py): the repo's .env gives
# the database (RAILWAY_DATABASE_URL) and the S3 credentials (AWS_ACCESS_KEY_ID,
# AWS_SECRET_ACCESS_KEY); the output goes to D:\hl-daily\daily.log.
$repo = Split-Path -Parent $PSScriptRoot
foreach ($line in Get-Content (Join-Path $repo '.env')) {
    if ($line -match '^\s*([A-Z_]+)\s*=\s*(.*)\s*$') { Set-Item -Path "Env:$($Matches[1])" -Value $Matches[2] }
}
$env:DATABASE_URL = $env:RAILWAY_DATABASE_URL
$env:PYTHONIOENCODING = 'utf-8'
New-Item -ItemType Directory -Force 'D:\hl-daily' | Out-Null
$py = 'C:\Users\lugov\AppData\Local\Programs\Python\Python312\python.exe'
& $py -u (Join-Path $PSScriptRoot 'daily.py') *>> 'D:\hl-daily\daily.log'
