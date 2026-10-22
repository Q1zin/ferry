use crate::devices::{ask, finish};
use ferry_core::{
    control::{Request, Response},
    device::Fail,
    storage::{Kind, Request as StorageRequest},
};
use std::io;

pub fn run(args: &[&str]) -> io::Result<i32> {
    let (device, mut rest) = match args {
        [device, tail @ ..] if !device.starts_with('-') => (Some((*device).to_string()), tail),
        _ => (None, args),
    };
    let mut request = StorageRequest {
        kind: None,
        only: None,
        older_than_days: None,
        max_bytes: None,
        apply: None,
    };
    while !rest.is_empty() {
        match rest {
            ["--only", id, tail @ ..] => {
                request.only = Some((*id).into());
                rest = tail;
            }
            ["--kind", kind, tail @ ..] => {
                request.kind = Some(
                    serde_json::from_value::<Kind>(serde_json::Value::String((*kind).into()))
                        .map_err(io::Error::other)?,
                );
                rest = tail;
            }
            ["--older-than", days, tail @ ..] => {
                request.older_than_days = Some(days.parse().map_err(io::Error::other)?);
                rest = tail;
            }
            ["--max-bytes", bytes, tail @ ..] => {
                request.max_bytes = Some(bytes.parse().map_err(io::Error::other)?);
                rest = tail;
            }
            ["--apply", token, tail @ ..] => {
                request.apply = Some((*token).into());
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    finish((|| {
        let response = ask(Request::Storage { device, request })?;
        let value = match response {
            Response::Report(s) => {
                serde_json::from_str::<serde_json::Value>(&s).map_err(Fail::new)?
            }
            _ => return Err(Fail::new("нет отчёта storage")),
        };
        let failed = value["errors"].as_array().is_some_and(|e| !e.is_empty());
        println!("{value}");
        Ok(i32::from(failed))
    })())
}
