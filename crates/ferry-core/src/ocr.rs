//! Текст на экране (OCR) — для интерфейсов без дерева элементов: canvas, WebGL, игры. Оба
//! распознавателя встроены в систему: на маке — Vision (из JavaScript для автоматизации,
//! `osascript`), на Windows — Windows.Media.Ocr (из PowerShell).

use std::io;
use std::path::Path;

/// Распознанный текст: строка целиком или, при поиске, место искомого в ней. Место — в пикселях
/// картинки: `x y ширина высота`.
#[derive(Debug, Clone, PartialEq)]
pub struct Text {
    /// Строка целиком равна искомому (без учёта регистра).
    pub exact: bool,
    pub rect: (i32, i32, i32, i32),
    pub text: String,
}

impl Text {
    pub fn center(&self) -> (i32, i32) {
        let (x, y, width, height) = self.rect;
        (x + width / 2, y + height / 2)
    }

    fn contains(&self, (px, py): (i32, i32)) -> bool {
        let (x, y, width, height) = self.rect;
        (x..=x + width).contains(&px) && (y..=y + height).contains(&py)
    }
}

/// Строки текста на картинке, сверху вниз; с `want` — места этой подстроки (без учёта регистра).
pub fn read(image: &Path, want: &str) -> io::Result<Vec<Text>> {
    let mut found = parse(&os::run(image, &want.to_lowercase())?);
    found.sort_by_key(|t| (t.rect.1, t.rect.0));
    Ok(found)
}

/// Сколько ждать распознавателя: обычно — секунда-две, а сломанный Vision (видели на бете
/// macOS 27: ошибка e5rt) висит почти минуту и ничего не находит.
const LIMIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Запустить распознаватель и дождаться его не дольше [`LIMIT`]; не успел — убить.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
fn finish(mut command: std::process::Command) -> io::Result<String> {
    use std::process::Stdio;
    // Своя группа процессов: по пределу убиваем её целиком (`kill_tree`).
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let out = match rx.recv_timeout(LIMIT) {
        Ok(out) => out?,
        Err(_) => {
            crate::desk::kill_tree(pid).ok();
            return Err(io::Error::other(format!(
                "распознавание текста не ответило за {} с — распознаватель системы на этом \
                 устройстве не работает",
                LIMIT.as_secs()
            )));
        }
    };
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        return Err(io::Error::other(format!(
            "распознавание текста не вышло: {}",
            said.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Строки распознавателя: `вид<TAB>x<TAB>y<TAB>ширина<TAB>высота<TAB>текст`, вид — `line`,
/// `exact` или `part`.
fn parse(out: &str) -> Vec<Text> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.trim_end_matches('\r').splitn(6, '\t');
            let kind = fields.next()?;
            let mut number = || fields.next()?.parse::<i32>().ok();
            let rect = (number()?, number()?, number()?, number()?);
            Some(Text {
                exact: kind == "exact",
                rect,
                text: fields.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// Куда кликать: точные совпадения, а нет их — частичные. Одно место, найденное дважды (на
/// Windows ищут два распознавателя), — один раз.
pub fn pick(found: Vec<Text>) -> Vec<Text> {
    let exact = found.iter().any(|t| t.exact);
    let mut kept: Vec<Text> = Vec::new();
    for hit in found.into_iter().filter(|t| t.exact || !exact) {
        if !kept.iter().any(|k| k.contains(hit.center())) {
            kept.push(hit);
        }
    }
    kept
}

#[cfg(target_os = "macos")]
mod os {
    use std::io;
    use std::path::Path;
    use std::process::Command;

    /// Vision: русский и английский одним запросом; место подстроки — `boundingBoxForRange`.
    /// Рамки у Vision — доли картинки от левого нижнего угла.
    const SCRIPT: &str = r#"
ObjC.import('Vision');
ObjC.import('AppKit');
function run(argv) {
    var url = $.NSURL.fileURLWithPath(argv[0]), want = argv[1] || '';
    var image = $.NSImage.alloc.initWithContentsOfURL(url);
    if (image.isNil()) { throw new Error('не открылась картинка ' + argv[0]); }
    var bitmap = $.NSBitmapImageRep.imageRepWithData(image.TIFFRepresentation);
    var W = bitmap.pixelsWide, H = bitmap.pixelsHigh;
    var request = $.VNRecognizeTextRequest.alloc.init;
    request.recognitionLevel = 0;
    request.recognitionLanguages = $(['ru-RU', 'en-US']);
    request.usesLanguageCorrection = true;
    var error = $();
    if (!$.VNImageRequestHandler.alloc.initWithURLOptions(url, $()).performRequestsError($([request]), error)) {
        throw new Error('Vision не распознал: ' + (error.isNil() ? '?' : error.localizedDescription.js));
    }
    var box = function (b) {
        return [Math.round(b.origin.x * W), Math.round((1 - b.origin.y - b.size.height) * H),
                Math.round(b.size.width * W), Math.round(b.size.height * H)].join('\t');
    };
    var out = [], results = request.results;
    for (var i = 0; i < results.count; i++) {
        var observation = results.objectAtIndex(i);
        var top = observation.topCandidates(1).objectAtIndex(0);
        var text = top.string.js.replace(/[\t\r\n]+/g, ' ');
        if (!want) { out.push('line\t' + box(observation.boundingBox) + '\t' + text); continue; }
        var at = text.toLowerCase().indexOf(want);
        if (at < 0) { continue; }
        var kind = text.trim().toLowerCase() === want ? 'exact' : 'part';
        var part = top.boundingBoxForRangeError($.NSMakeRange(at, want.length), null);
        var b = part.isNil() ? observation.boundingBox : part.boundingBox;
        out.push(kind + '\t' + box(b) + '\t' + text);
    }
    return out.join('\n');
}
"#;

    pub fn run(image: &Path, want: &str) -> io::Result<String> {
        let mut command = Command::new("osascript");
        command
            .args(["-l", "JavaScript", "-e", SCRIPT])
            .arg(image)
            .arg(want);
        super::finish(command)
    }
}

#[cfg(windows)]
mod os {
    use std::io;
    use std::path::Path;

    /// Windows.Media.Ocr. Один распознаватель знает один язык, а в интерфейсах и кириллица, и
    /// латиница — ищут два: язык пользователя и английский; каждый хорош на своём алфавите, а
    /// чужой превращает в мусор, который с искомым не совпадёт. Мелкий шрифт читается лучше
    /// увеличенным — вдвое, если влезает в предел распознавателя. Место подстроки — по словам.
    const SCRIPT: &str = r#"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$path = $args[0]; $want = "$($args[1])"
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$null = [Windows.Storage.StorageFile, Windows.Storage, ContentType = WindowsRuntime]
$null = [Windows.Media.Ocr.OcrEngine, Windows.Foundation, ContentType = WindowsRuntime]
$null = [Windows.Graphics.Imaging.BitmapDecoder, Windows.Foundation, ContentType = WindowsRuntime]
$asTask = [System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
    $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and
    $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
} | Select-Object -First 1
function Await($op, [Type]$type) {
    $task = $asTask.MakeGenericMethod($type).Invoke($null, @($op)); $task.Wait() | Out-Null; $task.Result
}
$file = Await ([Windows.Storage.StorageFile]::GetFileFromPathAsync($path)) ([Windows.Storage.StorageFile])
$stream = Await ($file.OpenAsync([Windows.Storage.FileAccessMode]::Read)) ([Windows.Storage.Streams.IRandomAccessStream])
$decoder = Await ([Windows.Graphics.Imaging.BitmapDecoder]::CreateAsync($stream)) ([Windows.Graphics.Imaging.BitmapDecoder])
$k = 2
$max = [Windows.Media.Ocr.OcrEngine]::MaxImageDimension
if ($decoder.PixelWidth * $k -gt $max -or $decoder.PixelHeight * $k -gt $max) { $k = 1 }
$transform = New-Object Windows.Graphics.Imaging.BitmapTransform
$transform.ScaledWidth = $decoder.PixelWidth * $k
$transform.ScaledHeight = $decoder.PixelHeight * $k
$transform.InterpolationMode = [Windows.Graphics.Imaging.BitmapInterpolationMode]::Fant
$bitmap = Await ($decoder.GetSoftwareBitmapAsync(
    [Windows.Graphics.Imaging.BitmapPixelFormat]::Bgra8, [Windows.Graphics.Imaging.BitmapAlphaMode]::Premultiplied,
    $transform, [Windows.Graphics.Imaging.ExifOrientationMode]::IgnoreExifOrientation,
    [Windows.Graphics.Imaging.ColorManagementMode]::DoNotColorManage)) ([Windows.Graphics.Imaging.SoftwareBitmap])
$tags = @()
$user = [Windows.Media.Ocr.OcrEngine]::TryCreateFromUserProfileLanguages()
if ($user) { $tags += $user.RecognizerLanguage.LanguageTag }
$tags += 'en-US'
$seen = @{}
foreach ($tag in ($tags | Select-Object -Unique)) {
    $engine = [Windows.Media.Ocr.OcrEngine]::TryCreateFromLanguage((New-Object Windows.Globalization.Language $tag))
    if (-not $engine) { continue }
    $result = Await ($engine.RecognizeAsync($bitmap)) ([Windows.Media.Ocr.OcrResult])
    foreach ($line in $result.Lines) {
        $words = @($line.Words)
        if ($words.Count -eq 0) { continue }
        $text = ($words | ForEach-Object { $_.Text }) -join ' '
        $kind = 'line'; $from = 0; $to = $words.Count - 1
        if ($want) {
            $at = $text.ToLower().IndexOf($want)
            if ($at -lt 0) { continue }
            $kind = if ($text.Trim().ToLower() -eq $want) { 'exact' } else { 'part' }
            $pos = 0; $from = -1
            for ($i = 0; $i -lt $words.Count; $i++) {
                $end = $pos + $words[$i].Text.Length
                if ($end -gt $at -and $pos -lt $at + $want.Length) { if ($from -lt 0) { $from = $i }; $to = $i }
                $pos = $end + 1
            }
        }
        $r = @($words[$from..$to] | ForEach-Object { $_.BoundingRect })
        $x = ($r | ForEach-Object { $_.X } | Measure-Object -Minimum).Minimum
        $y = ($r | ForEach-Object { $_.Y } | Measure-Object -Minimum).Minimum
        $right = ($r | ForEach-Object { $_.X + $_.Width } | Measure-Object -Maximum).Maximum
        $bottom = ($r | ForEach-Object { $_.Y + $_.Height } | Measure-Object -Maximum).Maximum
        $out = "{0}`t{1}`t{2}`t{3}`t{4}`t{5}" -f $kind, [int]($x / $k), [int]($y / $k),
            [int](($right - $x) / $k), [int](($bottom - $y) / $k), $text
        if (-not $seen.ContainsKey($out)) { $seen[$out] = $true; $out }
    }
}
"#;

    pub fn run(image: &Path, want: &str) -> io::Result<String> {
        let mut command = crate::screen::powershell("ferry-ocr.ps1", SCRIPT)?;
        command.arg(image).arg(want);
        super::finish(command)
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod os {
    use std::io;
    use std::path::Path;

    pub fn run(_: &Path, _: &str) -> io::Result<String> {
        Err(io::Error::other(
            "распознавание текста — только на macOS и Windows",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(exact: bool, rect: (i32, i32, i32, i32), text: &str) -> Text {
        Text {
            exact,
            rect,
            text: text.into(),
        }
    }

    #[test]
    fn recognizer_lines_parse() {
        let out = "exact\t10\t20\t30\t12\tГотово\r\npart\t5\t6\t7\t8\tФайл\tс табом\nмусор\n";
        assert_eq!(
            parse(out),
            [
                text(true, (10, 20, 30, 12), "Готово"),
                text(false, (5, 6, 7, 8), "Файл\tс табом"),
            ]
        );
    }

    #[test]
    fn exact_matches_win_and_doubles_collapse() {
        // Точное совпадение важнее частичного.
        let found = vec![
            text(false, (0, 0, 100, 20), "Сохранить как"),
            text(true, (200, 0, 80, 20), "Сохранить"),
        ];
        assert_eq!(pick(found), [text(true, (200, 0, 80, 20), "Сохранить")]);
        // Два распознавателя нашли одно и то же место — один кандидат; другое место — второй.
        let found = vec![
            text(false, (10, 10, 50, 20), "ready"),
            text(false, (12, 11, 48, 18), "ready"),
            text(false, (10, 300, 50, 20), "ready"),
        ];
        assert_eq!(pick(found).len(), 2);
    }

    /// Настоящий Vision на маленькой картинке с текстом из TextEdit.
    #[cfg(target_os = "macos")]
    #[test]
    fn vision_reads_and_finds_text() {
        let image = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ocr-sample.png");
        let lines = read(&image, "").unwrap();
        let all: Vec<&str> = lines.iter().map(|t| t.text.as_str()).collect();
        assert!(all.iter().any(|t| t.contains("second line")), "{all:?}");
        let hits = pick(read(&image, "SECOND").unwrap());
        assert_eq!(hits.len(), 1, "{hits:?}");
        let (x, y) = hits[0].center();
        // «second» — начало второй строки: слева, во второй строке.
        assert!(x < 120 && (28..60).contains(&y), "{hits:?}");
    }
}
