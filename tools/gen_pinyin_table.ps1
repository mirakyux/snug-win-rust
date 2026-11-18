# 生成 crates/snug-core/src/pinyin/gb2312.rs
#
# 对应 C# 类：SnugWin.Core.PhoneScreen.PinyinSearch
#
# C# 版运行时用 GB2312(CP936) 把字编码成两个字节、算出有符号码
# (b0*256+b1-65536)，再在 PinyinSearch.Gb2312Table 上做下界二分查拼音。
# Rust 版没有编码依赖，于是先用 tools/refdump（.NET 10 + 参考源码本身）导出
# "字符 -> 拼音"的真值（tools/pinyin_ref_dump.txt），再压缩成
# "拼音 -> 该读音下的所有字符" 的静态表。
#
# 用法（先用 tools/refdump 导出真值，再跑本脚本）：
#   tools/refdump/bin/Debug/net10.0/refdump.exe tools crates/snug-core/src/pinyin/corpus.tsv
#   powershell -ExecutionPolicy Bypass -File tools/gen_pinyin_table.ps1
#
# 生成的表以参考实现为准：修改请改脚本后重新生成，不要手改 gb2312.rs。
# 注：本脚本只需 Windows PowerShell（自带 CP936），不依赖 .NET SDK。
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$dumpPath = Join-Path $root 'tools/pinyin_ref_dump.txt'
$outPath = Join-Path $root 'crates/snug-core/src/pinyin/gb2312.rs'

if (-not (Test-Path -LiteralPath $dumpPath)) { throw "缺少 $dumpPath，先用 tools/refdump 导出" }

$pairs = [ordered]@{}
foreach ($line in Get-Content -LiteralPath $dumpPath -Encoding UTF8) {
    if (-not $line.StartsWith('TABLE ')) { continue }
    $parts = $line.Split(' ')
    $cp = [Convert]::ToInt32($parts[1], 16)
    $pinyin = $parts[2]
    $ch = [char]$cp
    # 同一字符不应出现两次；真值里 full 就是该字的读音
    $pairs[$ch] = $pinyin
}

if ($pairs.Count -lt 7000 -or $pairs.Count -gt 8000) { throw "真值条数异常：$($pairs.Count)" }

$groups = @{}
foreach ($kv in $pairs.GetEnumerator()) {
    if (-not $groups.ContainsKey($kv.Value)) { $groups[$kv.Value] = New-Object System.Collections.Generic.List[char] }
    $groups[$kv.Value].Add([char]$kv.Key)
}

function Escape([char]$c) {
    if ($c -eq '"') { return '\"' }
    if ($c -eq '\') { return '\\' }
    if ([int]$c -lt 0x20) { return ('\u{0:x4}' -f [int]$c) }
    return [string]$c
}

$sb = New-Object System.Text.StringBuilder
[void]$sb.AppendLine('//! GB2312(CP936) 汉字到拼音的映射表（含 best-fit 异体字，共 ' + $pairs.Count + ' 字）。')
[void]$sb.AppendLine('//!')
[void]$sb.AppendLine('//! 由 ``tools/gen_pinyin_table.ps1`` 生成，对应 C# 类 `SnugWin.Core.PhoneScreen.PinyinSearch`：')
[void]$sb.AppendLine('//! C# 版运行时把字编码成 GB2312 两个字节、算出 ``b0 * 256 + b1 - 65536``，')
[void]$sb.AppendLine('//! 再在 ``Gb2312Table`` 区间表上做下界二分查读音；这里把同一算法的结果离线展开，')
[void]$sb.AppendLine('//! 按"拼音 -> 字符组"压缩存放（表结构照搬 C# 的``读音对应一串字``，读起来一一对应）。')
[void]$sb.AppendLine('//! 修改请改脚本后重新生成，不要手改本文件。')
[void]$sb.AppendLine('')
[void]$sb.AppendLine('/// 拼音 -> 该读音下的所有汉字（组内按码点升序）。')
[void]$sb.AppendLine('pub(crate) static GB2312_GLYPHS: &[(&str, &str)] = &[')
foreach ($py in ($groups.Keys | Sort-Object)) {
    $chars = $groups[$py] | Sort-Object
    $text = -join ($chars | ForEach-Object { Escape $_ })
    [void]$sb.AppendLine("    (`"$py`", `"$text`"),")
}
[void]$sb.AppendLine('];')

New-Item -ItemType Directory -Force -Path (Split-Path -Parent $outPath) | Out-Null
[System.IO.File]::WriteAllText($outPath, $sb.ToString(), (New-Object System.Text.UTF8Encoding($false)))
Write-Host "写入 $outPath：$($pairs.Count) 字，$($groups.Count) 个读音"
