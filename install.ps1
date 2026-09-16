# Узел ferry на Windows: OpenSSH Server, ключ хаба, демон рабочего стола.
#
# Запускать из PowerShell от администратора, войдя под тем пользователем, на чьём рабочем
# столе будут выполняться команды:
#   powershell -ExecutionPolicy Bypass -File install.ps1 -From http://<хаб>:8000
# -From — каталог или http-адрес, где лежат ferry.exe и ferry.pub. Повторный запуск
# безопасен: обновляет ferry.exe и перезапускает демон.
#
# Файл в UTF-8 с BOM: без него Windows PowerShell 5.1 читает кириллицу как ANSI.
param([Parameter(Mandatory = $true)][string]$From)

$ErrorActionPreference = 'Stop'

function Step($text) { Write-Host "==> $text" -ForegroundColor Cyan }

function Fetch($name, $dest) {
    if ($From -match '^https?://') {
        Invoke-WebRequest "$From/$name" -OutFile $dest -UseBasicParsing
    } else {
        Copy-Item (Join-Path $From $name) $dest -Force
    }
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$admin = (New-Object Security.Principal.WindowsPrincipal $identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) { throw 'Нужен PowerShell от администратора' }
$user = $identity.Name

Step 'Git Bash'
$git = Get-Command git.exe -ErrorAction SilentlyContinue
if (-not $git) { throw 'Нужен Git for Windows: https://git-scm.com/download/win' }
# git.exe лежит в <Git>\cmd, bash.exe — в <Git>\bin.
$bash = Join-Path (Split-Path (Split-Path $git.Path)) 'bin\bash.exe'
if (-not (Test-Path $bash)) { throw "Не нашёл bash.exe рядом с $($git.Path)" }

Step 'OpenSSH Server'
if (-not (Get-Service sshd -ErrorAction SilentlyContinue)) {
    try {
        Add-WindowsCapability -Online -Name 'OpenSSH.Server~~~~0.0.1.0' | Out-Null
    } catch {
        throw "Не удалось поставить OpenSSH Server ($_). Поставь MSI с https://github.com/PowerShell/Win32-OpenSSH/releases и запусти скрипт снова"
    }
}
Set-Service sshd -StartupType Automatic
# Первый старт создаёт ключи хоста и %ProgramData%\ssh\sshd_config.
Start-Service sshd

# Git Bash вместо cmd.exe: команды хаба одинаковы для macOS и Windows.
$reg = 'HKLM:\SOFTWARE\OpenSSH'
if (-not (Test-Path $reg)) { New-Item $reg | Out-Null }
New-ItemProperty $reg -Name DefaultShell -Value $bash -PropertyType String -Force | Out-Null
New-ItemProperty $reg -Name DefaultShellCommandOption -Value '-c' -PropertyType String -Force | Out-Null

Step 'Брандмауэр: SSH только из локальной сети'
$rule = 'OpenSSH-Server-In-TCP'
if (-not (Get-NetFirewallRule -Name $rule -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -Name $rule -DisplayName 'OpenSSH Server (sshd)' -Direction Inbound `
        -Protocol TCP -LocalPort 22 -Action Allow | Out-Null
}
Set-NetFirewallRule -Name $rule -Enabled True -Profile Any -RemoteAddress LocalSubnet

Step 'Ключ хаба'
$pub = Join-Path $env:TEMP 'ferry.pub'
Fetch 'ferry.pub' $pub
$key = (Get-Content $pub -Raw).Trim()
# Для администраторов sshd читает только общий файл (Match Group administrators в sshd_config).
$auth = Join-Path $env:ProgramData 'ssh\administrators_authorized_keys'
$keys = @()
if (Test-Path $auth) { $keys = @(Get-Content $auth) }
if ($keys -notcontains $key) { Set-Content $auth ($keys + $key) -Encoding Ascii }
# С лишними правами на файл sshd молча его игнорирует. Группы — по SID: имена локализованы.
icacls $auth /inheritance:r /grant '*S-1-5-32-544:F' /grant '*S-1-5-18:F' | Out-Null
if ($LASTEXITCODE) { throw "icacls не выставил права на $auth" }
Restart-Service sshd

Step 'ferry.exe'
$bin = Join-Path $HOME '.ferry\bin'
$exe = Join-Path $bin 'ferry.exe'
New-Item -ItemType Directory $bin -Force | Out-Null
# Запущенный exe не перезаписать: гасим демон и клиентов.
if (Get-ScheduledTask -TaskName ferry -ErrorAction SilentlyContinue) { Stop-ScheduledTask -TaskName ferry }
Get-Process ferry -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force
Start-Sleep -Milliseconds 500
Fetch 'ferry.exe' $exe
$path = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($path -split ';') -notcontains $bin) {
    $path = if ($path) { "$path;$bin" } else { $bin }
    [Environment]::SetEnvironmentVariable('Path', $path, 'User')
}
[Environment]::SetEnvironmentVariable('FERRY_SHELL', $bash, 'User')

Step 'Демон рабочего стола: планировщик, при входе пользователя'
$action = New-ScheduledTaskAction -Execute $exe -Argument 'daemon'
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
$runAs = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName ferry -Action $action -Trigger $trigger -Principal $runAs `
    -Settings $settings -Force | Out-Null
$state = Join-Path $HOME '.ferry\desk'
Remove-Item $state -ErrorAction SilentlyContinue
Start-ScheduledTask -TaskName ferry
for ($i = 0; -not (Test-Path $state); $i++) {
    if ($i -ge 50) { throw 'Демон не запустился: Get-ScheduledTaskInfo -TaskName ferry' }
    Start-Sleep -Milliseconds 200
}
$reply = & $exe desk 'echo ok'
if ($reply -ne 'ok') { throw "Демон ответил не так: $reply" }

Step 'Питание от сети: без сна и без гашения экрана'
powercfg /change standby-timeout-ac 0
powercfg /change monitor-timeout-ac 0

$ips = (Get-NetIPAddress -AddressFamily IPv4 |
    Where-Object { $_.IPAddress -notmatch '^(127|169\.254)\.' }).IPAddress -join ', '
Write-Host ''
Write-Host "Готово. Пользователь $env:USERNAME, адреса: $ips" -ForegroundColor Green
Write-Host "С хаба: ssh -i ~/.ssh/ferry $env:USERNAME@<адрес> ferry desk notepad"
