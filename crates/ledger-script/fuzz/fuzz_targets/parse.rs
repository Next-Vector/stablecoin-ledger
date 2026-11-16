//! The parser must never panic, and an error must point inside the text (FR-60).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    if let Err(error) = ledger_script::parse(source) {
        let lines = source.split('\n').count() as u32;
        assert!(
            (1..=lines).contains(&error.span.line),
            "line {} outside 1..={lines}: {error}",
            error.span.line
        );
        let line = source
            .split('\n')
            .nth(error.span.line as usize - 1)
            .unwrap_or("");
        let columns = line.chars().count() as u32 + 1;
        assert!(
            (1..=columns).contains(&error.span.column),
            "column {} outside 1..={columns}: {error}",
            error.span.column
        );
        assert!(!error.message.is_empty());
    }
});
