use kdl::KdlDocument;

use super::snapshot::TestResult;

#[test]
fn configuration_contract_examples_parse_as_kdl_v2() -> TestResult {
    let contract = include_str!("../../docs/configuration.md");
    let valid_examples = contract.split("## 9.").next().ok_or("missing examples")?;
    let migrated = include_str!("../../writing-block.md").replace(" true", " #true");
    for document in [valid_examples, migrated.as_str()] {
        let mut checked = 0;
        for block in document.split("```kdl\n").skip(1) {
            let source = block.split("```").next().ok_or("unterminated KDL block")?;
            KdlDocument::parse_v2(source)?;
            checked += 1;
        }
        assert!(checked > 0);
    }
    Ok(())
}

#[test]
fn v2_rejects_legacy_bare_booleans() -> TestResult {
    assert!(KdlDocument::parse_v2("spa true\n").is_err());
    let correct = KdlDocument::parse_v2("spa #true\n")?;
    assert_eq!(
        correct.nodes()[0].entries()[0].value().as_bool(),
        Some(true)
    );
    Ok(())
}
