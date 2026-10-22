//! Свойства узла: система, железо, питание, экран, ferry. Узел отдаёт строки `ключ=значение`
//! (`ferry info`), хаб превращает их в подписи и предупреждения — одни и те же для CLI и
//! приложения хаба.

use std::io;

/// Свойства этой машины — для `ferry info` на узле.
pub fn collect() -> io::Result<Vec<(String, String)>> {
    let mut pairs = platform()?;
    pairs.push(("ferry".into(), crate::VERSION.into()));
    pairs.push(("mode".into(), crate::mode::current().name().into()));
    let human = crate::human::presence::read(&crate::ferry_home()?);
    pairs.push((
        "human_idle_s".into(),
        human["idle_seconds"]
            .as_u64()
            .map_or_else(|| "unknown".into(), |s| s.to_string()),
    ));
    pairs.push((
        "human_input_status".into(),
        human["note"].as_str().unwrap_or("unknown").into(),
    ));
    pairs.push((
        "human_observed_since".into(),
        human["observed_since"].to_string(),
    ));
    Ok(pairs)
}

pub fn command() -> io::Result<i32> {
    for (key, value) in collect()? {
        println!("{key}={value}");
    }
    Ok(0)
}

/// Строки `ключ=значение` → пары; пустые и без `=` пропускаются.
pub fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

fn get<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn number(pairs: &[(String, String)], key: &str) -> Option<f64> {
    get(pairs, key)?.replace(',', ".").parse().ok()
}

/// Свойства для людей: подпись и значение, в порядке показа.
pub fn describe(pairs: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut push = |label: &str, value: Option<String>| {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            out.push((label.to_string(), value));
        }
    };
    let text = |key: &str| get(pairs, key).map(String::from);
    let gb = |v: f64| format!("{v:.1}").replace('.', ",");

    push("система", text("os"));
    push("имя ПК", text("computer"));
    push("пользователь", text("user"));
    push(
        "последний ввод человека",
        get(pairs, "human_idle_s").map(|s| match s.parse::<u64>() {
            Ok(seconds) => format!("{} назад", duration(seconds)),
            Err(_) => "неизвестно".into(),
        }),
    );
    push(
        "работает",
        number(pairs, "uptime_s").map(|s| duration(s as u64)),
    );
    push(
        "процессор",
        text("cpu").map(|cpu| match get(pairs, "threads") {
            Some(threads) => format!("{cpu}, потоков: {threads}"),
            None => cpu,
        }),
    );
    push(
        "память",
        number(pairs, "ram_free_mb")
            .zip(number(pairs, "ram_total_mb"))
            .map(|(free, total)| {
                format!(
                    "свободно {} из {} ГБ",
                    gb(free / 1024.0),
                    gb(total / 1024.0)
                )
            }),
    );
    push(
        "диск",
        number(pairs, "disk_free_gb")
            .zip(number(pairs, "disk_total_gb"))
            .map(|(free, total)| format!("свободно {} из {} ГБ", gb(free), gb(total))),
    );
    push("экран", text("screens"));
    push("питание", power(pairs));
    push(
        "экран заблокирован",
        get(pairs, "locked").map(|v| if flag(v) { "да" } else { "нет" }.into()),
    );
    push("ferry", text("ferry"));
    push(
        "режим",
        get(pairs, "mode")
            .and_then(crate::mode::Mode::parse)
            .map(|m| m.label().into()),
    );
    out
}

/// То, что ломает работу молча.
pub fn warnings(pairs: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    if get(pairs, "locked").is_some_and(flag) {
        out.push("экран заблокирован — ввод и GUI-тесты не пройдут".into());
    }
    if get(pairs, "ac").is_some_and(|v| !flag(v)) {
        let battery = get(pairs, "battery")
            .map(|b| format!(" {b}%"))
            .unwrap_or_default();
        out.push(format!(
            "на батарее{battery} — сон отключён только от сети, может уснуть"
        ));
    }
    if let Some(free) = number(pairs, "disk_free_gb").filter(|free| *free < 20.0) {
        out.push(format!("на диске {free:.0} ГБ — для сборки с CEF мало"));
    }
    if let Some(version) = get(pairs, "ferry").filter(|v| *v != crate::VERSION) {
        out.push(format!(
            "ferry на узле {version}, на хабе {} — ferry update",
            crate::VERSION
        ));
    }
    if let Some(mode) = get(pairs, "mode").and_then(crate::mode::Mode::parse)
        && mode != crate::mode::Mode::Active
    {
        out.push(format!("на узле: {}", mode.label()));
    }
    out
}

fn power(pairs: &[(String, String)]) -> Option<String> {
    let ac = flag(get(pairs, "ac")?);
    let battery = get(pairs, "battery").map(|b| format!("{b}%"));
    Some(match (ac, battery) {
        (true, None) => "от сети".into(),
        (true, Some(b)) => format!("от сети, батарея {b}"),
        (false, Some(b)) => format!("от батареи, {b}"),
        (false, None) => "от батареи".into(),
    })
}

fn flag(value: &str) -> bool {
    value.eq_ignore_ascii_case("true")
}

fn duration(secs: u64) -> String {
    let (days, hours, minutes) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes} мин"),
        (0, _) => format!("{hours} ч {minutes} мин"),
        _ => format!("{days} д {hours} ч"),
    }
}

#[cfg(windows)]
fn platform() -> io::Result<Vec<(String, String)>> {
    // UTF-8 на выходе: имя системы на русской Windows — кириллицей.
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Add-Type -AssemblyName System.Windows.Forms
$os = Get-CimInstance Win32_OperatingSystem
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$disk = Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='$env:SystemDrive'"
$video = Get-CimInstance Win32_VideoController | Where-Object { $_.CurrentHorizontalResolution }
$power = [System.Windows.Forms.SystemInformation]::PowerStatus
"os=$($os.Caption) ($($os.BuildNumber))"
"computer=$env:COMPUTERNAME"
"user=$env:USERNAME"
"uptime_s=$([int]((Get-Date) - $os.LastBootUpTime).TotalSeconds)"
"cpu=$($cpu.Name.Trim())"
"threads=$($cpu.NumberOfLogicalProcessors)"
"ram_total_mb=$([int]($os.TotalVisibleMemorySize / 1024))"
"ram_free_mb=$([int]($os.FreePhysicalMemory / 1024))"
"disk_total_gb=$([math]::Round($disk.Size / 1GB, 1))"
"disk_free_gb=$([math]::Round($disk.FreeSpace / 1GB, 1))"
"screens=$(($video | ForEach-Object { "$($_.CurrentHorizontalResolution)x$($_.CurrentVerticalResolution)" }) -join ' ')"
"ac=$($power.PowerLineStatus -eq 'Online')"
if ("$($power.BatteryChargeStatus)" -notmatch 'NoSystemBattery') { "battery=$([int]($power.BatteryLifePercent * 100))" }
"locked=$([bool](Get-Process LogonUI))"
"#;
    let out = crate::screen::powershell("ferry-info.ps1", SCRIPT)?.output()?;
    Ok(parse(&String::from_utf8_lossy(&out.stdout)))
}

/// Не Windows — то, что есть без лишних утилит; полные свойства пока только у Windows-узлов.
#[cfg(not(windows))]
fn platform() -> io::Result<Vec<(String, String)>> {
    let run = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    Ok(vec![
        ("os".into(), run("uname", &["-sr"])),
        ("computer".into(), run("hostname", &[])),
        ("user".into(), std::env::var("USER").unwrap_or_default()),
        #[cfg(target_os = "macos")]
        ("locked".into(), crate::screen::locked().to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(text: &str) -> Vec<(String, String)> {
        parse(text)
    }

    #[test]
    fn describes_a_windows_laptop() {
        let p = pairs(
            "os=Microsoft Windows 10 Pro (17763)\nuptime_s=11520\nram_total_mb=8192\nram_free_mb=3174\n\
             disk_total_gb=237.0\ndisk_free_gb=12.3\nac=False\nbattery=41\nlocked=True\nmode=no-desktop\n",
        );
        let shown = describe(&p);
        let value = |label: &str| {
            shown
                .iter()
                .find(|(l, _)| l == label)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(value("работает"), Some("3 ч 12 мин"));
        assert_eq!(value("память"), Some("свободно 3,1 из 8,0 ГБ"));
        assert_eq!(value("диск"), Some("свободно 12,3 из 237,0 ГБ"));
        assert_eq!(value("питание"), Some("от батареи, 41%"));
        assert_eq!(value("экран заблокирован"), Some("да"));
        assert_eq!(value("режим"), Some("не трогать рабочий стол"));

        let warned = warnings(&p).join("\n");
        for what in [
            "заблокирован",
            "на батарее 41%",
            "12 ГБ",
            "не трогать рабочий стол",
        ] {
            assert!(warned.contains(what), "{what}: {warned}");
        }
    }

    #[test]
    fn healthy_node_has_no_warnings() {
        let p = pairs(&format!(
            "ac=True\nbattery=100\nlocked=False\ndisk_free_gb=120\nferry={}\nmode=active\n",
            crate::VERSION
        ));
        assert!(warnings(&p).is_empty(), "{:?}", warnings(&p));
        assert_eq!(
            describe(&p).iter().find(|(l, _)| l == "питание").unwrap().1,
            "от сети, батарея 100%"
        );
    }
}
