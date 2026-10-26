import XCTest
import TembraNative

final class KernelTests: XCTestCase {
    func testIsolationAndRelease() throws {
        let first = try TembraKernel()
        let second = try TembraKernel()
        defer { second.close() }
        XCTAssertEqual(try first.request("{\"op\":\"allocate\"}"), "{\"ok\":1}")
        XCTAssertEqual(try second.request("{\"op\":\"allocate\"}"), "{\"ok\":1}")
        XCTAssertTrue(try first.request("invalid").contains("\"error\""))
        first.close()
        first.close()
        XCTAssertThrowsError(try first.request("{}"))
        XCTAssertEqual(try second.request("{\"op\":\"allocate\"}"), "{\"ok\":2}")
    }
}
