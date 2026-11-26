function Get-RustTokens([string]$Text) {
    $tokens = [Collections.Generic.List[object]]::new()
    $index = 0
    while ($index -lt $Text.Length) {
        $character = $Text[$index]
        if ([char]::IsWhiteSpace($character)) { $index++; continue }
        if ($index + 1 -lt $Text.Length -and $Text.Substring($index, 2) -eq '//') {
            $end = $Text.IndexOf("`n", $index + 2)
            if ($end -lt 0) { break }
            $index = $end + 1
            continue
        }
        if ($index + 1 -lt $Text.Length -and $Text.Substring($index, 2) -eq '/*') {
            $depth = 1
            $index += 2
            while ($depth -gt 0 -and $index + 1 -lt $Text.Length) {
                $pair = $Text.Substring($index, 2)
                if ($pair -eq '/*') { $depth++; $index += 2 }
                elseif ($pair -eq '*/') { $depth--; $index += 2 }
                else { $index++ }
            }
            if ($depth -ne 0) { throw 'Unterminated Rust block comment' }
            continue
        }
        if ($character -in @('r', 'b', 'c')) {
            $raw = [regex]::Match($Text.Substring($index), '^(?:br|cr|r)(#*)"')
            if ($raw.Success) {
                $terminator = '"' + $raw.Groups[1].Value
                $end = $Text.IndexOf($terminator, $index + $raw.Length, [StringComparison]::Ordinal)
                if ($end -lt 0) { throw 'Unterminated Rust raw string' }
                $index = $end + $terminator.Length
                continue
            }
        }
        if ($character -eq '"') {
            $index++
            $closed = $false
            while ($index -lt $Text.Length) {
                if ($Text[$index] -eq '\') { $index += 2 }
                elseif ($Text[$index] -eq '"') { $index++; $closed = $true; break }
                else { $index++ }
            }
            if (-not $closed) { throw 'Unterminated Rust string' }
            continue
        }
        if ($character -eq "'") {
            $literal = [regex]::Match($Text.Substring($index), "^'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\r\n])'")
            if ($literal.Success) { $index += $literal.Length; continue }
        }
        $start = $index
        if ([char]::IsLetterOrDigit($character) -or $character -eq '_') {
            do { $index++ } while ($index -lt $Text.Length -and ([char]::IsLetterOrDigit($Text[$index]) -or $Text[$index] -eq '_'))
        } else { $index++ }
        $tokens.Add([pscustomobject]@{ Text = $Text.Substring($start, $index - $start); Start = $start; End = $index })
    }
    return ,$tokens.ToArray()
}

function Get-RustItem([string]$Text, [string]$Head) {
    $tokens = Get-RustTokens $Text
    $headTokens = Get-RustTokens $Head
    $found = -1
    for ($candidate = 0; $candidate -le $tokens.Count - $headTokens.Count; $candidate++) {
        $matchesHead = $true
        for ($offset = 0; $offset -lt $headTokens.Count; $offset++) {
            if ($tokens[$candidate + $offset].Text -cne $headTokens[$offset].Text) { $matchesHead = $false; break }
        }
        if ($matchesHead) { $found = $candidate; break }
    }
    if ($found -lt 0) { throw "Missing Rust item: $Head" }
    $braces = 0
    $parentheses = 0
    $brackets = 0
    $hasBody = $false
    for ($cursor = $found + $headTokens.Count; $cursor -lt $tokens.Count; $cursor++) {
        switch ($tokens[$cursor].Text) {
            '{' { $braces++; $hasBody = $true }
            '}' { $braces-- }
            '(' { $parentheses++ }
            ')' { $parentheses-- }
            '[' { $brackets++ }
            ']' { $brackets-- }
        }
        if ($braces -eq 0 -and $parentheses -eq 0 -and $brackets -eq 0 -and
            (($hasBody -and $tokens[$cursor].Text -eq '}') -or $tokens[$cursor].Text -eq ';')) {
            return $Text.Substring($tokens[$found].Start, $tokens[$cursor].End - $tokens[$found].Start)
        }
    }
    throw "Unterminated Rust item: $Head"
}
