import XCTest

/// AMUX-4981. These exist so the Share Extension's bootstrap parser is covered
/// by a test that IMPORTS it rather than one that copies it.
///
/// Until this target existed there was nowhere in the project for an iOS unit
/// test to live, so the AMUX-4976 verification was done with standalone .swift
/// scripts that duplicated the function body. Those pin the behaviour of the
/// day they were written and cannot notice a later edit — the exact drift this
/// replaces. AmuxClient.swift is compiled into this target via project.yml,
/// the same mechanism AmuxStore.swift already uses to reach both targets, so
/// these call the shipping code.
///
/// Fixtures are SYNTHETIC and match the shape the server emits. A captured
/// page is not used on purpose: the real bootstrap carries a live owner token.
final class AmuxClientBootstrapTests: XCTestCase {

    /// The server writes string values through serde_json, so they are quoted
    /// and escaped.
    private let page = """
    <!-- AMUX-BOOTSTRAP-BEGIN --><script>\
    window._AMUX_S3_ICAL_URL="https://example.invalid/cal.ics";\
    window._AMUX_AUTH_TOKEN="tok-abcdefghijklmnopqrstuvwxyz0123456789";\
    window._AMUX_HOME="/Users/someone";window._AMUX_AUTH_WITHHELD=false;\
    </script><!-- AMUX-BOOTSTRAP-END -->
    """

    func testExtractsTheOwnerTokenFromAnAuthorizedBootstrap() {
        XCTAssertEqual(
            AmuxClient.bootstrapValue(page, key: "_AMUX_AUTH_TOKEN"),
            "tok-abcdefghijklmnopqrstuvwxyz0123456789")
    }

    /// The value must stop at its own closing quote and not run into the next
    /// assignment.
    func testAValueStopsAtItsClosingQuote() {
        let v = AmuxClient.bootstrapValue(page, key: "_AMUX_S3_ICAL_URL")
        XCTAssertEqual(v, "https://example.invalid/cal.ics")
        XCTAssertFalse(v?.contains(";") ?? true)
    }

    /// serde_json escapes an embedded quote. Splitting on `"` would truncate.
    func testAnEscapedQuoteDoesNotTruncateTheValue() {
        XCTAssertEqual(AmuxClient.bootstrapValue(#"window._X="a\"b";"#, key: "_X"), "a\"b")
    }

    func testAbsentOrUnterminatedValuesAreNil() {
        XCTAssertNil(AmuxClient.bootstrapValue("<html></html>", key: "_X"))
        XCTAssertNil(AmuxClient.bootstrapValue(#"window._X="unterminated"#, key: "_X"))
    }

    func testAnEmptyValueParsesAsEmptyRatherThanNil() {
        // Distinguishable from absent: auth disabled emits an empty token, and
        // anonymous is then the correct request to send.
        XCTAssertEqual(AmuxClient.bootstrapValue(#"window._X="";"#, key: "_X"), "")
    }

    /// THE BUG THIS FILE WOULD HAVE CAUGHT. `_AMUX_AUTH_WITHHELD` is a BARE
    /// bool, not a quoted string, so the string reader — which scans for `="`
    /// — never matches it. Reading it with bootstrapValue made the
    /// "not signed in as owner" branch unreachable: dead code that looked
    /// correct.
    func testTheWithheldFlagNeedsTheBareReaderNotTheStringReader() {
        XCTAssertNil(AmuxClient.bootstrapValue(page, key: "_AMUX_AUTH_WITHHELD"),
                     "the string reader must NOT match a bare bool")
        XCTAssertEqual(AmuxClient.bootstrapFlag(page, key: "_AMUX_AUTH_WITHHELD"), "false")
    }

    /// And the branch must be able to fire.
    func testAWithheldPageReadsTrueSoTheErrorBranchCanFire() {
        XCTAssertEqual(
            AmuxClient.bootstrapFlag(#"window._AMUX_AUTH_WITHHELD=true;x"#,
                                     key: "_AMUX_AUTH_WITHHELD"),
            "true")
    }
}
