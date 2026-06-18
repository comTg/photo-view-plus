$dev  = "$env:LOCALAPPDATA\com.vetoer.photoviewplus.dev"
$prod = "$env:LOCALAPPDATA\com.vetoer.photoviewplus"
$bak  = "$prod.backup-$(Get-Date -Format yyyyMMdd-HHmmss)"

Stop-Process -Name "photo-view-plus" -ErrorAction SilentlyContinue

if (Test-Path $prod) {
  Copy-Item $prod $bak -Recurse -Force
}

New-Item -ItemType Directory -Force $prod | Out-Null

Copy-Item "$dev\db"      "$prod\db"      -Recurse -Force
Copy-Item "$dev\thumbs"  "$prod\thumbs"  -Recurse -Force
Copy-Item "$dev\vectors" "$prod\vectors" -Recurse -Force

if (Test-Path "$dev\config.json") {
  Copy-Item "$dev\config.json" "$prod\config.json" -Force
}