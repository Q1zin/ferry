//! Экран узла: снимок, запись кадрами, окна и настоящий ввод — мышь и клавиатура через ОС,
//! а не через CDP. Всё это запускается на рабочем столе (`ferry desk`): в Session 0 экрана нет.
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

/// `ferry record <секунды> <кадров в секунду> <каталог>`: кадры `frame-0001.png`… с курсором
/// и `sheet.png` — до 12 равномерно выбранных кадров с отметкой времени на одном листе.
pub fn record(seconds: &str, fps: &str, dir: &str) -> io::Result<i32> {
    let dir = std::path::absolute(dir)?;
    Ok(exit_code(
        record_command()?.arg(dir).args([seconds, fps]).status()?,
    ))
}

/// `ferry input <действие> …` — окна и ввод на этой машине.
pub fn input(args: &[&str]) -> io::Result<i32> {
    let number = |s: &str| {
        s.parse::<i32>()
            .map_err(|_| io::Error::other(format!("ожидалось число, а не {s:?}")))
    };
    match args {
        ["windows"] => win::windows(),
        ["move", x, y] => win::pointer(number(x)?, number(y)?, &[]),
        ["click", x, y, how @ ..] => {
            // `right` и `--right` — одно и то же.
            let how: Vec<&str> = how.iter().map(|h| h.trim_start_matches('-')).collect();
            let (down, up) = match how.as_slice() {
                [] | ["left"] => (MOUSE_LEFT_DOWN, MOUSE_LEFT_UP),
                ["right"] => (MOUSE_RIGHT_DOWN, MOUSE_RIGHT_UP),
                ["double"] => (MOUSE_LEFT_DOWN, MOUSE_LEFT_UP),
                _ => return Err(io::Error::other("клик: left, right или double")),
            };
            let presses = if how == ["double"] { 2 } else { 1 };
            let buttons: Vec<u32> = (0..presses).flat_map(|_| [down, up]).collect();
            win::pointer(number(x)?, number(y)?, &buttons)
        }
        ["scroll", clicks] => win::scroll(number(clicks)?),
        ["type", text @ ..] if !text.is_empty() => win::type_text(&text.join(" ")),
        ["key", combos @ ..] if !combos.is_empty() => {
            let combos = combos
                .iter()
                .map(|c| combo(c))
                .collect::<Result<Vec<_>, _>>()?;
            win::keys(&combos)
        }
        _ => Err(io::Error::other(
            "ferry input windows | move <x> <y> | click <x> <y> [right|double] | scroll <n> \
             | type <текст> | key <комбинация>…",
        )),
    }
}

const MOUSE_LEFT_DOWN: u32 = 0x0002;
const MOUSE_LEFT_UP: u32 = 0x0004;
const MOUSE_RIGHT_DOWN: u32 = 0x0008;
const MOUSE_RIGHT_UP: u32 = 0x0010;

/// Клавиша: виртуальный код и нужен ли ей флаг «расширенная» (стрелки, Home, Delete…).
type Key = (u16, bool);

/// `ctrl+shift+s` → клавиши по порядку нажатия; последняя — основная.
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
    std::fs::write(&path, script)?;
    let mut command = Command::new("powershell");
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

#[cfg(not(windows))]
fn record_command() -> io::Result<Command> {
    Err(io::Error::other(
        "запись экрана пока только на Windows-узлах",
    ))
}

#[cfg(not(windows))]
mod win {
    use super::Key;
    use std::io;

    fn unsupported() -> io::Result<i32> {
        Err(io::Error::other("окна и ввод пока только на Windows-узлах"))
    }
    pub fn windows() -> io::Result<i32> {
        unsupported()
    }
    pub fn pointer(_: i32, _: i32, _: &[u32]) -> io::Result<i32> {
        unsupported()
    }
    pub fn scroll(_: i32) -> io::Result<i32> {
        unsupported()
    }
    pub fn type_text(_: &str) -> io::Result<i32> {
        unsupported()
    }
    pub fn keys(_: &[Vec<Key>]) -> io::Result<i32> {
        unsupported()
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

    fn send(inputs: &[Input]) -> io::Result<i32> {
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                size_of::<Input>() as i32,
            )
        };
        if sent as usize == inputs.len() {
            return Ok(0);
        }
        Err(io::Error::other(format!(
            "SendInput принял {sent} из {}: экран заблокирован или ввод не пускают",
            inputs.len()
        )))
    }

    /// Курсор — в точку снимка, затем нажатия кнопок (пусто — просто навести).
    pub fn pointer(x: i32, y: i32, buttons: &[u32]) -> io::Result<i32> {
        let (_, _, width, height) = desktop();
        // Абсолютные координаты SendInput — 0..65535 по всему рабочему столу.
        let scale = |v: i32, size: i32| (v * 65535 + (size - 1) / 2) / (size - 1).max(1);
        let mut inputs = vec![mouse(
            scale(x, width),
            scale(y, height),
            0,
            MOUSE_MOVE | MOUSE_ABSOLUTE | MOUSE_VIRTUAL_DESK,
        )];
        inputs.extend(buttons.iter().map(|&button| mouse(0, 0, 0, button)));
        send(&inputs)
    }

    /// Колесо: больше нуля — вверх.
    pub fn scroll(clicks: i32) -> io::Result<i32> {
        send(&[mouse(0, 0, (clicks * 120) as u32, MOUSE_WHEEL)])
    }

    /// Текст юникодом — не зависит от раскладки; перевод строки и таб — настоящими клавишами.
    pub fn type_text(text: &str) -> io::Result<i32> {
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
    pub fn keys(combos: &[Vec<Key>]) -> io::Result<i32> {
        let mut inputs = Vec::new();
        for combo in combos {
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

    /// Видимые окна с заголовком — сверху вниз по z-порядку; `*` — окно с фокусом.
    pub fn windows() -> io::Result<i32> {
        extern "system" fn collect(window: isize, list: isize) -> i32 {
            unsafe { (*(list as *mut Vec<isize>)).push(window) };
            1
        }
        let (left, top, _, _) = desktop();
        let mut found: Vec<isize> = Vec::new();
        unsafe { EnumWindows(collect, &mut found as *mut Vec<isize> as isize) };
        let names = process_names();
        let focused = unsafe { GetForegroundWindow() };

        println!(
            "  {:<26} {:>11} {:>11}  заголовок",
            "процесс (pid)", "x,y", "ширина×выс."
        );
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
            let process = format!("{} ({pid})", names.get(&pid).map_or("?", String::as_str));
            let (place, size) = if unsafe { IsIconic(window) } != 0 {
                ("свёрнуто".to_string(), String::new())
            } else {
                (
                    format!("{},{}", rect.left - left, rect.top - top),
                    format!("{width}×{height}"),
                )
            };
            let mark = if window == focused { '*' } else { ' ' };
            let title = String::from_utf16_lossy(&title[..len as usize]);
            println!("{mark} {process:<26} {place:>11} {size:>11}  {title}");
        }
        Ok(0)
    }

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
}
