use quick_xml::{
    events::{BytesStart, Event},
    Reader,
};

use super::ranges::{line_len, line_range, whole_file_range};
use super::{make_fact, ConfigFact, ConfigFactCaveat, ConfigFactKind, EdgeContext};
use crate::semantic_facts::{FactReliability, InternalRange};

pub(super) fn extract_mybatis_xml(
    path: &str,
    source: &str,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
) -> std::result::Result<Vec<ConfigFact>, String> {
    if !looks_like_mybatis_mapper(source) {
        return Ok(Vec::new());
    }

    let mut reader = Reader::from_str(source);
    reader.config_mut().trim_text(true);
    reader.config_mut().check_end_names = true;

    let mut buf = Vec::new();
    let mut namespace = None;
    let mut facts = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                let tag = tag_name(&element)?;
                if tag == "mapper" {
                    if let Some(value) = attr_value(&reader, &element, b"namespace")? {
                        facts.push(mybatis_fact(
                            path,
                            source,
                            &tag,
                            "namespace",
                            &value,
                            ConfigFactKind::MyBatisNamespace,
                            "mybatis.namespace".to_string(),
                            value.clone(),
                            "namespace",
                            edge_context,
                            base_caveats,
                        ));
                        namespace = Some(value);
                    }
                } else if tag == "resultMap" {
                    if let (Some(ns), Some(id)) =
                        (namespace.as_deref(), attr_value(&reader, &element, b"id")?)
                    {
                        let qualified = qualify(ns, &id);
                        facts.push(mybatis_fact(
                            path,
                            source,
                            &tag,
                            "id",
                            &id,
                            ConfigFactKind::MyBatisResultMap,
                            format!("mybatis.result_map.{qualified}"),
                            qualified,
                            "resultMap",
                            edge_context,
                            base_caveats,
                        ));
                    }
                } else if tag == "sql" {
                    if let (Some(ns), Some(id)) =
                        (namespace.as_deref(), attr_value(&reader, &element, b"id")?)
                    {
                        let qualified = qualify(ns, &id);
                        facts.push(mybatis_fact(
                            path,
                            source,
                            &tag,
                            "id",
                            &id,
                            ConfigFactKind::MyBatisSqlFragment,
                            format!("mybatis.sql_fragment.{qualified}"),
                            qualified,
                            "sql",
                            edge_context,
                            base_caveats,
                        ));
                    }
                } else if is_statement_tag(&tag) {
                    collect_statement_facts(
                        path,
                        source,
                        &reader,
                        &element,
                        &tag,
                        namespace.as_deref(),
                        edge_context,
                        base_caveats,
                        &mut facts,
                    )?;
                } else if tag == "include" {
                    if let (Some(ns), Some(refid)) = (
                        namespace.as_deref(),
                        attr_value(&reader, &element, b"refid")?,
                    ) {
                        facts.push(reference_fact(
                            path,
                            source,
                            &tag,
                            "refid",
                            &refid,
                            ns,
                            "include refid",
                            edge_context,
                            base_caveats,
                        ));
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(format!(
                    "structured MyBatis XML parse failed at byte {}: {error}",
                    reader.error_position()
                ));
            }
        }
        buf.clear();
    }
    Ok(facts)
}

fn collect_statement_facts(
    path: &str,
    source: &str,
    reader: &Reader<&[u8]>,
    element: &BytesStart<'_>,
    tag: &str,
    namespace: Option<&str>,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
    facts: &mut Vec<ConfigFact>,
) -> std::result::Result<(), String> {
    let Some(ns) = namespace else {
        return Ok(());
    };
    if let Some(id) = attr_value(reader, element, b"id")? {
        let qualified = qualify(ns, &id);
        facts.push(mybatis_fact(
            path,
            source,
            tag,
            "id",
            &id,
            ConfigFactKind::MyBatisStatement,
            format!("mybatis.statement.{qualified}"),
            qualified,
            tag,
            edge_context,
            base_caveats,
        ));
    }
    if let Some(result_map) = attr_value(reader, element, b"resultMap")? {
        facts.push(reference_fact(
            path,
            source,
            tag,
            "resultMap",
            &result_map,
            ns,
            "statement resultMap",
            edge_context,
            base_caveats,
        ));
    }
    Ok(())
}

fn mybatis_fact(
    path: &str,
    source: &str,
    tag: &str,
    attr: &str,
    attr_value: &str,
    fact_kind: ConfigFactKind,
    key_path: String,
    name: String,
    value: &str,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
) -> ConfigFact {
    make_fact(
        path,
        attr_range(source, tag, attr, attr_value),
        fact_kind,
        Some(key_path),
        Some(name),
        Some(value),
        FactReliability::ConfigFact,
        edge_context,
        base_caveats,
    )
}

fn reference_fact(
    path: &str,
    source: &str,
    tag: &str,
    attr: &str,
    attr_value: &str,
    namespace: &str,
    value: &str,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
) -> ConfigFact {
    let qualified = qualify(namespace, attr_value);
    mybatis_fact(
        path,
        source,
        tag,
        attr,
        attr_value,
        ConfigFactKind::MyBatisReference,
        format!("mybatis.reference.{qualified}"),
        qualified,
        value,
        edge_context,
        base_caveats,
    )
}

fn looks_like_mybatis_mapper(source: &str) -> bool {
    source.contains("<mapper") && source.contains("namespace")
}

fn is_statement_tag(tag: &str) -> bool {
    matches!(tag, "select" | "insert" | "update" | "delete")
}

fn tag_name(element: &BytesStart<'_>) -> std::result::Result<String, String> {
    std::str::from_utf8(element.name().as_ref())
        .map(str::to_string)
        .map_err(|error| format!("structured MyBatis XML tag decode failed: {error}"))
}

fn attr_value(
    reader: &Reader<&[u8]>,
    element: &BytesStart<'_>,
    wanted: &[u8],
) -> std::result::Result<Option<String>, String> {
    for attr in element.attributes() {
        let attr = attr
            .map_err(|error| format!("structured MyBatis XML attribute parse failed: {error}"))?;
        if attr.key.as_ref() == wanted {
            let value = attr
                .decode_and_unescape_value(reader.decoder())
                .map_err(|error| {
                    format!("structured MyBatis XML attribute decode failed: {error}")
                })?;
            return Ok(Some(value.into_owned()));
        }
    }
    Ok(None)
}

fn qualify(namespace: &str, value: &str) -> String {
    if value.contains('.') {
        value.to_string()
    } else {
        format!("{namespace}.{value}")
    }
}

fn attr_range(source: &str, tag: &str, attr: &str, value: &str) -> InternalRange {
    let double_quoted = format!("{attr}=\"{value}\"");
    let single_quoted = format!("{attr}='{value}'");
    source
        .lines()
        .enumerate()
        .find_map(|(index, line)| {
            (line.contains(&format!("<{tag}"))
                && (line.contains(&double_quoted) || line.contains(&single_quoted)))
            .then_some(line_range(source, index, 0, line_len(source, index)))
        })
        .unwrap_or_else(|| whole_file_range(source))
}
