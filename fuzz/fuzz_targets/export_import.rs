//! The export v1 import (`memory.import` of a `shogunagent-export/v1` package). Invariants: no
//! panic; a malformed package never fails on the database (records are skipped, a non-export is
//! refused whole); the report accounts for every record line; no transaction is left open.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shogun_memory::actor::{Actor, WriteContext};
use shogun_memory::export::{import_v1, is_export_v1, ImportError, FORMAT};

fuzz_target!(|data: &[u8]| {
    let body = String::from_utf8_lossy(data);
    // Half the inputs get a valid manifest so the fuzzer spends its time on records.
    let text = if data.first().is_some_and(|b| b % 2 == 0) {
        format!("{{\"type\":\"manifest\",\"format\":\"{FORMAT}\"}}\n{body}")
    } else {
        body.into_owned()
    };
    let Ok(mut conn) = shogun_memory::open_in_memory() else { return };
    let Ok(actor) = Actor::agent("fuzz") else { return };
    let records = text.lines().filter(|l| !l.trim().is_empty()).count().saturating_sub(1);
    match import_v1(&mut conn, &text, &WriteContext::world(actor), 1_760_000_000_000) {
        Ok(report) => assert_eq!(report.entities + report.facts + report.tombstones + report.skipped, records),
        Err(ImportError::NotExportV1) => assert!(!is_export_v1(&text)),
        Err(ImportError::NotExportV2) => panic!("the v1 importer answered with the v2 refusal"),
        Err(ImportError::TooLarge) => {}
        Err(ImportError::Db(error)) => panic!("database error on a malformed package: {error:?}"),
    }
    assert!(conn.is_autocommit());
});
