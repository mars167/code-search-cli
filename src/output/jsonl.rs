use std::io::{self, Write};

use serde::Serialize;
use serde_json::Value;

use super::projection::{public_response, PublicPage};

#[derive(Debug, Serialize)]
struct ResultEvent<'a> {
    event: &'static str,
    result: &'a Value,
}

#[derive(Debug, Serialize)]
struct PageEvent {
    event: &'static str,
    page: PublicPage,
    caveats: Vec<Value>,
}

pub(super) fn render_jsonl(value: &Value, out: &mut dyn Write) -> io::Result<()> {
    let public = public_response(value);
    if let Some(results) = public.results.as_array() {
        for result in results {
            let event = ResultEvent {
                event: "result",
                result,
            };
            serde_json::to_writer(&mut *out, &event)?;
            writeln!(out)?;
        }
    }
    let event = PageEvent {
        event: "page",
        page: public.page,
        caveats: public.caveats,
    };
    serde_json::to_writer(&mut *out, &event)?;
    writeln!(out)?;
    Ok(())
}
