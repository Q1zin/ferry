//! Файлы между устройствами: кусками по каналу irpc, каталоги — архивом tar. Архив собирается
//! и разбирается во временном файле: так проще с большими файлами и с ошибкой посредине.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use irpc::channel::mpsc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::proto::{Chunk, Fail};

const CHUNK: usize = 256 * 1024;

/// Временный файл в `<dir>/tmp`: имя не повторяется ни в процессе, ни между процессами.
pub fn temp(dir: &Path, ext: &str) -> io::Result<PathBuf> {
    let tmp = dir.join("tmp");
    fs::create_dir_all(&tmp)?;
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(tmp.join(format!("{name}.{ext}")))
}

/// Файл кусками, в конце — `End`. Ошибка отправки — та сторона ушла.
pub async fn send_file(path: &Path, tx: &mpsc::Sender<Chunk>) -> Result<(), Fail> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        tx.send(Chunk::Data(buf[..n].to_vec()))
            .await
            .map_err(crate::proto::lost)?;
    }
    tx.send(Chunk::End).await.map_err(crate::proto::lost)
}

/// Принять файл кусками в `path`.
pub async fn recv_file(rx: &mut mpsc::Receiver<Chunk>, path: &Path) -> Result<(), Fail> {
    let mut file = tokio::fs::File::create(path).await?;
    loop {
        match rx.recv().await.map_err(crate::proto::lost)? {
            Some(Chunk::Data(data)) => file.write_all(&data).await?,
            Some(Chunk::End) => break,
            Some(Chunk::Fail(fail)) => return Err(fail),
            None => return Err(Fail::new("передача оборвалась")),
        }
    }
    file.flush().await?;
    Ok(())
}

/// Ответ потоком: ошибка — последним куском.
pub async fn reply_file(result: Result<PathBuf, Fail>, tx: &mpsc::Sender<Chunk>) {
    match result {
        Ok(path) => {
            if let Err(fail) = send_file(&path, tx).await {
                tx.send(Chunk::Fail(fail)).await.ok();
            }
            let _ = fs::remove_file(&path);
        }
        Err(fail) => {
            tx.send(Chunk::Fail(fail)).await.ok();
        }
    }
}

/// Архив `rels` (пути относительно `base`) в `out`. Каталоги — целиком.
pub fn pack(base: &Path, rels: &[PathBuf], out: &Path) -> io::Result<()> {
    let mut tar = tar::Builder::new(fs::File::create(out)?);
    for rel in rels {
        let full = base.join(rel);
        if full.is_dir() {
            tar.append_dir_all(rel, &full)?;
        } else {
            tar.append_path_with_name(&full, rel)?;
        }
    }
    tar.finish()
}

/// Распаковать архив в `dest`; ответ — имена верхнего уровня.
pub fn unpack(archive: &Path, dest: &Path) -> io::Result<Vec<String>> {
    fs::create_dir_all(dest)?;
    let mut tops = Vec::new();
    let mut tar = tar::Archive::new(fs::File::open(archive)?);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if let Some(top) = path.components().next() {
            let top = top.as_os_str().to_string_lossy().into_owned();
            if !tops.contains(&top) {
                tops.push(top);
            }
        }
        entry.unpack_in(dest)?;
    }
    Ok(tops)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_and_unpack_keep_files_and_dirs() {
        let root = std::env::temp_dir().join(format!("ferry-files-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/report")).unwrap();
        fs::write(root.join("src/report/index.html"), "<h1>ok</h1>").unwrap();
        fs::write(root.join("src/.env"), "KEY=1").unwrap();
        let archive = temp(&root, "tar").unwrap();
        pack(
            &root.join("src"),
            &["report".into(), ".env".into()],
            &archive,
        )
        .unwrap();

        let tops = unpack(&archive, &root.join("out")).unwrap();
        assert_eq!(tops, ["report", ".env"]);
        assert_eq!(
            fs::read_to_string(root.join("out/report/index.html")).unwrap(),
            "<h1>ok</h1>"
        );
        assert_eq!(fs::read_to_string(root.join("out/.env")).unwrap(), "KEY=1");
        let _ = fs::remove_dir_all(&root);
    }
}
