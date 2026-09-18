# ferry

Руки для Claude (и для человека) на других машинах локальной сети: команды по SSH, в том числе на настоящем рабочем столе Windows.

Этап 1: узел на Windows — OpenSSH Server с Git Bash и демон рабочего стола. `ferry desk <команда>` выполняет команду в интерактивной сессии пользователя.

## Зачем демон

По SSH команды на Windows попадают в Session 0, где нет рабочего стола: окна не видны, фокуса нет. Демон `ferry daemon` запускает планировщик при входе пользователя, то есть в его сессии. `ferry desk`, вызванный по SSH, передаёт ему команду через 127.0.0.1 и возвращает вывод и код выхода; если соединение оборвалось, команда убивается вместе с деревом процессов.

## Узел на Windows

Нужны Windows 10 1809+ и [Git for Windows](https://git-scm.com/download/win).

На хабе (macOS):

```sh
brew install mingw-w64                                   # один раз
ssh-keygen -t ed25519 -f ~/.ssh/ferry -N ''              # один раз: ключ хаба
cargo build --release --target x86_64-pc-windows-gnu
mkdir -p dist && cp target/x86_64-pc-windows-gnu/release/ferry.exe install.ps1 ~/.ssh/ferry.pub dist/
python3 -m http.server 8000 -d dist
```

На узле, в PowerShell от администратора, под тем пользователем, на чьём рабочем столе будут идти команды:

```powershell
iwr http://<хаб>:8000/install.ps1 -OutFile $env:TEMP\ferry.ps1 -UseBasicParsing
powershell -ExecutionPolicy Bypass -File $env:TEMP\ferry.ps1 -From http://<хаб>:8000
```

Повторный запуск обновляет `ferry.exe` и перезапускает демон.

## Обрыв соединения

Демон гасит дерево команды, как только клиент `ferry desk` умирает. При обрыве ssh это
срабатывает так: `ClientAliveInterval` (ставит установщик) заставляет sshd снести мёртвую
сессию, и команда с выводом ловит это первой же записью в закрытый канал — умирает за ~15–30 с,
демон убирает дерево. Абсолютно молчаливую бесконечную команду (`sleep 1000` без вывода) так не
убить: Win32-OpenSSH не держит job-объект на сессию и сироту не трогает. Для неё в фазе 2 будет
явный `ferry stop` и снятие зависших процессов перед запуском.

## Проверка

```sh
ssh -i ~/.ssh/ferry <user>@<узел> ferry desk notepad    # Блокнот появляется на экране узла
cargo test                                             # демон и клиент на этой машине (unix)
```
