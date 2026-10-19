//! Экран узла: снимок, запись кадрами, окна и настоящий ввод — мышь и клавиатура через ОС,
//! а не через CDP. Windows — SendInput и UI Automation, macOS — CGEvent и Accessibility (нужен
//! доступ к «Универсальному доступу»).
//!
//! Координаты везде — пиксели снимка (`ferry shot`): от левого верхнего угла всего рабочего
//! стола, в физических пикселях (процесс объявляет себя DPI-aware, иначе на экране с
//! масштабом координаты разъехались бы).

use std::io;
use std::process::Command;

use crate::exit_code;

/// Снимок всего экрана в PNG.
pub fn screenshot(file: &str) -> io::Result<i32> {
    let file = std::path::absolute(file)?;
    Ok(exit_code(screenshot_command()?.arg(file).status()?))
}

/// Снимок области экрана: `x y ширина высота` в пикселях снимка — окно или часть экрана.
pub fn screenshot_area(file: &str, area: [i32; 4]) -> io::Result<i32> {
    let file = std::path::absolute(file)?;
    let [x, y, width, height] = area;
    if width <= 0 || height <= 0 {
        return Err(io::Error::other(
            "у области нужны ширина и высота больше нуля",
        ));
    }
    #[cfg(target_os = "macos")]
    {
        // screencapture -R ждёт точки, а координаты — пиксели снимка.
        let k = mac::pixels_per_point();
        let points = |v: i32| (f64::from(v) / k).round() as i64;
        let region = format!(
            "{},{},{},{}",
            points(x),
            points(y),
            points(width),
            points(height)
        );
        let status = Command::new("screencapture")
            .args(["-x", "-R", &region])
            .arg(file)
            .status()?;
        Ok(exit_code(status))
    }
    #[cfg(windows)]
    {
        let script = r#"
Add-Type -MemberDefinition '[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();' -Name Dpi -Namespace Ferry
[Ferry.Dpi]::SetProcessDPIAware() | Out-Null
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$b = [System.Windows.Forms.SystemInformation]::VirtualScreen
$x = [int]$args[1]; $y = [int]$args[2]; $w = [int]$args[3]; $h = [int]$args[4]
$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($b.X + $x, $b.Y + $y, 0, 0, (New-Object System.Drawing.Size $w, $h))
$bmp.Save($args[0], [System.Drawing.Imaging.ImageFormat]::Png)
"#;
        let numbers = [x, y, width, height].map(|v| v.to_string());
        let status = powershell("ferry-screenshot-area.ps1", script)?
            .arg(file)
            .args(numbers)
            .status()?;
        Ok(exit_code(status))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (file, x, y);
        Err(io::Error::other(
            "снимок экрана есть только на Windows и macOS",
        ))
    }
}

/// `ferry record <секунды> <кадров в секунду> <каталог>`: кадры `frame-0001.png`… с курсором
/// и `sheet.png` — до 12 равномерно выбранных кадров с отметкой времени на одном листе.
/// На маке листа нет — только кадры.
pub fn record(seconds: &str, fps: &str, dir: &str) -> io::Result<i32> {
    let dir = std::path::absolute(dir)?;
    #[cfg(target_os = "macos")]
    return mac::record(seconds, fps, &dir);
    #[cfg(not(target_os = "macos"))]
    Ok(exit_code(
        record_command()?.arg(dir).args([seconds, fps]).status()?,
    ))
}

#[cfg(target_os = "macos")]
use mac as os;
#[cfg(not(any(windows, target_os = "macos")))]
use none as os;
/// Доступы macOS, без которых ferry не работает с экраном: (запись экрана, ввод).
#[cfg(target_os = "macos")]
pub fn access() -> (bool, bool) {
    mac::access()
}

/// Попросить доступ: системный запрос (если macOS ещё не спрашивала) и нужный раздел настроек.
/// `screen` — запись экрана, `input` — «Универсальный доступ».
#[cfg(target_os = "macos")]
pub fn ask_access(kind: &str) -> io::Result<()> {
    mac::ask_access(kind)
}

#[cfg(windows)]
use win as os;

/// `ferry input <действие> …` — окна и ввод на этой машине. Для `windows` — таблица окон,
/// для действий — пусто.
pub fn input(args: &[&str]) -> io::Result<String> {
    let number = |s: &str| {
        s.parse::<i32>()
            .map_err(|_| io::Error::other(format!("ожидалось число, а не {s:?}")))
    };
    let buttons = |how: &[&str]| -> io::Result<Vec<u32>> {
        // `right` и `--right` — одно и то же.
        let how: Vec<&str> = how.iter().map(|h| h.trim_start_matches('-')).collect();
        let (down, up) = match how.as_slice() {
            [] | ["left"] => (MOUSE_LEFT_DOWN, MOUSE_LEFT_UP),
            ["right"] => (MOUSE_RIGHT_DOWN, MOUSE_RIGHT_UP),
            ["double"] => (MOUSE_LEFT_DOWN, MOUSE_LEFT_UP),
            _ => return Err(io::Error::other("клик: left, right или double")),
        };
        let presses = if how == ["double"] { 2 } else { 1 };
        Ok((0..presses).flat_map(|_| [down, up]).collect())
    };
    let (args, hold) = held(args);
    match args {
        ["windows"] => Ok(table(&os::windows()?)),
        ["window", name @ ..] if !name.is_empty() => window_rect(&os::windows()?, &name.join(" ")),
        ["focus", name @ ..] if !name.is_empty() => {
            let (windows, name) = (os::windows()?, name.join(" "));
            os::focus(find_window(&windows, &name)?)?;
            window_rect(&windows, &name)
        }
        // Для `ferry wait`: есть ли сейчас — ответ, а не ошибка, чтобы «нет» не спутать с обрывом.
        ["probe-window", name @ ..] if !name.is_empty() => {
            Ok(match find_window(&os::windows()?, &name.join(" ")) {
                Ok(w) => {
                    let (x, y, width, height) = w.rect.unwrap_or_default();
                    format!("found {x} {y} {width} {height}\n")
                }
                Err(_) => "none\n".into(),
            })
        }
        ["probe", name @ ..] if !name.is_empty() => os::probe(&name.join(" ")),
        ["ui", window @ ..] => os::ui(&window.join(" ")),
        ["move", "--name", name] => {
            let (x, y) = os::find(name)?;
            os::pointer(x, y, &[], hold)
        }
        ["click", "--name", name, how @ ..] => {
            let (x, y) = os::find(name)?;
            os::pointer(x, y, &buttons(how)?, hold)
        }
        ["move", x, y] => os::pointer(number(x)?, number(y)?, &[], hold),
        ["click", x, y, how @ ..] => os::pointer(number(x)?, number(y)?, &buttons(how)?, hold),
        ["drag", x1, y1, x2, y2] => {
            os::drag((number(x1)?, number(y1)?), (number(x2)?, number(y2)?), hold)
        }
        ["scroll", clicks] => os::scroll(number(clicks)?),
        ["type", text @ ..] if !text.is_empty() => os::type_text(&text.join(" ")),
        ["key", combo, "--for", seconds] => os::hold_key(combo, duration(seconds)?),
        ["key", combos @ ..] if !combos.is_empty() => os::keys(combos),
        ["clip"] => os::clip_get(),
        ["clip", text] => os::clip_set(text),
        _ => Err(io::Error::other(
            "ferry input windows | ui [<окно>] | focus <окно> | move <x> <y> | click <x> <y> [right|double] \
             | click --name <имя|#id> [right|double] | drag <x1> <y1> <x2> <y2> | scroll <n> \
             | type <текст> | key <комбинация>… | key <комбинация> --for <секунд> | clip [<текст>]; \
             у click, move и drag — [--hold <комбинация>]",
        )),
    }
}

/// Окно для `windows` и поиска по имени: место — в пикселях снимка, `None` — свёрнуто.
struct Window {
    /// Чем ОС снимает именно это окно: HWND на Windows, номер окна на маке.
    id: i64,
    process: String,
    pid: i64,
    title: String,
    rect: Option<(i64, i64, i64, i64)>,
    focused: bool,
}

/// Окна сверху вниз: процесс (pid), место, размер, заголовок; `*` — с фокусом.
fn table(windows: &[Window]) -> String {
    let mut table = format!(
        "  {:<26} {:>11} {:>11}  заголовок\n",
        "процесс (pid)", "x,y", "ширина×выс."
    );
    for w in windows {
        let process = format!("{} ({})", w.process, w.pid);
        let (place, size) = match w.rect {
            Some((x, y, width, height)) => (format!("{x},{y}"), format!("{width}×{height}")),
            None => ("свёрнуто".to_string(), String::new()),
        };
        let mark = if w.focused { '*' } else { ' ' };
        table += &format!("{mark} {process:<26} {place:>11} {size:>11}  {}\n", w.title);
    }
    table
}

/// Самое верхнее несвёрнутое окно: сначала по имени процесса (без учёта регистра, можно без
/// .exe), потом по части заголовка — иначе `ferry` нашёл бы окно с файлом `ferry-test.txt`
/// поверх самого ferry. Его место — `rect`, всегда есть.
fn find_window<'a>(windows: &'a [Window], name: &str) -> io::Result<&'a Window> {
    let want = name.to_lowercase();
    let bare = |p: &str| p.trim_end_matches(".exe").to_string();
    let open = || windows.iter().filter(|w| w.rect.is_some());
    open()
        .find(|w| bare(&w.process.to_lowercase()) == bare(&want))
        .or_else(|| open().find(|w| w.title.to_lowercase().contains(&want)))
        .ok_or_else(|| {
            io::Error::other(format!(
                "нет окна «{name}» (процесс или часть заголовка) — ferry windows покажет, какие есть"
            ))
        })
}

/// Место окна — `x y ширина высота` в пикселях снимка.
fn window_rect(windows: &[Window], name: &str) -> io::Result<String> {
    let (x, y, width, height) = find_window(windows, name)?.rect.unwrap_or_default();
    Ok(format!("{x} {y} {width} {height}\n"))
}

/// Снимок самого окна — даже закрытого другими окнами (в отличие от области экрана).
pub fn screenshot_window(file: &str, name: &str) -> io::Result<i32> {
    let file = std::path::absolute(file)?;
    let windows = os::windows()?;
    let window = find_window(&windows, name)?;
    #[cfg(target_os = "macos")]
    {
        // -o — без тени: размер снимка совпадает с местом окна.
        let status = Command::new("screencapture")
            .args(["-x", "-o", &format!("-l{}", window.id)])
            .arg(file)
            .status()?;
        Ok(exit_code(status))
    }
    #[cfg(windows)]
    {
        // PrintWindow с PW_RENDERFULLCONTENT: окно рисует себя в наш bitmap, так снимаются и
        // закрытые окна, и Chromium.
        let script = r#"
Add-Type @'
using System; using System.Runtime.InteropServices;
public class FerryWindow {
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr window, IntPtr dc, uint flags);
}
'@
[FerryWindow]::SetProcessDPIAware() | Out-Null
Add-Type -AssemblyName System.Drawing
$bmp = New-Object System.Drawing.Bitmap ([int]$args[2]), ([int]$args[3])
$g = [System.Drawing.Graphics]::FromImage($bmp)
$dc = $g.GetHdc()
$ok = [FerryWindow]::PrintWindow([IntPtr][long]$args[1], $dc, 2)
$g.ReleaseHdc($dc)
if (-not $ok) { [Console]::Error.WriteLine('окно не дало себя снять (PrintWindow)'); exit 2 }
$bmp.Save($args[0], [System.Drawing.Imaging.ImageFormat]::Png)
"#;
        let (_, _, width, height) = window.rect.unwrap_or_default();
        let status = powershell("ferry-screenshot-window.ps1", script)?
            .arg(file)
            .args([window.id.to_string(), width.to_string(), height.to_string()])
            .status()?;
        Ok(exit_code(status))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (file, window);
        Err(io::Error::other(
            "снимок экрана есть только на Windows и macOS",
        ))
    }
}

/// `--hold <комбинация>` в конце `click`, `move` и `drag`: держать её, пока работает мышь
/// (shift+клик, cmd+клик, перетаскивание с alt).
fn held<'a, 'b>(args: &'a [&'b str]) -> (&'a [&'b str], &'b str) {
    match args {
        [first, .., "--hold", combo] if matches!(*first, "click" | "move" | "drag") => {
            (&args[..args.len() - 2], combo)
        }
        _ => (args, ""),
    }
}

/// Сколько держать клавишу: больше нуля и не дольше минуты — чтобы не залипла надолго.
fn duration(seconds: &str) -> io::Result<std::time::Duration> {
    seconds
        .parse::<f64>()
        .ok()
        .filter(|s| *s > 0.0 && *s <= 60.0)
        .map(std::time::Duration::from_secs_f64)
        .ok_or_else(|| io::Error::other(format!("--for: секунды от 0 до 60, а не {seconds:?}")))
}

/// Перетаскивание идёт шагами: приложения ждут движения мыши с нажатой кнопкой, а не прыжка.
const DRAG_STEPS: i32 = 20;
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
const DRAG_PAUSE: std::time::Duration = std::time::Duration::from_millis(15);

/// Точка на шаге `step` из [`DRAG_STEPS`] по пути от `from` к `to`.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn along(from: (i32, i32), to: (i32, i32), step: i32) -> (i32, i32) {
    (
        from.0 + (to.0 - from.0) * step / DRAG_STEPS,
        from.1 + (to.1 - from.1) * step / DRAG_STEPS,
    )
}

const MOUSE_LEFT_DOWN: u32 = 0x0002;
const MOUSE_LEFT_UP: u32 = 0x0004;
const MOUSE_RIGHT_DOWN: u32 = 0x0008;
const MOUSE_RIGHT_UP: u32 = 0x0010;

/// Клавиша Windows: виртуальный код и нужен ли ей флаг «расширенная» (стрелки, Home, Delete…).
type Key = (u16, bool);

/// `ctrl+shift+s` → клавиши Windows по порядку нажатия; последняя — основная.
#[cfg_attr(not(windows), allow(dead_code))]
fn combo(text: &str) -> io::Result<Vec<Key>> {
    text.split('+')
        .map(|name| {
            key(&name.to_ascii_lowercase()).ok_or_else(|| {
                io::Error::other(format!(
                    "неизвестная клавиша {name:?}: буквы, цифры, f1–f24, ctrl, shift, alt, win, \
                     enter, esc, tab, space, backspace, delete, insert, home, end, pageup, \
                     pagedown, up, down, left, right"
                ))
            })
        })
        .collect()
}

#[cfg_attr(not(windows), allow(dead_code))]
fn key(name: &str) -> Option<Key> {
    let plain = |code| Some((code, false));
    let extended = |code| Some((code, true));
    match name {
        "ctrl" | "control" => plain(0x11),
        "shift" => plain(0x10),
        "alt" => plain(0x12),
        "win" | "cmd" | "meta" => extended(0x5B),
        "enter" | "return" => plain(0x0D),
        "esc" | "escape" => plain(0x1B),
        "tab" => plain(0x09),
        "space" => plain(0x20),
        "backspace" => plain(0x08),
        "delete" | "del" => extended(0x2E),
        "insert" | "ins" => extended(0x2D),
        "home" => extended(0x24),
        "end" => extended(0x23),
        "pageup" | "pgup" => extended(0x21),
        "pagedown" | "pgdn" => extended(0x22),
        "up" => extended(0x26),
        "down" => extended(0x28),
        "left" => extended(0x25),
        "right" => extended(0x27),
        _ => match name.as_bytes() {
            [c @ b'a'..=b'z'] => plain(u16::from(c.to_ascii_uppercase())),
            [c @ b'0'..=b'9'] => plain(u16::from(*c)),
            [b'f', digits @ ..] => std::str::from_utf8(digits)
                .ok()?
                .parse::<u16>()
                .ok()
                .filter(|n| (1..=24).contains(n))
                .and_then(|n| plain(0x6F + n)),
            _ => None,
        },
    }
}

#[cfg(windows)]
pub(crate) fn powershell(name: &str, script: &str) -> io::Result<Command> {
    let path = std::env::temp_dir().join(name);
    // С BOM: файл без него Windows PowerShell 5.1 читает в кодировке ANSI, и кириллица в
    // скрипте ломает разбор.
    std::fs::write(&path, format!("\u{feff}{script}"))?;
    use std::os::windows::process::CommandExt;
    let mut command = Command::new("powershell");
    // Без своего окна консоли: из приложения (GUI-процесс) оно открылось бы поверх рабочего
    // стола — и попало бы в собственный снимок.
    command.creation_flags(crate::desk::CREATE_NO_WINDOW);
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(path);
    Ok(command)
}

/// Без SetProcessDPIAware на экране с масштабом снимется только часть.
#[cfg(windows)]
const POWERSHELL_SETUP: &str = r#"
Add-Type -MemberDefinition '[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();' -Name Dpi -Namespace Ferry
[Ferry.Dpi]::SetProcessDPIAware() | Out-Null
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$b = [System.Windows.Forms.SystemInformation]::VirtualScreen
$bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
$g = [System.Drawing.Graphics]::FromImage($bmp)
"#;

#[cfg(windows)]
fn screenshot_command() -> io::Result<Command> {
    let script = format!(
        "{POWERSHELL_SETUP}
$g.CopyFromScreen($b.Location, [System.Drawing.Point]::Empty, $b.Size)
$bmp.Save($args[0], [System.Drawing.Imaging.ImageFormat]::Png)
"
    );
    powershell("ferry-screenshot.ps1", &script)
}

#[cfg(windows)]
fn record_command() -> io::Result<Command> {
    // Курсор CopyFromScreen не снимает — рисуем стрелку сами: видно, куда кликали.
    // Кадры пишутся по расписанию от секундомера: медленный кадр не сдвигает остальные.
    let script = format!(
        r#"{POWERSHELL_SETUP}
$dir = $args[0]; $seconds = [double]$args[1]; $fps = [double]$args[2]
if (Test-Path $dir) {{ Remove-Item $dir -Recurse -Force }}
New-Item -ItemType Directory $dir | Out-Null
$sw = [Diagnostics.Stopwatch]::StartNew()
$times = @()
while ($sw.ElapsedMilliseconds -lt $seconds * 1000) {{
    $g.CopyFromScreen($b.Location, [System.Drawing.Point]::Empty, $b.Size)
    $p = [System.Windows.Forms.Cursor]::Position
    [System.Windows.Forms.Cursors]::Default.Draw($g, (New-Object System.Drawing.Rectangle ($p.X - $b.X), ($p.Y - $b.Y), 32, 32))
    $times += $sw.ElapsedMilliseconds
    $bmp.Save((Join-Path $dir ('frame-{{0:D4}}.png' -f $times.Count)), [System.Drawing.Imaging.ImageFormat]::Png)
    $wait = [int]($times.Count * 1000 / $fps - $sw.ElapsedMilliseconds)
    if ($wait -gt 0) {{ Start-Sleep -Milliseconds $wait }}
}}
$n = $times.Count
if ($n -gt 0) {{
    $pick = [Math]::Min(12, $n); $cols = [Math]::Min(4, $pick); $rows = [Math]::Ceiling($pick / $cols)
    $tw = 480; $th = [int]($b.Height * $tw / $b.Width)
    $sheet = New-Object System.Drawing.Bitmap ($tw * $cols), ($th * $rows)
    $sg = [System.Drawing.Graphics]::FromImage($sheet)
    $font = New-Object System.Drawing.Font 'Arial', 13
    for ($i = 0; $i -lt $pick; $i++) {{
        $k = [int][Math]::Floor($i * ($n - 1) / [Math]::Max(1, $pick - 1)) + 1
        $img = [System.Drawing.Image]::FromFile((Join-Path $dir ('frame-{{0:D4}}.png' -f $k)))
        $x = ($i % $cols) * $tw; $y = [Math]::Floor($i / $cols) * $th
        $sg.DrawImage($img, $x, $y, $tw, $th); $img.Dispose()
        $label = '#{{0}}  {{1:N1}} s' -f $k, ($times[$k - 1] / 1000)
        $sg.FillRectangle([System.Drawing.Brushes]::Black, $x, $y, 120, 24)
        $sg.DrawString($label, $font, [System.Drawing.Brushes]::Yellow, $x + 4, $y + 3)
    }}
    $sheet.Save((Join-Path $dir 'sheet.png'), [System.Drawing.Imaging.ImageFormat]::Png)
}}
"$n"
"#
    );
    powershell("ferry-record.ps1", &script)
}

#[cfg(target_os = "macos")]
fn screenshot_command() -> io::Result<Command> {
    let mut command = Command::new("screencapture");
    command.arg("-x");
    Ok(command)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn screenshot_command() -> io::Result<Command> {
    Err(io::Error::other(
        "снимок экрана есть только на Windows и macOS",
    ))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn record_command() -> io::Result<Command> {
    Err(io::Error::other(
        "запись экрана пока только на Windows-узлах",
    ))
}

#[cfg(not(any(windows, target_os = "macos")))]
mod none {
    use std::io;

    fn unsupported() -> io::Result<String> {
        Err(io::Error::other("окна и ввод — только на Windows и macOS"))
    }
    pub fn windows() -> io::Result<Vec<super::Window>> {
        unsupported().map(|_| Vec::new())
    }
    pub fn ui(_: &str) -> io::Result<String> {
        unsupported()
    }
    pub fn find(_: &str) -> io::Result<(i32, i32)> {
        Err(io::Error::other("окна и ввод — только на Windows и macOS"))
    }
    pub fn probe(_: &str) -> io::Result<String> {
        unsupported()
    }
    pub fn focus(_: &super::Window) -> io::Result<()> {
        unsupported().map(drop)
    }
    pub fn pointer(_: i32, _: i32, _: &[u32], _: &str) -> io::Result<String> {
        unsupported()
    }
    pub fn drag(_: (i32, i32), _: (i32, i32), _: &str) -> io::Result<String> {
        unsupported()
    }
    pub fn hold_key(_: &str, _: std::time::Duration) -> io::Result<String> {
        unsupported()
    }
    pub fn clip_get() -> io::Result<String> {
        unsupported()
    }
    pub fn clip_set(_: &str) -> io::Result<String> {
        unsupported()
    }
    pub fn scroll(_: i32) -> io::Result<String> {
        unsupported()
    }
    pub fn type_text(_: &str) -> io::Result<String> {
        unsupported()
    }
    pub fn keys(_: &[&str]) -> io::Result<String> {
        unsupported()
    }
}

/// Ввод через CGEvent, окна через CGWindowList. Координаты — пиксели снимка главного дисплея
/// (`screencapture`), CGEvent ждёт точки: на Retina в точке два пикселя.
#[cfg(target_os = "macos")]
mod mac {
    use std::io;
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::{CFType, CFTypeID, CFTypeRef, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::number::CFNumber;
    use core_foundation::string::{CFString, CFStringRef};
    use core_graphics::display::CGDisplay;
    use core_graphics::event::{
        CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
        ScrollEventUnit,
    };
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::{CGPoint, CGRect};
    use core_graphics::window::{
        copy_window_info, kCGNullWindowID, kCGWindowBounds, kCGWindowLayer,
        kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly, kCGWindowName,
        kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID,
    };

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
        fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
        fn AXUIElementCopyAttributeValue(
            element: CFTypeRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> i32;
        fn AXUIElementCopyMultipleAttributeValues(
            element: CFTypeRef,
            attributes: CFArrayRef,
            options: u32,
            values: *mut CFArrayRef,
        ) -> i32;
        fn AXValueGetTypeID() -> CFTypeID;
        fn AXValueGetType(value: CFTypeRef) -> u32;
        fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut std::ffi::c_void) -> bool;
    }

    pub fn access() -> (bool, bool) {
        unsafe { (CGPreflightScreenCaptureAccess(), AXIsProcessTrusted()) }
    }

    pub fn ask_access(kind: &str) -> io::Result<()> {
        let pane = match kind {
            "screen" => {
                unsafe { CGRequestScreenCaptureAccess() };
                "Privacy_ScreenCapture"
            }
            "input" => {
                let _ = trusted();
                "Privacy_Accessibility"
            }
            _ => return Err(io::Error::other(format!("неизвестный доступ: {kind}"))),
        };
        Command::new("open")
            .arg(format!(
                "x-apple.systempreferences:com.apple.preference.security?{pane}"
            ))
            .status()?;
        Ok(())
    }

    /// Ввод macOS пускает только приложениям с доступом к «Универсальному доступу». Нет его —
    /// система сама покажет запрос, а команда скажет, куда идти.
    fn trusted() -> io::Result<()> {
        let prompt = CFDictionary::from_CFType_pairs(&[(
            CFString::new("AXTrustedCheckOptionPrompt"),
            CFBoolean::true_value(),
        )]);
        if unsafe { AXIsProcessTrustedWithOptions(prompt.as_concrete_TypeRef()) } {
            return Ok(());
        }
        Err(io::Error::other(
            "ввод на этом маке не пускают: «Системные настройки» → «Конфиденциальность и \
             безопасность» → «Универсальный доступ» — включи ferry и повтори",
        ))
    }

    pub fn pixels_per_point() -> f64 {
        scale()
    }

    /// Пикселей в точке на главном дисплее. `pixels_wide` на Retina отдаёт точки — настоящие
    /// пиксели, как у `screencapture`, знает только режим дисплея.
    fn scale() -> f64 {
        let main = CGDisplay::main();
        let points = main.bounds().size.width;
        let pixels = main
            .display_mode()
            .map_or(points, |mode| mode.pixel_width() as f64);
        if points > 0.0 { pixels / points } else { 1.0 }
    }

    fn event(made: Result<CGEvent, ()>) -> io::Result<CGEvent> {
        made.map_err(|()| io::Error::other("событие ввода не создалось"))
    }

    fn source() -> io::Result<CGEventSource> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| io::Error::other("источник событий ввода не создался"))
    }

    /// Точка снимка → точка экрана.
    fn point(x: i32, y: i32) -> CGPoint {
        let k = scale();
        CGPoint::new(f64::from(x) / k, f64::from(y) / k)
    }

    /// Событие мыши с флагами удерживаемых модификаторов — по ним приложения узнают cmd+клик.
    fn post_mouse(
        kind: CGEventType,
        at: CGPoint,
        button: CGMouseButton,
        flags: CGEventFlags,
        clicks: i64,
    ) -> io::Result<()> {
        let mouse = event(CGEvent::new_mouse_event(source()?, kind, at, button))?;
        mouse.set_flags(flags);
        if clicks > 0 {
            mouse.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, clicks);
        }
        mouse.post(CGEventTapLocation::HID);
        Ok(())
    }

    /// Нажать клавиши комбинации `hold` (пусто — ничего), сделать `act` с их флагами, отпустить
    /// — отпускаем в любом случае, иначе модификатор залипнет.
    fn holding(hold: &str, act: impl FnOnce(CGEventFlags) -> io::Result<()>) -> io::Result<String> {
        let keys = match hold {
            "" => Vec::new(),
            _ => combo(hold)?,
        };
        trusted()?;
        let mut flags = CGEventFlags::CGEventFlagNull;
        let mut pressed = Ok(());
        let mut down = 0;
        for &(code, flag) in &keys {
            flags |= flag;
            pressed = key(code, true, flags);
            if pressed.is_err() {
                break;
            }
            down += 1;
        }
        let acted = pressed.and_then(|()| act(flags));
        let mut released = Ok(());
        for &(code, flag) in keys[..down].iter().rev() {
            flags.remove(flag);
            released = released.and(key(code, false, flags));
        }
        acted.and(released).map(|()| String::new())
    }

    /// Курсор — в точку снимка, затем нажатия кнопок (коды — как у Windows: `MOUSE_*`); `hold`
    /// держится всё это время.
    pub fn pointer(x: i32, y: i32, buttons: &[u32], hold: &str) -> io::Result<String> {
        let at = point(x, y);
        holding(hold, |flags| {
            post_mouse(CGEventType::MouseMoved, at, CGMouseButton::Left, flags, 0)?;
            // Двойной клик — второе нажатие с отметкой «второй по счёту».
            let mut presses = 0;
            for &b in buttons {
                let (kind, button) = match b {
                    super::MOUSE_LEFT_DOWN => (CGEventType::LeftMouseDown, CGMouseButton::Left),
                    super::MOUSE_LEFT_UP => (CGEventType::LeftMouseUp, CGMouseButton::Left),
                    super::MOUSE_RIGHT_DOWN => (CGEventType::RightMouseDown, CGMouseButton::Right),
                    _ => (CGEventType::RightMouseUp, CGMouseButton::Right),
                };
                if b == super::MOUSE_LEFT_DOWN || b == super::MOUSE_RIGHT_DOWN {
                    presses += 1;
                }
                post_mouse(kind, at, button, flags, presses)?;
            }
            Ok(())
        })
    }

    /// Нажать левую кнопку в `from`, провести шагами до `to`, отпустить. Кнопку отпускаем, даже
    /// если по дороге событие не создалось.
    pub fn drag(from: (i32, i32), to: (i32, i32), hold: &str) -> io::Result<String> {
        holding(hold, |flags| {
            let left = CGMouseButton::Left;
            let start = point(from.0, from.1);
            let moved = post_mouse(CGEventType::MouseMoved, start, left, flags, 0)
                .and_then(|()| post_mouse(CGEventType::LeftMouseDown, start, left, flags, 1))
                .and_then(|()| {
                    for step in 1..=super::DRAG_STEPS {
                        std::thread::sleep(super::DRAG_PAUSE);
                        let (x, y) = super::along(from, to, step);
                        post_mouse(CGEventType::LeftMouseDragged, point(x, y), left, flags, 1)?;
                    }
                    Ok(())
                });
            let end = point(to.0, to.1);
            let released = post_mouse(CGEventType::LeftMouseUp, end, left, flags, 1);
            moved.and(released)
        })
    }

    /// Держать комбинацию `time` и отпустить.
    pub fn hold_key(combo: &str, time: Duration) -> io::Result<String> {
        holding(combo, |_| {
            std::thread::sleep(time);
            Ok(())
        })
    }

    /// pbpaste и pbcopy разбирают байты по локали, а у приложения из Finder её нет — без
    /// UTF-8 кириллица превратится в мусор.
    fn pasteboard(tool: &str) -> Command {
        let mut command = Command::new(tool);
        command.env("LANG", "en_US.UTF-8");
        command
    }

    /// Текст из буфера обмена; не текст или пусто — пустая строка.
    pub fn clip_get() -> io::Result<String> {
        let out = pasteboard("pbpaste").output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn clip_set(text: &str) -> io::Result<String> {
        use std::io::Write;
        let mut copy = pasteboard("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        copy.stdin
            .take()
            .ok_or_else(|| io::Error::other("pbcopy без stdin"))?
            .write_all(text.as_bytes())?;
        match copy.wait()? {
            status if status.success() => Ok(String::new()),
            status => Err(io::Error::other(format!("pbcopy вышел с {status}"))),
        }
    }

    /// Колесо: больше нуля — вверх.
    pub fn scroll(clicks: i32) -> io::Result<String> {
        trusted()?;
        let wheel = CGEvent::new_scroll_event(source()?, ScrollEventUnit::LINE, 1, clicks, 0, 0);
        event(wheel)?.post(CGEventTapLocation::HID);
        Ok(String::new())
    }

    fn key(code: u16, down: bool, flags: CGEventFlags) -> io::Result<()> {
        let press = event(CGEvent::new_keyboard_event(source()?, code, down))?;
        press.set_flags(flags);
        press.post(CGEventTapLocation::HID);
        Ok(())
    }

    /// Текст юникодом — раскладка не важна; перевод строки и таб — настоящими клавишами.
    pub fn type_text(text: &str) -> io::Result<String> {
        trusted()?;
        let none = CGEventFlags::CGEventFlagNull;
        let mut run = String::new();
        let flush = |run: &mut String| -> io::Result<()> {
            if !run.is_empty() {
                for down in [true, false] {
                    let press = event(CGEvent::new_keyboard_event(source()?, 0, down))?;
                    press.set_string(run);
                    press.post(CGEventTapLocation::HID);
                }
                run.clear();
            }
            Ok(())
        };
        for c in text.chars() {
            let special = match c {
                '\n' => Some(36),
                '\t' => Some(48),
                _ => None,
            };
            match special {
                Some(code) => {
                    flush(&mut run)?;
                    key(code, true, none)?;
                    key(code, false, none)?;
                }
                // Одно событие несёт до 20 знаков UTF-16.
                None => {
                    run.push(c);
                    if run.encode_utf16().count() >= 16 {
                        flush(&mut run)?;
                    }
                }
            }
        }
        flush(&mut run)?;
        Ok(String::new())
    }

    /// Каждая комбинация: нажать по порядку, отпустить в обратном. Модификаторы — ещё и флагами
    /// на каждом событии: по ним приложения узнают cmd+s.
    pub fn keys(combos: &[&str]) -> io::Result<String> {
        let combos = combos
            .iter()
            .map(|c| combo(c))
            .collect::<io::Result<Vec<_>>>()?;
        trusted()?;
        for keys in combos {
            let mut flags = CGEventFlags::CGEventFlagNull;
            for &(code, flag) in &keys {
                flags |= flag;
                key(code, true, flags)?;
            }
            for &(code, flag) in keys.iter().rev() {
                flags.remove(flag);
                key(code, false, flags)?;
            }
        }
        Ok(String::new())
    }

    pub(super) fn combo(text: &str) -> io::Result<Vec<(u16, CGEventFlags)>> {
        text.split('+')
            .map(|name| {
                keycode(&name.to_ascii_lowercase()).ok_or_else(|| {
                    io::Error::other(format!(
                        "неизвестная клавиша {name:?}: буквы, цифры, f1–f20, cmd, ctrl, shift, \
                         alt, enter, esc, tab, space, backspace, delete, home, end, pageup, \
                         pagedown, up, down, left, right"
                    ))
                })
            })
            .collect()
    }

    /// Коды клавиш macOS (раскладка ANSI) и флаг модификатора.
    fn keycode(name: &str) -> Option<(u16, CGEventFlags)> {
        let plain = |code| Some((code, CGEventFlags::CGEventFlagNull));
        match name {
            "cmd" | "command" | "win" | "meta" => Some((55, CGEventFlags::CGEventFlagCommand)),
            "ctrl" | "control" => Some((59, CGEventFlags::CGEventFlagControl)),
            "shift" => Some((56, CGEventFlags::CGEventFlagShift)),
            "alt" | "option" | "opt" => Some((58, CGEventFlags::CGEventFlagAlternate)),
            "enter" | "return" => plain(36),
            "esc" | "escape" => plain(53),
            "tab" => plain(48),
            "space" => plain(49),
            "backspace" => plain(51),
            "delete" | "del" => plain(117),
            "home" => plain(115),
            "end" => plain(119),
            "pageup" | "pgup" => plain(116),
            "pagedown" | "pgdn" => plain(121),
            "up" => plain(126),
            "down" => plain(125),
            "left" => plain(123),
            "right" => plain(124),
            _ => {
                const LETTERS: &[u8] = b"asdfhgzxcv\0bqweryt";
                const MORE: &[(u8, u16)] = &[
                    (b'o', 31),
                    (b'u', 32),
                    (b'i', 34),
                    (b'p', 35),
                    (b'l', 37),
                    (b'j', 38),
                    (b'k', 40),
                    (b'n', 45),
                    (b'm', 46),
                ];
                const DIGITS: [u16; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
                const F: [u16; 20] = [
                    122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111, 105, 107, 113, 106, 64,
                    79, 80, 90,
                ];
                match name.as_bytes() {
                    [c @ b'a'..=b'z'] => LETTERS
                        .iter()
                        .position(|l| l == c)
                        .map(|i| i as u16)
                        .or_else(|| MORE.iter().find(|(l, _)| l == c).map(|&(_, code)| code))
                        .and_then(plain),
                    [d @ b'0'..=b'9'] => plain(DIGITS[usize::from(d - b'0')]),
                    [b'f', digits @ ..] => std::str::from_utf8(digits)
                        .ok()?
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (1..=20).contains(n))
                        .and_then(|n| plain(F[n - 1])),
                    _ => None,
                }
            }
        }
    }

    /// Видимые окна приложений — сверху вниз; с фокусом — самое верхнее. Заголовки чужих окон
    /// macOS отдаёт только с доступом к записи экрана (он и так нужен снимкам).
    pub fn windows() -> io::Result<Vec<super::Window>> {
        let k = scale();
        let list = copy_window_info(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
            kCGNullWindowID,
        )
        .ok_or_else(|| io::Error::other("macOS не отдал список окон"))?;
        let mut windows = Vec::new();
        for item in list.iter() {
            let window: CFDictionary<CFString, CFType> =
                unsafe { CFDictionary::wrap_under_get_rule(*item as CFDictionaryRef) };
            let field =
                |name: CFStringRef| window.find(unsafe { CFString::wrap_under_get_rule(name) });
            let number = |name| {
                field(name)
                    .and_then(|v| v.downcast::<CFNumber>())
                    .and_then(|n| n.to_i64())
            };
            let text = |name| {
                field(name)
                    .and_then(|v| v.downcast::<CFString>())
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            };
            // Слой 0 — обычные окна; выше — строка меню, док, всплывающие.
            if number(unsafe { kCGWindowLayer }) != Some(0) {
                continue;
            }
            let Some(rect) = field(unsafe { kCGWindowBounds })
                .and_then(|v| v.downcast::<CFDictionary>())
                .and_then(|d| CGRect::from_dict_representation(&d))
            else {
                continue;
            };
            let px = |v: f64| (v * k).round() as i64;
            windows.push(super::Window {
                id: number(unsafe { kCGWindowNumber }).unwrap_or_default(),
                process: text(unsafe { kCGWindowOwnerName }),
                pid: number(unsafe { kCGWindowOwnerPID }).unwrap_or_default(),
                title: text(unsafe { kCGWindowName }),
                rect: Some((
                    px(rect.origin.x),
                    px(rect.origin.y),
                    px(rect.size.width),
                    px(rect.size.height),
                )),
                focused: windows.is_empty(),
            });
        }
        Ok(windows)
    }

    /// Элемент окна (Accessibility): роль, имя, #id (AXIdentifier), место в точках.
    struct Element {
        depth: usize,
        role: String,
        name: String,
        id: String,
        rect: (f64, f64, f64, f64),
        enabled: bool,
        focused: bool,
    }

    impl Element {
        /// Строка как у UI Automation на Windows: `роль "имя" #id x,y ширина×высота`.
        fn line(&self, k: f64) -> String {
            let px = |v: f64| (v * k).round() as i64;
            let mut text = format!("{}{}", "  ".repeat(self.depth), self.role);
            if self.name.chars().count() > 80 {
                text += &format!(" \"{}…\"", self.name.chars().take(79).collect::<String>());
            } else if !self.name.is_empty() {
                text += &format!(" \"{}\"", self.name);
            }
            if !self.id.is_empty() {
                text += &format!(" #{}", self.id);
            }
            let (x, y, width, height) = self.rect;
            text += &format!(" {},{} {}×{}", px(x), px(y), px(width), px(height));
            if !self.enabled {
                text += " выключен";
            }
            if self.focused {
                text += " *фокус";
            }
            text
        }

        fn center(&self, k: f64) -> (i32, i32) {
            let (x, y, width, height) = self.rect;
            (
                ((x + width / 2.0) * k).round() as i32,
                ((y + height / 2.0) * k).round() as i32,
            )
        }
    }

    fn attribute(element: &CFType, name: &str) -> Option<CFType> {
        let mut value: CFTypeRef = std::ptr::null();
        let name = CFString::new(name);
        let error = unsafe {
            AXUIElementCopyAttributeValue(
                element.as_CFTypeRef(),
                name.as_concrete_TypeRef(),
                &mut value,
            )
        };
        (error == 0 && !value.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(value) })
    }

    fn items(array: Option<CFType>) -> Vec<CFType> {
        array
            .and_then(|a| a.downcast_into::<CFArray>())
            .map(|a| {
                a.get_all_values()
                    .into_iter()
                    .map(|item| unsafe { CFType::wrap_under_get_rule(item) })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Окно для `ui` и `--name`: с фокусом или по процессу / части заголовка, как в
    /// `ferry windows`; у приложения — окно с тем же заголовком, иначе то, что с фокусом.
    fn window(filter: &str) -> io::Result<(CFType, String)> {
        trusted()?;
        let windows = windows()?;
        let found = match filter {
            "" => windows
                .first()
                .ok_or_else(|| io::Error::other("на экране нет окон"))?,
            _ => super::find_window(&windows, filter)?,
        };
        let app = unsafe {
            CFType::wrap_under_create_rule(AXUIElementCreateApplication(found.pid as i32))
        };
        let title = |w: &CFType| {
            attribute(w, "AXTitle")
                .and_then(|t| t.downcast_into::<CFString>())
                .map(|t| t.to_string())
                .unwrap_or_default()
        };
        let all = items(attribute(&app, "AXWindows"));
        let window = all
            .iter()
            .find(|w| !found.title.is_empty() && title(w) == found.title)
            .cloned()
            .or_else(|| attribute(&app, "AXFocusedWindow"))
            .or_else(|| all.first().cloned())
            .ok_or_else(|| io::Error::other(format!("у {} нет окон", found.process)))?;
        let name = title(&window);
        Ok((window, name))
    }

    /// Видимые элементы окна сверху вниз. Безымянные контейнеры не печатаются — только их
    /// дети; элементы вне окна (прокручены, свёрнуты) пропускаются вместе с детьми. Предел —
    /// 1500 элементов, как на Windows.
    fn elements(window: &CFType) -> Vec<Element> {
        const NAMES: [&str; 10] = [
            "AXRole",
            "AXTitle",
            "AXDescription",
            "AXValue",
            "AXIdentifier",
            "AXPosition",
            "AXSize",
            "AXEnabled",
            "AXFocused",
            "AXChildren",
        ];
        let names = CFArray::from_CFTypes(&NAMES.map(CFString::new));
        let mut out = Vec::new();
        let mut bounds = None;
        let mut stack = vec![(window.clone(), 0usize, 0usize)];
        while let Some((element, depth, level)) = stack.pop() {
            if out.len() >= 1500 || level > 40 {
                continue;
            }
            let mut values: CFArrayRef = std::ptr::null();
            let error = unsafe {
                AXUIElementCopyMultipleAttributeValues(
                    element.as_CFTypeRef(),
                    names.as_concrete_TypeRef(),
                    0,
                    &mut values,
                )
            };
            if error != 0 || values.is_null() {
                continue;
            }
            let values: CFArray<CFType> = unsafe { CFArray::wrap_under_create_rule(values) };
            let get = |i: isize| values.get(i).map(|v| v.clone());
            let text = |i| {
                get(i)
                    .and_then(|v| v.downcast_into::<CFString>())
                    .map(|s| {
                        s.to_string()
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default()
            };
            let flag = |i| {
                get(i)
                    .and_then(|v| v.downcast_into::<CFBoolean>())
                    .map(bool::from)
            };
            // AXPosition и AXSize — AXValue с CGPoint и CGSize; отсутствующее — AXValue-ошибка.
            let pair = |i, kind| -> Option<(f64, f64)> {
                let v = get(i)?;
                if v.type_of() != unsafe { AXValueGetTypeID() }
                    || unsafe { AXValueGetType(v.as_CFTypeRef()) } != kind
                {
                    return None;
                }
                let mut xy = [0f64; 2];
                unsafe { AXValueGetValue(v.as_CFTypeRef(), kind, xy.as_mut_ptr().cast()) }
                    .then_some((xy[0], xy[1]))
            };
            let (Some((x, y)), Some((width, height))) = (pair(5, 1), pair(6, 2)) else {
                continue;
            };
            let (left, top, right, bottom) = *bounds.get_or_insert((x, y, x + width, y + height));
            if level > 0 && (x >= right || y >= bottom || x + width <= left || y + height <= top) {
                continue;
            }
            let role = text(0);
            let role = role.strip_prefix("AX").unwrap_or(&role).to_string();
            let name = [1, 2, 3]
                .map(text)
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or_default();
            let id = text(4);
            let noise = name.is_empty()
                && id.is_empty()
                && ["Group", "ScrollArea", "SplitGroup", "LayoutArea", "Unknown"]
                    .contains(&role.as_str());
            let next = if level == 0 || !noise {
                out.push(Element {
                    depth,
                    role,
                    name,
                    id,
                    rect: (x, y, width, height),
                    enabled: flag(7) != Some(false),
                    focused: flag(8) == Some(true),
                });
                depth + 1
            } else {
                depth
            };
            // В стек — с конца: дети выходят в своём порядке.
            for child in items(get(9)).into_iter().rev() {
                stack.push((child, next, level + 1));
            }
        }
        out
    }

    pub fn ui(filter: &str) -> io::Result<String> {
        let (window, _) = window(filter)?;
        let k = scale();
        let all = elements(&window);
        let mut text: String = all.iter().map(|e| e.line(k) + "\n").collect();
        if all.len() >= 1500 {
            text += "… дальше не показано: элементов больше 1500\n";
        }
        Ok(text)
    }

    /// Элементы окна с фокусом с этим именем (без учёта регистра) или `#id`; нет точных —
    /// с частью имени.
    fn hits(name: &str) -> io::Result<(String, Vec<Element>)> {
        let (window, title) = window("")?;
        let want = name.to_lowercase();
        // Само окно (глубина 0) — не кандидат, как и на Windows.
        let (exact, rest): (Vec<_>, Vec<_>) = elements(&window)
            .into_iter()
            .filter(|e| e.depth > 0)
            .partition(|e| e.name.to_lowercase() == want || format!("#{}", e.id) == name);
        let hits = if exact.is_empty() && !name.starts_with('#') {
            rest.into_iter()
                .filter(|e| e.name.to_lowercase().contains(&want))
                .collect()
        } else {
            exact
        };
        Ok((title, hits))
    }

    /// Центр видимого элемента в окне с фокусом. Не нашёлся или их несколько — ошибка.
    pub fn find(name: &str) -> io::Result<(i32, i32)> {
        let (title, hits) = hits(name)?;
        let k = scale();
        match hits.as_slice() {
            [one] => Ok(one.center(k)),
            [] => Err(io::Error::other(format!(
                "в окне «{title}» нет элемента «{name}» — ferry ui покажет, что там есть"
            ))),
            many => {
                let list: Vec<String> = many
                    .iter()
                    .take(10)
                    .map(|e| format!("  {}", e.line(k).trim_start()))
                    .collect();
                Err(io::Error::other(format!(
                    "«{name}» — элементов несколько ({}), уточни имя:\n{}",
                    many.len(),
                    list.join("\n")
                )))
            }
        }
    }

    /// Есть ли элемент сейчас: `found x y`, `none` или `many N` — для `ferry wait`.
    pub fn probe(name: &str) -> io::Result<String> {
        let (_, hits) = hits(name)?;
        Ok(match hits.as_slice() {
            [one] => {
                let (x, y) = one.center(scale());
                format!("found {x} {y}\n")
            }
            [] => "none\n".into(),
            many => format!("many {}\n", many.len()),
        })
    }

    pub fn focus(_: &super::Window) -> io::Result<()> {
        Err(io::Error::other(
            "ferry focus пока только на Windows — на маке кликни по видимой части окна",
        ))
    }

    /// Кадры `frame-0001.png`… с курсором по расписанию от секундомера. Листа-обзора нет.
    pub fn record(seconds: &str, fps: &str, dir: &Path) -> io::Result<i32> {
        let number = |s: &str| {
            s.parse::<f64>()
                .ok()
                .filter(|v| *v > 0.0)
                .ok_or_else(|| io::Error::other(format!("ожидалось число больше нуля, а не {s:?}")))
        };
        let (seconds, fps) = (number(seconds)?, number(fps)?);
        std::fs::create_dir_all(dir)?;
        let start = Instant::now();
        let mut frames = 0;
        while start.elapsed().as_secs_f64() < seconds {
            frames += 1;
            let frame = dir.join(format!("frame-{frames:04}.png"));
            let status = Command::new("screencapture")
                .args(["-x", "-C", "-t", "png"])
                .arg(frame)
                .status()?;
            if !status.success() {
                return Ok(super::exit_code(status));
            }
            let next = Duration::from_secs_f64(f64::from(frames) / fps);
            if let Some(wait) = next.checked_sub(start.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        Ok(0)
    }
}

#[cfg(windows)]
mod win {
    use super::Key;
    use std::collections::HashMap;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    #[repr(C)]
    #[derive(Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct MouseInput {
        dx: i32,
        dy: i32,
        data: u32,
        flags: u32,
        time: u32,
        extra: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KeyInput {
        vk: u16,
        scan: u16,
        flags: u32,
        time: u32,
        extra: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    union Event {
        mouse: MouseInput,
        key: KeyInput,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Input {
        kind: u32,
        event: Event,
    }

    const INPUT_MOUSE: u32 = 0;
    const INPUT_KEYBOARD: u32 = 1;
    const MOUSE_MOVE: u32 = 0x0001;
    const MOUSE_WHEEL: u32 = 0x0800;
    const MOUSE_VIRTUAL_DESK: u32 = 0x4000;
    const MOUSE_ABSOLUTE: u32 = 0x8000;
    const KEY_EXTENDED: u32 = 0x0001;
    const KEY_UP: u32 = 0x0002;
    const KEY_UNICODE: u32 = 0x0004;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetProcessDPIAware() -> i32;
        fn GetSystemMetrics(index: i32) -> i32;
        fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
        fn EnumWindows(callback: extern "system" fn(isize, isize) -> i32, param: isize) -> i32;
        fn IsWindowVisible(window: isize) -> i32;
        fn IsIconic(window: isize) -> i32;
        fn GetWindowTextW(window: isize, text: *mut u16, max: i32) -> i32;
        fn GetWindowRect(window: isize, rect: *mut Rect) -> i32;
        fn GetForegroundWindow() -> isize;
        fn GetWindowThreadProcessId(window: isize, pid: *mut u32) -> u32;
        fn AttachThreadInput(attach: u32, to: u32, on: i32) -> i32;
        fn BringWindowToTop(window: isize) -> i32;
        fn SetForegroundWindow(window: isize) -> i32;
        fn OpenClipboard(owner: isize) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn GetClipboardData(format: u32) -> isize;
        fn SetClipboardData(format: u32, data: isize) -> isize;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn GlobalAlloc(flags: u32, bytes: usize) -> isize;
        fn GlobalLock(memory: isize) -> *mut u16;
        fn GlobalUnlock(memory: isize) -> i32;
        fn GlobalFree(memory: isize) -> isize;
    }

    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    /// Открыть буфер обмена: его может держать другое приложение — подождать до полсекунды.
    fn open_clipboard() -> io::Result<()> {
        for _ in 0..20 {
            if unsafe { OpenClipboard(0) } != 0 {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        Err(io::Error::other("буфер обмена занят другим приложением"))
    }

    /// Текст из буфера обмена; не текст или пусто — пустая строка.
    pub fn clip_get() -> io::Result<String> {
        open_clipboard()?;
        let text = unsafe {
            let data = GetClipboardData(CF_UNICODETEXT);
            let chars = if data == 0 {
                std::ptr::null_mut()
            } else {
                GlobalLock(data)
            };
            if chars.is_null() {
                String::new()
            } else {
                let len = (0..).take_while(|&i| *chars.add(i) != 0).count();
                let text = String::from_utf16_lossy(std::slice::from_raw_parts(chars, len));
                GlobalUnlock(data);
                text
            }
        };
        unsafe { CloseClipboard() };
        Ok(text)
    }

    pub fn clip_set(text: &str) -> io::Result<String> {
        let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
        open_clipboard()?;
        let result = unsafe {
            EmptyClipboard();
            let memory = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
            let chars = if memory == 0 {
                std::ptr::null_mut()
            } else {
                GlobalLock(memory)
            };
            if chars.is_null() {
                Err(io::Error::other("нет памяти под текст буфера обмена"))
            } else {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), chars, wide.len());
                GlobalUnlock(memory);
                // Принятую память буфер обмена забирает себе; не принял — освобождаем сами.
                if SetClipboardData(CF_UNICODETEXT, memory) == 0 {
                    GlobalFree(memory);
                    Err(io::Error::other("Windows не принял текст в буфер обмена"))
                } else {
                    Ok(String::new())
                }
            }
        };
        unsafe { CloseClipboard() };
        result
    }

    /// Левый верхний угол и размер всего рабочего стола (всех мониторов) в физических пикселях.
    fn desktop() -> (i32, i32, i32, i32) {
        unsafe {
            SetProcessDPIAware();
            (
                GetSystemMetrics(76),
                GetSystemMetrics(77),
                GetSystemMetrics(78),
                GetSystemMetrics(79),
            )
        }
    }

    fn mouse(dx: i32, dy: i32, data: u32, flags: u32) -> Input {
        Input {
            kind: INPUT_MOUSE,
            event: Event {
                mouse: MouseInput {
                    dx,
                    dy,
                    data,
                    flags,
                    time: 0,
                    extra: 0,
                },
            },
        }
    }

    fn key(vk: u16, scan: u16, flags: u32) -> Input {
        Input {
            kind: INPUT_KEYBOARD,
            event: Event {
                key: KeyInput {
                    vk,
                    scan,
                    flags,
                    time: 0,
                    extra: 0,
                },
            },
        }
    }

    fn send(inputs: &[Input]) -> io::Result<String> {
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                size_of::<Input>() as i32,
            )
        };
        if sent as usize == inputs.len() {
            return Ok(String::new());
        }
        Err(io::Error::other(format!(
            "SendInput принял {sent} из {}: экран заблокирован или ввод не пускают",
            inputs.len()
        )))
    }

    /// Курсор — в точку снимка.
    fn move_to(x: i32, y: i32) -> Input {
        let (_, _, width, height) = desktop();
        // Абсолютные координаты SendInput — 0..65535 по всему рабочему столу.
        let scale = |v: i32, size: i32| (v * 65535 + (size - 1) / 2) / (size - 1).max(1);
        mouse(
            scale(x, width),
            scale(y, height),
            0,
            MOUSE_MOVE | MOUSE_ABSOLUTE | MOUSE_VIRTUAL_DESK,
        )
    }

    /// Нажать клавиши комбинации по порядку и отпустить в обратном (пусто — ничего).
    fn presses(combo: &str) -> io::Result<(Vec<Input>, Vec<Input>)> {
        if combo.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let keys = super::combo(combo)?;
        let flags = |extended: bool| if extended { KEY_EXTENDED } else { 0 };
        let down = keys.iter().map(|&(vk, ext)| key(vk, 0, flags(ext)));
        let up = keys
            .iter()
            .rev()
            .map(|&(vk, ext)| key(vk, 0, flags(ext) | KEY_UP));
        Ok((down.collect(), up.collect()))
    }

    /// Курсор — в точку снимка, затем нажатия кнопок (пусто — просто навести); `hold` держится
    /// всё это время.
    pub fn pointer(x: i32, y: i32, buttons: &[u32], hold: &str) -> io::Result<String> {
        let (mut inputs, up) = presses(hold)?;
        inputs.push(move_to(x, y));
        inputs.extend(buttons.iter().map(|&button| mouse(0, 0, 0, button)));
        inputs.extend(up);
        send(&inputs)
    }

    /// Нажать левую кнопку в `from`, провести шагами до `to`, отпустить. Кнопку и клавиши
    /// отпускаем, даже если по дороге ввод не прошёл, — иначе они залипнут.
    pub fn drag(from: (i32, i32), to: (i32, i32), hold: &str) -> io::Result<String> {
        let (mut start, up) = presses(hold)?;
        start.extend([
            move_to(from.0, from.1),
            mouse(0, 0, 0, super::MOUSE_LEFT_DOWN),
        ]);
        let moved = send(&start).and_then(|_| {
            for step in 1..=super::DRAG_STEPS {
                std::thread::sleep(super::DRAG_PAUSE);
                let (x, y) = super::along(from, to, step);
                send(&[move_to(x, y)])?;
            }
            Ok(String::new())
        });
        let mut end = vec![mouse(0, 0, 0, super::MOUSE_LEFT_UP)];
        end.extend(up);
        let released = send(&end);
        moved.and(released)
    }

    /// Держать комбинацию `for` и отпустить — отпускаем в любом случае.
    pub fn hold_key(combo: &str, time: std::time::Duration) -> io::Result<String> {
        let (down, up) = presses(combo)?;
        let pressed = send(&down);
        if pressed.is_ok() {
            std::thread::sleep(time);
        }
        let released = send(&up);
        pressed.and(released)
    }

    /// Колесо: больше нуля — вверх.
    pub fn scroll(clicks: i32) -> io::Result<String> {
        send(&[mouse(0, 0, (clicks * 120) as u32, MOUSE_WHEEL)])
    }

    /// Текст юникодом — не зависит от раскладки; перевод строки и таб — настоящими клавишами.
    pub fn type_text(text: &str) -> io::Result<String> {
        let mut inputs = Vec::new();
        for unit in text.encode_utf16() {
            match unit {
                0x0A => inputs.extend([key(0x0D, 0, 0), key(0x0D, 0, KEY_UP)]),
                0x09 => inputs.extend([key(0x09, 0, 0), key(0x09, 0, KEY_UP)]),
                _ => inputs.extend([
                    key(0, unit, KEY_UNICODE),
                    key(0, unit, KEY_UNICODE | KEY_UP),
                ]),
            }
        }
        send(&inputs)
    }

    /// Каждая комбинация: всё нажать по порядку, отпустить в обратном.
    pub fn keys(combos: &[&str]) -> io::Result<String> {
        let combos = combos
            .iter()
            .map(|c| super::combo(c))
            .collect::<io::Result<Vec<Vec<Key>>>>()?;
        let mut inputs = Vec::new();
        for combo in &combos {
            let flags = |extended: bool| if extended { KEY_EXTENDED } else { 0 };
            inputs.extend(combo.iter().map(|&(vk, ext)| key(vk, 0, flags(ext))));
            inputs.extend(
                combo
                    .iter()
                    .rev()
                    .map(|&(vk, ext)| key(vk, 0, flags(ext) | KEY_UP)),
            );
        }
        send(&inputs)
    }

    /// Видимые окна с заголовком — сверху вниз по z-порядку.
    pub fn windows() -> io::Result<Vec<super::Window>> {
        extern "system" fn collect(window: isize, list: isize) -> i32 {
            unsafe { (*(list as *mut Vec<isize>)).push(window) };
            1
        }
        let (left, top, _, _) = desktop();
        let mut found: Vec<isize> = Vec::new();
        unsafe { EnumWindows(collect, &mut found as *mut Vec<isize> as isize) };
        let names = process_names();
        let focused = unsafe { GetForegroundWindow() };
        let mut windows = Vec::new();
        for window in found {
            let mut title = [0u16; 512];
            let len = unsafe { GetWindowTextW(window, title.as_mut_ptr(), title.len() as i32) };
            if len <= 0 || unsafe { IsWindowVisible(window) } == 0 {
                continue;
            }
            let mut rect = Rect::default();
            unsafe { GetWindowRect(window, &mut rect) };
            let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
            if width <= 0 || height <= 0 {
                continue;
            }
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(window, &mut pid) };
            let minimized = unsafe { IsIconic(window) } != 0;
            windows.push(super::Window {
                id: window as i64,
                process: names.get(&pid).cloned().unwrap_or_else(|| "?".into()),
                pid: i64::from(pid),
                title: String::from_utf16_lossy(&title[..len as usize]),
                rect: (!minimized).then(|| {
                    let at = |v: i32, origin: i32| i64::from(v - origin);
                    (
                        at(rect.left, left),
                        at(rect.top, top),
                        i64::from(width),
                        i64::from(height),
                    )
                }),
                focused: window == focused,
            });
        }
        Ok(windows)
    }

    /// Вывести окно вперёд, с фокусом. Windows отдаёт фокус только тому, у кого он уже есть, —
    /// окна задач ferry открываются позади, — поэтому на время подключаемся к вводу окна с фокусом.
    pub fn focus(window: &super::Window) -> io::Result<()> {
        let window = window.id as isize;
        unsafe {
            let ours = GetCurrentThreadId();
            let theirs = GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
            let attached = theirs != ours && AttachThreadInput(ours, theirs, 1) != 0;
            BringWindowToTop(window);
            SetForegroundWindow(window);
            if attached {
                AttachThreadInput(ours, theirs, 0);
            }
        }
        if unsafe { GetForegroundWindow() } != window {
            return Err(io::Error::other("Windows не дал вывести окно вперёд"));
        }
        Ok(())
    }

    /// Элементы окна (UI Automation): роль, имя, #id, место в пикселях снимка. Окно — с фокусом
    /// или по имени процесса / части заголовка.
    pub fn ui(window: &str) -> io::Result<String> {
        automation(&["tree", window])
    }

    /// Центр видимого элемента в окне с фокусом: по имени (точно, иначе — часть имени) или по
    /// `#id` — у безымянных элементов, вроде поля Блокнота. Не нашёлся или их несколько —
    /// ошибка со списком кандидатов.
    pub fn find(name: &str) -> io::Result<(i32, i32)> {
        let out = automation(&["find", name])?;
        let mut xy = out.split_whitespace().map(str::parse::<i32>);
        match (xy.next(), xy.next()) {
            (Some(Ok(x)), Some(Ok(y))) => Ok((x, y)),
            _ => Err(io::Error::other(format!(
                "UI Automation ответил непонятно: {out}"
            ))),
        }
    }

    /// Есть ли элемент сейчас: `found x y`, `none` или `many N` — для `ferry wait`.
    pub fn probe(name: &str) -> io::Result<String> {
        automation(&["probe", name])
    }

    fn automation(args: &[&str]) -> io::Result<String> {
        let out = super::powershell("ferry-ui.ps1", UI_SCRIPT)?
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_string();
        if out.status.success() {
            Ok(text(&out.stdout) + "\n")
        } else {
            Err(io::Error::other(text(&out.stderr)))
        }
    }

    /// Обход дерева UI Automation. Безымянные контейнеры (Pane, Group, Custom) не печатаются —
    /// только их дети; невидимое (за краем, свёрнутое) пропускается. Предел — 1500 элементов:
    /// дерево страницы в браузере бывает огромным.
    const UI_SCRIPT: &str = r#"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Add-Type -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
[DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
'@ -Name Win -Namespace Ferry
[Ferry.Win]::SetProcessDPIAware() | Out-Null
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Windows.Forms
$screen = [System.Windows.Forms.SystemInformation]::VirtualScreen
$AE = [System.Windows.Automation.AutomationElement]
$all = [System.Windows.Automation.Condition]::TrueCondition
$walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
$mode = $args[0]; $arg = $args[1]

function Fail($text) { [Console]::Error.WriteLine($text); exit 2 }

function Window($filter) {
    if (-not $filter) { return $AE::FromHandle([Ferry.Win]::GetForegroundWindow()) }
    foreach ($w in $AE::RootElement.FindAll([System.Windows.Automation.TreeScope]::Children, $all)) {
        $proc = (Get-Process -Id $w.Current.ProcessId -ErrorAction SilentlyContinue).ProcessName
        if ($w.Current.Name -like "*$filter*" -or $proc -eq $filter -or "$proc.exe" -eq $filter) { return $w }
    }
    Fail "нет окна «$filter» — ferry windows покажет, какие есть"
}

function Line($e, $depth) {
    $c = $e.Current
    $text = $c.ControlType.ProgrammaticName -replace '^ControlType\.', ''
    $name = ($c.Name -replace '\s+', ' ').Trim()
    if ($name.Length -gt 80) { $name = $name.Substring(0, 79) + '…' }
    if ($name) { $text += ' "' + $name + '"' }
    if ($c.AutomationId) { $text += ' #' + $c.AutomationId }
    $r = $c.BoundingRectangle
    if (-not $r.IsEmpty) {
        $text += ' {0},{1} {2}×{3}' -f [int]($r.X - $screen.X), [int]($r.Y - $screen.Y), [int]$r.Width, [int]$r.Height
    }
    if (-not $c.IsEnabled) { $text += ' выключен' }
    if ($c.HasKeyboardFocus) { $text += ' *фокус' }
    ('  ' * $depth) + $text
}

$script:count = 0
function Walk($e, $depth) {
    if ($script:count -ge 1500 -or $depth -gt 40) { return }
    $c = $e.Current
    if ($depth -gt 0 -and $c.IsOffscreen) { return }
    $noise = -not $c.Name -and -not $c.AutomationId -and
        $c.ControlType.ProgrammaticName -in 'ControlType.Pane', 'ControlType.Group', 'ControlType.Custom'
    $next = $depth
    if ($depth -eq 0 -or -not $noise) { Line $e $depth; $script:count++; $next = $depth + 1 }
    $child = $walker.GetFirstChild($e)
    while ($child) { Walk $child $next; $child = $walker.GetNextSibling($child) }
}

if ($mode -eq 'tree') {
    Walk (Window $arg) 0
    if ($script:count -ge 1500) { '… дальше не показано: элементов больше 1500' }
    exit 0
}

$w = Window ''
$visible = @($w.FindAll([System.Windows.Automation.TreeScope]::Descendants, $all) |
    Where-Object { -not $_.Current.IsOffscreen -and -not $_.Current.BoundingRectangle.IsEmpty })
$hits = @($visible | Where-Object { $_.Current.Name -eq $arg -or ('#' + $_.Current.AutomationId) -eq $arg })
if (-not $hits -and -not $arg.StartsWith('#')) { $hits = @($visible | Where-Object { $_.Current.Name -like "*$arg*" }) }
if ($mode -eq 'probe') {
    if ($hits.Count -eq 1) {
        $r = $hits[0].Current.BoundingRectangle
        'found {0} {1}' -f [int]($r.X + $r.Width / 2 - $screen.X), [int]($r.Y + $r.Height / 2 - $screen.Y)
    } elseif (-not $hits) { 'none' } else { "many $($hits.Count)" }
    exit 0
}
if ($hits.Count -eq 1) {
    $r = $hits[0].Current.BoundingRectangle
    '{0} {1}' -f [int]($r.X + $r.Width / 2 - $screen.X), [int]($r.Y + $r.Height / 2 - $screen.Y)
    exit 0
}
if (-not $hits) { Fail "в окне «$($w.Current.Name)» нет элемента «$arg» — ferry ui покажет, что там есть" }
$list = ($hits | Select-Object -First 10 | ForEach-Object { Line $_ 1 }) -join "`n"
Fail "«$arg» — элементов несколько ($($hits.Count)), уточни имя:`n$list"
"#;

    /// pid → имя exe: один tasklist на всех.
    fn process_names() -> HashMap<u32, String> {
        let out = Command::new("tasklist")
            .args(["/fo", "csv", "/nh"])
            .creation_flags(crate::desk::CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        let Ok(out) = out else {
            return HashMap::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split("\",\"");
                let name = fields.next()?.trim_start_matches('"');
                let pid = fields.next()?.parse().ok()?;
                Some((pid, name.to_string()))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_is_taken_only_from_mouse_actions() {
        assert_eq!(
            held(&["click", "1", "2", "right", "--hold", "shift"]),
            (&["click", "1", "2", "right"][..], "shift")
        );
        assert_eq!(
            held(&["drag", "1", "2", "3", "4", "--hold", "ctrl+alt"]),
            (&["drag", "1", "2", "3", "4"][..], "ctrl+alt")
        );
        assert_eq!(held(&["click", "1", "2"]), (&["click", "1", "2"][..], ""));
        // Текст, который случайно похож на флаг, остаётся текстом.
        assert_eq!(
            held(&["type", "--hold", "shift"]),
            (&["type", "--hold", "shift"][..], "")
        );
    }

    #[test]
    fn key_hold_time_and_drag_path() {
        assert_eq!(
            duration("1.5").unwrap(),
            std::time::Duration::from_millis(1500)
        );
        for wrong in ["0", "-1", "61", "долго"] {
            assert!(duration(wrong).is_err(), "{wrong}");
        }
        assert_eq!(along((0, 0), (200, -100), 0), (0, 0));
        assert_eq!(along((0, 0), (200, -100), DRAG_STEPS / 2), (100, -50));
        assert_eq!(along((10, 20), (210, 120), DRAG_STEPS), (210, 120));
    }

    #[test]
    fn parses_key_combos() {
        assert_eq!(
            combo("ctrl+shift+s").unwrap(),
            [(0x11, false), (0x10, false), (0x53, false)]
        );
        assert_eq!(combo("Alt+F4").unwrap(), [(0x12, false), (0x73, false)]);
        assert_eq!(combo("win+d").unwrap(), [(0x5B, true), (0x44, false)]);
        assert_eq!(combo("up").unwrap(), [(0x26, true)]);
        assert_eq!(combo("7").unwrap(), [(0x37, false)]);
        assert!(combo("ctrl+f25").is_err());
        assert!(combo("ctrl+ыы").is_err());
    }

    #[test]
    fn finds_windows_by_process_or_title() {
        let window = |process: &str, title: &str, rect, focused| Window {
            id: 1,
            process: process.into(),
            pid: 7,
            title: title.into(),
            rect,
            focused,
        };
        let windows = [
            window("notepad.exe", "*Безымянный – Блокнот", None, false),
            window("ferry-app.exe", "ferry", Some((104, 104, 1116, 759)), true),
            window(
                "notepad.exe",
                "Заметки – Блокнот",
                Some((288, 153, 1440, 759)),
                false,
            ),
        ];
        assert_eq!(
            window_rect(&windows, "Notepad").unwrap(),
            "288 153 1440 759\n"
        );
        assert_eq!(
            window_rect(&windows, "ferry-app.exe").unwrap(),
            "104 104 1116 759\n"
        );
        assert_eq!(
            window_rect(&windows, "заметки").unwrap(),
            "288 153 1440 759\n"
        );
        assert!(window_rect(&windows, "chrome").is_err());
        let table = table(&windows);
        assert!(table.contains("* ferry-app.exe (7)"), "{table}");
        assert!(table.contains("свёрнуто"), "{table}");

        // Имя процесса важнее совпадения в заголовке окна поверх.
        let windows = [
            window(
                "TextEdit",
                "ferry-test.txt",
                Some((300, 134, 1312, 844)),
                true,
            ),
            window("ferry", "ferry", Some((396, 126, 2200, 1440)), false),
        ];
        assert_eq!(
            window_rect(&windows, "ferry").unwrap(),
            "396 126 2200 1440\n"
        );
        assert_eq!(
            window_rect(&windows, "test.txt").unwrap(),
            "300 134 1312 844\n"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parses_mac_key_combos() {
        use core_graphics::event::CGEventFlags as F;
        let codes = |text| mac::combo(text).unwrap();
        assert_eq!(
            codes("cmd+s"),
            [(55, F::CGEventFlagCommand), (1, F::CGEventFlagNull)]
        );
        assert_eq!(
            codes("Alt+F4"),
            [(58, F::CGEventFlagAlternate), (118, F::CGEventFlagNull)]
        );
        assert_eq!(codes("m"), [(46, F::CGEventFlagNull)]);
        assert_eq!(codes("0"), [(29, F::CGEventFlagNull)]);
        assert!(mac::combo("ctrl+f21").is_err());
        assert!(mac::combo("ctrl+ыы").is_err());
    }
}
