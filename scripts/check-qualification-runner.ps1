# Syntax and embedded Win32 binding compilation only; no desktop actions.
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$path = Join-Path $PSScriptRoot 'qualify-video-lifecycle.ps1'
$ast = [System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) { throw ($errors | Out-String) }
$binding = $ast.Find({ param($node)
    $node -is [System.Management.Automation.Language.StringConstantExpressionAst] -and
    $node.Value.Contains('public class WallpaperWindows')
}, $true)
if (-not $binding) { throw 'Win32 window-observation binding is missing.' }
Add-Type -TypeDefinition $binding.Value
Write-Host 'Qualification runner parses; embedded Win32 binding compiles. Desktop execution NOT RUN.'
