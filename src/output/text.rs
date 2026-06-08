mod status;

use std::io::{self, Write};

use serde_json::Value;

use super::caveats::public_caveats;

use status::{is_status_like, render_text_status_like};

pub(super) fn render_text(value: &Value, out: &mut dyn Write) -> io::Result<()> {
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        let message = value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        let mut lines = message.lines();
        let first = lines.next().unwrap_or("unknown error").trim();
        writeln!(out, "error: {first}")?;
        for line in lines {
            let line = line.trim();
            if line.starts_with("caused by:") {
                writeln!(out, "  {line}")?;
            }
        }
        return Ok(());
    }

    if value.pointer("/guard/triggered").and_then(Value::as_bool) == Some(true) {
        let reason = value
            .pointer("/guard/reason")
            .and_then(Value::as_str)
            .unwrap_or("broad_query");
        let suppressed = value
            .pointer("/guard/suppressedResults")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        writeln!(
            out,
            "warning: broad query guard triggered ({reason}); suppressed {suppressed} results"
        )?;
        render_text_summary(value, out)?;
        render_text_results(value, out)?;
        return Ok(());
    }

    if value.get("noMatch").is_some() {
        let command = value
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("query");
        writeln!(out, "no matches for {command}")?;
        return Ok(());
    }

    if value
        .pointer("/ambiguity/triggered")
        .and_then(Value::as_bool)
        == Some(true)
    {
        let count = value
            .pointer("/ambiguity/candidateCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        writeln!(out, "ambiguous results: {count} candidates")?;
        render_text_facets(value.pointer("/ambiguity/groups/kind"), out, "kinds")?;
        render_text_facets(value.pointer("/ambiguity/groups/topDir"), out, "top dirs")?;
    }

    render_text_results(value, out)?;
    render_text_caveats(value, out)?;
    Ok(())
}

fn render_text_results(value: &Value, out: &mut dyn Write) -> io::Result<()> {
    if let Some(results) = value.get("results").and_then(Value::as_array) {
        let command = value.get("command").and_then(Value::as_str).unwrap_or("");
        if matches!(command, "calls" | "callers") {
            return render_text_graph(value, results, out);
        }
        if command == "read" {
            return render_text_read(results, out);
        }
        if is_status_like(command) {
            return render_text_status_like(command, results, out);
        }
        for result in results {
            render_text_result(result, out)?;
        }
        return Ok(());
    }

    writeln!(out, "{value}")?;
    Ok(())
}

fn render_text_result(result: &Value, out: &mut dyn Write) -> io::Result<()> {
    if let Some(path) = result.get("path").and_then(Value::as_str) {
        let location = format_location(path, result.get("range"));
        if let Some(name) = result
            .get("name")
            .or_else(|| result.get("symbolName"))
            .and_then(Value::as_str)
        {
            let kind = result
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("symbol");
            writeln!(out, "{kind:<12}{name}")?;
            writeln!(out, "  {location}")?;
            return Ok(());
        }
        if let Some(preview) = result.get("preview").and_then(Value::as_str) {
            writeln!(out, "{location}  {}", preview.trim())?;
            return Ok(());
        }
        writeln!(out, "{location}")?;
        return Ok(());
    }

    if let Some(path) = result.get("file").and_then(Value::as_str) {
        writeln!(out, "{path}")?;
        return Ok(());
    }

    writeln!(out, "{}", one_line_json(result))?;
    Ok(())
}

fn render_text_read(results: &[Value], out: &mut dyn Write) -> io::Result<()> {
    for (idx, result) in results.iter().enumerate() {
        if idx > 0 {
            writeln!(out)?;
        }
        let path = result.get("path").and_then(Value::as_str).unwrap_or("read");
        if result.get("binary").and_then(Value::as_bool) == Some(true) {
            writeln!(out, "{path}: binary file not displayed")?;
            continue;
        }
        if let Some(content) = result.get("content").and_then(Value::as_str) {
            write!(out, "{content}")?;
            if !content.ends_with('\n') {
                writeln!(out)?;
            }
        } else {
            writeln!(out, "{}", format_location(path, result.get("range")))?;
        }
    }
    Ok(())
}

fn render_text_graph(value: &Value, results: &[Value], out: &mut dyn Write) -> io::Result<()> {
    let command = value
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("calls");
    let identifier = value
        .pointer("/query/identifier")
        .and_then(Value::as_str)
        .unwrap_or("symbol");
    let title = if command == "callers" {
        format!("Callers of \"{identifier}\" ({})", results.len())
    } else {
        format!("Callees of \"{identifier}\" ({})", results.len())
    };
    writeln!(out, "{title}")?;
    if results.is_empty() {
        return Ok(());
    }
    writeln!(out)?;
    for result in results {
        let caller = result
            .get("enclosingSymbol")
            .and_then(Value::as_str)
            .map(display_symbol)
            .unwrap_or_else(|| identifier.to_string());
        let callee = result
            .get("target")
            .and_then(Value::as_str)
            .map(display_symbol)
            .unwrap_or_else(|| identifier.to_string());
        let path = result.get("path").and_then(Value::as_str).unwrap_or("");
        let location = if path.is_empty() {
            String::new()
        } else {
            format_location(path, result.get("range"))
        };
        if location.is_empty() {
            writeln!(out, "{caller} -> {callee}")?;
        } else {
            writeln!(out, "{caller} -> {callee}")?;
            writeln!(out, "  {location}")?;
        }
    }
    Ok(())
}

fn render_text_caveats(value: &Value, out: &mut dyn Write) -> io::Result<()> {
    let caveats = public_caveats(value);
    let filtered = caveats
        .iter()
        .filter(|caveat| {
            !matches!(
                caveat.get("code").and_then(Value::as_str),
                Some("no_match" | "broad_query_guard_triggered")
            )
        })
        .collect::<Vec<_>>();
    if filtered.is_empty() {
        return Ok(());
    }
    writeln!(out)?;
    for caveat in filtered {
        let code = caveat
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("caveat");
        let message = caveat
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(code);
        writeln!(out, "caveat: {code}: {message}")?;
    }
    Ok(())
}

fn format_location(path: &str, range: Option<&Value>) -> String {
    let Some(range) = range else {
        return path.to_string();
    };
    let start = range
        .pointer("/start/line")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let end = range
        .pointer("/end/line")
        .and_then(Value::as_u64)
        .unwrap_or(start);
    if start == end {
        format!("{path}:{start}")
    } else {
        format!("{path}:{start}-{end}")
    }
}

fn display_symbol(symbol: &str) -> String {
    let symbol = symbol.trim();
    if symbol.contains("::") {
        return symbol.to_string();
    }
    symbol
        .rsplit(['.', '/', '#'])
        .find(|part| !part.is_empty())
        .unwrap_or(symbol)
        .trim_start_matches("function")
        .trim_start_matches('-')
        .to_string()
}

fn one_line_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn render_text_summary(value: &Value, out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "summary:")?;
    if let Some(matches) = value
        .pointer("/guard/estimatedMatches")
        .and_then(Value::as_u64)
    {
        writeln!(out, "  estimated matches: {matches}")?;
    }
    if let Some(files) = value.pointer("/guard/matchedFiles").and_then(Value::as_u64) {
        writeln!(out, "  matched files: {files}")?;
    }
    render_text_facets(
        value.pointer("/summary/facets/language"),
        out,
        "top languages",
    )?;
    render_text_facets(value.pointer("/summary/facets/topDir"), out, "top dirs")?;
    Ok(())
}

fn render_text_facets(facets: Option<&Value>, out: &mut dyn Write, label: &str) -> io::Result<()> {
    let Some(values) = facets.and_then(Value::as_array) else {
        return Ok(());
    };
    if values.is_empty() {
        return Ok(());
    }
    let rendered = values
        .iter()
        .take(5)
        .filter_map(|facet| {
            let value = facet.get("value").and_then(Value::as_str)?;
            let count = facet.get("count").and_then(Value::as_u64)?;
            Some(format!("{value}={count}"))
        })
        .collect::<Vec<_>>();
    if !rendered.is_empty() {
        writeln!(out, "  {label}: {}", rendered.join(", "))?;
    }
    Ok(())
}
