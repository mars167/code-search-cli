use std::fs;

use codetrail::{
    config_facts::{
        extract_config_facts_for_file, extract_workspace_config_facts, ConfigFactCaveatCode,
        ConfigFactExtractOptions, ConfigFactKind,
    },
    project_graph::discover_project_graph,
    semantic_facts::FactReliability,
};
use tempfile::tempdir;

fn write(path: &std::path::Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

#[test]
fn extracts_mybatis_mapper_namespace_statements_and_xml_references() {
    // Given: a Java project with a MyBatis mapper XML file.
    let dir = tempdir().unwrap();
    write(&dir.path().join("pom.xml"), "<project />\n");
    write(
        &dir.path().join("src/main/java/com/example/UserMapper.java"),
        "package com.example;\ninterface UserMapper {}\n",
    );
    let mapper_path = "src/main/resources/mappers/UserMapper.xml";
    let mapper_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<mapper namespace="com.example.UserMapper">
  <resultMap id="userResult" type="com.example.User">
    <id property="id" column="id"/>
  </resultMap>
  <sql id="BaseColumns">id, name</sql>
  <select id="findById" resultMap="userResult">
    select <include refid="BaseColumns"/> from users where id = #{id}
  </select>
  <update id="rename">
    update users set name = #{name} where id = #{id}
  </update>
</mapper>
"#;
    write(&dir.path().join(mapper_path), mapper_xml);

    // When: workspace config facts are extracted.
    let graph = discover_project_graph(dir.path()).unwrap();
    let facts =
        extract_workspace_config_facts(dir.path(), &graph, ConfigFactExtractOptions::test())
            .unwrap();

    // Then: mapper XML facts are exposed as config facts, not semantic proof.
    assert!(facts.iter().any(|fact| {
        fact.path == mapper_path
            && fact.fact_kind == ConfigFactKind::MyBatisNamespace
            && fact.name.as_deref() == Some("com.example.UserMapper")
            && fact.reliability == FactReliability::ConfigFact
    }));
    assert!(facts.iter().any(|fact| {
        fact.path == mapper_path
            && fact.fact_kind == ConfigFactKind::MyBatisStatement
            && fact.key_path.as_deref() == Some("mybatis.statement.com.example.UserMapper.findById")
            && fact.name.as_deref() == Some("com.example.UserMapper.findById")
            && fact.value_preview.as_deref() == Some("select")
    }));
    assert!(facts.iter().any(|fact| {
        fact.path == mapper_path
            && fact.fact_kind == ConfigFactKind::MyBatisStatement
            && fact.key_path.as_deref() == Some("mybatis.statement.com.example.UserMapper.rename")
            && fact.value_preview.as_deref() == Some("update")
    }));
    assert!(facts.iter().any(|fact| {
        fact.path == mapper_path
            && fact.fact_kind == ConfigFactKind::MyBatisResultMap
            && fact.name.as_deref() == Some("com.example.UserMapper.userResult")
    }));
    assert!(facts.iter().any(|fact| {
        fact.path == mapper_path
            && fact.fact_kind == ConfigFactKind::MyBatisSqlFragment
            && fact.name.as_deref() == Some("com.example.UserMapper.BaseColumns")
    }));
    assert!(facts.iter().any(|fact| {
        fact.path == mapper_path
            && fact.fact_kind == ConfigFactKind::MyBatisReference
            && fact.key_path.as_deref()
                == Some("mybatis.reference.com.example.UserMapper.BaseColumns")
            && fact.value_preview.as_deref() == Some("include refid")
    }));
    assert!(facts.iter().any(|fact| {
        fact.path == mapper_path
            && fact.fact_kind == ConfigFactKind::MyBatisReference
            && fact.key_path.as_deref()
                == Some("mybatis.reference.com.example.UserMapper.userResult")
            && fact.value_preview.as_deref() == Some("statement resultMap")
    }));
}

#[test]
fn malformed_mybatis_xml_yields_parse_failure_source_fallback_without_secret_leak() {
    // Given: malformed mapper XML containing a secret-looking value.
    let dir = tempdir().unwrap();
    write(&dir.path().join("pom.xml"), "<project />\n");
    let path = "src/main/resources/mappers/UserMapper.xml";
    let source = r#"<mapper namespace="com.example.UserMapper">
  <select id="find">select "not-a-real-test-secret"
</mapper broken>
"#;
    write(&dir.path().join(path), source);

    // When: the single file extractor parses it.
    let graph = discover_project_graph(dir.path()).unwrap();
    let facts =
        extract_config_facts_for_file(path, source, &graph, ConfigFactExtractOptions::test());

    // Then: extraction degrades to a source fallback with a parse caveat and
    // does not leak the raw malformed XML body.
    assert_eq!(facts.len(), 1);
    let fallback = &facts[0];
    assert_eq!(fallback.fact_kind, ConfigFactKind::SourceFactFallback);
    assert_eq!(fallback.reliability, FactReliability::SourceFact);
    assert!(fallback
        .caveats
        .iter()
        .any(|caveat| caveat.code == ConfigFactCaveatCode::ParseFailure));
    assert!(!serde_json::to_string(fallback)
        .unwrap()
        .contains("not-a-real-test-secret"));
}
