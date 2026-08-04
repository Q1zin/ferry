mod desk;

const USAGE: &str = "\
ferry — команды на рабочем столе этой машины

  ferry daemon           принимать команды (запускает планировщик при входе пользователя)
  ferry desk <команда>   выполнить команду на рабочем столе; вывод и код выхода — сюда
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("daemon") => desk::daemon(),
        // Аргументы склеиваются через пробел и уходят в shell одной строкой, как у ssh.
        Some("desk") if args.len() > 1 => desk::run(&args[1..].join(" ")),
        _ => {
            eprint!("{USAGE}");
            Ok(2)
        }
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("ferry: {e}");
            std::process::exit(255)
        }
    }
}
