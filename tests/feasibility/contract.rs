use kdl::KdlDocument;

use super::snapshot::TestResult;

#[test]
fn frozen_contract_examples_parse_as_kdl_v2() -> TestResult {
    let contract = include_str!("../../docs/plans/caddy-feature-parity/CONTRACT.md");
    let mut checked = 0;
    for block in contract.split("```kdl\n").skip(1) {
        let source = block.split("```").next().ok_or("unterminated KDL block")?;
        KdlDocument::parse_v2(source)?;
        checked += 1;
    }
    assert!(checked > 0);
    Ok(())
}
