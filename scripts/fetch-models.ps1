# waifu2x cunet(art) ONNX 모델 다운로드
# 출처: https://huggingface.co/deepghs/waifu2x_onnx (nunif 기반)
# 실행: pwsh scripts/fetch-models.ps1

$ErrorActionPreference = "Stop"
$base = "https://huggingface.co/deepghs/waifu2x_onnx/resolve/main/20230504/onnx_models/cunet/art"
$dir = Join-Path $PSScriptRoot "..\src-tauri\resources\models\cunet"
New-Item -ItemType Directory -Force $dir | Out-Null

$files = @(
  "noise0.onnx", "noise1.onnx", "noise2.onnx", "noise3.onnx",
  "scale2x.onnx",
  "noise0_scale2x.onnx", "noise1_scale2x.onnx", "noise2_scale2x.onnx", "noise3_scale2x.onnx"
)

foreach ($f in $files) {
  $out = Join-Path $dir $f
  if (Test-Path $out) { Write-Host "skip  $f"; continue }
  Write-Host "fetch $f"
  Invoke-WebRequest -Uri "$base/$f" -OutFile $out
}
Write-Host "done → $dir"
